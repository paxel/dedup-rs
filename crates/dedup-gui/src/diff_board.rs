//! The **diff board**: the Transfer tab's DIFF command rendered as a
//! side-by-side comparison of two repositories.
//!
//! | ⟨left actions⟩ | ⟨left path⟩ | SIZE | MODIFIED | ⟨right path⟩ | SIZE | MODIFIED | ⟨right actions⟩ |
//!
//! One row per pairing key (content or path, see [`dedup_core::diff::DiffPairing`]).
//! Each side lists everything that repo holds for that key, so a side holding
//! the same content twice shows both names. The buttons offered depend on what
//! the row's two sides say about each other:
//!
//! - only one side has it → **COPY** it across, or **DELETE** it here
//! - same content, different names → **RENAME** either side to the other's name
//! - same name, different content → **OVERWRITE** either side with the other,
//!   or **DELETE** one
//! - equal → nothing to do (equal rows are hidden by default)
//!
//! Built on the same virtualised, click-to-sort `egui_extras` table as the
//! review board, paged [`PAGE_SIZE`] rows at a time. This module is
//! presentation only: it reports the action the user clicked and the caller
//! executes it against the core primitives.

use crate::icon;
use crate::theme;
use crate::util::{format_mtime, format_size};
use dedup_core::diff::{DiffFile, DiffRelation, RepoDiffRow};
use egui::{Align, Layout, RichText};
use egui_extras::{Column, TableBuilder};

/// Rows shown per page.
const PAGE_SIZE: usize = 500;

/// Height of one file line inside a row.
const LINE_HEIGHT: f32 = 26.0;

/// Which column the board is sorted on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BoardCol {
    LeftPath,
    LeftSize,
    LeftModified,
    RightPath,
    RightSize,
    RightModified,
}

/// Which follow-up question a row's button opened.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PopupKind {
    /// "Delete every copy on this side" — needs confirming.
    ConfirmDeleteAll,
    /// "Keep which one?" — picking a path deletes that side's others.
    KeepOne,
    /// "Take which name?" — picking one of the other side's names renames.
    PickName,
}

/// An open popup: the row it belongs to, the side it acts on, and what it asks.
pub struct Popup {
    /// Index into the rows slice the board was rendered with.
    pub row: usize,
    pub on_left: bool,
    pub kind: PopupKind,
}

/// Per-view board state, persisted across frames.
pub struct BoardState {
    pub sort_col: BoardCol,
    pub sort_asc: bool,
    /// Whether the (least interesting) equal rows are shown. Off by default.
    pub show_equal: bool,
    /// Current zero-based page of the visible rows.
    pub page: usize,
    /// The follow-up question currently on screen, if any.
    pub popup: Option<Popup>,
}

impl Default for BoardState {
    fn default() -> Self {
        Self {
            sort_col: BoardCol::LeftPath,
            sort_asc: true,
            show_equal: false,
            page: 0,
            popup: None,
        }
    }
}

/// A row action the user clicked; the caller executes it with the matching
/// core primitive and re-plans the diff.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BoardAction {
    /// Copy this file to the other repo, keeping its relative path.
    Copy { from_left: bool, rel_path: String },
    /// Delete this file from its repo.
    Delete { on_left: bool, rel_path: String },
    /// Rename this side's file to the name the other side uses.
    Rename {
        on_left: bool,
        from: String,
        to: String,
    },
    /// Replace the other side's file with this one's content.
    Overwrite {
        from_left: bool,
        from_rel: String,
        to_rel: String,
    },
    /// Delete several files from one side at once (DELETE ALL, or the copies
    /// KEEP 1 did not keep).
    DeleteMany {
        on_left: bool,
        rel_paths: Vec<String>,
    },
    /// Not a file operation: open the two versions side by side.
    Inspect { left_rel: String, right_rel: String },
    /// Not a file operation: a row's button needs a follow-up answer first.
    /// Carries the row it came from, so the popup knows which row it acts on.
    OpenPopup {
        row: usize,
        on_left: bool,
        kind: PopupKind,
    },
}

