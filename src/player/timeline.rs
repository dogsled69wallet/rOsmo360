//! The timeline under the view. The top row holds the edit's cuts as
//! blocks showing thumbnails of their footage: click one to select it, drag
//! it to move it, drag its edges to trim it. Below is the seek bar with each
//! cut's keyframes as diamonds (shaped by their ease): click the bar to seek,
//! click a diamond to jump to it, drag one to retime it.
//!
//! Under the seek bar is the music lane: the music track's clips with their
//! waveforms, moved and trimmed like cuts, and a line where the video ends
//! (music beyond it fades out in the export).
//!
//! It shows a window of the edit: the wheel zooms around the pointer, Shift
//! or a sideways wheel pans, and when zoomed in a scrollbar at the bottom
//! shows and moves the window.

use super::{Message, music, thumbs};
use crate::view::Ease;
use iced::alignment::Vertical;
use iced::mouse;
use iced::widget::canvas::{self, Frame, Geometry, Path, Stroke, Text};
use iced::widget::image::Handle;
use iced::{Point, Rectangle, Renderer, Size, Theme};
use std::sync::Arc;

/// Diamond half-size, and how close a click must land to pick a diamond or
/// a cut's edge.
const DIAMOND: f32 = 6.0;
const PICK: f32 = 7.0;
const BAR: f32 = 8.0;
/// The cut row's height, and the gap between cut blocks.
const CUTS: f32 = 40.0;
const GAP: f32 = 2.0;
/// The seek bar's row and the music lane's, under the cuts.
const SEEK: f32 = 28.0;
const MUSIC: f32 = 24.0;
const MUSIC_TOP: f32 = CUTS + SEEK;
/// The scrollbar's row at the bottom, and the bar's thickness.
const SCROLL: f32 = 8.0;
const SCROLL_BAR: f32 = 5.0;
pub const HEIGHT: f32 = CUTS + SEEK + MUSIC + SCROLL;
/// A press on a cut that moves less than this is a click, not a drag.
const DRAG_FROM: f32 = 4.0;
/// A music edge dragged this close to a cut boundary, the playhead or
/// another song's edge snaps to it.
const SNAP: f32 = 8.0;

/// One cut as the timeline shows it.
pub struct Block {
    /// Its names, longest first; the first that fits is shown.
    pub labels: Vec<String>,
    /// Where it starts on the timeline, and how long it lasts.
    pub from: f64,
    pub len: f64,
    /// Its in point in clip time.
    pub start: f64,
    /// Its keyframes within the cut: index into the cut's keyframes, clip
    /// time, and how it moves on to the next (drawn as its shape).
    /// Keyframes: index, clip time, ease, and whether it follows the stick.
    pub keys: Vec<(usize, f64, Ease, bool)>,
    /// Thumbnails of its clip by clip time, in order (see `thumbs`).
    pub thumbs: Vec<(f64, Handle)>,
}

impl Block {
    /// The thumbnail nearest clip time `t`.
    fn thumb_at(&self, t: f64) -> Option<&Handle> {
        let i = self.thumbs.partition_point(|(at, _)| *at < t);
        let before = i.checked_sub(1).map(|j| &self.thumbs[j]);
        match (before, self.thumbs.get(i)) {
            (Some(a), Some(b)) => Some(if t - a.0 < b.0 - t { &a.1 } else { &b.1 }),
            (a, b) => a.or(b).map(|x| &x.1),
        }
    }
}

/// A music clip as the music lane shows it.
pub struct MusicBlock {
    pub label: String,
    /// Where it starts on the edit, its in point in the song, and its length.
    pub from: f64,
    pub start: f64,
    pub len: f64,
    /// The song's waveform, once read.
    pub wave: Option<Arc<music::Waveform>>,
}

