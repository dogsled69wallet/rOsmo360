//! CPU reference stitcher: a telemetry-parser `CameraRig` -> equirectangular.
//! This is the ground truth the GPU shader will later be checked against.
//!
//! Conventions are video-types' geometry module: rig axes x right, y down,
//! z forward; `Lens::rotation` is rig-from-camera; blend ramps are per lens.

use anyhow::{Result, bail};
use rayon::prelude::*;
use telemetry_parser::rig::geometry::{self, LensModel, MaskPolarity, Quat};

pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub rgb: Vec<u8>,
}

pub struct Lens {
    /// Index into the rig's pictures, i.e. which decoded video track.
    pub input: usize,
    pub(crate) cam_from_rig: [[f64; 3]; 3],
    pub(crate) k: [f64; 5],
    pub(crate) p: [f64; 2],
    pub(crate) focal: [f64; 2],
    pub(crate) centre: [f64; 2],
    pub(crate) skew: f64,
    pub(crate) mirror: bool,
    pub(crate) calib_to_picture: geometry::Affine2,
    pub(crate) fov_limit: f64,
    pub(crate) blend: geometry::Blend,
    pub(crate) calib_dim: [usize; 2],
    /// Image validity from the circle and masks, rasterised in calibration pixels.
    pub(crate) valid: Vec<bool>,
}

impl Lens {
    pub fn new(lens: &geometry::Lens) -> Result<Self> {
        let LensModel::ThetaPoly { k, p } = lens.intrinsics.model else {
            bail!("unsupported lens model {:?}", lens.intrinsics.model);
        };
        let calib_dim = [lens.intrinsics.calib_dim[0] as usize, lens.intrinsics.calib_dim[1] as usize];
        Ok(Lens {
            input: lens.picture.input as usize,
            cam_from_rig: transpose(quat_to_mat(lens.rotation)),
            k,
            p,
            focal: lens.intrinsics.focal,
            centre: lens.intrinsics.centre,
            skew: lens.intrinsics.skew,
            mirror: lens.mirror,
            calib_to_picture: lens.calib_to_picture,
            fov_limit: lens.fov_limit.unwrap_or(std::f64::consts::FRAC_PI_2),
            blend: lens.blend,
            calib_dim,
            valid: rasterise_validity(lens, calib_dim),
        })
    }

    /// Projects a rig-frame direction to picture pixels, plus its angle from
    /// the optical axis. `None` where the lens has no valid picture.
    fn project(&self, d: [f64; 3]) -> Option<(f64, f64, f64)> {
        let c = apply(self.cam_from_rig, d);
        let rho = (c[0] * c[0] + c[1] * c[1]).sqrt();
        let theta = rho.atan2(c[2]);
        if theta > self.fov_limit {
            return None;
        }
        let t2 = theta * theta;
        let k = &self.k;
        let theta_d = theta * (1.0 + t2 * (k[0] + t2 * (k[1] + t2 * (k[2] + t2 * (k[3] + t2 * k[4])))));
        let (x, y) = if rho > 1e-12 { (theta_d * c[0] / rho, theta_d * c[1] / rho) } else { (0.0, 0.0) };
        let td2 = theta_d * theta_d;
        let [p1, p2] = self.p;
        let xd = x + 2.0 * p1 * x * y + p2 * (td2 + 2.0 * x * x);
        let yd = y + p1 * (td2 + 2.0 * y * y) + 2.0 * p2 * x * y;
        let mut u = self.focal[0] * xd + self.skew * yd + self.centre[0];
        let v = self.focal[1] * yd + self.centre[1];
        if self.mirror {
            u = 2.0 * self.centre[0] - u;
        }
        let (iu, iv) = (u.floor(), v.floor());
        if iu < 0.0 || iv < 0.0 || iu >= self.calib_dim[0] as f64 || iv >= self.calib_dim[1] as f64 {
            return None;
        }
        if !self.valid[iv as usize * self.calib_dim[0] + iu as usize] {
            return None;
        }
        let a = &self.calib_to_picture;
        let pu = a.a[0][0] * u + a.a[0][1] * v + a.t[0];
        let pv = a.a[1][0] * u + a.a[1][1] * v + a.t[1];
        Some((pu, pv, theta))
    }
}

/// Feathered composite: each lens weighted by its own blend ramp, normalised.
/// `only` forces a single lens (for checking alignment in the overlap zone).
///
/// `views` holds `rig_from_view` at instants spread evenly over the frame's
/// readout. With more than one, each ray is re-aimed with the rotation at the
/// sensor row it lands on (rolling-shutter correction, top-down readout).
pub fn equirect(lenses: &[Lens], frames: &[Frame], out_w: usize, only: Option<usize>, views: &[M3]) -> Frame {
    use std::f64::consts::PI;
    let out_h = out_w / 2;
    let mut rgb = vec![0u8; out_w * out_h * 3];
    rgb.par_chunks_mut(out_w * 3).enumerate().for_each(|(y, row)| {
        let lat = PI / 2.0 - (y as f64 + 0.5) / out_h as f64 * PI;
        for x in 0..out_w {
            let lon = (x as f64 + 0.5) / out_w as f64 * 2.0 * PI - PI;
            let d = [lat.cos() * lon.sin(), -lat.sin(), lat.cos() * lon.cos()];
            let mid = apply(views[views.len() / 2], d);
            let mut acc = [0f64; 3];
            let mut wsum = 0.0;
            for (i, lens) in lenses.iter().enumerate() {
                let Some(mut hit) = lens.project(mid) else { continue };
                if views.len() > 1 {
                    let row = (hit.1 / frames[lens.input].height as f64).clamp(0.0, 1.0);
                    let at = views[(row * (views.len() - 1) as f64).round() as usize];
                    let Some(h) = lens.project(apply(at, d)) else { continue };
                    hit = h;
                }
                let (u, v, theta) = hit;
                let w = match only {
                    Some(o) if o == i => 1.0,
                    Some(_) => 0.0,
                    None => lens.blend.weight(theta),
                };
                if w <= 0.0 {
                    continue;
                }
                let px = sample(&frames[lens.input], u, v);
                for c in 0..3 {
                    acc[c] += px[c] * w;
                }
                wsum += w;
            }
            if wsum > 0.0 {
                for c in 0..3 {
                    row[x * 3 + c] = (acc[c] / wsum).round().clamp(0.0, 255.0) as u8;
                }
            }
        }
    });
    Frame { width: out_w, height: out_h, rgb }
}

