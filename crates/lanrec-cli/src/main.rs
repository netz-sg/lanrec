//! Headless front end to lanrec-core -- the sender side.
//!
//! Exists so the capture pipeline can be measured without the UI in the way:
//! when a recording drops frames, the first question is always whether the app or
//! the pipeline is at fault. It reads the same settings file as the app, so
//! adapter names match in both.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use socket2::{Domain, Socket, Type};

use lanrec_core::capture::{self, Capture, MonitorInfo, Texture};
use lanrec_core::config::{self, Labels};
use lanrec_core::d3d::Gpu;
use lanrec_core::net::nic::{self, Medium, Suitability};
use lanrec_core::nvenc::encoder::{Encoder, FileSink, FrameSink};
use lanrec_core::nvenc::{self, Nvenc};
use lanrec_core::pace::{Pacer, Source, Step};
use lanrec_core::profile::{BitDepth, Chroma, Profile, RateControl};
use lanrec_wire::{write_end, write_frame, write_info, Kind, StreamInfo, FLAG_KEYFRAME};

/// Matches the receiver. The OS default stalls a few-hundred-Mbit/s stream.
const SOCKET_BUFFER: usize = 8 << 20;

#[derive(Parser)]
#[command(name = "lanrec", about = "Capture a gaming PC over Ethernet", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

/// Encode settings shared by `record` and `send`.
#[derive(clap::Args, Clone)]
struct EncodeArgs {
    #[arg(long, default_value_t = 10)]
    seconds: u64,
    #[arg(long, default_value_t = 60)]
    fps: u32,
    /// Constant quantiser. Lower is better quality and more bitrate.
    #[arg(long, default_value_t = 14)]
    qp: u8,
    /// Encode 4:2:0 instead of 4:4:4.
    #[arg(long)]
    chroma420: bool,
    /// Encode 8-bit instead of 10-bit.
    #[arg(long)]
    eight_bit: bool,
    /// Which display, by index from `monitors`. Defaults to the primary.
    #[arg(long)]
    monitor: Option<usize>,
}

#[derive(Subcommand)]
enum Cmd {
    /// List network adapters and judge their fitness for carrying the stream.
    Nics,
    /// Give an adapter a name. Pass an empty label to clear it.
    Rename {
        /// MAC address of the adapter, as shown by `nics`.
        mac: String,
        /// The name to show instead of the Windows one.
        label: String,
    },
    /// Report what this GPU's encoder can actually do.
    Caps,
    /// List the displays that can be captured.
    Monitors,
    /// Capture a display for a while and report how the pacer coped.
    Capture {
        #[arg(long, default_value_t = 5)]
        seconds: u64,
        #[arg(long, default_value_t = 60)]
        fps: u32,
        #[arg(long)]
        monitor: Option<usize>,
    },
    /// Record a display to a local elementary stream.
    Record {
        #[arg(short, long, default_value = "out.hevc")]
        output: String,
        #[command(flatten)]
        enc: EncodeArgs,
    },
    /// Stream a display to a lanrec-recv on another machine.
    Send {
        /// Receiver address, e.g. 10.0.0.2:9000.
        #[arg(short, long)]
        to: String,
        #[command(flatten)]
        enc: EncodeArgs,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Nics => list_nics(),
        Cmd::Rename { mac, label } => rename(&mac, &label),
        Cmd::Caps => show_caps(),
        Cmd::Monitors => list_monitors(),
        Cmd::Capture {
            seconds,
            fps,
            monitor,
        } => measure_capture(seconds, fps, monitor),
        Cmd::Record { output, enc } => record(&output, &enc),
        Cmd::Send { to, enc } => send(&to, &enc),
    }
}

// ----------------------------------------------------------------- pipeline ---

/// What one capture run produced beyond what the encoder itself counts.
struct RunStats {
    repeats: u64,
    paced_out: u64,
    queue_dropped: u64,
}

