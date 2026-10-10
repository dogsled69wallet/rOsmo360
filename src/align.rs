//! Seam alignment for close subjects. The lenses sit a few centimetres apart,
//! so something near the camera (the hand on the selfie stick) lands at
//! different angles in each, and the feathered blend shows it twice at half
//! strength. Each frame, both lenses are unrolled onto a strip along the seam
//! and patches are matched across it; each lens is then shifted half-way
//! towards the other where they disagree, fading out away from the seam.
//!
//! Parallax from back-to-back lenses runs across the seam (along α), so the
//! search is one-dimensional. Matching is symmetric about the output angle,
//! so the field needs no inversion: lens 0 samples at α − δ/2 and lens 1 at
//! α + δ/2.

use crate::ring::Ring;
use crate::stitch::Lens;
use rayon::prelude::*;
use std::path::Path;
use std::f64::consts::TAU;

/// Field cells around the ring and across the seam.
pub const CELLS_PHI: usize = 128;
pub const CELLS_ALPHA: usize = 7;
/// Angle of the first and last row of cells.
pub const CELL_ALPHA_LO: f64 = 84.0;
pub const CELL_ALPHA_HI: f64 = 96.0;
/// The shift is full across the blend band and fades to nothing this far
/// beyond it, so the warp has no edge of its own.
pub const TAPER_LO: f64 = 83.0;
pub const TAPER_HI: f64 = 97.0;
pub const TAPER_WIDTH: f64 = 12.0;
/// Share of the way the field moves towards each new frame's measurement.
pub const EASE: f32 = 0.3;

/// The strip both lenses are unrolled onto: φ columns, α rows.
const STRIP_W: usize = 1024;
const STRIP_ALPHA_LO: f64 = 74.0;
const STRIP_STEP: f64 = 0.2;
const STRIP_H: usize = 161;
/// Largest shift searched, in strip rows each way per lens (±6° in all).
const MAX_HALF: isize = 15;
/// Patch half-size in columns and rows.
const PATCH_W: isize = 8;
const PATCH_H: isize = 6;
/// Cost added per degree of shift, so featureless patches settle on the
/// calibration's own (distant) alignment.
const DISTANCE_PRIOR: f64 = 0.06;
/// Cost added per degree away from the previous frame's shift: a cell only
/// moves when the picture clearly says so. With the prior above this cut
/// frame-to-frame wobble of the shift ~4× (`rosmo360 seam` on 0037, 0026,
/// 0043) without leaving more ghosting.
const TEMPORAL: f64 = 0.1;
/// Matching runs on detail only: each strip minus its local mean over this
/// radius. On raw brightness, smooth sky gradients (sun haze, the lenses'
/// leftover exposure difference) "matched" at large shifts, warping the sky
/// along the seam into faint streaks. With detail only, the sky warped
/// 5× less (4.3% → 0.8% of it changed on 0043 at 8.81 s) and eased jitter
/// fell up to 5× (0037: 0.076° → 0.014°), for ~1% more mismatch.
const DETAIL_RADIUS: isize = 4;
/// How much a shift's cost must beat no shift by: with the detail-only
/// matching, sky changes fell to 0.5% at 2–3% more mismatch.
const ZERO_MARGIN: f64 = 0.05;

/// The disagreement cut only acts where both lenses see detail across the
/// blend band (the smaller of their mean |detail| per azimuth bin), fading
/// in between these. Sun glare that one lens catches and the other doesn't
/// is smooth: on 0037 at 34.6 s its bins scored 0.002–0.006 and the cut drew
/// a hard edge through the halo. The hand on the stick and rock score
/// 0.02–0.15, so the cut still takes them.
const CUT_DETAIL_FROM: f64 = 0.008;
const CUT_DETAIL_FULL: f64 = 0.02;

/// The seam correction measured on one frame.
#[derive(Clone, Debug)]
pub struct Field {
    /// δ in radians per cell: how much further from lens 0's axis lens 1
    /// sees the content at that output angle.
    pub shift: Vec<f32>,
    /// 0..1 per azimuth bin: how far the disagreement cut may act there.
    pub cut: Vec<f32>,
}

impl Default for Field {
    fn default() -> Self {
        Field { shift: vec![0.0; CELLS_PHI * CELLS_ALPHA], cut: vec![1.0; CELLS_PHI] }
    }
}

