//! The File Management tab: pick a source repo and a target repo, choose a
//! command (copy / move / delete), narrow with a filter, preview the first
//! `from → to` transfers, then run it on a background thread with confirmation.
//!
//! Semantics reuse the core diff operations (content compared by size + hash):
//! - **Copy/Move** transfer source files whose content the target lacks into the
//!   target repo's directory (move also marks the source entries missing).
//! - **Delete** removes source files whose content the target already has.

use crate::icon;
use crate::theme;
use crossbeam_channel::{Receiver, Sender};
use dedup_core::diff::{DiffItem, diff_copy, diff_delete, diff_print};
use dedup_core::store::Store;
use dedup_core::update::CancellationToken;
use egui::{Id, RichText};
use std::sync::Arc;

const PREVIEW_LIMIT: usize = 30;

#[derive(PartialEq, Clone, Copy)]
enum Command {
    Copy,
    Move,
    Delete,
}

impl Command {
    fn label(self) -> &'static str {
        match self {
            Command::Copy => "COPY",
            Command::Move => "MOVE",
            Command::Delete => "DELETE",
        }
    }
    fn destructive(self) -> bool {
        !matches!(self, Command::Copy)
    }
}

#[derive(PartialEq, Clone, Copy)]
enum FilterKind {
    All,
    Mime,
    Name,
    Size,
}

struct PreviewRow {
    from: String,
    to: String,
}

enum OpResult {
    Copied {
        copied: u64,
        cancelled: bool,
        moved: bool,
    },
    Deleted {
        deleted: u64,
        cancelled: bool,
    },
    Error(String),
}

pub struct FilesView {
    repos: Vec<String>,
    loaded: bool,
    source: Option<String>,
    target: Option<String>,
    command: Command,
    filter_kind: FilterKind,
    filter_value: String,
    preview: Vec<PreviewRow>,
    preview_total: usize,
    status: Option<String>,
    error: Option<String>,
    confirm: Option<String>,
    running: bool,
    cancel: CancellationToken,
    tx: Sender<OpResult>,
    rx: Receiver<OpResult>,
}

enum Act {
    PickSource(String),
    PickTarget(String),
    SetCommand(Command),
    Reload,
    Preview,
    Ask,
    Confirm,
    CancelConfirm,
    CancelRun,
}

