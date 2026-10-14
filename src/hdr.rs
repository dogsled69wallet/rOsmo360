//! HDR video (a phone's HLG or PQ, BT.2020) brought to the SDR picture the
//! rest of the pipeline works in: display-encoded (sRGB) BT.709. The
//! shader's `hdr_to_sdr` mirrors `to_sdr`; change them together.
//!
//! The method is ffmpeg's `zscale=t=linear:npl=100,tonemap=hable,desat=0`
//! (the look the user judged against, 2026-10-14): the picture as a
//! 1000-nit display shows it, colours into BT.709, then Hable's curve on
//! each pixel's brightest channel so colours keep their hue as highlights
//! roll off, 100 nits as SDR white. Encoded as sRGB rather than ffmpeg's
//! BT.709 curve, as the pipeline expects: on a sunny summit frame of the
//! user's phone that came within 5 levels of ffmpeg's on average, and kept
//! the sky blue and the clouds around the sun.

use serde::{Deserialize, Serialize};

/// How a video's brightness is coded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Hdr {
    /// Ordinary SDR video.
    #[default]
    None,
    /// Hybrid log-gamma (`arib-std-b67`): phones, broadcast.
    Hlg,
    /// Perceptual quantiser (`smpte2084`): HDR10, HDR10+, Dolby Vision's base.
    Pq,
}

impl Hdr {
    /// From ffprobe's `color_transfer`.
    pub fn from_transfer(transfer: &str) -> Hdr {
        match transfer.trim() {
            "arib-std-b67" => Hdr::Hlg,
            "smpte2084" => Hdr::Pq,
            _ => Hdr::None,
        }
    }

    /// The shader's code for it (`flat.w`).
    pub fn code(self) -> f32 {
        match self {
            Hdr::None => 0.0,
            Hdr::Hlg => 1.0,
            Hdr::Pq => 2.0,
        }
    }
}

/// The display HLG is shown on, and what's taken for PQ's brightest.
const PEAK_NITS: f32 = 1000.0;
/// SDR white, in nits (ffmpeg's `npl=100`).
const WHITE_NITS: f32 = 100.0;

/// BT.2020 non-constant-luminance Y′CbCr (Y′ 0..1, Cb/Cr −0.5..0.5) to
/// R′G′B′, as the shader does before `to_sdr` (the CPU has ffmpeg do it).
#[cfg(test)]
pub fn bt2020_rgb([y, cb, cr]: [f32; 3]) -> [f32; 3] {
    [y + 1.4746 * cr, y - 0.16455 * cb - 0.57135 * cr, y + 1.8814 * cb]
}

/// A BT.2020 R′G′B′ HDR signal (0..1) as display-encoded sRGB BT.709 (0..1).
pub fn to_sdr(signal: [f32; 3], hdr: Hdr) -> [f32; 3] {
    let e = signal.map(|v| v.clamp(0.0, 1.0));
    let nits = match hdr {
        Hdr::None => return e,
        Hdr::Hlg => {
            // Inverse OETF to scene light, then the OOTF of a 1000-nit
            // display (system gamma 1.2).
            let (a, b, c) = (0.178_832_77, 0.284_668_92, 0.559_910_7);
            let scene = e.map(|v| if v <= 0.5 { v * v / 3.0 } else { (((v - c) / a).exp() + b) / 12.0 });
            let ys = 0.2627 * scene[0] + 0.6780 * scene[1] + 0.0593 * scene[2];
            let k = PEAK_NITS * ys.max(1e-6).powf(0.2);
            scene.map(|v| k * v)
        }
        Hdr::Pq => {
            let (m1, m2, c1, c2, c3) = (0.159_301_76, 78.843_75, 0.835_937_5, 18.851_563, 18.6875);
            e.map(|v| {
                let p = v.powf(1.0 / m2);
                10000.0 * ((p - c1).max(0.0) / (c2 - c3 * p)).powf(1.0 / m1)
            })
        }
    };
    // BT.2020 to BT.709 primaries, in linear light; what falls outside
    // BT.709 is clipped.
    let [r, g, b] = nits.map(|v| v / WHITE_NITS);
    let lin = [
        (1.6605 * r - 0.5876 * g - 0.0728 * b).max(0.0),
        (-0.1246 * r + 1.1329 * g - 0.0083 * b).max(0.0),
        (-0.0182 * r - 0.1006 * g + 1.1187 * b).max(0.0),
    ];
    let sig = lin[0].max(lin[1]).max(lin[2]).max(1e-6);
    let k = hable(sig) / hable(PEAK_NITS / WHITE_NITS) / sig;
    lin.map(|v| srgb((v * k).clamp(0.0, 1.0)))
}

fn hable(x: f32) -> f32 {
    let (a, b, c, d, e, f) = (0.15, 0.50, 0.10, 0.20, 0.02, 0.30);
    (x * (a * x + c * b) + d * e) / (x * (a * x + b) + d * f) - e / f
}

fn srgb(v: f32) -> f32 {
    if v <= 0.003_130_8 { 12.92 * v } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hdr_comes_down_to_sdr() {
        // Black stays black; HLG's peak and PQ's 1000 nits are white.
        assert_eq!(to_sdr([0.0; 3], Hdr::Hlg), [0.0; 3]);
        for v in to_sdr([1.0; 3], Hdr::Hlg).into_iter().chain(to_sdr([0.751_827_5; 3], Hdr::Pq)) {
            assert!((v - 1.0).abs() < 0.01, "{v}");
        }
        // HLG's reference white (75%, 203 nits) is a bright but not blown grey.
        let white = to_sdr([0.75; 3], Hdr::Hlg)[1];
        assert!((0.7..0.95).contains(&white), "{white}");
        // A saturated BT.2020 green stays green.
        let g = to_sdr([0.2, 0.7, 0.2], Hdr::Hlg);
        assert!(g[1] > g[0] + 0.2 && g[1] > g[2] + 0.2, "{g:?}");
        assert_eq!(Hdr::from_transfer("arib-std-b67"), Hdr::Hlg);
        assert_eq!(Hdr::from_transfer("bt709"), Hdr::None);
    }
}
