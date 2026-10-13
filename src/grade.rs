//! Colour effects on the framed picture: exposure, white balance, tone,
//! saturation, a vignette and opacity, keyframed per cut, plus the edit's
//! fades from and to black.
//!
//! They act on the stitched view, after framing, so the player and both
//! export paths grade the same picture and a crossfade blends two graded
//! cuts. The shader's `grade` mirrors `Grade::apply`; change them together.
//! Exposure and white balance work on light (the display values decoded
//! with a 2.2 gamma), the rest on the display values themselves, as a
//! colourist's controls feel.

use crate::view::Ease;
use serde::{Deserialize, Serialize};

/// The effects' values; the default (all 0, opacity 1) is the picture
/// untouched.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Grade {
    /// Stops, −3..3.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub exposure: f64,
    /// −1..1, each a stop of contrast around mid-grey.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub contrast: f64,
    /// −1..1: pull back or lift the bright and the dark parts.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub highlights: f64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub shadows: f64,
    /// −1..1: cooler to warmer, and green to magenta.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub temperature: f64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub tint: f64,
    /// −1..1: −1 is black and white, 1 double.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub saturation: f64,
    /// −1..1: like saturation, but mostly on dull colours.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub vibrance: f64,
    /// 0..1: how dark the corners go.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub vignette: f64,
    /// 0..1: how much of the picture shows. On the main track what's left
    /// is black, as in a fade; an overlay will show what's beneath.
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub opacity: f64,
    /// 0..1: how far faded to black, from the edit's fades; not keyframed.
    #[serde(skip)]
    pub black: f64,
}

impl Default for Grade {
    fn default() -> Self {
        Grade::from_values(NEUTRAL)
    }
}

fn is_zero(v: &f64) -> bool {
    *v == 0.0
}

fn one() -> f64 {
    1.0
}

fn is_one(v: &f64) -> bool {
    *v == 1.0
}

/// `Grade::values` of the picture untouched.
pub const NEUTRAL: [f64; 10] = [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0];

/// Stops of colour per unit of temperature or tint: at ±1, red and blue
/// are four times apart, enough to neutralise shade or low sun.
const WB_STOPS: f64 = 1.0;

impl Grade {
    pub fn is_neutral(&self) -> bool {
        *self == Grade::default()
    }

    /// The fields as a list, for blending and the panel.
    pub fn values(&self) -> [f64; 10] {
        [self.exposure, self.contrast, self.highlights, self.shadows, self.temperature, self.tint, self.saturation, self.vibrance, self.vignette, self.opacity]
    }

    pub fn from_values(v: [f64; 10]) -> Grade {
        let [exposure, contrast, highlights, shadows, temperature, tint, saturation, vibrance, vignette, opacity] = v;
        Grade { exposure, contrast, highlights, shadows, temperature, tint, saturation, vibrance, vignette, opacity, black: 0.0 }
    }

