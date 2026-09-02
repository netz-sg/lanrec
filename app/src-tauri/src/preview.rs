//! Live preview, running on its own thread with its own GPU device.
//!
//! Kept off the main device on purpose: the preview is a convenience and must
//! never contend for the same D3D11 context a recording is using. Everything it
//! needs is created inside the thread, so nothing has to cross a thread boundary.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use lanrec_core::capture::{self, Capture};
use lanrec_core::d3d::Gpu;
use lanrec_core::preview::Downscaler;

/// Preview rate. Fast enough to look live, slow enough to cost nothing.
const PREVIEW_FPS: u64 = 10;

/// Visibly lossy would defeat the purpose; visually perfect would waste the IPC.
const JPEG_QUALITY: u8 = 72;

pub struct Preview {
    stop: Arc<AtomicBool>,
    latest: Arc<Mutex<Option<Frame>>>,
    error: Arc<Mutex<Option<String>>>,
    handle: Option<JoinHandle<()>>,
}

#[derive(Clone)]
pub struct Frame {
    pub jpeg: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

impl Preview {
    pub fn start(monitor: Option<usize>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let latest = Arc::new(Mutex::new(None));
        let error = Arc::new(Mutex::new(None));

        let handle = {
            let (stop, latest, error) = (stop.clone(), latest.clone(), error.clone());
            std::thread::Builder::new()
                .name("lanrec-preview".into())
                .spawn(move || {
                    if let Err(e) = run(monitor, &stop, &latest) {
                        // Surfaced through the UI rather than swallowed: a preview
                        // that silently shows nothing is worse than one that says
                        // why.
                        *error.lock().expect("preview error poisoned") = Some(format!("{e:#}"));
                    }
                })
                .ok()
        };

        Self {
            stop,
            latest,
            error,
            handle,
        }
    }

    pub fn frame(&self) -> Option<Frame> {
        self.latest.lock().expect("preview frame poisoned").clone()
    }

    pub fn error(&self) -> Option<String> {
        self.error.lock().expect("preview error poisoned").clone()
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            // The thread wakes at least every 200 ms from its receive timeout, so
            // this join is short.
            let _ = h.join();
        }
    }
}

fn run(monitor: Option<usize>, stop: &AtomicBool, latest: &Mutex<Option<Frame>>) -> Result<()> {
    let gpu = Gpu::new()?;
    let monitors = capture::monitors(&gpu)?;
    if monitors.is_empty() {
        bail!("keine Displays gefunden");
    }
    let idx = match monitor {
        Some(i) if i < monitors.len() => i,
        _ => monitors.iter().position(|m| m.primary).unwrap_or(0),
    };
    let m = &monitors[idx];

    let cap = Capture::monitor(&gpu, m)?;
    let down = Downscaler::new(&gpu, m.width, m.height)?;
    let interval = Duration::from_millis(1000 / PREVIEW_FPS);
    let mut next = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        let Some(frame) = cap.recv_timeout(Duration::from_millis(200))? else {
            continue;
        };

        // Frames arrive as fast as the screen changes; the preview only needs a
        // few per second.
        if Instant::now() < next {
            continue;
        }
        next = Instant::now() + interval;

        let jpeg = down.jpeg(&frame.texture, JPEG_QUALITY)?;
        *latest.lock().expect("preview frame poisoned") = Some(Frame {
            jpeg,
            width: down.width,
            height: down.height,
        });
    }

    Ok(())
}
