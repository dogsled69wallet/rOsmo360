//! Real-time 360 player: an iced window with the stitched view, an open-file
//! dialog and playback controls. Free look on all three axes, stabilisation
//! and rolling-shutter correction run in the view's shader.

mod audio;
mod edit;
mod export;
mod gpu;
mod playback;
pub mod render;
mod settings;
mod source;
mod thumbs;
mod music;
mod timeline;

use crate::ffmpeg::{self, Hwaccel};
use crate::stab::{ImuAxes, Mode, Stabilizer};
use crate::project::{Cut, Project};
use crate::stitch::M3;
use crate::view::{Camera, Ease, Follow, Keyframes, Projection, View};
use anyhow::{Context, Result};
use gpu::{Rig, Shown};
use iced::keyboard::{self, Key, key::Named};
use iced::mouse;
use iced::widget::{button, canvas, center, checkbox, column, progress_bar, container, image, pick_list, responsive, row, shader, slider, space, stack, svg, text, toggler, tooltip};
use iced::window;
use iced::{Alignment, Element, Length, Point, Rectangle, Size, Subscription, Task, Theme};
use playback::Playback;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone)]
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

/// What the command line asked for; what it leaves out comes from the
/// remembered settings.
pub struct Launch {
    pub osv_size: Option<u32>,
    pub hw: Option<Hwaccel>,
    pub stabilize: Option<Mode>,
    pub imu_axes: ImuAxes,
    pub world_z_down: bool,
}

/// Settings for loading and playing clips; the GUI updates them so the
/// next clip opens the way the last one was left.
#[derive(Clone)]
pub struct Options {
    pub osv_size: Option<u32>,
    /// Forced decoder, or the one found by probing the first clip.
    pub hw: Option<Hwaccel>,
    pub stabilize: Mode,
    pub imu_axes: ImuAxes,
    pub world_z_down: bool,
}

/// Caches `input` the way opening it in the player would: its telemetry and
/// timeline thumbnails (`rosmo360 cache add`).
pub fn cache_clip(input: &Path) -> Result<()> {
    let opts = Options { osv_size: None, hw: Some(Hwaccel::Software), stabilize: Mode::Horizon, imu_axes: ImuAxes::X180, world_z_down: false };
    thumbs::make(&Media::load(input.to_owned(), &opts, &|_| {})?, |_, _| true)
}

pub use settings::config_dir;

impl Media {
    /// `progress` hears how far reading the clip's telemetry has got (0..1),
    /// which is most of the time a clip takes to load.
    fn load(input: PathBuf, opts: &Options, progress: &dyn Fn(f64)) -> Result<Media> {
        let stitch = crate::StitchOpts { stabilize: Some(opts.stabilize), imu_axes: opts.imu_axes, world_z_down: opts.world_z_down, ..Default::default() };
        let p = crate::Pipeline::load(&input, &stitch, opts.stabilize, progress)?;
        let proxy = ffmpeg::proxy(&input);
        let hw = match opts.hw {
            Some(hw) => hw,
            None => {
                let hw = Hwaccel::pick(&input, p.tracks[0])?;
                eprintln!("decoding with {hw:?}");
                hw
            }
        };
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

pub fn run(inputs: Vec<PathBuf>, launch: Launch) -> Result<()> {
    let settings = settings::Settings::load();
    let opts = Options {
        osv_size: launch.osv_size.or(settings.osv_size),
        hw: launch.hw,
        stabilize: launch.stabilize.unwrap_or(settings.stabilize),
        imu_axes: launch.imu_axes,
        world_z_down: launch.world_z_down,
    };
    iced::application(
        move || {
            let mut app = App::new(opts.clone(), settings.clone());
            let is_project = |p: &PathBuf| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("json"));
            let task = match inputs.iter().find(|p| is_project(p)) {
                Some(project) => app.update(Message::ProjectPicked(Some(project.clone()))),
                None => {
                    app.queue.extend(inputs.iter().map(|p| (p.clone(), true)));
                    app.open_next()
                }
            };
            (app, task)
        },
        App::update,
        App::view,
    )
    .title(App::title)
    .subscription(App::subscription)
    .theme(|app: &App| app.theme.clone())
    .window(window::Settings {
        size: iced::Size::new(1280.0, 820.0),
        // Closing asks first when the edit has unsaved changes.
        exit_on_close_request: false,
        // Wayland matches the window to its menu entry and icon by this id: in a
        // Flatpak that's the Flatpak's own (io.github.ravelund.rOsmo360).
        #[cfg(target_os = "linux")]
        platform_specific: window::settings::PlatformSpecific {
            application_id: std::env::var("FLATPAK_ID").unwrap_or_else(|_| NAME.to_owned()),
            ..Default::default()
        },
        ..Default::default()
    })
    .run()
    .context("running the player window")
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

/// The shape of the view, and so of a saved view: free fills the window.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Aspect {
    Free,
    Ratio(u16, u16),
}

impl Aspect {
    const ALL: [Aspect; 6] = [Aspect::Free, Aspect::Ratio(16, 9), Aspect::Ratio(9, 16), Aspect::Ratio(4, 3), Aspect::Ratio(1, 1), Aspect::Ratio(2, 1)];

    /// Width over height, if locked.
    fn ratio(self) -> Option<f32> {
        match self {
            Aspect::Free => None,
            Aspect::Ratio(w, h) => Some(w as f32 / h as f32),
        }
    }
}

impl fmt::Display for Aspect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Aspect::Free => f.write_str("Free"),
            Aspect::Ratio(w, h) => write!(f, "{w}:{h}"),
        }
    }
}

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

/// What waits on the edit being saved or let go.
#[derive(Debug, Clone, Copy)]
enum Then {
    Close,
    OpenProject,
    NewProject,
}

/// An answer to "save changes first?".
#[derive(Debug, Clone, Copy)]
enum Choice {
    Save,
    Discard,
    Cancel,
}

/// The playing cut's clip: its playback plus the frame the view draws.
struct Clip {
    playback: Playback,
    shown: Option<Arc<Shown>>,
}

#[derive(Debug, Clone)]
enum Message {
    Open,
    /// Clips to add to the edit, in order.
    Picked(Vec<PathBuf>),
    OpenProject,
    ProjectPicked(Option<PathBuf>),
    /// The window's close button was pressed.
    CloseRequested,
    /// The answer to "save changes first?" before doing `.0`.
    Confirmed(Then, Choice),
    /// Save the project; to a new file if true or never saved.
    SaveProject(bool),
    SaveProjectTo(Option<PathBuf>),
    Resolution(crate::Resolution),
    /// Export each cut to its own file.
    ExportClips(bool),
    /// Export for DaVinci Resolve (true) or as MP4.
    ExportResolve(bool),
    /// Export the edit: ask where (a file, or a folder for the clips), then
    /// render in the background.
    Export,
    ExportTo(Option<PathBuf>),
    /// Fraction of the export done.
    ExportProgress(f32),
    /// The files written.
    ExportDone(Result<Vec<PathBuf>, String>),
    ExportCancel,
    /// Starts (or stops) drawing a box over a subject to track its colour.
    PickColour,
    /// Pick a song for the music track.
    AddMusic,
    MusicPicked(Option<PathBuf>),
    /// A song's waveform, read off the UI thread.
    Waveform(PathBuf, Result<Arc<music::Waveform>, String>),
    SelectMusic(usize),
    /// Music clip `i` dragged to start at this edit time.
    MusicMove(usize, f64),
    /// Music clip `i`'s in (`true`) or out edge dragged to this song time.
    MusicTrim(usize, bool, f64),
    /// The mix: the clips' sound and the music track, in dB.
    SoundLevel(f64),
    MusicLevel(f64),
    /// A level slider let go.
    LevelDone,
    /// Start an empty edit (asking first if there are unsaved changes).
    NewProject,
    /// A redraw while clips load, for the loading bar.
    LoadingTick,
    /// The box being drawn, in fractions of the view.
    PickArea(Rectangle),
    /// The box drawn, and the view's width over its height.
    AreaPicked(Rectangle, f64),
    TrackProgress(f32),
    TrackCancel,
    Tracked(Result<TrackDone, String>),
    /// Never cloned in flight, so the clip can be taken back out of the `Arc`.
    Loaded(Arc<Result<Media, String>>),
    Tick,
    Key(Key, keyboard::Modifiers),
    Toggle,
    Scrub(f64),
    ScrubDone,
    /// Add a keyframe with the current view at the playhead, or update the
    /// one there.
    KeyAdd,
    /// Delete the keyframe at the playhead.
    KeyDelete,
    /// Zoom the timeline by a factor, keeping edit time `.1` (the playhead
    /// if `None`) where it is on screen.
    TimelineZoom(f64, Option<f64>),
    /// Zoom the timeline to slider position `.0` (0 whole edit, 1 closest).
    TimelineZoomTo(f64),
    /// Scroll the timeline so it starts at edit time `.0`.
    TimelinePan(f64),
    /// A thumbnail of clip `.0` at clip time `.1`.
    Thumb(PathBuf, f64, image::Handle),
    /// How the keyframe at the playhead moves on to the next.
    KeyEase(Ease),
    KeyFollow(Aiming),
    /// Jump to keyframe `.1` of cut `.0`.
    KeyJump(usize, usize),
    /// Drag keyframe `.1` of cut `.0` to clip time `.2`.
    KeyMove(usize, usize, f64),
    Select(usize),
    /// Drag cut `.0`'s in (`.1` true) or out point to clip time `.2`.
    Trim(usize, bool, f64),
    /// Move cut `.0` to position `.1`.
    MoveCut(usize, usize),
    /// Split the cut at the playhead.
    Split,
    /// Delete the selected cut.
    DeleteCut,
    Undo,
    Redo,
    Look { yaw: f64, pitch: f64, roll: f64 },
    /// Scroll notches; positive zooms out.
    Zoom(f64),
    Fov(f64),
    Stab(Mode),
    Projection(Projection),
    Source(Source),
    RollingShutter(bool),
    FullOnPause(bool),
    Mute,
    Volume(f32),
    Aspect(Aspect),
    /// The OS theme changed; read it again.
    ThemeChanged,
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
    /// Tracks that keyframes follow, read once each (`None`: unreadable).
    followed: std::cell::RefCell<std::collections::HashMap<PathBuf, Option<Arc<crate::track::Track>>>>,
    theme: Theme,
    logo: image::Handle,
    /// Every clip opened so far, ready for the cuts that use it.
    library: Vec<Media>,
    /// Each clip's timeline thumbnails by clip time, as they arrive.
    thumbs: std::collections::HashMap<PathBuf, Vec<(f64, image::Handle)>>,
    /// Songs' waveforms, and the music clip selected on its lane.
    waveforms: std::collections::HashMap<PathBuf, Arc<music::Waveform>>,
    selected_music: Option<usize>,
    music: music::Player,
    edit: edit::Edit,
    /// The cut being played, and the one selected on the timeline.
    current: usize,
    selected: Option<usize>,
    /// Clips waiting to be loaded, in order, and whether each is added as a
    /// cut (opened) or only loaded for a project's cuts.
    queue: std::collections::VecDeque<(PathBuf, bool)>,
    /// Whether the clip loading now is added as a cut.
    loading_adds: bool,
    /// The next cut's playback, primed in the last `PREROLL` seconds of the
    /// one playing, with the cut it was made for: its clip and in point, so
    /// one made stale by an edit is never used.
    preroll: Option<(usize, PathBuf, f64, Playback)>,
    /// Clips loaded since the queue was last empty, and how far the one
    /// loading now has got (an f64's bits, set from its loading thread):
    /// together with the queue, a project's loading progress.
    loaded: usize,
    loading_part: Arc<std::sync::atomic::AtomicU64>,
    /// The project file, once opened or saved, and the edit version saved.
    project: Option<PathBuf>,
    saved_version: u64,
    /// What to do once the project has been saved.
    after_save: Option<Then>,
    /// Exports are this many pixels on the short side.
    resolution: crate::Resolution,
    /// Export each cut to its own file, and in which format.
    export_clips: bool,
    export_format: ffmpeg::Format,
    /// A running export: its cancel flag and how far it has got.
    exporting: Option<(Arc<std::sync::atomic::AtomicBool>, f32)>,
    /// A trim or keyframe drag has saved the edit for undo already.
    dragging: bool,
    /// Timeline zoom (1 shows the whole edit) and the edit time at its left
    /// edge; `timeline_window` keeps them in range.
    timeline_zoom: f64,
    timeline_start: f64,
    clip: Option<Clip>,
    /// The picture as it was drawn when the playhead crossed into another
    /// cut, kept until that cut's first frame arrives. Drawing the old frame
    /// with the new cut's stabilisation and keyframes showed it at a wrong
    /// angle for a moment, like a ghost of the previous clip.
    held: Option<Sphere>,
    loading: Option<PathBuf>,
    error: Option<String>,
    /// The free-look view: what is shown unless following keyframes.
    look: View,
    /// Show the keyframes' view rather than `look`. Playing, seeking and
    /// keyframe edits turn it on; looking around by hand turns it off.
    follow: bool,
    rolling_shutter: bool,
    /// Show a full-resolution frame when paused.
    full_on_pause: bool,
    fullscreen: bool,
    volume: f32,
    muted: bool,
    view_size: Size,
    aspect: Aspect,
    /// What the last save did, or that one is running.
    save_status: Option<String>,
    /// Picking a colour to track: `Some(None)` until a box is drawn, then
    /// the box in fractions of the view.
    picking: Option<Option<Rectangle>>,
    /// A running colour track: its cancel flag and progress, 0..1.
    tracking: Option<(Arc<std::sync::atomic::AtomicBool>, f32)>,
    /// The settings as last written, to save only when they change.
    saved_settings: settings::Settings,
}

