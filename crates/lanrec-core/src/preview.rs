//! A small live image of what is being captured.
//!
//! Downscaling happens on the GPU via the mip chain: the frame is copied into
//! mip 0 of a texture that has one, `GenerateMips` fills the rest, and only the
//! small mip is read back. Reading a full 1440p frame instead would push about
//! 850 MB/s across the bus for a picture a few hundred pixels wide -- the whole
//! reason this pipeline never touches system memory.
//!
//! The result is JPEG rather than raw pixels because it goes to a web view, and
//! a few kilobytes per frame over the IPC boundary is the difference between a
//! preview that is free and one that is not.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use image::ExtendedColorType;
use image::codecs::jpeg::JpegEncoder;
use windows::Win32::Graphics::Direct3D::D3D11_SRV_DIMENSION_TEXTURE2D;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_RESOURCE_MISC_GENERATE_MIPS, D3D11_SHADER_RESOURCE_VIEW_DESC,
    D3D11_SHADER_RESOURCE_VIEW_DESC_0, D3D11_TEX2D_SRV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_STAGING, ID3D11DeviceContext, ID3D11ShaderResourceView, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

use crate::d3d::Gpu;

/// How far down the mip chain to go for the preview.
///
/// Level 3 turns 2560x1440 into 320x180 -- large enough to see what is happening,
/// small enough that the readback and the JPEG are both trivial.
const PREVIEW_MIP: u32 = 3;

pub struct Downscaler {
    ctx: Arc<Mutex<ID3D11DeviceContext>>,
    /// Full-size texture that owns a mip chain.
    mips: ID3D11Texture2D,
    srv: ID3D11ShaderResourceView,
    /// CPU-readable copy of just the small mip.
    staging: ID3D11Texture2D,
    pub width: u32,
    pub height: u32,
}

impl Downscaler {
    pub fn new(gpu: &Gpu, src_width: u32, src_height: u32) -> Result<Self> {
        let width = (src_width >> PREVIEW_MIP).max(1);
        let height = (src_height >> PREVIEW_MIP).max(1);

        let mips = create_texture(
            gpu,
            &D3D11_TEXTURE2D_DESC {
                Width: src_width,
                Height: src_height,
                // 0 asks D3D11 for the full chain down to 1x1.
                MipLevels: 0,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                // GenerateMips needs both a render target and a shader resource.
                BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: D3D11_RESOURCE_MISC_GENERATE_MIPS.0 as u32,
            },
        )
        .context("Mip-Textur fuer die Vorschau anlegen")?;

        let srv_desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_SRV {
                    MostDetailedMip: 0,
                    MipLevels: u32::MAX,
                },
            },
        };
        let mut srv: Option<ID3D11ShaderResourceView> = None;
        unsafe {
            gpu.device
                .CreateShaderResourceView(&mips, Some(&srv_desc), Some(&mut srv))
        }
        .context("ShaderResourceView anlegen")?;

        let staging = create_texture(
            gpu,
            &D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            },
        )
        .context("Staging-Textur fuer die Vorschau anlegen")?;

        Ok(Self {
            ctx: gpu.context_handle(),
            mips,
            srv: srv.context("CreateShaderResourceView lieferte nichts")?,
            staging,
            width,
            height,
        })
    }

    /// Downscale one frame and return it as JPEG.
    pub fn jpeg(&self, src: &ID3D11Texture2D, quality: u8) -> Result<Vec<u8>> {
        let rgb = self.read_small(src)?;

        let mut out = Vec::with_capacity(16 * 1024);
        JpegEncoder::new_with_quality(&mut out, quality)
            .encode(&rgb, self.width, self.height, ExtendedColorType::Rgb8)
            .context("JPEG kodieren")?;
        Ok(out)
    }

    /// Everything that needs the device context, in one lock.
    fn read_small(&self, src: &ID3D11Texture2D) -> Result<Vec<u8>> {
        let ctx = self.ctx.lock().expect("D3D11 context poisoned");

        unsafe {
            ctx.CopySubresourceRegion(&self.mips, 0, 0, 0, 0, src, 0, None);
            ctx.GenerateMips(&self.srv);
            ctx.CopySubresourceRegion(&self.staging, 0, 0, 0, 0, &self.mips, PREVIEW_MIP, None);
        }

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe { ctx.Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }
            .context("Staging-Textur mappen")?;

        let (w, h) = (self.width as usize, self.height as usize);
        let mut rgb = vec![0u8; w * h * 3];

        // The mapped rows are padded to RowPitch, which is rarely width * 4.
        for y in 0..h {
            let row = unsafe {
                std::slice::from_raw_parts(
                    (mapped.pData as *const u8).add(y * mapped.RowPitch as usize),
                    w * 4,
                )
            };
            for x in 0..w {
                let (b, g, r) = (row[x * 4], row[x * 4 + 1], row[x * 4 + 2]);
                let o = (y * w + x) * 3;
                rgb[o] = r;
                rgb[o + 1] = g;
                rgb[o + 2] = b;
            }
        }

        unsafe { ctx.Unmap(&self.staging, 0) };
        Ok(rgb)
    }
}

fn create_texture(gpu: &Gpu, desc: &D3D11_TEXTURE2D_DESC) -> Result<ID3D11Texture2D> {
    let mut tex: Option<ID3D11Texture2D> = None;
    unsafe { gpu.device.CreateTexture2D(desc, None, Some(&mut tex)) }?;
    tex.context("CreateTexture2D lieferte keine Textur")
}
