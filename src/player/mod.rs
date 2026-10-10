//! Real-time 360 player: an iced window with the stitched view, an open-file
//! dialog and playback controls. Free look on all three axes, stabilisation
//! and rolling-shutter correction run in the view's shader.

mod audio;
mod export;
mod gpu;
mod playback;
mod source;

use crate::ffmpeg::{self, Hwaccel};
use crate::stab::{ImuAxes, Mode, Stabilizer};
use crate::stitch::{M3, mat_mul};
use anyhow::{Context, Result};
use gpu::{Rig, Shown};
use iced::keyboard::{self, Key, key::Named};
use iced::mouse;
use iced::widget::{button, center, column, container, pick_list, row, shader, slider, space, svg, text, toggler, tooltip};
use iced::window;
use iced::{Alignment, Element, Length, Point, Rectangle, Size, Subscription, Task, Theme};
use playback::Playback;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct Media {
    pub input: PathBuf,
    /// Side-by-side proxy, if the clip has one.
    pub proxy: Option<PathBuf>,
    /// Stream the OSV at this size instead of the proxy.
    pub osv_size: Option<u32>,
    /// Pixels square of the OSV's lens tracks.
    pub osv_full: u32,
    pub hw: Hwaccel,
    pub has_audio: bool,
    pub rig: Arc<Rig>,
    pub stab: Stabilizer,
    pub readout_ms: f64,
    pub fps: f64,
    pub duration: f64,
}

impl fmt::Debug for Media {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Media").field("input", &self.input).finish_non_exhaustive()
    }
}

/// Settings from the command line; the GUI updates them so the next clip
/// opens the way the last one was left.
#[derive(Clone)]
pub struct Options {
    pub osv_size: Option<u32>,
    /// Forced decoder, or the one found by probing the first clip.
    pub hw: Option<Hwaccel>,
    pub stabilize: Mode,
    pub imu_axes: ImuAxes,
    pub world_z_down: bool,
}

impl Media {
    fn load(input: PathBuf, opts: &Options) -> Result<Media> {
        let render = crate::RenderOpts {
            width: 0,
            variant: None,
            only: None,
            stabilize: opts.stabilize,
            smooth: 500.0,
            imu_axes: opts.imu_axes,
            world_z_down: opts.world_z_down,
            no_rs: false,
            no_exposure: false,
            no_align: false,
        };
        let p = crate::Pipeline::new(&input, &render)?;
        let proxy = ["LRF", "lrf"].iter().map(|ext| input.with_extension(ext)).find(|p| p.exists());
        let hw = match opts.hw {
            Some(hw) => hw,
            None => Hwaccel::pick(&input, p.tracks[0])?,
        };
        eprintln!("decoding with {hw:?}");
        Ok(Media {
            duration: ffmpeg::duration(&input)?,
            osv_full: ffmpeg::dimensions(&input, p.tracks[0])?.0 as u32,
            has_audio: ffmpeg::has_audio(&input)?,
            proxy,
            osv_size: opts.osv_size,
            hw,
            rig: Arc::new(Rig::new(p.lenses, p.tracks)),
            stab: p.stab,
            readout_ms: p.readout_ms,
            fps: p.fps,
            input,
        })
    }
}

pub fn run(input: Option<PathBuf>, opts: Options) -> Result<()> {
    iced::application(
        move || {
            let mut app = App::new(opts.clone());
            let task = match &input {
                Some(path) => app.open(path.clone()),
                None => Task::none(),
            };
            (app, task)
        },
        App::update,
        App::view,
    )
    .title(App::title)
    .subscription(App::subscription)
    .theme(|_: &App| Theme::Dark)
    .window_size((1280.0, 820.0))
    .run()
    .context("running the player window")
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Projection {
    Perspective,
    LittlePlanet,
    Equirect,
}

impl Projection {
    const ALL: [Projection; 3] = [Projection::Perspective, Projection::LittlePlanet, Projection::Equirect];
}

impl fmt::Display for Projection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Projection::Perspective => "Perspective",
            Projection::LittlePlanet => "Little planet",
            Projection::Equirect => "Equirect",
        })
    }
}

/// What "save" writes: the view as framed, or the whole sphere.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Saving {
    View,
    Sphere,
}

/// Longest side of a saved view, and width of a saved sphere: the lenses'
/// full resolution holds about this much detail.
const SAVE_VIEW_PX: f32 = 3840.0;
const SAVE_SPHERE_PX: usize = 7680;

