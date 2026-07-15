//! Persistent GUI settings, stored as JSON in the store's config dir so user
//! choices survive a relaunch (they used to reset every launch). Loaded once at
//! startup and written by eframe's periodic `save` hook.
//!
//! Deliberately excludes per-repo read-only state: re-locking every repo on
//! load is a safety default (deletion must be a fresh, conscious unlock), so it
//! stays session-only.

use std::path::{Path, PathBuf};

const FILE: &str = "gui_settings.json";

/// How much explanation a hover tooltip gives: a terse one-liner, or a fuller
/// paragraph on what the control does and when to use it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TooltipVerbosity {
    #[default]
    Short,
    Verbose,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Hashing thread count used by scans (0 = rayon default).
    pub threads: usize,
    /// Duplicates-tab similarity slider position (percent).
    pub similarity_threshold: f64,
    /// Transfer-tab SIMILAR folder-export similarity slider position (percent).
    pub transfer_similarity_threshold: f64,
    /// Hover-tooltip wording: short one-liners or verbose explanations.
    pub tooltip_verbosity: TooltipVerbosity,
    /// Last window inner size in logical points `[w, h]`, restored next launch
    /// (`None` until the window has been sized once).
    pub window_size: Option<[f32; 2]>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            threads: 0,
            similarity_threshold: 99.0,
            transfer_similarity_threshold: 90.0,
            tooltip_verbosity: TooltipVerbosity::default(),
            window_size: None,
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
            transfer_similarity_threshold: 88.0,
            tooltip_verbosity: TooltipVerbosity::Verbose,
            window_size: Some([1280.0, 800.0]),
        };
        s.save(dir.path());
        assert_eq!(Settings::load(dir.path()), s);

        // Corrupt file → defaults, never a panic.
        std::fs::write(dir.path().join(FILE), b"not json").unwrap();
        assert_eq!(Settings::load(dir.path()), Settings::default());
    }

    /// The verbosity tag round-trips through JSON as a plain string, so a
    /// hand-edited config file (`"tooltip_verbosity": "Verbose"`) keeps working.
    #[test]
    fn tooltip_verbosity_json_tag_is_stable() {
        assert_eq!(
            serde_json::to_string(&TooltipVerbosity::Short).unwrap(),
            "\"Short\""
        );
        assert_eq!(
            serde_json::to_string(&TooltipVerbosity::Verbose).unwrap(),
            "\"Verbose\""
        );
        assert_eq!(
            serde_json::from_str::<TooltipVerbosity>("\"Verbose\"").unwrap(),
            TooltipVerbosity::Verbose
        );
        assert_eq!(TooltipVerbosity::default(), TooltipVerbosity::Short);
    }
}
