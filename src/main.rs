mod stitch;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;
use telemetry_parser::rig::{CameraRig, RigPicture, geometry::ProjectionKind};

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
        /// Use this alternative calibration (see `rig`'s `variants`) instead of the default.
        #[arg(long)]
        variant: Option<usize>,
        /// Render only this lens (0 or 1), for checking overlap alignment.
        #[arg(long)]
        only: Option<usize>,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Rig { input } => {
            let rig = load_rig(&input)?;
            println!("{}", serde_json::to_string_pretty(&rig)?);
        }
        Cmd::Stitch { input, time, output, width, variant, only } => {
            let rig = load_rig(&input)?;
            let projection = match variant {
                Some(i) => &rig.variants.get(i).context("no such variant")?.projection,
                None => &rig.projection,
            };
            let ProjectionKind::Rig { cameras, .. } = &projection.kind else {
                bail!("not a multi-lens rig");
            };
            let lenses = cameras.iter().map(stitch::Lens::new).collect::<Result<Vec<_>>>()?;
            let frames = rig
                .pictures
                .iter()
                .map(|p| match p {
                    RigPicture::VideoTrack(n) => decode_frame(&input, *n, time),
                    RigPicture::Sibling { .. } => bail!("multi-file rigs are not supported"),
                })
                .collect::<Result<Vec<_>>>()?;
            let out = stitch::equirect(&lenses, &frames, width, only);
            image::save_buffer(&output, &out.rgb, out.width as u32, out.height as u32, image::ColorType::Rgb8)?;
            eprintln!("wrote {}", output.display());
        }
    }
    Ok(())
}

fn load_rig(input: &Path) -> Result<CameraRig> {
    let mut f = File::open(input)?;
    let size = f.metadata()?.len() as usize;
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let parsed = telemetry_parser::Input::from_stream(&mut f, size, input, |_| {}, cancel)?;
    telemetry_parser::util::camera_rig(&parsed).context("no camera rig in file")
}

/// Decodes one frame of the `n`th video track to RGB24 via the ffmpeg CLI.
/// Placeholder until playback moves to hardware decode.
fn decode_frame(input: &Path, n: u32, time: f64) -> Result<stitch::Frame> {
    let probe = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", &format!("v:{n}"), "-show_entries", "stream=width,height"])
        .args(["-of", "csv=p=0"])
        .arg(input)
        .output()
        .context("running ffprobe")?;
    let dims = String::from_utf8_lossy(&probe.stdout);
    let (w, h) = dims.trim().split_once(',').context("ffprobe gave no dimensions")?;
    let (w, h): (usize, usize) = (w.parse()?, h.parse()?);

    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-ss", &time.to_string(), "-i"])
        .arg(input)
        .args(["-map", &format!("0:v:{n}"), "-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
        .output()
        .context("running ffmpeg")?;
    if !out.status.success() {
        bail!("ffmpeg failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    if out.stdout.len() != w * h * 3 {
        bail!("unexpected frame size {} for {w}x{h}", out.stdout.len());
    }
    Ok(stitch::Frame { width: w, height: h, rgb: out.stdout })
}