impl App {
    fn new(opts: Options, saved: settings::Settings) -> Self {
        App {
            opts,
            followed: Default::default(),
            theme: system_theme(),
            logo: image::Handle::from_bytes(LOGO),
            library: Vec::new(),
            thumbs: Default::default(),
            waveforms: Default::default(),
            selected_music: None,
            music: Default::default(),
            edit: edit::Edit::default(),
            current: 0,
            selected: None,
            dragging: false,
            timeline_zoom: 1.0,
            timeline_start: 0.0,
            queue: Default::default(),
            loading_adds: true,
            loaded: 0,
            loading_part: Arc::default(),
            preroll: None,
            project: None,
            saved_version: 0,
            after_save: None,
            resolution: saved.resolution,
            export_clips: saved.export_clips,
            export_format: saved.export_format,
            exporting: None,
            clip: None,
            held: None,
            loading: None,
            error: None,
            look: View::default(),
            follow: true,
            rolling_shutter: saved.rolling_shutter,
            full_on_pause: saved.full_on_pause,
            fullscreen: false,
            volume: saved.volume,
            muted: saved.muted,
            view_size: Size::new(16.0, 9.0),
            aspect: saved.aspect.map_or(Aspect::Free, |[w, h]| Aspect::Ratio(w, h)),
            save_status: None,
            picking: None,
            tracking: None,
            saved_settings: saved,
        }
    }

    /// The settings to remember, as they are now.
    fn settings(&self) -> settings::Settings {
        settings::Settings {
            stabilize: self.opts.stabilize,
            osv_size: self.opts.osv_size,
            rolling_shutter: self.rolling_shutter,
            full_on_pause: self.full_on_pause,
            volume: self.volume,
            muted: self.muted,
            resolution: self.resolution,
            aspect: match self.aspect {
                Aspect::Free => None,
                Aspect::Ratio(w, h) => Some([w, h]),
            },
            export_clips: self.export_clips,
            export_format: self.export_format,
        }
    }

    /// Handles `message`, then saves the settings if it changed any.
    fn update(&mut self, message: Message) -> Task<Message> {
        let task = self.handle(message);
        self.follow_music();
        let now = self.settings();
        if now != self.saved_settings {
            now.save();
            self.saved_settings = now;
        }
        task
    }

    /// Keeps the music in step with the edit: playing where playback is, at
    /// its level, or stopped while playback is.
    fn follow_music(&mut self) {
        let listen = self.volume();
        let (playing, t) = match &self.clip {
            // Only while the clock runs: waiting for its first frame it stands
            // still, and music started then took it as drift and restarted
            // over and over until the picture came.
            Some(c) if self.current < self.edit.cuts.len() => (c.playback.running(), self.edit.edit_time(self.current, c.playback.now())),
            _ => (false, 0.0),
        };
        self.music.follow(&self.edit.music, t, self.edit.duration(), listen, playing);
    }

    /// The clips' sound at the speakers: the listening volume and its level
    /// in the mix.
    fn clip_volume(&self) -> f32 {
        self.volume() * music::gain(self.edit.sound.volume)
    }

    /// Reads `path`'s waveform for the music lane, unless it has it.
    fn load_waveform(&self, path: &Path) -> Task<Message> {
        if self.waveforms.contains_key(path) {
            return Task::none();
        }
        let path = path.to_owned();
        Task::perform(
            blocking({
                let path = path.clone();
                move || music::Waveform::read(&path).map(Arc::new).map_err(|e| format!("{e:#}"))
            }),
            move |r| Message::Waveform(path.clone(), r),
        )
    }

    /// What the speakers get: nothing while muted, so unmuting restores
    /// the level that was set.
    fn volume(&self) -> f32 {
        if self.muted { 0.0 } else { self.volume }
    }

    /// Adds the next queued clip: straight away if it is in the library
    /// already, else once it has loaded.
    fn open_next(&mut self) -> Task<Message> {
        // A project's first cut plays once its clip is in, while the rest load.
        if self.clip.is_none() && self.edit.cuts.first().is_some_and(|c| self.library.iter().any(|m| m.input == c.input)) {
            let start = self.edit.cuts[0].start;
            self.follow = true;
            self.goto(0, start, false);
        }
        while let Some((path, add)) = self.queue.pop_front() {
            match self.library.iter().find(|m| m.input == path).map(|m| m.duration) {
                Some(duration) if add => self.add_cut(path, duration),
                Some(_) => {}
                None => {
                    self.loading_adds = add;
                    return self.open(path);
                }
            }
        }
        self.loaded = 0;
        Task::none()
    }

    /// The edit as a project file holds it.
    fn to_project(&self) -> Project {
        let aspect = match self.aspect {
            Aspect::Free => None,
            Aspect::Ratio(w, h) => Some(crate::view::Aspect(w.into(), h.into())),
        };
        Project { aspect, cuts: self.edit.cuts.clone(), sound: self.edit.sound, music: self.edit.music.clone() }
    }

