//! The Transfer tab: pick a source repo and a target repo, choose a command
//! (copy / move / sync), narrow with a filter, preview the first `from → to`
//! transfers, then run it on a background thread with confirmation.
//!
//! Semantics reuse the core diff operations (content compared by size + hash):
//! - **Copy/Move** transfer source files whose content the target lacks into the
//!   target repo's directory (move also marks the source entries missing).
//! - **Sync** mirrors the source into the target at the same relative path:
//!   copy content the target lacks, and (with DELETE MISSING on) delete target
//!   files whose content the source has lost. The source is never changed.
//! - **Diff** compares the two repos side by side (by content or by path) and
//!   leaves every decision to the user: each row offers copy / delete / rename /
//!   overwrite per side, applied one click at a time (see `diff_board.rs`).

use crate::compare_view::{DiffCompare, DiffPick, DiffSide};
use crate::filter_ui::FilterBuilder;
use crate::icon;
use crate::media_cell::{FileFacts, facts_for, open_facts};
use crate::settings::TooltipVerbosity;
use crate::theme;
use crate::thumbs::ThumbCache;
use crate::util::ExplainExt;
use crossbeam_channel::{Receiver, Sender};
use dedup_core::diff::{
    CopyDest, DiffAction, DiffEvent, DiffItem, DiffPairing, DiffProgress, DiffRelation, DiffRun,
    FolderMode, RepoDiffRow, SyncDelete, copy_file_between, delete_file, diff_copy, diff_print,
    diff_sync, export_to_folder, overwrite_file, plan_folder_export, plan_repo_diff, plan_sync,
    rename_file,
};
use dedup_core::store::{Store, SyncGroup, SyncMode};
use dedup_core::sync_group::{delete_mode, guard_mirror_source};
use dedup_core::update::CancellationToken;
use egui::{Id, RichText};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::board;
use crate::board::PREVIEW_CAP;

/// How many recent actions the running panel keeps in its scrolling log.
const RUN_LOG_LIMIT: usize = 10;

/// Longest texture edge uploaded to the GPU for a DIFF preview, matching the
/// lightbox's limit; larger images are downscaled by the decoder to stay within
/// driver limits.

#[derive(PartialEq, Clone, Copy)]
enum Command {
    Copy,
    Move,
    Sync,
    Mirror,
    /// Push the source (a sync group's main) to some or all of its sinks, each
    /// in its own stored mode. Only offered when the source is a group's main.
    GroupSync,
    /// Pull one of this group's sinks back into the main: promote content the
    /// main never had, and offer to resurrect content the main deleted that the
    /// sink still holds. Only offered when the source is a group's main.
    GroupSyncBack,
    Diff,
}

impl Command {
    fn label(self) -> &'static str {
        match self {
            Command::Copy => "COPY",
            Command::Move => "MOVE",
            Command::Sync => "SYNC",
            Command::Mirror => "MIRROR",
            Command::GroupSync => "GROUP SYNC",
            Command::GroupSyncBack => "GROUP SYNC BACK",
            Command::Diff => "DIFF",
        }
    }
    /// Whether the command is inherently destructive to on-disk data by itself.
    /// SYNC is additive by default (it only *copies* into the target); its
    /// optional DELETE MISSING toggle makes a given run destructive — see
    /// [`TransferView::destructive_run`]. MIRROR always deletes. GROUP SYNC's
    /// destructiveness depends on the selected sinks' own modes, so it is not
    /// statically destructive either — see `destructive_run`.
    fn destructive(self) -> bool {
        matches!(self, Command::Move | Command::Mirror)
    }
    /// Whether the command runs repo→repo at the same relative path (SYNC /
    /// MIRROR / GROUP SYNC), which hides the DEST / subdir / folder / dupe-pool
    /// controls.
    fn repo_to_repo(self) -> bool {
        matches!(
            self,
            Command::Sync
                | Command::Mirror
                | Command::GroupSync
                | Command::GroupSyncBack
                | Command::Diff
        )
    }
    /// DIFF is a manual side-by-side view rather than a batch run: it has no
    /// filter, no RUN button and no confirmation — every change is made by
    /// clicking a single row's action.
    fn is_diff(self) -> bool {
        matches!(self, Command::Diff)
    }
    /// (short, verbose) tooltip text for this command's selector button.
    fn tooltip(self) -> (&'static str, &'static str) {
        match self {
            Command::Copy => (
                "Copy files the target doesn't have",
                "Copy source files whose content the target (and any ALSO REF repos) \
                 doesn't already have into the target repo's directory. Source files are \
                 left in place.",
            ),
            Command::Move => (
                "Move files the target doesn't have",
                "Move source files whose content the target (and any ALSO REF repos) \
                 doesn't already have into the target repo's directory, marking the \
                 source entries missing.",
            ),
            Command::Sync => (
                "Copy the source into the target",
                "Copy source content the target lacks into the target at the same \
                 relative path. Turn on DELETE MISSING to also delete target files \
                 whose content the source has since lost. The source is never changed.",
            ),
            Command::Mirror => (
                "Make the target an exact copy of the source",
                "Copy source content the target lacks AND delete everything in the target \
                 the source does not have, so the target ends up holding exactly the \
                 source's content. Deletions cannot be undone. The source is never changed.",
            ),
            Command::GroupSync => (
                "Push this group's main to its sinks",
                "Push the source (this group's main) to the selected sinks below, each in \
                 its own stored mode — ADD ONLY copies and never deletes, MIRROR also \
                 deletes what the main no longer has. The main is never changed.",
            ),
            Command::GroupSyncBack => (
                "Pull a sink's changes back into this main",
                "Pull one of this group's sinks back into the main: promote content the \
                 main never had (files you added straight to the backup), and offer to \
                 bring back content the main deleted that the sink still holds — a \
                 resurrection, marked in blue, that you choose file by file. Nothing on \
                 the sink is changed.",
            ),
            Command::Diff => (
                "Compare the two repos side by side",
                "Compare the source and target repository file by file and resolve the \
                 differences one row at a time: copy what only one side has, delete it, \
                 rename a file to the other side's name, or overwrite one side with the \
                 other. Nothing happens until you click a row's button.",
            ),
        }
    }
}

/// Where a COPY/MOVE lands: into another repo, or into a plain folder.
#[derive(PartialEq, Clone, Copy)]
enum Destination {
    /// Into the target repo (content compared against target + references).
    Repo,
    /// Into a user-picked folder (a deduplicated selection of the source).
    Folder,
}

/// How a folder export groups the source to pick which copies to keep.
#[derive(PartialEq, Clone, Copy)]
enum SelectMode {
    /// Exact-content duplicate groups.
    Exact,
    /// Perceptual-similarity groups (at this tab's own similarity threshold).
    Similar,
}

impl SelectMode {
    fn label(self) -> &'static str {
        match self {
            SelectMode::Exact => "EXACT",
            SelectMode::Similar => "SIMILAR",
        }
    }
}

/// A snapshot of the destination captured when a run starts, so the worker
/// thread owns everything it needs without borrowing the view.
#[derive(Clone)]
enum StartDest {
    Repo {
        references: Vec<String>,
        target: String,
        subdir: String,
    },
    Folder {
        references: Vec<String>,
        dir: PathBuf,
        mode: FolderMode,
        invert: bool,
    },
    Sync {
        target: String,
        delete: SyncDelete,
        mirror: bool,
    },
}

/// Everything a REVIEW, its confirmation, and the RUN it authorises need,
/// snapshotted at the moment the user asks — so nothing the live controls do
/// between an async plan landing and PROCEED can change what actually runs.
/// (Same reasoning as `pending_group_confirm` for GROUP SYNC.)
#[derive(Clone)]
struct RunConfig {
    source: String,
    command: Command,
    dest: StartDest,
    filter: Option<String>,
    move_files: bool,
}

/// The result of a board preview (Copy/Move/Sync/folder), built off the UI
/// thread. Rows come back unsorted; the board sorts them when applied.
struct ReviewPreviewData {
    rows: Vec<board::RowMeta>,
    bodies: Vec<board::RowBody>,
    preview_total: usize,
    sync_delete_total: usize,
    /// `[to-delete, only-here, differing, unchanged]`.
    preview_totals: [usize; 4],
    source_header: String,
    target_header: String,
    status: String,
}

/// One side of a board row, as a preview builder describes it.
#[derive(Default)]
struct SideSpec {
    status: Option<board::Status>,
    path: Option<String>,
    facts: Option<FileFacts>,
    /// Only set when the board's side spans several repos (GROUP SYNC's sinks),
    /// where each row names its own.
    repo: Option<String>,
    /// This side's own status veil (the golden rule: a cell only talks about
    /// itself) — green NEW where a file arrives, red WILL DELETE on the file a
    /// plan removes, blue WAS DELETED where this side deleted the content.
    overlay: Option<crate::media_cell::CellOverlay>,
}

impl SideSpec {
    fn absent() -> Self {
        Self::default()
    }
    fn at(status: board::Status, path: &str, facts: Option<FileFacts>) -> Self {
        Self {
            status: Some(status),
            path: Some(path.to_string()),
            facts,
            repo: None,
            overlay: None,
        }
    }
    fn in_repo(mut self, repo: &str) -> Self {
        self.repo = Some(repo.to_string());
        self
    }
    fn veiled(mut self, overlay: crate::media_cell::CellOverlay) -> Self {
        self.overlay = Some(overlay);
        self
    }
    /// A tombstone side: no file, no path — just the blue "this side deleted
    /// exactly this content" cell.
    fn tombstone() -> Self {
        Self {
            status: Some(board::Status::Resurrect),
            path: None,
            facts: None,
            repo: None,
            overlay: Some(crate::media_cell::CellOverlay::WasDeleted),
        }
    }
}

/// Assemble one board row and its body from the two side descriptions, keeping
/// the two collections the board takes index-aligned.
///
/// `key` namespaces the row the way the core ops do (see `dedup_core::diff`):
/// the source path for source-side actions, the target path for rows that only
/// exist on the target (a sync deletion).
fn board_row(
    left: SideSpec,
    right: SideSpec,
    unchanged: bool,
    cmds: Vec<board::Cmd>,
) -> (board::RowMeta, board::RowBody) {
    let key = match (&left.path, &right.path) {
        (Some(p), _) => dedup_core::diff::source_key(p),
        (None, Some(p)) => dedup_core::diff::target_key(p),
        (None, None) => String::new(),
    };
    let meta = board::RowMeta {
        key,
        left_status: left.status.unwrap_or(board::Status::Absent),
        right_status: right.status.unwrap_or(board::Status::Absent),
        left_size: left.facts.as_ref().map(|f| f.size).unwrap_or(0),
        right_size: right.facts.as_ref().map(|f| f.size).unwrap_or(0),
        left_modified: left.facts.as_ref().map(|f| f.modified_ms).unwrap_or(0),
        right_modified: right.facts.as_ref().map(|f| f.modified_ms).unwrap_or(0),
        left_paths: left.path.into_iter().collect(),
        right_paths: right.path.into_iter().collect(),
        unchanged,
        cmds,
    };
    let body = board::RowBody {
        left: board::SideBody {
            facts: left.facts,
            repo: left.repo,
            repo_is_main: false,
            overlay: left.overlay,
        },
        right: board::SideBody {
            facts: right.facts,
            repo: right.repo,
            repo_is_main: false,
            overlay: right.overlay,
        },
    };
    (meta, body)
}

/// The commands a planned preview offers per row: run this one now, or drop it
/// from the board and from what RUN will do.
fn planned_cmds() -> Vec<board::Cmd> {
    vec![board::Cmd::Apply, board::Cmd::Hide]
}

/// DIFF's rows as the board's cheap model. What a row offers depends on what
/// its two sides say about each other; a side holding the same content under
/// several names is narrowed down first, so only 1:1 rows offer RENAME.
/// A batch operation over every DIFF row currently listed. Offered only when
/// the listed rows actually contain the relation it acts on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BulkOp {
    /// Copy everything only the left side has into the right repo.
    CopyMissingRight,
    /// Copy everything only the right side has into the left repo.
    CopyMissingLeft,
    /// Rename each left file to the name the right side uses.
    RenameAllLeft,
    /// Rename each right file to the name the left side uses.
    RenameAllRight,
}

impl BulkOp {
    fn label(self) -> &'static str {
        match self {
            BulkOp::CopyMissingRight => "COPY MISSING >",
            BulkOp::CopyMissingLeft => "< COPY MISSING",
            BulkOp::RenameAllLeft => "RENAME ALL L",
            BulkOp::RenameAllRight => "RENAME ALL R",
        }
    }

    fn describe(self, n: usize) -> String {
        match self {
            BulkOp::CopyMissingRight => {
                format!("Copy {n} file(s) the right side does not have into it?")
            }
            BulkOp::CopyMissingLeft => {
                format!("Copy {n} file(s) the left side does not have into it?")
            }
            BulkOp::RenameAllLeft => {
                format!("Rename {n} file(s) on the left to the right side's names?")
            }
            BulkOp::RenameAllRight => {
                format!("Rename {n} file(s) on the right to the left side's names?")
            }
        }
    }
}

/// Whether a board command is allowed under the session locks: commands that
/// delete or overwrite a side's *existing* files need that side unlocked;
/// additions (copies), renames, compare and hide are always allowed.
fn cmd_allowed(cmd: board::Cmd, left_ro: bool, right_ro: bool) -> bool {
    use board::Cmd;
    match cmd {
        Cmd::DeleteLeft | Cmd::DeleteAllLeft | Cmd::KeepOneLeft | Cmd::OverwriteLeft => !left_ro,
        Cmd::DeleteRight | Cmd::DeleteAllRight | Cmd::KeepOneRight | Cmd::OverwriteRight => {
            !right_ro
        }
        _ => true,
    }
}

fn diff_metas(rows: &[RepoDiffRow], left_ro: bool, right_ro: bool) -> Vec<board::RowMeta> {
    use board::{Cmd, Status};
    use dedup_core::diff::DiffRelation as R;
    rows.iter()
        .map(|row| {
            let paths = |files: &[dedup_core::diff::DiffFile]| -> Vec<String> {
                files.iter().map(|f| f.rel_path.clone()).collect()
            };
            let (left_status, right_status, mut cmds) = match row.relation {
                R::Equal => (Status::Same, Status::Same, Vec::new()),
                // The golden rule: the holder's cell shows its file plain (its
                // green path already says "only here"). When the *other* repo
                // once held this content and deleted it, that side — not the
                // living file — is marked Resurrect: it renders a blue
                // WAS DELETED tombstone cell, and copying across becomes an
                // informed decision.
                R::OnlyLeft => (
                    Status::OnlyHere,
                    if row.deleted_in_right {
                        Status::Resurrect
                    } else {
                        Status::Absent
                    },
                    vec![Cmd::CopyRight, Cmd::DeleteLeft],
                ),
                R::OnlyRight => (
                    if row.deleted_in_left {
                        Status::Resurrect
                    } else {
                        Status::Absent
                    },
                    Status::OnlyHere,
                    vec![Cmd::CopyLeft, Cmd::DeleteRight],
                ),
                R::Renamed => {
                    let mut c = Vec::new();
                    if row.left.len() > 1 {
                        c.push(Cmd::DeleteAllLeft);
                        c.push(Cmd::KeepOneLeft);
                    } else if !row.left.is_empty() {
                        c.push(Cmd::RenameLeft);
                    }
                    if row.right.len() > 1 {
                        c.push(Cmd::DeleteAllRight);
                        c.push(Cmd::KeepOneRight);
                    } else if !row.right.is_empty() {
                        c.push(Cmd::RenameRight);
                    }
                    (Status::Differs, Status::Differs, c)
                }
                R::Conflict => (
                    Status::Differs,
                    Status::Differs,
                    vec![
                        Cmd::Compare,
                        Cmd::OverwriteRight,
                        Cmd::OverwriteLeft,
                        Cmd::DeleteLeft,
                        Cmd::DeleteRight,
                    ],
                ),
            };
            cmds.push(Cmd::Hide);
            // The review shows only the buttons the session locks allow — a
            // command that would delete or overwrite in a locked repo is left
            // out, not shown disabled (unlock the repo's padlock to get it).
            cmds.retain(|&c| cmd_allowed(c, left_ro, right_ro));
            let first = |files: &[dedup_core::diff::DiffFile]| files.first().cloned();
            board::RowMeta {
                key: format!(
                    "{}|{}",
                    row.left.first().map(|f| f.rel_path.as_str()).unwrap_or(""),
                    row.right.first().map(|f| f.rel_path.as_str()).unwrap_or(""),
                ),
                left_status,
                right_status,
                left_size: first(&row.left).map(|f| f.size).unwrap_or(0),
                right_size: first(&row.right).map(|f| f.size).unwrap_or(0),
                left_modified: first(&row.left).map(|f| f.modified_ms).unwrap_or(0),
                right_modified: first(&row.right).map(|f| f.modified_ms).unwrap_or(0),
                left_paths: paths(&row.left),
                right_paths: paths(&row.right),
                unchanged: row.relation == R::Equal,
                cmds,
            }
        })
        .collect()
}

/// DIFF's summary counts in the board's `[to-delete, only-here, differing,
/// unchanged]` order. A diff plans nothing, so nothing is "to delete".
fn diff_totals(rows: &[RepoDiffRow]) -> [usize; 4] {
    use dedup_core::diff::DiffRelation as R;
    let mut totals = [0usize; 4];
    for row in rows {
        match row.relation {
            R::Equal => totals[3] += 1,
            R::OnlyLeft | R::OnlyRight => totals[1] += 1,
            R::Renamed | R::Conflict => totals[2] += 1,
        }
    }
    totals
}

/// Translate a board command on DIFF row `i` into the file operation the caller
/// executes. Returns `None` when the row cannot supply what the command needs.
fn diff_action(
    rows: &[RepoDiffRow],
    i: usize,
    cmd: board::Cmd,
) -> Option<crate::diff_board::BoardAction> {
    use crate::diff_board::{BoardAction, PopupKind};
    use board::Cmd;
    let row = rows.get(i)?;
    let left = row.left.first().map(|f| f.rel_path.clone());
    let right = row.right.first().map(|f| f.rel_path.clone());
    let popup = |kind, on_left| {
        Some(BoardAction::OpenPopup {
            row: i,
            on_left,
            kind,
        })
    };
    match cmd {
        // The row body opens the shared viewer — the law: clicking any file
        // anywhere shows it. Same destination COMPARE used to reach, now without
        // a command competing for row space. A row with only one side opens
        // that file alone (it used to open nothing — `?` on the absent side
        // swallowed the click).
        Cmd::OpenRow | Cmd::Compare => {
            if left.is_none() && right.is_none() {
                None
            } else {
                Some(BoardAction::Inspect {
                    left_rel: left,
                    right_rel: right,
                })
            }
        }
        Cmd::CopyRight => Some(BoardAction::Copy {
            from_left: true,
            rel_path: left?,
        }),
        Cmd::CopyLeft => Some(BoardAction::Copy {
            from_left: false,
            rel_path: right?,
        }),
        Cmd::DeleteLeft => Some(BoardAction::Delete {
            on_left: true,
            rel_path: left?,
        }),
        Cmd::DeleteRight => Some(BoardAction::Delete {
            on_left: false,
            rel_path: right?,
        }),
        Cmd::OverwriteRight => Some(BoardAction::Overwrite {
            from_left: true,
            from_rel: left?,
            to_rel: right?,
        }),
        Cmd::OverwriteLeft => Some(BoardAction::Overwrite {
            from_left: false,
            from_rel: right?,
            to_rel: left?,
        }),
        // Renaming to one of several names on the other side needs an answer
        // first; a 1:1 pair can be renamed outright.
        Cmd::RenameLeft if row.right.len() > 1 => popup(PopupKind::PickName, true),
        Cmd::RenameLeft => Some(BoardAction::Rename {
            on_left: true,
            from: left?,
            to: right?,
        }),
        Cmd::RenameRight if row.left.len() > 1 => popup(PopupKind::PickName, false),
        Cmd::RenameRight => Some(BoardAction::Rename {
            on_left: false,
            from: right?,
            to: left?,
        }),
        Cmd::KeepOneLeft => popup(PopupKind::KeepOne, true),
        Cmd::KeepOneRight => popup(PopupKind::KeepOne, false),
        Cmd::DeleteAllLeft => popup(PopupKind::ConfirmDeleteAll, true),
        Cmd::DeleteAllRight => popup(PopupKind::ConfirmDeleteAll, false),
        // The board handles HIDE itself; APPLY belongs to a planned preview.
        Cmd::Hide | Cmd::Apply => None,
    }
}

/// Plan a review-board preview for `config`, off the UI thread. Dispatches on
/// where the transfer lands; each branch reads the relevant index(es), which is
/// the work that must not block the window on large repos.
fn build_review_preview(store: &Store, config: &RunConfig) -> Result<ReviewPreviewData, String> {
    match &config.dest {
        StartDest::Sync { target, delete, .. } => preview_sync(store, config, target, *delete),
        StartDest::Repo {
            references,
            target,
            subdir,
        } => preview_repo(store, config, references, target, subdir),
        StartDest::Folder {
            references,
            dir,
            mode,
            invert,
        } => preview_folder(store, config, references, dir, *mode, *invert),
    }
}

/// Plan a GROUP SYNC push for `group` (already filtered to the selected
/// sinks), off the UI thread — the full index scan runs per sink, so a large
/// group must not block the window. Calls `plan_sync` directly (not
/// [`dedup_core::sync_group::plan_group_sync`]) so `filter` can be threaded
/// through, which that function does not accept; `guard_mirror_source` is
/// called explicitly to keep its empty-main-mirror refusal.
fn build_group_preview(
    store: &Store,
    group: &SyncGroup,
    filter: Option<&str>,
) -> Result<GroupPreviewData, String> {
    guard_mirror_source(store, group).map_err(|e| e.to_string())?;
    let mut rows = Vec::new();
    let mut bodies = Vec::new();
    let (mut added, mut removed) = (0usize, 0usize);
    let mut wholesale_sinks = Vec::new();
    // Copies carry the main's file, so their facts come from the main index.
    let (main_db, main_base) = open_facts(store, &group.main);
    for sink in &group.sinks {
        let plan = plan_sync(
            store,
            &group.main,
            &sink.repo,
            true,
            delete_mode(sink.mode),
            filter,
        )
        .map_err(|e| e.to_string())?;
        // A plan that deletes everything the sink holds today is a wholesale
        // replacement, not an incremental sync — worth naming before proceeding.
        let live = store
            .get_repo_stats(&sink.repo)
            .map(|s| s.file_count)
            .unwrap_or(0);
        if live > 0 && plan.deletes.len() as u64 >= live {
            wholesale_sinks.push((sink.repo.clone(), live));
        }
        // Deletions carry the sink's file, so their facts come from the sink index.
        let (sink_db, sink_base) = open_facts(store, &sink.repo);
        let sink_idx = content_idx(sink_db.as_deref());
        // The sink rides in the row's own repo chip rather than being folded
        // into the path — `format!("{sink}: {rel}")` made sorting by path sort
        // by sink name, and a rel-path containing ": " was ambiguous.
        //
        // No per-row commands: this run is all-or-nothing (`start_group_sync`
        // builds a `DiffRun` with no selection), so offering HIDE would promise
        // to skip a deletion and then make it anyway.
        for rel in &plan.copies {
            added += 1;
            if rows.len() < PREVIEW_CAP {
                // A push can also resurrect: content this sink deleted that
                // the main still holds comes back. Golden rule: the main's
                // cell stays plain (unchanged); the *sink's* cell shows the
                // incoming preview — green NEW, or blue WAS DELETED when the
                // push would bring back what this sink deleted.
                let resurrect = copy_resurrects(main_db.as_deref(), sink_idx.as_ref(), rel);
                let facts = facts_for(main_db.as_deref(), main_base.as_deref(), rel);
                let (arrive_status, veil) = if resurrect {
                    (board::Status::Resurrect, CO_WAS_DELETED)
                } else {
                    (board::Status::OnlyHere, CO_NEW)
                };
                let (meta, body) = board_row(
                    SideSpec::at(board::Status::Same, rel, facts.clone()),
                    SideSpec::at(arrive_status, rel, facts)
                        .in_repo(&sink.repo)
                        .veiled(veil),
                    false,
                    Vec::new(),
                );
                rows.push(meta);
                bodies.push(body);
            }
        }
        for rel in &plan.deletes {
            removed += 1;
            if rows.len() < PREVIEW_CAP {
                let (meta, body) = board_row(
                    SideSpec::absent(),
                    SideSpec::at(
                        board::Status::WillDelete,
                        rel,
                        facts_for(sink_db.as_deref(), sink_base.as_deref(), rel),
                    )
                    .in_repo(&sink.repo)
                    .veiled(CO_WILL_DELETE),
                    false,
                    Vec::new(),
                );
                rows.push(meta);
                bodies.push(body);
            }
        }
    }
    let sink_count = group.sinks.len();
    Ok(GroupPreviewData {
        group: group.clone(),
        main_header: TransferView::repo_header(store, &group.main),
        rows,
        bodies,
        added,
        removed,
        sink_count,
        wholesale_sinks,
    })
}

/// Plan a GROUP SYNC BACK pull of `sink` into `group.main`, off the UI thread.
/// Rows show the main gaining each file — green ([`board::Status::OnlyHere`]) for
/// a new promote, blue ([`board::Status::Resurrect`]) for content the main
/// deleted that the sink still holds. No per-row commands yet (that, and the run,
/// are the next slice). Reuses [`GroupPreviewData`], carrying `added` = new count
/// and `removed` = resurrection count for the shared render path.
fn build_group_back_preview(
    store: &Store,
    group: &SyncGroup,
    sink: &str,
    filter: Option<&str>,
) -> Result<GroupPreviewData, String> {
    let pull = dedup_core::diff::plan_sync_back(store, sink, &group.main, filter)
        .map_err(|e| e.to_string())?;
    let (sink_db, sink_base) = open_facts(store, sink);
    let mut rows = Vec::new();
    let mut bodies = Vec::new();
    let (mut new_count, mut resurrect_count) = (0usize, 0usize);
    for item in &pull {
        let main_status = match item.kind {
            dedup_core::diff::PullKind::New => {
                new_count += 1;
                board::Status::OnlyHere
            }
            dedup_core::diff::PullKind::Resurrection => {
                resurrect_count += 1;
                board::Status::Resurrect
            }
        };
        if rows.len() < PREVIEW_CAP {
            let sink_facts = facts_for(sink_db.as_deref(), sink_base.as_deref(), &item.rel_path);
            let veil = match item.kind {
                dedup_core::diff::PullKind::New => CO_NEW,
                dedup_core::diff::PullKind::Resurrection => CO_WAS_DELETED,
            };
            let (meta, body) = board_row(
                // Golden rule: the main *receives* — its cell shows the
                // incoming file's preview under green NEW (or blue
                // WAS DELETED for a resurrection: the main deleted this).
                // The sink's own cell shows its file plain; nothing happens
                // to the sink on a promote.
                SideSpec::at(main_status, &item.rel_path, sink_facts.clone()).veiled(veil),
                SideSpec::at(board::Status::Same, &item.rel_path, sink_facts).in_repo(sink),
                false,
                // Each row is a triage decision: `< COPY` pulls this one file
                // into the main (the only way to opt a resurrection in, and a
                // way to promote a single new file), `DELETE R` removes it from
                // the sink instead — everything in the sink is either worth
                // promoting or worth purging. (DELETE R is withheld at render
                // time while the sink is locked.)
                vec![board::Cmd::CopyLeft, board::Cmd::DeleteRight],
            );
            rows.push(meta);
            bodies.push(body);
        }
    }
    Ok(GroupPreviewData {
        group: SyncGroup {
            main: group.main.clone(),
            sinks: group
                .sinks
                .iter()
                .filter(|s| s.repo == sink)
                .cloned()
                .collect(),
        },
        main_header: TransferView::repo_header(store, &group.main),
        rows,
        bodies,
        added: new_count,
        removed: resurrect_count,
        sink_count: 1,
        wholesale_sinks: Vec::new(),
    })
}

/// The target's content index (for tombstone probes), read once per preview;
/// `None` when the target repo can't be opened — everything then reads as
/// "not a resurrection".
type ContentIdx =
    std::collections::HashMap<dedup_core::store::ContentKey, dedup_core::store::ContentState>;

fn content_idx(db: Option<&redb::Database>) -> Option<ContentIdx> {
    db.and_then(|db| dedup_core::store::read_content_index(db).ok())
}

/// Whether copying `rel` (a live source file) into the target would resurrect
/// content the target once held and deleted — i.e. the target index knows the
/// content only as tombstones. Best-effort: unreadable indexes read as "no".
fn copy_resurrects(
    src_db: Option<&redb::Database>,
    tgt_idx: Option<&ContentIdx>,
    rel: &str,
) -> bool {
    let (Some(src), Some(idx)) = (src_db, tgt_idx) else {
        return false;
    };
    let Ok(Some(entry)) = dedup_core::store::get_entry(src, rel) else {
        return false;
    };
    dedup_core::store::content_tombstoned(idx, entry.size, &entry.hash)
}

