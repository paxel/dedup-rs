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

use crate::filter_ui::FilterBuilder;
use crate::icon;
use crate::review;
use crate::settings::TooltipVerbosity;
use crate::theme;
use crate::util::ExplainExt;
use crossbeam_channel::{Receiver, Sender};
use dedup_core::diff::{
    CopyDest, DiffAction, DiffEvent, DiffItem, DiffPairing, DiffProgress, DiffRun, FolderMode,
    RepoDiffRow, SyncDelete, copy_file_between, delete_file, diff_copy, diff_print, diff_sync,
    export_to_folder, overwrite_file, plan_folder_export, plan_repo_diff, plan_sync, rename_file,
};
use dedup_core::store::Store;
use dedup_core::update::CancellationToken;
use egui::{Id, RichText};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::review::PREVIEW_CAP;

/// How many recent actions the running panel keeps in its scrolling log.
const RUN_LOG_LIMIT: usize = 10;

#[derive(PartialEq, Clone, Copy)]
enum Command {
    Copy,
    Move,
    Sync,
    Mirror,
    Diff,
}

impl Command {
    fn label(self) -> &'static str {
        match self {
            Command::Copy => "COPY",
            Command::Move => "MOVE",
            Command::Sync => "SYNC",
            Command::Mirror => "MIRROR",
            Command::Diff => "DIFF",
        }
    }
    /// Whether the command is inherently destructive to on-disk data by itself.
    /// SYNC is additive by default (it only *copies* into the target); its
    /// optional DELETE MISSING toggle makes a given run destructive — see
    /// [`TransferView::destructive_run`]. MIRROR always deletes.
    fn destructive(self) -> bool {
        matches!(self, Command::Move | Command::Mirror)
    }
    /// Whether the command runs repo→repo at the same relative path (SYNC /
    /// MIRROR), which hides the DEST / subdir / folder / dupe-pool controls.
    fn repo_to_repo(self) -> bool {
        matches!(self, Command::Sync | Command::Mirror | Command::Diff)
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
    loaded: bool,
    source: Option<String>,
    target: Option<String>,
    /// Extra reference repos beyond the target: a file counts as "new" only
    /// when neither the target nor any of these already has its content.
    extra_refs: Vec<String>,
    command: Command,
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
    preview: Vec<review::ReviewRow>,
    /// DIFF: how the two repos are paired up (by content or by path).
    pairing: DiffPairing,
    /// DIFF: the rows of the current comparison, empty until PREVIEW.
    diff_rows: Vec<RepoDiffRow>,
    /// Sort/paging state of the diff board.
    board_state: crate::diff_board::BoardState,
    /// The open side-by-side comparison of one conflicting row, if any.
    inspect: Option<crate::diff_inspect::Inspect>,
    /// Full per-kind counts (indexed by [`review::RowKind::idx`]) for the review
    /// summary; independent of the capped `preview` sample.
    preview_totals: [usize; 3],
    /// The two review-table column headers: the source and target absolute paths.
    preview_source_header: String,
    preview_target_header: String,
    preview_total: usize,
    /// Sort column + direction for the review table.
    review_state: review::ReviewState,
    status: Option<String>,
    error: Option<String>,
    confirm: Option<String>,
    running: bool,
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
}

impl TransferView {
    pub fn new() -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        Self {
            repos: Vec::new(),
            loaded: false,
            source: None,
            target: None,
            extra_refs: Vec::new(),
            command: Command::Copy,
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
            preview_totals: [0; 3],
            preview_source_header: String::new(),
            preview_target_header: String::new(),
            preview_total: 0,
            review_state: review::ReviewState::default(),
            status: None,
            error: None,
            confirm: None,
            running: false,
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
        }
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

