//! Framing the sphere: a `View` (where to look, how wide, which projection),
//! and a `Reframe`, keyframed views over time. The player looks through a
//! `View`; `export` renders a `Reframe`; both turn into a `Camera` for drawing.
//!
//! Angles are in degrees. Yaw turns right, pitch looks up, roll tilts
//! clockwise. With stabilisation on, yaw 0 is straight ahead (`horizon`
//! follows the camera's heading, `lock` holds the clip's first heading), and
//! the equirect's longitude and latitude are the yaw and pitch to look there.

use crate::stitch::{M3, mat_mul};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Projection {
    Perspective,
    LittlePlanet,
    Equirect,
}

impl Projection {
    pub const ALL: [Projection; 3] = [Projection::Perspective, Projection::LittlePlanet, Projection::Equirect];

    /// How far between the perspective (0) and little-planet (1) mappings.
    fn planet(self) -> Option<f64> {
        match self {
            Projection::Perspective => Some(0.0),
            Projection::LittlePlanet => Some(1.0),
            Projection::Equirect => None,
        }
    }
}

impl fmt::Display for Projection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Projection::Perspective => "Perspective",
            Projection::LittlePlanet => "Little planet",
            Projection::Equirect => "Equirect",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct View {
    pub yaw: f64,
    pub pitch: f64,
    pub roll: f64,
    /// Vertical field of view; for the little planet, how much of the
    /// sphere the picture's height spans.
    pub fov: f64,
    pub projection: Projection,
}

/// Opens on the little planet, as DJI Studio does.
impl Default for View {
    fn default() -> Self {
        View::home(Projection::LittlePlanet)
    }
}

impl View {
    /// The starting view for a projection: the planet looks straight down.
    pub fn home(projection: Projection) -> View {
        match projection {
            Projection::LittlePlanet => View { yaw: 0.0, pitch: -90.0, roll: 0.0, fov: 120.0, projection },
            _ => View { yaw: 0.0, pitch: 0.0, roll: 0.0, fov: 90.0, projection },
        }
    }

    pub fn reset(&mut self) {
        *self = View::home(self.projection);
    }

    /// The planet zooms out until the whole sphere is a small disc, the way
    /// DJI Studio's tiny planet does.
    pub fn fov_range(&self) -> (f64, f64) {
        if self.projection == Projection::LittlePlanet { (10.0, 179.5) } else { (20.0, 150.0) }
    }

    /// Zooms by `steps` scroll notches, each scaling the picture by 10%:
    /// near the planet's limit a fixed step in degrees would barely move.
    pub fn zoom_by(&mut self, steps: f64) {
        let (lo, hi) = self.fov_range();
        let half = ((self.fov.to_radians() / 2.0).tan() * 1.1f64.powf(steps)).atan();
        self.fov = (2.0 * half.to_degrees()).clamp(lo, hi);
    }

    pub fn camera(&self) -> Camera {
        Camera { user: self.matrix(), zoom: (self.fov.to_radians() / 2.0).tan(), planet: self.projection.planet() }
    }

    /// view_from_screen-ray: yaw about +y, then pitch about +x, then roll.
    fn matrix(&self) -> M3 {
        let (sy, cy) = self.yaw.to_radians().sin_cos();
        let (sp, cp) = self.pitch.to_radians().sin_cos();
        let (sr, cr) = self.roll.to_radians().sin_cos();
        let ry = [[cy, 0.0, sy], [0.0, 1.0, 0.0], [-sy, 0.0, cy]];
        let rx = [[1.0, 0.0, 0.0], [0.0, cp, -sp], [0.0, sp, cp]];
        let rz = [[cr, -sr, 0.0], [sr, cr, 0.0], [0.0, 0.0, 1.0]];
        mat_mul(ry, mat_mul(rx, rz))
    }
}

/// A view ready to draw.
#[derive(Clone, Copy, Debug)]
pub struct Camera {
    /// view_from_screen-ray rotation.
    pub user: M3,
    /// Tangent of half the vertical field of view.
    pub zoom: f64,
    /// `Some(d)`: an azimuthal mapping from perspective (0) to little planet
    /// (1); in between is how one morphs into the other. `None`: equirect.
    pub planet: Option<f64>,
}

