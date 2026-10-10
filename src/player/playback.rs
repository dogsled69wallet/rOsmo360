//! The playback clock: plays from the stream (proxy or scaled OSV) and, if
//! asked to, swaps in a full-resolution OSV frame whenever playback is paused.

use super::gpu::RS_STEPS_MAX;
use super::source::{self, Frame, Kind, Stream};
use super::Media;
use super::audio::Audio;
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
    /// Swap in a full-resolution frame when paused. Off, a paused frame
    /// stays the stream's quality, so stopping doesn't change the picture.
    full_on_pause: bool,
    /// While the seek bar is dragged: whether to resume playing on release,
    /// and the latest position asked for while a preview was still decoding.
    scrub: Option<(bool, Option<f64>)>,
    /// Seam corrections, eased from frame to frame while playing.
    fix: SeamFix,
    /// Plays while playing, and is then the clock.
    audio: Option<Audio>,
    /// Sound starts with the next frame shown. Sound is ready in
    /// milliseconds but a fresh HEVC decode takes far longer, so starting both
    /// together left the picture trailing the sound after every seek or
    /// source switch.
    sound_waits: bool,
    volume: f32,
    pub error: Option<String>,
}

impl Playback {
    pub fn new(media: Media, volume: f32, full_on_pause: bool) -> Self {
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
            full_on_pause,
            scrub: None,
            fix: SeamFix::default(),
            audio: None,
            sound_waits: false,
            volume,
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
        match (self.playing, &self.audio) {
            (false, _) => self.anchor_pts,
            _ if self.sound_waits => self.anchor_pts,
            (true, Some(audio)) => self.anchor_pts + audio.elapsed(),
            (true, None) => self.anchor_pts + self.anchor_at.elapsed().as_secs_f64(),
        }
    }

    /// Stops the sound until the next frame is shown, and the clock with it.
    fn hold_sound(&mut self) {
        self.audio = None;
        self.sound_waits = self.playing && self.media.has_audio;
    }

    /// Starts sound at `t`. Without a usable device the clip plays silently
    /// on the wall clock; that is reported once, not on every play.
    fn start_audio(&mut self, t: f64) {
        self.audio = None;
        if !self.media.has_audio {
            return;
        }
        match Audio::start(&self.media.input, t, self.volume) {
            Ok(a) => self.audio = Some(a),
            Err(e) => {
                eprintln!("no sound: {e:#}");
                self.media.has_audio = false;
            }
        }
    }

    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume;
        if let Some(a) = &self.audio {
            a.set_volume(volume);
        }
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
        self.hold_sound();
    }

    pub fn pause(&mut self) {
        self.playing = false;
        self.hold_sound();
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
            self.hold_sound();
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
            self.hold_sound();
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

    /// Turns the full-resolution paused frame on or off, swapping the one on
    /// screen now if paused.
    pub fn set_full_on_pause(&mut self, on: bool) {
        self.full_on_pause = on;
        if !self.playing && self.scrub.is_none() {
            if on {
                self.request_still(self.shown_pts);
            } else {
                self.still = None;
                if self.showing_full {
                    self.showing_full = false;
                    self.preview_only = true;
                    self.start_stream(self.shown_pts);
                }
            }
        }
    }

    fn request_still(&mut self, t: f64) {
        if !self.full_on_pause {
            return;
        }
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
        // A slow machine at full resolution can fall behind the sound; then
        // the sound restarts from the picture rather than drifting from it.
        if self.audio.is_some() && self.clock() - self.shown_pts > 0.5 {
            self.anchor_pts = self.shown_pts;
            self.hold_sound();
        }
        if self.stream.is_some() {
            let due = if self.playing && !self.sound_waits { self.clock() + 0.5 / self.media.fps } else { f64::INFINITY };
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
                    if self.preview_only || self.sound_waits {
                        break;
                    }
                } else {
                    self.pending = Some(next);
                    break;
                }
            }
            if let Some(f) = latest {
                if self.sound_waits && self.playing {
                    self.sound_waits = false;
                    self.anchor_pts = f.pts;
                    self.start_audio(f.pts);
                }
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
            } else if self.playing && self.audio.is_none() && self.shown_pts < self.clock() - 0.25 {
                // Decoding is slower than real time: let the clock wait for
                // it. Sound can't wait, so with sound the picture skips.
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
            let now = aligner.measure(&aligner.strips(|i, u, v| rgb(i, u, v).map(|c| c.iter().sum::<f64>() / 3.0)), continuous.then_some(&self.fix.field));
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