impl Field {
    pub fn ease(&mut self, to: &Field, k: f32) {
        for (a, b) in self.shift.iter_mut().zip(&to.shift).chain(self.cut.iter_mut().zip(&to.cut)) {
            *a += (b - *a) * k;
        }
    }

    /// The cut's strength at azimuth `phi`, linear between bins.
    pub fn cut_at(&self, phi: f64) -> f64 {
        let x = phi / TAU * CELLS_PHI as f64 - 0.5;
        let (x0, f) = (x.floor(), x - x.floor());
        let v = |i: f64| self.cut[(i as isize).rem_euclid(CELLS_PHI as isize) as usize] as f64;
        v(x0) * (1.0 - f) + v(x0 + 1.0) * f
    }

    /// Bilinear in cells, wrapping round the ring and clamped across it.
    fn at(&self, phi: f64, alpha_deg: f64) -> f64 {
        let x = phi / TAU * CELLS_PHI as f64 - 0.5;
        let y = ((alpha_deg - CELL_ALPHA_LO) / (CELL_ALPHA_HI - CELL_ALPHA_LO) * (CELLS_ALPHA - 1) as f64).clamp(0.0, (CELLS_ALPHA - 1) as f64);
        let (x0, y0) = (x.floor(), y.floor());
        let (fx, fy) = (x - x0, y - y0);
        let col = |i: f64| (i as isize).rem_euclid(CELLS_PHI as isize) as usize;
        let row = |j: f64| (j as usize).min(CELLS_ALPHA - 1);
        let v = |i: f64, j: f64| self.shift[row(j) * CELLS_PHI + col(i)] as f64;
        let top = v(x0, y0) * (1.0 - fx) + v(x0 + 1.0, y0) * fx;
        let bot = v(x0, y0 + 1.0) * (1.0 - fx) + v(x0 + 1.0, y0 + 1.0) * fx;
        top * (1.0 - fy) + bot * fy
    }
}

/// Both lenses' luma on the strip, NaN where a lens has no picture.
pub struct Strips([Vec<f32>; 2]);

impl Strips {
    /// |lens 0 − lens 1| at strip pixel (x, y) once `field` is applied.
    fn disagreement(&self, field: &Field, x: usize, y: usize) -> f64 {
        let a = STRIP_ALPHA_LO + y as f64 * STRIP_STEP;
        let half = 0.5 * field.at((x as f64 + 0.5) / STRIP_W as f64 * TAU, a).to_degrees() / STRIP_STEP;
        (self.at(0, x, y as f64 - half) - self.at(1, x, y as f64 + half)).abs()
    }

    /// Linear in α between strip rows.
    fn at(&self, lens: usize, x: usize, y: f64) -> f64 {
        let y0 = y.floor();
        if y0 < 0.0 || y0 + 1.0 >= STRIP_H as f64 {
            return f64::NAN;
        }
        let s = &self.0[lens];
        let (a, b) = (s[y0 as usize * STRIP_W + x] as f64, s[(y0 as usize + 1) * STRIP_W + x] as f64);
        a + (b - a) * (y - y0)
    }
}

pub struct Aligner {
    pub ring: Ring,
    /// Per strip pixel, picture pixels in lens 0 and lens 1.
    strip: Vec<[Option<(f64, f64)>; 2]>,
}

impl Aligner {
    /// `None` unless the rig is two back-to-back lenses.
    pub fn new(lenses: &[Lens]) -> Option<Aligner> {
        let ring = Ring::new(lenses)?;
        let strip = (0..STRIP_W * STRIP_H)
            .map(|i| {
                let (x, y) = (i % STRIP_W, i / STRIP_W);
                let d = ring.dir((x as f64 + 0.5) / STRIP_W as f64 * TAU, (STRIP_ALPHA_LO + y as f64 * STRIP_STEP).to_radians());
                [0, 1].map(|l| lenses[l].project(d).map(|p| (p.0, p.1)))
            })
            .collect();
        Some(Aligner { ring, strip })
    }

    /// Both lenses unrolled onto the strip. `luma(lens, u, v)` returns 0..1
    /// at that lens's picture pixel.
    pub fn strips(&self, luma: impl Fn(usize, f64, f64) -> Option<f64> + Sync) -> Strips {
        Strips([0, 1].map(|l| self.strip.par_iter().map(|at| at[l].and_then(|(u, v)| luma(l, u, v)).map_or(f32::NAN, |v| v as f32)).collect()))
    }

