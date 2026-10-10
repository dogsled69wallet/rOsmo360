//! Orientation-based stabilisation. Produces `rig_from_view` rotations: a
//! view-frame ray (x right, y down, z forward) is turned into the rig ray the
//! stitcher samples.
//!
//! The attitude series is world_from_imu. On the Osmo 360 (telemetry-parser's
//! series) world +z points up and the IMU sits a half turn about the rig's x
//! axis — found on handheld footage by world-locking across fast pans until
//! the sun held still, then checking the horizon levels at 30–50° of tilt.

use crate::stitch::{M3, mat_mul, quat_to_mat, transpose};
use telemetry_parser::rig::geometry::Quat;

/// How the IMU's axes sit relative to the rig's: identity or a half turn
/// about one rig axis.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum ImuAxes {
    Identity,
    X180,
    Y180,
    Z180,
}

impl ImuAxes {
    fn imu_from_rig(self) -> M3 {
        let d = match self {
            ImuAxes::Identity => [1.0, 1.0, 1.0],
            ImuAxes::X180 => [1.0, -1.0, -1.0],
            ImuAxes::Y180 => [-1.0, 1.0, -1.0],
            ImuAxes::Z180 => [-1.0, -1.0, 1.0],
        };
        [[d[0], 0.0, 0.0], [0.0, d[1], 0.0], [0.0, 0.0, d[2]]]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Mode {
    /// No stabilisation: the view is the rig.
    Off,
    /// Level horizon, heading follows the camera's smoothed heading.
    Horizon,
    /// Fully world-locked to the orientation at the start of the clip.
    Lock,
}

impl Mode {
    pub const ALL: [Mode; 3] = [Mode::Off, Mode::Horizon, Mode::Lock];
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Mode::Off => "Off",
            Mode::Horizon => "Horizon",
            Mode::Lock => "World lock",
        })
    }
}

#[derive(Clone)]
pub struct Stabilizer {
    /// (ms, world_from_imu)
    quats: Vec<(f64, Quat)>,
    imu_from_rig: M3,
    /// Sign of world z along gravity: +1 for z down (NED), -1 for z up.
    world_down: f64,
    mode: Mode,
    smooth_ms: f64,
}

impl Stabilizer {
    pub fn new(quats: &[(f64, [f64; 4])], mode: Mode, axes: ImuAxes, world_down: f64, smooth_ms: f64) -> Self {
        let quats = quats.iter().map(|&(t, [w, x, y, z])| (t, Quat { w, x, y, z })).collect();
        Stabilizer { quats, imu_from_rig: axes.imu_from_rig(), world_down, mode, smooth_ms }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    pub fn has_data(&self) -> bool {
        !self.quats.is_empty()
    }

    fn world_from_rig(&self, t_ms: f64) -> M3 {
        mat_mul(quat_to_mat(self.attitude(t_ms)), self.imu_from_rig)
    }

    /// Slerped attitude at `t_ms`, clamped to the ends of the series.
    fn attitude(&self, t_ms: f64) -> Quat {
        let q = &self.quats;
        let i = q.partition_point(|(t, _)| *t < t_ms);
        if i == 0 {
            return q[0].1;
        }
        if i == q.len() {
            return q[q.len() - 1].1;
        }
        let ((t0, a), (t1, b)) = (q[i - 1], q[i]);
        slerp(a, b, ((t_ms - t0) / (t1 - t0).max(1e-9)).clamp(0.0, 1.0))
    }

    /// The world_from_view basis at frame time `t_ms`.
    fn world_from_view(&self, t_ms: f64) -> M3 {
        match self.mode {
            Mode::Off => self.world_from_rig(t_ms),
            Mode::Lock => self.world_from_rig(self.quats[0].0),
            Mode::Horizon => {
                // Average the rig's forward axis over the window, then level it.
                let n = 32;
                let mut fwd = [0.0; 3];
                for k in 0..=n {
                    let t = t_ms + self.smooth_ms * (2.0 * k as f64 / n as f64 - 1.0);
                    let r = self.world_from_rig(t);
                    for i in 0..3 {
                        fwd[i] += r[i][2];
                    }
                }
                let down = [0.0, 0.0, self.world_down];
                fwd[2] = 0.0;
                let fwd = normalise(fwd);
                let right = cross(down, fwd);
                // Columns are the view's x (right), y (down), z (forward) in world.
                transpose([right, down, fwd])
            }
        }
    }

    /// `rig_from_view` at `steps` instants spread over the frame's readout,
    /// first row to last. A single step means no rolling-shutter correction.
    ///
    /// A frame's timestamp is when its readout starts: centring the readout
    /// on it instead left distant terrain swimming by ~0.1-0.5° per frame in
    /// world lock, and the residual is smallest at exactly half the readout.
    pub fn rig_from_view(&self, t_ms: f64, readout_ms: f64, steps: usize) -> Vec<M3> {
        let view = self.world_from_view(t_ms + readout_ms / 2.0);
        (0..steps)
            .map(|k| {
                let s = if steps > 1 { k as f64 / (steps - 1) as f64 } else { 0.5 };
                let rig = self.world_from_rig(t_ms + s * readout_ms);
                mat_mul(transpose(rig), view)
            })
            .collect()
    }
}

fn slerp(a: Quat, mut b: Quat, t: f64) -> Quat {
    let mut dot = a.w * b.w + a.x * b.x + a.y * b.y + a.z * b.z;
    if dot < 0.0 {
        b = Quat { w: -b.w, x: -b.x, y: -b.y, z: -b.z };
        dot = -dot;
    }
    let (wa, wb) = if dot > 0.9995 {
        (1.0 - t, t)
    } else {
        let th = dot.acos();
        ((((1.0 - t) * th).sin()) / th.sin(), (t * th).sin() / th.sin())
    };
    let q = [wa * a.w + wb * b.w, wa * a.x + wb * b.x, wa * a.y + wb * b.y, wa * a.z + wb * b.z];
    let n = q.iter().map(|v| v * v).sum::<f64>().sqrt();
    Quat { w: q[0] / n, x: q[1] / n, y: q[2] / n, z: q[3] / n }
}

fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

fn normalise(v: [f64; 3]) -> [f64; 3] {
    let n = v.iter().map(|x| x * x).sum::<f64>().sqrt().max(1e-12);
    v.map(|x| x / n)
}
