//! The eframe application: a tabbed LCARS shell (Repositories / Duplicates /
//! Transfer / Grooming) wired to the core store and a background update worker.
//! This module owns the Repository Management tab and the (currently empty)
//! Grooming tab, and delegates the others to [`crate::dupes_view`] and
//! [`crate::transfer_view`].

use crate::dupes_view::DupesView;
use crate::grooming_view::GroomingView;
use crate::icon;
use crate::settings::TooltipVerbosity;
use crate::status::{self, Location};
use crate::theme;
use crate::transfer_view::TransferView;
use crate::util::{ExplainExt, format_size};
use crate::worker::{ChannelProgress, JobKind, JobOutcome, RepoStatus, WorkerMsg, WorkerState};
use crossbeam_channel::{Receiver, Sender};
use dedup_core::store::{RepoStats, Store};
use dedup_core::update::{CancellationToken, ProgressEvent, check_repo, update_repo};
use egui::{Align, Color32, Id, Layout, RichText};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Repos scan one at a time (each scan already parallelizes across all CPU
/// cores), so the queue starts a new scan only while fewer than this many run.
const MAX_CONCURRENT: usize = 1;

#[derive(PartialEq, Eq, Clone, Copy)]
enum Tab {
    Repositories,
    Duplicates,
    Transfer,
    Grooming,
    Browse,
}

/// Freshness of a repo's index relative to disk, from the last CHECK.
#[derive(Clone, Copy)]
enum Freshness {
    /// Not checked yet this session.
    Unknown,
    /// The last check found nothing new, changed, or vanished.
    UpToDate,
    /// The last check found work an update would do.
    Stale { changed: u64, missing: u64 },
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
    /// Location + reachability, from the last status refresh.
    location: Option<Location>,
    /// Index freshness, from the last CHECK.
    freshness: Freshness,
}

/// Where a native folder-picker result should be routed, since the pick is
/// resolved on a background thread after the invoking widget is gone.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FolderTarget {
    /// The "add repository" form's path field.
    Add,
    /// The inline relocate editor's path buffer.
    Relocate,
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
    UpdateAll,
    Check(String),
    RefreshStatus,
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
    ChooseFolder(FolderTarget),
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
    /// Where the next folder-picker result should land (add form vs relocate).
    folder_target: FolderTarget,
    edit: Option<Edit>,

    show_settings: bool,
    show_about: bool,
    /// Whether the one-time startup status probe has been kicked off.
    did_initial_status: bool,
    threads: usize,
    tooltip_verbosity: TooltipVerbosity,

    tx: Sender<WorkerMsg>,
    rx: Receiver<WorkerMsg>,
    worker: WorkerState,
    /// Repos waiting for a job, in FIFO order. Drained one at a time.
    queue: VecDeque<(String, JobKind)>,
    cancels: HashMap<String, CancellationToken>,
    /// Location/reachability results delivered from the status-refresh thread.
    status_tx: Sender<(String, Location)>,
    status_rx: Receiver<(String, Location)>,
    dupes: DupesView,
    transfer: TransferView,
    grooming: GroomingView,
    browse: crate::browse_view::BrowseView,
    /// Last settings written to disk, to avoid rewriting an unchanged file.
    saved_settings: crate::settings::Settings,
    /// Current window inner size (logical points), captured each frame and
    /// flushed on exit so the next launch reopens at the same size.
    window_size: Option<[f32; 2]>,
}