    /// Grades display-encoded `rgb` (0..1) at screen point `at` (−1..1 each
    /// way) of a picture `aspect` wide to its height.
    pub fn apply(&self, rgb: [f64; 3], at: [f64; 2], aspect: f64) -> [f64; 3] {
        let luma = |c: [f64; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
        // Light: exposure and white balance.
        let gain = 2f64.powf(self.exposure);
        let wb = [2f64.powf(WB_STOPS * self.temperature), 2f64.powf(-WB_STOPS * self.tint), 2f64.powf(-WB_STOPS * self.temperature)];
        let mut c = [0.0; 3];
        for i in 0..3 {
            c[i] = (rgb[i].max(0.0).powf(2.2) * gain * wb[i]).powf(1.0 / 2.2);
        }
        // Tone: highlights and shadows move the brightness, keeping the hue.
        let l = luma(c);
        let smooth = |a: f64, b: f64, x: f64| {
            let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
            t * t * (3.0 - 2.0 * t)
        };
        let lift = 0.25 * (self.shadows * (1.0 - smooth(0.0, 0.6, l)) + self.highlights * smooth(0.4, 1.0, l));
        if l > 1e-4 {
            let k = ((l + lift) / l).max(0.0);
            c = c.map(|v| v * k);
        } else {
            c = c.map(|v| (v + lift).max(0.0));
        }
        let k = 2f64.powf(self.contrast);
        c = c.map(|v| (v - 0.5) * k + 0.5);
        // Colour: saturation, and vibrance most on what's least saturated.
        let l = luma(c);
        let spread = c.iter().fold(f64::MIN, |a, &v| a.max(v)) - c.iter().fold(f64::MAX, |a, &v| a.min(v));
        let s = (1.0 + self.saturation) * (1.0 + self.vibrance * (1.0 - spread.clamp(0.0, 1.0)));
        c = c.map(|v| l + (v - l) * s);
        // The vignette, by distance from the centre (1 at the corners), the
        // opacity over black and the fade to black.
        let r = ((at[0] * aspect).powi(2) + at[1].powi(2)).sqrt() / (aspect * aspect + 1.0).sqrt();
        let keep = (1.0 - self.vignette * smooth(0.3, 1.0, r)) * self.opacity * (1.0 - self.black);
        c.map(|v| (v * keep).clamp(0.0, 1.0))
    }

    /// For the shader: (exposure gain, temperature and tint in stops,
    /// contrast gain),
    /// (highlights, shadows, saturation, vibrance), (vignette, black, 1 if
    /// it does anything, opacity).
    pub fn uniforms(&self) -> [[f32; 4]; 3] {
        let f = |v: f64| v as f32;
        [
            [f(2f64.powf(self.exposure)), f(WB_STOPS * self.temperature), f(WB_STOPS * self.tint), f(2f64.powf(self.contrast))],
            [f(self.highlights), f(self.shadows), f(self.saturation), f(self.vibrance)],
            [f(self.vignette), f(self.black), if self.is_neutral() { 0.0 } else { 1.0 }, f(self.opacity)],
        ]
    }
}

/// The temperature and tint that make `rgb` (display-encoded, of something
/// that should be grey or white) neutral, within −1..1.
pub fn white_balance(rgb: [f64; 3]) -> (f64, f64) {
    let [r, g, b] = rgb.map(|v| v.max(1e-4).powf(2.2));
    // r·2^(sT) = b·2^(−sT), and then g·2^(−s·tint) their geometric mean.
    let temperature = ((b / r).log2() / (2.0 * WB_STOPS)).clamp(-1.0, 1.0);
    let (r, b) = (r * 2f64.powf(WB_STOPS * temperature), b * 2f64.powf(-WB_STOPS * temperature));
    let tint = ((g / (r * b).sqrt()).log2() / WB_STOPS).clamp(-1.0, 1.0);
    (temperature, tint)
}

/// A cut's effects from clip time `t`, eased like camera keyframes into the
/// next.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct EffectKey {
    pub t: f64,
    #[serde(flatten)]
    pub grade: Grade,
    #[serde(default, skip_serializing_if = "is_smooth")]
    pub ease: Ease,
}

fn is_smooth(e: &Ease) -> bool {
    *e == Ease::Smooth
}

