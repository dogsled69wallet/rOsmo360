mod ffmpeg;
mod imu;
mod stab;
mod stitch;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Instant;
use telemetry_parser::rig::{RigPicture, geometry::ProjectionKind};

#[derive(Parser)]
#[command(about = "Tools for DJI Osmo 360 .OSV files")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Dump the camera rig (DJI Studio's default stitch geometry) as JSON.
    Rig { input: PathBuf },
    /// Dump orientation (q), gyro (g) and accelerometer (a) samples as CSV.
    Imu { input: PathBuf },
    /// Stitch one frame to an equirectangular PNG.
    Stitch {
        input: PathBuf,
        /// Time in seconds.
        #[arg(short, long, default_value_t = 0.0)]
        time: f64,
        #[arg(short, long, default_value = "equirect.png")]
        output: PathBuf,
        #[command(flatten)]
        opts: RenderOpts,
    },
    /// Stitch a span of the clip to an equirectangular MP4.
    Render {
        input: PathBuf,
        /// Start time in seconds.
        #[arg(short, long, default_value_t = 0.0)]
        start: f64,
        /// Duration in seconds; the rest of the clip if omitted.
        #[arg(short, long)]
        duration: Option<f64>,
        #[arg(short, long, default_value = "equirect.mp4")]
        output: PathBuf,
        #[command(flatten)]
        opts: RenderOpts,
    },
}

#[derive(Args)]
struct RenderOpts {
    /// Output width; height is width / 2.
    #[arg(short, long, default_value_t = 4096)]
    width: usize,
    /// Use this alternative calibration (see `rig`'s `variants`) instead of the default.
    #[arg(long)]
    variant: Option<usize>,
    /// Render only this lens (0 or 1), for checking overlap alignment.
    #[arg(long)]
    only: Option<usize>,
    /// Stabilisation mode, from the camera's attitude data.
    #[arg(long, value_enum, default_value_t = stab::Mode::Off)]
    stabilize: stab::Mode,
    /// Heading smoothing window for `horizon`, in ms either side.
    #[arg(long, default_value_t = 500.0)]
    smooth: f64,
    /// How the IMU's axes sit relative to the rig's.
    #[arg(long, value_enum, default_value_t = stab::ImuAxes::X180)]
    imu_axes: stab::ImuAxes,
    /// Treat the attitude's world z as pointing down rather than up.
    #[arg(long)]
    world_z_down: bool,
    /// Skip rolling-shutter correction.
    #[arg(long)]
    no_rs: bool,
}

/// Everything needed to turn decoded lens frames at a given time into an
/// equirect frame.
struct Pipeline {
    lenses: Vec<stitch::Lens>,
    tracks: Vec<u32>,
    stab: stab::Stabilizer,
    readout_ms: f64,
    fps: f64,
    width: usize,
    only: Option<usize>,
    rs_steps: usize,
}

impl Pipeline {
    fn new(input: &Path, opts: &RenderOpts) -> Result<Self> {
        let parsed = load_input(input)?;
        let rig = telemetry_parser::util::camera_rig(&parsed).context("no camera rig in file")?;
        let projection = match opts.variant {
            Some(i) => &rig.variants.get(i).context("no such variant")?.projection,
            None => &rig.projection,
        };
        let ProjectionKind::Rig { cameras, .. } = &projection.kind else {
            bail!("not a multi-lens rig");
        };
        let lenses = cameras.iter().map(stitch::Lens::new).collect::<Result<Vec<_>>>()?;
        let tracks = rig
            .pictures
            .iter()
            .map(|p| match p {
                RigPicture::VideoTrack(n) => Ok(*n),
                RigPicture::Sibling { .. } => bail!("multi-file rigs are not supported"),
            })
            .collect::<Result<Vec<_>>>()?;
        let stab = stab::Stabilizer::new(&imu::read(&parsed).quats, opts.stabilize, opts.imu_axes, if opts.world_z_down { 1.0 } else { -1.0 }, opts.smooth);
        if !stab.has_data() && opts.stabilize != stab::Mode::Off {
            bail!("no attitude data to stabilise with");
        }
        let readout_ms = cameras[0].readout.map_or(0.0, |r| r.time_us / 1000.0);
        let rs_steps = if opts.no_rs || readout_ms == 0.0 || !stab.has_data() { 1 } else { 33 };
        Ok(Pipeline {
            lenses,
            tracks,
            stab,
            readout_ms,
            fps: ffmpeg::frame_rate(input)?,
            width: opts.width,
            only: opts.only,
            rs_steps,
        })
    }

