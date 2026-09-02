//! Encode profiles -- what quality gets sent over the wire.
//!
//! A profile is the full description of the stream: codec, chroma, bit depth,
//! geometry, rate control. It is validated against what the GPU actually reports
//! and against the link that has to carry it, so an impossible combination is
//! rejected in the UI rather than at encoder init.

use serde::{Deserialize, Serialize};

use crate::nvenc::{Codec, GpuCaps};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Chroma {
    /// Half horizontal and vertical chroma resolution. Invisible on photographic
    /// content, destructive on HUD text and thin coloured lines.
    Yuv420,
    /// Full chroma resolution.
    Yuv444,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BitDepth {
    Eight,
    /// Worth taking even for SDR content: removes banding in gradients (sky,
    /// smoke, fog) at almost no cost on Ada.
    Ten,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "mode",
    rename_all = "lowercase",
    rename_all_fields = "camelCase"
)]
pub enum RateControl {
    /// Constant quality. Bitrate floats with scene complexity, which is the right
    /// choice when the link has headroom to spare.
    Cqp { qp: u8 },
    /// Capped bitrate. Needed when the link is the binding constraint.
    Vbr { target_bps: u64, max_bps: u64 },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    pub codec: Codec,
    pub chroma: Chroma,
    pub depth: BitDepth,
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    pub rate_control: RateControl,
    /// Keyframe interval. Longer is more efficient; shorter recovers faster from
    /// a lost packet.
    pub gop_seconds: f32,
}

/// Reference point of the bitrate model: HEVC, 4:2:0, 8-bit, QP 20, game content.
const REF_BPP: f64 = 0.30;
const REF_QP: f64 = 20.0;

impl Profile {
    /// The profile this project was designed around.
    pub fn maximum_quality() -> Self {
        Self {
            codec: Codec::Hevc,
            chroma: Chroma::Yuv444,
            depth: BitDepth::Ten,
            width: 2560,
            height: 1440,
            fps_num: 60,
            fps_den: 1,
            rate_control: RateControl::Cqp { qp: 14 },
            gop_seconds: 2.0,
        }
    }

    pub fn pixels_per_second(&self) -> f64 {
        self.width as f64 * self.height as f64 * self.fps_num as f64 / self.fps_den as f64
    }

    /// Rough bitrate estimate, so the UI can show the cost of a setting before
    /// anything is recorded.
    ///
    /// The model is a log-linear fit: each 6 steps of QP roughly halves the
    /// bitrate, with fixed multipliers for chroma, depth and codec. It is good
    /// enough to tell 150 Mbit/s apart from 400, and not good enough to trust to
    /// the megabit -- game content varies far too much. M1 replaces the constants
    /// with numbers measured on real captures.
    pub fn estimated_bps(&self) -> u64 {
        match self.rate_control {
            // A capped mode does what it is told; no need to model it.
            RateControl::Vbr { target_bps, .. } => target_bps,
            RateControl::Cqp { qp } => {
                let quality = 2f64.powf((REF_QP - qp as f64) / 6.0);
                let chroma = match self.chroma {
                    Chroma::Yuv420 => 1.0,
                    Chroma::Yuv444 => 1.55,
                };
                let depth = match self.depth {
                    BitDepth::Eight => 1.0,
                    BitDepth::Ten => 1.12,
                };
                let codec = match self.codec {
                    Codec::Hevc => 1.0,
                    Codec::Av1 => 0.72,
                    Codec::H264 => 1.5,
                };
                (REF_BPP * quality * chroma * depth * codec * self.pixels_per_second()) as u64
            }
        }
    }
}

/// Why a profile cannot be used, or should not be.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Issue {
    /// `true` means the encode would fail outright; `false` is a warning.
    pub blocking: bool,
    pub message: String,
}

