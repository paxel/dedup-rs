//! The eframe application: a tabbed LCARS shell (Repository / Duplicate / File
//! management) wired to the core store and a background update worker. This
//! module owns the Repository Management tab and delegates the other two to
//! [`crate::dupes_view`] and [`crate::files_view`].

use crate::dupes_view::DupesView;
use crate::files_view::FilesView;
use crate::icon;
use crate::theme;
use crate::util::format_size;
use crate::worker::{ChannelProgress, WorkerMsg, WorkerState};
use crossbeam_channel::{Receiver, Sender};
use dedup_core::store::{RepoStats, Store};
use dedup_core::update::{CancellationToken, ProgressEvent, update_repo};
use egui::{Align, Color32, Id, Layout, RichText};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(PartialEq, Eq, Clone, Copy)]
enum Tab {
    Repositories,
    Duplicates,
    Files,
}

/// A snapshot of one registered repo for rendering.
#[derive(Clone)]
struct RepoRow {
    name: String,
    path: String,
    stats: RepoStats,
    /// MIME distribution (`mime → count`), sorted by count descending.
    mimes: Vec<(String, u64)>,
    /// Result of the most recent update, if any (for a one-line status).
    last: Option<String>,
}

/// In-progress inline edit for a repo row.
enum Edit {
    Rename {
        name: String,
        buf: String,
    },
    Relocate {
        name: String,
        buf: String,
    },
    Duplicate {
        name: String,
        dest: String,
        path: String,
    },
    ConfirmDelete {
        name: String,
    },
}

/// A deferred mutation collected during rendering and applied after the UI pass
/// so the immediate-mode closures never borrow `self` mutably twice.
enum Action {
    Update(String),
    Cancel(String),
    BeginRename(String),
    BeginRelocate(String),
    BeginDuplicate(String),
    BeginDelete(String),
    CommitRename(String, String),
    CommitRelocate(String, String),
    CommitDuplicate {
        source: String,
        dest: String,
        path: String,
    },
    CommitDelete(String),
    CancelEdit,
    OpenAdd,
    CloseAdd,
    ChooseFolder,
    Create,
}

pub struct DedupApp {
    store: Arc<Store>,
    tab: Tab,
    repos: Vec<RepoRow>,
    load_error: Option<String>,

    show_add: bool,
    new_name: String,
    new_path: String,
    form_error: Option<String>,
    /// Native folder-picker results delivered from a background thread.
    folder_tx: Sender<PathBuf>,
    folder_rx: Receiver<PathBuf>,
    edit: Option<Edit>,

    show_settings: bool,
    threads: usize,

    tx: Sender<WorkerMsg>,
    rx: Receiver<WorkerMsg>,
    worker: WorkerState,
    cancels: HashMap<String, CancellationToken>,
    dupes: DupesView,
    files: FilesView,
}