impl Camera {
    /// The view-space ray through screen point `ndc` (−1..1 each way, y down).
    /// The shader's `screen_ray` must match.
    ///
    /// The azimuthal mappings put the ray at angle θ from the view axis where
    /// s = sin θ / (d + cos θ), s being the distance from the centre in units
    /// of `zoom`: d = 0 is perspective (s = tan θ), d = 1 stereographic
    /// (s = tan θ/2), so blending d morphs one into the other smoothly.
    pub fn ray(&self, aspect: f64, [nx, ny]: [f64; 2]) -> [f64; 3] {
        let Some(d) = self.planet else {
            let (lon, lat) = (nx * std::f64::consts::PI, -ny * std::f64::consts::FRAC_PI_2);
            return [lat.cos() * lon.sin(), -lat.sin(), lat.cos() * lon.cos()];
        };
        let q = [nx * aspect * self.zoom, ny * self.zoom];
        let s = (q[0] * q[0] + q[1] * q[1]).sqrt().max(1e-12);
        let th = s.atan() + (s * d / (1.0 + s * s).sqrt()).asin();
        [th.sin() * q[0] / s, th.sin() * q[1] / s, th.cos()]
    }
}

/// A picture shape, written "16:9" in JSON and on the command line.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Aspect(pub u32, pub u32);

impl Aspect {
    /// The size with `short` pixels on the short side, both sides even so
    /// encoders that halve chroma accept it.
    pub fn size(self, short: u32) -> [u32; 2] {
        let long = |a: u32, b: u32| ((short as f64 * a as f64 / b as f64 / 2.0).round() as u32).max(8) * 2;
        if self.0 >= self.1 { [long(self.0, self.1), short] } else { [short, long(self.1, self.0)] }
    }
}

impl std::str::FromStr for Aspect {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let (w, h) = s.split_once(':').ok_or_else(|| anyhow::anyhow!("aspect {s:?} should look like 16:9"))?;
        let (w, h): (u32, u32) = (w.trim().parse()?, h.trim().parse()?);
        if w == 0 || h == 0 || w.max(h) > 8 * w.min(h) {
            bail!("aspect {s:?} is out of range");
        }
        Ok(Aspect(w, h))
    }
}

impl TryFrom<String> for Aspect {
    type Error = anyhow::Error;

    fn try_from(s: String) -> Result<Self> {
        s.parse()
    }
}

impl From<Aspect> for String {
    fn from(a: Aspect) -> String {
        format!("{}:{}", a.0, a.1)
    }
}

/// How a keyframe moves on to the next one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Ease {
    /// Eases out of this keyframe and into the next.
    #[default]
    Smooth,
    /// Constant speed.
    Linear,
    /// Stays put, then cuts to the next keyframe.
    Hold,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Keyframe {
    /// Clip time in seconds.
    pub t: f64,
    #[serde(flatten)]
    pub view: View,
    pub ease: Ease,
}

/// Keyframed views over a stretch of one clip. Between keyframes yaw, pitch
/// and roll move evenly, the zoom changes at an even rate (in tan of half the
/// FOV), and perspective and little planet morph into each other. Yaw isn't
/// wrapped: 0 → 360 is a full turn.
///
/// The JSON form lets a keyframe leave out anything that stays the same:
/// missing fields carry over from the keyframe before, and the first
/// keyframe's from its projection's home view.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawReframe")]
pub struct Reframe {
    pub keyframes: Vec<Keyframe>,
    /// The picture's shape, width:height, if the reframe was made for one
    /// (a 9:16 framing stays 9:16 whatever the resolution).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aspect: Option<Aspect>,
}

impl Reframe {
    pub fn fixed(view: View) -> Reframe {
        Reframe { keyframes: vec![Keyframe { t: 0.0, view, ease: Ease::Hold }], aspect: None }
    }

    /// The span the keyframes cover, if more than one instant.
    pub fn span(&self) -> Option<(f64, f64)> {
        let (a, b) = (self.keyframes.first()?.t, self.keyframes.last()?.t);
        (b > a).then_some((a, b))
    }

