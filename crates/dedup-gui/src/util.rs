//! Small formatting helpers shared across views.

use crate::settings::TooltipVerbosity;
use crate::theme;
use egui::RichText;

/// Attaches a hover tooltip whose wording depends on the app's tooltip
/// verbosity setting, so every callsite picks short/verbose text once instead
/// of branching on the setting itself.
pub trait ExplainExt {
    /// Show `short` when the setting is [`TooltipVerbosity::Short`], `verbose`
    /// when it's [`TooltipVerbosity::Verbose`].
    fn explain(self, verbosity: TooltipVerbosity, short: &str, verbose: &str) -> Self;
}

impl ExplainExt for egui::Response {
    fn explain(self, verbosity: TooltipVerbosity, short: &str, verbose: &str) -> Self {
        self.on_hover_text(pick_tooltip(verbosity, short, verbose))
    }
}

/// The pure short/verbose selection, factored out so it's unit-testable
/// without needing a live `egui::Response`.
fn pick_tooltip<'a>(verbosity: TooltipVerbosity, short: &'a str, verbose: &'a str) -> &'a str {
    match verbosity {
        TooltipVerbosity::Short => short,
        TooltipVerbosity::Verbose => verbose,
    }
}

/// A persistent one-line keyboard-shortcut hint, styled like the lightbox's, so
/// every view advertises its shortcuts the same way. Middot-separated, e.g.
/// `"F find · ←/→ page · M mode"`.
pub fn shortcut_bar(ui: &mut egui::Ui, text: &str) {
    ui.add_space(2.0);
    ui.label(RichText::new(text).color(theme::lilac()).size(11.0));
}

/// The shared "similarity" threshold control: a labelled `50–100 %` slider
/// (with an "identical" hint at the top) used everywhere a SIMILAR grouping is
/// chosen, so the Duplicates and Transfer tabs present it identically. Edits
/// `threshold` in place. Meant to sit in its own `ui.horizontal` row.
pub fn similarity_slider(ui: &mut egui::Ui, threshold: &mut f64, verbosity: TooltipVerbosity) {
    ui.label(RichText::new("similarity").color(theme::text()).size(12.0));
    // The value box draws on the orange pill, where the theme's global cream
    // text is unreadable — use black there, and a light backdrop while typing.
    let visuals = ui.visuals_mut();
    visuals.override_text_color = Some(theme::black());
    visuals.extreme_bg_color = theme::tan();
    ui.add(
        egui::Slider::new(threshold, 50.0..=100.0)
            .suffix("%")
            .max_decimals(1),
    )
    .explain(
        verbosity,
        "Minimum similarity to group as similar",
        "How alike two files' perceptual hashes must be to group as similar: \
         similarity % = (1 − hamming distance / bits) × 100. Lower catches more \
         (and riskier) matches; 100% is bit-identical.",
    );
    // 100% is bit-identical (512-bit hash); ≥99.5% is visually identical.
    if *threshold >= 99.5 {
        ui.label(RichText::new("identical").color(theme::blue()).size(11.0));
    }
}

