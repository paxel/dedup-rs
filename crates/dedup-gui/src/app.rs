//! The eframe application: a tabbed LCARS shell whose Repository Management tab
//! is fully wired to the core store and background update worker. The Duplicate
//! and File tabs are placeholders for Phases 6 and 7.

use crate::theme;
use crate::worker::{ChannelProgress, WorkerMsg, WorkerState};
use crossbeam_channel::{Receiver, Sender};
use dedup_core::store::{RepoStats, Store};
use dedup_core::update::{CancellationToken, ProgressEvent, update_repo};
use egui::{Align, Color32, Id, Layout, RichText};
use std::collections::HashMap;
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
    Create,
}

pub struct DedupApp {
    store: Arc<Store>,
    tab: Tab,
    repos: Vec<RepoRow>,
    load_error: Option<String>,

    new_name: String,
    new_path: String,
    form_error: Option<String>,
    edit: Option<Edit>,

    show_settings: bool,
    threads: usize,

    tx: Sender<WorkerMsg>,
    rx: Receiver<WorkerMsg>,
    worker: WorkerState,
    cancels: HashMap<String, CancellationToken>,
}

impl DedupApp {
    pub fn new(store: Arc<Store>) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut app = Self {
            store,
            tab: Tab::Repositories,
            repos: Vec::new(),
            load_error: None,
            new_name: String::new(),
            new_path: String::new(),
            form_error: None,
            edit: None,
            show_settings: false,
            threads: 0,
            tx,
            rx,
            worker: WorkerState::default(),
            cancels: HashMap::new(),
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
                self.repos = list
                    .into_iter()
                    .map(|(name, meta, stats)| {
                        let last = prev.get(&name).cloned().flatten();
                        RepoRow {
                            name,
                            path: meta.abs_path,
                            stats,
                            last,
                        }
                    })
                    .collect();
                self.load_error = None;
            }
            Err(e) => self.load_error = Some(e.to_string()),
        }
    }

    /// Refresh a single repo's stats — used right after its update finishes,
    /// when its db is released again.
    fn refresh_repo(&mut self, name: &str) {
        if let Ok(stats) = self.store.get_repo_stats(name)
            && let Some(row) = self.repos.iter_mut().find(|r| r.name == name)
        {
            row.stats = stats;
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
            Action::Create => {
                let name = self.new_name.trim().to_string();
                let path = self.new_path.trim().to_string();
                if name.is_empty() || path.is_empty() {
                    self.form_error = Some("Name and path are required.".into());
                } else if let Err(e) = self.store.create_repo(&name, &path) {
                    self.form_error = Some(e.to_string());
                } else {
                    self.new_name.clear();
                    self.new_path.clear();
                    self.form_error = None;
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

        let mut actions: Vec<Action> = Vec::new();
        self.top_bar(ui);
        egui::CentralPanel::default().show(ui, |ui| match self.tab {
            Tab::Repositories => self.repositories_view(ui, &mut actions),
            Tab::Duplicates => placeholder(ui, "DUPLICATE MANAGEMENT", "Arrives in Phase 6."),
            Tab::Files => placeholder(ui, "FILE MANAGEMENT", "Arrives in Phase 7."),
        });
        if self.show_settings {
            self.settings_modal(&ctx);
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
                                RichText::new("⚙ SETTINGS").color(theme::BLACK),
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

        let rows = self.repos.clone();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if rows.is_empty() {
                    ui.add_space(8.0);
                    ui.colored_label(theme::TEXT, "No repositories yet. Add one below to begin.");
                }
                for row in &rows {
                    self.repo_card(ui, row, actions);
                }
                ui.add_space(12.0);
                self.add_form(ui, actions);
            });
    }

    fn repo_card(&mut self, ui: &mut egui::Ui, row: &RepoRow, actions: &mut Vec<Action>) {
        let active = self.worker.is_active(&row.name);
        egui::Frame::new()
            .fill(theme::PANEL)
            .corner_radius(theme::PILL)
            .inner_margin(12.0)
            .outer_margin(egui::Margin {
                left: 0,
                right: 0,
                top: 0,
                bottom: 8,
            })
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(&row.name)
                            .color(theme::AMBER)
                            .size(17.0)
                            .strong(),
                    );
                    ui.label(RichText::new(&row.path).color(theme::TEXT).size(12.0));
                });
                ui.horizontal(|ui| {
                    stat(
                        ui,
                        "FILES",
                        &row.stats.file_count.to_string(),
                        theme::ORANGE,
                    );
                    stat(ui, "SIZE", &format_size(row.stats.total_size), theme::BLUE);
                    stat(
                        ui,
                        "MISSING",
                        &row.stats.missing_count.to_string(),
                        theme::LILAC,
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
                                egui::Button::new(RichText::new("CANCEL").color(theme::BLACK))
                                    .fill(theme::RED),
                            )
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
                    if ui.button(RichText::new("OK").color(theme::BLACK)).clicked() {
                        actions.push(Action::CommitRename(name.clone(), buf.trim().to_string()));
                    }
                    if ui.button(RichText::new("×").color(theme::BLACK)).clicked() {
                        actions.push(Action::CancelEdit);
                    }
                });
                return;
            }
            Some(Edit::Relocate { name, buf }) if *name == row.name => {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("RELOCATE →").color(theme::LILAC));
                    ui.text_edit_singleline(buf);
                    if ui.button(RichText::new("OK").color(theme::BLACK)).clicked() {
                        actions.push(Action::CommitRelocate(name.clone(), buf.trim().to_string()));
                    }
                    if ui.button(RichText::new("×").color(theme::BLACK)).clicked() {
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
                    if ui.button(RichText::new("OK").color(theme::BLACK)).clicked() {
                        actions.push(Action::CommitDuplicate {
                            source: name.clone(),
                            dest: dest.trim().to_string(),
                            path: path.trim().to_string(),
                        });
                    }
                    if ui.button(RichText::new("×").color(theme::BLACK)).clicked() {
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
                    RichText::new("UPDATE / SCAN").color(theme::BLACK),
                ))
                .clicked()
            {
                actions.push(Action::Update(row.name.clone()));
            }
            if ui
                .button(RichText::new("RENAME").color(theme::BLACK))
                .clicked()
            {
                actions.push(Action::BeginRename(row.name.clone()));
            }
            if ui
                .button(RichText::new("RELOCATE").color(theme::BLACK))
                .clicked()
            {
                actions.push(Action::BeginRelocate(row.name.clone()));
            }
            if ui
                .button(RichText::new("DUPLICATE").color(theme::BLACK))
                .clicked()
            {
                actions.push(Action::BeginDuplicate(row.name.clone()));
            }
            if ui
                .add(
                    egui::Button::new(RichText::new("DELETE").color(theme::BLACK)).fill(theme::RED),
                )
                .clicked()
            {
                actions.push(Action::BeginDelete(row.name.clone()));
            }
        });
    }

    fn add_form(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        egui::Frame::new()
            .fill(theme::PANEL)
            .corner_radius(theme::PILL)
            .inner_margin(12.0)
            .show(ui, |ui| {
                ui.label(
                    RichText::new("＋ ADD REPOSITORY")
                        .color(theme::BLUE)
                        .strong(),
                );
                ui.horizontal(|ui| {
                    ui.label(RichText::new("NAME").color(theme::TEXT).size(12.0));
                    ui.text_edit_singleline(&mut self.new_name);
                });
                ui.horizontal(|ui| {
                    ui.label(RichText::new("PATH").color(theme::TEXT).size(12.0));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.new_path)
                            .desired_width(360.0)
                            .hint_text("/absolute/or/relative/path"),
                    );
                });
                if let Some(err) = &self.form_error {
                    ui.colored_label(theme::RED, err);
                }
                if ui
                    .add(
                        egui::Button::new(RichText::new("ADD").color(theme::BLACK))
                            .fill(theme::BLUE),
                    )
                    .clicked()
                {
                    actions.push(Action::Create);
                }
            });
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

fn stat(ui: &mut egui::Ui, label: &str, value: &str, color: Color32) {
    ui.add_space(2.0);
    ui.label(
        RichText::new(format!("{label} "))
            .color(theme::TEXT)
            .size(12.0),
    );
    ui.label(RichText::new(value).color(color).strong());
    ui.add_space(10.0);
}

fn placeholder(ui: &mut egui::Ui, title: &str, note: &str) {
    ui.add_space(40.0);
    ui.vertical_centered(|ui| {
        ui.label(RichText::new(title).color(theme::LILAC).size(24.0).strong());
        ui.add_space(8.0);
        ui.label(RichText::new(note).color(theme::TEXT));
    });
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

fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    const TB: u64 = GB * 1024;
    if bytes >= TB {
        format!("{:.2} TB", bytes as f64 / TB as f64)
    } else if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}