    /// Renders the edit to `path` on its own thread with the export the
    /// command line uses, at the chosen resolution, in the aspect lock's
    /// shape (16:9 when free), stabilised as the player is.
    fn start_export(&mut self, path: PathBuf) -> Task<Message> {
        use iced::futures::{SinkExt, StreamExt};
        let project = self.to_project();
        let [w, h] = project.aspect.unwrap_or(crate::view::Aspect(16, 9)).size(self.resolution.short_side());
        let target = if self.export_clips {
            crate::Target::Clips { dir: path, name: self.export_name(), handles: 0.0 }
        } else {
            crate::Target::One(path.with_extension(self.export_format.extension()))
        };
        let out = crate::ExportOpts { size: (w as usize, h as usize), draft: false, cpu: false, format: self.export_format, target };
        let opts = crate::StitchOpts {
            stabilize: Some(self.opts.stabilize),
            imu_axes: self.opts.imu_axes,
            world_z_down: self.opts.world_z_down,
            no_rs: !self.rolling_shutter,
            ..Default::default()
        };
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.exporting = Some((cancel.clone(), 0.0));
        self.save_status = None;
        let work = iced::stream::channel(8, async move |mut output: iced::futures::channel::mpsc::Sender<Message>| {
            let (tx, mut rx) = iced::futures::channel::mpsc::unbounded();
            std::thread::spawn(move || {
                let mut progress = |done: usize, total: usize| {
                    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                        anyhow::bail!("cancelled");
                    }
                    if done.is_multiple_of(5) || done == total {
                        let _ = tx.unbounded_send(Message::ExportProgress(done as f32 / total.max(1) as f32));
                    }
                    Ok(())
                };
                let result = crate::export(&project, &opts, &out, &mut progress);
                let _ = tx.unbounded_send(Message::ExportDone(result.map_err(|e| format!("{e:#}"))));
            });
            while let Some(m) = rx.next().await {
                if output.send(m).await.is_err() {
                    break;
                }
            }
        });
        Task::run(work, |m| m)
    }

    /// What exports are named after: the project, else the first clip.
    fn export_base(&self) -> &Path {
        self.project.as_deref().unwrap_or(&self.edit.cuts[0].input)
    }

    fn export_name(&self) -> String {
        self.export_base().file_stem().map_or_else(|| "export".to_owned(), |s| s.to_string_lossy().into_owned())
    }

    /// Tracks `colour` from the playhead to the end of the cut on a thread,
    /// starting from `dir` (where the picked box was, in the rig), into a
    /// CSV beside the project (or the clip, before it is saved).
    fn start_track(&mut self, colour: crate::track::Colour, hex: String, dir: [f64; 3]) -> Task<Message> {
        use iced::futures::{SinkExt, StreamExt};
        let Some((cut, t)) = self.playing_cut() else { return Task::none() };
        let (input, end, i) = (cut.input.clone(), cut.end, self.current);
        let folder = self.project.as_deref().unwrap_or(&input).parent().unwrap_or(Path::new("")).to_owned();
        let stem = input.file_stem().map_or_else(|| "clip".to_owned(), |s| s.to_string_lossy().into_owned());
        let path = folder.join(format!("{stem}-colour-{t:.1}s.csv"));
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.tracking = Some((cancel.clone(), 0.0));
        let work = iced::stream::channel(8, async move |mut output: iced::futures::channel::mpsc::Sender<Message>| {
            let (tx, mut rx) = iced::futures::channel::mpsc::unbounded();
            std::thread::spawn(move || {
                let mut progress = |done: usize, total: usize| {
                    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                        anyhow::bail!("cancelled");
                    }
                    let _ = tx.unbounded_send(Message::TrackProgress(done as f32 / total.max(1) as f32));
                    Ok(())
                };
                let result = (|| {
                    use std::io::Write;
                    let mut file = std::io::BufWriter::new(std::fs::File::create(&path).with_context(|| format!("writing {}", path.display()))?);
                    let near = Some(crate::track::Near::Rig(dir));
                    let (found, rows) = crate::track_to(&input, &colour, near, Some(t), Some(end - t), 0.1, &mut file, &mut progress)?;
                    file.flush()?;
                    Ok::<_, anyhow::Error>(TrackDone { path: path.clone(), cut: i, t, found, rows, colour: hex })
                })();
                if result.is_err() {
                    let _ = std::fs::remove_file(&path);
                }
                let _ = tx.unbounded_send(Message::Tracked(result.map_err(|e| format!("{e:#}"))));
            });
            while let Some(m) = rx.next().await {
                if output.send(m).await.is_err() {
                    break;
                }
            }
        });
        Task::run(work, |m| m)
    }

    /// Makes the keyframe where a finished track starts follow it, with the
    /// subject in the middle of the view.
    fn follow_track(&mut self, done: TrackDone) {
        self.save_status = Some(format!("Tracked {}: found in {} of {} samples", done.colour, done.found, done.rows));
        if done.cut >= self.edit.cuts.len() {
            return;
        }
        self.followed.borrow_mut().remove(&done.path);
        let follow = Follow::Track(done.path);
        let here = done.cut == self.current;
        let mut aims = if here { self.aims(done.t) } else { Vec::new() };
        let subject = if here { self.aim(&follow, done.t) } else { None };
        aims.push((follow.clone(), subject));
        let mut view = if here { self.shown_view() } else { View::home(Projection::Perspective) };
        if let Some((yaw, pitch)) = subject {
            (view.yaw, view.pitch) = (yaw, pitch);
        }
        let aim = |f: &Follow, _| aims.iter().find(|a| &a.0 == f).and_then(|a| a.1);
        let tolerance = self.tolerance();
        self.edit.checkpoint();
        let keyframes = &mut self.edit.cuts[done.cut].keyframes;
        let k = keyframes.set(done.t, view, tolerance, &aim);
        keyframes.set_follow(k, Some(follow), &aim);
        self.follow = true;
    }

    /// The median colour inside `area` (fractions of the view, `aspect`
    /// wide over tall) of the frame on screen, and the rig direction of its
    /// middle. Sampled from the lens pictures as the shader maps them.
    fn sample_area(&self, area: Rectangle, aspect: f64) -> Option<([f64; 3], [f64; 3])> {
        const N: usize = 16;
        let c = self.clip.as_ref()?;
        let frame = c.shown.as_ref()?.frame();
        let rig = &c.playback.media.rig;
        let camera = self.camera();
        let views = c.playback.rig_from_view(self.rolling_shutter);
        let rig_from_view = views[views.len() / 2];
        let dir = |fx: f64, fy: f64| crate::stitch::apply(rig_from_view, crate::stitch::apply(camera.user, camera.ray(aspect, [2.0 * fx - 1.0, 2.0 * fy - 1.0])));
        let mut channels: [Vec<f64>; 3] = Default::default();
        for gy in 0..N {
            for gx in 0..N {
                let fx = area.x as f64 + (gx as f64 + 0.5) / N as f64 * area.width as f64;
                let fy = area.y as f64 + (gy as f64 + 0.5) / N as f64 * area.height as f64;
                let d = dir(fx, fy);
                // The lens that weighs most there, as the blend would.
                let best = rig.lenses.iter().filter_map(|l| l.project(d).map(|h| (l, h))).max_by(|a, b| (a.0.blend.weight(a.1.2) * a.1.3).total_cmp(&(b.0.blend.weight(b.1.2) * b.1.3)));
                if let Some((lens, (u, v, _, _))) = best
                    && let Some(rgb) = frame.rgb_at(lens, rig.tracks[lens.input], u, v)
                {
                    for k in 0..3 {
                        channels[k].push(rgb[k]);
                    }
                }
            }
        }
        if channels[0].is_empty() {
            return None;
        }
        let median = channels.map(|mut c| {
            c.sort_by(f64::total_cmp);
            c[c.len() / 2]
        });
        Some((median, dir(area.x as f64 + area.width as f64 / 2.0, area.y as f64 + area.height as f64 / 2.0)))
    }

    /// Export button, resolution and options, or a running export's progress.
    fn export_controls(&self) -> Element<'_, Message> {
        if let Some((_, done)) = &self.tracking {
            return row![
                text(format!("Tracking {:.0}%", done * 100.0)),
                progress_bar(0.0..=1.0, *done).length(140).girth(8),
                tip(icon_button(CANCEL, Some(Message::TrackCancel)), text("Stop tracking"), tooltip::Position::Bottom),
            ]
            .spacing(8)
            .align_y(Alignment::Center)
            .into();
        }
        match &self.exporting {
            Some((_, done)) => row![
                text(format!("Exporting {:.0}%", done * 100.0)),
                progress_bar(0.0..=1.0, *done).length(140).girth(8),
                tip(icon_button(CANCEL, Some(Message::ExportCancel)), text("Stop the export"), tooltip::Position::Bottom),
            ]
            .spacing(8)
            .align_y(Alignment::Center)
            .into(),
            None => row![
                tip(
                    labelled("Aspect", pick_list(&Aspect::ALL[..], Some(self.aspect), Message::Aspect)),
                    text("The shape of the view, of saved views and of the export (16:9 when free)"),
                    tooltip::Position::Bottom,
                ),
                tip(
                    icon_button(EXPORT, (!self.edit.cuts.is_empty()).then_some(Message::Export)),
                    text("Export… the edit with sound, at this resolution, in the aspect's shape"),
                    tooltip::Position::Bottom,
                ),
                pick_list(&crate::Resolution::ALL[..], Some(self.resolution), Message::Resolution),
                tip(
                    checkbox(self.export_clips).label("Clips").on_toggle(Message::ExportClips),
                    text("Export each cut to its own file, for another editor: \"<project> - 001\" and on, into a folder you pick"),
                    tooltip::Position::Bottom,
                ),
                tip(
                    checkbox(self.export_format == ffmpeg::Format::Resolve).label("Resolve").on_toggle(Message::ExportResolve),
                    text("For DaVinci Resolve, whose free Linux version can't read MP4's H.264 and AAC: DNxHR HQ with PCM sound in .mov. Large: about 27 MB a second at 1080p"),
                    tooltip::Position::Bottom,
                ),
            ]
            .spacing(6)
            .align_y(Alignment::Center)
            .into(),
        }
    }

    /// Asks whether to save the edit before doing `then`.
    fn confirm(&self, then: Then) -> Task<Message> {
        let name = self.project.as_ref().and_then(|p| p.file_name()).map_or_else(|| "this edit".to_owned(), |n| n.to_string_lossy().into_owned());
        let ask = rfd::AsyncMessageDialog::new()
            .set_level(rfd::MessageLevel::Warning)
            .set_title("Unsaved changes")
            .set_description(format!("Save the changes to {name} first?"))
            .set_buttons(rfd::MessageButtons::YesNoCancelCustom("Save".into(), "Don't save".into(), "Cancel".into()));
        Task::perform(ask.show(), move |answer| {
            // Backends report custom buttons by label, some as yes/no.
            let choice = match answer {
                rfd::MessageDialogResult::Yes => Choice::Save,
                rfd::MessageDialogResult::No => Choice::Discard,
                rfd::MessageDialogResult::Custom(label) if label == "Save" => Choice::Save,
                rfd::MessageDialogResult::Custom(label) if label == "Don't save" => Choice::Discard,
                _ => Choice::Cancel,
            };
            Message::Confirmed(then, choice)
        })
    }

    /// Does what waited on the edit being saved or let go.
    fn proceed(&mut self, then: Then) -> Task<Message> {
        match then {
            Then::Close => iced::exit(),
            Then::OpenProject => self.update(Message::OpenProject),
            Then::NewProject => self.update(Message::NewProject),
        }
    }

    fn unsaved(&self) -> bool {
        self.edit.version != self.saved_version && self.has_edit()
    }

    /// Whether there is anything to save: cuts, or music to fill.
    fn has_edit(&self) -> bool {
        !self.edit.cuts.is_empty() || !self.edit.music.clips.is_empty()
    }

    /// Parses the clip and probes the decoder off the UI thread.
    fn open(&mut self, path: PathBuf) -> Task<Message> {
        self.loading = Some(path.clone());
        self.error = None;
        let opts = self.opts.clone();
        let part = self.loading_part.clone();
        part.store(0f64.to_bits(), std::sync::atomic::Ordering::Relaxed);
        let progress = move |f: f64| part.store(f.clamp(0.0, 1.0).to_bits(), std::sync::atomic::Ordering::Relaxed);
        Task::perform(blocking(move || Media::load(path, &opts, &progress).map_err(|e| format!("{e:#}"))), |r| Message::Loaded(Arc::new(r)))
    }

    fn title(&self) -> String {
        let name = |p: &Path| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        // A dot marks unsaved changes, as in many editors.
        let project = match &self.project {
            Some(p) => format!(" · {}{}", name(p), if self.unsaved() { " •" } else { "" }),
            None if self.unsaved() => " · unsaved edit •".to_owned(),
            None => String::new(),
        };
        match (&self.loading, &self.clip) {
            (Some(_), _) => format!("{NAME}{project} · loading {:.0}%", self.load_progress() * 100.0),
            (None, Some(c)) => format!("{NAME}{project} · {}", name(&c.playback.media.input)),
            (None, None) => format!("{NAME}{project}"),
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        let keys = keyboard::listen().filter_map(|e| match e {
            keyboard::Event::KeyPressed { key, modifiers, .. } => Some(Message::Key(key, modifiers)),
            _ => None,
        });
        let theme = Subscription::run(theme_changes);
        let busy = self.clip.as_ref().is_some_and(|c| c.playback.busy());
        let close = window::close_requests().map(|_| Message::CloseRequested);
        if busy {
            Subscription::batch([keys, theme, close, window::frames().map(|_| Message::Tick)])
        } else if self.loading.is_some() {
            // Only to redraw the loading bar as it fills.
            Subscription::batch([keys, theme, close, window::frames().map(|_| Message::LoadingTick)])
        } else {
            Subscription::batch([keys, theme, close])
        }
    }

    fn handle(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Open => {
                let dir = self.clip.as_ref().and_then(|c| c.playback.media.input.parent().map(Path::to_owned));
                return Task::perform(
                    async move {
                        let mut dialog = rfd::AsyncFileDialog::new().set_title("Open Osmo 360 clips").add_filter("Osmo 360 video", &["OSV", "osv"]);
                        if let Some(dir) = dir {
                            dialog = dialog.set_directory(dir);
                        }
                        dialog.pick_files().await.unwrap_or_default().iter().map(|f| f.path().to_owned()).collect()
                    },
                    Message::Picked,
                );
            }
            Message::Picked(paths) => {
                self.queue.extend(paths.into_iter().map(|p| (p, true)));
                if self.loading.is_none() {
                    return self.open_next();
                }
            }
            Message::Loaded(result) => {
                self.loading = None;
                self.loaded += 1;
                match Arc::into_inner(result).expect("loaded clip is not shared") {
                    Ok(media) => {
                        self.opts.hw = Some(media.hw);
                        let (input, duration) = (media.input.clone(), media.duration);
                        let thumbs = (!self.thumbs.contains_key(&input)).then(|| Task::run(thumbs::stream(media.clone()), |m| m));
                        self.thumbs.entry(input.clone()).or_default();
                        self.library.push(media);
                        if self.loading_adds {
                            self.add_cut(input, duration);
                        }
                        return Task::batch([self.open_next(), thumbs.unwrap_or_else(Task::none)]);
                    }
                    Err(e) => self.error = Some(e),
                }
                return self.open_next();
            }
            Message::OpenProject if self.unsaved() => return self.confirm(Then::OpenProject),
            Message::NewProject if self.unsaved() => return self.confirm(Then::NewProject),
            Message::NewProject => {
                // Back to the start screen with an empty edit. Loaded clips
                // stay in the library, so adding them again is instant; one
                // still loading finishes there without becoming a cut.
                self.edit.replace(Vec::new(), Default::default(), Default::default());
                self.selected_music = None;
                self.saved_version = self.edit.version;
                self.project = None;
                self.clip = None;
                self.held = None;
                self.current = 0;
                self.selected = None;
                self.queue.clear();
                self.loading_adds = false;
                self.error = None;
                self.save_status = None;
                self.timeline_zoom = 1.0;
                self.timeline_start = 0.0;
            }
            Message::CloseRequested if self.unsaved() => return self.confirm(Then::Close),
            Message::CloseRequested => return iced::exit(),
            Message::Confirmed(then, choice) => match choice {
                Choice::Save => {
                    self.after_save = Some(then);
                    return self.update(Message::SaveProject(false));
                }
                Choice::Discard => {
                    // Let the changes go: the next save starts fresh.
                    self.saved_version = self.edit.version;
                    return self.proceed(then);
                }
                Choice::Cancel => {}
            },
            Message::OpenProject => {
                let dir = self.project.as_ref().and_then(|p| p.parent().map(Path::to_owned));
                return Task::perform(
                    async move {
                        let mut dialog = rfd::AsyncFileDialog::new().set_title("Open project").add_filter("rOsmo360 project", &["json"]);
                        if let Some(dir) = dir {
                            dialog = dialog.set_directory(dir);
                        }
                        dialog.pick_file().await.map(|f| f.path().to_owned())
                    },
                    Message::ProjectPicked,
                );
            }
            Message::ProjectPicked(None) => {}
            Message::SaveProjectTo(None) => self.after_save = None,
            Message::ProjectPicked(Some(path)) => {
                let project = match Project::load(&path) {
                    Ok(p) => p,
                    Err(e) => {
                        self.error = Some(format!("{e:#}"));
                        return Task::none();
                    }
                };
                // Cuts of clips that are gone can't play; say so and leave them out.
                let (mut cuts, missing): (Vec<_>, Vec<_>) = project.cuts.into_iter().partition(|c| c.input.exists());
                // Each cut gets a mode of its own, so changing one leaves the rest.
                for c in &mut cuts {
                    c.stabilize.get_or_insert(self.opts.stabilize);
                }
                self.error = (!missing.is_empty()).then(|| {
                    let names: Vec<_> = missing.iter().map(|c| c.input.display().to_string()).collect();
                    format!("Left out cuts of missing clips: {}", names.join(", "))
                });
                self.aspect = project.aspect.map_or(Aspect::Free, |a| Aspect::Ratio(a.0 as u16, a.1 as u16));
                self.clip = None;
                self.current = 0;
                self.selected = None;
                // Music that's gone is left out the same way.
                let mut music = project.music;
                let (here, gone): (Vec<_>, Vec<_>) = music.clips.into_iter().partition(|c| c.input.exists());
                music.clips = here;
                if !gone.is_empty() {
                    let names: Vec<_> = gone.iter().map(|c| c.input.display().to_string()).collect();
                    let said = self.error.take().map_or(String::new(), |e| e + ". ");
                    self.error = Some(format!("{said}Left out missing music: {}", names.join(", ")));
                }
                let waves: Vec<_> = music.clips.iter().map(|c| self.load_waveform(&c.input)).collect();
                self.queue.extend(cuts.iter().map(|c| (c.input.clone(), false)));
                let complete = missing.is_empty() && gone.is_empty();
                self.edit.replace(cuts, music, project.sound);
                self.saved_version = if complete { self.edit.version } else { 0 };
                self.project = Some(path);
                self.selected_music = None;
                let next = if self.loading.is_none() { self.open_next() } else { Task::none() };
                return Task::batch(waves.into_iter().chain([next]));
            }
            Message::SaveProject(as_new) => {
                if !self.has_edit() {
                    return Task::none();
                }
                if let (Some(path), false) = (&self.project, as_new) {
                    return self.update(Message::SaveProjectTo(Some(path.clone())));
                }
                // Named after the first clip, or the song a project starts from.
                let first = self.edit.cuts.first().map(|c| &c.input).or(self.edit.music.clips.first().map(|c| &c.input)).unwrap();
                let dir = self.project.as_ref().unwrap_or(first).parent().map(Path::to_owned);
                let name = self.project.as_ref().and_then(|p| p.file_name()).map_or_else(
                    || format!("{}_edit.json", first.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()),
                    |n| n.to_string_lossy().into_owned(),
                );
                return Task::perform(
                    async move {
                        let mut dialog = rfd::AsyncFileDialog::new().set_title("Save project").add_filter("rOsmo360 project", &["json"]).set_file_name(name);
                        if let Some(dir) = dir {
                            dialog = dialog.set_directory(dir);
                        }
                        dialog.save_file().await.map(|f| f.path().to_owned())
                    },
                    Message::SaveProjectTo,
                );
            }
            Message::Resolution(r) => self.resolution = r,
            Message::ExportClips(on) => self.export_clips = on,
            Message::ExportResolve(on) => self.export_format = if on { ffmpeg::Format::Resolve } else { ffmpeg::Format::Mp4 },
            Message::Export => {
                if self.edit.cuts.is_empty() || self.exporting.is_some() {
                    return Task::none();
                }
                let dir = self.export_base().parent().map(Path::to_owned);
                let (clips, ext) = (self.export_clips, self.export_format.extension());
                let name = format!("{}_{}.{ext}", self.export_name(), self.resolution);
                return Task::perform(
                    async move {
                        let mut dialog = rfd::AsyncFileDialog::new();
                        if let Some(dir) = dir {
                            dialog = dialog.set_directory(dir);
                        }
                        if clips {
                            dialog.set_title("Export the cuts as clips, into the folder").pick_folder().await.map(|f| f.path().to_owned())
                        } else {
                            let kind = if ext == "mov" { "QuickTime video" } else { "MP4 video" };
                            dialog.set_title("Export video").add_filter(kind, &[ext]).set_file_name(name).save_file().await.map(|f| f.path().to_owned())
                        }
                    },
                    Message::ExportTo,
                );
            }
            Message::ExportTo(None) => {}
            Message::ExportTo(Some(path)) => return self.start_export(path),
            Message::ExportProgress(f) => {
                if let Some((_, done)) = &mut self.exporting {
                    *done = f;
                }
            }
            Message::ExportCancel => {
                if let Some((cancel, _)) = &self.exporting {
                    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
            Message::PickColour => {
                if self.picking.take().is_none() && self.clip.is_some() {
                    self.with_playback(Playback::pause);
                    self.picking = Some(None);
                    self.save_status = Some("Draw a box over the subject to track its colour (Esc cancels)".to_owned());
                } else {
                    self.save_status = None;
                }
            }
            Message::PickArea(area) => {
                if self.picking.is_some() {
                    self.picking = Some(Some(area));
                }
            }
            Message::AreaPicked(area, aspect) => {
                self.picking = None;
                self.save_status = None;
                if area.width < 0.004 || area.height < 0.004 {
                    return Task::none();
                }
                if let Some((rgb, dir)) = self.sample_area(area, aspect) {
                    match crate::track::Colour::from_rgb(rgb, 18.0) {
                        Ok(colour) => return self.start_track(colour, crate::track::hex(rgb), dir),
                        Err(e) => self.error = Some(format!("{e:#}")),
                    }
                }
            }
            Message::TrackProgress(f) => {
                if let Some((_, done)) = &mut self.tracking {
                    *done = f;
                }
            }
            Message::TrackCancel => {
                if let Some((cancel, _)) = &self.tracking {
                    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
            Message::Tracked(result) => {
                let cancelled = self.tracking.take().is_some_and(|(c, _)| c.load(std::sync::atomic::Ordering::Relaxed));
                match result {
                    Ok(done) => self.follow_track(done),
                    Err(_) if cancelled => self.save_status = Some("Tracking cancelled".to_owned()),
                    Err(e) => self.error = Some(format!("Tracking failed: {e}")),
                }
            }
            Message::ExportDone(result) => {
                let cancelled = self.exporting.take().is_some_and(|(c, _)| c.load(std::sync::atomic::Ordering::Relaxed));
                match result {
                    Ok(paths) => {
                        let name = |p: &PathBuf| p.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                        self.save_status = Some(match paths.as_slice() {
                            [one] => format!("Exported {}", name(one)),
                            [first, ..] => format!("Exported {} clips to {}", paths.len(), first.parent().map_or_else(String::new, |d| d.display().to_string())),
                            [] => "Exported nothing".to_owned(),
                        });
                    }
                    Err(_) if cancelled => self.save_status = Some("Export cancelled".to_owned()),
                    Err(e) => self.error = Some(format!("Export failed: {e}")),
                }
            }
            Message::SaveProjectTo(Some(path)) => match self.to_project().save(&path) {
                Ok(()) => {
                    self.saved_version = self.edit.version;
                    self.save_status = Some(format!("Saved {}", path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned())));
                    self.project = Some(path);
                    if let Some(then) = self.after_save.take() {
                        return self.proceed(then);
                    }
                }
                Err(e) => {
                    self.after_save = None;
                    self.error = Some(format!("Saving the project failed: {e:#}"));
                }
            },
            Message::LoadingTick => {}
            Message::Tick => {
                self.tick();
                if self.clip.as_ref().is_some_and(|c| c.playback.playing) {
                    self.reveal_playhead();
                }
            }
            Message::Key(key, mods) => {
                let task = self.key(key, mods);
                // Keys can seek out of sight (arrows, [ ]): follow.
                self.reveal_playhead();
                return task;
            }
            Message::TimelineZoom(factor, anchor) => self.zoom_timeline(self.timeline_zoom * factor, anchor),
            Message::TimelineZoomTo(v) => self.zoom_timeline(self.timeline_most_zoom().powf(v), None),
            Message::TimelinePan(start) => self.timeline_start = start,
            Message::Toggle => {
                self.follow = true;
                let playing = self.clip.as_ref().is_some_and(|c| c.playback.playing);
                if !playing && self.position() >= self.edit.duration() - 1.5 / self.fps() {
                    // From the end of the edit, play again from the start.
                    let start = self.edit.cuts.first().map_or(0.0, |c| c.start);
                    self.goto(0, start, true);
                } else {
                    self.with_playback(Playback::toggle);
                }
            }
            Message::Scrub(t) => {
                self.follow = true;
                if let Some((i, t)) = self.edit.locate(t) {
                    self.scrub_to(i, t);
                }
            }
            Message::ScrubDone => {
                self.dragging = false;
                self.with_playback(Playback::scrub_end);
            }
            Message::Select(i) => {
                self.selected = Some(i);
                self.selected_music = None;
            }
            Message::AddMusic => {
                let dir = self.project.as_ref().and_then(|p| p.parent().map(Path::to_owned));
                return Task::perform(
                    async move {
                        let mut dialog = rfd::AsyncFileDialog::new()
                            .set_title("Add music")
                            .add_filter("Music", &["mp3", "MP3", "flac", "wav", "m4a", "aac", "ogg", "opus", "wma"]);
                        if let Some(dir) = dir {
                            dialog = dialog.set_directory(dir);
                        }
                        dialog.pick_file().await.map(|f| f.path().to_owned())
                    },
                    Message::MusicPicked,
                );
            }
            Message::MusicPicked(None) => {}
            Message::MusicPicked(Some(path)) => {
                // Its length comes with the waveform; until then a stand-in
                // that the waveform corrects.
                let duration = ffmpeg::duration(&path).unwrap_or(60.0);
                self.edit.checkpoint();
                // One song for now: it replaces the one there, from the start.
                self.edit.music.clips = vec![crate::project::MusicClip { input: path.clone(), at: 0.0, start: 0.0, end: duration, fade_in: 0.0, fade_out: 0.0 }];
                self.selected_music = Some(0);
                self.selected = None;
                return self.load_waveform(&path);
            }
            Message::Waveform(path, Ok(wave)) => {
                self.waveforms.insert(path, wave);
            }
            Message::Waveform(path, Err(e)) => self.error = Some(format!("Can't read {}: {e}", path.display())),
            Message::SelectMusic(i) => {
                self.selected_music = Some(i);
                self.selected = None;
            }
            Message::MusicMove(i, at) => {
                self.begin_drag();
                if let Some(c) = self.edit.music.clips.get_mut(i) {
                    c.at = at.max(0.0);
                }
            }
            Message::MusicTrim(i, in_edge, t) => {
                self.begin_drag();
                let song = self.edit.music.clips.get(i).and_then(|c| self.waveforms.get(&c.input)).map_or(f64::INFINITY, |w| w.duration);
                if let Some(c) = self.edit.music.clips.get_mut(i) {
                    if in_edge {
                        // The song stays put on the edit: trimming its start
                        // moves where the clip begins, not what plays when.
                        let start = t.clamp((c.start - c.at).max(0.0), c.end - edit::MIN_CUT);
                        c.at += start - c.start;
                        c.start = start;
                    } else {
                        c.end = t.clamp(c.start + edit::MIN_CUT, song);
                    }
                }
            }
            Message::SoundLevel(db) => {
                self.begin_drag();
                self.edit.sound.volume = db;
                let v = self.clip_volume();
                self.with_playback(|p| p.set_volume(v));
            }
            Message::MusicLevel(db) => {
                self.begin_drag();
                self.edit.music.volume = db;
            }
            Message::LevelDone => self.dragging = false,
            Message::Trim(i, start, t) => {
                let Some(clip_len) = self.edit.cuts.get(i).and_then(|c| self.library.iter().find(|m| m.input == c.input)).map(|m| m.duration) else {
                    return Task::none();
                };
                self.begin_drag();
                self.edit.trim(i, start, t, clip_len);
                let c = &self.edit.cuts[i];
                // Show the frame at the edge being trimmed.
                let t = if start { c.start } else { c.end - 1.0 / self.fps() };
                self.follow = true;
                self.scrub_to(i, t);
            }
            Message::MoveCut(from, to) => {
                if to != from && to != from + 1 {
                    self.edit.checkpoint();
                    let at = |i: usize| {
                        let landed = if to > from { to - 1 } else { to };
                        if i == from {
                            return landed;
                        }
                        let j = if i > from { i - 1 } else { i };
                        if j >= landed { j + 1 } else { j }
                    };
                    self.edit.move_cut(from, to);
                    self.current = at(self.current);
                    self.selected = Some(at(from));
                }
            }
            Message::Split => {
                let Some((_, t)) = self.playing_cut() else { return Task::none() };
                let i = self.current;
                self.edit.checkpoint();
                if self.edit.split(i, t) {
                    // The playhead is now at the start of the second half.
                    self.current = i + 1;
                    self.selected = Some(i + 1);
                } else {
                    self.edit.undo();
                }
            }
            Message::DeleteCut => {
                let Some(i) = self.selected.or(self.clip.as_ref().map(|_| self.current)) else { return Task::none() };
                if i >= self.edit.cuts.len() {
                    return Task::none();
                }
                self.edit.checkpoint();
                self.edit.cuts.remove(i);
                self.after_edit(self.edit.offset(i.min(self.edit.cuts.len())));
            }
            Message::Undo | Message::Redo => {
                let at = self.position();
                let changed = if matches!(message, Message::Undo) { self.edit.undo() } else { self.edit.redo() };
                if changed {
                    self.after_edit(at);
                }
            }
            Message::KeyAdd => {
                let (view, tolerance) = (self.shown_view(), self.tolerance());
                let Some((_, t)) = self.playing_cut() else { return Task::none() };
                let aims = self.aims(t);
                self.edit.checkpoint();
                if let Some(cut) = self.edit.cuts.get_mut(self.current) {
                    cut.keyframes.set(t, view, tolerance, &|f, _| aims.iter().find(|a| &a.0 == f).and_then(|a| a.1));
                    self.follow = true;
                }
            }
            Message::KeyDelete => {
                if let Some(k) = self.current_key() {
                    self.edit.checkpoint();
                    self.edit.cuts[self.current].keyframes.remove(k);
                }
            }
            Message::Thumb(input, t, handle) => {
                let thumbs = self.thumbs.entry(input).or_default();
                let i = thumbs.partition_point(|(at, _)| *at < t);
                thumbs.insert(i, (t, handle));
            }
            Message::KeyEase(ease) => {
                if let Some(k) = self.current_key()
                    && self.edit.cuts[self.current].keyframes[k].ease != ease
                {
                    self.edit.checkpoint();
                    self.edit.cuts[self.current].keyframes.set_ease(k, ease);
                    self.follow = true;
                }
            }
            Message::KeyFollow(aiming) => {
                let (Some(k), Some((_, t))) = (self.current_key(), self.playing_cut()) else { return Task::none() };
                if Aiming::of(&self.edit.cuts[self.current].keyframes[k].follow) == aiming {
                    return Task::none();
                }
                let follow = match aiming {
                    Aiming::Fixed => None,
                    Aiming::Stick => Some(Follow::Stick),
                    // A track comes from picking its colour; the keyframe
                    // here follows it once tracked.
                    Aiming::Colour => return self.update(Message::PickColour),
                };
                let aims = self.aims(t);
                self.edit.checkpoint();
                self.edit.cuts[self.current].keyframes.set_follow(k, follow, &|f, _| aims.iter().find(|a| &a.0 == f).and_then(|a| a.1));
                self.follow = true;
            }
            Message::KeyJump(i, k) => {
                self.follow = true;
                if let Some(t) = self.edit.cuts.get(i).and_then(|c| c.keyframes.get(k)).map(|k| k.t) {
                    let playing = self.clip.as_ref().is_some_and(|c| c.playback.playing);
                    self.goto(i, t, playing);
                }
            }
            Message::KeyMove(i, k, t) => {
                self.follow = true;
                self.begin_drag();
                let gap = 1.0 / self.fps();
                let Some(cut) = self.edit.cuts.get_mut(i) else { return Task::none() };
                cut.keyframes.retime(k, t, gap);
                let t = cut.keyframes[k].t;
                self.scrub_to(i, t);
            }
            Message::Look { yaw, pitch, roll } => {
                let look = self.free_look();
                look.yaw += yaw.to_degrees();
                look.pitch += pitch.to_degrees();
                look.roll += roll.to_degrees();
            }
            Message::Zoom(deg) => self.free_look().zoom_by(deg),
            Message::Fov(deg) => self.free_look().fov = deg,
            Message::Stab(mode) => {
                // The playing cut's, and the one new cuts get.
                self.opts.stabilize = mode;
                if self.edit.cuts.get(self.current).is_some_and(|c| c.stabilize != Some(mode)) {
                    self.edit.checkpoint();
                    self.edit.cuts[self.current].stabilize = Some(mode);
                }
                self.with_playback(|p| p.media.stab.set_mode(mode));
            }
            Message::Projection(p) => {
                let look = self.free_look();
                look.projection = p;
                look.reset();
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
            Message::FullOnPause(on) => {
                self.full_on_pause = on;
                self.with_playback(|p| p.set_full_on_pause(on));
            }
            Message::ViewSize(size) => self.view_size = size,
            Message::Aspect(a) => self.aspect = a,
            Message::ThemeChanged => self.theme = system_theme(),
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
                            camera: self.camera(),
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
                let v = self.clip_volume();
                self.with_playback(|p| p.set_volume(v));
            }
            Message::Volume(v) => {
                self.volume = v;
                self.muted = false;
                let v = self.clip_volume();
                self.with_playback(|p| p.set_volume(v));
            }
            Message::ResetView => self.free_look().reset(),
            Message::Fullscreen => {
                self.fullscreen = !self.fullscreen;
                let mode = if self.fullscreen { window::Mode::Fullscreen } else { window::Mode::Windowed };
                return window::latest().and_then(move |id| window::set_mode(id, mode));
            }
        }
        Task::none()
    }

    /// Half a frame: how near the playhead must be to sit on a keyframe.
    fn tolerance(&self) -> f64 {
        self.clip.as_ref().map_or(0.0, |c| 0.5 / c.playback.media.fps)
    }

    /// The playing cut, and the playhead's clip time. That is where a seek
    /// went even before its frame has decoded, and within the cut: in points
    /// fall between frames, so the frame shown can be a hair before it, and a
    /// keyframe there would be outside the cut, never drawn.
    fn playing_cut(&self) -> Option<(&Cut, f64)> {
        let cut = self.edit.cuts.get(self.current)?;
        Some((cut, self.clip.as_ref()?.playback.target().clamp(cut.start, cut.end)))
    }

    fn fps(&self) -> f64 {
        self.clip.as_ref().map_or(30.0, |c| c.playback.media.fps)
    }

    /// The playhead in edit time.
    fn position(&self) -> f64 {
        match &self.clip {
            Some(c) if self.current < self.edit.cuts.len() => {
                // In points fall between frames, so the frame shown can be a
                // hair before the cut.
                let cut = &self.edit.cuts[self.current];
                self.edit.edit_time(self.current, c.playback.position().clamp(cut.start, cut.end))
            }
            _ => 0.0,
        }
    }

    /// The keyframe at the shown frame, if there is one.
    fn current_key(&self) -> Option<usize> {
        let (cut, t) = self.playing_cut()?;
        cut.keyframes.near(t, self.tolerance())
    }

    /// The keyframes framing the playing cut at its clip time `t`, as
    /// `export` frames it: a cut without any holds the edit's last view.
    fn keyframed(&self, t: f64) -> Option<(&Keyframes, f64)> {
        crate::project::keyframed(&self.edit.cuts, self.current, t)
    }

    /// Where `follow` points in the playing clip's view at its time `t`.
    fn aim(&self, follow: &Follow, t: f64) -> Option<(f64, f64)> {
        let media = &self.clip.as_ref()?.playback.media;
        match follow {
            Follow::Stick => media.stab.stick(t * 1000.0, media.readout_ms),
            Follow::Track(path) => {
                let track = self
                    .followed
                    .borrow_mut()
                    .entry(path.clone())
                    .or_insert_with(|| crate::track::Track::load(path).inspect_err(|e| eprintln!("{e:#}")).ok().map(Arc::new))
                    .clone()?;
                track.aim(&media.stab, t, media.readout_ms)
            }
        }
    }

    /// Where everything a keyframe could follow points at clip time `t`
    /// (the stick and the edit's tracks), for keyframe edits, which can't
    /// look while the edit is borrowed.
    fn aims(&self, t: f64) -> Vec<(Follow, Option<(f64, f64)>)> {
        let mut follows = vec![Follow::Stick];
        for f in self.edit.cuts.iter().flat_map(|c| c.keyframes.iter()).filter_map(|k| k.follow.clone()) {
            if !follows.contains(&f) {
                follows.push(f);
            }
        }
        follows.into_iter().map(|f| (f.clone(), self.aim(&f, t))).collect()
    }

    /// The view on screen, as angles.
    fn shown_view(&self) -> View {
        match self.playing_cut() {
            Some((_, t)) if self.follow => self.keyframed(t).and_then(|(k, at)| k.view_at(at, &crate::project::held(&|f, t| self.aim(f, t), at == t, t))).unwrap_or(self.look),
            _ => self.look,
        }
    }

    /// The view on screen, ready to draw: mid-morph between perspective and
    /// little planet when following keyframes.
    fn camera(&self) -> Camera {
        match self.playing_cut() {
            Some((_, t)) if self.follow => self.keyframed(t).and_then(|(k, at)| k.camera_at(at, &crate::project::held(&|f, t| self.aim(f, t), at == t, t))).unwrap_or_else(|| self.look.camera()),
            _ => self.look.camera(),
        }
    }

    /// The 360 view as it should be drawn now, once a frame is decoded.
    fn sphere(&self) -> Option<Sphere> {
        let c = self.clip.as_ref()?;
        Some(Sphere {
            rig: c.playback.media.rig.clone(),
            shown: c.shown.clone()?,
            camera: self.camera(),
            rig_from_view: c.playback.rig_from_view(self.rolling_shutter),
            fov_deg: self.shown_view().fov,
            picking: self.picking.is_some(),
        })
    }

    /// Adds the whole clip as a cut at the end of the edit and selects it.
    /// The first clip starts playing, as opening a clip always did; later
    /// ones leave playback where it is.
    fn add_cut(&mut self, input: PathBuf, duration: f64) {
        self.edit.checkpoint();
        self.edit.cuts.push(Cut { input, start: 0.0, end: duration, keyframes: Default::default(), stabilize: Some(self.opts.stabilize), note: None });
        let i = self.edit.cuts.len() - 1;
        self.selected = Some(i);
        if self.clip.is_none() {
            self.follow = true;
            self.goto(i, 0.0, true);
        }
    }

    /// Shows cut `i` at clip time `t`, playing or paused. A cut of another
    /// clip gets a fresh playback of it from the library; the last frame
    /// stays on screen until the new one is decoded.
    fn goto(&mut self, i: usize, t: f64, play: bool) {
        let Some(cut) = self.edit.cuts.get(i) else { return };
        let t = t.clamp(cut.start, cut.end);
        let other_clip = self.clip.as_ref().is_none_or(|c| c.playback.media.input != cut.input);
        if (i != self.current || other_clip) && self.held.is_none() {
            self.held = self.sphere();
        }
        match &mut self.clip {
            Some(c) if !other_clip => c.playback.jump(t, play),
            _ => {
                let Some(media) = self.library.iter().find(|m| m.input == cut.input) else { return };
                let mut media = media.clone();
                media.osv_size = self.opts.osv_size;
                let mut playback = Playback::new(media, self.clip_volume(), self.full_on_pause);
                playback.jump(t, play);
                self.clip = Some(Clip { playback, shown: None });
            }
        }
        self.current = i;
        let mode = self.edit.cuts[i].stabilize.unwrap_or(self.opts.stabilize);
        self.with_playback(|p| p.media.stab.set_mode(mode));
    }

    /// Follows a drag (the playhead, a trim, a keyframe) to clip time `t`
    /// of cut `i`, switching clips if the drag crossed into another one.
    fn scrub_to(&mut self, i: usize, t: f64) {
        if i != self.current || self.clip.is_none() {
            self.goto(i, t, false);
        }
        self.with_playback(|p| p.scrub(t));
    }

    /// Saves the edit for undo once per drag.
    fn begin_drag(&mut self) {
        if !self.dragging {
            self.edit.checkpoint();
            self.dragging = true;
        }
    }

    /// After cuts were removed or restored: back to edit time `at`, or the
    /// start screen if no cut is left.
    fn after_edit(&mut self, at: f64) {
        self.selected = self.selected.filter(|&i| i < self.edit.cuts.len());
        match self.edit.locate(at.min(self.edit.duration())) {
            Some((i, t)) => {
                // The playback may be of a clip no longer at this index.
                if self.edit.cuts[i].input != self.clip.as_ref().map_or_else(PathBuf::new, |c| c.playback.media.input.clone()) {
                    self.clip = None;
                }
                self.goto(i, t, false);
            }
            None => {
                self.clip = None;
                self.current = 0;
            }
        }
    }

    /// The view to change by hand: stops following keyframes, carrying on
    /// from the view they show now.
    fn free_look(&mut self) -> &mut View {
        if self.follow {
            self.look = self.shown_view();
            self.follow = false;
        }
        &mut self.look
    }

    /// Jumps to the nearest keyframe before (`-1`) or after (`1`) the playhead.
    fn key_step(&mut self, dir: f64) -> Task<Message> {
        let tolerance = self.tolerance();
        let Some((cut, now)) = self.playing_cut() else { return Task::none() };
        let within = |k: &&crate::view::Keyframe| k.t >= cut.start - tolerance && k.t <= cut.end + tolerance;
        let keys: Vec<_> = cut.keyframes.iter().enumerate().filter(|(_, k)| within(k)).collect();
        let next = if dir < 0.0 { keys.iter().rev().find(|(_, k)| k.t < now - tolerance) } else { keys.iter().find(|(_, k)| k.t > now + tolerance) };
        let current = self.current;
        next.map(|(i, _)| *i).map_or(Task::none(), |i| self.update(Message::KeyJump(current, i)))
    }

    fn with_playback(&mut self, f: impl FnOnce(&mut Playback)) {
        if let Some(c) = &mut self.clip {
            f(&mut c.playback);
            self.error = c.playback.error.clone();
        }
    }

    fn tick(&mut self) {
        let volume = self.clip_volume();
        let Some(c) = &mut self.clip else { return };
        if let Some((frame, fix)) = c.playback.tick() {
            c.shown = Some(Arc::new(Shown::new(frame, fix)));
            self.held = None;
        }
        self.error = c.playback.error.clone();
        let Some(cut) = self.edit.cuts.get(self.current) else { return };
        let p = &c.playback;
        // A cut running to its clip's end has paused there (`ended`), and
        // still moves on.
        if !p.playing && !p.ended {
            self.preroll = None;
            return;
        }
        let next = self.current + 1;
        let next_cut = self.edit.cuts.get(next);
        let loaded = |input: &PathBuf| self.library.iter().find(|m| &m.input == input);
        // A next cut that starts where this one ends in the same clip (after
        // a split) just plays on: no seek, nothing to prime.
        let carries_on = next_cut.is_some_and(|n| n.input == cut.input && (n.start - cut.end).abs() < 1.5 / p.media.fps);
        // In the last `PREROLL` seconds, ready the next cut so the change is
        // instant.
        if p.now() >= cut.end - PREROLL
            && !carries_on
            && let Some(n) = next_cut
            && self.preroll.as_ref().is_none_or(|(i, input, start, _)| (*i, input, *start) != (next, &n.input, n.start))
            && let Some(media) = loaded(&n.input)
        {
            let mut media = media.clone();
            media.osv_size = self.opts.osv_size;
            media.stab.set_mode(n.stabilize.unwrap_or(self.opts.stabilize));
            let primed = Playback::primed(media, n.start, volume, self.full_on_pause);
            self.preroll = Some((next, n.input.clone(), n.start, primed));
        }
        // Its sound starts a device latency early, so it's heard from the
        // moment this cut's ends (that latency is otherwise a gap, and the
        // picture waits through it to stay in sync).
        if let Some((i, input, at, primed)) = &mut self.preroll
            && next_cut.is_some_and(|n| (*i, &*input, *at) == (next, &n.input, n.start))
            && primed.sound_lead().is_some_and(|lead| p.now() >= cut.end - lead)
        {
            primed.start_sound();
        }
        // At a cut's out point (or the clip's end), play on into the next cut.
        if p.now() < cut.end && !p.ended {
            return;
        }
        match next_cut {
            None if p.playing => c.playback.pause(),
            None => {}
            // Its clip is still loading: wait at the out point.
            Some(n) if loaded(&n.input).is_none() => {
                if p.playing {
                    c.playback.pause();
                    self.save_status = Some(format!("Cut {} is still loading", next + 1));
                }
            }
            Some(_) if carries_on && !p.ended => {
                self.current = next;
                let mode = self.edit.cuts[next].stabilize.unwrap_or(self.opts.stabilize);
                self.with_playback(|p| p.media.stab.set_mode(mode));
            }
            Some(n) => {
                let start = n.start;
                match self.preroll.take() {
                    Some((i, input, at, mut primed)) if (i, &input, at) == (next, &n.input, n.start) => {
                        // The last frame stays up until the primed one's first.
                        if self.held.is_none() {
                            self.held = self.sphere();
                        }
                        primed.go();
                        self.clip = Some(Clip { playback: primed, shown: None });
                        self.current = next;
                    }
                    _ => self.goto(next, start, true),
                }
            }
        }
    }

    /// The stretch of edit time the timeline shows: (start, span).
    fn timeline_window(&self) -> (f64, f64) {
        let d = self.edit.frame();
        let span = (d / self.timeline_zoom).max(TIMELINE_SPAN.min(d));
        (self.timeline_start.clamp(0.0, (d - span).max(0.0)), span)
    }

    /// The closest zoom: `TIMELINE_SPAN` seconds across.
    fn timeline_most_zoom(&self) -> f64 {
        (self.edit.frame() / TIMELINE_SPAN).max(1.0)
    }

    /// Zooms the timeline keeping `anchor` in place: by default the playhead
    /// if it is in sight, else the middle.
    fn zoom_timeline(&mut self, zoom: f64, anchor: Option<f64>) {
        let (start, span) = self.timeline_window();
        let position = self.position();
        let anchor = anchor.unwrap_or(if position >= start && position <= start + span { position } else { start + span / 2.0 });
        self.timeline_zoom = zoom.clamp(1.0, self.timeline_most_zoom());
        let (_, new_span) = self.timeline_window();
        self.timeline_start = anchor - (anchor - start) / span.max(1e-9) * new_span;
        self.timeline_start = self.timeline_window().0;
    }

    /// Scrolls the timeline to keep the playhead in sight, a page at a time.
    fn reveal_playhead(&mut self) {
        let (start, span) = self.timeline_window();
        let position = self.position();
        if position < start || position > start + span {
            self.timeline_start = (position - span * 0.05).max(0.0);
        }
        self.timeline_start = self.timeline_window().0;
    }

    fn key(&mut self, key: Key, mods: keyboard::Modifiers) -> Task<Message> {
        let step = 5.0;
        match key.as_ref() {
            Key::Named(Named::Escape) if self.picking.is_some() => return self.update(Message::PickColour),
            Key::Named(Named::Escape) if self.fullscreen => return self.update(Message::Fullscreen),
            Key::Named(Named::Space) => self.with_playback(Playback::toggle),
            Key::Named(Named::ArrowLeft) if mods.shift() => self.free_look().yaw -= 2.0 * step,
            Key::Named(Named::ArrowRight) if mods.shift() => self.free_look().yaw += 2.0 * step,
            Key::Named(arrow @ (Named::ArrowLeft | Named::ArrowRight)) => {
                self.follow = true;
                let t = self.position() + if arrow == Named::ArrowLeft { -5.0 } else { 5.0 };
                if let Some((i, t)) = self.edit.locate(t.max(0.0)) {
                    let playing = self.clip.as_ref().is_some_and(|c| c.playback.playing);
                    self.goto(i, t, playing);
                }
            }
            Key::Named(Named::ArrowUp) => self.free_look().pitch += step,
            Key::Named(Named::ArrowDown) => self.free_look().pitch -= step,
            Key::Named(Named::Home) => self.free_look().reset(),
            Key::Named(Named::Delete | Named::Backspace) if mods.shift() => return self.update(Message::DeleteCut),
            Key::Named(Named::Delete | Named::Backspace) if self.selected_music.is_some() => {
                let i = self.selected_music.take().unwrap();
                if i < self.edit.music.clips.len() {
                    self.edit.checkpoint();
                    self.edit.music.clips.remove(i);
                }
            }
            Key::Named(Named::Delete | Named::Backspace) => return self.update(Message::KeyDelete),
            Key::Character(c) => match c {
                "o" | "O" if mods.command() && mods.shift() => return self.update(Message::OpenProject),
                "n" | "N" if mods.command() => return self.update(Message::NewProject),
                "o" if mods.command() => return self.update(Message::Open),
                "s" | "S" if mods.command() => return self.update(Message::SaveProject(mods.shift())),
                "z" | "Z" if mods.command() && mods.shift() => return self.update(Message::Redo),
                "z" if mods.command() => return self.update(Message::Undo),
                "y" if mods.command() => return self.update(Message::Redo),
                "c" | "C" => return self.update(Message::Split),
                "q" | "Q" => self.free_look().roll -= step,
                "e" | "E" => self.free_look().roll += step,
                "=" | "+" if mods.command() => return self.update(Message::TimelineZoom(1.5, None)),
                "-" if mods.command() => return self.update(Message::TimelineZoom(1.0 / 1.5, None)),
                "0" if mods.command() => return self.update(Message::TimelineZoomTo(0.0)),
                "0" => self.free_look().reset(),
                "k" | "K" => return self.update(Message::KeyAdd),
                "j" | "J" => {
                    if let Some(k) = self.current_key() {
                        let ease = self.edit.cuts[self.current].keyframes[k].ease;
                        let i = Ease::ALL.iter().position(|&e| e == ease).unwrap_or(0);
                        return self.update(Message::KeyEase(Ease::ALL[(i + 1) % Ease::ALL.len()]));
                    }
                }
                "[" => return self.key_step(-1.0),
                "]" => return self.key_step(1.0),
                "," | "." => {
                    self.follow = true;
                    let frames = if c == "," { -1.0 } else { 1.0 };
                    self.with_playback(|p| p.step(frames));
                }
                "+" | "=" => self.free_look().zoom_by(-1.0),
                "-" => self.free_look().zoom_by(1.0),
                "s" | "S" => {
                    let mode = self.clip.as_ref().map_or(self.opts.stabilize, |c| c.playback.media.stab.mode());
                    let next = match mode {
                        Mode::Off => Mode::Horizon,
                        Mode::Horizon => Mode::Level,
                        Mode::Level => Mode::Lock,
                        Mode::Lock => Mode::Off,
                    };
                    return self.update(Message::Stab(next));
                }
                "r" | "R" => self.rolling_shutter = !self.rolling_shutter,
                "m" | "M" => return self.update(Message::Mute),
                "p" | "P" => {
                    let i = Projection::ALL.iter().position(|&p| p == self.shown_view().projection).unwrap_or(0);
                    return self.update(Message::Projection(Projection::ALL[(i + 1) % Projection::ALL.len()]));
                }
                "f" | "F" => return self.update(Message::Fullscreen),
                _ => {}
            },
            _ => {}
        }
        Task::none()
    }

    /// The logo shown while a clip loads or decodes its first frame.
    fn logo(&self) -> Element<'_, Message> {
        image(self.logo.clone()).width(Length::Fixed(420.0)).into()
    }

    /// Clips loaded and to load since the queue was last empty: a clip
    /// that several cuts use loads once.
    fn load_total(&self) -> usize {
        let waiting: std::collections::HashSet<&PathBuf> =
            self.queue.iter().map(|(p, _)| p).filter(|p| self.loading.as_ref() != Some(*p) && !self.library.iter().any(|m| &m.input == *p)).collect();
        self.loaded + waiting.len() + usize::from(self.loading.is_some())
    }

    /// How far loading the queued clips has got, 0..1.
    fn load_progress(&self) -> f32 {
        let part = f64::from_bits(self.loading_part.load(std::sync::atomic::Ordering::Relaxed));
        ((self.loaded as f64 + part) / self.load_total().max(1) as f64) as f32
    }

    /// A bar for loading clips, rather than their names going by.
    fn loading_bar(&self, length: f32) -> Element<'_, Message> {
        let total = self.load_total();
        let label = if total > 1 { format!("Loading clip {} of {total}", self.loaded + 1) } else { "Loading…".to_owned() };
        row![text(label), progress_bar(0.0..=1.0, self.load_progress()).length(length).girth(8)].spacing(8).align_y(Alignment::Center).into()
    }

    fn view(&self) -> Element<'_, Message> {
        let open_hint = if self.edit.cuts.is_empty() { "Open clips (Ctrl+O)" } else { "Add clips to the edit as cuts (Ctrl+O)" };
        let error = self.error.as_ref().map(|e| text(e.clone()).style(text::danger));
        let Some(clip) = &self.clip else {
            // Music first: the song is chosen and sets the length to fill.
            let song = self.edit.music.clips.first().map(|c| {
                let name = c.input.file_stem().map_or_else(String::new, |s| s.to_string_lossy().into_owned());
                format!("Music: {name} ({}). Add clips to fill it.", clock(self.edit.music.end()))
            });
            let status: Element<'_, Message> = match (&self.loading, song) {
                (Some(_), _) => self.loading_bar(280.0),
                (None, Some(song)) => text(song).into(),
                (None, None) => text("Open DJI Osmo 360 .OSV clips, or a saved project, to start. Or start from a song.").into(),
            };
            let idle = self.loading.is_none();
            let buttons = row![
                labelled_button(ADD_CLIP, "Open clips…", idle.then_some(Message::Open)),
                labelled_button(OPEN_PROJECT, "Open project…", idle.then_some(Message::OpenProject)),
                labelled_button(ADD_MUSIC, "Add music…", idle.then_some(Message::AddMusic)),
            ]
            .spacing(12);
            let start = center(column![self.logo(), status, buttons].push(error).spacing(16).align_x(Alignment::Center));
            return column![start, credit()].into();
        };
        let p = &clip.playback;
        let media = &p.media;

        let quality = p.quality();
        let showing = if quality.starts_with("loading") {
            status_icon(SHOWING_LOADING, format!("Showing the playback picture, {quality}"))
        } else if quality == "proxy" {
            status_icon(SHOWING_PROXY, "Showing the proxy (LRF): quick to decode, less detail".to_owned())
        } else {
            status_icon(SHOWING_FULL, format!("Showing the {}", if quality == "full res" { "OSV at full resolution".to_owned() } else { quality.clone() }))
        };
        let decode = match media.hw {
            Hwaccel::Software => status_icon(DECODE_CPU, "Decoding in software: no GPU decoder worked here".to_owned()),
            hw => status_icon(DECODE_GPU, format!("Decoding on the GPU with {}", match hw {
                Hwaccel::Vulkan => "Vulkan Video",
                Hwaccel::Vaapi => "VA-API",
                _ => "CUDA",
            })),
        };
        let message = self.save_status.as_ref().map(|s| text(s.clone()).style(text::secondary));
        let status: Element<'_, Message> = match &self.loading {
            Some(_) => self.loading_bar(140.0),
            None => row![].push(message).push(showing).push(decode).spacing(10).align_y(Alignment::Center).into(),
        };
        let top = row![
            tip(icon_button(NEW_PROJECT, Some(Message::NewProject)), text("Start a new, empty project (Ctrl+N)"), tooltip::Position::Bottom),
            tip(icon_button(ADD_CLIP, Some(Message::Open)), text(open_hint), tooltip::Position::Bottom),
            tip(
                icon_button(ADD_MUSIC, Some(Message::AddMusic)),
                text(if self.edit.music.clips.is_empty() { "Add music under the edit" } else { "Replace the music" }),
                tooltip::Position::Bottom,
            ),
            tip(icon_button(OPEN_PROJECT, Some(Message::OpenProject)), text("Open a saved edit (Ctrl+Shift+O)"), tooltip::Position::Bottom),
            tip(
                icon_button(SAVE, self.unsaved().then_some(Message::SaveProject(false))),
                text("Save the edit: cuts, keyframes and shape (Ctrl+S)"),
                tooltip::Position::Bottom,
            ),
            tip(
                icon_button(SAVE_AS, (!self.edit.cuts.is_empty()).then_some(Message::SaveProject(true))),
                text("Save the edit as a new project file (Ctrl+Shift+S)"),
                tooltip::Position::Bottom,
            ),
            self.export_controls(),
            // The file name: the full path is in the tooltip.
            tip(
                text(media.input.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned())),
                text(media.input.display().to_string()),
                tooltip::Position::Bottom,
            ),
            space().width(Length::Fill),
            status
        ]
            .spacing(12)
            .padding([6, 10])
            .align_y(Alignment::Center);

        let shown_view = self.shown_view();
        let view: Element<'_, Message> = match self.held.clone().or_else(|| self.sphere()) {
            Some(sphere) => {
                let (aspect, picking) = (self.aspect, self.picking);
                // A locked aspect is the largest box of that shape that fits.
                responsive(move |room| {
                    let (w, h) = match aspect.ratio() {
                        None => (room.width, room.height),
                        Some(r) => if room.width / room.height > r { (room.height * r, room.height) } else { (room.width, room.width / r) },
                    };
                    let sphere = shader(sphere.clone()).width(w).height(h);
                    // The box's layer is there from the start of picking: adding it
                    // mid-drag would rebuild the view's state and lose the drag.
                    match picking {
                        Some(area) => center(stack![sphere, canvas(PickBox(area)).width(w).height(h)]).into(),
                        None => center(sphere).into(),
                    }
                })
                .into()
            }
            None => center(column![self.logo(), text("Decoding…")].spacing(16).align_x(Alignment::Center)).into(),
        };
        let view = container(view).width(Length::Fill).height(Length::Fill).style(|_| container::background(iced::Color::BLACK));

        let current = self.current_key();
        let aiming = current.map(|k| Aiming::of(&self.edit.cuts[self.current].keyframes[k].follow));
        let tolerance = self.tolerance();
        let mut from = 0.0;
        let blocks = self
            .edit
            .cuts
            .iter()
            .map(|c| {
                let keys = c.keyframes.iter().enumerate().filter(|(_, k)| k.t >= c.start - tolerance && k.t <= c.end + tolerance).map(|(i, k)| (i, k.t, k.ease, k.follow.is_some())).collect();
                // Only those in the cut, and one either side for its ends.
                let thumbs = self.thumbs.get(&c.input).map_or_else(Vec::new, |all| {
                    let from = all.partition_point(|(t, _)| *t < c.start).saturating_sub(1);
                    let to = (all.partition_point(|(t, _)| *t <= c.end) + 1).min(all.len());
                    all[from..to].to_vec()
                });
                let block = timeline::Block { labels: cut_labels(c, self.opts.stabilize), from, len: c.end - c.start, start: c.start, keys, thumbs };
                from += c.end - c.start;
                block
            })
            .collect();
        let (timeline_start, timeline_span) = self.timeline_window();
        let music = self
            .edit
            .music
            .clips
            .iter()
            .map(|c| timeline::MusicBlock {
                label: c.input.file_stem().map_or_else(String::new, |s| s.to_string_lossy().into_owned()),
                from: c.at,
                start: c.start,
                len: c.end - c.start,
                wave: self.waveforms.get(&c.input).cloned(),
            })
            .collect();
        let timeline = timeline::Timeline {
            blocks,
            music,
            selected_music: self.selected_music,
            video: self.edit.duration(),
            duration: self.edit.frame(),
            start: timeline_start,
            span: timeline_span,
            position: self.position(),
            selected: self.selected,
            current: current.map(|k| (self.current, k)),
        }
        .view();
        let can_split = self.edit.cuts.get(self.current).is_some_and(|c| p.target() > c.start + edit::MIN_CUT && p.target() < c.end - edit::MIN_CUT);
        let transport = row![
            tip(
                icon_button(if p.playing { PAUSE } else { PLAY }, Some(Message::Toggle)),
                text(if p.playing { "Pause (Space)" } else { "Play (Space)" }),
                tooltip::Position::Top,
            ),
            tip(
                icon_button(SAVE_VIEW, (!p.playing).then_some(Message::Save(Saving::View))),
                text("Save what you see as a PNG (pause first)"),
                tooltip::Position::Top,
            ),
            tip(
                icon_button(SAVE_SPHERE, (!p.playing).then_some(Message::Save(Saving::Sphere))),
                text("Save the whole sphere as an equirect PNG (pause first)"),
                tooltip::Position::Top,
            ),
            text(format!("{} / {}", clock(self.position()), clock(self.edit.duration()))),
            tip(
                icon_button(KEY_ADD, Some(Message::KeyAdd)),
                text(if current.is_some() { "Update this keyframe with the view (K)" } else { "Add a keyframe with this view (K)" }),
                tooltip::Position::Top,
            ),
            tip(icon_button(KEY_DELETE, current.map(|_| Message::KeyDelete)), text("Delete this keyframe (Del)"), tooltip::Position::Top),
            tip(
                pick_list(&Ease::ALL[..], current.map(|k| self.edit.cuts[self.current].keyframes[k].ease), Message::KeyEase).placeholder("Ease").width(96),
                text("How this keyframe moves on to the next (J cycles)"),
                // Below: the list opens upwards and the hint would cover it.
                tooltip::Position::Bottom,
            ),
            tip(
                pick_list(&Aiming::ALL[..], aiming, Message::KeyFollow).placeholder("Follow").width(140),
                text("What this keyframe looks at: fixed angles, the selfie stick (whoever holds it), or a colour track (pick a new one by drawing a box over the subject)"),
                tooltip::Position::Bottom,
            ),
            tip(
                icon_button(PICK, (self.clip.is_some() && self.tracking.is_none()).then_some(Message::PickColour)),
                text("Track a colour: draw a box over a subject (a jacket, a tent); the keyframe here then follows it"),
                tooltip::Position::Top,
            ),
            tip(icon_button(SPLIT, can_split.then_some(Message::Split)), text("Split the cut at the playhead (C)"), tooltip::Position::Top),
            tip(
                icon_button(DELETE_CUT, self.selected.map(|_| Message::DeleteCut)),
                text("Delete the selected cut (Shift+Del)"),
                tooltip::Position::Top,
            ),
            tip(icon_button(UNDO, Some(Message::Undo)), text("Undo (Ctrl+Z)"), tooltip::Position::Top),
            tip(icon_button(REDO, Some(Message::Redo)), text("Redo (Ctrl+Shift+Z)"), tooltip::Position::Top),
            tip(icon_button(ZOOM_OUT, Some(Message::TimelineZoom(1.0 / 1.5, None))), text("Zoom the timeline out (Ctrl+−, or scroll on it)"), tooltip::Position::Top),
            tip(
                slider(0.0..=1.0, self.timeline_zoom.ln() / self.timeline_most_zoom().ln().max(1e-9), Message::TimelineZoomTo).step(0.001).width(90),
                text("Timeline zoom"),
                tooltip::Position::Top,
            ),
            tip(icon_button(ZOOM_IN, Some(Message::TimelineZoom(1.5, None))), text("Zoom the timeline in (Ctrl+=, or scroll on it)"), tooltip::Position::Top),
            tip(icon_button(ZOOM_FIT, Some(Message::TimelineZoomTo(0.0))), text("Show the whole edit (Ctrl+0)"), tooltip::Position::Top),
            space().width(Length::Fill),
            tip(
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
        let (lo, hi) = shown_view.fov_range();
        let controls = row![
            labelled("Stabilisation", pick_list(&Mode::ALL[..], Some(media.stab.mode()), Message::Stab)),
            labelled("Projection", pick_list(&Projection::ALL[..], Some(shown_view.projection), Message::Projection)),
            labelled("Playback source", pick_list(sources, Some(source), Message::Source)),
            labelled("FOV", slider(lo..=hi, shown_view.fov, Message::Fov).width(120)),
            tip(
                labelled(
                    "Clip sound",
                    row![
                        slider(crate::project::OFF_DB..=6.0, self.edit.sound.volume, Message::SoundLevel).step(0.5).on_release(Message::LevelDone).width(100),
                        text(db_label(self.edit.sound.volume)).width(70),
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center),
                ),
                text("The clips' own sound in the mix, and in the export"),
                tooltip::Position::Top,
            ),
            tip(
                labelled(
                    "Music",
                    row![
                        slider(crate::project::OFF_DB..=6.0, self.edit.music.volume, Message::MusicLevel).step(0.5).on_release(Message::LevelDone).width(100),
                        text(db_label(self.edit.music.volume)).width(70),
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center),
                ),
                text("The music track in the mix, and in the export"),
                tooltip::Position::Top,
            ),
            toggler(self.rolling_shutter).label("Rolling shutter").on_toggle(Message::RollingShutter),
            tip(
                toggler(self.full_on_pause).label("Full res when paused").on_toggle(Message::FullOnPause),
                text("Off: paused frames stay at the playback source's quality"),
                tooltip::Position::Top,
            ),
            tip(icon_button(RESET_VIEW, Some(Message::ResetView)), text("Reset the view: straight ahead, level (0)"), tooltip::Position::Top),
            tip(
                icon_button(if self.fullscreen { FULLSCREEN_EXIT } else { FULLSCREEN }, Some(Message::Fullscreen)),
                text(if self.fullscreen { "Leave fullscreen (F)" } else { "Fullscreen (F)" }),
                tooltip::Position::Top,
            ),
        ]
        .spacing(18)
        .padding([6, 10])
        .align_y(Alignment::Center)
        .wrap();

        let timeline = container(timeline).padding([2, 10]);
        column![top, view, transport, timeline, controls].push(error.map(|e| container(e).padding([0, 10]))).push(credit()).into()
    }
}

