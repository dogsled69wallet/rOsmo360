//! Exposure matching across the seam. The two lenses meter separately and
//! catch flare differently, so in the overlap they can differ by 5-20%; the
//! blend then fades from one level to the other and leaves a light or dark
//! band along the seam. Each frame, both lenses are sampled at the same
//! directions around the seam and each is pulled half-way to the other,
//! fading in from well inside the lens so the correction itself has no edge.

use crate::ring::Ring;
use crate::stitch::Lens;
use std::f64::consts::TAU;

/// Bins around the seam ring; the ratio varies along it (haze, sun side).
pub const BINS: usize = 32;
/// The correction fades in from this angle off each lens's axis...
pub const FADE_FROM: f64 = 60.0 * std::f64::consts::PI / 180.0;
/// ...and is complete at the seam's middle.
pub const FADE_TO: f64 = std::f64::consts::FRAC_PI_2;

/// Share of the way gains move towards each new frame's measurement.
pub const EASE: f32 = 0.25;

/// Per lens, per bin, RGB gain applied to display-encoded colour.
#[derive(Clone, Copy, Debug)]
pub struct Gains(pub [[[f32; 3]; BINS]; 2]);

impl Default for Gains {
    fn default() -> Self {
        Gains([[[1.0; 3]; BINS]; 2])
    }
}

impl Gains {
    /// Moves a fraction `k` of the way to `to`, so gains don't flicker.
    pub fn ease(&mut self, to: &Gains, k: f32) {
        for (a, b) in self.0.iter_mut().flatten().flatten().zip(to.0.iter().flatten().flatten()) {
            *a += (b - *a) * k;
        }
    }
}

/// Where to sample the overlap: directions seen by both lenses, binned by
/// azimuth around the first lens's axis.
pub struct Seam {
    pub ring: Ring,
    /// (bin, picture pixels in lens 0, in lens 1).
    samples: Vec<(usize, [(f64, f64); 2])>,
}

impl Seam {
    /// `None` unless the rig is two back-to-back lenses.
    pub fn new(lenses: &[Lens]) -> Option<Seam> {
        let ring = Ring::new(lenses)?;
        let [a, b] = lenses else { return None };
        let mut samples = Vec::new();
        for bin in 0..BINS {
            for s in 0..6 {
                let phi = (bin as f64 + (s as f64 + 0.5) / 6.0) / BINS as f64 * TAU;
                for off in [-4.5f64, -2.5, -0.5, 1.5, 3.5] {
                    let d = ring.dir(phi, (90.0 + off).to_radians());
                    if let (Some(pa), Some(pb)) = (a.project(d), b.project(d)) {
                        samples.push((bin, [(pa.0, pa.1), (pb.0, pb.1)]));
                    }
                }
            }
        }
        Some(Seam { ring, samples })
    }

    /// Gains that pull each lens half-way to the other. `sample(lens, u, v)`
    /// returns display RGB in 0..1 at that lens's picture pixel.
    pub fn measure(&self, sample: impl Fn(usize, f64, f64) -> Option<[f64; 3]>) -> Gains {
        let mut ratios: Vec<[Vec<f64>; 3]> = (0..BINS).map(|_| Default::default()).collect();
        for (bin, at) in &self.samples {
            let (Some(c0), Some(c1)) = (sample(0, at[0].0, at[0].1), sample(1, at[1].0, at[1].1)) else { continue };
            // Clipped or near-black pixels say nothing about exposure.
            let usable = |c: [f64; 3]| c.iter().all(|&v| v > 0.03 && v < 0.97);
            if !usable(c0) || !usable(c1) {
                continue;
            }
            for ch in 0..3 {
                ratios[*bin][ch].push((c1[ch] / c0[ch]).ln());
            }
        }
        // Median per bin: a hand or stick in the overlap disagrees wildly but
        // only in a few samples. Bins with too few samples borrow from the
        // smoothing below.
        let mut log_r = [[0f64; 3]; BINS];
        let mut have = [false; BINS];
        for (bin, r) in ratios.iter_mut().enumerate() {
            if r[0].len() >= 8 {
                have[bin] = true;
                for ch in 0..3 {
                    r[ch].sort_by(f64::total_cmp);
                    log_r[bin][ch] = r[ch][r[ch].len() / 2].clamp(-0.4, 0.4);
                }
            }
        }
        for _ in 0..3 {
            let prev = log_r;
            for bin in 0..BINS {
                let (l, r) = ((bin + BINS - 1) % BINS, (bin + 1) % BINS);
                let w = [have[l] as u8 as f64, 2.0 * have[bin] as u8 as f64, have[r] as u8 as f64];
                let sum = w.iter().sum::<f64>();
                if sum > 0.0 {
                    for ch in 0..3 {
                        log_r[bin][ch] = (w[0] * prev[l][ch] + w[1] * prev[bin][ch] + w[2] * prev[r][ch]) / sum;
                    }
                }
            }
            have = have.map(|_| true);
        }
        Gains([0.5, -0.5].map(|half| log_r.map(|r| r.map(|r| (half * r).exp() as f32))))
    }

    /// The gain for `lens` at rig direction `d`, `theta` off that lens's
    /// axis. The shader's `exposure_gain` must match.
    pub fn gain(&self, gains: &Gains, lens: usize, d: [f64; 3], theta: f64) -> [f64; 3] {
        let (phi, _) = self.ring.coords(d);
        let x = phi / TAU * BINS as f64 - 0.5;
        let i = x.floor();
        let f = x - i;
        let (b0, b1) = ((i as isize).rem_euclid(BINS as isize) as usize, ((i as isize) + 1).rem_euclid(BINS as isize) as usize);
        let t = ((theta - FADE_FROM) / (FADE_TO - FADE_FROM)).clamp(0.0, 1.0);
        let fade = t * t * (3.0 - 2.0 * t);
        let g = &gains.0[lens];
        [0, 1, 2].map(|ch| 1.0 + ((g[b0][ch] as f64) * (1.0 - f) + (g[b1][ch] as f64) * f - 1.0) * fade)
    }
}