/// What the player streams while playing.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Source {
    Proxy,
    Osv(u32),
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Source::Proxy => f.write_str("Proxy (LRF)"),
            Source::Osv(s) => write!(f, "OSV {s} px"),
        }
    }
}

struct Look {
    yaw: f64,
    pitch: f64,
    roll: f64,
    fov_deg: f64,
    projection: Projection,
}

impl Look {
    fn reset(&mut self) {
        *self = match self.projection {
            Projection::LittlePlanet => Look::default(),
            projection => Look { yaw: 0.0, pitch: 0.0, roll: 0.0, fov_deg: 90.0, projection },
        };
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

    /// The planet zooms out until the whole sphere is a small disc, the way
    /// DJI Studio's tiny planet does.
    fn fov_range(&self) -> (f64, f64) {
        if self.projection == Projection::LittlePlanet { (10.0, 179.5) } else { (20.0, 150.0) }
    }

    /// Zooms by `steps` scroll notches, each scaling the picture by 10%:
    /// near the planet's limit a fixed step in degrees would barely move.
    fn zoom_by(&mut self, steps: f64) {
        let (lo, hi) = self.fov_range();
        let half = ((self.fov_deg.to_radians() / 2.0).tan() * 1.1f64.powf(steps)).atan();
        self.fov_deg = (2.0 * half.to_degrees()).clamp(lo, hi);
    }
}

/// Opens on the little planet, as DJI Studio does.
impl Default for Look {
    fn default() -> Self {
        Look { yaw: 0.0, pitch: -90f64.to_radians(), roll: 0.0, fov_deg: 120.0, projection: Projection::LittlePlanet }
    }
}

/// An open clip: its playback plus the frame the view draws.
struct Clip {
    playback: Playback,
    shown: Option<Arc<Shown>>,
}

#[derive(Debug, Clone)]
enum Message {
    Open,
    Picked(Option<PathBuf>),
    /// Never cloned in flight, so the clip can be taken back out of the `Arc`.
    Loaded(Arc<Result<Media, String>>),
    Tick,
    Key(Key, keyboard::Modifiers),
    Toggle,
    Scrub(f64),
    ScrubDone,
    Look { yaw: f64, pitch: f64, roll: f64 },
    /// Scroll notches; positive zooms out.
    Zoom(f64),
    Fov(f64),
    Stab(Mode),
    Projection(Projection),
    Source(Source),
    RollingShutter(bool),
    Mute,
    Volume(f32),
    /// The 360 view's size on screen, so a saved view keeps its shape.
    ViewSize(Size),
    Save(Saving),
    SaveTo(Saving, Option<PathBuf>),
    Saved(Result<PathBuf, String>),
    ResetView,
    Fullscreen,
}

struct App {
    opts: Options,
    clip: Option<Clip>,
    loading: Option<PathBuf>,
    error: Option<String>,
    look: Look,
    rolling_shutter: bool,
    fullscreen: bool,
    volume: f32,
    muted: bool,
    view_size: Size,
    /// What the last save did, or that one is running.
    save_status: Option<String>,
}

impl App {
    fn new(opts: Options) -> Self {
        App {
            opts,
            clip: None,
            loading: None,
            error: None,
            look: Look::default(),
            rolling_shutter: true,
            fullscreen: false,
            volume: 1.0,
            muted: false,
            view_size: Size::new(16.0, 9.0),
            save_status: None,
        }
    }

    /// What the speakers get: nothing while muted, so unmuting restores
    /// the level that was set.
    fn volume(&self) -> f32 {
        if self.muted { 0.0 } else { self.volume }
    }

    /// Parses the clip and probes the decoder off the UI thread.
    fn open(&mut self, path: PathBuf) -> Task<Message> {
        self.loading = Some(path.clone());
        self.error = None;
        let opts = self.opts.clone();
        Task::perform(blocking(move || Media::load(path, &opts).map_err(|e| format!("{e:#}"))), |r| Message::Loaded(Arc::new(r)))
    }

