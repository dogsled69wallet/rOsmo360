//! Video I/O through the ffmpeg CLI. A placeholder until playback moves to
//! in-process hardware decode.

use crate::stitch::Frame;
use anyhow::{Context, Result, bail};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
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

/// The clip's recording time as the camera tagged it, if it did.
pub fn creation_time(input: &Path) -> Result<Option<String>> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "format_tags=creation_time", "-of", "csv=p=0"])
        .arg(input)
        .output()
        .context("running ffprobe")?;
    let t = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    Ok((!t.is_empty()).then_some(t))
}

/// The low-resolution proxy DJI records next to the clip, if it is there:
/// the lens tracks side by side, each the full circle scaled down.
pub fn proxy(input: &Path) -> Option<PathBuf> {
    ["LRF", "lrf"].iter().map(|ext| input.with_extension(ext)).find(|p| p.exists())
}

/// Cuts a side-by-side proxy frame into one square picture per track.
pub fn split_proxy(frame: &Frame, tracks: &[u32]) -> Vec<Frame> {
    let side = frame.height;
    tracks
        .iter()
        .map(|&t| {
            let x0 = t as usize * side;
            let rgb = (0..side).flat_map(|y| &frame.rgb[(y * frame.width + x0) * 3..(y * frame.width + x0 + side) * 3]).copied().collect();
            Frame { width: side, height: side, rgb }
        })
        .collect()
}

/// Decodes only `track`'s keyframes, scaled to `size`, and hands each to
/// `each` with its time until it returns false. Skipping the other frames
/// makes this cheap (0037's 55 s proxy: 84 frames in about 1 s); the cost
/// is mostly reading the file.
pub fn key_frames(input: &Path, track: u32, (w, h): (usize, usize), mut each: impl FnMut(f64, Frame) -> bool) -> Result<()> {
    let mut child = Command::new("ffmpeg")
        .args(["-hide_banner", "-nostdin", "-nostats", "-loglevel", "info", "-skip_frame", "nokey", "-i"])
        .arg(input)
        .args(["-map", &format!("0:v:{track}"), "-vf", &format!("scale={w}:{h},showinfo"), "-fps_mode", "passthrough"])
        .args(["-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running ffmpeg")?;
    // showinfo logs each frame's time; read it on its own thread, or a
    // full stderr pipe would stall the decode.
    let stderr = child.stderr.take().unwrap();
    let (tx, times) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stderr).lines().map_while(Result::ok) {
            if let Some(t) = line.split("pts_time:").nth(1).and_then(|r| r.split_whitespace().next()).and_then(|t| t.parse::<f64>().ok())
                && tx.send(t).is_err()
            {
                break;
            }
        }
    });
    let mut stdout = child.stdout.take().unwrap();
    let result = (|| -> Result<()> {
        loop {
            let mut rgb = vec![0; w * h * 3];
            match stdout.read_exact(&mut rgb) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(e) => return Err(e.into()),
            }
            let t = times.recv().context("ffmpeg gave a frame without its time")?;
            if !each(t, Frame { width: w, height: h, rgb }) {
                return Ok(());
            }
        }
    })();
    let _ = child.kill();
    let _ = child.wait();
    result
}

/// RMS and peak level in dBFS of the first audio stream, per `window`
/// seconds from the start.
pub fn audio_levels(input: &Path, window: f64) -> Result<Vec<(f64, f64)>> {
    const RATE: f64 = 16000.0;
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(input)
        .args(["-map", "0:a:0", "-ac", "1", "-ar", &RATE.to_string(), "-f", "f32le", "-"])
        .stderr(Stdio::inherit())
        .output()
        .context("running ffmpeg")?;
    let samples: Vec<f32> = out.stdout.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
    let db = |v: f64| 20.0 * v.max(1e-6).log10();
    Ok(samples
        .chunks(((window * RATE).round() as usize).max(1))
        .map(|w| {
            let rms = (w.iter().map(|&s| s as f64 * s as f64).sum::<f64>() / w.len() as f64).sqrt();
            (db(rms), db(w.iter().fold(0.0f64, |m, &s| m.max(s.abs() as f64))))
        })
        .collect())
}

