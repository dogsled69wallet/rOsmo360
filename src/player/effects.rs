//! The Effects panel: the playing cut's colour effects at the playhead as
//! sliders, and the edit's fades.
//!
//! Moving a slider sets the effect keyframe at the playhead, making one
//! there if there isn't (holding the look there so far, so nothing else
//! moves): a cut's single keyframe is its look throughout, and more of them
//! animate it.

use super::{App, Message, icon_button, tip};
use crate::grade::{EffectKey, Grade, grade_at};
use crate::view::Ease;
use iced::widget::{button, column, container, pick_list, row, rule, scrollable, slider, text, tooltip};
use iced::{Alignment, Element, Length};

#[derive(Debug, Clone)]
pub enum EffectMsg {
    /// Open or close the panel.
    Toggle,
    /// Effect `i` (as in `PARAMS`) moved to this value.
    Set(usize, f64),
    /// A slider let go: the next move is a change of its own for undo.
    Done,
    /// A keyframe at the playhead holding the effects there.
    Key,
    DeleteKey,
    Ease(Ease),
    /// Effect `i` back to none here, or the cut's effects all gone.
    Reset(Option<usize>),
    /// The look at the playhead on every cut.
    ToAll,
    /// Draw a box over something grey or white to set the white balance.
    PickWhite,
    FadeIn(f64),
    FadeOut(f64),
    /// Jump to cut `cut`'s effect keyframe at clip time `t`.
    Jump(usize, f64),
}

/// Each effect: its name, range, and what it does. Untouched, each is
/// `grade::NEUTRAL`'s.
const PARAMS: [(&str, f64, f64, &str); 10] = [
    ("Exposure", -3.0, 3.0, "Brighter or darker, in stops, like the camera's EV"),
    ("Contrast", -1.0, 1.0, "More or less contrast, around mid-grey"),
    ("Highlights", -1.0, 1.0, "Pull back (−) or lift the bright parts, such as the sky"),
    ("Shadows", -1.0, 1.0, "Lift (+) or deepen the dark parts"),
    ("Temperature", -1.0, 1.0, "Cooler (−) or warmer"),
    ("Tint", -1.0, 1.0, "Greener (−) or more magenta"),
    ("Saturation", -1.0, 1.0, "Less or more colour; −1 is black and white"),
    ("Vibrance", -1.0, 1.0, "More colour mostly where there is little, sparing skin and sky"),
    ("Vignette", 0.0, 1.0, "Darker corners"),
    ("Opacity", 0.0, 1.0, "How much of the picture shows; on the main track it fades towards black"),
];

/// The panel's width.
const WIDTH: f32 = 300.0;

/// A slider's value as shown: stops for exposure, else a signed amount.
fn label(i: usize, v: f64) -> String {
    match i {
        0 => format!("{v:+.2} EV"),
        8 => format!("{v:.2}"),
        9 => format!("{:.0} %", v * 100.0),
        _ => format!("{v:+.2}"),
    }
}

impl App {
    /// The effect keyframe of cut `i` at clip time `t`, if there's one.
    fn effect_key_at(&self, i: usize, t: f64) -> Option<usize> {
        let tolerance = self.tolerance();
        self.edit.cuts.get(i)?.effects.iter().position(|k| (k.t - t).abs() <= tolerance)
    }

    /// Sets the playing cut's effects at the playhead with `change`: in the
    /// keyframe there, made from the look so far if there's none.
    fn change_effects(&mut self, change: impl FnOnce(&mut Grade)) {
        let Some((_, t)) = self.playing_cut() else { return };
        let (i, at) = (self.current, self.effect_key_at(self.current, t));
        let keys = &mut self.edit.cuts[i].effects;
        let k = match at {
            Some(k) => k,
            None => {
                let k = keys.partition_point(|k| k.t <= t);
                keys.insert(k, EffectKey { t, grade: grade_at(keys, t), ease: Ease::Smooth });
                k
            }
        };
        change(&mut keys[k].grade);
    }

    pub(super) fn effect_message(&mut self, m: EffectMsg) {
        match m {
            EffectMsg::Toggle => self.effects_open = !self.effects_open,
            EffectMsg::Set(p, v) => {
                self.begin_drag();
                self.change_effects(|g| {
                    let mut values = g.values();
                    values[p] = v;
                    *g = Grade::from_values(values);
                });
            }
            EffectMsg::Done => self.dragging = false,
            EffectMsg::Key => {
                self.edit.checkpoint();
                self.change_effects(|_| {});
            }
            EffectMsg::DeleteKey => {
                let Some((_, t)) = self.playing_cut() else { return };
                if let Some(k) = self.effect_key_at(self.current, t) {
                    self.edit.checkpoint();
                    self.edit.cuts[self.current].effects.remove(k);
                }
            }
            EffectMsg::Ease(ease) => {
                let Some((_, t)) = self.playing_cut() else { return };
                if let Some(k) = self.effect_key_at(self.current, t) {
                    self.edit.checkpoint();
                    self.edit.cuts[self.current].effects[k].ease = ease;
                }
            }
            EffectMsg::Reset(Some(p)) => {
                self.edit.checkpoint();
                self.change_effects(|g| {
                    let mut values = g.values();
                    values[p] = crate::grade::NEUTRAL[p];
                    *g = Grade::from_values(values);
                });
            }
            EffectMsg::Reset(None) => {
                if let Some(cut) = self.edit.cuts.get(self.current)
                    && !cut.effects.is_empty()
                {
                    self.edit.checkpoint();
                    self.edit.cuts[self.current].effects.clear();
                }
            }
            EffectMsg::ToAll => {
                let Some((_, t)) = self.playing_cut() else { return };
                let grade = grade_at(&self.edit.cuts[self.current].effects, t);
                self.edit.checkpoint();
                let current = self.current;
                for (i, cut) in self.edit.cuts.iter_mut().enumerate() {
                    if i != current {
                        cut.effects = vec![EffectKey { t: cut.start, grade, ease: Ease::Smooth }];
                    }
                }
            }
            EffectMsg::PickWhite => {
                if self.picking.take().is_none() && self.clip.is_some() {
                    self.with_playback(super::Playback::pause);
                    self.picking = Some(None);
                    self.picking_white = true;
                    self.save_status = Some("Draw a box over something that should be grey or white (Esc cancels)".to_owned());
                } else {
                    self.picking_white = false;
                    self.save_status = None;
                }
            }
            EffectMsg::FadeIn(v) => {
                self.begin_drag();
                self.edit.fades.fade_in = v;
            }
            EffectMsg::FadeOut(v) => {
                self.begin_drag();
                self.edit.fades.fade_out = v;
            }
            EffectMsg::Jump(cut, t) => {
                self.follow = true;
                self.selected = Some(cut);
                self.goto(cut, t, false);
            }
        }
    }

