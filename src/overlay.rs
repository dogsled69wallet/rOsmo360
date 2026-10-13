//! The overlay track: pictures drawn over the edit's picture, after its
//! grade, each moved, sized and turned by its own keyframes and graded by
//! its own effects, its opacity letting the picture beneath show.
//!
//! Items sit at times on the edit, like music, so recutting the video
//! leaves them where they are; their keyframes are in their own clip time,
//! so moving an item keeps its moves. The project holds a list of lanes
//! (the first lowest); the player shows one. The edit's fades cover the
//! overlays too. Clips export leaves them out: another editor does them.

use crate::ffmpeg::{self, FrameSink};
use crate::grade::{EffectKey, Grade, grade_at};
use crate::stitch::Frame;
use crate::view::Ease;
use anyhow::{Context, Result};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// One overlay lane: items one after another, never two at once.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lane {
    #[serde(default)]
    pub items: Vec<Item>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Item {
    /// The picture; a relative path is relative to the project file.
    pub input: PathBuf,
    /// Where it starts on the edit, in seconds.
    pub at: f64,
    /// In and out points in its own time (a picture's start at 0).
    #[serde(rename = "in")]
    pub start: f64,
    #[serde(rename = "out")]
    pub end: f64,
    /// Where it is on the frame, in its own time, in time order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub motion: Vec<MotionKey>,
    /// Its effects, in its own time; its opacity is how much shows.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<EffectKey>,
}

impl Item {
    /// Where it ends on the edit.
    pub fn ends(&self) -> f64 {
        self.at + self.end - self.start
    }

    /// Its own time at edit time `t`.
    pub fn local(&self, t: f64) -> f64 {
        self.start + t - self.at
    }
}

/// Where an item is: its centre on the frame (0..1 across and down), its
/// size (1 fits the whole frame, keeping its shape) and its turn.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Place {
    #[serde(default = "half")]
    pub x: f64,
    #[serde(default = "half")]
    pub y: f64,
    #[serde(default = "one")]
    pub size: f64,
    /// Degrees clockwise.
    #[serde(default)]
    pub rotation: f64,
}

fn half() -> f64 {
    0.5
}

fn one() -> f64 {
    1.0
}

impl Default for Place {
    /// Centred, filling as much of the frame as its shape allows.
    fn default() -> Self {
        Place { x: 0.5, y: 0.5, size: 1.0, rotation: 0.0 }
    }
}

impl Place {
    pub fn values(&self) -> [f64; 4] {
        [self.x, self.y, self.size, self.rotation]
    }

    pub fn from_values([x, y, size, rotation]: [f64; 4]) -> Place {
        Place { x, y, size, rotation }
    }
}

/// Where an item is from its own time `t`, eased like camera keyframes.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct MotionKey {
    pub t: f64,
    #[serde(flatten)]
    pub place: Place,
    #[serde(default, skip_serializing_if = "is_smooth")]
    pub ease: Ease,
}

fn is_smooth(e: &Ease) -> bool {
    *e == Ease::Smooth
}

/// The place at own time `t` from `keys` (in time order): held before the
/// first and after the last, the default without any.
pub fn place_at(keys: &[MotionKey], t: f64) -> Place {
    let i = keys.partition_point(|k| k.t <= t);
    if i == 0 || i == keys.len() {
        return keys.get(i.saturating_sub(1)).map_or_else(Place::default, |k| k.place);
    }
    let (a, b) = (&keys[i - 1], &keys[i]);
    let Some(u) = a.ease.shape(((t - a.t) / (b.t - a.t)).clamp(0.0, 1.0)) else { return a.place };
    let (va, vb) = (a.place.values(), b.place.values());
    Place::from_values(std::array::from_fn(|k| va[k] + (vb[k] - va[k]) * u))
}

impl Lane {
    /// The item showing at edit time `t`, if any.
    pub fn at(&self, t: f64) -> Option<(usize, &Item)> {
        self.items.iter().enumerate().find(|(_, it)| t >= it.at && t < it.ends())
    }

