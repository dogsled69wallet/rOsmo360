mod align;
mod exposure;
mod ffmpeg;
mod imu;
mod player;
mod project;
mod ring;
mod sheet;
mod stab;
mod stitch;
mod view;

use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use clap::{Args, Parser, Subcommand};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Instant;
use view::{Projection, Reframe, View};
use telemetry_parser::rig::{RigPicture, geometry::ProjectionKind};

#[derive(Parser)]
#[command(name = player::NAME, version, about = "rOsmo360: play and stitch DJI Osmo 360 .OSV files")]
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
    /// The clip's length, frame rate, recording time and what it holds, as JSON.
    Info { input: PathBuf },
    /// Stitch one frame to an equirectangular PNG.
    Stitch {
        input: PathBuf,
        /// Time in seconds.
        #[arg(short, long, default_value_t = 0.0)]
        time: f64,
        #[arg(short, long, default_value = "equirect.png")]
        output: PathBuf,
        /// Output width; height is width / 2.
        #[arg(short, long, default_value_t = 4096)]
        width: usize,
        #[command(flatten)]
        opts: StitchOpts,
    },
    /// Play clips in a window with free look, stabilisation and rolling-shutter correction.
    ///
    /// Drag: look around · right-drag or Q/E: roll · arrows: pitch, seek (shift: yaw) ·
    /// scroll: zoom · space: play/pause · , .: frame step · S: stabilisation ·
    /// R: rolling shutter · P: projection · M: mute · 0: reset view · F: fullscreen · Ctrl+O: open ·
    /// K: keyframe the view here · [ ]: previous/next keyframe · Del: delete keyframe · J: its ease ·
    /// C: split the cut · Shift+Del: delete the selected cut · Ctrl+Z / Ctrl+Shift+Z: undo / redo ·
    /// Ctrl+= / Ctrl+− / Ctrl+0: zoom the timeline in / out / to the whole edit (or scroll on it)
    Play {
        /// Clips to open at start, one cut each in this order, or a saved
        /// project (.json); otherwise use the Open buttons.
        input: Vec<PathBuf>,
        /// Stream the OSV itself at this size (pixels square) instead of the
        /// LRF proxy [default: the last source used].
        #[arg(long)]
        osv_size: Option<u32>,
        /// Video decoder; the first that works, in this order, if omitted.
        #[arg(long, value_enum)]
        hwaccel: Option<ffmpeg::Hwaccel>,
        /// Stabilisation mode [default: the last one used, at first horizon].
        #[arg(long, value_enum)]
        stabilize: Option<stab::Mode>,
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
        /// Write the first frame's seam strips, differences and shift field
        /// as images into this directory.
        #[arg(long)]
        dump: Option<PathBuf>,
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
        /// Output width; height is width / 2.
        #[arg(short, long, default_value_t = 4096)]
        width: usize,
        #[command(flatten)]
        opts: StitchOpts,
    },
    /// Render a framed (reframed) video with sound: a clip with a fixed view
    /// or keyframed views from a JSON file, or a project (.json) of cuts from
    /// several clips, each with its own keyframes.
    ///
    /// For a clip without -s/-d, the keyframes' span is rendered.
    /// Stabilisation defaults to horizon.
    Export {
        /// A clip (.OSV), or a project (.json): {"aspect": "9:16", "cuts":
        /// [{"input": "a.OSV", "in": 28, "out": 37, "keyframes": [...]}, ...]}.
        input: PathBuf,
        #[command(flatten)]
        framing: Framing,
        /// Start time in seconds.
        #[arg(short, long)]
        start: Option<f64>,
        /// Duration in seconds.
        #[arg(short, long)]
        duration: Option<f64>,
        /// Picture height for landscape, width for portrait: 1080p is
        /// 1920×1080 at 16:9 and 1080×1920 at 9:16.
        #[arg(short, long, value_enum, default_value_t = Resolution::P1080)]
        resolution: Resolution,
        /// Shape, W:H (16:9, 9:16, 1:1, 4:5, ...); else the reframe's, else 16:9.
        #[arg(short, long)]
        aspect: Option<view::Aspect>,
        /// Exact size, WIDTHxHEIGHT, instead of a resolution and aspect.
        #[arg(long, value_parser = parse_size, conflicts_with_all = ["resolution", "aspect"])]
        size: Option<[u32; 2]>,
        #[arg(short, long, default_value = "clip.mp4")]
        output: PathBuf,
        /// Stitch from the low-resolution proxy (LRF): a quick, soft draft.
        #[arg(long)]
        draft: bool,
        /// Stitch on the CPU, the reference the GPU shader must match.
        /// Without a usable GPU this happens anyway.
        #[arg(long)]
        cpu: bool,
        #[command(flatten)]
        opts: StitchOpts,
    },
    /// Stitch evenly spaced moments onto one contact-sheet PNG, each tile
    /// labelled with its time, to skim a clip. Tiles show the whole sphere,
    /// marked at yaw −90/0/90° and the horizon, unless a framing is given.
    ///
    /// Given a project (.json), one row per cut (more with --every), each
    /// tile framed as the export frames it and labelled with the cut's
    /// number and the clip time.
    ///
    /// Uses the proxy (LRF) unless --full. Stabilisation defaults to horizon.
    Preview {
        input: PathBuf,
        #[command(flatten)]
        framing: Framing,
        /// Start time in seconds.
        #[arg(short, long)]
        start: Option<f64>,
        /// Duration in seconds.
        #[arg(short, long)]
        duration: Option<f64>,
        /// Seconds between tiles; else 24 tiles, or the keyframes' times.
        #[arg(long)]
        every: Option<f64>,
        /// Tile width in pixels.
        #[arg(long, default_value_t = 640)]
        tile: usize,
        #[arg(long, default_value_t = 4)]
        columns: usize,
        #[arg(short, long, default_value = "preview.png")]
        output: PathBuf,
        /// Stitch from the full-resolution lens tracks instead of the proxy.
        #[arg(long)]
        full: bool,
        #[command(flatten)]
        opts: StitchOpts,
    },
    /// Summarise the clip over time as CSV, to find moments worth a look:
    /// the camera's heading and tilt, how fast it turns or shakes, and how
    /// loud it is.
    Scan {
        input: PathBuf,
        /// Seconds per row.
        #[arg(long, default_value_t = 1.0)]
        every: f64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, clap::ValueEnum, serde::Serialize, serde::Deserialize)]
