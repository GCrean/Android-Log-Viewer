//! Colour palettes (light/dark) and persisted user settings.

use crate::log::Level;
use gpui::{rgb, Hsla, WindowAppearance};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeMode {
    pub fn label(self) -> &'static str {
        match self {
            ThemeMode::System => "Auto",
            ThemeMode::Light => "Light",
            ThemeMode::Dark => "Dark",
        }
    }

    pub fn palette(self, appearance: WindowAppearance) -> Palette {
        let dark = match self {
            ThemeMode::Dark => true,
            ThemeMode::Light => false,
            ThemeMode::System => matches!(appearance, WindowAppearance::Dark | WindowAppearance::VibrantDark),
        };
        if dark {
            Palette::dark()
        } else {
            Palette::light()
        }
    }
}

#[derive(Clone, Copy)]
pub struct Palette {
    pub bg: Hsla,
    pub panel: Hsla,
    pub border: Hsla,
    pub text: Hsla,
    pub text_dim: Hsla,
    pub accent: Hsla,
    pub accent_text: Hsla,
    pub input_bg: Hsla,
    pub button_hover: Hsla,
    pub row_alt: Hsla,
    pub row_hover: Hsla,
    pub selected: Hsla,
    pub error_text: Hsla,
    levels: [Hsla; 6],
    level_bg: [Hsla; 6],
}

impl Palette {
    pub fn level(&self, l: Level) -> Hsla {
        self.levels[l as usize]
    }
    /// Subtle row tint for warnings and errors.
    pub fn level_bg(&self, l: Level) -> Hsla {
        self.level_bg[l as usize]
    }

    pub fn dark() -> Self {
        let tint = |c: u32, a: f32| {
            let mut h: Hsla = rgb(c).into();
            h.a = a;
            h
        };
        Palette {
            bg: rgb(0x1b1d23).into(),
            panel: rgb(0x23262e).into(),
            border: rgb(0x343843).into(),
            text: rgb(0xd7dae0).into(),
            text_dim: rgb(0x8b92a1).into(),
            accent: rgb(0x3d7eff).into(),
            accent_text: rgb(0xffffff).into(),
            input_bg: rgb(0x15171c).into(),
            button_hover: rgb(0x2f3440).into(),
            row_alt: tint(0xffffff, 0.02),
            row_hover: tint(0xffffff, 0.06),
            selected: tint(0x3d7eff, 0.30),
            error_text: rgb(0xff6b6b).into(),
            levels: [
                rgb(0x8b92a1).into(),
                rgb(0x5fb3f5).into(),
                rgb(0x6fcf7f).into(),
                rgb(0xf0c24b).into(),
                rgb(0xff6b6b).into(),
                rgb(0xff4fa3).into(),
            ],
            level_bg: [
                tint(0x000000, 0.0),
                tint(0x000000, 0.0),
                tint(0x000000, 0.0),
                tint(0xf0c24b, 0.07),
                tint(0xff6b6b, 0.12),
                tint(0xff4fa3, 0.18),
            ],
        }
    }

    pub fn light() -> Self {
        let tint = |c: u32, a: f32| {
            let mut h: Hsla = rgb(c).into();
            h.a = a;
            h
        };
        Palette {
            bg: rgb(0xffffff).into(),
            panel: rgb(0xf3f4f7).into(),
            border: rgb(0xd8dbe2).into(),
            text: rgb(0x22252b).into(),
            text_dim: rgb(0x6b7280).into(),
            accent: rgb(0x2563eb).into(),
            accent_text: rgb(0xffffff).into(),
            input_bg: rgb(0xffffff).into(),
            button_hover: rgb(0xe4e7ed).into(),
            row_alt: tint(0x000000, 0.025),
            row_hover: tint(0x000000, 0.06),
            selected: tint(0x2563eb, 0.18),
            error_text: rgb(0xc62828).into(),
            levels: [
                rgb(0x6b7280).into(),
                rgb(0x0b6fc2).into(),
                rgb(0x1b7f32).into(),
                rgb(0x9a6700).into(),
                rgb(0xc62828).into(),
                rgb(0xb0156b).into(),
            ],
            level_bg: [
                tint(0x000000, 0.0),
                tint(0x000000, 0.0),
                tint(0x000000, 0.0),
                tint(0xf0c24b, 0.14),
                tint(0xff6b6b, 0.14),
                tint(0xff4fa3, 0.16),
            ],
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Settings {
    pub theme: ThemeMode,
    /// Optional explicit path to the adb executable.
    pub adb_path: Option<String>,
    /// Last host:port used with "Connect over network".
    pub last_address: String,
}

impl Settings {
    fn path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("LogViewer").join("settings.json"))
    }

    pub fn load() -> Self {
        Self::path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let Some(path) = Self::path() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }
}
