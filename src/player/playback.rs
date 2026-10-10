//! The playback clock: plays from the stream (proxy or scaled OSV) and swaps
//! in a full-resolution OSV frame whenever playback is paused.

use super::gpu::RS_STEPS_MAX;
use super::source::{self, Frame, Kind, Stream};
use super::Media;
use super::gpu::SeamFix;
use crate::{align, exposure};
use crate::stitch::M3;
use anyhow::Result;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::Instant;

pub struct Playback {
    pub media: Media,
    pub playing: bool,
    /// Playback clock: `anchor_pts` at `anchor_at`.
    anchor_pts: f64,
    anchor_at: Instant,
    pub shown_pts: f64,
    stream: Option<Stream>,
    pending: Option<Frame>,
    /// While paused, a stream is only kept for its first frame.
    preview_only: bool,
    still: Option<(f64, Receiver<Result<Frame>>)>,
    showing_full: bool,
    /// While the seek bar is dragged: whether to resume playing on release,
    /// and the latest position asked for while a preview was still decoding.
    scrub: Option<(bool, Option<f64>)>,
    /// Seam corrections, eased from frame to frame while playing.
    fix: SeamFix,
    pub error: Option<String>,
}

impl Playback {
    pub fn new(media: Media) -> Self {
        let mut p = Playback {
            media,
            playing: false,
            anchor_pts: 0.0,
            anchor_at: Instant::now(),
            shown_pts: 0.0,
            stream: None,
            pending: None,
            preview_only: false,
            still: None,
            showing_full: false,
            scrub: None,
            fix: SeamFix::default(),
            error: None,
        };
        p.play();
        p
    }

    fn kind(&self) -> Kind {
        match (&self.media.proxy, self.media.osv_size) {
            (Some(p), None) => Kind::Lrf(p.clone()),
            (_, size) => Kind::Osv { path: self.media.input.clone(), tracks: self.media.rig.tracks.clone(), size: size.unwrap_or(1920) },
        }
    }

    /// Streams at this OSV size, or from the proxy if `None`.
    pub fn set_osv_size(&mut self, size: Option<u32>) {
        self.media.osv_size = size;
        if self.playing {
            self.seek(self.shown_pts);
        }
    }

    fn snap(&self, t: f64) -> f64 {
        let last = (self.media.duration - 1.0 / self.media.fps).max(0.0);
        ((t * self.media.fps).round() / self.media.fps).clamp(0.0, last)
    }

    fn clock(&self) -> f64 {
        if self.playing { self.anchor_pts + self.anchor_at.elapsed().as_secs_f64() } else { self.anchor_pts }
    }

    /// Starts decoding at `t`. A paused preview only needs one frame, so it
    /// comes from the proxy when there is one: it starts fastest.
    fn start_stream(&mut self, t: f64) {
        self.pending = None;
        let kind = match (&self.media.proxy, self.playing) {
            (Some(p), false) => Kind::Lrf(p.clone()),
            _ => self.kind(),
        };
        self.stream = match source::spawn(kind, self.media.hw, t, self.media.fps) {
            Ok(s) => Some(s),
            Err(e) => {
                self.error = Some(format!("{e:#}"));
                None
            }
        };
    }

    pub fn toggle(&mut self) {
        if self.playing { self.pause() } else { self.play() }
    }

    pub fn play(&mut self) {
        if self.shown_pts >= self.snap(self.media.duration) {
            self.shown_pts = 0.0;
        }
        self.playing = true;
        self.preview_only = false;
        self.still = None;
        self.anchor_pts = self.shown_pts;
        self.anchor_at = Instant::now();
        self.start_stream(self.shown_pts);
    }

    pub fn pause(&mut self) {
        self.playing = false;
        self.anchor_pts = self.shown_pts;
        self.stream = None;
        self.pending = None;
        self.request_still(self.shown_pts);
    }

    /// Jumps to `t`. While paused a proxy frame shows first, then the
    /// full-resolution one.
    pub fn seek(&mut self, t: f64) {
        let t = self.snap(t);
        self.anchor_pts = t;
        if self.playing {
            self.anchor_at = Instant::now();
            self.start_stream(t);
        } else {
            self.preview_only = true;
            self.start_stream(t);
            self.request_still(t);
        }
    }

    pub fn step(&mut self, frames: f64) {
        if !self.playing {
            self.seek(self.shown_pts + frames / self.media.fps);
        }
    }

    /// Follows the seek bar while it is dragged: playback holds, and each
    /// preview starts only once the last one has shown, so a fast drag never
    /// keeps restarting the decoder before anything appears.
    pub fn scrub(&mut self, t: f64) {
        let t = self.snap(t);
        if self.scrub.is_none() {
            self.scrub = Some((self.playing, None));
            self.playing = false;
            self.still = None;
        }
        self.anchor_pts = t;
        // The full-resolution frame on screen is of another time now.
        self.showing_full = false;
        if self.stream.is_some() && self.preview_only {
            self.scrub = self.scrub.map(|(resume, _)| (resume, Some(t)));
        } else {
            self.preview_only = true;
            self.start_stream(t);
        }
    }