enum Resolution {
    #[value(name = "720p")]
    #[serde(rename = "720p")]
    P720,
    #[value(name = "1080p")]
    #[serde(rename = "1080p")]
    P1080,
    #[value(name = "1440p")]
    #[serde(rename = "1440p")]
    P1440,
    #[value(name = "2160p", alias = "4k")]
    #[serde(rename = "2160p")]
    P2160,
}

impl Resolution {
    const ALL: [Resolution; 4] = [Resolution::P720, Resolution::P1080, Resolution::P1440, Resolution::P2160];

    /// Pixels on the picture's short side.
    fn short_side(self) -> u32 {
        match self {
            Resolution::P720 => 720,
            Resolution::P1080 => 1080,
            Resolution::P1440 => 1440,
            Resolution::P2160 => 2160,
        }
    }
}

impl std::fmt::Display for Resolution {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}p", self.short_side())
    }
}

/// How `export` and `preview` frame the sphere: keyframes from a file or a
/// fixed view.
#[derive(Args)]
struct Framing {
    /// Keyframed views as JSON: {"aspect": "9:16", "keyframes": [{"t": 12,
    /// "yaw": 40, "pitch": -5, "fov": 80, "projection": "perspective", "ease":
    /// "smooth"}, ...]}. Keyframes may leave out what doesn't change.
    #[arg(long, conflicts_with = "view")]
    reframe: Option<PathBuf>,
    /// A fixed view: YAW,PITCH[,ROLL[,FOV]] in degrees.
    #[arg(long, value_parser = parse_view, allow_hyphen_values = true)]
    view: Option<Angles>,
    /// Projection for --view.
    #[arg(long, value_enum, default_value_t = Projection::Perspective, requires = "view")]
    projection: Projection,
}

impl Framing {
    fn reframe(&self) -> Result<Option<Reframe>> {
        if let Some(path) = &self.reframe {
            let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
            return Ok(Some(serde_json::from_str(&text).with_context(|| format!("reading {}", path.display()))?));
        }
        Ok(self.view.as_ref().map(|Angles(v)| {
            let home = View::home(self.projection);
            Reframe::fixed(View { yaw: v[0], pitch: v[1], roll: v.get(2).copied().unwrap_or(home.roll), fov: v.get(3).copied().unwrap_or(home.fov), ..home })
        }))
    }
}

/// --view's YAW,PITCH[,ROLL[,FOV]]. A newtype, since clap takes a bare
/// `Vec` for a flag given several times.
#[derive(Clone)]
struct Angles(Vec<f64>);

fn parse_view(s: &str) -> Result<Angles, String> {
    let v = s.split(',').map(|x| x.trim().parse::<f64>().map_err(|e| format!("{x:?}: {e}"))).collect::<Result<Vec<_>, _>>()?;
    if !(2..=4).contains(&v.len()) {
        return Err("expected YAW,PITCH[,ROLL[,FOV]]".into());
    }
    Ok(Angles(v))
}

