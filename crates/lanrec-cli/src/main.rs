//! Headless front end to lanrec-core -- the sender side.
//!
//! Exists so the capture pipeline can be measured without the UI in the way:
//! when a recording drops frames, the first question is always whether the app or
//! the pipeline is at fault. It reads the same settings file as the app, and
//! drives the same `session::run`, so the two cannot behave differently.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use lanrec_core::capture::{self, Capture, MonitorInfo};
use lanrec_core::config::{self, Labels};
use lanrec_core::d3d::Gpu;
use lanrec_core::net::nic::{self, Medium, Suitability};
use lanrec_core::nvenc;
use lanrec_core::pace::{Pacer, Step};
use lanrec_core::profile::{BitDepth, Chroma, Profile, RateControl};
use lanrec_core::session::{self, SessionStatus, Target};

#[derive(Parser)]
#[command(name = "lanrec", about = "Capture a gaming PC over Ethernet", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

/// Encode settings shared by `record` and `send`.
#[derive(clap::Args, Clone)]
struct EncodeArgs {
    /// How long to record. 0 means until Ctrl-C.
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
        output: PathBuf,
        #[command(flatten)]
        enc: EncodeArgs,
    },
    /// Stream a display to a lanrec receiver on another machine.
    Send {
        /// Receiver address, e.g. 10.0.0.2:9000.
        #[arg(short, long)]
        to: String,
        /// Which adapter to send over: its name, IPv4 or MAC.
        ///
        /// Without this the routing table decides, which with two adapters can
        /// silently mean the wrong one.
        #[arg(long)]
        via: Option<String>,
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
        Cmd::Record { output, enc } => record(Target::File(output), &enc, None),
        Cmd::Send { to, via, enc } => send(&to, via.as_deref(), &enc),
    }
}

// ----------------------------------------------------------------- recording ---

fn record(target: Target, a: &EncodeArgs, via_label: Option<String>) -> Result<()> {
    let gpu = Gpu::new()?;
    let monitor = pick_monitor(&gpu, a.monitor)?;
    let profile = build_profile(&monitor, a);
    // Dropping the device here keeps the session's own one the only user of the
    // GPU; it creates a fresh device on the thread that drives the encode.
    drop(gpu);

    describe(&monitor, &profile, a);
    match (&target, &via_label) {
        (Target::File(p), _) => println!("-> {}\n", p.display()),
        (Target::Net { addr, .. }, Some(v)) => println!("-> {addr}  ueber {v}\n"),
        (Target::Net { addr, .. }, None) => {
            println!("-> {addr}  (Route vom System gewaehlt)\n")
        }
    }

    let cfg = session::Config {
        monitor,
        profile,
        target,
        duration: (a.seconds > 0).then(|| Duration::from_secs(a.seconds)),
    };

    let mut last_line = Instant::now();
    let mut done = false;

    session::run(&cfg, &AtomicBool::new(false), &mut |s: &SessionStatus| {
        if s.finished && !done {
            done = true;
            report(s);
            return;
        }
        // One line a second is enough to see whether it is keeping up.
        if !done && last_line.elapsed() >= Duration::from_secs(1) {
            last_line = Instant::now();
            eprintln!(
                "  {:>3}s  {} Frames  {:.0} Mbit/s",
                s.seconds as u64,
                s.frames,
                s.bitrate_bps / 1e6
            );
        }
    })
}

fn send(to: &str, via: Option<&str>, a: &EncodeArgs) -> Result<()> {
    let addr: SocketAddr = to
        .parse()
        .with_context(|| format!("{to} ist keine gueltige Adresse (z.B. 10.0.0.2:9000)"))?;

    // Resolve before anything expensive happens, so a typo fails immediately
    // rather than after the encoder is up.
    let nic = match via {
        Some(spec) => {
            let labels = Labels::load(&config::labels_path()?);
            Some(nic::find(spec, &labels)?)
        }
        None => None,
    };
    let label = nic.as_ref().map(|n| n.display_name.clone());

    record(
        Target::Net {
            addr,
            via: nic.map(Box::new),
        },
        a,
        label,
    )
}

fn report(s: &SessionStatus) {
    println!(
        "Frames      {}  ({} neu, {} wiederholt)",
        s.frames,
        s.frames.saturating_sub(s.repeats),
        s.repeats
    );
    println!("Keyframes   {}", s.keyframes);
    println!("Groesse     {:.1} MB", s.bytes as f64 / 1e6);
    println!("Bitrate     {:.0} Mbit/s", s.bitrate_bps / 1e6);
    println!(
        "Groesster   {:.0} kB (Keyframe)",
        s.peak_frame_bytes as f64 / 1e3
    );
    println!("Verworfen   {} (Pacing)", s.paced_out);
    println!("Queue-Drop  {} (Encoder zu langsam)", s.queue_dropped);
    if let Some(local) = &s.local_addr {
        println!("Gesendet    ueber {local}");
    }

    if let Some(e) = &s.error {
        println!("\nFehler: {e}");
    } else if s.queue_dropped > 0 {
        println!("\n! Queue-Drops: der Encoder oder die Leitung kam nicht mit.");
    }
}

// ----------------------------------------------------------------- inspection ---

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
    let how_long = if a.seconds > 0 {
        format!("{}s", a.seconds)
    } else {
        "bis Ctrl-C".into()
    };
    println!(
        "{} {}x{} -> {} {} {}-bit, QP {}, {} fps, {how_long}",
        m.device,
        m.width,
        m.height,
        p.codec.label(),
        if a.chroma420 { "4:2:0" } else { "4:4:4" },
        if a.eight_bit { 8 } else { 10 },
        a.qp,
        a.fps,
    );
}

fn list_monitors() -> Result<()> {
    let gpu = Gpu::new()?;
    for (i, m) in capture::monitors(&gpu)?.iter().enumerate() {
        let tag = if m.primary { "  (primaer)" } else { "" };
        println!("[{i}] {}  {}x{}{tag}", m.device, m.width, m.height);
    }
    Ok(())
}

/// Capture for a while and report what actually arrived, without encoding.
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

    println!(
        "Eingang       {captured} Frames  ({:.1} fps)",
        rate(captured)
    );
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
    if b { "ja" } else { "nein" }
}
