//! Small formatting helpers shared across views.

use crate::settings::TooltipVerbosity;

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
