//! Safe wrapper around the NVENC API.
//!
//! NVENC is reached through a function-pointer table filled in by
//! `NvEncodeAPICreateInstance`, exported from `nvEncodeAPI64.dll`, which ships
//! with the display driver. Loading it dynamically means the binary runs on
//! machines without an NVIDIA GPU and can say so, instead of failing to start.

pub mod encoder;
pub mod sys;

use std::ffi::{c_void, CStr};
use std::ptr;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use windows::core::{s, Interface};
use windows::Win32::Foundation::{FreeLibrary, HMODULE};
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};

/// Which codecs we care about, keyed off the GUIDs the driver reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

impl Codec {
    fn guid(self) -> sys::GUID {
        match self {
            Codec::H264 => sys::NV_ENC_CODEC_H264_GUID,
            Codec::Hevc => sys::NV_ENC_CODEC_HEVC_GUID,
            Codec::Av1 => sys::NV_ENC_CODEC_AV1_GUID,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Codec::H264 => "H.264",
            Codec::Hevc => "HEVC",
            Codec::Av1 => "AV1",
        }
    }
}

/// What one codec can actually do on this GPU.
///
/// Queried rather than inferred from the model name: chroma and bit-depth support
/// varies across generations in ways that are easy to get wrong from memory (AV1
/// on Ada encodes 4:2:0 only, for instance), and guessing wrong here would offer
/// the user a quality setting that fails at encoder init.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodecCaps {
    pub codec: Codec,
    pub label: &'static str,
    pub yuv444: bool,
    pub ten_bit: bool,
    pub lossless: bool,
    pub max_width: u32,
    pub max_height: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuCaps {
    pub adapter: String,
    /// Number of independent NVENC engines. More than one means a second
    /// recording (or a stream) can run without stealing throughput.
    pub encoder_engines: u32,
    pub codecs: Vec<CodecCaps>,
}

pub struct Nvenc {
    lib: HMODULE,
    api: sys::NV_ENCODE_API_FUNCTION_LIST,
}

impl Nvenc {
    pub fn load() -> Result<Self> {
        let lib = unsafe { LoadLibraryA(s!("nvEncodeAPI64.dll")) }
            .context("nvEncodeAPI64.dll konnte nicht geladen werden -- NVIDIA-Treiber installiert?")?;

        // The driver refuses any call from a client built against a newer API than
        // it implements, so check up front and say which side is behind rather than
        // letting every later call fail with a bare status code.
        let max_supported = unsafe {
            let f = GetProcAddress(lib, s!("NvEncodeAPIGetMaxSupportedVersion"))
                .context("NvEncodeAPIGetMaxSupportedVersion fehlt in nvEncodeAPI64.dll")?;
            let f: unsafe extern "C" fn(*mut u32) -> sys::NVENCSTATUS = std::mem::transmute(f);
            let mut v = 0u32;
            check_status(f(&mut v), None, "NvEncodeAPIGetMaxSupportedVersion")?;
            v
        };

        // NVENCAPI_VERSION packs minor in the high byte; the driver reports the
        // pair as (major << 4) | minor.
        let major = sys::NVENCAPI_VERSION & 0x00FF_FFFF;
        let minor = sys::NVENCAPI_VERSION >> 24;
        let header = (major << 4) | minor;
        if header > max_supported {
            let _ = unsafe { FreeLibrary(lib) };
            bail!(
                "Treiber unterstuetzt NVENC-API {}.{}, gebaut wurde gegen {major}.{minor} -- Treiber aktualisieren",
                max_supported >> 4,
                max_supported & 0xF
            );
        }

        let mut api = sys::NV_ENCODE_API_FUNCTION_LIST {
            version: sys::NV_ENCODE_API_FUNCTION_LIST_VER,
            ..Default::default()
        };
        unsafe {
            let f = GetProcAddress(lib, s!("NvEncodeAPICreateInstance"))
                .context("NvEncodeAPICreateInstance fehlt in nvEncodeAPI64.dll")?;
            let f: unsafe extern "C" fn(
                *mut sys::NV_ENCODE_API_FUNCTION_LIST,
            ) -> sys::NVENCSTATUS = std::mem::transmute(f);
            check_status(f(&mut api), None, "NvEncodeAPICreateInstance")?;
        }

        Ok(Self { lib, api })
    }

