//! User configuration shared by all Brim frontends.
//!
//! A single JSON file (`$XDG_CONFIG_HOME/brim/config.json`, usually
//! `~/.config/brim/config.json`) is the whole settings surface — the CLI,
//! the GUI and future background services all read and write it through
//! this module. Missing keys fall back to defaults, so older builds keep
//! working with newer files and vice versa.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Top-level configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub sources: SourceConfig,
    pub gui: GuiConfig,
    /// Keys written by newer builds that this build does not know,
    /// preserved verbatim across load/save.
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, serde_json::Value>,
}

/// Which package sources are active.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SourceConfig {
    pub dnf5: bool,
    pub copr: bool,
    pub flatpak: bool,
}

impl Default for SourceConfig {
    fn default() -> Self {
        Self {
            dnf5: true,
            copr: true,
            flatpak: true,
        }
    }
}

/// GUI-only preferences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GuiConfig {
    pub icon_downloads: bool,
}

impl Default for GuiConfig {
    fn default() -> Self {
        Self {
            icon_downloads: true,
        }
    }
}

impl Config {
    /// All user-facing keys, in display order.
    pub const KEYS: &'static [&'static str] = &[
        "sources.dnf5",
        "sources.copr",
        "sources.flatpak",
        "gui.icon_downloads",
    ];

    /// A key's value, or `None` for unknown keys.
    pub fn get(&self, key: &str) -> Option<bool> {
        match key {
            "sources.dnf5" => Some(self.sources.dnf5),
            "sources.copr" => Some(self.sources.copr),
            "sources.flatpak" => Some(self.sources.flatpak),
            "gui.icon_downloads" => Some(self.gui.icon_downloads),
            _ => None,
        }
    }

    /// Set a key's value; `false` for unknown keys.
    pub fn set(&mut self, key: &str, value: bool) -> bool {
        match key {
            "sources.dnf5" => self.sources.dnf5 = value,
            "sources.copr" => self.sources.copr = value,
            "sources.flatpak" => self.sources.flatpak = value,
            "gui.icon_downloads" => self.gui.icon_downloads = value,
            _ => return false,
        }
        true
    }

    /// Load the shared config file (see [`Config::load_from`]).
    pub fn load() -> Config {
        Config::load_from(&config_path())
    }

    /// Load from `path`. A missing file yields defaults and is created so
    /// the user can see and edit it; an unparseable file yields defaults
    /// without touching the file (the CLI warns about it separately).
    pub fn load_from(path: &Path) -> Config {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => {
                let config = Config::default();
                // Best effort: loading must never fail just because the
                // file could not be created.
                let _ = config.save_to(path);
                config
            }
        }
    }

    /// Whether `path` holds a parseable config. A missing file counts as
    /// valid (defaults will be written on load; nothing to warn about).
    pub fn file_is_valid(path: &Path) -> bool {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str::<Config>(&text).is_ok(),
            Err(_) => true,
        }
    }

    /// Save to the shared config file (see [`Config::save_to`]).
    pub fn save(&self) -> Result<()> {
        self.save_to(&config_path())
    }

    /// Save to `path` atomically (temp file + rename), creating parent
    /// directories as needed.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(self).expect("Config serialization cannot fail");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

/// The path of the shared config file.
pub fn config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("brim").join("config.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("brim-test-{}-{name}", std::process::id()))
    }

    #[test]
    fn missing_file_yields_defaults_and_is_created() {
        let path = temp_path("missing.json");
        let _ = std::fs::remove_file(&path);
        let config = Config::load_from(&path);
        assert_eq!(config, Config::default());
        assert!(path.is_file());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn corrupt_file_yields_defaults_and_is_left_alone() {
        let path = temp_path("corrupt.json");
        std::fs::write(&path, "{not json").unwrap();
        let config = Config::load_from(&path);
        assert_eq!(config, Config::default());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{not json");
        assert!(!Config::file_is_valid(&path));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_keys_fall_back_to_defaults() {
        let path = temp_path("partial.json");
        std::fs::write(&path, r#"{"sources": {"flatpak": false}}"#).unwrap();
        let config = Config::load_from(&path);
        assert!(!config.sources.flatpak);
        assert!(config.sources.dnf5);
        assert!(config.sources.copr);
        assert!(config.gui.icon_downloads);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn unknown_keys_round_trip() {
        let path = temp_path("unknown.json");
        std::fs::write(
            &path,
            r#"{"sources": {"dnf5": false}, "future_section": {"x": 1}, "future_flag": true}"#,
        )
        .unwrap();
        let config = Config::load_from(&path);
        assert!(!config.sources.dnf5);
        config.save_to(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let reparsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(reparsed["future_section"]["x"], 1);
        assert_eq!(reparsed["future_flag"], true);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_load_round_trip() {
        let path = temp_path("roundtrip.json");
        let mut config = Config::default();
        config.sources.copr = false;
        config.gui.icon_downloads = false;
        config.save_to(&path).unwrap();
        assert_eq!(Config::load_from(&path), config);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn get_set_by_key() {
        let mut config = Config::default();
        assert_eq!(config.get("sources.dnf5"), Some(true));
        assert!(config.set("sources.dnf5", false));
        assert_eq!(config.get("sources.dnf5"), Some(false));
        assert!(config.set("gui.icon_downloads", false));
        assert_eq!(config.get("gui.icon_downloads"), Some(false));
        assert!(!config.set("sources.snap", true));
        assert_eq!(config.get("sources.snap"), None);
    }

    #[test]
    fn keys_cover_every_gettable_key() {
        let config = Config::default();
        for key in Config::KEYS {
            assert!(
                config.get(key).is_some(),
                "KEYS entry {key} must be gettable"
            );
        }
    }
}