const CO_NEW: crate::media_cell::CellOverlay = crate::media_cell::CellOverlay::New;
const CO_WAS_DELETED: crate::media_cell::CellOverlay = crate::media_cell::CellOverlay::WasDeleted;
const CO_WILL_DELETE: crate::media_cell::CellOverlay = crate::media_cell::CellOverlay::WillDelete;

fn preview_sync(
    store: &Store,
    config: &RunConfig,
    target: &str,
    delete: SyncDelete,
) -> Result<ReviewPreviewData, String> {
    let plan = plan_sync(
        store,
        &config.source,
        target,
        true,
        delete,
        config.filter.as_deref(),
    )
    .map_err(|e| e.to_string())?;
    // Facts come from whichever side holds the file: a copy's source, a
    // deletion's target.
    let (src_db, src_base) = open_facts(store, &config.source);
    let (tgt_db, tgt_base) = open_facts(store, target);
    // A copy: source keeps the file (unchanged), target gains it (added). A
    // delete: the source no longer has it (absent), the target loses it
    // (removed). Capped.
    let tgt_idx = content_idx(tgt_db.as_deref());
    // The resurrection count is a *whole-plan* safety number (the status line
    // reports whole-plan totals): counted over every copy, never just the
    // capped preview rows, or a big plan would hide exactly the warning the
    // tombstones exist to raise. It is an in-memory map probe — cheap.
    let resurrections = plan
        .copies
        .iter()
        .filter(|rel| copy_resurrects(src_db.as_deref(), tgt_idx.as_ref(), rel))
        .count();
    let (mut rows, mut bodies): (Vec<_>, Vec<_>) = plan
        .copies
        .iter()
        .take(PREVIEW_CAP)
        .map(|rel| {
            // SYNC copies by content the target *currently* lacks — including
            // content it deliberately deleted, which silently undoes a
            // deletion. The golden rule: the source cell shows its file plain
            // (nothing happens to it); the *receiving* cell shows the incoming
            // file's preview under green NEW — or blue WAS DELETED when the
            // copy resurrects (it still runs; the veil informs).
            let resurrect = copy_resurrects(src_db.as_deref(), tgt_idx.as_ref(), rel);
            let facts = facts_for(src_db.as_deref(), src_base.as_deref(), rel);
            let (arrive_status, veil) = if resurrect {
                (board::Status::Resurrect, CO_WAS_DELETED)
            } else {
                (board::Status::OnlyHere, CO_NEW)
            };
            board_row(
                SideSpec::at(board::Status::Same, rel, facts.clone()),
                SideSpec::at(arrive_status, rel, facts).veiled(veil),
                false,
                planned_cmds(),
            )
        })
        .unzip();
    for rel in plan
        .deletes
        .iter()
        .take(PREVIEW_CAP.saturating_sub(rows.len()))
    {
        let (meta, body) = board_row(
            SideSpec::absent(),
            SideSpec::at(
                board::Status::WillDelete,
                rel,
                facts_for(tgt_db.as_deref(), tgt_base.as_deref(), rel),
            )
            .veiled(CO_WILL_DELETE),
            false,
            planned_cmds(),
        );
        rows.push(meta);
        bodies.push(body);
    }
    let verb = config.command.label();
    let mut status = if delete == SyncDelete::None {
        format!("{verb}: {} to copy.", plan.copies.len())
    } else {
        format!(
            "{verb}: {} to copy, {} to delete.",
            plan.copies.len(),
            plan.deletes.len()
        )
    };
    if resurrections > 0 {
        status.push_str(&format!(
            " {resurrections} would bring back content the target deleted (blue rows)."
        ));
    }
    Ok(ReviewPreviewData {
        rows,
        bodies,
        preview_total: plan.copies.len(),
        sync_delete_total: plan.deletes.len(),
        preview_totals: [plan.deletes.len(), plan.copies.len(), 0, 0],
        source_header: TransferView::repo_header(store, &config.source),
        target_header: TransferView::repo_header(store, target),
        status,
    })
}

fn preview_repo(
    store: &Store,
    config: &RunConfig,
    references: &[String],
    target: &str,
    subdir: &str,
) -> Result<ReviewPreviewData, String> {
    let ref_slice: Vec<&str> = references.iter().map(String::as_str).collect();
    let items = diff_print(store, &config.source, &ref_slice, config.filter.as_deref())
        .map_err(|e| e.to_string())?;
    // A file the target lacks (New) is added on the target side; on the source
    // side a COPY leaves it unchanged while a MOVE removes it. Files the target
    // already has (Equal) are unchanged on both sides. DeletedInReference isn't
    // part of a transfer.
    let source_state = if config.move_files {
        board::Status::WillDelete
    } else {
        board::Status::Same
    };
    // The source holds every New/Equal file; the target holds the Equal ones.
    let (src_db, src_base) = open_facts(store, &config.source);
    let (tgt_db, tgt_base) = open_facts(store, target);
    let mut acted = 0usize;
    let mut unchanged = 0usize;
    let mut skipped_deleted = 0usize;
    let mut rows: Vec<board::RowMeta> = Vec::new();
    let mut bodies: Vec<board::RowBody> = Vec::new();
    for item in &items {
        match item {
            DiffItem::New { rel_path } => {
                acted += 1;
                if rows.len() < PREVIEW_CAP {
                    let to = if subdir.is_empty() {
                        rel_path.clone()
                    } else {
                        format!("{subdir}/{rel_path}")
                    };
                    let facts = facts_for(src_db.as_deref(), src_base.as_deref(), rel_path);
                    // Golden rule: the receiving cell shows the incoming
                    // file's preview under green NEW; a MOVE's source file
                    // wears red WILL DELETE (its own fate — it leaves).
                    let mut left = SideSpec::at(source_state, rel_path, facts.clone());
                    if config.move_files {
                        left = left.veiled(CO_WILL_DELETE);
                    }
                    let (meta, body) = board_row(
                        left,
                        SideSpec::at(board::Status::OnlyHere, &to, facts).veiled(CO_NEW),
                        false,
                        planned_cmds(),
                    );
                    rows.push(meta);
                    bodies.push(body);
                }
            }
            DiffItem::Equal { rel_path, .. } => {
                unchanged += 1;
                if rows.len() < PREVIEW_CAP {
                    let (meta, body) = board_row(
                        SideSpec::at(
                            board::Status::Same,
                            rel_path,
                            facts_for(src_db.as_deref(), src_base.as_deref(), rel_path),
                        ),
                        SideSpec::at(
                            board::Status::Same,
                            rel_path,
                            facts_for(tgt_db.as_deref(), tgt_base.as_deref(), rel_path),
                        ),
                        true,
                        planned_cmds(),
                    );
                    rows.push(meta);
                    bodies.push(body);
                }
            }
            // The copy engine refuses content the target once held and
            // deleted (copying it would resurrect a deletion). That refusal
            // used to be silent — the file simply never arrived. Now it is a
            // visible blue row: the file's preview under a WAS DELETED veil,
            // informational only (no APPLY — the engine will not copy it).
            DiffItem::DeletedInReference { rel_path } => {
                skipped_deleted += 1;
                if rows.len() < PREVIEW_CAP {
                    // Golden rule: the source file exists and nothing happens
                    // to it — plain preview. The *target* side tells its own
                    // story: a blue tombstone cell, no preview (no file).
                    let (meta, body) = board_row(
                        SideSpec::at(
                            board::Status::Same,
                            rel_path,
                            facts_for(src_db.as_deref(), src_base.as_deref(), rel_path),
                        ),
                        SideSpec::tombstone(),
                        false,
                        vec![board::Cmd::Hide],
                    );
                    rows.push(meta);
                    bodies.push(body);
                }
            }
        }
    }
    // A move both removes from source and adds to target; a copy only adds.
    // Totals are [to-delete, only-here, differing, unchanged].
    let preview_totals = if config.move_files {
        [acted, acted, 0, unchanged]
    } else {
        [0, acted, 0, unchanged]
    };
    let mut status = format!(
        "{acted} match the {}.",
        config.command.label().to_lowercase()
    );
    if skipped_deleted > 0 {
        status.push_str(&format!(
            " {skipped_deleted} skipped — the target deleted that content (blue rows)."
        ));
    }
    Ok(ReviewPreviewData {
        rows,
        bodies,
        preview_total: acted,
        sync_delete_total: 0,
        preview_totals,
        source_header: TransferView::repo_header(store, &config.source),
        target_header: TransferView::repo_header(store, target),
        status,
    })
}

fn preview_folder(
    store: &Store,
    config: &RunConfig,
    references: &[String],
    dir: &Path,
    mode: FolderMode,
    invert: bool,
) -> Result<ReviewPreviewData, String> {
    let ref_slice: Vec<&str> = references.iter().map(String::as_str).collect();
    let rels = plan_folder_export(
        store,
        &config.source,
        &ref_slice,
        mode,
        invert,
        config.filter.as_deref(),
    )
    .map_err(|e| e.to_string())?;
    // Exporting adds each file into the folder; a MOVE also removes it from the
    // source repo, a COPY leaves the source unchanged.
    let source_state = if config.move_files {
        board::Status::WillDelete
    } else {
        board::Status::Same
    };
    // The exported files live in the source repo; the target is a plain folder,
    // not a repo, so the added side has no index facts.
    let (src_db, src_base) = open_facts(store, &config.source);
    let (rows, bodies): (Vec<_>, Vec<_>) = rels
        .iter()
        .take(PREVIEW_CAP)
        .map(|rel| {
            board_row(
                SideSpec::at(
                    source_state,
                    rel,
                    facts_for(src_db.as_deref(), src_base.as_deref(), rel),
                ),
                SideSpec::at(board::Status::OnlyHere, rel, None),
                false,
                planned_cmds(),
            )
        })
        .unzip();
    let preview_totals = if config.move_files {
        [rels.len(), rels.len(), 0, 0]
    } else {
        [0, rels.len(), 0, 0]
    };
    let what = if invert { "redundant" } else { "unique" };
    Ok(ReviewPreviewData {
        rows,
        bodies,
        preview_total: rels.len(),
        sync_delete_total: 0,
        preview_totals,
        source_header: TransferView::repo_header(store, &config.source),
        target_header: dir.to_string_lossy().into_owned(),
        status: format!(
            "{} {what} file(s) to {}.",
            rels.len(),
            config.command.label().to_lowercase()
        ),
    })
}

/// The confirmation text for a batch RUN of `config`, from its counts. Pure, so
/// the prompt describes exactly what was planned. `None` for DIFF (no batch).
fn prompt_for(config: &RunConfig, copies: usize, deletes: usize) -> Option<String> {
    let source = &config.source;
    let dest = match &config.dest {
        StartDest::Repo { target, subdir, .. } => {
            if subdir.is_empty() {
                target.clone()
            } else {
                format!("{target}/{subdir}")
            }
        }
        StartDest::Folder { dir, .. } => dir.to_string_lossy().into_owned(),
        StartDest::Sync { target, .. } => target.clone(),
    };
    let deletes_missing =
        matches!(&config.dest, StartDest::Sync { delete, .. } if *delete != SyncDelete::None);
    Some(match config.command {
        Command::Copy => format!("Copy {copies} file(s) from '{source}' into '{dest}'?"),
        Command::Move => format!(
            "Move {copies} file(s) from '{source}' into '{dest}'? They are removed from the \
             source directory."
        ),
        Command::Sync if deletes_missing => format!(
            "Sync '{source}' → '{dest}': copy {copies} file(s) into the target and delete \
             {deletes} file(s) from the target. Deletions cannot be undone. The source is not \
             changed."
        ),
        Command::Sync => format!(
            "Sync '{source}' → '{dest}': copy {copies} file(s) into the target. Nothing is \
             deleted and the source is not changed."
        ),
        Command::Mirror => format!(
            "Mirror '{source}' → '{dest}': copy {copies} file(s) into the target and DELETE \
             {deletes} file(s) the source does not have, so the target ends up holding exactly \
             the source's content. Deletions cannot be undone. The source is not changed."
        ),
        // DIFF never runs as a batch: its rows are applied one by one. GROUP
        // SYNC never reaches here either — it has its own confirm text (see
        // `TransferView::raise_group_confirm`), built from a `SyncGroup`, not
        // a `RunConfig` (`capture_run_config` returns `None` for it).
        Command::Diff | Command::GroupSync | Command::GroupSyncBack => return None,
    })
}

#[derive(Debug)]
enum OpResult {
    Copied {
        copied: u64,
        cancelled: bool,
        moved: bool,
    },
    Synced {
        copied: u64,
        deleted: u64,
        skipped: u64,
        errors: u64,
        cancelled: bool,
        /// True for a MIRROR run (labels the status line), false for SYNC.
        mirror: bool,
    },
    /// A single DIFF board row action finished; the message is the status line.
    Applied {
        message: String,
    },
    Error(String),
}

/// Messages flowing from the worker thread to the UI thread: live per-file
/// progress events plus the single terminal result.
enum Msg {
    Progress(DiffEvent),
    Done(OpResult),
    /// A finished DIFF comparison, built off the UI thread (it scans both
    /// repos' full indexes). Rows come back unsorted; the board sorts them.
    DiffPreview(Result<DiffPreviewData, String>),
    /// A finished review-board preview (Copy/Move/Sync/folder). `confirm`
    /// carries the run to authorise once the plan is in hand — the deferred
    /// half of a RUN click.
    ReviewPreview {
        result: Result<ReviewPreviewData, String>,
        confirm: Option<Box<RunConfig>>,
    },
    /// A finished GROUP SYNC plan, built off the UI thread. `confirm`, when
    /// set, raises the RUN confirmation once the plan lands with real counts —
    /// the deferred half of a RUN click.
    GroupPreview {
        result: Result<GroupPreviewData, String>,
        confirm: bool,
    },
    /// A finished GROUP SYNC BACK plan (pull a sink into the main), built off the
    /// UI thread. `confirm` raises the RUN confirmation once the real counts land.
    GroupBackPreview {
        result: Result<GroupPreviewData, String>,
        confirm: bool,
    },
    /// A finished GROUP SYNC push, aggregated across every sink pushed.
    GroupDone(Result<GroupSyncResult, String>),
}

/// The result of planning a GROUP SYNC push, built off the UI thread. `group`
/// is already filtered to the sinks that were selected when the plan started.
struct GroupPreviewData {
    group: SyncGroup,
    /// The main's absolute path, resolved where the store is at hand, so the
    /// header names it the same way every other surface does.
    main_header: String,
    rows: Vec<board::RowMeta>,
    bodies: Vec<board::RowBody>,
    added: usize,
    removed: usize,
    sink_count: usize,
    /// Sinks the plan would empty of their current contents, `(sink, live)`.
    wholesale_sinks: Vec<(String, u64)>,
}

/// The aggregated outcome of a GROUP SYNC push across every sink pushed.
/// Per-file problems are not carried here — they arrive as `Msg::Progress`
/// events and accumulate in `TransferView::run_problems` like every other
/// command's, so `drain` folds them in when this message lands.
struct GroupSyncResult {
    main: String,
    copied: u64,
    deleted: u64,
    errors: u64,
    cancelled: bool,
    /// Sinks that failed outright, as ready-to-display `"sink: error"` lines.
    failures: Vec<String>,
    /// Sinks a cancel cut short before they were reached — still worth naming,
    /// since their backups are now stale.
    skipped: Vec<String>,
}

/// The result of a DIFF preview: the paired rows and each side's header.
struct DiffPreviewData {
    rows: Vec<RepoDiffRow>,
    source_header: String,
    target_header: String,
}

/// [`DiffProgress`] adapter that forwards every diff event onto the TransferView
/// channel. Sends never block; a dropped receiver is fine.
struct ChannelDiffProgress {
    tx: Sender<Msg>,
}

impl DiffProgress for ChannelDiffProgress {
    fn on(&self, event: DiffEvent) {
        let _ = self.tx.send(Msg::Progress(event));
    }
}

pub struct TransferView {
    repos: Vec<String>,
    /// Repos that are the main of a sync group, for the chip badge. Refreshed
    /// with `repos` whenever the tab is shown.
    mains: std::collections::HashSet<String>,
    loaded: bool,
    source: Option<String>,
    target: Option<String>,
    /// Extra reference repos beyond the target: a file counts as "new" only
    /// when neither the target nor any of these already has its content.
    extra_refs: Vec<String>,
    command: Command,
    /// GROUP SYNC: the sync group whose main is the current source, if any —
    /// refreshed whenever the source or the repo list changes. `None` hides
    /// the GROUP SYNC command entirely.
    current_group: Option<SyncGroup>,
    /// GROUP SYNC: which of `current_group`'s sinks the next push includes.
    /// Reset to every sink whenever `current_group` changes.
    selected_sinks: Vec<String>,
    /// GROUP SYNC preview: sinks the plan would empty of their current
    /// contents, `(sink, files it holds now)` — mirrors MIRROR's warning, but
    /// per sink since a group push can span several.
    wholesale_sinks: Vec<(String, u64)>,
    /// GROUP SYNC: the group (already filtered to the selected sinks) a raised
    /// confirmation authorises, captured when its plan landed. PROCEED pushes
    /// *this*, not whatever is selected when the button is clicked.
    pending_group_confirm: Option<SyncGroup>,
    /// GROUP SYNC BACK's authorised pull, `(main, sink)`, captured when its plan
    /// landed — the batch promotes only the new files.
    pending_group_back: Option<(String, String)>,
    /// Whether COPY/MOVE goes into a repo or a picked folder.
    destination: Destination,
    /// Absolute path of the export folder (Destination::Folder).
    folder: String,
    /// Grouping basis for a folder export.
    select_mode: SelectMode,
    /// Export the redundant copies instead of the unique files.
    invert: bool,
    /// SYNC only: also delete target files whose content the source marks
    /// missing (off by default — SYNC is additive unless this is on).
    sync_delete_missing: bool,
    /// SYNC preview: how many target files a run would delete (companion to
    /// `preview_total`, which counts the copies).
    sync_delete_total: usize,
    /// This tab's own similarity threshold (%), shown as a slider in SIMILAR
    /// mode; used when a folder export groups by perceptual similarity.
    similar_threshold: f64,
    subdir: String,
    /// The shared FILTER wizard (conditions, presets, suggestions, live count).
    filter: FilterBuilder,
    preview: Vec<board::RowMeta>,
    /// Thumbnails and facts for `preview`, kept index-aligned with it: the board
    /// resolves a row's body only for the rows actually on screen.
    preview_bodies: Vec<board::RowBody>,
    /// DIFF: how the two repos are paired up (by content or by path).
    pairing: DiffPairing,
    /// DIFF: the rows of the current comparison, empty until REVIEW.
    diff_rows: Vec<RepoDiffRow>,
    /// DIFF's open follow-up question, if any. Everything else about the diff
    /// board moved to `preview_board` when DIFF was routed onto the shared board.
    board_state: crate::diff_board::BoardState,
    /// The open side-by-side comparison of one conflicting row, if any.
    inspect: Option<DiffCompare>,
    /// A bulk action awaiting confirmation: what it is, and every file operation
    /// it would perform.
    bulk_confirm: Option<(BulkOp, Vec<crate::diff_board::BoardAction>)>,
    /// Full counts `[to-delete, only-here, differing, unchanged]` for the board
    /// summary; independent of the capped `preview` sample.
    preview_totals: [usize; 4],
    /// The two board region headers: the source and target absolute paths.
    preview_source_header: String,
    preview_target_header: String,
    preview_total: usize,
    /// Sort key, side, direction and hidden rows for the board — shared by the
    /// planned previews and by DIFF, which are mutually exclusive commands.
    preview_board: board::BoardState,
    status: Option<String>,
    error: Option<String>,
    confirm: Option<String>,
    running: bool,
    /// Set while a preview (DIFF or review-board) is being planned on a worker
    /// thread, so the UI shows it is busy and does not launch a second one.
    previewing: bool,
    /// The run a raised confirmation authorises, captured when its plan landed.
    /// PROCEED runs *this*, not whatever the live controls say — the two can
    /// differ across the async plan/confirm gap. Cleared when the dialog closes.
    pending_confirm: Option<Box<RunConfig>>,
    /// Set while a single-row APPLY runs: refresh the preview when it finishes.
    pending_refresh: bool,
    cancel: CancellationToken,
    // Live run progress: the last N actions, the running counters and the
    // file currently being handled.
    run_log: VecDeque<String>,
    run_done: u64,
    run_total: u64,
    run_current: String,
    /// Every per-file failure of the current run, capped. The live `run_log`
    /// keeps only the last handful, so on a large run its errors scroll away;
    /// this retains the full list for the end-of-run report.
    run_problems: Vec<String>,
    /// The report of the last finished batch run.
    result: crate::run_result::ResultModal,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    /// Tooltip wording for this frame, set at the top of [`Self::show`] from
    /// the app-wide setting (not persisted here; `app.rs` owns that).
    verbosity: TooltipVerbosity,
    /// Decodes the review board's row thumbnails; polled once per frame.
    thumbs: ThumbCache,
    /// The app-wide repo lock registry (see [`crate::locks`]): which repos'
    /// existing files may be deleted or overwritten this session.
    locks: crate::locks::RepoLocks,
}

enum Act {
    PickSource(String),
    PickTarget(String),
    ToggleExtraRef(String),
    SetCommand(Command),
    SetDestination(Destination),
    SetMode(SelectMode),
    ToggleInvert,
    ToggleSyncDelete,
    FolderChanged,
    BrowseFolder,
    SubdirChanged,
    BrowseSubdir,
    Preview,
    Ask,
    Confirm,
    CancelConfirm,
    CancelRun,
    /// Apply a single review row immediately (its namespaced key).
    ApplyRow(String),
    SetPairing(DiffPairing),
    /// Execute a single DIFF board row action.
    Board(crate::diff_board::BoardAction),
    /// GROUP SYNC: toggle one sink's inclusion in the next push.
    ToggleSink(String),
    SelectAllSinks,
    SelectNoSinks,
    /// GROUP SYNC BACK: pick exactly one sink to pull back (single-select).
    SelectOnlySink(String),
    /// GROUP SYNC BACK: delete one file (by sink-relative path) from the sink —
    /// the "not worth promoting, not worth keeping" half of the triage.
    DeleteSinkRow(String),
    /// Open one existing file `(repo, rel_path)` from a preview row in the
    /// single-file INSPECT viewer (planned counterparts may not exist yet).
    OpenPreviewRow(String, String),
}

