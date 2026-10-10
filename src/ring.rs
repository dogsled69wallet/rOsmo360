//! The seam ring of a back-to-back lens pair: directions parametrised by
//! azimuth φ around the first lens's axis and angle α from that axis. The
//! seam's middle is α = 90°. Exposure matching bins along φ; seam alignment
//! shifts along α.

use crate::stitch::Lens;
use std::f64::consts::TAU;

#[derive(Clone, Copy)]
pub struct Ring {
    /// Rows: the first lens's axis, then two directions spanning the ring.
    pub basis: [[f64; 3]; 3],
}

impl Ring {
    /// `None` unless the rig is two back-to-back lenses.
    pub fn new(lenses: &[Lens]) -> Option<Ring> {
        let [a, _] = lenses else { return None };
        // The camera's z axis in rig coordinates is cam_from_rig's last row.
        let axis = a.cam_from_rig[2];
        let any = if axis[0].abs() < 0.9 { [1.0, 0.0, 0.0] } else { [0.0, 1.0, 0.0] };
        let e1 = normalise(cross(axis, any));
        let e2 = cross(axis, e1);
        Some(Ring { basis: [axis, e1, e2] })
    }

    pub fn dir(&self, phi: f64, alpha: f64) -> [f64; 3] {
        let [a, e1, e2] = self.basis;
        let (sa, ca) = alpha.sin_cos();
        let (sp, cp) = phi.sin_cos();
        [0, 1, 2].map(|k| ca * a[k] + sa * (cp * e1[k] + sp * e2[k]))
    }

    /// (φ in 0..2π, α in 0..π) of a unit direction.
    pub fn coords(&self, d: [f64; 3]) -> (f64, f64) {
        let [a, e1, e2] = self.basis;
        let phi = dot(d, e2).atan2(dot(d, e1)).rem_euclid(TAU);
        (phi, dot(d, a).clamp(-1.0, 1.0).acos())
    }
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

fn normalise(v: [f64; 3]) -> [f64; 3] {
    let n = dot(v, v).sqrt();
    v.map(|x| x / n)
}