impl DedupApp {
    pub fn new(store: Arc<Store>) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        let (folder_tx, folder_rx) = crossbeam_channel::unbounded();
        let (status_tx, status_rx) = crossbeam_channel::unbounded();
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
            folder_target: FolderTarget::Add,
            edit: None,
            show_settings: false,
            show_about: false,
            did_initial_status: false,
            threads: 0,
            tooltip_verbosity: TooltipVerbosity::default(),
            tx,
            rx,
            worker: WorkerState::default(),
            queue: VecDeque::new(),
            cancels: HashMap::new(),
            status_tx,
            status_rx,
            dupes: DupesView::new(),
            transfer: TransferView::new(),
            grooming: GroomingView::new(),
            browse: crate::browse_view::BrowseView::new(),
            saved_settings: crate::settings::Settings::default(),
            window_size: None,
        };
        // Restore persisted settings (thread count, similarity threshold,
        // tooltip verbosity).
        let settings = crate::settings::Settings::load(app.store.config_dir());
        app.threads = settings.threads;
        app.dupes.set_threshold(settings.similarity_threshold);
        app.transfer
            .set_threshold(settings.transfer_similarity_threshold);
        app.tooltip_verbosity = settings.tooltip_verbosity;
        app.saved_settings = settings;
        app.reload_all();
        app
    }

    /// The persistable settings snapshot for the current UI state.
    fn current_settings(&self) -> crate::settings::Settings {
        crate::settings::Settings {
            threads: self.threads,
            similarity_threshold: self.dupes.threshold(),
            transfer_similarity_threshold: self.transfer.threshold(),
            tooltip_verbosity: self.tooltip_verbosity,
            window_size: self.window_size,
        }
    }

    /// Kick off a background probe of every repo's location + reachability.
    /// Results arrive on `status_rx` and are applied per frame. The probe runs
    /// off the UI thread because a dead network mount can block on `stat`.
    fn refresh_status(&self, ctx: &egui::Context) {
        for row in &self.repos {
            let tx = self.status_tx.clone();
            let name = row.name.clone();
            let path = row.path.clone();
            let repaint = ctx.clone();
            std::thread::spawn(move || {
                let _ = tx.send((name, status::classify(&path)));
                repaint.request_repaint();
            });
        }
    }

    /// Reload every repo row from the registry. Safe only when no update is
    /// running (it opens each repo db to read stats); callers gate on that.
    fn reload_all(&mut self) {
        match self.store.list_repos() {
            Ok(list) => {
                // Carry the last-result line and status across a reload, keyed
                // by name (a renamed/relocated repo simply re-probes).
                let mut prev: HashMap<String, (Option<String>, Option<Location>, Freshness)> = self
                    .repos
                    .drain(..)
                    .map(|r| (r.name, (r.last, r.location, r.freshness)))
                    .collect();
                let mut rows = Vec::with_capacity(list.len());
                for (name, meta, stats) in list {
                    let (last, location, freshness) =
                        prev.remove(&name)
                            .unwrap_or((None, None, Freshness::Unknown));
                    let mimes = self.store.get_mime_stats(&name).unwrap_or_default();
                    rows.push(RepoRow {
                        name,
                        path: meta.abs_path,
                        stats,
                        mimes,
                        last,
                        location,
                        freshness,
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

    /// Add a `kind` job for a repo to the queue. No-op if it is already queued
    /// or running. The worker thread is started later by [`Self::pump_queue`].
    fn enqueue(&mut self, name: String, kind: JobKind) {
        if self.worker.is_tracked(&name) {
            return;
        }
        self.worker.mark_queued(&name, kind);
        if let Some(row) = self.repos.iter_mut().find(|r| r.name == name) {
            row.last = None;
        }
        self.queue.push_back((name, kind));
    }

    /// Start queued jobs until [`MAX_CONCURRENT`] are running. Called once per
    /// frame after completions are drained and new work is enqueued.
    fn pump_queue(&mut self, ctx: &egui::Context) {
        while self.worker.running_count() < MAX_CONCURRENT {
            let Some((name, kind)) = self.queue.pop_front() else {
                break;
            };
            self.worker.mark_running(&name);

            let cancel = CancellationToken::new();
            self.cancels.insert(name.clone(), cancel.clone());

            let store = Arc::clone(&self.store);
            let tx = self.tx.clone();
            let threads = self.threads;
            let repaint = ctx.clone();
            std::thread::spawn(move || {
                let progress = ChannelProgress::new(name.clone(), tx.clone());
                let outcome = match kind {
                    JobKind::Update => JobOutcome::Update(
                        update_repo(&store, &name, threads, &progress, &cancel)
                            .map_err(|e| e.to_string()),
                    ),
                    JobKind::Check => JobOutcome::Check(
                        check_repo(&store, &name, &progress, &cancel).map_err(|e| e.to_string()),
                    ),
                };
                let _ = tx.send(WorkerMsg::Completed {
                    repo: name,
                    outcome,
                });
                repaint.request_repaint();
            });
        }
    }

    fn apply(&mut self, ctx: &egui::Context, action: Action) {
        match action {
            Action::Update(name) => self.enqueue(name, JobKind::Update),
            Action::UpdateAll => {
                // Skip known-unreachable repos so a dead mount can't hang a
                // worker; not-yet-probed (Unknown) repos are still included.
                let names: Vec<String> = self
                    .repos
                    .iter()
                    .filter(|r| r.location.is_none_or(|l| l.reachable()))
                    .map(|r| r.name.clone())
                    .collect();
                for name in names {
                    self.enqueue(name, JobKind::Update);
                }
            }
            Action::Check(name) => self.enqueue(name, JobKind::Check),
            Action::RefreshStatus => {
                // Re-probe location/reachability, and run a freshness CHECK on
                // every reachable repo — the status analog of UPDATE ALL. Repos
                // already known to need an update are skipped (re-checking would
                // only confirm what the pill already shows).
                self.refresh_status(ctx);
                let names: Vec<String> = self
                    .repos
                    .iter()
                    .filter(|r| {
                        r.location.is_none_or(|l| l.reachable())
                            && !matches!(r.freshness, Freshness::Stale { .. })
                    })
                    .map(|r| r.name.clone())
                    .collect();
                for name in names {
                    self.enqueue(name, JobKind::Check);
                }
            }
            Action::Cancel(name) => {
                if let Some(token) = self.cancels.get(&name) {
                    // Running: signal cooperative cancellation; the worker
                    // reports completion when it stops.
                    token.cancel();
                } else {
                    // Still queued: drop it before it ever starts.
                    self.queue.retain(|(n, _)| n != &name);
                    self.worker.remove(&name);
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
                let mut relocated = false;
                if !new_path.is_empty() {
                    match self.store.relocate_repo(&name, &new_path) {
                        Ok(()) => relocated = true,
                        Err(e) => self.load_error = Some(e.to_string()),
                    }
                }
                self.reload_all();
                if relocated {
                    // A moved repo's stale "missing" status must not linger:
                    // clear it and re-probe location/reachability against the new
                    // path (so it reads Local/Remote if the folder is now there).
                    if let Some(row) = self.repos.iter_mut().find(|r| r.name == name) {
                        row.location = None;
                    }
                    self.refresh_status(ctx);
                }
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
            Action::ChooseFolder(target) => {
                self.folder_target = target;
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
        // One-time startup probe of every repo's location/reachability.
        if !self.did_initial_status {
            self.did_initial_status = true;
            self.refresh_status(&ctx);
        }

        // Drain worker messages; a completed job updates the repo's row.
        for (repo, outcome) in self.worker.drain(&self.rx) {
            self.cancels.remove(&repo);
            match outcome {
                JobOutcome::Update(result) => {
                    // A clean, uncancelled update brings the index in sync.
                    let clean = matches!(&result, Ok(s) if !s.cancelled);
                    let summary = match result {
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
                        if clean {
                            row.freshness = Freshness::UpToDate;
                        }
                    }
                }
                JobOutcome::Check(result) => {
                    if let Some(row) = self.repos.iter_mut().find(|r| r.name == repo) {
                        match result {
                            Ok(c) if c.cancelled => row.last = Some("check cancelled".into()),
                            Ok(c) => {
                                row.freshness = if c.up_to_date() {
                                    Freshness::UpToDate
                                } else {
                                    Freshness::Stale {
                                        changed: c.changed,
                                        missing: c.missing,
                                    }
                                };
                                row.last = Some(format!(
                                    "checked — {} changed, {} missing, {} unchanged",
                                    c.changed, c.missing, c.unchanged
                                ));
                            }
                            Err(e) => row.last = Some(format!("check error: {e}")),
                        }
                    }
                }
            }
        }

        // Apply any location/reachability results from the status thread.
        while let Ok((repo, location)) = self.status_rx.try_recv() {
            if let Some(row) = self.repos.iter_mut().find(|r| r.name == repo) {
                row.location = Some(location);
            }
        }

        // Folder-picker results land in whichever field asked for them.
        while let Ok(dir) = self.folder_rx.try_recv() {
            let picked = dir.to_string_lossy().into_owned();
            match self.folder_target {
                // Add form: fill the path and auto-name from the last path
                // component unless the user already typed a name.
                FolderTarget::Add => {
                    if self.new_name.trim().is_empty()
                        && let Some(base) = dir.file_name()
                    {
                        self.new_name = base.to_string_lossy().into_owned();
                    }
                    self.new_path = picked;
                }
                // Relocate editor: fill its path buffer (if still open).
                FolderTarget::Relocate => {
                    if let Some(Edit::Relocate { buf, .. }) = &mut self.edit {
                        *buf = picked;
                    }
                }
            }
            ctx.request_repaint();
        }

        let mut actions: Vec<Action> = Vec::new();
        self.top_bar(ui);
        // Audio preview belongs to the Duplicates tab; stop it elsewhere.
        if self.tab != Tab::Duplicates {
            self.dupes.stop_audio();
        }
        egui::CentralPanel::default().show(ui, |ui| match self.tab {
            Tab::Repositories => self.repositories_view(ui, &mut actions),
            Tab::Duplicates => self.dupes.show(ui, &self.store, self.tooltip_verbosity),
            Tab::Transfer => self.transfer.show(ui, &self.store, self.tooltip_verbosity),
            Tab::Grooming => self.grooming.show(ui, &self.store, self.tooltip_verbosity),
            Tab::Browse => self.browse.show(ui, &self.store, self.tooltip_verbosity),
        });
        if self.show_settings {
            self.settings_modal(&ctx);
        }
        if self.show_about {
            self.about_modal(&ctx);
        }
        if self.show_add {
            self.add_modal(&ctx, &mut actions);
        }
        for action in actions {
            self.apply(&ctx, action);
        }

        // Completions (drained above) free the running slot; the actions loop
        // may have enqueued more. Start whatever can run now.
        self.pump_queue(&ctx);

        // Poll at ~10 Hz while work is running instead of repainting per event.
        // This also ticks the queued/scanning timers.
        if self.worker.active_count() > 0 {
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        // Track the window's logical size for persistence (flushed on exit).
        // `screen_rect` works on Wayland, where winit can't report the window
        // position so `viewport().inner_rect` is None; scaling by the zoom
        // factor converts egui points to the logical points `with_inner_size`
        // expects, independent of HiDPI or `--ui-scale`.
        let sz = ctx.viewport_rect().size() * ctx.zoom_factor();
        if sz.x >= 1.0 && sz.y >= 1.0 {
            self.window_size = Some([sz.x, sz.y]);
        }

        // Persist settings the moment a *control* changes (eframe's own storage
        // isn't enabled, so we own the file). Comparing first keeps this to one
        // write per actual change. A window resize alone doesn't write here —
        // that would thrash the file every frame of a drag — the final size is
        // flushed in `on_exit`.
        let current = self.current_settings();
        let control_changed = current.threads != self.saved_settings.threads
            || current.similarity_threshold != self.saved_settings.similarity_threshold
            || current.transfer_similarity_threshold
                != self.saved_settings.transfer_similarity_threshold
            || current.tooltip_verbosity != self.saved_settings.tooltip_verbosity;
        if control_changed {
            current.save(self.store.config_dir());
            self.saved_settings = current;
        }
    }

    fn on_exit(&mut self) {
        // Flush the final window size (and any current control values) so the
        // next launch reopens where the user left it.
        self.current_settings().save(self.store.config_dir());
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
                    ui.label(
                        RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                            .color(theme::LILAC)
                            .size(13.0),
                    );
                    ui.add_space(16.0);
                    tab_button(
                        ui,
                        &mut self.tab,
                        Tab::Repositories,
                        "REPOSITORIES",
                        theme::ORANGE,
                        self.tooltip_verbosity,
                        "Add, update, and manage repository links",
                    );
                    tab_button(
                        ui,
                        &mut self.tab,
                        Tab::Duplicates,
                        "DUPLICATES",
                        theme::LILAC,
                        self.tooltip_verbosity,
                        "Find and review exact or perceptually similar duplicates",
                    );
                    tab_button(
                        ui,
                        &mut self.tab,
                        Tab::Transfer,
                        "TRANSFER",
                        theme::BLUE,
                        self.tooltip_verbosity,
                        "Copy or move files between repositories by content",
                    );
                    tab_button(
                        ui,
                        &mut self.tab,
                        Tab::Grooming,
                        "GROOMING",
                        theme::TAN,
                        self.tooltip_verbosity,
                        "Prune and reorganize repositories (coming soon)",
                    );
                    tab_button(
                        ui,
                        &mut self.tab,
                        Tab::Browse,
                        "BROWSE",
                        theme::AMBER,
                        self.tooltip_verbosity,
                        "Browse a repo's files by directory, from the index",
                    );

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add(egui::Button::new(
                                RichText::new(format!("{} SETTINGS", icon::GEAR))
                                    .color(theme::BLACK),
                            ))
                            .explain(
                                self.tooltip_verbosity,
                                "App settings",
                                "Open app settings: hashing thread count and tooltip verbosity.",
                            )
                            .clicked()
                        {
                            self.show_settings = true;
                        }
                        // Added after SETTINGS so it renders immediately to its
                        // left in this right-to-left layout.
                        if ui
                            .add(egui::Button::new(
                                RichText::new("ABOUT").color(theme::BLACK),
                            ))
                            .explain(
                                self.tooltip_verbosity,
                                "Version and license",
                                "Show the app version, license, and contact info.",
                            )
                            .clicked()
                        {
                            self.show_about = true;
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
            if ui
                .add_enabled(!busy, add)
                .explain(
                    self.tooltip_verbosity,
                    "Register a new repository",
                    "Register a new repository: pick a folder on disk to track and scan for \
                     duplicates. Disabled while a scan is running elsewhere in the app.",
                )
                .clicked()
            {
                actions.push(Action::OpenAdd);
            }
            // Enqueues every repo; it only touches names (no db access), so it
            // stays enabled even while a batch is running.
            let update_all = egui::Button::new(
                RichText::new(format!("{} UPDATE ALL", icon::REFRESH)).color(theme::BLACK),
            )
            .fill(theme::ORANGE);
            if ui
                .add_enabled(!self.repos.is_empty(), update_all)
                .explain(
                    self.tooltip_verbosity,
                    "Scan every repository",
                    "Queue an UPDATE / SCAN for every registered repository, one at a time. \
                     Already up-to-date repos finish almost instantly.",
                )
                .clicked()
            {
                actions.push(Action::UpdateAll);
            }
            // Re-probe every repo's location/reachability (filesystem only, no
            // db access), so it is fine to run any time.
            let refresh = egui::Button::new(
                RichText::new(format!("{} REFRESH STATUS", icon::REFRESH)).color(theme::BLACK),
            )
            .fill(theme::LILAC);
            if ui
                .add_enabled(!self.repos.is_empty(), refresh)
                .explain(
                    self.tooltip_verbosity,
                    "Re-check location and staleness",
                    "Re-check every repository's location and reachability, and whether its \
                     index is stale (dry-run — no hashing, no writes).",
                )
                .clicked()
            {
                actions.push(Action::RefreshStatus);
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
        // Owned snapshot of this repo's queue state, taken before the frame
        // closure so it doesn't borrow `self.worker` across `card_controls`.
        let tracked = self.worker.get(&row.name).map(|r| {
            (
                r.kind,
                r.status,
                r.queued_at.elapsed(),
                r.started_at.map(|s| s.elapsed()),
                r.event.clone(),
                r.eta(),
            )
        });
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
                    status_pills(ui, row, self.tooltip_verbosity);
                    ui.label(RichText::new(&row.path).color(theme::TEXT).size(12.0))
                        .explain(
                            self.tooltip_verbosity,
                            &row.path,
                            &format!("On-disk folder this repository indexes: {}", row.path),
                        );
                    // MIME breakdown, share-sorted, pinned to the top-right.
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        mime_tags(ui, row, self.tooltip_verbosity);
                    });
                });
                ui.horizontal(|ui| {
                    stat(
                        ui,
                        "FILES",
                        &row.stats.file_count.to_string(),
                        theme::ORANGE,
                        "Indexed files (missing files excluded)",
                        "Number of files currently indexed for this repository. Files that \
                         were indexed before but have since vanished from disk are excluded \
                         from this count.",
                        self.tooltip_verbosity,
                    );
                    stat(
                        ui,
                        "SIZE",
                        &format_size(row.stats.total_size),
                        theme::BLUE,
                        "Total size of indexed files",
                        "Sum of the on-disk size of every indexed (non-missing) file in this \
                         repository.",
                        self.tooltip_verbosity,
                    );
                    stat(
                        ui,
                        "MISSING",
                        &row.stats.missing_count.to_string(),
                        theme::LILAC,
                        "Indexed before but no longer on disk",
                        "Files that were indexed by a previous scan but are no longer found \
                         on disk. They stay in the index as history but are excluded from \
                         stats and duplicate search until a rescan confirms they're back.",
                        self.tooltip_verbosity,
                    );
                    stat(
                        ui,
                        "SCANNED",
                        &format_last_scan(row.stats.last_scan_ms),
                        theme::TAN,
                        "When this repository was last scanned",
                        "Date and time of the most recent completed UPDATE / SCAN of this \
                         repository. \"never\" means it hasn't been scanned yet.",
                        self.tooltip_verbosity,
                    );
                    if row.stats.triage_done_ms > 0 {
                        stat(
                            ui,
                            "TRIAGED",
                            &format_last_scan(row.stats.triage_done_ms),
                            theme::BLUE,
                            "This repo's unique content was copied into a sanitized dir",
                            "This repository was marked triage-done: its unique content was \
                             already copied into a sanitized directory (via `dedup sanitize` \
                             or MARK SOURCE DONE in the Transfer tab), so it's safe to \
                             consider fully processed.",
                            self.tooltip_verbosity,
                        );
                    }
                });

                match tracked {
                    Some((kind, RepoStatus::Queued, waited, _, _, _)) => {
                        let verb = if kind == JobKind::Check {
                            "check"
                        } else {
                            "scan"
                        };
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(format!(
                                    "queued to {verb} — waiting {}",
                                    format_elapsed(waited)
                                ))
                                .color(theme::TAN),
                            );
                            if cancel_button(
                                ui,
                                "Remove from the queue",
                                "Remove this repository from the queue before its scan/check starts.",
                                self.tooltip_verbosity,
                            )
                            .clicked()
                            {
                                actions.push(Action::Cancel(row.name.clone()));
                            }
                        });
                    }
                    Some((kind, RepoStatus::Running, _, elapsed, event, eta)) => {
                        let elapsed = elapsed.unwrap_or_default();
                        let checking = kind == JobKind::Check;
                        ui.horizontal(|ui| {
                            ui.add(egui::Spinner::new().color(theme::AMBER));
                            match &event {
                                // Only a full update hashes; a check never does.
                                ProgressEvent::Hashing { done, total, .. }
                                    if *total > 0 && !checking =>
                                {
                                    let frac = *done as f32 / *total as f32;
                                    let pct = (frac * 100.0) as u32;
                                    ui.add(
                                        egui::ProgressBar::new(frac)
                                            .desired_width(240.0)
                                            .text(format!("{pct}% · {done}/{total}")),
                                    );
                                }
                                ProgressEvent::Scanning { files, dirs } if checking => {
                                    ui.label(
                                        RichText::new(format!(
                                            "checking — {files} files, {dirs} dirs"
                                        ))
                                        .color(theme::AMBER),
                                    );
                                }
                                other => {
                                    ui.label(
                                        RichText::new(progress_line(other)).color(theme::AMBER),
                                    );
                                }
                            }
                            let (hover, hover_verbose) = if checking {
                                (
                                    "Stop the check",
                                    "Stop this dry-run check. No index changes have been made, \
                                     so there's nothing to roll back.",
                                )
                            } else {
                                (
                                    "Stop the scan (already-hashed files stay indexed)",
                                    "Stop this scan. Files already hashed before you cancelled \
                                     stay committed to the index; only unhashed files are left \
                                     for next time.",
                                )
                            };
                            if cancel_button(ui, hover, hover_verbose, self.tooltip_verbosity).clicked() {
                                actions.push(Action::Cancel(row.name.clone()));
                            }
                        });
                        let verb = if checking { "checking" } else { "scanning" };
                        let hashing = !checking && matches!(&event, ProgressEvent::Hashing { total, .. } if *total > 0);
                        let timing = match eta {
                            Some(eta) if hashing => format!(
                                "scanning for {} · ETA {}",
                                format_elapsed(elapsed),
                                format_elapsed(eta)
                            ),
                            _ => format!("{verb} for {}", format_elapsed(elapsed)),
                        };
                        ui.label(RichText::new(timing).color(theme::TAN).size(12.0));
                    }
                    None => {
                        self.card_controls(ui, row, actions);
                        if let Some(last) = &row.last {
                            ui.label(RichText::new(last).color(theme::TAN).size(12.0));
                        }
                    }
                }
            });
    }

    fn card_controls(&mut self, ui: &mut egui::Ui, row: &RepoRow, actions: &mut Vec<Action>) {
        // Inline editors take over the row when active for this repo.
        match &mut self.edit {
            Some(Edit::Rename { name, buf }) if *name == row.name => {
                let verbosity = self.tooltip_verbosity;
                ui.horizontal(|ui| {
                    ui.label(RichText::new("RENAME →").color(theme::LILAC));
                    ui.text_edit_singleline(buf).explain(
                        verbosity,
                        "New name",
                        "The new name for this repository (its registry entry only — the \
                         on-disk folder it points at is unchanged).",
                    );
                    if ui
                        .button(RichText::new(format!("{} OK", icon::CHECK)).color(theme::BLACK))
                        .explain(verbosity, "Confirm rename", "Apply the new name.")
                        .clicked()
                    {
                        actions.push(Action::CommitRename(name.clone(), buf.trim().to_string()));
                    }
                    if ui
                        .button(RichText::new(icon::X).color(theme::BLACK))
                        .explain(
                            verbosity,
                            "Cancel",
                            "Discard this rename and close the editor.",
                        )
                        .clicked()
                    {
                        actions.push(Action::CancelEdit);
                    }
                });
                return;
            }
            Some(Edit::Relocate { name, buf }) if *name == row.name => {
                let verbosity = self.tooltip_verbosity;
                ui.horizontal(|ui| {
                    ui.label(RichText::new("RELOCATE →").color(theme::LILAC));
                    if ui
                        .button(
                            RichText::new(format!("{} CHOOSE…", icon::FOLDER_OPEN))
                                .color(theme::BLACK),
                        )
                        .explain(
                            verbosity,
                            "Pick a folder",
                            "Open a native folder picker to choose the new folder this \
                             repository should point at.",
                        )
                        .clicked()
                    {
                        actions.push(Action::ChooseFolder(FolderTarget::Relocate));
                    }
                    ui.text_edit_singleline(buf).explain(
                        verbosity,
                        "New folder path",
                        "The new on-disk folder this repository should point at. The \
                         existing index is kept — only the target path changes.",
                    );
                    if ui
                        .button(RichText::new(format!("{} OK", icon::CHECK)).color(theme::BLACK))
                        .explain(verbosity, "Confirm relocate", "Apply the new folder path.")
                        .clicked()
                    {
                        actions.push(Action::CommitRelocate(name.clone(), buf.trim().to_string()));
                    }
                    if ui
                        .button(RichText::new(icon::X).color(theme::BLACK))
                        .explain(
                            verbosity,
                            "Cancel",
                            "Discard this relocate and close the editor.",
                        )
                        .clicked()
                    {
                        actions.push(Action::CancelEdit);
                    }
                });
                return;
            }
            Some(Edit::Duplicate { name, dest, path }) if *name == row.name => {
                let verbosity = self.tooltip_verbosity;
                ui.horizontal(|ui| {
                    ui.label(RichText::new("COPY → NAME").color(theme::LILAC));
                    ui.add(egui::TextEdit::singleline(dest).desired_width(140.0))
                        .explain(
                            verbosity,
                            "New repository's name",
                            "Name for the new repository the index is copied into.",
                        );
                    ui.label(RichText::new("PATH").color(theme::LILAC));
                    ui.add(
                        egui::TextEdit::singleline(path)
                            .desired_width(240.0)
                            .hint_text("/new/repo/path"),
                    )
                    .explain(
                        verbosity,
                        "New repository's folder",
                        "On-disk folder the new repository will point at. The source \
                         repository is left completely unchanged.",
                    );
                    if ui
                        .button(RichText::new(format!("{} OK", icon::CHECK)).color(theme::BLACK))
                        .explain(
                            verbosity,
                            "Confirm duplicate",
                            "Copy this repository's index into the new one at the new path.",
                        )
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
                        .explain(
                            verbosity,
                            "Cancel",
                            "Discard this duplicate and close the editor.",
                        )
                        .clicked()
                    {
                        actions.push(Action::CancelEdit);
                    }
                });
                return;
            }
            Some(Edit::ConfirmDelete { name }) if *name == row.name => {
                let verbosity = self.tooltip_verbosity;
                ui.horizontal(|ui| {
                    ui.colored_label(theme::RED, format!("Delete '{name}' and its index?"));
                    if ui
                        .add(
                            egui::Button::new(RichText::new("DELETE").color(theme::BLACK))
                                .fill(theme::RED),
                        )
                        .explain(
                            verbosity,
                            "Confirm delete",
                            "Permanently remove this repository's registry entry and its \
                             index database. The on-disk files it tracked are never touched.",
                        )
                        .clicked()
                    {
                        actions.push(Action::CommitDelete(name.clone()));
                    }
                    if ui
                        .button(RichText::new("KEEP").color(theme::BLACK))
                        .explain(
                            verbosity,
                            "Cancel",
                            "Keep this repository; close the confirmation.",
                        )
                        .clicked()
                    {
                        actions.push(Action::CancelEdit);
                    }
                });
                return;
            }
            _ => {}
        }

        // An unreachable folder can't be scanned or walked (and a dead network
        // mount would hang the worker), so gate both jobs on reachability.
        let reachable = row.location.is_none_or(|l| l.reachable());
        ui.horizontal(|ui| {
            let update = egui::Button::new(
                RichText::new(format!("{} UPDATE / SCAN", icon::REFRESH)).color(theme::BLACK),
            );
            if ui
                .add_enabled(reachable, update)
                .explain(
                    self.tooltip_verbosity,
                    "Scan the folder and index new or changed files",
                    "Walk this repository's folder, hash any new or changed files, and \
                     mark vanished files missing. Already-hashed unchanged files are \
                     skipped, so a repeat scan is fast.",
                )
                .clicked()
            {
                actions.push(Action::Update(row.name.clone()));
            }
            let check = egui::Button::new(
                RichText::new(format!("{} CHECK", icon::SEARCH)).color(theme::BLACK),
            );
            if ui
                .add_enabled(reachable, check)
                .explain(
                    self.tooltip_verbosity,
                    "Dry-run: report new, changed, and missing files without hashing or writing",
                    "Dry-run a scan: report how many files are new, changed, or missing \
                     compared to the index, without hashing anything or writing to the \
                     index. Use this to see if UPDATE / SCAN has real work to do.",
                )
                .clicked()
            {
                actions.push(Action::Check(row.name.clone()));
            }
            if ui
                .button(RichText::new(format!("{} RENAME", icon::PENCIL)).color(theme::BLACK))
                .explain(
                    self.tooltip_verbosity,
                    "Rename this repository",
                    "Rename this repository's registry entry. The on-disk folder it \
                     points at is not moved.",
                )
                .clicked()
            {
                actions.push(Action::BeginRename(row.name.clone()));
            }
            if ui
                .button(RichText::new(format!("{} RELOCATE", icon::RELOCATE)).color(theme::BLACK))
                .explain(
                    self.tooltip_verbosity,
                    "Point this repository at a different folder",
                    "Point this repository at a different on-disk folder while keeping its \
                     existing index — use this after moving the data to a new location.",
                )
                .clicked()
            {
                actions.push(Action::BeginRelocate(row.name.clone()));
            }
            if ui
                .button(RichText::new(format!("{} DUPLICATE", icon::COPY)).color(theme::BLACK))
                .explain(
                    self.tooltip_verbosity,
                    "Copy this repository's index into a new one at a new path",
                    "Clone this repository's entire index into a brand-new repository at a \
                     new path. The source repository is left completely unchanged — this is \
                     for branching off a snapshot, not moving anything.",
                )
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
                .explain(
                    self.tooltip_verbosity,
                    "Remove this repository and delete its index",
                    "Remove this repository's registry entry and delete its index database. \
                     The on-disk files it tracked are never touched — only the tracking \
                     record disappears.",
                )
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
                    .explain(
                        self.tooltip_verbosity,
                        "Pick a folder",
                        "Open a native folder picker to choose the folder this repository \
                         should index.",
                    )
                    .clicked()
                {
                    actions.push(Action::ChooseFolder(FolderTarget::Add));
                }
                ui.add(
                    egui::TextEdit::singleline(&mut self.new_path)
                        .desired_width(300.0)
                        .hint_text("/path/to/folder"),
                )
                .explain(
                    self.tooltip_verbosity,
                    "Folder to index",
                    "Absolute or relative path to the folder this repository should index. \
                     You can type it directly or use CHOOSE… above.",
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
                ui.add(name_edit).explain(
                    self.tooltip_verbosity,
                    "Repository name",
                    "Display name for this repository in the registry. Leave blank to use \
                     the folder's own name; must be unique among registered repositories.",
                );
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
                if ui
                    .add_enabled(can_add, add)
                    .explain(
                        self.tooltip_verbosity,
                        "Register this repository",
                        "Register the repository with this folder and name. It won't be \
                         scanned automatically — use UPDATE / SCAN afterwards.",
                    )
                    .clicked()
                {
                    actions.push(Action::Create);
                }
                if ui
                    .button(RichText::new("CANCEL").color(theme::BLACK))
                    .explain(
                        self.tooltip_verbosity,
                        "Cancel",
                        "Close this dialog without registering a repository.",
                    )
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
                ui.add(egui::DragValue::new(&mut self.threads).range(0..=64))
                    .explain(
                        self.tooltip_verbosity,
                        "Parallel hashing threads for scans",
                        "How many threads a repo scan uses to hash files in parallel. \
                         Higher uses more CPU but finishes faster; 0 lets the hashing \
                         library pick one thread per CPU core.",
                    );
            });
            ui.label(
                RichText::new("0 = one thread per CPU core")
                    .color(theme::TAN)
                    .size(12.0),
            );
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Tooltips").color(theme::TEXT));
                let short = self.tooltip_verbosity == TooltipVerbosity::Short;
                egui::Frame::new()
                    .stroke(egui::Stroke::new(1.0, theme::BLUE))
                    .corner_radius(6)
                    .inner_margin(egui::Margin::symmetric(4, 2))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let (short_fill, short_text) = if short {
                                (theme::BLUE, theme::BLACK)
                            } else {
                                (theme::PANEL, theme::BLUE)
                            };
                            if ui
                                .add(
                                    egui::Button::new(RichText::new("SHORT").color(short_text))
                                        .fill(short_fill),
                                )
                                .explain(
                                    self.tooltip_verbosity,
                                    "Terse one-line hints",
                                    "Hover text stays a short one-liner naming what a \
                                     control does.",
                                )
                                .clicked()
                            {
                                self.tooltip_verbosity = TooltipVerbosity::Short;
                            }
                            let (verbose_fill, verbose_text) = if short {
                                (theme::PANEL, theme::LILAC)
                            } else {
                                (theme::LILAC, theme::BLACK)
                            };
                            if ui
                                .add(
                                    egui::Button::new(RichText::new("VERBOSE").color(verbose_text))
                                        .fill(verbose_fill),
                                )
                                .explain(
                                    self.tooltip_verbosity,
                                    "Fuller explanations",
                                    "Hover text expands into a fuller explanation of what \
                                     the control does and when to use it.",
                                )
                                .clicked()
                            {
                                self.tooltip_verbosity = TooltipVerbosity::Verbose;
                            }
                        });
                    });
            });
            ui.label(
                RichText::new("Controls how much detail hover tooltips show throughout the app")
                    .color(theme::TAN)
                    .size(12.0),
            );
            ui.add_space(12.0);
            if ui
                .add(egui::Button::new(
                    RichText::new("CLOSE").color(theme::BLACK),
                ))
                .explain(
                    self.tooltip_verbosity,
                    "Close this dialog",
                    "Close the settings dialog; changes are saved automatically as you make them.",
                )
                .clicked()
            {
                self.show_settings = false;
            }
        });
        if response.should_close() {
            self.show_settings = false;
        }
    }

    fn about_modal(&mut self, ctx: &egui::Context) {
        let response = egui::Modal::new(Id::new("about")).show(ctx, |ui| {
            ui.set_width(320.0);
            ui.label(
                RichText::new("ABOUT")
                    .color(theme::AMBER)
                    .size(18.0)
                    .strong(),
            );
            ui.add_space(8.0);
            ui.label(
                RichText::new(format!("DEDUP  v{}", env!("CARGO_PKG_VERSION"))).color(theme::TEXT),
            );
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("License:").color(theme::TAN).size(12.0));
                ui.hyperlink_to(
                    RichText::new("MIT").color(theme::LILAC).size(12.0),
                    "https://opensource.org/license/mit",
                );
            });
            ui.label(
                RichText::new("© 2026 Patrick Zimmer")
                    .color(theme::TAN)
                    .size(12.0),
            );
            ui.hyperlink_to(
                RichText::new("dedup@tuta.io")
                    .color(theme::LILAC)
                    .size(12.0),
                "mailto:dedup@tuta.io",
            );
            ui.add_space(12.0);
            if ui
                .add(egui::Button::new(
                    RichText::new("CLOSE").color(theme::BLACK),
                ))
                .explain(
                    self.tooltip_verbosity,
                    "Close this dialog",
                    "Close the about dialog.",
                )
                .clicked()
            {
                self.show_about = false;
            }
        });
        if response.should_close() {
            self.show_about = false;
        }
    }
}

