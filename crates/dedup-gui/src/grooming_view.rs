//! The Grooming tab: prune and tidy a single repository. Unlike Transfer's one
//! shared form, each grooming command is different enough to get its own layout,
//! selected from a top command bar:
//!
//! - **DEDUPE** — delete every file in a source repo whose content also lives in
//!   any of the selected "dupe pool" repos (wraps `dedup_core::diff::diff_delete`).
//! - **PURGE** — delete every file in a repo matching a filter, wildcards and all
//!   (wraps `dedup_core::groom::delete_by_filter`).
//! - **EMPTY DIRS** — remove empty directories under a repo's root
//!   (wraps `dedup_core::groom::delete_empty_dirs`).
//! - **ORGANIZE** — move a repo's files into new rule-based paths in place
//!   (wraps `dedup_core::organize::organize_apply`).
//! - **PRUNE** — drop missing (deleted-from-disk) records and compact the index
//!   (wraps `dedup_core::groom::prune`).
//!
//! All destructive runs go through a confirmation modal and execute on a
//! background thread, reusing the same `DiffEvent` progress plumbing as Transfer.

use crate::board;
use crate::filter_ui::FilterBuilder;
use crate::icon;
use crate::media_cell::{facts_for, open_facts};
use crate::settings::TooltipVerbosity;
use crate::theme;
use crate::thumbs::ThumbCache;
use crate::util::ExplainExt;
use crossbeam_channel::{Receiver, Sender};
use dedup_core::diff::{DiffRun, PlanProgress, diff_delete, diff_print_reporting};
use dedup_core::groom::{
    delete_by_filter, delete_empty_dirs, preview_by_filter, preview_prune, prune,
};
use dedup_core::organize::{DEFAULT_TEMPLATE, OrganizeRule, organize_apply, plan_organize};
use dedup_core::store::Store;
use dedup_core::update::CancellationToken;
use egui::{Id, RichText};
use std::sync::Arc;

use crate::board::PREVIEW_CAP;

/// File name of the persisted ORGANIZE presets inside the store's config dir.
const ORGANIZE_PRESETS_FILE: &str = "organize_presets.json";

/// The template tokens offered as one-click chips beneath each rule's template.
/// (`label`, `insert`) — the insert text is what's appended to the template.
const TEMPLATE_TOKENS: &[(&str, &str)] = &[
    ("o-path", "{o-path}"),
    ("o-name", "{o-name}"),
    ("o-stem", "{o-stem}"),
    ("o-ext", "{o-ext}"),
    ("year", "{year}"),
    ("month", "{month}"),
    ("day", "{day}"),
    ("mimetop", "{mimetop}"),
    ("camera", "{camera}"),
    ("origin", "{origin}"),
    ("size", "{size}"),
    ("/", "/"),
];

#[derive(PartialEq, Clone, Copy)]
enum Command {
    /// Delete source files whose content is also in any selected pool repo.
    Dedupe,
    /// Delete every file matching a filter.
    Purge,
    /// Remove empty directories.
    EmptyDirs,
    /// Reorganize a repo's files in place by rule-based path templates.
    Organize,
    /// Drop missing (deleted-from-disk) records and compact the index.
    Prune,
}

impl Command {
    fn label(self) -> &'static str {
        match self {
            Command::Dedupe => "DEDUPE",
            Command::Purge => "PURGE",
            Command::EmptyDirs => "EMPTY DIRS",
            Command::Organize => "ORGANIZE",
            Command::Prune => "PRUNE",
        }
    }
    /// (short, verbose) selector-button tooltip.
    fn tooltip(self) -> (&'static str, &'static str) {
        match self {
            Command::Dedupe => (
                "Delete a repo's duplicates of other repos",
                "Delete every file in the source repo whose content also exists in any of \
                 the selected dupe-pool repos — the source copy is redundant. The pool \
                 repos are never changed.",
            ),
            Command::Purge => (
                "Delete everything matching a filter",
                "Delete every file in the repo that matches the filter (mime / size / name, \
                 with `*` wildcards) — e.g. everything ending in `.db`. This is not gated \
                 by any other repo, so use it carefully. Without a filter nothing matches — \
                 an empty filter never purges the whole repo.",
            ),
            Command::EmptyDirs => (
                "Remove empty directories",
                "Remove every empty directory under the repo's root, bottom-up. The repo \
                 root itself is kept and indexed files are untouched. On a backup group's \
                 main, its sinks are cleaned in the same run.",
            ),
            Command::Organize => (
                "Reorganize files by path templates",
                "Move a repo's files into new relative paths built from templates (date, \
                 mime, camera, original name, …). Files matching no rule stay put; nothing \
                 is ever overwritten.",
            ),
            Command::Prune => (
                "Forget deleted files and shrink the index",
                "Permanently remove this repo's records of files that no longer exist on \
                 disk (its 'missing' entries), then rewrite the index file to reclaim their \
                 space. The repo's existing files are untouched — only the leftover records \
                 of already-deleted files are dropped.",
            ),
        }
    }
}

#[derive(Debug)]
enum OpResult {
    Deleted {
        deleted: u64,
        cancelled: bool,
    },
    EmptyDirs {
        removed: u64,
        /// How many repositories were swept (1, or main + sinks for a group).
        repos: u64,
        /// Per-repo failures ("repo: error"), merged into the run report.
        errors: Vec<String>,
    },
    Organized {
        moved: u64,
        skipped: u64,
        errors: u64,
        cancelled: bool,
    },
    Pruned {
        pruned: u64,
        compacted: bool,
        cancelled: bool,
    },
    /// A single review row was applied: the card to show.
    Applied {
        note: crate::activity::Notification,
    },
    Error(String),
}

impl OpResult {
    /// The report the activity modal ends on for a batch run.
    fn report(&self) -> crate::run_result::RunReport {
        use crate::run_result::RunReport;
        match self {
            OpResult::Deleted { deleted, cancelled } => RunReport::new("Delete")
                .count("deleted", *deleted)
                .cancelled(*cancelled),
            OpResult::EmptyDirs {
                removed,
                repos,
                errors,
            } => {
                let mut report = RunReport::new("Remove empty dirs")
                    .count("removed", *removed)
                    .problems(errors.iter().cloned());
                if *repos > 1 {
                    report = report.count("repositories swept", *repos);
                }
                report
            }
            OpResult::Organized {
                moved,
                skipped,
                errors,
                cancelled,
            } => {
                let mut report = RunReport::new("Organize")
                    .count("moved", *moved)
                    .count("skipped", *skipped)
                    .cancelled(*cancelled);
                if *errors > 0 {
                    report = report.count("errors", *errors);
                }
                report
            }
            OpResult::Pruned {
                pruned,
                compacted,
                cancelled,
            } => {
                let mut report = RunReport::new("Prune")
                    .count("records dropped", *pruned)
                    .cancelled(*cancelled);
                report = report.note(if *cancelled {
                    "Cancelled: the index was not compacted."
                } else if *compacted {
                    "The index was compacted."
                } else {
                    "The index was already compact."
                });
                report
            }
            OpResult::Applied { note } => {
                let mut report = RunReport::new(note.action.clone());
                if let Err(e) = &note.outcome {
                    report.problem(e.clone());
                }
                report
            }
            OpResult::Error(e) => {
                let mut report = RunReport::new("Grooming");
                report.problem(e.clone());
                report
            }
        }
    }
}

/// A finished preview, built off the UI thread.
struct PreviewData {
    metas: Vec<board::RowMeta>,
    bodies: Vec<board::RowBody>,
    total: usize,
    totals: [usize; 4],
    source_header: String,
    target_header: Option<String>,
    status: String,
}

/// What a preview needs, captured from the controls when it starts so the
/// worker never reads live state.
enum PreviewSpec {
    Dedupe {
        source: String,
        pool: Vec<String>,
        filter: Option<String>,
    },
    Purge {
        repo: String,
        filter: Option<String>,
    },
    Prune {
        repo: String,
    },
    Organize {
        repo: String,
        rules: Vec<OrganizeRule>,
    },
}

/// One ORGANIZE rule in the UI: a shared filter wizard plus a path template.
struct RuleUi {
    filter: FilterBuilder,
    template: String,
}

impl RuleUi {
    fn new() -> Self {
        Self {
            filter: FilterBuilder::new(),
            template: DEFAULT_TEMPLATE.to_string(),
        }
    }
}

/// A saved, named ORGANIZE preset: an ordered list of `(filter, template)`
/// rules, persisted as JSON and re-appliable to any repo.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct OrganizePreset {
    name: String,
    rules: Vec<SavedRule>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct SavedRule {
    /// The filter expression (empty = match all).
    filter: String,
    template: String,
}

enum Msg {
    Done(OpResult),
    /// A finished preview; `confirm` raises the RUN confirmation once the
    /// rows land with real counts (the deferred half of a RUN click).
    Preview {
        result: Result<PreviewData, String>,
        confirm: bool,
    },
    /// A preview the user cancelled from the activity modal.
    PreviewCancelled,
}

pub struct GroomingView {
    repos: Vec<String>,
    /// Repos that are the main of a sync group, for the chip badge. Refreshed
    /// with `repos` whenever the tab is shown.
    mains: std::collections::HashSet<String>,
    loaded: bool,
    command: Command,
    /// DEDUPE: the repo whose duplicates are deleted.
    source: Option<String>,
    /// DEDUPE: the dupe-pool repos whose content makes a source file deletable.
    pool: Vec<String>,
    /// PURGE / EMPTY DIRS / ORGANIZE: the single repo the command acts on.
    repo: Option<String>,
    /// The shared FILTER wizard (used by DEDUPE and PURGE).
    filter: FilterBuilder,
    /// ORGANIZE: the ordered rule list (each a filter wizard + template).
    rules: Vec<RuleUi>,
    /// ORGANIZE: saved presets, loaded once from the config dir.
    presets: Vec<OrganizePreset>,
    presets_loaded: bool,
    /// ORGANIZE: index of the preset currently being renamed inline, if any.
    renaming_preset: Option<usize>,
    /// ORGANIZE: text buffer for the in-progress preset rename.
    rename_buf: String,
    /// ORGANIZE: set for one frame when a rename just started, so its text
    /// field can grab keyboard focus.
    focus_rename_pending: bool,
    /// The board's cheap per-row model, and the thumbnails/facts that go with
    /// it — kept parallel, indexed alike, because the board resolves a row's
    /// body only for the rows actually on screen.
    preview: Vec<board::RowMeta>,
    preview_bodies: Vec<board::RowBody>,
    /// Full counts `[to-delete, only-here, differing, unchanged]` for the board
    /// summary; independent of the capped `preview` sample.
    preview_totals: [usize; 4],
    /// The board's region headers (absolute paths). ORGANIZE uses the same
    /// repo on both sides (old path → new path); the single-repo deletions
    /// (DEDUPE/PURGE/PRUNE) have no target side, so `preview_target_header` is
    /// `None` and the board renders one-sided.
    preview_source_header: String,
    preview_target_header: Option<String>,
    preview_total: usize,
    /// Sort key, side, direction and the hidden-row set for the board.
    board_state: board::BoardState,
    status: Option<String>,
    error: Option<String>,
    confirm: Option<String>,
    running: bool,
    /// Set while a preview is being planned behind the activity modal.
    previewing: bool,
    /// Whether the run in flight is a row action (a card when it lands)
    /// rather than an operation on the activity modal.
    row_action: bool,
    /// Set while a single-row APPLY runs: refresh the preview when it finishes.
    pending_refresh: bool,
    /// The app-wide activity owner: long work runs behind its modal, row
    /// actions answer with its cards, and every file changed is in its log.
    activity: crate::activity::Shared,
    /// WHAT has been answered: a command was picked (or a number key
    /// pressed), so the repository section may show.
    command_chosen: bool,
    /// After REVIEW or RUN the selection sections fold into one summary line
    /// until CHANGE.
    selection_collapsed: bool,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    verbosity: TooltipVerbosity,
    /// The open comparison, if any — the same shared surface the Transfer DIFF
    /// board opens, so a DEDUPE row can be inspected before it is applied.
    inspect: Option<crate::compare_view::DiffCompare>,
    /// Decodes the review board's row thumbnails; polled once per frame.
    thumbs: ThumbCache,
    /// The app-wide repo lock registry (see [`crate::locks`]): which repos'
    /// existing files may be deleted or overwritten this session.
    locks: crate::locks::RepoLocks,
}

enum Act {
    /// Open the shared viewer on a review row: the acted-on file, beside its
    /// counterpart when one exists — a DEDUPE row's redundant copy opens the
    /// comparison, a one-sided row (PURGE/PRUNE, an ORGANIZE relocation of the
    /// same file) opens the single-file view.
    Inspect(String, Option<String>),
    SetCommand(Command),
    PickSource(String),
    TogglePool(String),
    PickRepo(String),
    AddRule,
    RemoveRule(usize),
    StorePreset,
    ApplyPreset(usize),
    RemovePreset(usize),
    CommitRenamePreset(usize),
    Preview,
    Ask,
    Confirm,
    CancelConfirm,
    /// Apply a single review row immediately (its namespaced key).
    ApplyRow(String),
}

