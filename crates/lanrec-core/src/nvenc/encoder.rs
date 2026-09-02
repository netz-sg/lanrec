//! Hardware encode of D3D11 textures.
//!
//! The encoder takes BGRA textures straight from capture and lets NVENC do the
//! RGB-to-YUV conversion on the way in. That keeps the whole path on the GPU and
//! gets a real bitstream out of the door; the cost is that the conversion matrix
//! and colour range are the driver's choice, not ours. Once M1 has produced
//! numbers, a compute shader doing the conversion explicitly is the next step --
//! see the note on [`Encoder::new`].

use std::io::Write;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::core::Interface;

use super::{Codec, Nvenc, Session, sys};

/// Where finished frames go.
///
/// The encoder deals in whole frames, not a byte stream: writing a file wants
/// them concatenated, sending them over the wire wants each one framed with its
/// own timestamp and keyframe flag. A sink keeps that decision out of here.
pub trait FrameSink {
    fn frame(&mut self, pts_ns: u64, keyframe: bool, data: &[u8]) -> Result<()>;
}

/// Concatenates frames into an Annex B elementary stream.
pub struct FileSink<W: Write>(pub W);

impl<W: Write> FrameSink for FileSink<W> {
    fn frame(&mut self, _pts_ns: u64, _keyframe: bool, data: &[u8]) -> Result<()> {
        self.0.write_all(data).context("Bitstream schreiben")
    }
}

use crate::d3d::Gpu;
use crate::profile::{BitDepth, Chroma, Profile, RateControl};

pub struct Encoder {
    session: Session,
    ctx: Arc<Mutex<ID3D11DeviceContext>>,
    /// The one texture NVENC reads from. Registering a resource is expensive, so
    /// there is a single input that every frame is copied into, rather than a
    /// registration per captured texture.
    input: ID3D11Texture2D,
    registered: sys::NV_ENC_REGISTERED_PTR,
    bitstream: sys::NV_ENC_OUTPUT_PTR,
    width: u32,
    height: u32,

    pub frames: u64,
    pub bytes: u64,
    /// Largest single encoded frame, which is what a link has to absorb as a
    /// burst rather than as an average.
    pub peak_frame_bytes: u64,
    pub keyframes: u64,
}

