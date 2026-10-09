//! Frame sources for the player: NV12 pictures streamed from ffmpeg on
//! background threads, either the LRF proxy (both lenses side by side, cheap
//! to decode) or the OSV lens tracks via VA-API.

use anyhow::{Context, Result, bail};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread;

/// One NV12 picture: Y plane then interleaved UV at half resolution.
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl Picture {
    pub fn y(&self) -> &[u8] {
        &self.data[..(self.width * self.height) as usize]
    }
    pub fn uv(&self) -> &[u8] {
        &self.data[(self.width * self.height) as usize..]
    }
}

/// How the decoded pictures relate to the lens tracks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Layout {
    /// One picture holding every track side by side in squares of its height.
    SideBySide,
    /// One picture per lens track.
    PerTrack,
}

pub struct Frame {
    pub pts: f64,
    pub layout: Layout,
    pub pictures: Vec<Picture>,
}

#[derive(Clone)]
pub enum Kind {
    Lrf(PathBuf),
    /// OSV tracks scaled on the GPU to this many pixels square.
    Osv { path: PathBuf, tracks: Vec<u32>, size: u32 },
}

/// A running decode from `start`, delivering frames in order.
pub struct Stream {
    pub frames: Receiver<Frame>,
}

pub fn spawn(kind: Kind, start: f64, fps: f64) -> Result<Stream> {
    let (tx, rx) = sync_channel(3);
    let procs: Vec<(Child, u32, u32)> = match &kind {
        Kind::Lrf(path) => {
            let (w, h) = dims(path, 0)?;
            vec![(ffmpeg_nv12(path, 0, start, None, false)?, w, h)]
        }
        Kind::Osv { path, tracks, size } => {
            tracks.iter().map(|&t| Ok((ffmpeg_nv12(path, t, start, Some(*size), true)?, *size, *size))).collect::<Result<_>>()?
        }
    };
    let layout = match &kind {
        Kind::Lrf(_) => Layout::SideBySide,
        Kind::Osv { .. } => Layout::PerTrack,
    };
    thread::spawn(move || pump(procs, layout, start, fps, tx));
    Ok(Stream { frames: rx })
}

fn pump(mut procs: Vec<(Child, u32, u32)>, layout: Layout, start: f64, fps: f64, tx: SyncSender<Frame>) {
    let mut outs: Vec<_> = procs.iter_mut().map(|(c, _, _)| c.stdout.take().unwrap()).collect();
    for n in 0.. {
        let mut pictures = Vec::with_capacity(procs.len());
        for ((_, w, h), out) in procs.iter().zip(outs.iter_mut()) {
            let mut data = vec![0; (*w * *h * 3 / 2) as usize];
            if out.read_exact(&mut data).is_err() {
                return kill(procs);
            }
            pictures.push(Picture { width: *w, height: *h, data });
        }
        let frame = Frame { pts: start + n as f64 / fps, layout, pictures };
        if tx.send(frame).is_err() {
            return kill(procs);
        }
    }
}

fn kill(procs: Vec<(Child, u32, u32)>) {
    for (mut c, _, _) in procs {
        let _ = c.kill();
        let _ = c.wait();
    }
}

/// Decodes one full-resolution frame of every OSV track, trying VA-API first.
pub fn still(path: &Path, tracks: &[u32], t: f64) -> Result<Frame> {
    let pictures = tracks
        .iter()
        .map(|&track| {
            let (w, h) = dims(path, track)?;
            let read = |hw: bool| -> Result<Vec<u8>> {
                let mut child = ffmpeg_nv12(path, track, t, None, hw)?;
                let mut data = vec![0; (w * h * 3 / 2) as usize];
                child.stdout.take().unwrap().read_exact(&mut data)?;
                let _ = child.kill();
                let _ = child.wait();
                Ok(data)
            };
            let data = read(true).or_else(|_| read(false))?;
            Ok(Picture { width: w, height: h, data })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Frame { pts: t, layout: Layout::PerTrack, pictures })
}

fn ffmpeg_nv12(path: &Path, track: u32, start: f64, size: Option<u32>, hw: bool) -> Result<Child> {
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-v", "error", "-nostdin"]);
    if hw {
        cmd.args(["-hwaccel", "vaapi", "-hwaccel_output_format", "vaapi"]);
    }
    cmd.args(["-ss", &start.to_string(), "-i"]).arg(path).args(["-map", &format!("0:v:{track}")]);
    let filter = match (hw, size) {
        (true, Some(s)) => format!("scale_vaapi=w={s}:h={s}:format=nv12,hwdownload,format=nv12"),
        (true, None) => "scale_vaapi=format=nv12,hwdownload,format=nv12".to_owned(),
        (false, Some(s)) => format!("scale={s}:{s},format=nv12"),
        (false, None) => "format=nv12".to_owned(),
    };
    cmd.args(["-vf", &filter, "-f", "rawvideo", "-"]).stdout(Stdio::piped()).stderr(Stdio::null());
    cmd.spawn().context("running ffmpeg")
}

fn dims(path: &Path, track: u32) -> Result<(u32, u32)> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", &format!("v:{track}"), "-show_entries", "stream=width,height", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .context("running ffprobe")?;
    let s = String::from_utf8_lossy(&out.stdout);
    let Some((w, h)) = s.trim().split_once(',') else { bail!("no video track {track} in {}", path.display()) };
    Ok((w.parse()?, h.parse()?))
}