impl DedupApp {
    pub fn new(store: Arc<Store>) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        let (folder_tx, folder_rx) = crossbeam_channel::unbounded();
        let mut app = Self {
            store,
            tab: Tab::Repositories,
            repos: Vec::new(),
            load_error: None,
            show_add: false,
            new_name: String::new(),
            new_path: String::new(),
            form_error: None,
            folder_tx,
            folder_rx,
            edit: None,
            show_settings: false,
            threads: 0,
            tx,
            rx,
            worker: WorkerState::default(),
            cancels: HashMap::new(),
            dupes: DupesView::new(),
            files: FilesView::new(),
        };
        app.reload_all();
        app
    }

    /// Reload every repo row from the registry. Safe only when no update is
    /// running (it opens each repo db to read stats); callers gate on that.
    fn reload_all(&mut self) {
        match self.store.list_repos() {
            Ok(list) => {
                let prev: HashMap<String, Option<String>> =
                    self.repos.drain(..).map(|r| (r.name, r.last)).collect();
                let mut rows = Vec::with_capacity(list.len());
                for (name, meta, stats) in list {
                    let last = prev.get(&name).cloned().flatten();
                    let mimes = self.store.get_mime_stats(&name).unwrap_or_default();
                    rows.push(RepoRow {
                        name,
                        path: meta.abs_path,
                        stats,
                        mimes,
                        last,
                    });
                }
                self.repos = rows;
                self.load_error = None;
            }
            Err(e) => self.load_error = Some(e.to_string()),
        }
    }

    /// Refresh a single repo's stats — used right after its update finishes,
    /// when its db is released again.
    fn refresh_repo(&mut self, name: &str) {
        let stats = self.store.get_repo_stats(name).ok();
        let mimes = self.store.get_mime_stats(name).ok();
        if let Some(row) = self.repos.iter_mut().find(|r| r.name == name) {
            if let Some(stats) = stats {
                row.stats = stats;
            }
            if let Some(mimes) = mimes {
                row.mimes = mimes;
            }
        }
    }

    fn start_update(&mut self, ctx: &egui::Context, name: String) {
        if self.worker.is_active(&name) {
            return;
        }
        let cancel = CancellationToken::new();
        self.cancels.insert(name.clone(), cancel.clone());
        self.worker.mark_started(&name);
        if let Some(row) = self.repos.iter_mut().find(|r| r.name == name) {
            row.last = None;
        }

        let store = Arc::clone(&self.store);
        let tx = self.tx.clone();
        let threads = self.threads;
        let repaint = ctx.clone();
        std::thread::spawn(move || {
            let progress = ChannelProgress::new(name.clone(), tx.clone());
            let outcome =
                update_repo(&store, &name, threads, &progress, &cancel).map_err(|e| e.to_string());
            let _ = tx.send(WorkerMsg::Completed {
                repo: name,
                outcome,
            });
            repaint.request_repaint();
        });
    }

    fn apply(&mut self, ctx: &egui::Context, action: Action) {
        match action {
            Action::Update(name) => self.start_update(ctx, name),
            Action::Cancel(name) => {
                if let Some(token) = self.cancels.get(&name) {
                    token.cancel();
                }
            }
            Action::BeginRename(name) => {
                self.edit = Some(Edit::Rename {
                    buf: name.clone(),
                    name,
                });
            }
            Action::BeginRelocate(name) => {
                let buf = self
                    .repos
                    .iter()
                    .find(|r| r.name == name)
                    .map(|r| r.path.clone())
                    .unwrap_or_default();
                self.edit = Some(Edit::Relocate { name, buf });
            }
            Action::BeginDuplicate(name) => {
                self.edit = Some(Edit::Duplicate {
                    dest: format!("{name}-copy"),
                    path: String::new(),
                    name,
                });
            }
            Action::BeginDelete(name) => self.edit = Some(Edit::ConfirmDelete { name }),
            Action::CancelEdit => self.edit = None,
            Action::CommitRename(name, new_name) => {
                self.edit = None;
                if !new_name.is_empty() && new_name != name {
                    if let Err(e) = self.store.rename_repo(&name, &new_name) {
                        self.load_error = Some(e.to_string());
                    }
                    self.reload_all();
                }
            }
            Action::CommitRelocate(name, new_path) => {
                self.edit = None;
                if !new_path.is_empty()
                    && let Err(e) = self.store.relocate_repo(&name, &new_path)
                {
                    self.load_error = Some(e.to_string());
                }
                self.reload_all();
            }
            Action::CommitDuplicate { source, dest, path } => {
                self.edit = None;
                if dest.is_empty() || path.is_empty() {
                    self.load_error = Some("Duplicate needs a new name and path.".into());
                } else {
                    if let Err(e) = self.store.duplicate_repo(&source, &dest, &path) {
                        self.load_error = Some(e.to_string());
                    }
                    self.reload_all();
                }
            }
            Action::CommitDelete(name) => {
                self.edit = None;
                if let Err(e) = self.store.remove_repo(&name) {
                    self.load_error = Some(e.to_string());
                }
                self.reload_all();
            }
            Action::OpenAdd => {
                self.show_add = true;
                self.new_name.clear();
                self.new_path.clear();
                self.form_error = None;
            }
            Action::CloseAdd => {
                self.show_add = false;
                self.form_error = None;
            }
            Action::ChooseFolder => {
                let tx = self.folder_tx.clone();
                let repaint = ctx.clone();
                std::thread::spawn(move || {
                    if let Some(dir) = rfd::FileDialog::new()
                        .set_title("Choose a folder")
                        .pick_folder()
                    {
                        let _ = tx.send(dir);
                        repaint.request_repaint();
                    }
                });
            }
            Action::Create => {
                let path = self.new_path.trim().to_string();
                // Default the name to the folder's own name when left blank.
                let name = effective_name(&self.new_name, &self.new_path);
                if name.is_empty() || path.is_empty() {
                    self.form_error = Some("A folder is required.".into());
                } else if let Err(e) = self.store.create_repo(&name, &path) {
                    self.form_error = Some(e.to_string());
                } else {
                    self.new_name.clear();
                    self.new_path.clear();
                    self.form_error = None;
                    self.show_add = false;
                    self.reload_all();
                }
            }
        }
    }
}