/// A click-to-sort column header label: shows the title, and when this column
/// is the active sort key appends an up/down caret and paints it amber. Shared
/// by every `egui_extras` table (Browse, the review board) so their headers
/// behave identically. Returns the label's `Response` so callers detect clicks.
pub fn sort_header(ui: &mut egui::Ui, title: &str, active: bool, asc: bool) -> egui::Response {
    let text = if active {
        let caret = if asc {
            crate::icon::CARET_UP
        } else {
            crate::icon::CARET_DOWN
        };
        format!("{title} {caret}")
    } else {
        title.to_string()
    };
    let color = if active {
        theme::amber()
    } else {
        theme::text()
    };
    ui.add(egui::Label::new(RichText::new(text).color(color).strong()).sense(egui::Sense::click()))
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Human-readable byte size (binary units).
pub fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    const TB: u64 = GB * 1024;
    if bytes >= TB {
        format!("{:.2} TB", bytes as f64 / TB as f64)
    } else if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Format an epoch-millisecond timestamp as `YYYY-MM-DD HH:MM`, or `—` if invalid.
pub fn format_mtime(ms: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_millis_opt(ms) {
        chrono::LocalResult::Single(dt) => dt.format("%Y-%m-%d %H:%M").to_string(),
        _ => "—".to_string(),
    }
}

/// Fall back to the default for a read whose failure only costs a hint — a tag
/// list, a mime suggestion — while still recording it in the session log.
///
/// Only for reads the user can't act on. A failure that changes what is on
/// screen (which repos exist, which are sinks) must reach the user directly,
/// not just the log.
pub fn or_log_default<T: Default, E: std::fmt::Display>(result: Result<T, E>, what: &str) -> T {
    match result {
        Ok(value) => value,
        Err(e) => {
            log::warn!("{what}: {e}");
            T::default()
        }
    }
}

/// The folder last picked in any native folder dialog, process-wide — seeded
/// from the persisted settings at startup, read back into them on save. Kept
/// here (not threaded through every view) because pickers open from surfaces
/// that don't otherwise know about settings.
static LAST_PICKED_DIR: std::sync::Mutex<Option<std::path::PathBuf>> = std::sync::Mutex::new(None);

/// Seed the picker memory from the persisted settings (startup).
pub fn seed_last_picked_dir(dir: Option<std::path::PathBuf>) {
    *LAST_PICKED_DIR.lock().unwrap_or_else(|e| e.into_inner()) = dir;
}

/// The remembered last-picked folder, for persisting back into the settings.
pub fn last_picked_dir() -> Option<std::path::PathBuf> {
    LAST_PICKED_DIR
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Record a dialog selection into the picker memory.
pub fn remember_picked_dir(dir: &std::path::Path) {
    *LAST_PICKED_DIR.lock().unwrap_or_else(|e| e.into_inner()) = Some(dir.to_path_buf());
}

/// Open a native folder picker that starts at the **parent of the last
/// selection** (any picker, any session) instead of the home directory, and
/// remembers whatever is picked. Pickers with a smarter contextual anchor set
/// their own start directory and only *record* into this memory.
pub fn pick_folder_remembered() -> Option<std::path::PathBuf> {
    let mut dialog = rfd::FileDialog::new();
    if let Some(last) = last_picked_dir() {
        let start = match last.parent() {
            Some(p) if p.as_os_str().is_empty() => last.clone(),
            Some(p) => p.to_path_buf(),
            None => last.clone(),
        };
        if start.is_dir() {
            dialog = dialog.set_directory(start);
        }
    }
    let picked = dialog.pick_folder();
    if let Some(p) = &picked {
        remember_picked_dir(p);
    }
    picked
}

/// Attach a right-click **Copy** context menu to a widget's response, copying
/// `text` to the clipboard. Any label carrying a path or name goes through
/// this, so grabbing a filename to search elsewhere never needs a shortcut.
pub fn copy_menu(resp: &egui::Response, text: &str) {
    // Labels don't sense clicks by default; a context menu still works because
    // egui tracks secondary clicks on the response's rect via `interact`.
    //
    // A response that *already* senses clicks must NOT be re-interacted: that
    // call re-registers the widget under the response's own id with the
    // response's rect, and a table row's response is the union of its cells
    // carrying the **first cell's** id. Re-registering bound that id to a rect
    // with the first column cut out of it, so clicks anywhere in that column
    // landed on nothing — rows could only be picked by hitting a cell the
    // union happened to cover (an empty one, usually).
    let resp = if resp.sense.senses_click() {
        resp.clone()
    } else {
        resp.clone().interact(egui::Sense::click())
    };
    resp.context_menu(|ui| {
        if ui.button("Copy").clicked() {
            ui.ctx().copy_text(text.to_string());
        }
    });
}

#[cfg(test)]
mod explain_tests {
    use super::*;

    #[test]
    fn picks_short_or_verbose_by_setting() {
        assert_eq!(pick_tooltip(TooltipVerbosity::Short, "s", "v"), "s");
        assert_eq!(pick_tooltip(TooltipVerbosity::Verbose, "s", "v"), "v");
    }

    #[test]
    fn or_log_default_passes_values_through_and_defaults_on_error() {
        let ok: Result<Vec<u8>, String> = Ok(vec![1, 2]);
        assert_eq!(or_log_default(ok, "reading"), vec![1, 2]);
        let err: Result<Vec<u8>, String> = Err("boom".to_string());
        assert!(or_log_default(err, "reading").is_empty());
    }
}
