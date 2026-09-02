//! The lanrec wire format.
//!
//! Both ends of the link are ours, so the stream does not need a container. MPEG-TS
//! would force PCR handling, 188-byte padding and a 90 kHz timestamp grid on us;
//! a fixed 32-byte header keeps nanosecond timestamps and leaves room for
//! metadata that a container would have no place for.
//!
//! Everything is little-endian, which is what both ends run natively.
//!
//! # Transport
//!
//! Frames are written to a plain TCP stream. The original plan was SRT, and SRT
//! remains the right answer over a switch or a WAN -- but on a direct cable
//! between two machines there is no competing traffic, no congestion and
//! effectively no loss, so SRT's retransmission machinery buys nothing while
//! costing a C dependency on both platforms. Recording also does not care about
//! the latency spike a TCP retransmit causes; it cares about every byte arriving.
//! The framing is transport-agnostic, so swapping in SRT later touches only the
//! socket.

use std::io::{Read, Write};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// "LANR". Lets the receiver reject anything that is not us before it tries to
/// allocate a payload buffer from a bogus length.
pub const MAGIC: u32 = 0x4C41_4E52;

pub const VERSION: u16 = 1;

pub const HEADER_LEN: usize = 32;

/// Refuse absurd payload lengths outright. A 1440p keyframe runs a few hundred
/// kilobytes; 64 MB is far past anything legitimate and stops a corrupt or
/// hostile length from turning into a huge allocation.
pub const MAX_PAYLOAD: u32 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum Kind {
    /// Stream description, sent once before any media.
    Info = 0,
    Video = 1,
    /// Reserved for M3.
    Audio = 2,
    /// Sender is done; the receiver can close the file.
    End = 3,
}

impl Kind {
    fn from_u16(v: u16) -> Result<Self> {
        Ok(match v {
            0 => Kind::Info,
            1 => Kind::Video,
            2 => Kind::Audio,
            3 => Kind::End,
            other => bail!("unbekannter Frame-Typ {other}"),
        })
    }
}

/// Set on video frames that can be decoded without anything before them.
pub const FLAG_KEYFRAME: u32 = 1 << 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub kind: Kind,
    pub pts_ns: u64,
    pub dts_ns: u64,
    pub flags: u32,
    pub len: u32,
}

impl Header {
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut b = [0u8; HEADER_LEN];
        b[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        b[4..6].copy_from_slice(&VERSION.to_le_bytes());
        b[6..8].copy_from_slice(&(self.kind as u16).to_le_bytes());
        b[8..16].copy_from_slice(&self.pts_ns.to_le_bytes());
        b[16..24].copy_from_slice(&self.dts_ns.to_le_bytes());
        b[24..28].copy_from_slice(&self.flags.to_le_bytes());
        b[28..32].copy_from_slice(&self.len.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8; HEADER_LEN]) -> Result<Self> {
        let magic = u32::from_le_bytes(b[0..4].try_into().unwrap());
        if magic != MAGIC {
            bail!("kein lanrec-Strom (Magic {magic:#010x})");
        }
        let version = u16::from_le_bytes(b[4..6].try_into().unwrap());
        if version != VERSION {
            bail!("Protokollversion {version}, erwartet {VERSION} -- beide Seiten aktualisieren");
        }

        let len = u32::from_le_bytes(b[28..32].try_into().unwrap());
        if len > MAX_PAYLOAD {
            bail!("Nutzlast von {len} Bytes ist unplausibel -- Strom vermutlich beschaedigt");
        }

        Ok(Self {
            kind: Kind::from_u16(u16::from_le_bytes(b[6..8].try_into().unwrap()))?,
            pts_ns: u64::from_le_bytes(b[8..16].try_into().unwrap()),
            dts_ns: u64::from_le_bytes(b[16..24].try_into().unwrap()),
            flags: u32::from_le_bytes(b[24..28].try_into().unwrap()),
            len,
        })
    }

    pub fn is_keyframe(&self) -> bool {
        self.flags & FLAG_KEYFRAME != 0
    }
}

/// What the receiver needs to know before the first media frame.
///
/// Sent as JSON rather than packed fields: it is written once per connection, so
/// the size does not matter, and a new field must not break an older receiver.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamInfo {
    /// Lowercase codec name, e.g. "hevc".
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    /// "yuv444" or "yuv420".
    pub chroma: String,
    pub bit_depth: u8,
    /// Free-form, for the receiver to show and to name the file.
    pub source: String,
    /// Sender version, so a mismatch is visible in logs.
    pub sender: String,
}

impl StreamInfo {
    /// File extension for a raw elementary stream of this codec.
    pub fn extension(&self) -> &'static str {
        match self.codec.as_str() {
            "hevc" => "hevc",
            "av1" => "obu",
            _ => "h264",
        }
    }
}

/// Write one framed message.
pub fn write_frame(
    out: &mut impl Write,
    kind: Kind,
    pts_ns: u64,
    flags: u32,
    payload: &[u8],
) -> Result<()> {
    let header = Header {
        kind,
        pts_ns,
        // No B-frames anywhere in this pipeline, so decode order is presentation
        // order and DTS is simply PTS.
        dts_ns: pts_ns,
        flags,
        len: payload.len() as u32,
    };
    out.write_all(&header.encode()).context("Header senden")?;
    out.write_all(payload).context("Nutzlast senden")?;
    Ok(())
}

pub fn write_info(out: &mut impl Write, info: &StreamInfo) -> Result<()> {
    let json = serde_json::to_vec(info).context("StreamInfo serialisieren")?;
    write_frame(out, Kind::Info, 0, 0, &json)
}

