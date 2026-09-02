//! The receiver, with a window.
//!
//! Wraps `lanrec_recv` rather than reimplementing it, so this and the headless
//! `lanrec-recv` binary cannot drift apart. Everything platform-specific lives on
//! the sender side, which is why this builds on macOS.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use lanrec_recv::{Config, Status, default_out_dir, serve};
use serde::{Deserialize, Serialize};
use tauri::State;

#[derive(Default)]
struct AppState {
    session: Mutex<Option<Session>>,
}

/// A running listener and the thread driving it.
struct Session {
    stop: Arc<AtomicBool>,
    status: Arc<Mutex<Status>>,
    /// Set when the listener itself failed -- a taken port, a bad address.
    fatal: Arc<Mutex<Option<String>>>,
    handle: Option<JoinHandle<()>>,
    out_dir: PathBuf,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            // The accept loop and every read wake at least twice a second, so
            // this join is short.
            let _ = h.join();
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartRequest {
    /// e.g. "0.0.0.0:9000" or "10.0.0.2:9000".
    listen: String,
    out_dir: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct View {
    running: bool,
    out_dir: String,
    status: Option<Status>,
    /// The listener could not start at all.
    fatal: Option<String>,
}

#[tauri::command]
fn suggested_out_dir() -> String {
    default_out_dir().display().to_string()
}

#[tauri::command]
fn start(req: StartRequest, state: State<AppState>) -> Result<(), String> {
    let listen: SocketAddr = req.listen.trim().parse().map_err(|_| {
        format!(
            "\"{}\" ist keine gültige Adresse — erwartet wird etwa 10.0.0.2:9000",
            req.listen.trim()
        )
    })?;

    let out_dir = req
        .out_dir
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(default_out_dir);

    let cfg = Config {
        listen,
        out_dir: out_dir.clone(),
        file: None,
        // A receiver with a window should stay ready for the next recording
        // rather than quitting after one.
        keep_running: true,
    };

    let stop = Arc::new(AtomicBool::new(false));
    let status = Arc::new(Mutex::new(Status {
        listening_on: listen.to_string(),
        ..Default::default()
    }));
    let fatal = Arc::new(Mutex::new(None));

    let handle = {
        let (stop, status, fatal) = (stop.clone(), status.clone(), fatal.clone());
        std::thread::Builder::new()
            .name("lanrec-recv".into())
            .spawn(move || {
                let result = serve(&cfg, &stop, &mut |s: &Status| {
                    *status.lock().expect("status poisoned") = s.clone();
                });
                if let Err(e) = result {
                    *fatal.lock().expect("fatal poisoned") = Some(format!("{e:#}"));
                }
            })
            .map_err(|e| e.to_string())?
    };

    let mut slot = state.session.lock().map_err(|e| e.to_string())?;
    // Dropping the previous session stops its thread before a new one binds the
    // same port.
    *slot = None;
    *slot = Some(Session {
        stop,
        status,
        fatal,
        handle: Some(handle),
        out_dir,
    });
    Ok(())
}

#[tauri::command]
fn stop(state: State<AppState>) -> Result<(), String> {
    let mut slot = state.session.lock().map_err(|e| e.to_string())?;
    *slot = None;
    Ok(())
}

/// Polled by the UI a few times a second.
#[tauri::command]
fn view(state: State<AppState>) -> Result<View, String> {
    let slot = state.session.lock().map_err(|e| e.to_string())?;
    Ok(match slot.as_ref() {
        Some(s) => View {
            running: true,
            out_dir: s.out_dir.display().to_string(),
            status: Some(s.status.lock().map_err(|e| e.to_string())?.clone()),
            fatal: s.fatal.lock().map_err(|e| e.to_string())?.clone(),
        },
        None => View {
            running: false,
            out_dir: default_out_dir().display().to_string(),
            status: None,
            fatal: None,
        },
    })
}

/// Show a finished recording in Finder or Explorer.
#[tauri::command]
fn reveal(path: String, app: tauri::AppHandle) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let parent = PathBuf::from(&path)
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or(path);
    app.opener()
        .open_path(parent, None::<&str>)
        .map_err(|e| e.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            suggested_out_dir,
            start,
            stop,
            view,
            reveal
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
