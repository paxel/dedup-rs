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
//!
//! All destructive runs go through a confirmation modal and execute on a
//! background thread, reusing the same `DiffEvent` progress plumbing as Transfer.

use crate::filter_ui::FilterBuilder;
use crate::settings::TooltipVerbosity;
use crate::theme;
use crate::util::ExplainExt;
use crossbeam_channel::{Receiver, Sender};
use dedup_core::diff::{DiffAction, DiffEvent, DiffProgress, DiffRun, diff_delete};
use dedup_core::groom::{delete_by_filter, delete_empty_dirs, preview_by_filter};
use dedup_core::store::Store;
use dedup_core::update::CancellationToken;
use egui::{Id, RichText};
use std::collections::VecDeque;
use std::sync::Arc;

const PREVIEW_LIMIT: usize = 30;
const RUN_LOG_LIMIT: usize = 10;

#[derive(PartialEq, Clone, Copy)]
enum Command {
    /// Delete source files whose content is also in any selected pool repo.
    Dedupe,
    /// Delete every file matching a filter.
    Purge,
    /// Remove empty directories.
    EmptyDirs,
}

impl Command {
    fn label(self) -> &'static str {
        match self {
            Command::Dedupe => "DEDUPE",
            Command::Purge => "PURGE",
            Command::EmptyDirs => "EMPTY DIRS",
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
        }
    }
}

struct PreviewRow {
    path: String,
}

enum OpResult {
    Deleted { deleted: u64, cancelled: bool },
    EmptyDirs { removed: u64 },
    Error(String),
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
    /// PURGE / EMPTY DIRS: the single repo the command acts on.
    repo: Option<String>,
    /// The shared FILTER wizard (used by DEDUPE and PURGE).
    filter: FilterBuilder,
    preview: Vec<PreviewRow>,
    preview_total: usize,
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
    Reload,
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