impl TransferView {
    pub fn new() -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        Self {
            repos: Vec::new(),
            mains: std::collections::HashSet::new(),
            loaded: false,
            source: None,
            target: None,
            extra_refs: Vec::new(),
            command: Command::Copy,
            current_group: None,
            selected_sinks: Vec::new(),
            wholesale_sinks: Vec::new(),
            pending_group_confirm: None,
            pending_group_back: None,
            destination: Destination::Repo,
            folder: String::new(),
            select_mode: SelectMode::Exact,
            invert: false,
            sync_delete_missing: false,
            sync_delete_total: 0,
            similar_threshold: 90.0,
            subdir: String::new(),
            filter: FilterBuilder::new(),
            preview: Vec::new(),
            pairing: DiffPairing::ByHash,
            diff_rows: Vec::new(),
            board_state: crate::diff_board::BoardState::default(),
            inspect: None,
            bulk_confirm: None,
            preview_totals: [0; 4],
            preview_source_header: String::new(),
            preview_target_header: String::new(),
            preview_total: 0,
            preview_bodies: Vec::new(),
            preview_board: board::BoardState::default(),
            status: None,
            error: None,
            confirm: None,
            running: false,
            previewing: false,
            pending_confirm: None,
            pending_refresh: false,
            cancel: CancellationToken::new(),
            run_log: VecDeque::new(),
            run_problems: Vec::new(),
            result: crate::run_result::ResultModal::default(),
            run_done: 0,
            run_total: 0,
            run_current: String::new(),
            tx,
            rx,
            verbosity: TooltipVerbosity::default(),
            thumbs: ThumbCache::new(3),
            locks: crate::locks::RepoLocks::new(),
        }
    }

    /// Construct wired to the app's shared lock registry, so a repo unlocked
    /// here is unlocked on every tab (and vice versa).
    pub fn new_with_locks(locks: crate::locks::RepoLocks) -> Self {
        let mut me = Self::new();
        me.locks = locks;
        me
    }

    /// The SIMILAR folder-export similarity threshold (percent), persisted
    /// across launches by `app.rs`.
    pub fn threshold(&self) -> f64 {
        self.similar_threshold
    }

    /// Restore the persisted SIMILAR similarity threshold.
    pub fn set_threshold(&mut self, threshold: f64) {
        self.similar_threshold = threshold.clamp(50.0, 100.0);
    }

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        store: &Arc<Store>,
        verbosity: TooltipVerbosity,
        frame: Option<&eframe::Frame>,
    ) {
        self.verbosity = verbosity;
        if self.thumbs.poll(ui.ctx()) {
            ui.ctx().request_repaint();
        }
        self.drain(ui);
        // A finished single-row APPLY refreshes the preview, so the board
        // reflects the applied action instead of dropping to the run log.
        if self.pending_refresh && !self.running {
            self.pending_refresh = false;
            self.reset_run();
            self.run_preview(store);
        }
        if !self.loaded {
            self.sync_repos(store);
        }
        // The end-of-run report sits above everything, and swallows shortcuts
        // while it is up.
        let result_open = self.result.show(ui);

        let mut acts: Vec<Act> = Vec::new();

        // Keyboard shortcuts — skipped while a modal is up, a run or preview is
        // active, or a text field is focused.
        if self.confirm.is_none()
            && !result_open
            && !self.running
            && !self.previewing
            && !ui.ctx().egui_wants_keyboard_input()
        {
            ui.input(|i| {
                if i.key_pressed(egui::Key::Num1) {
                    acts.push(Act::SetCommand(Command::Copy));
                }
                if i.key_pressed(egui::Key::Num2) {
                    acts.push(Act::SetCommand(Command::Move));
                }
                if i.key_pressed(egui::Key::Num3) {
                    acts.push(Act::SetCommand(Command::Sync));
                }
                if i.key_pressed(egui::Key::Num4) {
                    acts.push(Act::SetCommand(Command::Mirror));
                }
                if i.key_pressed(egui::Key::Num5) {
                    acts.push(Act::SetCommand(Command::Diff));
                }
                if i.key_pressed(egui::Key::P) {
                    acts.push(Act::Preview);
                }
                if i.key_pressed(egui::Key::R) {
                    acts.push(Act::Ask);
                }
            });
        }

        // The whole tab scrolls as one page: the controls above the preview
        // (filter wizard, dest/mode bars) grow with the command, and the
        // preview table can be tall, so without this the lower rows — and with
        // enough controls the preview entirely — fall off the bottom of the
        // window with no way to reach them. The virtualised preview table
        // culls to the visible band via its clip rect, so nesting it here keeps
        // the single outer scrollbar cheap.
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(6.0);
                ui.label(
                    RichText::new("TRANSFER")
                        .color(theme::blue())
                        .size(18.0)
                        .strong(),
                );
                crate::util::shortcut_bar(
                    ui,
                    "1 copy · 2 move · 3 sync · 4 mirror · 5 diff · P review · R run",
                );

                self.repo_rows(ui, &mut acts);
                self.command_bar(ui, &mut acts);
                // SYNC/MIRROR are always repo→repo at the same relative path, so
                // they hide the DEST/subdir/folder controls: SYNC shows its
                // DELETE MISSING toggle; MIRROR shows a warning (it always
                // deletes).
                match self.command {
                    Command::Diff => self.pairing_bar(ui, &mut acts),
                    Command::Sync => self.sync_bar(ui, &mut acts),
                    Command::Mirror => self.mirror_bar(ui),
                    Command::GroupSync => self.group_sinks_bar(ui, &mut acts),
                    Command::GroupSyncBack => self.group_back_sink_bar(ui, &mut acts),
                    _ => {
                        self.dest_bar(ui, &mut acts);
                        match self.destination {
                            Destination::Repo => self.subdir_bar(ui, &mut acts),
                            Destination::Folder => {
                                self.folder_bar(ui, &mut acts);
                                self.mode_bar(ui, &mut acts);
                            }
                        }
                    }
                }
                // The shared FILTER wizard; the source repo backs its MIME
                // suggestions and live match count. DIFF compares the repos
                // whole, so it has nothing to filter.
                if !self.command.is_diff() {
                    let source = self.source.clone();
                    let outcome = self.filter.ui(ui, store, source.as_deref(), self.verbosity);
                    if outcome.changed {
                        self.clear_preview();
                    }
                    if outcome.status.is_some() {
                        self.status = outcome.status;
                    }
                    if outcome.error.is_some() {
                        self.error = outcome.error;
                    }
                }
                self.action_bar(ui, &mut acts);

                if let Some(err) = &self.error {
                    ui.colored_label(theme::red(), err);
                }
                if let Some(status) = &self.status {
                    ui.label(RichText::new(status).color(theme::tan()).size(13.0));
                }
                ui.separator();
                // RUN and REVIEW are mutually exclusive: while a run is active
                // or has left a log, show the live run panel; otherwise show the
                // preview.
                if self.running || !self.run_log.is_empty() {
                    self.run_panel(ui);
                } else {
                    self.preview_panel(ui, store, &mut acts);
                }
            });

        if let Some(prompt) = self.confirm.clone() {
            self.confirm_modal(ui, &prompt, &mut acts);
        }
        // The comparison sits above everything, and its buttons feed the same
        // row actions the board offers.
        if self.bulk_confirm.is_some() {
            let store = Arc::clone(store);
            self.bulk_confirm_modal(&ui.ctx().clone(), &store);
        }
        if let Some(inspect) = self.inspect.as_mut()
            && let Some(pick) = inspect.view(&ui.ctx().clone(), verbosity, None)
        {
            let (left_rel, right_rel) = (
                inspect.left.rel_path.clone(),
                inspect.right.rel_path.clone(),
            );
            self.inspect = None;
            match pick {
                DiffPick::Close => {}
                // DIFF's sides are read-only, so neither a mark toggle nor an
                // in-place save can arrive here.
                DiffPick::ToggleMark { .. } | DiffPick::Edited { .. } => {}
                DiffPick::Delete { on_left } => {
                    acts.push(Act::Board(crate::diff_board::BoardAction::Delete {
                        on_left,
                        rel_path: if on_left { left_rel } else { right_rel },
                    }));
                }
                DiffPick::Overwrite { from_left } => {
                    acts.push(Act::Board(crate::diff_board::BoardAction::Overwrite {
                        from_left,
                        from_rel: if from_left {
                            left_rel.clone()
                        } else {
                            right_rel.clone()
                        },
                        to_rel: if from_left { right_rel } else { left_rel },
                    }));
                }
            }
        }

        for act in acts {
            self.apply(store, frame, act);
        }
    }

    /// Sync the repo list with the store, keeping the current source/target/pool
    /// picks and dropping any that no longer exist. Called on first show and
    /// whenever the tab is re-shown, so no manual refresh button is needed.
    pub fn sync_repos(&mut self, store: &Store) {
        match store.list_repos() {
            Ok(list) => {
                // Sinks are managed through their group's main, not operated on
                // directly, so they are not offered here.
                let sinks = store.sink_repo_names().unwrap_or_default();
                self.mains = store.main_repo_names().unwrap_or_default();
                self.repos = list
                    .into_iter()
                    .map(|(n, _, _)| n)
                    .filter(|n| !sinks.contains(n))
                    .collect();
                if let Some(s) = &self.source
                    && !self.repos.contains(s)
                {
                    self.source = None;
                }
                if let Some(t) = &self.target
                    && !self.repos.contains(t)
                {
                    self.target = None;
                }
                self.extra_refs.retain(|r| self.repos.contains(r));
                self.loaded = true;
                self.error = None;
                self.refresh_group(store);
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    /// Re-resolve GROUP SYNC's group from the current source: the sync group
    /// (if any) whose main *is* the source, and every one of its sinks
    /// selected by default. Falls back off GROUP SYNC when the source no
    /// longer names a group's main (e.g. the group was disbanded elsewhere).
    fn refresh_group(&mut self, store: &Store) {
        self.current_group = self.source.as_deref().and_then(|src| {
            store
                .list_sync_groups()
                .ok()?
                .into_iter()
                .map(|(_, group)| group)
                .find(|group| group.main == src)
        });
        self.selected_sinks = self
            .current_group
            .as_ref()
            .map(|g| g.sinks.iter().map(|s| s.repo.clone()).collect())
            .unwrap_or_default();
        if self.command == Command::GroupSync && self.current_group.is_none() {
            self.command = Command::Copy;
        }
    }

    fn repo_rows(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        crate::lcars::section_lcars(ui, "REPOS — PICK SOURCE & TARGET", theme::lilac(), |ui| {
            // SOURCE: every repo, orange when picked.
            let src = self.repos.clone();
            let mains = self.mains.clone();
            crate::repo_chip::chip_row(ui, "xfer_source", "SOURCE", src.len(), |ui, i| {
                let name = &src[i];
                let sel = self.source.as_deref() == Some(name.as_str());
                let chip = crate::repo_chip::repo_chip(
                    ui,
                    name,
                    sel,
                    theme::orange(),
                    mains.contains(name),
                    Some(self.locks.read_only(name)),
                );
                if chip
                    .name
                    .explain(
                        self.verbosity,
                        "Pick as the source repo",
                        "Use this repository as the source: its files are compared against \
                         the target (and any DupePool repos) to decide what's new or already \
                         known.",
                    )
                    .clicked()
                {
                    acts.push(Act::PickSource(name.clone()));
                }
                self.locks.handle_badge(chip.lock, self.verbosity, name);
                chip.outer
            });

            // TARGET (only when copying/moving into a repo — a folder export has
            // no target, and both GROUP SYNC and GROUP SYNC BACK pick their other
            // repo in the SINK panel below, not here: GROUP SYNC pushes the main
            // to its sinks, GROUP SYNC BACK pulls a chosen sink into the main).
            if self.destination == Destination::Repo
                && !matches!(self.command, Command::GroupSync | Command::GroupSyncBack)
            {
                let tgt: Vec<String> = self
                    .repos
                    .iter()
                    .filter(|n| self.source.as_deref() != Some(n.as_str()))
                    .cloned()
                    .collect();
                let mains = self.mains.clone();
                crate::repo_chip::chip_row(ui, "xfer_target", "TARGET", tgt.len(), |ui, i| {
                    let name = &tgt[i];
                    let sel = self.target.as_deref() == Some(name.as_str());
                    let chip = crate::repo_chip::repo_chip(
                        ui,
                        name,
                        sel,
                        theme::blue(),
                        mains.contains(name),
                        Some(self.locks.read_only(name)),
                    );
                    if chip
                        .name
                        .explain(
                            self.verbosity,
                            "Pick as the target repo",
                            "Use this repository as the target: it's where COPY/MOVE files land, \
                             and it always counts as a reference for deciding what's new.",
                        )
                        .clicked()
                    {
                        acts.push(Act::PickTarget(name.clone()));
                    }
                    self.locks.handle_badge(chip.lock, self.verbosity, name);
                    chip.outer
                });
            }

            // DUPEPOOL: content any of these repos already holds is treated as
            // "already known" and never re-copied. The target is *always* a
            // reference (handled in `references`), so — like the source — it's
            // simply left out of this row rather than shown as a locked chip. SYNC
            // compares source against the single target only, so it has no pool.
            if !self.command.repo_to_repo() {
                // The toggleable pool repos: everything except the source and target.
                let eligible: Vec<String> = self
                    .repos
                    .iter()
                    .filter(|n| {
                        self.source.as_deref() != Some(n.as_str())
                            && !(self.destination == Destination::Repo
                                && self.target.as_deref() == Some(n.as_str()))
                    })
                    .cloned()
                    .collect();
                ui.horizontal(|ui| {
                    ui.label(RichText::new("DUPEPOOL").color(theme::text()).size(12.0));
                    if crate::repo_chip::small_button(ui, "ALL", theme::lilac())
                        .explain(
                            self.verbosity,
                            "Add every eligible repo to the pool",
                            "Treat content held by any other repo as already known.",
                        )
                        .clicked()
                    {
                        self.extra_refs = eligible.clone();
                    }
                    if crate::repo_chip::small_button(ui, "NONE", theme::lilac())
                        .explain(
                            self.verbosity,
                            "Clear the dupe pool",
                            "Compare against the target only (plus SYNC's single target).",
                        )
                        .clicked()
                    {
                        self.extra_refs.clear();
                    }
                });
                let mains = self.mains.clone();
                crate::repo_chip::chip_row(ui, "xfer_pool", "", eligible.len(), |ui, i| {
                    let name = &eligible[i];
                    let sel = self.extra_refs.iter().any(|r| r == name);
                    let chip = crate::repo_chip::repo_chip(
                        ui,
                        name,
                        sel,
                        theme::lilac(),
                        mains.contains(name),
                        Some(self.locks.read_only(name)),
                    );
                    if chip
                        .name
                        .explain(
                            self.verbosity,
                            "Add to the dupe pool",
                            "Also check for dupes vs these repos in addition to the target repo.",
                        )
                        .clicked()
                    {
                        acts.push(Act::ToggleExtraRef(name.clone()));
                    }
                    self.locks.handle_badge(chip.lock, self.verbosity, name);
                    chip.outer
                });
            }
        });
    }

    /// The reference list for the diff ops: the target (primary) plus any extra
    /// references, skipping ones that are no longer valid repos.
    fn references(&self, target: &str) -> Vec<String> {
        let mut refs = vec![target.to_string()];
        for r in &self.extra_refs {
            if r != target && self.source.as_deref() != Some(r.as_str()) {
                refs.push(r.clone());
            }
        }
        refs
    }

    /// The DupePool repos to subtract from a folder export: the extra references
    /// (there is no target in folder mode), minus the source.
    fn folder_references(&self) -> Vec<String> {
        self.extra_refs
            .iter()
            .filter(|r| self.source.as_deref() != Some(r.as_str()))
            .cloned()
            .collect()
    }

    /// The [`FolderMode`] currently selected for a folder export.
    fn folder_mode(&self) -> FolderMode {
        match self.select_mode {
            SelectMode::Exact => FolderMode::Exact,
            SelectMode::Similar => FolderMode::Similar {
                threshold: self.similar_threshold,
            },
        }
    }

    /// Whether REVIEW/RUN can act: a source is picked, the destination is
    /// resolved (a target repo, a non-blank export folder, or — for GROUP
    /// SYNC — at least one sink selected), and nothing is already running.
    fn ready(&self) -> bool {
        // A preview in flight disables REVIEW/RUN too, so a second click cannot
        // launch an overlapping worker.
        if self.running || self.previewing || self.source.is_none() {
            return false;
        }
        if matches!(self.command, Command::GroupSync | Command::GroupSyncBack) {
            return self.current_group.is_some() && !self.selected_sinks.is_empty();
        }
        match self.destination {
            Destination::Repo => self.target.is_some(),
            Destination::Folder => !self.folder.trim().is_empty(),
        }
    }

    fn command_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        let has_group = self.current_group.is_some();
        let title = if has_group {
            "COMMAND — COPY, MOVE, SYNC, MIRROR, GROUP SYNC, GROUP SYNC BACK OR DIFF"
        } else {
            "COMMAND — COPY, MOVE, SYNC, MIRROR OR DIFF"
        };
        crate::lcars::section_lcars(ui, title, theme::orange(), |ui| {
            ui.horizontal(|ui| {
                let mut cmds = vec![Command::Copy, Command::Move, Command::Sync, Command::Mirror];
                // Only offered when the source is a sync group's main — GROUP SYNC
                // pushes it to sinks, GROUP SYNC BACK pulls a sink into it.
                if has_group {
                    cmds.push(Command::GroupSync);
                    cmds.push(Command::GroupSyncBack);
                }
                cmds.push(Command::Diff);
                for cmd in cmds {
                    let sel = self.command == cmd;
                    let accent = if cmd.destructive() {
                        theme::red()
                    } else {
                        theme::amber()
                    };
                    let fill = if sel { accent } else { theme::panel() };
                    // Unselected pills sit on the dark panel — black text would
                    // vanish there, so they carry their accent color instead.
                    let col = if sel { theme::black() } else { accent };
                    let (short, verbose) = cmd.tooltip();
                    if ui
                        .add(egui::Button::new(RichText::new(cmd.label()).color(col)).fill(fill))
                        .explain(self.verbosity, short, verbose)
                        .clicked()
                    {
                        acts.push(Act::SetCommand(cmd));
                    }
                }
            });
            self.hint(ui);
        });
    }

    fn subdir_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        crate::lcars::section_lcars(
            ui,
            "INTO — SUBFOLDER INSIDE THE TARGET",
            theme::blue(),
            |ui| {
                ui.horizontal(|ui| {
                    let changed = ui
                        .add(
                            egui::TextEdit::singleline(&mut self.subdir)
                                .desired_width(220.0)
                                .hint_text("relative/subdir (optional)"),
                        )
                        .explain(
                            self.verbosity,
                            "Relative subfolder inside the target",
                            "Place transferred files under this relative subfolder inside the \
                         target repo, preserving each file's source-relative path. Leave \
                         blank to place them at the target root. Paths escaping the target \
                         (absolute or containing `..`) are rejected.",
                        )
                        .changed();
                    if changed {
                        acts.push(Act::SubdirChanged);
                    }
                    let can_browse = self.target.is_some();
                    if ui
                        .add_enabled(
                            can_browse,
                            egui::Button::new(
                                RichText::new(format!("{} BROWSE", icon::FOLDER_OPEN))
                                    .color(theme::black()),
                            ),
                        )
                        .explain(
                            self.verbosity,
                            "Pick or create a subfolder",
                            "Open a native folder picker rooted at the target repo to pick (or \
                         create) the subfolder transferred files go into.",
                        )
                        .clicked()
                    {
                        acts.push(Act::BrowseSubdir);
                    }
                });
                ui.label(
                RichText::new(
                    "Files keep their source-relative path under this folder inside the target.",
                )
                .color(theme::lilac())
                .size(11.0),
            );
            },
        );
    }

    /// Selector for where COPY/MOVE lands: into a repo or into a picked folder.
    fn dest_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        crate::lcars::section_lcars(ui, "DEST — WHERE COPIED FILES LAND", theme::blue(), |ui| {
            ui.horizontal(|ui| {
                for (dest, label, short, verbose) in [
                    (
                        Destination::Repo,
                        "REPO",
                        "Transfer into the target repo",
                        "COPY/MOVE the source files the target (and DupePool) don't have \
                         into the target repository.",
                    ),
                    (
                        Destination::Folder,
                        "FOLDER",
                        "Export into a picked folder",
                        "COPY/MOVE a deduplicated selection of the source into a plain \
                         folder you pick, keeping each file's source-relative path.",
                    ),
                ] {
                    let sel = self.destination == dest;
                    let fill = if sel { theme::blue() } else { theme::panel() };
                    let col = if sel { theme::black() } else { theme::blue() };
                    if ui
                        .add(egui::Button::new(RichText::new(label).color(col)).fill(fill))
                        .explain(self.verbosity, short, verbose)
                        .clicked()
                    {
                        acts.push(Act::SetDestination(dest));
                    }
                }
            });
        });
    }

    /// The export-folder path input and its native folder picker (FOLDER mode).
    fn folder_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        crate::lcars::section_lcars(
            ui,
            "FOLDER — EXPORT DESTINATION ON DISK",
            theme::blue(),
            |ui| {
                ui.horizontal(|ui| {
                    let changed = ui
                        .add(
                            egui::TextEdit::singleline(&mut self.folder)
                                .desired_width(320.0)
                                .hint_text("/absolute/export/folder"),
                        )
                        .explain(
                            self.verbosity,
                            "Absolute export folder",
                            "The folder the selected files are copied/moved into. Files keep \
                         their source-relative path under it.",
                        )
                        .changed();
                    if changed {
                        acts.push(Act::FolderChanged);
                    }
                    if ui
                        .add(egui::Button::new(
                            RichText::new(format!("{} BROWSE", icon::FOLDER_OPEN))
                                .color(theme::black()),
                        ))
                        .explain(
                            self.verbosity,
                            "Pick or create the export folder",
                            "Open a native folder picker to choose (or create) the folder the \
                         selected files go into.",
                        )
                        .clicked()
                    {
                        acts.push(Act::BrowseFolder);
                    }
                });
            },
        );
    }

    /// Grouping mode (exact/similar) and the invert toggle for a folder export.
    fn mode_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        crate::lcars::section_lcars(
            ui,
            "MODE — EXACT OR SIMILAR MATCHING",
            theme::lilac(),
            |ui| {
                ui.horizontal(|ui| {
                    for mode in [SelectMode::Exact, SelectMode::Similar] {
                        let sel = self.select_mode == mode;
                        let fill = if sel { theme::lilac() } else { theme::panel() };
                        let col = if sel { theme::black() } else { theme::lilac() };
                        let (short, verbose) = match mode {
                            SelectMode::Exact => (
                                "Group by exact content",
                                "Treat only byte-identical files (same size + hash) as copies of \
                             each other.",
                            ),
                            SelectMode::Similar => (
                                "Group by perceptual similarity",
                                "Treat perceptually similar media (at the similarity threshold \
                             below) as copies — e.g. one photo per burst.",
                            ),
                        };
                        if ui
                            .add(
                                egui::Button::new(RichText::new(mode.label()).color(col))
                                    .fill(fill),
                            )
                            .explain(self.verbosity, short, verbose)
                            .clicked()
                        {
                            acts.push(Act::SetMode(mode));
                        }
                    }
                    ui.separator();
                    let fill = if self.invert {
                        theme::orange()
                    } else {
                        theme::panel()
                    };
                    let col = if self.invert {
                        theme::black()
                    } else {
                        theme::orange()
                    };
                    if ui
                        .add(egui::Button::new(RichText::new("INVERT").color(col)).fill(fill))
                        .explain(
                            self.verbosity,
                            "Export the redundant copies instead",
                            "Off: export the unique files (one best copy per group plus every \
                         singleton). On: export the redundant copies instead (every \
                         non-best member of a group) — what a dedup would remove.",
                        )
                        .clicked()
                    {
                        acts.push(Act::ToggleInvert);
                    }
                });
                // In SIMILAR mode the threshold is chosen right here (the same shared
                // control as the Duplicates tab), not borrowed from another tab.
                if self.select_mode == SelectMode::Similar {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        crate::util::similarity_slider(
                            ui,
                            &mut self.similar_threshold,
                            self.verbosity,
                        );
                    });
                }
                let hint = if self.invert {
                    "Exports the redundant copies (every non-best member of a group)."
                } else {
                    "Exports the unique files (best copy of each group plus every singleton)."
                };
                ui.label(RichText::new(hint).color(theme::lilac()).size(11.0));
            },
        );
    }

    fn hint(&self, ui: &mut egui::Ui) {
        let text = match self.command {
            Command::Copy => "Copy source files the target does not have into the target repo.",
            Command::Move => {
                "Move source files the target does not have into the target repo \
                 (they are removed from the source directory)."
            }
            Command::Sync => {
                "Copy the source into the target: copy content it lacks (same relative \
                 path); optionally delete target files the source has lost."
            }
            Command::Mirror => {
                "Make the target an exact copy of the source: copy what it lacks and delete \
                 everything the source does not have."
            }
            Command::GroupSync => {
                "Push the source (this group's main) to the sinks selected below, each in its \
                 own stored mode."
            }
            Command::GroupSyncBack => {
                "Pull the sink selected below back into the main — promote files it added, and \
                 choose whether to bring back files the main deleted."
            }
            Command::Diff => {
                "Compare the two repos side by side and resolve each difference yourself — \
                 copy, delete, rename or overwrite, one row at a time."
            }
        };
        ui.label(RichText::new(text).color(theme::lilac()).size(11.0));
    }

    /// MIRROR's info bar: no toggle (it always deletes), just a red warning that
    /// it removes everything in the target the source lacks.
    fn mirror_bar(&self, ui: &mut egui::Ui) {
        crate::lcars::section_lcars(
            ui,
            &format!("{} DELETES EXTRAS", icon::TRASH),
            theme::red(),
            |ui| {
                ui.label(
                    RichText::new(
                        "Everything in the target whose content the source does not have is \
                     deleted, so the target ends up holding exactly the source's content. \
                     Deletions cannot be undone.",
                    )
                    .color(theme::lilac())
                    .size(11.0),
                );
            },
        );
    }

    /// GROUP SYNC's option bar: which of the group's sinks the next push
    /// includes, defaulting to all of them. Each sink shows its own stored
    /// push mode (set on the Repositories tab, not editable here) so the
    /// selection reads honestly — this panel picks *which* sinks, not *how*.
    /// GROUP SYNC BACK's sink picker — single-select: you pull one sink back at a
    /// time (the drive you edited). A multi-sink pull is a semantic not yet taken
    /// on. Otherwise mirrors GROUP SYNC's sink chips.
    fn group_back_sink_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        crate::lcars::section_lcars(
            ui,
            "SINK — PULL ITS CHANGES BACK INTO THE MAIN",
            theme::blue(),
            |ui| {
                let Some(group) = self.current_group.clone() else {
                    return;
                };
                crate::repo_chip::chip_row(ui, "xfer_back_sink", "", group.sinks.len(), |ui, i| {
                    let sink = &group.sinks[i];
                    let mode = match sink.mode {
                        SyncMode::AddOnly => "ADD ONLY",
                        SyncMode::Mirror => "MIRROR",
                    };
                    let sel = self
                        .selected_sinks
                        .first()
                        .map(|s| s == &sink.repo)
                        .unwrap_or(false);
                    let accent = if sink.mode == SyncMode::Mirror {
                        theme::red()
                    } else {
                        theme::blue()
                    };
                    let row = ui.horizontal(|ui| {
                        let chip = crate::repo_chip::repo_chip(
                            ui,
                            &sink.repo,
                            sel,
                            accent,
                            false,
                            Some(self.locks.read_only(&sink.repo)),
                        );
                        ui.label(
                            RichText::new(format!("MODE: {mode}"))
                                .color(accent)
                                .size(10.0),
                        );
                        chip
                    });
                    if row
                        .inner
                        .name
                        .explain(
                            self.verbosity,
                            "Pull this sink back into the main",
                            "Compare this sink against the main and pull its changes back: \
                             promote files the main never had, and choose whether to bring back \
                             files the main deleted that the sink still holds.",
                        )
                        .clicked()
                    {
                        acts.push(Act::SelectOnlySink(sink.repo.clone()));
                    }
                    self.locks
                        .handle_badge(row.inner.lock, self.verbosity, &sink.repo);
                    row.response
                });
            },
        );
    }

    fn group_sinks_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        crate::lcars::section_lcars(
            ui,
            "SINKS — WHERE THE MAIN IS PUSHED",
            theme::blue(),
            |ui| {
                let Some(group) = self.current_group.clone() else {
                    return;
                };
                ui.horizontal(|ui| {
                    ui.label(RichText::new("SINKS").color(theme::text()).size(12.0));
                    if crate::repo_chip::small_button(ui, "ALL", theme::blue())
                        .explain(
                            self.verbosity,
                            "Include every sink",
                            "Include every sink of this group in the next push.",
                        )
                        .clicked()
                    {
                        acts.push(Act::SelectAllSinks);
                    }
                    if crate::repo_chip::small_button(ui, "NONE", theme::blue())
                        .explain(
                            self.verbosity,
                            "Clear the sink selection",
                            "Deselect every sink (REVIEW/RUN are disabled until at least one is \
                         picked).",
                        )
                        .clicked()
                    {
                        acts.push(Act::SelectNoSinks);
                    }
                });
                crate::repo_chip::chip_row(ui, "xfer_sinks", "", group.sinks.len(), |ui, i| {
                    let sink = &group.sinks[i];
                    let mode = match sink.mode {
                        SyncMode::AddOnly => "ADD ONLY",
                        SyncMode::Mirror => "MIRROR",
                    };
                    let sel = self.selected_sinks.iter().any(|s| s == &sink.repo);
                    let accent = if sink.mode == SyncMode::Mirror {
                        theme::red()
                    } else {
                        theme::blue()
                    };
                    // The chip gets the bare repo name: the identicon is hashed from
                    // whatever string it is handed, so folding the mode into the name
                    // gave this sink a different glyph here than on every other tab.
                    // The mode rides alongside as its own label instead.
                    //
                    // Chip and label are wrapped together, and the *wrapper's*
                    // response is what this closure returns: `chip_row` packs rows
                    // from that rect, so a label drawn outside it would never be
                    // budgeted and the row would overrun the available width.
                    let row = ui.horizontal(|ui| {
                        let chip = crate::repo_chip::repo_chip(
                            ui,
                            &sink.repo,
                            sel,
                            accent,
                            false,
                            Some(self.locks.read_only(&sink.repo)),
                        );
                        // Same wording as the sink's mode pill on the Repositories tab.
                        ui.label(
                            RichText::new(format!("MODE: {mode}"))
                                .color(accent)
                                .size(10.0),
                        );
                        chip
                    });
                    // A MIRROR push can delete this sink's existing files, so a
                    // locked MIRROR sink cannot be included — the padlock next
                    // to it says why, and unlocking it re-enables the toggle.
                    // ADD ONLY sinks only ever gain files, so the lock does not
                    // bar them.
                    let barred = sink.mode == SyncMode::Mirror && self.locks.read_only(&sink.repo);
                    let (hover, hover_verbose) = if barred {
                        (
                            "Locked MIRROR sink — unlock it to include it in the push",
                            "This sink pushes in MIRROR mode, which can delete its existing \
                             files, and it is locked. Click its padlock to unlock it if you \
                             want the push to include it.",
                        )
                    } else {
                        (
                            "Include this sink in the push",
                            "Toggle whether this sink is included when GROUP SYNC runs. Its mode \
                             (ADD ONLY / MIRROR) is set on the Repositories tab.",
                        )
                    };
                    if row
                        .inner
                        .name
                        .explain(self.verbosity, hover, hover_verbose)
                        .clicked()
                        && !barred
                    {
                        acts.push(Act::ToggleSink(sink.repo.clone()));
                    }
                    self.locks
                        .handle_badge(row.inner.lock, self.verbosity, &sink.repo);
                    row.response
                });
                ui.label(
                    RichText::new(
                        "Each selected sink pushes in its own stored mode: ADD ONLY copies and \
                     never deletes; MIRROR also deletes what the main no longer has. The main \
                     is never changed.",
                    )
                    .color(theme::lilac())
                    .size(11.0),
                );
            },
        );
    }

    /// The delete policy the current command runs with: MIRROR always deletes
    /// everything the source lacks; SYNC deletes the source's own lost content
    /// only when DELETE MISSING is on; COPY/MOVE never reach here.
    fn sync_delete_mode(&self) -> SyncDelete {
        match self.command {
            Command::Mirror => SyncDelete::Absent,
            Command::Sync if self.sync_delete_missing => SyncDelete::Missing,
            _ => SyncDelete::None,
        }
    }

    /// SYNC's option bar: the DELETE MISSING toggle (off by default). SYNC has
    /// no subdir/folder/mode controls — it always mirrors source→target at the
    /// same relative path.
    /// DIFF's option bar: how the two repos are paired up.
    fn pairing_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        crate::lcars::section_lcars(
            ui,
            "PAIR BY — HOW FILES ARE MATCHED",
            theme::blue(),
            |ui| {
                ui.horizontal(|ui| {
                    for (pairing, label, short, verbose) in [
                        (
                            DiffPairing::ByHash,
                            "BY HASH",
                            "Match files by content",
                            "Match files by their content, so the same photo under two \
                         different names is one row you can resolve with a rename. \
                         This is the view for finding what one repo has and the other \
                         doesn't, whatever things are called.",
                        ),
                        (
                            DiffPairing::ByPath,
                            "BY PATH",
                            "Match files by name and folder",
                            "Match files by their path inside the repo, so the same name on \
                         both sides is one row — and when the two versions differ you can \
                         overwrite one side with the other. This is the view for spotting \
                         edited files.",
                        ),
                    ] {
                        let selected = self.pairing == pairing;
                        if crate::lcars::toggle_button(ui, label, selected, theme::blue())
                            .explain(self.verbosity, short, verbose)
                            .clicked()
                        {
                            acts.push(Act::SetPairing(pairing));
                        }
                    }
                });
                let hint = match self.pairing {
                    DiffPairing::ByHash => {
                        "Rows pair files with identical content; a file only one side has can \
                     be copied across or deleted."
                    }
                    DiffPairing::ByPath => {
                        "Rows pair files with the same path; same name with different content \
                     is a conflict you resolve per side."
                    }
                };
                ui.label(RichText::new(hint).color(theme::lilac()).size(11.0));
            },
        );
    }

    fn sync_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        crate::lcars::section_lcars(ui, "OPTIONS — SYNC BEHAVIOUR", theme::blue(), |ui| {
            ui.horizontal(|ui| {
                let fill = if self.sync_delete_missing {
                    theme::red()
                } else {
                    theme::panel()
                };
                let col = if self.sync_delete_missing {
                    theme::black()
                } else {
                    theme::red()
                };
                if ui
                    .add(egui::Button::new(RichText::new("DELETE MISSING").color(col)).fill(fill))
                    .explain(
                        self.verbosity,
                        "Also delete target files the source has lost",
                        "Off: SYNC only copies content the target lacks (the source is \
                         mirrored into the target, nothing is deleted). On: also delete \
                         target files whose content the source once had and has since \
                         lost — deletions cannot be undone.",
                    )
                    .clicked()
                {
                    acts.push(Act::ToggleSyncDelete);
                }
            });
            let hint = if self.sync_delete_missing {
                "Copies content the target lacks AND deletes target files the source has lost."
            } else {
                "Copies content the target lacks. Nothing in the target is deleted."
            };
            ui.label(RichText::new(hint).color(theme::lilac()).size(11.0));
        });
    }

    /// Whether the *current* run would delete on-disk data: MOVE and MIRROR
    /// always do, and SYNC does only when DELETE MISSING is on. Drives the red
    /// accent on the confirm dialog.
    /// Why the session locks bar this RUN, if they do. A command is barred only
    /// when the operation *as a whole* would delete or overwrite existing files
    /// in a locked repo — additions are always allowed (see [`crate::locks`]).
    fn lock_block(&self) -> Option<String> {
        match self.command {
            Command::Mirror => {
                let t = self.target.as_deref()?;
                self.locks.read_only(t).then(|| {
                    format!("MIRROR can delete files in '{t}' — unlock it (its padlock) to run.")
                })
            }
            Command::Move => {
                let s = self.source.as_deref()?;
                self.locks.read_only(s).then(|| {
                    format!("MOVE removes files from '{s}' — unlock it (its padlock) to run.")
                })
            }
            Command::Sync if self.sync_delete_missing => {
                let t = self.target.as_deref()?;
                self.locks.read_only(t).then(|| {
                    format!(
                        "SYNC with DELETE MISSING can delete files in '{t}' — unlock it or \
                         turn DELETE MISSING off."
                    )
                })
            }
            Command::GroupSync => {
                // Locked MIRROR sinks are excluded from the push; the run is
                // barred only when that leaves nothing to push to.
                let group = self.current_group.as_ref()?;
                let any_eligible = group.sinks.iter().any(|s| {
                    self.selected_sinks.contains(&s.repo)
                        && !(s.mode == SyncMode::Mirror && self.locks.read_only(&s.repo))
                });
                (!self.selected_sinks.is_empty() && !any_eligible).then(|| {
                    "Every selected sink is a locked MIRROR sink — unlock one (its padlock) \
                     or select an ADD ONLY sink."
                        .to_string()
                })
            }
            // COPY / SYNC (add-only) / GROUP SYNC BACK's batch promote only ever
            // add files; DIFF changes nothing by itself.
            _ => None,
        }
    }

    fn destructive_run(&self) -> bool {
        self.command.destructive()
            || (self.command == Command::Sync && self.sync_delete_missing)
            || (self.command == Command::GroupSync
                && self.current_group.as_ref().is_some_and(|g| {
                    g.sinks.iter().any(|s| {
                        self.selected_sinks.contains(&s.repo) && s.mode == SyncMode::Mirror
                    })
                }))
    }

    fn action_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        crate::lcars::section_lcars(ui, "ACTION — REVIEW & RUN", theme::amber(), |ui| {
            ui.horizontal(|ui| {
                let ready = self.ready();
                if ui
                    .add_enabled(
                        ready,
                        egui::Button::new(RichText::new("REVIEW").color(theme::black())),
                    )
                    .explain(
                        self.verbosity,
                        "Review the first transfers",
                        "Show the first matching `from → to` transfers (up to a \
                         limit) and a total count, without changing anything on disk. \
                         REVIEW and RUN are mutually exclusive — starting a run clears the \
                         review.",
                    )
                    .clicked()
                {
                    acts.push(Act::Preview);
                }
                if self.command.is_diff() {
                    if self.running || self.previewing {
                        ui.add(egui::Spinner::new().color(theme::amber()));
                    }
                    return;
                }
                let run = egui::Button::new(RichText::new("RUN").color(theme::black()))
                    .fill(theme::amber());
                // The session locks can bar the whole run (e.g. MIRROR into a
                // locked target). The disabled button's hover says exactly why
                // and which padlock to click.
                let lock_block = self.lock_block();
                let resp = ui.add_enabled(ready && lock_block.is_none(), run);
                let resp = if let Some(why) = &lock_block {
                    resp.on_disabled_hover_text(why.clone())
                } else {
                    resp.explain(
                        self.verbosity,
                        "Run the command",
                        "Run the selected command (COPY/MOVE) on a background thread, \
                         after a confirmation dialog. Progress, the current file, and a \
                         running count are shown live.",
                    )
                };
                if resp.clicked() {
                    acts.push(Act::Ask);
                }
                if self.running {
                    ui.add(egui::Spinner::new().color(theme::amber()));
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new("CANCEL").color(theme::ink_on(theme::red())),
                            )
                            .fill(theme::red()),
                        )
                        .explain(
                            self.verbosity,
                            "Stop the running operation",
                            "Cancel the in-progress operation. Files already transferred \
                             before cancelling stay as they are — this stops further \
                             work, it doesn't roll back.",
                        )
                        .clicked()
                    {
                        acts.push(Act::CancelRun);
                    }
                }
            });
        });
    }

    /// The rows a bulk action would touch: those currently *listed* on the
    /// board — after the show-unchanged toggle and excluding hidden rows.
    /// Hiding a row is how the user excludes it from a bulk action.
    fn listed_diff_rows(&self, metas: &[board::RowMeta]) -> Vec<usize> {
        metas
            .iter()
            .enumerate()
            .filter(|(_, m)| {
                !self.preview_board.hidden.contains(&m.key)
                    && (self.preview_board.show_unchanged || !m.unchanged)
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// The bulk operations worth offering for the rows on screen. A mode that
    /// cannot produce a relation never offers its bulk action — BY HASH yields
    /// no `Conflict`, BY PATH no `Renamed`.
    fn offered_bulk_ops(&self, listed: &[usize]) -> Vec<BulkOp> {
        let mut ops = Vec::new();
        let has = |want: DiffRelation| listed.iter().any(|&i| self.diff_rows[i].relation == want);
        if has(DiffRelation::OnlyLeft) {
            ops.push(BulkOp::CopyMissingRight);
        }
        if has(DiffRelation::OnlyRight) {
            ops.push(BulkOp::CopyMissingLeft);
        }
        if has(DiffRelation::Renamed) {
            ops.push(BulkOp::RenameAllLeft);
            ops.push(BulkOp::RenameAllRight);
        }
        ops
    }

    /// Every concrete file operation `op` would perform over `listed`.
    fn bulk_plan(&self, op: BulkOp, listed: &[usize]) -> Vec<crate::diff_board::BoardAction> {
        use crate::diff_board::BoardAction;
        let mut plan = Vec::new();
        for &i in listed {
            let row = &self.diff_rows[i];
            match (op, row.relation) {
                (BulkOp::CopyMissingRight, DiffRelation::OnlyLeft) => {
                    for f in &row.left {
                        plan.push(BoardAction::Copy {
                            from_left: true,
                            rel_path: f.rel_path.clone(),
                        });
                    }
                }
                (BulkOp::CopyMissingLeft, DiffRelation::OnlyRight) => {
                    for f in &row.right {
                        plan.push(BoardAction::Copy {
                            from_left: false,
                            rel_path: f.rel_path.clone(),
                        });
                    }
                }
                // Rename this side's file to the name the other side uses. Only
                // a 1:1 pair is unambiguous; a side holding several names needs
                // the per-row picker, so it is left out of the batch.
                (BulkOp::RenameAllLeft, DiffRelation::Renamed) => {
                    if let ([from], [to]) = (row.left.as_slice(), row.right.as_slice()) {
                        plan.push(BoardAction::Rename {
                            on_left: true,
                            from: from.rel_path.clone(),
                            to: to.rel_path.clone(),
                        });
                    }
                }
                (BulkOp::RenameAllRight, DiffRelation::Renamed) => {
                    if let ([to], [from]) = (row.left.as_slice(), row.right.as_slice()) {
                        plan.push(BoardAction::Rename {
                            on_left: false,
                            from: from.rel_path.clone(),
                            to: to.rel_path.clone(),
                        });
                    }
                }
                _ => {}
            }
        }
        plan
    }

    fn preview_panel(&mut self, ui: &mut egui::Ui, store: &Store, acts: &mut Vec<Act>) {
        if self.command.is_diff() {
            if self.diff_rows.is_empty() {
                ui.add_space(6.0);
                ui.colored_label(
                    theme::text(),
                    "Pick two repos and press REVIEW to compare them.",
                );
                return;
            }
            let left_ro = self
                .source
                .as_deref()
                .is_none_or(|r| self.locks.read_only(r));
            let right_ro = self
                .target
                .as_deref()
                .is_none_or(|r| self.locks.read_only(r));
            let metas = diff_metas(&self.diff_rows, left_ro, right_ro);
            // Facts are looked up per visible row rather than carried on the
            // rows: `DiffFile` has only a path, size and date, and the board
            // asks for a body only for what is on screen.
            let (ldb, lbase) = self
                .source
                .as_deref()
                .map(|r| open_facts(store, r))
                .unwrap_or((None, None));
            let (rdb, rbase) = self
                .target
                .as_deref()
                .map(|r| open_facts(store, r))
                .unwrap_or((None, None));
            // Bulk actions over everything currently listed. Offered above the
            // board, so it reads as acting on the whole list rather than a row.
            let listed = self.listed_diff_rows(&metas);
            let offered = self.offered_bulk_ops(&listed);
            if !offered.is_empty() {
                let mut want: Option<BulkOp> = None;
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        egui::RichText::new("ALL LISTED")
                            .color(theme::lilac())
                            .size(11.0),
                    );
                    for op in &offered {
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new(op.label()).color(theme::black()),
                                )
                                .fill(theme::amber()),
                            )
                            .explain(
                                self.verbosity,
                                "Apply to every listed row",
                                "Run this action on every row currently on the board. Rows \
                                 you have hidden are left alone.",
                            )
                            .clicked()
                        {
                            want = Some(*op);
                        }
                    }
                });
                if let Some(op) = want {
                    let plan = self.bulk_plan(op, &listed);
                    if plan.is_empty() {
                        self.status = Some("Nothing listed for that action.".to_string());
                    } else {
                        self.bulk_confirm = Some((op, plan));
                    }
                }
                ui.add_space(4.0);
            }

            let rows = &self.diff_rows;
            let action = board::board(
                ui,
                &mut self.preview_board,
                &metas,
                board::BoardView {
                    left_role: "LEFT",
                    left_repo: self.source.as_deref().unwrap_or(""),
                    left_is_main: false,
                    left_path: &self.preview_source_header,
                    right: Some(board::RightHeader {
                        role: "RIGHT",
                        repo: self.target.as_deref().unwrap_or(""),
                        is_main: false,
                        path: &self.preview_target_header,
                        multi_repo: false,
                    }),
                    totals: diff_totals(rows),
                    full_len: rows.len(),
                    // DIFF runs each command as it is clicked; there is no RUN
                    // for a hidden row to be skipped by.
                    hide_skips_run: false,
                },
                &mut self.thumbs,
                &mut |i| {
                    let row = &rows[i];
                    let side = |files: &[dedup_core::diff::DiffFile],
                                db: Option<&redb::Database>,
                                base: Option<&str>,
                                deleted_here: bool| {
                        board::SideBody {
                            facts: files.first().and_then(|f| facts_for(db, base, &f.rel_path)),
                            repo: None,
                            repo_is_main: false,
                            // Golden rule: a side with no file but a tombstone
                            // for this content tells its own story — a blue
                            // WAS DELETED cell. The living file stays plain.
                            overlay: (files.is_empty() && deleted_here)
                                .then_some(crate::media_cell::CellOverlay::WasDeleted),
                        }
                    };
                    board::RowBody {
                        left: side(
                            &row.left,
                            ldb.as_deref(),
                            lbase.as_deref(),
                            row.deleted_in_left,
                        ),
                        right: side(
                            &row.right,
                            rdb.as_deref(),
                            rbase.as_deref(),
                            row.deleted_in_right,
                        ),
                    }
                },
            );
            if let Some(a) = action
                && let Some(mapped) = diff_action(&self.diff_rows, a.row, a.cmd)
            {
                acts.push(Act::Board(mapped));
            }
            // The three follow-up modals (delete-all, keep-one, pick-a-name)
            // still belong to the diff board's own state.
            if let Some(answer) =
                crate::diff_board::popup(ui, &mut self.board_state, &self.diff_rows)
            {
                acts.push(Act::Board(answer));
            }
            return;
        }
        if self.preview.is_empty() {
            ui.add_space(6.0);
            let hint = if self.command == Command::GroupSync {
                "Pick at least one sink above, then press REVIEW."
            } else {
                "Pick a source, a target and a command, then press REVIEW."
            };
            ui.colored_label(theme::text(), hint);
            return;
        }
        // GROUP SYNC pushes several sinks as one all-or-nothing run, so each
        // row names its own sink and offers no commands (the rows themselves
        // carry an empty command set).
        let group_sync = self.command == Command::GroupSync;
        let group_back = self.command == Command::GroupSyncBack;
        let (left_role, right_role) = if group_sync {
            ("MAIN", "SINKS")
        } else if group_back {
            ("MAIN", "SINK")
        } else {
            ("SOURCE", "TARGET")
        };
        let bodies = std::mem::take(&mut self.preview_bodies);
        // The review shows only the buttons the session locks allow: while the
        // whole command is barred (e.g. MOVE with a locked source), each row's
        // APPLY is withheld; while the back-sync sink is locked, its DELETE R
        // is withheld (the promote `< COPY` only adds and always stays). HIDE
        // stays either way, and unlocking the padlock brings the rest back.
        let strip_apply = self.lock_block().is_some();
        let strip_del_r = self.command == Command::GroupSyncBack
            && self
                .selected_sinks
                .first()
                .is_some_and(|s| self.locks.read_only(s));
        let lock_filtered: Vec<board::RowMeta>;
        let metas: &[board::RowMeta] = if strip_apply || strip_del_r {
            lock_filtered = self
                .preview
                .iter()
                .map(|m| {
                    let mut m = m.clone();
                    m.cmds.retain(|&c| {
                        !(strip_apply && c == board::Cmd::Apply)
                            && !(strip_del_r && c == board::Cmd::DeleteRight)
                    });
                    m
                })
                .collect();
            &lock_filtered
        } else {
            &self.preview
        };
        let action = board::board(
            ui,
            &mut self.preview_board,
            metas,
            board::BoardView {
                left_role,
                left_repo: self.source.as_deref().unwrap_or(""),
                left_is_main: group_sync || group_back,
                left_path: &self.preview_source_header,
                // Transfer is always two-sided (source → target/folder/sinks).
                right: Some(board::RightHeader {
                    role: right_role,
                    // GROUP SYNC's right side spans several repos, so the
                    // header names none of them — each row carries its own
                    // chip. GROUP SYNC BACK's right side is the one selected
                    // sink, named with a full chip like every other header.
                    repo: if group_sync {
                        ""
                    } else if group_back {
                        self.selected_sinks
                            .first()
                            .map(String::as_str)
                            .unwrap_or("")
                    } else {
                        self.target.as_deref().unwrap_or("")
                    },
                    is_main: false,
                    path: &self.preview_target_header,
                    multi_repo: group_sync,
                }),
                totals: self.preview_totals,
                // Every row the plan produced, not just the actionable ones —
                // the cap notice compares this against what was materialised.
                full_len: self.preview_totals.iter().sum(),
                hide_skips_run: true,
            },
            &mut self.thumbs,
            &mut |i| bodies.get(i).cloned().unwrap_or_default(),
        );
        self.preview_bodies = bodies;
        if let Some(a) = action
            && let Some(meta) = self.preview.get(a.row)
        {
            match a.cmd {
                // APPLY (planned previews) and the back-sync `< COPY` promote
                // both run just this row.
                board::Cmd::Apply | board::Cmd::CopyLeft => {
                    acts.push(Act::ApplyRow(meta.key.clone()));
                }
                // The back-sync triage's other half: purge the file from the
                // sink instead of promoting it.
                board::Cmd::DeleteRight => {
                    if let Some(rel) = meta.right_paths.first() {
                        acts.push(Act::DeleteSinkRow(rel.clone()));
                    }
                }
                // Clicking the row opens the file that actually exists — the
                // sink's copy on a back-sync board, the source's otherwise (a
                // planned target file may not be on disk yet). A row with no
                // source side at all — a SYNC/MIRROR *deletion* row — falls
                // back to the target's file: those are exactly the rows worth
                // inspecting before data is lost, so a click must never be a
                // no-op.
                board::Cmd::OpenRow => {
                    let (repo, rel) = if self.command == Command::GroupSyncBack {
                        (
                            self.selected_sinks.first().cloned(),
                            meta.right_paths.first().cloned(),
                        )
                    } else if let Some(rel) = meta.left_paths.first() {
                        (self.source.clone(), Some(rel.clone()))
                    } else {
                        (self.target.clone(), meta.right_paths.first().cloned())
                    };
                    if let (Some(repo), Some(rel)) = (repo, rel) {
                        acts.push(Act::OpenPreviewRow(repo, rel));
                    }
                }
                _ => {}
            }
        }
    }

    /// A repo's absolute path for a review-table column header, falling back to
    /// its name if it can't be resolved.
    fn repo_header(store: &Store, name: &str) -> String {
        store
            .get_repo(name)
            .map(|m| m.abs_path)
            .unwrap_or_else(|_| name.to_string())
    }

    /// The live run panel: a spinner, the file currently being handled, a
    /// scrolling list of the last N actions and a running summary line. Styled
    /// like the repo scan progress in `app.rs`.
    fn run_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if self.running {
                ui.add(egui::Spinner::new().color(theme::amber()));
            }
            let current = if self.run_current.is_empty() {
                "preparing…".to_string()
            } else {
                self.run_current.clone()
            };
            ui.label(RichText::new(current).color(theme::amber()).strong());
        });

        let summary = if self.run_total > 0 {
            format!("{} / {}", self.run_done, self.run_total)
        } else {
            self.run_done.to_string()
        };
        ui.label(
            RichText::new(format!("Processed {summary}"))
                .color(theme::tan())
                .size(12.0),
        );

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .max_height(180.0)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for line in &self.run_log {
                    ui.label(RichText::new(line).color(theme::text()).size(12.0));
                }
            });
    }

    fn confirm_modal(&mut self, ui: &mut egui::Ui, prompt: &str, acts: &mut Vec<Act>) {
        egui::Modal::new(Id::new("transfer-confirm")).show(&ui.ctx().clone(), |ui| {
            ui.set_width(380.0);
            ui.label(
                RichText::new(format!("CONFIRM {}", self.command.label()))
                    .color(theme::amber())
                    .size(16.0)
                    .strong(),
            );
            ui.add_space(6.0);
            ui.colored_label(theme::text(), prompt);
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let fill = if self.destructive_run() {
                    theme::red()
                } else {
                    theme::amber()
                };
                if ui
                    .add(
                        egui::Button::new(RichText::new("PROCEED").color(theme::black()))
                            .fill(fill),
                    )
                    .explain(
                        self.verbosity,
                        "Confirm and run",
                        "Confirm and start the run on a background thread.",
                    )
                    .clicked()
                {
                    acts.push(Act::Confirm);
                }
                if ui
                    .button(RichText::new("CANCEL").color(theme::black()))
                    .explain(
                        self.verbosity,
                        "Cancel",
                        "Close this dialog without running anything.",
                    )
                    .clicked()
                {
                    acts.push(Act::CancelConfirm);
                }
            });
        });
    }

    fn apply(&mut self, store: &Arc<Store>, frame: Option<&eframe::Frame>, act: Act) {
        match act {
            Act::PickSource(name) => {
                if self.target.as_deref() == Some(name.as_str()) {
                    self.target = None;
                }
                self.extra_refs.retain(|r| r != &name);
                self.source = Some(name);
                self.refresh_group(store);
                self.clear_preview();
            }
            Act::PickTarget(name) => {
                self.extra_refs.retain(|r| r != &name);
                self.target = Some(name);
                self.clear_preview();
            }
            Act::ToggleExtraRef(name) => {
                if let Some(pos) = self.extra_refs.iter().position(|r| r == &name) {
                    self.extra_refs.remove(pos);
                } else {
                    self.extra_refs.push(name);
                }
                self.clear_preview();
            }
            Act::SetCommand(cmd) => {
                self.command = cmd;
                // SYNC/MIRROR are repo→repo only; snap the destination back to a
                // repo so the target row is available (the DEST toggle is hidden).
                if cmd.repo_to_repo() {
                    self.destination = Destination::Repo;
                }
                self.clear_preview();
            }
            Act::SetDestination(dest) => {
                self.destination = dest;
                self.clear_preview();
            }
            Act::SetMode(mode) => {
                self.select_mode = mode;
                self.clear_preview();
            }
            Act::ToggleInvert => {
                self.invert = !self.invert;
                self.clear_preview();
            }
            Act::ToggleSyncDelete => {
                self.sync_delete_missing = !self.sync_delete_missing;
                self.clear_preview();
            }
            Act::FolderChanged => self.clear_preview(),
            Act::BrowseFolder => {
                if let Some(frame) = frame {
                    self.browse_folder(frame);
                }
            }
            Act::SubdirChanged => self.clear_preview(),
            Act::BrowseSubdir => {
                if let Some(frame) = frame {
                    self.browse_subdir(store, frame);
                }
            }
            Act::Preview => self.run_preview(store),
            Act::Ask => {
                // Plan on a worker thread; the confirmation is raised (for this
                // captured config) once the plan lands with real counts. DIFF
                // has no batch RUN, so it never reaches here.
                self.reset_run();
                if self.command == Command::GroupSync {
                    self.spawn_group_preview(store, true);
                } else if self.command == Command::GroupSyncBack {
                    self.spawn_group_back_preview(store, true);
                } else if let Some(config) = self.capture_run_config() {
                    let confirm = Box::new(config.clone());
                    self.spawn_review_preview(store, config, Some(confirm));
                }
            }
            Act::CancelConfirm => {
                self.confirm = None;
                self.pending_confirm = None;
                self.pending_group_confirm = None;
                self.pending_group_back = None;
            }
            Act::Confirm => {
                self.confirm = None;
                // Run the config/group the confirmation was built for, not
                // live state.
                if let Some((main, sink)) = self.pending_group_back.take() {
                    self.start_group_back_pull(store, main, sink, None);
                } else if let Some(group) = self.pending_group_confirm.take() {
                    self.start_group_sync(store, group);
                } else if let Some(config) = self.pending_confirm.take() {
                    self.start(store, *config, None);
                }
            }
            Act::ApplyRow(key) => {
                // GROUP SYNC BACK has no RunConfig — a row's APPLY pulls just that
                // file into the main (this is how a single resurrection is opted
                // in). Every other command runs against the previewed config.
                if self.command == Command::GroupSyncBack {
                    // A back-sync promote only *adds* to the main, so no lock
                    // can bar it.
                    if let (Some(group), Some(sink)) = (
                        self.current_group.clone(),
                        self.selected_sinks.first().cloned(),
                    ) {
                        let only = std::iter::once(key).collect();
                        self.start_group_back_pull(store, group.main, sink, Some(only));
                    }
                } else if self.lock_block().is_none()
                    && let Some(config) = self.capture_run_config()
                {
                    // Never act past the lock, even from a preview built before
                    // the repo was re-locked.
                    self.start(store, config, Some(key));
                }
            }
            Act::DeleteSinkRow(rel) => {
                // Back-sync triage: this sink file is wanted neither in the
                // main nor in the sink. Deleting existing data needs the sink
                // unlocked — the button is withheld while locked, and this
                // re-check means a stale frame can never slip past the lock.
                let Some(sink) = self.selected_sinks.first().cloned() else {
                    return;
                };
                if self.locks.read_only(&sink) {
                    return;
                }
                let deleted = store
                    .get_repo(&sink)
                    .map_err(|e| e.to_string())
                    .and_then(|meta| {
                        let path = std::path::PathBuf::from(&meta.abs_path).join(&rel);
                        match std::fs::remove_file(&path) {
                            Ok(()) => Ok(()),
                            // Already gone — removing the index entry is still right.
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                            Err(e) => Err(format!("could not delete {}: {e}", path.display())),
                        }
                    })
                    .and_then(|()| {
                        store
                            .remove_file_entry(&sink, &rel)
                            .map_err(|e| e.to_string())
                    });
                match deleted {
                    // The plan changed on disk: rebuild the preview so the row
                    // disappears with correct counts.
                    Ok(()) => self.run_preview(store),
                    Err(e) => self.error = Some(e),
                }
            }
            Act::OpenPreviewRow(repo, rel) => self.open_single(store, &repo, &rel),
            Act::SetPairing(pairing) => {
                self.pairing = pairing;
                self.clear_preview();
            }
            Act::Board(crate::diff_board::BoardAction::Inspect {
                left_rel,
                right_rel,
            }) => self.open_inspect(store, left_rel.as_deref(), right_rel.as_deref()),
            // A follow-up question is board state, not a file operation: it
            // opens the modal rather than running anything.
            Act::Board(crate::diff_board::BoardAction::OpenPopup { row, on_left, kind }) => {
                self.board_state.popup = Some(crate::diff_board::Popup { row, on_left, kind });
            }
            Act::Board(action) => {
                self.board_state.popup = None;
                self.start_board_action(store, action)
            }
            Act::CancelRun => self.cancel.cancel(),
            Act::ToggleSink(name) => {
                if let Some(pos) = self.selected_sinks.iter().position(|s| s == &name) {
                    self.selected_sinks.remove(pos);
                } else {
                    self.selected_sinks.push(name);
                }
                self.clear_preview();
            }
            Act::SelectAllSinks => {
                self.selected_sinks = self
                    .current_group
                    .as_ref()
                    .map(|g| g.sinks.iter().map(|s| s.repo.clone()).collect())
                    .unwrap_or_default();
                self.clear_preview();
            }
            Act::SelectNoSinks => {
                self.selected_sinks.clear();
                self.clear_preview();
            }
            Act::SelectOnlySink(name) => {
                self.selected_sinks = vec![name];
                self.clear_preview();
            }
        }
    }

    fn clear_preview(&mut self) {
        self.pending_confirm = None;
        self.pending_group_confirm = None;
        self.wholesale_sinks.clear();
        self.preview.clear();
        self.diff_rows.clear();
        // A popup (and an open comparison) belongs to the rows it was opened
        // from.
        self.board_state.popup = None;
        self.inspect = None;
        self.preview_totals = [0; 4];
        self.preview_source_header.clear();
        self.preview_target_header.clear();
        self.preview_total = 0;
        self.sync_delete_total = 0;
        self.preview_bodies.clear();
        // Hidden rows are keyed to the preview they were hidden in.
        self.preview_board.hidden.clear();
    }

    /// The subdir trimmed of surrounding whitespace and slashes; empty means
    /// "place files at the target root".
    fn normalized_subdir(&self) -> String {
        self.subdir.trim().trim_matches('/').to_string()
    }

    /// Open the native folder dialog rooted at the target repo and, on a pick,
    /// store the chosen folder as a path relative to the target root.
    fn browse_subdir(&mut self, store: &Arc<Store>, frame: &eframe::Frame) {
        let Some(target) = self.target.clone() else {
            return;
        };
        let target_root = match store.get_repo(&target) {
            Ok(meta) => PathBuf::from(meta.abs_path),
            Err(e) => {
                self.error = Some(e.to_string());
                return;
            }
        };
        // Run the native picker modally, parented to our window, so it grabs
        // focus and a second one can't be opened while it's up.
        if let Some(dir) = rfd::FileDialog::new()
            .set_title("Choose a subdirectory inside the target")
            .set_directory(&target_root)
            .set_parent(frame)
            .pick_folder()
        {
            // Contextually anchored to the target root, but still feeds the
            // global picker memory.
            crate::util::remember_picked_dir(&dir);
            match dir.strip_prefix(&target_root) {
                Ok(rel) => {
                    self.subdir = rel.to_string_lossy().replace('\\', "/");
                    self.error = None;
                    self.clear_preview();
                }
                Err(_) => {
                    self.error = Some("The chosen folder is outside the target repo.".to_string());
                }
            }
        }
    }

    /// Open the native folder dialog and store the picked absolute path as the
    /// export folder (Destination::Folder).
    fn browse_folder(&mut self, frame: &eframe::Frame) {
        // Start in the current export folder when set; else at the parent of
        // the last selection anywhere (the shared picker memory) — never
        // dumped back at the home directory.
        let start = Some(self.folder.clone())
            .filter(|f| Path::new(f).is_dir())
            .map(std::path::PathBuf::from)
            .or_else(|| {
                crate::util::last_picked_dir().and_then(|last| {
                    let p = last.parent().map(|p| p.to_path_buf()).unwrap_or(last);
                    p.is_dir().then_some(p)
                })
            });
        // Run the native picker modally, parented to our window, so it grabs
        // focus and a second one can't be opened while it's up.
        let mut dialog = rfd::FileDialog::new()
            .set_title("Choose the export folder")
            .set_parent(frame);
        if let Some(dir) = start {
            dialog = dialog.set_directory(dir);
        }
        if let Some(dir) = dialog.pick_folder() {
            crate::util::remember_picked_dir(&dir);
            self.folder = dir.to_string_lossy().into_owned();
            self.error = None;
            self.clear_preview();
        }
    }

    /// The current filter expression composed by the shared wizard.
    fn filter_string(&self) -> Option<String> {
        self.filter.filter_string()
    }

    fn run_preview(&mut self, store: &Arc<Store>) {
        let Some(source) = self.source.clone() else {
            return;
        };
        // REVIEW and RUN are mutually exclusive: reviewing drops any run log.
        self.reset_run();
        if self.command.is_diff() {
            self.run_preview_diff(store, &source);
            return;
        }
        if self.command == Command::GroupSync {
            self.spawn_group_preview(store, false);
            return;
        }
        if self.command == Command::GroupSyncBack {
            self.spawn_group_back_preview(store, false);
            return;
        }
        // Every other command feeds the review board. Plan off the UI thread.
        if let Some(config) = self.capture_run_config() {
            self.spawn_review_preview(store, config, None);
        }
    }

    /// Snapshot the source, command, destination, filter and move flag the
    /// current controls describe — the whole of what a preview and a run need.
    /// Returns `None` when a required repo/folder is not chosen.
    fn capture_run_config(&self) -> Option<RunConfig> {
        // DIFF has no batch run — it is applied row by row. `Command::Diff` is
        // in `repo_to_repo()`, so without this it would build a bogus Sync
        // config (reachable via the R shortcut, which fires regardless of mode).
        // GROUP SYNC has its own plan/run pipeline (`spawn_group_preview` /
        // `start_group_sync`) since it targets several sinks, not one target.
        if self.command.is_diff() || self.command == Command::GroupSync {
            return None;
        }
        let source = self.source.clone()?;
        let command = self.command;
        let dest = if command.repo_to_repo() {
            StartDest::Sync {
                target: self.target.clone()?,
                delete: self.sync_delete_mode(),
                mirror: command == Command::Mirror,
            }
        } else {
            match self.destination {
                Destination::Repo => {
                    let target = self.target.clone()?;
                    StartDest::Repo {
                        references: self.references(&target),
                        target,
                        subdir: self.normalized_subdir(),
                    }
                }
                Destination::Folder => {
                    let folder = self.folder.trim().to_string();
                    if folder.is_empty() {
                        return None;
                    }
                    StartDest::Folder {
                        references: self.folder_references(),
                        dir: PathBuf::from(&folder),
                        mode: self.folder_mode(),
                        invert: self.invert,
                    }
                }
            }
        };
        Some(RunConfig {
            source,
            command,
            dest,
            filter: self.filter_string(),
            move_files: command == Command::Move,
        })
    }

    /// Plan a review-board preview on a worker thread. `confirm`, when set,
    /// rides through to the result: the RUN confirmation is raised (for that
    /// captured config) once the plan lands with real counts.
    /// Plan a GROUP SYNC push on a worker thread: every sink currently
    /// selected, filtered from the full group. `confirm` carries through to
    /// the result — when set, the RUN confirmation is raised once the plan
    /// lands with real counts.
    fn spawn_group_preview(&mut self, store: &Arc<Store>, confirm: bool) {
        let Some(group) = self.current_group.clone() else {
            return;
        };
        let group = SyncGroup {
            main: group.main,
            sinks: group
                .sinks
                .into_iter()
                .filter(|s| self.selected_sinks.contains(&s.repo))
                // A locked MIRROR sink is never pushed to — MIRROR can delete
                // its existing files, and the lock forbids that. (The include
                // toggle already bars it; this is the backstop for a sink
                // locked after being selected.)
                .filter(|s| !(s.mode == SyncMode::Mirror && self.locks.read_only(&s.repo)))
                .collect(),
        };
        if group.sinks.is_empty() {
            return;
        }
        let filter = self.filter_string();
        let store = Arc::clone(store);
        let tx = self.tx.clone();
        self.previewing = true;
        self.status = Some("planning…".to_string());
        std::thread::spawn(move || {
            let result = build_group_preview(&store, &group, filter.as_deref());
            let _ = tx.send(Msg::GroupPreview { result, confirm });
        });
    }

    /// Plan a GROUP SYNC BACK pull of the single selected sink into the main,
    /// off the UI thread. `confirm` defers the RUN confirmation until the plan
    /// lands with real counts.
    fn spawn_group_back_preview(&mut self, store: &Arc<Store>, confirm: bool) {
        let Some(group) = self.current_group.clone() else {
            return;
        };
        let Some(sink) = self.selected_sinks.first().cloned() else {
            return;
        };
        let filter = self.filter_string();
        let store = Arc::clone(store);
        let tx = self.tx.clone();
        self.previewing = true;
        self.status = Some("planning…".to_string());
        std::thread::spawn(move || {
            let result = build_group_back_preview(&store, &group, &sink, filter.as_deref());
            let _ = tx.send(Msg::GroupBackPreview { result, confirm });
        });
    }

    /// Fold a finished GROUP SYNC BACK plan into the board: new files to promote
    /// (green) and resurrection candidates (blue). When `confirm`, raise the RUN
    /// confirmation for the batch promote (new files only).
    fn apply_group_back_preview(
        &mut self,
        result: Result<GroupPreviewData, String>,
        confirm: bool,
    ) {
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(e) => {
                self.error = Some(e);
                return;
            }
        };
        // The 4-slot summary has no resurrection bucket; new files ride the
        // "only on one side" (green) slot, and the resurrection count is spoken
        // in the status line and shown as the blue rows themselves.
        self.preview_totals = [0, outcome.added, 0, 0];
        self.preview_total = outcome.added + outcome.removed;
        self.preview_source_header = outcome.main_header.clone();
        self.preview_target_header = outcome
            .group
            .sinks
            .first()
            .map(|s| s.repo.clone())
            .unwrap_or_default();
        self.wholesale_sinks = Vec::new();
        self.preview = outcome.rows;
        self.preview_bodies = outcome.bodies;
        self.status = Some(format!(
            "{} new file(s) to promote, {} resurrection candidate(s).",
            outcome.added, outcome.removed
        ));
        self.error = None;
        if confirm {
            // The batch promotes only the new files; resurrection is per-row.
            let sink = outcome
                .group
                .sinks
                .first()
                .map(|s| s.repo.clone())
                .unwrap_or_default();
            self.confirm = Some(format!(
                "Promote {} new file(s) from sink '{}' into main '{}'? {} resurrection \
                 candidate(s) are left for you to pull one by one. Nothing on the sink is \
                 changed.",
                outcome.added, sink, outcome.group.main, outcome.removed
            ));
            self.pending_group_back = Some((outcome.group.main.clone(), sink));
        }
    }

    /// Fold a finished GROUP SYNC plan into the board and, if this plan was
    /// for a RUN click, raise the confirmation now that the real counts are
    /// known.
    fn apply_group_preview(&mut self, result: Result<GroupPreviewData, String>, confirm: bool) {
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(e) => {
                self.error = Some(e);
                return;
            }
        };
        self.preview_totals = [outcome.removed, outcome.added, 0, 0];
        self.preview_total = outcome.added + outcome.removed;
        // The left header names the main like every other surface does — by its
        // path, not its bare name. The right one says how many sinks the push
        // covers; each row names the sink it belongs to with its own chip.
        self.preview_source_header = outcome.main_header.clone();
        self.preview_target_header = format!(
            "{} sink(s) selected",
            outcome
                .group
                .sinks
                .len()
                .min(self.selected_sinks.len().max(1))
        );
        self.wholesale_sinks = outcome.wholesale_sinks;
        // The board sorts through its own index; the caller just hands over the
        // rows and their bodies, index-aligned.
        self.preview = outcome.rows;
        self.preview_bodies = outcome.bodies;
        self.status = Some(format!(
            "{} file(s) to copy, {} to delete across {} sink(s).",
            outcome.added, outcome.removed, outcome.sink_count
        ));
        self.error = None;
        if confirm {
            // Confirm and push the group that was *planned*, captured here —
            // the selection may have changed while the scan ran.
            self.raise_group_confirm(&outcome.group);
            self.pending_group_confirm = Some(outcome.group);
        }
    }

    /// Build the GROUP SYNC RUN confirmation from the plan just applied. A
    /// confirmation that cannot say how much it deletes is not one the user
    /// can weigh.
    fn raise_group_confirm(&mut self, group: &SyncGroup) {
        let [deletes, copies, _, _] = self.preview_totals;
        let mut prompt = format!(
            "Push '{}' to {} sink(s): copy {copies} file(s)",
            group.main,
            group.sinks.len()
        );
        if group.sinks.iter().any(|s| s.mode == SyncMode::Mirror) {
            prompt.push_str(&format!(
                " and DELETE {deletes} file(s) from the mirror sink(s), which cannot be undone"
            ));
        }
        prompt.push_str(". The main is never changed.");
        if !self.wholesale_sinks.is_empty() {
            // Say what actually happens: nothing the sink holds today
            // survives, and the main's content takes its place. It is not
            // left empty — claiming that would be false, and a confirmation
            // nobody trusts is worse than none.
            let listed: Vec<String> = self
                .wholesale_sinks
                .iter()
                .map(|(sink, live)| format!("{sink} (all {live} of its files)"))
                .collect();
            prompt.push_str(&format!(
                "\n\nWARNING: this replaces the entire current contents of {} with the main's \
                 content — nothing they hold today survives. If that is not what you expect, \
                 check the main is complete first.",
                listed.join(", ")
            ));
        }
        self.confirm = Some(prompt);
    }

    /// Push `group` (already filtered to the sinks that were selected when it
    /// was planned) to every one of its sinks, off the UI thread. Live
    /// progress flows through the same `ChannelDiffProgress` → `run_problems`
    /// path every other command uses; only the terminal aggregation across
    /// sinks is GROUP SYNC's own.
    /// Run a GROUP SYNC BACK pull of `sink` into `main`. `only = None` is the
    /// batch: promote every **new** file, deliberately excluding the resurrection
    /// set (tombstoned content the sink still holds). `only = Some(keys)` pulls
    /// exactly those rows — how a single resurrection is opted in. Off the UI
    /// thread; reports through the shared GROUP SYNC done channel.
    fn start_group_back_pull(
        &mut self,
        store: &Arc<Store>,
        main: String,
        sink: String,
        only: Option<std::collections::HashSet<String>>,
    ) {
        let filter = self.filter_string();
        let store = Arc::clone(store);
        let tx = self.tx.clone();
        self.cancel = CancellationToken::new();
        let cancel = self.cancel.clone();
        self.running = true;
        self.status = Some(format!("pulling '{sink}' into '{main}'…"));
        self.clear_preview();
        self.reset_run();

        std::thread::spawn(move || {
            let keys: std::collections::HashSet<String> = match only {
                Some(keys) => keys,
                None => {
                    match dedup_core::diff::plan_sync_back(&store, &sink, &main, filter.as_deref())
                    {
                        Ok(items) => items
                            .into_iter()
                            .filter(|i| i.kind == dedup_core::diff::PullKind::New)
                            .map(|i| dedup_core::diff::source_key(&i.rel_path))
                            .collect(),
                        Err(e) => {
                            let _ = tx.send(Msg::GroupDone(Err(e.to_string())));
                            return;
                        }
                    }
                }
            };
            let progress = ChannelDiffProgress { tx: tx.clone() };
            let run = DiffRun::new(&progress, &cancel).with_selection(None, Some(&keys));
            // Copy sink content the main lacks, scoped to the new files, into the
            // main at the same relative path; the main is re-indexed by the sync.
            let result = dedup_core::diff::diff_sync(
                &store,
                &sink,
                &main,
                true,
                SyncDelete::None,
                filter.as_deref(),
                &run,
            );
            let done = match result {
                Ok(stats) => Ok(GroupSyncResult {
                    main: main.clone(),
                    copied: stats.copied,
                    deleted: 0,
                    errors: stats.errors,
                    cancelled: stats.cancelled,
                    failures: Vec::new(),
                    skipped: Vec::new(),
                }),
                Err(e) => Err(format!("{sink}: {e}")),
            };
            let _ = tx.send(Msg::GroupDone(done));
        });
    }

    fn start_group_sync(&mut self, store: &Arc<Store>, group: SyncGroup) {
        let filter = self.filter_string();
        let store = Arc::clone(store);
        let tx = self.tx.clone();
        self.cancel = CancellationToken::new();
        let cancel = self.cancel.clone();
        self.running = true;
        self.status = Some(format!("syncing '{}'…", group.main));
        self.clear_preview();
        self.reset_run();

        std::thread::spawn(move || {
            // Re-checked here, not just at plan time: the main could have
            // been rescanned to empty in the gap between REVIEW and RUN.
            if let Err(e) = guard_mirror_source(&store, &group) {
                let _ = tx.send(Msg::GroupDone(Err(e.to_string())));
                return;
            }
            let progress = ChannelDiffProgress { tx: tx.clone() };
            let run = DiffRun::new(&progress, &cancel);
            let (mut copied, mut deleted, mut errors) = (0u64, 0u64, 0u64);
            let mut failures = Vec::new();
            let mut skipped = Vec::new();
            let mut cancelled = false;
            for sink in &group.sinks {
                if run.cancel.is_cancelled() {
                    cancelled = true;
                    skipped.push(sink.repo.clone());
                    continue;
                }
                match diff_sync(
                    &store,
                    &group.main,
                    &sink.repo,
                    true,
                    delete_mode(sink.mode),
                    filter.as_deref(),
                    &run,
                ) {
                    Ok(stats) => {
                        copied += stats.copied;
                        deleted += stats.deleted;
                        errors += stats.errors;
                        cancelled |= stats.cancelled;
                    }
                    Err(e) => failures.push(format!("{}: {e}", sink.repo)),
                }
            }
            let _ = tx.send(Msg::GroupDone(Ok(GroupSyncResult {
                main: group.main,
                copied,
                deleted,
                errors,
                cancelled,
                failures,
                skipped,
            })));
        });
    }

    fn spawn_review_preview(
        &mut self,
        store: &Arc<Store>,
        config: RunConfig,
        confirm: Option<Box<RunConfig>>,
    ) {
        let store = Arc::clone(store);
        let tx = self.tx.clone();
        self.previewing = true;
        self.status = Some(format!("{}…", config.command.label().to_lowercase()));
        std::thread::spawn(move || {
            let result = build_review_preview(&store, &config);
            let _ = tx.send(Msg::ReviewPreview { result, confirm });
        });
    }

    /// Fold a finished review preview into the board, and — if the plan was for
    /// a RUN click — raise its confirmation now that the counts are known.
    fn apply_review_preview(
        &mut self,
        result: Result<ReviewPreviewData, String>,
        confirm: Option<Box<RunConfig>>,
    ) {
        let data = match result {
            Ok(data) => data,
            Err(e) => {
                self.error = Some(e);
                return;
            }
        };
        self.preview_total = data.preview_total;
        self.sync_delete_total = data.sync_delete_total;
        self.preview_totals = data.preview_totals;
        self.preview_source_header = data.source_header;
        self.preview_target_header = data.target_header;
        self.preview = data.rows;
        self.preview_bodies = data.bodies;
        self.status = Some(data.status);
        self.error = None;
        if let Some(config) = confirm {
            // Confirm and run the config that was *planned*, not whatever the
            // live controls say now — the two can differ across the async gap.
            if let Some(mut prompt) =
                prompt_for(&config, data.preview_total, data.sync_delete_total)
            {
                let hidden = self.preview_board.hidden.len();
                if hidden > 0 {
                    prompt.push_str(&format!(" {hidden} hidden row(s) will be skipped."));
                }
                self.confirm = Some(prompt);
                self.pending_confirm = Some(config);
            }
        }
    }

    /// Plan the two-repo DIFF on a worker thread. `plan_repo_diff` reads both
    /// repos' full indexes, so on the whole-disk repos this tool targets it
    /// would freeze the window for seconds if run inline; the result comes back
    /// over the channel and is applied in [`Self::apply_diff_preview`].
    fn run_preview_diff(&mut self, store: &Arc<Store>, source: &str) {
        let Some(target) = self.target.clone() else {
            return;
        };
        let store = Arc::clone(store);
        let source = source.to_string();
        let pairing = self.pairing;
        let tx = self.tx.clone();
        self.previewing = true;
        self.status = Some(format!("comparing '{source}' and '{target}'…"));
        std::thread::spawn(move || {
            let result = plan_repo_diff(&store, &source, &target, pairing)
                .map(|rows| DiffPreviewData {
                    rows,
                    source_header: Self::repo_header(&store, &source),
                    target_header: Self::repo_header(&store, &target),
                })
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::DiffPreview(result));
        });
    }

    /// Fold a finished DIFF comparison into the board.
    fn apply_diff_preview(&mut self, result: Result<DiffPreviewData, String>) {
        match result {
            Ok(data) => {
                let rows = data.rows;
                let differing = rows
                    .iter()
                    .filter(|r| r.relation != dedup_core::diff::DiffRelation::Equal)
                    .count();
                self.preview_source_header = data.source_header;
                self.preview_target_header = data.target_header;
                self.preview_total = differing;
                self.diff_rows = rows;
                self.status = Some(format!("{differing} difference(s)."));
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
    }

    /// Open the side-by-side comparison for a conflicting row: both versions
    /// of the same path, with everything needed to judge them.
    fn open_inspect(
        &mut self,
        store: &Arc<Store>,
        left_rel: Option<&str>,
        right_rel: Option<&str>,
    ) {
        let (Some(source), Some(target)) = (self.source.clone(), self.target.clone()) else {
            return;
        };
        let side = |repo: &str, rel: &str| -> Option<DiffSide> {
            let meta = store.get_repo(repo).ok()?;
            let entry = store.get_file_entry(repo, rel).ok().flatten()?;
            let abs_path = PathBuf::from(&meta.abs_path).join(rel);
            Some(DiffSide {
                repo: repo.to_string(),
                rel_path: rel.to_string(),
                facts: FileFacts::from_entry(&entry, abs_path),
                // The real session lock, not a hardcoded "protected" — the
                // viewer's DELETE/OVERWRITE honor the same registry as
                // everything else.
                read_only: self.locks.read_only(repo),
            })
        };
        let left = left_rel.and_then(|rel| side(&source, rel));
        let right = right_rel.and_then(|rel| side(&target, rel));
        match (left, right) {
            (Some(left), Some(right)) => {
                // A DIFF row offers exactly these two files, so the pool is the
                // pair itself and neither side renders a switcher — there is
                // nowhere else to go.
                let pool = vec![left.clone(), right.clone()];
                self.inspect = Some(DiffCompare::new_with_pool(left, Some(right), pool));
                self.error = None;
            }
            // An only-on-one-side row: open that file alone. There is no pair,
            // so the viewer is a single-file INSPECT (no SHOW B, no pair
            // actions).
            (Some(only), None) | (None, Some(only)) => {
                let mut lb = DiffCompare::new_with_pool(only.clone(), None, vec![only]);
                lb.set_title("INSPECT");
                self.inspect = Some(lb);
                self.error = None;
            }
            (None, None) => {
                self.error = Some("Could not read that row's file.".to_string());
            }
        }
    }

    /// Open one indexed file in the single-file INSPECT viewer (a preview row's
    /// existing side — the planned counterpart may not be on disk yet).
    fn open_single(&mut self, store: &Arc<Store>, repo: &str, rel: &str) {
        let side = (|| -> Option<DiffSide> {
            let meta = store.get_repo(repo).ok()?;
            let entry = store.get_file_entry(repo, rel).ok().flatten()?;
            let abs_path = PathBuf::from(&meta.abs_path).join(rel);
            Some(DiffSide {
                repo: repo.to_string(),
                rel_path: rel.to_string(),
                facts: FileFacts::from_entry(&entry, abs_path),
                read_only: self.locks.read_only(repo),
            })
        })();
        match side {
            Some(side) => {
                let mut lb = DiffCompare::new_with_pool(side.clone(), None, vec![side]);
                lb.set_title("INSPECT");
                self.inspect = Some(lb);
                self.error = None;
            }
            None => self.error = Some("Could not read that row's file.".to_string()),
        }
    }

    /// Execute one DIFF board row action on a worker thread (a single file can
    /// still be large), then re-plan the diff so the row reflects the result.
    /// Confirm a bulk action before it runs. It is destructive and touches many
    /// files at once, so the exact count is stated and declining does nothing.
    fn bulk_confirm_modal(&mut self, ctx: &egui::Context, store: &Arc<Store>) {
        let Some((op, plan)) = self.bulk_confirm.clone() else {
            return;
        };
        let mut decision: Option<bool> = None;
        let response = egui::Modal::new(egui::Id::new("diff-bulk-confirm")).show(ctx, |ui| {
            ui.set_width(400.0);
            ui.label(
                egui::RichText::new("APPLY TO EVERY LISTED ROW")
                    .color(theme::amber())
                    .size(16.0)
                    .strong(),
            );
            ui.add_space(8.0);
            ui.colored_label(theme::text(), op.describe(plan.len()));
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new("Rows you have hidden are not touched.")
                    .color(theme::tan())
                    .size(11.0),
            );
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui
                    .add(
                        egui::Button::new(
                            egui::RichText::new("APPLY").color(theme::ink_on(theme::red())),
                        )
                        .fill(theme::red()),
                    )
                    .clicked()
                {
                    decision = Some(true);
                }
                if ui
                    .add(
                        egui::Button::new(egui::RichText::new("CANCEL").color(theme::text()))
                            .fill(theme::panel()),
                    )
                    .clicked()
                {
                    decision = Some(false);
                }
            });
        });
        if let Some(go) = decision {
            self.bulk_confirm = None;
            if go {
                self.start_bulk(store, plan);
            }
        } else if response.should_close() {
            self.bulk_confirm = None;
        }
    }

    /// Run a whole bulk plan on a worker thread, then re-plan the diff.
    ///
    /// Every operation is attempted — one failure does not abandon the rest —
    /// and the summary reports both counts, so a partial failure is visible
    /// rather than silently swallowed.
    fn start_bulk(&mut self, store: &Arc<Store>, plan: Vec<crate::diff_board::BoardAction>) {
        use crate::diff_board::BoardAction;
        let (Some(source), Some(target)) = (self.source.clone(), self.target.clone()) else {
            return;
        };
        let store = Arc::clone(store);
        let tx = self.tx.clone();
        let cancel = self.cancel.clone();
        self.running = true;
        self.pending_refresh = true;
        self.reset_run();
        self.status = Some(format!("applying {} operation(s)…", plan.len()));

        std::thread::spawn(move || {
            let side = |on_left: bool| {
                if on_left {
                    (source.clone(), target.clone())
                } else {
                    (target.clone(), source.clone())
                }
            };
            let (mut done, mut failed) = (0usize, 0usize);
            let mut cancelled = false;
            for action in plan {
                if cancel.is_cancelled() {
                    cancelled = true;
                    break;
                }
                let outcome = match action {
                    BoardAction::Copy {
                        from_left,
                        rel_path,
                    } => {
                        let (from, to) = side(from_left);
                        copy_file_between(&store, &from, &rel_path, &to, &rel_path)
                    }
                    BoardAction::Rename { on_left, from, to } => {
                        let (repo, _) = side(on_left);
                        rename_file(&store, &repo, &from, &to)
                    }
                    BoardAction::Delete { on_left, rel_path } => {
                        let (repo, _) = side(on_left);
                        delete_file(&store, &repo, &rel_path)
                    }
                    // Not produced by `bulk_plan`.
                    _ => Ok(()),
                };
                match outcome {
                    Ok(()) => done += 1,
                    Err(e) => {
                        log::warn!("bulk action failed: {e}");
                        failed += 1;
                    }
                }
            }
            let mut message = format!("Applied {done} operation(s)");
            if failed > 0 {
                message.push_str(&format!(", {failed} failed"));
            }
            if cancelled {
                message.push_str(" (cancelled)");
            }
            message.push('.');
            let _ = tx.send(Msg::Done(OpResult::Applied { message }));
        });
    }

    fn start_board_action(&mut self, store: &Arc<Store>, action: crate::diff_board::BoardAction) {
        use crate::diff_board::BoardAction;
        let (Some(source), Some(target)) = (self.source.clone(), self.target.clone()) else {
            return;
        };
        let store = Arc::clone(store);
        let tx = self.tx.clone();
        self.running = true;
        self.pending_refresh = true;
        self.reset_run();
        self.status = Some("applying…".to_string());

        std::thread::spawn(move || {
            // "left" is always the source repo, "right" the target.
            let side = |on_left: bool| {
                if on_left {
                    (source.clone(), target.clone())
                } else {
                    (target.clone(), source.clone())
                }
            };
            let outcome = match action {
                BoardAction::Copy {
                    from_left,
                    rel_path,
                } => {
                    let (from, to) = side(from_left);
                    copy_file_between(&store, &from, &rel_path, &to, &rel_path)
                        .map(|()| format!("Copied '{rel_path}' to '{to}'."))
                }
                BoardAction::Delete { on_left, rel_path } => {
                    let (repo, _) = side(on_left);
                    delete_file(&store, &repo, &rel_path)
                        .map(|()| format!("Deleted '{rel_path}' from '{repo}'."))
                }
                BoardAction::Rename { on_left, from, to } => {
                    let (repo, _) = side(on_left);
                    rename_file(&store, &repo, &from, &to)
                        .map(|()| format!("Renamed '{from}' to '{to}' in '{repo}'."))
                }
                BoardAction::Overwrite {
                    from_left,
                    from_rel,
                    to_rel,
                } => {
                    let (from, to) = side(from_left);
                    overwrite_file(&store, &from, &from_rel, &to, &to_rel)
                        .map(|()| format!("Overwrote '{to_rel}' in '{to}' with '{from}'s copy."))
                }
                BoardAction::DeleteMany { on_left, rel_paths } => {
                    let (repo, _) = side(on_left);
                    // Best effort as a batch: stop at the first failure so the
                    // message names the file that could not be removed.
                    let mut deleted = 0usize;
                    let mut failed = None;
                    for rel_path in &rel_paths {
                        match delete_file(&store, &repo, rel_path) {
                            Ok(()) => deleted += 1,
                            Err(e) => {
                                failed = Some(e);
                                break;
                            }
                        }
                    }
                    match failed {
                        Some(e) => Err(e),
                        None => Ok(format!("Deleted {deleted} file(s) from '{repo}'.")),
                    }
                }
                // Popups and the compare view are handled in the UI itself.
                BoardAction::OpenPopup { .. } | BoardAction::Inspect { .. } => Ok(String::new()),
            };
            let result = match outcome {
                Ok(message) => OpResult::Applied { message },
                Err(e) => OpResult::Error(e.to_string()),
            };
            let _ = tx.send(Msg::Done(result));
        });
    }

    /// Start `config`'s command on a worker thread. `only` restricts the run to
    /// a single review row (the APPLY button); `None` runs the whole batch minus
    /// any rejected rows. `config` is a snapshot taken when the run was asked
    /// for, so nothing the live controls do since can change what runs.
    fn start(&mut self, store: &Arc<Store>, config: RunConfig, only: Option<String>) {
        let RunConfig {
            source,
            command,
            dest,
            filter,
            move_files,
        } = config;
        let hidden: std::collections::HashSet<String> = self.preview_board.hidden.clone();
        let store = Arc::clone(store);
        let tx = self.tx.clone();
        self.cancel = CancellationToken::new();
        let cancel = self.cancel.clone();
        self.running = true;
        self.status = Some(format!("{}…", command.label().to_lowercase()));
        if only.is_some() {
            // A single-row APPLY keeps the preview on screen (it refreshes
            // when the run finishes) instead of dropping to the run log.
            self.pending_refresh = true;
            self.reset_run();
        } else {
            // RUN and REVIEW are mutually exclusive: starting a run drops the
            // stale preview and resets the live run log/counters.
            self.clear_preview();
            self.reset_run();
        }

        std::thread::spawn(move || {
            let progress = ChannelDiffProgress { tx: tx.clone() };
            let only_set: Option<std::collections::HashSet<String>> =
                only.map(|k| std::collections::HashSet::from([k]));
            let run = DiffRun::new(&progress, &cancel)
                .with_selection((!hidden.is_empty()).then_some(&hidden), only_set.as_ref());
            // Copy/Move (repo or folder) both yield CopyStats → Copied; Sync
            // yields SyncStats → Synced. Map each to its OpResult in place.
            let copied_result = |stats: Result<dedup_core::diff::CopyStats, String>| match stats {
                Ok(s) => OpResult::Copied {
                    copied: s.copied,
                    cancelled: s.cancelled,
                    moved: move_files,
                },
                Err(e) => OpResult::Error(e),
            };
            let result = match &dest {
                StartDest::Repo {
                    references,
                    target,
                    subdir,
                } => {
                    let ref_slice: Vec<&str> = references.iter().map(String::as_str).collect();
                    let stats = match store.get_repo(target) {
                        Ok(meta) => {
                            let target_dir = PathBuf::from(meta.abs_path);
                            let subdir = if subdir.is_empty() {
                                None
                            } else {
                                Some(subdir.as_str())
                            };
                            diff_copy(
                                &store,
                                &source,
                                &ref_slice,
                                CopyDest {
                                    dir: &target_dir,
                                    subdir,
                                },
                                move_files,
                                filter.as_deref(),
                                &run,
                            )
                            .map_err(|e| e.to_string())
                        }
                        Err(e) => Err(e.to_string()),
                    };
                    copied_result(stats)
                }
                StartDest::Folder {
                    references,
                    dir,
                    mode,
                    invert,
                } => {
                    let ref_slice: Vec<&str> = references.iter().map(String::as_str).collect();
                    let stats = export_to_folder(
                        &store,
                        &source,
                        &ref_slice,
                        dir,
                        *mode,
                        *invert,
                        move_files,
                        filter.as_deref(),
                        &run,
                    )
                    .map_err(|e| e.to_string());
                    copied_result(stats)
                }
                StartDest::Sync {
                    target,
                    delete,
                    mirror,
                } => match diff_sync(
                    &store,
                    &source,
                    target,
                    true,
                    *delete,
                    filter.as_deref(),
                    &run,
                ) {
                    Ok(s) => OpResult::Synced {
                        copied: s.copied,
                        deleted: s.deleted,
                        skipped: s.skipped,
                        errors: s.errors,
                        cancelled: s.cancelled,
                        mirror: *mirror,
                    },
                    Err(e) => OpResult::Error(e.to_string()),
                },
            };
            let _ = tx.send(Msg::Done(result));
        });
    }

    /// Clear the live run log and counters (used when a run starts or a
    /// preview replaces it).
    fn reset_run(&mut self) {
        self.run_log.clear();
        self.run_problems.clear();
        self.result.close();
        self.run_done = 0;
        self.run_total = 0;
        self.run_current.clear();
    }

    /// Fold one live progress event into the running counters, current line
    /// and last-N action log.
    fn apply_progress(&mut self, event: DiffEvent) {
        match event {
            DiffEvent::Progress {
                action,
                done,
                total,
                rel_path,
            } => {
                let verb = match action {
                    DiffAction::Copy => "Copied",
                    DiffAction::Move => "Moved",
                    DiffAction::Delete => "Deleted",
                };
                self.run_done = done;
                self.run_total = total;
                self.run_current = rel_path.clone();
                self.run_log.push_back(format!("{verb} {rel_path}"));
                while self.run_log.len() > RUN_LOG_LIMIT {
                    self.run_log.pop_front();
                }
            }
            DiffEvent::Error { path, message } => {
                // Session log gets every failure, so a large run's error list
                // survives even as the live log rolls; the report keeps a
                // capped copy for the UI.
                log::warn!("transfer error: {path}: {message}");
                if self.run_problems.len() < crate::run_result::MAX_PROBLEMS {
                    self.run_problems.push(format!("{path}: {message}"));
                }
                self.run_log.push_back(format!("✗ {path}: {message}"));
                while self.run_log.len() > RUN_LOG_LIMIT {
                    self.run_log.pop_front();
                }
            }
        }
    }

    fn drain(&mut self, ui: &egui::Ui) {
        let mut got = false;
        while let Ok(msg) = self.rx.try_recv() {
            got = true;
            match msg {
                Msg::Progress(event) => self.apply_progress(event),
                Msg::DiffPreview(result) => {
                    self.previewing = false;
                    self.apply_diff_preview(result);
                }
                Msg::ReviewPreview { result, confirm } => {
                    self.previewing = false;
                    self.apply_review_preview(result, confirm);
                }
                Msg::GroupPreview { result, confirm } => {
                    self.previewing = false;
                    self.apply_group_preview(result, confirm);
                }
                Msg::GroupBackPreview { result, confirm } => {
                    self.previewing = false;
                    self.apply_group_back_preview(result, confirm);
                }
                Msg::GroupDone(result) => {
                    self.running = false;
                    log::info!("group sync finished: {}", result.is_ok());
                    match result {
                        Ok(r) => {
                            let mut report = crate::run_result::RunReport::new(format!(
                                "Sync group '{}'",
                                r.main
                            ))
                            .count("copied", r.copied)
                            .count("deleted", r.deleted)
                            .cancelled(r.cancelled)
                            .problems(std::mem::take(&mut self.run_problems))
                            .problems(r.failures);
                            if !r.skipped.is_empty() {
                                report = report.note(format!(
                                    "{} sink(s) were never pushed and are now stale: {}",
                                    r.skipped.len(),
                                    r.skipped.join(", ")
                                ));
                            }
                            // `errors` counts failures the capped list may not
                            // hold all of; keep the true count visible.
                            if r.errors > report.problem_count() {
                                report = report.count("files that failed to copy", r.errors);
                            }
                            self.status = Some(report.headline());
                            self.error = None;
                            self.result.open(report);
                        }
                        Err(e) => self.error = Some(e),
                    }
                }
                Msg::Done(result) => {
                    self.running = false;
                    // The session log gets every finished run, so a bug report
                    // covering the Transfer tab has a trail.
                    log::info!("transfer finished: {result:?}");
                    match result {
                        OpResult::Copied {
                            copied,
                            cancelled,
                            moved,
                        } => {
                            let verb = if moved { "Move" } else { "Copy" };
                            let report = crate::run_result::RunReport::new(verb)
                                .count("copied", copied)
                                .cancelled(cancelled)
                                .problems(std::mem::take(&mut self.run_problems));
                            self.status = Some(report.headline());
                            self.error = None;
                            self.result.open(report);
                        }
                        OpResult::Synced {
                            copied,
                            deleted,
                            skipped,
                            errors,
                            cancelled,
                            mirror,
                        } => {
                            let title = if mirror { "Mirror" } else { "Sync" };
                            let mut report = crate::run_result::RunReport::new(title)
                                .count("copied", copied)
                                .count("deleted", deleted)
                                .count("skipped", skipped)
                                .cancelled(cancelled)
                                .problems(std::mem::take(&mut self.run_problems));
                            // `errors` counts failures the capped list may not
                            // hold all of; keep the true count visible.
                            if errors > report.problem_count() {
                                report = report.count("errors", errors);
                            }
                            self.status = Some(report.headline());
                            self.error = None;
                            self.result.open(report);
                        }
                        OpResult::Applied { message } => {
                            self.status = Some(message);
                            self.error = None;
                        }
                        OpResult::Error(e) => self.error = Some(e),
                    }
                }
            }
        }
        if got || self.running || self.previewing {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(100));
        }
    }
}

