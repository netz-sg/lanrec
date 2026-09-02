//! The receiving half of lanrec.
//!
//! Runs on the machine that is not gaming. It does not decode: it reads framed
//! messages off the socket and appends the payloads to a file. Remuxing rather
//! than re-encoding is the whole point, so this side stays close to idle even at
//! a few hundred megabits per second.
//!
//! Deliberately depends on nothing but std and the shared wire crate, so it
//! builds on macOS and Linux as well as Windows -- and so the desktop receiver
//! app and the headless CLI share exactly this code rather than two copies of it.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use lanrec_wire::{Kind, StreamInfo, read_frame};
use serde::Serialize;
use socket2::{Domain, Socket, Type};

/// Big enough that the disk is written in large chunks rather than per frame.
const WRITE_BUFFER: usize = 4 << 20;

/// The OS default receive buffer is a few hundred kilobytes, which is not enough
/// to keep a few-hundred-megabit stream flowing: the window closes, the sender
/// stalls, and the capture queue behind it starts dropping frames. Raising it is
/// the single most effective thing on this side.
const SOCKET_BUFFER: usize = 8 << 20;

/// How often the caller is told what is happening.
const STATUS_INTERVAL: Duration = Duration::from_millis(250);

/// How long the accept loop sleeps between checks while idle.
///
/// Short enough that a stop request feels immediate, long enough not to spin.
const ACCEPT_POLL: Duration = Duration::from_millis(100);

/// Bounds how long a read waits, so a stop request is noticed even while the
/// sender is quiet. Not a deadline for the recording -- a timeout just loops.
const READ_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Debug, Clone)]
pub struct Config {
    pub listen: SocketAddr,
    /// Where generated file names go.
    pub out_dir: PathBuf,
    /// Write to exactly this path instead of a generated name.
    pub file: Option<PathBuf>,
    /// Keep listening for the next sender after one finishes.
    pub keep_running: bool,
}

/// What the receiver is doing, flat enough to hand straight to a UI.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub listening_on: String,
    /// Set while a sender is connected.
    pub peer: Option<String>,
    pub codec: Option<String>,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub chroma: Option<String>,
    pub bit_depth: u8,
    pub source: Option<String>,

    pub frames: u64,
    pub keyframes: u64,
    pub bytes: u64,
    pub seconds: f64,
    pub bitrate_bps: f64,
    pub path: Option<String>,

    /// True once a recording has ended, cleanly or otherwise.
    pub finished: bool,
    /// False means the sender vanished instead of saying goodbye, so the file
    /// stops wherever the link did.
    pub clean_end: bool,
    pub error: Option<String>,
}

impl Status {
    fn reset_stream(&mut self) {
        let listening = std::mem::take(&mut self.listening_on);
        *self = Status {
            listening_on: listening,
            ..Default::default()
        };
    }
}

/// Bind a listener with a receive buffer large enough for the stream.
///
/// `TcpListener::bind` gives no way to set socket options before the bind, hence
/// the detour through socket2.
pub fn listen(addr: SocketAddr) -> Result<TcpListener> {
    let socket =
        Socket::new(Domain::for_address(addr), Type::STREAM, None).context("Socket anlegen")?;
    socket.set_reuse_address(true).ok();
    // Best effort: some systems clamp this, and a clamped buffer is still better
    // than refusing to start.
    if let Err(e) = socket.set_recv_buffer_size(SOCKET_BUFFER) {
        eprintln!("Hinweis: Empfangspuffer konnte nicht gesetzt werden: {e}");
    }
    socket
        .bind(&addr.into())
        .with_context(|| format!("an {addr} binden"))?;
    socket.listen(16).context("listen")?;
    Ok(socket.into())
}

/// Accept senders and write what they send, until `stop` is set.
///
/// `on_status` is called on connect, roughly four times a second while
/// receiving, and once when a recording ends.
pub fn serve(cfg: &Config, stop: &AtomicBool, on_status: &mut dyn FnMut(&Status)) -> Result<()> {
    let listener = listen(cfg.listen)?;
    // Non-blocking so a stop request does not have to wait for a sender that
    // may never come.
    listener
        .set_nonblocking(true)
        .context("Listener auf non-blocking setzen")?;

    let mut status = Status {
        listening_on: cfg.listen.to_string(),
        ..Default::default()
    };
    on_status(&status);

    while !stop.load(Ordering::Relaxed) {
        let (stream, peer) = match listener.accept() {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_POLL);
                continue;
            }
            Err(e) => return Err(e).context("Verbindung annehmen"),
        };

        status.reset_stream();
        status.peer = Some(peer.to_string());
        on_status(&status);

        // A connection that goes wrong must not take the receiver down; the
        // sender may simply have been unplugged.
        if let Err(e) = receive(stream, cfg, stop, &mut status, on_status) {
            status.error = Some(format!("{e:#}"));
        }
        status.finished = true;
        on_status(&status);

        if !cfg.keep_running {
            return Ok(());
        }
    }

    Ok(())
}