impl Encoder {
    /// Configure and open an encoder for `profile`.
    ///
    /// Input is always 8-bit BGRA because that is what the desktop compositor
    /// produces for SDR content. Encoding at 10 bits is still worth it: the extra
    /// precision is used by the *encoder*, which is where banding in gradients
    /// gets introduced.
    pub fn new(nvenc: &Arc<Nvenc>, gpu: &Gpu, profile: &Profile) -> Result<Self> {
        let session = nvenc.open_session(&gpu.device)?;
        let (width, height) = (profile.width, profile.height);

        let codec_guid = match profile.codec {
            Codec::Hevc => sys::NV_ENC_CODEC_HEVC_GUID,
            Codec::Av1 => sys::NV_ENC_CODEC_AV1_GUID,
            Codec::H264 => sys::NV_ENC_CODEC_H264_GUID,
        };
        // P7 is the slowest, highest-quality preset. On Ada it still runs far
        // ahead of 60 fps at this resolution, and the GPU time it costs is time
        // the encoder block would otherwise spend idle.
        let preset_guid = sys::NV_ENC_PRESET_P7_GUID;
        let tuning = sys::NV_ENC_TUNING_INFO_HIGH_QUALITY;

        // Start from the driver's own preset and change only what the profile
        // dictates, rather than filling a config from scratch and silently
        // disagreeing with the driver about the defaults.
        let mut preset = sys::NV_ENC_PRESET_CONFIG {
            version: sys::NV_ENC_PRESET_CONFIG_VER,
            ..Default::default()
        };
        preset.presetCfg.version = sys::NV_ENC_CONFIG_VER;
        unsafe {
            let f = session
                .api()
                .nvEncGetEncodePresetConfigEx
                .context("nvEncGetEncodePresetConfigEx nicht verfuegbar")?;
            session.status(
                f(session.raw(), codec_guid, preset_guid, tuning, &mut preset),
                "nvEncGetEncodePresetConfigEx",
            )?;
        }

        let mut cfg = preset.presetCfg;
        cfg.version = sys::NV_ENC_CONFIG_VER;

        let fps = (profile.fps_num as f32 / profile.fps_den as f32).max(1.0);
        let gop = (fps * profile.gop_seconds).round().max(1.0) as u32;
        cfg.gopLength = gop;
        // frameIntervalP = 1 means IPPP with no B-frames: no reordering, no delay,
        // and at this bitrate B-frames buy nothing visible.
        cfg.frameIntervalP = 1;

        match profile.rate_control {
            RateControl::Cqp { qp } => {
                cfg.rcParams.rateControlMode = sys::NV_ENC_PARAMS_RC_CONSTQP;
                cfg.rcParams.constQP = sys::NV_ENC_QP {
                    qpInterP: qp as u32,
                    qpInterB: qp as u32,
                    qpIntra: qp as u32,
                };
            }
            RateControl::Vbr {
                target_bps,
                max_bps,
            } => {
                cfg.rcParams.rateControlMode = sys::NV_ENC_PARAMS_RC_VBR;
                cfg.rcParams.averageBitRate = target_bps.min(u32::MAX as u64) as u32;
                cfg.rcParams.maxBitRate = max_bps.min(u32::MAX as u64) as u32;
            }
        }

        let ten_bit = profile.depth == BitDepth::Ten;
        let yuv444 = profile.chroma == Chroma::Yuv444;

        if profile.codec == Codec::Hevc {
            // FREXT is the range-extensions profile; Main and Main10 cannot carry
            // 4:4:4 at all.
            cfg.profileGUID = if yuv444 {
                sys::NV_ENC_HEVC_PROFILE_FREXT_GUID
            } else if ten_bit {
                sys::NV_ENC_HEVC_PROFILE_MAIN10_GUID
            } else {
                sys::NV_ENC_HEVC_PROFILE_MAIN_GUID
            };

            // Safety: the union is only ever read as the variant matching the
            // codec GUID set above.
            let hevc = unsafe { &mut cfg.encodeCodecConfig.hevcConfig };
            hevc.set_chromaFormatIDC(if yuv444 { 3 } else { 1 });
            hevc.inputBitDepth = sys::NV_ENC_BIT_DEPTH_8;
            hevc.outputBitDepth = if ten_bit {
                sys::NV_ENC_BIT_DEPTH_10
            } else {
                sys::NV_ENC_BIT_DEPTH_8
            };
            hevc.idrPeriod = gop;
            // Repeat the parameter sets on every keyframe so the stream can be
            // joined mid-flight -- which is exactly what a receiver reconnecting
            // over SRT has to do in M2.
            hevc.set_repeatSPSPPS(1);
        }

        let mut init = sys::NV_ENC_INITIALIZE_PARAMS {
            version: sys::NV_ENC_INITIALIZE_PARAMS_VER,
            encodeGUID: codec_guid,
            presetGUID: preset_guid,
            encodeWidth: width,
            encodeHeight: height,
            darWidth: width,
            darHeight: height,
            maxEncodeWidth: width,
            maxEncodeHeight: height,
            frameRateNum: profile.fps_num,
            frameRateDen: profile.fps_den,
            // Let the encoder decide picture types itself.
            enablePTD: 1,
            encodeConfig: &mut cfg,
            tuningInfo: tuning,
            bufferFormat: sys::NV_ENC_BUFFER_FORMAT_ARGB,
            ..Default::default()
        };
        unsafe {
            let f = session
                .api()
                .nvEncInitializeEncoder
                .context("nvEncInitializeEncoder nicht verfuegbar")?;
            session.status(f(session.raw(), &mut init), "nvEncInitializeEncoder")?;
        }

        let input = create_input_texture(gpu, width, height)?;

        let mut reg = sys::NV_ENC_REGISTER_RESOURCE {
            version: sys::NV_ENC_REGISTER_RESOURCE_VER,
            resourceType: sys::NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX,
            width,
            height,
            // The driver knows the layout of a D3D11 texture; a pitch of 0 tells
            // it to work that out itself.
            pitch: 0,
            resourceToRegister: input.as_raw(),
            bufferFormat: sys::NV_ENC_BUFFER_FORMAT_ARGB,
            bufferUsage: sys::NV_ENC_INPUT_IMAGE,
            ..Default::default()
        };
        unsafe {
            let f = session
                .api()
                .nvEncRegisterResource
                .context("nvEncRegisterResource nicht verfuegbar")?;
            session.status(f(session.raw(), &mut reg), "nvEncRegisterResource")?;
        }

        let mut buf = sys::NV_ENC_CREATE_BITSTREAM_BUFFER {
            version: sys::NV_ENC_CREATE_BITSTREAM_BUFFER_VER,
            ..Default::default()
        };
        unsafe {
            let f = session
                .api()
                .nvEncCreateBitstreamBuffer
                .context("nvEncCreateBitstreamBuffer nicht verfuegbar")?;
            session.status(f(session.raw(), &mut buf), "nvEncCreateBitstreamBuffer")?;
        }

        Ok(Self {
            session,
            ctx: gpu.context_handle(),
            input,
            registered: reg.registeredResource,
            bitstream: buf.bitstreamBuffer,
            width,
            height,
            frames: 0,
            bytes: 0,
            peak_frame_bytes: 0,
            keyframes: 0,
        })
    }

