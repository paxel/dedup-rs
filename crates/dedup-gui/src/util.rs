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
    ui.label(RichText::new(text).color(theme::LILAC).size(11.0));
}

/// The shared "similarity" threshold control: a labelled `50–100 %` slider
/// (with an "identical" hint at the top) used everywhere a SIMILAR grouping is
/// chosen, so the Duplicates and Transfer tabs present it identically. Edits
/// `threshold` in place. Meant to sit in its own `ui.horizontal` row.
pub fn similarity_slider(ui: &mut egui::Ui, threshold: &mut f64, verbosity: TooltipVerbosity) {
    ui.label(RichText::new("similarity").color(theme::TEXT).size(12.0));
    // The value box draws on the orange pill, where the theme's global cream
    // text is unreadable — use black there, and a light backdrop while typing.
    let visuals = ui.visuals_mut();
    visuals.override_text_color = Some(theme::BLACK);
    visuals.extreme_bg_color = theme::TAN;
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
        ui.label(RichText::new("identical").color(theme::BLUE).size(11.0));
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
    let color = if active { theme::AMBER } else { theme::TEXT };
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

#[cfg(test)]
mod explain_tests {
    use super::*;

    #[test]
    fn picks_short_or_verbose_by_setting() {
        assert_eq!(pick_tooltip(TooltipVerbosity::Short, "s", "v"), "s");
        assert_eq!(pick_tooltip(TooltipVerbosity::Verbose, "s", "v"), "v");
    }
}