    pub fn show(&mut self, ui: &mut egui::Ui, store: &Arc<Store>, verbosity: TooltipVerbosity) {
        self.verbosity = verbosity;
        self.drain();
        if !self.loaded {
            self.reload(store);
        }

        let mut acts: Vec<Act> = Vec::new();
        ui.add_space(6.0);
        ui.label(
            RichText::new("GROOMING")
                .color(theme::TAN)
                .size(18.0)
                .strong(),
        );

        self.command_bar(ui, &mut acts);
        match self.command {
            Command::Dedupe => self.dedupe_layout(ui, &mut acts),
            Command::Purge => self.purge_layout(ui, &mut acts),
            Command::EmptyDirs => self.empty_dirs_layout(ui, &mut acts),
        }
        // The shared FILTER wizard for the commands that filter. Its MIME
        // suggestions and live count are backed by the acted-on repo.
        let count_repo = match self.command {
            Command::Dedupe => self.source.clone(),
            Command::Purge => self.repo.clone(),
            Command::EmptyDirs => None,
        };
        if self.command != Command::EmptyDirs {
            let outcome = self
                .filter
                .ui(ui, store, count_repo.as_deref(), self.verbosity);
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
        if self.running || !self.run_log.is_empty() {
            self.run_panel(ui);
        } else {
            self.preview_panel(ui);
        }

        if let Some(prompt) = self.confirm.clone() {
            self.confirm_modal(ui, &prompt, &mut acts);
        }

        for act in acts {
            self.apply(store, act);
        }
    }

    fn command_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        theme::section(theme::ORANGE).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("COMMAND").color(theme::TEXT).size(12.0));
                for cmd in [Command::Dedupe, Command::Purge, Command::EmptyDirs] {
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
                if ui
                    .button(RichText::new("RELOAD").color(theme::BLACK))
                    .explain(
                        self.verbosity,
                        "Reload the repository list",
                        "Reload the list of registered repositories, e.g. after adding one \
                         in the Repositories tab.",
                    )
                    .clicked()
                {
                    acts.push(Act::Reload);
                }
            });
        });
    }

    fn dedupe_layout(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        theme::section(theme::LILAC).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("SOURCE").color(theme::TEXT).size(12.0));
                for name in &self.repos {
                    let sel = self.source.as_deref() == Some(name.as_str());
                    let fill = if sel { theme::ORANGE } else { theme::PANEL };
                    let col = if sel { theme::BLACK } else { theme::TEXT };
                    if ui
                        .add(egui::Button::new(RichText::new(name).color(col)).fill(fill))
                        .explain(
                            self.verbosity,
                            "Pick the repo to delete duplicates from",
                            "Files in this repo whose content is also in any dupe-pool repo \
                             are deleted from here.",
                        )
                        .clicked()
                    {
                        acts.push(Act::PickSource(name.clone()));
                    }
                }
            });
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("DUPEPOOL").color(theme::TEXT).size(12.0));
                for name in &self.repos {
                    if self.source.as_deref() == Some(name.as_str()) {
                        continue;
                    }
                    let sel = self.pool.iter().any(|r| r == name);
                    let fill = if sel { theme::LILAC } else { theme::PANEL };
                    let col = if sel { theme::BLACK } else { theme::LILAC };
                    if ui
                        .add(egui::Button::new(RichText::new(name).color(col)).fill(fill))
                        .explain(
                            self.verbosity,
                            "Add to the dupe pool",
                            "A source file is deleted when its content exists in any of these \
                             repos. The pool repos themselves are never modified.",
                        )
                        .clicked()
                    {
                        acts.push(Act::TogglePool(name.clone()));
                    }
                }
            });
        });
    }

    fn purge_layout(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        self.single_repo_bar(ui, acts, "Delete matching files from this repo.");
    }

    fn empty_dirs_layout(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        self.single_repo_bar(ui, acts, "Remove empty directories under this repo's root.");
    }

    /// A single-repo picker used by PURGE and EMPTY DIRS (they act on one repo).
    fn single_repo_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>, hint: &str) {
        theme::section(theme::LILAC).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("REPO").color(theme::TEXT).size(12.0));
                for name in &self.repos {
                    let sel = self.repo.as_deref() == Some(name.as_str());
                    let fill = if sel { theme::ORANGE } else { theme::PANEL };
                    let col = if sel { theme::BLACK } else { theme::TEXT };
                    if ui
                        .add(egui::Button::new(RichText::new(name).color(col)).fill(fill))
                        .explain(self.verbosity, "Pick the repo to act on", hint)
                        .clicked()
                    {
                        acts.push(Act::PickRepo(name.clone()));
                    }
                }
            });
            ui.label(RichText::new(hint).color(theme::LILAC).size(11.0));
        });
    }

    /// A single-line filter expression (mime / size / name with `*` wildcards).
    fn action_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        theme::section(theme::AMBER).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("ACTION").color(theme::TEXT).size(12.0));
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
        ui.label(
            RichText::new(format!(
                "{} file(s) match · showing first {}",
                self.preview_total,
                self.preview.len()
            ))
            .color(theme::AMBER)
            .strong(),
        );
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for row in &self.preview {
                    ui.label(RichText::new(&row.path).color(theme::TEXT).size(12.0));
                }
            });
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
            Command::Purge | Command::EmptyDirs => self.repo.is_some(),
        }
    }

    fn filter_string(&self) -> Option<String> {
        self.filter.filter_string()
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
            Act::Reload => self.reload(store),
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
                        (matched.into_iter().take(PREVIEW_LIMIT).collect(), total)
                    })
                    .map_err(|e| e.to_string())
            }
            Command::Purge => {
                let Some(repo) = self.repo.clone() else {
                    return;
                };
                preview_by_filter(store, &repo, filter.as_deref(), PREVIEW_LIMIT)
                    .map_err(|e| e.to_string())
            }
            // EMPTY DIRS has no file preview.
            Command::EmptyDirs => return,
        };
        match result {
            Ok((paths, total)) => {
                self.preview_total = total;
                self.preview = paths.into_iter().map(|path| PreviewRow { path }).collect();
                self.status = Some(format!("{total} file(s) match."));
                self.error = None;
            }
            Err(e) => self.error = Some(e),
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
        }
    }

    fn start(&mut self, store: &Arc<Store>) {
        let command = self.command;
        let filter = self.filter_string();
        let source = self.source.clone();
        let pool = self.pool.clone();
        let repo = self.repo.clone();
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
                        OpResult::Error(e) => self.error = Some(e),
                    }
                }
            }
        }
    }

    fn reload(&mut self, store: &Store) {
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

        let empty = grooming_harness(store, Command::EmptyDirs);
        assert!(
            empty.query_by_label("REPO").is_some(),
            "EMPTY DIRS has REPO"
        );
        assert!(
            empty.query_by_label("FILTER").is_none(),
            "EMPTY DIRS has no filter"
        );
    }
}
