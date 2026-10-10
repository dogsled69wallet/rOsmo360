mod align;
mod exposure;
mod ffmpeg;
mod imu;
mod player;
mod ring;
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
    /// Play clips in a window with free look, stabilisation and rolling-shutter correction.
    ///
    /// Drag: look around · right-drag or Q/E: roll · arrows: pitch, seek (shift: yaw) ·
    /// scroll: zoom · space: play/pause · , .: frame step · S: stabilisation ·
    /// R: rolling shutter · P: projection · M: mute · 0: reset view · F: fullscreen · Ctrl+O: open
    Play {
        /// Clip to open at start; otherwise use the Open button.
        input: Option<PathBuf>,
        /// Stream the OSV itself at this size (pixels square) instead of the LRF proxy.
        #[arg(long)]
        osv_size: Option<u32>,
        /// Video decoder; the first that works, in this order, if omitted.
        #[arg(long, value_enum)]
        hwaccel: Option<ffmpeg::Hwaccel>,
        #[arg(long, value_enum, default_value_t = stab::Mode::Horizon)]
        stabilize: stab::Mode,
        #[arg(long, value_enum, default_value_t = stab::ImuAxes::X180)]
        imu_axes: stab::ImuAxes,
        #[arg(long)]
        world_z_down: bool,
    },
    /// Report how well the seam alignment works over a span: what ghosting
    /// is left (lens disagreement across the blend band) and how much the
    /// shift wobbles from frame to frame.
    Seam {
        input: PathBuf,
        /// Start time in seconds.
        #[arg(short, long, default_value_t = 0.0)]
        start: f64,
        /// Duration in seconds.
        #[arg(short, long, default_value_t = 5.0)]
        duration: f64,
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
    /// Skip evening out the lenses' exposure at the seam.
    #[arg(long)]
    no_exposure: bool,
    /// Skip shifting the lenses to line up close subjects at the seam.
    #[arg(long)]
    no_align: bool,
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
    seam: Option<exposure::Seam>,
    aligner: Option<align::Aligner>,
    hw: ffmpeg::Hwaccel,
}

