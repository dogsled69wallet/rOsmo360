//! On-screen handles for overlays, a layer over the view while paused:
//! click an overlay where it shows to select it, drag it to move it, a corner to size it
//! (about its centre) and the knob above it to turn it (Shift snaps to 15°).
//! Each drag sets the motion keyframe at the playhead, as the Effects
//! panel's sliders do. Elsewhere the layer takes nothing, so the view under
//! it still looks around: iced's `stack` hides the pointer from the layers
//! below one that claims it.

use super::Message;
use super::effects::EffectMsg;
use crate::overlay::{ItemRef, Picture, Place, Quad};
use iced::widget::canvas::{self, Frame, Geometry, Path, Stroke};
use iced::{Color, Point, Rectangle, Renderer, Size, Theme, keyboard, mouse};
use std::sync::Arc;

/// A corner square's half size, and how near the pointer must be.
const CORNER: f32 = 5.0;
const REACH: f32 = 9.0;
/// The turning knob, above the item's top edge.
const KNOB_GAP: f32 = 26.0;
const KNOB: f32 = 6.0;
/// The centre snaps to the middle of the frame within this many pixels.
const SNAP: f64 = 6.0;

/// The overlays showing at the playhead, in drawing order (the topmost
/// last), with the selected one.
#[derive(Clone)]
pub struct Handles {
    pub items: Vec<(ItemRef, Place, Arc<Picture>)>,
    pub selected: Option<ItemRef>,
}

#[derive(Clone, Copy)]
enum Grip {
    Move,
    Size,
    Turn,
}

#[derive(Default)]
pub struct Drag {
    /// What's held, the place it started from and where the pointer was.
    held: Option<(Grip, Place, Point)>,
    shift: bool,
}

impl Handles {
    fn quad(&self, place: &Place, picture: &Picture, bounds: Rectangle) -> Quad {
        Quad::new(place, picture, (bounds.width.round() as usize, bounds.height.round() as usize))
    }

    fn selected(&self) -> Option<&(ItemRef, Place, Arc<Picture>)> {
        self.items.iter().find(|(r, ..)| Some(*r) == self.selected)
    }

    /// What's under `p` (in the view): a grip of the selected item, else
    /// the topmost item, to select and move.
    fn hit(&self, p: Point, bounds: Rectangle) -> Option<(ItemRef, Place, Grip)> {
        if let Some((r, place, picture)) = self.selected() {
            let q = self.quad(place, picture, bounds);
            if distance(p, knob(&q)) <= REACH {
                return Some((*r, *place, Grip::Turn));
            }
            if corners(&q).iter().any(|&c| distance(p, c) <= REACH) {
                return Some((*r, *place, Grip::Size));
            }
        }
        // The selected one by its whole box; the others only where they
        // show, or one sized to the frame would stop looking around.
        self.items.iter().rev().find_map(|(r, place, picture)| {
            let q = self.quad(place, picture, bounds);
            let near = [(0.0, 0.0), (-4.0, 0.0), (4.0, 0.0), (0.0, -4.0), (0.0, 4.0)];
            let over = if Some(*r) == self.selected {
                let [u, v] = q.to_item(p.x as f64, p.y as f64);
                u.abs() <= 1.0 && v.abs() <= 1.0
            } else {
                near.iter().any(|(dx, dy)| shows(picture, q.to_item(p.x as f64 + dx, p.y as f64 + dy)))
            };
            over.then_some((*r, *place, Grip::Move))
        })
    }
}

/// Whether `picture` is solid enough to grab at item point `[u, v]`.
fn shows(picture: &Picture, [u, v]: [f64; 2]) -> bool {
    if u.abs() > 1.0 || v.abs() > 1.0 {
        return false;
    }
    let x = (((u + 1.0) / 2.0 * picture.width as f64) as usize).min(picture.width.saturating_sub(1));
    let y = (((v + 1.0) / 2.0 * picture.height as f64) as usize).min(picture.height.saturating_sub(1));
    picture.rgba.get((y * picture.width + x) * 4 + 3).is_some_and(|&a| a > 24)
}

fn distance(a: Point, b: Point) -> f32 {
    ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt()
}

/// Item point (`u`, `v`), −1..1 each way, on the view.
fn on_view(q: &Quad, u: f64, v: f64) -> Point {
    let (s, c) = q.turn.sin_cos();
    let (x, y) = (u * q.half[0], v * q.half[1]);
    Point::new((q.centre[0] + c * x - s * y) as f32, (q.centre[1] + s * x + c * y) as f32)
}

fn corners(q: &Quad) -> [Point; 4] {
    [on_view(q, -1.0, -1.0), on_view(q, 1.0, -1.0), on_view(q, 1.0, 1.0), on_view(q, -1.0, 1.0)]
}

/// The turning knob, `KNOB_GAP` above the top edge's middle, turned with it.
fn knob(q: &Quad) -> Point {
    let (s, c) = q.turn.sin_cos();
    let up = q.half[1] + KNOB_GAP as f64;
    Point::new((q.centre[0] + s * up) as f32, (q.centre[1] - c * up) as f32)
}

fn place_msg(place: Place) -> Message {
    Message::Effect(EffectMsg::Drag(place))
}

impl canvas::Program<Message> for Handles {
    type State = Drag;

