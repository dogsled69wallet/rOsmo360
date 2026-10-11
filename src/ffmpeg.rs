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
    /// Volume points in clip time, applied only in a mix.
    pub volume: &'a [crate::project::VolumeKey],
}

/// Writes `duration` seconds of `input`'s sound from `start` to `output` as
/// a 48 kHz 24-bit WAV, untouched otherwise: music for another editor, in a
/// form any of them reads (Resolve's free version on Linux reads no AAC or
/// MP3).
pub fn wav(input: &Path, start: f64, duration: f64, output: &Path) -> Result<()> {
    let status = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-y", "-ss", &start.to_string(), "-t", &duration.to_string(), "-i"])
        .arg(input)
        .args(["-map", "0:a:0", "-ar", "48000", "-c:a", "pcm_s24le"])
        .arg(output)
        .status()
        .context("running ffmpeg")?;
    if !status.success() {
        bail!("ffmpeg couldn't write {}", output.display());
    }
    Ok(())
}

/// What a whole edit's sound gets on top of joining its clips' stretches:
/// their level, and the music track.
pub struct Mix<'a> {
    /// The clips' sound, dB.
    pub sound_db: f64,
    /// The music track: its level and clips.
    pub music: &'a crate::project::Music,
    /// The edit's length: music running on is cut here, faded out over
    /// `project::END_FADE`.
    pub length: f64,
}

impl Mix<'_> {
    /// Whether it changes anything.
    fn matters(&self) -> bool {
        self.sound_db != 0.0 || !self.music.clips.is_empty()
    }
}

impl Encoder {
    /// The sound is `audio`'s stretches back to back. Each stretch should
    /// last exactly as long as the frames written for it, so later cuts stay
    /// in sync. Several stretches are joined only if every clip has sound.
    /// No `-shortest`: the sound already ends with the frames, and with the
    /// frames arriving slower than the sound it cut the last frame off.
    /// With `mix`, the joined sound is set to its level and the music mixed
    /// in.
    pub fn new(output: &Path, (w, h): (usize, usize), fps: f64, audio: &[Sound<'_>], mix: Option<&Mix<'_>>, format: Format) -> Result<Self> {
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-v", "error", "-y", "-f", "rawvideo", "-pix_fmt", "rgb24"]).args(["-s", &format!("{w}x{h}"), "-r", &fps.to_string(), "-i", "-"]);
        for s in audio {
            cmd.args(["-ss", &s.start.to_string()]);
            if let Some(d) = s.duration {
                cmd.args(["-t", &d.to_string()]);
            }
            cmd.arg("-i").arg(s.input);
        }
        // The music's inputs, before any output option.
        let points = audio.iter().any(|s| !s.volume.is_empty());
        let graph = mix.filter(|m| m.matters() || points).map(|m| mixed(&mut cmd, audio, m)).transpose()?;
        cmd.args(["-map", "0:v"]);
        if let Some(graph) = graph {
            cmd.args(["-filter_complex", &graph, "-map", "[a]"]);
        } else if let [_] = audio {
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

/// `base` dB and the line through volume points `keys` as a `volume`
/// filter, the same line the player follows (straight in dB between
/// points), worked out per audio frame (~20 ms). `t` is the expression
/// for the points' time from the stream's.
fn level_filter(base: f64, keys: &[crate::project::VolumeKey], t: &str) -> String {
    if keys.is_empty() {
        return format!("volume={}", crate::project::gain(base));
    }
    // dB at `t`: nested ifs from the last stretch back to the first.
    let mut db = format!("{}", keys[keys.len() - 1].volume);
    for w in keys.windows(2).rev() {
        let (a, b) = (w[0], w[1]);
        db = format!("if(lt({t},{}),{}+({})*({t}-{})/{},{db})", b.t, a.volume, b.volume - a.volume, a.t, (b.t - a.t).max(1e-9));
    }
    db = format!("if(lt({t},{}),{},{db})", keys[0].t, keys[0].volume);
    let off = crate::project::OFF_DB;
    format!("volume=eval=frame:volume='if(lte({base}+{db},{off}),0,pow(10,({base}+{db})/20))'")
}

/// The filter graph for `Encoder::new` with a mix, adding the music's inputs
/// to `cmd`: the clips' stretches joined (or silence where they have none)
/// at their level, and each music clip trimmed to its stretch and to the
/// edit, faded, placed at its time and at the track's level, all mixed over
/// the edit's length.
fn mixed(cmd: &mut Command, audio: &[Sound<'_>], mix: &Mix<'_>) -> Result<String> {
    let stereo = "aresample=48000,aformat=sample_fmts=fltp:channel_layouts=stereo";
    let mut graph = String::new();
    if !audio.is_empty() && audio.iter().all(|s| has_audio(s.input).unwrap_or(false)) {
        for (i, sound) in (1..).zip(audio) {
            // Its volume points, in clip time: the stretch's time starts at 0.
            let level = if sound.volume.is_empty() { String::new() } else { format!(",{}", level_filter(0.0, sound.volume, &format!("(t+{})", sound.start))) };
            graph += &format!("[{i}:a:0]{stereo}{level}[c{i}];");
        }
        let inputs: String = (1..=audio.len()).map(|i| format!("[c{i}]")).collect();
        graph += &format!("{inputs}concat=n={}:v=0:a=1,volume={}[s];", audio.len(), crate::project::gain(mix.sound_db));
    } else {
        if !audio.is_empty() {
            eprintln!("not every clip has sound; exporting the music alone");
        }
        graph += &format!("anullsrc=r=48000:cl=stereo,atrim=duration={}[s];", mix.length);
    }
    let mut parts = vec!["[s]".to_owned()];
    for (i, clip) in mix.music.clips.iter().enumerate() {
        // What's left of the edit from where it starts.
        let room = mix.length - clip.at;
        let len = (clip.end - clip.start).min(room);
        if len <= 0.0 {
            continue;
        }
        let input = audio.len() + parts.len();
        cmd.args(["-ss", &clip.start.to_string(), "-t", &len.to_string()]).arg("-i").arg(&clip.input);
        let mut chain = format!("[{input}:a:0]{stereo}");
        if clip.fade_in > 0.0 {
            chain += &format!(",afade=t=in:st=0:d={}", clip.fade_in.min(len));
        }
        // Cut off by the edit's end: fade out there instead of its own fade.
        let cut_off = clip.end - clip.start > room;
        let fade_out = if cut_off { crate::project::END_FADE } else { clip.fade_out };
        if fade_out > 0.0 {
            let d = fade_out.min(len);
            chain += &format!(",afade=t=out:st={}:d={d}", len - d);
        }
        // Crossfades with the clips either side, equal power as the player
        // has them (a quarter sine each way).
        let (cross_in, cross_out) = mix.music.crossfades(i);
        if cross_in > 0.0 {
            chain += &format!(",afade=t=in:st=0:d={}:curve=qsin", cross_in.min(len));
        }
        if cross_out > 0.0 && !cut_off {
            chain += &format!(",afade=t=out:st={}:d={cross_out}:curve=qsin", len - cross_out);
        }
        let delay = (clip.at * 1000.0).round();
        graph += &format!("{chain},adelay=delays={delay}:all=1,{}[m{input}];", level_filter(mix.music.volume, &mix.music.keyframes, "t"));
        parts.push(format!("[m{input}]"));
    }
    graph += &format!("{}amix=inputs={}:duration=first:normalize=0[a]", parts.concat(), parts.len());
    Ok(graph)
}
