//! Screen capture via Windows.Graphics.Capture.
//!
//! WGC hands over frames as GPU textures that are already in the compositor's
//! memory, so nothing is read back to the CPU. It also carries a QPC-based
//! timestamp per frame, which is what the pacer needs to map a 165 Hz source onto
//! a fixed output grid.
//!
//! Frames arrive on a threadpool thread and their textures are recycled the
//! moment the handler returns, so each kept frame is copied into a texture we
//! own. That copy stays on the GPU: about 850 MB/s at 1440p60, against roughly
//! 288 GB/s of memory bandwidth on the target card.

use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use windows::core::Interface;
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter, IDXGIDevice, IDXGIOutput, DXGI_ERROR_NOT_FOUND};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

use crate::d3d::Gpu;

/// How many frames the capture queue holds before dropping.
///
/// Small on purpose. If the consumer cannot keep up, the useful response is to
/// drop the newest frame and report it, not to build a latency backlog.
const QUEUE_DEPTH: usize = 3;

/// WGC's own ring of surfaces. Two is the documented minimum; three absorbs a
/// slow handler without stalling the compositor.
const POOL_BUFFERS: i32 = 3;

#[derive(Debug, Clone)]
pub struct MonitorInfo {
    /// Device path, e.g. `\\.\DISPLAY1`.
    pub device: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
    /// Raw HMONITOR, used to build a capture item.
    pub handle: isize,
}

/// Displays currently attached to the desktop, in adapter order.
pub fn monitors(gpu: &Gpu) -> Result<Vec<MonitorInfo>> {
    let dxgi: IDXGIDevice = gpu.device.cast().context("device is not an IDXGIDevice")?;
    let adapter: IDXGIAdapter = unsafe { dxgi.GetAdapter() }.context("GetAdapter failed")?;

    let mut out = Vec::new();
    for i in 0.. {
        let output: IDXGIOutput = match unsafe { adapter.EnumOutputs(i) } {
            Ok(o) => o,
            // The only way to learn how many outputs there are is to walk until
            // the adapter says there are no more.
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => return Err(e).context("EnumOutputs failed"),
        };

        let desc = unsafe { output.GetDesc() }.context("IDXGIOutput::GetDesc failed")?;
        if !desc.AttachedToDesktop.as_bool() {
            continue;
        }

        let end = desc
            .DeviceName
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(desc.DeviceName.len());
        let r = desc.DesktopCoordinates;

        out.push(MonitorInfo {
            device: String::from_utf16_lossy(&desc.DeviceName[..end]),
            width: (r.right - r.left).unsigned_abs(),
            height: (r.bottom - r.top).unsigned_abs(),
            // The primary display is the one whose top-left is the desktop origin.
            primary: r.left == 0 && r.top == 0,
            handle: desc.Monitor.0 as isize,
        });
    }

    Ok(out)
}

/// A GPU texture holding one frame.
///
/// Aliased so callers can hold frames without taking a direct dependency on the
/// windows crate.
pub type Texture = ID3D11Texture2D;

/// One captured frame, in a texture the caller owns.
pub struct CapturedFrame {
    pub texture: ID3D11Texture2D,
    /// QPC-based capture time in nanoseconds, as reported by WGC.
    pub timestamp_ns: u64,
}

pub struct Capture {
    session: GraphicsCaptureSession,
    pool: Direct3D11CaptureFramePool,
    _item: GraphicsCaptureItem,
    rx: Receiver<CapturedFrame>,
    dropped: Arc<Mutex<u64>>,
    pub width: u32,
    pub height: u32,
}

impl Capture {
    /// Start capturing a monitor.
    pub fn monitor(gpu: &Gpu, m: &MonitorInfo) -> Result<Self> {
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
            .context("GraphicsCaptureItem interop factory nicht verfuegbar")?;
        let item: GraphicsCaptureItem = unsafe {
            interop.CreateForMonitor(windows::Win32::Graphics::Gdi::HMONITOR(m.handle as *mut _))
        }
        .context("CreateForMonitor fehlgeschlagen -- Bildschirmaufnahme erlaubt?")?;

        Self::start(gpu, item)
    }

    fn start(gpu: &Gpu, item: GraphicsCaptureItem) -> Result<Self> {
        let size: SizeInt32 = item.Size().context("Groesse des Capture-Ziels")?;
        let (width, height) = (size.Width as u32, size.Height as u32);

        let device = winrt_device(&gpu.device)?;

        // Free-threaded: frames arrive on a threadpool thread rather than needing a
        // dispatcher, so this works in a headless CLI as well as under a window.
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &device,
            // WGC composes to BGRA8. Asking for anything else just makes WGC do
            // the conversion, so take it as-is and convert once, later, on the way
            // into the encoder.
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            POOL_BUFFERS,
            size,
        )
        .context("Direct3D11CaptureFramePool::CreateFreeThreaded fehlgeschlagen")?;

        let (tx, rx) = sync_channel::<CapturedFrame>(QUEUE_DEPTH);
        let dropped = Arc::new(Mutex::new(0u64));