fn parse_size(s: &str) -> Result<[u32; 2], String> {
    let (w, h) = s.split_once(['x', 'X']).ok_or("expected WIDTHxHEIGHT")?;
    let (w, h): (u32, u32) = (w.parse().map_err(|e| format!("{e}"))?, h.parse().map_err(|e| format!("{e}"))?);
    if w < 16 || h < 16 || w % 2 == 1 || h % 2 == 1 {
        return Err("width and height must be even and at least 16".into());
    }
    Ok([w, h])
}

#[derive(Args, Clone)]
struct StitchOpts {
    /// Use this alternative calibration (see `rig`'s `variants`) instead of the default.
    #[arg(long)]
    variant: Option<usize>,
    /// Render only this lens (0 or 1), for checking overlap alignment.
    #[arg(long)]
    only: Option<usize>,
    /// Stabilisation mode, from the camera's attitude data [default: off
    /// for stitch and render, horizon for export and preview].
    #[arg(long, value_enum)]
    stabilize: Option<stab::Mode>,
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
    /// Blend everywhere instead of cutting where the lenses disagree.
    #[arg(long)]
    no_cut: bool,
}

impl Default for StitchOpts {
    fn default() -> Self {
        StitchOpts {
            variant: None,
            only: None,
            stabilize: None,
            smooth: 500.0,
            imu_axes: stab::ImuAxes::X180,
            world_z_down: false,
            no_rs: false,
            no_exposure: false,
            no_align: false,
            no_cut: false,
        }
    }
}

/// What to draw from the stitched sphere.
enum Shot {
    /// The whole sphere, this wide.
    Equirect(usize),
    View { camera: view::Camera, size: (usize, usize) },
}

/// Everything needed to turn decoded lens frames at a given time into a
/// stitched picture.
struct Pipeline {
    input: PathBuf,
    lenses: Vec<stitch::Lens>,
    tracks: Vec<u32>,
    stab: stab::Stabilizer,
    readout_ms: f64,
    fps: f64,
    only: Option<usize>,
    rs_steps: usize,
    seam: Option<exposure::Seam>,
    aligner: Option<align::Aligner>,
    cut: bool,
    hw: ffmpeg::Hwaccel,
    /// Decode this side-by-side proxy instead of the lens tracks.
    proxy: Option<PathBuf>,
}

/// Seam state carried from frame to frame so it eases instead of flickering.
#[derive(Default)]
struct Carry {
    gains: Option<exposure::Gains>,
    field: Option<align::Field>,
}

impl Pipeline {
    /// `stabilize` is the mode when the options don't name one.
    fn new(input: &Path, opts: &StitchOpts, stabilize: stab::Mode) -> Result<Self> {
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
        let mode = opts.stabilize.unwrap_or(stabilize);
        let stab = stab::Stabilizer::new(&imu::read(&parsed).quats, mode, opts.imu_axes, if opts.world_z_down { 1.0 } else { -1.0 }, opts.smooth);
        if !stab.has_data() && mode != stab::Mode::Off {
            bail!("no attitude data to stabilise with");
        }
        let readout_ms = cameras[0].readout.map_or(0.0, |r| r.time_us / 1000.0);
        let rs_steps = if opts.no_rs || readout_ms == 0.0 || !stab.has_data() { 1 } else { 33 };
        let seam = if opts.no_exposure || opts.only.is_some() { None } else { exposure::Seam::new(&lenses) };
        let aligner = if opts.no_align || opts.only.is_some() { None } else { align::Aligner::new(&lenses) };
        let hw = ffmpeg::Hwaccel::pick(input, tracks[0])?;
        Ok(Pipeline {
            input: input.to_owned(),
            lenses,
            tracks,
            stab,
            readout_ms,
            fps: ffmpeg::frame_rate(input)?,
            only: opts.only,
            rs_steps,
            seam,
            aligner,
            cut: !opts.no_cut && opts.only.is_none(),
            hw,
            proxy: None,
        })
    }

    /// Decode from the clip's proxy from now on, if it has one.
    fn use_proxy(&mut self) {
        self.proxy = ffmpeg::proxy(&self.input);
        if self.proxy.is_none() {
            eprintln!("no proxy (LRF) next to the clip; using the full-resolution tracks");
        }
    }

    /// Rounds to the nearest frame so attitude lookups match decoded pictures.
    fn snap(&self, t: f64) -> f64 {
        (t * self.fps).round() / self.fps
    }

