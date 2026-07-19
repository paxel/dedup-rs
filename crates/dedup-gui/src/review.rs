//! The shared **review board** the Transfer and Grooming previews render into:
//! a side-by-side diff of a *source* repo and a *target* repo. Four columns:
//!
//! | STATUS | ⟨source abs path⟩ | ⟨target abs path⟩ | STATUS |
//! |--------|-------------------|-------------------|--------|
//! | source status glyph | source relative path | target relative path | target status glyph |
//!
//! Each side's status is one icon — added (green `+`), unchanged (grey `✓`),
//! removed (red trash) — and the relative-path cell is coloured to match; a side
//! where the file is absent shows a grey `✗` glyph and an empty path cell.
//!
//! Built on the same virtualised, click-to-sort `egui_extras` table as the
//! Browse tab (reusing [`crate::util::sort_header`]), so it scrolls through
//! arbitrarily many rows. Unchanged rows are the least interesting, so they are
//! hidden by default behind a toggle. This module is presentation only.

use crate::icon;
use crate::theme;
use egui::{Align, Layout, RichText};
use egui_extras::{Column, TableBuilder};

/// A file's status on one side of the diff.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SideStatus {
    Added,
    Removed,
    Unchanged,
    /// The file is absent on this side (e.g. a single-repo deletion has no
    /// target side, a sync deletion has no source side).
    Absent,
}

impl SideStatus {
    /// The single status icon (Phosphor glyph).
    pub fn symbol(self) -> &'static str {
        match self {
            SideStatus::Added => icon::PLUS,
            SideStatus::Removed => icon::TRASH,
            SideStatus::Unchanged => icon::CHECK,
            SideStatus::Absent => icon::X,
        }
    }

    pub fn color(self) -> egui::Color32 {
        match self {
            SideStatus::Added => theme::GREEN,
            SideStatus::Removed => theme::RED,
            SideStatus::Unchanged | SideStatus::Absent => theme::GREY,
        }
    }

    /// Sort rank (absent last).
    fn rank(self) -> u8 {
        match self {
            SideStatus::Added => 0,
            SideStatus::Removed => 1,
            SideStatus::Unchanged => 2,
            SideStatus::Absent => 3,
        }
    }
}

/// One file in the review: its status and relative path on each side. A side's
/// path is empty when the file is absent there; the two paths differ only for an
/// organize move (old → new within one repo).
pub struct ReviewRow {
    pub source: SideStatus,
    pub target: SideStatus,
    pub source_path: String,
    pub target_path: String,
}

impl ReviewRow {
    /// A row is "unchanged" only when neither side changes.
    fn is_unchanged(&self) -> bool {
        self.source == SideStatus::Unchanged && self.target == SideStatus::Unchanged
    }
}

/// Which column the table is sorted on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReviewCol {
    SourceStatus,
    Source,
    Target,
    TargetStatus,
}

/// Per-view review state, persisted across frames.
pub struct ReviewState {
    pub sort_col: ReviewCol,
    pub sort_asc: bool,
    /// Whether the (least-interesting) unchanged rows are shown. Off by default.
    pub show_unchanged: bool,
}

impl Default for ReviewState {
    fn default() -> Self {
        Self {
            sort_col: ReviewCol::Source,
            sort_asc: true,
            show_unchanged: false,
        }
    }
}

/// Sort `rows` into the state's column + direction, tie-breaking on the source
/// path so the order is stable. Sorting on a status column groups that side's
/// additions / deletions together.
pub fn sort(rows: &mut [ReviewRow], state: &ReviewState) {
    rows.sort_by(|a, b| {
        let by_source = || a.source_path.to_lowercase().cmp(&b.source_path.to_lowercase());
        let ord = match state.sort_col {
            ReviewCol::SourceStatus => a.source.rank().cmp(&b.source.rank()).then_with(by_source),
            ReviewCol::Source => by_source(),
            ReviewCol::Target => a
                .target_path
                .to_lowercase()
                .cmp(&b.target_path.to_lowercase())
                .then_with(by_source),
            ReviewCol::TargetStatus => a.target.rank().cmp(&b.target.rank()).then_with(by_source),
        };
        if state.sort_asc { ord } else { ord.reverse() }
    });
}