fn receive(
    stream: TcpStream,
    cfg: &Config,
    stop: &AtomicBool,
    status: &mut Status,
    on_status: &mut dyn FnMut(&Status),
) -> Result<()> {
    stream.set_nodelay(true).ok();
    // On Windows a socket returned by accept() inherits the listener's
    // non-blocking mode, which turns the very first read into a WouldBlock
    // error instead of a wait. Put it back into blocking mode explicitly and
    // bound the waiting with a timeout instead.
    stream
        .set_nonblocking(false)
        .context("Verbindung auf blocking setzen")?;
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .context("Lese-Timeout setzen")?;

    let mut input = stream;
    let mut payload = Vec::new();

    // The first frame describes the stream, and names the file. A timeout here
    // is not an error either -- the sender may still be starting its encoder.
    let header = loop {
        if stop.load(Ordering::Relaxed) {
            bail!("abgebrochen, bevor der Sender sich gemeldet hat");
        }
        match read_frame(&mut input, &mut payload) {
            Ok(Some(h)) => break h,
            Ok(None) => bail!("Verbindung ohne einen einzigen Frame geschlossen"),
            Err(e) if is_timeout(&e) => continue,
            Err(e) => return Err(e),
        }
    };
    if header.kind != Kind::Info {
        bail!("erster Frame war {:?}, erwartet Info", header.kind);
    }
    let info: StreamInfo =
        serde_json::from_slice(&payload).context("StreamInfo konnte nicht gelesen werden")?;

    let path = match &cfg.file {
        Some(p) => p.clone(),
        None => {
            fs::create_dir_all(&cfg.out_dir)
                .with_context(|| format!("{} anlegen", cfg.out_dir.display()))?;
            cfg.out_dir.join(generated_name(&info))
        }
    };

    status.codec = Some(info.codec.clone());
    status.width = info.width;
    status.height = info.height;
    status.fps = info.fps_num as f64 / info.fps_den.max(1) as f64;
    status.chroma = Some(info.chroma.clone());
    status.bit_depth = info.bit_depth;
    status.source = Some(info.source.clone());
    status.path = Some(path.display().to_string());
    on_status(status);

    let mut out = BufWriter::with_capacity(
        WRITE_BUFFER,
        File::create(&path).with_context(|| format!("{} anlegen", path.display()))?,
    );

    let started = Instant::now();
    let mut next_report = started + STATUS_INTERVAL;

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        let frame = match read_frame(&mut input, &mut payload) {
            Ok(Some(h)) => h,
            Ok(None) => break,
            // A read timeout is not an error: the sender is simply quiet.
            Err(e) if is_timeout(&e) => continue,
            Err(e) => return Err(e),
        };

        match frame.kind {
            Kind::Video => {
                out.write_all(&payload)
                    .context("auf die Platte schreiben")?;
                status.frames += 1;
                status.bytes += payload.len() as u64;
                if frame.is_keyframe() {
                    status.keyframes += 1;
                }
            }
            Kind::End => {
                status.clean_end = true;
                break;
            }
            // Audio arrives in M3; ignoring it keeps an older receiver usable
            // against a newer sender.
            Kind::Audio | Kind::Info => {}
        }

        if Instant::now() >= next_report {
            next_report = Instant::now() + STATUS_INTERVAL;
            update_rates(status, started);
            on_status(status);
        }
    }

    out.flush().context("Datei abschliessen")?;
    update_rates(status, started);
    Ok(())
}

fn update_rates(status: &mut Status, started: Instant) {
    status.seconds = started.elapsed().as_secs_f64();
    status.bitrate_bps = if status.seconds > 0.0 {
        status.bytes as f64 * 8.0 / status.seconds
    } else {
        0.0
    };
}

/// A read timeout surfaces differently across platforms.
fn is_timeout(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            )
        })
    })
}

/// `lanrec-1788342433-1440p60.hevc`
pub fn generated_name(info: &StreamInfo) -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let fps = (info.fps_num as f64 / info.fps_den.max(1) as f64).round() as u32;
    format!("lanrec-{secs}-{}p{fps}.{}", info.height, info.extension())
}

/// Default place to put recordings: `~/lanrec`.
pub fn default_out_dir() -> PathBuf {
    home()
        .map(|h| h.join("lanrec"))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn home() -> Option<PathBuf> {
    // Deliberately no dependency for this: one variable on each platform.
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|p: &PathBuf| p.as_path() != Path::new(""))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> StreamInfo {
        StreamInfo {
            codec: "hevc".into(),
            width: 2560,
            height: 1440,
            fps_num: 60,
            fps_den: 1,
            chroma: "yuv444".into(),
            bit_depth: 10,
            source: "test".into(),
            sender: "test".into(),
        }
    }

    #[test]
    fn generated_name_describes_the_stream() {
        let n = generated_name(&info());
        assert!(n.starts_with("lanrec-"), "{n}");
        assert!(n.ends_with("-1440p60.hevc"), "{n}");
    }

    #[test]
    fn av1_gets_its_own_extension() {
        let mut i = info();
        i.codec = "av1".into();
        assert!(generated_name(&i).ends_with(".obu"));
    }

    #[test]
    fn reset_keeps_the_listen_address() {
        // The address is a property of the receiver, not of one recording, and
        // the UI would flicker if it vanished between senders.
        let mut s = Status {
            listening_on: "10.0.0.2:9000".into(),
            frames: 123,
            ..Default::default()
        };
        s.reset_stream();
        assert_eq!(s.listening_on, "10.0.0.2:9000");
        assert_eq!(s.frames, 0);
    }
}
