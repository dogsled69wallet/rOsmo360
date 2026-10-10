//! The timeline under the view. The top row holds the edit's cuts as
//! blocks showing thumbnails of their footage: click one to select it, drag
//! it to move it, drag its edges to trim it. Below is the seek bar with each
//! cut's keyframes as diamonds (shaped by their ease): click the bar to seek,
//! click a diamond to jump to it, drag one to retime it.
//!
//! It shows a window of the edit: the wheel zooms around the pointer, Shift
//! or a sideways wheel pans, and when zoomed in a scrollbar at the bottom
//! shows and moves the window.

use super::{Message, thumbs};
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
/// The scrollbar's row at the bottom, and the bar's thickness.
const SCROLL: f32 = 8.0;
const SCROLL_BAR: f32 = 5.0;
pub const HEIGHT: f32 = 76.0;
/// A press on a cut that moves less than this is a click, not a drag.
const DRAG_FROM: f32 = 4.0;

/// One cut as the timeline shows it.
pub struct Block {
    pub label: String,
    /// Where it starts on the timeline, and how long it lasts.
    pub from: f64,
    pub len: f64,
    /// Its in point in clip time.
    pub start: f64,
    /// Its keyframes within the cut: index into the cut's keyframes, clip
    /// time, and how it moves on to the next (drawn as its shape).
    pub keys: Vec<(usize, f64, Ease)>,
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

pub struct Timeline {
    pub blocks: Vec<Block>,
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
            for &(ki, t, _) in &c.keys {
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
                };
                Some(canvas::Action::publish(message).and_capture())
            }
            Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => match std::mem::take(drag) {
                Drag::None => None,
                Drag::Move { cut, moved: true, to, .. } => Some(canvas::Action::publish(Message::MoveCut(cut, to)).and_capture()),
                Drag::Move { .. } | Drag::Scroll { .. } => Some(canvas::Action::request_redraw().and_capture()),
                Drag::Playhead | Drag::Key { .. } | Drag::Trim { .. } => Some(canvas::Action::publish(Message::ScrubDone).and_capture()),
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
            // Labels only where they fit (about 7 px a character), on a
            // dark tag, readable over any picture.
            if r - l > c.label.len() as f32 * 7.0 + 24.0 {
                let tag = Path::rounded_rectangle(Point::new(l + 9.0, CUTS - 19.0), Size::new(c.label.len() as f32 * 5.9 + 8.0, 15.0), 3.0.into());
                frame.fill(&tag, iced::Color::from_rgba(0.0, 0.0, 0.0, 0.6));
                frame.fill_text(Text {
                    content: c.label.clone(),
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

        let mid = CUTS + (bounds.height - CUTS - SCROLL) / 2.0;
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
            for &(ki, t, ease) in &c.keys {
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
            }
        }
        frame.fill(&Path::rectangle(Point::new(at - 1.0, 0.0), Size::new(2.0, CUTS)), palette.primary.strong.color);
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
            Drag::Trim { .. } => mouse::Interaction::ResizingHorizontally,
            Drag::None => {
                let Some(p) = cursor.position_in(bounds) else { return mouse::Interaction::default() };
                if p.y >= HEIGHT - SCROLL {
                    return if self.zoomed() { mouse::Interaction::Grab } else { mouse::Interaction::default() };
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
