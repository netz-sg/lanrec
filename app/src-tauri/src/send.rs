//! Driving a recording from the app.
//!
//! The pipeline itself lives in `lanrec_core::session`, the same one the CLI
//! drives. This is only the thread, the stop flag and the status the window
//! polls.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use anyhow::{Context, Result, bail};
use lanrec_core::capture::{self, MonitorInfo};
use lanrec_core::config::{self, Labels};
use lanrec_core::d3d::Gpu;
use lanrec_core::net::nic;
use lanrec_core::profile::Profile;
use lanrec_core::session::{self, SessionStatus, Target};

pub struct Session {
    stop: Arc<AtomicBool>,
    status: Arc<Mutex<SessionStatus>>,
    handle: Option<JoinHandle<()>>,
}

impl Session {
    pub fn status(&self) -> SessionStatus {
        self.status.lock().expect("send status poisoned").clone()
    }

    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// True once the pipeline has actually wound down.
    pub fn is_done(&self) -> bool {
        self.status().finished
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            // The capture loop wakes every 200 ms, so this join is short.
            let _ = h.join();
        }
    }
}

/// Resolve everything that can fail before starting the thread, so a typo or an
/// unplugged cable surfaces immediately rather than as a status field later.
pub fn start(
    to: &str,
    via_mac: Option<&str>,
    monitor_index: Option<usize>,
    profile: &Profile,
    file: Option<PathBuf>,
) -> Result<Session> {
    let monitor = resolve_monitor(monitor_index)?;

    let target = match file {
        Some(path) => Target::File(path),
        None => {
            let addr: SocketAddr = to.trim().parse().with_context(|| {
                format!(
                    "\"{}\" ist keine gültige Adresse — erwartet wird etwa 10.0.0.2:9000",
                    to.trim()
                )
            })?;
            let via = match via_mac {
                Some(mac) if !mac.is_empty() => {
                    let labels = Labels::load(&config::labels_path()?);
                    Some(Box::new(nic::find(mac, &labels)?))
                }
                _ => None,
            };
            Target::Net { addr, via }
        }
    };

    // The pipeline does not scale, so the encode has to match the display it is
    // reading. The window shows the result rather than the request.
    let profile = Profile {
        width: monitor.width,
        height: monitor.height,
        ..profile.clone()
    };

    let cfg = session::Config {
        monitor,
        profile,
        target,
        // Runs until the user presses stop.
        duration: None,
    };

    let stop = Arc::new(AtomicBool::new(false));
    let status = Arc::new(Mutex::new(SessionStatus::default()));

    let handle = {
        let (stop, status) = (stop.clone(), status.clone());
        std::thread::Builder::new()
            .name("lanrec-send".into())
            .spawn(move || {
                // Errors land in the status rather than being lost: run() puts
                // them there before it returns.
                let _ = session::run(&cfg, &stop, &mut |s: &SessionStatus| {
                    *status.lock().expect("send status poisoned") = s.clone();
                });
            })
            .context("Sende-Thread starten")?
    };

    Ok(Session {
        stop,
        status,
        handle: Some(handle),
    })
}

fn resolve_monitor(index: Option<usize>) -> Result<MonitorInfo> {
    // A short-lived device just to enumerate; the session creates its own on the
    // thread that will actually use it.
    let gpu = Gpu::new()?;
    let monitors = capture::monitors(&gpu)?;
    if monitors.is_empty() {
        bail!("keine Displays gefunden");
    }
    let idx = match index {
        Some(i) if i < monitors.len() => i,
        _ => monitors.iter().position(|m| m.primary).unwrap_or(0),
    };
    Ok(monitors[idx].clone())
}
