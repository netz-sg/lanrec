//! The D3D11 device everything else hangs off.
//!
//! Capture, encode and the eventual colour conversion all have to run against the
//! *same* device, otherwise textures cannot be shared between them and every frame
//! would need a round trip through system memory -- which is exactly what this
//! design exists to avoid.

use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context, Result};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_SDK_VERSION,
};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter, IDXGIDevice};

pub struct Gpu {
    pub device: ID3D11Device,
    /// The immediate context, behind a lock.
    ///
    /// D3D11 immediate contexts are explicitly not safe for concurrent use, and
    /// there are two users here on different threads: the capture callback runs
    /// on a threadpool thread, the encoder on the caller's. Handing out clones of
    /// the raw context instead of this lock wedges the driver -- no error, no
    /// crash, the process simply stops making progress.
    context: Arc<Mutex<ID3D11DeviceContext>>,
}

impl Gpu {
    pub fn new() -> Result<Self> {
        let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        let mut level = D3D_FEATURE_LEVEL::default();

        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                // BGRA support is required to interoperate with WinRT's
                // IDirect3DDevice, which is how Windows.Graphics.Capture hands
                // frames over.
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut level),
                Some(&mut context),
            )
        }
        .context("D3D11CreateDevice failed")?;

        Ok(Self {
            device: device.context("D3D11CreateDevice returned no device")?,
            context: Arc::new(Mutex::new(
                context.context("D3D11CreateDevice returned no context")?,
            )),
        })
    }

    /// A handle to the shared context, for code that outlives this borrow.
    pub fn context_handle(&self) -> Arc<Mutex<ID3D11DeviceContext>> {
        Arc::clone(&self.context)
    }

    /// Lock the immediate context for a few calls.
    ///
    /// Keep the guard short: every other GPU user waits on it.
    pub fn context(&self) -> MutexGuard<'_, ID3D11DeviceContext> {
        self.context.lock().expect("D3D11 context poisoned")
    }

    /// Marketing name of the adapter the device landed on.
    pub fn adapter_name(&self) -> Result<String> {
        let dxgi: IDXGIDevice = self.device.cast().context("device is not an IDXGIDevice")?;
        let adapter: IDXGIAdapter = unsafe { dxgi.GetAdapter() }.context("GetAdapter failed")?;
        let desc = unsafe { adapter.GetDesc() }.context("GetDesc failed")?;

        let end = desc
            .Description
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(desc.Description.len());
        Ok(String::from_utf16_lossy(&desc.Description[..end]))
    }
}
