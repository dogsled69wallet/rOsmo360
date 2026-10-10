//! The timeline under the view: the clip as a bar to click or drag for
//! seeking, with its keyframes as diamonds. Click a diamond to jump to it,
//! drag one to retime it.

use super::Message;
use iced::mouse;
use iced::widget::canvas::{self, Frame, Geometry, Path, Stroke};
use iced::{Point, Rectangle, Renderer, Size, Theme};

/// Diamond half-size, and how close a click must land to pick one.
const DIAMOND: f32 = 6.0;
const PICK: f32 = 8.0;
const BAR: f32 = 8.0;

pub struct Timeline {
    pub duration: f64,
    pub position: f64,
    /// Keyframe times.
    pub keys: Vec<f64>,
    /// The keyframe at the playhead, drawn highlighted.
    pub current: Option<usize>,
}

#[derive(Default)]
pub enum Drag {
    #[default]
    None,
    Playhead,
    Key(usize),
}

impl Timeline {
    /// Pixels either side are left free so diamonds at the ends fit.
    fn x(&self, t: f64, b: Rectangle) -> f32 {
        let w = (b.width - 2.0 * PICK).max(1.0);
        PICK + (t / self.duration.max(1e-9)).clamp(0.0, 1.0) as f32 * w
    }

    fn t(&self, x: f32, b: Rectangle) -> f64 {
        let w = (b.width - 2.0 * PICK).max(1.0);
        (((x - PICK) / w).clamp(0.0, 1.0) as f64) * self.duration
    }

    fn key_under(&self, p: Point, b: Rectangle) -> Option<usize> {
        // The nearest wins where diamonds overlap.
        let d = |i: &usize| (self.x(self.keys[*i], b) - p.x).abs();
        (0..self.keys.len()).filter(|i| d(i) <= PICK).min_by(|a, b| d(a).total_cmp(&d(b)))
    }
}

impl canvas::Program<Message> for Timeline {
    type State = Drag;

    fn update(&self, drag: &mut Drag, event: &iced::Event, bounds: Rectangle, cursor: mouse::Cursor) -> Option<canvas::Action<Message>> {
        use iced::Event::Mouse;
        match event {
            Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let p = cursor.position_in(bounds)?;
                let message = match self.key_under(p, bounds) {
                    Some(i) => {
                        *drag = Drag::Key(i);
                        Message::KeyJump(i)
                    }
                    None => {
                        *drag = Drag::Playhead;
                        Message::Scrub(self.t(p.x, bounds))
                    }
                };
                Some(canvas::Action::publish(message).and_capture())
            }
            Mouse(mouse::Event::CursorMoved { position }) => {
                let t = self.t(position.x - bounds.x, bounds);
                let message = match drag {
                    Drag::None => return None,
                    Drag::Playhead => Message::Scrub(t),
                    Drag::Key(i) => Message::KeyMove(*i, t),
                };
                Some(canvas::Action::publish(message).and_capture())
            }
            Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => match std::mem::take(drag) {
                Drag::None => None,
                // Both scrub the picture while dragging.
                Drag::Playhead | Drag::Key(_) => Some(canvas::Action::publish(Message::ScrubDone).and_capture()),
            },
            _ => None,
        }
    }

    fn draw(&self, _: &Drag, renderer: &Renderer, theme: &Theme, bounds: Rectangle, _: mouse::Cursor) -> Vec<Geometry> {
        let palette = theme.extended_palette();
        let mut frame = Frame::new(renderer, bounds.size());
        let mid = bounds.height / 2.0;
        let (x0, x1) = (self.x(0.0, bounds), self.x(self.duration, bounds));
        let bar = |from: f32, to: f32| Path::rounded_rectangle(Point::new(from, mid - BAR / 2.0), Size::new((to - from).max(0.0), BAR), (BAR / 2.0).into());
        frame.fill(&bar(x0, x1), palette.background.strong.color);
        let at = self.x(self.position, bounds);
        frame.fill(&bar(x0, at), palette.primary.base.color);
        for (i, &t) in self.keys.iter().enumerate() {
            let x = self.x(t, bounds);
            let diamond = Path::new(|p| {
                p.move_to(Point::new(x, mid - DIAMOND));
                p.line_to(Point::new(x + DIAMOND, mid));
                p.line_to(Point::new(x, mid + DIAMOND));
                p.line_to(Point::new(x - DIAMOND, mid));
                p.close();
            });
            let fill = if Some(i) == self.current { palette.primary.strong.color } else { palette.background.base.text };
            frame.fill(&diamond, fill);
            frame.stroke(&diamond, Stroke::default().with_width(1.5).with_color(palette.background.base.color));
        }
        let head = Path::circle(Point::new(at, mid), BAR);
        frame.fill(&head, palette.primary.strong.color);
        vec![frame.into_geometry()]
    }

    fn mouse_interaction(&self, drag: &Drag, bounds: Rectangle, cursor: mouse::Cursor) -> mouse::Interaction {
        match drag {
            Drag::Key(_) | Drag::Playhead => mouse::Interaction::Grabbing,
            Drag::None => match cursor.position_in(bounds) {
                Some(p) if self.key_under(p, bounds).is_some() => mouse::Interaction::Grab,
                Some(_) => mouse::Interaction::Pointer,
                None => mouse::Interaction::default(),
            },
        }
    }
}