/// The effects at clip time `t` from `keys` (in time order): held before
/// the first and after the last, none without any.
pub fn grade_at(keys: &[EffectKey], t: f64) -> Grade {
    let i = keys.partition_point(|k| k.t <= t);
    if i == 0 || i == keys.len() {
        return keys.get(i.saturating_sub(1)).map_or_else(Grade::default, |k| k.grade);
    }
    let (a, b) = (&keys[i - 1], &keys[i]);
    let Some(u) = a.ease.shape(((t - a.t) / (b.t - a.t)).clamp(0.0, 1.0)) else { return a.grade };
    let (va, vb) = (a.grade.values(), b.grade.values());
    Grade::from_values(std::array::from_fn(|k| va[k] + (vb[k] - va[k]) * u))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neutral_leaves_the_picture_and_each_control_moves_it_its_way() {
        let grey = [0.5, 0.5, 0.5];
        let mid = [0.0, 0.0];
        let out = Grade::default().apply(grey, mid, 1.6);
        assert!(out.iter().all(|v| (v - 0.5).abs() < 1e-9), "{out:?}");
        let g = |grade: Grade, c: [f64; 3]| grade.apply(c, mid, 1.6);
        // A stop more light: 2^(1/2.2) brighter in display values.
        assert!((g(Grade { exposure: 1.0, ..Default::default() }, [0.3; 3])[0] - 0.3 * 2f64.powf(1.0 / 2.2)).abs() < 1e-9);
        let warm = g(Grade { temperature: 0.5, ..Default::default() }, grey);
        assert!(warm[0] > warm[1] && warm[1] > warm[2]);
        let bw = g(Grade { saturation: -1.0, ..Default::default() }, [0.8, 0.4, 0.2]);
        assert!((bw[0] - bw[1]).abs() < 1e-9 && (bw[1] - bw[2]).abs() < 1e-9);
        let punchy = g(Grade { contrast: 1.0, ..Default::default() }, [0.75; 3]);
        assert!((punchy[0] - 1.0).abs() < 1e-9);
        // Corners darken, the centre doesn't; a fade takes it all to black.
        let v = Grade { vignette: 1.0, ..Default::default() };
        assert_eq!(v.apply(grey, mid, 1.6), grey);
        assert!(v.apply(grey, [1.0, 1.0], 1.6)[0] < 0.01);
        assert_eq!(Grade { black: 1.0, ..Default::default() }.apply(grey, mid, 1.6), [0.0; 3]);
        // Opacity goes towards black, and its default is the picture whole.
        assert!(Grade::default().opacity == 1.0 && Grade::default().is_neutral());
        assert!((Grade { opacity: 0.25, ..Default::default() }.apply(grey, mid, 1.6)[0] - 0.125).abs() < 1e-9);
    }

    #[test]
    fn white_balance_neutralises_a_colour_cast() {
        let cast = [0.62, 0.55, 0.42];
        let (temperature, tint) = white_balance(cast);
        let out = Grade { temperature, tint, ..Default::default() }.apply(cast, [0.0, 0.0], 1.0);
        assert!((out[0] - out[1]).abs() < 1e-6 && (out[1] - out[2]).abs() < 1e-6, "{out:?}");
    }

    #[test]
    fn effect_keys_ease_and_hold() {
        let key = |t, exposure, ease| EffectKey { t, grade: Grade { exposure, ..Default::default() }, ease };
        let keys = [key(10.0, 0.0, Ease::Linear), key(20.0, 1.0, Ease::Hold), key(30.0, -1.0, Ease::Smooth)];
        assert_eq!(grade_at(&keys, 5.0).exposure, 0.0);
        assert_eq!(grade_at(&keys, 15.0).exposure, 0.5);
        assert_eq!(grade_at(&keys, 25.0).exposure, 1.0);
        assert_eq!(grade_at(&keys, 40.0).exposure, -1.0);
        assert!(grade_at(&[], 3.0).is_neutral());
        // Only what's set is written.
        let json = serde_json::to_string(&key(10.0, 0.5, Ease::Smooth)).unwrap();
        assert_eq!(json, r#"{"t":10.0,"exposure":0.5}"#);
        let old: EffectKey = serde_json::from_str(r#"{"t":1.0,"vignette":0.5}"#).unwrap();
        assert_eq!(old.grade.opacity, 1.0);
        let faded = EffectKey { t: 2.0, grade: Grade { opacity: 0.5, ..Default::default() }, ease: Ease::Smooth };
        assert_eq!(serde_json::to_string(&faded).unwrap(), r#"{"t":2.0,"opacity":0.5}"#);
        // Eased like the rest.
        let keys = [EffectKey { t: 0.0, ..faded }, EffectKey { t: 10.0, grade: Grade::default(), ease: Ease::Linear }];
        assert!((grade_at(&[EffectKey { ease: Ease::Linear, ..keys[0] }, keys[1]], 5.0).opacity - 0.75).abs() < 1e-9);
    }
}