        // The one shared context lock. The encoder holds the same one, so the
        // pool thread and the encode thread cannot be inside the driver at once.
        let ctx = gpu.context_handle();
        let d3d = gpu.device.clone();

        let handler = {
            let dropped = Arc::clone(&dropped);
            TypedEventHandler::<Direct3D11CaptureFramePool, windows::core::IInspectable>::new(
                move |pool, _| {
                    let pool = pool.as_ref().expect("FrameArrived without a frame pool");
                    if let Err(e) = on_frame(pool, &d3d, &ctx, &tx, &dropped) {
                        // A failed frame must not kill the capture session; the
                        // next one usually succeeds (a resize, a mode switch).
                        eprintln!("lanrec: Frame verworfen: {e:#}");
                    }
                    Ok(())
                },
            )
        };

        pool.FrameArrived(&handler)
            .context("FrameArrived-Handler konnte nicht registriert werden")?;

        let session = pool
            .CreateCaptureSession(&item)
            .context("CreateCaptureSession fehlgeschlagen")?;

        // The mouse pointer is composited into the frame by default; a recording
        // of gameplay wants the game, not the cursor.
        let _ = session.SetIsCursorCaptureEnabled(false);
        // Windows 11 draws a yellow border around captured content unless asked
        // not to. Older builds do not know the setting, hence the ignored error.
        let _ = session.SetIsBorderRequired(false);

        session.StartCapture().context("StartCapture fehlgeschlagen")?;

        Ok(Self {
            session,
            pool,
            _item: item,
            rx,
            dropped,
            width,
            height,
        })
    }

    /// Next captured frame, blocking until one arrives.
    ///
    /// On a completely static screen this waits forever, because WGC produces
    /// nothing at all when nothing changes. Anything with a deadline wants
    /// [`Capture::recv_timeout`].
    pub fn recv(&self) -> Result<CapturedFrame> {
        self.rx.recv().context("Capture-Session beendet")
    }

    /// Next captured frame, or `None` if none arrived within `timeout`.
    ///
    /// A timeout is not an error: it is what a motionless screen looks like.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Option<CapturedFrame>> {
        match self.rx.recv_timeout(timeout) {
            Ok(f) => Ok(Some(f)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => bail!("Capture-Session beendet"),
        }
    }

    /// Frames the capture had to discard because the consumer was behind.
    pub fn dropped(&self) -> u64 {
        *self.dropped.lock().expect("dropped counter poisoned")
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

fn on_frame(
    pool: &Direct3D11CaptureFramePool,
    device: &ID3D11Device,
    ctx: &Mutex<ID3D11DeviceContext>,
    tx: &SyncSender<CapturedFrame>,
    dropped: &Mutex<u64>,
) -> Result<()> {
    let frame = pool
        .TryGetNextFrame()
        .context("TryGetNextFrame fehlgeschlagen")?;

    // SystemRelativeTime is QPC-derived, in 100 ns units.
    let timestamp_ns = frame.SystemRelativeTime().context("SystemRelativeTime")?.Duration as u64
        * 100;

    let surface = frame.Surface().context("Frame ohne Surface")?;
    let access: IDirect3DDxgiInterfaceAccess = surface
        .cast()
        .context("Surface exposes no IDirect3DDxgiInterfaceAccess")?;
    let src: ID3D11Texture2D = unsafe { access.GetInterface() }
        .context("Surface haelt keine ID3D11Texture2D")?;

    // WGC recycles this texture as soon as the handler returns, so take a copy we
    // control. GPU to GPU -- nothing crosses the bus.
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { src.GetDesc(&mut desc) };
    desc.Usage = D3D11_USAGE_DEFAULT;
    desc.BindFlags = (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32;
    desc.CPUAccessFlags = 0;
    desc.MiscFlags = 0;

    let mut copy: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut copy)) }
        .context("CreateTexture2D fuer die Frame-Kopie")?;
    let copy = copy.context("CreateTexture2D lieferte keine Textur")?;

    {
        let ctx = ctx.lock().expect("D3D11 context poisoned");
        unsafe { ctx.CopyResource(&copy, &src) };
    }

    match tx.try_send(CapturedFrame {
        texture: copy,
        timestamp_ns,
    }) {
        Ok(()) => {}
        // Consumer is behind. Dropping the newest frame keeps latency bounded;
        // queueing it would only defer the problem and grow the delay.
        Err(TrySendError::Full(_)) => {
            *dropped.lock().expect("dropped counter poisoned") += 1;
        }
        Err(TrySendError::Disconnected(_)) => {}
    }

    Ok(())
}

/// Wrap a D3D11 device as the WinRT device WGC expects.
fn winrt_device(device: &ID3D11Device) -> Result<IDirect3DDevice> {
    let dxgi: IDXGIDevice = device.cast().context("device is not an IDXGIDevice")?;
    let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
        .context("CreateDirect3D11DeviceFromDXGIDevice fehlgeschlagen")?;
    inspectable
        .cast()
        .context("Ergebnis ist kein IDirect3DDevice")
}
