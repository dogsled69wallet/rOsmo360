//! Real-time 360 player: free look on all three axes, stabilisation and
//! rolling-shutter correction on the GPU. Plays from the LRF proxy and swaps
//! in a full-resolution OSV frame whenever playback is paused.

mod gpu;
mod source;

use crate::stab::{Mode, Stabilizer};
use crate::stitch::{Lens, M3, mat_mul};
use anyhow::Result;
use source::{Frame, Kind, Stream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::Instant;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Fullscreen, Window, WindowId};

pub struct Media {
    pub input: PathBuf,
    /// Side-by-side proxy, if the clip has one.
    pub proxy: Option<PathBuf>,
    /// Stream the OSV at this size instead of the proxy.
    pub osv_size: Option<u32>,
    pub lenses: Vec<Lens>,
    pub tracks: Vec<u32>,
    pub stab: Stabilizer,
    pub readout_ms: f64,
    pub fps: f64,
    pub duration: f64,
}

pub fn run(media: Media) -> Result<()> {
    let event_loop = EventLoop::new()?;
    let mut app = App::new(media);
    event_loop.run_app(&mut app)?;
    app.error.map_or(Ok(()), Err)
}

const PROJECTIONS: [&str; 3] = ["perspective", "little planet", "equirect"];

struct Look {
    yaw: f64,
    pitch: f64,
    roll: f64,
    fov_deg: f64,
    projection: u32,
}

impl Look {
    fn reset(&mut self) {
        *self = Look { projection: self.projection, ..Look::default() };
        if self.projection == 1 {
            self.pitch = -90f64.to_radians();
            self.fov_deg = 120.0;
        }
    }

    /// view_from_screen-ray: yaw about +y, then pitch about +x, then roll.
    fn matrix(&self) -> M3 {
        let (sy, cy) = self.yaw.sin_cos();
        let (sp, cp) = self.pitch.sin_cos();
        let (sr, cr) = self.roll.sin_cos();
        let ry = [[cy, 0.0, sy], [0.0, 1.0, 0.0], [-sy, 0.0, cy]];
        let rx = [[1.0, 0.0, 0.0], [0.0, cp, -sp], [0.0, sp, cp]];
        let rz = [[cr, -sr, 0.0], [sr, cr, 0.0], [0.0, 0.0, 1.0]];
        mat_mul(ry, mat_mul(rx, rz))
    }

    fn zoom(&self) -> f32 {
        (self.fov_deg.to_radians() / 2.0).tan() as f32
    }
}

impl Default for Look {
    fn default() -> Self {
        Look { yaw: 0.0, pitch: 0.0, roll: 0.0, fov_deg: 90.0, projection: 0 }
    }
}

struct App {
    media: Media,
    window: Option<Arc<Window>>,
    gpu: Option<gpu::Gpu>,
    error: Option<anyhow::Error>,

    look: Look,
    rolling_shutter: bool,
    mods: ModifiersState,
    cursor: Option<PhysicalPosition<f64>>,
    drag: Option<MouseButton>,

    playing: bool,
    /// Playback clock: `anchor_pts` at `anchor_at`.
    anchor_pts: f64,
    anchor_at: Instant,
    shown_pts: f64,
    stream: Option<Stream>,
    pending: Option<Frame>,
    /// While paused, a stream is only kept for its first frame.
    preview_only: bool,
    still: Option<(f64, Receiver<Result<Frame>>)>,
    showing_full: bool,
    /// Something changed that a paused, idle player has to redraw for.
    dirty: bool,
}

impl App {
    fn new(media: Media) -> Self {
        App {
            media,
            window: None,
            gpu: None,
            error: None,
            look: Look::default(),
            rolling_shutter: true,
            mods: ModifiersState::empty(),
            cursor: None,
            drag: None,
            playing: false,
            anchor_pts: 0.0,
            anchor_at: Instant::now(),
            shown_pts: 0.0,
            stream: None,
            pending: None,
            preview_only: false,
            still: None,
            showing_full: false,
            dirty: true,
        }
    }

    fn kind(&self) -> Kind {
        match (&self.media.proxy, self.media.osv_size) {
            (Some(p), None) => Kind::Lrf(p.clone()),
            (_, size) => Kind::Osv { path: self.media.input.clone(), tracks: self.media.tracks.clone(), size: size.unwrap_or(1920) },
        }
    }

