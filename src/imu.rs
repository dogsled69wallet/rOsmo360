//! Orientation and raw IMU series as telemetry-parser exposes them.

use telemetry_parser::Input;
use telemetry_parser::tags_impl::{GetWithType, GroupId, TagId, TimeQuaternion, TimeVector3};

#[derive(Default)]
pub struct Imu {
    /// Fused attitude, (ms, [w, x, y, z]), in telemetry-parser's IMU frame.
    pub quats: Vec<(f64, [f64; 4])>,
    /// (s, rad/s)
    pub gyro: Vec<(f64, [f64; 3])>,
    /// (s, g)
    pub accel: Vec<(f64, [f64; 3])>,
}

pub fn read(input: &Input) -> Imu {
    let mut imu = Imu::default();
    for sample in input.samples.iter().flatten() {
        let Some(tags) = &sample.tag_map else { continue };
        if let Some(q) = tags.get(&GroupId::Quaternion).and_then(|m| m.get_t(TagId::Data) as Option<&Vec<TimeQuaternion<f64>>>) {
            imu.quats.extend(q.iter().map(|q| (q.t, [q.v.w, q.v.x, q.v.y, q.v.z])));
        }
        let vec3 = |g: GroupId| {
            tags.get(&g)
                .and_then(|m| m.get_t(TagId::Data) as Option<&Vec<TimeVector3<f64>>>)
                .map(|v| v.iter().map(|v| (v.t, [v.x, v.y, v.z])).collect::<Vec<_>>())
                .unwrap_or_default()
        };
        imu.gyro.extend(vec3(GroupId::Gyroscope));
        imu.accel.extend(vec3(GroupId::Accelerometer));
    }
    imu
}