/// Capture, pace and encode until the clock runs out.
///
/// Shared by `record` and `send`; only the sink differs.
fn run(
    gpu: &Gpu,
    m: &MonitorInfo,
    seconds: u64,
    fps: u32,
    enc: &mut Encoder,
    sink: &mut dyn FrameSink,
) -> Result<RunStats> {
    let cap = Capture::monitor(gpu, m)?;
    let mut pacer = Pacer::new(fps, 1);
    let period = pacer.period_ns();

    // The frame the pacer held back, and the last one actually encoded -- the
    // latter is what gets repeated into slots where the screen did not change.
    let mut held: Option<lanrec_core::capture::CapturedFrame> = None;
    let mut last: Option<(Texture, u64)> = None;
    let mut repeats = 0u64;

    let started = Instant::now();
    let mut next_report = started + Duration::from_secs(1);

    while started.elapsed().as_secs() < seconds {
        if Instant::now() >= next_report {
            next_report = Instant::now() + Duration::from_secs(1);
            eprintln!(
                "  {:>3}s  {} Frames  {:.0} Mbit/s",
                started.elapsed().as_secs(),
                enc.frames,
                enc.bytes as f64 * 8.0 / 1e6 / started.elapsed().as_secs_f64()
            );
        }

        // Polled rather than blocking: a motionless screen delivers no frames at
        // all, and the run still has to end when its time is up.
        let Some(frame) = cap.recv_timeout(Duration::from_millis(200))? else {
            continue;
        };

        match pacer.step(frame.timestamp_ns) {
            Step::Hold => held = Some(frame),
            Step::Emit {
                pts_ns,
                source,
                gap_slots,
            } => {
                if let Some((tex, last_pts)) = &last {
                    for k in 1..=gap_slots {
                        enc.encode(tex, last_pts + period * k, sink)?;
                        repeats += 1;
                    }
                }

                let texture = match source {
                    Source::Held => held
                        .replace(frame)
                        .context("Pacer wollte den gehaltenen Frame, es gab aber keinen")?
                        .texture,
                    Source::Incoming => {
                        held = None;
                        frame.texture
                    }
                };

                enc.encode(&texture, pts_ns, sink)?;
                last = Some((texture, pts_ns));
            }
        }
    }

    enc.finish()?;

    Ok(RunStats {
        repeats,
        paced_out: pacer.dropped,
        queue_dropped: cap.dropped(),
    })
}

fn pick_monitor(gpu: &Gpu, which: Option<usize>) -> Result<MonitorInfo> {
    let monitors = capture::monitors(gpu)?;
    if monitors.is_empty() {
        bail!("keine Displays gefunden");
    }
    let idx = match which {
        Some(i) if i < monitors.len() => i,
        Some(i) => bail!("Monitor {i} gibt es nicht -- `lanrec monitors` zeigt die vorhandenen"),
        None => monitors.iter().position(|m| m.primary).unwrap_or(0),
    };
    Ok(monitors[idx].clone())
}

fn build_profile(m: &MonitorInfo, a: &EncodeArgs) -> Profile {
    Profile {
        width: m.width,
        height: m.height,
        fps_num: a.fps,
        fps_den: 1,
        chroma: if a.chroma420 {
            Chroma::Yuv420
        } else {
            Chroma::Yuv444
        },
        depth: if a.eight_bit {
            BitDepth::Eight
        } else {
            BitDepth::Ten
        },
        rate_control: RateControl::Cqp { qp: a.qp },
        ..Profile::maximum_quality()
    }
}

fn describe(m: &MonitorInfo, p: &Profile, a: &EncodeArgs) {
    println!(
        "{} {}x{} -> {} {} {}-bit, QP {}, {} fps, {}s",
        m.device,
        m.width,
        m.height,
        p.codec.label(),
        if a.chroma420 { "4:2:0" } else { "4:4:4" },
        if a.eight_bit { 8 } else { 10 },
        a.qp,
        a.fps,
        a.seconds,
    );
}

fn report(enc: &Encoder, stats: &RunStats, elapsed: f64) {
    println!(
        "Frames      {}  ({} neu, {} wiederholt)",
        enc.frames,
        enc.frames - stats.repeats,
        stats.repeats
    );
    println!("Keyframes   {}", enc.keyframes);
    println!("Groesse     {:.1} MB", enc.bytes as f64 / 1e6);
    println!(
        "Bitrate     {:.0} Mbit/s",
        enc.bytes as f64 * 8.0 / 1e6 / elapsed
    );
    println!(
        "Groesster   {:.0} kB (Keyframe)",
        enc.peak_frame_bytes as f64 / 1e3
    );
    println!("Verworfen   {} (Pacing)", stats.paced_out);
    println!("Queue-Drop  {} (Encoder zu langsam)", stats.queue_dropped);

    if stats.queue_dropped > 0 {
        println!("\n! Queue-Drops: der Encoder oder die Leitung kam nicht mit.");
    }
}

// ----------------------------------------------------------------- commands ---