/// A small rounded status chip with black text on `fill`.
fn pill(ui: &mut egui::Ui, text: &str, fill: Color32) -> egui::Response {
    egui::Frame::new()
        .fill(fill)
        .corner_radius(6)
        .inner_margin(egui::Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(text).color(theme::BLACK).size(11.0))
        })
        .inner
}

/// Render a repo's location + freshness as chips next to its name. Anything
/// still `Unknown` (not yet probed/checked) draws nothing.
fn status_pills(ui: &mut egui::Ui, row: &RepoRow, verbosity: TooltipVerbosity) {
    match row.location {
        Some(Location::Local) => {
            pill(ui, "LOCAL", theme::BLUE).explain(
                verbosity,
                "On this machine",
                "The repository's folder is on a local disk of this machine.",
            );
        }
        Some(Location::Remote) => {
            pill(ui, "REMOTE", theme::LILAC).explain(
                verbosity,
                "Network mount",
                "The repository's folder is on a reachable network mount (e.g. NFS/SMB).",
            );
        }
        Some(Location::Offline) => {
            pill(ui, "OFFLINE", theme::AMBER).explain(
                verbosity,
                "Network mount is not reachable right now",
                "This repository's network mount is not reachable right now — scans and \
                 checks will fail until it's back online.",
            );
        }
        Some(Location::Missing) => {
            pill(ui, "MISSING", theme::RED).explain(
                verbosity,
                "Local folder is not accessible",
                "This repository's local folder no longer exists or can't be read — RELOCATE \
                 it, or restore the folder, before scanning.",
            );
        }
        None => {}
    }
    match row.freshness {
        Freshness::Unknown => {}
        Freshness::UpToDate => {
            pill(ui, "UP TO DATE", theme::TAN).explain(
                verbosity,
                "No changes since the last scan",
                "The last CHECK found no new, changed, or missing files since the last scan.",
            );
        }
        Freshness::Stale { changed, missing } => {
            pill(ui, "UPDATE REQUIRED", theme::ORANGE).explain(
                verbosity,
                &format!("{changed} new/changed, {missing} missing since the last scan"),
                &format!(
                    "The last CHECK found {changed} new or changed file(s) and {missing} \
                     missing file(s) since the last scan — run UPDATE / SCAN to bring the \
                     index back in sync."
                ),
            );
        }
    }
}