        // Keyboard shortcuts — skipped while a modal is up, a run is active, or
        // a text field is focused.
        if self.confirm.is_none()
            && !result_open
            && !self.running
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
                        .color(theme::BLUE)
                        .size(18.0)
                        .strong(),
                );
                crate::util::shortcut_bar(
                    ui,
                    "1 copy · 2 move · 3 sync · 4 mirror · 5 diff · P preview · R run",
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
                    ui.colored_label(theme::RED, err);
                }
                if let Some(status) = &self.status {
                    ui.label(RichText::new(status).color(theme::TAN).size(13.0));
                }
                ui.separator();
                // RUN and PREVIEW are mutually exclusive: while a run is active
                // or has left a log, show the live run panel; otherwise show the
                // preview.
                if self.running || !self.run_log.is_empty() {
                    self.run_panel(ui);
                } else {
                    self.preview_panel(ui, &mut acts);
                }
            });

        if let Some(prompt) = self.confirm.clone() {
            self.confirm_modal(ui, &prompt, &mut acts);
        }
        // The comparison sits above everything, and its buttons feed the same
        // row actions the board offers.
        if let Some(inspect) = self.inspect.as_mut()
            && let Some(outcome) = inspect.view(&ui.ctx().clone(), verbosity)
        {
            use crate::diff_inspect::InspectOutcome;
            let (left_rel, right_rel) = (
                inspect.left.rel_path.clone(),
                inspect.right.rel_path.clone(),
            );
            self.inspect = None;
            match outcome {
                InspectOutcome::Close => {}
                InspectOutcome::Delete { on_left } => {
                    acts.push(Act::Board(crate::diff_board::BoardAction::Delete {
                        on_left,
                        rel_path: if on_left { left_rel } else { right_rel },
                    }));
                }
                InspectOutcome::Overwrite { from_left } => {
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
                self.repos = list.into_iter().map(|(n, _, _)| n).collect();
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
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    fn repo_rows(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        crate::lcars::section_lcars(ui, "REPOS — PICK SOURCE & TARGET", theme::LILAC, |ui| {
            // SOURCE: every repo, orange when picked.
            let src = self.repos.clone();
            crate::repo_chip::chip_row(ui, "xfer_source", "SOURCE", src.len(), |ui, i| {
                let name = &src[i];
                let sel = self.source.as_deref() == Some(name.as_str());
                let chip = crate::repo_chip::repo_chip(ui, name, sel, theme::ORANGE, None);
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
                chip.outer
            });

            // TARGET (only when copying/moving into a repo — a folder export has
            // no target): the repos that aren't the source, blue when picked.
            if self.destination == Destination::Repo {
                let tgt: Vec<String> = self
                    .repos
                    .iter()
                    .filter(|n| self.source.as_deref() != Some(n.as_str()))
                    .cloned()
                    .collect();
                crate::repo_chip::chip_row(ui, "xfer_target", "TARGET", tgt.len(), |ui, i| {
                    let name = &tgt[i];
                    let sel = self.target.as_deref() == Some(name.as_str());
                    let chip = crate::repo_chip::repo_chip(ui, name, sel, theme::BLUE, None);
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
                    ui.label(RichText::new("DUPEPOOL").color(theme::TEXT).size(12.0));
                    if crate::repo_chip::small_button(ui, "ALL", theme::LILAC)
                        .explain(
                            self.verbosity,
                            "Add every eligible repo to the pool",
                            "Treat content held by any other repo as already known.",
                        )
                        .clicked()
                    {
                        self.extra_refs = eligible.clone();
                    }
                    if crate::repo_chip::small_button(ui, "NONE", theme::LILAC)
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
                crate::repo_chip::chip_row(ui, "xfer_pool", "", eligible.len(), |ui, i| {
                    let name = &eligible[i];
                    let sel = self.extra_refs.iter().any(|r| r == name);
                    let chip = crate::repo_chip::repo_chip(ui, name, sel, theme::LILAC, None);
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

    /// Whether PREVIEW/RUN can act: a source is picked, the destination is
    /// resolved (a target repo, or a non-blank export folder), and nothing is
    /// already running.
    fn ready(&self) -> bool {
        if self.running || self.source.is_none() {
            return false;
        }
        match self.destination {
            Destination::Repo => self.target.is_some(),
            Destination::Folder => !self.folder.trim().is_empty(),
        }
    }

    fn command_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        crate::lcars::section_lcars(
            ui,
            "COMMAND — COPY, MOVE, SYNC, MIRROR OR DIFF",
            theme::ORANGE,
            |ui| {
                ui.horizontal(|ui| {
                    for cmd in [
                        Command::Copy,
                        Command::Move,
                        Command::Sync,
                        Command::Mirror,
                        Command::Diff,
                    ] {
                        let sel = self.command == cmd;
                        let accent = if cmd.destructive() {
                            theme::RED
                        } else {
                            theme::AMBER
                        };
                        let fill = if sel { accent } else { theme::PANEL };
                        // Unselected pills sit on the dark panel — black text would
                        // vanish there, so they carry their accent color instead.
                        let col = if sel { theme::BLACK } else { accent };
                        let (short, verbose) = cmd.tooltip();
                        if ui
                            .add(
                                egui::Button::new(RichText::new(cmd.label()).color(col)).fill(fill),
                            )
                            .explain(self.verbosity, short, verbose)
                            .clicked()
                        {
                            acts.push(Act::SetCommand(cmd));
                        }
                    }
                });
                self.hint(ui);
            },
        );
    }

    fn subdir_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        crate::lcars::section_lcars(
            ui,
            "INTO — SUBFOLDER INSIDE THE TARGET",
            theme::BLUE,
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
                                    .color(theme::BLACK),
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
                .color(theme::LILAC)
                .size(11.0),
            );
            },
        );
    }

    /// Selector for where COPY/MOVE lands: into a repo or into a picked folder.
    fn dest_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        crate::lcars::section_lcars(ui, "DEST — WHERE COPIED FILES LAND", theme::BLUE, |ui| {
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
                    let fill = if sel { theme::BLUE } else { theme::PANEL };
                    let col = if sel { theme::BLACK } else { theme::BLUE };
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
            theme::BLUE,
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
                                .color(theme::BLACK),
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
            theme::LILAC,
            |ui| {
                ui.horizontal(|ui| {
                    for mode in [SelectMode::Exact, SelectMode::Similar] {
                        let sel = self.select_mode == mode;
                        let fill = if sel { theme::LILAC } else { theme::PANEL };
                        let col = if sel { theme::BLACK } else { theme::LILAC };
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
                        theme::ORANGE
                    } else {
                        theme::PANEL
                    };
                    let col = if self.invert {
                        theme::BLACK
                    } else {
                        theme::ORANGE
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
                ui.label(RichText::new(hint).color(theme::LILAC).size(11.0));
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
            Command::Diff => {
                "Compare the two repos side by side and resolve each difference yourself — \
                 copy, delete, rename or overwrite, one row at a time."
            }
        };
        ui.label(RichText::new(text).color(theme::LILAC).size(11.0));
    }

    /// MIRROR's info bar: no toggle (it always deletes), just a red warning that
    /// it removes everything in the target the source lacks.
    fn mirror_bar(&self, ui: &mut egui::Ui) {
        crate::lcars::section_lcars(
            ui,
            &format!("{} DELETES EXTRAS", icon::TRASH),
            theme::RED,
            |ui| {
                ui.label(
                    RichText::new(
                        "Everything in the target whose content the source does not have is \
                     deleted, so the target ends up holding exactly the source's content. \
                     Deletions cannot be undone.",
                    )
                    .color(theme::LILAC)
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
        crate::lcars::section_lcars(ui, "PAIR BY — HOW FILES ARE MATCHED", theme::BLUE, |ui| {
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
                    if crate::lcars::toggle_button(ui, label, selected, theme::BLUE)
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
            ui.label(RichText::new(hint).color(theme::LILAC).size(11.0));
        });
    }

    fn sync_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        crate::lcars::section_lcars(ui, "OPTIONS — SYNC BEHAVIOUR", theme::BLUE, |ui| {
            ui.horizontal(|ui| {
                let fill = if self.sync_delete_missing {
                    theme::RED
                } else {
                    theme::PANEL
                };
                let col = if self.sync_delete_missing {
                    theme::BLACK
                } else {
                    theme::RED
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
            ui.label(RichText::new(hint).color(theme::LILAC).size(11.0));
        });
    }

    /// Whether the *current* run would delete on-disk data: MOVE and MIRROR
    /// always do, and SYNC does only when DELETE MISSING is on. Drives the red
    /// accent on the confirm dialog.
    fn destructive_run(&self) -> bool {
        self.command.destructive() || (self.command == Command::Sync && self.sync_delete_missing)
    }

    fn action_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        crate::lcars::section_lcars(ui, "ACTION — PREVIEW & RUN", theme::AMBER, |ui| {
            ui.horizontal(|ui| {
                let ready = self.ready();
                if ui
                    .add_enabled(
                        ready,
                        egui::Button::new(RichText::new("PREVIEW").color(theme::BLACK)),
                    )
                    .explain(
                        self.verbosity,
                        "Preview the first transfers",
                        "Show the first matching `from → to` transfers (up to a preview \
                         limit) and a total count, without changing anything on disk. \
                         PREVIEW and RUN are mutually exclusive — starting a run clears the \
                         preview.",
                    )
                    .clicked()
                {
                    acts.push(Act::Preview);
                }
                if self.command.is_diff() {
                    if self.running {
                        ui.add(egui::Spinner::new().color(theme::AMBER));
                    }
                    return;
                }
                let run =
                    egui::Button::new(RichText::new("RUN").color(theme::BLACK)).fill(theme::AMBER);
                if ui
                    .add_enabled(ready, run)
                    .explain(
                        self.verbosity,
                        "Run the command",
                        "Run the selected command (COPY/MOVE) on a background thread, \
                         after a confirmation dialog. Progress, the current file, and a \
                         running count are shown live.",
                    )
                    .clicked()
                {
                    acts.push(Act::Ask);
                }
                if self.running {
                    ui.add(egui::Spinner::new().color(theme::AMBER));
                    if ui
                        .add(
                            egui::Button::new(RichText::new("CANCEL").color(theme::BLACK))
                                .fill(theme::RED),
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

    fn preview_panel(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        if self.command.is_diff() {
            if self.diff_rows.is_empty() {
                ui.add_space(6.0);
                ui.colored_label(
                    theme::TEXT,
                    "Pick two repos and press PREVIEW to compare them.",
                );
                return;
            }
            if let Some(action) = crate::diff_board::board(
                ui,
                &mut self.board_state,
                &self.diff_rows,
                &self.preview_source_header,
                &self.preview_target_header,
            ) {
                acts.push(Act::Board(action));
            }
            return;
        }
        if self.preview.is_empty() {
            ui.add_space(6.0);
            ui.colored_label(
                theme::TEXT,
                "Pick a source, a target and a command, then press PREVIEW.",
            );
            return;
        }
        if let Some(review::ReviewAction::Apply(key)) = review::table(
            ui,
            &mut self.review_state,
            &mut self.preview,
            self.preview_totals,
            &self.preview_source_header,
            &self.preview_target_header,
            review::RowControls::Enabled,
        ) {
            acts.push(Act::ApplyRow(key));
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
                ui.add(egui::Spinner::new().color(theme::AMBER));
            }
            let current = if self.run_current.is_empty() {
                "preparing…".to_string()
            } else {
                self.run_current.clone()
            };
            ui.label(RichText::new(current).color(theme::AMBER).strong());
        });

        let summary = if self.run_total > 0 {
            format!("{} / {}", self.run_done, self.run_total)
        } else {
            self.run_done.to_string()
        };
        ui.label(
            RichText::new(format!("Processed {summary}"))
                .color(theme::TAN)
                .size(12.0),
        );

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .max_height(180.0)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for line in &self.run_log {
                    ui.label(RichText::new(line).color(theme::TEXT).size(12.0));
                }
            });
    }

    fn confirm_modal(&mut self, ui: &mut egui::Ui, prompt: &str, acts: &mut Vec<Act>) {
        egui::Modal::new(Id::new("transfer-confirm")).show(&ui.ctx().clone(), |ui| {
            ui.set_width(380.0);
            ui.label(
                RichText::new(format!("CONFIRM {}", self.command.label()))
                    .color(theme::AMBER)
                    .size(16.0)
                    .strong(),
            );
            ui.add_space(6.0);
            ui.colored_label(theme::TEXT, prompt);
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let fill = if self.destructive_run() {
                    theme::RED
                } else {
                    theme::AMBER
                };
                if ui
                    .add(egui::Button::new(RichText::new("PROCEED").color(theme::BLACK)).fill(fill))
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
                    .button(RichText::new("CANCEL").color(theme::BLACK))
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
                if let Some(mut prompt) = self.build_prompt(store) {
                    let rejected = self.review_state.rejected.len();
                    if rejected > 0 {
                        prompt.push_str(&format!(" {rejected} rejected row(s) will be skipped."));
                    }
                    self.confirm = Some(prompt);
                }
            }
            Act::CancelConfirm => self.confirm = None,
            Act::Confirm => {
                self.confirm = None;
                self.start(store, None);
            }
            Act::ApplyRow(key) => self.start(store, Some(key)),
            Act::SetPairing(pairing) => {
                self.pairing = pairing;
                self.clear_preview();
            }
            Act::Board(crate::diff_board::BoardAction::Inspect {
                left_rel,
                right_rel,
            }) => self.open_inspect(store, &left_rel, &right_rel),
            Act::Board(action) => self.start_board_action(store, action),
            Act::CancelRun => self.cancel.cancel(),
        }
    }

    fn clear_preview(&mut self) {
        self.preview.clear();
        self.diff_rows.clear();
        self.board_state.page = 0;
        // A popup (and an open comparison) belongs to the rows it was opened
        // from.
        self.board_state.popup = None;
        self.inspect = None;
        self.preview_totals = [0; 3];
        self.preview_source_header.clear();
        self.preview_target_header.clear();
        self.preview_total = 0;
        self.sync_delete_total = 0;
        // Rejections are keyed to the preview they were made in.
        self.review_state.rejected.clear();
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
        // Start the dialog in the current folder if it is a real directory.
        let start = Some(self.folder.clone()).filter(|f| Path::new(f).is_dir());
        // Run the native picker modally, parented to our window, so it grabs
        // focus and a second one can't be opened while it's up.
        let mut dialog = rfd::FileDialog::new()
            .set_title("Choose the export folder")
            .set_parent(frame);
        if let Some(dir) = start {
            dialog = dialog.set_directory(dir);
        }
        if let Some(dir) = dialog.pick_folder() {
            self.folder = dir.to_string_lossy().into_owned();
            self.error = None;
            self.clear_preview();
        }
    }

    /// The current filter expression composed by the shared wizard.
    fn filter_string(&self) -> Option<String> {
        self.filter.filter_string()
    }

    fn run_preview(&mut self, store: &Store) {
        let Some(source) = self.source.clone() else {
            return;
        };
        // PREVIEW and RUN are mutually exclusive: previewing drops any run log.
        self.reset_run();
        if self.command.is_diff() {
            self.run_preview_diff(store, &source);
            return;
        }
        if self.command.repo_to_repo() {
            self.run_preview_sync(store, &source);
            return;
        }
        match self.destination {
            Destination::Repo => self.run_preview_repo(store, &source),
            Destination::Folder => self.run_preview_folder(store, &source),
        }
    }

    fn run_preview_sync(&mut self, store: &Store, source: &str) {
        let Some(target) = self.target.clone() else {
            return;
        };
        let filter = self.filter_string();
        let delete = self.sync_delete_mode();
        match plan_sync(store, source, &target, true, delete, filter.as_deref()) {
            Ok(plan) => {
                self.preview_total = plan.copies.len();
                self.sync_delete_total = plan.deletes.len();
                self.preview_totals = [plan.copies.len(), plan.deletes.len(), 0];
                self.preview_source_header = Self::repo_header(store, source);
                self.preview_target_header = Self::repo_header(store, &target);
                // A copy: source keeps the file (unchanged), target gains it
                // (added). A delete: the source no longer has it (absent), the
                // target loses it (removed). Capped, then sorted.
                let mut rows: Vec<review::ReviewRow> = plan
                    .copies
                    .iter()
                    .take(PREVIEW_CAP)
                    .map(|rel| review::ReviewRow {
                        source: review::SideStatus::Unchanged,
                        target: review::SideStatus::Added,
                        source_path: rel.clone(),
                        target_path: rel.clone(),
                    })
                    .collect();
                for rel in plan
                    .deletes
                    .iter()
                    .take(PREVIEW_CAP.saturating_sub(rows.len()))
                {
                    rows.push(review::ReviewRow {
                        source: review::SideStatus::Absent,
                        target: review::SideStatus::Removed,
                        source_path: String::new(),
                        target_path: rel.clone(),
                    });
                }
                review::sort(&mut rows, &self.review_state);
                self.preview = rows;
                let verb = self.command.label();
                self.status = Some(if delete == SyncDelete::None {
                    format!("{verb}: {} to copy.", self.preview_total)
                } else {
                    format!(
                        "{verb}: {} to copy, {} to delete.",
                        self.preview_total, self.sync_delete_total
                    )
                });
                self.error = None;
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    fn run_preview_repo(&mut self, store: &Store, source: &str) {
        let Some(target) = self.target.clone() else {
            return;
        };
        let filter = self.filter_string();
        let references = self.references(&target);
        let ref_slice: Vec<&str> = references.iter().map(String::as_str).collect();
        match diff_print(store, source, &ref_slice, filter.as_deref()) {
            Ok(items) => {
                // A file the target lacks (New) is added on the target side; on
                // the source side a COPY leaves it unchanged while a MOVE removes
                // it. Files the target already has (Equal) are unchanged on both
                // sides. DeletedInReference isn't part of a transfer.
                let subdir = self.normalized_subdir();
                let move_files = self.command == Command::Move;
                let source_state = if move_files {
                    review::SideStatus::Removed
                } else {
                    review::SideStatus::Unchanged
                };
                let mut acted = 0usize;
                let mut unchanged = 0usize;
                let mut rows: Vec<review::ReviewRow> = Vec::new();
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
                                rows.push(review::ReviewRow {
                                    source: source_state,
                                    target: review::SideStatus::Added,
                                    source_path: rel_path.clone(),
                                    target_path: to,
                                });
                            }
                        }
                        DiffItem::Equal { rel_path, .. } => {
                            unchanged += 1;
                            if rows.len() < PREVIEW_CAP {
                                rows.push(review::ReviewRow {
                                    source: review::SideStatus::Unchanged,
                                    target: review::SideStatus::Unchanged,
                                    source_path: rel_path.clone(),
                                    target_path: rel_path.clone(),
                                });
                            }
                        }
                        DiffItem::DeletedInReference { .. } => {}
                    }
                }
                self.preview_total = acted;
                // A move both removes from source and adds to target; a copy only
                // adds. Totals are [added, removed, unchanged].
                self.preview_totals = if move_files {
                    [acted, acted, unchanged]
                } else {
                    [acted, 0, unchanged]
                };
                self.preview_source_header = Self::repo_header(store, source);
                self.preview_target_header = Self::repo_header(store, &target);
                review::sort(&mut rows, &self.review_state);
                self.preview = rows;
                self.status = Some(format!(
                    "{acted} match the {}.",
                    self.command.label().to_lowercase()
                ));
                self.error = None;
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    /// DIFF: compare source and target and fill the board. Like the other
    /// previews this is a plain index read, so it runs on the UI thread.
    fn run_preview_diff(&mut self, store: &Store, source: &str) {
        let Some(target) = self.target.clone() else {
            return;
        };
        match plan_repo_diff(store, source, &target, self.pairing) {
            Ok(mut rows) => {
                crate::diff_board::sort(&mut rows, &self.board_state);
                let differing = rows
                    .iter()
                    .filter(|r| r.relation != dedup_core::diff::DiffRelation::Equal)
                    .count();
                self.preview_source_header = Self::repo_header(store, source);
                self.preview_target_header = Self::repo_header(store, &target);
                self.preview_total = differing;
                self.diff_rows = rows;
                self.status = Some(format!(
                    "{differing} difference(s) between '{source}' and '{target}'."
                ));
                self.error = None;
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    /// Open the side-by-side comparison for a conflicting row: both versions
    /// of the same path, with everything needed to judge them.
    fn open_inspect(&mut self, store: &Arc<Store>, left_rel: &str, right_rel: &str) {
        let (Some(source), Some(target)) = (self.source.clone(), self.target.clone()) else {
            return;
        };
        let side = |repo: &str, rel: &str| -> Option<crate::diff_inspect::InspectSide> {
            let meta = store.get_repo(repo).ok()?;
            let entry = store.get_file_entry(repo, rel).ok().flatten()?;
            Some(crate::diff_inspect::InspectSide {
                repo: repo.to_string(),
                rel_path: rel.to_string(),
                abs_path: PathBuf::from(&meta.abs_path).join(rel),
                size: entry.size,
                modified_ms: entry.modified_ms,
                mime: entry.mime.clone(),
                hash_hex: dedup_core::thumbnail::hash_hex(&entry.hash),
            })
        };
        match (side(&source, left_rel), side(&target, right_rel)) {
            (Some(left), Some(right)) => {
                self.inspect = Some(crate::diff_inspect::Inspect::new(left, right));
                self.error = None;
            }
            _ => self.error = Some("Could not read both versions of that file.".to_string()),
        }
    }

    /// Execute one DIFF board row action on a worker thread (a single file can
    /// still be large), then re-plan the diff so the row reflects the result.
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

    fn run_preview_folder(&mut self, store: &Store, source: &str) {
        let folder = self.folder.trim().to_string();
        if folder.is_empty() {
            return;
        }
        let filter = self.filter_string();
        let references = self.folder_references();
        let ref_slice: Vec<&str> = references.iter().map(String::as_str).collect();
        match plan_folder_export(
            store,
            source,
            &ref_slice,
            self.folder_mode(),
            self.invert,
            filter.as_deref(),
        ) {
            Ok(rels) => {
                self.preview_total = rels.len();
                let move_files = self.command == Command::Move;
                let source_state = if move_files {
                    review::SideStatus::Removed
                } else {
                    review::SideStatus::Unchanged
                };
                // Exporting adds each file into the folder; a MOVE also removes
                // it from the source repo, a COPY leaves the source unchanged.
                self.preview_totals = if move_files {
                    [rels.len(), rels.len(), 0]
                } else {
                    [rels.len(), 0, 0]
                };
                self.preview_source_header = Self::repo_header(store, source);
                self.preview_target_header = folder.clone();
                let mut rows: Vec<review::ReviewRow> = rels
                    .iter()
                    .take(PREVIEW_CAP)
                    .map(|rel| review::ReviewRow {
                        source: source_state,
                        target: review::SideStatus::Added,
                        source_path: rel.clone(),
                        target_path: rel.clone(),
                    })
                    .collect();
                review::sort(&mut rows, &self.review_state);
                self.preview = rows;
                let what = if self.invert { "redundant" } else { "unique" };
                self.status = Some(format!(
                    "{} {what} file(s) to {}.",
                    self.preview_total,
                    self.command.label().to_lowercase()
                ));
                self.error = None;
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    fn build_prompt(&mut self, store: &Store) -> Option<String> {
        // Refresh the count so the confirmation reflects the current filter.
        self.run_preview(store);
        let source = self.source.as_ref()?;
        let dest = match self.destination {
            Destination::Repo => {
                let target = self.target.as_ref()?;
                let subdir = self.normalized_subdir();
                if subdir.is_empty() {
                    target.to_string()
                } else {
                    format!("{target}/{subdir}")
                }
            }
            Destination::Folder => {
                let folder = self.folder.trim();
                if folder.is_empty() {
                    return None;
                }
                folder.to_string()
            }
        };
        Some(match self.command {
            Command::Copy => format!(
                "Copy {} file(s) from '{source}' into '{dest}'?",
                self.preview_total
            ),
            Command::Move => format!(
                "Move {} file(s) from '{source}' into '{dest}'? They are removed from the source directory.",
                self.preview_total
            ),
            Command::Sync if self.sync_delete_missing => format!(
                "Sync '{source}' → '{dest}': copy {} file(s) into the target and delete {} \
                 file(s) from the target. Deletions cannot be undone. The source is not changed.",
                self.preview_total, self.sync_delete_total
            ),
            Command::Sync => format!(
                "Sync '{source}' → '{dest}': copy {} file(s) into the target. Nothing is \
                 deleted and the source is not changed.",
                self.preview_total
            ),
            Command::Mirror => format!(
                "Mirror '{source}' → '{dest}': copy {} file(s) into the target and DELETE {} \
                 file(s) the source does not have, so the target ends up holding exactly the \
                 source's content. Deletions cannot be undone. The source is not changed.",
                self.preview_total, self.sync_delete_total
            ),
            // DIFF never runs as a batch: its rows are applied one by one.
            Command::Diff => return None,
        })
    }

    /// Start the configured command on a worker thread. `only` restricts the
    /// run to a single review row (the APPLY button); `None` runs the whole
    /// batch minus any rejected rows.
    fn start(&mut self, store: &Arc<Store>, only: Option<String>) {
        let Some(source) = self.source.clone() else {
            return;
        };
        let rejected: std::collections::HashSet<String> = self.review_state.rejected.clone();
        // Snapshot everything the worker needs before spawning, branching on
        // where the transfer lands. SYNC/MIRROR are their own destination
        // (repo→repo at the same relative path), independent of REPO/FOLDER.
        let dest = if self.command.repo_to_repo() {
            let Some(target) = self.target.clone() else {
                return;
            };
            StartDest::Sync {
                target,
                delete: self.sync_delete_mode(),
                mirror: self.command == Command::Mirror,
            }
        } else {
            match self.destination {
                Destination::Repo => {
                    let Some(target) = self.target.clone() else {
                        return;
                    };
                    StartDest::Repo {
                        references: self.references(&target),
                        target,
                        subdir: self.normalized_subdir(),
                    }
                }
                Destination::Folder => {
                    let folder = self.folder.trim().to_string();
                    if folder.is_empty() {
                        return;
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
        let filter = self.filter_string();
        let command = self.command;
        let move_files = command == Command::Move;
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
            // RUN and PREVIEW are mutually exclusive: starting a run drops the
            // stale preview and resets the live run log/counters.
            self.clear_preview();
            self.reset_run();
        }

        std::thread::spawn(move || {
            let progress = ChannelDiffProgress { tx: tx.clone() };
            let only_set: Option<std::collections::HashSet<String>> =
                only.map(|k| std::collections::HashSet::from([k]));
            let run = DiffRun::new(&progress, &cancel).with_selection(
                (!rejected.is_empty()).then_some(&rejected),
                only_set.as_ref(),
            );
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
        if got || self.running {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(100));
        }
    }
}

/// Kittest UI tests for the Transfer view. Mirrors the harness pattern
/// established in `dupes_view.rs`'s `ui_tests` module.
#[cfg(test)]
mod ui_tests {
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
                        crate::theme::apply(ui.ctx());
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
                        crate::theme::apply(ui.ctx());
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
                        crate::theme::apply(ui.ctx());
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
                        crate::theme::apply(ui.ctx());
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
        view.preview = vec![
            // A copy: source unchanged (grey ✓), target added (green +).
            review::ReviewRow {
                source: review::SideStatus::Unchanged,
                target: review::SideStatus::Added,
                source_path: "holiday.jpg".to_string(),
                target_path: "holiday.jpg".to_string(),
            },
            // Unchanged on both sides (hidden until the toggle is on).
            review::ReviewRow {
                source: review::SideStatus::Unchanged,
                target: review::SideStatus::Unchanged,
                source_path: "notes.txt".to_string(),
                target_path: "notes.txt".to_string(),
            },
        ];
        view.preview_totals = [1, 0, 1];
        review::sort(&mut view.preview, &view.review_state);

        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1120.0, 1100.0))
            .build_ui_state(
                move |ui, view: &mut TransferView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx());
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
            },
            RepoDiffRow {
                relation: dedup_core::diff::DiffRelation::Renamed,
                left: vec![file("old-name.txt", 12)],
                right: vec![file("new-name.txt", 12)],
            },
            RepoDiffRow {
                relation: dedup_core::diff::DiffRelation::Equal,
                left: vec![file("notes.txt", 15)],
                right: vec![file("notes.txt", 15)],
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
                        crate::theme::apply(ui.ctx());
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
            h.query_by_label("PREVIEW").is_some(),
            "PREVIEW still builds the comparison"
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
        // A one-sided row offers COPY on the side that lacks it and DELETE on
        // the side that has it; a rename offers RENAME on both sides.
        assert_eq!(
            h.get_all_by_label("COPY").count(),
            2,
            "the COPY command button plus the row's copy-across action"
        );
        assert!(h.query_by_label("DELETE").is_some(), "or delete it here");
        assert_eq!(
            h.get_all_by_label("RENAME").count(),
            2,
            "a renamed pair can be resolved from either side"
        );
        // Sizes and dates are shown for both sides.
        assert!(
            h.query_by_label_contains("2.00 KB").is_some(),
            "the size column is filled"
        );

        h.get_by_label_contains("SHOW EQUAL").click();
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
        // The board renders below the command bar, so the row's COPY button is
        // the second one on screen (the first is the COPY command).
        match h.get_all_by_label("COPY").last() {
            Some(button) => button.click(),
            None => panic!("no COPY button on the board"),
        }
        // The action runs on a worker thread; pump frames until it lands.
        for _ in 0..200 {
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

    /// Render snapshot of the DIFF board to `target/transfer_diff.png`.
    #[test]
    #[ignore = "renders a PNG for manual inspection"]
    fn render_diff_board() {
        let mut h = diff_harness();
        let out =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/transfer_diff.png");
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
            h.query_by_label_contains("1 added").is_some(),
            "summary shows the added count"
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
            !h.state().review_state.show_unchanged,
            "unchanged hidden by default"
        );
        assert!(
            h.query_all_by_label("notes.txt").next().is_none(),
            "the unchanged row is hidden until the toggle is on"
        );
        h.get_by_label_contains("SHOW UNCHANGED").click();
        h.run();
        assert!(h.state().review_state.show_unchanged, "toggle turns it on");
        assert!(
            h.query_all_by_label("notes.txt").next().is_some(),
            "the unchanged row appears once shown"
        );

        // Source path is the default sort column; clicking its header (the repo
        // path) flips direction.
        assert!(h.state().review_state.sort_asc, "starts ascending");
        h.get_by_label_contains("/repos/source").click();
        h.run();
        assert!(
            !h.state().review_state.sort_asc,
            "clicking the source header toggles the sort direction"
        );
    }

    /// Renders the review table to a PNG for manual inspection. `--ignored`.
    #[test]
    #[ignore = "renders a PNG for manual inspection"]
    fn render_review_table() {
        let mut h = review_harness();
        // Seed one of each status and reveal unchanged, so the PNG shows the full
        // side-by-side vocabulary (added / removed / unchanged / absent).
        {
            let v = h.state_mut();
            v.review_state.show_unchanged = true;
            v.preview_totals = [1, 1, 1];
            v.preview = vec![
                review::ReviewRow {
                    source: review::SideStatus::Unchanged,
                    target: review::SideStatus::Added,
                    source_path: "holiday.jpg".to_string(),
                    target_path: "holiday.jpg".to_string(),
                },
                review::ReviewRow {
                    source: review::SideStatus::Removed,
                    target: review::SideStatus::Absent,
                    source_path: "old.tmp".to_string(),
                    target_path: String::new(),
                },
                review::ReviewRow {
                    source: review::SideStatus::Unchanged,
                    target: review::SideStatus::Unchanged,
                    source_path: "notes.txt".to_string(),
                    target_path: "notes.txt".to_string(),
                },
            ];
            review::sort(&mut v.preview, &v.review_state);
        }
        h.run();
        let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/transfer_review.png");
        let img = h.render().expect("wgpu render failed");
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());
    }
}
