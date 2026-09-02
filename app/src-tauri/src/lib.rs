mod preview;
mod send;

use std::path::PathBuf;
use std::sync::Mutex;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use lanrec_core::capture;
use lanrec_core::config::{self, Labels};
use lanrec_core::d3d::Gpu;
use lanrec_core::net::nic::{self, NicView};
use lanrec_core::nvenc::{self, GpuCaps};
use lanrec_core::profile::{self, Issue, Profile};
use lanrec_core::session::SessionStatus;
use serde::{Deserialize, Serialize};
use tauri::State;

use preview::Preview;

struct AppState {
    /// Probing the encoder opens a real NVENC session, so it is done once and
    /// kept -- the answer cannot change while the app is running.
    caps: Mutex<Option<GpuCaps>>,
    labels: Mutex<Labels>,
    labels_path: PathBuf,
    /// `None` while the preview is off. Dropping it stops the thread.
    preview: Mutex<Option<Preview>>,
    /// `None` while nothing is being recorded.
    send: Mutex<Option<send::Session>>,
}

impl AppState {
    fn new() -> Self {
        // A missing APPDATA would be very odd, but it is not worth refusing to
        // start over: fall back to the working directory and carry on unnamed.
        let labels_path = config::labels_path().unwrap_or_else(|_| PathBuf::from("labels.json"));
        let labels = Labels::load(&labels_path);
        Self {
            caps: Mutex::new(None),
            labels: Mutex::new(labels),
            labels_path,
            preview: Mutex::new(None),
            send: Mutex::new(None),
        }
    }
}

// -------------------------------------------------------------- adapters ---

/// Adapters plus their verdict and the user's own names, ready to render.
///
/// The frontend polls this rather than subscribing to an event, because Windows
/// has no single notification that covers link state, negotiated speed and MTU,
/// and the call is cheap enough (one `GetAdaptersAddresses`) that a short poll is
/// simpler than stitching together several change notifications.
#[tauri::command]
fn list_nics(state: State<AppState>) -> Result<Vec<NicView>, String> {
    let labels = state.labels.lock().map_err(|e| e.to_string())?;
    nic::enumerate_view(&labels).map_err(|e| e.to_string())
}

/// Give an adapter a name of the user's own. An empty label clears it.
#[tauri::command]
fn rename_nic(mac: String, label: String, state: State<AppState>) -> Result<(), String> {
    let mut labels = state.labels.lock().map_err(|e| e.to_string())?;
    labels.set(&mac, &label);
    labels.save(&state.labels_path).map_err(|e| e.to_string())
}

// ------------------------------------------------------------- encoder ---