    /// The start nearest `want` where item `i` fits without overlapping the
    /// others: pushed into one, it stops edge to edge, and hops past it once
    /// `want` is nearer the far side (as music clips do).
    pub fn place(&self, i: usize, want: f64) -> f64 {
        let len = self.items[i].end - self.items[i].start;
        let others = || self.items.iter().enumerate().filter(move |&(j, _)| j != i).map(|(_, c)| c);
        let free = |at: f64| at >= 0.0 && others().all(|c| at + len <= c.at + 1e-6 || at >= c.ends() - 1e-6);
        std::iter::once(want.max(0.0))
            .chain(others().flat_map(|c| [c.ends(), c.at - len]))
            .filter(|&at| free(at))
            .min_by(|a, b| (a - want).abs().total_cmp(&(b - want).abs()))
            .unwrap_or_else(|| others().map(Item::ends).fold(0.0, f64::max))
    }

    /// The edit times item `i` may stretch to before meeting a neighbour:
    /// (earliest start, latest end).
    pub fn room(&self, i: usize) -> (f64, f64) {
        let me = &self.items[i];
        let others = self.items.iter().enumerate().filter(|&(j, _)| j != i).map(|(_, c)| c);
        let (mut lo, mut hi) = (0.0f64, f64::INFINITY);
        for c in others {
            if c.ends() <= me.at + 1e-6 {
                lo = lo.max(c.ends());
            } else if c.at >= me.ends() - 1e-6 {
                hi = hi.min(c.at);
            }
        }
        (lo, hi)
    }
}

/// A picture to draw: premultiplied RGBA, upright.
pub struct Picture {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

impl std::fmt::Debug for Picture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Picture({}×{})", self.width, self.height)
    }
}

impl Picture {
    /// Reads a picture, over nothing where it's transparent (premultiplied,
    /// so its edges filter cleanly), scaled down to `most` pixels on its
    /// long side if it's bigger: sampled without mipmaps, a picture much
    /// bigger than it's drawn would shimmer.
    pub fn load(input: &Path, most: usize) -> Result<Picture> {
        let flat = ffmpeg::flat(input)?.with_context(|| format!("{} is a 360 clip, not a picture", input.display()))?;
        let (w, h) = flat.size();
        let k = (most as f64 / w.max(h) as f64).min(1.0);
        let size = (((w as f64 * k).round() as u32).max(1), ((h as f64 * k).round() as u32).max(1));
        let rgba = ffmpeg::picture_rgba(input, flat, size)?;
        Ok(Picture { width: size.0 as usize, height: size.1 as usize, rgba })
    }

    /// Premultiplied RGBA 0..1 at picture point (`u`, `v`) in pixels,
    /// bilinear; nothing outside.
    fn sample(&self, u: f64, v: f64) -> [f64; 4] {
        let (w, h) = (self.width as f64, self.height as f64);
        if u < 0.0 || v < 0.0 || u > w || v > h {
            return [0.0; 4];
        }
        let (x, y) = ((u - 0.5).clamp(0.0, w - 1.0), (v - 0.5).clamp(0.0, h - 1.0));
        let (x0, y0) = (x.floor() as usize, y.floor() as usize);
        let (x1, y1) = ((x0 + 1).min(self.width - 1), (y0 + 1).min(self.height - 1));
        let (fx, fy) = (x - x0 as f64, y - y0 as f64);
        let px = |x: usize, y: usize, c: usize| self.rgba[(y * self.width + x) * 4 + c] as f64 / 255.0;
        std::array::from_fn(|c| {
            let top = px(x0, y0, c) * (1.0 - fx) + px(x1, y0, c) * fx;
            let bottom = px(x0, y1, c) * (1.0 - fx) + px(x1, y1, c) * fx;
            top * (1.0 - fy) + bottom * fy
        })
    }
}

/// An item as drawn on a frame: where its centre is (pixels), its half
/// size (pixels) and its turn (radians clockwise).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Quad {
    pub centre: [f64; 2],
    pub half: [f64; 2],
    pub turn: f64,
}

impl Quad {
    /// `place` for a picture `picture` big (its shape) on a frame `frame` big.
    pub fn new(place: &Place, picture: (usize, usize), frame: (usize, usize)) -> Quad {
        let (pw, ph) = (picture.0.max(1) as f64, picture.1.max(1) as f64);
        let (fw, fh) = (frame.0 as f64, frame.1 as f64);
        let fit = (fw / pw).min(fh / ph) * place.size.max(0.0);
        Quad { centre: [place.x * fw, place.y * fh], half: [pw * fit / 2.0, ph * fit / 2.0], turn: place.rotation.to_radians() }
    }