/// Colour vocabulary shared with the review board: what a row means at a
/// glance — grey for "nothing to do", tan for "needs a decision", green for
/// "content only one side has".
fn relation_color(relation: DiffRelation) -> egui::Color32 {
    match relation {
        DiffRelation::Equal => theme::GREY,
        DiffRelation::Renamed | DiffRelation::Conflict => theme::TAN,
        DiffRelation::OnlyLeft | DiffRelation::OnlyRight => theme::GREEN,
    }
}

/// Sort `rows` by the state's column and direction, tie-breaking on the left
/// path so the order is stable.
pub fn sort(rows: &mut [RepoDiffRow], state: &BoardState) {
    let first = |files: &[DiffFile]| files.first().cloned();
    rows.sort_by(|a, b| {
        let path = |files: &[DiffFile]| {
            files
                .first()
                .map(|f| f.rel_path.to_lowercase())
                .unwrap_or_default()
        };
        let by_left = || path(&a.left).cmp(&path(&b.left));
        let ord = match state.sort_col {
            BoardCol::LeftPath => by_left(),
            BoardCol::RightPath => path(&a.right).cmp(&path(&b.right)).then_with(by_left),
            BoardCol::LeftSize => first(&a.left)
                .map(|f| f.size)
                .cmp(&first(&b.left).map(|f| f.size))
                .then_with(by_left),
            BoardCol::RightSize => first(&a.right)
                .map(|f| f.size)
                .cmp(&first(&b.right).map(|f| f.size))
                .then_with(by_left),
            BoardCol::LeftModified => first(&a.left)
                .map(|f| f.modified_ms)
                .cmp(&first(&b.left).map(|f| f.modified_ms))
                .then_with(by_left),
            BoardCol::RightModified => first(&a.right)
                .map(|f| f.modified_ms)
                .cmp(&first(&b.right).map(|f| f.modified_ms))
                .then_with(by_left),
        };
        if state.sort_asc { ord } else { ord.reverse() }
    });
}

/// How many rows each relation accounts for, for the summary line.
fn totals(rows: &[RepoDiffRow]) -> [usize; 4] {
    let mut counts = [0usize; 4];
    for row in rows {
        let slot = match row.relation {
            DiffRelation::Equal => 0,
            DiffRelation::Renamed => 1,
            DiffRelation::Conflict => 2,
            DiffRelation::OnlyLeft | DiffRelation::OnlyRight => 3,
        };
        counts[slot] += 1;
    }
    counts
}