impl GroomingView {
    pub fn new() -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        Self {
            repos: Vec::new(),
            mains: std::collections::HashSet::new(),
            loaded: false,
            command: Command::Dedupe,
            source: None,
            pool: Vec::new(),
            repo: None,
            filter: FilterBuilder::new(),
            rules: vec![RuleUi::new()],
            presets: Vec::new(),
            presets_loaded: false,
            renaming_preset: None,
            rename_buf: String::new(),
            focus_rename_pending: false,
            preview: Vec::new(),
            preview_bodies: Vec::new(),
            preview_totals: [0; 4],
            preview_source_header: String::new(),
            preview_target_header: None,
            preview_total: 0,
            board_state: board::BoardState::default(),
            status: None,
            error: None,
            confirm: None,
            running: false,
            previewing: false,
            row_action: false,
            pending_refresh: false,
            activity: crate::activity::scratch(),
            command_chosen: false,
            selection_collapsed: false,
            tx,
            rx,
            verbosity: TooltipVerbosity::default(),
            inspect: None,
            thumbs: ThumbCache::new(3),
            locks: crate::locks::RepoLocks::new(),
        }
    }

    /// Construct wired to the app's shared lock registry, so a repo unlocked
    /// here is unlocked on every tab (and vice versa).
    pub fn new_with_locks(
        locks: crate::locks::RepoLocks,
        activity: crate::activity::Shared,
    ) -> Self {
        let mut me = Self::new();
        me.locks = locks;
        me.activity = activity;
        me
    }

    pub fn show(&mut self, ui: &mut egui::Ui, store: &Arc<Store>, verbosity: TooltipVerbosity) {
        self.verbosity = verbosity;
        if self.thumbs.poll(ui.ctx()) {
            ui.ctx().request_repaint();
        }
        self.drain(&ui.ctx().clone());
        // The shared comparison, when a DEDUPE row asked for it. Any pick just
        // closes it: Grooming's own APPLY / HIDE are how a row is acted on, so
        // the viewer is shared but the decisions stay this view's.
        if let Some(inspect) = self.inspect.as_mut()
            && inspect.view(&ui.ctx().clone(), verbosity, None).is_some()
        {
            self.inspect = None;
        }
        // A finished single-row APPLY refreshes the preview, so the board
        // reflects the applied action instead of dropping to the run log.
        if self.pending_refresh && !self.running {
            self.pending_refresh = false;
            self.run_preview(store, &ui.ctx().clone(), false);
        }
        if !self.loaded {
            self.sync_repos(store);
        }

        let mut acts: Vec<Act> = Vec::new();

        // Keyboard shortcuts — skipped while the confirmation or the activity
        // modal is up, or a text field is focused.
        let activity_busy = crate::activity::lock(&self.activity).is_running();
        if self.confirm.is_none() && !activity_busy && !ui.ctx().egui_wants_keyboard_input() {
            ui.input(|i| {
                for (key, cmd) in [
                    (egui::Key::Num1, Command::Dedupe),
                    (egui::Key::Num2, Command::Purge),
                    (egui::Key::Num3, Command::EmptyDirs),
                    (egui::Key::Num4, Command::Organize),
                    (egui::Key::Num5, Command::Prune),
                ] {
                    if i.key_pressed(key) {
                        acts.push(Act::SetCommand(cmd));
                    }
                }
                if i.key_pressed(egui::Key::P) {
                    acts.push(Act::Preview);
                }
                if i.key_pressed(egui::Key::R) {
                    acts.push(Act::Ask);
                }
            });
        }

        // The whole tab scrolls as one page: ORGANIZE stacks a wizard per rule
        // and the preview table can be tall, so without this the lower rows —
        // and with enough rules the preview entirely — fall off the bottom of
        // the window with no way to reach them. The virtualised preview table
        // culls to the visible band via its clip rect, so nesting it here keeps
        // the single outer scrollbar cheap.
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(6.0);
                ui.label(
                    RichText::new("GROOMING")
                        .color(theme::tan())
                        .size(18.0)
                        .strong(),
                );
                crate::util::shortcut_bar(
                    ui,
                    "1 dedupe · 2 purge · 3 empty-dirs · 4 organize · 5 prune · P review · R run",
                );

                // Reading order is the workflow: WHAT (the tool), WITH WHICH
                // (the repository, plus the dupe pool for DEDUPE), HOW (the
                // filter or the rules), then RUN. Each section appears once the
                // one before it has an answer; after REVIEW or RUN they fold
                // into one line.
                if self.selection_collapsed {
                    self.selection_summary(ui);
                    if self.ready() {
                        self.action_bar(ui, &mut acts);
                    }
                } else {
                    self.command_bar(ui, &mut acts);
                    if self.command_chosen {
                        match self.command {
                            Command::Dedupe => self.dedupe_layout(ui, &mut acts),
                            Command::Purge => self.purge_layout(ui, &mut acts),
                            Command::EmptyDirs => self.empty_dirs_layout(ui, &mut acts),
                            Command::Organize => self.organize_layout(ui, &mut acts),
                            Command::Prune => self.prune_layout(ui, &mut acts),
                        }
                    }
                    if self.command_chosen && self.repo_picked() {
                        self.how_sections(ui, store, &mut acts);
                    }
                    if self.command_chosen && self.ready() {
                        self.action_bar(ui, &mut acts);
                    }
                }

                if let Some(err) = &self.error {
                    ui.colored_label(theme::red(), err);
                }
                if let Some(status) = &self.status {
                    ui.label(RichText::new(status).color(theme::tan()).size(13.0));
                }
                ui.separator();
                self.preview_panel(ui, &mut acts);
            });

        if let Some(prompt) = self.confirm.clone() {
            self.confirm_modal(ui, &prompt, &mut acts);
        }

        let ctx = ui.ctx().clone();
        for act in acts {
            self.apply(store, &ctx, act);
        }
    }

    fn command_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        crate::lcars::section_lcars(
            ui,
            "WHAT — DEDUPE, PURGE, EMPTY DIRS, ORGANIZE OR PRUNE",
            theme::orange(),
            |ui| {
                ui.horizontal(|ui| {
                    for cmd in [
                        Command::Dedupe,
                        Command::Purge,
                        Command::EmptyDirs,
                        Command::Organize,
                        Command::Prune,
                    ] {
                        let sel = self.command_chosen && self.command == cmd;
                        let (short, verbose) = cmd.tooltip();
                        if crate::lcars::toggle_button(ui, cmd.label(), sel, theme::red())
                            .explain(self.verbosity, short, verbose)
                            .clicked()
                        {
                            acts.push(Act::SetCommand(cmd));
                        }
                    }
                });
            },
        );
    }

    /// Whether WITH WHICH has an answer: the repository the command acts on.
    fn repo_picked(&self) -> bool {
        match self.command {
            Command::Dedupe => self.source.is_some(),
            _ => self.repo.is_some(),
        }
    }

    /// HOW: the filter (DEDUPE, PURGE) or the rules (ORGANIZE). EMPTY DIRS and
    /// PRUNE have nothing to set.
    fn how_sections(&mut self, ui: &mut egui::Ui, store: &Arc<Store>, acts: &mut Vec<Act>) {
        match self.command {
            Command::Dedupe => {
                let repo = self.source.clone();
                self.shared_filter(ui, store, repo.as_deref());
            }
            Command::Purge => {
                let repo = self.repo.clone();
                self.shared_filter(ui, store, repo.as_deref());
                if self.filter_string().is_none() {
                    ui.label(
                        RichText::new(
                            "PURGE needs at least one filter condition — with no filter, \
                             nothing matches and nothing can be deleted.",
                        )
                        .color(theme::tan())
                        .size(12.0),
                    );
                }
            }
            Command::Organize => self.rules_section(ui, store, acts),
            Command::EmptyDirs | Command::Prune => {}
        }
    }

    /// The one line the selection folds into after REVIEW or RUN, with
    /// CHANGE to unfold it.
    fn selection_summary(&mut self, ui: &mut egui::Ui) {
        let repo = match self.command {
            Command::Dedupe => format!(
                "from '{}' against {}",
                self.source.clone().unwrap_or_default(),
                self.pool.join(", ")
            ),
            _ => format!("in '{}'", self.repo.clone().unwrap_or_default()),
        };
        let how = match self.command {
            Command::Dedupe | Command::Purge => self
                .filter_string()
                .map(|f| format!(" · filter: {f}"))
                .unwrap_or_default(),
            Command::Organize => format!(" · {} rule(s)", self.organize_rules().len()),
            Command::EmptyDirs | Command::Prune => String::new(),
        };
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("{} {repo}{how}", self.command.label()))
                    .color(theme::tan())
                    .size(12.5),
            );
            if crate::lcars::toggle_button(ui, "CHANGE", false, theme::lilac())
                .explain(
                    self.verbosity,
                    "Change the tool or repository",
                    "Unfold the tool, repository and option sections to set up another \
                     grooming run. The board below stays until the next REVIEW.",
                )
                .clicked()
            {
                self.selection_collapsed = false;
            }
        });
    }

    fn dedupe_layout(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        crate::lcars::section_lcars(
            ui,
            "WITH WHICH — SOURCE & DUPE POOL",
            theme::lilac(),
            |ui| {
                // SOURCE: the repo duplicates are deleted from, orange when picked.
                let src = self.repos.clone();
                let mains = self.mains.clone();
                crate::repo_chip::chip_row(ui, "groom_source", "SOURCE", src.len(), |ui, i| {
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
                            "Pick the repo to delete duplicates from",
                            "Files in this repo whose content is also in any dupe-pool repo are \
                         deleted from here.",
                        )
                        .clicked()
                    {
                        acts.push(Act::PickSource(name.clone()));
                    }
                    self.locks.handle_badge(chip.lock, self.verbosity, name);
                    chip.outer
                });
                // DUPEPOOL: the repos to check the source against, lilac when on.
                let pool: Vec<String> = self
                    .repos
                    .iter()
                    .filter(|n| self.source.as_deref() != Some(n.as_str()))
                    .cloned()
                    .collect();
                ui.horizontal(|ui| {
                    ui.label(RichText::new("DUPEPOOL").color(theme::text()).size(12.0));
                    if crate::repo_chip::small_button(ui, "ALL", theme::lilac())
                        .explain(
                            self.verbosity,
                            "Add every eligible repo to the pool",
                            "A source file is deleted when its content exists in any pool repo.",
                        )
                        .clicked()
                    {
                        self.pool = pool.clone();
                    }
                    if crate::repo_chip::small_button(ui, "NONE", theme::lilac())
                        .explain(
                            self.verbosity,
                            "Clear the dupe pool",
                            "No pool repos selected.",
                        )
                        .clicked()
                    {
                        self.pool.clear();
                    }
                });
                let mains = self.mains.clone();
                crate::repo_chip::chip_row(ui, "groom_pool", "", pool.len(), |ui, i| {
                    let name = &pool[i];
                    let sel = self.pool.iter().any(|r| r == name);
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
                        "A source file is deleted when its content exists in any of these repos. \
                         The pool repos themselves are never modified.",
                    )
                    .clicked()
                {
                    acts.push(Act::TogglePool(name.clone()));
                }
                    self.locks.handle_badge(chip.lock, self.verbosity, name);
                    chip.outer
                });
            },
        );
    }

    fn purge_layout(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        self.single_repo_bar(ui, acts, "Delete matching files from this repo.");
    }

    fn empty_dirs_layout(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        self.single_repo_bar(
            ui,
            acts,
            "Remove empty directories under this repo's root — a backup group's main is \
             cleaned together with its sinks.",
        );
    }

    fn prune_layout(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        self.single_repo_bar(
            ui,
            acts,
            "Drop records of files already deleted from this repo, then shrink its index.",
        );
    }

    /// Draw the shared single FILTER wizard (DEDUPE/PURGE) backed by `count_repo`
    /// and fold its outcome into this view's status/error/preview.
    fn shared_filter(&mut self, ui: &mut egui::Ui, store: &Arc<Store>, count_repo: Option<&str>) {
        let outcome = self.filter.ui(ui, store, count_repo, self.verbosity);
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

    /// ORGANIZE: a repo picker and one RULES section holding the add/preset row
    /// plus every rule as a nested, collapsible section (each rule = the shared
    /// FILTER wizard + a path template with token chips and an inline delete).
    fn organize_layout(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        self.single_repo_bar(ui, acts, "Reorganize the files in this repo in place.");
    }

    /// ORGANIZE's HOW: one RULES section holding the add/preset row plus every
    /// rule as a nested, collapsible section.
    fn rules_section(&mut self, ui: &mut egui::Ui, store: &Arc<Store>, acts: &mut Vec<Act>) {
        let repo = self.repo.clone();

        crate::lcars::section_lcars(
            ui,
            "HOW — RULES THAT MATCH FILES & BUILD THEIR NEW PATHS",
            theme::lilac(),
            |ui| {
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .add(
                            egui::Button::new(RichText::new("+ RULE").color(theme::black()))
                                .fill(theme::amber()),
                        )
                        .explain(
                            self.verbosity,
                            "Add a rule",
                            "Add another filter → template rule. Rules are tried top to bottom; \
                         the first whose filter matches a file decides its new path.",
                        )
                        .clicked()
                    {
                        acts.push(Act::AddRule);
                    }
                    self.preset_row(ui, acts);
                });
                let rule_count = self.rules.len();
                for i in 0..rule_count {
                    self.rule_section(ui, store, repo.as_deref(), i, acts);
                }
            },
        );
    }

    /// One collapsible RULE section: filter wizard, template row (with the
    /// inline delete), and the token chips.
    fn rule_section(
        &mut self,
        ui: &mut egui::Ui,
        store: &Arc<Store>,
        repo: Option<&str>,
        i: usize,
        acts: &mut Vec<Act>,
    ) {
        let title = format!("RULE {} — MATCH & RENAME", i + 1);
        crate::lcars::section_lcars_collapsible(ui, &title, theme::blue(), true, |ui| {
            // The rule's filter (which files this rule applies to).
            let outcome = self.rules[i].filter.ui(ui, store, repo, self.verbosity);
            if outcome.changed {
                self.clear_preview();
            }
            if outcome.error.is_some() {
                self.error = outcome.error;
            }
            // The target path template, its inline delete, and the token chips.
            let template_id = egui::Id::new(("organize_template", i));
            ui.horizontal(|ui| {
                ui.label(RichText::new("TEMPLATE").color(theme::text()).size(12.0));
                let changed = ui
                    .add(
                        egui::TextEdit::singleline(&mut self.rules[i].template)
                            .id(template_id)
                            .desired_width(360.0)
                            .hint_text(DEFAULT_TEMPLATE),
                    )
                    .explain(
                        self.verbosity,
                        "New relative path template",
                        "The new relative path for matching files. Tokens in {…} are \
                         filled per file; `|` gives fallbacks and \"quoted\" text is a \
                         literal default, e.g. {year}/{o-stem}-{camera|\"nocam\"}.{o-ext}.",
                    )
                    .changed();
                if changed {
                    self.clear_preview();
                }
                if ui
                    .add(
                        egui::Button::new(RichText::new(icon::TRASH).color(theme::red()))
                            .fill(theme::panel()),
                    )
                    .explain(
                        self.verbosity,
                        "Delete this rule",
                        "Delete this organize rule. Files it would have matched fall \
                         through to later rules, or stay put if none match.",
                    )
                    .clicked()
                {
                    acts.push(Act::RemoveRule(i));
                }
            });
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("insert:").color(theme::lilac()).size(11.0));
                for (label, insert) in TEMPLATE_TOKENS {
                    if ui
                        .add(
                            egui::Button::new(RichText::new(*label).color(theme::blue()))
                                .fill(theme::panel()),
                        )
                        .clicked()
                    {
                        insert_at_cursor(
                            ui.ctx(),
                            template_id,
                            &mut self.rules[i].template,
                            insert,
                        );
                        self.clear_preview();
                    }
                }
            });
        });
    }

    /// The ORGANIZE saved-preset row: apply/forget pills (right-click to
    /// rename), plus a STORE PRESET pill for the current rule list.
    fn preset_row(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        ui.separator();
        ui.label(RichText::new("PRESETS").color(theme::text()).size(12.0));
        // Snapshot names first so the loop body is free to mutate `self`
        // (rename state) without fighting a borrow of `self.presets`.
        let presets: Vec<(usize, String)> = self
            .presets
            .iter()
            .enumerate()
            .map(|(i, p)| (i, p.name.clone()))
            .collect();
        for (i, name) in presets {
            if self.renaming_preset == Some(i) {
                let resp =
                    ui.add(egui::TextEdit::singleline(&mut self.rename_buf).desired_width(120.0));
                if self.focus_rename_pending {
                    resp.request_focus();
                    self.focus_rename_pending = false;
                }
                if resp.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    self.renaming_preset = None;
                } else if resp.lost_focus() {
                    acts.push(Act::CommitRenamePreset(i));
                }
                continue;
            }
            let resp = ui
                .add(
                    egui::Button::new(RichText::new(&name).color(theme::tan()))
                        .fill(theme::panel()),
                )
                .explain(
                    self.verbosity,
                    "Apply this preset",
                    "Replace the current rules with this saved preset's rules. Right-click \
                     to rename it.",
                );
            if resp.clicked() {
                acts.push(Act::ApplyPreset(i));
            }
            resp.context_menu(|ui| {
                if ui.button("Rename").clicked() {
                    self.renaming_preset = Some(i);
                    self.rename_buf = name.clone();
                    self.focus_rename_pending = true;
                    ui.close();
                }
            });
            if ui
                .add(egui::Button::new(RichText::new("×").color(theme::red())))
                .explain(
                    self.verbosity,
                    "Forget this preset",
                    "Delete this saved preset.",
                )
                .clicked()
            {
                acts.push(Act::RemovePreset(i));
            }
        }
        if !self.rules.is_empty()
            && ui
                .add(
                    egui::Button::new(RichText::new("STORE PRESET").color(theme::black()))
                        .fill(theme::amber()),
                )
                .explain(
                    self.verbosity,
                    "Store the current rules as a preset",
                    "Save the current rule list as a new preset, named \"Preset #n\" \
                     automatically, for one-click reuse on any repo. Right-click a preset \
                     afterwards to give it a better name.",
                )
                .clicked()
        {
            acts.push(Act::StorePreset);
        }
    }

    /// A single-repo picker used by PURGE and EMPTY DIRS (they act on one repo).
    fn single_repo_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>, hint: &str) {
        crate::lcars::section_lcars(
            ui,
            "WITH WHICH — THE REPOSITORY TO GROOM",
            theme::lilac(),
            |ui| {
                let repos = self.repos.clone();
                let mains = self.mains.clone();
                crate::repo_chip::chip_row(ui, "groom_repo", "", repos.len(), |ui, i| {
                    let name = &repos[i];
                    let sel = self.repo.as_deref() == Some(name.as_str());
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
                        .explain(self.verbosity, "Pick the repo to act on", hint)
                        .clicked()
                    {
                        acts.push(Act::PickRepo(name.clone()));
                    }
                    self.locks.handle_badge(chip.lock, self.verbosity, name);
                    chip.outer
                });
                ui.label(RichText::new(hint).color(theme::lilac()).size(11.0));
            },
        );
    }

    /// A single-line filter expression (mime / size / name with `*` wildcards).
    fn action_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        crate::lcars::section_lcars(
            ui,
            "RUN — REVIEW THE PLAN, THEN RUN IT",
            theme::amber(),
            |ui| {
                ui.horizontal(|ui| {
                    let ready = self.ready();
                    // EMPTY DIRS has no meaningful file preview (its count is
                    // only known after walking), so REVIEW is offered for the
                    // other tools.
                    if self.command != Command::EmptyDirs
                        && crate::lcars::action_button(ui, "REVIEW", ready, theme::blue())
                            .explain(
                                self.verbosity,
                                "Review what would change",
                                "Plan the run in the activity window and list the matching \
                                 files (up to a limit) and a total count on the board below, \
                                 without changing anything on disk.",
                            )
                            .clicked()
                    {
                        acts.push(Act::Preview);
                    }
                    // The session lock bars a run that would change the
                    // groomed repo's existing files; the blocked button says
                    // which padlock to click.
                    let lock_block = self.lock_block();
                    let resp = crate::lcars::action_button(
                        ui,
                        "RUN",
                        ready && lock_block.is_none(),
                        theme::red(),
                    );
                    let resp = if let Some(why) = &lock_block {
                        resp.on_hover_text(why.clone())
                    } else {
                        resp.explain(
                            self.verbosity,
                            "Run the command",
                            "After a confirmation, run the command in the activity window: \
                             it names each file as it goes, lists any problems, and ends \
                             with the result. CANCEL stops further work.",
                        )
                    };
                    if resp.clicked() {
                        acts.push(Act::Ask);
                    }
                });
            },
        );
    }

    fn preview_panel(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        if self.preview.is_empty() {
            ui.add_space(6.0);
            // EMPTY DIRS has no review board — its only action is RUN, so the
            // hint must not send the user hunting for a REVIEW button.
            let hint = if self.command == Command::EmptyDirs {
                "Pick a repo, then press RUN."
            } else {
                "Pick a repo and command, then press REVIEW."
            };
            ui.colored_label(theme::text(), hint);
            return;
        }
        let bodies = std::mem::take(&mut self.preview_bodies);
        // The review shows only the buttons the session locks allow: while the
        // groomed repo is locked, the per-row APPLY (a deletion/move) is
        // withheld — HIDE stays, and unlocking the padlock brings APPLY back.
        let lock_filtered: Vec<board::RowMeta>;
        let metas: &[board::RowMeta] = if self.lock_block().is_some() {
            lock_filtered = self
                .preview
                .iter()
                .map(|m| {
                    let mut m = m.clone();
                    m.cmds.retain(|c| *c != board::Cmd::Apply);
                    m
                })
                .collect();
            &lock_filtered
        } else {
            &self.preview
        };
        let action = board::board(
            ui,
            &mut self.board_state,
            metas,
            board::BoardView {
                left_role: "SOURCE",
                left_repo: "",
                left_is_main: false,
                left_path: &self.preview_source_header,
                right: self
                    .preview_target_header
                    .as_deref()
                    .map(|path| board::RightHeader {
                        role: "TARGET",
                        repo: "",
                        is_main: false,
                        path,
                        multi_repo: false,
                    }),
                totals: self.preview_totals,
                full_len: self.preview_total,
                hide_skips_run: true,
            },
            &mut self.thumbs,
            &mut |i| bodies.get(i).cloned().unwrap_or_default(),
        );
        self.preview_bodies = bodies;
        match action {
            Some(a) if a.cmd == board::Cmd::Apply => {
                if let Some(meta) = self.preview.get(a.row) {
                    acts.push(Act::ApplyRow(meta.key.clone()));
                }
            }
            Some(a) if a.cmd == board::Cmd::OpenRow => {
                // Every row opens: with its counterpart when the row has one
                // (DEDUPE), as the single file otherwise — never a no-op.
                if let Some(meta) = self.preview.get(a.row)
                    && let Some(l) = meta.left_paths.first()
                {
                    acts.push(Act::Inspect(l.clone(), meta.right_paths.first().cloned()));
                }
            }
            _ => {}
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

    fn confirm_modal(&mut self, ui: &mut egui::Ui, prompt: &str, acts: &mut Vec<Act>) {
        egui::Modal::new(Id::new("grooming-confirm")).show(&ui.ctx().clone(), |ui| {
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
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new("PROCEED").color(theme::ink_on(theme::red())),
                        )
                        .fill(theme::red()),
                    )
                    .clicked()
                {
                    acts.push(Act::Confirm);
                }
                if ui
                    .add(egui::Button::new(
                        RichText::new("CANCEL").color(theme::text()),
                    ))
                    .clicked()
                {
                    acts.push(Act::CancelConfirm);
                }
            });
        });
    }

    /// Whether the current command has everything it needs to preview/run.
    fn ready(&self) -> bool {
        match self.command {
            Command::Dedupe => self.source.is_some() && !self.pool.is_empty(),
            // PURGE without a filter would match every file; require one.
            Command::Purge => self.repo.is_some() && self.filter_string().is_some(),
            Command::EmptyDirs | Command::Prune => self.repo.is_some(),
            Command::Organize => self.repo.is_some() && !self.rules.is_empty(),
        }
    }

    /// Why the session locks bar this RUN, if they do. Every grooming command
    /// except PRUNE deletes or moves the groomed repo's existing files, so it
    /// needs that repo unlocked. PRUNE only drops index records of files that
    /// are already gone — the index is not the data the lock protects.
    fn lock_block(&self) -> Option<String> {
        let repo = match self.command {
            Command::Dedupe => self.source.as_deref()?,
            Command::Prune => return None,
            _ => self.repo.as_deref()?,
        };
        self.locks.read_only(repo).then(|| {
            format!(
                "{} changes existing files in '{repo}' — unlock it (its padlock) to run.",
                self.command.label()
            )
        })
    }

    fn filter_string(&self) -> Option<String> {
        self.filter.filter_string()
    }

    /// Save the current ORGANIZE rules as a new preset named `Preset #n` (the
    /// next unused number), then persist to disk.
    fn store_preset(&mut self, store: &Store) {
        if self.rules.is_empty() {
            return;
        }
        let rules: Vec<SavedRule> = self
            .rules
            .iter()
            .map(|r| SavedRule {
                filter: r.filter.filter_string().unwrap_or_default(),
                template: r.template.clone(),
            })
            .collect();
        let name = self.next_preset_name();
        self.presets.push(OrganizePreset {
            name: name.clone(),
            rules,
        });
        self.save_presets(store);
        self.status = Some(format!("Saved organize preset '{name}'."));
    }

    /// The next unused `Preset #n` name, based on what's already saved.
    fn next_preset_name(&self) -> String {
        let mut n = self.presets.len() + 1;
        loop {
            let candidate = format!("Preset #{n}");
            if !self.presets.iter().any(|p| p.name == candidate) {
                return candidate;
            }
            n += 1;
        }
    }

    /// Persist the preset list as JSON in the config dir (best effort).
    fn save_presets(&mut self, store: &Store) {
        let path = store.config_dir().join(ORGANIZE_PRESETS_FILE);
        match serde_json::to_vec_pretty(&self.presets) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, json) {
                    self.error = Some(format!("Could not save organize presets: {e}"));
                }
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    /// Load the persisted presets once (a corrupt/missing file → empty list).
    fn load_presets(&mut self, store: &Store) {
        let path = store.config_dir().join(ORGANIZE_PRESETS_FILE);
        self.presets = std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        self.presets_loaded = true;
    }

    fn apply(&mut self, store: &Arc<Store>, ctx: &egui::Context, act: Act) {
        match act {
            Act::Inspect(left_rel, right_rel) => {
                // Left is the file the command acts on, in the repo the
                // current command works with (DEDUPE's source, everyone
                // else's single repo); right is the copy that makes a DEDUPE
                // row redundant, held by one of the pool repos — whichever
                // actually has that path. With a counterpart the viewer opens
                // as the comparison; without one (PURGE/PRUNE, an ORGANIZE
                // move of the same file) the single file opens alone.
                let left_repo = match self.command {
                    Command::Dedupe => self.source.clone(),
                    _ => self.repo.clone(),
                };
                let Some(left_repo) = left_repo else {
                    return;
                };
                let side = |repo: &str, rel: &str| {
                    let meta = store.get_repo(repo).ok()?;
                    let entry = store.get_file_entry(repo, rel).ok().flatten()?;
                    let abs = std::path::PathBuf::from(&meta.abs_path).join(rel);
                    Some(crate::compare_view::DiffSide {
                        repo: repo.to_string(),
                        rel_path: rel.to_string(),
                        facts: crate::media_cell::FileFacts::from_entry(&entry, abs),
                        read_only: self.locks.read_only(repo),
                    })
                };
                // Only a DEDUPE row's right path names a second file (the pool
                // copy). An ORGANIZE row's right path is the same file's
                // future home — there is nothing else to compare against.
                let counterpart_rel = match self.command {
                    Command::Dedupe => right_rel,
                    _ => None,
                };
                let has_counterpart = counterpart_rel.is_some();
                let right = counterpart_rel
                    .and_then(|rel| self.pool.iter().find_map(|repo| side(repo, &rel)));
                match (side(&left_repo, &left_rel), right) {
                    (Some(l), Some(r)) => {
                        self.inspect = Some(crate::compare_view::DiffCompare::new(l, r));
                    }
                    // A row that promises a counterpart must not quietly open
                    // one file — say what went wrong instead.
                    (Some(_), None) if has_counterpart => {
                        self.error =
                            Some("Could not read both copies to compare them.".to_string());
                    }
                    (Some(only), None) => {
                        self.inspect = Some(crate::compare_view::DiffCompare::inspect(only));
                    }
                    (None, _) => {
                        self.error = Some("Could not read that row's file.".to_string());
                    }
                }
            }
            Act::SetCommand(cmd) => {
                self.command = cmd;
                self.command_chosen = true;
                self.clear_preview();
            }
            Act::PickSource(name) => {
                self.pool.retain(|r| r != &name);
                self.source = Some(name);
                self.clear_preview();
            }
            Act::TogglePool(name) => {
                if let Some(pos) = self.pool.iter().position(|r| r == &name) {
                    self.pool.remove(pos);
                } else {
                    self.pool.push(name);
                }
                self.clear_preview();
            }
            Act::PickRepo(name) => {
                self.repo = Some(name);
                self.clear_preview();
            }
            Act::AddRule => {
                self.rules.push(RuleUi::new());
                self.clear_preview();
            }
            Act::RemoveRule(i) => {
                if i < self.rules.len() {
                    self.rules.remove(i);
                }
                self.clear_preview();
            }
            Act::StorePreset => self.store_preset(store),
            Act::ApplyPreset(i) => {
                if let Some(preset) = self.presets.get(i) {
                    self.rules = preset
                        .rules
                        .iter()
                        .map(|r| {
                            let mut filter = FilterBuilder::new();
                            filter.set_expression(&r.filter);
                            RuleUi {
                                filter,
                                template: r.template.clone(),
                            }
                        })
                        .collect();
                    if self.rules.is_empty() {
                        self.rules.push(RuleUi::new());
                    }
                    self.clear_preview();
                }
            }
            Act::RemovePreset(i) => {
                if i < self.presets.len() {
                    self.presets.remove(i);
                    self.save_presets(store);
                }
            }
            Act::CommitRenamePreset(i) => {
                self.renaming_preset = None;
                let new_name = self.rename_buf.trim().to_string();
                if !new_name.is_empty()
                    && let Some(preset) = self.presets.get_mut(i)
                {
                    preset.name = new_name;
                    self.save_presets(store);
                }
            }
            Act::Preview => self.run_preview(store, ctx, false),
            Act::Ask => {
                // EMPTY DIRS has no plan to count; every other tool plans
                // behind the activity modal first, and the confirmation is
                // raised once the plan lands with its real count.
                if self.command == Command::EmptyDirs {
                    if let Some(prompt) = self.build_prompt() {
                        self.confirm = Some(prompt);
                    }
                } else {
                    self.run_preview(store, ctx, true);
                }
            }
            Act::CancelConfirm => self.confirm = None,
            Act::Confirm => {
                self.confirm = None;
                self.start(store, ctx, None);
            }
            Act::ApplyRow(key) => {
                // A preview built before the repo was re-locked could still
                // carry APPLY buttons for a frame — never act past the lock.
                if self.lock_block().is_none() {
                    self.start(store, ctx, Some(key));
                }
            }
        }
    }

    fn clear_preview(&mut self) {
        self.preview.clear();
        self.preview_bodies.clear();
        self.preview_totals = [0; 4];
        self.preview_source_header.clear();
        self.preview_target_header = None;
        self.preview_total = 0;
        // Hidden rows are keyed to the preview they were hidden in.
        self.board_state.hidden.clear();
    }

    /// Plan the current command behind the activity modal. `confirm` raises
    /// the RUN confirmation (with the real count) once the plan lands.
    fn run_preview(&mut self, store: &Arc<Store>, ctx: &egui::Context, confirm: bool) {
        let filter = self.filter_string();
        let (spec, repos) = match self.command {
            Command::Dedupe => {
                let Some(source) = self.source.clone() else {
                    return;
                };
                let mut repos = vec![source.clone()];
                repos.extend(self.pool.iter().cloned());
                (
                    PreviewSpec::Dedupe {
                        source,
                        pool: self.pool.clone(),
                        filter,
                    },
                    repos,
                )
            }
            Command::Purge => {
                let Some(repo) = self.repo.clone() else {
                    return;
                };
                (
                    PreviewSpec::Purge {
                        repo: repo.clone(),
                        filter,
                    },
                    vec![repo],
                )
            }
            Command::Prune => {
                let Some(repo) = self.repo.clone() else {
                    return;
                };
                (PreviewSpec::Prune { repo: repo.clone() }, vec![repo])
            }
            Command::Organize => {
                let Some(repo) = self.repo.clone() else {
                    return;
                };
                (
                    PreviewSpec::Organize {
                        repo: repo.clone(),
                        rules: self.organize_rules(),
                    },
                    vec![repo],
                )
            }
            // EMPTY DIRS has no file preview.
            Command::EmptyDirs => return,
        };
        let title = format!("REVIEW {}", self.command.label());
        let store = Arc::clone(store);
        let tx = self.tx.clone();
        let started = crate::activity::lock(&self.activity).start_quiet(
            ctx,
            crate::activity::Spec {
                title: title.clone(),
                repos,
            },
            move |progress, cancel| {
                let result = build_preview(&store, spec, progress, cancel);
                if cancel.is_cancelled() {
                    let _ = tx.send(Msg::PreviewCancelled);
                    return Ok(());
                }
                let outcome = result.as_ref().map(|_| ()).map_err(Clone::clone);
                let _ = tx.send(Msg::Preview { result, confirm });
                outcome
            },
        );
        match started {
            Ok(()) => {
                self.previewing = true;
                self.selection_collapsed = true;
                self.status = Some(format!("{}…", title.to_lowercase()));
            }
            Err(busy) => crate::activity::lock(&self.activity)
                .card(ctx, crate::activity::Notification::refused("REVIEW", &busy)),
        }
    }

    /// Fold a finished preview into the board; with `confirm`, raise the RUN
    /// confirmation now that the count is real.
    fn apply_preview(&mut self, result: Result<PreviewData, String>, confirm: bool) {
        match result {
            Ok(data) => {
                self.preview = data.metas;
                self.preview_bodies = data.bodies;
                self.preview_total = data.total;
                self.preview_totals = data.totals;
                self.preview_source_header = data.source_header;
                self.preview_target_header = data.target_header;
                self.status = Some(data.status);
                self.error = None;
                if confirm && let Some(prompt) = self.build_prompt() {
                    self.confirm = Some(prompt);
                }
            }
            Err(e) => self.error = Some(e),
        }
    }

    /// Build the current ORGANIZE rules as core rules (filter expression +
    /// template), skipping rules with a blank template.
    fn organize_rules(&self) -> Vec<OrganizeRule> {
        self.rules
            .iter()
            .filter(|r| !r.template.trim().is_empty())
            .map(|r| OrganizeRule {
                filter: r.filter.filter_string(),
                template: r.template.trim().to_string(),
            })
            .collect()
    }

    /// The RUN confirmation for the current command, from the preview's
    /// count (EMPTY DIRS has none). Hidden rows are named as skipped.
    fn build_prompt(&self) -> Option<String> {
        let mut prompt = match self.command {
            Command::Dedupe => {
                let source = self.source.as_ref()?;
                format!(
                    "Delete {} file(s) from '{source}' whose content is in the dupe pool? \
                     This cannot be undone.",
                    self.preview_total
                )
            }
            Command::Purge => {
                let repo = self.repo.as_ref()?;
                format!(
                    "Delete all {} file(s) matching the filter from '{repo}'? \
                     This cannot be undone.",
                    self.preview_total
                )
            }
            Command::EmptyDirs => {
                let repo = self.repo.as_ref()?;
                format!("Remove all empty directories under '{repo}'?")
            }
            Command::Prune => {
                let repo = self.repo.as_ref()?;
                format!(
                    "Permanently drop {} record(s) of deleted files from '{repo}' and \
                     compact its index? This cannot be undone.",
                    self.preview_total
                )
            }
            Command::Organize => {
                let repo = self.repo.as_ref()?;
                format!(
                    "Move {} file(s) into their new layout in '{repo}'? Files move within \
                     the repo; nothing is overwritten (collisions are renamed).",
                    self.preview_total
                )
            }
        };
        let hidden = self.board_state.hidden.len();
        if hidden > 0 {
            prompt.push_str(&format!(" {hidden} hidden row(s) will be skipped."));
        }
        Some(prompt)
    }

    /// Start the selected command on a worker thread. `only` restricts the run
    /// to a single review row (the APPLY button); `None` runs the whole batch
    /// minus any rejected rows.
    fn start(&mut self, store: &Arc<Store>, ctx: &egui::Context, only: Option<String>) {
        let command = self.command;
        let filter = self.filter_string();
        let source = self.source.clone();
        let pool = self.pool.clone();
        let repo = self.repo.clone();
        let rules = self.organize_rules();
        let hidden: std::collections::HashSet<String> = self.board_state.hidden.clone();
        let store = Arc::clone(store);
        // The repository the run changes: what the log lines name.
        let Some(acted_on) = (match command {
            Command::Dedupe => source.clone(),
            _ => repo.clone(),
        }) else {
            return;
        };
        let title = format!("{} '{acted_on}'", command.label());
        let mut repos = vec![acted_on.clone()];
        if command == Command::Dedupe {
            repos.extend(pool.iter().cloned());
        }
        let only_set: Option<std::collections::HashSet<String>> =
            only.clone().map(|k| std::collections::HashSet::from([k]));
        let card_repo = acted_on.clone();

        let work = move |progress: &crate::activity::ActivityProgress,
                         cancel: &CancellationToken|
              -> OpResult {
            let run_progress = crate::activity::RunProgress {
                activity: progress.clone(),
                repo: acted_on.clone(),
            };
            let run = DiffRun::new(&run_progress, cancel)
                .with_selection((!hidden.is_empty()).then_some(&hidden), only_set.as_ref());
            match command {
                Command::Dedupe => {
                    let Some(source) = source else {
                        return OpResult::Error("no source repository".into());
                    };
                    let ref_slice: Vec<&str> = pool.iter().map(String::as_str).collect();
                    match diff_delete(&store, &source, &ref_slice, filter.as_deref(), &run) {
                        Ok(s) => OpResult::Deleted {
                            deleted: s.deleted,
                            cancelled: s.cancelled,
                        },
                        Err(e) => OpResult::Error(e.to_string()),
                    }
                }
                Command::Purge => {
                    let Some(repo) = repo else {
                        return OpResult::Error("no repository".into());
                    };
                    match delete_by_filter(&store, &repo, filter.as_deref(), &run) {
                        Ok(s) => OpResult::Deleted {
                            deleted: s.deleted,
                            cancelled: s.cancelled,
                        },
                        Err(e) => OpResult::Error(e.to_string()),
                    }
                }
                Command::EmptyDirs => {
                    let Some(repo) = repo else {
                        return OpResult::Error("no repository".into());
                    };
                    // A group main's sinks mirror its tree, so the same stale
                    // directory skeletons accumulate there — sweep them in the
                    // same run. Empty directories hold no data, so sink locks
                    // are not in play; an unreachable sink walks as empty and
                    // is a harmless no-op.
                    let mut repos = vec![repo.clone()];
                    if let Ok(groups) = store.list_sync_groups()
                        && let Some((_, g)) = groups.into_iter().find(|(_, g)| g.main == repo)
                    {
                        repos.extend(g.sinks.into_iter().map(|s| s.repo));
                    }
                    let mut removed = 0u64;
                    let mut errors: Vec<String> = Vec::new();
                    for (i, r) in repos.iter().enumerate() {
                        progress.phase(
                            format!("sweeping '{r}'"),
                            i as u64,
                            Some(repos.len() as u64),
                        );
                        match delete_empty_dirs(&store, r) {
                            Ok(n) => {
                                removed += n;
                                if n > 0 {
                                    progress.record(&crate::activity::Notification::changed(
                                        "Removed empty directories",
                                        r,
                                        &format!(
                                            "{n} director{}",
                                            if n == 1 { "y" } else { "ies" }
                                        ),
                                    ));
                                }
                            }
                            Err(e) => {
                                let e = e.to_string();
                                progress.problem(format!("{r}: {e}"));
                                errors.push(format!("{r}: {e}"));
                            }
                        }
                    }
                    OpResult::EmptyDirs {
                        removed,
                        repos: repos.len() as u64,
                        errors,
                    }
                }
                Command::Organize => {
                    let Some(repo) = repo else {
                        return OpResult::Error("no repository".into());
                    };
                    match organize_apply(&store, &repo, &rules, &run) {
                        Ok(s) => OpResult::Organized {
                            moved: s.moved,
                            skipped: s.skipped,
                            errors: s.errors,
                            cancelled: s.cancelled,
                        },
                        Err(e) => OpResult::Error(e.to_string()),
                    }
                }
                Command::Prune => {
                    let Some(repo) = repo else {
                        return OpResult::Error("no repository".into());
                    };
                    progress.phase("dropping records of deleted files", 0, None);
                    match prune(&store, &repo, &run) {
                        Ok(s) => OpResult::Pruned {
                            pruned: s.pruned,
                            compacted: s.compacted,
                            cancelled: s.cancelled,
                        },
                        Err(e) => OpResult::Error(e.to_string()),
                    }
                }
            }
        };

        match only {
            // One review row: a row action with a card, and the board
            // refreshes when it lands.
            Some(key) => {
                let what = key
                    .split_once(':')
                    .map(|(_, rel)| rel.to_string())
                    .unwrap_or(key);
                let repo_name = card_repo;
                let mut activity = crate::activity::lock(&self.activity);
                if let Err(busy) = activity.begin_row_action() {
                    activity.card(ctx, crate::activity::Notification::refused("APPLY", &busy));
                    return;
                }
                let progress = activity.progress_handle(ctx);
                drop(activity);
                self.running = true;
                self.row_action = true;
                self.pending_refresh = true;
                self.status = Some("applying…".to_string());
                let tx = self.tx.clone();
                std::thread::spawn(move || {
                    let result = work(&progress, &CancellationToken::new());
                    let did = match command {
                        Command::Organize => "Moved",
                        Command::Prune => "Pruned",
                        _ => "Deleted",
                    };
                    let note = match &result {
                        OpResult::Deleted { deleted: n, .. } if *n > 0 => {
                            crate::activity::Notification::changed(did, &repo_name, &what)
                        }
                        OpResult::Organized { moved: n, .. } if *n > 0 => {
                            crate::activity::Notification::changed(did, &repo_name, &what)
                        }
                        OpResult::Pruned { .. } => {
                            crate::activity::Notification::noted(did, &repo_name, &what)
                        }
                        OpResult::Error(e) => {
                            crate::activity::Notification::failed(did, &repo_name, &what, e)
                        }
                        _ => crate::activity::Notification::failed(
                            did,
                            &repo_name,
                            &what,
                            "nothing was changed",
                        ),
                    };
                    let _ = tx.send(Msg::Done(OpResult::Applied { note }));
                    progress.repaint();
                });
            }
            None => {
                let tx = self.tx.clone();
                let started = crate::activity::lock(&self.activity).start(
                    ctx,
                    crate::activity::Spec {
                        title: title.clone(),
                        repos,
                    },
                    move |progress, cancel| {
                        let result = work(progress, cancel);
                        let report = result.report();
                        let _ = tx.send(Msg::Done(result));
                        report
                    },
                );
                match started {
                    Ok(()) => {
                        self.running = true;
                        self.row_action = false;
                        self.selection_collapsed = true;
                        self.clear_preview();
                        self.status = Some(format!("{}…", title.to_lowercase()));
                    }
                    Err(busy) => crate::activity::lock(&self.activity)
                        .card(ctx, crate::activity::Notification::refused("RUN", &busy)),
                }
            }
        }
    }

    fn drain(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::PreviewCancelled => {
                    self.previewing = false;
                    self.status = Some("review cancelled".to_string());
                }
                Msg::Preview { result, confirm } => {
                    self.previewing = false;
                    self.apply_preview(result, confirm);
                }
                Msg::Done(result) => {
                    self.running = false;
                    log::info!("grooming finished: {result:?}");
                    if self.row_action {
                        self.row_action = false;
                        crate::activity::lock(&self.activity).end_row_action();
                    }
                    match result {
                        OpResult::Applied { note } => {
                            self.status = Some(note.headline());
                            self.error = None;
                            crate::activity::lock(&self.activity).card(ctx, note);
                        }
                        OpResult::Error(e) => self.error = Some(e),
                        other => {
                            self.status = Some(other.report().headline());
                            self.error = None;
                        }
                    }
                }
            }
        }
    }

    pub fn sync_repos(&mut self, store: &Store) {
        if !self.presets_loaded {
            self.load_presets(store);
        }
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
                if let Some(r) = &self.repo
                    && !self.repos.contains(r)
                {
                    self.repo = None;
                }
                self.pool.retain(|r| self.repos.contains(r));
                self.loaded = true;
                self.error = None;
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }
}