    fn title(&self) -> String {
        let name = |p: &Path| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        match (&self.loading, &self.clip) {
            (Some(p), _) => format!("osmo360 · loading {}", name(p)),
            (None, Some(c)) => format!("osmo360 · {}", name(&c.playback.media.input)),
            (None, None) => "osmo360".to_owned(),
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        let keys = keyboard::listen().filter_map(|e| match e {
            keyboard::Event::KeyPressed { key, modifiers, .. } => Some(Message::Key(key, modifiers)),
            _ => None,
        });
        let busy = self.clip.as_ref().is_some_and(|c| c.playback.busy());
        if busy { Subscription::batch([keys, window::frames().map(|_| Message::Tick)]) } else { keys }
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Open => {
                let dir = self.clip.as_ref().and_then(|c| c.playback.media.input.parent().map(Path::to_owned));
                return Task::perform(
                    async move {
                        let mut dialog = rfd::AsyncFileDialog::new().set_title("Open Osmo 360 clip").add_filter("Osmo 360 video", &["OSV", "osv"]);
                        if let Some(dir) = dir {
                            dialog = dialog.set_directory(dir);
                        }
                        dialog.pick_file().await.map(|f| f.path().to_owned())
                    },
                    Message::Picked,
                );
            }
            Message::Picked(Some(path)) => return self.open(path),
            Message::Picked(None) => {}
            Message::Loaded(result) => {
                self.loading = None;
                match Arc::into_inner(result).expect("loaded clip is not shared") {
                    Ok(media) => {
                        self.opts.hw = Some(media.hw);
                        let playback = Playback::new(media, self.volume());
                        self.error = playback.error.clone();
                        self.clip = Some(Clip { playback, shown: None });
                    }
                    Err(e) => self.error = Some(e),
                }
            }
            Message::Tick => self.tick(),
            Message::Key(key, mods) => return self.key(key, mods),
            Message::Toggle => self.with_playback(Playback::toggle),
            Message::Scrub(t) => self.with_playback(|p| p.scrub(t)),
            Message::ScrubDone => self.with_playback(Playback::scrub_end),
            Message::Look { yaw, pitch, roll } => {
                self.look.yaw += yaw;
                self.look.pitch += pitch;
                self.look.roll += roll;
            }
            Message::Zoom(deg) => self.look.zoom_by(deg),
            Message::Fov(deg) => self.look.fov_deg = deg,
            Message::Stab(mode) => {
                self.opts.stabilize = mode;
                self.with_playback(|p| p.media.stab.set_mode(mode));
            }
            Message::Projection(p) => {
                self.look.projection = p;
                self.look.reset();
            }
            Message::Source(s) => {
                let size = match s {
                    Source::Proxy => None,
                    Source::Osv(size) => Some(size),
                };
                self.opts.osv_size = size;
                self.with_playback(|p| p.set_osv_size(size));
            }
            Message::RollingShutter(on) => self.rolling_shutter = on,
            Message::ViewSize(size) => self.view_size = size,
            Message::Save(what) => {
                let Some(c) = &self.clip else { return Task::none() };
                let media = &c.playback.media;
                let stem = media.input.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                let name = format!("{stem}_{:.2}s_{}.png", c.playback.shown_pts, if what == Saving::View { "view" } else { "360" });
                let dir = media.input.parent().map(Path::to_owned);
                return Task::perform(
                    async move {
                        let mut dialog = rfd::AsyncFileDialog::new().set_title("Save photo").add_filter("PNG image", &["png"]).set_file_name(name);
                        if let Some(dir) = dir {
                            dialog = dialog.set_directory(dir);
                        }
                        dialog.save_file().await.map(|f| f.path().to_owned())
                    },
                    move |path| Message::SaveTo(what, path),
                );
            }
            Message::SaveTo(_, None) => {}
            Message::SaveTo(what, Some(path)) => {
                let Some(c) = &self.clip else { return Task::none() };
                let media = &c.playback.media;
                let shot = match what {
                    Saving::Sphere => export::Shot::Sphere { width: SAVE_SPHERE_PX },
                    Saving::View => {
                        let k = SAVE_VIEW_PX / self.view_size.width.max(self.view_size.height).max(1.0);
                        // Even sizes keep image tools that halve chroma happy.
                        let px = |v: f32| ((v * k / 2.0).round() as usize).max(1) * 2;
                        export::Shot::View {
                            user: self.look.matrix(),
                            zoom: self.look.zoom() as f64,
                            projection: self.look.projection as u32,
                            size: (px(self.view_size.width), px(self.view_size.height)),
                        }
                    }
                };
                let request = export::Request {
                    input: media.input.clone(),
                    hw: media.hw,
                    rig: media.rig.clone(),
                    stab: media.stab.clone(),
                    readout_ms: media.readout_ms,
                    t: c.playback.shown_pts,
                    rolling_shutter: self.rolling_shutter,
                    shot,
                    path,
                };
                self.save_status = Some("Saving photo…".to_owned());
                return Task::perform(blocking(move || export::save(request).map_err(|e| format!("{e:#}"))), Message::Saved);
            }
            Message::Saved(result) => match result {
                Ok(path) => self.save_status = Some(format!("Saved {}", path.display())),
                Err(e) => {
                    self.save_status = None;
                    self.error = Some(format!("Saving failed: {e}"));
                }
            },
            Message::Mute => {
                self.muted = !self.muted;
                let v = self.volume();
                self.with_playback(|p| p.set_volume(v));
            }
            Message::Volume(v) => {
                self.volume = v;
                self.muted = false;
                self.with_playback(|p| p.set_volume(v));
            }
            Message::ResetView => self.look.reset(),
            Message::Fullscreen => {
                self.fullscreen = !self.fullscreen;
                let mode = if self.fullscreen { window::Mode::Fullscreen } else { window::Mode::Windowed };
                return window::latest().and_then(move |id| window::set_mode(id, mode));
            }
        }
        Task::none()
    }