/// Kittest UI tests for the Transfer view. Mirrors the harness pattern
/// established in `dupes_view.rs`'s `ui_tests` module.
#[cfg(test)]
mod ui_tests {
    use dedup_core::diff::{DiffFile, DiffRelation};

    /// The session locks bar exactly the runs that would lose existing data:
    /// MIRROR needs the target unlocked, MOVE the source; COPY and the
    /// back-sync promote only add, so no lock ever bars them.
    #[test]
    fn lock_blocks_only_loss_operations() {
        let mut v = TransferView::new();
        v.source = Some("src".into());
        v.target = Some("tgt".into());

        v.command = Command::Mirror;
        assert!(v.lock_block().is_some(), "MIRROR into a locked target");
        v.locks.toggle("tgt");
        assert!(
            v.lock_block().is_none(),
            "unlocking the target frees MIRROR"
        );

        v.command = Command::Move;
        assert!(v.lock_block().is_some(), "MOVE from a locked source");
        v.locks.toggle("src");
        assert!(v.lock_block().is_none(), "unlocking the source frees MOVE");

        v.locks.toggle("src");
        v.locks.toggle("tgt"); // both locked again
        v.command = Command::Copy;
        assert!(v.lock_block().is_none(), "COPY only adds — never barred");
        v.command = Command::Sync;
        assert!(v.lock_block().is_none(), "plain SYNC only adds");
        v.sync_delete_missing = true;
        assert!(
            v.lock_block().is_some(),
            "SYNC with DELETE MISSING can delete in the locked target"
        );
        v.command = Command::GroupSyncBack;
        assert!(
            v.lock_block().is_none(),
            "the back-sync batch promote only adds to the main"
        );
    }