/// Image circle and include polygons are unioned, exclude polygons subtracted;
/// with neither a circle nor an include, the whole frame is valid.
fn rasterise_validity(lens: &geometry::Lens, [w, h]: [usize; 2]) -> Vec<bool> {
    let includes: Vec<_> = lens.masks.iter().filter(|m| m.polarity == MaskPolarity::Include).collect();
    let excludes: Vec<_> = lens.masks.iter().filter(|m| m.polarity != MaskPolarity::Include).collect();
    let open = includes.is_empty() && lens.image_circle.is_none();
    let mut valid = vec![open; w * h];
    valid.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        let py = y as f64 + 0.5;
        if let Some(c) = &lens.image_circle {
            for (x, v) in row.iter_mut().enumerate() {
                let (dx, dy) = (x as f64 + 0.5 - c.centre[0], py - c.centre[1]);
                *v |= dx * dx + dy * dy <= c.radius * c.radius;
            }
        }
        for m in &includes {
            fill_row(row, &m.points, py, true);
        }
        for m in &excludes {
            fill_row(row, &m.points, py, false);
        }
    });
    valid
}

/// Even-odd scanline fill of one polygon on row centre `py`, half-open edges.
fn fill_row(row: &mut [bool], pts: &[[f64; 2]], py: f64, value: bool) {
    let mut xs: Vec<f64> = (0..pts.len())
        .filter_map(|i| {
            let (a, b) = (pts[i], pts[(i + 1) % pts.len()]);
            ((a[1] <= py) != (b[1] <= py)).then(|| a[0] + (py - a[1]) / (b[1] - a[1]) * (b[0] - a[0]))
        })
        .collect();
    xs.sort_by(f64::total_cmp);
    for pair in xs.chunks_exact(2) {
        let x0 = (pair[0] - 0.5).ceil().max(0.0) as usize;
        let x1 = ((pair[1] - 0.5).floor() + 1.0).clamp(0.0, row.len() as f64) as usize;
        for v in row.iter_mut().take(x1).skip(x0) {
            *v = value;
        }
    }
}

fn sample(f: &Frame, u: f64, v: f64) -> [f64; 3] {
    let x0 = (u - 0.5).floor();
    let y0 = (v - 0.5).floor();
    let (fx, fy) = (u - 0.5 - x0, v - 0.5 - y0);
    let px = |x: f64, y: f64| -> [f64; 3] {
        let x = (x as isize).clamp(0, f.width as isize - 1) as usize;
        let y = (y as isize).clamp(0, f.height as isize - 1) as usize;
        let i = (y * f.width + x) * 3;
        [f.rgb[i] as f64, f.rgb[i + 1] as f64, f.rgb[i + 2] as f64]
    };
    let (a, b, c, d) = (px(x0, y0), px(x0 + 1.0, y0), px(x0, y0 + 1.0), px(x0 + 1.0, y0 + 1.0));
    std::array::from_fn(|i| {
        let top = a[i] + (b[i] - a[i]) * fx;
        let bot = c[i] + (d[i] - c[i]) * fx;
        top + (bot - top) * fy
    })
}

pub type M3 = [[f64; 3]; 3];

pub fn mat_mul(a: M3, b: M3) -> M3 {
    std::array::from_fn(|i| std::array::from_fn(|j| (0..3).map(|k| a[i][k] * b[k][j]).sum()))
}

pub fn quat_to_mat(q: Quat) -> M3 {
    let Quat { w, x, y, z } = q;
    [
        [1.0 - 2.0 * (y * y + z * z), 2.0 * (x * y - w * z), 2.0 * (x * z + w * y)],
        [2.0 * (x * y + w * z), 1.0 - 2.0 * (x * x + z * z), 2.0 * (y * z - w * x)],
        [2.0 * (x * z - w * y), 2.0 * (y * z + w * x), 1.0 - 2.0 * (x * x + y * y)],
    ]
}

pub fn transpose(m: M3) -> M3 {
    std::array::from_fn(|i| std::array::from_fn(|j| m[j][i]))
}

fn apply(m: M3, v: [f64; 3]) -> [f64; 3] {
    std::array::from_fn(|i| m[i][0] * v[0] + m[i][1] * v[1] + m[i][2] * v[2])
}