fn record(output: &str, a: &EncodeArgs) -> Result<()> {
    let gpu = Gpu::new()?;
    let m = pick_monitor(&gpu, a.monitor)?;
    let profile = build_profile(&m, a);

    describe(&m, &profile, a);
    println!("-> {output}\n");

    let nvenc = Arc::new(Nvenc::load()?);
    let mut enc = Encoder::new(&nvenc, &gpu, &profile)?;
    let mut sink = FileSink(BufWriter::new(
        File::create(output).with_context(|| format!("{output} anlegen"))?,
    ));

    let started = Instant::now();
    let stats = run(&gpu, &m, a.seconds, a.fps, &mut enc, &mut sink)?;
    sink.0.flush().context("Datei abschliessen")?;

    report(&enc, &stats, started.elapsed().as_secs_f64());
    Ok(())
}

/// Frames each encoded picture and puts it on the wire.
struct NetSink<W: Write> {
    out: W,
}

impl<W: Write> FrameSink for NetSink<W> {
    fn frame(&mut self, pts_ns: u64, keyframe: bool, data: &[u8]) -> Result<()> {
        let flags = if keyframe { FLAG_KEYFRAME } else { 0 };
        write_frame(&mut self.out, Kind::Video, pts_ns, flags, data)
    }
}

fn send(to: &str, a: &EncodeArgs) -> Result<()> {
    let gpu = Gpu::new()?;
    let m = pick_monitor(&gpu, a.monitor)?;
    let profile = build_profile(&m, a);

    describe(&m, &profile, a);
    println!("-> {to}\n");

    let stream = connect(to)?;
    let mut sink = NetSink {
        out: BufWriter::with_capacity(1 << 20, stream),
    };

    // The receiver needs the geometry before the first frame, both to report it
    // and to name the file.
    write_info(
        &mut sink.out,
        &StreamInfo {
            codec: "hevc".into(),
            width: profile.width,
            height: profile.height,
            fps_num: profile.fps_num,
            fps_den: profile.fps_den,
            chroma: if a.chroma420 { "yuv420" } else { "yuv444" }.into(),
            bit_depth: if a.eight_bit { 8 } else { 10 },
            source: m.device.clone(),
            sender: format!("lanrec {}", env!("CARGO_PKG_VERSION")),
        },
    )?;

    let nvenc = Arc::new(Nvenc::load()?);
    let mut enc = Encoder::new(&nvenc, &gpu, &profile)?;

    let started = Instant::now();
    let stats = run(&gpu, &m, a.seconds, a.fps, &mut enc, &mut sink)?;

    // Tell the receiver this was a clean end, so it does not warn about a
    // recording that stops mid-air.
    write_end(&mut sink.out)?;
    sink.out.flush().context("Verbindung leeren")?;

    report(&enc, &stats, started.elapsed().as_secs_f64());
    Ok(())
}

/// Connect with a send buffer big enough for the stream.
fn connect(to: &str) -> Result<TcpStream> {
    let addr: SocketAddr = to
        .parse()
        .with_context(|| format!("{to} ist keine gueltige Adresse (z.B. 10.0.0.2:9000)"))?;

    let socket =
        Socket::new(Domain::for_address(addr), Type::STREAM, None).context("Socket anlegen")?;
    if let Err(e) = socket.set_send_buffer_size(SOCKET_BUFFER) {
        eprintln!("Hinweis: Sendepuffer konnte nicht gesetzt werden: {e}");
    }
    socket
        .connect(&addr.into())
        .with_context(|| format!("keine Verbindung zu {addr} -- laeuft lanrec-recv dort?"))?;

    let stream: TcpStream = socket.into();
    // Each frame is written as one burst; waiting to coalesce only adds latency.
    stream.set_nodelay(true).ok();
    Ok(stream)
}

fn list_monitors() -> Result<()> {
    let gpu = Gpu::new()?;
    for (i, m) in capture::monitors(&gpu)?.iter().enumerate() {
        let tag = if m.primary { "  (primaer)" } else { "" };
        println!("[{i}] {}  {}x{}{tag}", m.device, m.width, m.height);
    }
    Ok(())
}