    /// Stitches one frame, measuring the seam afresh and easing towards it.
    fn render(&self, frames: &[stitch::Frame], t: f64, carry: &mut Carry, shot: &Shot) -> stitch::Frame {
        let views = if self.stab.has_data() { self.stab.rig_from_view(t * 1000.0, self.readout_ms, self.rs_steps) } else { vec![stitch::IDENTITY] };
        let rgb = |i: usize, u: f64, v: f64| self.lenses[i].sample(frames, u, v).map(|c| c / 255.0);
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
        let seam = stitch::Seam { exposure: exposure.as_ref().map(|(s, g)| (*s, g)), align: align.as_ref().map(|(a, f)| (*a, f)), cut: self.cut };
        match *shot {
            Shot::Equirect(width) => stitch::equirect(&self.lenses, frames, width, self.only, &views, &seam),
            Shot::View { camera, size: (w, h) } => {
                let ray = |x: f64, y: f64| stitch::apply(camera.user, camera.ray(w as f64 / h as f64, [x / w as f64 * 2.0 - 1.0, y / h as f64 * 2.0 - 1.0]));
                stitch::render(&self.lenses, frames, (w, h), ray, self.only, &views, &seam)
            }
        }
    }

    /// Every lens's frame at `t`.
    fn stills(&self, t: f64) -> Result<Vec<stitch::Frame>> {
        match &self.proxy {
            Some(proxy) => Ok(ffmpeg::split_proxy(&ffmpeg::still(proxy, 0, t, self.hw)?, &self.tracks)),
            None => self.tracks.iter().map(|&n| ffmpeg::still(&self.input, n, t, self.hw)).collect(),
        }
    }

    fn stream(&self, start: f64) -> Result<Stream> {
        Ok(match &self.proxy {
            Some(proxy) => Stream::Proxy(ffmpeg::Decoder::new(proxy, 0, start, self.hw)?, self.tracks.clone()),
            None => Stream::Tracks(self.tracks.iter().map(|&n| ffmpeg::Decoder::new(&self.input, n, start, self.hw)).collect::<Result<_>>()?),
        })
    }

    /// The span to render: `start`/`duration` if given, else the keyframes'
    /// span, else from `start` to the end of the clip.
    fn span(&self, start: Option<f64>, duration: Option<f64>, reframe: Option<&Reframe>) -> Result<(f64, f64)> {
        let (start, end) = span(&self.input, start, duration, reframe)?;
        Ok((self.snap(start), end))
    }
}

/// The span of `input` to render: `start`/`duration` if given, else the
/// keyframes' span, else from `start` to the end of the clip.
fn span(input: &Path, start: Option<f64>, duration: Option<f64>, reframe: Option<&Reframe>) -> Result<(f64, f64)> {
    let keyed = reframe.and_then(Reframe::span);
    let start = start.or(keyed.map(|k| k.0)).unwrap_or(0.0);
    let end = match (duration, keyed) {
        (Some(d), _) => start + d,
        (None, Some((_, end))) if end > start => end,
        _ => ffmpeg::duration(input)?,
    };
    if end <= start {
        bail!("nothing to render between {start} and {end} s");
    }
    Ok((start, end))
}

/// Renders every cut of `project` back to back into one MP4 at `size`,
/// with their sound. Each clip is set up once however many cuts use it.
/// `progress(done, total)` hears of every frame; an error from it (a
/// cancel) stops the export and removes the unfinished file.
/// Sets up each cut's clip once per clip and stabilisation (the cut's own
/// wins over the command's): the pipelines, and per cut the index of its own.
fn cut_pipelines(project: &project::Project, opts: &StitchOpts, proxy: bool) -> Result<(Vec<Pipeline>, Vec<usize>)> {
    let mut pipelines: Vec<Pipeline> = Vec::new();
    let mut of_cut = Vec::with_capacity(project.cuts.len());
    for cut in &project.cuts {
        let mode = cut.stabilize.or(opts.stabilize).unwrap_or(stab::Mode::Horizon);
        let i = match pipelines.iter().position(|p| p.input == cut.input && p.stab.mode() == mode) {
            Some(i) => i,
            None => {
                let mut p = Pipeline::new(&cut.input, &StitchOpts { stabilize: Some(mode), ..opts.clone() }, mode)?;
                if proxy {
                    p.use_proxy();
                }
                pipelines.push(p);
                pipelines.len() - 1
            }
        };
        of_cut.push(i);
    }
    Ok((pipelines, of_cut))
}