/// Insert `text` into `template` at the caret of the `TextEdit` identified by
/// `id`, leaving the caret just after the insertion so consecutive chip clicks
/// build the template left to right. Appends when the field has no cursor
/// state yet (never focused).
/// The planned rows before their board bodies are built: each file with
/// its counterpart (the surviving copy, or the new path), the true total
/// past the preview cap, and the header of the right side.
struct Planned {
    /// The single repo these files live in, for their thumbnails + facts.
    facts_repo: String,
    paths: Vec<(String, Option<String>)>,
    total: usize,
    target_header: Option<String>,
    /// An ORGANIZE relocation: the counterpart is the same file's new path.
    organize: bool,
}

/// Plan a preview off the UI thread: the rows, their bodies and the counts.
fn build_preview(
    store: &Store,
    spec: PreviewSpec,
    progress: &crate::activity::ActivityProgress,
    cancel: &CancellationToken,
) -> Result<PreviewData, String> {
    let report = |p: PlanProgress| crate::activity::plan_phase(progress, p);
    let Planned {
        facts_repo,
        paths,
        total,
        target_header,
        organize,
    } = match spec {
        PreviewSpec::Dedupe {
            source,
            pool,
            filter,
        } => {
            let ref_slice: Vec<&str> = pool.iter().map(String::as_str).collect();
            // Contents accepted in the source are allowed to exist there:
            // the run skips them, so the preview must not promise them.
            let accepted = crate::util::or_log_default(
                store.accepted_paths(&source),
                "accepted contents of the source",
            );
            let items = diff_print_reporting(
                store,
                &source,
                &ref_slice,
                filter.as_deref(),
                &report,
                cancel,
            )
            .map_err(|e| e.to_string())?;
            // Keep the reference path alongside each doomed file: it is *why*
            // the file is redundant, and showing it is the difference between
            // "trust the plan" and seeing the copy that will survive.
            // `DeletedInReference` has no live counterpart — the reference
            // knows the content but no longer holds it — so it stays one-sided.
            let matched: Vec<(String, Option<String>)> = items
                .into_iter()
                .filter(|item| match item {
                    dedup_core::diff::DiffItem::Equal { rel_path, .. }
                    | dedup_core::diff::DiffItem::DeletedInReference { rel_path } => {
                        !accepted.contains(rel_path)
                    }
                    dedup_core::diff::DiffItem::New { .. } => true,
                })
                .filter_map(|item| match item {
                    dedup_core::diff::DiffItem::Equal {
                        rel_path,
                        reference_path,
                    } => Some((
                        rel_path,
                        (!reference_path.is_empty()).then_some(reference_path),
                    )),
                    dedup_core::diff::DiffItem::DeletedInReference { rel_path } => {
                        Some((rel_path, None))
                    }
                    dedup_core::diff::DiffItem::New { .. } => None,
                })
                .collect();
            let total = matched.len();
            let target = (!pool.is_empty()).then(|| pool.join(", "));
            Planned {
                facts_repo: source,
                paths: matched.into_iter().take(PREVIEW_CAP).collect(),
                total,
                target_header: target,
                organize: false,
            }
        }
        PreviewSpec::Purge { repo, filter } => {
            progress.phase(format!("reading '{repo}'"), 0, None);
            let (paths, total) = preview_by_filter(store, &repo, filter.as_deref(), PREVIEW_CAP)
                .map_err(|e| e.to_string())?;
            Planned {
                facts_repo: repo,
                paths: one_sided(paths),
                total,
                target_header: None,
                organize: false,
            }
        }
        PreviewSpec::Prune { repo } => {
            progress.phase(format!("reading '{repo}'"), 0, None);
            let (paths, total) =
                preview_prune(store, &repo, PREVIEW_CAP).map_err(|e| e.to_string())?;
            Planned {
                facts_repo: repo,
                paths: one_sided(paths),
                total,
                target_header: None,
                organize: false,
            }
        }
        PreviewSpec::Organize { repo, rules } => {
            progress.phase(format!("reading '{repo}'"), 0, None);
            let moves = plan_organize(store, &repo, &rules).map_err(|e| e.to_string())?;
            let total = moves.len();
            let paths = moves
                .into_iter()
                .take(PREVIEW_CAP)
                .map(|(from, to)| (from, Some(to)))
                .collect();
            Planned {
                facts_repo: repo,
                paths,
                total,
                target_header: None,
                organize: true,
            }
        }
    };
    if cancel.is_cancelled() {
        return Err("cancelled".to_string());
    }
    progress.phase("building the board", 0, None);
    let source_header = GroomingView::repo_header(store, &facts_repo);
    let (db, base) = open_facts(store, &facts_repo);
    if organize {
        // ORGANIZE relocates within one repo: the old path is removed and the
        // new path added — same repo on both sides. The file is still at its
        // old path until the move runs, so its facts come from the removed
        // (source) side.
        let (metas, bodies) = paths
            .into_iter()
            .map(|(from, to)| {
                let facts = facts_for(db.as_deref(), base.as_deref(), &from);
                let meta = board::RowMeta {
                    key: dedup_core::diff::source_key(&from),
                    left_status: board::Status::WillDelete,
                    right_status: board::Status::OnlyHere,
                    left_size: facts.as_ref().map(|f| f.size).unwrap_or(0),
                    left_modified: facts.as_ref().map(|f| f.modified_ms).unwrap_or(0),
                    right_size: 0,
                    right_modified: 0,
                    left_paths: vec![from],
                    right_paths: to.into_iter().collect(),
                    unchanged: false,
                    cmds: vec![board::Cmd::Apply, board::Cmd::Hide],
                };
                let body = board::RowBody {
                    left: board::SideBody {
                        facts: facts.clone(),
                        overlay: Some(crate::media_cell::CellOverlay::WillDelete),
                        ..Default::default()
                    },
                    // Golden rule: the arriving side shows the file that will
                    // be there — the same file, new path.
                    right: board::SideBody {
                        facts,
                        overlay: Some(crate::media_cell::CellOverlay::New),
                        ..Default::default()
                    },
                };
                (meta, body)
            })
            .unzip();
        return Ok(PreviewData {
            metas,
            bodies,
            total,
            // A relocation both removes the old path and adds the new one.
            totals: [total, total, 0, 0],
            source_header: source_header.clone(),
            target_header: Some(source_header),
            status: format!("{total} file(s) would move."),
        });
    }
    // PURGE / PRUNE remove files from one repo, so the right side is absent.
    // DEDUPE also removes from one repo, but it removes them *because* the
    // pool already holds the content — so the pool is named on the right and
    // each row shows the copy that survives.
    let (metas, bodies) = paths
        .into_iter()
        .map(|(from, reference)| {
            let facts = facts_for(db.as_deref(), base.as_deref(), &from);
            let right_status = if reference.is_some() {
                board::Status::Same
            } else {
                board::Status::Absent
            };
            let meta = board::RowMeta {
                key: dedup_core::diff::source_key(&from),
                left_status: board::Status::WillDelete,
                right_status,
                left_size: facts.as_ref().map(|f| f.size).unwrap_or(0),
                left_modified: facts.as_ref().map(|f| f.modified_ms).unwrap_or(0),
                // The counterpart is the same content by definition, so it
                // carries the same size.
                right_size: reference
                    .as_ref()
                    .map(|_| facts.as_ref().map(|f| f.size).unwrap_or(0))
                    .unwrap_or(0),
                right_modified: 0,
                left_paths: vec![from],
                right_paths: reference.iter().cloned().collect(),
                unchanged: false,
                // No COMPARE command: clicking the row opens the shared
                // viewer, so a row-level command would be a second door to the
                // same place.
                cmds: vec![board::Cmd::Apply, board::Cmd::Hide],
            };
            let body = board::RowBody {
                left: board::SideBody {
                    facts,
                    // Its own fate, painted on its own preview.
                    overlay: Some(crate::media_cell::CellOverlay::WillDelete),
                    ..Default::default()
                },
                right: board::SideBody::default(),
            };
            (meta, body)
        })
        .unzip();
    Ok(PreviewData {
        metas,
        bodies,
        total,
        totals: [total, 0, 0, 0],
        source_header,
        target_header,
        status: format!("{total} file(s) match."),
    })
}

