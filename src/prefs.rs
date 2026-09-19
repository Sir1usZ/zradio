use std::fs;
use std::path::{Path, PathBuf};

use ratatui::style::{Color, Modifier, Style};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThemeName {
    System,
    Latte,
    #[default]
    Frappe,
    Macchiato,
    Mocha,
}

impl ThemeName {
    pub fn next(self) -> Self {
        match self {
            Self::System => Self::Latte,
            Self::Latte => Self::Frappe,
            Self::Frappe => Self::Macchiato,
            Self::Macchiato => Self::Mocha,
            Self::Mocha => Self::System,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Latte => "latte",
            Self::Frappe => "frappe",
            Self::Macchiato => "macchiato",
            Self::Mocha => "mocha",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum VisualizeMode {
    Off,
    #[default]
    Bars,
    Oscilloscope,
    Cnm,
}

impl VisualizeMode {
    pub fn next(self) -> Self {
        match self {
            Self::Off => Self::Bars,
            Self::Bars => Self::Oscilloscope,
            Self::Oscilloscope => Self::Cnm,
            Self::Cnm => Self::Off,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Bars => "bars",
            Self::Oscilloscope => "scope",
            Self::Cnm => "cnm",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prefs {
    pub theme: ThemeName,
    pub transparent: bool,
    pub visualize: VisualizeMode,
    pub lyrics_fetch: bool,
    pub cover_fetch: bool,
    pub resume: bool,
    pub library: String,
    pub eq_db: [f32; 5],
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            theme: ThemeName::Frappe,
            transparent: false,
            visualize: VisualizeMode::Bars,
            lyrics_fetch: false,
            cover_fetch: false,
            resume: true,
            library: String::new(),
            eq_db: [0.0; 5],
        }
    }
}

impl Prefs {
    pub fn load() -> Self {
        let path = prefs_path();
        fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let path = prefs_path();
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = fs::write(path, json);
        }
    }

    pub fn library_path(&self) -> PathBuf {
        if self.library.trim().is_empty() {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join("音乐")
        } else {
            PathBuf::from(&self.library)
        }
    }

    pub fn set_library(&mut self, path: &Path) {
        self.library = path.display().to_string();
    }

    pub fn reset_eq(&mut self) {
        self.eq_db = [0.0; 5];
    }
}

fn prefs_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("zradio").join("prefs.json")
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    pub text: (u8, u8, u8),
    pub dim: (u8, u8, u8),
    pub accent: (u8, u8, u8),
    pub peak: (u8, u8, u8),
}

impl Palette {
    pub fn is_light(self) -> bool {
        let (r, g, b) = self.text;
        (0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32) / 255.0 < 0.5
    }

    pub fn rgb(c: (u8, u8, u8)) -> Color {
        Color::Rgb(c.0, c.1, c.2)
    }

    pub fn text_style(self) -> Style {
        Style::default().fg(Self::rgb(self.text))
    }

    pub fn dim_style(self) -> Style {
        Style::default().fg(Self::rgb(self.dim))
    }

    pub fn accent_style(self) -> Style {
        Style::default().fg(Self::rgb(self.accent))
    }

    pub fn peak_style(self) -> Style {
        Style::default().fg(Self::rgb(self.peak))
    }

    pub fn scrim(self) -> Color {
        if self.is_light() {
            Color::Rgb(226, 226, 230)
        } else {
            Color::Rgb(18, 18, 22)
        }
    }

    pub fn panel(self) -> Color {
        if self.is_light() {
            Color::Rgb(239, 239, 244)
        } else {
            Color::Rgb(28, 28, 34)
        }
    }

    pub fn highlight(self) -> Style {
        let fg = if self.is_light() {
            Color::Rgb(255, 255, 255)
        } else {
            Color::Rgb(24, 24, 27)
        };
        Style::default()
            .add_modifier(Modifier::BOLD)
            .fg(fg)
            .bg(Self::rgb(self.accent))
    }

    pub fn for_theme(name: ThemeName) -> Self {
        match name {
            ThemeName::System if terminal_is_light() => Self::for_theme(ThemeName::Latte),
            ThemeName::Latte => Self {
                text: (76, 79, 105),
                dim: (92, 95, 119),
                accent: (4, 102, 166),
                peak: (188, 110, 8),
            },
            ThemeName::Frappe | ThemeName::System => Self {
                text: (198, 208, 245),
                dim: (165, 173, 206),
                accent: (140, 170, 238),
                peak: (229, 200, 144),
            },
            ThemeName::Macchiato => Self {
                text: (202, 211, 245),
                dim: (165, 173, 203),
                accent: (138, 173, 244),
                peak: (238, 212, 159),
            },
            ThemeName::Mocha => Self {
                text: (205, 214, 244),
                dim: (166, 173, 200),
                accent: (137, 180, 250),
                peak: (249, 226, 175),
            },
        }
    }
}

fn terminal_is_light() -> bool {
    std::env::var("COLORFGBG")
        .ok()
        .and_then(|v| v.split(';').nth(1)?.parse::<u8>().ok())
        .is_some_and(|bg| bg == 7 || bg == 15)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visualize_cycles_through_cnm() {
        let mut v = VisualizeMode::Off;
        let mut labels = Vec::new();
        for _ in 0..4 {
            v = v.next();
            labels.push(v.label());
        }
        assert_eq!(labels, ["bars", "scope", "cnm", "off"]);
        assert_eq!(v.next(), VisualizeMode::Bars);
    }

    #[test]
    fn theme_cycles_five_names() {
        let mut t = ThemeName::System;
        for _ in 0..4 {
            t = t.next();
        }
        assert_eq!(t.next(), ThemeName::System);
    }

    #[test]
    fn empty_library_falls_back_to_music_dir() {
        let prefs = Prefs::default();
        assert!(prefs.library_path().ends_with("音乐"));
    }

    #[test]
    fn latte_text_is_dark_frappe_text_is_light() {
        let latte = Palette::for_theme(ThemeName::Latte);
        let frappe = Palette::for_theme(ThemeName::Frappe);
        assert!(
            luminance(latte.text) < 0.45,
            "latte text must stay dark on a light terminal, got {:?}",
            latte.text
        );
        assert!(
            luminance(frappe.text) > 0.55,
            "frappe text must stay light on a dark terminal, got {:?}",
            frappe.text
        );
        assert!(latte.is_light());
        assert!(!frappe.is_light());
    }

    fn luminance((r, g, b): (u8, u8, u8)) -> f32 {
        (0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32) / 255.0
    }
}