/// The tool's name: Osmo 360, with an r for its Rust core.
pub const NAME: &str = "rOsmo360";

const LOGO: &[u8] = include_bytes!("../../vibewithravelund-lowres-corrected.jpg");

/// Who made it, and how: credit to the author and to the AI-assisted way it
/// was built.
fn credit<'a>() -> Element<'a, Message> {
    container(text(concat!("rOsmo360 ", env!("CARGO_PKG_VERSION"), " · Made by Vibing with Ravelund · 2026")).size(12).style(text::secondary)).width(Length::Fill).align_x(Alignment::Center).padding([2, 10]).into()
}

/// Edit icons: scissors (split), a bin (delete cut), arrows (undo, redo).
const SPLIT: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="6" cy="6" r="3"/><circle cx="6" cy="18" r="3"/><path d="M20 4 8.1 15.9M14.5 14.5 20 20M8.1 8.1 12 12"/></svg>"#;
const DELETE_CUT: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 6h18M8 6V4h8v2M19 6l-1 14H6L5 6M10 11v6M14 11v6"/></svg>"#;
const UNDO: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M9 14 4 9l5-5"/><path d="M4 9h10.5a5.5 5.5 0 0 1 0 11H11"/></svg>"#;
/// The next cut is primed this many seconds before the playing one ends:
/// enough for its decoder to open and seek. From a network share that took
/// 0.2-1.6 s; at 1 s ahead the slower ones still stalled the change.
const PREROLL: f64 = 3.0;

