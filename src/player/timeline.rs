//! The timeline under the view. The top row holds the edit's cuts as
//! blocks: click one to select it, drag it to move it, drag its edges to
//! trim it. The bottom row is the seek bar with each cut's keyframes as
//! diamonds (shaped by their ease): click the bar to seek, click a diamond to jump to it, drag one
//! to retime it.

use super::Message;
use crate::view::Ease;
use iced::alignment::Vertical;
use iced::mouse;
use iced::widget::canvas::{self, Frame, Geometry, Path, Stroke, Text};
use iced::{Point, Rectangle, Renderer, Size, Theme};

/// Diamond half-size, and how close a click must land to pick a diamond or
/// a cut's edge.
const DIAMOND: f32 = 6.0;
const PICK: f32 = 7.0;
const BAR: f32 = 8.0;
/// The cut row's height, and the gap between cut blocks.
const CUTS: f32 = 22.0;
const GAP: f32 = 2.0;
pub const HEIGHT: f32 = 50.0;
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
}

pub struct Timeline {
    pub blocks: Vec<Block>,
    pub duration: f64,
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
}

impl Timeline {
    fn width(b: Rectangle) -> f32 {
        (b.width - 2.0 * PICK).max(1.0)
    }

    /// Edit time to x, with room either side for diamonds at the ends.
    fn x(&self, t: f64, b: Rectangle) -> f32 {
        PICK + (t / self.duration.max(1e-9)).clamp(0.0, 1.0) as f32 * Self::width(b)
    }

    fn t(&self, x: f32, b: Rectangle) -> f64 {
        (((x - PICK) / Self::width(b)).clamp(0.0, 1.0) as f64) * self.duration
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

impl canvas::Program<Message> for Timeline {
    type State = Drag;

    fn update(&self, drag: &mut Drag, event: &iced::Event, bounds: Rectangle, cursor: mouse::Cursor) -> Option<canvas::Action<Message>> {
        use iced::Event::Mouse;
        match event {
            Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let p = cursor.position_in(bounds)?;
                let message = if p.y < CUTS {
                    let cut = self.block_at(p.x, bounds)?;
                    let c = &self.blocks[cut];
                    match self.edge_at(cut, p.x, bounds) {
                        Some(start) => {
                            let per_px = self.duration / Self::width(bounds) as f64;
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
                };
                Some(canvas::Action::publish(message).and_capture())
            }
            Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => match std::mem::take(drag) {
                Drag::None => None,
                Drag::Move { cut, moved: true, to, .. } => Some(canvas::Action::publish(Message::MoveCut(cut, to)).and_capture()),
                Drag::Move { .. } => Some(canvas::Action::request_redraw().and_capture()),
                Drag::Playhead | Drag::Key { .. } | Drag::Trim { .. } => Some(canvas::Action::publish(Message::ScrubDone).and_capture()),
            },
            _ => None,
        }
    }

    fn draw(&self, drag: &Drag, renderer: &Renderer, theme: &Theme, bounds: Rectangle, _: mouse::Cursor) -> Vec<Geometry> {
        let palette = theme.extended_palette();
        let mut frame = Frame::new(renderer, bounds.size());

        for (i, c) in self.blocks.iter().enumerate() {
            let (l, r) = (self.x(c.from, bounds) + GAP / 2.0, self.x(c.from + c.len, bounds) - GAP / 2.0);
            let block = Path::rounded_rectangle(Point::new(l, 0.0), Size::new((r - l).max(1.0), CUTS), 4.0.into());
            let selected = Some(i) == self.selected;
            frame.fill(&block, if selected { palette.primary.weak.color } else { palette.background.strong.color });
            if selected {
                frame.stroke(&block, Stroke::default().with_width(1.5).with_color(palette.primary.strong.color));
            }
            // Trim handles.
            for x in [l + 3.0, r - 3.0] {
                frame.fill(&Path::rounded_rectangle(Point::new(x - 1.0, 5.0), Size::new(2.0, CUTS - 10.0), 1.0.into()), palette.background.base.text.scale_alpha(0.5));
            }
            // Labels only where they fit (about 7 px a character).
            if r - l > c.label.len() as f32 * 7.0 + 16.0 {
                frame.fill_text(Text {
                    content: c.label.clone(),
                    position: Point::new(l + 8.0, CUTS / 2.0),
                    color: if selected { palette.primary.weak.text } else { palette.background.strong.text },
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

        let mid = CUTS + (bounds.height - CUTS) / 2.0;
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
        vec![frame.into_geometry()]
    }

    fn mouse_interaction(&self, drag: &Drag, bounds: Rectangle, cursor: mouse::Cursor) -> mouse::Interaction {
        match drag {
            Drag::Trim { .. } => mouse::Interaction::ResizingHorizontally,
            Drag::None => {
                let Some(p) = cursor.position_in(bounds) else { return mouse::Interaction::default() };
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
