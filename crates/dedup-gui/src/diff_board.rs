//! What remains of the **diff board** now that DIFF renders on the shared
//! [`crate::board`]: the file operations a row's command maps to, and the three
//! follow-up questions that need an answer before one can run.
//!
//! The rows, their commands and the rendering all live in `board.rs`;
//! `transfer_view`'s `diff_metas` / `diff_action` translate between a
//! [`dedup_core::diff::RepoDiffRow`] and this module's [`BoardAction`].
//!
//! A side holding the same content under several names is narrowed down before
//! it can be renamed, which is what the modals are for:
//!
//! - **delete every copy on this side** — needs confirming
//! - **keep which one?** — picking a path deletes that side's others
//! - **take which name?** — the other side offers several
//!
//! Presentation only: it reports the action the user chose and the caller
//! executes it against the core primitives.

use crate::theme;
use dedup_core::diff::RepoDiffRow;
use egui::RichText;

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

/// What the diff board still owns across frames: the follow-up question a row's
/// button opened, if any. Sorting, filtering and paging moved to the shared
/// board when DIFF was routed onto it.
#[derive(Default)]
pub struct BoardState {
    pub popup: Option<Popup>,
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

/// Render the open popup, if any, and turn the user's answer into an action.
pub fn popup(
    ui: &mut egui::Ui,
    state: &mut BoardState,
    rows: &[RepoDiffRow],
) -> Option<BoardAction> {
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
                theme::red(),
            ),
            PopupKind::KeepOne => (
                "KEEP ONE COPY",
                "Pick the copy to keep — every other file listed here is deleted. \
                 This cannot be undone.",
                theme::red(),
            ),
            PopupKind::PickName => (
                "TAKE WHICH NAME?",
                "The other repository holds this content under several names. Pick \
                 the one this file should take.",
                theme::tan(),
            ),
        };
        ui.label(RichText::new(title).color(title_color).size(16.0).strong());
        ui.add_space(6.0);
        ui.colored_label(theme::text(), blurb);
        ui.add_space(10.0);

        match kind {
            PopupKind::ConfirmDeleteAll => {
                for file in here {
                    ui.colored_label(theme::red(), &file.rel_path);
                }
                ui.add_space(10.0);
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new("DELETE ALL").color(theme::ink_on(theme::red())),
                        )
                        .fill(theme::red()),
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
                            RichText::new(&file.rel_path).color(theme::text()),
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
                            RichText::new(&file.rel_path).color(theme::text()),
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
            .button(RichText::new("CANCEL").color(theme::black()))
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