#[allow(clippy::too_many_arguments)]
fn export(project: &project::Project, opts: &StitchOpts, size: (usize, usize), draft: bool, cpu: bool, output: &Path, progress: &mut dyn FnMut(usize, usize) -> Result<()>) -> Result<()> {
    let (pipelines, of_cut) = cut_pipelines(project, opts, draft)?;
    // The shader always draws both lenses, so `--only` needs the CPU.
    let mut gpu = if cpu || opts.only.is_some() {
        None
    } else {
        match player::render::Gpu::new() {
            Ok(gpu) => {
                eprintln!("stitching on {}", gpu.name);
                Some(gpu)
            }
            Err(e) => {
                eprintln!("stitching on the CPU: {e:#}");
                None
            }
        }
    };
    let clips: Vec<_> = if gpu.is_some() { pipelines.iter().map(player::render::Clip::new).collect::<Result<_>>()? } else { Vec::new() };
    // Per cut: its clip's pipeline, start (on a frame) and frame count.
    let mut plan = Vec::with_capacity(project.cuts.len());
    for (cut, &i) in project.cuts.iter().zip(&of_cut) {
        let p = &pipelines[i];
        let start = p.snap(cut.start);
        let end = cut.end.min(ffmpeg::duration(&cut.input)?);
        if end <= start {
            bail!("the cut from {} s starts after {} ends", cut.start, cut.input.display());
        }
        plan.push((i, start, ((end - start) * p.fps).round().max(1.0) as usize));
    }
    let fps = pipelines[0].fps;
    if pipelines.iter().any(|p| (p.fps - fps).abs() > 0.01) {
        eprintln!("the clips' frame rates differ; exporting at {fps:.3} fps");
    }
    let sound: Vec<_> = plan.iter().map(|&(i, start, frames)| ffmpeg::Sound { input: &pipelines[i].input, start, duration: Some(frames as f64 / fps) }).collect();
    let mut encoder = ffmpeg::Encoder::new(output, size, fps, &sound)?;
    let total = plan.iter().map(|p| p.2).sum();
    let mut done = 0;
    for (cut, &(i, start, frames)) in project.cuts.iter().zip(&plan) {
        let mut tick = |n: usize| progress(done + n, total);
        let result = match &mut gpu {
            Some(gpu) => player::render::encode(gpu, &pipelines[i], &clips[i], start, frames, &mut encoder, |t| cut.camera_at(t), size, &mut tick),
            None => encode(&pipelines[i], start, Some(frames), &mut encoder, |t| Shot::View { camera: cut.camera_at(t), size }, &mut tick),
        };
        if let Err(e) = result {
            encoder.abort();
            let _ = std::fs::remove_file(output);
            return Err(e);
        }
        done += frames;
    }
    encoder.finish()
}

/// Progress on the terminal: frames done, out of how many, and the rate.
fn print_progress() -> impl FnMut(usize, usize) -> Result<()> {
    let began = Instant::now();
    move |done, total| {
        if done.is_multiple_of(10) || done == total {
            eprint!("\rframe {done}/{total}  {:.2} fps", done as f64 / began.elapsed().as_secs_f64().max(1e-3));
        }
        if done == total {
            eprintln!();
        }
        Ok(())
    }
}

/// Decoded lens frames in order, from the lens tracks or the proxy.
enum Stream {
    Tracks(Vec<ffmpeg::Decoder>),
    Proxy(ffmpeg::Decoder, Vec<u32>),
}