    fn snap(&self, t: f64) -> f64 {
        let last = (self.media.duration - 1.0 / self.media.fps).max(0.0);
        ((t * self.media.fps).round() / self.media.fps).clamp(0.0, last)
    }

    fn clock(&self) -> f64 {
        if self.playing { self.anchor_pts + self.anchor_at.elapsed().as_secs_f64() } else { self.anchor_pts }
    }

    fn start_stream(&mut self, t: f64) {
        self.pending = None;
        self.stream = match source::spawn(self.kind(), t, self.media.fps) {
            Ok(s) => Some(s),
            Err(e) => {
                self.error = Some(e);
                None
            }
        };
    }

    fn play(&mut self) {
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

    fn pause(&mut self) {
        self.playing = false;
        self.anchor_pts = self.shown_pts;
        self.stream = None;
        self.pending = None;
        self.request_still(self.shown_pts);
    }

    fn seek(&mut self, t: f64) {
        let t = self.snap(t);
        if self.playing {
            self.anchor_pts = t;
            self.anchor_at = Instant::now();
            self.start_stream(t);
        } else {
            self.anchor_pts = t;
            self.preview_only = true;
            self.start_stream(t);
            self.request_still(t);
        }
    }

    fn request_still(&mut self, t: f64) {
        let (tx, rx) = channel();
        let (path, tracks) = (self.media.input.clone(), self.media.tracks.clone());
        std::thread::spawn(move || {
            let _ = tx.send(source::still(&path, &tracks, t));
        });
        self.still = Some((t, rx));
    }

    fn upload(&mut self, frame: &Frame, full: bool) {
        if let Some(gpu) = &mut self.gpu {
            gpu.upload(frame, &self.media.lenses, &self.media.tracks);
        }
        self.shown_pts = frame.pts;
        self.showing_full = full;
    }

    /// Pulls whatever the decoders have ready for the current clock.
    fn advance(&mut self) {
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
                    self.upload(&f, false);
                }
                if self.preview_only {
                    self.stream = None;
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
                        self.upload(&frame, true);
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
    }

    fn redraw(&mut self) {
        self.dirty = false;
        self.advance();
        let steps = if self.rolling_shutter && self.media.readout_ms > 0.0 { gpu::RS_STEPS_MAX } else { 1 };
        let rig_from_view = if self.media.stab.has_data() {
            self.media.stab.rig_from_view(self.shown_pts * 1000.0, self.media.readout_ms, steps)
        } else {
            vec![[[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]]
        };
        let view = gpu::View { user: self.look.matrix(), zoom: self.look.zoom(), projection: self.look.projection, rig_from_view };
        if let Some(gpu) = &mut self.gpu
            && let Err(e) = gpu.render(&view)
        {
            self.error = Some(e);
        }
        self.update_title();
    }

    fn update_title(&self) {
        let Some(w) = &self.window else { return };
        let mode = match self.media.stab.mode() {
            Mode::Off => "stab off",
            Mode::Horizon => "horizon",
            Mode::Lock => "world lock",
        };
        w.set_title(&format!(
            "osmo360 · {:.2} / {:.1} s {} · {} · RS {} · {} · {} · fov {:.0}° · yaw {:.0}° pitch {:.0}° roll {:.0}°",
            self.shown_pts,
            self.media.duration,
            if self.playing { "▶" } else { "❚❚" },
            mode,
            if self.rolling_shutter { "on" } else { "off" },
            if self.showing_full { "full res" } else if self.still.is_some() { "loading full res…" } else { "proxy" },
            PROJECTIONS[self.look.projection as usize],
            self.look.fov_deg,
            self.look.yaw.to_degrees(),
            self.look.pitch.to_degrees(),
            self.look.roll.to_degrees(),
        ));
    }

    fn key(&mut self, event: &KeyEvent, el: &ActiveEventLoop) {
        if event.state != ElementState::Pressed {
            return;
        }
        let step = 5f64.to_radians();
        let shift = self.mods.shift_key();
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => el.exit(),
            Key::Named(NamedKey::Space) => {
                if self.playing {
                    self.pause()
                } else {
                    self.play()
                }
            }
            Key::Named(NamedKey::ArrowLeft) if shift => self.look.yaw -= 2.0 * step,
            Key::Named(NamedKey::ArrowRight) if shift => self.look.yaw += 2.0 * step,
            Key::Named(NamedKey::ArrowLeft) => self.seek(self.shown_pts - 5.0),
            Key::Named(NamedKey::ArrowRight) => self.seek(self.shown_pts + 5.0),
            Key::Named(NamedKey::ArrowUp) => self.look.pitch += step,
            Key::Named(NamedKey::ArrowDown) => self.look.pitch -= step,
            Key::Named(NamedKey::Home) => self.look.reset(),
            Key::Character(c) => match c.as_str() {
                "q" | "Q" => self.look.roll -= step,
                "e" | "E" => self.look.roll += step,
                "0" => self.look.reset(),
                "," if !self.playing => self.seek(self.shown_pts - 1.0 / self.media.fps),
                "." if !self.playing => self.seek(self.shown_pts + 1.0 / self.media.fps),
                "+" | "=" => self.zoom_by(-5.0),
                "-" => self.zoom_by(5.0),
                "s" | "S" => {
                    let next = match self.media.stab.mode() {
                        Mode::Off => Mode::Horizon,
                        Mode::Horizon => Mode::Lock,
                        Mode::Lock => Mode::Off,
                    };
                    self.media.stab.set_mode(next);
                }
                "r" | "R" => self.rolling_shutter = !self.rolling_shutter,
                "p" | "P" => {
                    self.look.projection = (self.look.projection + 1) % PROJECTIONS.len() as u32;
                    self.look.reset();
                }
                "f" | "F" => {
                    if let Some(w) = &self.window {
                        w.set_fullscreen(if w.fullscreen().is_some() { None } else { Some(Fullscreen::Borderless(None)) });
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    fn zoom_by(&mut self, deg: f64) {
        let (lo, hi) = if self.look.projection == 1 { (60.0, 170.0) } else { (20.0, 150.0) };
        self.look.fov_deg = (self.look.fov_deg + deg).clamp(lo, hi);
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes().with_title("osmo360").with_inner_size(LogicalSize::new(1280.0, 720.0));
        let result = el.create_window(attrs).map_err(anyhow::Error::from).and_then(|w| {
            let w = Arc::new(w);
            let gpu = gpu::Gpu::new(w.clone(), &self.media.lenses)?;
            Ok((w, gpu))
        });
        match result {
            Ok((w, gpu)) => {
                self.window = Some(w);
                self.gpu = Some(gpu);
                self.play();
            }
            Err(e) => {
                self.error = Some(e);
                el.exit();
            }
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::Resized(size) => {
                if let Some(gpu) = &mut self.gpu {
                    gpu.resize(size.width, size.height);
                }
            }
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::KeyboardInput { event, .. } => self.key(&event, el),
            WindowEvent::MouseInput { state, button, .. } => {
                self.drag = (state == ElementState::Pressed).then_some(button);
            }
            WindowEvent::CursorMoved { position, .. } => {
                if let (Some(prev), Some(button), Some(gpu)) = (self.cursor, self.drag, &self.gpu) {
                    // Grab-style: the picture follows the pointer.
                    let k = self.look.fov_deg.to_radians() / gpu.size().1 as f64;
                    let (dx, dy) = (position.x - prev.x, position.y - prev.y);
                    match button {
                        MouseButton::Left => {
                            self.look.yaw -= dx * k;
                            self.look.pitch += dy * k;
                        }
                        MouseButton::Right => self.look.roll += dx * k,
                        _ => {}
                    }
                }
                self.cursor = Some(position);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as f64,
                    MouseScrollDelta::PixelDelta(p) => p.y / 40.0,
                };
                self.zoom_by(-lines * 5.0);
            }
            WindowEvent::RedrawRequested => {
                self.redraw();
                return;
            }
            _ => {}
        }
        self.dirty = true;
        if self.error.is_some() {
            el.exit();
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        let busy = self.playing || self.stream.is_some() || self.still.is_some();
        if let Some(w) = &self.window
            && (busy || self.dirty)
        {
            w.request_redraw();
        }
        el.set_control_flow(ControlFlow::Wait);
    }
}