    /// Encode one texture and append the bitstream to `out`.
    ///
    /// `pts_ns` is carried through NVENC and comes back on the encoded frame, so
    /// the muxer downstream never has to reconstruct timing.
    pub fn encode(
        &mut self,
        src: &ID3D11Texture2D,
        pts_ns: u64,
        sink: &mut dyn FrameSink,
    ) -> Result<usize> {
        // GPU-to-GPU. The alternative -- registering every captured texture --
        // costs more than the copy does. The lock is the same one capture holds,
        // so the two threads never touch the immediate context together.
        {
            let ctx = self.ctx.lock().expect("D3D11 context poisoned");
            unsafe { ctx.CopyResource(&self.input, src) };
        }

        let mut map = sys::NV_ENC_MAP_INPUT_RESOURCE {
            version: sys::NV_ENC_MAP_INPUT_RESOURCE_VER,
            registeredResource: self.registered,
            ..Default::default()
        };
        unsafe {
            let f = self
                .session
                .api()
                .nvEncMapInputResource
                .context("nvEncMapInputResource nicht verfuegbar")?;
            self.session
                .status(f(self.session.raw(), &mut map), "nvEncMapInputResource")?;
        }

        let result = self.encode_mapped(&map, pts_ns, sink);

        // Unmap even when the encode failed, or the resource stays locked and
        // every following frame fails too.
        unsafe {
            if let Some(f) = self.session.api().nvEncUnmapInputResource {
                let _ = f(self.session.raw(), map.mappedResource);
            }
        }

        result
    }

    fn encode_mapped(
        &mut self,
        map: &sys::NV_ENC_MAP_INPUT_RESOURCE,
        pts_ns: u64,
        sink: &mut dyn FrameSink,
    ) -> Result<usize> {
        let mut pic = sys::NV_ENC_PIC_PARAMS {
            version: sys::NV_ENC_PIC_PARAMS_VER,
            inputWidth: self.width,
            inputHeight: self.height,
            inputBuffer: map.mappedResource,
            outputBitstream: self.bitstream,
            bufferFmt: sys::NV_ENC_BUFFER_FORMAT_ARGB,
            pictureStruct: sys::NV_ENC_PIC_STRUCT_FRAME,
            inputTimeStamp: pts_ns,
            ..Default::default()
        };

        let status = unsafe {
            let f = self
                .session
                .api()
                .nvEncEncodePicture
                .context("nvEncEncodePicture nicht verfuegbar")?;
            f(self.session.raw(), &mut pic)
        };

        // With no B-frames and no lookahead this should not happen, but if the
        // encoder ever does buffer a frame, treating it as an error would be
        // wrong -- there is simply nothing to write yet.
        if status == sys::NV_ENC_ERR_NEED_MORE_INPUT {
            return Ok(0);
        }
        self.session.status(status, "nvEncEncodePicture")?;

        let n = self.drain(sink)?;
        self.frames += 1;
        Ok(n)
    }

