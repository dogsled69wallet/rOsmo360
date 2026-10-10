//! Real-time 360 player: an iced window with the stitched view, an open-file
//! dialog and playback controls. Free look on all three axes, stabilisation
//! and rolling-shutter correction run in the view's shader.

mod audio;
mod edit;
mod export;
mod gpu;
mod playback;
mod settings;
mod source;
mod thumbs;
mod timeline;

use crate::ffmpeg::{self, Hwaccel};
use crate::stab::{ImuAxes, Mode, Stabilizer};
use crate::project::{Cut, Project};
use crate::stitch::M3;
use crate::view::{Camera, Ease, Projection, View};
use anyhow::{Context, Result};
use gpu::{Rig, Shown};
use iced::keyboard::{self, Key, key::Named};
use iced::mouse;
use iced::widget::{button, center, column, progress_bar, container, image, pick_list, responsive, row, shader, slider, space, svg, text, toggler, tooltip};
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

impl Media {
    fn load(input: PathBuf, opts: &Options) -> Result<Media> {
        let stitch = crate::StitchOpts { stabilize: Some(opts.stabilize), imu_axes: opts.imu_axes, world_z_down: opts.world_z_down, ..Default::default() };
        let p = crate::Pipeline::new(&input, &stitch, opts.stabilize)?;
        let proxy = ffmpeg::proxy(&input);
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
        #[cfg(target_os = "linux")]
        platform_specific: window::settings::PlatformSpecific { application_id: NAME.to_owned(), ..Default::default() },
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
    /// Export the edit: ask where, then render in the background.
    Export,
    ExportTo(Option<PathBuf>),
    /// Fraction of the export done.
    ExportProgress(f32),
    ExportDone(Result<PathBuf, String>),
    ExportCancel,
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
    theme: Theme,
    logo: image::Handle,
    /// Every clip opened so far, ready for the cuts that use it.
    library: Vec<Media>,
    /// Each clip's timeline thumbnails by clip time, as they arrive.
    thumbs: std::collections::HashMap<PathBuf, Vec<(f64, image::Handle)>>,
    edit: edit::Edit,
    /// The cut being played, and the one selected on the timeline.
    current: usize,
    selected: Option<usize>,
    /// Clips waiting to be loaded, in order, and whether each is added as a
    /// cut (opened) or only loaded for a project's cuts.
    queue: std::collections::VecDeque<(PathBuf, bool)>,
    /// Whether the clip loading now is added as a cut.
    loading_adds: bool,
    /// The project file, once opened or saved, and the edit version saved.
    project: Option<PathBuf>,
    saved_version: u64,
    /// What to do once the project has been saved.
    after_save: Option<Then>,
    /// Exports are this many pixels on the short side.
    resolution: crate::Resolution,
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
    /// The settings as last written, to save only when they change.
    saved_settings: settings::Settings,
}

impl App {
    fn new(opts: Options, saved: settings::Settings) -> Self {
        App {
            opts,
            theme: system_theme(),
            logo: image::Handle::from_bytes(LOGO),
            library: Vec::new(),
            thumbs: Default::default(),
            edit: edit::Edit::default(),
            current: 0,
            selected: None,
            dragging: false,
            timeline_zoom: 1.0,
            timeline_start: 0.0,
            queue: Default::default(),
            loading_adds: true,
            project: None,
            saved_version: 0,
            after_save: None,
            resolution: saved.resolution,
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
        }
    }

    /// Handles `message`, then saves the settings if it changed any.
    fn update(&mut self, message: Message) -> Task<Message> {
        let task = self.handle(message);
        let now = self.settings();
        if now != self.saved_settings {
            now.save();
            self.saved_settings = now;
        }
        task
    }

    /// What the speakers get: nothing while muted, so unmuting restores
    /// the level that was set.
    fn volume(&self) -> f32 {
        if self.muted { 0.0 } else { self.volume }
    }

    /// Adds the next queued clip: straight away if it is in the library
    /// already, else once it has loaded.
    fn open_next(&mut self) -> Task<Message> {
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
        // A project's first cut can play once its clip is in.
        if self.clip.is_none() && !self.edit.cuts.is_empty() {
            let start = self.edit.cuts[0].start;
            self.follow = true;
            self.goto(0, start, false);
        }
        Task::none()
    }