/// Capture for a while and report what actually arrived.
fn measure_capture(seconds: u64, fps: u32, monitor: Option<usize>) -> Result<()> {
    let gpu = Gpu::new()?;
    let m = pick_monitor(&gpu, monitor)?;

    println!(
        "Capture {} {}x{} -> {fps} fps, {seconds}s\n",
        m.device, m.width, m.height
    );

    let cap = Capture::monitor(&gpu, &m)?;
    let mut pacer = Pacer::new(fps, 1);

    let started = Instant::now();
    let (mut captured, mut fresh, mut repeats) = (0u64, 0u64, 0u64);
    let (mut first_ts, mut last_ts) = (None, 0u64);

    while started.elapsed().as_secs() < seconds {
        let Some(frame) = cap.recv_timeout(Duration::from_millis(200))? else {
            continue;
        };
        captured += 1;
        first_ts.get_or_insert(frame.timestamp_ns);
        last_ts = frame.timestamp_ns;

        if let Step::Emit { gap_slots, .. } = pacer.step(frame.timestamp_ns) {
            repeats += gap_slots;
            fresh += 1;
        }
    }

    let span_s = (last_ts - first_ts.unwrap_or(0)) as f64 / 1e9;
    let rate = |n: u64| if span_s > 0.0 { n as f64 / span_s } else { 0.0 };
    let total = fresh + repeats;

    println!("Eingang       {captured} Frames  ({:.1} fps)", rate(captured));
    println!("Ausgang       {total} Frames  ({:.1} fps)", rate(total));
    println!("  davon neu   {fresh}");
    println!("  Wiederholt  {repeats} (Slots ohne Bildaenderung)");
    println!("Verworfen     {} (Pacing)", pacer.dropped);
    println!("Queue-Drop    {} (Consumer zu langsam)", cap.dropped());

    if cap.dropped() > 0 {
        println!("\n! Queue-Drops bedeuten, dass der Consumer nicht mitkommt.");
    } else if repeats > fresh {
        println!("\nMehr Wiederholungen als neue Frames -- normal auf ruhendem Desktop,");
        println!("im Spiel ein Zeichen dafuer, dass die Aufnahme hungert.");
    }
    Ok(())
}

fn list_nics() -> Result<()> {
    let labels = Labels::load(&config::labels_path()?);
    let nics = nic::enumerate_view(&labels)?;

    println!("Netzwerkadapter\n");

    for n in &nics {
        let mark = match n.suitability {
            Suitability::Good => "[+]",
            Suitability::Marginal => "[~]",
            Suitability::Unusable => "[-]",
        };
        let medium = match n.medium {
            Medium::Ethernet => "Ethernet",
            Medium::WiFi => "WLAN",
            Medium::Loopback => "Loopback",
            Medium::Other => "sonstige",
        };
        let link = if n.up {
            format!("Link up @ {}", n.link_speed_label)
        } else {
            "kein Link".to_string()
        };

        // Show the Windows name alongside a custom one, so the adapter is still
        // findable in the Windows dialogs.
        match &n.label {
            Some(_) => println!("{mark} {}  ({})", n.display_name, n.name),
            None => println!("{mark} {}", n.display_name),
        }
        println!("      {}", n.description);
        println!("      {medium}, {link}, MTU {}", n.mtu);
        if let Some(mac) = &n.mac {
            println!("      MAC  {mac}");
        }
        if !n.ipv4.is_empty() {
            let hint = if n.has_gateway {
                ""
            } else {
                "  (kein Gateway - sieht nach Direktverbindung aus)"
            };
            println!("      IPv4 {}{hint}", n.ipv4.join(", "));
        }
        if let Some(note) = &n.note {
            println!("      ! {note}");
        }
        println!();
    }

    Ok(())
}

fn rename(mac: &str, label: &str) -> Result<()> {
    let path = config::labels_path()?;
    let mut labels = Labels::load(&path);

    // Refuse a MAC that no adapter has, rather than silently storing a label that
    // will never be shown.
    let known = nic::enumerate()?
        .iter()
        .filter_map(|n| n.mac)
        .any(|m| format_mac(&m).eq_ignore_ascii_case(mac.trim()));
    if !known {
        bail!("kein Adapter mit MAC {mac} gefunden -- `lanrec nics` zeigt die vorhandenen");
    }

    labels.set(mac, label);
    labels.save(&path)?;

    if label.trim().is_empty() {
        println!("Name entfernt.");
    } else {
        println!("{mac} heisst jetzt \"{}\".", label.trim());
    }
    Ok(())
}

fn format_mac(m: &[u8; 6]) -> String {
    m.iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

fn show_caps() -> Result<()> {
    let gpu = Gpu::new()?;
    let caps = nvenc::probe(&gpu.device, gpu.adapter_name()?)?;

    println!("{}", caps.adapter);
    println!("NVENC-Engines: {}\n", caps.encoder_engines);

    for c in &caps.codecs {
        println!("{}", c.label);
        println!("      4:4:4       {}", yes_no(c.yuv444));
        println!("      10-bit      {}", yes_no(c.ten_bit));
        println!("      lossless    {}", yes_no(c.lossless));
        println!("      max         {}x{}", c.max_width, c.max_height);
        println!();
    }
    Ok(())
}

fn yes_no(b: bool) -> &'static str {
    if b {
        "ja"
    } else {
        "nein"
    }
}