fn one_sided(paths: Vec<String>) -> Vec<(String, Option<String>)> {
    paths.into_iter().map(|p| (p, None)).collect()
}

fn insert_at_cursor(ctx: &egui::Context, id: egui::Id, template: &mut String, text: &str) {
    let Some(mut state) = egui::TextEdit::load_state(ctx, id) else {
        template.push_str(text);
        return;
    };
    let chars = template.chars().count();
    let at = state
        .cursor
        .char_range()
        .map(|r| r.primary.index.0.min(chars))
        .unwrap_or(chars);
    let byte = template
        .char_indices()
        .nth(at)
        .map(|(b, _)| b)
        .unwrap_or(template.len());
    template.insert_str(byte, text);
    let after = egui::text::CCursor::new(at + text.chars().count());
    state
        .cursor
        .set_char_range(Some(egui::text::CCursorRange::one(after)));
    state.store(ctx, id);
}

#[cfg(test)]
mod ui_tests {
    use super::*;
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;

    fn sample_store() -> (tempfile::TempDir, Arc<Store>) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open_at(tmp.path().join("cfg")).unwrap();
        for name in ["a", "b"] {
            let dir = tmp.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            store.create_repo(name, &dir.to_string_lossy()).unwrap();
        }
        (tmp, Arc::new(store))
    }

    fn grooming_harness(store: Arc<Store>, command: Command) -> Harness<'static, GroomingView> {
        let mut view = GroomingView::new();
        view.command_chosen = true;
        view.loaded = true;
        view.repos = vec!["a".to_string(), "b".to_string()];
        view.command = command;
        // WITH WHICH answered, so the command's HOW section is on screen.
        view.source = Some("a".to_string());
        view.repo = Some("a".to_string());

        let store_ui = Arc::clone(&store);
        let mut init = false;
        // Tall enough that a seeded preview has room for several rows: the
        // board virtualises to the visible band, so a short window legitimately
        // draws only the first row.
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1120.0, 1000.0))
            .build_ui_state(
                move |ui, view: &mut GroomingView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    view.show(ui, &store_ui, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();
        harness
    }

    /// A one-sided review row opens the single-file viewer on click: a PURGE
    /// row has no counterpart at all, and an ORGANIZE row's right path is the
    /// same file's future home — neither may be a dead click or an error.
    #[test]
    fn one_sided_rows_open_the_single_file_viewer() {
        let (tmp, store) = sample_store();
        std::fs::write(tmp.path().join("a").join("junk.txt"), b"junk").unwrap();
        dedup_core::update::update_repo(
            &store,
            "a",
            1,
            &dedup_core::update::NoProgress,
            &CancellationToken::new(),
        )
        .unwrap();
        let mut h = grooming_harness(Arc::clone(&store), Command::Purge);
        h.state_mut().repo = Some("a".to_string());
        h.state_mut().apply(
            &store,
            &egui::Context::default(),
            Act::Inspect("junk.txt".to_string(), None),
        );
        assert!(
            h.state().inspect.is_some(),
            "a PURGE row opens the file alone: {:?}",
            h.state().error
        );
        assert!(h.state().error.is_none());

        h.state_mut().inspect = None;
        h.state_mut().command = Command::Organize;
        h.state_mut().apply(
            &store,
            &egui::Context::default(),
            Act::Inspect("junk.txt".to_string(), Some("2026/junk.txt".to_string())),
        );
        assert!(
            h.state().inspect.is_some(),
            "an ORGANIZE row opens the file alone: {:?}",
            h.state().error
        );
        assert!(h.state().error.is_none());
    }

    /// EMPTY DIRS on a backup group's main sweeps its sinks in the same run —
    /// sinks are not offered as groomable repos, so this is the only way their
    /// stale directory skeletons get cleaned.
    #[test]
    fn empty_dirs_sweeps_a_group_mains_sinks_too() {
        let (tmp, store) = sample_store();
        std::fs::create_dir_all(tmp.path().join("a/old/empty")).unwrap();
        std::fs::create_dir_all(tmp.path().join("b/stale/nested")).unwrap();
        store.create_sync_group("a", "a").unwrap();
        store
            .add_sync_sink("a", "b", dedup_core::store::SyncMode::AddOnly)
            .unwrap();
        let mut h = grooming_harness(Arc::clone(&store), Command::EmptyDirs);
        h.state_mut().repo = Some("a".to_string());
        h.state_mut()
            .apply(&store, &egui::Context::default(), Act::Confirm);
        for _ in 0..200 {
            h.step();
            if !h.state().running && h.state().status.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            !tmp.path().join("a/old").exists(),
            "the main's empty tree is gone"
        );
        assert!(
            !tmp.path().join("b/stale").exists(),
            "the sink's empty tree is gone in the same run: {:?}",
            h.state().status
        );
    }

    /// A DEDUPE preview leaves out source files whose content is accepted in
    /// the source — the run skips them, so the plan must not promise them.
    #[test]
    fn dedupe_preview_skips_accepted_source_contents() {
        let (tmp, store) = sample_store();
        let entry = |hash: u8| dedup_core::store::FileEntry {
            size: 10,
            hash: [hash; 32],
            modified_ms: 0,
            missing: false,
            mime: None,
            img_fingerprint: None,
            video_hash: None,
            pdf_hash: None,
            audio: None,
            img_size: None,
            origin: None,
            exif: None,
        };
        store
            .update_file_entry("a", "cover.jpg", &entry(1))
            .unwrap();
        store.update_file_entry("a", "dupe.txt", &entry(2)).unwrap();
        store
            .update_file_entry("b", "cover.jpg", &entry(1))
            .unwrap();
        store.update_file_entry("b", "keep.txt", &entry(2)).unwrap();
        store.accept_content("a", 10, &[1u8; 32]).unwrap();

        let mut h = grooming_harness(Arc::clone(&store), Command::Dedupe);
        h.state_mut().source = Some("a".to_string());
        h.state_mut().pool = vec!["b".to_string()];
        let ctx = h.ctx.clone();
        h.state_mut().run_preview(&store, &ctx, false);
        wait_done(&mut h);
        assert_eq!(
            h.state().preview_total,
            1,
            "only the unaccepted duplicate is planned"
        );
        let _ = &tmp;
    }

    /// The number keys switch commands (mirroring the segmented selector).
    #[test]
    fn number_keys_select_command() {
        let (_tmp, store) = sample_store();
        let mut h = grooming_harness(Arc::clone(&store), Command::Dedupe);
        assert!(h.state().command == Command::Dedupe);

        h.key_press(egui::Key::Num2);
        h.run();
        h.run();
        assert!(h.state().command == Command::Purge, "2 selects PURGE");

        h.key_press(egui::Key::Num4);
        h.run();
        h.run();
        assert!(h.state().command == Command::Organize, "4 selects ORGANIZE");

        h.key_press(egui::Key::Num5);
        h.run();
        h.run();
        assert!(h.state().command == Command::Prune, "5 selects PRUNE");
    }

    /// PRUNE shows just the single REPO picker — no FILTER and no dupe pool
    /// (its target is the repo's own missing records).
    #[test]
    fn prune_layout_shows_repo_only() {
        let (_tmp, store) = sample_store();
        let prune = grooming_harness(store, Command::Prune);
        assert!(
            prune.query_by_label_contains("WITH WHICH — ").is_some(),
            "PRUNE has REPO"
        );
        assert!(
            prune.query_by_label_contains("FILTER — ").is_none(),
            "PRUNE has no filter"
        );
        assert!(
            prune.query_by_label("DUPEPOOL").is_none(),
            "PRUNE has no dupe pool"
        );
    }

    /// DEDUPE shows SOURCE + DUPEPOOL + FILTER; PURGE shows a single REPO +
    /// FILTER; EMPTY DIRS shows just REPO and no FILTER. Each command's layout
    /// is distinct.
    #[test]
    fn each_command_has_its_own_layout() {
        let (_tmp, store) = sample_store();

        let dedupe = grooming_harness(Arc::clone(&store), Command::Dedupe);
        assert!(
            dedupe.query_by_label("SOURCE").is_some(),
            "DEDUPE has SOURCE"
        );
        assert!(
            dedupe.query_by_label("DUPEPOOL").is_some(),
            "DEDUPE has DUPEPOOL"
        );
        assert!(
            dedupe.query_by_label_contains("FILTER — ").is_some(),
            "DEDUPE has FILTER"
        );
        assert!(
            dedupe
                .query_by_label_contains("THE REPOSITORY TO GROOM")
                .is_none(),
            "DEDUPE uses SOURCE, not the single REPO picker"
        );

        let purge = grooming_harness(Arc::clone(&store), Command::Purge);
        assert!(
            purge.query_by_label_contains("WITH WHICH — ").is_some(),
            "PURGE has REPO"
        );
        assert!(
            purge.query_by_label_contains("FILTER — ").is_some(),
            "PURGE has FILTER"
        );
        assert!(
            purge.query_by_label("DUPEPOOL").is_none(),
            "PURGE has no dupe pool"
        );

        let empty = grooming_harness(Arc::clone(&store), Command::EmptyDirs);
        assert!(
            empty.query_by_label_contains("WITH WHICH — ").is_some(),
            "EMPTY DIRS has REPO"
        );
        assert!(
            empty.query_by_label_contains("FILTER — ").is_none(),
            "EMPTY DIRS has no filter"
        );

        let organize = grooming_harness(store, Command::Organize);
        assert!(
            organize.query_by_label_contains("WITH WHICH — ").is_some(),
            "ORGANIZE has a repo picker"
        );
        assert!(
            organize.query_by_label("TEMPLATE").is_some(),
            "ORGANIZE shows a rule template field"
        );
        assert!(
            organize.query_by_label("+ RULE").is_some(),
            "ORGANIZE shows the add-rule button"
        );
        assert!(
            organize.query_by_label("DUPEPOOL").is_none(),
            "ORGANIZE has no dupe pool"
        );
    }

    /// The rule's remove control is an inline trash button sharing the TEMPLATE
    /// row (no dedicated full-width delete row), and the preset row offers
    /// STORE PRESET instead of a name field + SAVE.
    #[test]
    fn organize_delete_sits_inline_on_the_template_row() {
        let (_tmp, store) = sample_store();
        let organize = grooming_harness(store, Command::Organize);
        let template = organize.get_by_label("TEMPLATE").rect();
        let trash = organize.get_by_label(icon::TRASH).rect();
        assert!(
            (trash.center().y - template.center().y).abs() < template.height(),
            "the delete button shares the TEMPLATE row (trash y={}, template y={})",
            trash.center().y,
            template.center().y
        );
        assert!(
            organize.query_by_label_contains("DELETE RULE").is_none(),
            "the old dedicated DELETE RULE row is gone"
        );
        assert!(
            organize.query_by_label_contains("STORE PRESET").is_some(),
            "the preset row offers a STORE PRESET pill"
        );
        assert!(
            organize.query_by_label("SAVE").is_none(),
            "the old name-field + SAVE preset UI is gone"
        );
    }

    /// Rule sections nest inside the RULES elbow: RULE 1's chrome starts to the
    /// right of the RULES rail, below the RULES header.
    #[test]
    fn rule_sections_nest_inside_rules_elbow() {
        let (_tmp, store) = sample_store();
        let organize = grooming_harness(store, Command::Organize);
        let rules = organize.get_by_label_contains("RULES THAT MATCH").rect();
        let rule1 = organize.get_by_label_contains("RULE 1").rect();
        assert!(
            rule1.left() > rules.left(),
            "RULE 1 (left={}) should be inset within the RULES section (left={})",
            rule1.left(),
            rules.left()
        );
        assert!(
            rule1.top() > rules.top(),
            "RULE 1 should sit below the RULES header"
        );
    }

    /// Every elbow section collapses on a header click, not just the organize
    /// rules: folding COMMAND hides the command selector, clicking again
    /// restores it.
    #[test]
    fn every_section_collapses_on_header_click() {
        let (_tmp, store) = sample_store();
        let mut h = grooming_harness(store, Command::Purge);
        assert!(
            h.query_by_label("DEDUPE").is_some(),
            "the command selector starts visible"
        );
        h.get_by_label_contains("WHAT — ").click();
        h.run();
        assert!(
            h.query_by_label("DEDUPE").is_none(),
            "collapsing COMMAND hides the selector"
        );
        h.get_by_label_contains("WHAT — ").click();
        h.run();
        assert!(
            h.query_by_label("DEDUPE").is_some(),
            "clicking again expands the section"
        );
    }

    /// Clicking a rule's header bar collapses its body (the TEMPLATE field
    /// disappears); clicking again restores it.
    #[test]
    fn rule_header_click_collapses_and_expands_the_body() {
        let (_tmp, store) = sample_store();
        let mut h = grooming_harness(store, Command::Organize);
        assert!(
            h.query_by_label("TEMPLATE").is_some(),
            "rule body starts expanded"
        );
        h.get_by_label_contains("RULE 1").click();
        h.run();
        assert!(
            h.query_by_label("TEMPLATE").is_none(),
            "collapsing the rule hides its body"
        );
        h.get_by_label_contains("RULE 1").click();
        h.run();
        assert!(
            h.query_by_label("TEMPLATE").is_some(),
            "clicking again expands the body"
        );
    }

    /// The FILTER wizard (shared by PURGE) no longer offers EXPORT/IMPORT, and
    /// offers STORE PRESET once a condition is entered.
    #[test]
    fn filter_no_longer_offers_export_import() {
        let (_tmp, store) = sample_store();
        let mut h = grooming_harness(store, Command::Purge);
        h.state_mut().filter.set_expression("name:foo");
        h.run();
        assert!(
            h.query_by_label("EXPORT").is_none(),
            "EXPORT is gone from the filter preset row"
        );
        assert!(
            h.query_by_label("IMPORT").is_none(),
            "IMPORT is gone from the filter preset row"
        );
        assert!(
            h.query_by_label_contains("STORE PRESET").is_some(),
            "the filter preset row offers STORE PRESET once a condition is set"
        );
    }

    /// `section_lcars` now always claims the panel's full width, so even a
    /// Rows carry no COMPARE command any more: clicking the row itself opens the
    /// shared viewer, so a row-level command would be a second door to the same
    /// place. This deliberately undoes part of the 2026-07-31 backlog batch.
    #[test]
    fn rows_carry_no_compare_command() {
        let cmds = [board::Cmd::Apply, board::Cmd::Hide];
        assert!(
            !cmds.contains(&board::Cmd::Compare),
            "the row body is how a row is opened, not a command"
        );
    }

    /// narrow-content rule section (a single button + short fields) spans
    /// A DEDUPE row names the copy that makes the file redundant, so the user can
    /// see what survives instead of trusting a bare deletion list. PURGE and
    /// PRUNE have no such counterpart and stay one-sided.
    #[test]
    fn a_dedupe_row_shows_the_copy_that_makes_it_redundant() {
        // The shape `run_preview` builds for DEDUPE: left is doomed, right names
        // the reference copy.
        let with_ref = board::RowMeta {
            key: dedup_core::diff::source_key("dupe.jpg"),
            left_status: board::Status::WillDelete,
            right_status: board::Status::Same,
            left_paths: vec!["dupe.jpg".to_string()],
            right_paths: vec!["archive/original.jpg".to_string()],
            left_size: 10,
            right_size: 10,
            left_modified: 0,
            right_modified: 0,
            unchanged: false,
            cmds: vec![board::Cmd::Apply, board::Cmd::Hide],
        };
        assert_eq!(
            with_ref.right_paths,
            ["archive/original.jpg"],
            "the surviving copy is named on the row"
        );
        assert_ne!(
            with_ref.right_status,
            board::Status::Absent,
            "so the right side actually renders"
        );

        // PURGE / PRUNE keep the one-sided shape.
        let one_sided = one_sided(vec!["junk.tmp".to_string()]);
        assert_eq!(one_sided, [("junk.tmp".to_string(), None)]);
    }

    /// nearly the whole window rather than shrinking to fit its content.
    #[test]
    fn rule_elbow_spans_full_panel_width() {
        let (_tmp, store) = sample_store();
        let width = 1120.0;
        let organize = grooming_harness(store, Command::Organize);
        let rule_title = organize.get_by_label_contains("RULE 1").rect();
        assert!(
            rule_title.right() > width - 220.0,
            "the RULE 1 elbow should span nearly the full panel width, right={}",
            rule_title.right()
        );
    }

    /// Seed a PURGE-shaped preview: every row is a deletion from one repo.
    fn seed_purge(v: &mut GroomingView, paths: &[String]) {
        v.preview_source_header = "/repos/junk".to_string();
        v.preview_target_header = None;
        v.preview_total = paths.len();
        v.preview_totals = [paths.len(), 0, 0, 0];
        v.preview = paths
            .iter()
            .map(|p| board::RowMeta {
                key: dedup_core::diff::source_key(p),
                left_status: board::Status::WillDelete,
                right_status: board::Status::Absent,
                left_paths: vec![p.clone()],
                right_paths: Vec::new(),
                left_size: 0,
                right_size: 0,
                left_modified: 0,
                right_modified: 0,
                unchanged: false,
                cmds: vec![board::Cmd::Apply, board::Cmd::Hide],
            })
            .collect();
        v.preview_bodies = vec![board::RowBody::default(); paths.len()];
    }

    /// A seeded PURGE preview renders the board with a to-delete summary, the
    /// repo path in the region header, and a sort bar in place of the old
    /// click-to-sort column headers.
    #[test]
    fn purge_preview_shows_the_summary_header_and_sort_bar() {
        let (_tmp, store) = sample_store();
        let mut h = grooming_harness(store, Command::Purge);
        seed_purge(h.state_mut(), &["a.tmp".to_string(), "b.tmp".to_string()]);
        h.run();
        assert!(
            h.query_by_label_contains("2 to delete").is_some(),
            "summary shows the to-delete count"
        );
        assert!(
            h.query_by_label_contains("/repos/junk").is_some(),
            "the left region is headed by the repo path"
        );
        assert!(h.state().board_state.sort_asc, "starts ascending");
        // The sort direction is its own control now, not a header click.
        h.get_by_label("▲").click();
        h.run();
        assert!(
            !h.state().board_state.sort_asc,
            "the direction toggle reverses the sort"
        );
    }

    /// PURGE is single-sided, so the board offers no LEFT/RIGHT sort switch —
    /// there is no right-hand side to sort by.
    #[test]
    fn a_single_sided_board_offers_no_side_switch() {
        let (_tmp, store) = sample_store();
        let mut h = grooming_harness(store, Command::Purge);
        seed_purge(h.state_mut(), &["a.tmp".to_string()]);
        h.run();
        assert!(
            h.query_by_label("PATH").is_some(),
            "the sort keys are offered"
        );
        assert!(
            h.query_by_label("RIGHT").is_none(),
            "a one-sided board has no right side to sort by"
        );
        assert!(
            h.state().board_state.sort_left,
            "the sort stays on the left"
        );
    }

    /// HIDE drops a row from the board and from what RUN will do. It is
    /// deliberately one-way: no counter, and no control to bring it back.
    #[test]
    fn hide_removes_a_row_from_the_board_and_from_the_run() {
        let (_tmp, store) = sample_store();
        let mut h = grooming_harness(store, Command::Purge);
        seed_purge(h.state_mut(), &["a.tmp".to_string(), "b.tmp".to_string()]);
        h.run();
        assert_eq!(h.get_all_by_label("HIDE").count(), 2, "one HIDE per row");

        h.get_all_by_label("HIDE").next().unwrap().click();
        h.run();
        h.run();
        assert_eq!(
            h.state().board_state.hidden.len(),
            1,
            "the row is recorded as hidden, which is what RUN skips"
        );
        assert_eq!(
            h.get_all_by_label("HIDE").count(),
            1,
            "the hidden row has left the board"
        );
        assert!(
            h.query_by_label_contains("hidden").is_none(),
            "there is deliberately no hidden-row counter"
        );
        assert!(
            h.query_by_label_contains("UNHIDE").is_none(),
            "and deliberately no way back"
        );
    }

    /// Every row offers APPLY, and clicking it runs just that row.
    #[test]
    fn a_row_can_be_applied_on_its_own() {
        let (_tmp, store) = sample_store();
        let mut h = grooming_harness(store, Command::Purge);
        // APPLY deletes, so the groomed repo must be unlocked to offer it.
        h.state().locks.toggle("a");
        seed_purge(h.state_mut(), &["a.tmp".to_string(), "b.tmp".to_string()]);
        h.run();
        assert_eq!(
            h.get_all_by_label("APPLY").count(),
            2,
            "each row offers APPLY"
        );
    }

    /// A preview larger than a screenful is virtualised rather than paged: the
    /// row count is shown, every row is reachable by scrolling, and the old
    /// PREV / PAGE / NEXT strip is gone.
    #[test]
    fn a_large_preview_is_virtualised_not_paged() {
        let (_tmp, store) = sample_store();
        let mut h = grooming_harness(store, Command::Purge);
        let paths: Vec<String> = (0..501).map(|i| format!("f{i:04}.tmp")).collect();
        seed_purge(h.state_mut(), &paths);
        h.run();
        assert!(
            h.query_by_label_contains("501 rows").is_some(),
            "the row count is always shown"
        );
        assert!(
            h.query_by_label("NEXT").is_none(),
            "paging is replaced by scrolling"
        );
        assert!(
            h.query_by_label_contains("PAGE 1").is_none(),
            "no page indicator"
        );
        // Virtualised: only the rows near the viewport are built, not all 501.
        let drawn = h.get_all_by_label("HIDE").count();
        assert!(
            drawn > 0 && drawn < 501,
            "only the visible rows are drawn, got {drawn}"
        );
    }

    /// Token chips insert at the caret (and move it), not blindly at the end.
    #[test]
    fn insert_at_cursor_inserts_at_caret_and_advances_it() {
        let ctx = egui::Context::default();
        let id = egui::Id::new("tpl-test");
        let mut template = String::from("{year}/{o-name}");
        // No cursor state yet (field never focused) → appends.
        insert_at_cursor(&ctx, id, &mut template, "{day}");
        assert_eq!(template, "{year}/{o-name}{day}");
        // Caret after "{year}" (6 chars) → the token lands mid-string.
        let mut st = egui::widgets::text_edit::TextEditState::default();
        st.cursor.set_char_range(Some(egui::text::CCursorRange::one(
            egui::text::CCursor::new(6),
        )));
        st.store(&ctx, id);
        insert_at_cursor(&ctx, id, &mut template, "{month}");
        assert_eq!(template, "{year}{month}/{o-name}{day}");
        let caret = egui::TextEdit::load_state(&ctx, id)
            .expect("state was stored")
            .cursor
            .char_range()
            .expect("caret was set")
            .primary
            .index
            .0;
        assert_eq!(
            caret,
            "{year}{month}".chars().count(),
            "caret follows the insertion"
        );
    }

    /// Doc screenshot: a one-sided PURGE preview on the unified board, to
    /// `docs/screenshots/groom_purge_board.png`. Rendered rather than
    /// label-queried, because a label query cannot see a layout fault.
    #[test]
    #[ignore = "generates a doc screenshot (needs wgpu)"]
    fn doc_screenshot_groom_purge_board() {
        let (_tmp, store) = sample_store();
        let mut h = grooming_harness(store, Command::Purge);
        // Real bytes behind each row, so the cells show byte-view previews
        // under their red WILL DELETE veils instead of empty space.
        let dir = tempfile::tempdir().unwrap();
        dedup_core::thumbnail::set_cache_dir(dir.path().join("thumbs"));
        {
            let paths: Vec<String> = (0..6).map(|i| format!("cache/file{i}.db")).collect();
            seed_purge(h.state_mut(), &paths);
            let v = h.state_mut();
            v.preview_bodies = paths
                .iter()
                .enumerate()
                .map(|(i, rel)| {
                    let name = rel.rsplit('/').next().unwrap_or(rel);
                    let path = dir.path().join(name);
                    let bytes: Vec<u8> = (0..4096u32)
                        .map(|b| ((b / 64 + i as u32) % 5 * 53) as u8)
                        .collect();
                    std::fs::write(&path, &bytes).unwrap();
                    board::RowBody {
                        left: board::SideBody {
                            facts: Some(crate::media_cell::FileFacts {
                                size: bytes.len() as u64,
                                modified_ms: 1_700_000_000_000,
                                missing: false,
                                mime: Some("application/octet-stream".into()),
                                img_size: None,
                                audio_ms: None,
                                audio_seed: None,
                                hash_hex: format!("purge-{i}"),
                                abs_path: path,
                                origin: None,
                                exif: None,
                            }),
                            repo: None,
                            repo_is_main: false,
                            overlay: Some(crate::media_cell::CellOverlay::WillDelete),
                        },
                        right: board::SideBody::default(),
                    }
                })
                .collect();
        }
        for _ in 0..40 {
            h.step();
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let img = h.render().expect("wgpu render failed");
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/screenshots");
        std::fs::create_dir_all(&dir).expect("screenshot dir");
        let out = dir.join("groom_purge_board.png");
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());
    }

    /// Two scanned repos with real content for behavior tests: "a" (groomed)
    /// holds a duplicate of pool content, a unique file, purgeable junk and an
    /// empty directory tree; "b" is the dupe pool.
    fn seeded_store() -> (tempfile::TempDir, Arc<Store>) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open_at(tmp.path().join("cfg")).unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir_all(a.join("empty/nested")).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("dup.txt"), b"shared-content").unwrap();
        std::fs::write(a.join("unique.txt"), b"only-in-a").unwrap();
        std::fs::write(a.join("junk.tmp"), b"cache junk").unwrap();
        std::fs::write(b.join("kept.txt"), b"shared-content").unwrap();
        store.create_repo("a", &a.to_string_lossy()).unwrap();
        store.create_repo("b", &b.to_string_lossy()).unwrap();
        for repo in ["a", "b"] {
            dedup_core::update::update_repo(
                &store,
                repo,
                1,
                &dedup_core::update::NoProgress,
                &CancellationToken::new(),
            )
            .unwrap();
        }
        (tmp, Arc::new(store))
    }

    /// Step until the background run finishes (never `run()` — the running
    /// spinner repaints forever and would blow kittest's settle cap).
    fn wait_done(h: &mut Harness<'static, GroomingView>) {
        for _ in 0..600 {
            h.step();
            if !h.state().running && !h.state().previewing {
                h.step();
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("the grooming run never finished");
    }

    /// DEDUPE end to end: REVIEW plans exactly the pool-covered file, RUN is
    /// inert while the source is locked (the session default), and after
    /// unlocking PROCEED deletes only that file from disk.
    #[test]
    fn dedupe_runs_after_unlock_and_deletes_only_covered_files() {
        let (tmp, store) = seeded_store();
        let mut h = grooming_harness(Arc::clone(&store), Command::Dedupe);
        h.state_mut().source = Some("a".to_string());
        h.state_mut().pool = vec!["b".to_string()];
        h.run();
        h.get_by_label("REVIEW").click_accesskit();
        wait_done(&mut h);
        assert_eq!(
            h.state().preview_total,
            1,
            "exactly the pool-covered file is planned"
        );

        // Locked source: RUN never reaches a confirmation.
        h.get_by_label("RUN").click_accesskit();
        h.step();
        h.step();
        assert!(
            h.state().confirm.is_none(),
            "a locked repo blocks the run: {:?}",
            h.state().confirm
        );

        h.state().locks.toggle("a");
        h.run();
        h.get_by_label("RUN").click_accesskit();
        for _ in 0..100 {
            h.step();
            if h.state().confirm.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(h.state().confirm.is_some(), "RUN asks before deleting");
        h.step(); // draw the freshly-raised modal before querying it
        h.get_by_label("PROCEED").click_accesskit();
        wait_done(&mut h);
        let a = tmp.path().join("a");
        assert!(!a.join("dup.txt").exists(), "the duplicate was deleted");
        assert!(a.join("unique.txt").exists(), "unique content stays");
        assert!(a.join("junk.tmp").exists(), "non-duplicate content stays");
        assert!(
            tmp.path().join("b").join("kept.txt").exists(),
            "the pool is never changed"
        );
    }

    /// PURGE deletes exactly the filter's matches — and an empty filter
    /// matches nothing, so the repo can never be purged by accident.
    #[test]
    fn purge_deletes_only_filter_matches() {
        let (tmp, store) = seeded_store();
        let mut h = grooming_harness(Arc::clone(&store), Command::Purge);
        h.state_mut().repo = Some("a".to_string());
        h.state_mut().filter.set_expression("name:*.tmp");
        h.state().locks.toggle("a");
        h.run();
        h.get_by_label("REVIEW").click_accesskit();
        wait_done(&mut h);
        assert_eq!(h.state().preview_total, 1, "only the junk file matches");
        h.get_by_label("RUN").click_accesskit();
        for _ in 0..100 {
            h.step();
            if h.state().confirm.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        h.step(); // draw the freshly-raised modal before querying it
        h.get_by_label("PROCEED").click_accesskit();
        wait_done(&mut h);
        let a = tmp.path().join("a");
        assert!(!a.join("junk.tmp").exists(), "the match was purged");
        assert!(a.join("dup.txt").exists(), "non-matches stay");
        assert!(a.join("unique.txt").exists(), "non-matches stay");
        // The run went through the activity owner: it ends on a report, and
        // the purged file is in the event log under the repo.
        let mut reported = false;
        for _ in 0..200 {
            if crate::activity::lock(&h.state().activity).has_report() {
                reported = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(reported, "the activity modal ends on the report");
        let activity = crate::activity::lock(&h.state().activity);
        assert!(
            activity
                .logged()
                .iter()
                .any(|e| e.action == "Deleted" && e.repo == "a" && e.path == "junk.tmp"),
            "the event log records the deletion: {:?}",
            activity.logged()
        );
        assert!(
            h.state().selection_collapsed,
            "the selection folds into its summary once REVIEW starts"
        );
    }

    /// The tab asks its questions in order — WHAT, WITH WHICH, HOW, RUN —
    /// each section appearing once the one before it has an answer.
    #[test]
    fn sections_reveal_in_reading_order() {
        let (_tmp, store) = sample_store();
        let store_ui = Arc::clone(&store);
        let mut init = false;
        let mut h = Harness::builder()
            .with_size(egui::vec2(1120.0, 800.0))
            .build_ui_state(
                move |ui, view: &mut GroomingView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    view.show(ui, &store_ui, TooltipVerbosity::default());
                },
                {
                    let mut view = GroomingView::new();
                    view.loaded = true;
                    view.repos = vec!["a".to_string(), "b".to_string()];
                    view
                },
            );
        h.run();
        assert!(h.query_by_label("PURGE").is_some(), "WHAT shows first");
        assert!(
            h.query_by_label("a").is_none(),
            "no repository chip before a tool is chosen"
        );
        assert!(h.query_by_label_contains("FILTER — ").is_none());
        assert!(h.query_by_label("REVIEW").is_none());

        h.get_by_label("PURGE").click();
        h.run();
        let purge = h.get_by_label("PURGE").rect();
        let repo_chip = h.get_by_label("a").rect();
        assert!(
            repo_chip.top() > purge.bottom(),
            "the repository sits below the tool: {repo_chip:?} vs {purge:?}"
        );
        assert!(
            h.query_by_label_contains("FILTER — ").is_none(),
            "HOW waits for a repository"
        );

        h.get_by_label("a").click();
        h.run();
        let filter = h.get_by_label_contains("FILTER — ").rect();
        assert!(
            filter.top() > repo_chip.bottom(),
            "HOW sits below the repository"
        );
        assert!(
            h.query_by_label("REVIEW").is_none(),
            "RUN waits for PURGE's filter condition"
        );

        h.state_mut().filter.set_expression("name:*.tmp");
        h.run();
        let review = h.get_by_label("REVIEW").rect();
        assert!(review.top() > filter.top(), "RUN comes last");
    }

    /// A single row's APPLY is a row action: a card names the file, the
    /// event log keeps the line.
    #[test]
    fn a_single_row_apply_answers_with_a_card_and_a_log_line() {
        let (tmp, store) = seeded_store();
        let mut h = grooming_harness(Arc::clone(&store), Command::Purge);
        h.state_mut().repo = Some("a".to_string());
        h.state_mut().filter.set_expression("name:*.tmp");
        h.state().locks.toggle("a");
        h.run();
        h.get_by_label("REVIEW").click_accesskit();
        wait_done(&mut h);
        h.get_by_label("APPLY").click_accesskit();
        wait_done(&mut h);
        assert!(
            !tmp.path().join("a").join("junk.tmp").exists(),
            "the row's file was deleted"
        );
        let activity = crate::activity::lock(&h.state().activity);
        assert!(
            activity
                .card_lines()
                .iter()
                .any(|l| l == "Deleted junk.tmp"),
            "a card names the deleted file: {:?}",
            activity.card_lines()
        );
        assert!(
            activity
                .logged()
                .iter()
                .any(|e| e.action == "Deleted" && e.repo == "a" && e.path == "junk.tmp"),
            "and the event log records it"
        );
    }

    /// EMPTY DIRS removes the whole empty tree bottom-up and keeps the root
    /// and every indexed file.
    #[test]
    fn empty_dirs_removes_the_empty_tree_only() {
        let (tmp, store) = seeded_store();
        let mut h = grooming_harness(Arc::clone(&store), Command::EmptyDirs);
        h.state_mut().repo = Some("a".to_string());
        h.state().locks.toggle("a");
        h.run();
        h.get_by_label("RUN").click_accesskit();
        for _ in 0..100 {
            h.step();
            if h.state().confirm.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        h.step(); // draw the freshly-raised modal before querying it
        h.get_by_label("PROCEED").click_accesskit();
        wait_done(&mut h);
        let a = tmp.path().join("a");
        assert!(!a.join("empty").exists(), "the empty tree is gone");
        assert!(a.exists(), "the repo root is kept");
        assert!(a.join("dup.txt").exists(), "files are untouched");
    }

    /// PRUNE drops the records of files deleted from disk and reports the
    /// count; the surviving files' records stay.
    #[test]
    fn prune_forgets_missing_records() {
        let (tmp, store) = seeded_store();
        // Delete one file and rescan: its record is now "missing".
        std::fs::remove_file(tmp.path().join("a").join("unique.txt")).unwrap();
        dedup_core::update::update_repo(
            &store,
            "a",
            1,
            &dedup_core::update::NoProgress,
            &CancellationToken::new(),
        )
        .unwrap();
        let mut h = grooming_harness(Arc::clone(&store), Command::Prune);
        h.state_mut().repo = Some("a".to_string());
        h.run();
        h.get_by_label("RUN").click_accesskit();
        for _ in 0..100 {
            h.step();
            if h.state().confirm.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        h.step(); // draw the freshly-raised modal before querying it
        h.get_by_label("PROCEED").click_accesskit();
        wait_done(&mut h);
        let status = h.state().status.clone().unwrap_or_default();
        assert!(
            status.contains("records dropped 1"),
            "one missing record is pruned: {status}"
        );
    }

    /// ORGANIZE presets round-trip through disk: saving the current rules and
    /// reloading in a fresh view yields the same named preset, and applying it
    /// restores the rules.
    #[test]
    fn organize_presets_roundtrip_and_apply() {
        let (_tmp, store) = seeded_store();
        let mut view = GroomingView::new();
        view.command_chosen = true;
        // Edit the default rule in place (a fresh view already has one).
        view.rules[0].template = "{year}/{month}/{o-name}".to_string();
        view.rules[0].filter.set_expression("mime:image");
        view.store_preset(&store);
        assert_eq!(view.presets.len(), 1, "the preset was stored");

        let mut fresh = GroomingView::new();
        fresh.command_chosen = true;
        fresh.load_presets(&store);
        assert_eq!(fresh.presets.len(), 1, "the preset survives a restart");
        assert_eq!(fresh.presets[0].rules.len(), 1);
        assert_eq!(
            fresh.presets[0].rules[0].template,
            "{year}/{month}/{o-name}"
        );
        assert_eq!(fresh.presets[0].rules[0].filter, "mime:image");

        // Applying the preset replaces the live rules with the saved ones.
        fresh.rules.clear();
        fresh.apply(&store, &egui::Context::default(), Act::ApplyPreset(0));
        assert_eq!(fresh.rules.len(), 1, "the preset's rules are applied");
        assert_eq!(fresh.rules[0].template, "{year}/{month}/{o-name}");
    }

    /// Manual visual check of the reworked ORGANIZE layout (nested collapsible
    /// rules, inline delete): `cargo test -p dedup-gui organize_snapshot -- --ignored`.
    #[test]
    #[ignore = "renders a PNG for manual inspection"]
    fn render_organize_snapshot() {
        let (_tmp, store) = sample_store();
        let mut h = grooming_harness(store, Command::Organize);
        h.state_mut().rules.push(RuleUi::new());
        h.run();
        // Fold two sections so the snapshot also shows the collapsed form
        // (stadium bar + right-caret hint) next to open ones.
        h.get_by_label_contains("WITH WHICH — ").click();
        h.run();
        h.get_by_label_contains("RULE 2").click();
        h.run();
        let img = h.render().expect("wgpu render failed");
        let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/organize_snapshot.png");
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());
    }
}
