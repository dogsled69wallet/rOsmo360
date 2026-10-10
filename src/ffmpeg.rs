//! Video I/O through the ffmpeg CLI. A placeholder until playback moves to
//! in-process hardware decode.

use crate::stitch::Frame;
use anyhow::{Context, Result, bail};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

fn probe(input: &Path, stream: &str, entries: &str) -> Result<String> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", stream, "-show_entries", entries, "-of", "csv=p=0"])
        .arg(input)
        .output()
        .context("running ffprobe")?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

pub fn frame_rate(input: &Path) -> Result<f64> {
    let rate = probe(input, "v:0", "stream=r_frame_rate")?;
    let (n, d) = rate.split_once('/').context("ffprobe gave no frame rate")?;
    Ok(n.parse::<f64>()? / d.parse::<f64>()?)
}

pub fn duration(input: &Path) -> Result<f64> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"])
        .arg(input)
        .output()
        .context("running ffprobe")?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().parse()?)
}

pub fn dimensions(input: &Path, track: u32) -> Result<(usize, usize)> {
    let dims = probe(input, &format!("v:{track}"), "stream=width,height")?;
    let (w, h) = dims.split_once(',').context("ffprobe gave no dimensions")?;
    Ok((w.parse()?, h.parse()?))
}

/// Hardware decoders, in the order they are tried. Vulkan Video works on every
/// vendor with a recent driver; VA-API and CUDA cover stacks without it.
#[derive(Clone, Copy, Debug, PartialEq, clap::ValueEnum)]
pub enum Hwaccel {
    Vulkan,
    Vaapi,
    Cuda,
    Software,
}

impl Hwaccel {
    fn name(self) -> Option<&'static str> {
        match self {
            Hwaccel::Vulkan => Some("vulkan"),
            Hwaccel::Vaapi => Some("vaapi"),
            Hwaccel::Cuda => Some("cuda"),
            Hwaccel::Software => None,
        }
    }

    /// The first decoder that gets a frame of `track` out of `input`.
    /// `hwdownload` rejects software frames, so a probe only succeeds if the
    /// GPU really decoded; ffmpeg's own fallback would hide a failure.
    pub fn pick(input: &Path, track: u32) -> Result<Hwaccel> {
        for hw in [Hwaccel::Vulkan, Hwaccel::Vaapi, Hwaccel::Cuda] {
            let ok = nv12(input, track, 0.0, None, hw)?
                .args(["-frames:v", "1", "-f", "null", "-"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .context("running ffmpeg")?
                .success();
            if ok {
                return Ok(hw);
            }
        }
        Ok(Hwaccel::Software)
    }
}

/// An ffmpeg command, missing only its output, that decodes `track` from
/// `start` to NV12, scaled to `size` pixels square if given. Scaling and
/// conversion run on the CPU: the GPU filters differ per backend, and timing
/// showed the download, not the conversion, is the cost.
pub fn nv12(input: &Path, track: u32, start: f64, size: Option<u32>, hw: Hwaccel) -> Result<Command> {
    let mut filters = Vec::new();
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-v", "error", "-nostdin"]);
    if let Some(name) = hw.name() {
        let ten_bit = probe(input, &format!("v:{track}"), "stream=pix_fmt")?.contains("10");
        cmd.args(["-hwaccel", name, "-hwaccel_output_format", name]);
        filters.push(format!("hwdownload,format={}", if ten_bit { "p010le" } else { "nv12" }));
    }
    if let Some(s) = size {
        filters.push(format!("scale={s}:{s}:flags=bilinear"));
    }
    filters.push("format=nv12".to_owned());
    cmd.args(["-ss", &start.to_string(), "-i"]).arg(input);
    cmd.args(["-map", &format!("0:v:{track}"), "-vf", &filters.join(",")]);
    Ok(cmd)
}

/// Streams RGB24 frames of one video track, starting at the first frame at or
/// after `start` seconds.
pub struct Decoder {
    child: Child,
    stdout: ChildStdout,
    width: usize,
    height: usize,
}

impl Decoder {
    pub fn new(input: &Path, track: u32, start: f64) -> Result<Self> {
        let (width, height) = dimensions(input, track)?;
        let mut child = Command::new("ffmpeg")
            .args(["-v", "error", "-ss", &start.to_string(), "-i"])
            .arg(input)
            .args(["-map", &format!("0:v:{track}"), "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
            .stdout(Stdio::piped())
            .spawn()
            .context("running ffmpeg")?;
        let stdout = child.stdout.take().unwrap();
        Ok(Decoder { child, stdout, width, height })
    }

    /// The next frame, or `None` at the end of the stream.
    pub fn next(&mut self) -> Result<Option<Frame>> {
        let mut rgb = vec![0; self.width * self.height * 3];
        match self.stdout.read_exact(&mut rgb) {
            Ok(()) => Ok(Some(Frame { width: self.width, height: self.height, rgb })),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Encodes RGB24 frames to H.264, muxing the source's audio over the same span.
pub struct Encoder {
    child: Child,
    stdin: Option<ChildStdin>,
}

impl Encoder {
    pub fn new(output: &Path, (w, h): (usize, usize), fps: f64, audio_from: &Path, start: f64) -> Result<Self> {
        let mut child = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "rawvideo", "-pix_fmt", "rgb24"])
            .args(["-s", &format!("{w}x{h}"), "-r", &fps.to_string(), "-i", "-"])
            .args(["-ss", &start.to_string(), "-i"])
            .arg(audio_from)
            .args(["-map", "0:v", "-map", "1:a:0?", "-shortest"])
            .args(["-c:v", "libx264", "-crf", "18", "-preset", "medium", "-pix_fmt", "yuv420p", "-c:a", "aac"])
            .arg(output)
            .stdin(Stdio::piped())
            .spawn()
            .context("running ffmpeg")?;
        let stdin = child.stdin.take();
        Ok(Encoder { child, stdin })
    }

    pub fn write(&mut self, frame: &Frame) -> Result<()> {
        self.stdin.as_mut().unwrap().write_all(&frame.rgb)?;
        Ok(())
    }

    pub fn finish(mut self) -> Result<()> {
        drop(self.stdin.take());
        if !self.child.wait()?.success() {
            bail!("ffmpeg encoder failed");
        }
        Ok(())
    }
}