/// The timeline zooms in until this many seconds fill it.
const TIMELINE_SPAN: f64 = 2.0;

/// Timeline zoom icons: a magnifier with a minus or plus, and arrows out to
/// both ends (fit).
const ZOOM_OUT: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round"><circle cx="10" cy="10" r="7"/><path d="m21 21-6-6M7 10h6"/></svg>"#;
const ZOOM_IN: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round"><circle cx="10" cy="10" r="7"/><path d="m21 21-6-6M7 10h6M10 7v6"/></svg>"#;
const ZOOM_FIT: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 5v14M21 5v14M7 12h10M10 9l-3 3 3 3M14 9l3 3-3 3"/></svg>"#;
const REDO: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m15 14 5-5-5-5"/><path d="M20 9H9.5a5.5 5.5 0 0 0 0 11H13"/></svg>"#;

/// A short name for a clip on the timeline: DJI's clip number (the digits
/// in CAM_20260713153034_0037_D), else the file name.
fn clip_name(path: &Path) -> String {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    stem.split('_').rev().find(|p| p.len() == 4 && p.bytes().all(|b| b.is_ascii_digit())).map_or(stem.clone(), str::to_owned)
}

/// Keyframe icons: a diamond with a plus (add or update) or a cross (delete).
const PICK: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m2 22 1-1h3l9-9M3 21v-3l9-9"/><path d="m15 6 3.4-3.4a2.1 2.1 0 1 1 3 3L18 9l.4.4a2.1 2.1 0 1 1-3 3l-3.8-3.8a2.1 2.1 0 1 1 3-3z"/></svg>"#;
const KEY_ADD: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linejoin="round"><path d="M10 2 18 10 10 18 2 10z" fill="white"/><path d="M19 15v8M15 19h8" stroke-linecap="round"/></svg>"#;
const KEY_DELETE: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linejoin="round"><path d="M10 2 18 10 10 18 2 10z"/><path d="m16 16 6 6M22 16l-6 6" stroke-linecap="round"/></svg>"#;