/// Check a profile against the hardware and, optionally, the link that has to
/// carry it.
///
/// `link_bps` is the negotiated line rate of the chosen adapter, not a budget --
/// the usable fraction is applied here.
pub fn validate(profile: &Profile, caps: &GpuCaps, link_bps: Option<u64>) -> Vec<Issue> {
    let mut issues = Vec::new();

    let Some(c) = caps.codecs.iter().find(|c| c.codec == profile.codec) else {
        issues.push(Issue {
            blocking: true,
            message: format!(
                "{} wird von dieser GPU nicht unterstuetzt",
                profile.codec.label()
            ),
        });
        return issues;
    };

    if profile.chroma == Chroma::Yuv444 && !c.yuv444 {
        issues.push(Issue {
            blocking: true,
            message: format!(
                "{} kann auf dieser GPU kein 4:4:4 -- 4:2:0 waehlen oder Codec wechseln",
                c.label
            ),
        });
    }
    if profile.depth == BitDepth::Ten && !c.ten_bit {
        issues.push(Issue {
            blocking: true,
            message: format!("{} kann auf dieser GPU kein 10-bit", c.label),
        });
    }
    if profile.width > c.max_width || profile.height > c.max_height {
        issues.push(Issue {
            blocking: true,
            message: format!(
                "{}x{} ueberschreitet das Maximum von {}x{}",
                profile.width, profile.height, c.max_width, c.max_height
            ),
        });
    }

    if let Some(link) = link_bps {
        let usable = (link as f64 * crate::net::USABLE_LINK_FRACTION) as u64;
        let need = profile.estimated_bps();
        if need > usable {
            issues.push(Issue {
                blocking: true,
                message: format!(
                    "geschaetzte {} Mbit/s passen nicht in {} Mbit/s nutzbare Bandbreite",
                    need / 1_000_000,
                    usable / 1_000_000
                ),
            });
        } else if need * 10 > usable * 8 {
            issues.push(Issue {
                blocking: false,
                message: format!(
                    "{} % der Leitung -- wenig Reserve fuer Bitratenspitzen",
                    need * 100 / usable
                ),
            });
        }
    }

    if profile.chroma == Chroma::Yuv420 {
        issues.push(Issue {
            blocking: false,
            message: "4:2:0 halbiert die Chroma-Aufloesung -- HUD-Text und duenne farbige Linien werden matschig".into(),
        });
    }

    issues
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn design_profile_lands_in_the_expected_range() {
        // The whole plan rests on 1440p60 4:4:4 10-bit sitting comfortably inside
        // a gigabit link. If a change to the model breaks that, the plan changes.
        let bps = Profile::maximum_quality().estimated_bps();
        assert!(
            (150_000_000..=400_000_000).contains(&bps),
            "expected 150-400 Mbit/s, got {} Mbit/s",
            bps / 1_000_000
        );
    }

    #[test]
    fn lower_qp_costs_more_bitrate() {
        let mut a = Profile::maximum_quality();
        a.rate_control = RateControl::Cqp { qp: 14 };
        let mut b = Profile::maximum_quality();
        b.rate_control = RateControl::Cqp { qp: 26 };
        assert!(a.estimated_bps() > b.estimated_bps());
    }

    #[test]
    fn six_qp_steps_roughly_halve_the_bitrate() {
        let mut a = Profile::maximum_quality();
        a.rate_control = RateControl::Cqp { qp: 18 };
        let mut b = Profile::maximum_quality();
        b.rate_control = RateControl::Cqp { qp: 24 };
        let ratio = a.estimated_bps() as f64 / b.estimated_bps() as f64;
        assert!((1.9..=2.1).contains(&ratio), "ratio was {ratio}");
    }

    #[test]
    fn chroma_444_costs_more_than_420() {
        let mut a = Profile::maximum_quality();
        a.chroma = Chroma::Yuv444;
        let mut b = Profile::maximum_quality();
        b.chroma = Chroma::Yuv420;
        assert!(a.estimated_bps() > b.estimated_bps());
    }

    #[test]
    fn capped_mode_reports_its_cap() {
        let mut p = Profile::maximum_quality();
        p.rate_control = RateControl::Vbr {
            target_bps: 120_000_000,
            max_bps: 200_000_000,
        };
        assert_eq!(p.estimated_bps(), 120_000_000);
    }
}