    pub fn scrub_end(&mut self) {
        let Some((resume, _)) = self.scrub.take() else { return };
        if resume {
            self.shown_pts = self.anchor_pts;
            self.play();
        } else {
            self.seek(self.anchor_pts);
        }
    }

    /// Where the seek bar should sit: under the pointer while dragging.
    pub fn position(&self) -> f64 {
        if self.scrub.is_some() { self.anchor_pts } else { self.shown_pts }
    }

    fn request_still(&mut self, t: f64) {
        let (tx, rx) = channel();
        let (path, tracks, hw) = (self.media.input.clone(), self.media.rig.tracks.clone(), self.media.hw);
        std::thread::spawn(move || {
            let _ = tx.send(source::still(&path, &tracks, t, hw));
        });
        self.still = Some((t, rx));
    }

    /// Whether the clock or a decode needs polling every display frame.
    pub fn busy(&self) -> bool {
        self.playing || self.stream.is_some() || self.still.is_some()
    }

    /// Pulls whatever the decoders have ready for the current clock and
    /// returns the frame to show now, if it changed, with its seam corrections.
    pub fn tick(&mut self) -> Option<(Frame, SeamFix)> {
        let mut show = None;
        if self.stream.is_some() {
            let due = if self.playing { self.clock() + 0.5 / self.media.fps } else { f64::INFINITY };
            let mut latest = None;
            let mut ended = false;
            loop {
                let next = match self.pending.take() {
                    Some(f) => f,
                    None => match self.stream.as_ref().unwrap().frames.try_recv() {
                        Ok(f) => f,
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            ended = true;
                            break;
                        }
                    },
                };
                if next.pts <= due {
                    latest = Some(next);
                    if self.preview_only {
                        break;
                    }
                } else {
                    self.pending = Some(next);
                    break;
                }
            }
            if let Some(f) = latest {
                // A paused preview never replaces the full-resolution still.
                if !(self.preview_only && self.showing_full && self.still.is_none()) {
                    show = Some((f, false));
                }
                if self.preview_only {
                    self.stream = None;
                    if let Some((resume, Some(t))) = self.scrub {
                        self.scrub = Some((resume, None));
                        self.start_stream(t);
                    }
                }
            } else if self.playing && self.shown_pts < self.clock() - 0.25 {
                // Decoding is slower than real time: let the clock wait for it.
                self.anchor_pts = self.shown_pts;
                self.anchor_at = Instant::now();
            }
            if ended && self.playing {
                self.pause();
            }
        }
        if let Some((t, rx)) = &self.still {
            match rx.try_recv() {
                Ok(Ok(frame)) => {
                    let current = !self.playing && (*t - self.anchor_pts).abs() < 0.5 / self.media.fps;
                    self.still = None;
                    if current {
                        show = Some((frame, true));
                    }
                }
                Ok(Err(e)) => {
                    eprintln!("full-resolution frame failed: {e:#}");
                    self.still = None;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => self.still = None,
            }
        }
        let (frame, full) = show?;
        // Playing frames follow on from each other; anything else is a jump.
        let continuous = self.playing && !full && (frame.pts - self.shown_pts).abs() < 2.0 / self.media.fps;
        self.shown_pts = frame.pts;
        self.showing_full = full;
        let rig = &self.media.rig;
        let rgb = |i: usize, u: f64, v: f64| frame.rgb_at(&rig.lenses[i], rig.tracks[rig.lenses[i].input], u, v);
        if let Some(seam) = &rig.seam {
            let now = seam.measure(rgb);
            if continuous {
                self.fix.gains.ease(&now, exposure::EASE);
            } else {
                self.fix.gains = now;
            }
        }
        if let Some(aligner) = &rig.aligner {
            let now = aligner.measure(|i, u, v| rgb(i, u, v).map(|c| c.iter().sum::<f64>() / 3.0));
            if continuous {
                self.fix.field.ease(&now, align::EASE);
            } else {
                self.fix.field = now;
            }
        }
        Some((frame, self.fix.clone()))
    }

    pub fn rig_from_view(&self, rolling_shutter: bool) -> Vec<M3> {
        let steps = if rolling_shutter && self.media.readout_ms > 0.0 { RS_STEPS_MAX } else { 1 };
        if self.media.stab.has_data() {
            self.media.stab.rig_from_view(self.shown_pts * 1000.0, self.media.readout_ms, steps)
        } else {
            vec![[[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]]
        }
    }

    /// What is on screen: full resolution, the stream, or a pending still.
    pub fn quality(&self) -> String {
        if self.showing_full {
            "full res".to_owned()
        } else if self.still.is_some() {
            "loading full res…".to_owned()
        } else {
            match self.kind() {
                Kind::Lrf(_) => "proxy".to_owned(),
                Kind::Osv { size, .. } => format!("OSV {size} px"),
            }
        }
    }
}
