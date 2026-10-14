//! Crossfades between cuts in an export: frames written one cut after
//! another, the last frames of a cut held back and blended into the first
//! of the next.
//!
//! Each cut is rendered on its own (its own decoder and framing), so the two
//! cuts of a crossfade are never rendered side by side. The outgoing cut's
//! tail goes to a file instead, and is read back frame by frame as the
//! incoming cut's head arrives: at 2160p a 2 s crossfade would hold 1.5 GB
//! in memory.

use crate::ffmpeg::FrameSink;
use crate::stitch::Frame;
use anyhow::{Context, Result};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::PathBuf;

pub struct Dissolve<'a> {
    out: &'a mut dyn FrameSink,
    /// The cut being written: frames so far, how many it has, how many of
    /// its first blend with the last cut's held tail, and how many of its
    /// last are held for the next.
    n: usize,
    frames: usize,
    head: usize,
    tail: usize,
    held: Option<BufReader<File>>,
    holding: Option<BufWriter<File>>,
    /// The two files, swapped at each cut: one read, one written.
    files: [PathBuf; 2],
}

impl<'a> Dissolve<'a> {
    pub fn new(out: &'a mut dyn FrameSink) -> Self {
        let file = |k| std::env::temp_dir().join(format!("rosmo360-dissolve-{}-{k}.rgb", std::process::id()));
        Dissolve { out, n: 0, frames: 0, head: 0, tail: 0, held: None, holding: None, files: [file(0), file(1)] }
    }

    /// Starts the next cut of `frames` frames, whose first `head` cross the
    /// last cut's held tail and whose last `tail` cross the next cut.
    pub fn cut(&mut self, frames: usize, head: usize, tail: usize) -> Result<()> {
        self.held = None;
        if let Some(mut w) = self.holding.take() {
            w.flush()?;
            drop(w);
            self.files.swap(0, 1);
            self.held = Some(BufReader::new(File::open(&self.files[0])?));
        }
        if tail > 0 {
            self.holding = Some(BufWriter::new(File::create(&self.files[1]).with_context(|| format!("creating {}", self.files[1].display()))?));
        }
        (self.n, self.frames, self.head, self.tail) = (0, frames, head.min(frames), tail.min(frames));
        Ok(())
    }
}

impl FrameSink for Dissolve<'_> {
    fn write(&mut self, frame: &Frame) -> Result<()> {
        let n = self.n;
        self.n += 1;
        if n + self.tail >= self.frames
            && let Some(w) = &mut self.holding
        {
            return Ok(w.write_all(&frame.rgb)?);
        }
        if n < self.head
            && let Some(r) = &mut self.held
        {
            let mut before = vec![0; frame.rgb.len()];
            r.read_exact(&mut before).context("reading the crossfade's held frames")?;
            // Linear, by frame centres: the first frame already a little in.
            let a = (n as f32 + 0.5) / self.head as f32;
            let mix = |b: f32, c: f32| b + (c - b) * a;
            let rgb = if frame.deep {
                let sample = |s: &[u8]| u16::from_le_bytes([s[0], s[1]]) as f32;
                before.chunks_exact(2).zip(frame.rgb.chunks_exact(2)).flat_map(|(b, c)| (mix(sample(b), sample(c)).round() as u16).to_le_bytes()).collect()
            } else {
                before.iter().zip(&frame.rgb).map(|(&b, &c)| mix(b as f32, c as f32).round() as u8).collect()
            };
            return self.out.write(&Frame { width: frame.width, height: frame.height, rgb, deep: frame.deep });
        }
        self.out.write(frame)
    }
}

impl Drop for Dissolve<'_> {
    fn drop(&mut self) {
        (self.held, self.holding) = (None, None);
        for f in &self.files {
            let _ = std::fs::remove_file(f);
        }
    }
}