impl eframe::App for DedupApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // Drain worker messages; refresh stats for anything that just finished.
        for (repo, outcome) in self.worker.drain(&self.rx) {
            self.cancels.remove(&repo);
            let summary = match outcome {
                Ok(s) if s.cancelled => {
                    format!("cancelled — added {}, updated {}", s.added, s.updated)
                }
                Ok(s) => format!(
                    "added {}, updated {}, unchanged {}, missing {}, errors {}",
                    s.added, s.updated, s.unchanged, s.marked_missing, s.errors
                ),
                Err(e) => format!("error: {e}"),
            };
            self.refresh_repo(&repo);
            if let Some(row) = self.repos.iter_mut().find(|r| r.name == repo) {
                row.last = Some(summary);
            }
        }

        // Folder-picker results: fill the path and auto-name from the last path
        // component unless the user already typed a name.
        while let Ok(dir) = self.folder_rx.try_recv() {
            if self.new_name.trim().is_empty()
                && let Some(base) = dir.file_name()
            {
                self.new_name = base.to_string_lossy().into_owned();
            }
            self.new_path = dir.to_string_lossy().into_owned();
            ctx.request_repaint();
        }

        let mut actions: Vec<Action> = Vec::new();
        self.top_bar(ui);
        egui::CentralPanel::default().show(ui, |ui| match self.tab {
            Tab::Repositories => self.repositories_view(ui, &mut actions),
            Tab::Duplicates => self.dupes.show(ui, &self.store),
            Tab::Files => self.files.show(ui, &self.store),
        });
        if self.show_settings {
            self.settings_modal(&ctx);
        }
        if self.show_add {
            self.add_modal(&ctx, &mut actions);
        }
        for action in actions {
            self.apply(&ctx, action);
        }

        // Poll at ~10 Hz while work is running instead of repainting per event.
        if self.worker.active_count() > 0 {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }
}

