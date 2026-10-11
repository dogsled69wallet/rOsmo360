//! Colour tracking, for keyframes that follow a subject the way they follow
//! the selfie stick: a friend's jacket, a tent, a dog's harness. `rosmo360
//! track` stitches the clip small and unstabilised, so the picture's
//! directions are the rig's, and finds the colour in each sampled frame.
//!
//! The subject's direction is the mean of its pixels' unit directions, each
//! weighted by the solid angle its equirect pixel covers (cos latitude). So
//! unlike a centroid in picture coordinates, it is right at the poles, where
//! the equirect stretches a subject straight below the camera into a band.

use crate::stab::{Mode, Stabilizer};
use anyhow::{Context, Result, bail};
use std::path::Path;

/// Samples are averaged in the world with Gaussian weights of this σ in
/// seconds. On 0025, following the holder's shirt, a ±0.25 s box average
/// stepped each time a sample (every 0.1 s) came into reach: frame-to-frame
/// change of the aim's speed was 2.09° RMS, against 0.25° for the stick.
/// With σ 0.25 s it is 0.17°; 0.4 s gained little more (0.14°) and would
/// loosen the framing on quick moves.
const SMOOTH: f64 = 0.25;
/// Further than this from any sample the track says nothing (it was lost).
const MAX_GAP: f64 = 1.0;
/// Search this far (degrees) around where the subject was last.
const RADIUS: f64 = 30.0;
/// A match covering less of the sphere than this is noise, not the subject.
const MIN_SHARE: f64 = 2e-5;

/// A colour to look for, from "#rrggbb".
pub struct Colour {
    hue: f64,
    sat: f64,
    val: f64,
    /// Hue tolerance either side, degrees.
    tolerance: f64,
}

impl Colour {
    pub fn parse(s: &str, tolerance: f64) -> Result<Colour> {
        let hex = s.trim().trim_start_matches('#');
        if hex.len() != 6 {
            bail!("colour {s:?} should look like #ff7a1a");
        }
        let c = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).map(|v| v as f64 / 255.0).with_context(|| format!("colour {s:?}"));
        let (hue, sat, val) = hsv([c(0)?, c(2)?, c(4)?]);
        if sat < 0.25 {
            bail!("colour {s:?} is too grey to track; pick a saturated one");
        }
        Ok(Colour { hue, sat, val, tolerance })
    }

    /// Whether display RGB (0..1) is this colour, in hue and roughly in
    /// strength: shade and sun change value and saturation more than hue,
    /// so those may vary, but within limits both ways. With only a lower
    /// one, a dark blue lake on 0037 (#386484) ran into the sky above it,
    /// the same hue but far brighter.
    fn matches(&self, rgb: [f64; 3]) -> bool {
        let (h, s, v) = hsv(rgb);
        let dh = (h - self.hue + 180.0).rem_euclid(360.0) - 180.0;
        dh.abs() <= self.tolerance && s >= (0.5 * self.sat).max(0.25) && v >= (0.4 * self.val).max(0.15) && v <= 1.6 * self.val
    }
}