pub struct Timeline {
    pub blocks: Vec<Block>,
    pub music: Vec<MusicBlock>,
    pub selected_music: Option<usize>,
    /// Where the video ends; `duration` runs on to the music's end if later.
    pub video: f64,
    pub duration: f64,
    /// The stretch of edit time shown: from `start`, `span` seconds.
    pub start: f64,
    pub span: f64,
    /// Playhead, in edit time.
    pub position: f64,
    pub selected: Option<usize>,
    /// The keyframe at the playhead (cut, keyframe), drawn highlighted.
    pub current: Option<(usize, usize)>,
}

#[derive(Default)]
pub enum Drag {
    #[default]
    None,
    Playhead,
    Key { cut: usize, key: usize },
    /// Trimming a cut's in (`start`) or out point. The scale is kept from
    /// the press: the timeline rescales as the edit's length changes.
    Trim { cut: usize, start: bool, x0: f32, from: f64, per_px: f64 },
    /// Moving a cut; `to` is where it would go.
    Move { cut: usize, x0: f32, moved: bool, to: usize },
    /// Dragging the scrollbar, which was at `start` when pressed at `x0`.
    Scroll { x0: f32, start: f64 },
    /// Moving music clip `clip`, which started at `at` when pressed at `x0`.
    MusicMove { clip: usize, x0: f32, at: f64, per_px: f64 },
    /// Trimming music clip `clip`'s in (`start`) or out edge, at song time
    /// `from` when pressed.
    MusicTrim { clip: usize, start: bool, x0: f32, from: f64, per_px: f64 },
}

#[derive(Default)]
pub struct State {
    drag: Drag,
    /// Shift held: the wheel pans instead of zooming.
    shift: bool,
}

impl Timeline {
    fn width(b: Rectangle) -> f32 {
        (b.width - 2.0 * PICK).max(1.0)
    }

    /// Edit time to x, with room either side for diamonds at the ends.
    /// Times outside the window land outside the widget.
    fn x(&self, t: f64, b: Rectangle) -> f32 {
        PICK + ((t - self.start) / self.span.max(1e-9)) as f32 * Self::width(b)
    }

    fn t(&self, x: f32, b: Rectangle) -> f64 {
        (self.start + ((x - PICK) / Self::width(b)) as f64 * self.span).clamp(0.0, self.duration)
    }

    fn zoomed(&self) -> bool {
        self.span < self.duration - 1e-6
    }

    /// The scrollbar's thumb, left and right x.
    fn scroll_thumb(&self, b: Rectangle) -> (f32, f32) {
        let w = Self::width(b) as f64 / self.duration.max(1e-9);
        (PICK + (self.start * w) as f32, PICK + ((self.start + self.span) * w) as f32)
    }

    fn block_at(&self, x: f32, b: Rectangle) -> Option<usize> {
        self.blocks.iter().position(|c| x >= self.x(c.from, b) && x <= self.x(c.from + c.len, b))
    }

    /// The nearest diamond within reach of `x`: (cut, keyframe).
    fn key_at(&self, x: f32, b: Rectangle) -> Option<(usize, usize)> {
        let mut best = None;
        for (ci, c) in self.blocks.iter().enumerate() {
            for &(ki, t, _, _) in &c.keys {
                let d = (self.x(c.from + t - c.start, b) - x).abs();
                if d <= PICK && best.is_none_or(|(bd, _)| d < bd) {
                    best = Some((d, (ci, ki)));
                }
            }
        }
        best.map(|(_, k)| k)
    }

    /// Whether `x` is on cut `i`'s in edge (`Some(true)`) or out edge.
    fn edge_at(&self, i: usize, x: f32, b: Rectangle) -> Option<bool> {
        let c = &self.blocks[i];
        let (l, r) = (self.x(c.from, b), self.x(c.from + c.len, b));
        if r - l < 3.0 * PICK {
            // Too narrow for a body: the nearer edge.
            return Some(x - l < r - x);
        }
        if x - l <= PICK {
            Some(true)
        } else if r - x <= PICK {
            Some(false)
        } else {
            None
        }
    }

