//! Persistent GUI settings, stored as JSON in the store's config dir so user
//! choices survive a relaunch (they used to reset every launch). Loaded once at
//! startup and written by eframe's periodic `save` hook.
//!
//! Deliberately excludes per-repo read-only state: re-locking every repo on
//! load is a safety default (deletion must be a fresh, conscious unlock), so it
//! stays session-only.

use std::path::{Path, PathBuf};

const FILE: &str = "gui_settings.json";

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Hashing thread count used by scans (0 = rayon default).
    pub threads: usize,
    /// Duplicate-similarity slider position (percent).
    pub similarity_threshold: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            threads: 0,
            similarity_threshold: 99.0,
        }
    }
}

impl Settings {
    fn path(config_dir: &Path) -> PathBuf {
        config_dir.join(FILE)
    }

    /// Load settings, falling back to defaults when absent or unreadable (a
    /// corrupt file must never block startup).
    pub fn load(config_dir: &Path) -> Self {
        std::fs::read(Self::path(config_dir))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Write settings back (best effort; errors are ignored).
    pub fn save(&self, config_dir: &Path) {
        if let Ok(json) = serde_json::to_vec_pretty(self) {
            let _ = std::fs::write(Self::path(config_dir), json);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_and_default_on_missing() {
        let dir = tempfile::tempdir().unwrap();
        // Missing file → defaults.
        assert_eq!(Settings::load(dir.path()), Settings::default());

        let s = Settings {
            threads: 4,
            similarity_threshold: 97.5,
        };
        s.save(dir.path());
        assert_eq!(Settings::load(dir.path()), s);

        // Corrupt file → defaults, never a panic.
        std::fs::write(dir.path().join(FILE), b"not json").unwrap();
        assert_eq!(Settings::load(dir.path()), Settings::default());
    }
}