impl DedupApp {
    fn top_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("top")
            .exact_size(56.0)
            .frame(egui::Frame::new().fill(theme::BLACK).inner_margin(8.0))
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(
                        RichText::new("DEDUP")
                            .color(theme::ORANGE)
                            .size(26.0)
                            .strong(),
                    );
                    ui.add_space(6.0);
                    ui.label(RichText::new("LCARS 47").color(theme::LILAC).size(13.0));
                    ui.add_space(16.0);
                    tab_button(
                        ui,
                        &mut self.tab,
                        Tab::Repositories,
                        "REPOSITORIES",
                        theme::ORANGE,
                    );
                    tab_button(
                        ui,
                        &mut self.tab,
                        Tab::Duplicates,
                        "DUPLICATES",
                        theme::LILAC,
                    );
                    tab_button(ui, &mut self.tab, Tab::Files, "FILES", theme::BLUE);

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add(egui::Button::new(
                                RichText::new(format!("{} SETTINGS", icon::GEAR))
                                    .color(theme::BLACK),
                            ))
                            .clicked()
                        {
                            self.show_settings = true;
                        }
                    });
                });
            });
    }

    fn repositories_view(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        ui.add_space(6.0);
        ui.label(
            RichText::new("REPOSITORY MANAGEMENT")
                .color(theme::AMBER)
                .size(18.0)
                .strong(),
        );
        ui.add_space(4.0);

        if let Some(err) = &self.load_error {
            ui.colored_label(theme::RED, err);
        }

        // The registry is locked while any repo is updating, so adding a repo
        // (which reads every repo's stats) must wait until scans finish.
        let busy = self.worker.active_count() > 0;
        ui.horizontal(|ui| {
            let add = egui::Button::new(
                RichText::new(format!("{} ADD REPOSITORY", icon::PLUS)).color(theme::BLACK),
            )
            .fill(theme::BLUE);
            if ui.add_enabled(!busy, add).clicked() {
                actions.push(Action::OpenAdd);
            }
            if busy {
                ui.label(
                    RichText::new("· busy: a scan is running")
                        .color(theme::TAN)
                        .size(12.0),
                );
            }
        });
        ui.add_space(4.0);

        let rows = self.repos.clone();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if rows.is_empty() {
                    ui.add_space(8.0);
                    ui.colored_label(theme::TEXT, "No repositories yet — use ADD REPOSITORY.");
                }
                for row in &rows {
                    self.repo_card(ui, row, actions);
                }
            });
    }

    fn repo_card(&mut self, ui: &mut egui::Ui, row: &RepoRow, actions: &mut Vec<Action>) {
        let active = self.worker.is_active(&row.name);
        egui::Frame::new()
            .fill(theme::PANEL)
            .corner_radius(theme::PILL)
            .stroke(egui::Stroke::new(1.5, theme::ORANGE))
            .inner_margin(12.0)
            .outer_margin(egui::Margin {
                left: 0,
                right: 0,
                top: 0,
                bottom: 10,
            })
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(&row.name)
                            .color(theme::AMBER)
                            .size(17.0)
                            .strong(),
                    );
                    ui.label(RichText::new(&row.path).color(theme::TEXT).size(12.0))
                        .on_hover_text(&row.path);
                    // MIME breakdown, share-sorted, pinned to the top-right.
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        mime_tags(ui, row);
                    });
                });
                ui.horizontal(|ui| {
                    stat(
                        ui,
                        "FILES",
                        &row.stats.file_count.to_string(),
                        theme::ORANGE,
                        "Indexed files (missing files excluded)",
                    );
                    stat(
                        ui,
                        "SIZE",
                        &format_size(row.stats.total_size),
                        theme::BLUE,
                        "Total size of indexed files",
                    );
                    stat(
                        ui,
                        "MISSING",
                        &row.stats.missing_count.to_string(),
                        theme::LILAC,
                        "Indexed before but no longer on disk",
                    );
                    stat(
                        ui,
                        "SCANNED",
                        &format_last_scan(row.stats.last_scan_ms),
                        theme::TAN,
                        "When this repository was last scanned",
                    );
                });

                if active {
                    let line = self
                        .worker
                        .progress(&row.name)
                        .map(progress_line)
                        .unwrap_or_else(|| "working…".into());
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().color(theme::AMBER));
                        ui.label(RichText::new(line).color(theme::AMBER));
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new(format!("{} CANCEL", icon::X))
                                        .color(theme::BLACK),
                                )
                                .fill(theme::RED),
                            )
                            .on_hover_text("Stop the scan (already-hashed files stay indexed)")
                            .clicked()
                        {
                            actions.push(Action::Cancel(row.name.clone()));
                        }
                    });
                } else {
                    self.card_controls(ui, row, actions);
                    if let Some(last) = &row.last {
                        ui.label(RichText::new(last).color(theme::TAN).size(12.0));
                    }
                }
            });
    }

    fn card_controls(&mut self, ui: &mut egui::Ui, row: &RepoRow, actions: &mut Vec<Action>) {
        // Inline editors take over the row when active for this repo.
        match &mut self.edit {
            Some(Edit::Rename { name, buf }) if *name == row.name => {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("RENAME →").color(theme::LILAC));
                    ui.text_edit_singleline(buf);
                    if ui
                        .button(RichText::new(format!("{} OK", icon::CHECK)).color(theme::BLACK))
                        .clicked()
                    {
                        actions.push(Action::CommitRename(name.clone(), buf.trim().to_string()));
                    }
                    if ui
                        .button(RichText::new(icon::X).color(theme::BLACK))
                        .clicked()
                    {
                        actions.push(Action::CancelEdit);
                    }
                });
                return;
            }
            Some(Edit::Relocate { name, buf }) if *name == row.name => {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("RELOCATE →").color(theme::LILAC));
                    ui.text_edit_singleline(buf);
                    if ui
                        .button(RichText::new(format!("{} OK", icon::CHECK)).color(theme::BLACK))
                        .clicked()
                    {
                        actions.push(Action::CommitRelocate(name.clone(), buf.trim().to_string()));
                    }
                    if ui
                        .button(RichText::new(icon::X).color(theme::BLACK))
                        .clicked()
                    {
                        actions.push(Action::CancelEdit);
                    }
                });
                return;
            }
            Some(Edit::Duplicate { name, dest, path }) if *name == row.name => {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("COPY → NAME").color(theme::LILAC));
                    ui.add(egui::TextEdit::singleline(dest).desired_width(140.0));
                    ui.label(RichText::new("PATH").color(theme::LILAC));
                    ui.add(
                        egui::TextEdit::singleline(path)
                            .desired_width(240.0)
                            .hint_text("/new/repo/path"),
                    );
                    if ui
                        .button(RichText::new(format!("{} OK", icon::CHECK)).color(theme::BLACK))
                        .clicked()
                    {
                        actions.push(Action::CommitDuplicate {
                            source: name.clone(),
                            dest: dest.trim().to_string(),
                            path: path.trim().to_string(),
                        });
                    }
                    if ui
                        .button(RichText::new(icon::X).color(theme::BLACK))
                        .clicked()
                    {
                        actions.push(Action::CancelEdit);
                    }
                });
                return;
            }
            Some(Edit::ConfirmDelete { name }) if *name == row.name => {
                ui.horizontal(|ui| {
                    ui.colored_label(theme::RED, format!("Delete '{name}' and its index?"));
                    if ui
                        .add(
                            egui::Button::new(RichText::new("DELETE").color(theme::BLACK))
                                .fill(theme::RED),
                        )
                        .clicked()
                    {
                        actions.push(Action::CommitDelete(name.clone()));
                    }
                    if ui
                        .button(RichText::new("KEEP").color(theme::BLACK))
                        .clicked()
                    {
                        actions.push(Action::CancelEdit);
                    }
                });
                return;
            }
            _ => {}
        }

        ui.horizontal(|ui| {
            if ui
                .add(egui::Button::new(
                    RichText::new(format!("{} UPDATE / SCAN", icon::REFRESH)).color(theme::BLACK),
                ))
                .on_hover_text("Scan the folder and index new or changed files")
                .clicked()
            {
                actions.push(Action::Update(row.name.clone()));
            }
            if ui
                .button(RichText::new(format!("{} RENAME", icon::PENCIL)).color(theme::BLACK))
                .on_hover_text("Rename this repository")
                .clicked()
            {
                actions.push(Action::BeginRename(row.name.clone()));
            }
            if ui
                .button(RichText::new(format!("{} RELOCATE", icon::RELOCATE)).color(theme::BLACK))
                .on_hover_text("Point this repository at a different folder")
                .clicked()
            {
                actions.push(Action::BeginRelocate(row.name.clone()));
            }
            if ui
                .button(RichText::new(format!("{} DUPLICATE", icon::COPY)).color(theme::BLACK))
                .on_hover_text("Copy this repository's index into a new one at a new path")
                .clicked()
            {
                actions.push(Action::BeginDuplicate(row.name.clone()));
            }
            if ui
                .add(
                    egui::Button::new(
                        RichText::new(format!("{} DELETE", icon::TRASH)).color(theme::BLACK),
                    )
                    .fill(theme::RED),
                )
                .on_hover_text("Remove this repository and delete its index")
                .clicked()
            {
                actions.push(Action::BeginDelete(row.name.clone()));
            }
        });
    }

    fn add_modal(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        let busy = self.worker.active_count() > 0;
        // Effective name = what Create would use (typed name, or the folder's
        // own name when blank). Adding is blocked if it clashes with an existing
        // repo, so the user sees the problem before submitting.
        let effective = effective_name(&self.new_name, &self.new_path);
        let clashes = !effective.is_empty() && self.repos.iter().any(|r| r.name == effective);
        let has_path = !self.new_path.trim().is_empty();
        let can_add = !busy && has_path && !effective.is_empty() && !clashes;

        let response = egui::Modal::new(Id::new("add-repo")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.label(
                RichText::new("ADD REPOSITORY")
                    .color(theme::AMBER)
                    .size(18.0)
                    .strong(),
            );
            ui.add_space(8.0);

            ui.horizontal(|ui| {
                ui.label(RichText::new("FOLDER").color(theme::TEXT).size(12.0));
                if ui
                    .button(
                        RichText::new(format!("{} CHOOSE…", icon::FOLDER_OPEN)).color(theme::BLACK),
                    )
                    .clicked()
                {
                    actions.push(Action::ChooseFolder);
                }
                ui.add(
                    egui::TextEdit::singleline(&mut self.new_path)
                        .desired_width(300.0)
                        .hint_text("/path/to/folder"),
                );
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new("NAME  ").color(theme::TEXT).size(12.0));
                let mut name_edit = egui::TextEdit::singleline(&mut self.new_name)
                    .desired_width(300.0)
                    .hint_text("defaults to the folder name");
                if clashes {
                    name_edit = name_edit.text_color(theme::RED);
                }
                ui.add(name_edit);
            });

            if clashes {
                ui.add_space(4.0);
                ui.colored_label(
                    theme::RED,
                    format!("A repository named '{effective}' already exists."),
                );
            } else if let Some(err) = &self.form_error {
                ui.add_space(4.0);
                ui.colored_label(theme::RED, err);
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let add =
                    egui::Button::new(RichText::new("ADD").color(theme::BLACK)).fill(theme::BLUE);
                if ui.add_enabled(can_add, add).clicked() {
                    actions.push(Action::Create);
                }
                if ui
                    .button(RichText::new("CANCEL").color(theme::BLACK))
                    .clicked()
                {
                    actions.push(Action::CloseAdd);
                }
            });
        });
        if response.should_close() {
            actions.push(Action::CloseAdd);
        }
    }

    fn settings_modal(&mut self, ctx: &egui::Context) {
        let response = egui::Modal::new(Id::new("settings")).show(ctx, |ui| {
            ui.set_width(320.0);
            ui.label(
                RichText::new("SETTINGS")
                    .color(theme::AMBER)
                    .size(18.0)
                    .strong(),
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Hashing threads").color(theme::TEXT));
                ui.add(egui::DragValue::new(&mut self.threads).range(0..=64));
            });
            ui.label(
                RichText::new("0 = one thread per CPU core")
                    .color(theme::TAN)
                    .size(12.0),
            );
            ui.add_space(12.0);
            if ui
                .add(egui::Button::new(
                    RichText::new("CLOSE").color(theme::BLACK),
                ))
                .clicked()
            {
                self.show_settings = false;
            }
        });
        if response.should_close() {
            self.show_settings = false;
        }
    }
}