    /// The edit as a project file holds it.
    fn to_project(&self) -> Project {
        let aspect = match self.aspect {
            Aspect::Free => None,
            Aspect::Ratio(w, h) => Some(crate::view::Aspect(w.into(), h.into())),
        };
        Project { aspect, cuts: self.edit.cuts.clone() }
    }

    /// Renders the edit to `path` on its own thread with the export the
    /// command line uses, at the chosen resolution, in the aspect lock's
    /// shape (16:9 when free), stabilised as the player is.
    fn start_export(&mut self, path: PathBuf) -> Task<Message> {
        use iced::futures::{SinkExt, StreamExt};
        let project = self.to_project();
        let [w, h] = project.aspect.unwrap_or(crate::view::Aspect(16, 9)).size(self.resolution.short_side());
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
                let result = crate::export(&project, &opts, (w as usize, h as usize), false, &path, &mut progress);
                let _ = tx.unbounded_send(Message::ExportDone(result.map(|()| path).map_err(|e| format!("{e:#}"))));
            });
            while let Some(m) = rx.next().await {
                if output.send(m).await.is_err() {
                    break;
                }
            }
        });
        Task::run(work, |m| m)
    }

    /// Export button and resolution, or a running export's progress.
    fn export_controls(&self) -> Element<'_, Message> {
        match &self.exporting {
            Some((_, done)) => row![
                text(format!("Exporting {:.0}%", done * 100.0)),
                progress_bar(0.0..=1.0, *done).length(140).girth(8),
                button("Cancel").on_press(Message::ExportCancel),
            ]
            .spacing(8)
            .align_y(Alignment::Center)
            .into(),
            None => row![
                tooltip(
                    labelled("Aspect", pick_list(&Aspect::ALL[..], Some(self.aspect), Message::Aspect)),
                    text("The shape of the view, of saved views and of the export (16:9 when free)"),
                    tooltip::Position::Bottom,
                ),
                tooltip(
                    button("Export…").on_press_maybe((!self.edit.cuts.is_empty()).then_some(Message::Export)),
                    text("Render the edit to MP4 with sound, at this resolution, in the aspect's shape"),
                    tooltip::Position::Bottom,
                ),
                pick_list(&crate::Resolution::ALL[..], Some(self.resolution), Message::Resolution),
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
        }
    }

    fn unsaved(&self) -> bool {
        self.edit.version != self.saved_version && !self.edit.cuts.is_empty()
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
        // A dot marks unsaved changes, as in many editors.
        let project = match &self.project {
            Some(p) => format!(" · {}{}", name(p), if self.unsaved() { " •" } else { "" }),
            None if self.unsaved() => " · unsaved edit •".to_owned(),
            None => String::new(),
        };
        match (&self.loading, &self.clip) {
            (Some(p), _) => format!("{NAME}{project} · loading {}", name(p)),
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
        if busy { Subscription::batch([keys, theme, close, window::frames().map(|_| Message::Tick)]) } else { Subscription::batch([keys, theme, close]) }
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
                self.queue.extend(cuts.iter().map(|c| (c.input.clone(), false)));
                self.edit.replace(cuts);
                self.saved_version = if missing.is_empty() { self.edit.version } else { 0 };
                self.project = Some(path);
                if self.loading.is_none() {
                    return self.open_next();
                }
            }
            Message::SaveProject(as_new) => {
                if self.edit.cuts.is_empty() {
                    return Task::none();
                }
                if let (Some(path), false) = (&self.project, as_new) {
                    return self.update(Message::SaveProjectTo(Some(path.clone())));
                }
                let first = &self.edit.cuts[0].input;
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
            Message::Export => {
                if self.edit.cuts.is_empty() || self.exporting.is_some() {
                    return Task::none();
                }
                let base = self.project.as_ref().unwrap_or(&self.edit.cuts[0].input);
                let dir = base.parent().map(Path::to_owned);
                let name = format!("{}_{}.mp4", base.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(), self.resolution);
                return Task::perform(
                    async move {
                        let mut dialog = rfd::AsyncFileDialog::new().set_title("Export video").add_filter("MP4 video", &["mp4"]).set_file_name(name);
                        if let Some(dir) = dir {
                            dialog = dialog.set_directory(dir);
                        }
                        dialog.save_file().await.map(|f| f.path().to_owned())
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
            Message::ExportDone(result) => {
                let cancelled = self.exporting.take().is_some_and(|(c, _)| c.load(std::sync::atomic::Ordering::Relaxed));
                match result {
                    Ok(path) => self.save_status = Some(format!("Exported {}", path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned()))),
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
            Message::Select(i) => self.selected = Some(i),
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
                self.edit.checkpoint();
                if let Some(cut) = self.edit.cuts.get_mut(self.current) {
                    cut.keyframes.set(t, view, tolerance);
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
                let v = self.volume();
                self.with_playback(|p| p.set_volume(v));
            }
            Message::Volume(v) => {
                self.volume = v;
                self.muted = false;
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

    /// The view on screen, as angles.
    fn shown_view(&self) -> View {
        match self.playing_cut() {
            Some((cut, t)) if self.follow => cut.keyframes.view_at(t).unwrap_or(self.look),
            _ => self.look,
        }
    }

    /// The view on screen, ready to draw: mid-morph between perspective and
    /// little planet when following keyframes.
    fn camera(&self) -> Camera {
        match self.playing_cut() {
            Some((cut, t)) if self.follow => cut.keyframes.camera_at(t).unwrap_or_else(|| self.look.camera()),
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
        })
    }

    /// Adds the whole clip as a cut at the end of the edit and selects it.
    /// The first clip starts playing, as opening a clip always did; later
    /// ones leave playback where it is.
    fn add_cut(&mut self, input: PathBuf, duration: f64) {
        self.edit.checkpoint();
        self.edit.cuts.push(Cut { input, start: 0.0, end: duration, keyframes: Default::default(), stabilize: Some(self.opts.stabilize) });
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
                let mut playback = Playback::new(media, self.volume(), self.full_on_pause);
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
        let Some(c) = &mut self.clip else { return };
        if let Some((frame, fix)) = c.playback.tick() {
            c.shown = Some(Arc::new(Shown::new(frame, fix)));
            self.held = None;
        }
        self.error = c.playback.error.clone();
        // At a cut's out point (or the clip's end), play on into the next cut.
        let Some(cut) = self.edit.cuts.get(self.current) else { return };
        let p = &c.playback;
        if (p.playing && p.now() >= cut.end) || p.ended {
            if self.current + 1 < self.edit.cuts.len() {
                let next = self.current + 1;
                let start = self.edit.cuts[next].start;
                self.goto(next, start, true);
            } else if p.playing {
                c.playback.pause();
            }
        }
    }

    /// The stretch of edit time the timeline shows: (start, span).
    fn timeline_window(&self) -> (f64, f64) {
        let d = self.edit.duration();
        let span = (d / self.timeline_zoom).max(TIMELINE_SPAN.min(d));
        (self.timeline_start.clamp(0.0, (d - span).max(0.0)), span)
    }

    /// The closest zoom: `TIMELINE_SPAN` seconds across.
    fn timeline_most_zoom(&self) -> f64 {
        (self.edit.duration() / TIMELINE_SPAN).max(1.0)
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
            Key::Named(Named::Delete | Named::Backspace) => return self.update(Message::KeyDelete),
            Key::Character(c) => match c {
                "o" | "O" if mods.command() && mods.shift() => return self.update(Message::OpenProject),
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
                        Mode::Horizon => Mode::Lock,
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

    fn loading_text(&self, path: &Path) -> String {
        format!("Loading {}…", path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned()))
    }

    fn view(&self) -> Element<'_, Message> {
        let open = button(if self.edit.cuts.is_empty() { "Open…" } else { "Add clip…" }).on_press(Message::Open);
        let error = self.error.as_ref().map(|e| text(e.clone()).style(text::danger));
        let Some(clip) = &self.clip else {
            let status = match &self.loading {
                Some(p) => self.loading_text(p),
                None => "Open DJI Osmo 360 .OSV clips, or a saved project, to start.".to_owned(),
            };
            let buttons = row![open, button("Open project…").on_press(Message::OpenProject)].spacing(12);
            let start = center(column![self.logo(), text(status), buttons].push(error).spacing(16).align_x(Alignment::Center));
            return column![start, credit()].into();
        };
        let p = &clip.playback;
        let media = &p.media;

        let status = match (&self.loading, &self.save_status) {
            (Some(path), _) => self.loading_text(path),
            (None, Some(s)) => format!("{s} · {} · {:?} decode", p.quality(), media.hw),
            (None, None) => format!("{} · {:?} decode", p.quality(), media.hw),
        };
        let top = row![
            open,
            tooltip(button("Open project…").on_press(Message::OpenProject), text("Open a saved edit (Ctrl+Shift+O)"), tooltip::Position::Bottom),
            tooltip(
                button("Save project").on_press_maybe(self.unsaved().then_some(Message::SaveProject(false))),
                text("Save the edit: cuts, keyframes and shape (Ctrl+S; Ctrl+Shift+S saves as)"),
                tooltip::Position::Bottom,
            ),
            self.export_controls(),
            // The file name: the full path is in the tooltip.
            tooltip(
                text(media.input.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned())),
                text(media.input.display().to_string()),
                tooltip::Position::Bottom,
            ),
            space().width(Length::Fill),
            text(status)
        ]
            .spacing(12)
            .padding([6, 10])
            .align_y(Alignment::Center);

        let shown_view = self.shown_view();
        let view: Element<'_, Message> = match self.held.clone().or_else(|| self.sphere()) {
            Some(sphere) => {
                let aspect = self.aspect;
                // A locked aspect is the largest box of that shape that fits.
                responsive(move |room| {
                    let (w, h) = match aspect.ratio() {
                        None => (room.width, room.height),
                        Some(r) => if room.width / room.height > r { (room.height * r, room.height) } else { (room.width, room.width / r) },
                    };
                    center(shader(sphere.clone()).width(w).height(h)).into()
                })
                .into()
            }
            None => center(column![self.logo(), text("Decoding…")].spacing(16).align_x(Alignment::Center)).into(),
        };
        let view = container(view).width(Length::Fill).height(Length::Fill).style(|_| container::background(iced::Color::BLACK));

        let current = self.current_key();
        let tolerance = self.tolerance();
        let mut from = 0.0;
        let blocks = self
            .edit
            .cuts
            .iter()
            .map(|c| {
                let keys = c.keyframes.iter().enumerate().filter(|(_, k)| k.t >= c.start - tolerance && k.t <= c.end + tolerance).map(|(i, k)| (i, k.t, k.ease)).collect();
                // Only those in the cut, and one either side for its ends.
                let thumbs = self.thumbs.get(&c.input).map_or_else(Vec::new, |all| {
                    let from = all.partition_point(|(t, _)| *t < c.start).saturating_sub(1);
                    let to = (all.partition_point(|(t, _)| *t <= c.end) + 1).min(all.len());
                    all[from..to].to_vec()
                });
                let block = timeline::Block { label: cut_label(c, self.opts.stabilize), from, len: c.end - c.start, start: c.start, keys, thumbs };
                from += c.end - c.start;
                block
            })
            .collect();
        let (timeline_start, timeline_span) = self.timeline_window();
        let timeline = timeline::Timeline {
            blocks,
            duration: self.edit.duration(),
            start: timeline_start,
            span: timeline_span,
            position: self.position(),
            selected: self.selected,
            current: current.map(|k| (self.current, k)),
        }
        .view();
        let can_split = self.edit.cuts.get(self.current).is_some_and(|c| p.target() > c.start + edit::MIN_CUT && p.target() < c.end - edit::MIN_CUT);
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
            text(format!("{} / {}", clock(self.position()), clock(self.edit.duration()))),
            tooltip(
                icon_button(KEY_ADD, Some(Message::KeyAdd)),
                text(if current.is_some() { "Update this keyframe with the view (K)" } else { "Add a keyframe with this view (K)" }),
                tooltip::Position::Top,
            ),
            tooltip(icon_button(KEY_DELETE, current.map(|_| Message::KeyDelete)), text("Delete this keyframe (Del)"), tooltip::Position::Top),
            tooltip(
                pick_list(&Ease::ALL[..], current.map(|k| self.edit.cuts[self.current].keyframes[k].ease), Message::KeyEase).placeholder("Ease").width(96),
                text("How this keyframe moves on to the next (J cycles)"),
                tooltip::Position::Top,
            ),
            tooltip(icon_button(SPLIT, can_split.then_some(Message::Split)), text("Split the cut at the playhead (C)"), tooltip::Position::Top),
            tooltip(
                icon_button(DELETE_CUT, self.selected.map(|_| Message::DeleteCut)),
                text("Delete the selected cut (Shift+Del)"),
                tooltip::Position::Top,
            ),
            tooltip(icon_button(UNDO, Some(Message::Undo)), text("Undo (Ctrl+Z)"), tooltip::Position::Top),
            tooltip(icon_button(REDO, Some(Message::Redo)), text("Redo (Ctrl+Shift+Z)"), tooltip::Position::Top),
            tooltip(icon_button(ZOOM_OUT, Some(Message::TimelineZoom(1.0 / 1.5, None))), text("Zoom the timeline out (Ctrl+−, or scroll on it)"), tooltip::Position::Top),
            tooltip(
                slider(0.0..=1.0, self.timeline_zoom.ln() / self.timeline_most_zoom().ln().max(1e-9), Message::TimelineZoomTo).step(0.001).width(90),
                text("Timeline zoom"),
                tooltip::Position::Top,
            ),
            tooltip(icon_button(ZOOM_IN, Some(Message::TimelineZoom(1.5, None))), text("Zoom the timeline in (Ctrl+=, or scroll on it)"), tooltip::Position::Top),
            tooltip(icon_button(ZOOM_FIT, Some(Message::TimelineZoomTo(0.0))), text("Show the whole edit (Ctrl+0)"), tooltip::Position::Top),
            space().width(Length::Fill),
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
        let (lo, hi) = shown_view.fov_range();
        let controls = row![
            labelled("Stabilisation", pick_list(&Mode::ALL[..], Some(media.stab.mode()), Message::Stab)),
            labelled("Projection", pick_list(&Projection::ALL[..], Some(shown_view.projection), Message::Projection)),
            labelled("Playback source", pick_list(sources, Some(source), Message::Source)),
            labelled("FOV", slider(lo..=hi, shown_view.fov, Message::Fov).width(120)),
            toggler(self.rolling_shutter).label("Rolling shutter").on_toggle(Message::RollingShutter),
            tooltip(
                toggler(self.full_on_pause).label("Full res when paused").on_toggle(Message::FullOnPause),
                text("Off: paused frames stay at the playback source's quality"),
                tooltip::Position::Top,
            ),
            button("Reset view").on_press(Message::ResetView),
            button(if self.fullscreen { "Exit fullscreen" } else { "Fullscreen" }).on_press(Message::Fullscreen),
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
const KEY_ADD: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linejoin="round"><path d="M10 2 18 10 10 18 2 10z" fill="white"/><path d="M19 15v8M15 19h8" stroke-linecap="round"/></svg>"#;
const KEY_DELETE: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linejoin="round"><path d="M10 2 18 10 10 18 2 10z"/><path d="m16 16 6 6M22 16l-6 6" stroke-linecap="round"/></svg>"#;

fn icon_button<'a>(icon: &'static [u8], on_press: Option<Message>) -> Element<'a, Message> {
    button(svg(svg::Handle::from_memory(icon)).width(20).height(20)).padding(5).on_press_maybe(on_press).into()
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

/// A cut's name on the timeline: its clip, length, and stabilisation when
/// that isn't horizon (the usual).
fn cut_label(c: &Cut, default: Mode) -> String {
    let label = format!("{} · {:.1} s", clip_name(&c.input), c.end - c.start);
    match c.stabilize.unwrap_or(default) {
        Mode::Horizon => label,
        Mode::Off => format!("{label} · unstabilised"),
        Mode::Lock => format!("{label} · world lock"),
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
            camera: self.camera,
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