impl FilesView {
    pub fn new() -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        Self {
            repos: Vec::new(),
            loaded: false,
            source: None,
            target: None,
            command: Command::Copy,
            filter_kind: FilterKind::All,
            filter_value: String::new(),
            preview: Vec::new(),
            preview_total: 0,
            status: None,
            error: None,
            confirm: None,
            running: false,
            cancel: CancellationToken::new(),
            tx,
            rx,
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, store: &Arc<Store>) {
        self.drain(ui);
        if !self.loaded {
            self.reload(store);
        }

        let mut acts: Vec<Act> = Vec::new();
        ui.add_space(6.0);
        ui.label(
            RichText::new("FILE MANAGEMENT")
                .color(theme::BLUE)
                .size(18.0)
                .strong(),
        );

        self.repo_rows(ui, &mut acts);
        self.command_bar(ui, &mut acts);
        self.filter_bar(ui, &mut acts);

        if let Some(err) = &self.error {
            ui.colored_label(theme::RED, err);
        }
        if let Some(status) = &self.status {
            ui.label(RichText::new(status).color(theme::TAN).size(13.0));
        }
        ui.separator();
        self.preview_panel(ui);

        if let Some(prompt) = self.confirm.clone() {
            self.confirm_modal(ui, &prompt, &mut acts);
        }

        for act in acts {
            self.apply(store, act);
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
                if let Some(t) = &self.target
                    && !self.repos.contains(t)
                {
                    self.target = None;
                }
                self.loaded = true;
                self.error = None;
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    fn repo_rows(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("SOURCE").color(theme::TEXT).size(12.0));
            for name in &self.repos {
                let sel = self.source.as_deref() == Some(name.as_str());
                let fill = if sel { theme::ORANGE } else { theme::PANEL };
                let col = if sel { theme::BLACK } else { theme::TEXT };
                if ui
                    .add(egui::Button::new(RichText::new(name).color(col)).fill(fill))
                    .clicked()
                {
                    acts.push(Act::PickSource(name.clone()));
                }
            }
            if ui
                .button(RichText::new(icon::REFRESH).color(theme::BLACK))
                .clicked()
            {
                acts.push(Act::Reload);
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("TARGET").color(theme::TEXT).size(12.0));
            for name in &self.repos {
                // The target is chosen from the repos that are not the source.
                if self.source.as_deref() == Some(name.as_str()) {
                    continue;
                }
                let sel = self.target.as_deref() == Some(name.as_str());
                let fill = if sel { theme::BLUE } else { theme::PANEL };
                let col = if sel { theme::BLACK } else { theme::BLUE };
                if ui
                    .add(egui::Button::new(RichText::new(name).color(col)).fill(fill))
                    .clicked()
                {
                    acts.push(Act::PickTarget(name.clone()));
                }
            }
        });
    }

    fn command_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        ui.horizontal(|ui| {
            for cmd in [Command::Copy, Command::Move, Command::Delete] {
                let sel = self.command == cmd;
                let fill = if sel {
                    if cmd.destructive() {
                        theme::RED
                    } else {
                        theme::AMBER
                    }
                } else {
                    theme::PANEL
                };
                if ui
                    .add(
                        egui::Button::new(RichText::new(cmd.label()).color(theme::BLACK))
                            .fill(fill),
                    )
                    .clicked()
                {
                    acts.push(Act::SetCommand(cmd));
                }
            }
            ui.separator();
            let ready = self.source.is_some() && self.target.is_some() && !self.running;
            if ui
                .add_enabled(
                    ready,
                    egui::Button::new(RichText::new("PREVIEW").color(theme::BLACK)),
                )
                .clicked()
            {
                acts.push(Act::Preview);
            }
            let run =
                egui::Button::new(RichText::new("RUN").color(theme::BLACK)).fill(theme::AMBER);
            if ui.add_enabled(ready, run).clicked() {
                acts.push(Act::Ask);
            }
            if self.running {
                ui.add(egui::Spinner::new().color(theme::AMBER));
                if ui
                    .add(
                        egui::Button::new(RichText::new("CANCEL").color(theme::BLACK))
                            .fill(theme::RED),
                    )
                    .clicked()
                {
                    acts.push(Act::CancelRun);
                }
            }
        });
        self.hint(ui);
    }

    fn hint(&self, ui: &mut egui::Ui) {
        let text = match self.command {
            Command::Copy => "Copy source files the target does not have into the target repo.",
            Command::Move => {
                "Move source files the target does not have (source entries marked missing)."
            }
            Command::Delete => "Delete source files whose content the target already has.",
        };
        ui.label(RichText::new(text).color(theme::LILAC).size(11.0));
    }

    fn filter_bar(&mut self, ui: &mut egui::Ui, _acts: &mut [Act]) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("FILTER").color(theme::TEXT).size(12.0));
            for (kind, label) in [
                (FilterKind::All, "ALL"),
                (FilterKind::Mime, "MIME"),
                (FilterKind::Name, "NAME"),
                (FilterKind::Size, "SIZE"),
            ] {
                let sel = self.filter_kind == kind;
                let fill = if sel { theme::LILAC } else { theme::PANEL };
                let col = if sel { theme::BLACK } else { theme::LILAC };
                if ui
                    .add(egui::Button::new(RichText::new(label).color(col)).fill(fill))
                    .clicked()
                {
                    self.filter_kind = kind;
                }
            }
            if self.filter_kind != FilterKind::All {
                let hint = match self.filter_kind {
                    FilterKind::Mime => "image/  ·  text/plain",
                    FilterKind::Name => "substring of the path",
                    FilterKind::Size => ">=1000  ·  <500  ·  =0",
                    FilterKind::All => "",
                };
                ui.add(
                    egui::TextEdit::singleline(&mut self.filter_value)
                        .desired_width(240.0)
                        .hint_text(hint),
                );
            }
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
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&row.from).color(theme::TEXT).size(12.0));
                        ui.label(RichText::new(icon::ARROW_RIGHT).color(theme::ORANGE));
                        ui.label(RichText::new(&row.to).color(theme::BLUE).size(12.0));
                    });
                }
            });
    }

    fn confirm_modal(&mut self, ui: &mut egui::Ui, prompt: &str, acts: &mut Vec<Act>) {
        egui::Modal::new(Id::new("files-confirm")).show(&ui.ctx().clone(), |ui| {
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
                let fill = if self.command.destructive() {
                    theme::RED
                } else {
                    theme::AMBER
                };
                if ui
                    .add(egui::Button::new(RichText::new("PROCEED").color(theme::BLACK)).fill(fill))
                    .clicked()
                {
                    acts.push(Act::Confirm);
                }
                if ui
                    .button(RichText::new("CANCEL").color(theme::BLACK))
                    .clicked()
                {
                    acts.push(Act::CancelConfirm);
                }
            });
        });
    }

    fn apply(&mut self, store: &Arc<Store>, act: Act) {
        match act {
            Act::PickSource(name) => {
                if self.target.as_deref() == Some(name.as_str()) {
                    self.target = None;
                }
                self.source = Some(name);
                self.clear_preview();
            }
            Act::PickTarget(name) => {
                self.target = Some(name);
                self.clear_preview();
            }
            Act::SetCommand(cmd) => {
                self.command = cmd;
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

    fn filter_string(&self) -> Option<String> {
        let value = self.filter_value.trim();
        match self.filter_kind {
            FilterKind::All => None,
            FilterKind::Mime => Some(format!("mime:{value}")),
            FilterKind::Name => Some(format!("name:{value}")),
            FilterKind::Size => Some(format!("size:{value}")),
        }
    }

    fn run_preview(&mut self, store: &Store) {
        let (Some(source), Some(target)) = (self.source.clone(), self.target.clone()) else {
            return;
        };
        let filter = self.filter_string();
        match diff_print(store, &source, &target, filter.as_deref()) {
            Ok(items) => {
                let deleting = self.command == Command::Delete;
                let matched: Vec<&DiffItem> = items
                    .iter()
                    .filter(|item| match item {
                        // Copy/Move act on content the target lacks (New).
                        DiffItem::New { .. } => !deleting,
                        // Delete acts on content the target already knows.
                        DiffItem::Equal { .. } | DiffItem::DeletedInReference { .. } => deleting,
                    })
                    .collect();
                self.preview_total = matched.len();
                self.preview = matched
                    .into_iter()
                    .take(PREVIEW_LIMIT)
                    .map(|item| self.preview_row(&source, &target, item))
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

    fn preview_row(&self, source: &str, target: &str, item: &DiffItem) -> PreviewRow {
        match item {
            DiffItem::New { rel_path } => PreviewRow {
                from: format!("{source}/{rel_path}"),
                to: format!("{target}/{rel_path}"),
            },
            DiffItem::Equal { rel_path, .. } | DiffItem::DeletedInReference { rel_path } => {
                PreviewRow {
                    from: format!("{source}/{rel_path}"),
                    to: "✗ delete (already in target)".to_string(),
                }
            }
        }
    }

    fn build_prompt(&mut self, store: &Store) -> Option<String> {
        // Refresh the count so the confirmation reflects the current filter.
        self.run_preview(store);
        let (source, target) = (self.source.as_ref()?, self.target.as_ref()?);
        Some(match self.command {
            Command::Copy => format!(
                "Copy {} file(s) from '{source}' into '{target}'?",
                self.preview_total
            ),
            Command::Move => format!(
                "Move {} file(s) from '{source}' into '{target}'? Source entries will be marked missing.",
                self.preview_total
            ),
            Command::Delete => format!(
                "Delete {} file(s) from '{source}' that already exist in '{target}'? This cannot be undone.",
                self.preview_total
            ),
        })
    }

    fn start(&mut self, store: &Arc<Store>) {
        let (Some(source), Some(target)) = (self.source.clone(), self.target.clone()) else {
            return;
        };
        let filter = self.filter_string();
        let command = self.command;
        let store = Arc::clone(store);
        let tx = self.tx.clone();
        self.cancel = CancellationToken::new();
        let cancel = self.cancel.clone();
        self.running = true;
        self.status = Some(format!("{}…", command.label().to_lowercase()));

        std::thread::spawn(move || {
            let result = match command {
                Command::Copy | Command::Move => {
                    let move_files = command == Command::Move;
                    match store.get_repo(&target) {
                        Ok(meta) => {
                            let target_dir = std::path::PathBuf::from(meta.abs_path);
                            match diff_copy(
                                &store,
                                &source,
                                &target,
                                &target_dir,
                                move_files,
                                filter.as_deref(),
                                &cancel,
                            ) {
                                Ok(s) => OpResult::Copied {
                                    copied: s.copied,
                                    cancelled: s.cancelled,
                                    moved: move_files,
                                },
                                Err(e) => OpResult::Error(e.to_string()),
                            }
                        }
                        Err(e) => OpResult::Error(e.to_string()),
                    }
                }
                Command::Delete => {
                    match diff_delete(&store, &source, &target, filter.as_deref(), &cancel) {
                        Ok(s) => OpResult::Deleted {
                            deleted: s.deleted,
                            cancelled: s.cancelled,
                        },
                        Err(e) => OpResult::Error(e.to_string()),
                    }
                }
            };
            let _ = tx.send(result);
        });
    }

    fn drain(&mut self, ui: &egui::Ui) {
        let mut got = false;
        while let Ok(result) = self.rx.try_recv() {
            got = true;
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
                OpResult::Deleted { deleted, cancelled } => {
                    self.status = Some(format!(
                        "Deleted {deleted} file(s){}.",
                        if cancelled { " (cancelled)" } else { "" }
                    ));
                    self.error = None;
                }
                OpResult::Error(e) => self.error = Some(e),
            }
            self.clear_preview();
        }
        if got || self.running {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(100));
        }
    }
}