fn tab_button(ui: &mut egui::Ui, current: &mut Tab, tab: Tab, label: &str, color: Color32) {
    let selected = *current == tab;
    let fill = if selected { color } else { theme::PANEL };
    let text_color = if selected { theme::BLACK } else { color };
    if ui
        .add(egui::Button::new(RichText::new(label).color(text_color)).fill(fill))
        .clicked()
    {
        *current = tab;
    }
}

fn stat(ui: &mut egui::Ui, label: &str, value: &str, color: Color32, tip: &str) {
    ui.add_space(2.0);
    ui.label(
        RichText::new(format!("{label} "))
            .color(theme::TEXT)
            .size(12.0),
    )
    .on_hover_text(tip);
    ui.label(RichText::new(value).color(color).strong())
        .on_hover_text(tip);
    ui.add_space(10.0);
}

/// Last-scan timestamp as a short date, or "never".
fn format_last_scan(ms: u64) -> String {
    if ms == 0 {
        "never".to_string()
    } else {
        crate::util::format_mtime(i64::try_from(ms).unwrap_or(i64::MAX))
    }
}

/// Number of MIME tags shown on a repo card.
const MIME_TAG_LIMIT: usize = 5;

/// Render the repo's MIME distribution as the top few share-sorted tags, each in
/// a stable pastel color derived from the MIME name. Rendered inside a
/// right-to-left layout, so the largest share sits in the top-right corner.
fn mime_tags(ui: &mut egui::Ui, row: &RepoRow) {
    let total = row.stats.file_count;
    if total == 0 || row.mimes.is_empty() {
        return;
    }
    let extra = row.mimes.len().saturating_sub(MIME_TAG_LIMIT);
    if extra > 0 {
        ui.label(
            RichText::new(format!("+{extra}"))
                .color(theme::TEXT)
                .size(11.0),
        )
        .on_hover_text(format!("{extra} more MIME type(s)"));
    }
    for (mime, count) in row.mimes.iter().take(MIME_TAG_LIMIT) {
        egui::Frame::new()
            .fill(mime_color(mime))
            .corner_radius(6)
            .inner_margin(egui::Margin::symmetric(6, 2))
            .show(ui, |ui| {
                ui.label(
                    RichText::new(format!("{mime} {}", mime_pct(*count, total)))
                        .color(theme::BLACK)
                        .size(11.0),
                )
                .on_hover_text(format!("{count} file(s) · {mime}"));
            });
    }
}