    pub fn camera_at(&self, t: f64) -> Camera {
        let k = &self.keyframes;
        let i = k.partition_point(|f| f.t <= t);
        if i == 0 {
            return k[0].view.camera();
        }
        if i == k.len() {
            return k[i - 1].view.camera();
        }
        let (a, b) = (&k[i - 1], &k[i]);
        let x = ((t - a.t) / (b.t - a.t)).clamp(0.0, 1.0);
        let u = match a.ease {
            Ease::Smooth => x * x * (3.0 - 2.0 * x),
            Ease::Linear => x,
            Ease::Hold => return a.view.camera(),
        };
        let lerp = |p: f64, q: f64| p + (q - p) * u;
        let (va, vb) = (a.view, b.view);
        let zoom = |v: View| (v.fov.to_radians() / 2.0).tan().ln();
        let view = View { yaw: lerp(va.yaw, vb.yaw), pitch: lerp(va.pitch, vb.pitch), roll: lerp(va.roll, vb.roll), ..va };
        let planet = match (va.projection.planet(), vb.projection.planet()) {
            (Some(p), Some(q)) => Some(lerp(p, q)),
            // Equirect has no in-between: cut half way.
            _ => if u < 0.5 { va } else { vb }.projection.planet(),
        };
        Camera { user: view.matrix(), zoom: lerp(zoom(va), zoom(vb)).exp(), planet }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawReframe {
    keyframes: Vec<RawKeyframe>,
    #[serde(default)]
    aspect: Option<Aspect>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawKeyframe {
    t: f64,
    yaw: Option<f64>,
    pitch: Option<f64>,
    roll: Option<f64>,
    fov: Option<f64>,
    projection: Option<Projection>,
    ease: Option<Ease>,
}

impl TryFrom<RawReframe> for Reframe {
    type Error = anyhow::Error;

    fn try_from(raw: RawReframe) -> Result<Self> {
        if raw.keyframes.is_empty() {
            bail!("a reframe needs at least one keyframe");
        }
        let mut keyframes: Vec<Keyframe> = Vec::with_capacity(raw.keyframes.len());
        for r in raw.keyframes {
            let prev = keyframes.last();
            if prev.is_some_and(|p| r.t < p.t) {
                bail!("keyframe at {} s comes after one at {} s; keep them in time order", r.t, prev.unwrap().t);
            }
            let base = match (prev, r.projection) {
                (None, p) => View::home(p.unwrap_or(Projection::Perspective)),
                (Some(p), proj) => View { projection: proj.unwrap_or(p.view.projection), ..p.view },
            };
            let view = View {
                yaw: r.yaw.unwrap_or(base.yaw),
                pitch: r.pitch.unwrap_or(base.pitch),
                roll: r.roll.unwrap_or(base.roll),
                fov: r.fov.unwrap_or(base.fov),
                projection: base.projection,
            };
            if !(1.0..=359.0).contains(&view.fov) {
                bail!("fov {} at {} s is out of range (1–359°)", view.fov, r.t);
            }
            keyframes.push(Keyframe { t: r.t, view, ease: r.ease.or(prev.map(|p| p.ease)).unwrap_or_default() });
        }
        Ok(Reframe { keyframes, aspect: raw.aspect })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: [f64; 3], b: [f64; 3]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-9)
    }

    #[test]
    fn azimuthal_matches_perspective_and_planet() {
        let cam = |planet| Camera { user: crate::stitch::IDENTITY, zoom: 0.7, planet: Some(planet) };
        let ndc = [0.3, -0.8];
        let (q, z) = ([0.3 * 1.5 * 0.7, -0.8 * 0.7], 0.7);
        let n = (q[0] * q[0] + q[1] * q[1] + 1.0f64).sqrt();
        assert!(close(cam(0.0).ray(1.5, ndc), [q[0] / n, q[1] / n, 1.0 / n]));
        // The old little planet: stereographic on twice the scaled point.
        let p = [ndc[0] * 1.5 * z * 2.0, ndc[1] * z * 2.0];
        let r = (p[0] * p[0] + p[1] * p[1]).sqrt();
        let th = 2.0 * (r / 2.0).atan();
        assert!(close(cam(1.0).ray(1.5, ndc), [th.sin() * p[0] / r, th.sin() * p[1] / r, th.cos()]));
    }

    #[test]
    fn missing_fields_carry_over() {
        let r: Reframe = serde_json::from_str(
            r#"{"aspect": "9:16", "keyframes": [{"t": 1, "yaw": 30, "fov": 70}, {"t": 3, "pitch": 10, "ease": "linear"}, {"t": 5, "projection": "little-planet"}]}"#,
        )
        .unwrap();
        let k = &r.keyframes;
        assert_eq!(k[0].view, View { yaw: 30.0, pitch: 0.0, roll: 0.0, fov: 70.0, projection: Projection::Perspective });
        assert_eq!((k[1].view.yaw, k[1].view.pitch, k[1].view.fov), (30.0, 10.0, 70.0));
        assert_eq!((k[2].view.projection, k[2].ease), (Projection::LittlePlanet, Ease::Linear));
        assert_eq!(r.span(), Some((1.0, 5.0)));
        assert_eq!(r.aspect.map(|a| a.size(1080)), Some([1080, 1920]));
        let back: Reframe = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn interpolates_between_keyframes() {
        let r: Reframe = serde_json::from_str(r#"{"keyframes": [{"t": 0, "yaw": 0, "ease": "linear"}, {"t": 2, "yaw": 90}]}"#).unwrap();
        let mid = View { yaw: 45.0, ..View::home(Projection::Perspective) }.camera();
        assert!(close(r.camera_at(1.0).user[0], mid.user[0]));
        assert!(close(r.camera_at(-1.0).user[0], r.keyframes[0].view.camera().user[0]));
    }
}