/// Render the diff board: a summary line, the equal-rows toggle, page controls
/// and the table itself. Returns the action the user clicked, if any.
pub fn board(
    ui: &mut egui::Ui,
    state: &mut BoardState,
    rows: &[RepoDiffRow],
    left_header: &str,
    right_header: &str,
) -> Option<BoardAction> {
    summary(ui, totals(rows));

    let counts = totals(rows);
    if counts[0] > 0 {
        let label = format!("{} EQUAL", if state.show_equal { "HIDE" } else { "SHOW" });
        if crate::lcars::toggle_button(ui, &label, state.show_equal, theme::GREY).clicked() {
            state.show_equal = !state.show_equal;
            state.page = 0;
        }
    }

    let visible: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| state.show_equal || r.relation != DiffRelation::Equal)
        .map(|(i, _)| i)
        .collect();

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

    let cols = [
        (BoardCol::LeftPath, left_header),
        (BoardCol::LeftSize, "SIZE"),
        (BoardCol::LeftModified, "MODIFIED"),
        (BoardCol::RightPath, right_header),
        (BoardCol::RightSize, "SIZE"),
        (BoardCol::RightModified, "MODIFIED"),
    ];
    let (sort_col, sort_asc) = (state.sort_col, state.sort_asc);
    let mut clicked: Option<BoardCol> = None;
    let mut action: Option<BoardAction> = None;

    TableBuilder::new(ui)
        .striped(true)
        .resizable(true)
        .cell_layout(Layout::left_to_right(Align::Center))
        // left actions, left path/size/date, right path/size/date, right actions
        .column(Column::exact(120.0))
        .column(Column::initial(240.0).at_least(120.0).clip(true))
        .column(Column::initial(80.0).at_least(60.0).clip(true))
        .column(Column::initial(120.0).at_least(80.0).clip(true))
        .column(Column::remainder().at_least(120.0).clip(true))
        .column(Column::initial(80.0).at_least(60.0).clip(true))
        .column(Column::initial(120.0).at_least(80.0).clip(true))
        .column(Column::exact(120.0))
        .header(24.0, |mut header| {
            header.col(|ui| {
                ui.label(RichText::new("ACTIONS").color(theme::TEXT).size(12.0));
            });
            for (col, title) in cols {
                header.col(|ui| {
                    if crate::util::sort_header(ui, title, sort_col == col, sort_asc).clicked() {
                        clicked = Some(col);
                    }
                });
            }
            header.col(|ui| {
                ui.label(RichText::new("ACTIONS").color(theme::TEXT).size(12.0));
            });
        })
        .body(|body| {
            let heights = page_rows
                .iter()
                .map(|i| row_height(&rows[*i]))
                .collect::<Vec<_>>();
            body.heterogeneous_rows(heights.into_iter(), |mut row| {
                let row_index = page_rows[row.index()];
                let r = &rows[row_index];
                let color = relation_color(r.relation);
                row.col(|ui| {
                    if let Some(a) = side_actions(ui, r, row_index, true) {
                        action = Some(a);
                    }
                });
                row.col(|ui| paths_cell(ui, &r.left, color));
                row.col(|ui| sizes_cell(ui, &r.left, color));
                row.col(|ui| dates_cell(ui, &r.left, color));
                row.col(|ui| paths_cell(ui, &r.right, color));
                row.col(|ui| sizes_cell(ui, &r.right, color));
                row.col(|ui| dates_cell(ui, &r.right, color));
                row.col(|ui| {
                    if let Some(a) = side_actions(ui, r, row_index, false) {
                        action = Some(a);
                    }
                });
            });
        });

    if let Some(col) = clicked {
        if state.sort_col == col {
            state.sort_asc = !state.sort_asc;
        } else {
            state.sort_col = col;
            state.sort_asc = true;
        }
        state.page = 0;
    }
    // A button that needs a follow-up answer opens a popup instead of acting.
    // The action carries the row it came from, so no side channel is needed.
    if let Some(BoardAction::OpenPopup { row, on_left, kind }) = &action {
        state.popup = Some(Popup {
            row: *row,
            on_left: *on_left,
            kind: *kind,
        });
        return None;
    }
    if action.is_some() {
        // Any direct action supersedes a half-answered question.
        state.popup = None;
        return action;
    }
    popup(ui, state, rows)
}