/// Hue in degrees, saturation and value 0..1.
fn hsv([r, g, b]: [f64; 3]) -> (f64, f64, f64) {
    let max = r.max(g).max(b);
    let d = max - r.min(g).min(b);
    let hue = if d <= 0.0 {
        0.0
    } else if max == r {
        60.0 * ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    (hue, if max > 0.0 { d / max } else { 0.0 }, max)
}

/// Finds the subject in an unstabilised equirect `rgb` (`w` × `w/2`): near
/// `near` (a rig direction) if given, else where the colour is densest.
/// Returns its direction and the share of the sphere it covers.
pub fn find(rgb: &[u8], w: usize, colour: &Colour, near: Option<[f64; 3]>) -> Option<([f64; 3], f64)> {
    let h = w / 2;
    let mut hits = Vec::new();
    for y in 0..h {
        let lat = std::f64::consts::FRAC_PI_2 - (y as f64 + 0.5) / h as f64 * std::f64::consts::PI;
        for x in 0..w {
            let i = (y * w + x) * 3;
            if colour.matches([rgb[i] as f64 / 255.0, rgb[i + 1] as f64 / 255.0, rgb[i + 2] as f64 / 255.0]) {
                let lon = (x as f64 + 0.5) / w as f64 * std::f64::consts::TAU - std::f64::consts::PI;
                hits.push(([lat.cos() * lon.sin(), -lat.sin(), lat.cos() * lon.cos()], lat.cos()));
            }
        }
    }
    // The sphere's area in pixel weights: Σ cos(lat) over every pixel.
    let sphere = w as f64 * h as f64 * 2.0 / std::f64::consts::PI;
    let mut centre = match near {
        Some(d) => d,
        None => densest(&hits)?,
    };
    let cos_r = RADIUS.to_radians().cos();
    let mut share = 0.0;
    // Twice: the mean around the guess, then around that mean.
    for _ in 0..2 {
        let (mut sum, mut weight) = ([0.0; 3], 0.0);
        for (d, wt) in hits.iter().filter(|(d, _)| dot(*d, centre) >= cos_r) {
            for k in 0..3 {
                sum[k] += d[k] * wt;
            }
            weight += wt;
        }
        share = weight / sphere;
        if share < MIN_SHARE {
            return None;
        }
        centre = normalise(sum);
    }
    Some((centre, share))
}

/// The middle of the 10°-cell with the most of the colour.
fn densest(hits: &[([f64; 3], f64)]) -> Option<[f64; 3]> {
    let mut cells = vec![(0.0, [0.0; 3]); 36 * 18];
    for (d, wt) in hits {
        let lon = d[0].atan2(d[2]).to_degrees() + 180.0;
        let lat = (-d[1]).clamp(-1.0, 1.0).asin().to_degrees() + 90.0;
        let c = &mut cells[((lat / 10.0) as usize).min(17) * 36 + ((lon / 10.0) as usize).min(35)];
        c.0 += wt;
        for k in 0..3 {
            c.1[k] += d[k] * wt;
        }
    }
    let best = cells.iter().max_by(|a, b| a.0.total_cmp(&b.0))?;
    (best.0 > 0.0).then(|| normalise(best.1))
}

/// A track as `rosmo360 track` wrote it.
pub struct Track {
    /// (clip time in seconds, rig direction), lost samples left out.
    samples: Vec<(f64, [f64; 3])>,
}

impl Track {
    pub fn load(path: &Path) -> Result<Track> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading track {}", path.display()))?;
        let mut lines = text.lines();
        let header: Vec<&str> = lines.next().unwrap_or_default().split(',').collect();
        let col = |name: &str| header.iter().position(|h| h.trim() == name).with_context(|| format!("track {} has no {name} column", path.display()));
        let (t, x, y, z) = (col("t")?, col("x")?, col("y")?, col("z")?);
        let mut samples = Vec::new();
        for line in lines {
            let f: Vec<&str> = line.split(',').collect();
            let num = |i: usize| f.get(i).and_then(|v| v.trim().parse::<f64>().ok());
            if let (Some(t), Some(x), Some(y), Some(z)) = (num(t), num(x), num(y), num(z)) {
                samples.push((t, [x, y, z]));
            }
        }
        if samples.is_empty() {
            bail!("track {} has no samples", path.display());
        }
        Ok(Track { samples })
    }

    /// Where the subject is in the view at clip time `t` (seconds): its
    /// samples averaged in the world (so the camera's own turning doesn't
    /// smear it) with Gaussian weights over time, so the view glides instead
    /// of stepping each time a sample comes into reach. Nothing more than
    /// `MAX_GAP` from any sample.
    pub fn aim(&self, stab: &Stabilizer, t: f64, readout_ms: f64) -> Option<(f64, f64)> {
        let s = &self.samples;
        let i = s.partition_point(|p| p.0 < t);
        let nearest = [i.checked_sub(1), (i < s.len()).then_some(i)].into_iter().flatten().map(|j| (s[j].0 - t).abs()).fold(f64::INFINITY, f64::min);
        if nearest > MAX_GAP {
            return None;
        }
        let (lo, hi) = (s.partition_point(|p| p.0 < t - 3.0 * SMOOTH), s.partition_point(|p| p.0 <= t + 3.0 * SMOOTH));
        let mut world = [0.0; 3];
        for &(ts, d) in &s[lo..hi] {
            let w = (-0.5 * ((ts - t) / SMOOTH).powi(2)).exp();
            let v = stab.to_world(ts * 1000.0, d);
            for k in 0..3 {
                world[k] += w * v[k];
            }
        }
        if world == [0.0; 3] {
            return None;
        }
        Some(stab.view_angles(t * 1000.0, readout_ms, world))
    }
}