/// Seam state carried from frame to frame so it eases instead of flickering.
#[derive(Default)]
struct Carry {
    gains: Option<exposure::Gains>,
    field: Option<align::Field>,
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
        let seam = if opts.no_exposure || opts.only.is_some() { None } else { exposure::Seam::new(&lenses) };
        let aligner = if opts.no_align || opts.only.is_some() { None } else { align::Aligner::new(&lenses) };
        let hw = ffmpeg::Hwaccel::pick(input, tracks[0])?;
        Ok(Pipeline {
            lenses,
            tracks,
            stab,
            readout_ms,
            fps: ffmpeg::frame_rate(input)?,
            width: opts.width,
            only: opts.only,
            rs_steps,
            seam,
            aligner,
            hw,
        })
    }

    /// Rounds to the nearest frame so attitude lookups match decoded pictures.
    fn snap(&self, t: f64) -> f64 {
        (t * self.fps).round() / self.fps
    }

    /// Stitches one frame, measuring the seam afresh and easing towards it.
    fn render(&self, frames: &[stitch::Frame], t: f64, carry: &mut Carry) -> stitch::Frame {
        let views = if self.stab.has_data() {
            self.stab.rig_from_view(t * 1000.0, self.readout_ms, self.rs_steps)
        } else {
            vec![[[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]]
        };
        let rgb = |i: usize, u: f64, v: f64| stitch::sample(&frames[self.lenses[i].input], u, v).map(|c| c / 255.0);
        let exposure = self.seam.as_ref().map(|seam| {
            let now = seam.measure(|i, u, v| Some(rgb(i, u, v)));
            let g = carry.gains.get_or_insert(now);
            g.ease(&now, exposure::EASE);
            (seam, *g)
        });
        let align = self.aligner.as_ref().map(|aligner| {
            let now = aligner.measure(&aligner.strips(|i, u, v| Some(rgb(i, u, v).iter().sum::<f64>() / 3.0)), carry.field.as_ref());
            let f = carry.field.get_or_insert_with(|| now.clone());
            f.ease(&now, align::EASE);
            (aligner, f.clone())
        });
        let seam = stitch::Seam { exposure: exposure.as_ref().map(|(s, g)| (*s, g)), align: align.as_ref().map(|(a, f)| (*a, f)) };
        stitch::equirect(&self.lenses, frames, self.width, self.only, &views, &seam)
    }

    fn decoders(&self, input: &Path, start: f64) -> Result<Vec<ffmpeg::Decoder>> {
        self.tracks.iter().map(|&n| ffmpeg::Decoder::new(input, n, start, self.hw)).collect()
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
        Cmd::Play { input, osv_size, hwaccel, stabilize, imu_axes, world_z_down } => {
            player::run(input, player::Options { osv_size, hw: hwaccel, stabilize, imu_axes, world_z_down })?;
        }
        Cmd::Seam { input, start, duration } => seam_report(&input, start, duration)?,
        Cmd::Stitch { input, time, output, opts } => {
            let p = Pipeline::new(&input, &opts)?;
            let time = p.snap(time);
            let frames = p.tracks.iter().map(|&n| ffmpeg::still(&input, n, time, p.hw)).collect::<Result<Vec<_>>>()?;
            let out = p.render(&frames, time, &mut Carry::default());
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
            let mut carry = Carry::default();
            let mut n = 0;
            while total.is_none_or(|total| n < total) {
                let Some(frames) = next_frames(&mut decoders)? else { break };
                encoder.write(&p.render(&frames, start + n as f64 / p.fps, &mut carry))?;
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

fn seam_report(input: &Path, start: f64, duration: f64) -> Result<()> {
    let opts = RenderOpts {
        width: 0,
        variant: None,
        only: None,
        stabilize: stab::Mode::Off,
        smooth: 500.0,
        imu_axes: stab::ImuAxes::X180,
        world_z_down: false,
        no_rs: true,
        no_exposure: true,
        no_align: false,
    };
    let p = Pipeline::new(input, &opts)?;
    let aligner = p.aligner.as_ref().context("not a two-lens rig")?;
    let start = p.snap(start);
    let mut decoders = p.decoders(input, start)?;
    let (mut eased, mut prev_raw): (Option<align::Field>, Option<align::Field>) = (None, None);
    let mut sums = [0.0; 5];
    let mut n = 0;
    let jitter = |a: &align::Field, b: &align::Field| a.0.iter().zip(&b.0).map(|(x, y)| (x - y).abs() as f64).sum::<f64>() / a.0.len() as f64;
    println!("t,mismatch_none,mismatch_raw,mismatch_eased,jitter_raw_deg,jitter_eased_deg");
    while n < (duration * p.fps).round() as usize {
        let Some(frames) = next_frames(&mut decoders)? else { break };
        let strips = aligner.strips(|i, u, v| Some(stitch::sample(&frames[p.lenses[i].input], u, v).iter().sum::<f64>() / (3.0 * 255.0)));
        let raw = aligner.measure(&strips, eased.as_ref());
        let before = eased.clone();
        let e = eased.get_or_insert_with(|| raw.clone());
        e.ease(&raw, align::EASE);
        let row = [
            aligner.mismatch(&strips, &align::Field::default()),
            aligner.mismatch(&strips, &raw),
            aligner.mismatch(&strips, e),
            prev_raw.as_ref().map_or(0.0, |r| jitter(r, &raw).to_degrees()),
            before.as_ref().map_or(0.0, |b| jitter(b, e).to_degrees()),
        ];
        println!("{:.3},{:.5},{:.5},{:.5},{:.4},{:.4}", start + n as f64 / p.fps, row[0], row[1], row[2], row[3], row[4]);
        for (s, v) in sums.iter_mut().zip(row) {
            *s += v;
        }
        prev_raw = Some(raw);
        n += 1;
    }
    let m = sums.map(|s| s / n.max(1) as f64);
    eprintln!("{n} frames: mismatch none {:.5} raw {:.5} eased {:.5}; jitter raw {:.4}° eased {:.4}°", m[0], m[1], m[2], m[3], m[4]);
    Ok(())
}

fn load_input(input: &Path) -> Result<telemetry_parser::Input> {
    let mut f = File::open(input)?;
    let size = f.metadata()?.len() as usize;
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    Ok(telemetry_parser::Input::from_stream(&mut f, size, input, |_| {}, cancel)?)
}
