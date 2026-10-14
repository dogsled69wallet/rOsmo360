//! The Audio panel: what you hear (mute and the listening volume) and the
//! mix the export makes (the clips' sound and the music), docked at the
//! right of the view like the Effects panel, opened by the speaker button.

use super::{App, Message, SPEAKER, SPEAKER_MUTED, db_label, icon_button, tip};
use iced::widget::{column, container, row, rule, scrollable, slider, text, tooltip};
use iced::{Alignment, Element, Length};

const WIDTH: f32 = 300.0;

/// Small dimmed text under a control, saying what it does.
fn note<'a>(s: &'a str) -> Element<'a, Message> {
    text(s).size(13).style(text::secondary).into()
}

impl App {
    pub(super) fn audio_panel(&self) -> Element<'_, Message> {
        let level = |name: &'static str, db: f64, set: fn(f64) -> Message, about: &'static str| {
            column![
                row![text(name).width(Length::Fill), text(db_label(db)).size(13)],
                slider(crate::project::OFF_DB..=6.0, db, set).step(0.5).on_release(Message::LevelDone),
                note(about),
            ]
            .spacing(4)
        };
        let body = column![
            text("Audio").size(18),
            column![
                text("Listening"),
                row![
                    tip(icon_button(if self.muted { SPEAKER_MUTED } else { SPEAKER }, Some(Message::Mute)), text(if self.muted { "Unmute (M)" } else { "Mute (M)" }), tooltip::Position::Bottom),
                    slider(0.0..=1.0, self.volume(), Message::Volume).step(0.01f32),
                    text(format!("{:.0}%", self.volume() * 100.0)).size(13).width(40),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
                note("Only what you hear: the export and the meter don't change with it."),
            ]
            .spacing(6),
            rule::horizontal(1),
            text("Mix").size(16),
            level("Clip sound", self.edit.sound.volume, Message::SoundLevel, "The clips' own sound, in playback and the export."),
            level("Music", self.edit.music.volume, Message::MusicLevel, "The music track, in playback and the export."),
            note("To change the level within a cut or a song, Ctrl+click its line on the timeline for a volume point."),
        ]
        .spacing(14)
        .padding(12);
        container(scrollable(body)).width(WIDTH).height(Length::Fill).into()
    }
}