/// Render the review board: a summary line (true full counts), an optional
/// "show unchanged" toggle, an "only showing first N" note when the sample was
/// capped, then the virtualised, click-to-sort four-column diff. `totals` is
/// `[added, removed, unchanged]` full counts, independent of the capped sample.
pub fn table(
    ui: &mut egui::Ui,
    state: &mut ReviewState,
    rows: &mut [ReviewRow],
    totals: [usize; 3],
    source_header: &str,
    target_header: &str,
) {
    summary(ui, totals);

    // The unchanged toggle only matters when there are unchanged rows to hide.
    if totals[2] > 0 {
        let label = format!("{} UNCHANGED", if state.show_unchanged { "HIDE" } else { "SHOW" });
        if crate::lcars::toggle_button(ui, &label, state.show_unchanged, theme::GREY).clicked() {
            state.show_unchanged = !state.show_unchanged;
        }
    }

    // Visible rows: hide unchanged unless the toggle is on.
    let visible: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| state.show_unchanged || !r.is_unchanged())
        .map(|(i, _)| i)
        .collect();

    let full: usize = totals.iter().sum();
    if full > rows.len() {
        ui.label(
            RichText::new(format!(
                "showing first {} of {full} — refine the filter to see the rest",
                rows.len()
            ))
            .color(theme::TAN)
            .size(11.0),
        );
    }
    ui.add_space(2.0);

    let cols = [
        (ReviewCol::SourceStatus, "STATUS"),
        (ReviewCol::Source, source_header),
        (ReviewCol::Target, target_header),
        (ReviewCol::TargetStatus, "STATUS"),
    ];
    let (sort_col, sort_asc) = (state.sort_col, state.sort_asc);
    let mut clicked: Option<ReviewCol> = None;

    TableBuilder::new(ui)
        .striped(true)
        .resizable(true)
        .cell_layout(Layout::left_to_right(Align::Center))
        .column(Column::initial(90.0).at_least(70.0).resizable(true))
        .column(Column::initial(340.0).at_least(140.0).clip(true).resizable(true))
        .column(Column::remainder().at_least(140.0).clip(true).resizable(true))
        .column(Column::initial(90.0).at_least(70.0))
        .header(24.0, |mut header| {
            for (col, title) in cols {
                header.col(|ui| {
                    if crate::util::sort_header(ui, title, sort_col == col, sort_asc).clicked() {
                        clicked = Some(col);
                    }
                });
            }
        })
        .body(|body| {
            body.rows(20.0, visible.len(), |mut row| {
                let r = &rows[visible[row.index()]];
                row.col(|ui| status_cell(ui, r.source));
                row.col(|ui| path_cell(ui, r.source, &r.source_path));
                row.col(|ui| path_cell(ui, r.target, &r.target_path));
                row.col(|ui| status_cell(ui, r.target));
            });
        });

    if let Some(col) = clicked {
        if state.sort_col == col {
            state.sort_asc = !state.sort_asc;
        } else {
            state.sort_col = col;
            state.sort_asc = true;
        }
        sort(rows, state);
    }
}

/// One side's status cell: just the coloured status icon.
fn status_cell(ui: &mut egui::Ui, status: SideStatus) {
    ui.label(RichText::new(status.symbol()).color(status.color()).size(15.0));
}

/// One side's path cell: the relative path coloured by that side's status.
/// Absent sides (or empty paths) render nothing.
fn path_cell(ui: &mut egui::Ui, status: SideStatus, path: &str) {
    if status == SideStatus::Absent || path.is_empty() {
        return;
    }
    ui.add(egui::Label::new(RichText::new(path).color(status.color())).truncate());
}

/// The summary line: `«icon» «count» «label»` for each non-zero status, in its
/// colour, using the true full totals `[added, removed, unchanged]`.
fn summary(ui: &mut egui::Ui, totals: [usize; 3]) {
    let entries = [
        (SideStatus::Added, totals[0], "added"),
        (SideStatus::Removed, totals[1], "removed"),
        (SideStatus::Unchanged, totals[2], "unchanged"),
    ];
    ui.horizontal_wrapped(|ui| {
        for (status, n, label) in entries {
            if n == 0 {
                continue;
            }
            ui.label(
                RichText::new(format!("{} {n} {label}", status.symbol()))
                    .color(status.color())
                    .strong(),
            );
            ui.add_space(10.0);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(source: SideStatus, target: SideStatus, sp: &str, tp: &str) -> ReviewRow {
        ReviewRow {
            source,
            target,
            source_path: sp.to_string(),
            target_path: tp.to_string(),
        }
    }

    #[test]
    fn each_status_has_a_distinct_symbol_and_colour() {
        use SideStatus::*;
        let all = [Added, Removed, Unchanged, Absent];
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                assert_ne!(all[i].symbol(), all[j].symbol(), "symbols differ");
            }
        }
        assert_eq!(Added.color(), theme::GREEN);
        assert_eq!(Removed.color(), theme::RED);
        assert_eq!(Unchanged.color(), theme::GREY);
        assert_eq!(Absent.symbol(), icon::X);
    }

    #[test]
    fn is_unchanged_requires_both_sides() {
        assert!(row(SideStatus::Unchanged, SideStatus::Unchanged, "a", "a").is_unchanged());
        assert!(!row(SideStatus::Unchanged, SideStatus::Added, "a", "a").is_unchanged());
    }

    #[test]
    fn sort_by_target_status_groups_removals_then_toggles() {
        let mut rows = vec![
            row(SideStatus::Unchanged, SideStatus::Added, "b", "b"), // target Added (0)
            row(SideStatus::Absent, SideStatus::Removed, "", "a"),   // target Removed (1)
            row(SideStatus::Unchanged, SideStatus::Added, "a", "a"), // target Added (0)
        ];
        let mut state = ReviewState {
            sort_col: ReviewCol::TargetStatus,
            sort_asc: true,
            show_unchanged: false,
        };
        sort(&mut rows, &state);
        assert_eq!(rows[0].target, SideStatus::Added);
        assert_eq!(rows[2].target, SideStatus::Removed);

        state.sort_asc = false;
        sort(&mut rows, &state);
        assert_eq!(rows[0].target, SideStatus::Removed);
    }

    #[test]
    fn sort_by_source_path_is_alphabetical() {
        let mut rows = vec![
            row(SideStatus::Unchanged, SideStatus::Added, "zeta", "zeta"),
            row(SideStatus::Removed, SideStatus::Absent, "alpha", ""),
        ];
        let state = ReviewState {
            sort_col: ReviewCol::Source,
            sort_asc: true,
            show_unchanged: false,
        };
        sort(&mut rows, &state);
        assert_eq!(rows[0].source_path, "alpha");
        assert_eq!(rows[1].source_path, "zeta");
    }
}
