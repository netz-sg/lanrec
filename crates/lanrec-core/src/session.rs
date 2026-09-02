//! One recording: capture, pace, encode, and put the result somewhere.
//!
//! Lives in the core rather than in either front end, because both the CLI and
//! the app need exactly this loop. A second copy of it would drift, and the
//! parts that are easy to get subtly wrong -- gap filling, adapter pinning,
//! flushing per frame -- would then be wrong in only one of them.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::os::windows::io::AsRawSocket;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use lanrec_wire::{FLAG_KEYFRAME, Kind, StreamInfo, write_end, write_frame, write_info};
use serde::Serialize;
use socket2::{Domain, Socket, Type};

use crate::capture::{Capture, CapturedFrame, MonitorInfo, Texture};
use crate::clock;
use crate::d3d::Gpu;
use crate::net::bind;
use crate::net::nic::NicView;
use crate::nvenc::Nvenc;
use crate::nvenc::encoder::{Encoder, FrameSink};
use crate::pace::{Pacer, Source, Step};
use crate::profile::{BitDepth, Chroma, Profile};

/// Matches the receiver. The OS default stalls a few-hundred-megabit stream.
const SOCKET_BUFFER: usize = 8 << 20;

/// How long a receive waits before the loop checks the clock and the stop flag.
const RECV_TIMEOUT: Duration = Duration::from_millis(200);

/// How often the caller hears about progress.
const STATUS_INTERVAL: Duration = Duration::from_millis(250);

/// Where a recording goes.
pub enum Target {
    File(PathBuf),
    Net {
        addr: SocketAddr,
        /// Adapter to force the stream onto. `None` leaves the choice to the
        /// routing table, which with two adapters can silently be the wrong one.
        ///
        /// Boxed so a plain file target does not carry the weight of a NicView.
        via: Option<Box<NicView>>,
    },
}