/// What this GPU's encoder can actually do.
///
/// Queried from the driver rather than inferred from the model name, so the UI
/// only ever offers settings that will survive encoder init.
#[tauri::command]
fn gpu_caps(state: State<AppState>) -> Result<GpuCaps, String> {
    let mut slot = state.caps.lock().map_err(|e| e.to_string())?;
    if let Some(cached) = slot.as_ref() {
        return Ok(cached.clone());
    }

    let gpu = Gpu::new().map_err(|e| e.to_string())?;
    let name = gpu.adapter_name().map_err(|e| e.to_string())?;
    let caps = nvenc::probe(&gpu.device, name).map_err(|e| e.to_string())?;

    *slot = Some(caps.clone());
    Ok(caps)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Evaluation {
    estimated_bps: u64,
    issues: Vec<Issue>,
}

/// Cost and viability of a profile, for live feedback while the user drags a slider.
#[tauri::command]
fn evaluate_profile(
    profile: Profile,
    link_bps: Option<u64>,
    state: State<AppState>,
) -> Result<Evaluation, String> {
    let caps = gpu_caps(state)?;
    Ok(Evaluation {
        estimated_bps: profile.estimated_bps(),
        issues: profile::validate(&profile, &caps, link_bps),
    })
}

#[tauri::command]
fn default_profile() -> Profile {
    Profile::maximum_quality()
}

// -------------------------------------------------------------- displays ---

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MonitorView {
    index: usize,
    device: String,
    width: u32,
    height: u32,
    primary: bool,
}

#[tauri::command]
fn list_monitors() -> Result<Vec<MonitorView>, String> {
    let gpu = Gpu::new().map_err(|e| e.to_string())?;
    Ok(capture::monitors(&gpu)
        .map_err(|e| e.to_string())?
        .into_iter()
        .enumerate()
        .map(|(index, m)| MonitorView {
            index,
            device: m.device,
            width: m.width,
            height: m.height,
            primary: m.primary,
        })
        .collect())
}

// --------------------------------------------------------------- preview ---

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PreviewFrame {
    /// `data:image/jpeg;base64,...`, ready to drop into an <img src>.
    data_url: String,
    width: u32,
    height: u32,
}

#[tauri::command]
fn preview_start(monitor: Option<usize>, state: State<AppState>) -> Result<(), String> {
    let mut slot = state.preview.lock().map_err(|e| e.to_string())?;
    // Dropping the old one stops its thread before a new one starts capturing.
    *slot = None;
    *slot = Some(Preview::start(monitor));
    Ok(())
}

#[tauri::command]
fn preview_stop(state: State<AppState>) -> Result<(), String> {
    let mut slot = state.preview.lock().map_err(|e| e.to_string())?;
    *slot = None;
    Ok(())
}

/// Latest preview image, or `None` until the first frame arrives.
///
/// Polled rather than pushed: at ten frames a second an event stream would buy
/// nothing, and a poll cannot outrun the UI.
#[tauri::command]
fn preview_frame(state: State<AppState>) -> Result<Option<PreviewFrame>, String> {
    let slot = state.preview.lock().map_err(|e| e.to_string())?;
    let Some(p) = slot.as_ref() else {
        return Ok(None);
    };
    if let Some(e) = p.error() {
        return Err(e);
    }
    Ok(p.frame().map(|f| PreviewFrame {
        data_url: format!("data:image/jpeg;base64,{}", B64.encode(&f.jpeg)),
        width: f.width,
        height: f.height,
    }))
}

// ------------------------------------------------------------------ sending ---

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SendRequest {
    /// Receiver address, e.g. 10.0.0.2:9000. Ignored when `file` is set.
    to: String,
    /// MAC of the adapter to force the stream onto. Empty leaves the choice to
    /// the routing table.
    via_mac: Option<String>,
    monitor: Option<usize>,
    profile: Profile,
    /// Record locally instead of sending.
    file: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SendView {
    running: bool,
    status: Option<SessionStatus>,
}

#[tauri::command]
fn send_start(req: SendRequest, state: State<AppState>) -> Result<(), String> {
    let session = send::start(
        &req.to,
        req.via_mac.as_deref(),
        req.monitor,
        &req.profile,
        req.file.filter(|f| !f.trim().is_empty()).map(Into::into),
    )
    .map_err(|e| format!("{e:#}"))?;

    let mut slot = state.send.lock().map_err(|e| e.to_string())?;
    // Dropping the old one stops its thread before a new encoder opens.
    *slot = None;
    *slot = Some(session);
    Ok(())
}

/// Ask the pipeline to wind down. It finishes the current frame, closes the
/// timeline and tells the receiver this was a clean end, so this returns before
/// the file is complete -- the UI watches `finished` for that.
#[tauri::command]
fn send_stop(state: State<AppState>) -> Result<(), String> {
    let slot = state.send.lock().map_err(|e| e.to_string())?;
    if let Some(s) = slot.as_ref() {
        s.request_stop();
    }
    Ok(())
}

#[tauri::command]
fn send_view(state: State<AppState>) -> Result<SendView, String> {
    let slot = state.send.lock().map_err(|e| e.to_string())?;
    Ok(match slot.as_ref() {
        Some(s) => SendView {
            running: !s.is_done(),
            status: Some(s.status()),
        },
        None => SendView {
            running: false,
            status: None,
        },
    })
}

/// Forget a finished recording so the panel goes back to its idle state.
#[tauri::command]
fn send_clear(state: State<AppState>) -> Result<(), String> {
    let mut slot = state.send.lock().map_err(|e| e.to_string())?;
    *slot = None;
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(AppState::new())
        .invoke_handler(tauri::generate_handler![
            list_nics,
            rename_nic,
            gpu_caps,
            evaluate_profile,
            default_profile,
            list_monitors,
            preview_start,
            preview_stop,
            preview_frame,
            send_start,
            send_stop,
            send_view,
            send_clear
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