pub fn has_audio(input: &Path) -> Result<bool> {
    Ok(!probe(input, "a", "stream=index")?.is_empty())
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
    pub fn pick(input: &Path, track: u32) -> Result<Hwaccel> {
        for hw in [Hwaccel::Vulkan, Hwaccel::Vaapi, Hwaccel::Cuda] {
            if hw.works(input, track)? {
                return Ok(hw);
            }
        }
        Ok(Hwaccel::Software)
    }

    /// Whether this decoder gets a full-size frame of `track` out of `input`.
    /// `hwdownload` rejects software frames, so a probe only succeeds if the
    /// GPU really decoded; ffmpeg's own fallback would hide a failure.
    ///
    /// Each probe takes 0.5-1 s, and every Osmo 360 clip's tracks are the
    /// same (HEVC Main10, 3840²), so each decoder is probed once a run: an
    /// export of several clips otherwise spent seconds probing.
    pub fn works(self, input: &Path, track: u32) -> Result<bool> {
        static PROBED: std::sync::Mutex<Vec<(Hwaccel, bool)>> = std::sync::Mutex::new(Vec::new());
        if let Some(&(_, ok)) = PROBED.lock().unwrap().iter().find(|(hw, _)| *hw == self) {
            return Ok(ok);
        }
        let ok = nv12(input, track, 0.0, None, self)?
            .args(["-frames:v", "1", "-f", "null", "-"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("running ffmpeg")?
            .success();
        PROBED.lock().unwrap().push((self, ok));
        Ok(ok)
    }
}

/// An ffmpeg command, missing only its output, that decodes `track` from
/// `start` to NV12, scaled to `size` pixels square if given. Scaling and
/// conversion run on the CPU: the GPU filters differ per backend, and timing
/// showed the download, not the conversion, is the cost.
///
/// Except on CUDA at full size: `scale_cuda` converts the 10-bit frames to
/// NV12 before the download, which halves the bytes. Both 3840² lenses of
/// 0037 then decode at 1.9× real time instead of 0.9× on Vulkan.
pub fn nv12(input: &Path, track: u32, start: f64, size: Option<u32>, hw: Hwaccel) -> Result<Command> {
    let mut filters = Vec::new();
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-v", "error", "-nostdin"]);
    if let Some(name) = hw.name() {
        let ten_bit = probe(input, &format!("v:{track}"), "stream=pix_fmt")?.contains("10");
        cmd.args(["-hwaccel", name, "-hwaccel_output_format", name]);
        if hw == Hwaccel::Cuda && size.is_none() {
            filters.push("scale_cuda=format=nv12,hwdownload".to_owned());
        } else {
            filters.push(format!("hwdownload,format={}", if ten_bit { "p010le" } else { "nv12" }));
        }
    }
    if let Some(s) = size {
        filters.push(format!("scale={s}:{s}:flags=bilinear"));
    }
    filters.push("format=nv12".to_owned());
    cmd.args(["-ss", &start.to_string(), "-i"]).arg(input);
    cmd.args(["-map", &format!("0:v:{track}"), "-vf", &filters.join(",")]);
    Ok(cmd)
}

/// One RGB24 frame of a video track at `t`, decoded on `hw` when it can be.
/// Asking for exactly one frame lets ffmpeg finish cleanly instead of
/// complaining about a closed pipe.
pub fn still(input: &Path, track: u32, t: f64, hw: Hwaccel) -> Result<Frame> {
    let (width, height) = dimensions(input, track)?;
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-v", "error", "-nostdin"]);
    if let Some(name) = hw.name() {
        cmd.args(["-hwaccel", name]);
    }
    let out = cmd
        .args(["-ss", &t.to_string(), "-i"])
        .arg(input)
        .args(["-map", &format!("0:v:{track}"), "-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
        .stderr(Stdio::inherit())
        .output()
        .context("running ffmpeg")?;
    if out.stdout.len() != width * height * 3 {
        bail!("no frame at {t:.2} s");
    }
    Ok(Frame { width, height, rgb: out.stdout })
}

/// Streams RGB24 frames of one video track, starting at the first frame at or
/// after `start` seconds. Decodes on `hw` when it can (3.5× faster for a
/// 3840² HEVC still on Vulkan) and quietly in software when it can't.
pub struct Decoder {
    child: Child,
    stdout: ChildStdout,
    width: usize,
    height: usize,
}

impl Decoder {
    pub fn new(input: &Path, track: u32, start: f64, hw: Hwaccel) -> Result<Self> {
        let (width, height) = dimensions(input, track)?;
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-v", "error"]);
        if let Some(name) = hw.name() {
            cmd.args(["-hwaccel", name]);
        }
        let mut child = cmd
            .args(["-ss", &start.to_string(), "-i"])
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

/// What `Encoder` writes.
#[derive(Clone, Copy, Debug, Default, PartialEq, clap::ValueEnum, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    /// H.264 and AAC in MP4: plays anywhere, small.
    #[default]
    Mp4,
    /// DNxHR HQ and 16-bit PCM in MOV, for DaVinci Resolve: its free Linux
    /// version decodes neither H.264/H.265 nor AAC. About 27 MB a second
    /// at 1080p (110 at 2160p), an editing intermediate.
    Resolve,
}

impl Format {
    pub fn extension(self) -> &'static str {
        match self {
            Format::Mp4 => "mp4",
            Format::Resolve => "mov",
        }
    }

    fn codecs(self) -> &'static [&'static str] {
        match self {
            Format::Mp4 => &["-c:v", "libx264", "-crf", "18", "-preset", "medium", "-pix_fmt", "yuv420p", "-c:a", "aac"],
            Format::Resolve => &["-c:v", "dnxhd", "-profile:v", "dnxhr_hq", "-pix_fmt", "yuv422p", "-c:a", "pcm_s16le", "-ar", "48000"],
        }
    }
}

/// Encodes RGB24 frames, muxing the source's audio over the same span.
pub struct Encoder {
    child: Child,
    stdin: Option<ChildStdin>,
}

/// A stretch of a clip's sound: from `start` seconds, for `duration` (to
/// the end if `None`).
pub struct Sound<'a> {
    pub input: &'a Path,
    pub start: f64,
    pub duration: Option<f64>,
}

impl Encoder {
    /// The sound is `audio`'s stretches back to back. Each stretch should
    /// last exactly as long as the frames written for it, so later cuts stay
    /// in sync. Several stretches are joined only if every clip has sound.
    /// No `-shortest`: the sound already ends with the frames, and with the
    /// frames arriving slower than the sound it cut the last frame off.
    pub fn new(output: &Path, (w, h): (usize, usize), fps: f64, audio: &[Sound<'_>], format: Format) -> Result<Self> {
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-v", "error", "-y", "-f", "rawvideo", "-pix_fmt", "rgb24"]).args(["-s", &format!("{w}x{h}"), "-r", &fps.to_string(), "-i", "-"]);
        for s in audio {
            cmd.args(["-ss", &s.start.to_string()]);
            if let Some(d) = s.duration {
                cmd.args(["-t", &d.to_string()]);
            }
            cmd.arg("-i").arg(s.input);
        }
        cmd.args(["-map", "0:v"]);
        if let [_] = audio {
            cmd.args(["-map", "1:a:0?"]);
        } else if audio.iter().all(|s| has_audio(s.input).unwrap_or(false)) {
            // Same camera, but resample anyway: concat needs matching formats.
            let parts: String = (1..=audio.len()).map(|i| format!("[{i}:a:0]aresample=48000,aformat=channel_layouts=stereo[a{i}];")).collect();
            let inputs: String = (1..=audio.len()).map(|i| format!("[a{i}]")).collect();
            cmd.args(["-filter_complex", &format!("{parts}{inputs}concat=n={}:v=0:a=1[a]", audio.len()), "-map", "[a]"]);
        } else if !audio.is_empty() {
            eprintln!("not every clip has sound; exporting without");
        }
        let mut child = cmd
            .args(format.codecs())
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

    /// Stops encoding, leaving whatever was written.
    pub fn abort(mut self) {
        drop(self.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    pub fn finish(mut self) -> Result<()> {
        drop(self.stdin.take());
        if !self.child.wait()?.success() {
            bail!("ffmpeg encoder failed");
        }
        Ok(())
    }
}