/// File icons: a page with a plus (new project), a film strip with a plus
/// (open or add clips), a note with a plus (add music), an open folder (open
/// a project), a diskette (save) and one with a pencil (save as).
const ADD_MUSIC: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M9 18V5l10-2v9"/><circle cx="6" cy="18" r="3"/><path d="M19 15v6M16 18h6"/></svg>"#;
const NEW_PROJECT: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7z"/><path d="M14 2v4a2 2 0 0 0 2 2h4M9 15h6M12 18v-6"/></svg>"#;
const ADD_CLIP: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="2" y="3" width="14" height="18" rx="2"/><path d="M6 3v18M12 3v18M2 8h4M2 16h4M12 8h4M12 16h4M20 9v6M17 12h6"/></svg>"#;
const OPEN_PROJECT: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m6 14 1.5-2.9A2 2 0 0 1 9.24 10H20a2 2 0 0 1 1.94 2.5l-1.54 6a2 2 0 0 1-1.95 1.5H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h3.9a2 2 0 0 1 1.69.9l.81 1.2a2 2 0 0 0 1.67.9H18a2 2 0 0 1 2 2v2"/></svg>"#;
const SAVE: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M15.2 3a2 2 0 0 1 1.4.6l3.8 3.8a2 2 0 0 1 .6 1.4V19a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2z"/><path d="M17 21v-7a1 1 0 0 0-1-1H8a1 1 0 0 0-1 1v7M7 3v4a1 1 0 0 0 1 1h7"/></svg>"#;
const SAVE_AS: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M11 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h10.2a2 2 0 0 1 1.4.6l3.8 3.8a2 2 0 0 1 .6 1.4V10"/><path d="M7 3v4a1 1 0 0 0 1 1h7M7 21v-7a1 1 0 0 1 1-1h4"/><path d="M21.4 15.6a1.98 1.98 0 0 0-2.8-2.8L14 17.4V20.2h2.8z"/></svg>"#;
/// An arrow out of a tray (export) and a crossed circle (cancel a running job).
const EXPORT: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 15V3M7 8l5-5 5 5"/><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/></svg>"#;
const CANCEL: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="9"/><path d="m15 9-6 6M9 9l6 6"/></svg>"#;
/// Transport: play, pause, a camera (save the view) and a globe (save the
/// whole sphere).
const PLAY: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M7 4v16l13-8z" fill="white"/></svg>"#;
const PAUSE: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="6" y="4" width="4" height="16" rx="1" fill="white"/><rect x="14" y="4" width="4" height="16" rx="1" fill="white"/></svg>"#;
const SAVE_VIEW: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M14.5 4h-5L7 7H4a2 2 0 0 0-2 2v9a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2V9a2 2 0 0 0-2-2h-3z"/><circle cx="12" cy="13" r="3"/></svg>"#;
const SAVE_SPHERE: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><path d="M2 12h20M12 2a15.3 15.3 0 0 1 4 10 15.3 15.3 0 0 1-4 10 15.3 15.3 0 0 1-4-10 15.3 15.3 0 0 1 4-10z"/></svg>"#;
/// View: turn back (reset the view) and corners out or in (fullscreen).
const RESET_VIEW: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8"/><path d="M3 3v5h5"/></svg>"#;
const FULLSCREEN: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M8 3H5a2 2 0 0 0-2 2v3M21 8V5a2 2 0 0 0-2-2h-3M3 16v3a2 2 0 0 0 2 2h3M16 21h3a2 2 0 0 0 2-2v-3"/></svg>"#;
const FULLSCREEN_EXIT: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M8 3v3a2 2 0 0 1-2 2H3M21 8h-3a2 2 0 0 1-2-2V3M3 16h3a2 2 0 0 1 2 2v3M16 21v-3a2 2 0 0 1 2-2h3"/></svg>"#;

