//! Headless receiver.
//!
//! A thin shell over `lanrec_recv`, so the desktop receiver app and this share
//! one implementation rather than drifting apart.

use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result};
use clap::Parser;
use lanrec_recv::{Config, Status, default_out_dir, serve};

#[derive(Parser)]
#[command(
    name = "lanrec-recv",
    about = "Receive a lanrec stream and write it to disk",
    version
)]
struct Cli {
    /// Address to listen on. Use the address of the direct link rather than
    /// 0.0.0.0 unless you mean to accept from anywhere.
    #[arg(short, long, default_value = "0.0.0.0:9000")]
    listen: String,

    /// Where recordings go. The file name is derived from the stream.
    #[arg(short, long)]
    out_dir: Option<PathBuf>,

    /// Write to exactly this file instead of a generated name.
    #[arg(short = 'f', long)]
    file: Option<PathBuf>,

    /// Keep listening for the next sender after one finishes.
    #[arg(long)]
    keep_running: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let listen: SocketAddr = cli.listen.parse().with_context(|| {
        format!(
            "{} ist keine gueltige Adresse (z.B. 10.0.0.2:9000)",
            cli.listen
        )
    })?;

    let cfg = Config {
        listen,
        out_dir: cli.out_dir.unwrap_or_else(default_out_dir),
        file: cli.file,
        keep_running: cli.keep_running,
    };

    println!("lanrec-recv lauscht auf {}", cfg.listen);

    // Tracks what has already been printed, so the header for one recording is
    // written once rather than on every status update.
    let mut announced = false;
    let mut done = false;

    serve(&cfg, &AtomicBool::new(false), &mut |s: &Status| {
        if s.finished && !done {
            done = true;
            report(s);
            return;
        }
        if s.peer.is_none() || s.finished {
            return;
        }
        if !announced && s.codec.is_some() {
            announced = true;
            header(s);
        }
        if announced {
            print!(
                "\r  {} Frames  {:.0} MB  {:.0} Mbit/s   ",
                s.frames,
                s.bytes as f64 / 1e6,
                s.bitrate_bps / 1e6
            );
            let _ = std::io::stdout().flush();
        }
    })
}

fn header(s: &Status) {
    println!("\nVerbindung von {}", s.peer.as_deref().unwrap_or("?"));
    println!(
        "  {} {}x{} {} {}-bit @ {:.0} fps",
        s.codec.as_deref().unwrap_or("?").to_uppercase(),
        s.width,
        s.height,
        s.chroma.as_deref().unwrap_or("?"),
        s.bit_depth,
        s.fps,
    );
    println!("  Quelle: {}", s.source.as_deref().unwrap_or("?"));
    println!("  -> {}", s.path.as_deref().unwrap_or("?"));
}

fn report(s: &Status) {
    println!(
        "\r  {} Frames, davon {} Keyframes          ",
        s.frames, s.keyframes
    );
    println!(
        "  {:.1} MB in {:.1}s  ({:.0} Mbit/s)",
        s.bytes as f64 / 1e6,
        s.seconds,
        s.bitrate_bps / 1e6
    );
    if let Some(p) = &s.path {
        println!("  {p}");
    }

    if let Some(e) = &s.error {
        println!("\n  ! {e}");
    }
    if !s.clean_end {
        // Worth saying out loud: the file is still playable, but it stops where
        // the link did, and that is not what the user asked for.
        println!("\n  ! Der Sender hat sich nicht ordentlich abgemeldet.");
        println!("    Die Aufnahme endet dort, wo die Verbindung abriss.");
    }
}