    /// The disparity field of one frame; `prev` is the field shown on the
    /// frame before, when this one follows on from it.
    pub fn measure(&self, strips: &Strips, prev: Option<&Field>) -> Field {
        let detail = [highpass(&strips.0[0], DETAIL_RADIUS), highpass(&strips.0[1], DETAIL_RADIUS)];
        let mut shift: Vec<f32> =
            (0..CELLS_PHI * CELLS_ALPHA).into_par_iter().map(|c| match_cell(&detail, c % CELLS_PHI, c / CELLS_PHI, prev.map(|p| p.shift[c]))).collect();
        fill_and_smooth(&mut shift);
        Field { shift, cut: (0..CELLS_PHI).map(|b| cut_strength(&detail, b)).collect() }
    }

    /// Writes the frame's strips as images for inspection: each lens, their
    /// difference before and after `field`, and the field itself (red is
    /// lens 1 further from lens 0's axis, blue nearer; full colour at 6°).
    pub fn dump(&self, strips: &Strips, field: &Field, dir: &Path) -> anyhow::Result<()> {
        let gray = |name: &str, f: &dyn Fn(usize, usize) -> f64| -> anyhow::Result<()> {
            let px: Vec<u8> = (0..STRIP_W * STRIP_H).map(|i| f(i % STRIP_W, i / STRIP_W)).map(|v| if v.is_nan() { 0 } else { (v * 255.0).clamp(0.0, 255.0) as u8 }).collect();
            image::save_buffer(dir.join(name), &px, STRIP_W as u32, STRIP_H as u32, image::ColorType::L8)?;
            Ok(())
        };
        gray("lens0.png", &|x, y| strips.at(0, x, y as f64))?;
        gray("lens1.png", &|x, y| strips.at(1, x, y as f64))?;
        let none = Field::default();
        gray("diff_none.png", &|x, y| 4.0 * strips.disagreement(&none, x, y))?;
        gray("diff_aligned.png", &|x, y| 4.0 * strips.disagreement(field, x, y))?;
        let k = 8;
        let rgb: Vec<u8> = (0..CELLS_ALPHA * k)
            .flat_map(|y| (0..CELLS_PHI * k).map(move |x| (x / k, y / k)))
            .flat_map(|(cx, cy)| {
                let d = (field.shift[cy * CELLS_PHI + cx] as f64).to_degrees() / 6.0;
                let v = (d.abs().min(1.0) * 255.0) as u8;
                if d > 0.0 { [v, 0, 0] } else { [0, 0, v] }
            })
            .collect();
        image::save_buffer(dir.join("field.png"), &rgb, (CELLS_PHI * k) as u32, (CELLS_ALPHA * k) as u32, image::ColorType::Rgb8)?;
        Ok(())
    }

    /// The strip column a rig direction falls in, if it is on the strip.
    pub fn column(&self, d: [f64; 3]) -> usize {
        let (phi, _) = self.ring.coords(d);
        ((phi / TAU * STRIP_W as f64) as usize).min(STRIP_W - 1)
    }

    /// Mean absolute luma difference between the lenses across the blend
    /// band once `field` is applied: what ghosting is left.
    /// `around` limits it to the strip columns within ±17° of that column
    /// (the stick zone, say), where the global average hides what happens.
    pub fn mismatch(&self, strips: &Strips, field: &Field, around: Option<usize>) -> f64 {
        let (mut sum, mut n) = (0.0, 0usize);
        for y in 0..STRIP_H {
            let a = STRIP_ALPHA_LO + y as f64 * STRIP_STEP;
            if !(TAPER_LO..=TAPER_HI).contains(&a) {
                continue;
            }
            for x in 0..STRIP_W {
                if let Some(c) = around {
                    let d = (x as isize - c as isize).rem_euclid(STRIP_W as isize);
                    if d.min(STRIP_W as isize - d) > 48 {
                        continue;
                    }
                }
                let v = strips.disagreement(field, x, y);
                if !v.is_nan() {
                    sum += v;
                    n += 1;
                }
            }
        }
        sum / n.max(1) as f64
    }