pub struct Config {
    pub monitor: MonitorInfo,
    pub profile: Profile,
    pub target: Target,
    /// `None` records until stopped.
    pub duration: Option<Duration>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStatus {
    pub destination: String,
    /// Adapter actually used, once known.
    pub via: Option<String>,
    /// Local address the connection got, read back rather than assumed.
    pub local_addr: Option<String>,

    pub frames: u64,
    pub keyframes: u64,
    /// Frames that repeat the previous one because nothing on screen changed.
    pub repeats: u64,
    pub bytes: u64,
    pub seconds: f64,
    pub bitrate_bps: f64,
    pub peak_frame_bytes: u64,

    /// Frames discarded by pacing. Expected, and large at 165 -> 60.
    pub paced_out: u64,
    /// Frames lost because the encoder or the link could not keep up. Should
    /// stay at zero; anything else is the real limit being hit.
    pub queue_dropped: u64,

    pub finished: bool,
    pub error: Option<String>,
}

/// Concatenates frames into an Annex B elementary stream.
struct FileSink<W: Write>(W);

impl<W: Write> FrameSink for FileSink<W> {
    fn frame(&mut self, _pts_ns: u64, _keyframe: bool, data: &[u8]) -> Result<()> {
        self.0.write_all(data).context("Bitstream schreiben")
    }
}

/// Frames each encoded picture and puts it on the wire.
struct NetSink<W: Write> {
    out: W,
}

impl<W: Write> FrameSink for NetSink<W> {
    fn frame(&mut self, pts_ns: u64, keyframe: bool, data: &[u8]) -> Result<()> {
        let flags = if keyframe { FLAG_KEYFRAME } else { 0 };
        write_frame(&mut self.out, Kind::Video, pts_ns, flags, data)?;

        // Flush per frame. The buffer exists so that a header and its payload
        // leave as one write, not to batch frames: left to fill, it holds the
        // whole recording of a quiet screen and the receiver sees nothing until
        // the very end. One syscall per frame at 60 fps costs nothing.
        self.out.flush().context("Frame absenden")
    }
}

/// Record until the duration elapses or `stop` is set.
///
/// `on_status` is called about four times a second, and once at the end.
pub fn run(
    cfg: &Config,
    stop: &AtomicBool,
    on_status: &mut dyn FnMut(&SessionStatus),
) -> Result<()> {
    let mut status = SessionStatus {
        destination: match &cfg.target {
            Target::File(p) => p.display().to_string(),
            Target::Net { addr, .. } => addr.to_string(),
        },
        via: match &cfg.target {
            Target::Net { via: Some(n), .. } => Some(n.display_name.clone()),
            _ => None,
        },
        ..Default::default()
    };

    let result = drive(cfg, stop, &mut status, on_status);
    if let Err(e) = &result {
        status.error = Some(format!("{e:#}"));
    }
    status.finished = true;
    on_status(&status);
    result
}

fn drive(
    cfg: &Config,
    stop: &AtomicBool,
    status: &mut SessionStatus,
    on_status: &mut dyn FnMut(&SessionStatus),
) -> Result<()> {
    let gpu = Gpu::new()?;
    let nvenc = Arc::new(Nvenc::load()?);
    let mut enc = Encoder::new(&nvenc, &gpu, &cfg.profile)?;

    match &cfg.target {
        Target::File(path) => {
            let mut sink = FileSink(BufWriter::new(
                File::create(path).with_context(|| format!("{} anlegen", path.display()))?,
            ));
            let r = pump(cfg, stop, &gpu, &mut enc, &mut sink, status, on_status);
            sink.0.flush().context("Datei abschliessen")?;
            r
        }
        Target::Net { addr, via } => {
            let stream = connect(*addr, via.as_deref())?;
            status.local_addr = stream.local_addr().ok().map(|a| a.ip().to_string());
            on_status(status);

            let mut sink = NetSink {
                out: BufWriter::with_capacity(1 << 20, stream),
            };
            write_info(&mut sink.out, &stream_info(cfg))?;

            let r = pump(cfg, stop, &gpu, &mut enc, &mut sink, status, on_status);

            // Tell the receiver this was a clean end, so it does not warn about
            // a recording that stops mid-air.
            write_end(&mut sink.out)?;
            sink.out.flush().context("Verbindung leeren")?;
            r
        }
    }
}

fn stream_info(cfg: &Config) -> StreamInfo {
    StreamInfo {
        codec: "hevc".into(),
        width: cfg.profile.width,
        height: cfg.profile.height,
        fps_num: cfg.profile.fps_num,
        fps_den: cfg.profile.fps_den,
        chroma: match cfg.profile.chroma {
            Chroma::Yuv444 => "yuv444",
            Chroma::Yuv420 => "yuv420",
        }
        .into(),
        bit_depth: match cfg.profile.depth {
            BitDepth::Ten => 10,
            BitDepth::Eight => 8,
        },
        source: cfg.monitor.device.clone(),
        sender: format!("lanrec {}", env!("CARGO_PKG_VERSION")),
    }
}

#[allow(clippy::too_many_arguments)]
fn pump(
    cfg: &Config,
    stop: &AtomicBool,
    gpu: &Gpu,
    enc: &mut Encoder,
    sink: &mut dyn FrameSink,
    status: &mut SessionStatus,
    on_status: &mut dyn FnMut(&SessionStatus),
) -> Result<()> {
    let cap = Capture::monitor(gpu, &cfg.monitor)?;
    let mut pacer = Pacer::new(cfg.profile.fps_num, cfg.profile.fps_den);
    let period = pacer.period_ns();

    // The frame the pacer held back, and the last one actually encoded -- the
    // latter is what gets repeated into slots where the screen did not change.
    let mut held: Option<CapturedFrame> = None;
    let mut last: Option<(Texture, u64)> = None;

    let started = Instant::now();
    let mut next_report = started + STATUS_INTERVAL;

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if cfg.duration.is_some_and(|d| started.elapsed() >= d) {
            break;
        }

        if Instant::now() >= next_report {
            next_report = Instant::now() + STATUS_INTERVAL;
            update(status, enc, &pacer, &cap, started);
            on_status(status);
        }

        // Polled rather than blocking: a motionless screen delivers no frames at
        // all, and the run still has to notice the clock and the stop flag.
        let Some(frame) = cap.recv_timeout(RECV_TIMEOUT)? else {
            fill_gaps(&mut pacer, &mut last, period, enc, sink, status)?;
            continue;
        };

        match pacer.step(frame.timestamp_ns) {
            Step::Hold => held = Some(frame),
            Step::Emit {
                pts_ns,
                source,
                gap_slots,
            } => {
                if let Some((tex, last_pts)) = last.clone() {
                    for k in 1..=gap_slots {
                        enc.encode(&tex, last_pts + period * k, sink)?;
                        status.repeats += 1;
                    }
                }

                let texture = match source {
                    Source::Held => {
                        held.replace(frame)
                            .context("Pacer wollte den gehaltenen Frame, es gab aber keinen")?
                            .texture
                    }
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

    // The run may well end during a stall, so close the timeline before the
    // encoder does.
    fill_gaps(&mut pacer, &mut last, period, enc, sink, status)?;
    enc.finish()?;

    update(status, enc, &pacer, &cap, started);
    Ok(())
}

fn update(
    status: &mut SessionStatus,
    enc: &Encoder,
    pacer: &Pacer,
    cap: &Capture,
    started: Instant,
) {
    status.frames = enc.frames;
    status.keyframes = enc.keyframes;
    status.bytes = enc.bytes;
    status.peak_frame_bytes = enc.peak_frame_bytes;
    status.paced_out = pacer.dropped;
    status.queue_dropped = cap.dropped();
    status.seconds = started.elapsed().as_secs_f64();
    status.bitrate_bps = if status.seconds > 0.0 {
        enc.bytes as f64 * 8.0 / status.seconds
    } else {
        0.0
    };
}

/// Repeat the last encoded frame into every output slot that has passed with no
/// new one, and advance `last` to match.
///
/// Without this, a screen that stops changing stalls the whole timeline: the
/// receiver goes silent, and a run that ends during the stall is missing its
/// tail. Repeats of identical content cost almost nothing.
fn fill_gaps(
    pacer: &mut Pacer,
    last: &mut Option<(Texture, u64)>,
    period: u64,
    enc: &mut Encoder,
    sink: &mut dyn FrameSink,
    status: &mut SessionStatus,
) -> Result<()> {
    // Before the first real frame there is nothing to repeat.
    let Some((tex, last_pts)) = last.clone() else {
        return Ok(());
    };

    let fill = pacer.catch_up(clock::now_ns());
    for k in 1..=fill {
        enc.encode(&tex, last_pts + period * k, sink)?;
        status.repeats += 1;
    }
    if fill > 0 {
        *last = Some((tex, last_pts + period * fill));
    }
    Ok(())
}

/// Connect with a send buffer big enough for the stream, optionally forced onto
/// one adapter.
///
/// When `via` is given the socket is both pinned to that interface and bound to
/// its address, and the result is verified afterwards -- see the module comment
/// on [`crate::net::bind`] for why neither alone is enough.
pub fn connect(addr: SocketAddr, via: Option<&NicView>) -> Result<TcpStream> {
    let socket =
        Socket::new(Domain::for_address(addr), Type::STREAM, None).context("Socket anlegen")?;
    if let Err(e) = socket.set_send_buffer_size(SOCKET_BUFFER) {
        eprintln!("Hinweis: Sendepuffer konnte nicht gesetzt werden: {e}");
    }

    let wanted = match via {
        Some(n) => {
            if !n.up {
                bail!("{} hat keinen Link -- Kabel steckt nicht", n.display_name);
            }
            let ip: IpAddr = n
                .ipv4
                .first()
                .with_context(|| {
                    format!(
                        "{} hat keine IPv4-Adresse -- ohne die kann nichts darueber gesendet werden",
                        n.display_name
                    )
                })?
                .parse()
                .context("IPv4 des Adapters parsen")?;

            bind::pin_to_interface(socket.as_raw_socket(), n.index)?;
            socket
                .bind(&SocketAddr::new(ip, 0).into())
                .with_context(|| format!("an {ip} binden"))?;
            Some(ip)
        }
        None => None,
    };

    socket.connect(&addr.into()).with_context(|| match via {
        Some(n) => format!(
            "keine Verbindung zu {addr} ueber {} -- ist der Empfaenger an diesem Kabel?",
            n.display_name
        ),
        None => format!("keine Verbindung zu {addr} -- laeuft lanrec-recv dort?"),
    })?;

    let stream: TcpStream = socket.into();
    // Each frame is written as one burst; waiting to coalesce only adds latency.
    stream.set_nodelay(true).ok();

    // Trust nothing: read back which address the connection actually got.
    if let Some(want) = wanted {
        let got = stream
            .local_addr()
            .context("lokale Adresse ermitteln")?
            .ip();
        if got != want {
            bail!(
                "Verbindung laeuft ueber {got} statt ueber {want} -- der gewaehlte Adapter wurde nicht benutzt"
            );
        }
    }

    Ok(stream)
}