impl Stream {
    /// One frame per lens; `None` once the clip ends.
    fn next(&mut self) -> Result<Option<Vec<stitch::Frame>>> {
        match self {
            Stream::Tracks(decoders) => decoders.iter_mut().map(|d| d.next()).collect::<Result<Option<Vec<_>>>>(),
            Stream::Proxy(decoder, tracks) => Ok(decoder.next()?.map(|f| ffmpeg::split_proxy(&f, tracks))),
        }
    }
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
        Cmd::Info { input } => info(&input)?,
        Cmd::Play { input, osv_size, hwaccel, stabilize, imu_axes, world_z_down } => {
            player::run(input, player::Launch { osv_size, hw: hwaccel, stabilize, imu_axes, world_z_down })?;
        }
        Cmd::Seam { input, start, duration, dump } => seam_report(&input, start, duration, dump.as_deref())?,
        Cmd::Stitch { input, time, output, width, opts } => {
            let p = Pipeline::new(&input, &opts, stab::Mode::Off)?;
            let time = p.snap(time);
            let out = p.render(&p.stills(time)?, time, &mut Carry::default(), &Shot::Equirect(width));
            save_png(&output, &out)?;
            eprintln!("wrote {}", output.display());
        }
        Cmd::Render { input, start, duration, output, width, opts } => {
            let p = Pipeline::new(&input, &opts, stab::Mode::Off)?;
            let start = p.snap(start);
            let total = duration.map(|d| (d * p.fps).round() as usize);
            let mut encoder = ffmpeg::Encoder::new(&output, (width, width / 2), p.fps, &[ffmpeg::Sound { input: &input, start, duration }])?;
            let began = Instant::now();
            let mut tick = |n: usize| {
                if n.is_multiple_of(10) {
                    eprint!("\rframe {n}{}  {:.2} fps", total.map_or(String::new(), |t| format!("/{t}")), n as f64 / began.elapsed().as_secs_f64());
                }
                Ok(())
            };
            encode(&p, start, total, &mut encoder, |_| Shot::Equirect(width), &mut tick)?;
            eprintln!();
            encoder.finish()?;
            eprintln!("wrote {}", output.display());
        }
        Cmd::Export { input, framing, start, duration, resolution, aspect, size, output, draft, cpu, opts } => {
            let project = if is_project(&input) {
                if framing.reframe.is_some() || framing.view.is_some() || start.is_some() || duration.is_some() {
                    bail!("a project sets its own cuts and views; leave out --reframe, --view, -s and -d");
                }
                project::Project::load(&input)?
            } else {
                // One clip is a project of one cut.
                let reframe = framing.reframe()?.unwrap_or_else(|| Reframe::fixed(View::home(Projection::Perspective)));
                let (start, end) = span(&input, start, duration, Some(&reframe))?;
                project::Project { aspect: reframe.aspect, cuts: vec![project::Cut { input, start, end, keyframes: reframe.keyframes, stabilize: None }] }
            };
            let aspect = aspect.or(project.aspect).unwrap_or(view::Aspect(16, 9));
            let [w, h] = size.unwrap_or_else(|| aspect.size(resolution.short_side())).map(|v| v as usize);
            export(&project, &opts, (w, h), draft, cpu, &output, &mut print_progress())?;
            eprintln!("wrote {} ({w}×{h}, {:.2} s)", output.display(), project.duration());
        }
        Cmd::Preview { input, framing, start, duration, every, tile, columns, output, full, opts } if is_project(&input) => {
            if framing.reframe.is_some() || framing.view.is_some() || start.is_some() || duration.is_some() {
                bail!("a project sets its own cuts and views; leave out --reframe, --view, -s and -d");
            }
            let project = project::Project::load(&input)?;
            let (sheet, tiles) = preview_project(&project, &opts, every, tile, columns.max(1), !full)?;
            save_png(&output, &sheet)?;
            eprintln!("wrote {}: {tiles} tiles of {} cuts", output.display(), project.cuts.len());
        }
        Cmd::Preview { input, framing, start, duration, every, tile, columns, output, full, opts } => {
            let reframe = framing.reframe()?;
            let mut p = Pipeline::new(&input, &opts, stab::Mode::Horizon)?;
            if !full {
                p.use_proxy();
            }
            let times = match (every, &reframe) {
                (Some(every), _) => {
                    let (a, b) = p.span(start, duration, reframe.as_ref())?;
                    (0..).map(|n| a + n as f64 * every.max(1.0 / p.fps)).take_while(|&t| t < b).collect()
                }
                (None, Some(r)) if start.is_none() && duration.is_none() && r.keyframes.len() > 1 => r.keyframes.iter().map(|k| k.t).collect(),
                _ => {
                    let (a, b) = p.span(start, duration, None)?;
                    (0..24).map(|n| a + (n as f64 + 0.5) * (b - a) / 24.0).collect::<Vec<_>>()
                }
            };
            let shot = |t: f64| match &reframe {
                None => Shot::Equirect(tile),
                Some(r) => {
                    let view::Aspect(w, h) = r.aspect.unwrap_or(view::Aspect(16, 9));
                    Shot::View { camera: r.camera_at(t), size: (tile, ((tile * h as usize / w as usize) / 2).max(8) * 2) }
                }
            };
            let tiles = times
                .par_iter()
                .map(|&t| {
                    let t = p.snap(t);
                    let mut f = p.render(&p.stills(t)?, t, &mut Carry::default(), &shot(t));
                    if reframe.is_none() {
                        sheet::equirect_grid(&mut f);
                    }
                    sheet::label(&mut f, 4, 4, &format!("{t:.1}s"));
                    Ok(f)
                })
                .collect::<Result<Vec<_>>>()?;
            save_png(&output, &sheet::compose(&tiles, columns))?;
            eprintln!("wrote {}: {} tiles at {}", output.display(), tiles.len(), times.iter().map(|t| format!("{:.1}", p.snap(*t))).collect::<Vec<_>>().join(" "));
        }
        Cmd::Scan { input, every } => scan(&input, every)?,
    }
    Ok(())
}

fn is_project(input: &Path) -> bool {
    input.extension().is_some_and(|e| e.eq_ignore_ascii_case("json"))
}