/// A rig direction at `t_ms` as yaw and pitch of a level view looking at
/// it: directions in the world, comparable with `scan`'s heading.
pub fn level_angles(stab: &Stabilizer, t_ms: f64, readout_ms: f64, d: [f64; 3]) -> (f64, f64) {
    let mut level = stab.clone();
    level.set_mode(Mode::Level);
    level.view_angles(t_ms, readout_ms, stab.to_world(t_ms, d))
}

/// The rig direction a level view at (`yaw`, `pitch`) looks along at `t_ms`.
pub fn from_level(stab: &Stabilizer, t_ms: f64, readout_ms: f64, yaw: f64, pitch: f64) -> [f64; 3] {
    let mut level = stab.clone();
    level.set_mode(Mode::Level);
    let (y, p) = (yaw.to_radians(), pitch.to_radians());
    let view = [y.sin() * p.cos(), -p.sin(), y.cos() * p.cos()];
    crate::stitch::apply(level.rig_from_view(t_ms, readout_ms, 1)[0], view)
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn normalise(v: [f64; 3]) -> [f64; 3] {
    let n = dot(v, v).sqrt().max(1e-12);
    v.map(|x| x / n)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An equirect `w` wide with `colour` within `radius`° of `centre`.
    fn picture(w: usize, centre: [f64; 3], radius: f64, colour: [u8; 3]) -> Vec<u8> {
        let h = w / 2;
        let mut rgb = vec![90u8; w * h * 3];
        for y in 0..h {
            let lat = std::f64::consts::FRAC_PI_2 - (y as f64 + 0.5) / h as f64 * std::f64::consts::PI;
            for x in 0..w {
                let lon = (x as f64 + 0.5) / w as f64 * std::f64::consts::TAU - std::f64::consts::PI;
                let d = [lat.cos() * lon.sin(), -lat.sin(), lat.cos() * lon.cos()];
                if dot(d, centre) >= radius.to_radians().cos() {
                    rgb[(y * w + x) * 3..][..3].copy_from_slice(&colour);
                }
            }
        }
        rgb
    }

    #[test]
    fn finds_a_subject_straight_below() {
        let orange = Colour::parse("#e8641e", 18.0).unwrap();
        // Just off straight down (rig +y is down): a smear across the
        // bottom rows of the equirect, but one small disc on the sphere.
        let below = normalise([0.1, 1.0, 0.05]);
        let rgb = picture(512, below, 8.0, [232, 100, 30]);
        let (d, share) = find(&rgb, 512, &orange, None).unwrap();
        assert!(dot(d, below) > (0.5f64).to_radians().cos(), "{d:?}");
        // A cap of 8° covers (1 - cos 8°) / 2 of the sphere.
        assert!((share / ((1.0 - 8f64.to_radians().cos()) / 2.0) - 1.0).abs() < 0.1, "{share}");
    }

    #[test]
    fn keeps_to_the_subject_near_its_last_place() {
        let orange = Colour::parse("#e8641e", 18.0).unwrap();
        let (a, b) = ([0.0, 0.0, 1.0], normalise([1.0, 0.0, 0.0]));
        let mut rgb = picture(512, a, 5.0, [232, 100, 30]);
        // A bigger orange patch 90° away is ignored when we know where to look.
        for (i, px) in picture(512, b, 12.0, [232, 100, 30]).chunks(3).enumerate() {
            if px[0] == 232 {
                rgb[i * 3..][..3].copy_from_slice(px);
            }
        }
        assert!(dot(find(&rgb, 512, &orange, Some(normalise([0.1, 0.0, 1.0]))).unwrap().0, a) > 0.999);
        assert!(dot(find(&rgb, 512, &orange, None).unwrap().0, b) > 0.999);
        // Far brighter, the same hue: the sky over a dark lake.
        let lake = Colour::parse("#386484", 12.0).unwrap();
        assert!(lake.matches([0.22, 0.39, 0.52]) && !lake.matches([0.55, 0.72, 0.95]));
        assert!(Colour::parse("#808080", 18.0).is_err());
    }
}