    /// Clicking an only-on-one-side row opens its single file — it used to
    /// open nothing at all (the absent side's `?` swallowed the click), which
    /// made a 2250-row one-sided diff completely uninspectable.
    #[test]
    fn a_one_sided_diff_row_opens_a_single_inspect() {
        use crate::diff_board::BoardAction;
        use board::Cmd;
        let rows = vec![
            drow(DiffRelation::OnlyLeft, vec![dfile("l.pdf", 1, 0)], vec![]),
            drow(DiffRelation::OnlyRight, vec![], vec![dfile("r.jpg", 2, 0)]),
        ];
        assert_eq!(
            diff_action(&rows, 0, Cmd::OpenRow),
            Some(BoardAction::Inspect {
                left_rel: Some("l.pdf".into()),
                right_rel: None,
            }),
            "an only-left row opens the left file alone"
        );
        assert_eq!(
            diff_action(&rows, 1, Cmd::OpenRow),
            Some(BoardAction::Inspect {
                left_rel: None,
                right_rel: Some("r.jpg".into()),
            }),
            "an only-right row opens the right file alone"
        );
    }

    /// The golden rule in DIFF: the living file's side stays plain
    /// (`OnlyHere`); the side that deleted this content wears `Resurrect` —
    /// it renders as the blue WAS DELETED tombstone cell, never as a veil
    /// over the surviving copy.
    #[test]
    fn tombstones_mark_the_side_that_deleted_not_the_survivor() {
        let mut row = drow(DiffRelation::OnlyLeft, vec![dfile("a.txt", 1, 0)], vec![]);
        row.deleted_in_right = true;
        let metas = diff_metas(&[row], false, false);
        assert_eq!(
            metas[0].left_status,
            board::Status::OnlyHere,
            "the living file is not the one with a story"
        );
        assert_eq!(
            metas[0].right_status,
            board::Status::Resurrect,
            "the side that deleted the content carries the state"
        );
        assert!(
            metas[0].is_resurrection(),
            "and the RESURRECTIONS ONLY filter still catches the row"
        );
    }

