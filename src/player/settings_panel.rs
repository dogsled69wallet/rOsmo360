//! The Settings panel: how the player behaves, kept between runs
//! (`settings.rs`), docked at the left of the view like the Info panel.

use super::{App, Message, tip};
use iced::widget::{button, column, container, pick_list, row, rule, scrollable, text, toggler, tooltip};
use iced::{Alignment, Element, Length, Theme};
use std::fmt;

const WIDTH: f32 = 300.0;

/// The colour theme chosen: the desktop's (following its light or dark
/// mode, or held to one), or one of iced's own by name.
#[derive(Clone, Debug, PartialEq)]
pub enum ThemeChoice {
    System,
    SystemLight,
    SystemDark,
    Named(String),
}

impl ThemeChoice {
    /// Every choice, the desktop's first.
    pub fn all() -> Vec<ThemeChoice> {
        [ThemeChoice::System, ThemeChoice::SystemLight, ThemeChoice::SystemDark].into_iter().chain(Theme::ALL.iter().map(|t| ThemeChoice::Named(t.to_string()))).collect()
    }

    /// Its name in the settings file.
    pub fn key(&self) -> String {
        match self {
            ThemeChoice::System => "system".to_owned(),
            ThemeChoice::SystemLight => "system-light".to_owned(),
            ThemeChoice::SystemDark => "system-dark".to_owned(),
            ThemeChoice::Named(name) => name.clone(),
        }
    }

    /// The choice the settings file names; an unknown name (a theme a
    /// later iced dropped) is the desktop's.
    pub fn from_key(key: &str) -> ThemeChoice {
        match key {
            "system-light" => ThemeChoice::SystemLight,
            "system-dark" => ThemeChoice::SystemDark,
            name if Theme::ALL.iter().any(|t| t.to_string() == name) => ThemeChoice::Named(name.to_owned()),
            _ => ThemeChoice::System,
        }
    }

    /// The theme to draw with.
    pub fn resolve(&self) -> Theme {
        match self {
            ThemeChoice::System => super::system_theme(None),
            ThemeChoice::SystemLight => super::system_theme(Some(false)),
            ThemeChoice::SystemDark => super::system_theme(Some(true)),
            ThemeChoice::Named(name) => Theme::ALL.iter().find(|t| t.to_string() == *name).cloned().unwrap_or_else(|| super::system_theme(None)),
        }
    }
}

impl fmt::Display for ThemeChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ThemeChoice::System => f.write_str("System"),
            ThemeChoice::SystemLight => f.write_str("System light"),
            ThemeChoice::SystemDark => f.write_str("System dark"),
            ThemeChoice::Named(name) => f.write_str(name),
        }
    }
}

impl App {
    /// The Settings panel, while it's open.
    pub(super) fn settings_panel(&self) -> Option<Element<'_, Message>> {
        if !self.settings_open {
            return None;
        }
        let close = tip(button(text("✕").size(14)).padding([0, 6]).style(button::text).on_press(Message::SettingsPanel), text("Close"), tooltip::Position::Bottom);
        let note = |s: &'static str| text(s).size(13).style(text::secondary);
        let body = column![
            row![text("Settings").size(18).width(Length::Fill), close].align_y(Alignment::Center),
            text("Look").size(15),
            column![
                pick_list(ThemeChoice::all(), Some(self.theme_choice.clone()), Message::Theme).width(Length::Fill),
                note("System follows the desktop's light or dark mode and accent colour; System light and System dark keep its colours but hold to one."),
            ]
            .spacing(6),
            rule::horizontal(1),
            text("Decoding").size(15),
            column![
                toggler(self.ten_bit).label("10-bit").on_toggle(Message::TenBit),
                note("Decodes the OSV and 10-bit videos (a phone's HDR) at all their 10 bits instead of 8: smoother skies and gradients, twice the data to move. Applies from the next seek or clip. Being tried out for 0.5."),
            ]
            .spacing(6),
            rule::horizontal(1),
            text("Keyframes").size(15),
            column![
                toggler(self.auto_key).label("Auto keyframe").on_toggle(Message::AutoKey),
                note("On: looking around while paused sets the view keyframe at the playhead, or updates the one there, as K does. Each look-around is one undo step. While playing, looking around stays free."),
                note("Off: looking around is free until you press K."),
            ]
            .spacing(6),
        ]
        .spacing(12)
        .padding(12);
        Some(container(scrollable(body)).width(WIDTH).height(Length::Fill).into())
    }
}

#[cfg(test)]
mod tests {
    use super::ThemeChoice;

    #[test]
    fn every_theme_is_read_back_as_saved() {
        for choice in ThemeChoice::all() {
            assert_eq!(ThemeChoice::from_key(&choice.key()), choice);
        }
        assert_eq!(ThemeChoice::from_key("no such theme"), ThemeChoice::System);
    }
}