    fn update(&self, drag: &mut Drag, event: &iced::Event, bounds: Rectangle, cursor: mouse::Cursor) -> Option<canvas::Action<Message>> {
        let at = |p: Point| Point::new(p.x - bounds.x, p.y - bounds.y);
        match event {
            iced::Event::Keyboard(keyboard::Event::ModifiersChanged(m)) => {
                drag.shift = m.shift();
                None
            }
            iced::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let p = at(cursor.position_over(bounds)?);
                let (r, place, grip) = self.hit(p, bounds)?;
                drag.held = Some((grip, place, p));
                let action = if Some(r) == self.selected { canvas::Action::request_redraw() } else { canvas::Action::publish(Message::OverlaySelect(Some(r))) };
                Some(action.and_capture())
            }
            iced::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                drag.held.take()?;
                Some(canvas::Action::publish(Message::Effect(EffectMsg::Done)).and_capture())
            }
            iced::Event::Mouse(mouse::Event::CursorMoved { position }) => {
                let (grip, from, start) = drag.held?;
                let p = at(*position);
                let (w, h) = (bounds.width as f64, bounds.height as f64);
                let mut place = from;
                match grip {
                    Grip::Move => {
                        let (x, y) = (from.x * w + (p.x - start.x) as f64, from.y * h + (p.y - start.y) as f64);
                        // The middle of the frame is easy to hit.
                        place.x = if (x - w / 2.0).abs() <= SNAP { 0.5 } else { x / w };
                        place.y = if (y - h / 2.0).abs() <= SNAP { 0.5 } else { y / h };
                    }
                    Grip::Size => {
                        let c = Point::new((from.x * w) as f32, (from.y * h) as f32);
                        let k = distance(p, c) / distance(start, c).max(1.0);
                        place.size = (from.size * k as f64).max(0.01);
                    }
                    Grip::Turn => {
                        let c = Point::new((from.x * w) as f32, (from.y * h) as f32);
                        let angle = |q: Point| ((q.y - c.y) as f64).atan2((q.x - c.x) as f64).to_degrees();
                        let mut turn = from.rotation + angle(p) - angle(start);
                        turn = (turn + 180.0).rem_euclid(360.0) - 180.0;
                        if drag.shift {
                            turn = (turn / 15.0).round() * 15.0;
                        }
                        place.rotation = turn;
                    }
                }
                Some(canvas::Action::publish(place_msg(place)).and_capture())
            }
            _ => None,
        }
    }

    fn draw(&self, _: &Drag, renderer: &Renderer, _: &Theme, bounds: Rectangle, _: mouse::Cursor) -> Vec<Geometry> {
        let Some((_, place, picture)) = self.selected() else { return vec![] };
        let mut frame = Frame::new(renderer, bounds.size());
        let q = self.quad(place, picture, bounds);
        let [a, b, c, d] = corners(&q);
        let top = on_view(&q, 0.0, -1.0);
        let k = knob(&q);
        let outline = Path::new(|p| {
            p.move_to(a);
            p.line_to(b);
            p.line_to(c);
            p.line_to(d);
            p.close();
            p.move_to(top);
            p.line_to(k);
        });
        // Dark under light, so it shows on any picture.
        frame.stroke(&outline, Stroke::default().with_width(3.0).with_color(Color { a: 0.6, ..Color::BLACK }));
        frame.stroke(&outline, Stroke::default().with_width(1.0).with_color(Color::WHITE));
        for p in [a, b, c, d] {
            let square = Path::rectangle(Point::new(p.x - CORNER, p.y - CORNER), Size::new(2.0 * CORNER, 2.0 * CORNER));
            frame.fill(&square, Color::WHITE);
            frame.stroke(&square, Stroke::default().with_width(1.0).with_color(Color::BLACK));
        }
        let circle = Path::circle(k, KNOB);
        frame.fill(&circle, Color::WHITE);
        frame.stroke(&circle, Stroke::default().with_width(1.0).with_color(Color::BLACK));
        vec![frame.into_geometry()]
    }

    fn mouse_interaction(&self, drag: &Drag, bounds: Rectangle, cursor: mouse::Cursor) -> mouse::Interaction {
        let grip = match drag.held {
            Some((grip, ..)) => Some(grip),
            None => cursor.position_over(bounds).and_then(|p| self.hit(Point::new(p.x - bounds.x, p.y - bounds.y), bounds)).map(|(.., g)| g),
        };
        match grip {
            Some(Grip::Move) => mouse::Interaction::Move,
            Some(Grip::Size) => mouse::Interaction::ResizingDiagonallyDown,
            Some(Grip::Turn) => if drag.held.is_some() { mouse::Interaction::Grabbing } else { mouse::Interaction::Grab },
            // Nothing here: the view under it gets the pointer.
            None => mouse::Interaction::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_sit_on_the_item_as_drawn() {
        let q = Quad { centre: [300.0, 200.0], half: [80.0, 40.0], turn: 30f64.to_radians() };
        for (c, want) in corners(&q).iter().zip([[-1.0, -1.0], [1.0, -1.0], [1.0, 1.0], [-1.0, 1.0]]) {
            let got = q.to_item(c.x as f64, c.y as f64);
            assert!((got[0] - want[0]).abs() < 1e-4 && (got[1] - want[1]).abs() < 1e-4, "{got:?} {want:?}");
        }
        // The knob is straight up from the top edge, turned with the item.
        let k = q.to_item(knob(&q).x as f64, knob(&q).y as f64);
        assert!(k[0].abs() < 1e-4 && (k[1] + (40.0 + KNOB_GAP as f64) / 40.0).abs() < 1e-4, "{k:?}");
        let quarter = Quad { turn: 90f64.to_radians(), ..q };
        let k = knob(&quarter);
        assert!((k.x - (300.0 + 40.0 + KNOB_GAP)).abs() < 1e-3 && (k.y - 200.0).abs() < 1e-3, "{k:?}");
    }
}