pub fn write_end(out: &mut impl Write) -> Result<()> {
    write_frame(out, Kind::End, 0, 0, &[])
}

/// Read one framed message, appending the payload into `buf`.
///
/// `buf` is reused across calls so a long recording does not allocate per frame.
/// Returns `None` at a clean end of stream.
pub fn read_frame(input: &mut impl Read, buf: &mut Vec<u8>) -> Result<Option<Header>> {
    let mut raw = [0u8; HEADER_LEN];
    match read_exact_or_eof(input, &mut raw)? {
        false => return Ok(None),
        true => {}
    }

    let header = Header::decode(&raw)?;
    buf.clear();
    buf.resize(header.len as usize, 0);
    if header.len > 0 {
        input
            .read_exact(buf)
            .context("Nutzlast unvollstaendig -- Verbindung abgebrochen?")?;
    }
    Ok(Some(header))
}

/// `Ok(false)` means the stream ended cleanly on a frame boundary.
fn read_exact_or_eof(input: &mut impl Read, buf: &mut [u8]) -> Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match input.read(&mut buf[filled..]) {
            Ok(0) if filled == 0 => return Ok(false),
            Ok(0) => bail!("Verbindung mitten im Header abgebrochen ({filled} von {} Bytes)", buf.len()),
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e).context("Header lesen"),
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trips() {
        let h = Header {
            kind: Kind::Video,
            pts_ns: 16_666_667,
            dts_ns: 16_666_667,
            flags: FLAG_KEYFRAME,
            len: 4242,
        };
        assert_eq!(Header::decode(&h.encode()).unwrap(), h);
    }

    #[test]
    fn header_is_exactly_32_bytes() {
        // The receiver reads a fixed-size header before it knows anything else.
        assert_eq!(
            Header {
                kind: Kind::Video,
                pts_ns: 0,
                dts_ns: 0,
                flags: 0,
                len: 0
            }
            .encode()
            .len(),
            32
        );
    }

    #[test]
    fn foreign_data_is_rejected() {
        let mut b = [0u8; HEADER_LEN];
        b[0..4].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        assert!(Header::decode(&b).unwrap_err().to_string().contains("Magic"));
    }

    #[test]
    fn version_mismatch_is_named_not_guessed() {
        let mut b = Header {
            kind: Kind::Video,
            pts_ns: 0,
            dts_ns: 0,
            flags: 0,
            len: 0,
        }
        .encode();
        b[4..6].copy_from_slice(&99u16.to_le_bytes());
        let msg = Header::decode(&b).unwrap_err().to_string();
        assert!(msg.contains("99"), "{msg}");
    }

    #[test]
    fn absurd_length_is_refused_before_allocating() {
        let mut b = Header {
            kind: Kind::Video,
            pts_ns: 0,
            dts_ns: 0,
            flags: 0,
            len: 0,
        }
        .encode();
        b[28..32].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(Header::decode(&b).is_err());
    }

    #[test]
    fn frames_round_trip_through_a_stream() {
        let mut wire = Vec::new();
        let info = StreamInfo {
            codec: "hevc".into(),
            width: 2560,
            height: 1440,
            fps_num: 60,
            fps_den: 1,
            chroma: "yuv444".into(),
            bit_depth: 10,
            source: r"\\.\DISPLAY1".into(),
            sender: "test".into(),
        };
        write_info(&mut wire, &info).unwrap();
        write_frame(&mut wire, Kind::Video, 16_666_667, FLAG_KEYFRAME, &[1, 2, 3]).unwrap();
        write_frame(&mut wire, Kind::Video, 33_333_334, 0, &[4, 5]).unwrap();
        write_end(&mut wire).unwrap();

        let mut r = wire.as_slice();
        let mut buf = Vec::new();

        let h = read_frame(&mut r, &mut buf).unwrap().unwrap();
        assert_eq!(h.kind, Kind::Info);
        let back: StreamInfo = serde_json::from_slice(&buf).unwrap();
        assert_eq!(back.width, 2560);
        assert_eq!(back.bit_depth, 10);

        let h = read_frame(&mut r, &mut buf).unwrap().unwrap();
        assert!(h.is_keyframe());
        assert_eq!(buf, [1, 2, 3]);
        assert_eq!(h.pts_ns, 16_666_667);

        let h = read_frame(&mut r, &mut buf).unwrap().unwrap();
        assert!(!h.is_keyframe());
        assert_eq!(buf, [4, 5]);

        assert_eq!(read_frame(&mut r, &mut buf).unwrap().unwrap().kind, Kind::End);
        assert!(read_frame(&mut r, &mut buf).unwrap().is_none());
    }

    #[test]
    fn truncation_mid_header_is_an_error_not_a_clean_end() {
        // A clean end can only happen on a frame boundary. Anything else means the
        // recording is incomplete and the user needs to know.
        let mut wire = Vec::new();
        write_frame(&mut wire, Kind::Video, 0, 0, &[1, 2, 3]).unwrap();
        wire.truncate(HEADER_LEN - 4);

        let mut r = wire.as_slice();
        assert!(read_frame(&mut r, &mut Vec::new()).is_err());
    }

    #[test]
    fn truncated_payload_is_an_error() {
        let mut wire = Vec::new();
        write_frame(&mut wire, Kind::Video, 0, 0, &[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        wire.truncate(HEADER_LEN + 3);

        let mut r = wire.as_slice();
        assert!(read_frame(&mut r, &mut Vec::new()).is_err());
    }
}