    fn music_at(&self, x: f32, b: Rectangle) -> Option<usize> {
        self.music.iter().position(|m| x >= self.x(m.from, b) && x <= self.x(m.from + m.len, b))
    }

    /// Whether `x` is on music clip `i`'s in edge (`Some(true)`) or out edge.
    fn music_edge_at(&self, i: usize, x: f32, b: Rectangle) -> Option<bool> {
        let m = &self.music[i];
        let (l, r) = (self.x(m.from, b), self.x(m.from + m.len, b));
        if r - l < 3.0 * PICK {
            Some(x - l < r - x)
        } else if x - l <= PICK {
            Some(true)
        } else if r - x <= PICK {
            Some(false)
        } else {
            None
        }
    }

    /// Edit times a music clip's edges snap to, but for music clip `clip`'s
    /// own: the start, the cut boundaries, the playhead, and the other songs'
    /// edges.
    fn snap_targets(&self, clip: usize) -> Vec<f64> {
        let mut at = vec![0.0, self.position];
        at.extend(self.blocks.iter().flat_map(|c| [c.from, c.from + c.len]));
        at.extend(self.music.iter().enumerate().filter(|&(i, _)| i != clip).flat_map(|(_, m)| [m.from, m.from + m.len]));
        at
    }

    /// How far to shift any of `edges` (edit times) onto the nearest snap
    /// target within `SNAP` pixels, or 0.
    fn snap(&self, clip: usize, edges: &[f64], per_px: f64) -> f64 {
        let reach = SNAP as f64 * per_px;
        self.snap_targets(clip)
            .iter()
            .flat_map(|t| edges.iter().map(move |e| t - e))
            .filter(|d| d.abs() < reach)
            .min_by(|a, b| a.abs().total_cmp(&b.abs()))
            .unwrap_or(0.0)
    }

    /// Where a moved cut would land: before the first cut whose middle is
    /// right of `x`.
    fn drop_at(&self, x: f32, b: Rectangle) -> usize {
        self.blocks.iter().filter(|c| self.x(c.from + c.len / 2.0, b) < x).count()
    }
}

impl Timeline {
    /// The two layers: block backgrounds with their thumbnails, and over them
    /// everything else, which also takes the mouse. Within one layer iced
    /// draws images over shapes, so the labels and handles need their own.
    pub fn view<'a>(self) -> iced::Element<'a, Message> {
        let t = Arc::new(self);
        use iced::widget::canvas;
        let layers = iced::widget::stack![
            canvas(Pictures(t.clone())).width(iced::Length::Fill).height(HEIGHT),
            canvas(Marks(t)).width(iced::Length::Fill).height(HEIGHT),
        ];
        // Zoomed in, what is outside the window is drawn outside the widget;
        // canvases don't clip, so the container does.
        iced::widget::container(layers).clip(true).into()
    }

