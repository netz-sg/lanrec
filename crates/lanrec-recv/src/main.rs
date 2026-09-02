//! The lanrec receiver.
//!
//! Runs on the machine that is not gaming -- a Mac, another PC, anything with a
//! disk. It does not decode: it reads framed messages off the socket and appends
//! the payloads to a file. Remuxing rather than re-encoding is the whole point,
//! so this side stays close to idle even at a few hundred Mbit/s.
//!
//! Deliberately depends on nothing but std and the shared wire crate, so it
//! builds anywhere.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::Parser;
use lanrec_wire::{read_frame, Kind, StreamInfo};
use socket2::{Domain, Socket, Type};

/// Big enough that the disk is written in large chunks rather than per frame.
const WRITE_BUFFER: usize = 4 << 20;

/// The OS default receive buffer is a few hundred kilobytes, which is not enough
/// to keep a few-hundred-Mbit/s stream flowing: the window closes, the sender
/// stalls, and the capture queue behind it starts dropping frames. Raising it is
/// the single most effective thing on this side.
const SOCKET_BUFFER: usize = 8 << 20;

#[derive(Parser)]
#[command(name = "lanrec-recv", about = "Receive a lanrec stream and write it to disk", version)]
struct Cli {
    /// Address to listen on. Use the IP of the direct link, not 0.0.0.0, unless
    /// you mean to accept from anywhere.
    #[arg(short, long, default_value = "0.0.0.0:9000")]
    listen: String,

    /// Where recordings go. The file name is derived from the stream.
    #[arg(short, long, default_value = ".")]
    out_dir: PathBuf,

    /// Write to exactly this file instead of a generated name.
    #[arg(short = 'f', long)]
    file: Option<PathBuf>,

    /// Keep listening for the next sender after one finishes.
    #[arg(long)]
    keep_running: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let listener = listen(&cli.listen)?;
    println!("lanrec-recv lauscht auf {}", cli.listen);

    loop {
        let (stream, peer) = listener.accept().context("Verbindung annehmen")?;
        println!("\nVerbindung von {peer}");

        if let Err(e) = receive(stream, &cli) {
            // One failed recording must not take the receiver down; the sender may
            // simply have been unplugged.
            eprintln!("Aufnahme abgebrochen: {e:#}");
        }

        if !cli.keep_running {
            return Ok(());
        }
    }
}

/// Bind a listener with a receive buffer large enough for the stream.
///
/// `TcpListener::bind` gives no way to set socket options before the bind, hence
/// the detour through socket2.
fn listen(addr: &str) -> Result<TcpListener> {
    let addr: SocketAddr = addr
        .parse()
        .with_context(|| format!("{addr} ist keine gueltige Adresse (z.B. 10.0.0.2:9000)"))?;

    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, None)
        .context("Socket anlegen")?;
    socket.set_reuse_address(true).ok();
    // Best effort: some systems clamp this, and a clamped buffer is still better
    // than refusing to start.
    if let Err(e) = socket.set_recv_buffer_size(SOCKET_BUFFER) {
        eprintln!("Hinweis: Empfangspuffer konnte nicht auf {SOCKET_BUFFER} gesetzt werden: {e}");
    }
    socket.bind(&addr.into()).with_context(|| format!("an {addr} binden"))?;
    socket.listen(16).context("listen")?;

    Ok(socket.into())
}

fn receive(stream: TcpStream, cli: &Cli) -> Result<()> {
    stream.set_nodelay(true).ok();

    let mut input = stream;
    let mut payload = Vec::new();

    // The first frame describes the stream, and names the file.
    let header = read_frame(&mut input, &mut payload)?
        .context("Verbindung ohne einen einzigen Frame geschlossen")?;
    if header.kind != Kind::Info {
        anyhow::bail!("erster Frame war {:?}, erwartet Info", header.kind);
    }
    let info: StreamInfo =
        serde_json::from_slice(&payload).context("StreamInfo konnte nicht gelesen werden")?;

    println!(
        "  {} {}x{} {} {}-bit @ {:.0} fps",
        info.codec.to_uppercase(),
        info.width,
        info.height,
        info.chroma,
        info.bit_depth,
        info.fps_num as f64 / info.fps_den.max(1) as f64,
    );
    println!("  Quelle: {}  (Sender {})", info.source, info.sender);

    let path = match &cli.file {
        Some(p) => p.clone(),
        None => {
            fs::create_dir_all(&cli.out_dir)
                .with_context(|| format!("{} anlegen", cli.out_dir.display()))?;
            cli.out_dir.join(generated_name(&info))
        }
    };
    println!("  -> {}", path.display());

    let mut out = BufWriter::with_capacity(
        WRITE_BUFFER,
        File::create(&path).with_context(|| format!("{} anlegen", path.display()))?,
    );

    let started = Instant::now();
    let mut next_report = started + std::time::Duration::from_secs(1);
    let (mut frames, mut bytes, mut keyframes) = (0u64, 0u64, 0u64);
    let mut clean_end = false;

    while let Some(h) = read_frame(&mut input, &mut payload)? {
        match h.kind {
            Kind::Video => {
                out.write_all(&payload).context("auf die Platte schreiben")?;
                frames += 1;
                bytes += payload.len() as u64;
                if h.is_keyframe() {
                    keyframes += 1;
                }
            }
            Kind::End => {
                clean_end = true;
                break;
            }
            // Audio arrives in M3; ignoring it now keeps an older receiver usable
            // against a newer sender.
            Kind::Audio | Kind::Info => {}
        }

        if Instant::now() >= next_report {
            next_report = Instant::now() + std::time::Duration::from_secs(1);
            let secs = started.elapsed().as_secs_f64();
            print!(
                "\r  {frames} Frames  {:.0} MB  {:.0} Mbit/s   ",
                bytes as f64 / 1e6,
                bytes as f64 * 8.0 / 1e6 / secs
            );
            let _ = std::io::stdout().flush();
        }
    }

    out.flush().context("Datei abschliessen")?;
    let secs = started.elapsed().as_secs_f64().max(1e-9);

    println!("\r  {frames} Frames, davon {keyframes} Keyframes          ");
    println!("  {:.1} MB in {:.1}s  ({:.0} Mbit/s)", bytes as f64 / 1e6, secs, bytes as f64 * 8.0 / 1e6 / secs);
    println!("  {}", path.display());

    if !clean_end {
        // Worth saying out loud: the file is still playable, but it stops wherever
        // the link did, and that is not what the user asked for.
        println!("\n  ! Der Sender hat sich nicht ordentlich abgemeldet.");
        println!("    Die Aufnahme endet dort, wo die Verbindung abriss.");
    }

    Ok(())
}

/// `lanrec-20260902-104500-1440p60.hevc`
fn generated_name(info: &StreamInfo) -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let fps = (info.fps_num as f64 / info.fps_den.max(1) as f64).round() as u32;
    format!(
        "lanrec-{secs}-{}p{fps}.{}",
        info.height,
        info.extension()
    )
}