    /// The review board renders only the commands the locks allow: a locked
    /// side loses its deletes/overwrites, and unlocking restores them.
    #[test]
    fn diff_rows_withhold_commands_for_locked_sides() {
        use board::Cmd;
        let rows = vec![drow(
            DiffRelation::Conflict,
            vec![dfile("a", 1, 0)],
            vec![dfile("a", 2, 0)],
        )];
        // Left unlocked, right locked: right-side loss commands are withheld.
        let metas = diff_metas(&rows, false, true);
        assert!(metas[0].cmds.contains(&Cmd::DeleteLeft));
        assert!(metas[0].cmds.contains(&Cmd::OverwriteLeft));
        assert!(!metas[0].cmds.contains(&Cmd::DeleteRight));
        assert!(
            !metas[0].cmds.contains(&Cmd::OverwriteRight),
            "overwriting the locked right side would lose its existing file"
        );
        assert!(
            metas[0].cmds.contains(&Cmd::Compare) && metas[0].cmds.contains(&Cmd::Hide),
            "non-destructive commands stay"
        );
    }

    fn dfile(rel: &str, size: u64, ms: i64) -> DiffFile {
        DiffFile {
            rel_path: rel.to_string(),
            size,
            modified_ms: ms,
        }
    }

    fn drow(relation: DiffRelation, left: Vec<DiffFile>, right: Vec<DiffFile>) -> RepoDiffRow {
        RepoDiffRow {
            relation,
            left,
            right,
            deleted_in_right: false,
            deleted_in_left: false,
        }
    }

    /// A row's sort keys come from the first file on each side, so a side
    /// holding several names still sorts by one value.
    #[test]
    fn diff_metas_take_their_sort_keys_from_the_first_file() {
        let rows = vec![drow(
            DiffRelation::Renamed,
            vec![dfile("b.jpg", 500, 20), dfile("a.jpg", 900, 10)],
            vec![dfile("c.jpg", 700, 30)],
        )];
        let metas = diff_metas(&rows, false, false);
        assert_eq!(metas[0].left_size, 500, "the first left file's size");
        assert_eq!(metas[0].left_modified, 20);
        assert_eq!(metas[0].right_size, 700);
        assert_eq!(
            metas[0].left_paths,
            vec!["b.jpg".to_string(), "a.jpg".to_string()],
            "every name on the side is listed, so the row grows to fit them"
        );
    }

    /// Each relation lands in the right summary bucket. A diff plans nothing,
    /// so nothing is ever counted as "to delete".
    #[test]
    fn diff_totals_bucket_each_relation() {
        let rows = vec![
            drow(
                DiffRelation::Equal,
                vec![dfile("a", 1, 0)],
                vec![dfile("a", 1, 0)],
            ),
            drow(DiffRelation::OnlyLeft, vec![dfile("b", 1, 0)], vec![]),
            drow(DiffRelation::OnlyRight, vec![], vec![dfile("c", 1, 0)]),
            drow(
                DiffRelation::Conflict,
                vec![dfile("d", 1, 0)],
                vec![dfile("d", 2, 0)],
            ),
            drow(
                DiffRelation::Renamed,
                vec![dfile("e", 1, 0)],
                vec![dfile("f", 1, 0)],
            ),
        ];
        assert_eq!(diff_totals(&rows), [0, 2, 2, 1]);
    }

    /// What a row offers follows what its two sides say about each other.
    #[test]
    fn diff_rows_offer_the_commands_their_relation_allows() {
        use board::Cmd;
        let only_left = diff_metas(
            &[drow(DiffRelation::OnlyLeft, vec![dfile("a", 1, 0)], vec![])],
            false,
            false,
        );
        assert_eq!(
            only_left[0].cmds,
            vec![Cmd::CopyRight, Cmd::DeleteLeft, Cmd::Hide]
        );

        let conflict = diff_metas(
            &[drow(
                DiffRelation::Conflict,
                vec![dfile("a", 1, 0)],
                vec![dfile("a", 2, 0)],
            )],
            false,
            false,
        );
        assert!(conflict[0].cmds.contains(&Cmd::Compare));
        assert!(conflict[0].cmds.contains(&Cmd::OverwriteRight));

        // A side holding several names is narrowed down before it can be
        // renamed, so that side offers KEEP 1 / DEL ALL instead of RENAME.
        let multi = diff_metas(
            &[drow(
                DiffRelation::Renamed,
                vec![dfile("a", 1, 0), dfile("b", 1, 0)],
                vec![dfile("c", 1, 0)],
            )],
            false,
            false,
        );
        assert!(multi[0].cmds.contains(&Cmd::KeepOneLeft));
        assert!(!multi[0].cmds.contains(&Cmd::RenameLeft));
        assert!(
            multi[0].cmds.contains(&Cmd::RenameRight),
            "the 1:1 side can still be renamed"
        );

        // Equal rows are unchanged and offer nothing but HIDE.
        let equal = diff_metas(
            &[drow(
                DiffRelation::Equal,
                vec![dfile("a", 1, 0)],
                vec![dfile("a", 1, 0)],
            )],
            false,
            false,
        );
        assert!(equal[0].unchanged);
        assert_eq!(equal[0].cmds, vec![Cmd::Hide]);
    }

    /// A command becomes the file operation the caller executes — and a rename
    /// against several candidate names asks first instead of picking one.
    #[test]
    fn diff_commands_map_to_file_operations() {
        use crate::diff_board::{BoardAction, PopupKind};
        use board::Cmd;
        let rows = vec![
            drow(DiffRelation::OnlyLeft, vec![dfile("a", 1, 0)], vec![]),
            drow(
                DiffRelation::Renamed,
                vec![dfile("x", 1, 0)],
                vec![dfile("y", 1, 0), dfile("z", 1, 0)],
            ),
        ];
        assert_eq!(
            diff_action(&rows, 0, Cmd::CopyRight),
            Some(BoardAction::Copy {
                from_left: true,
                rel_path: "a".to_string()
            })
        );
        assert_eq!(
            diff_action(&rows, 0, Cmd::DeleteLeft),
            Some(BoardAction::Delete {
                on_left: true,
                rel_path: "a".to_string()
            })
        );
        assert_eq!(
            diff_action(&rows, 1, Cmd::RenameLeft),
            Some(BoardAction::OpenPopup {
                row: 1,
                on_left: true,
                kind: PopupKind::PickName
            }),
            "several names on the other side means asking which one"
        );
        assert_eq!(
            diff_action(&rows, 1, Cmd::RenameRight),
            Some(BoardAction::Rename {
                on_left: false,
                from: "y".to_string(),
                to: "x".to_string()
            }),
            "the 1:1 direction renames outright"
        );
        assert_eq!(
            diff_action(&rows, 0, Cmd::Hide),
            None,
            "HIDE is the board's own business"
        );
    }

    use super::*;
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;