/// Render the open popup, if any, and turn the user's answer into an action.
fn popup(ui: &mut egui::Ui, state: &mut BoardState, rows: &[RepoDiffRow]) -> Option<BoardAction> {
    let open = state.popup.as_ref()?;
    let Some(row) = rows.get(open.row) else {
        state.popup = None;
        return None;
    };
    let (here, there) = if open.on_left {
        (&row.left, &row.right)
    } else {
        (&row.right, &row.left)
    };
    let on_left = open.on_left;
    let kind = open.kind;
    let mut answer: Option<BoardAction> = None;
    let mut close = false;

    egui::Modal::new(egui::Id::new("diff-board-popup")).show(&ui.ctx().clone(), |ui| {
        ui.set_width(460.0);
        let (title, blurb, title_color) = match kind {
            PopupKind::ConfirmDeleteAll => (
                "DELETE ALL COPIES",
                "Delete every one of these files. The content stays in the other \
                 repository. This cannot be undone.",
                theme::RED,
            ),
            PopupKind::KeepOne => (
                "KEEP ONE COPY",
                "Pick the copy to keep — every other file listed here is deleted. \
                 This cannot be undone.",
                theme::RED,
            ),
            PopupKind::PickName => (
                "TAKE WHICH NAME?",
                "The other repository holds this content under several names. Pick \
                 the one this file should take.",
                theme::TAN,
            ),
        };
        ui.label(RichText::new(title).color(title_color).size(16.0).strong());
        ui.add_space(6.0);
        ui.colored_label(theme::TEXT, blurb);
        ui.add_space(10.0);

        match kind {
            PopupKind::ConfirmDeleteAll => {
                for file in here {
                    ui.colored_label(theme::RED, &file.rel_path);
                }
                ui.add_space(10.0);
                if ui
                    .add(
                        egui::Button::new(RichText::new("DELETE ALL").color(theme::BLACK))
                            .fill(theme::RED),
                    )
                    .clicked()
                {
                    answer = Some(BoardAction::DeleteMany {
                        on_left,
                        rel_paths: here.iter().map(|f| f.rel_path.clone()).collect(),
                    });
                }
            }
            PopupKind::KeepOne => {
                for file in here {
                    if ui
                        .add(egui::Button::new(
                            RichText::new(&file.rel_path).color(theme::TEXT),
                        ))
                        .on_hover_text("Keep this one and delete the others")
                        .clicked()
                    {
                        answer = Some(BoardAction::DeleteMany {
                            on_left,
                            rel_paths: here
                                .iter()
                                .filter(|f| f.rel_path != file.rel_path)
                                .map(|f| f.rel_path.clone())
                                .collect(),
                        });
                    }
                }
            }
            PopupKind::PickName => {
                let from = here.first().map(|f| f.rel_path.clone()).unwrap_or_default();
                for file in there {
                    if ui
                        .add(egui::Button::new(
                            RichText::new(&file.rel_path).color(theme::TEXT),
                        ))
                        .on_hover_text("Rename this file to that name")
                        .clicked()
                    {
                        answer = Some(BoardAction::Rename {
                            on_left,
                            from: from.clone(),
                            to: file.rel_path.clone(),
                        });
                    }
                }
            }
        }
        ui.add_space(10.0);
        if ui
            .button(RichText::new("CANCEL").color(theme::BLACK))
            .clicked()
        {
            close = true;
        }
    });

    if answer.is_some() || close {
        state.popup = None;
    }
    answer
}

/// A row is as tall as its longest side (a side can hold the same content
/// under several names).
fn row_height(row: &RepoDiffRow) -> f32 {
    LINE_HEIGHT * row.left.len().max(row.right.len()).max(1) as f32
}

/// The paths one side holds, one per line.
fn paths_cell(ui: &mut egui::Ui, files: &[DiffFile], color: egui::Color32) {
    ui.vertical(|ui| {
        for file in files {
            ui.add(
                egui::Label::new(RichText::new(&file.rel_path).color(color).size(12.0))
                    .truncate()
                    .selectable(false),
            )
            .on_hover_text(&file.rel_path);
        }
    });
}

fn sizes_cell(ui: &mut egui::Ui, files: &[DiffFile], color: egui::Color32) {
    ui.vertical(|ui| {
        for file in files {
            ui.label(
                RichText::new(format_size(file.size))
                    .color(color)
                    .size(12.0),
            );
        }
    });
}

fn dates_cell(ui: &mut egui::Ui, files: &[DiffFile], color: egui::Color32) {
    ui.vertical(|ui| {
        for file in files {
            ui.label(
                RichText::new(format_mtime(file.modified_ms))
                    .color(color)
                    .size(12.0),
            );
        }
    });
}