    /// Copy one completed frame out of the bitstream buffer.
    fn drain(&mut self, sink: &mut dyn FrameSink) -> Result<usize> {
        let mut lock = sys::NV_ENC_LOCK_BITSTREAM {
            version: sys::NV_ENC_LOCK_BITSTREAM_VER,
            outputBitstream: self.bitstream,
            ..Default::default()
        };
        unsafe {
            let f = self
                .session
                .api()
                .nvEncLockBitstream
                .context("nvEncLockBitstream nicht verfuegbar")?;
            self.session
                .status(f(self.session.raw(), &mut lock), "nvEncLockBitstream")?;
        }

        let n = lock.bitstreamSizeInBytes as usize;
        // IDR is the only picture type a receiver can start decoding from, which
        // is what the flag on the wire means. A plain I-frame is not enough --
        // it does not reset the reference list.
        let keyframe = lock.pictureType == sys::NV_ENC_PIC_TYPE_IDR;

        let write = if n > 0 && !lock.bitstreamBufferPtr.is_null() {
            // Safety: the driver guarantees this many bytes are readable until
            // the matching unlock.
            let bytes =
                unsafe { std::slice::from_raw_parts(lock.bitstreamBufferPtr as *const u8, n) };
            sink.frame(lock.outputTimeStamp, keyframe, bytes)
        } else {
            Ok(())
        };

        unsafe {
            if let Some(f) = self.session.api().nvEncUnlockBitstream {
                let _ = f(self.session.raw(), self.bitstream);
            }
        }
        write?;

        self.bytes += n as u64;
        self.peak_frame_bytes = self.peak_frame_bytes.max(n as u64);
        if keyframe {
            self.keyframes += 1;
        }
        Ok(n)
    }

    /// Tell the encoder no more frames are coming.
    pub fn finish(&mut self) -> Result<()> {
        let mut pic = sys::NV_ENC_PIC_PARAMS {
            version: sys::NV_ENC_PIC_PARAMS_VER,
            encodePicFlags: sys::NV_ENC_PIC_FLAG_EOS as u32,
            ..Default::default()
        };
        let status = unsafe {
            let f = self
                .session
                .api()
                .nvEncEncodePicture
                .context("nvEncEncodePicture nicht verfuegbar")?;
            f(self.session.raw(), &mut pic)
        };
        self.session.status(status, "nvEncEncodePicture (EOS)")?;

        // Deliberately no drain here. In synchronous mode with no B-frames the
        // bitstream is locked and emptied after every single encode, so nothing
        // is ever left pending -- and nvEncLockBitstream on an already-drained
        // buffer does not return "empty", it blocks forever waiting for output
        // that will never be produced.
        Ok(())
    }

    /// Average bitrate implied by what has been encoded so far.
    pub fn bitrate_bps(&self, fps: f64) -> f64 {
        if self.frames == 0 {
            return 0.0;
        }
        self.bytes as f64 * 8.0 / self.frames as f64 * fps
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // Order matters: the bitstream buffer and the registration belong to the
        // encoder, so both have to go before Session's own Drop destroys it.
        unsafe {
            if let Some(f) = self.session.api().nvEncDestroyBitstreamBuffer {
                let _ = f(self.session.raw(), self.bitstream);
            }
            if let Some(f) = self.session.api().nvEncUnregisterResource {
                let _ = f(self.session.raw(), self.registered);
            }
        }
    }
}

fn create_input_texture(gpu: &Gpu, width: u32, height: u32) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        // Matches NV_ENC_BUFFER_FORMAT_ARGB, which despite the name is
        // word-ordered A8R8G8B8 -- B,G,R,A in memory, exactly BGRA8.
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };

    let mut tex: Option<ID3D11Texture2D> = None;
    unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut tex)) }
        .context("Encoder-Eingangstextur anlegen")?;
    tex.context("CreateTexture2D lieferte keine Textur")
}