/// Status: what the view shows (the proxy, the OSV or full res, a full-res
/// frame on its way) and how it decodes (on the GPU, or in software).
const SHOWING_PROXY: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="3" width="18" height="18" rx="2"/><path d="M7 3v18M17 3v18M3 7.5h4M3 12h18M3 16.5h4M17 7.5h4M17 16.5h4"/></svg>"#;
const SHOWING_FULL: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="3" width="18" height="18" rx="2"/><circle cx="9" cy="9" r="2"/><path d="m21 15-3.1-3.1a2 2 0 0 0-2.8 0L6 21"/></svg>"#;
const SHOWING_LOADING: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M5 22h14M5 2h14M17 22v-4.2a2 2 0 0 0-.6-1.4L12 12l-4.4 4.4a2 2 0 0 0-.6 1.4V22M7 2v4.2a2 2 0 0 0 .6 1.4L12 12l4.4-4.4a2 2 0 0 0 .6-1.4V2"/></svg>"#;
const DECODE_GPU: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M13 2 3 14h9l-1 8 10-12h-9z"/></svg>"#;
const DECODE_CPU: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="4" y="4" width="16" height="16" rx="2"/><rect x="9" y="9" width="6" height="6" rx="1"/><path d="M9 1v3M15 1v3M9 20v3M15 20v3M20 9h3M20 14h3M1 9h3M1 14h3"/></svg>"#;