/// A share as a percentage that never rounds a real value down to `0%`.
fn mime_pct(count: u64, total: u64) -> String {
    let pct = count as f64 / total as f64 * 100.0;
    if pct >= 1.0 {
        format!("{pct:.0}%")
    } else if pct >= 0.01 {
        format!("{pct:.2}%")
    } else {
        "<0.01%".to_string()
    }
}

/// A stable pastel color for a MIME type: the name is hashed to a hue, with
/// fixed saturation/lightness so every tag shares one cohesive palette.
fn mime_color(mime: &str) -> Color32 {
    let mut hash: u32 = 2166136261; // FNV-1a
    for b in mime.bytes() {
        hash ^= u32::from(b);
        hash = hash.wrapping_mul(16777619);
    }
    hsl_to_color((hash % 360) as f32, 0.50, 0.74)
}

fn hsl_to_color(h: f32, s: f32, l: f32) -> Color32 {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let to = |v: f32| (((v + m) * 255.0).round()).clamp(0.0, 255.0) as u8;
    Color32::from_rgb(to(r), to(g), to(b))
}

/// The repo name Create will use: the typed name, or the chosen folder's own
/// name when the name field is left blank.
fn effective_name(name: &str, path: &str) -> String {
    let name = name.trim();
    if !name.is_empty() {
        return name.to_string();
    }
    std::path::Path::new(path.trim())
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn progress_line(event: &ProgressEvent) -> String {
    match event {
        ProgressEvent::Scanning { files, dirs } => {
            format!("scanning — {files} files, {dirs} dirs")
        }
        ProgressEvent::Hashing { done, total, .. } => format!("hashing {done}/{total}"),
        ProgressEvent::Error { message, .. } => format!("warning: {message}"),
        ProgressEvent::Finished { .. } => "finishing…".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::effective_name;

    #[test]
    fn effective_name_prefers_typed_name() {
        assert_eq!(effective_name("photos", "/data/holiday"), "photos");
        assert_eq!(effective_name("  photos  ", "/data/holiday"), "photos");
    }

    #[test]
    fn effective_name_falls_back_to_folder_basename() {
        assert_eq!(effective_name("", "/data/holiday"), "holiday");
        assert_eq!(effective_name("   ", "/data/holiday/"), "holiday");
        assert_eq!(effective_name("", ""), "");
    }

    #[test]
    fn mime_pct_never_shows_bare_zero() {
        use super::mime_pct;
        assert_eq!(mime_pct(50, 100), "50%");
        assert_eq!(mime_pct(1, 100), "1%");
        assert_eq!(mime_pct(1, 1000), "0.10%");
        assert_eq!(mime_pct(1, 100_000), "<0.01%");
        // A real, present type is never rendered as "0%".
        for total in [1u64, 7, 999, 100_000, 10_000_000] {
            assert_ne!(mime_pct(1, total), "0%");
        }
    }

    #[test]
    fn mime_color_is_stable_per_name() {
        use super::mime_color;
        assert_eq!(mime_color("image/png"), mime_color("image/png"));
        assert_ne!(mime_color("image/png"), mime_color("application/pdf"));
    }
}