    /// The direction lens `lens` should sample for output direction `d`.
    /// The shader's `align_warp` must match.
    pub fn warp(&self, field: &Field, lens: usize, d: [f64; 3]) -> [f64; 3] {
        let (phi, alpha) = self.ring.coords(d);
        let a = alpha.to_degrees();
        let taper = (1.0 - (TAPER_LO - a).max(a - TAPER_HI).max(0.0) / TAPER_WIDTH).max(0.0);
        if taper == 0.0 {
            return d;
        }
        let half = 0.5 * field.at(phi, a) * taper;
        self.ring.dir(phi, if lens == 0 { alpha - half } else { alpha + half })
    }
}

/// Best shift for one cell by zero-mean normalised cross-correlation, which
/// ignores any brightness difference left between the lenses. NaN where the
/// patch has too little texture or overlap to say.
fn match_cell(strips: &[Vec<f32>; 2], cx: usize, cy: usize, prev: Option<f32>) -> f32 {
    let x0 = ((cx as f64 + 0.5) * (STRIP_W / CELLS_PHI) as f64) as isize;
    let alpha = CELL_ALPHA_LO + cy as f64 * (CELL_ALPHA_HI - CELL_ALPHA_LO) / (CELLS_ALPHA - 1) as f64;
    let y0 = ((alpha - STRIP_ALPHA_LO) / STRIP_STEP).round() as isize;
    let px = |s: &Vec<f32>, x: isize, y: isize| -> f32 {
        if y < 0 || y >= STRIP_H as isize {
            return f32::NAN;
        }
        s[y as usize * STRIP_W + x.rem_euclid(STRIP_W as isize) as usize]
    };
    let full = ((2 * PATCH_W + 1) * (2 * PATCH_H + 1)) as f64;
    let mut costs = Vec::with_capacity((2 * MAX_HALF + 1) as usize);
    for k in -MAX_HALF..=MAX_HALF {
        let (mut n, mut sa, mut sb, mut saa, mut sbb, mut sab) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
        for j in -PATCH_H..=PATCH_H {
            for i in -PATCH_W..=PATCH_W {
                let a = px(&strips[0], x0 + i, y0 + j - k) as f64;
                let b = px(&strips[1], x0 + i, y0 + j + k) as f64;
                if a.is_nan() || b.is_nan() {
                    continue;
                }
                n += 1.0;
                sa += a;
                sb += b;
                saa += a * a;
                sbb += b * b;
                sab += a * b;
            }
        }
        if n < 0.6 * full {
            costs.push(f64::NAN);
            continue;
        }
        let va = saa / n - (sa / n).powi(2);
        let vb = sbb / n - (sb / n).powi(2);
        // Flat sky or shadow: any shift matches, so it says nothing.
        if va < 0.015f64.powi(2) || vb < 0.015f64.powi(2) {
            costs.push(f64::NAN);
            continue;
        }
        let ncc = (sab / n - sa / n * sb / n) / (va * vb).sqrt();
        let shift_deg = 2.0 * k as f64 * STRIP_STEP;
        let temporal = prev.map_or(0.0, |p| TEMPORAL * (shift_deg - (p as f64).to_degrees()).abs());
        costs.push(1.0 - ncc + DISTANCE_PRIOR * shift_deg.abs() + temporal);
    }
    let Some((best, &c)) = costs.iter().enumerate().filter(|(_, c)| !c.is_nan()).min_by(|a, b| a.1.total_cmp(b.1)) else {
        return f32::NAN;
    };
    // A weak best match is as likely to be wrong as right.
    if c > 0.5 {
        return f32::NAN;
    }
    // A shift has to beat no shift clearly, or smooth sky drifts.
    if let Some(&c0) = costs.get(MAX_HALF as usize)
        && !c0.is_nan()
        && c0 - c < ZERO_MARGIN
    {
        return 0.0;
    }
    // Sub-row refinement from the neighbouring costs.
    let mut k = best as f64 - MAX_HALF as f64;
    if best > 0 && best + 1 < costs.len() && !costs[best - 1].is_nan() && !costs[best + 1].is_nan() {
        let (l, r) = (costs[best - 1], costs[best + 1]);
        let den = l - 2.0 * c + r;
        if den > 1e-9 {
            k += (0.5 * (l - r) / den).clamp(-0.5, 0.5);
        }
    }
    (2.0 * k * STRIP_STEP).to_radians() as f32
}

