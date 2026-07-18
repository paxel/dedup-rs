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

use crate::filter_ui::FilterBuilder;
use crate::icon;
use crate::settings::TooltipVerbosity;
use crate::theme;
use crate::util::ExplainExt;
use crossbeam_channel::{Receiver, Sender};
use dedup_core::diff::{
    CopyDest, DiffAction, DiffEvent, DiffItem, DiffProgress, DiffRun, FolderMode, SyncDelete,
    diff_copy, diff_print, diff_sync, export_to_folder, plan_folder_export, plan_sync,
};
use dedup_core::store::Store;
use dedup_core::update::CancellationToken;
use egui::{Id, RichText};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const PREVIEW_LIMIT: usize = 30;
/// How many recent actions the running panel keeps in its scrolling log.
const RUN_LOG_LIMIT: usize = 10;

#[derive(PartialEq, Clone, Copy)]
enum Command {
    Copy,
    Move,
    Sync,
    Mirror,
}

impl Command {
    fn label(self) -> &'static str {
        match self {
            Command::Copy => "COPY",
            Command::Move => "MOVE",
            Command::Sync => "SYNC",
            Command::Mirror => "MIRROR",
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
        matches!(self, Command::Sync | Command::Mirror)
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

struct PreviewRow {
    from: String,
    to: String,
    /// A SYNC deletion (rendered in red as `path → deleted`) rather than a
    /// `from → to` transfer.
    del: bool,
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
    subdir_tx: Sender<Result<String, String>>,
    subdir_rx: Receiver<Result<String, String>>,
    /// Absolute export folder picked by the native folder dialog thread.
    folder_tx: Sender<Result<String, String>>,
    folder_rx: Receiver<Result<String, String>>,
    /// The shared FILTER wizard (conditions, presets, suggestions, live count).
    filter: FilterBuilder,
    preview: Vec<PreviewRow>,
    preview_total: usize,
    status: Option<String>,
    error: Option<String>,
    confirm: Option<String>,
    running: bool,
    cancel: CancellationToken,
    // Live run progress: the last N actions, the running counters and the
    // file currently being handled.
    run_log: VecDeque<String>,
    run_done: u64,
    run_total: u64,
    run_current: String,
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
    MarkSourceDone,
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
}

impl TransferView {
    pub fn new() -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        let (subdir_tx, subdir_rx) = crossbeam_channel::unbounded();
        let (folder_tx, folder_rx) = crossbeam_channel::unbounded();
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
            subdir_tx,
            subdir_rx,
            folder_tx,
            folder_rx,
            filter: FilterBuilder::new(),
            preview: Vec::new(),
            preview_total: 0,
            status: None,
            error: None,
            confirm: None,
            running: false,
            cancel: CancellationToken::new(),
            run_log: VecDeque::new(),
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

    pub fn show(&mut self, ui: &mut egui::Ui, store: &Arc<Store>, verbosity: TooltipVerbosity) {
        self.verbosity = verbosity;
        self.drain(ui);
        if !self.loaded {
            self.sync_repos(store);
        }

        let mut acts: Vec<Act> = Vec::new();

        // Keyboard shortcuts — skipped while the confirm modal is up, a run is
        // active, or a text field is focused.
        if self.confirm.is_none() && !self.running && !ui.ctx().egui_wants_keyboard_input() {
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
                if i.key_pressed(egui::Key::P) {
                    acts.push(Act::Preview);
                }
                if i.key_pressed(egui::Key::R) {
                    acts.push(Act::Ask);
                }
            });
        }

        ui.add_space(6.0);
        ui.label(
            RichText::new("TRANSFER")
                .color(theme::BLUE)
                .size(18.0)
                .strong(),
        );
        crate::util::shortcut_bar(
            ui,
            "1 copy · 2 move · 3 sync · 4 mirror · P preview · R run",
        );

        self.repo_rows(ui, &mut acts);
        self.command_bar(ui, &mut acts);
        // SYNC/MIRROR are always repo→repo at the same relative path, so they
        // hide the DEST/subdir/folder controls: SYNC shows its DELETE MISSING
        // toggle; MIRROR shows a warning (it always deletes).
        match self.command {
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
        // The shared FILTER wizard; the source repo backs its MIME suggestions
        // and live match count.
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
        self.action_bar(ui, &mut acts);

        if let Some(err) = &self.error {
            ui.colored_label(theme::RED, err);
        }
        if let Some(status) = &self.status {
            ui.label(RichText::new(status).color(theme::TAN).size(13.0));
        }
        ui.separator();
        // RUN and PREVIEW are mutually exclusive: while a run is active or has
        // left a log, show the live run panel; otherwise show the preview.
        if self.running || !self.run_log.is_empty() {
            self.run_panel(ui);
        } else {
            self.preview_panel(ui);
        }

        if let Some(prompt) = self.confirm.clone() {
            self.confirm_modal(ui, &prompt, &mut acts);
        }

        let ctx = ui.ctx().clone();
        for act in acts {
            self.apply(store, &ctx, act);
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
        theme::section(theme::LILAC).show(ui, |ui| {
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
            // "already known" and never re-copied. In REPO mode the target is
            // always a reference, shown as a pinned (always-on) chip. SYNC
            // compares source against the single target only, so it has no pool.
            if !self.command.repo_to_repo() {
                // Pinned target first (if any), then the toggleable pool repos.
                enum Pool {
                    Pinned(String),
                    Toggle(String),
                }
                let mut items: Vec<Pool> = Vec::new();
                if self.destination == Destination::Repo
                    && let Some(target) = self.target.clone()
                {
                    items.push(Pool::Pinned(target));
                }
                for name in &self.repos {
                    if self.source.as_deref() == Some(name.as_str()) {
                        continue;
                    }
                    if self.destination == Destination::Repo
                        && self.target.as_deref() == Some(name.as_str())
                    {
                        continue;
                    }
                    items.push(Pool::Toggle(name.clone()));
                }
                crate::repo_chip::chip_row(ui, "xfer_pool", "DUPEPOOL", items.len(), |ui, i| {
                    match &items[i] {
                        // Always on and non-toggleable: the target is always in the pool.
                        Pool::Pinned(target) => {
                            let chip =
                                crate::repo_chip::repo_chip(ui, target, true, theme::LILAC, None);
                            chip.name.explain(
                                self.verbosity,
                                "Always in the pool (it's the target)",
                                "The target repo is always in the dupe pool — COPY/MOVE never \
                                 re-copies content the target already has — so it can't be \
                                 toggled off.",
                            );
                            chip.outer
                        }
                        Pool::Toggle(name) => {
                            let sel = self.extra_refs.iter().any(|r| r == name);
                            let chip =
                                crate::repo_chip::repo_chip(ui, name, sel, theme::LILAC, None);
                            if chip
                                .name
                                .explain(
                                    self.verbosity,
                                    "Add to the dupe pool",
                                    "Also check for dupes vs these repos in addition to the \
                                     target repo.",
                                )
                                .clicked()
                            {
                                acts.push(Act::ToggleExtraRef(name.clone()));
                            }
                            chip.outer
                        }
                    }
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
        theme::section(theme::ORANGE).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("COMMAND").color(theme::TEXT).size(12.0));
                for cmd in [Command::Copy, Command::Move, Command::Sync, Command::Mirror] {
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
        theme::section(theme::BLUE).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("INTO").color(theme::TEXT).size(12.0));
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
        });
    }

    /// Selector for where COPY/MOVE lands: into a repo or into a picked folder.
    fn dest_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        theme::section(theme::BLUE).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("DEST").color(theme::TEXT).size(12.0));
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
        theme::section(theme::BLUE).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("FOLDER").color(theme::TEXT).size(12.0));
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
                        RichText::new(format!("{} BROWSE", icon::FOLDER_OPEN)).color(theme::BLACK),
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
        });
    }

    /// Grouping mode (exact/similar) and the invert toggle for a folder export.
    fn mode_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        theme::section(theme::LILAC).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("MODE").color(theme::TEXT).size(12.0));
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
                        .add(egui::Button::new(RichText::new(mode.label()).color(col)).fill(fill))
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
                    crate::util::similarity_slider(ui, &mut self.similar_threshold, self.verbosity);
                });
            }
            let hint = if self.invert {
                "Exports the redundant copies (every non-best member of a group)."
            } else {
                "Exports the unique files (best copy of each group plus every singleton)."
            };
            ui.label(RichText::new(hint).color(theme::LILAC).size(11.0));
        });
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
        };
        ui.label(RichText::new(text).color(theme::LILAC).size(11.0));
    }

    /// MIRROR's info bar: no toggle (it always deletes), just a red warning that
    /// it removes everything in the target the source lacks.
    fn mirror_bar(&self, ui: &mut egui::Ui) {
        theme::section(theme::RED).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(icon::TRASH).color(theme::RED).size(12.0));
                ui.label(
                    RichText::new("DELETES EXTRAS")
                        .color(theme::RED)
                        .size(12.0)
                        .strong(),
                );
            });
            ui.label(
                RichText::new(
                    "Everything in the target whose content the source does not have is \
                     deleted, so the target ends up holding exactly the source's content. \
                     Deletions cannot be undone.",
                )
                .color(theme::LILAC)
                .size(11.0),
            );
        });
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
    fn sync_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        theme::section(theme::BLUE).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("OPTIONS").color(theme::TEXT).size(12.0));
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
        theme::section(theme::AMBER).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("ACTION").color(theme::TEXT).size(12.0));
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
                // After sanitizing a disk, mark the source repo triage-done.
                if self.source.is_some() && !self.running {
                    ui.separator();
                    if ui
                        .add(
                            egui::Button::new(RichText::new("MARK SOURCE DONE").color(theme::BLUE))
                                .fill(theme::PANEL),
                        )
                        .explain(
                            self.verbosity,
                            "Flag the source repo as triaged (its uniques copied out)",
                            "Mark the source repository triage-done: its unique content has \
                             already been copied out into a sanitized directory, so it shows \
                             a TRIAGED stat in Repository management and can be treated as \
                             fully processed.",
                        )
                        .clicked()
                    {
                        acts.push(Act::MarkSourceDone);
                    }
                }
            });
        });
    }

    fn preview_panel(&mut self, ui: &mut egui::Ui) {
        if self.preview.is_empty() {
            ui.add_space(6.0);
            ui.colored_label(
                theme::TEXT,
                "Pick a source, a target and a command, then press PREVIEW.",
            );
            return;
        }
        let header = if self.command.repo_to_repo() && self.sync_delete_mode() != SyncDelete::None {
            format!(
                "{} to copy · {} to delete · showing first {}",
                self.preview_total,
                self.sync_delete_total,
                self.preview.len()
            )
        } else {
            format!(
                "{} file(s) match · showing first {}",
                self.preview_total,
                self.preview.len()
            )
        };
        ui.label(RichText::new(header).color(theme::AMBER).strong());
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Grid::new("preview_grid")
                    .num_columns(3)
                    .striped(true)
                    .spacing(egui::vec2(12.0, 4.0))
                    .show(ui, |ui| {
                        for row in &self.preview {
                            if row.del {
                                // A SYNC deletion: `target/path → deleted`, in red.
                                ui.label(RichText::new(&row.from).color(theme::RED).size(12.0));
                                ui.label(RichText::new(icon::ARROW_RIGHT).color(theme::RED));
                                ui.label(RichText::new("deleted").color(theme::RED).size(12.0));
                            } else {
                                ui.label(RichText::new(&row.from).color(theme::TEXT).size(12.0));
                                ui.label(RichText::new(icon::ARROW_RIGHT).color(theme::ORANGE));
                                ui.label(RichText::new(&row.to).color(theme::BLUE).size(12.0));
                            }
                            ui.end_row();
                        }
                    });
            });
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

    fn apply(&mut self, store: &Arc<Store>, ctx: &egui::Context, act: Act) {
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
            Act::MarkSourceDone => {
                if let Some(source) = self.source.clone() {
                    match store.set_triage_done(&source, true) {
                        Ok(()) => {
                            self.status = Some(format!("Marked '{source}' triage-done."));
                            self.error = None;
                        }
                        Err(e) => self.error = Some(e.to_string()),
                    }
                }
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
            Act::BrowseFolder => self.browse_folder(ctx),
            Act::SubdirChanged => self.clear_preview(),
            Act::BrowseSubdir => self.browse_subdir(store, ctx),
            Act::Preview => self.run_preview(store),
            Act::Ask => {
                if let Some(prompt) = self.build_prompt(store) {
                    self.confirm = Some(prompt);
                }
            }
            Act::CancelConfirm => self.confirm = None,
            Act::Confirm => {
                self.confirm = None;
                self.start(store);
            }
            Act::CancelRun => self.cancel.cancel(),
        }
    }

    fn clear_preview(&mut self) {
        self.preview.clear();
        self.preview_total = 0;
        self.sync_delete_total = 0;
    }

    /// The subdir trimmed of surrounding whitespace and slashes; empty means
    /// "place files at the target root".
    fn normalized_subdir(&self) -> String {
        self.subdir.trim().trim_matches('/').to_string()
    }

    /// Open the native folder dialog rooted at the target repo and, on a pick,
    /// store the chosen folder as a path relative to the target root.
    fn browse_subdir(&mut self, store: &Arc<Store>, ctx: &egui::Context) {
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
        let tx = self.subdir_tx.clone();
        let repaint = ctx.clone();
        std::thread::spawn(move || {
            if let Some(dir) = rfd::FileDialog::new()
                .set_title("Choose a subdirectory inside the target")
                .set_directory(&target_root)
                .pick_folder()
            {
                let msg = match dir.strip_prefix(&target_root) {
                    Ok(rel) => Ok(rel.to_string_lossy().replace('\\', "/")),
                    Err(_) => Err("The chosen folder is outside the target repo.".to_string()),
                };
                let _ = tx.send(msg);
                repaint.request_repaint();
            }
        });
    }

    /// Open the native folder dialog and store the picked absolute path as the
    /// export folder (Destination::Folder).
    fn browse_folder(&mut self, ctx: &egui::Context) {
        let tx = self.folder_tx.clone();
        let repaint = ctx.clone();
        // Start the dialog in the current folder if it is a real directory.
        let start = Some(self.folder.clone()).filter(|f| Path::new(f).is_dir());
        std::thread::spawn(move || {
            let mut dialog = rfd::FileDialog::new().set_title("Choose the export folder");
            if let Some(dir) = start {
                dialog = dialog.set_directory(dir);
            }
            if let Some(dir) = dialog.pick_folder() {
                let _ = tx.send(Ok(dir.to_string_lossy().into_owned()));
                repaint.request_repaint();
            }
        });
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
                // Copies first (green-ish `from → to`), then any deletions
                // (red `path → deleted`), up to the shared preview limit.
                let mut rows: Vec<PreviewRow> = plan
                    .copies
                    .iter()
                    .take(PREVIEW_LIMIT)
                    .map(|rel| PreviewRow {
                        from: format!("{source}/{rel}"),
                        to: format!("{target}/{rel}"),
                        del: false,
                    })
                    .collect();
                for rel in plan
                    .deletes
                    .iter()
                    .take(PREVIEW_LIMIT.saturating_sub(rows.len()))
                {
                    rows.push(PreviewRow {
                        from: format!("{target}/{rel}"),
                        to: String::new(),
                        del: true,
                    });
                }
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
                // Copy/Move act on content the target lacks (New).
                let matched: Vec<&DiffItem> = items
                    .iter()
                    .filter(|item| matches!(item, DiffItem::New { .. }))
                    .collect();
                self.preview_total = matched.len();
                self.preview = matched
                    .into_iter()
                    .take(PREVIEW_LIMIT)
                    .map(|item| self.preview_row(source, &target, item))
                    .collect();
                self.status = Some(format!(
                    "{} match the {}.",
                    self.preview_total,
                    self.command.label().to_lowercase()
                ));
                self.error = None;
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    fn run_preview_folder(&mut self, store: &Store, source: &str) {
        let folder = self.folder.trim();
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
                self.preview = rels
                    .iter()
                    .take(PREVIEW_LIMIT)
                    .map(|rel| PreviewRow {
                        from: format!("{source}/{rel}"),
                        to: format!("{folder}/{rel}"),
                        del: false,
                    })
                    .collect();
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

    fn preview_row(&self, source: &str, target: &str, item: &DiffItem) -> PreviewRow {
        let subdir = self.normalized_subdir();
        match item {
            DiffItem::New { rel_path } => {
                let to = if subdir.is_empty() {
                    format!("{target}/{rel_path}")
                } else {
                    format!("{target}/{subdir}/{rel_path}")
                };
                PreviewRow {
                    from: format!("{source}/{rel_path}"),
                    to,
                    del: false,
                }
            }
            // Transfer only previews New items (see `run_preview`); content the
            // target already has is never shown as a transfer.
            DiffItem::Equal { rel_path, .. } | DiffItem::DeletedInReference { rel_path } => {
                PreviewRow {
                    from: format!("{source}/{rel_path}"),
                    to: String::new(),
                    del: false,
                }
            }
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
        })
    }

    fn start(&mut self, store: &Arc<Store>) {
        let Some(source) = self.source.clone() else {
            return;
        };
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
        // RUN and PREVIEW are mutually exclusive: starting a run drops the
        // stale preview and resets the live run log/counters.
        self.clear_preview();
        self.reset_run();

        std::thread::spawn(move || {
            let progress = ChannelDiffProgress { tx: tx.clone() };
            let run = DiffRun::new(&progress, &cancel);
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
                self.run_log.push_back(format!("✗ {path}: {message}"));
                while self.run_log.len() > RUN_LOG_LIMIT {
                    self.run_log.pop_front();
                }
            }
        }
    }

    fn drain(&mut self, ui: &egui::Ui) {
        let mut got = false;
        // Apply any subfolder picked by the native subdir dialog thread.
        while let Ok(picked) = self.subdir_rx.try_recv() {
            got = true;
            match picked {
                Ok(rel) => {
                    self.subdir = rel;
                    self.error = None;
                    self.clear_preview();
                }
                Err(e) => self.error = Some(e),
            }
        }
        // Apply any export folder picked by the native folder dialog thread.
        while let Ok(picked) = self.folder_rx.try_recv() {
            got = true;
            match picked {
                Ok(abs) => {
                    self.folder = abs;
                    self.error = None;
                    self.clear_preview();
                }
                Err(e) => self.error = Some(e),
            }
        }
        while let Ok(msg) = self.rx.try_recv() {
            got = true;
            match msg {
                Msg::Progress(event) => self.apply_progress(event),
                Msg::Done(result) => {
                    self.running = false;
                    match result {
                        OpResult::Copied {
                            copied,
                            cancelled,
                            moved,
                        } => {
                            let verb = if moved { "Moved" } else { "Copied" };
                            self.status = Some(format!(
                                "{verb} {copied} file(s){}.",
                                if cancelled { " (cancelled)" } else { "" }
                            ));
                            self.error = None;
                        }
                        OpResult::Synced {
                            copied,
                            deleted,
                            skipped,
                            errors,
                            cancelled,
                            mirror,
                        } => {
                            let mut parts = vec![format!("copied {copied}")];
                            if deleted > 0 {
                                parts.push(format!("deleted {deleted}"));
                            }
                            if skipped > 0 {
                                parts.push(format!("skipped {skipped}"));
                            }
                            if errors > 0 {
                                parts.push(format!("errors {errors}"));
                            }
                            let verb = if mirror { "Mirror" } else { "Sync" };
                            self.status = Some(format!(
                                "{verb} done: {}{}.",
                                parts.join(", "),
                                if cancelled { " (cancelled)" } else { "" }
                            ));
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
                    view.show(ui, &store_ui, TooltipVerbosity::default());
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
            harness.query_by_label("DELETES EXTRAS").is_some(),
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
            harness.query_by_label("DEST").is_none(),
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
            harness.query_by_label("DEST").is_none(),
            "the REPO/FOLDER destination toggle must be hidden in SYNC mode"
        );
        assert!(
            harness.query_by_label("INTO").is_none(),
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
            harness.query_by_label("MODE").is_some(),
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
            harness.query_by_label("INTO").is_none(),
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
            harness.query_by_label("INTO").is_some(),
            "the INTO subdir bar should be shown in repo mode"
        );
        assert!(
            harness.query_by_label("MODE").is_none(),
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
                    view.show(ui, &store_ui, TooltipVerbosity::default());
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
                    view.show(ui, &store_ui, TooltipVerbosity::default());
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
                    view.show(ui, &store_ui, TooltipVerbosity::default());
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
}
