//! The shared **review board** the Transfer and Grooming previews render into:
//! a side-by-side diff of a *source* repo and a *target* repo. Four columns:
//!
//! | STATUS | ⟨source abs path⟩ | ⟨target abs path⟩ | STATUS |
//! |--------|-------------------|-------------------|--------|
//! | source status glyph | source relative path | target relative path | target status glyph |
//!
//! Each side's status is an icon plus its meaning — added (green `+ ADDED`),
//! unchanged (grey `✓ UNCHANGED`), removed (red trash `REMOVED`) — and the
//! relative-path cell is coloured to match; a side where the file is absent
//! shows a grey `✗` and an empty path cell. Commands that have no target side at all (PURGE and
//! the other single-repo grooming commands) pass an empty `target_header`, and
//! the two target columns are dropped entirely.
//!
//! Built on the same virtualised, click-to-sort `egui_extras` table as the
//! Browse tab (reusing [`crate::util::sort_header`]), paged [`PAGE_SIZE`] rows
//! at a time with PREV/NEXT controls. Unchanged rows are the least interesting,
//! so they are hidden by default behind a toggle. This module is presentation
//! only.
//!
//! Per-row reject/apply controls are opt-in ([`RowControls`]): a caller that
//! runs its plan all-or-nothing (the Sync Groups push) asks for a read-only
//! board, because a reject toggle that the run ignores would promise to skip a
//! deletion and then make it anyway.

use crate::icon;
use crate::theme;
use egui::{Align, Layout, RichText};
use egui_extras::{Column, TableBuilder};

/// Safety cap on how many rows a preview materialises in memory. The summary
/// counts stay the true totals regardless; past the cap the table tells the
/// user to refine the filter.
pub const PREVIEW_CAP: usize = 10_000;

/// Rows shown per page of the review table.
const PAGE_SIZE: usize = 500;

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
    /// The word shown next to the icon in a status cell. An absent side has
    /// nothing to say — the cell stays empty.
    fn word(self) -> &'static str {
        match self {
            SideStatus::Added => "ADDED",
            SideStatus::Removed => "REMOVED",
            SideStatus::Unchanged => "UNCHANGED",
            SideStatus::Absent => "",
        }
    }

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

    /// The row's namespaced selection key, matching what the core ops check
    /// (see `dedup_core::diff`): the source path for source-side actions, the
    /// target path for target-side-only rows (sync/mirror deletions).
    pub fn key(&self) -> String {
        if self.source_path.is_empty() {
            dedup_core::diff::target_key(&self.target_path)
        } else {
            dedup_core::diff::source_key(&self.source_path)
        }
    }

    /// Whether the row represents an action at all (unchanged rows have
    /// nothing to reject or apply).
    fn is_actionable(&self) -> bool {
        !self.is_unchanged()
    }
}

/// A row-level interaction the caller must execute: apply this row's action
/// now (the batch op restricted to the row's [`ReviewRow::key`]).
pub enum ReviewAction {
    Apply(String),
}

/// Whether a board offers per-row controls.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RowControls {
    /// Each row can be rejected (RUN skips it) or applied on its own. Only
    /// valid when the caller actually honors [`ReviewState::rejected`] and can
    /// run a single row.
    Enabled,
    /// A read-only preview: the run is all-or-nothing, so offering a reject
    /// toggle would promise something the caller cannot deliver.
    ReadOnly,
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
    /// Current zero-based page of the visible rows.
    pub page: usize,
    /// Namespaced keys ([`ReviewRow::key`]) of rows the user rejected: RUN
    /// skips them. Cleared whenever the preview is rebuilt.
    pub rejected: std::collections::HashSet<String>,
}

impl Default for ReviewState {
    fn default() -> Self {
        Self {
            sort_col: ReviewCol::Source,
            sort_asc: true,
            show_unchanged: false,
            page: 0,
            rejected: std::collections::HashSet::new(),
        }
    }
}