/// The RED "CANCEL" button shared by queued and running repo cards.
fn cancel_button(
    ui: &mut egui::Ui,
    hover: &str,
    hover_verbose: &str,
    verbosity: TooltipVerbosity,
) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(format!("{} CANCEL", icon::X)).color(theme::BLACK))
            .fill(theme::RED),
    )
    .explain(verbosity, hover, hover_verbose)
}

fn tab_button(
    ui: &mut egui::Ui,
    current: &mut Tab,
    tab: Tab,
    label: &str,
    color: Color32,
    verbosity: TooltipVerbosity,
    hover_verbose: &str,
) {
    let selected = *current == tab;
    let fill = if selected { color } else { theme::PANEL };
    let text_color = if selected { theme::BLACK } else { color };
    if ui
        .add(egui::Button::new(RichText::new(label).color(text_color)).fill(fill))
        .explain(verbosity, label, hover_verbose)
        .clicked()
    {
        *current = tab;
    }
}

fn stat(
    ui: &mut egui::Ui,
    label: &str,
    value: &str,
    color: Color32,
    tip: &str,
    tip_verbose: &str,
    verbosity: TooltipVerbosity,
) {
    ui.add_space(2.0);
    ui.label(
        RichText::new(format!("{label} "))
            .color(theme::TEXT)
            .size(12.0),
    )
    .explain(verbosity, tip, tip_verbose);
    ui.label(RichText::new(value).color(color).strong())
        .explain(verbosity, tip, tip_verbose);
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
fn mime_tags(ui: &mut egui::Ui, row: &RepoRow, verbosity: TooltipVerbosity) {
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
        .explain(
            verbosity,
            &format!("{extra} more MIME type(s)"),
            &format!(
                "{extra} more MIME type(s) present in this repository beyond the top \
                 {MIME_TAG_LIMIT} shown here, each too small a share to list."
            ),
        );
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
                .explain(
                    verbosity,
                    &format!("{count} file(s) · {mime}"),
                    &format!(
                        "{count} file(s) with MIME type {mime} — {} of this repository's \
                         indexed files.",
                        mime_pct(*count, total)
                    ),
                );
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

/// Format a duration compactly: `"45s"`, `"3m 12s"`, or `"1h 04m"`.
fn format_elapsed(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
    }
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

/// Kittest UI tests: generate doc screenshots of the Repository Management
/// tab and the Settings dialog by driving `DedupApp` directly (calling its
/// private view methods, not the `eframe::App` trait — no `eframe::Frame` is
/// needed that way).
#[cfg(test)]
mod ui_tests {
    use super::*;
    use dedup_core::update::{NoProgress, update_repo};
    use egui_kittest::Harness;

    /// A temp store with two scanned repos, so the Repository Management
    /// cards show real stats instead of all-zero placeholders.
    fn sample_app() -> (tempfile::TempDir, DedupApp) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        for (name, files) in [("Automatic Upload", 5), ("Videos", 2)] {
            let dir = tmp.path().join(name.replace(' ', "_"));
            std::fs::create_dir_all(&dir).unwrap();
            for i in 0..files {
                std::fs::write(dir.join(format!("f{i}.bin")), format!("sample data {i}")).unwrap();
            }
            store.create_repo(name, &dir.to_string_lossy()).unwrap();
            update_repo(&store, name, 1, &NoProgress, &CancellationToken::new()).unwrap();
        }
        (tmp, DedupApp::new(store))
    }

    /// The window size is flushed on exit and restored (via `Settings`) on the
    /// next launch, so the app reopens where the user left it.
    #[test]
    fn window_size_persists_on_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let config_dir = store.config_dir().to_path_buf();
        let mut app = DedupApp::new(store);

        // Nothing saved yet → the next `run()` would fall back to the default.
        assert_eq!(
            crate::settings::Settings::load(&config_dir).window_size,
            None
        );

        // A resize (captured each frame from `viewport_rect`) then a close.
        app.window_size = Some([912.0, 678.0]);
        eframe::App::on_exit(&mut app);

        assert_eq!(
            crate::settings::Settings::load(&config_dir).window_size,
            Some([912.0, 678.0]),
            "the closed size is restored on the next launch"
        );
    }

    fn doc_screenshot_path(name: &str) -> PathBuf {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/screenshots");
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    /// Doc screenshot: the Repository Management tab with two scanned repos,
    /// to `docs/screenshots/repositories_tab.png`. Run with `--ignored`.
    #[test]
    #[ignore = "generates a doc screenshot (needs wgpu)"]
    fn doc_screenshot_repositories_tab() {
        let (_tmp, app) = sample_app();
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1120.0, 620.0))
            .wgpu()
            .build_ui_state(
                move |ui, app: &mut DedupApp| {
                    if !init {
                        icon::install(ui.ctx());
                        theme::apply(ui.ctx());
                        init = true;
                    }
                    let mut actions = Vec::new();
                    app.repositories_view(ui, &mut actions);
                },
                app,
            );
        harness.run();
        let img = harness.render().expect("wgpu render failed");
        let out = doc_screenshot_path("repositories_tab.png");
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());
    }

    /// Doc screenshot: the Settings dialog (hashing threads, tooltip
    /// verbosity toggle) to `docs/screenshots/settings_modal.png`. `--ignored`.
    #[test]
    #[ignore = "generates a doc screenshot (needs wgpu)"]
    fn doc_screenshot_settings_modal() {
        let (_tmp, mut app) = sample_app();
        app.show_settings = true;
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(420.0, 320.0))
            .wgpu()
            .build_ui_state(
                move |ui, app: &mut DedupApp| {
                    if !init {
                        icon::install(ui.ctx());
                        theme::apply(ui.ctx());
                        init = true;
                    }
                    app.settings_modal(&ui.ctx().clone());
                },
                app,
            );
        harness.run();
        let img = harness.render().expect("wgpu render failed");
        let out = doc_screenshot_path("settings_modal.png");
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());
    }
}