    /// The music lane: its clips with their waveforms and handles, and where
    /// the video ends, the music past it dimmed (it fades out there).
    fn draw_music(&self, frame: &mut Frame, theme: &Theme, b: Rectangle) {
        let palette = theme.extended_palette();
        let (top, h) = (MUSIC_TOP + 2.0, MUSIC - 4.0);
        let lane = Path::rounded_rectangle(Point::new(self.x(0.0, b), top), Size::new((self.x(self.duration, b) - self.x(0.0, b)).max(0.0), h), 4.0.into());
        frame.fill(&lane, palette.background.weak.color.scale_alpha(0.5));
        for (i, m) in self.music.iter().enumerate() {
            let (l, r) = (self.x(m.from, b) + GAP / 2.0, self.x(m.from + m.len, b) - GAP / 2.0);
            if r < 0.0 || l > b.width {
                continue;
            }
            let block = Path::rounded_rectangle(Point::new(l, top), Size::new((r - l).max(1.0), h), 4.0.into());
            let selected = Some(i) == self.selected_music;
            frame.fill(&block, if selected { palette.success.weak.color } else { palette.success.base.color.scale_alpha(0.55) });
            // The waveform: per two pixels, the loudest peak under them.
            if let Some(w) = &m.wave {
                let per_px = m.len / (r - l).max(1.0) as f64;
                let bars = Path::new(|p| {
                    let mut x = l.max(0.0);
                    while x < r.min(b.width) {
                        let (t0, t1) = (m.start + (x - l) as f64 * per_px, m.start + (x + 2.0 - l) as f64 * per_px);
                        let (i0, i1) = ((t0 / music::WAVE_STEP) as usize, ((t1 / music::WAVE_STEP) as usize).max((t0 / music::WAVE_STEP) as usize + 1));
                        let peak = w.peaks.get(i0..i1.min(w.peaks.len())).map_or(0.0, |s| s.iter().fold(0.0f32, |a, &v| a.max(v)));
                        let bh = (peak * (h - 4.0)).max(1.0);
                        p.rectangle(Point::new(x, top + h / 2.0 - bh / 2.0), Size::new(1.5, bh));
                        x += 2.0;
                    }
                });
                frame.fill(&bars, palette.background.base.text.scale_alpha(0.7));
            }
            if selected {
                frame.stroke(&block, Stroke::default().with_width(2.0).with_color(palette.primary.strong.color));
            }
            for x in [l + 4.0, r - 4.0] {
                frame.fill(&Path::rounded_rectangle(Point::new(x - 1.0, top + 4.0), Size::new(2.0, h - 8.0), 1.0.into()), iced::Color::from_rgba(1.0, 1.0, 1.0, 0.8));
            }
            // On a dark tag, as on the cuts, so it reads over the waveform.
            if r - l > m.label.chars().count() as f32 * 6.0 + 28.0 {
                let tag = Path::rounded_rectangle(Point::new(l + 9.0, top + 2.5), Size::new(m.label.chars().count() as f32 * 5.5 + 8.0, h - 5.0), 3.0.into());
                frame.fill(&tag, iced::Color::from_rgba(0.0, 0.0, 0.0, 0.6));
                frame.fill_text(Text {
                    content: m.label.clone(),
                    position: Point::new(l + 13.0, top + h / 2.0),
                    color: iced::Color::WHITE,
                    size: 11.0.into(),
                    align_y: Vertical::Center,
                    ..Default::default()
                });
            }
        }
        // Overlaps are crossfades: an X across each, as editors draw them.
        for (i, a) in self.music.iter().enumerate() {
            for c in &self.music[i + 1..] {
                let (t0, t1) = (a.from.max(c.from), (a.from + a.len).min(c.from + c.len));
                if t1 - t0 <= 1e-6 {
                    continue;
                }
                let (l, r) = (self.x(t0, b), self.x(t1, b));
                frame.fill(&Path::rectangle(Point::new(l, top), Size::new(r - l, h)), iced::Color::from_rgba(0.0, 0.0, 0.0, 0.35));
                let x = Path::new(|p| {
                    p.move_to(Point::new(l, top + h));
                    p.line_to(Point::new(r, top));
                    p.move_to(Point::new(l, top));
                    p.line_to(Point::new(r, top + h));
                });
                frame.stroke(&x, Stroke::default().with_width(1.5).with_color(iced::Color::from_rgba(1.0, 1.0, 1.0, 0.85)));
            }
        }
        // Past the video's end the music fades out in the export: dim it,
        // and mark the end.
        if self.duration > self.video + 1e-6 {
            let x = self.x(self.video, b);
            let to = self.x(self.duration, b);
            frame.fill(&Path::rectangle(Point::new(x, top), Size::new((to - x).max(0.0), h)), iced::Color::from_rgba(0.0, 0.0, 0.0, 0.55));
            frame.fill(&Path::rectangle(Point::new(x - 1.0, MUSIC_TOP), Size::new(2.0, MUSIC)), palette.background.base.text.scale_alpha(0.8));
        }
    }