    /// Where frame pixel centre (`x`, `y`) falls on the item: −1..1 each way
    /// inside it.
    pub fn to_item(self, x: f64, y: f64) -> [f64; 2] {
        let (dx, dy) = (x - self.centre[0], y - self.centre[1]);
        let (s, c) = self.turn.sin_cos();
        // Undo the clockwise turn (y down).
        let (u, v) = (c * dx + s * dy, -s * dx + c * dy);
        [u / self.half[0].max(1e-9), v / self.half[1].max(1e-9)]
    }

    /// The frame pixels it can touch: (x0, y0, x1, y1), clipped to the frame.
    fn bounds(&self, (w, h): (usize, usize)) -> (usize, usize, usize, usize) {
        let (s, c) = self.turn.sin_cos();
        let ex = (c * self.half[0]).abs() + (s * self.half[1]).abs();
        let ey = (s * self.half[0]).abs() + (c * self.half[1]).abs();
        let clip = |v: f64, n: usize| v.clamp(0.0, n as f64) as usize;
        (clip((self.centre[0] - ex).floor(), w), clip((self.centre[1] - ey).floor(), h), clip((self.centre[0] + ex).ceil() + 1.0, w), clip((self.centre[1] + ey).ceil() + 1.0, h))
    }
}

/// Draws `picture` over `frame` as `quad`, graded by `grade` (its opacity
/// its see-through, the vignette across the item) and faded to black by
/// `black`, as the main picture under it is.
pub fn composite(frame: &mut Frame, picture: &Picture, quad: &Quad, grade: &Grade, black: f64) {
    let (w, h) = (frame.width, frame.height);
    let (x0, y0, x1, y1) = quad.bounds((w, h));
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let shade = Grade { opacity: 1.0, black: 0.0, ..*grade };
    let neutral = shade.is_neutral();
    let aspect = quad.half[0] / quad.half[1].max(1e-9);
    let (pw, ph) = (picture.width as f64, picture.height as f64);
    frame.rgb.par_chunks_mut(w * 3).enumerate().skip(y0).take(y1 - y0).for_each(|(y, row)| {
        for x in x0..x1 {
            let [u, v] = quad.to_item(x as f64 + 0.5, y as f64 + 0.5);
            if u.abs() > 1.0 || v.abs() > 1.0 {
                continue;
            }
            let p = picture.sample((u + 1.0) / 2.0 * pw, (v + 1.0) / 2.0 * ph);
            let a = p[3] * grade.opacity;
            if a <= 0.0 {
                continue;
            }
            // Graded as a picture of its own: straight colour.
            let straight = [p[0], p[1], p[2]].map(|c| c / p[3].max(1e-9));
            let c = if neutral { straight } else { shade.apply(straight, [u, v], aspect) };
            let px = &mut row[x * 3..x * 3 + 3];
            for k in 0..3 {
                let under = px[k] as f64 / 255.0;
                px[k] = ((c[k] * (1.0 - black) * a + under * (1.0 - a)) * 255.0).round().clamp(0.0, 255.0) as u8;
            }
        }
    });
}

/// What an item looks like at edit time `t`: its place and effects.
pub fn look(item: &Item, t: f64) -> (Place, Grade) {
    let local = item.local(t);
    (place_at(&item.motion, local), grade_at(&item.effects, local))
}

/// Burns the overlays into an export's frames, between the cuts (and their
/// crossfades) and the encoder: frame `n` of the edit is at `n / fps`.
pub struct Burn<'a> {
    out: &'a mut dyn FrameSink,
    lanes: &'a [Lane],
    pictures: Vec<(PathBuf, Arc<Picture>)>,
    fps: f64,
    fades: crate::project::Fades,
    length: f64,
    n: usize,
}

impl<'a> Burn<'a> {
    /// Loads every picture `lanes` uses, at most twice the frame's `size`
    /// (none drawn bigger than the frame needs more). With no lanes it
    /// passes frames on.
    pub fn new(out: &'a mut dyn FrameSink, lanes: &'a [Lane], size: (usize, usize), fps: f64, fades: crate::project::Fades, length: f64) -> Result<Self> {
        let mut pictures: Vec<(PathBuf, Arc<Picture>)> = Vec::new();
        for item in lanes.iter().flat_map(|l| &l.items) {
            if !pictures.iter().any(|(p, _)| *p == item.input) {
                pictures.push((item.input.clone(), Arc::new(Picture::load(&item.input, 2 * size.0.max(size.1))?)));
            }
        }
        Ok(Burn { out, lanes, pictures, fps, fades, length, n: 0 })
    }
}