    /// Neutralises the playing cut's colour cast from `rgb`, the median of
    /// something meant to be grey or white.
    pub(super) fn white_balance(&mut self, rgb: [f64; 3]) {
        let (temperature, tint) = crate::grade::white_balance(rgb);
        self.edit.checkpoint();
        self.change_effects(|g| {
            g.temperature = temperature;
            g.tint = tint;
        });
    }

    pub(super) fn effects_panel(&self) -> Element<'_, Message> {
        let m = |e: EffectMsg| Message::Effect(e);
        let at = self.playing_cut().map(|(c, t)| (self.current, c, t));
        let grade = at.map(|(_, c, t)| grade_at(&c.effects, t)).unwrap_or_default();
        let key = at.and_then(|(i, _, t)| self.effect_key_at(i, t));
        let enabled = at.is_some();
        let mut sliders = column![].spacing(6);
        for (p, (name, lo, hi, hint)) in PARAMS.iter().enumerate() {
            let v = grade.values()[p];
            let reset = button(text("↺").size(13)).padding([0, 5]).style(button::text).on_press_maybe((enabled && v != crate::grade::NEUTRAL[p]).then(|| m(EffectMsg::Reset(Some(p)))));
            let control = slider(*lo..=*hi, v, move |v| Message::Effect(EffectMsg::Set(p, v))).step(0.01).on_release(m(EffectMsg::Done));
            sliders = sliders.push(tip(
                column![row![text(*name).width(Length::Fill), text(label(p, v)).size(13), reset].spacing(4).align_y(Alignment::Center), control].spacing(2),
                text(*hint),
                tooltip::Position::Left,
            ));
        }
        let cut_name = at.map_or_else(|| "No cut".to_owned(), |(i, c, _)| format!("Cut {} · {}", i + 1, super::clip_name(&c.input)));
        let keys = row![
            tip(icon_button(super::KEY_ADD, enabled.then(|| m(EffectMsg::Key))), text("An effect keyframe here, holding the effects as they are; the sliders also make one as they move"), tooltip::Position::Bottom),
            tip(icon_button(super::KEY_DELETE, key.map(|_| m(EffectMsg::DeleteKey))), text("Delete the effect keyframe here"), tooltip::Position::Bottom),
            tip(
                pick_list(&Ease::ALL[..], key.and_then(|k| at.map(|(_, c, _)| c.effects[k].ease)), |e| Message::Effect(EffectMsg::Ease(e))).placeholder("Ease").width(96),
                text("How the effects move on from this keyframe to the next"),
                tooltip::Position::Bottom,
            ),
        ]
        .spacing(6)
        .align_y(Alignment::Center);
        let tools = row![
            tip(icon_button(super::PICK, enabled.then(|| m(EffectMsg::PickWhite))), text("White balance: draw a box over something that should be grey or white (snow, a cloud)"), tooltip::Position::Bottom),
            tip(icon_button(super::TO_ALL_CUTS, enabled.then(|| m(EffectMsg::ToAll))), text("To all cuts: give every other cut this look (as at the playhead), replacing their effects"), tooltip::Position::Bottom),
            tip(icon_button(super::RESET_VIEW, at.filter(|(_, c, _)| !c.effects.is_empty()).map(|_| m(EffectMsg::Reset(None)))), text("Reset: remove this cut's effects"), tooltip::Position::Bottom),
        ]
        .spacing(6)
        .align_y(Alignment::Center);
        let fades = &self.edit.fades;
        let fade = |name: &'static str, v: f64, f: fn(f64) -> EffectMsg, hint: &'static str| {
            tip(
                column![
                    row![text(name).width(Length::Fill), text(if v > 0.0 { format!("{v:.1} s") } else { "none".to_owned() }).size(13)],
                    slider(0.0..=5.0, v, move |v| Message::Effect(f(v))).step(0.1).on_release(Message::Effect(EffectMsg::Done)),
                ]
                .spacing(2),
                text(hint),
                tooltip::Position::Left,
            )
        };
        let body = column![
            text("Effects").size(18),
            text(cut_name).size(13),
            sliders,
            keys,
            tools,
            rule::horizontal(1),
            text("Edit").size(16),
            fade("Fade in", fades.fade_in, EffectMsg::FadeIn, "From black at the start of the edit, the sound with it"),
            fade("Fade out", fades.fade_out, EffectMsg::FadeOut, "To black at the end of the edit, the sound with it"),
        ]
        .spacing(10)
        .padding(12);
        container(scrollable(body)).width(WIDTH).height(Length::Fill).into()
    }
}
