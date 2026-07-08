// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 matthias

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Config {
    /// Root directory, searched recursively for PDF files.
    pub root_dir: PathBuf,
    /// Stroke width in PDF points (at medium pressure).
    #[serde(default = "default_pen_width")]
    pub pen_width: f64,
    /// Pen color as a hex value, e.g. "#cc0000".
    #[serde(default = "default_pen_color")]
    pub pen_color: String,
    /// Tuner reference pitch for A4 in Hz (e.g. 440.0 or 443.0).
    #[serde(default = "default_a4")]
    pub a4: f64,
    /// Note naming: "german" (C ... B, H) or "english" (C ... A#, B).
    /// Defaults to german.
    #[serde(default = "default_note_names")]
    pub note_names: String,
    /// Accidental style for the tuner: "flat" (Db/Des, Eb/Es) or "sharp"
    /// (C#, D#). Defaults to flat.
    #[serde(default = "default_accidentals")]
    pub accidentals: String,
    /// Start the app in fullscreen (F11 or the overlay button toggle it).
    #[serde(default = "default_start_fullscreen")]
    pub start_fullscreen: bool,
}

fn default_start_fullscreen() -> bool {
    true
}

fn default_accidentals() -> String {
    "flat".to_string()
}

fn default_a4() -> f64 {
    443.0
}

fn default_note_names() -> String {
    "german".to_string()
}

fn default_pen_width() -> f64 {
    1.5
}

fn default_pen_color() -> String {
    "#cc0000".to_string()
}

impl Default for Config {
    fn default() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        Config {
            root_dir: home.join("Noten"),
            pen_width: default_pen_width(),
            pen_color: default_pen_color(),
            a4: default_a4(),
            note_names: default_note_names(),
            accidentals: default_accidentals(),
            start_fullscreen: default_start_fullscreen(),
        }
    }
}

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("frack")
        .join("config.toml")
}

/// Loads the config, creating a default one if none exists. Also
/// returns whether the file was newly created.
pub fn load_or_create() -> (Config, bool) {
    let path = config_path();
    if let Ok(text) = std::fs::read_to_string(&path) {
        match toml::from_str::<Config>(&text) {
            Ok(cfg) => return (cfg, false),
            Err(e) => {
                eprintln!("error in {}: {e}; using defaults", path.display());
                return (Config::default(), false);
            }
        }
    }
    let cfg = Config::default();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = toml::to_string_pretty(&cfg) {
        let _ = std::fs::write(&path, text);
    }
    (cfg, true)
}

impl Config {
    /// Pen color as (r, g, b) in 0..=1.
    pub fn pen_rgb(&self) -> (f64, f64, f64) {
        parse_hex_color(&self.pen_color).unwrap_or((0.8, 0.0, 0.0))
    }
}

pub fn parse_hex_color(s: &str) -> Option<(f64, f64, f64)> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some((r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0))
}

/// Does the root directory exist? Used to show a hint in the library.
pub fn root_exists(cfg: &Config) -> bool {
    Path::new(&cfg.root_dir).is_dir()
}