    /// A cut block's left and right edges.
    fn block_x(&self, c: &Block, b: Rectangle) -> (f32, f32) {
        (self.x(c.from, b) + GAP / 2.0, self.x(c.from + c.len, b) - GAP / 2.0)
    }
}

struct Pictures(Arc<Timeline>);

impl canvas::Program<Message> for Pictures {
    type State = ();

    fn draw(&self, _: &(), renderer: &Renderer, theme: &Theme, bounds: Rectangle, _: mouse::Cursor) -> Vec<Geometry> {
        let palette = theme.extended_palette();
        let mut frame = Frame::new(renderer, bounds.size());
        for (i, c) in self.0.blocks.iter().enumerate() {
            let (l, r) = self.0.block_x(c, bounds);
            let block = Path::rounded_rectangle(Point::new(l, 0.0), Size::new((r - l).max(1.0), CUTS), 4.0.into());
            frame.fill(&block, if Some(i) == self.0.selected { palette.primary.weak.color } else { palette.background.strong.color });
            // Thumbnails side by side, each showing the moment at its middle.
            let (th, inset) = (CUTS - 4.0, 2.0);
            let tw = th * thumbs::SIZE.0 as f32 / thumbs::SIZE.1 as f32;
            if c.thumbs.is_empty() || r - l <= 2.0 * inset {
                continue;
            }
            let clip = Rectangle::new(Point::new(l + inset, inset), Size::new(r - l - 2.0 * inset, th));
            let per_px = c.len / (r - l) as f64;
            frame.with_clip(clip, |f| {
                // From the first tile in sight: zoomed in, a block can be
                // far wider than the widget.
                let mut x = clip.x + ((-clip.x) / tw).floor().max(0.0) * tw;
                while x < (clip.x + clip.width).min(bounds.width) {
                    let t = c.start + (x + tw / 2.0 - l) as f64 * per_px;
                    if let Some(h) = c.thumb_at(t) {
                        f.draw_image(Rectangle::new(Point::new(x, inset), Size::new(tw, th)), canvas::Image::new(h.clone()));
                    }
                    x += tw;
                }
            });
        }
        vec![frame.into_geometry()]
    }
}

struct Marks(Arc<Timeline>);

impl std::ops::Deref for Marks {
    type Target = Timeline;

    fn deref(&self) -> &Timeline {
        &self.0
    }
}

impl canvas::Program<Message> for Marks {
    type State = State;