impl FrameSink for Burn<'_> {
    fn write(&mut self, frame: &Frame) -> Result<()> {
        let t = self.n as f64 / self.fps;
        self.n += 1;
        let showing: Vec<_> = self.lanes.iter().filter_map(|l| l.at(t).map(|(_, it)| it)).collect();
        if showing.is_empty() {
            return self.out.write(frame);
        }
        let mut frame = Frame { width: frame.width, height: frame.height, rgb: frame.rgb.clone() };
        let black = self.fades.black(t, self.length);
        for item in showing {
            let picture = &self.pictures.iter().find(|(p, _)| *p == item.input).unwrap().1;
            let (place, grade) = look(item, t);
            let quad = Quad::new(&place, (picture.width, picture.height), (frame.width, frame.height));
            composite(&mut frame, picture, &quad, &grade, black);
        }
        self.out.write(&frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(at: f64, len: f64) -> Item {
        Item { input: "a.png".into(), at, start: 0.0, end: len, motion: Vec::new(), effects: Vec::new() }
    }

    #[test]
    fn items_keep_out_of_each_others_way() {
        let lane = Lane { items: vec![item(0.0, 4.0), item(10.0, 4.0), item(20.0, 2.0)] };
        assert_eq!(lane.at(11.0).map(|(i, _)| i), Some(1));
        assert!(lane.at(5.0).is_none());
        // Pushed into its neighbour it stops at its edge, past halfway it hops over.
        assert_eq!(lane.place(2, 12.0), 14.0);
        assert_eq!(lane.place(2, 8.5), 8.0);
        assert_eq!(lane.room(1), (4.0, 20.0));
    }

    #[test]
    fn places_ease_and_quads_fit_the_frame() {
        let key = |t, x, ease| MotionKey { t, place: Place { x, ..Default::default() }, ease };
        let keys = [key(0.0, 0.2, Ease::Linear), key(2.0, 0.6, Ease::Smooth)];
        assert!((place_at(&keys, 1.0).x - 0.4).abs() < 1e-9);
        assert_eq!(place_at(&[], 1.0), Place::default());
        // A square picture fills the height of a 16:9 frame, centred.
        let q = Quad::new(&Place::default(), (100, 100), (1920, 1080));
        assert_eq!((q.centre, q.half), ([960.0, 540.0], [540.0, 540.0]));
        // Turned a quarter clockwise, its top edge points right.
        let q = Quad { turn: std::f64::consts::FRAC_PI_2, ..q };
        let [u, v] = q.to_item(960.0 + 540.0, 540.0);
        assert!(u.abs() < 1e-9 && (v + 1.0).abs() < 1e-9, "{u} {v}");
        // Only what's set is written.
        let json = serde_json::to_string(&MotionKey { t: 1.0, place: Place { size: 0.3, ..Default::default() }, ease: Ease::Smooth }).unwrap();
        assert_eq!(json, r#"{"t":1.0,"x":0.5,"y":0.5,"size":0.3,"rotation":0.0}"#);
    }

    #[test]
    fn composite_blends_by_alpha_and_opacity() {
        // A 2×2 half-transparent white picture over grey, filling the frame.
        let picture = Picture { width: 2, height: 2, rgba: [128u8, 128, 128, 128].repeat(4) };
        let mut frame = Frame { width: 4, height: 4, rgb: vec![100; 48] };
        let quad = Quad::new(&Place::default(), (2, 2), (4, 4));
        composite(&mut frame, &picture, &quad, &Grade::default(), 0.0);
        assert!((frame.rgb[5 * 3] as i32 - 178).abs() <= 1, "{}", frame.rgb[5 * 3]);
        let mut frame = Frame { width: 4, height: 4, rgb: vec![100; 48] };
        composite(&mut frame, &picture, &quad, &Grade { opacity: 0.0, ..Default::default() }, 0.0);
        assert_eq!(frame.rgb[5 * 3], 100);
    }
}
