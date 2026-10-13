//! The audio meter: the mix's level as it plays (`audio::levels`, what the
//! clips' sound and the music send at their levels, as the export mixes
//! them), peak and RMS per channel, with each peak held a moment and the
//! loudest since play started, red from 0 dBFS, so a mix going over shows
//! while editing.

use super::audio;
use iced::widget::canvas::{self, Frame, Geometry, Path};
use iced::{Color, Point, Rectangle, Renderer, Size, Theme, mouse};
use std::time::Instant;

/// The scale, dBFS.
const FLOOR: f32 = -48.0;
const TOP: f32 = 6.0;
/// How far back the bars look, seconds: about a frame or two of the UI.
const WINDOW: f64 = 0.05;
/// The bars fall at this many dB a second, as meters do, so peaks can be read.
const FALL: f32 = 24.0;
/// How long a peak's mark stays, seconds.
const HOLD: f32 = 1.5;

pub const WIDTH: f32 = 90.0;
pub const HEIGHT: f32 = 14.0;

pub struct Meter {
    /// Shown, dBFS per channel.
    peak: [f32; 2],
    rms: [f32; 2],
    /// Held peak marks and when each was set.
    held: [(f32, Option<Instant>); 2],
    /// The loudest peak since play started, dBFS.
    pub loudest: Option<f32>,
    last: Option<Instant>,
    playing: bool,
}

impl Default for Meter {
    fn default() -> Self {
        Meter { peak: [FLOOR; 2], rms: [FLOOR; 2], held: [(FLOOR, None); 2], loudest: None, last: None, playing: false }
    }
}

fn db(x: f32) -> f32 {
    if x > 0.0 { (20.0 * x.log10()).max(FLOOR) } else { FLOOR }
}

impl Meter {
    /// Reads the mix while it plays; stopped, the bars drop and the loudest
    /// stays for reading until play starts again.
    pub fn follow(&mut self, playing: bool) {
        let now = Instant::now();
        let dt = self.last.map_or(0.0, |l| (now - l).as_secs_f32());
        self.last = Some(now);
        if playing && !self.playing {
            self.loudest = None;
        }
        self.playing = playing;
        if !playing {
            *self = Meter { loudest: self.loudest, ..Meter::default() };
            return;
        }
        let (peak, rms) = audio::levels(WINDOW);
        for c in 0..2 {
            let p = db(peak[c]);
            self.peak[c] = p.max(self.peak[c] - FALL * dt);
            self.rms[c] = db(rms[c]).max(self.rms[c] - FALL * dt);
            let (h, at) = self.held[c];
            if p >= h || at.is_none_or(|a| (now - a).as_secs_f32() > HOLD) {
                self.held[c] = (p, Some(now));
            }
            if p > FLOOR {
                self.loudest = Some(self.loudest.map_or(p, |l| l.max(p)));
            }
        }
    }

    pub fn view(&self) -> Bars {
        Bars { peak: self.peak, rms: self.rms, held: self.held.map(|(h, at)| at.map(|_| h)) }
    }
}

/// What the canvas draws, copied out of the app.
pub struct Bars {
    peak: [f32; 2],
    rms: [f32; 2],
    held: [Option<f32>; 2],
}

fn x(db: f32, width: f32) -> f32 {
    ((db - FLOOR) / (TOP - FLOOR)).clamp(0.0, 1.0) * width
}

/// Green, then yellow from −6 dB, red from 0 dBFS.
fn colour(db: f32) -> Color {
    if db >= 0.0 {
        Color::from_rgb8(0xe5, 0x3e, 0x3e)
    } else if db >= -6.0 {
        Color::from_rgb8(0xe8, 0xc3, 0x3a)
    } else {
        Color::from_rgb8(0x4c, 0xc2, 0x6a)
    }
}

impl<M> canvas::Program<M> for Bars {
    type State = ();

    fn draw(&self, _state: &(), renderer: &Renderer, theme: &Theme, bounds: Rectangle, _cursor: mouse::Cursor) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let w = bounds.width;
        let lane = (bounds.height - 2.0) / 2.0;
        let back = theme.extended_palette().background.weak.color;
        for c in 0..2 {
            let y = c as f32 * (lane + 2.0);
            frame.fill_rectangle(Point::new(0.0, y), Size::new(w, lane), back);
            // The peak, faint, over the RMS, solid; each coloured by its
            // zone, so the red shows only past 0 dBFS.
            for (level, alpha) in [(self.peak[c], 0.45), (self.rms[c], 1.0)] {
                for (from, to) in [(FLOOR, -6.0), (-6.0, 0.0), (0.0, TOP)] {
                    let (a, b) = (x(from, w), x(level.min(to), w));
                    if b > a {
                        frame.fill_rectangle(Point::new(a, y), Size::new(b - a, lane), Color { a: alpha, ..colour(from) });
                    }
                }
            }
            if let Some(h) = self.held[c].filter(|&h| h > FLOOR) {
                frame.fill_rectangle(Point::new(x(h, w) - 1.0, y), Size::new(2.0, lane), colour(h));
            }
        }
        // 0 dBFS.
        let zero = x(0.0, w);
        frame.stroke(
            &Path::line(Point::new(zero, 0.0), Point::new(zero, bounds.height)),
            canvas::Stroke::default().with_width(1.0).with_color(Color { a: 0.6, ..theme.palette().text }),
        );
        vec![frame.into_geometry()]
    }
}