    fn with_playback(&mut self, f: impl FnOnce(&mut Playback)) {
        if let Some(c) = &mut self.clip {
            f(&mut c.playback);
            self.error = c.playback.error.clone();
        }
    }

    fn tick(&mut self) {
        let Some(c) = &mut self.clip else { return };
        if let Some((frame, fix)) = c.playback.tick() {
            c.shown = Some(Arc::new(Shown::new(frame, fix)));
        }
        self.error = c.playback.error.clone();
    }

    fn key(&mut self, key: Key, mods: keyboard::Modifiers) -> Task<Message> {
        let step = 5f64.to_radians();
        match key.as_ref() {
            Key::Named(Named::Escape) if self.fullscreen => return self.update(Message::Fullscreen),
            Key::Named(Named::Space) => self.with_playback(Playback::toggle),
            Key::Named(Named::ArrowLeft) if mods.shift() => self.look.yaw -= 2.0 * step,
            Key::Named(Named::ArrowRight) if mods.shift() => self.look.yaw += 2.0 * step,
            Key::Named(Named::ArrowLeft) => self.with_playback(|p| p.seek(p.shown_pts - 5.0)),
            Key::Named(Named::ArrowRight) => self.with_playback(|p| p.seek(p.shown_pts + 5.0)),
            Key::Named(Named::ArrowUp) => self.look.pitch += step,
            Key::Named(Named::ArrowDown) => self.look.pitch -= step,
            Key::Named(Named::Home) => self.look.reset(),
            Key::Character(c) => match c {
                "o" if mods.command() => return self.update(Message::Open),
                "q" | "Q" => self.look.roll -= step,
                "e" | "E" => self.look.roll += step,
                "0" => self.look.reset(),
                "," => self.with_playback(|p| p.step(-1.0)),
                "." => self.with_playback(|p| p.step(1.0)),
                "+" | "=" => self.look.zoom_by(-1.0),
                "-" => self.look.zoom_by(1.0),
                "s" | "S" => {
                    let mode = self.clip.as_ref().map_or(self.opts.stabilize, |c| c.playback.media.stab.mode());
                    let next = match mode {
                        Mode::Off => Mode::Horizon,
                        Mode::Horizon => Mode::Lock,
                        Mode::Lock => Mode::Off,
                    };
                    return self.update(Message::Stab(next));
                }
                "r" | "R" => self.rolling_shutter = !self.rolling_shutter,
                "m" | "M" => return self.update(Message::Mute),
                "p" | "P" => {
                    let i = Projection::ALL.iter().position(|&p| p == self.look.projection).unwrap_or(0);
                    return self.update(Message::Projection(Projection::ALL[(i + 1) % Projection::ALL.len()]));
                }
                "f" | "F" => return self.update(Message::Fullscreen),
                _ => {}
            },
            _ => {}
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        let open = button("Open…").on_press(Message::Open);
        let error = self.error.as_ref().map(|e| text(e.clone()).style(text::danger));
        let Some(clip) = &self.clip else {
            let status = match &self.loading {
                Some(p) => format!("Loading {}…", p.display()),
                None => "Open a DJI Osmo 360 .OSV clip to start.".to_owned(),
            };
            return center(column![text(status), open].push(error).spacing(16).align_x(Alignment::Center)).into();
        };
        let p = &clip.playback;
        let media = &p.media;

        let status = match (&self.loading, &self.save_status) {
            (Some(path), _) => format!("Loading {}…", path.display()),
            (None, Some(s)) => format!("{s} · {} · {:?} decode", p.quality(), media.hw),
            (None, None) => format!("{} · {:?} decode", p.quality(), media.hw),
        };
        let top = row![open, text(media.input.display().to_string()), space().width(Length::Fill), text(status)]
            .spacing(12)
            .padding([6, 10])
            .align_y(Alignment::Center);

        let view: Element<'_, Message> = match &clip.shown {
            Some(shown) => shader(Sphere {
                rig: media.rig.clone(),
                shown: shown.clone(),
                user: self.look.matrix(),
                zoom: self.look.zoom(),
                projection: self.look.projection as u32,
                rig_from_view: p.rig_from_view(self.rolling_shutter),
                fov_deg: self.look.fov_deg,
            })
            .width(Length::Fill)
            .height(Length::Fill)
            .into(),
            None => center(text("Decoding…")).into(),
        };
        let view = container(view).width(Length::Fill).height(Length::Fill).style(|_| container::background(iced::Color::BLACK));

        let last = (media.duration - 1.0 / media.fps).max(0.0);
        let transport = row![
            button(text(if p.playing { "Pause" } else { "Play" }).width(48).align_x(Alignment::Center)).on_press(Message::Toggle),
            tooltip(
                button("Save view…").on_press_maybe((!p.playing).then_some(Message::Save(Saving::View))),
                text("Save what you see as a PNG (pause first)"),
                tooltip::Position::Top,
            ),
            tooltip(
                button("Save 360°…").on_press_maybe((!p.playing).then_some(Message::Save(Saving::Sphere))),
                text("Save the whole sphere as an equirect PNG (pause first)"),
                tooltip::Position::Top,
            ),
            text(format!("{} / {}", clock(p.position()), clock(media.duration))),
            slider(0.0..=last, p.position().min(last), Message::Scrub).step(1.0 / media.fps).on_release(Message::ScrubDone),
            tooltip(
                button(svg(svg::Handle::from_memory(if self.muted { SPEAKER_MUTED } else { SPEAKER })).width(20).height(20))
                    .padding(5)
                    .on_press(Message::Mute),
                text(if self.muted { "Unmute (M)" } else { "Mute (M)" }),
                tooltip::Position::Top,
            ),
            slider(0.0..=1.0, self.volume(), Message::Volume).step(0.01f32).width(90),
        ]
        .spacing(12)
        .padding([4, 10])
        .align_y(Alignment::Center);

        let mut sources = Vec::new();
        if media.proxy.is_some() {
            sources.push(Source::Proxy);
        }
        sources.push(Source::Osv(1920.min(media.osv_full)));
        sources.push(Source::Osv(media.osv_full));
        sources.dedup();
        let source = match (media.osv_size, &media.proxy) {
            (None, Some(_)) => Source::Proxy,
            (size, _) => Source::Osv(size.unwrap_or(1920)),
        };
        let (lo, hi) = self.look.fov_range();
        let controls = row![
            labelled("Stabilisation", pick_list(&Mode::ALL[..], Some(media.stab.mode()), Message::Stab)),
            labelled("Projection", pick_list(&Projection::ALL[..], Some(self.look.projection), Message::Projection)),
            labelled("Playback source", pick_list(sources, Some(source), Message::Source)),
            labelled("FOV", slider(lo..=hi, self.look.fov_deg, Message::Fov).width(120)),
            toggler(self.rolling_shutter).label("Rolling shutter").on_toggle(Message::RollingShutter),
            button("Reset view").on_press(Message::ResetView),
            button(if self.fullscreen { "Exit fullscreen" } else { "Fullscreen" }).on_press(Message::Fullscreen),
        ]
        .spacing(18)
        .padding([6, 10])
        .align_y(Alignment::Center)
        .wrap();

        column![top, view, transport, controls].push(error.map(|e| container(e).padding([0, 10]))).into()
    }
}

/// Speaker icons for the mute button, drawn in the button's text colour.
const SPEAKER: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M11 5 6 9H2v6h4l5 4z" fill="white"/><path d="M15.5 8.5a5 5 0 0 1 0 7"/><path d="M19 5a10 10 0 0 1 0 14"/></svg>"#;
const SPEAKER_MUTED: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M11 5 6 9H2v6h4l5 4z" fill="white"/><path d="m22 9-6 6"/><path d="m16 9 6 6"/></svg>"#;

fn labelled<'a>(label: &'a str, widget: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    row![text(label), widget.into()].spacing(6).align_y(Alignment::Center).into()
}