/// The buttons for one side of a row. `on_left` picks the side; the offer
/// depends on the row's relation and on how many names each side holds — a
/// side with several names is narrowed down first (that is step-by-step
/// duplicate resolution), so only 1:1 rows offer RENAME / OVERWRITE.
fn side_actions(
    ui: &mut egui::Ui,
    row: &RepoDiffRow,
    row_index: usize,
    on_left: bool,
) -> Option<BoardAction> {
    let (here, there) = if on_left {
        (&row.left, &row.right)
    } else {
        (&row.right, &row.left)
    };
    let mut action = None;
    ui.horizontal(|ui| {
        match row.relation {
            DiffRelation::Equal => {}
            DiffRelation::OnlyLeft | DiffRelation::OnlyRight => {
                if here.is_empty() {
                    // This side lacks the content: offer to copy it across.
                    if let Some(file) = there.first()
                        && button(ui, "COPY", theme::GREEN)
                            .on_hover_text("Copy this file into this repository")
                            .clicked()
                    {
                        action = Some(BoardAction::Copy {
                            from_left: !on_left,
                            rel_path: file.rel_path.clone(),
                        });
                    }
                } else if let Some(file) = here.first()
                    && button(ui, "DELETE", theme::RED)
                        .on_hover_text("Delete this file from this repository")
                        .clicked()
                {
                    action = Some(BoardAction::Delete {
                        on_left,
                        rel_path: file.rel_path.clone(),
                    });
                }
            }
            DiffRelation::Renamed => {
                // Several names for the same content on this side: narrow them
                // down first — drop them all, or keep exactly one.
                if here.len() > 1 {
                    if button(ui, "DELETE ALL", theme::RED)
                        .on_hover_text("Delete every copy of this content from this repository")
                        .clicked()
                    {
                        action = Some(BoardAction::OpenPopup {
                            row: row_index,
                            on_left,
                            kind: PopupKind::ConfirmDeleteAll,
                        });
                    }
                    if button(ui, "KEEP 1", theme::TAN)
                        .on_hover_text("Keep one of these copies and delete the others")
                        .clicked()
                    {
                        action = Some(BoardAction::OpenPopup {
                            row: row_index,
                            on_left,
                            kind: PopupKind::KeepOne,
                        });
                    }
                    return;
                }
                // One name here: rename it to the other side's name. When the
                // other side offers several names, the user picks which one.
                let Some(mine) = here.first() else {
                    return;
                };
                if there.len() > 1 {
                    if button(ui, "RENAME", theme::TAN)
                        .on_hover_text("Rename this file to one of the other side's names")
                        .clicked()
                    {
                        action = Some(BoardAction::OpenPopup {
                            row: row_index,
                            on_left,
                            kind: PopupKind::PickName,
                        });
                    }
                } else if let Some(theirs) = there.first()
                    && button(ui, "RENAME", theme::TAN)
                        .on_hover_text(format!("Rename this file to '{}'", theirs.rel_path))
                        .clicked()
                {
                    action = Some(BoardAction::Rename {
                        on_left,
                        from: mine.rel_path.clone(),
                        to: theirs.rel_path.clone(),
                    });
                }
            }
            DiffRelation::Conflict => {
                // Same name, different content: look at both, push this
                // version across, or drop it.
                if let (Some(mine), Some(theirs)) = (here.first(), there.first()) {
                    if on_left
                        && button(ui, "COMPARE", theme::LILAC)
                            .on_hover_text("Show both versions side by side")
                            .clicked()
                    {
                        action = Some(BoardAction::Inspect {
                            left_rel: mine.rel_path.clone(),
                            right_rel: theirs.rel_path.clone(),
                        });
                    }
                    if button(ui, "OVERWRITE", theme::TAN)
                        .on_hover_text("Replace the other repository's file with this one")
                        .clicked()
                    {
                        action = Some(BoardAction::Overwrite {
                            from_left: on_left,
                            from_rel: mine.rel_path.clone(),
                            to_rel: theirs.rel_path.clone(),
                        });
                    }
                    if button(ui, "DELETE", theme::RED)
                        .on_hover_text("Delete this file from this repository")
                        .clicked()
                    {
                        action = Some(BoardAction::Delete {
                            on_left,
                            rel_path: mine.rel_path.clone(),
                        });
                    }
                }
            }
        }
    });
    action
}