/// The strip minus its local mean over a (2r+1)² box of valid pixels, so
/// only edges and texture are left to match.
/// How far the cut may act in azimuth bin `b`, from the detail both lenses
/// see across the blend band. Bins with no overlap keep the full cut.
fn cut_strength(detail: &[Vec<f32>; 2], b: usize) -> f32 {
    let rows = ((TAPER_LO - STRIP_ALPHA_LO) / STRIP_STEP) as usize..((TAPER_HI - STRIP_ALPHA_LO) / STRIP_STEP) as usize;
    let cols = b * STRIP_W / CELLS_PHI..(b + 1) * STRIP_W / CELLS_PHI;
    let (mut sum, mut n) = ([0.0f64; 2], 0.0);
    for y in rows {
        for x in cols.clone() {
            let (a, c) = (detail[0][y * STRIP_W + x], detail[1][y * STRIP_W + x]);
            if !a.is_nan() && !c.is_nan() {
                sum[0] += a.abs() as f64;
                sum[1] += c.abs() as f64;
                n += 1.0;
            }
        }
    }
    if n == 0.0 {
        return 1.0;
    }
    let t = ((sum[0].min(sum[1]) / n - CUT_DETAIL_FROM) / (CUT_DETAIL_FULL - CUT_DETAIL_FROM)).clamp(0.0, 1.0);
    (t * t * (3.0 - 2.0 * t)) as f32
}

fn highpass(s: &[f32], r: isize) -> Vec<f32> {
    (0..STRIP_W * STRIP_H)
        .into_par_iter()
        .map(|i| {
            let v = s[i];
            if v.is_nan() {
                return f32::NAN;
            }
            let (x, y) = ((i % STRIP_W) as isize, (i / STRIP_W) as isize);
            let (mut sum, mut n) = (0.0f32, 0.0f32);
            for dy in -r..=r {
                let yy = y + dy;
                if yy < 0 || yy >= STRIP_H as isize {
                    continue;
                }
                for dx in -r..=r {
                    let w = s[yy as usize * STRIP_W + (x + dx).rem_euclid(STRIP_W as isize) as usize];
                    if !w.is_nan() {
                        sum += w;
                        n += 1.0;
                    }
                }
            }
            v - sum / n
        })
        .collect()
}

/// Fills unmeasured cells from their neighbours (or zero, the calibration's
/// own alignment), then takes a 3×3 median and a light blur so one bad
/// match can't tear the picture.
fn fill_and_smooth(cells: &mut [f32]) {
    let idx = |x: isize, y: isize| y.clamp(0, CELLS_ALPHA as isize - 1) as usize * CELLS_PHI + x.rem_euclid(CELLS_PHI as isize) as usize;
    for _ in 0..CELLS_PHI {
        if !cells.iter().any(|c| c.is_nan()) {
            break;
        }
        let prev = cells.to_vec();
        for y in 0..CELLS_ALPHA as isize {
            for x in 0..CELLS_PHI as isize {
                if !prev[idx(x, y)].is_nan() {
                    continue;
                }
                let near: Vec<f32> = [(-1, 0), (1, 0), (0, -1), (0, 1)].iter().map(|(dx, dy)| prev[idx(x + dx, y + dy)]).filter(|v| !v.is_nan()).collect();
                if !near.is_empty() {
                    cells[idx(x, y)] = near.iter().sum::<f32>() / near.len() as f32;
                }
            }
        }
    }
    for c in cells.iter_mut().filter(|c| c.is_nan()) {
        *c = 0.0;
    }
    let prev = cells.to_vec();
    for y in 0..CELLS_ALPHA as isize {
        for x in 0..CELLS_PHI as isize {
            let mut n: Vec<f32> = (-1..=1).flat_map(|dy| (-1..=1).map(move |dx| (dx, dy))).map(|(dx, dy)| prev[idx(x + dx, y + dy)]).collect();
            n.sort_by(f32::total_cmp);
            cells[idx(x, y)] = n[4];
        }
    }
    let prev = cells.to_vec();
    for y in 0..CELLS_ALPHA as isize {
        for x in 0..CELLS_PHI as isize {
            cells[idx(x, y)] = 0.25 * prev[idx(x - 1, y)] + 0.5 * prev[idx(x, y)] + 0.25 * prev[idx(x + 1, y)];
        }
    }
}
