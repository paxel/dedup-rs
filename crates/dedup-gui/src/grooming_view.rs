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

use crate::filter_ui::FilterBuilder;
use crate::review;
use crate::settings::TooltipVerbosity;
use crate::theme;
use crate::util::ExplainExt;
use crossbeam_channel::{Receiver, Sender};
use dedup_core::diff::{DiffAction, DiffEvent, DiffProgress, DiffRun, diff_delete};
use dedup_core::groom::{
    delete_by_filter, delete_empty_dirs, preview_by_filter, preview_prune, prune,
};
use dedup_core::organize::{DEFAULT_TEMPLATE, OrganizeRule, organize_apply, plan_organize};
use dedup_core::store::Store;
use dedup_core::update::CancellationToken;
use egui::{Id, RichText};
use std::collections::VecDeque;
use std::sync::Arc;

/// Safety cap on how many rows the review table materialises. The virtualised
/// table renders only visible rows, but we still bound the in-memory sample;
/// the summary counts are the true totals regardless of this cap.
const PREVIEW_CAP: usize = 100_000;
const RUN_LOG_LIMIT: usize = 10;

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
                 by any other repo, so use it carefully.",
            ),
            Command::EmptyDirs => (
                "Remove empty directories",
                "Remove every empty directory under the repo's root, bottom-up. The repo \
                 root itself is kept and indexed files are untouched.",
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

enum OpResult {
    Deleted {
        deleted: u64,
        cancelled: bool,
    },
    EmptyDirs {
        removed: u64,
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
    Error(String),
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
    Progress(DiffEvent),
    Done(OpResult),
}

/// [`DiffProgress`] adapter forwarding diff events onto the view's channel.
struct ChannelDiffProgress {
    tx: Sender<Msg>,
}

impl DiffProgress for ChannelDiffProgress {
    fn on(&self, event: DiffEvent) {
        let _ = self.tx.send(Msg::Progress(event));
    }
}

pub struct GroomingView {
    repos: Vec<String>,
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
    preview: Vec<review::ReviewRow>,
    /// Full per-kind counts (indexed by [`review::RowKind::idx`]) for the review
    /// summary; independent of the capped `preview` sample.
    preview_totals: [usize; 3],
    /// The two review-table column headers (absolute paths). ORGANIZE uses the
    /// same repo on both sides (old path → new path); the single-repo deletions
    /// leave the target header empty.
    preview_source_header: String,
    preview_target_header: String,
    preview_total: usize,
    /// Sort column + direction for the review table.
    review_state: review::ReviewState,
    status: Option<String>,
    error: Option<String>,
    confirm: Option<String>,
    running: bool,
    cancel: CancellationToken,
    run_log: VecDeque<String>,
    run_done: u64,
    run_total: u64,
    run_current: String,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    verbosity: TooltipVerbosity,
}

enum Act {
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
    CancelRun,
}

impl GroomingView {
    pub fn new() -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        Self {
            repos: Vec::new(),
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
            preview_totals: [0; 3],
            preview_source_header: String::new(),
            preview_target_header: String::new(),
            preview_total: 0,
            review_state: review::ReviewState::default(),
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

    pub fn show(&mut self, ui: &mut egui::Ui, store: &Arc<Store>, verbosity: TooltipVerbosity) {
        self.verbosity = verbosity;
        self.drain();
        if !self.loaded {
            self.sync_repos(store);
        }

        let mut acts: Vec<Act> = Vec::new();

        // Keyboard shortcuts — skipped while the confirm modal is up, a run is
        // active, or a text field is focused.
        if self.confirm.is_none() && !self.running && !ui.ctx().egui_wants_keyboard_input() {
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
                        .color(theme::TAN)
                        .size(18.0)
                        .strong(),
                );
                crate::util::shortcut_bar(
                    ui,
                    "1 dedupe · 2 purge · 3 empty-dirs · 4 organize · 5 prune · P preview · R run",
                );

                self.command_bar(ui, &mut acts);
                // Each command draws its own controls; DEDUPE/PURGE then get the
                // shared single FILTER wizard, backed by the repo they act on.
                // ORGANIZE draws its own per-rule wizards inside its layout.
                match self.command {
                    Command::Dedupe => {
                        self.dedupe_layout(ui, &mut acts);
                        let repo = self.source.clone();
                        self.shared_filter(ui, store, repo.as_deref());
                    }
                    Command::Purge => {
                        self.purge_layout(ui, &mut acts);
                        let repo = self.repo.clone();
                        self.shared_filter(ui, store, repo.as_deref());
                    }
                    Command::EmptyDirs => self.empty_dirs_layout(ui, &mut acts),
                    Command::Organize => self.organize_layout(ui, store, &mut acts),
                    Command::Prune => self.prune_layout(ui, &mut acts),
                }
                self.action_bar(ui, &mut acts);

                if let Some(err) = &self.error {
                    ui.colored_label(theme::RED, err);
                }
                if let Some(status) = &self.status {
                    ui.label(RichText::new(status).color(theme::TAN).size(13.0));
                }
                ui.separator();
                if self.running || !self.run_log.is_empty() {
                    self.run_panel(ui);
                } else {
                    self.preview_panel(ui);
                }
            });

        if let Some(prompt) = self.confirm.clone() {
            self.confirm_modal(ui, &prompt, &mut acts);
        }

        for act in acts {
            self.apply(store, act);
        }
    }

    fn command_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        crate::lcars::section_lcars(ui, "COMMAND", theme::ORANGE, |ui| {
            ui.horizontal(|ui| {
                for cmd in [
                    Command::Dedupe,
                    Command::Purge,
                    Command::EmptyDirs,
                    Command::Organize,
                    Command::Prune,
                ] {
                    let sel = self.command == cmd;
                    let fill = if sel { theme::RED } else { theme::PANEL };
                    let col = if sel { theme::BLACK } else { theme::RED };
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
        });
    }

    fn dedupe_layout(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        crate::lcars::section_lcars(ui, "REPOS", theme::LILAC, |ui| {
            // SOURCE: the repo duplicates are deleted from, orange when picked.
            let src = self.repos.clone();
            crate::repo_chip::chip_row(ui, "groom_source", "SOURCE", src.len(), |ui, i| {
                let name = &src[i];
                let sel = self.source.as_deref() == Some(name.as_str());
                let chip = crate::repo_chip::repo_chip(ui, name, sel, theme::ORANGE, None);
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
                ui.label(RichText::new("DUPEPOOL").color(theme::TEXT).size(12.0));
                if crate::repo_chip::small_button(ui, "ALL", theme::LILAC)
                    .explain(
                        self.verbosity,
                        "Add every eligible repo to the pool",
                        "A source file is deleted when its content exists in any pool repo.",
                    )
                    .clicked()
                {
                    self.pool = pool.clone();
                }
                if crate::repo_chip::small_button(ui, "NONE", theme::LILAC)
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
            crate::repo_chip::chip_row(ui, "groom_pool", "", pool.len(), |ui, i| {
                let name = &pool[i];
                let sel = self.pool.iter().any(|r| r == name);
                let chip = crate::repo_chip::repo_chip(ui, name, sel, theme::LILAC, None);
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
                chip.outer
            });
        });
    }

    fn purge_layout(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        self.single_repo_bar(ui, acts, "Delete matching files from this repo.");
    }

    fn empty_dirs_layout(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        self.single_repo_bar(ui, acts, "Remove empty directories under this repo's root.");
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

    /// ORGANIZE: a repo picker, the ordered rule list (each rule = the shared
    /// FILTER wizard + a path template with token chips), and saved presets.
    fn organize_layout(&mut self, ui: &mut egui::Ui, store: &Arc<Store>, acts: &mut Vec<Act>) {
        self.single_repo_bar(ui, acts, "Reorganize the files in this repo in place.");
        let repo = self.repo.clone();

        crate::lcars::section_lcars(
            ui,
            "RULES — ADD RULES & MANAGE PRESETS",
            theme::LILAC,
            |ui| {
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .add(
                            egui::Button::new(RichText::new("+ RULE").color(theme::BLACK))
                                .fill(theme::AMBER),
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
            },
        );

        let rule_count = self.rules.len();
        for i in 0..rule_count {
            let title = format!("RULE {} — MATCH & RENAME", i + 1);
            crate::lcars::section_lcars(ui, &title, theme::BLUE, |ui| {
                // The rule's filter (which files this rule applies to).
                let outcome = self.rules[i]
                    .filter
                    .ui(ui, store, repo.as_deref(), self.verbosity);
                if outcome.changed {
                    self.clear_preview();
                }
                if outcome.error.is_some() {
                    self.error = outcome.error;
                }
                // The target path template + one-click token chips.
                ui.horizontal(|ui| {
                    ui.label(RichText::new("TEMPLATE").color(theme::TEXT).size(12.0));
                    let changed = ui
                        .add(
                            egui::TextEdit::singleline(&mut self.rules[i].template)
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
                });
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("insert:").color(theme::LILAC).size(11.0));
                    for (label, insert) in TEMPLATE_TOKENS {
                        if ui
                            .add(
                                egui::Button::new(RichText::new(*label).color(theme::BLUE))
                                    .fill(theme::PANEL),
                            )
                            .clicked()
                        {
                            self.rules[i].template.push_str(insert);
                            self.clear_preview();
                        }
                    }
                });
                // A rule can always be removed (a repo may need zero rules).
                // Bottom-right, out of the way of the fields above it.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if crate::lcars::action_button(ui, "DELETE RULE", true, theme::RED)
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
            });
        }
    }

    /// The ORGANIZE saved-preset row: apply/forget pills (right-click to
    /// rename), plus a STORE PRESET pill for the current rule list.
    fn preset_row(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        ui.separator();
        ui.label(RichText::new("PRESETS").color(theme::TEXT).size(12.0));
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
                .add(egui::Button::new(RichText::new(&name).color(theme::TAN)).fill(theme::PANEL))
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
                .add(egui::Button::new(RichText::new("×").color(theme::RED)))
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
                    egui::Button::new(RichText::new("STORE PRESET").color(theme::BLACK))
                        .fill(theme::AMBER),
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
        crate::lcars::section_lcars(ui, "REPO", theme::LILAC, |ui| {
            let repos = self.repos.clone();
            crate::repo_chip::chip_row(ui, "groom_repo", "", repos.len(), |ui, i| {
                let name = &repos[i];
                let sel = self.repo.as_deref() == Some(name.as_str());
                let chip = crate::repo_chip::repo_chip(ui, name, sel, theme::ORANGE, None);
                if chip
                    .name
                    .explain(self.verbosity, "Pick the repo to act on", hint)
                    .clicked()
                {
                    acts.push(Act::PickRepo(name.clone()));
                }
                chip.outer
            });
            ui.label(RichText::new(hint).color(theme::LILAC).size(11.0));
        });
    }

    /// A single-line filter expression (mime / size / name with `*` wildcards).
    fn action_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        crate::lcars::section_lcars(ui, "ACTION", theme::AMBER, |ui| {
            ui.horizontal(|ui| {
                let ready = self.ready();
                // EMPTY DIRS has no meaningful file preview (its count is only
                // known after walking), so PREVIEW is offered for the other two.
                if self.command != Command::EmptyDirs
                    && ui
                        .add_enabled(
                            ready,
                            egui::Button::new(RichText::new("PREVIEW").color(theme::BLACK)),
                        )
                        .explain(
                            self.verbosity,
                            "Preview what would be deleted",
                            "List the first matching files (up to a limit) and a total count, \
                             without changing anything on disk.",
                        )
                        .clicked()
                {
                    acts.push(Act::Preview);
                }
                let run =
                    egui::Button::new(RichText::new("RUN").color(theme::BLACK)).fill(theme::RED);
                if ui
                    .add_enabled(ready, run)
                    .explain(
                        self.verbosity,
                        "Run the command",
                        "Run the selected command on a background thread, after a \
                         confirmation dialog.",
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
                            "Cancel the in-progress operation. Files already deleted stay \
                             deleted — this stops further work, it doesn't roll back.",
                        )
                        .clicked()
                    {
                        acts.push(Act::CancelRun);
                    }
                }
            });
        });
    }

    fn preview_panel(&mut self, ui: &mut egui::Ui) {
        if self.preview.is_empty() {
            ui.add_space(6.0);
            ui.colored_label(theme::TEXT, "Pick a repo and command, then press PREVIEW.");
            return;
        }
        review::table(
            ui,
            &mut self.review_state,
            &mut self.preview,
            self.preview_totals,
            &self.preview_source_header,
            &self.preview_target_header,
        );
    }

    /// A repo's absolute path for a review-table column header, falling back to
    /// its name if it can't be resolved.
    fn repo_header(store: &Store, name: &str) -> String {
        store
            .get_repo(name)
            .map(|m| m.abs_path)
            .unwrap_or_else(|_| name.to_string())
    }

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
        egui::Modal::new(Id::new("grooming-confirm")).show(&ui.ctx().clone(), |ui| {
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
                if ui
                    .add(
                        egui::Button::new(RichText::new("PROCEED").color(theme::BLACK))
                            .fill(theme::RED),
                    )
                    .clicked()
                {
                    acts.push(Act::Confirm);
                }
                if ui
                    .add(egui::Button::new(
                        RichText::new("CANCEL").color(theme::TEXT),
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
        if self.running {
            return false;
        }
        match self.command {
            Command::Dedupe => self.source.is_some() && !self.pool.is_empty(),
            Command::Purge | Command::EmptyDirs | Command::Prune => self.repo.is_some(),
            Command::Organize => self.repo.is_some() && !self.rules.is_empty(),
        }
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

    fn apply(&mut self, store: &Arc<Store>, act: Act) {
        match act {
            Act::SetCommand(cmd) => {
                self.command = cmd;
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
        self.preview_totals = [0; 3];
        self.preview_source_header.clear();
        self.preview_target_header.clear();
        self.preview_total = 0;
    }

    fn reset_run(&mut self) {
        self.run_log.clear();
        self.run_done = 0;
        self.run_total = 0;
        self.run_current.clear();
    }

    fn run_preview(&mut self, store: &Store) {
        self.reset_run();
        let filter = self.filter_string();
        let result = match self.command {
            Command::Dedupe => {
                let Some(source) = self.source.clone() else {
                    return;
                };
                self.preview_source_header = Self::repo_header(store, &source);
                let pool = self.pool.clone();
                let ref_slice: Vec<&str> = pool.iter().map(String::as_str).collect();
                dedup_core::diff::diff_print(store, &source, &ref_slice, filter.as_deref())
                    .map(|items| {
                        let matched: Vec<String> = items
                            .into_iter()
                            .filter_map(|item| match item {
                                dedup_core::diff::DiffItem::Equal { rel_path, .. }
                                | dedup_core::diff::DiffItem::DeletedInReference { rel_path } => {
                                    Some(rel_path)
                                }
                                dedup_core::diff::DiffItem::New { .. } => None,
                            })
                            .collect();
                        let total = matched.len();
                        (matched.into_iter().take(PREVIEW_CAP).collect(), total)
                    })
                    .map_err(|e| e.to_string())
            }
            Command::Purge => {
                let Some(repo) = self.repo.clone() else {
                    return;
                };
                self.preview_source_header = Self::repo_header(store, &repo);
                preview_by_filter(store, &repo, filter.as_deref(), PREVIEW_CAP)
                    .map_err(|e| e.to_string())
            }
            Command::Prune => {
                let Some(repo) = self.repo.clone() else {
                    return;
                };
                self.preview_source_header = Self::repo_header(store, &repo);
                preview_prune(store, &repo, PREVIEW_CAP).map_err(|e| e.to_string())
            }
            Command::Organize => {
                self.run_preview_organize(store);
                return;
            }
            // EMPTY DIRS has no file preview.
            Command::EmptyDirs => return,
        };
        match result {
            Ok((paths, total)) => {
                // DEDUPE / PURGE / PRUNE all remove files from the one repo: the
                // source side is removed, the target side is absent.
                self.preview_total = total;
                self.preview_totals = [0, total, 0];
                self.preview_target_header.clear();
                let mut rows: Vec<review::ReviewRow> = paths
                    .into_iter()
                    .map(|from| review::ReviewRow {
                        source: review::SideStatus::Removed,
                        target: review::SideStatus::Absent,
                        source_path: from,
                        target_path: String::new(),
                    })
                    .collect();
                review::sort(&mut rows, &self.review_state);
                self.preview = rows;
                self.status = Some(format!("{total} file(s) match."));
                self.error = None;
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

    fn run_preview_organize(&mut self, store: &Store) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        match plan_organize(store, &repo, &self.organize_rules()) {
            Ok(moves) => {
                self.preview_total = moves.len();
                // A relocation both removes the old path and adds the new one.
                self.preview_totals = [moves.len(), moves.len(), 0];
                // ORGANIZE relocates within one repo: the old path is removed and
                // the new path added — same repo on both sides.
                let header = Self::repo_header(store, &repo);
                self.preview_source_header = header.clone();
                self.preview_target_header = header;
                let mut rows: Vec<review::ReviewRow> = moves
                    .into_iter()
                    .take(PREVIEW_CAP)
                    .map(|(from, to)| review::ReviewRow {
                        source: review::SideStatus::Removed,
                        target: review::SideStatus::Added,
                        source_path: from,
                        target_path: to,
                    })
                    .collect();
                review::sort(&mut rows, &self.review_state);
                self.preview = rows;
                self.status = Some(format!("{} file(s) would move.", self.preview_total));
                self.error = None;
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    fn build_prompt(&mut self, store: &Store) -> Option<String> {
        match self.command {
            Command::Dedupe => {
                self.run_preview(store);
                let source = self.source.as_ref()?;
                Some(format!(
                    "Delete {} file(s) from '{source}' whose content is in the dupe pool? \
                     This cannot be undone.",
                    self.preview_total
                ))
            }
            Command::Purge => {
                self.run_preview(store);
                let repo = self.repo.as_ref()?;
                Some(format!(
                    "Delete all {} file(s) matching the filter from '{repo}'? \
                     This cannot be undone.",
                    self.preview_total
                ))
            }
            Command::EmptyDirs => {
                let repo = self.repo.as_ref()?;
                Some(format!("Remove all empty directories under '{repo}'?"))
            }
            Command::Prune => {
                self.run_preview(store);
                let repo = self.repo.as_ref()?;
                Some(format!(
                    "Permanently drop {} record(s) of deleted files from '{repo}' and \
                     compact its index? This cannot be undone.",
                    self.preview_total
                ))
            }
            Command::Organize => {
                self.run_preview(store);
                let repo = self.repo.as_ref()?;
                Some(format!(
                    "Move {} file(s) into their new layout in '{repo}'? Files move within \
                     the repo; nothing is overwritten (collisions are renamed).",
                    self.preview_total
                ))
            }
        }
    }

    fn start(&mut self, store: &Arc<Store>) {
        let command = self.command;
        let filter = self.filter_string();
        let source = self.source.clone();
        let pool = self.pool.clone();
        let repo = self.repo.clone();
        let rules = self.organize_rules();
        let store = Arc::clone(store);
        let tx = self.tx.clone();
        self.cancel = CancellationToken::new();
        let cancel = self.cancel.clone();
        self.running = true;
        self.status = Some(format!("{}…", command.label().to_lowercase()));
        self.clear_preview();
        self.reset_run();

        std::thread::spawn(move || {
            let progress = ChannelDiffProgress { tx: tx.clone() };
            let run = DiffRun::new(&progress, &cancel);
            let result = match command {
                Command::Dedupe => {
                    let Some(source) = source else { return };
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
                    let Some(repo) = repo else { return };
                    match delete_by_filter(&store, &repo, filter.as_deref(), &run) {
                        Ok(s) => OpResult::Deleted {
                            deleted: s.deleted,
                            cancelled: s.cancelled,
                        },
                        Err(e) => OpResult::Error(e.to_string()),
                    }
                }
                Command::EmptyDirs => {
                    let Some(repo) = repo else { return };
                    match delete_empty_dirs(&store, &repo) {
                        Ok(removed) => OpResult::EmptyDirs { removed },
                        Err(e) => OpResult::Error(e.to_string()),
                    }
                }
                Command::Organize => {
                    let Some(repo) = repo else { return };
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
                    let Some(repo) = repo else { return };
                    match prune(&store, &repo, &run) {
                        Ok(s) => OpResult::Pruned {
                            pruned: s.pruned,
                            compacted: s.compacted,
                            cancelled: s.cancelled,
                        },
                        Err(e) => OpResult::Error(e.to_string()),
                    }
                }
            };
            let _ = tx.send(Msg::Done(result));
        });
    }

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

    fn drain(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Progress(event) => self.apply_progress(event),
                Msg::Done(result) => {
                    self.running = false;
                    match result {
                        OpResult::Deleted { deleted, cancelled } => {
                            self.status = Some(format!(
                                "Deleted {deleted} file(s){}.",
                                if cancelled { " (cancelled)" } else { "" }
                            ));
                            self.error = None;
                        }
                        OpResult::EmptyDirs { removed } => {
                            self.status = Some(format!("Removed {removed} empty director(ies)."));
                            self.error = None;
                        }
                        OpResult::Organized {
                            moved,
                            skipped,
                            errors,
                            cancelled,
                        } => {
                            self.status = Some(format!(
                                "Moved {moved}, skipped {skipped}, {errors} error(s){}.",
                                if cancelled { " (cancelled)" } else { "" }
                            ));
                            self.error = None;
                        }
                        OpResult::Pruned {
                            pruned,
                            compacted,
                            cancelled,
                        } => {
                            self.status = Some(format!(
                                "Pruned {pruned} record(s){}.",
                                if cancelled {
                                    " (cancelled, index not compacted)"
                                } else if compacted {
                                    "; index compacted"
                                } else {
                                    "; index already compact"
                                }
                            ));
                            self.error = None;
                        }
                        OpResult::Error(e) => self.error = Some(e),
                    }
                }
            }
        }
    }

    /// Sync the repo list with the store, keeping the current source/repo/pool
    /// picks and dropping any that no longer exist. Called on first show and
    /// whenever the tab is re-shown, so no manual reload button is needed.
    pub fn sync_repos(&mut self, store: &Store) {
        if !self.presets_loaded {
            self.load_presets(store);
        }
        match store.list_repos() {
            Ok(list) => {
                self.repos = list.into_iter().map(|(n, _, _)| n).collect();
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
        view.loaded = true;
        view.repos = vec!["a".to_string(), "b".to_string()];
        view.command = command;

        let store_ui = Arc::clone(&store);
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1120.0, 620.0))
            .build_ui_state(
                move |ui, view: &mut GroomingView| {
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
        assert!(prune.query_by_label("REPO").is_some(), "PRUNE has REPO");
        assert!(
            prune.query_by_label("FILTER").is_none(),
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
            dedupe.query_by_label("FILTER").is_some(),
            "DEDUPE has FILTER"
        );
        assert!(
            dedupe.query_by_label("REPO").is_none(),
            "DEDUPE uses SOURCE, not the single REPO picker"
        );

        let purge = grooming_harness(Arc::clone(&store), Command::Purge);
        assert!(purge.query_by_label("REPO").is_some(), "PURGE has REPO");
        assert!(purge.query_by_label("FILTER").is_some(), "PURGE has FILTER");
        assert!(
            purge.query_by_label("DUPEPOOL").is_none(),
            "PURGE has no dupe pool"
        );

        let empty = grooming_harness(Arc::clone(&store), Command::EmptyDirs);
        assert!(
            empty.query_by_label("REPO").is_some(),
            "EMPTY DIRS has REPO"
        );
        assert!(
            empty.query_by_label("FILTER").is_none(),
            "EMPTY DIRS has no filter"
        );

        let organize = grooming_harness(store, Command::Organize);
        assert!(
            organize.query_by_label("REPO").is_some(),
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

    /// The rule's remove control reads "DELETE RULE" (not the old "× rule"), and
    /// the preset row offers STORE PRESET instead of a name field + SAVE.
    #[test]
    fn organize_shows_delete_rule_and_store_preset() {
        let (_tmp, store) = sample_store();
        let organize = grooming_harness(store, Command::Organize);
        assert!(
            organize.query_by_label_contains("DELETE RULE").is_some(),
            "the rule remove control is labeled DELETE RULE"
        );
        assert!(
            organize.query_by_label("× rule").is_none(),
            "the old '× rule' label is gone"
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
    /// narrow-content rule section (a single button + short fields) spans
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

    /// A seeded PURGE preview renders the review board with a `removed` summary,
    /// the repo path as the source header, and a click-to-sort FILE header.
    #[test]
    fn review_board_shows_removed_and_sorts() {
        let (_tmp, store) = sample_store();
        let mut h = grooming_harness(store, Command::Purge);
        {
            let v = h.state_mut();
            v.preview_source_header = "/repos/junk".to_string();
            v.preview = vec![
                review::ReviewRow {
                    source: review::SideStatus::Removed,
                    target: review::SideStatus::Absent,
                    source_path: "a.tmp".to_string(),
                    target_path: String::new(),
                },
                review::ReviewRow {
                    source: review::SideStatus::Removed,
                    target: review::SideStatus::Absent,
                    source_path: "b.tmp".to_string(),
                    target_path: String::new(),
                },
            ];
            v.preview_totals = [0, 2, 0];
            review::sort(&mut v.preview, &v.review_state);
        }
        h.run();
        assert!(
            h.query_by_label_contains("2 removed").is_some(),
            "summary shows the removed count"
        );
        assert!(
            h.query_by_label_contains("/repos/junk").is_some(),
            "the source column is headed by the repo path"
        );
        assert!(h.state().review_state.sort_asc, "starts ascending");
        h.get_by_label_contains("/repos/junk").click();
        h.run();
        assert!(
            !h.state().review_state.sort_asc,
            "clicking the source header toggles the sort direction"
        );
    }
}