/// Sort `rows` into the state's column + direction, tie-breaking on the source
/// path so the order is stable. Sorting on a status column groups that side's
/// additions / deletions together.
pub fn sort(rows: &mut [ReviewRow], state: &ReviewState) {
    rows.sort_by(|a, b| {
        let by_source = || {
            a.source_path
                .to_lowercase()
                .cmp(&b.source_path.to_lowercase())
        };
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
/// "show unchanged" toggle, a "previewing first N" note when the sample was
/// capped, a row count with page controls, then the virtualised, click-to-sort
/// four-column diff. `totals` is `[added, removed, unchanged]` full counts,
/// independent of the capped sample.
pub fn table(
    ui: &mut egui::Ui,
    state: &mut ReviewState,
    rows: &mut [ReviewRow],
    totals: [usize; 3],
    source_header: &str,
    // `None` is a declared single-sided board (PURGE and the other single-repo
    // commands): the target columns are left out. A two-sided caller always
    // passes `Some`, so an empty header string can never silently drop them.
    target: Option<&str>,
    controls: RowControls,
) -> Option<ReviewAction> {
    summary(ui, totals, state.rejected.len());
    let interactive = controls == RowControls::Enabled;

    let two_sided = target.is_some();
    let target_header = target.unwrap_or("");
    // A single-sided board has no target columns, so the sort must not sit on
    // one of them.
    if !two_sided && matches!(state.sort_col, ReviewCol::Target | ReviewCol::TargetStatus) {
        state.sort_col = ReviewCol::Source;
        sort(rows, state);
    }

    // The unchanged toggle only matters when there are unchanged rows to hide.
    if totals[2] > 0 {
        let label = format!(
            "{} UNCHANGED",
            if state.show_unchanged { "HIDE" } else { "SHOW" }
        );
        if crate::lcars::toggle_button(ui, &label, state.show_unchanged, theme::GREY).clicked() {
            state.show_unchanged = !state.show_unchanged;
            state.page = 0;
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
                "previewing the first {} of {full} — refine the filter to see the rest",
                rows.len()
            ))
            .color(theme::TAN)
            .size(11.0),
        );
    }

    // Page the visible rows; the count line and PREV/NEXT controls always tell
    // the user where they are, even for a single page.
    let pages = visible.len().div_ceil(PAGE_SIZE).max(1);
    state.page = state.page.min(pages - 1);
    let page_rows =
        &visible[state.page * PAGE_SIZE..(visible.len().min((state.page + 1) * PAGE_SIZE))];
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!("{} rows", visible.len()))
                .color(theme::TAN)
                .size(11.0),
        );
        if pages > 1 {
            if crate::lcars::action_button(ui, "PREV", state.page > 0, theme::LILAC).clicked() {
                state.page -= 1;
            }
            ui.label(
                RichText::new(format!("PAGE {} / {pages}", state.page + 1))
                    .color(theme::TEXT)
                    .size(11.0),
            );
            if crate::lcars::action_button(ui, "NEXT", state.page + 1 < pages, theme::LILAC)
                .clicked()
            {
                state.page += 1;
            }
        }
    });
    ui.add_space(2.0);

    let cols: &[(ReviewCol, &str)] = if two_sided {
        &[
            (ReviewCol::SourceStatus, "STATUS"),
            (ReviewCol::Source, source_header),
            (ReviewCol::Target, target_header),
            (ReviewCol::TargetStatus, "STATUS"),
        ]
    } else {
        &[
            (ReviewCol::SourceStatus, "STATUS"),
            (ReviewCol::Source, source_header),
        ]
    };
    let (sort_col, sort_asc) = (state.sort_col, state.sort_asc);
    let mut clicked: Option<ReviewCol> = None;
    let mut action: Option<ReviewAction> = None;
    let mut toggle_reject: Option<String> = None;

    let mut table = TableBuilder::new(ui)
        .striped(true)
        .resizable(true)
        .cell_layout(Layout::left_to_right(Align::Center));
    if interactive {
        table = table.column(Column::exact(76.0));
    }
    table = table.column(Column::initial(120.0).at_least(90.0).clip(true));
    if two_sided {
        table = table
            .column(
                Column::initial(340.0)
                    .at_least(140.0)
                    .clip(true)
                    .resizable(true),
            )
            .column(
                Column::remainder()
                    .at_least(140.0)
                    .clip(true)
                    .resizable(true),
            )
            .column(Column::initial(120.0).at_least(90.0).clip(true));
    } else {
        // One repo: the source path takes everything the two fixed columns
        // leave over.
        table = table.column(Column::remainder().at_least(140.0).clip(true));
    }
    table
        .header(24.0, |mut header| {
            if interactive {
                header.col(|ui| {
                    ui.label(RichText::new("REVIEW").color(theme::TEXT).size(12.0));
                });
            }
            for &(col, title) in cols {
                header.col(|ui| {
                    if crate::util::sort_header(ui, title, sort_col == col, sort_asc).clicked() {
                        clicked = Some(col);
                    }
                });
            }
        })
        .body(|body| {
            // Tall enough for the row's action buttons to fit uncut.
            body.rows(26.0, page_rows.len(), |mut row| {
                let r = &rows[page_rows[row.index()]];
                let key = r.key();
                let rejected = interactive && state.rejected.contains(&key);
                if interactive {
                    row.col(|ui| {
                        if !r.is_actionable() {
                            return;
                        }
                        // Reject toggle: red ✗, filled while the row is rejected.
                        let x = egui::Button::new(RichText::new(icon::X).color(if rejected {
                            theme::BLACK
                        } else {
                            theme::RED
                        }))
                        .small()
                        .fill(if rejected {
                            theme::RED
                        } else {
                            theme::PANEL
                        });
                        if ui
                            .add(x)
                            .on_hover_text(if rejected {
                                "Restore this action (it runs again with RUN)"
                            } else {
                                "Reject this action — RUN will skip it"
                            })
                            .clicked()
                        {
                            toggle_reject = Some(key.clone());
                        }
                        // Apply: execute just this row, right now.
                        if !rejected
                            && ui
                                .add(
                                    egui::Button::new(
                                        RichText::new(icon::ARROW_RIGHT).color(theme::GREEN),
                                    )
                                    .small()
                                    .fill(theme::PANEL),
                                )
                                .on_hover_text("Apply only this action, immediately")
                                .clicked()
                        {
                            action = Some(ReviewAction::Apply(key.clone()));
                        }
                    });
                }
                row.col(|ui| status_cell(ui, r.source, rejected));
                row.col(|ui| path_cell(ui, r.source, &r.source_path, rejected));
                if two_sided {
                    row.col(|ui| path_cell(ui, r.target, &r.target_path, rejected));
                    row.col(|ui| status_cell(ui, r.target, rejected));
                }
            });
        });

    if let Some(key) = toggle_reject
        && !state.rejected.remove(&key)
    {
        state.rejected.insert(key);
    }
    if let Some(col) = clicked {
        if state.sort_col == col {
            state.sort_asc = !state.sort_asc;
        } else {
            state.sort_col = col;
            state.sort_asc = true;
        }
        state.page = 0;
        sort(rows, state);
    }
    action
}