    fn update(&self, state: &mut State, event: &iced::Event, bounds: Rectangle, cursor: mouse::Cursor) -> Option<canvas::Action<Message>> {
        use iced::Event::{Keyboard, Mouse};
        let drag = &mut state.drag;
        match event {
            Keyboard(iced::keyboard::Event::ModifiersChanged(m)) => {
                state.shift = m.shift();
                None
            }
            Mouse(mouse::Event::WheelScrolled { delta }) => {
                let p = cursor.position_in(bounds)?;
                let (dx, dy) = match *delta {
                    mouse::ScrollDelta::Lines { x, y } => (x, y),
                    mouse::ScrollDelta::Pixels { x, y } => (x / 40.0, y / 40.0),
                };
                let (pan, zoom) = if state.shift { (dx + dy, 0.0) } else { (dx, dy) };
                let message = if zoom != 0.0 {
                    Message::TimelineZoom(1.25f64.powf(zoom as f64), Some(self.t(p.x, bounds)))
                } else if pan != 0.0 {
                    Message::TimelinePan(self.start - pan as f64 * self.span * 0.1)
                } else {
                    return None;
                };
                Some(canvas::Action::publish(message).and_capture())
            }
            Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let p = cursor.position_in(bounds)?;
                let message = if p.y >= HEIGHT - SCROLL {
                    if !self.zoomed() {
                        return None;
                    }
                    // On the thumb, drag it; elsewhere on the bar, centre it there first.
                    let (l, r) = self.scroll_thumb(bounds);
                    let per_px = self.duration / Timeline::width(bounds) as f64;
                    let start = if p.x >= l && p.x <= r { self.start } else { self.start + (p.x - (l + r) / 2.0) as f64 * per_px };
                    *drag = Drag::Scroll { x0: p.x, start };
                    Message::TimelinePan(start)
                } else if p.y < CUTS {
                    let cut = self.block_at(p.x, bounds)?;
                    let c = &self.blocks[cut];
                    match self.edge_at(cut, p.x, bounds) {
                        Some(start) => {
                            let per_px = self.span / Timeline::width(bounds) as f64;
                            *drag = Drag::Trim { cut, start, x0: p.x, from: if start { c.start } else { c.start + c.len }, per_px };
                        }
                        None => *drag = Drag::Move { cut, x0: p.x, moved: false, to: cut },
                    }
                    Message::Select(cut)
                } else if p.y >= MUSIC_TOP {
                    let clip = self.music_at(p.x, bounds)?;
                    let m = &self.music[clip];
                    let per_px = self.span / Timeline::width(bounds) as f64;
                    *drag = match self.music_edge_at(clip, p.x, bounds) {
                        Some(start) => Drag::MusicTrim { clip, start, x0: p.x, from: if start { m.start } else { m.start + m.len }, per_px },
                        None => Drag::MusicMove { clip, x0: p.x, at: m.from, per_px },
                    };
                    Message::SelectMusic(clip)
                } else if let Some((cut, key)) = self.key_at(p.x, bounds) {
                    *drag = Drag::Key { cut, key };
                    Message::KeyJump(cut, key)
                } else {
                    *drag = Drag::Playhead;
                    Message::Scrub(self.t(p.x, bounds))
                };
                Some(canvas::Action::publish(message).and_capture())
            }
            Mouse(mouse::Event::CursorMoved { position }) => {
                let x = position.x - bounds.x;
                let message = match drag {
                    Drag::None => return None,
                    Drag::Playhead => Message::Scrub(self.t(x, bounds)),
                    Drag::Key { cut, key } => {
                        let c = &self.blocks[*cut];
                        let edit_t = self.t(x, bounds).clamp(c.from, c.from + c.len);
                        Message::KeyMove(*cut, *key, c.start + edit_t - c.from)
                    }
                    Drag::Trim { cut, start, x0, from, per_px } => Message::Trim(*cut, *start, *from + (x - *x0) as f64 * *per_px),
                    Drag::Move { x0, moved, to, .. } => {
                        *moved |= (x - *x0).abs() > DRAG_FROM;
                        *to = self.drop_at(x, bounds);
                        return Some(canvas::Action::request_redraw().and_capture());
                    }
                    Drag::Scroll { x0, start } => Message::TimelinePan(*start + (x - *x0) as f64 * self.duration / Timeline::width(bounds) as f64),
                    Drag::MusicMove { clip, x0, at, per_px } => {
                        let at = *at + (x - *x0) as f64 * *per_px;
                        let len = self.music[*clip].len;
                        Message::MusicMove(*clip, at + self.snap(*clip, &[at, at + len], *per_px), state.shift)
                    }
                    Drag::MusicTrim { clip, start, x0, from, per_px } => {
                        // Song time to edit time is fixed while trimming:
                        // either edge moves, the song stays put.
                        let m = &self.music[*clip];
                        let t = *from + (x - *x0) as f64 * *per_px;
                        let to_edit = m.from - m.start;
                        Message::MusicTrim(*clip, *start, t + self.snap(*clip, &[t + to_edit], *per_px), state.shift)
                    }
                };
                Some(canvas::Action::publish(message).and_capture())
            }
            Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => match std::mem::take(drag) {
                Drag::None => None,
                Drag::Move { cut, moved: true, to, .. } => Some(canvas::Action::publish(Message::MoveCut(cut, to)).and_capture()),
                Drag::Move { .. } | Drag::Scroll { .. } => Some(canvas::Action::request_redraw().and_capture()),
                Drag::Playhead | Drag::Key { .. } | Drag::Trim { .. } | Drag::MusicMove { .. } | Drag::MusicTrim { .. } => {
                    Some(canvas::Action::publish(Message::ScrubDone).and_capture())
                }
            },
            _ => None,
        }
    }

    fn draw(&self, state: &State, renderer: &Renderer, theme: &Theme, bounds: Rectangle, _: mouse::Cursor) -> Vec<Geometry> {
        let drag = &state.drag;
        let palette = theme.extended_palette();
        let mut frame = Frame::new(renderer, bounds.size());
        for (i, c) in self.blocks.iter().enumerate() {
            let (l, r) = self.block_x(c, bounds);
            let block = Path::rounded_rectangle(Point::new(l, 0.0), Size::new((r - l).max(1.0), CUTS), 4.0.into());
            let selected = Some(i) == self.selected;
            if selected {
                frame.stroke(&block, Stroke::default().with_width(2.0).with_color(palette.primary.strong.color));
            }
            // Trim handles, light on a dark edge so they show on any picture.
            for x in [l + 4.0, r - 4.0] {
                frame.fill(&Path::rounded_rectangle(Point::new(x - 2.0, 5.0), Size::new(4.0, CUTS - 10.0), 2.0.into()), iced::Color::from_rgba(0.0, 0.0, 0.0, 0.5));
                frame.fill(&Path::rounded_rectangle(Point::new(x - 1.0, 6.0), Size::new(2.0, CUTS - 12.0), 1.0.into()), iced::Color::from_rgba(1.0, 1.0, 1.0, 0.8));
            }
            // The longest label that fits (about 7 px a character), on a
            // dark tag, readable over any picture.
            if let Some(label) = c.labels.iter().find(|s| r - l > s.chars().count() as f32 * 7.0 + 24.0) {
                let tag = Path::rounded_rectangle(Point::new(l + 9.0, CUTS - 19.0), Size::new(label.chars().count() as f32 * 5.9 + 8.0, 15.0), 3.0.into());
                frame.fill(&tag, iced::Color::from_rgba(0.0, 0.0, 0.0, 0.6));
                frame.fill_text(Text {
                    content: label.clone(),
                    position: Point::new(l + 13.0, CUTS - 11.5),
                    color: iced::Color::WHITE,
                    size: 12.0.into(),
                    align_y: Vertical::Center,
                    ..Default::default()
                });
            }
        }
        if let Drag::Move { moved: true, to, .. } = drag {
            let t = self.blocks.get(*to).map_or(self.duration, |c| c.from);
            let x = self.x(t, bounds);
            frame.fill(&Path::rectangle(Point::new(x - 1.5, 0.0), Size::new(3.0, CUTS)), palette.primary.strong.color);
        }

        self.draw_music(&mut frame, theme, bounds);
        let mid = CUTS + SEEK / 2.0;
        let (x0, x1) = (self.x(0.0, bounds), self.x(self.duration, bounds));
        let bar = |from: f32, to: f32| Path::rounded_rectangle(Point::new(from, mid - BAR / 2.0), Size::new((to - from).max(0.0), BAR), (BAR / 2.0).into());
        frame.fill(&bar(x0, x1), palette.background.strong.color);
        let at = self.x(self.position, bounds);
        frame.fill(&bar(x0, at), palette.primary.base.color);
        // Where one cut ends and the next begins.
        for c in self.blocks.iter().skip(1) {
            let x = self.x(c.from, bounds);
            frame.fill(&Path::rectangle(Point::new(x - 1.0, mid - BAR), Size::new(2.0, 2.0 * BAR)), palette.background.base.color);
        }
        for (ci, c) in self.blocks.iter().enumerate() {
            for &(ki, t, ease, follows) in &c.keys {
                let x = self.x(c.from + t - c.start, bounds);
                // Smooth is a diamond, linear a circle, hold a square.
                let mark = match ease {
                    Ease::Smooth => Path::new(|p| {
                        p.move_to(Point::new(x, mid - DIAMOND));
                        p.line_to(Point::new(x + DIAMOND, mid));
                        p.line_to(Point::new(x, mid + DIAMOND));
                        p.line_to(Point::new(x - DIAMOND, mid));
                        p.close();
                    }),
                    Ease::Linear => Path::circle(Point::new(x, mid), DIAMOND * 0.8),
                    Ease::Hold => Path::rectangle(Point::new(x - DIAMOND * 0.7, mid - DIAMOND * 0.7), Size::new(DIAMOND * 1.4, DIAMOND * 1.4)),
                };
                let fill = if Some((ci, ki)) == self.current { palette.primary.strong.color } else { palette.background.base.text };
                frame.fill(&mark, fill);
                frame.stroke(&mark, Stroke::default().with_width(1.5).with_color(palette.background.base.color));
                // A keyframe that follows the stick wears a ring.
                if follows {
                    frame.stroke(&Path::circle(Point::new(x, mid), DIAMOND * 1.7), Stroke::default().with_width(1.5).with_color(fill));
                }
            }
        }
        frame.fill(&Path::rectangle(Point::new(at - 1.0, 0.0), Size::new(2.0, CUTS)), palette.primary.strong.color);
        frame.fill(&Path::rectangle(Point::new(at - 1.0, MUSIC_TOP), Size::new(2.0, MUSIC)), palette.primary.strong.color);
        frame.fill(&Path::circle(Point::new(at, mid), BAR), palette.primary.strong.color);
        if self.zoomed() {
            let y = HEIGHT - SCROLL / 2.0 - SCROLL_BAR / 2.0;
            let pill = |l: f32, r: f32| Path::rounded_rectangle(Point::new(l, y), Size::new((r - l).max(SCROLL_BAR), SCROLL_BAR), (SCROLL_BAR / 2.0).into());
            frame.fill(&pill(PICK, PICK + Timeline::width(bounds)), palette.background.strong.color);
            let (l, r) = self.scroll_thumb(bounds);
            let active = matches!(drag, Drag::Scroll { .. });
            frame.fill(&pill(l, r), if active { palette.primary.strong.color } else { palette.background.base.text.scale_alpha(0.5) });
        }
        vec![frame.into_geometry()]
    }

    fn mouse_interaction(&self, state: &State, bounds: Rectangle, cursor: mouse::Cursor) -> mouse::Interaction {
        match state.drag {
            Drag::Trim { .. } | Drag::MusicTrim { .. } => mouse::Interaction::ResizingHorizontally,
            Drag::None => {
                let Some(p) = cursor.position_in(bounds) else { return mouse::Interaction::default() };
                if p.y >= HEIGHT - SCROLL {
                    return if self.zoomed() { mouse::Interaction::Grab } else { mouse::Interaction::default() };
                }
                if p.y >= MUSIC_TOP {
                    return match self.music_at(p.x, bounds) {
                        Some(i) if self.music_edge_at(i, p.x, bounds).is_some() => mouse::Interaction::ResizingHorizontally,
                        Some(_) => mouse::Interaction::Grab,
                        None => mouse::Interaction::default(),
                    };
                }
                if p.y >= CUTS {
                    return if self.key_at(p.x, bounds).is_some() { mouse::Interaction::Grab } else { mouse::Interaction::Pointer };
                }
                match self.block_at(p.x, bounds) {
                    Some(i) if self.edge_at(i, p.x, bounds).is_some() => mouse::Interaction::ResizingHorizontally,
                    Some(_) => mouse::Interaction::Grab,
                    None => mouse::Interaction::default(),
                }
            }
            _ => mouse::Interaction::Grabbing,
        }
    }
}
