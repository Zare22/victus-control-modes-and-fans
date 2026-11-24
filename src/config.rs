//! The remembered setting, in `%APPDATA%\victus-control\config.toml`:
//!
//! ```toml
//! mode = "performance"
//! max_fan = false
//! ```

use std::fs;
use std::io;
use std::path::PathBuf;

use crate::victus::Mode;

/// `None` = never chosen, so never re-applied.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Config {
    pub mode: Option<Mode>,
    pub max_fan: Option<bool>,
}

fn dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("victus-control"))
}

impl Config {
    /// A missing or unreadable file gives the default (nothing remembered).
    pub fn load() -> Config {
        dir()
            .and_then(|d| fs::read_to_string(d.join("config.toml")).ok())
            .map(|text| Config::parse(&text))
            .unwrap_or_default()
    }

    pub fn save(&self) -> io::Result<()> {
        let dir = dir().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "APPDATA is not set"))?;
        fs::create_dir_all(&dir)?;
        // Write-then-rename so a crash never leaves a half-written file.
        let tmp = dir.join("config.toml.tmp");
        fs::write(&tmp, self.to_text())?;
        fs::rename(tmp, dir.join("config.toml"))
    }

    fn parse(text: &str) -> Config {
        let mut cfg = Config::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else { continue };
            let value = value.trim().trim_matches('"');
            match key.trim() {
                "mode" => cfg.mode = Mode::parse(value),
                "max_fan" => cfg.max_fan = value.parse().ok(),
                _ => {}
            }
        }
        cfg
    }

    fn to_text(self) -> String {
        let mut text = String::new();
        if let Some(mode) = self.mode {
            text += &format!("mode = \"{}\"\n", mode.name().to_ascii_lowercase());
        }
        if let Some(on) = self.max_fan {
            text += &format!("max_fan = {on}\n");
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        for cfg in [
            Config::default(),
            Config { mode: Some(Mode::Quiet), max_fan: Some(true) },
            Config { mode: Some(Mode::Performance), max_fan: None },
            Config { mode: None, max_fan: Some(false) },
        ] {
            assert_eq!(Config::parse(&cfg.to_text()), cfg);
        }
    }

    #[test]
    fn parses_hand_edited_file() {
        let cfg = Config::parse("# comment\n  mode =   \"Default\"  \nmax_fan=true\nother = 1\n");
        assert_eq!(cfg, Config { mode: Some(Mode::Default), max_fan: Some(true) });
    }

    #[test]
    fn ignores_bad_values() {
        assert_eq!(Config::parse("mode = \"eco\"\nmax_fan = yes\n"), Config::default());
    }
}
