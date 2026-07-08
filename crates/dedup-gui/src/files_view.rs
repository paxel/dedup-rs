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
use dedup_core::diff::{CopyDest, DiffItem, diff_copy, diff_delete, diff_print};
use dedup_core::store::Store;
use dedup_core::update::CancellationToken;
use egui::{Id, RichText};
use std::path::PathBuf;
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
    subdir: String,
    subdir_tx: Sender<Result<String, String>>,
    subdir_rx: Receiver<Result<String, String>>,
    filter_mime: String,
    filter_name: String,
    filter_size: String,
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
    SubdirChanged,
    BrowseSubdir,
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
        let (subdir_tx, subdir_rx) = crossbeam_channel::unbounded();
        Self {
            repos: Vec::new(),
            loaded: false,
            source: None,
            target: None,
            command: Command::Copy,
            subdir: String::new(),
            subdir_tx,
            subdir_rx,
            filter_mime: String::new(),
            filter_name: String::new(),
            filter_size: String::new(),
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
        self.subdir_bar(ui, &mut acts);
        self.filter_bar(ui, &mut acts);
        self.action_bar(ui, &mut acts);

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

        let ctx = ui.ctx().clone();
        for act in acts {
            self.apply(store, &ctx, act);
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
        theme::section(theme::LILAC).show(ui, |ui| {
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
        });
    }

    fn command_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        theme::section(theme::ORANGE).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("COMMAND").color(theme::TEXT).size(12.0));
                for cmd in [Command::Copy, Command::Move, Command::Delete] {
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
                    if ui
                        .add(egui::Button::new(RichText::new(cmd.label()).color(col)).fill(fill))
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
        // The relative target subdirectory only applies to copy/move; delete
        // never writes into the target, so the group is hidden there.
        if self.command == Command::Delete {
            return;
        }
        theme::section(theme::BLUE).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("INTO").color(theme::TEXT).size(12.0));
                let changed = ui
                    .add(
                        egui::TextEdit::singleline(&mut self.subdir)
                            .desired_width(220.0)
                            .hint_text("relative/subdir (optional)"),
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

    fn hint(&self, ui: &mut egui::Ui) {
        let text = match self.command {
            Command::Copy => "Copy source files the target does not have into the target repo.",
            Command::Move => {
                "Move source files the target does not have into the target repo \
                 (they are removed from the source directory)."
            }
            Command::Delete => "Delete source files whose content the target already has.",
        };
        ui.label(RichText::new(text).color(theme::LILAC).size(11.0));
    }

    fn filter_bar(&mut self, ui: &mut egui::Ui, _acts: &mut [Act]) {
        theme::section(theme::LILAC).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("FILTER").color(theme::TEXT).size(12.0));

                ui.label(RichText::new("MIME:").color(theme::LILAC).size(11.0));
                ui.add(
                    egui::TextEdit::singleline(&mut self.filter_mime)
                        .desired_width(100.0)
                        .hint_text("image/"),
                );

                ui.label(RichText::new("NAME:").color(theme::LILAC).size(11.0));
                ui.add(
                    egui::TextEdit::singleline(&mut self.filter_name)
                        .desired_width(120.0)
                        .hint_text("substring"),
                );

                ui.label(RichText::new("SIZE:").color(theme::LILAC).size(11.0));
                ui.add(
                    egui::TextEdit::singleline(&mut self.filter_size)
                        .desired_width(100.0)
                        .hint_text(">=1000"),
                );

                let has_any = !self.filter_mime.is_empty()
                    || !self.filter_name.is_empty()
                    || !self.filter_size.is_empty();
                if has_any {
                    let fill = theme::RED;
                    let col = theme::BLACK;
                    if ui
                        .add(egui::Button::new(RichText::new("CLEAR").color(col)).fill(fill))
                        .clicked()
                    {
                        self.filter_mime.clear();
                        self.filter_name.clear();
                        self.filter_size.clear();
                    }
                }
            });
        });
    }

    fn action_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        theme::section(theme::AMBER).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("ACTION").color(theme::TEXT).size(12.0));
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
                egui::Grid::new("preview_grid")
                    .num_columns(3)
                    .striped(true)
                    .spacing(egui::vec2(12.0, 4.0))
                    .show(ui, |ui| {
                        for row in &self.preview {
                            ui.label(RichText::new(&row.from).color(theme::TEXT).size(12.0));
                            ui.label(RichText::new(icon::ARROW_RIGHT).color(theme::ORANGE));
                            ui.label(RichText::new(&row.to).color(theme::BLUE).size(12.0));
                            ui.end_row();
                        }
                    });
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

    fn apply(&mut self, store: &Arc<Store>, ctx: &egui::Context, act: Act) {
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
            Act::SubdirChanged => self.clear_preview(),
            Act::BrowseSubdir => self.browse_subdir(store, ctx),
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

    fn filter_string(&self) -> Option<String> {
        let mut parts = Vec::new();
        let mime = self.filter_mime.trim();
        if !mime.is_empty() {
            parts.push(format!("mime:{mime}"));
        }
        let name = self.filter_name.trim();
        if !name.is_empty() {
            parts.push(format!("name:{name}"));
        }
        let size = self.filter_size.trim();
        if !size.is_empty() {
            parts.push(format!("size:{size}"));
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(" "))
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
                }
            }
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
        let subdir = self.normalized_subdir();
        let dest = if subdir.is_empty() {
            target.to_string()
        } else {
            format!("{target}/{subdir}")
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
        let subdir = self.normalized_subdir();
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
                            let subdir = if subdir.is_empty() {
                                None
                            } else {
                                Some(subdir.as_str())
                            };
                            match diff_copy(
                                &store,
                                &source,
                                &target,
                                CopyDest {
                                    dir: &target_dir,
                                    subdir,
                                },
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
        // Apply any folder picked by the native subdir dialog thread.
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
