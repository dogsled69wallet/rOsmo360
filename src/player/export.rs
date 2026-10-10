//! Saving the paused frame as a PNG: either the view as framed, or the whole
//! sphere as an equirect. Rendered on the CPU from freshly decoded
//! full-resolution frames with the same stitch, stabilisation and seam
//! corrections as playback, so the size isn't tied to the window.

use super::gpu::{RS_STEPS_MAX, Rig};
use crate::ffmpeg;
use crate::stab::Stabilizer;
use crate::stitch::{self, M3, apply};
use anyhow::{Context, Result};
use std::f64::consts::PI;
use std::path::PathBuf;
use std::sync::Arc;

pub enum Shot {
    /// What is on screen, at this many pixels.
    View { user: M3, zoom: f64, projection: u32, size: (usize, usize) },
    /// The whole sphere, `width` × `width / 2`.
    Sphere { width: usize },
}

pub struct Request {
    pub input: PathBuf,
    pub hw: ffmpeg::Hwaccel,
    pub rig: Arc<Rig>,
    pub stab: Stabilizer,
    pub readout_ms: f64,
    pub t: f64,
    pub rolling_shutter: bool,
    pub shot: Shot,
    pub path: PathBuf,
}

pub fn save(r: Request) -> Result<PathBuf> {
    let frames = r
        .rig
        .tracks
        .iter()
        .map(|&track| ffmpeg::still(&r.input, track, r.t, r.hw))
        .collect::<Result<Vec<_>>>()?;
    let steps = if r.rolling_shutter && r.readout_ms > 0.0 { RS_STEPS_MAX } else { 1 };
    let views = if r.stab.has_data() { r.stab.rig_from_view(r.t * 1000.0, r.readout_ms, steps) } else { vec![stitch::IDENTITY] };

    let rgb = |i: usize, u: f64, v: f64| stitch::sample(&frames[r.rig.lenses[i].input], u, v).map(|c| c / 255.0);
    let gains = r.rig.seam.as_ref().map(|s| s.measure(|i, u, v| Some(rgb(i, u, v))));
    let field = r.rig.aligner.as_ref().map(|a| a.measure(&a.strips(|i, u, v| Some(rgb(i, u, v).iter().sum::<f64>() / 3.0)), None));
    let seam = stitch::Seam {
        exposure: r.rig.seam.as_ref().zip(gains.as_ref()),
        align: r.rig.aligner.as_ref().zip(field.as_ref()),
    };

    let out = match r.shot {
        Shot::Sphere { width } => stitch::equirect(&r.rig.lenses, &frames, width, None, &views, &seam),
        Shot::View { user, zoom, projection, size: (w, h) } => {
            let aspect = w as f64 / h as f64;
            let ray = |x: f64, y: f64| apply(user, screen_ray(projection, zoom, aspect, [x / w as f64 * 2.0 - 1.0, y / h as f64 * 2.0 - 1.0]));
            stitch::render(&r.rig.lenses, &frames, (w, h), ray, None, &views, &seam)
        }
    };
    image::save_buffer(&r.path, &out.rgb, out.width as u32, out.height as u32, image::ColorType::Rgb8).with_context(|| format!("writing {}", r.path.display()))?;
    Ok(r.path)
}

/// The view-space ray through screen point `ndc` (−1..1 each way, y down).
/// The shader's `screen_ray` must match.
fn screen_ray(projection: u32, zoom: f64, aspect: f64, [nx, ny]: [f64; 2]) -> [f64; 3] {
    match projection {
        // Little planet: stereographic around the view axis.
        1 => {
            let p = [nx * aspect * zoom * 2.0, ny * zoom * 2.0];
            let r = (p[0] * p[0] + p[1] * p[1]).sqrt().max(1e-9);
            let th = 2.0 * (r / 2.0).atan();
            [th.sin() * p[0] / r, th.sin() * p[1] / r, th.cos()]
        }
        // The whole sphere, flat.
        2 => {
            let (lon, lat) = (nx * PI, -ny * PI / 2.0);
            [lat.cos() * lon.sin(), -lat.sin(), lat.cos() * lon.cos()]
        }
        _ => {
            let v = [nx * aspect * zoom, ny * zoom, 1.0];
            let n = (v[0] * v[0] + v[1] * v[1] + 1.0).sqrt();
            v.map(|c| c / n)
        }
    }
}