fn clock(t: f64) -> String {
    format!("{}:{:05.2}", (t / 60.0) as u32, t % 60.0)
}

/// Runs `f` on its own thread so blocking work (parsing, ffprobe) stays off
/// the executor.
fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> impl Future<Output = T> {
    let (tx, rx) = iced::futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    async move { rx.await.expect("loader thread panicked") }
}

/// The 360 view widget: draws the shader primitive and turns drags and the
/// scroll wheel into look and zoom messages.
struct Sphere {
    rig: Arc<Rig>,
    shown: Arc<Shown>,
    user: M3,
    zoom: f32,
    projection: u32,
    rig_from_view: Vec<M3>,
    fov_deg: f64,
}

#[derive(Default)]
struct Drag {
    button: Option<mouse::Button>,
    last: Option<Point>,
    /// The size last reported to the app.
    size: Option<Size>,
}

impl Sphere {
    /// Drags look around, the wheel zooms.
    fn input(&self, drag: &mut Drag, event: &iced::Event, bounds: Rectangle, cursor: mouse::Cursor) -> Option<shader::Action<Message>> {
        match event {
            iced::Event::Mouse(mouse::Event::ButtonPressed(b @ (mouse::Button::Left | mouse::Button::Right))) => {
                drag.last = Some(cursor.position_over(bounds)?);
                drag.button = Some(*b);
                Some(shader::Action::capture())
            }
            iced::Event::Mouse(mouse::Event::ButtonReleased(_)) if drag.button.is_some() => {
                drag.button = None;
                Some(shader::Action::capture())
            }
            iced::Event::Mouse(mouse::Event::CursorMoved { position }) => {
                let (button, last) = (drag.button?, drag.last?);
                drag.last = Some(*position);
                // Grab-style: the picture follows the pointer.
                let k = self.fov_deg.to_radians() / bounds.height as f64;
                let (dx, dy) = ((position.x - last.x) as f64, (position.y - last.y) as f64);
                let look = match button {
                    mouse::Button::Right => Message::Look { yaw: 0.0, pitch: 0.0, roll: dx * k },
                    _ => Message::Look { yaw: -dx * k, pitch: dy * k, roll: 0.0 },
                };
                Some(shader::Action::publish(look).and_capture())
            }
            iced::Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                cursor.position_over(bounds)?;
                let lines = match delta {
                    mouse::ScrollDelta::Lines { y, .. } => *y as f64,
                    mouse::ScrollDelta::Pixels { y, .. } => *y as f64 / 40.0,
                };
                Some(shader::Action::publish(Message::Zoom(-lines)).and_capture())
            }
            _ => None,
        }
    }
}

impl shader::Program<Message> for Sphere {
    type State = Drag;
    type Primitive = gpu::Primitive;

    fn update(&self, drag: &mut Drag, event: &iced::Event, bounds: Rectangle, cursor: mouse::Cursor) -> Option<shader::Action<Message>> {
        let action = self.input(drag, event, bounds, cursor);
        if action.is_none() && drag.size != Some(bounds.size()) {
            drag.size = Some(bounds.size());
            return Some(shader::Action::publish(Message::ViewSize(bounds.size())));
        }
        action
    }

    fn draw(&self, _: &Drag, _: mouse::Cursor, _: Rectangle) -> gpu::Primitive {
        gpu::Primitive {
            rig: self.rig.clone(),
            shown: self.shown.clone(),
            user: self.user,
            zoom: self.zoom,
            projection: self.projection,
            rig_from_view: self.rig_from_view.clone(),
        }
    }

    fn mouse_interaction(&self, drag: &Drag, bounds: Rectangle, cursor: mouse::Cursor) -> mouse::Interaction {
        if drag.button.is_some() {
            mouse::Interaction::Grabbing
        } else if cursor.is_over(bounds) {
            mouse::Interaction::Grab
        } else {
            mouse::Interaction::default()
        }
    }
}