    /// A temp store with a `source` and `target` repo, `source` holding a
    /// couple of files so the filter builder and preview have something real
    /// to show.
    fn sample_store() -> (tempfile::TempDir, Arc<Store>) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open_at(tmp.path().join("cfg")).unwrap();
        let src_dir = tmp.path().join("source");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(src_dir.join("holiday.jpg"), b"fake jpeg bytes").unwrap();
        std::fs::write(src_dir.join("notes.txt"), b"fake text bytes").unwrap();
        store
            .create_repo("source", &src_dir.to_string_lossy())
            .unwrap();
        let dst_dir = tmp.path().join("target");
        std::fs::create_dir_all(&dst_dir).unwrap();
        store
            .create_repo("target", &dst_dir.to_string_lossy())
            .unwrap();
        (tmp, Arc::new(store))
    }

    /// A sink is managed through its group's main, so it is not offered as a
    /// source or target here — only the main and ungrouped repos are.
    #[test]
    fn a_sink_is_not_offered_as_a_repo() {
        let (_tmp, store) = sample_store();
        store.create_sync_group("source", "source").expect("group");
        store
            .add_sync_sink("source", "target", dedup_core::store::SyncMode::AddOnly)
            .expect("sink");
        let mut view = TransferView::new();
        view.sync_repos(&store);
        assert!(
            view.repos.contains(&"source".to_string()),
            "the main is offered"
        );
        assert!(
            !view.repos.contains(&"target".to_string()),
            "the sink is hidden"
        );
    }

    /// Every sink's chip **and its MODE label** must stay inside the window.
    ///
    /// `chip_row` wraps by greedy-packing each chip against the width the
    /// closure's returned response reports. The MODE label is drawn after the
    /// chip in the same row, so if it is not part of that measured response its
    /// width is never budgeted and the row overruns the available width — a
    /// layout bug a `query_by_label("MODE: …")` assertion cannot see, which is
    /// why this asserts geometry instead.
    #[test]
    fn group_sync_sink_chips_and_modes_stay_inside_the_window() {
        let (tmp, store) = sample_store();
        store.create_sync_group("grp", "source").expect("group");
        // Long names, so the row is forced to wrap rather than fitting by luck.
        for name in [
            "offsite-archive-north",
            "offsite-archive-south",
            "nas-cold-storage-two",
            "usb-rotation-drive-c",
        ] {
            let dir = tmp.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            store.create_repo(name, &dir.to_string_lossy()).unwrap();
            store
                .add_sync_sink("grp", name, dedup_core::store::SyncMode::Mirror)
                .expect("sink");
        }
        let store2 = Arc::clone(&store);
        let width = 900.0;
        let mut view = TransferView::new();
        view.loaded = true;
        view.source = Some("source".to_string());
        view.sync_repos(&store2);
        view.command = Command::GroupSync;

        let store_ui = Arc::clone(&store);
        let mut init = false;
        let mut h = Harness::builder()
            .with_size(egui::vec2(width, 800.0))
            .build_ui_state(
                move |ui, view: &mut TransferView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    view.show(ui, &store_ui, TooltipVerbosity::default(), None);
                },
                view,
            );
        // Two frames: chip_row packs from sizes measured the previous frame.
        h.run();
        h.run();

        for label in ["MODE: MIRROR"] {
            for node in h.query_all_by_label(label) {
                let r = node.rect();
                assert!(
                    r.right() <= width,
                    "a sink's {label} runs off the window: right {:.1} > {width}",
                    r.right()
                );
            }
        }
        for name in ["offsite-archive-north", "usb-rotation-drive-c"] {
            let r = h.get_by_label(name).rect();
            assert!(
                r.right() <= width,
                "sink chip {name} runs off the window: right {:.1} > {width}",
                r.right()
            );
        }
    }

    /// GROUP SYNC is only offered when the source names a sync group's main.
    #[test]
    fn group_sync_only_offered_when_source_is_a_group_main() {
        let (_tmp, store) = sample_store();
        store.create_sync_group("grp", "source").expect("group");
        store
            .add_sync_sink("grp", "target", dedup_core::store::SyncMode::AddOnly)
            .expect("sink");
        let store2 = Arc::clone(&store);
        let h = transfer_harness(Arc::clone(&store), move |v| {
            v.sync_repos(&store2);
        });
        assert!(
            h.query_by_label("GROUP SYNC").is_some(),
            "GROUP SYNC is offered when the source is a group's main"
        );
    }

    /// Doc screenshot: a GROUP SYNC BACK review board with a green new-file row
    /// and a blue resurrection row, to `docs/screenshots/group_sync_back.png`.
    /// `--ignored` (needs wgpu).
    #[test]
    #[ignore = "generates a doc screenshot (needs wgpu)"]
    fn doc_screenshot_group_sync_back() {
        let (_tmp, store) = back_preview_store();
        let mut view = TransferView::new();
        view.loaded = true;
        view.repos = vec!["source".to_string(), "target".to_string()];
        view.sync_repos(&store);
        view.source = Some("source".to_string());
        view.refresh_group(&store);
        view.command = Command::GroupSyncBack;

        let store_ui = Arc::clone(&store);
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1120.0, 940.0))
            .wgpu()
            .build_ui_state(
                move |ui, view: &mut TransferView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::LIGHT);
                        init = true;
                    }
                    view.show(ui, &store_ui, TooltipVerbosity::default(), None);
                },
                view,
            );
        harness.run();
        harness.get_by_label("REVIEW").click_accesskit();
        settle_preview(&mut harness);
        harness.run();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/screenshots");
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("group_sync_back.png");
        let img = harness.render().expect("wgpu render failed");
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());
    }

    /// GROUP SYNC BACK is offered under the same condition as GROUP SYNC — the
    /// source is a group's main — as its reverse, and is a distinct command.
    #[test]
    fn group_sync_back_offered_when_source_is_a_group_main() {
        let (_tmp, store) = sample_store();
        store.create_sync_group("grp", "source").expect("group");
        store
            .add_sync_sink("grp", "target", dedup_core::store::SyncMode::AddOnly)
            .expect("sink");
        let store2 = Arc::clone(&store);
        let h = transfer_harness(Arc::clone(&store), move |v| {
            v.sync_repos(&store2);
        });
        assert!(
            h.query_by_label("GROUP SYNC BACK").is_some(),
            "GROUP SYNC BACK is offered when the source is a group's main"
        );
        assert!(
            h.query_by_label("GROUP SYNC").is_some(),
            "and GROUP SYNC (the forward push) is still offered alongside it"
        );
    }

    /// A store whose group's sink holds one file the main never saw (new) and
    /// one the main deleted but the sink still has (resurrection).
    fn back_preview_store() -> (tempfile::TempDir, Arc<Store>) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open_at(tmp.path().join("cfg")).unwrap();
        let src = tmp.path().join("source"); // main
        let dst = tmp.path().join("target"); // sink
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        // A file that stays on the main, so deleting the next one does not empty
        // its index (a scan that would empty a repo is refused).
        std::fs::write(src.join("stays.txt"), b"stays").unwrap();
        std::fs::write(src.join("deleted.txt"), b"deleted-content").unwrap();
        std::fs::write(dst.join("deleted.txt"), b"deleted-content").unwrap();
        std::fs::write(dst.join("added-on-sink.txt"), b"brand-new").unwrap();
        store.create_repo("source", &src.to_string_lossy()).unwrap();
        store.create_repo("target", &dst.to_string_lossy()).unwrap();
        let scan = |repo: &str| {
            dedup_core::update::update_repo(
                &store,
                repo,
                1,
                &dedup_core::update::NoProgress,
                &CancellationToken::new(),
            )
            .unwrap();
        };
        scan("source");
        scan("target");
        // The main deletes its copy and rescans → a tombstone the sink outlives.
        std::fs::remove_file(src.join("deleted.txt")).unwrap();
        scan("source");
        store.create_sync_group("grp", "source").unwrap();
        store
            .add_sync_sink("grp", "target", dedup_core::store::SyncMode::AddOnly)
            .unwrap();
        (tmp, Arc::new(store))
    }

    /// GROUP SYNC BACK's REVIEW classifies the sink against the main: a file the
    /// main never had counts as new (promote), a file the main deleted that the
    /// sink still holds is a resurrection candidate — and both reach the board.
    #[test]
    fn group_sync_back_preview_separates_new_and_resurrection() {
        let (_tmp, store) = back_preview_store();
        let store2 = Arc::clone(&store);
        let mut h = transfer_harness(Arc::clone(&store), move |v| {
            v.sync_repos(&store2);
            v.command = Command::GroupSyncBack;
        });
        h.get_by_label("REVIEW").click_accesskit();
        settle_preview(&mut h);
        assert_eq!(
            h.state().preview.len(),
            2,
            "two rows reach the board: one new, one resurrection"
        );
        assert_eq!(
            h.state().preview_totals[1],
            1,
            "one new file to promote (added-on-sink.txt)"
        );
        assert!(
            h.state()
                .status
                .as_deref()
                .unwrap_or_default()
                .contains("1 resurrection candidate"),
            "status names the resurrection count: {:?}",
            h.state().status
        );
        // The new row is green (OnlyHere on the main side); the resurrection row
        // is blue (Resurrect) — the classification carried into the board.
        let statuses: Vec<board::Status> =
            h.state().preview.iter().map(|r| r.left_status).collect();
        assert!(
            statuses.contains(&board::Status::OnlyHere),
            "a new (green) row: {statuses:?}"
        );
        assert!(
            statuses.contains(&board::Status::Resurrect),
            "a resurrection (blue) row: {statuses:?}"
        );
    }

    /// GROUP SYNC BACK's RUN promotes only the new files into the main; the
    /// resurrection candidate is never auto-promoted (it is opt-in, per row).
    #[test]
    fn group_sync_back_run_promotes_new_but_not_resurrection() {
        let (tmp, store) = back_preview_store();
        let store2 = Arc::clone(&store);
        let mut h = transfer_harness(Arc::clone(&store), move |v| {
            v.sync_repos(&store2);
            v.command = Command::GroupSyncBack;
        });
        h.get_by_label("RUN").click_accesskit();
        settle_preview(&mut h);
        assert!(
            h.state().confirm.is_some(),
            "RUN raises a confirmation once the plan lands"
        );
        assert!(
            h.state()
                .confirm
                .as_deref()
                .unwrap_or_default()
                .contains("resurrection"),
            "the confirmation names the resurrection candidates left behind: {:?}",
            h.state().confirm
        );
        h.get_by_label("PROCEED").click_accesskit();
        for _ in 0..100 {
            h.step();
            if !h.state().running {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(!h.state().running, "the pull finished");
        let main_dir = tmp.path().join("source");
        assert!(
            main_dir.join("added-on-sink.txt").exists(),
            "the new file was promoted into the main"
        );
        assert!(
            !main_dir.join("deleted.txt").exists(),
            "the resurrection candidate was NOT auto-promoted (opt-in only)"
        );
    }

    /// The back-sync row's other half: DELETE R purges the file from the sink
    /// (disk + index) and the preview rebuilds without it. And the button obeys
    /// the sink's lock: locked (the default) means no DELETE R at all.
    #[test]
    fn group_sync_back_delete_r_purges_the_sink_file() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open_at(tmp.path().join("cfg")).unwrap();
        let src = tmp.path().join("source");
        let dst = tmp.path().join("target");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(src.join("stays.txt"), b"stays").unwrap();
        std::fs::write(dst.join("stays.txt"), b"stays").unwrap();
        std::fs::write(dst.join("junk.txt"), b"junk-only-on-sink").unwrap();
        store.create_repo("source", &src.to_string_lossy()).unwrap();
        store.create_repo("target", &dst.to_string_lossy()).unwrap();
        let scan = |repo: &str| {
            dedup_core::update::update_repo(
                &store,
                repo,
                1,
                &dedup_core::update::NoProgress,
                &CancellationToken::new(),
            )
            .unwrap();
        };
        scan("source");
        scan("target");
        store.create_sync_group("grp", "source").unwrap();
        store
            .add_sync_sink("grp", "target", dedup_core::store::SyncMode::AddOnly)
            .unwrap();
        let store = Arc::new(store);
        let mut view = TransferView::new();
        view.loaded = true;
        view.repos = vec!["source".to_string(), "target".to_string()];
        view.source = Some("source".to_string());
        view.sync_repos(&store);
        view.command = Command::GroupSyncBack;
        let store_ui = Arc::clone(&store);
        let mut init = false;
        let mut h = Harness::builder()
            .with_size(egui::vec2(1120.0, 940.0))
            .build_ui_state(
                move |ui, view: &mut TransferView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    view.show(ui, &store_ui, TooltipVerbosity::default(), None);
                },
                view,
            );
        h.run();
        h.get_by_label("REVIEW").click_accesskit();
        settle_preview(&mut h);
        // The sink is locked (the default): no DELETE is offered, the
        // promote stays.
        assert!(
            h.query_by_label("DELETE").is_none(),
            "a locked sink offers no deletion"
        );
        assert!(
            h.query_by_label("< COPY").is_some(),
            "promoting (an addition to the main) is never barred"
        );
        // Unlock the sink: DELETE appears in the sink's half-column; clicking
        // it purges the file.
        h.state_mut().locks.toggle("target");
        h.run();
        h.get_by_label("DELETE").click_accesskit();
        settle_preview(&mut h);
        assert!(
            !tmp.path().join("target").join("junk.txt").exists(),
            "the sink file is deleted from disk"
        );
        assert!(
            store
                .get_file_entry("target", "junk.txt")
                .unwrap()
                .is_none(),
            "and its index entry is gone"
        );
        assert!(
            h.state().preview.is_empty(),
            "the rebuilt preview has nothing left to promote"
        );
    }

    /// A single resurrection row's per-row `< COPY` pulls just that file into
    /// the main — how a mistakenly-deleted file is recreated from the backup.
    #[test]
    fn group_sync_back_per_row_apply_resurrects_one_file() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open_at(tmp.path().join("cfg")).unwrap();
        let src = tmp.path().join("source");
        let dst = tmp.path().join("target");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(src.join("stays.txt"), b"stays").unwrap();
        std::fs::write(src.join("deleted.txt"), b"deleted-content").unwrap();
        std::fs::write(dst.join("deleted.txt"), b"deleted-content").unwrap();
        store.create_repo("source", &src.to_string_lossy()).unwrap();
        store.create_repo("target", &dst.to_string_lossy()).unwrap();
        let scan = |repo: &str| {
            dedup_core::update::update_repo(
                &store,
                repo,
                1,
                &dedup_core::update::NoProgress,
                &CancellationToken::new(),
            )
            .unwrap();
        };
        scan("source");
        scan("target");
        std::fs::remove_file(src.join("deleted.txt")).unwrap();
        scan("source");
        store.create_sync_group("grp", "source").unwrap();
        store
            .add_sync_sink("grp", "target", dedup_core::store::SyncMode::AddOnly)
            .unwrap();
        let store = Arc::new(store);
        // A taller harness than the shared helper, so the single row's APPLY
        // button is on-screen (the board virtualizes off-screen rows away).
        let mut view = TransferView::new();
        view.loaded = true;
        view.repos = vec!["source".to_string(), "target".to_string()];
        view.source = Some("source".to_string());
        view.sync_repos(&store);
        view.command = Command::GroupSyncBack;
        let store_ui = Arc::clone(&store);
        let mut init = false;
        let mut h = Harness::builder()
            .with_size(egui::vec2(1120.0, 940.0))
            .build_ui_state(
                move |ui, view: &mut TransferView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    view.show(ui, &store_ui, TooltipVerbosity::default(), None);
                },
                view,
            );
        h.run();
        h.get_by_label("REVIEW").click_accesskit();
        settle_preview(&mut h);
        // One row (the resurrection); its `< COPY` pulls just that file.
        h.get_by_label("< COPY").click_accesskit();
        for _ in 0..100 {
            h.step();
            if !h.state().running {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(!h.state().running, "the per-row pull finished");
        assert!(
            src.join("deleted.txt").exists(),
            "the resurrection was recreated in the main by its per-row APPLY"
        );
    }

    #[test]
    fn group_sync_back_hidden_when_the_source_has_no_group() {
        let (_tmp, store) = sample_store();
        let store2 = Arc::clone(&store);
        let h = transfer_harness(Arc::clone(&store), move |v| {
            v.sync_repos(&store2);
        });
        assert!(
            h.query_by_label("GROUP SYNC BACK").is_none(),
            "GROUP SYNC BACK is hidden when the source has no group"
        );
    }

    #[test]
    fn group_sync_hidden_when_the_source_has_no_group() {
        let (_tmp, store) = sample_store();
        let store2 = Arc::clone(&store);
        let h = transfer_harness(Arc::clone(&store), move |v| {
            v.sync_repos(&store2);
        });
        assert!(
            h.query_by_label("GROUP SYNC").is_none(),
            "GROUP SYNC is hidden when the source has no group"
        );
    }

    /// Entering GROUP SYNC hides the single TARGET picker (it has several
    /// targets, one per sink) and shows a SINKS panel instead, defaulting to
    /// every sink selected.
    #[test]
    fn group_sync_hides_target_and_shows_sinks() {
        let (_tmp, store) = sample_store();
        store.create_sync_group("grp", "source").expect("group");
        store
            .add_sync_sink("grp", "target", dedup_core::store::SyncMode::Mirror)
            .expect("sink");
        let store2 = Arc::clone(&store);
        let h = transfer_harness(Arc::clone(&store), move |v| {
            v.sync_repos(&store2);
            v.command = Command::GroupSync;
        });
        assert!(
            h.query_by_label("TARGET").is_none(),
            "the single TARGET picker is hidden in GROUP SYNC"
        );
        assert!(
            h.query_by_label("SINKS").is_some(),
            "the SINKS panel is shown"
        );
        // The SOURCE chip badges the group's main. This is the only test of the
        // whole chain — `Store::main_repo_names` -> `TransferView::mains` ->
        // chip badge — the rest is covered widget-side in `repo_chip`.
        // Exact label: the section header "SINKS — WHERE THE MAIN IS PUSHED"
        // makes a `contains` query ambiguous.
        assert!(
            h.query_by_label("MAIN").is_some(),
            "the source chip badges the group's main"
        );
        // The chip carries the bare repo name — the identicon is hashed from it,
        // so decorating the name gave this sink a different glyph here than on
        // every other tab. The mode rides alongside as its own label.
        assert!(
            h.query_by_label("target").is_some(),
            "the sink chip names the sink, undecorated"
        );
        assert!(
            h.query_by_label("MODE: MIRROR").is_some(),
            "the sink's stored mode is shown next to its chip"
        );
        assert_eq!(
            h.state().selected_sinks,
            vec!["target".to_string()],
            "every sink is selected by default"
        );
    }

    /// GROUP SYNC BACK also hides the single TARGET picker. Like GROUP SYNC it
    /// picks its other repo in the SINK panel (the sink to pull back), and the
    /// target is implicitly the group's main — so the picker is unused and used
    /// to just linger.
    #[test]
    fn group_sync_back_hides_target() {
        let (_tmp, store) = sample_store();
        store.create_sync_group("grp", "source").expect("group");
        store
            .add_sync_sink("grp", "target", dedup_core::store::SyncMode::Mirror)
            .expect("sink");
        let store2 = Arc::clone(&store);
        let h = transfer_harness(Arc::clone(&store), move |v| {
            v.sync_repos(&store2);
            v.command = Command::GroupSyncBack;
        });
        assert!(
            h.query_by_label("TARGET").is_none(),
            "the single TARGET picker is hidden in GROUP SYNC BACK too"
        );
    }

    /// REVIEW plans every selected sink and folds the result into the shared
    /// review board, exactly like every other command's preview.
    #[test]
    fn group_sync_review_shows_planned_copies() {
        let (_tmp, store) = sample_store();
        store.create_sync_group("grp", "source").expect("group");
        store
            .add_sync_sink("grp", "target", dedup_core::store::SyncMode::AddOnly)
            .expect("sink");
        for repo in ["source", "target"] {
            dedup_core::update::update_repo(
                &store,
                repo,
                1,
                &dedup_core::update::NoProgress,
                &CancellationToken::new(),
            )
            .expect("scan");
        }
        let store2 = Arc::clone(&store);
        let mut h = transfer_harness(Arc::clone(&store), move |v| {
            v.sync_repos(&store2);
            v.command = Command::GroupSync;
        });
        h.get_by_label("REVIEW").click_accesskit();
        settle_preview(&mut h);
        assert_eq!(
            h.state().preview_totals[1],
            2,
            "both source files are new to the empty sink"
        );
        assert!(
            h.state()
                .status
                .as_deref()
                .unwrap_or_default()
                .contains("1 sink"),
            "status names the sink count: {:?}",
            h.state().status
        );
    }

    /// RUN plans, confirms (naming the sink count), and on PROCEED actually
    /// pushes the main's content into the sink on a background thread.
    #[test]
    fn group_sync_run_pushes_to_the_sink() {
        let (tmp, store) = sample_store();
        store.create_sync_group("grp", "source").expect("group");
        store
            .add_sync_sink("grp", "target", dedup_core::store::SyncMode::AddOnly)
            .expect("sink");
        for repo in ["source", "target"] {
            dedup_core::update::update_repo(
                &store,
                repo,
                1,
                &dedup_core::update::NoProgress,
                &CancellationToken::new(),
            )
            .expect("scan");
        }
        let store2 = Arc::clone(&store);
        let mut h = transfer_harness(Arc::clone(&store), move |v| {
            v.sync_repos(&store2);
            v.command = Command::GroupSync;
        });
        h.get_by_label("RUN").click_accesskit();
        settle_preview(&mut h);
        assert!(
            h.state().confirm.is_some(),
            "RUN raises a confirmation once the plan lands"
        );
        assert!(
            h.state()
                .confirm
                .as_deref()
                .unwrap_or_default()
                .contains("1 sink"),
            "the confirmation names the sink count: {:?}",
            h.state().confirm
        );
        h.get_by_label("PROCEED").click_accesskit();
        // The running spinner keeps requesting repaints, so `run` (step-capped)
        // would overflow — step manually until the push finishes.
        for _ in 0..100 {
            h.step();
            if !h.state().running {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(!h.state().running, "the push finished");
        assert!(
            tmp.path().join("target").join("holiday.jpg").exists(),
            "the file actually landed in the sink"
        );
    }

    /// The empty-main-mirror refusal (`guard_mirror_source`) survives bypassing
    /// `plan_group_sync`/`run_group_sync` to call `plan_sync`/`diff_sync`
    /// directly (done so the filter can be threaded through).
    #[test]
    fn group_sync_refuses_to_mirror_from_an_empty_main() {
        let (_tmp, store) = sample_store();
        store.create_sync_group("grp", "source").expect("group");
        store
            .add_sync_sink("grp", "target", dedup_core::store::SyncMode::Mirror)
            .expect("sink");
        // The sink is scanned; the main ("source") never is — an empty main.
        dedup_core::update::update_repo(
            &store,
            "target",
            1,
            &dedup_core::update::NoProgress,
            &CancellationToken::new(),
        )
        .expect("scan");
        let store2 = Arc::clone(&store);
        let mut h = transfer_harness(Arc::clone(&store), move |v| {
            v.sync_repos(&store2);
            v.command = Command::GroupSync;
            // A locked MIRROR sink is excluded from the push outright — unlock
            // it so the push is attempted and the empty-main refusal can fire.
            v.locks.toggle("target");
        });
        h.get_by_label("REVIEW").click_accesskit();
        settle_preview(&mut h);
        assert!(
            h.state().error.is_some(),
            "an empty MIRROR main is refused, not silently pushed"
        );
        assert!(h.state().preview.is_empty(), "nothing is planned");
    }

    /// Deselecting a sink narrows the plan to the sinks still selected — the
    /// SINKS panel actually filters what GROUP SYNC pushes to.
    #[test]
    fn group_sync_deselecting_a_sink_narrows_the_plan() {
        let (_tmp, store) = sample_store();
        store.create_sync_group("grp", "source").expect("group");
        store
            .add_sync_sink("grp", "target", dedup_core::store::SyncMode::AddOnly)
            .expect("sink");
        let other_dir = _tmp.path().join("other_sink");
        std::fs::create_dir_all(&other_dir).unwrap();
        store
            .create_repo("other_sink", &other_dir.to_string_lossy())
            .unwrap();
        store
            .add_sync_sink("grp", "other_sink", dedup_core::store::SyncMode::AddOnly)
            .expect("sink");
        for repo in ["source", "target", "other_sink"] {
            dedup_core::update::update_repo(
                &store,
                repo,
                1,
                &dedup_core::update::NoProgress,
                &CancellationToken::new(),
            )
            .expect("scan");
        }
        let store2 = Arc::clone(&store);
        let mut h = transfer_harness(Arc::clone(&store), move |v| {
            v.sync_repos(&store2);
            v.command = Command::GroupSync;
            v.selected_sinks = vec!["target".to_string()];
        });
        h.get_by_label("REVIEW").click_accesskit();
        settle_preview(&mut h);
        assert!(
            h.state()
                .status
                .as_deref()
                .unwrap_or_default()
                .contains("1 sink"),
            "only the selected sink is planned: {:?}",
            h.state().status
        );
    }

    /// The FILTER wizard actually narrows a GROUP SYNC plan, not just the
    /// generic COPY/MOVE/SYNC/MIRROR path — it would be worse to show a
    /// working-looking filter panel that GROUP SYNC silently ignored.
    #[test]
    fn group_sync_filter_narrows_the_plan() {
        let (_tmp, store) = sample_store();
        store.create_sync_group("grp", "source").expect("group");
        store
            .add_sync_sink("grp", "target", dedup_core::store::SyncMode::AddOnly)
            .expect("sink");
        for repo in ["source", "target"] {
            dedup_core::update::update_repo(
                &store,
                repo,
                1,
                &dedup_core::update::NoProgress,
                &CancellationToken::new(),
            )
            .expect("scan");
        }
        let store2 = Arc::clone(&store);
        let mut h = transfer_harness(Arc::clone(&store), move |v| {
            v.sync_repos(&store2);
            v.command = Command::GroupSync;
        });
        h.state_mut().filter.set_expression("name:holiday");
        // The FILTER's live match-count keeps requesting repaints, so `run`
        // (step-capped) would overflow — step manually past the debounce.
        for _ in 0..5 {
            h.step();
        }
        h.get_by_label("REVIEW").click_accesskit();
        settle_preview(&mut h);
        assert_eq!(
            h.state().preview_totals[1],
            1,
            "only the filter-matching file is planned, not both source files"
        );
    }

    /// Build a headless harness showing the Transfer view over `store`, driven
    /// by the given `setup` (which runs once, before the first frame, to select
    /// repos / destination / etc.).
    fn transfer_harness(
        store: Arc<Store>,
        setup: impl FnOnce(&mut TransferView),
    ) -> Harness<'static, TransferView> {
        let mut view = TransferView::new();
        view.loaded = true;
        view.repos = vec!["source".to_string(), "target".to_string()];
        view.source = Some("source".to_string());
        setup(&mut view);

        let store_ui = Arc::clone(&store);
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1120.0, 620.0))
            .build_ui_state(
                move |ui, view: &mut TransferView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    view.show(ui, &store_ui, TooltipVerbosity::default(), None);
                },
                view,
            );
        harness.run();
        harness
    }

    /// The number keys switch the COPY/MOVE command from the keyboard.
    #[test]
    fn number_keys_select_command() {
        let (_tmp, store) = sample_store();
        let mut h = transfer_harness(store, |_| {});
        assert!(h.state().command == Command::Copy, "starts on COPY");

        h.key_press(egui::Key::Num2);
        h.run();
        h.run();
        assert!(h.state().command == Command::Move, "2 selects MOVE");

        h.key_press(egui::Key::Num3);
        h.run();
        h.run();
        assert!(h.state().command == Command::Sync, "3 selects SYNC");

        h.key_press(egui::Key::Num4);
        h.run();
        h.run();
        assert!(h.state().command == Command::Mirror, "4 selects MIRROR");

        h.key_press(egui::Key::Num1);
        h.run();
        h.run();
        assert!(h.state().command == Command::Copy, "1 selects COPY");
    }

    /// MIRROR hides the DELETE MISSING toggle (it always deletes) and the
    /// copy/move-only controls, keeps the TARGET row, and warns via DELETES
    /// EXTRAS.
    #[test]
    fn mirror_mode_shows_warning_and_hides_toggles() {
        let (_tmp, store) = sample_store();
        let harness = transfer_harness(store, |view| {
            view.command = Command::Mirror;
            view.target = Some("target".to_string());
        });

        assert!(
            harness.query_by_label_contains("DELETES EXTRAS").is_some(),
            "MIRROR shows the DELETES EXTRAS warning"
        );
        assert!(
            harness.query_by_label("DELETE MISSING").is_none(),
            "MIRROR has no DELETE MISSING toggle (it always deletes)"
        );
        assert!(
            harness.query_by_label("TARGET").is_some(),
            "MIRROR still picks a target repo"
        );
        assert!(
            harness.query_by_label_contains("DEST — ").is_none(),
            "the REPO/FOLDER toggle must be hidden in MIRROR mode"
        );
        assert!(
            harness.query_by_label("DUPEPOOL").is_none(),
            "MIRROR compares source vs the single target, so no DUPEPOOL row"
        );
    }

    /// In SYNC mode the DELETE MISSING toggle appears and the copy/move-only
    /// controls (DEST toggle, INTO subdir, DUPEPOOL) are hidden; the TARGET row
    /// stays (SYNC is repo→repo).
    #[test]
    fn sync_mode_shows_delete_toggle_and_hides_transfer_controls() {
        let (_tmp, store) = sample_store();
        let harness = transfer_harness(store, |view| {
            view.command = Command::Sync;
            view.target = Some("target".to_string());
        });

        assert!(
            harness.query_by_label("DELETE MISSING").is_some(),
            "SYNC shows the DELETE MISSING toggle"
        );
        assert!(
            harness.query_by_label("TARGET").is_some(),
            "SYNC still picks a target repo"
        );
        assert!(
            harness.query_by_label_contains("DEST — ").is_none(),
            "the REPO/FOLDER destination toggle must be hidden in SYNC mode"
        );
        assert!(
            harness.query_by_label_contains("INTO — ").is_none(),
            "the INTO subdir bar must be hidden in SYNC mode"
        );
        assert!(
            harness.query_by_label("DUPEPOOL").is_none(),
            "SYNC compares source vs the single target, so no DUPEPOOL row"
        );
    }

    /// In FOLDER mode the folder/mode/invert controls appear and the repo-only
    /// controls (TARGET row and INTO subdir bar) are hidden.
    #[test]
    fn folder_mode_shows_folder_controls_and_hides_repo_controls() {
        let (_tmp, store) = sample_store();
        let harness = transfer_harness(store, |view| {
            view.destination = Destination::Folder;
        });

        // Folder-export controls are present (FOLDER appears twice: the DEST
        // toggle button and the folder-path row label).
        assert!(
            harness.query_all_by_label("FOLDER").next().is_some(),
            "FOLDER destination/label should be shown"
        );
        assert!(
            harness.query_by_label_contains("MODE — ").is_some(),
            "MODE selector should be shown in folder mode"
        );
        assert!(
            harness.query_by_label("INVERT").is_some(),
            "INVERT toggle should be shown in folder mode"
        );
        // Repo-only controls are hidden.
        assert!(
            harness.query_by_label("TARGET").is_none(),
            "the TARGET row must be hidden in folder mode"
        );
        assert!(
            harness.query_by_label_contains("INTO — ").is_none(),
            "the INTO subdir bar must be hidden in folder mode"
        );
    }

    /// SIMILAR mode reveals the shared similarity threshold slider inline (the
    /// threshold is owned here, not borrowed from the Duplicates tab); EXACT
    /// mode does not show it.
    #[test]
    fn similar_mode_shows_the_threshold_slider() {
        let (_tmp, store) = sample_store();
        let similar = transfer_harness(Arc::clone(&store), |view| {
            view.destination = Destination::Folder;
            view.select_mode = SelectMode::Similar;
        });
        assert!(
            similar.query_by_label("similarity").is_some(),
            "SIMILAR mode shows the similarity threshold control"
        );

        let exact = transfer_harness(store, |view| {
            view.destination = Destination::Folder;
            view.select_mode = SelectMode::Exact;
        });
        assert!(
            exact.query_by_label("similarity").is_none(),
            "EXACT mode has no similarity threshold control"
        );
    }

    /// In REPO mode the TARGET row and INTO subdir bar are shown, and the
    /// folder-export controls are absent.
    #[test]
    fn repo_mode_shows_repo_controls_and_hides_folder_controls() {
        let (_tmp, store) = sample_store();
        let harness = transfer_harness(store, |view| {
            view.destination = Destination::Repo;
            view.target = Some("target".to_string());
        });

        assert!(
            harness.query_by_label("TARGET").is_some(),
            "the TARGET row should be shown in repo mode"
        );
        assert!(
            harness.query_by_label_contains("INTO — ").is_some(),
            "the INTO subdir bar should be shown in repo mode"
        );
        assert!(
            harness.query_by_label_contains("MODE — ").is_none(),
            "the MODE selector must be hidden in repo mode"
        );
    }

    /// Doc screenshot: the Transfer tab with a source/target picked
    /// and a MIME filter condition active, to
    /// `docs/screenshots/files_tab.png`. Run with `--ignored`.
    #[test]
    #[ignore = "generates a doc screenshot (needs wgpu)"]
    fn doc_screenshot_files_tab() {
        let (_tmp, store) = sample_store();
        let mut view = TransferView::new();
        view.loaded = true;
        view.repos = vec!["source".to_string(), "target".to_string()];
        view.source = Some("source".to_string());
        view.target = Some("target".to_string());

        let store_ui = Arc::clone(&store);
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1120.0, 620.0))
            .wgpu()
            .build_ui_state(
                move |ui, view: &mut TransferView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    view.show(ui, &store_ui, TooltipVerbosity::default(), None);
                },
                view,
            );
        harness.run();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/screenshots");
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("files_tab.png");
        let img = harness.render().expect("wgpu render failed");
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());
    }

    /// Doc screenshot: GROUP SYNC selected on a group's main, with the TARGET
    /// picker hidden and the SINKS multiselect (one ADD ONLY, one MIRROR sink)
    /// shown instead, to `docs/screenshots/transfer_group_sync.png`. First
    /// render of this layout — the Overview-screen work this session found a
    /// real layout bug that only showed up once actually rendered, not from
    /// label-query tests alone, so this is checked visually before shipping.
    /// Run with `--ignored`.
    #[test]
    #[ignore = "generates a doc screenshot (needs wgpu)"]
    fn doc_screenshot_transfer_group_sync() {
        let (_tmp, store) = sample_store();
        store.create_sync_group("grp", "source").expect("group");
        store
            .add_sync_sink("grp", "target", dedup_core::store::SyncMode::AddOnly)
            .expect("sink");
        let other_dir = _tmp.path().join("archive");
        std::fs::create_dir_all(&other_dir).unwrap();
        store
            .create_repo("archive", &other_dir.to_string_lossy())
            .unwrap();
        store
            .add_sync_sink("grp", "archive", dedup_core::store::SyncMode::Mirror)
            .expect("sink");

        let mut view = TransferView::new();
        view.sync_repos(&store);
        view.source = Some("source".to_string());
        view.refresh_group(&store);
        view.command = Command::GroupSync;

        let store_ui = Arc::clone(&store);
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1120.0, 620.0))
            .wgpu()
            .build_ui_state(
                move |ui, view: &mut TransferView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    view.show(ui, &store_ui, TooltipVerbosity::default(), None);
                },
                view,
            );
        harness.run();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/screenshots");
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("transfer_group_sync.png");
        let img = harness.render().expect("wgpu render failed");
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());
    }

    /// Render snapshot of the SYNC command (with DELETE MISSING on) to
    /// `docs/screenshots/transfer_sync.png`, to eyeball the sync bar and hidden
    /// transfer controls. Run with `--ignored`.
    #[test]
    #[ignore = "generates a render snapshot (needs wgpu)"]
    fn render_transfer_sync() {
        let (_tmp, store) = sample_store();
        let mut view = TransferView::new();
        view.loaded = true;
        view.repos = vec!["source".to_string(), "target".to_string()];
        view.source = Some("source".to_string());
        view.target = Some("target".to_string());
        view.command = Command::Sync;
        view.sync_delete_missing = true;

        let store_ui = Arc::clone(&store);
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1120.0, 620.0))
            .wgpu()
            .build_ui_state(
                move |ui, view: &mut TransferView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    view.show(ui, &store_ui, TooltipVerbosity::default(), None);
                },
                view,
            );
        harness.run();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/screenshots");
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("transfer_sync.png");
        let img = harness.render().expect("wgpu render failed");
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());
    }

    /// Render snapshot of the MIRROR command to
    /// `docs/screenshots/transfer_mirror.png`, to eyeball the red DELETES
    /// EXTRAS warning and hidden toggles. Run with `--ignored`.
    #[test]
    #[ignore = "generates a render snapshot (needs wgpu)"]
    fn render_transfer_mirror() {
        let (_tmp, store) = sample_store();
        let mut view = TransferView::new();
        view.loaded = true;
        view.repos = vec!["source".to_string(), "target".to_string()];
        view.source = Some("source".to_string());
        view.target = Some("target".to_string());
        view.command = Command::Mirror;

        let store_ui = Arc::clone(&store);
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1120.0, 620.0))
            .wgpu()
            .build_ui_state(
                move |ui, view: &mut TransferView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    view.show(ui, &store_ui, TooltipVerbosity::default(), None);
                },
                view,
            );
        harness.run();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/screenshots");
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("transfer_mirror.png");
        let img = harness.render().expect("wgpu render failed");
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());
    }

    /// A harness whose preview is seeded with a mix of kinds, so the review
    /// table renders without needing a real indexed diff. Uses a tall viewport
    /// so the table (below the command/repo/filter chrome) is on-screen and its
    /// headers are clickable.
    fn review_harness() -> Harness<'static, TransferView> {
        let (_tmp, store) = sample_store();
        // Keep the temp dir alive for the harness's lifetime.
        std::mem::forget(_tmp);
        let mut view = TransferView::new();
        view.loaded = true;
        view.repos = vec!["source".to_string(), "target".to_string()];
        view.source = Some("source".to_string());
        view.target = Some("target".to_string());
        view.preview_source_header = "/repos/source".to_string();
        view.preview_target_header = "/repos/target".to_string();
        let (metas, bodies): (Vec<_>, Vec<_>) = [
            // A copy: the source keeps it, the target gains it.
            board_row(
                SideSpec::at(board::Status::Same, "holiday.jpg", None),
                SideSpec::at(board::Status::OnlyHere, "holiday.jpg", None),
                false,
                planned_cmds(),
            ),
            // Unchanged on both sides (hidden until the toggle is on).
            board_row(
                SideSpec::at(board::Status::Same, "notes.txt", None),
                SideSpec::at(board::Status::Same, "notes.txt", None),
                true,
                planned_cmds(),
            ),
        ]
        .into_iter()
        .unzip();
        view.preview = metas;
        view.preview_bodies = bodies;
        view.preview_total = 2;
        view.preview_totals = [0, 1, 0, 1];

        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1120.0, 1100.0))
            .build_ui_state(
                move |ui, view: &mut TransferView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    view.show(ui, &store, TooltipVerbosity::default(), None);
                },
                view,
            );
        harness.run();
        harness
    }

    /// A harness in DIFF mode with a seeded board: one file only the source
    /// has, one renamed pair, one equal pair.
    fn diff_harness() -> Harness<'static, TransferView> {
        let (_tmp, store) = sample_store();
        std::mem::forget(_tmp);
        diff_harness_over(store)
    }

    /// The DIFF harness over an existing store (so a test can index the repos
    /// first and let the row actions really run).
    fn diff_harness_over(store: Arc<Store>) -> Harness<'static, TransferView> {
        let mut view = TransferView::new();
        view.loaded = true;
        view.repos = vec!["source".to_string(), "target".to_string()];
        view.source = Some("source".to_string());
        view.target = Some("target".to_string());
        view.command = Command::Diff;
        // These tests exercise the full command set; the session locks (which
        // withhold deletes/overwrites on locked repos) are covered separately.
        view.locks.toggle("source");
        view.locks.toggle("target");
        view.preview_source_header = "/repos/source".to_string();
        view.preview_target_header = "/repos/target".to_string();
        let file = |rel: &str, size: u64| dedup_core::diff::DiffFile {
            rel_path: rel.to_string(),
            size,
            modified_ms: 1_700_000_000_000,
        };
        view.diff_rows = vec![
            RepoDiffRow {
                relation: dedup_core::diff::DiffRelation::OnlyLeft,
                left: vec![file("holiday.jpg", 2048)],
                right: Vec::new(),
                deleted_in_right: false,
                deleted_in_left: false,
            },
            RepoDiffRow {
                relation: dedup_core::diff::DiffRelation::Renamed,
                left: vec![file("old-name.txt", 12)],
                right: vec![file("new-name.txt", 12)],
                deleted_in_right: false,
                deleted_in_left: false,
            },
            RepoDiffRow {
                relation: dedup_core::diff::DiffRelation::Equal,
                left: vec![file("notes.txt", 15)],
                right: vec![file("notes.txt", 15)],
                deleted_in_right: false,
                deleted_in_left: false,
            },
        ];
        view.preview_total = 2;

        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1120.0, 1100.0))
            .build_ui_state(
                move |ui, view: &mut TransferView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    view.show(ui, &store, TooltipVerbosity::default(), None);
                },
                view,
            );
        harness.run();
        harness
    }

    /// DIFF mode shows the pairing toggles and drops the controls that make no
    /// sense for a manual comparison: no filter wizard, no RUN.
    #[test]
    fn diff_mode_shows_pairing_and_hides_filter_and_run() {
        let h = diff_harness();
        assert!(
            h.query_by_label("BY HASH").is_some(),
            "pairing toggles show"
        );
        assert!(h.query_by_label("BY PATH").is_some());
        assert!(
            h.query_by_label("RUN").is_none(),
            "DIFF has no batch RUN — rows are applied one at a time"
        );
        assert!(
            h.query_by_label_contains("FILTER").is_none(),
            "DIFF compares the repos whole, so the filter wizard is hidden"
        );
        assert!(
            h.query_by_label("REVIEW").is_some(),
            "REVIEW still builds the comparison"
        );
    }

    /// The board offers the actions each row's relation allows, and hides the
    /// equal row until the toggle is on.
    #[test]
    fn diff_board_offers_row_actions_and_hides_equal_rows() {
        let mut h = diff_harness();
        assert!(
            h.query_by_label_contains("holiday.jpg").is_some(),
            "the file only the source has is listed"
        );
        assert_eq!(
            h.query_all_by_label("notes.txt").count(),
            0,
            "equal rows are hidden by default"
        );
        // A one-sided row offers a copy across and a delete here; the arrowed
        // copy never collides with the COPY command in the bar above.
        assert_eq!(
            h.get_all_by_label("COPY >").count(),
            1,
            "the row offers to copy the left-only file across"
        );
        assert!(
            h.query_by_label("DELETE").is_some(),
            "or delete it where it is"
        );
        assert_eq!(
            h.query_all_by_label("RENAME").count(),
            2,
            "a renamed pair can be resolved from either side, one RENAME per \
             half-column"
        );
        // Each side's facts line carries its size.
        assert!(
            h.query_by_label_contains("2.00 KB").is_some(),
            "the facts line shows the size"
        );

        h.get_by_label_contains("SHOW UNCHANGED").click();
        h.run();
        assert_eq!(
            h.get_all_by_label("notes.txt").count(),
            2,
            "the toggle reveals the equal row, listed on both sides"
        );
    }

    /// Clicking a row's COPY hands that exact file to the core primitive: the
    /// file lands in the other repo, and the board refreshes itself.
    #[test]
    fn clicking_copy_copies_that_file_into_the_other_repo() {
        let (tmp, store) = sample_store();
        // Index both repos so the copy has a real entry to work from.
        for repo in ["source", "target"] {
            dedup_core::update::update_repo(
                &store,
                repo,
                1,
                &dedup_core::update::NoProgress,
                &dedup_core::update::CancellationToken::new(),
            )
            .expect("scan test repo");
        }
        let target_dir = tmp.path().join("target");
        let mut h = diff_harness_over(Arc::clone(&store));
        // The row's command names its direction, so it is unambiguous against
        // the COPY command in the bar above.
        h.get_by_label("COPY >").click();
        // The action runs on a worker thread; pump frames until it lands.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            // step(), not run(): the running spinner repaints every frame.
            h.step();
            if target_dir.join("holiday.jpg").exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            target_dir.join("holiday.jpg").exists(),
            "the clicked row's file was copied into the target repo: {:?}",
            h.state().error
        );
    }

    /// A side holding the same content under several names is narrowed down
    /// first: KEEP 1 asks which copy survives, and the answer deletes the rest.
    #[test]
    fn keep_one_popup_deletes_the_copies_the_user_did_not_pick() {
        let (tmp, store) = sample_store();
        let src_dir = tmp.path().join("source");
        // Three identical copies on the source side, one on the target under
        // another name — the classic "narrow it down" row.
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(src_dir.join(name), b"same content").expect("write copy");
        }
        std::fs::write(tmp.path().join("target/z.txt"), b"same content").expect("write target");
        for repo in ["source", "target"] {
            dedup_core::update::update_repo(
                &store,
                repo,
                1,
                &dedup_core::update::NoProgress,
                &dedup_core::update::CancellationToken::new(),
            )
            .expect("scan test repo");
        }
        let mut h = diff_harness_over(Arc::clone(&store));
        {
            // Real rows this time, straight from the core planner.
            let view = h.state_mut();
            view.diff_rows = dedup_core::diff::plan_repo_diff(
                &store,
                "source",
                "target",
                dedup_core::diff::DiffPairing::ByHash,
            )
            .expect("plan diff");
        }
        h.run();
        h.get_by_label("KEEP 1").click();
        h.run();
        // The popup lists all three copies; keep b.txt.
        assert!(
            h.query_by_label_contains("KEEP ONE COPY").is_some(),
            "the popup asks which copy to keep"
        );
        // The path is both a table cell and a popup button — pick the button.
        h.get_by_role_and_label(egui::accesskit::Role::Button, "b.txt")
            .click();
        for _ in 0..200 {
            // step(), not run(): the running spinner repaints every frame.
            h.step();
            if !src_dir.join("a.txt").exists() && !src_dir.join("c.txt").exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(src_dir.join("b.txt").exists(), "the picked copy stays");
        assert!(!src_dir.join("a.txt").exists(), "the others are deleted");
        assert!(!src_dir.join("c.txt").exists(), "the others are deleted");
        assert!(
            h.state().board_state.popup.is_none(),
            "answering closes the popup"
        );
    }

    /// A BY PATH conflict can be inspected side by side, and the comparison's
    /// own buttons carry out the same row actions.
    #[test]
    fn compare_opens_both_versions_and_can_delete_one() {
        let (tmp, store) = sample_store();
        // Same name on both sides, different content: a conflict row.
        std::fs::write(tmp.path().join("target/notes.txt"), b"other version").expect("write");
        for repo in ["source", "target"] {
            dedup_core::update::update_repo(
                &store,
                repo,
                1,
                &dedup_core::update::NoProgress,
                &dedup_core::update::CancellationToken::new(),
            )
            .expect("scan test repo");
        }
        let mut h = diff_harness_over(Arc::clone(&store));
        {
            let view = h.state_mut();
            view.pairing = dedup_core::diff::DiffPairing::ByPath;
            view.diff_rows = dedup_core::diff::plan_repo_diff(
                &store,
                "source",
                "target",
                dedup_core::diff::DiffPairing::ByPath,
            )
            .expect("plan diff");
        }
        h.run();
        h.get_by_label("COMPARE").click();
        h.run();
        assert!(
            h.query_by_label_contains("SAME PATH, DIFFERENT CONTENT")
                .is_some(),
            "the comparison opened"
        );
        assert!(
            h.state().inspect.is_some(),
            "the view holds the open comparison"
        );
        // Delete the target's version from inside the comparison.
        let target_file = tmp.path().join("target/notes.txt");
        match h.get_all_by_label("DELETE").last() {
            Some(button) => button.click(),
            None => panic!("no DELETE button in the comparison"),
        }
        for _ in 0..200 {
            h.step();
            if !target_file.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            !target_file.exists(),
            "the comparison's DELETE removed that side's file: {:?}",
            h.state().error
        );
        assert!(h.state().inspect.is_none(), "acting closes the comparison");
    }

    /// Render snapshot of the side-by-side comparison to
    /// `target/transfer_compare.png`.
    #[test]
    #[ignore = "renders a PNG for manual inspection"]
    fn render_diff_compare() {
        let (tmp, store) = sample_store();
        std::fs::write(tmp.path().join("target/notes.txt"), b"a different version").expect("write");
        for repo in ["source", "target"] {
            dedup_core::update::update_repo(
                &store,
                repo,
                1,
                &dedup_core::update::NoProgress,
                &dedup_core::update::CancellationToken::new(),
            )
            .expect("scan test repo");
        }
        let mut h = diff_harness_over(Arc::clone(&store));
        {
            let view = h.state_mut();
            view.pairing = dedup_core::diff::DiffPairing::ByPath;
            view.diff_rows = dedup_core::diff::plan_repo_diff(
                &store,
                "source",
                "target",
                dedup_core::diff::DiffPairing::ByPath,
            )
            .expect("plan diff");
        }
        h.run();
        h.get_by_label("COMPARE").click();
        h.run();
        let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/transfer_compare.png");
        let img = h.render().expect("wgpu render failed");
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());
    }

    /// Doc screenshot of the DIFF side-by-side compare, with two genuinely
    /// different (decodable) images at the same path, to
    /// `docs/screenshots/diff_compare.png`. Run with `--ignored`.
    #[test]
    #[ignore = "generates a doc screenshot (needs wgpu)"]
    fn doc_screenshot_diff_compare() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        // The same relative path in both repos, different image content: a BY
        // PATH conflict the compare can really show side by side.
        for (repo, tint) in [("source", 40u8), ("target", 200u8)] {
            let root = tmp.path().join(repo);
            std::fs::create_dir_all(&root).unwrap();
            image::RgbImage::from_fn(640, 480, |x, y| image::Rgb([x as u8, y as u8, tint]))
                .save(root.join("holiday.png"))
                .unwrap();
            store.create_repo(repo, &root.to_string_lossy()).unwrap();
            dedup_core::update::update_repo(
                &store,
                repo,
                1,
                &dedup_core::update::NoProgress,
                &dedup_core::update::CancellationToken::new(),
            )
            .expect("scan test repo");
        }

        let mut view = TransferView::new();
        view.loaded = true;
        view.repos = vec!["source".to_string(), "target".to_string()];
        view.source = Some("source".to_string());
        view.target = Some("target".to_string());
        view.command = Command::Diff;
        view.pairing = dedup_core::diff::DiffPairing::ByPath;
        view.diff_rows = dedup_core::diff::plan_repo_diff(
            &store,
            "source",
            "target",
            dedup_core::diff::DiffPairing::ByPath,
        )
        .expect("plan diff");

        let store_ui = Arc::clone(&store);
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1120.0, 820.0))
            .wgpu()
            .build_ui_state(
                move |ui, view: &mut TransferView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    view.show(ui, &store_ui, TooltipVerbosity::default(), None);
                },
                view,
            );
        harness.run();
        harness.get_by_label("COMPARE").click();
        // Both previews decode on worker threads; run a few frames so the
        // side-by-side A/B compare is populated before the shot.
        for _ in 0..12 {
            harness.run();
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        harness.run();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/screenshots");
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("diff_compare.png");
        let img = harness.render().expect("wgpu render failed");
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());
    }

    /// Doc screenshot of the DIFF board — a one-sided row and a rename pair —
    /// to `docs/screenshots/transfer_diff_board.png`.
    #[test]
    #[ignore = "generates a doc screenshot (needs wgpu)"]
    fn doc_screenshot_diff_board() {
        // A fully live DIFF: real repos, really scanned, so every row carries
        // real facts — image thumbnails, text heads and byte views under the
        // status veils, including a genuine WAS DELETED tombstone row.
        let tmp = tempfile::tempdir().unwrap();
        dedup_core::thumbnail::set_cache_dir(tmp.path().join("thumbs"));
        let store = Store::open_at(tmp.path().join("cfg")).unwrap();
        let src = tmp.path().join("source");
        let dst = tmp.path().join("target");
        std::fs::create_dir_all(src.join("a/b")).unwrap();
        std::fs::create_dir_all(dst.join("a/b")).unwrap();
        let jpg = |hue: u8| -> Vec<u8> {
            // A real JPEG (RGB) — the decoder picks its format from the file
            // extension, so the bytes must match the name.
            let im = image::RgbImage::from_fn(64, 48, |x, y| {
                image::Rgb([hue.saturating_add(x as u8 * 2), 70 + y as u8 * 3, 170])
            });
            let mut buf = Vec::new();
            image::DynamicImage::ImageRgb8(im)
                .write_to(
                    &mut std::io::Cursor::new(&mut buf),
                    image::ImageFormat::Jpeg,
                )
                .unwrap();
            buf
        };
        // Renamed: identical photo under two names.
        std::fs::write(src.join("a/b/holiday_v2.jpg"), jpg(90)).unwrap();
        std::fs::write(dst.join("a/b/holiday.jpg"), jpg(90)).unwrap();
        // Only-left text, only-right opaque blob.
        std::fs::write(
            src.join("notes.txt"),
            "Inheritance triage

- scan the NAS
- keep originals",
        )
        .unwrap();
        let blob: Vec<u8> = (0..4096u32).map(|i| (i % 5 * 53) as u8).collect();
        std::fs::write(dst.join("exports.db"), &blob).unwrap();
        // Tombstone: the target once held this and deleted it.
        std::fs::write(src.join("was_deleted.txt"), b"resurrect-me?").unwrap();
        std::fs::write(dst.join("was_deleted.txt"), b"resurrect-me?").unwrap();
        store.create_repo("source", &src.to_string_lossy()).unwrap();
        store.create_repo("target", &dst.to_string_lossy()).unwrap();
        let scan = |repo: &str| {
            dedup_core::update::update_repo(
                &store,
                repo,
                1,
                &dedup_core::update::NoProgress,
                &CancellationToken::new(),
            )
            .unwrap();
        };
        scan("source");
        scan("target");
        std::fs::remove_file(dst.join("was_deleted.txt")).unwrap();
        scan("target");
        let store = Arc::new(store);
        let mut view = TransferView::new();
        view.loaded = true;
        view.repos = vec!["source".to_string(), "target".to_string()];
        view.source = Some("source".to_string());
        view.target = Some("target".to_string());
        view.command = Command::Diff;
        // Unlocked, so the full command vocabulary shows.
        view.locks.toggle("source");
        view.locks.toggle("target");
        let store_ui = Arc::clone(&store);
        let mut init = false;
        let mut h = Harness::builder()
            .with_size(egui::vec2(1120.0, 1250.0))
            .wgpu()
            .build_ui_state(
                move |ui, view: &mut TransferView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    view.show(ui, &store_ui, TooltipVerbosity::default(), None);
                },
                view,
            );
        h.run();
        h.get_by_label("REVIEW").click_accesskit();
        settle_preview(&mut h);
        // Extra frames so the row previews (thumbnails, text heads, byte
        // views) land before the render.
        for _ in 0..40 {
            h.step();
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/screenshots/transfer_diff_board.png");
        let img = h.render().expect("wgpu render failed");
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());
    }

    /// The review board summarises the changes, heads the status columns with the
    /// repo paths, hides unchanged rows until the toggle is on, and sorts on a
    /// header click.
    #[test]
    fn review_board_summarises_hides_unchanged_and_sorts() {
        let mut h = review_harness();
        assert!(
            h.query_by_label_contains("1 only on one side").is_some(),
            "summary shows the count of files only one side has"
        );
        assert!(
            h.query_by_label_contains("1 unchanged").is_some(),
            "summary shows the unchanged count"
        );
        // The source path column is headed by the repo's absolute path.
        assert!(
            h.query_by_label_contains("/repos/source").is_some(),
            "the source column is headed by its absolute path"
        );
        // Unchanged rows are hidden by default; the toggle reveals them. (The
        // path appears in both the source and target columns, so use query_all.)
        assert!(
            !h.state().preview_board.show_unchanged,
            "unchanged hidden by default"
        );
        assert!(
            h.query_all_by_label("notes.txt").next().is_none(),
            "the unchanged row is hidden until the toggle is on"
        );
        h.get_by_label_contains("SHOW UNCHANGED").click();
        h.run();
        h.run();
        assert!(h.state().preview_board.show_unchanged, "toggle turns it on");
        assert!(
            h.query_all_by_label("notes.txt").next().is_some(),
            "the unchanged row appears once shown"
        );

        // Sorting is the explicit bar now, not a header click.
        assert!(h.state().preview_board.sort_asc, "starts ascending");
        h.get_by_label("▲").click();
        h.run();
        assert!(
            !h.state().preview_board.sort_asc,
            "the direction toggle reverses the sort"
        );
        // Two-sided, so the board offers a side switch its one-sided
        // counterpart does not.
        assert!(
            h.query_by_label("RIGHT").is_some(),
            "a two-sided board can sort by either side"
        );
    }

    /// Renders the review table to a PNG for manual inspection. `--ignored`.
    #[test]
    #[ignore = "generates a doc screenshot (needs wgpu)"]
    fn doc_screenshot_transfer_review_board() {
        let mut h = review_harness();
        // Real files behind the rows, so every cell shows a live preview —
        // an image thumbnail under the green NEW veil, a byte view under the
        // red WILL DELETE veil, a text head on the unchanged row.
        let dir = tempfile::tempdir().unwrap();
        dedup_core::thumbnail::set_cache_dir(dir.path().join("thumbs"));
        let facts = |name: &str, mime: &str, bytes: &[u8]| -> FileFacts {
            let path = dir.path().join(name);
            std::fs::write(&path, bytes).unwrap();
            FileFacts {
                size: bytes.len() as u64,
                modified_ms: 1_700_000_000_000,
                missing: false,
                mime: Some(mime.to_string()),
                img_size: None,
                audio_ms: None,
                audio_seed: None,
                hash_hex: format!("doc-{name}"),
                abs_path: path,
                origin: None,
                exif: None,
            }
        };
        let mut png = Vec::new();
        {
            // Real JPEG bytes under the .jpg name — the decoder trusts the
            // extension.
            let im = image::RgbImage::from_fn(64, 48, |x, y| {
                image::Rgb([120 + (x as u8), 80 + (y as u8 * 2), 190])
            });
            image::DynamicImage::ImageRgb8(im)
                .write_to(
                    &mut std::io::Cursor::new(&mut png),
                    image::ImageFormat::Jpeg,
                )
                .unwrap();
        }
        let tmp_bytes: Vec<u8> = (0..4096u32).map(|i| (i % 7 * 37) as u8).collect();
        // Seed one of each status and reveal unchanged, so the PNG shows the full
        // side-by-side vocabulary (added / removed / unchanged / absent).
        {
            let v = h.state_mut();
            v.preview_board.show_unchanged = true;
            v.preview_totals = [1, 1, 0, 1];
            v.preview_total = 3;
            let holiday = facts("holiday.jpg", "image/png", &png);
            let notes = facts(
                "notes.txt",
                "text/plain",
                b"Inheritance triage

- scan the NAS
- keep originals",
            );
            let (metas, bodies): (Vec<_>, Vec<_>) = [
                // A planned copy: source plain, the incoming file green on
                // the receiving side (the golden rule).
                board_row(
                    SideSpec::at(board::Status::Same, "holiday.jpg", Some(holiday.clone())),
                    SideSpec::at(board::Status::OnlyHere, "holiday.jpg", Some(holiday))
                        .veiled(crate::media_cell::CellOverlay::New),
                    false,
                    planned_cmds(),
                ),
                // A sync deletion: the doomed target file wears its own fate.
                board_row(
                    SideSpec::absent(),
                    SideSpec::at(
                        board::Status::WillDelete,
                        "old.tmp",
                        Some(facts("old.tmp", "application/octet-stream", &tmp_bytes)),
                    )
                    .veiled(crate::media_cell::CellOverlay::WillDelete),
                    false,
                    planned_cmds(),
                ),
                board_row(
                    SideSpec::at(board::Status::Same, "notes.txt", Some(notes.clone())),
                    SideSpec::at(board::Status::Same, "notes.txt", Some(notes)),
                    true,
                    planned_cmds(),
                ),
            ]
            .into_iter()
            .unzip();
            v.preview = metas;
            v.preview_bodies = bodies;
        }
        for _ in 0..40 {
            h.step();
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/screenshots");
        std::fs::create_dir_all(&dir).expect("screenshot dir");
        let out = dir.join("transfer_review_board.png");
        let img = h.render().expect("wgpu render failed");
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());
    }

    /// Pump frames until the DIFF preview worker has delivered its result.
    /// REVIEW plans off the UI thread now, so the click's own frame returns
    /// before the rows arrive; the tiny test repos finish near-instantly.
    /// `step()` rather than `run()`: freshly arrived rows kick off thumbnail /
    /// text-head reads whose completion requests a repaint, which `run()`
    /// would treat as "never settles".
    fn settle_preview(h: &mut Harness<'static, TransferView>) {
        for _ in 0..100 {
            h.step();
            if !h.state().previewing {
                h.step();
                h.step();
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("diff preview did not settle");
    }

    /// REVIEW on a DIFF command plans on a worker thread and fills the board
    /// once the comparison lands — the UI thread is never blocked on the scan.
    #[test]
    fn diff_preview_runs_off_thread_and_fills_the_board() {
        let (_tmp, store) = sample_store();
        // Give the two repos a difference to find: source has holiday.jpg,
        // target does not; both are scanned.
        for repo in ["source", "target"] {
            dedup_core::update::update_repo(
                &store,
                repo,
                1,
                &dedup_core::update::NoProgress,
                &CancellationToken::new(),
            )
            .expect("scan");
        }
        let mut h = transfer_harness(Arc::clone(&store), |v| {
            v.target = Some("target".to_string());
            v.command = Command::Diff;
        });

        // Nothing on the board yet — the plan hasn't been asked for.
        assert!(h.state().diff_rows.is_empty(), "board starts empty");

        h.get_by_label("REVIEW").click();
        // The result arrives over the channel from a worker thread, never inline
        // on the UI thread; settle pumps frames until it lands.
        settle_preview(&mut h);

        assert!(!h.state().previewing, "preview finished");
        assert!(
            h.state()
                .diff_rows
                .iter()
                .any(|r| r.left.iter().any(|f| f.rel_path == "holiday.jpg")),
            "the comparison landed and filled the board via the worker channel"
        );
    }

    /// REVIEW on a Copy command plans off-thread and fills the review board
    /// once the plan lands — end to end through spawn → channel → drain, the
    /// path the review-board tests otherwise inject around.
    #[test]
    fn review_preview_runs_off_thread_and_fills_the_board() {
        let (_tmp, store) = sample_store();
        for repo in ["source", "target"] {
            dedup_core::update::update_repo(
                &store,
                repo,
                1,
                &dedup_core::update::NoProgress,
                &CancellationToken::new(),
            )
            .expect("scan");
        }
        // source has holiday.jpg + notes.txt, target is empty → both are "new".
        let mut h = transfer_harness(Arc::clone(&store), |v| {
            v.target = Some("target".to_string());
            v.command = Command::Copy;
        });
        assert!(h.state().preview.is_empty(), "board starts empty");

        // The button can be scrolled off the short test window; accesskit clicks
        // reach it regardless.
        h.get_by_label("REVIEW").click_accesskit();
        settle_preview(&mut h);

        assert!(!h.state().previewing, "preview finished");
        assert!(
            h.state()
                .preview
                .iter()
                .any(|r| r.left_paths.iter().any(|p| p == "holiday.jpg")),
            "the plan landed and filled the review board via the worker channel"
        );
    }

    /// The full batch flow: RUN plans off-thread, the confirmation appears with
    /// the real count, and PROCEED actually copies the files.
    /// A bulk action is offered only when the listed rows actually contain the
    /// relation it acts on, and it plans exactly the rows on screen.
    #[test]
    fn bulk_actions_are_offered_only_for_relations_the_rows_hold() {
        let (_tmp, store) = sample_store();
        let mut v = TransferView::new();
        v.source = Some("source".into());
        v.target = Some("target".into());
        v.diff_rows = vec![
            RepoDiffRow {
                relation: DiffRelation::OnlyLeft,
                left: vec![dfile("only_left.txt", 1, 0)],
                right: vec![],
                deleted_in_right: false,
                deleted_in_left: false,
            },
            RepoDiffRow {
                relation: DiffRelation::Equal,
                left: vec![dfile("same.txt", 1, 0)],
                right: vec![dfile("same.txt", 1, 0)],
                deleted_in_right: false,
                deleted_in_left: false,
            },
        ];
        let metas = diff_metas(&v.diff_rows, false, false);
        let listed = v.listed_diff_rows(&metas);
        let offered = v.offered_bulk_ops(&listed);

        assert!(
            offered.contains(&BulkOp::CopyMissingRight),
            "a left-only row offers copying it across"
        );
        assert!(
            !offered.contains(&BulkOp::CopyMissingLeft),
            "there is no right-only row, so the mirror action is not offered"
        );
        assert!(
            !offered.contains(&BulkOp::RenameAllLeft),
            "BY PATH never yields Renamed, so no bulk rename is offered"
        );
        let _ = store;
    }

    /// Hiding a row is how a bulk action is opted out of — the plan must skip it.
    #[test]
    fn a_hidden_row_is_left_out_of_a_bulk_plan() {
        let (_tmp, _store) = sample_store();
        let mut v = TransferView::new();
        v.source = Some("source".into());
        v.target = Some("target".into());
        v.diff_rows = vec![
            RepoDiffRow {
                relation: DiffRelation::OnlyLeft,
                left: vec![dfile("keep.txt", 1, 0)],
                right: vec![],
                deleted_in_right: false,
                deleted_in_left: false,
            },
            RepoDiffRow {
                relation: DiffRelation::OnlyLeft,
                left: vec![dfile("skip.txt", 1, 0)],
                right: vec![],
                deleted_in_right: false,
                deleted_in_left: false,
            },
        ];
        let metas = diff_metas(&v.diff_rows, false, false);
        v.preview_board.hidden.insert(metas[1].key.clone());

        let listed = v.listed_diff_rows(&metas);
        assert_eq!(listed, vec![0], "the hidden row is not listed");
        let plan = v.bulk_plan(BulkOp::CopyMissingRight, &listed);
        assert_eq!(plan.len(), 1, "and is not in the plan: {plan:?}");
    }

    /// End to end against real files: the bulk copy lands every listed file on
    /// disk in the target repo.
    #[test]
    fn a_bulk_copy_lands_every_listed_file_on_disk() {
        let (tmp, store) = sample_store();
        for repo in ["source", "target"] {
            dedup_core::update::update_repo(
                &store,
                repo,
                1,
                &dedup_core::update::NoProgress,
                &CancellationToken::new(),
            )
            .expect("scan");
        }
        let target_dir = tmp.path().join("target");
        assert!(!target_dir.join("holiday.jpg").exists());

        let mut h = transfer_harness(Arc::clone(&store), |v| {
            v.target = Some("target".to_string());
            v.command = Command::Diff;
            v.diff_rows = vec![
                RepoDiffRow {
                    relation: DiffRelation::OnlyLeft,
                    left: vec![dfile("holiday.jpg", 15, 0)],
                    right: vec![],
                    deleted_in_right: false,
                    deleted_in_left: false,
                },
                RepoDiffRow {
                    relation: DiffRelation::OnlyLeft,
                    left: vec![dfile("notes.txt", 15, 0)],
                    right: vec![],
                    deleted_in_right: false,
                    deleted_in_left: false,
                },
            ];
        });
        h.run();

        let metas = diff_metas(&h.state().diff_rows, false, false);
        let listed = h.state().listed_diff_rows(&metas);
        let plan = h.state().bulk_plan(BulkOp::CopyMissingRight, &listed);
        assert_eq!(plan.len(), 2, "both left-only files are planned");

        h.state_mut().start_bulk(&store, plan);
        // Wait for the worker rather than polling a fixed budget — the fixed
        // budget was a known flake in this file.
        for _ in 0..600 {
            h.run();
            if !h.state().running {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        assert!(
            target_dir.join("holiday.jpg").exists(),
            "the bulk copy landed the first file"
        );
        assert!(target_dir.join("notes.txt").exists(), "and the second");
    }

    #[test]
    fn run_asks_then_copies_on_proceed() {
        let (tmp, store) = sample_store();
        for repo in ["source", "target"] {
            dedup_core::update::update_repo(
                &store,
                repo,
                1,
                &dedup_core::update::NoProgress,
                &CancellationToken::new(),
            )
            .expect("scan");
        }
        let target_dir = tmp.path().join("target");
        let mut h = transfer_harness(Arc::clone(&store), |v| {
            v.target = Some("target".to_string());
            v.command = Command::Copy;
        });

        h.get_by_label("RUN").click_accesskit();
        settle_preview(&mut h); // the plan lands and raises the confirmation
        assert!(
            h.state().confirm.is_some(),
            "RUN raises a confirmation once the plan lands"
        );
        assert!(
            h.state()
                .confirm
                .as_deref()
                .unwrap_or_default()
                .contains("Copy 2 file(s)"),
            "the prompt carries the real count: {:?}",
            h.state().confirm
        );

        h.get_by_label("PROCEED").click_accesskit();
        // Wait for the worker to actually finish, not for a fixed number of
        // ticks: the copy runs on a background thread, and a wall-clock budget
        // sized for an idle machine fails intermittently under a loaded test
        // run even though the run itself is healthy.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            h.step();
            if !h.state().running && h.state().error.is_none() {
                break;
            }
            if target_dir.join("holiday.jpg").exists() && target_dir.join("notes.txt").exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            target_dir.join("holiday.jpg").exists() && target_dir.join("notes.txt").exists(),
            "PROCEED copied the planned files: err={:?} running={} status={:?}",
            h.state().error,
            h.state().running,
            h.state().status
        );
    }

    /// A batch RUN plans off-thread, so the command/target can change before the
    /// confirmation lands. The prompt and the run it authorises must describe the
    /// config that was *planned*, not whatever the live controls say now.
    #[test]
    fn confirm_runs_the_planned_config_not_the_current_controls() {
        let mut view = TransferView::new();
        let planned = RunConfig {
            source: "SRC".to_string(),
            command: Command::Copy,
            dest: StartDest::Repo {
                references: Vec::new(),
                target: "DEST_A".to_string(),
                subdir: String::new(),
            },
            filter: None,
            move_files: false,
        };
        let data = ReviewPreviewData {
            rows: Vec::new(),
            bodies: Vec::new(),
            preview_total: 5,
            sync_delete_total: 0,
            preview_totals: [0, 5, 0, 0],
            source_header: "SRC".to_string(),
            target_header: "DEST_A".to_string(),
            status: String::new(),
        };
        // The user has since flipped the live controls to a Move elsewhere.
        view.command = Command::Move;
        view.target = Some("DEST_B".to_string());
        view.apply_review_preview(Ok(data), Some(Box::new(planned)));

        let prompt = view.confirm.as_deref().unwrap_or_default();
        assert!(
            prompt.contains("Copy 5 file(s)") && prompt.contains("DEST_A"),
            "the confirmation describes the planned Copy into DEST_A, got: {prompt}"
        );
        let pending = view.pending_confirm.as_ref().expect("a run is pending");
        assert!(
            pending.command == Command::Copy,
            "PROCEED runs the planned command"
        );
        assert!(
            matches!(&pending.dest, StartDest::Repo { target, .. } if target == "DEST_A"),
            "and the planned target"
        );
    }
}