    /// Rounds to the nearest frame so attitude lookups match decoded pictures.
    fn snap(&self, t: f64) -> f64 {
        (t * self.fps).round() / self.fps
    }

    fn render(&self, frames: &[stitch::Frame], t: f64) -> stitch::Frame {
        let views = if self.stab.has_data() {
            self.stab.rig_from_view(t * 1000.0, self.readout_ms, self.rs_steps)
        } else {
            vec![[[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]]
        };
        stitch::equirect(&self.lenses, frames, self.width, self.only, &views)
    }

    fn decoders(&self, input: &Path, start: f64) -> Result<Vec<ffmpeg::Decoder>> {
        self.tracks.iter().map(|&n| ffmpeg::Decoder::new(input, n, start)).collect()
    }
}

/// Pulls one frame from every decoder; `None` once any of them ends.
fn next_frames(decoders: &mut [ffmpeg::Decoder]) -> Result<Option<Vec<stitch::Frame>>> {
    decoders.iter_mut().map(|d| d.next()).collect::<Result<Option<Vec<_>>>>()
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Rig { input } => {
            let rig = telemetry_parser::util::camera_rig(&load_input(&input)?).context("no camera rig in file")?;
            println!("{}", serde_json::to_string_pretty(&rig)?);
        }
        Cmd::Imu { input } => {
            let imu = imu::read(&load_input(&input)?);
            println!("kind,t,w,x,y,z");
            for (t, q) in &imu.quats {
                println!("q,{t},{},{},{},{}", q[0], q[1], q[2], q[3]);
            }
            for (kind, series) in [("g", &imu.gyro), ("a", &imu.accel)] {
                for (t, v) in series {
                    println!("{kind},{t},,{},{},{}", v[0], v[1], v[2]);
                }
            }
        }
        Cmd::Stitch { input, time, output, opts } => {
            let p = Pipeline::new(&input, &opts)?;
            let time = p.snap(time);
            let frames = next_frames(&mut p.decoders(&input, time)?)?.context("no frame at that time")?;
            let out = p.render(&frames, time);
            image::save_buffer(&output, &out.rgb, out.width as u32, out.height as u32, image::ColorType::Rgb8)?;
            eprintln!("wrote {}", output.display());
        }
        Cmd::Render { input, start, duration, output, opts } => {
            let p = Pipeline::new(&input, &opts)?;
            let start = p.snap(start);
            let mut decoders = p.decoders(&input, start)?;
            let mut encoder = ffmpeg::Encoder::new(&output, (p.width, p.width / 2), p.fps, &input, start)?;
            let total = duration.map(|d| (d * p.fps).round() as usize);
            let began = Instant::now();
            let mut n = 0;
            while total.is_none_or(|total| n < total) {
                let Some(frames) = next_frames(&mut decoders)? else { break };
                encoder.write(&p.render(&frames, start + n as f64 / p.fps))?;
                n += 1;
                if n % 10 == 0 {
                    eprint!("\rframe {n}{}  {:.2} fps", total.map_or(String::new(), |t| format!("/{t}")), n as f64 / began.elapsed().as_secs_f64());
                }
            }
            encoder.finish()?;
            eprintln!("\nwrote {} ({n} frames)", output.display());
        }
    }
    Ok(())
}

fn load_input(input: &Path) -> Result<telemetry_parser::Input> {
    let mut f = File::open(input)?;
    let size = f.metadata()?.len() as usize;
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    Ok(telemetry_parser::Input::from_stream(&mut f, size, input, |_| {}, cancel)?)
}