/// A contact sheet of a project: per cut, `columns` moments spread across
/// it (or one every `every` seconds), framed as `export` frames them. Each
/// cut starts a row; blank tiles fill out the last. Returns the sheet and
/// how many tiles it shows.
fn preview_project(project: &project::Project, opts: &StitchOpts, every: Option<f64>, tile: usize, columns: usize, proxy: bool) -> Result<(stitch::Frame, usize)> {
    let (pipelines, of_cut) = cut_pipelines(project, opts, proxy)?;
    let view::Aspect(w, h) = project.aspect.unwrap_or(view::Aspect(16, 9));
    let size = (tile, ((tile * h as usize / w as usize) / 2).max(8) * 2);
    let mut shots = Vec::new();
    for (n, (cut, &i)) in project.cuts.iter().zip(&of_cut).enumerate() {
        let p = &pipelines[i];
        let len = cut.end - cut.start;
        let times: Vec<f64> = match every {
            Some(every) => (0..).map(|k| cut.start + k as f64 * every.max(1.0 / p.fps)).take_while(|&t| t < cut.end).collect(),
            None => (0..columns).map(|k| cut.start + (k as f64 + 0.5) * len / columns as f64).collect(),
        };
        let row_end = times.len().div_ceil(columns) * columns;
        shots.extend(times.iter().map(|&t| Some((n, i, p.snap(t).clamp(cut.start, cut.end)))));
        shots.extend((times.len()..row_end).map(|_| None));
    }
    let tiles = shots
        .par_iter()
        .map(|shot| {
            let Some((n, i, t)) = *shot else { return Ok(stitch::Frame { width: size.0, height: size.1, rgb: vec![0; size.0 * size.1 * 3] }) };
            let p = &pipelines[i];
            let mut f = p.render(&p.stills(t)?, t, &mut Carry::default(), &Shot::View { camera: project.cuts[n].camera_at(t), size });
            sheet::label(&mut f, 4, 4, &format!("#{} {t:.1}s", n + 1));
            Ok(f)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((sheet::compose(&tiles, columns), shots.iter().flatten().count()))
}

/// Stitches `total` frames (or to the end) from `start` into `encoder`,
/// framing each as `shot` says for its time. If the clip ends early, the
/// last frame is repeated up to `total`, so the sound stays in step.
fn encode(p: &Pipeline, start: f64, total: Option<usize>, encoder: &mut ffmpeg::Encoder, shot: impl Fn(f64) -> Shot, tick: &mut dyn FnMut(usize) -> Result<()>) -> Result<()> {
    let mut stream = p.stream(start)?;
    let mut carry = Carry::default();
    let mut last = None;
    let mut n = 0;
    while total.is_none_or(|total| n < total) {
        let Some(frames) = stream.next()? else { break };
        let t = start + n as f64 / p.fps;
        let frame = p.render(&frames, t, &mut carry, &shot(t));
        encoder.write(&frame)?;
        last = Some(frame);
        n += 1;
        tick(n)?;
    }
    if let (Some(total), Some(last)) = (total, &last) {
        for k in n..total {
            encoder.write(last)?;
            tick(k + 1)?;
        }
    }
    Ok(())
}

fn save_png(path: &Path, f: &stitch::Frame) -> Result<()> {
    image::save_buffer(path, &f.rgb, f.width as u32, f.height as u32, image::ColorType::Rgb8).with_context(|| format!("writing {}", path.display()))
}

fn info(input: &Path) -> Result<()> {
    let parsed = load_input(input)?;
    let info = serde_json::json!({
        "input": input,
        "proxy": ffmpeg::proxy(input),
        "duration": ffmpeg::duration(input)?,
        "fps": ffmpeg::frame_rate(input)?,
        "lens_size": ffmpeg::dimensions(input, 0)?,
        "recorded": ffmpeg::creation_time(input)?,
        "audio": ffmpeg::has_audio(input)?,
        "attitude": !imu::read(&parsed).quats.is_empty(),
    });
    println!("{}", serde_json::to_string_pretty(&info)?);
    Ok(())
}

/// One CSV row per `every` seconds; see `Cmd::Scan` and the header.
fn scan(input: &Path, every: f64) -> Result<()> {
    let parsed = load_input(input)?;
    let quats = imu::read(&parsed).quats;
    let duration = ffmpeg::duration(input)?;
    let levels = if ffmpeg::has_audio(input)? { ffmpeg::audio_levels(input, every)? } else { Vec::new() };
    let stab = (!quats.is_empty()).then(|| stab::Stabilizer::new(&quats, stab::Mode::Off, stab::ImuAxes::X180, -1.0, 0.0));
    // Turning is sampled every 20 ms: fine enough for hand shake, coarse
    // enough that the attitude's held-then-jumping samples average out.
    const STEP: f64 = 0.02;
    println!("t,heading,tilt,turn,rotation,shake,loudness,peak");
    let mut heading = None::<f64>;
    let rows = (duration / every).ceil() as usize;
    for n in 0..rows {
        let t = n as f64 * every;
        let level = levels.get(n).map_or(String::from(","), |(rms, peak)| format!("{rms:.1},{peak:.1}"));
        let Some(stab) = &stab else {
            println!("{t:.2},,,,,,{level}");
            continue;
        };
        let poses: Vec<_> = (0..=(every / STEP).round() as usize).map(|k| stab.pose((t + k as f64 * STEP) * 1000.0)).collect();
        let (first, last) = (&poses[0], &poses[poses.len() - 1]);
        let path: f64 = poses.windows(2).map(|w| w[0].turn_to(&w[1])).sum();
        // Unwrapped, so a full turn reads 360 rather than jumping back.
        let mut h = first.heading;
        if let Some(prev) = heading {
            h = prev + (h - prev + 540.0).rem_euclid(360.0) - 180.0;
        }
        heading = Some(h);
        let turn = (last.heading - first.heading + 540.0).rem_euclid(360.0) - 180.0;
        let rotation = path / every;
        let shake = (path - first.turn_to(last)) / every;
        println!("{t:.2},{h:.1},{:.1},{:.1},{rotation:.1},{shake:.1},{level}", first.tilt, turn / every);
    }
    Ok(())
}

fn seam_report(input: &Path, start: f64, duration: f64, dump: Option<&Path>) -> Result<()> {
    let opts = StitchOpts { no_rs: true, no_exposure: true, ..StitchOpts::default() };
    let p = Pipeline::new(input, &opts, stab::Mode::Off)?;
    let aligner = p.aligner.as_ref().context("not a two-lens rig")?;
    let start = p.snap(start);
    let mut stream = p.stream(start)?;
    let (mut eased, mut prev_raw): (Option<align::Field>, Option<align::Field>) = (None, None);
    let mut sums = [0.0; 7];
    // The selfie stick runs down the rig's y axis.
    let stick = aligner.column([0.0, 1.0, 0.0]);
    let mut n = 0;
    let jitter = |a: &align::Field, b: &align::Field| a.shift.iter().zip(&b.shift).map(|(x, y)| (x - y).abs() as f64).sum::<f64>() / a.shift.len() as f64;
    println!("t,mismatch_none,mismatch_raw,mismatch_eased,jitter_raw_deg,jitter_eased_deg,stick_none,stick_eased");
    while n < (duration * p.fps).round() as usize {
        let Some(frames) = stream.next()? else { break };
        let strips = aligner.strips(|i, u, v| Some(p.lenses[i].sample(&frames, u, v).iter().sum::<f64>() / (3.0 * 255.0)));
        let raw = aligner.measure(&strips, eased.as_ref());
        let before = eased.clone();
        let e = eased.get_or_insert_with(|| raw.clone());
        e.ease(&raw, align::EASE);
        let row = [
            aligner.mismatch(&strips, &align::Field::default(), None),
            aligner.mismatch(&strips, &raw, None),
            aligner.mismatch(&strips, e, None),
            prev_raw.as_ref().map_or(0.0, |r| jitter(r, &raw).to_degrees()),
            before.as_ref().map_or(0.0, |b| jitter(b, e).to_degrees()),
            aligner.mismatch(&strips, &align::Field::default(), Some(stick)),
            aligner.mismatch(&strips, e, Some(stick)),
        ];
        println!("{:.3},{:.5},{:.5},{:.5},{:.4},{:.4},{:.5},{:.5}", start + n as f64 / p.fps, row[0], row[1], row[2], row[3], row[4], row[5], row[6]);
        for (s, v) in sums.iter_mut().zip(row) {
            *s += v;
        }
        if n == 0
            && let Some(dir) = dump
        {
            aligner.dump(&strips, &raw, dir)?;
            eprintln!("rig down is strip column {}, rig up column {}", aligner.column([0.0, 1.0, 0.0]), aligner.column([0.0, -1.0, 0.0]));
        }
        prev_raw = Some(raw);
        n += 1;
    }
    let m = sums.map(|s| s / n.max(1) as f64);
    eprintln!(
        "{n} frames: mismatch none {:.5} raw {:.5} eased {:.5}; jitter raw {:.4}° eased {:.4}°; stick zone none {:.5} eased {:.5}",
        m[0], m[1], m[2], m[3], m[4], m[5], m[6]
    );
    Ok(())
}

fn load_input(input: &Path) -> Result<telemetry_parser::Input> {
    let mut f = File::open(input)?;
    let size = f.metadata()?.len() as usize;
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    Ok(telemetry_parser::Input::from_stream(&mut f, size, input, |_| {}, cancel)?)
}