/// A small stadium action button in the board's row-action columns.
fn button(ui: &mut egui::Ui, label: &str, color: egui::Color32) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(label).color(color).size(10.0))
            .small()
            .fill(theme::PANEL),
    )
}

/// The summary line: how many rows of each kind the diff found.
fn summary(ui: &mut egui::Ui, counts: [usize; 4]) {
    let entries = [
        (counts[3], "only on one side", theme::GREEN, icon::PLUS),
        (counts[1], "renamed", theme::TAN, icon::ARROW_RIGHT),
        (counts[2], "conflicting", theme::TAN, icon::X),
        (counts[0], "equal", theme::GREY, icon::CHECK),
    ];
    ui.horizontal_wrapped(|ui| {
        for (n, label, color, glyph) in entries {
            if n == 0 {
                continue;
            }
            ui.label(
                RichText::new(format!("{glyph} {n} {label}"))
                    .color(color)
                    .strong(),
            );
            ui.add_space(10.0);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(rel: &str, size: u64, ms: i64) -> DiffFile {
        DiffFile {
            rel_path: rel.to_string(),
            size,
            modified_ms: ms,
        }
    }

    fn row(relation: DiffRelation, left: Vec<DiffFile>, right: Vec<DiffFile>) -> RepoDiffRow {
        RepoDiffRow {
            relation,
            left,
            right,
        }
    }

    #[test]
    fn sort_by_left_size_orders_by_the_first_file_and_toggles() {
        let mut rows = vec![
            row(
                DiffRelation::OnlyLeft,
                vec![file("big.bin", 900, 1)],
                Vec::new(),
            ),
            row(
                DiffRelation::OnlyLeft,
                vec![file("small.bin", 10, 2)],
                Vec::new(),
            ),
        ];
        let mut state = BoardState {
            sort_col: BoardCol::LeftSize,
            ..Default::default()
        };
        sort(&mut rows, &state);
        assert_eq!(rows[0].left[0].rel_path, "small.bin");
        state.sort_asc = false;
        sort(&mut rows, &state);
        assert_eq!(rows[0].left[0].rel_path, "big.bin");
    }

    #[test]
    fn a_side_holding_several_names_makes_the_row_taller() {
        let one = row(
            DiffRelation::Renamed,
            vec![file("a.txt", 1, 1)],
            vec![file("b.txt", 1, 1)],
        );
        let three = row(
            DiffRelation::Renamed,
            vec![
                file("a.txt", 1, 1),
                file("b.txt", 1, 1),
                file("c.txt", 1, 1),
            ],
            vec![file("z.txt", 1, 1)],
        );
        assert_eq!(row_height(&one), LINE_HEIGHT);
        assert_eq!(row_height(&three), LINE_HEIGHT * 3.0);
    }

    #[test]
    fn totals_count_each_relation() {
        let rows = vec![
            row(DiffRelation::Equal, Vec::new(), Vec::new()),
            row(DiffRelation::Renamed, Vec::new(), Vec::new()),
            row(DiffRelation::Conflict, Vec::new(), Vec::new()),
            row(DiffRelation::OnlyLeft, Vec::new(), Vec::new()),
            row(DiffRelation::OnlyRight, Vec::new(), Vec::new()),
        ];
        assert_eq!(totals(&rows), [1, 1, 1, 2]);
    }
}