    /// Open an encode session bound to a D3D11 device.
    ///
    /// The session must use the same device as capture, so encoder input can stay
    /// a GPU texture.
    ///
    /// Takes an `Arc` rather than a borrow so a session can be stored alongside
    /// other state without infecting everything that holds it with a lifetime.
    pub fn open_session(self: &Arc<Self>, device: &ID3D11Device) -> Result<Session> {
        let mut params = sys::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
            version: sys::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER,
            deviceType: sys::NV_ENC_DEVICE_TYPE_DIRECTX,
            device: device.as_raw(),
            apiVersion: sys::NVENCAPI_VERSION,
            ..Default::default()
        };

        let mut enc: *mut c_void = ptr::null_mut();
        unsafe {
            let f = self
                .api
                .nvEncOpenEncodeSessionEx
                .context("nvEncOpenEncodeSessionEx nicht verfuegbar")?;
            check_status(f(&mut params, &mut enc), None, "nvEncOpenEncodeSessionEx")?;
        }
        if enc.is_null() {
            bail!("nvEncOpenEncodeSessionEx lieferte einen Null-Encoder");
        }

        Ok(Session {
            nvenc: Arc::clone(self),
            enc,
        })
    }
}

impl Drop for Nvenc {
    fn drop(&mut self) {
        let _ = unsafe { FreeLibrary(self.lib) };
    }
}

pub struct Session {
    nvenc: Arc<Nvenc>,
    enc: *mut c_void,
}

impl Session {
    /// The API function table. Every pointer in it is filled by the driver.
    pub(crate) fn api(&self) -> &sys::NV_ENCODE_API_FUNCTION_LIST {
        &self.nvenc.api
    }

    /// Raw encoder handle, for calls this wrapper does not cover.
    pub(crate) fn raw(&self) -> *mut c_void {
        self.enc
    }

    pub(crate) fn status(&self, status: sys::NVENCSTATUS, what: &str) -> Result<()> {
        check_status(status, Some((&self.nvenc.api, self.enc)), what)
    }

    /// Codec GUIDs this GPU advertises, mapped onto the ones we support.
    pub fn codecs(&self) -> Result<Vec<Codec>> {
        let mut count = 0u32;
        unsafe {
            let f = self
                .api()
                .nvEncGetEncodeGUIDCount
                .context("nvEncGetEncodeGUIDCount nicht verfuegbar")?;
            self.check(f(self.enc, &mut count), "nvEncGetEncodeGUIDCount")?;
        }

        let mut guids = vec![sys::GUID::default(); count as usize];
        let mut filled = 0u32;
        unsafe {
            let f = self
                .api()
                .nvEncGetEncodeGUIDs
                .context("nvEncGetEncodeGUIDs nicht verfuegbar")?;
            self.check(
                f(self.enc, guids.as_mut_ptr(), count, &mut filled),
                "nvEncGetEncodeGUIDs",
            )?;
        }
        guids.truncate(filled as usize);

        Ok([Codec::H264, Codec::Hevc, Codec::Av1]
            .into_iter()
            .filter(|c| guids.iter().any(|g| guid_eq(g, &c.guid())))
            .collect())
    }

    fn cap(&self, codec: Codec, which: sys::NV_ENC_CAPS) -> Result<i32> {
        let mut param = sys::NV_ENC_CAPS_PARAM {
            version: sys::NV_ENC_CAPS_PARAM_VER,
            capsToQuery: which,
            ..Default::default()
        };
        let mut value = 0i32;
        unsafe {
            let f = self
                .api()
                .nvEncGetEncodeCaps
                .context("nvEncGetEncodeCaps nicht verfuegbar")?;
            self.check(
                f(self.enc, codec.guid(), &mut param, &mut value),
                "nvEncGetEncodeCaps",
            )?;
        }
        Ok(value)
    }

