//! Frame sources for the player: NV12 pictures streamed from ffmpeg on
//! background threads, either the LRF proxy (both lenses side by side, cheap
//! to decode) or the OSV lens tracks, on whichever hardware decoder works.

use crate::ffmpeg::{self, Hwaccel};
use crate::stitch::Lens;
use anyhow::{Context, Result};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
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

impl Frame {
    /// Display RGB (0..1) at `lens`'s full-resolution picture pixel, wherever
    /// this frame's layout and scale put it. Must match the shader's mapping.
    pub fn rgb_at(&self, lens: &Lens, track: u32, u: f64, v: f64) -> Option<[f64; 3]> {
        let (pic, off_x) = match self.layout {
            Layout::SideBySide => (self.pictures.first()?, track as f64 * self.pictures.first()?.height as f64),
            Layout::PerTrack => (self.pictures.get(track as usize)?, 0.0),
        };
        let cell = if self.layout == Layout::SideBySide { pic.height } else { pic.width } as f64;
        let s = cell / lens.calib_dim[0] as f64;
        let (x, y) = ((s * u + off_x) as i64, (s * v) as i64);
        if x < 0 || y < 0 || x >= pic.width as i64 || y >= pic.height as i64 {
            return None;
        }
        let (x, y, w) = (x as usize, y as usize, pic.width as usize);
        let c = (y / 2) * w + (x / 2) * 2;
        let px = |b: u8| b as f64 / 255.0;
        let yy = (px(pic.y()[y * w + x]) - 16.0 / 255.0) * (255.0 / 219.0);
        let cu = (px(pic.uv()[c]) - 128.0 / 255.0) * (255.0 / 224.0);
        let cv = (px(pic.uv()[c + 1]) - 128.0 / 255.0) * (255.0 / 224.0);
        // BT.709, limited range, as in the shader.
        Some([yy + 1.5748 * cv, yy - 0.1873 * cu - 0.4681 * cv, yy + 1.8556 * cu].map(|v| v.clamp(0.0, 1.0)))
    }
}

#[derive(Clone)]
pub enum Kind {
    Lrf(PathBuf),
    /// OSV tracks scaled to this many pixels square.
    Osv { path: PathBuf, tracks: Vec<u32>, size: u32 },
}

/// A running decode from `start`, delivering frames in order.
pub struct Stream {
    pub frames: Receiver<Frame>,
}

pub fn spawn(kind: Kind, hw: Hwaccel, start: f64, fps: f64) -> Result<Stream> {
    let (tx, rx) = sync_channel(3);
    let procs: Vec<(Child, u32, u32)> = match &kind {
        Kind::Lrf(path) => {
            // The proxy is small H.264: software decodes it at 4-8x real time
            // on one to four threads, and starts in 0.15 s where setting up
            // Vulkan or CUDA takes 0.5-1 s, which made every scrub lag.
            let (w, h) = dims(path, 0)?;
            vec![(decode(path, 0, start, None, Hwaccel::Software)?, w, h)]
        }
        Kind::Osv { path, tracks, size } => {
            tracks.iter().map(|&t| Ok((decode(path, t, start, Some(*size), hw)?, *size, *size))).collect::<Result<_>>()?
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

/// Decodes one frame of every OSV track, at full resolution or scaled to
/// `size` pixels square, falling back to software if the hardware decoder
/// fails.
pub fn still(path: &Path, tracks: &[u32], t: f64, size: Option<u32>, hw: Hwaccel) -> Result<Frame> {
    let pictures = tracks
        .iter()
        .map(|&track| {
            let (w, h) = size.map_or_else(|| dims(path, track), |s| Ok((s, s)))?;
            let read = |hw| -> Result<Vec<u8>> {
                let mut child = decode(path, track, t, size, hw)?;
                let mut data = vec![0; (w * h * 3 / 2) as usize];
                child.stdout.take().unwrap().read_exact(&mut data)?;
                let _ = child.kill();
                let _ = child.wait();
                Ok(data)
            };
            let data = read(hw).or_else(|_| read(Hwaccel::Software))?;
            Ok(Picture { width: w, height: h, data })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Frame { pts: t, layout: Layout::PerTrack, pictures })
}

fn decode(path: &Path, track: u32, start: f64, size: Option<u32>, hw: Hwaccel) -> Result<Child> {
    let mut cmd = ffmpeg::nv12(path, track, start, size, hw)?;
    cmd.args(["-f", "rawvideo", "-"]).stdout(Stdio::piped()).stderr(Stdio::null());
    cmd.spawn().context("running ffmpeg")
}

fn dims(path: &Path, track: u32) -> Result<(u32, u32)> {
    let (w, h) = ffmpeg::dimensions(path, track)?;
    Ok((w as u32, h as u32))
}