/// A hint on hover, on a box of its own so it reads over the picture.
fn tip<'a>(content: impl Into<Element<'a, Message>>, hint: iced::widget::Text<'a>, position: tooltip::Position) -> Element<'a, Message> {
    tooltip(content, container(hint).padding([4, 8]).style(container::rounded_box), position).into()
}

fn icon_button<'a>(icon: &'static [u8], on_press: Option<Message>) -> Element<'a, Message> {
    button(svg(svg::Handle::from_memory(icon)).width(20).height(20)).padding(5).on_press_maybe(on_press).into()
}

/// A mix level for its slider: the bottom of the range is silence.
fn db_label(db: f64) -> String {
    if db <= crate::project::OFF_DB { "off".to_owned() } else { format!("{db:+.1} dB") }
}

/// A status icon, dimmed towards the background so it stays out of the
/// way, with what it means on hover.
fn status_icon<'a>(icon: &'static [u8], hint: String) -> Element<'a, Message> {
    let dim = svg(svg::Handle::from_memory(icon)).width(18).height(18).style(|theme: &Theme, _| svg::Style {
        color: Some(iced::Color { a: 0.45, ..theme.palette().text }),
    });
    tip(dim, text(hint), tooltip::Position::Bottom)
}

/// An icon with its words, for the start screen, which has the room.
fn labelled_button<'a>(icon: &'static [u8], label: &'a str, on_press: Option<Message>) -> Element<'a, Message> {
    button(row![svg(svg::Handle::from_memory(icon)).width(20).height(20), text(label)].spacing(8).align_y(Alignment::Center))
        .padding([6, 12])
        .on_press_maybe(on_press)
        .into()
}

/// Speaker icons for the mute button, drawn in the button's text colour.
const SPEAKER: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M11 5 6 9H2v6h4l5 4z" fill="white"/><path d="M15.5 8.5a5 5 0 0 1 0 7"/><path d="M19 5a10 10 0 0 1 0 14"/></svg>"#;
const SPEAKER_MUTED: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M11 5 6 9H2v6h4l5 4z" fill="white"/><path d="m22 9-6 6"/><path d="m16 9 6 6"/></svg>"#;

/// The OS's own colours (KDE colour scheme, GTK/portal, Windows, macOS), so
/// the player looks like the rest of the desktop; iced's dark theme where
/// they can't be read.
fn system_theme() -> Theme {
    match native_theme_iced::from_system() {
        Ok((theme, ..)) => theme,
        Err(e) => {
            eprintln!("using the default theme: {e}");
            Theme::Dark
        }
    }
}

/// A message whenever the OS theme changes. The watcher calls back on its
/// own thread; a channel hands that to iced.
fn theme_changes() -> impl iced::futures::Stream<Item = Message> {
    use iced::futures::{SinkExt, StreamExt};
    iced::stream::channel(1, async |mut output: iced::futures::channel::mpsc::Sender<Message>| {
        let (tx, mut rx) = iced::futures::channel::mpsc::unbounded();
        let _watch = match native_theme::watch::on_theme_change(move |_| {
            let _ = tx.unbounded_send(());
        }) {
            Ok(w) => w,
            Err(e) => return eprintln!("not following theme changes: {e}"),
        };
        while rx.next().await.is_some() {
            if output.send(Message::ThemeChanged).await.is_err() {
                break;
            }
        }
    })
}

fn labelled<'a>(label: &'a str, widget: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    row![text(label), widget.into()].spacing(6).align_y(Alignment::Center).into()
}

/// A finished colour track: its CSV, where it starts (cut and clip time),
/// how many samples found the colour, and the colour.
#[derive(Debug, Clone)]
pub struct TrackDone {
    path: PathBuf,
    cut: usize,
    t: f64,
    found: usize,
    rows: usize,
    colour: String,
}

/// The box being drawn to pick a colour, over the view.
struct PickBox(Option<Rectangle>);

impl canvas::Program<Message> for PickBox {
    type State = ();

    fn draw(&self, _: &(), renderer: &iced::Renderer, _: &Theme, bounds: Rectangle, _: mouse::Cursor) -> Vec<canvas::Geometry> {
        let Some(r) = &self.0 else { return vec![] };
        let mut frame = canvas::Frame::new(renderer, bounds.size());
        let rect = canvas::Path::rectangle(Point::new(r.x * bounds.width, r.y * bounds.height), Size::new(r.width * bounds.width, r.height * bounds.height));
        frame.fill(&rect, iced::Color { a: 0.15, ..iced::Color::WHITE });
        frame.stroke(&rect, canvas::Stroke::default().with_width(2.0).with_color(iced::Color::WHITE));
        vec![frame.into_geometry()]
    }
}

/// The Follow picker's choices for a keyframe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Aiming {
    Fixed,
    Stick,
    /// Any colour track: choosing it on a keyframe that follows none draws
    /// a box to pick one.
    Colour,
}

impl Aiming {
    const ALL: [Aiming; 3] = [Aiming::Fixed, Aiming::Stick, Aiming::Colour];

    fn of(follow: &Option<Follow>) -> Aiming {
        match follow {
            None => Aiming::Fixed,
            Some(Follow::Stick) => Aiming::Stick,
            Some(Follow::Track(_)) => Aiming::Colour,
        }
    }
}

impl std::fmt::Display for Aiming {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Aiming::Fixed => f.write_str("Fixed"),
            Aiming::Stick => f.write_str("Stick"),
            Aiming::Colour => f.write_str("Colour track"),
        }
    }
}

/// A cut's names on the timeline, longest first, for the block to show the
/// first that fits: its clip, length, stabilisation when that isn't horizon
/// (the usual), and note.
fn cut_labels(c: &Cut, default: Mode) -> Vec<String> {
    let mut label = format!("{} · {:.1} s", clip_name(&c.input), c.end - c.start);
    match c.stabilize.unwrap_or(default) {
        Mode::Horizon => {}
        Mode::Off => label += " · unstabilised",
        Mode::Level => label += " · level",
        Mode::Lock => label += " · world lock",
    }
    match c.note.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        Some(note) => vec![format!("{label} · {note}"), label],
        None => vec![label],
    }
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
#[derive(Clone)]
struct Sphere {
    rig: Arc<Rig>,
    shown: Arc<Shown>,
    camera: Camera,
    rig_from_view: Vec<M3>,
    fov_deg: f64,
    /// Drags draw a box to pick a colour instead of looking around.
    picking: bool,
}

#[derive(Default)]
struct Drag {
    button: Option<mouse::Button>,
    last: Option<Point>,
    /// The size last reported to the app.
    size: Option<Size>,
    /// Where a colour-picking box started, in the view.
    pick: Option<Point>,
}

impl Sphere {
    /// While picking a colour, a drag draws a box: the app hears of it as it
    /// grows and once it's drawn, in fractions of the view.
    fn pick(&self, drag: &mut Drag, event: &iced::Event, bounds: Rectangle, cursor: mouse::Cursor) -> Option<shader::Action<Message>> {
        let at = || cursor.position().map(|p| Point::new((p.x - bounds.x).clamp(0.0, bounds.width), (p.y - bounds.y).clamp(0.0, bounds.height)));
        let area = |a: Point, b: Point| {
            Rectangle::new(Point::new(a.x.min(b.x) / bounds.width, a.y.min(b.y) / bounds.height), Size::new((a.x - b.x).abs() / bounds.width, (a.y - b.y).abs() / bounds.height))
        };
        match event {
            iced::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                cursor.position_over(bounds)?;
                drag.pick = at();
                Some(shader::Action::capture())
            }
            iced::Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                let (a, b) = (drag.pick?, at()?);
                Some(shader::Action::publish(Message::PickArea(area(a, b))).and_capture())
            }
            iced::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                let (a, b) = (drag.pick.take()?, at()?);
                Some(shader::Action::publish(Message::AreaPicked(area(a, b), (bounds.width / bounds.height) as f64)).and_capture())
            }
            _ => None,
        }
    }

    /// Drags look around, the wheel zooms.
    fn input(&self, drag: &mut Drag, event: &iced::Event, bounds: Rectangle, cursor: mouse::Cursor) -> Option<shader::Action<Message>> {
        if self.picking {
            return self.pick(drag, event, bounds, cursor);
        }
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
            camera: self.camera,
            rig_from_view: self.rig_from_view.clone(),
        }
    }

    fn mouse_interaction(&self, drag: &Drag, bounds: Rectangle, cursor: mouse::Cursor) -> mouse::Interaction {
        if self.picking && cursor.is_over(bounds) {
            mouse::Interaction::Crosshair
        } else if drag.button.is_some() {
            mouse::Interaction::Grabbing
        } else if cursor.is_over(bounds) {
            mouse::Interaction::Grab
        } else {
            mouse::Interaction::default()
        }
    }
}