    pub fn codec_caps(&self, codec: Codec) -> Result<CodecCaps> {
        Ok(CodecCaps {
            codec,
            label: codec.label(),
            yuv444: self.cap(codec, sys::NV_ENC_CAPS_SUPPORT_YUV444_ENCODE)? != 0,
            ten_bit: self.cap(codec, sys::NV_ENC_CAPS_SUPPORT_10BIT_ENCODE)? != 0,
            lossless: self.cap(codec, sys::NV_ENC_CAPS_SUPPORT_LOSSLESS_ENCODE)? != 0,
            max_width: self.cap(codec, sys::NV_ENC_CAPS_WIDTH_MAX)? as u32,
            max_height: self.cap(codec, sys::NV_ENC_CAPS_HEIGHT_MAX)? as u32,
        })
    }

    /// NVENC engine count is a per-GPU property, but the API only exposes it
    /// through a codec query, so it needs some codec to ask about.
    pub fn encoder_engines(&self, codec: Codec) -> Result<u32> {
        Ok(self.cap(codec, sys::NV_ENC_CAPS_NUM_ENCODER_ENGINES)?.max(1) as u32)
    }

    fn check(&self, status: sys::NVENCSTATUS, what: &str) -> Result<()> {
        self.status(status, what)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(f) = self.nvenc.api.nvEncDestroyEncoder {
            let _ = unsafe { f(self.enc) };
        }
    }
}

/// Compare two GUIDs by value.
///
/// Written out rather than derived: bindgen can only add PartialEq to *every*
/// generated type, and the API function table is full of function pointers, whose
/// addresses are not guaranteed unique -- comparing those is meaningless, and the
/// compiler rightly says so 42 times.
fn guid_eq(a: &sys::GUID, b: &sys::GUID) -> bool {
    a.Data1 == b.Data1 && a.Data2 == b.Data2 && a.Data3 == b.Data3 && a.Data4 == b.Data4
}

/// Turn an NVENCSTATUS into an error carrying the driver's own message.
///
/// The status codes alone are close to useless -- INVALID_PARAM covers everything
/// from a wrong struct version to an unsupported resolution -- so pull
/// `nvEncGetLastErrorString` whenever a session exists to ask.
fn check_status(
    status: sys::NVENCSTATUS,
    session: Option<(&sys::NV_ENCODE_API_FUNCTION_LIST, *mut c_void)>,
    what: &str,
) -> Result<()> {
    if status == sys::NV_ENC_SUCCESS {
        return Ok(());
    }

    let detail = session
        .and_then(|(api, enc)| api.nvEncGetLastErrorString.map(|f| (f, enc)))
        .and_then(|(f, enc)| {
            let p = unsafe { f(enc) };
            if p.is_null() {
                None
            } else {
                Some(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
            }
        });

    match detail {
        Some(d) if !d.is_empty() => bail!("{what} fehlgeschlagen (Status {status}): {d}"),
        _ => bail!("{what} fehlgeschlagen (Status {status})"),
    }
}

/// Everything the UI needs to decide which quality settings to offer.
pub fn probe(device: &ID3D11Device, adapter: String) -> Result<GpuCaps> {
    let nvenc = Arc::new(Nvenc::load()?);
    let session = nvenc.open_session(device)?;

    let codecs = session.codecs()?;
    let engines = codecs
        .first()
        .map(|c| session.encoder_engines(*c))
        .transpose()?
        .unwrap_or(0);

    Ok(GpuCaps {
        adapter,
        encoder_engines: engines,
        codecs: codecs
            .into_iter()
            .map(|c| session.codec_caps(c))
            .collect::<Result<_>>()?,
    })
}