/// One side's status cell: the coloured status icon and what it means, dimmed
/// for a rejected row. An absent side keeps its bare `✗` — there is nothing to
/// spell out.
fn status_cell(ui: &mut egui::Ui, status: SideStatus, rejected: bool) {
    let color = if rejected {
        theme::HAIRLINE
    } else {
        status.color()
    };
    ui.label(RichText::new(status.symbol()).color(color).size(15.0));
    if !status.word().is_empty() {
        ui.add(
            egui::Label::new(RichText::new(status.word()).color(color).size(11.0))
                .truncate()
                .selectable(false),
        );
    }
}

/// One side's path cell: the relative path coloured by that side's status.
/// Absent sides (or empty paths) render nothing; a rejected row's path is
/// dimmed and struck through.
fn path_cell(ui: &mut egui::Ui, status: SideStatus, path: &str, rejected: bool) {
    if status == SideStatus::Absent || path.is_empty() {
        return;
    }
    let mut text = RichText::new(path).color(if rejected {
        theme::HAIRLINE
    } else {
        status.color()
    });
    if rejected {
        text = text.strikethrough();
    }
    ui.add(egui::Label::new(text).truncate());
}

/// The summary line: `«icon» «count» «label»` for each non-zero status, in its
/// colour, using the true full totals `[added, removed, unchanged]` — plus the
/// rejected count when any rows are rejected.
fn summary(ui: &mut egui::Ui, totals: [usize; 3], rejected: usize) {
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
        if rejected > 0 {
            ui.label(
                RichText::new(format!("{} {rejected} rejected — RUN skips these", icon::X))
                    .color(theme::HAIRLINE)
                    .strong(),
            );
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
    fn row_key_namespaces_source_and_target_rows() {
        // A copy row acts on the source side; a sync deletion (no source
        // path) acts on the target side. Keys must match the core ops'.
        let copy = row(SideStatus::Unchanged, SideStatus::Added, "a.txt", "a.txt");
        assert_eq!(copy.key(), dedup_core::diff::source_key("a.txt"));
        let del = row(SideStatus::Absent, SideStatus::Removed, "", "b.txt");
        assert_eq!(del.key(), dedup_core::diff::target_key("b.txt"));
        assert_ne!(
            row(SideStatus::Unchanged, SideStatus::Added, "x", "x").key(),
            row(SideStatus::Absent, SideStatus::Removed, "", "x").key(),
            "source- and target-side rows with the same rel path never collide"
        );
    }

    #[test]
    fn is_unchanged_requires_both_sides() {
        assert!(row(SideStatus::Unchanged, SideStatus::Unchanged, "a", "a").is_unchanged());
        assert!(!row(SideStatus::Unchanged, SideStatus::Added, "a", "a").is_unchanged());
    }

    /// One-sidedness is now a declared mode (`target: None`), not inferred from
    /// an empty header string. So a two-sided board whose header is momentarily
    /// empty keeps its target columns and sort, while a declared one-sided board
    /// resets a target sort onto the source column.
    #[test]
    fn one_sided_is_declared_not_inferred_from_an_empty_header() {
        assert_eq!(
            sort_col_after_table(Some("")),
            ReviewCol::Target,
            "Some(\"\") is two-sided: a target sort survives an empty header"
        );
        assert_eq!(
            sort_col_after_table(None),
            ReviewCol::Source,
            "None is one-sided: the target sort is reset onto the source column"
        );
    }

    /// Render `table` once with the sort on a target column and return where the
    /// sort ended up.
    fn sort_col_after_table(target: Option<&str>) -> ReviewCol {
        use egui_kittest::Harness;
        let state = ReviewState {
            sort_col: ReviewCol::Target,
            ..Default::default()
        };
        let rows = vec![row(SideStatus::Removed, SideStatus::Absent, "a", "")];
        let target = target.map(str::to_string);
        let mut harness = Harness::builder().build_ui_state(
            move |ui, (state, rows): &mut (ReviewState, Vec<ReviewRow>)| {
                crate::theme::apply(ui.ctx());
                table(
                    ui,
                    state,
                    rows,
                    [1, 0, 0],
                    "SRC",
                    target.as_deref(),
                    RowControls::ReadOnly,
                );
            },
            (state, rows),
        );
        harness.run();
        harness.state().0.sort_col
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
            ..Default::default()
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
            ..Default::default()
        };
        sort(&mut rows, &state);
        assert_eq!(rows[0].source_path, "alpha");
        assert_eq!(rows[1].source_path, "zeta");
    }
}
