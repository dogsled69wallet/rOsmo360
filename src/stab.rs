//! Orientation-based stabilisation. Produces `rig_from_view` rotations: a
//! view-frame ray (x right, y down, z forward) is turned into the rig ray the
//! stitcher samples.
//!
//! The attitude series is world_from_imu. On the Osmo 360 (telemetry-parser's
//! series) world +z points up and the IMU sits a half turn about the rig's x
//! axis — found on handheld footage by world-locking across fast pans until
//! the sun held still, then checking the horizon levels at 30–50° of tilt.

use crate::stitch::{M3, apply, mat_mul, quat_to_mat, transpose};
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// No stabilisation: the view is the rig.
    Off,
    /// Level horizon, heading follows the camera's smoothed heading.
    Horizon,
    /// Level horizon, heading fixed where the camera pointed at the start
    /// of the clip: yaw is a direction in the world (`scan`'s heading), so a
    /// view stays on a landmark while the camera turns.
    Level,
    /// Fully world-locked to the orientation at the start of the clip,
    /// tilt included.
    Lock,
}

impl Mode {
    pub const ALL: [Mode; 4] = [Mode::Off, Mode::Horizon, Mode::Level, Mode::Lock];
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Mode::Off => "Off",
            Mode::Horizon => "Horizon",
            Mode::Level => "Level",
            Mode::Lock => "World lock",
        })
    }
}

/// The stick's direction is averaged over this many ms either side, so
/// hand shake doesn't shake a view that follows it.
const STICK_SMOOTH_MS: f64 = 250.0;

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
            Mode::Level => {
                let r = self.world_from_rig(self.quats[0].0);
                self.levelled([r[0][2], r[1][2], r[2][2]])
            }
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
                self.levelled(fwd)
            }
        }
    }

    /// A level world_from_view looking along `fwd` flattened onto the horizon.
    fn levelled(&self, mut fwd: [f64; 3]) -> M3 {
        let down = [0.0, 0.0, self.world_down];
        fwd[2] = 0.0;
        let fwd = normalise(fwd);
        let right = cross(down, fwd);
        // Columns are the view's x (right), y (down), z (forward) in world.
        transpose([right, down, fwd])
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

    /// Where the selfie stick points (rig +y, towards whoever holds it) in
    /// the view at frame time `t_ms`, as view yaw and pitch in degrees.
    /// `None` without attitude data.
    pub fn stick(&self, t_ms: f64, readout_ms: f64) -> Option<(f64, f64)> {
        if !self.has_data() {
            return None;
        }
        let mid = t_ms + readout_ms / 2.0;
        let n = 16;
        let mut world = [0.0; 3];
        for k in 0..=n {
            let d = self.to_world(mid + STICK_SMOOTH_MS * (2.0 * k as f64 / n as f64 - 1.0), [0.0, 1.0, 0.0]);
            for i in 0..3 {
                world[i] += d[i];
            }
        }
        Some(self.view_angles(t_ms, readout_ms, world))
    }

    /// Rig direction `d` at `t_ms` as a world direction.
    pub fn to_world(&self, t_ms: f64, d: [f64; 3]) -> [f64; 3] {
        apply(self.world_from_rig(t_ms), d)
    }

    /// World direction `w` as a rig direction at `t_ms`.
    pub fn to_rig(&self, t_ms: f64, w: [f64; 3]) -> [f64; 3] {
        apply(transpose(self.world_from_rig(t_ms)), w)
    }

    /// World direction `w` as view yaw and pitch in degrees at frame time
    /// `t_ms`, for a view to look at it.
    ///
    /// Near straight below, sideways wobble turns into large yaw swings
    /// that spin the picture; smoothing over time (`w` comes averaged) is
    /// what calms them. Leaning the yaw towards straight ahead there made it
    /// worse: with the subject behind the camera it flipped half way round
    /// (up to 53° in one frame on 0025), and even the stick turned 0.90°/frame
    /// RMS with it against 0.77° without.
    pub fn view_angles(&self, t_ms: f64, readout_ms: f64, w: [f64; 3]) -> (f64, f64) {
        let d = normalise(apply(transpose(self.world_from_view(t_ms + readout_ms / 2.0)), w));
        // View x right, y down, z forward: yaw turns right, pitch looks up.
        (d[0].atan2(d[2]).to_degrees(), (-d[1]).clamp(-1.0, 1.0).asin().to_degrees())
    }

    /// The camera's pose at `t_ms`: its heading in degrees, turning the same
    /// way as the view's yaw and 0 where it pointed when the clip began (so
    /// in `lock` mode yaw = heading looks where it pointed), and how far the
    /// stick axis leans from vertical.
    pub fn pose(&self, t_ms: f64) -> Pose {
        let down = [0.0, 0.0, self.world_down];
        let column = |m: M3, j: usize| [m[0][j], m[1][j], m[2][j]];
        let dot = |a: [f64; 3], b: [f64; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
        let start = column(self.world_from_rig(self.quats[0].0), 2);
        let f0 = normalise([start[0], start[1], 0.0]);
        let r0 = cross(down, f0);
        let rig = self.world_from_rig(t_ms);
        let f = column(rig, 2);
        Pose {
            heading: dot(f, r0).atan2(dot(f, f0)).to_degrees(),
            tilt: dot(column(rig, 1), down).clamp(-1.0, 1.0).acos().to_degrees(),
            rig,
        }
    }
}

pub struct Pose {
    pub heading: f64,
    pub tilt: f64,
    /// world_from_rig.
    pub rig: M3,
}

impl Pose {
    /// The angle in degrees the camera turned through from `self` to `next`.
    pub fn turn_to(&self, next: &Pose) -> f64 {
        let r = mat_mul(transpose(self.rig), next.rig);
        ((r[0][0] + r[1][1] + r[2][2] - 1.0) / 2.0).clamp(-1.0, 1.0).acos().to_degrees()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_holds_the_starting_heading() {
        // A camera turning 90° about world z (up) over a second, tilted 30°.
        let quat = |yaw: f64| {
            let (y, t) = ((yaw / 2.0).to_radians(), (15.0f64).to_radians());
            // Yaw about z, then tilt about x.
            let (qz, qx) = ([y.cos(), 0.0, 0.0, y.sin()], [t.cos(), t.sin(), 0.0, 0.0]);
            [
                qz[0] * qx[0] - qz[3] * 0.0 - qz[1] * qx[1],
                qz[0] * qx[1] + qz[1] * qx[0],
                qz[3] * qx[1],
                qz[3] * qx[0],
            ]
        };
        let quats: Vec<_> = (0..=10).map(|k| (k as f64 * 100.0, quat(9.0 * k as f64))).collect();
        let level = Stabilizer::new(&quats, Mode::Level, ImuAxes::X180, -1.0, 0.0);
        let horizon = Stabilizer::new(&quats, Mode::Horizon, ImuAxes::X180, -1.0, 0.0);
        let (a, b) = (level.world_from_view(0.0), level.world_from_view(1000.0));
        assert!(a.iter().flatten().zip(b.iter().flatten()).all(|(x, y)| (x - y).abs() < 1e-9));
        // Level: the view's down is the world's (z up, so down is −z).
        assert!((a[2][1] + 1.0).abs() < 1e-9);
        // Horizon turned with the camera, by its 90°.
        let (h0, h1) = (horizon.world_from_view(0.0), horizon.world_from_view(1000.0));
        let cos = h0[0][2] * h1[0][2] + h0[1][2] * h1[1][2];
        assert!(cos.abs() < 1e-6, "{cos}");
    }
}
