//! The eframe application: a tabbed LCARS shell (Repositories / Duplicates /
//! Transfer / Grooming) wired to the core store and a background update worker.
//! This module owns the Repository Management tab and the (currently empty)
//! Grooming tab, and delegates the others to [`crate::dupes_view`] and
//! [`crate::transfer_view`].

use crate::dupes_view::DupesView;
use crate::grooming_view::GroomingView;
use crate::icon;
use crate::settings::{ThemeChoice, TooltipVerbosity};
use crate::status::{self, Location};
use crate::theme;
use crate::transfer_view::TransferView;
use crate::util::{ExplainExt, format_size};
use crate::worker::{ChannelProgress, JobKind, JobOutcome, RepoStatus, WorkerMsg, WorkerState};
use crossbeam_channel::{Receiver, Sender};
use dedup_core::store::{RepoStats, Store};
use dedup_core::update::{CancellationToken, ProgressEvent, check_repo};
use egui::{Align, Color32, Id, Layout, RichText};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Repos scan one at a time (each scan already parallelizes across all CPU
/// cores), so the queue starts a new scan only while fewer than this many run.
const MAX_CONCURRENT: usize = 1;

#[derive(PartialEq, Eq, Clone, Copy)]
pub(crate) enum Tab {
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
    /// The inline duplicate editor's path buffer.
    Duplicate,
    /// The inline "add a sink to a group" editor's path buffer (a duplicate of
    /// the group's main pointed at a new path).
    AddSink,
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
    /// Add a new sink to `group`: a clone of its `main` pointed at a new path.
    /// `main` is the row the inline editor renders under.
    AddSink {
        main: String,
        group: String,
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
    /// Turn an ungrouped repo into a sync-group main (a group named after it).
    MakeMain(String),
    /// Add an ungrouped repo to an existing group as a sink.
    SinkInto {
        repo: String,
        group: String,
    },
    /// Take a sink back out of its group.
    RemoveSink {
        group: String,
        repo: String,
    },
    /// Flip one sink's push mode (ADD ONLY ↔ MIRROR).
    SetSinkMode {
        group: String,
        repo: String,
        mode: dedup_core::store::SyncMode,
    },
    /// Queue an UPDATE / SCAN for every member of a group (main + sinks).
    UpdateGroup(String),
    /// Disband a group (its repos stay, just ungrouped).
    Ungroup(String),
    /// Open the inline "add a sink" editor on the group's main card.
    BeginAddSink {
        group: String,
        main: String,
    },
    /// Clone `main` into a new repo at `path` and add it to `group` as a sink.
    CommitAddSink {
        group: String,
        main: String,
        dest: String,
        path: String,
    },
    OpenAdd,
    CloseAdd,
    ChooseFolder(FolderTarget),
    Create,
}

pub struct DedupApp {
    store: Arc<Store>,
    tab: Tab,
    /// The tab shown last frame; when it changes we re-sync the newly-shown
    /// view's repo list from the store (so a repo added in the Repositories tab
    /// appears immediately — no manual refresh button needed).
    synced_tab: Option<Tab>,
    repos: Vec<RepoRow>,
    load_error: Option<String>,
    /// Transient non-error notice (e.g. a drag-and-drop add summary).
    notice: Option<String>,
    /// A scan was refused because it would have emptied this repo's index
    /// (name, entry count). Shows the confirmation that can authorise it.
    empty_scan_confirm: Option<(String, u64)>,

    show_add: bool,
    new_name: String,
    new_path: String,
    form_error: Option<String>,
    edit: Option<Edit>,

    show_settings: bool,
    show_about: bool,
    show_help: bool,
    /// The Status panel (Warnings + Activity) is open.
    show_status: bool,
    /// In-memory diagnostics registry, shared with background workers: startup
    /// probes and runtime failures file Critical/Warning events here, surfaced
    /// by the Status button.
    diag: crate::diagnostics::Diagnostics,
    /// Whether the one-time startup status probe has been kicked off.
    did_initial_status: bool,
    threads: usize,
    tooltip_verbosity: TooltipVerbosity,
    /// Chosen interface appearance; applied to egui each frame.
    theme: ThemeChoice,

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
    /// Sync groups as of the last reload: the Repositories list frames each
    /// group — its main and that main's sinks — in one LCARS section.
    groups: Vec<(String, dedup_core::store::SyncGroup)>,
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
        let (status_tx, status_rx) = crossbeam_channel::unbounded();
        // One lock registry for the whole app: a repo unlocked on any tab is
        // unlocked on every tab, for this session only.
        let locks = crate::locks::RepoLocks::new();
        let mut app = Self {
            store,
            tab: Tab::Repositories,
            synced_tab: None,
            repos: Vec::new(),
            load_error: None,
            notice: None,
            empty_scan_confirm: None,
            show_add: false,
            new_name: String::new(),
            new_path: String::new(),
            form_error: None,
            edit: None,
            show_settings: false,
            show_about: false,
            show_help: false,
            show_status: false,
            diag: crate::diagnostics::Diagnostics::new(),
            did_initial_status: false,
            threads: 0,
            tooltip_verbosity: TooltipVerbosity::default(),
            theme: ThemeChoice::default(),
            tx,
            rx,
            worker: WorkerState::default(),
            queue: VecDeque::new(),
            cancels: HashMap::new(),
            status_tx,
            status_rx,
            dupes: DupesView::new_with_locks(locks.clone()),
            transfer: TransferView::new_with_locks(locks.clone()),
            grooming: GroomingView::new_with_locks(locks.clone()),
            groups: Vec::new(),
            browse: crate::browse_view::BrowseView::new_with_locks(locks),
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
        app.theme = settings.theme;
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
            theme: self.theme,
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

    /// Re-sync the newly-shown view from the store, once per tab switch, so
    /// repos added or changed on another tab appear without a refresh button.
    ///
    /// The Repositories tab re-reads its own cards here: file counts and free
    /// space otherwise stayed stale after deleting duplicates on another tab
    /// until the user refreshed by hand. `reload_all` opens each repo db, so it
    /// runs only while no update is in flight — the same gate every other call
    /// site uses. Skipping a busy frame is harmless, because a job's completion
    /// handler reloads anyway.
    fn sync_shown_tab(&mut self) {
        if self.synced_tab == Some(self.tab) {
            return;
        }
        match self.tab {
            Tab::Repositories => {
                if self.worker.active_count() == 0 {
                    self.reload_all();
                }
            }
            Tab::Duplicates => self.dupes.sync_repos(&self.store),
            Tab::Transfer => self.transfer.sync_repos(&self.store),
            Tab::Grooming => self.grooming.sync_repos(&self.store),
            Tab::Browse => self.browse.sync_repos(&self.store),
        }
        self.synced_tab = Some(self.tab);
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
                    let mimes = crate::util::or_log_default(
                        self.store.get_mime_stats(&name),
                        &format!("mime stats for '{name}'"),
                    );
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
                self.notice = None;
                // Not silently defaulted: without the groups a sink renders as
                // an unrelated top-level repo, so the user would be acting on a
                // list that misrepresents what they own.
                match self.store.list_sync_groups() {
                    Ok(groups) => self.groups = groups,
                    Err(e) => {
                        log::error!("could not read sync groups: {e}");
                        self.groups.clear();
                        self.load_error = Some(format!(
                            "Could not read sync groups: {e}. Backup sinks are listed as \
                             ordinary repositories until this is resolved."
                        ));
                    }
                }
                // The repo set may have changed (add/remove/rename/relocate);
                // force the selector tabs to re-sync when next shown.
                self.synced_tab = None;
            }
            Err(e) => self.load_error = Some(e.to_string()),
        }
    }

    /// Add any folders dropped onto the window as repositories. Non-directory
    /// drops are ignored; each repo's name is derived from the folder's basename
    /// and made unique against existing repos and others in the same drop. No-op
    /// while an update runs (the registry is locked then, like the ADD button).
    fn handle_dropped_folders(&mut self, ctx: &egui::Context) {
        let (dropped, drop_count) = ctx.input(|i| {
            let dropped: Vec<PathBuf> = i
                .raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect();
            (dropped, i.raw.dropped_files.len())
        });
        if drop_count == 0 {
            return;
        }
        // Something was dropped but the windowing backend delivered no usable
        // path (seen on Wayland) — say so instead of silently ignoring it.
        if dropped.is_empty() {
            self.load_error = Some(
                "Your desktop didn't provide file paths for the drop (common on Wayland) — \
                 use ADD REPOSITORY instead."
                    .into(),
            );
            return;
        }
        if self.worker.active_count() > 0 {
            self.load_error = Some(
                "Can't add repositories while an update is running — try again once it finishes."
                    .into(),
            );
            return;
        }

        let mut taken: HashSet<String> = self.repos.iter().map(|r| r.name.clone()).collect();
        let mut added: Vec<String> = Vec::new();
        let mut errors: Vec<String> = Vec::new();
        let mut non_dirs = 0usize;
        for path in dropped {
            if !path.is_dir() {
                non_dirs += 1;
                continue;
            }
            let name = unique_repo_name(&sanitize_repo_name(&path), &taken);
            match self.store.create_repo(&name, &path.to_string_lossy()) {
                Ok(()) => {
                    taken.insert(name.clone());
                    added.push(name);
                }
                Err(e) => errors.push(format!("{}: {e}", path.display())),
            }
        }

        if !added.is_empty() {
            self.reload_all();
            self.refresh_status(ctx);
            self.tab = Tab::Repositories; // show the result of the drop
            self.load_error = None;
            self.notice = Some(format!(
                "Added {} repositor{}: {}",
                added.len(),
                if added.len() == 1 { "y" } else { "ies" },
                added.join(", "),
            ));
        }
        let mut problems: Vec<String> = Vec::new();
        if non_dirs > 0 {
            problems.push(format!(
                "ignored {non_dirs} dropped item(s) that weren't folders"
            ));
        }
        problems.extend(errors);
        if !problems.is_empty() {
            self.load_error = Some(problems.join("; "));
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
                log::info!("starting {kind:?} of '{name}' on {threads} thread(s)");
                let progress = ChannelProgress::new(name.clone(), tx.clone());
                let outcome = match kind {
                    JobKind::Update | JobKind::UpdateForced => {
                        let allow_empty = matches!(kind, JobKind::UpdateForced);
                        match dedup_core::update::update_repo_authorized(
                            &store,
                            &name,
                            threads,
                            &progress,
                            &cancel,
                            allow_empty,
                        ) {
                            // Not an error to report: the UI turns this into a
                            // confirmation offering to scan anyway.
                            Err(dedup_core::update::UpdateError::WouldEmptyIndex {
                                entries,
                                ..
                            }) => JobOutcome::UpdateWouldEmpty(entries),
                            other => JobOutcome::Update(other.map_err(|e| e.to_string())),
                        }
                    }
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

    /// Route a folder chosen from the native picker into whichever field asked
    /// for it (add form, relocate editor, or duplicate editor).
    fn route_picked_folder(&mut self, target: FolderTarget, dir: PathBuf) {
        let picked = dir.to_string_lossy().into_owned();
        match target {
            // Add form: fill the path and auto-name from the last path component
            // unless the user already typed a name.
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
            // Duplicate editor: fill the path, and auto-name the copy from the
            // folder's basename unless a name was already typed.
            FolderTarget::Duplicate => {
                if let Some(Edit::Duplicate { dest, path, .. }) = &mut self.edit {
                    if dest.trim().is_empty()
                        && let Some(base) = dir.file_name()
                    {
                        *dest = base.to_string_lossy().into_owned();
                    }
                    *path = picked;
                }
            }
            // Add-sink editor: same behaviour as the duplicate editor.
            FolderTarget::AddSink => {
                if let Some(Edit::AddSink { dest, path, .. }) = &mut self.edit {
                    if dest.trim().is_empty()
                        && let Some(base) = dir.file_name()
                    {
                        *dest = base.to_string_lossy().into_owned();
                    }
                    *path = picked;
                }
            }
        }
    }

    fn apply(&mut self, ctx: &egui::Context, frame: &eframe::Frame, action: Action) {
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
            Action::MakeMain(name) => {
                // Name the group after its main. Repo names are unique and groups
                // are a separate keyspace, so this only clashes with a group
                // already named for another repo — surfaced as an error.
                if let Err(e) = self.store.create_sync_group(&name, &name) {
                    self.load_error = Some(e.to_string());
                }
                self.reload_all();
            }
            Action::SinkInto { repo, group } => {
                if let Err(e) =
                    self.store
                        .add_sync_sink(&group, &repo, dedup_core::store::SyncMode::AddOnly)
                {
                    self.load_error = Some(e.to_string());
                }
                self.reload_all();
            }
            Action::RemoveSink { group, repo } => {
                if let Err(e) = self.store.remove_sync_sink(&group, &repo) {
                    self.load_error = Some(e.to_string());
                }
                self.reload_all();
            }
            Action::SetSinkMode { group, repo, mode } => {
                if let Err(e) = self.store.set_sink_mode(&group, &repo, mode) {
                    self.load_error = Some(e.to_string());
                }
                self.reload_all();
            }
            Action::UpdateGroup(group) => {
                // Queue every member (main + sinks). enqueue takes names only, so
                // collect first to avoid borrowing `self.groups` across the call.
                let members: Vec<String> = self
                    .groups
                    .iter()
                    .find(|(n, _)| *n == group)
                    .map(|(_, g)| g.members().map(str::to_string).collect())
                    .unwrap_or_default();
                // Skip known-unreachable members, like UPDATE ALL — a backup on an
                // unplugged drive would otherwise hang a worker. Not-yet-probed
                // (Unknown) members are still included.
                for member in members {
                    let reachable = self
                        .repos
                        .iter()
                        .find(|r| r.name == member)
                        .is_none_or(|r| r.location.is_none_or(|l| l.reachable()));
                    if reachable {
                        self.enqueue(member, JobKind::Update);
                    }
                }
            }
            Action::Ungroup(group) => {
                if let Err(e) = self.store.delete_sync_group(&group) {
                    self.load_error = Some(e.to_string());
                }
                self.reload_all();
            }
            Action::BeginAddSink { group, main } => {
                self.edit = Some(Edit::AddSink {
                    main,
                    group,
                    dest: String::new(),
                    path: String::new(),
                });
            }
            Action::CommitAddSink {
                group,
                main,
                dest,
                path,
            } => {
                self.edit = None;
                if dest.is_empty() || path.is_empty() {
                    self.load_error = Some("Add repo needs a new name and path.".into());
                } else {
                    match self.store.duplicate_repo(&main, &dest, &path) {
                        Ok(()) => {
                            if let Err(e) = self.store.add_sync_sink(
                                &group,
                                &dest,
                                dedup_core::store::SyncMode::AddOnly,
                            ) {
                                self.load_error = Some(e.to_string());
                            }
                        }
                        Err(e) => self.load_error = Some(e.to_string()),
                    }
                    self.reload_all();
                }
            }
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
                // Run the native picker modally, parented to our window: it grabs
                // focus and the app can't spawn a second one while it's open.
                // This blocks the UI thread until the user picks or cancels.
                if let Some(dir) = rfd::FileDialog::new()
                    .set_title("Choose a folder")
                    .set_parent(frame)
                    .pick_folder()
                {
                    self.route_picked_folder(target, dir);
                }
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
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // Drive egui from the chosen appearance, then follow whatever it
        // resolved to this frame so the application's own colours read from the
        // matching palette. Setting the same preference each frame is idempotent.
        ctx.set_theme(self.theme.preference());
        theme::sync_active(&ctx);
        // One-time startup probe of every repo's location/reachability, plus a
        // health check of the environment (audio device, external tools) so the
        // Status button warns about anything missing before it silently bites.
        if !self.did_initial_status {
            self.did_initial_status = true;
            self.refresh_status(&ctx);
            self.probe_environment(&ctx);
        }

        // Drain worker messages; a completed job updates the repo's row.
        for (repo, outcome) in self.worker.drain(&self.rx) {
            self.cancels.remove(&repo);
            match outcome {
                JobOutcome::UpdateWouldEmpty(entries) => {
                    // Nothing was written. Ask before letting a scan empty an
                    // index — an unmounted drive looks exactly like this, and an
                    // emptied sync-group main turns the next MIRROR into a wipe.
                    log::warn!(
                        "scan of '{repo}' refused: it walked empty over {entries} indexed entries"
                    );
                    self.empty_scan_confirm = Some((repo.clone(), entries));
                }
                JobOutcome::Update(result) => {
                    // A clean, uncancelled update brings the index in sync.
                    let clean = matches!(&result, Ok(s) if !s.cancelled);
                    // Anything a bug report would want to find in the log.
                    let worrying = match &result {
                        Err(_) => true,
                        Ok(s) => s.empty_walk || s.errors > 0,
                    };
                    let summary = match result {
                        Ok(s) if s.cancelled => {
                            format!("cancelled — added {}, updated {}", s.added, s.updated)
                        }
                        Ok(s) => {
                            let mut text = format!(
                                "added {}, updated {}, unchanged {}, missing {}, errors {}",
                                s.added, s.updated, s.unchanged, s.marked_missing, s.errors
                            );
                            // The scan saw an empty directory where the index
                            // held files. Usually a drive that did not mount —
                            // and an emptied repo is what turns a MIRROR sync
                            // into a wipe, so it must not read as a normal scan.
                            if s.empty_walk {
                                text.push_str(
                                    " — FOUND NO FILES AT ALL; check the drive is mounted",
                                );
                            }
                            text
                        }
                        Err(e) => format!("error: {e}"),
                    };
                    if worrying {
                        log::warn!("scan of '{repo}': {summary}");
                    } else {
                        log::info!("scan of '{repo}': {summary}");
                    }
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
            // A repo whose folder is gone or unreachable files one aggregated
            // Critical (keyed per repo, so it never floods), dovetailing with
            // the card's own MISSING/OFFLINE pill; when it comes back the event
            // clears itself.
            let key = format!("repo-unreachable:{repo}");
            match location {
                Location::Missing => self.diag.push(
                    crate::diagnostics::Severity::Critical,
                    &key,
                    format!("Repository '{repo}' is unreachable"),
                    "Its folder no longer exists or can't be read — a disconnected drive or an \
                     unmounted cloud folder looks exactly like this. Its files can't be scanned \
                     or previewed until it's back; nothing has been deleted.",
                ),
                Location::Offline => self.diag.push(
                    crate::diagnostics::Severity::Critical,
                    &key,
                    format!("Repository '{repo}' is offline"),
                    "A network location that isn't reachable. Reconnect it to scan or \
                     preview its files.",
                ),
                Location::Local | Location::Remote => self.diag.clear(&key),
            }
        }

        // Folders dropped onto the window are added as repositories.
        self.handle_dropped_folders(&ctx);
        // While folders hover the window, show a full-window drop affordance.
        if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            let screen = ctx.content_rect();
            let p = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("dnd-hint"),
            ));
            p.rect_filled(screen, 0.0, egui::Color32::from_black_alpha(190));
            p.text(
                screen.center(),
                egui::Align2::CENTER_CENTER,
                "Drop folders to add them as repositories",
                egui::FontId::proportional(22.0),
                theme::amber(),
            );
        }

        let mut actions: Vec<Action> = Vec::new();
        self.top_bar(ui);
        // Audio preview belongs to the Duplicates tab; stop it elsewhere.
        if self.tab != Tab::Duplicates {
            self.dupes.stop_audio();
        }
        // Folder read-ahead belongs to the Browse tab; elsewhere the disk is
        // the user's.
        if self.tab != Tab::Browse {
            self.browse.cancel_prefetch();
        }
        // On each tab switch, re-sync the newly-shown view's repo list from the
        // store, so repos added/removed elsewhere appear without a refresh
        // button. (The Repositories tab refreshes its own cards separately.)
        self.sync_shown_tab();
        egui::CentralPanel::default().show(ui, |ui| match self.tab {
            Tab::Repositories => self.repositories_view(ui, &mut actions),
            Tab::Duplicates => self.dupes.show(ui, &self.store, self.tooltip_verbosity),
            Tab::Transfer => {
                self.transfer
                    .show(ui, &self.store, self.tooltip_verbosity, Some(frame))
            }
            Tab::Grooming => self.grooming.show(ui, &self.store, self.tooltip_verbosity),
            Tab::Browse => self.browse.show(ui, &self.store, self.tooltip_verbosity),
        });
        if self.show_settings {
            self.settings_modal(&ctx);
        }
        if self.show_about {
            self.about_modal(&ctx);
        }
        if self.show_status {
            self.status_panel(&ctx);
        }
        if self.empty_scan_confirm.is_some() {
            self.empty_scan_modal(&ctx);
        }
        if self.show_help {
            self.help_window(&ctx);
        }
        if self.show_add {
            self.add_modal(&ctx, &mut actions);
        }
        for action in actions {
            self.apply(&ctx, frame, action);
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
            || current.tooltip_verbosity != self.saved_settings.tooltip_verbosity
            || current.theme != self.saved_settings.theme;
        if control_changed {
            current.save(self.store.config_dir());
            self.saved_settings = current;
        }
    }

    fn on_exit(&mut self) {
        // Flush the final window size (and any current control values) so the
        // next launch reopens where the user left it.
        self.current_settings().save(self.store.config_dir());
        // Converted-office-PDF cache: the user's documents must not outlive
        // the session on disk (this also sweeps a crashed session's leftover).
        crate::compare_view::remove_office_cache();
    }
}

impl DedupApp {
    fn top_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("top")
            .exact_size(56.0)
            .frame(egui::Frame::new().fill(theme::bg()).inner_margin(8.0))
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(
                        RichText::new("DEDUP")
                            .color(theme::orange())
                            .size(26.0)
                            .strong(),
                    );
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                            .color(theme::lilac())
                            .size(13.0),
                    );
                    ui.add_space(16.0);
                    // SETTINGS/ABOUT are pinned to the right (reserved first, in a
                    // right-to-left layout); the tab strip fills the space between
                    // the version and those buttons. The minimum window width can't
                    // fit all five tabs plus these buttons, so the strip scrolls
                    // horizontally when cramped (auto-hiding scrollbar) instead of
                    // letting the last tab slide behind ABOUT.
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if crate::lcars::action_button(
                            ui,
                            &format!("{} SETTINGS", icon::GEAR),
                            true,
                            theme::tan(),
                        )
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
                        if crate::lcars::action_button(ui, "ABOUT", true, theme::tan())
                            .explain(
                                self.tooltip_verbosity,
                                "Version and license",
                                "Show the app version, license, and contact info.",
                            )
                            .clicked()
                        {
                            self.show_about = true;
                        }
                        // Added after ABOUT so it renders immediately to its left.
                        if crate::lcars::action_button(ui, "HELP", true, theme::tan())
                            .explain(
                                self.tooltip_verbosity,
                                "Explain the current tab",
                                "Open a help window describing what the current tab is for \
                                 and how its controls fit together. Stays open (and updates) \
                                 as you switch tabs.",
                            )
                            .clicked()
                        {
                            self.show_help = true;
                        }
                        // STATUS: the health/activity centre. Its icon carries an
                        // amber count badge when there are unread warnings, so a
                        // silent problem (no audio device, missing ffmpeg, a file
                        // gone) announces itself instead of biting quietly.
                        let unread = self.diag.unread_count();
                        let label = if unread > 0 {
                            format!("{} STATUS ({unread})", icon::LIGHTNING)
                        } else {
                            format!("{} STATUS", icon::LIGHTNING)
                        };
                        let accent = if unread > 0 {
                            theme::amber()
                        } else {
                            theme::tan()
                        };
                        if crate::lcars::action_button(ui, &label, true, accent)
                            .explain(
                                self.tooltip_verbosity,
                                "Warnings and background activity",
                                "Show health warnings (missing audio device, ffmpeg, unreachable \
                                 files) and running background work. Each warning can be copied \
                                 for a bug report.",
                            )
                            .clicked()
                        {
                            self.show_status = true;
                            self.diag.mark_all_read();
                        }
                        // The remaining width (left of HELP) holds the scrollable
                        // tab strip, laid out left-to-right in its natural order.
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                            egui::ScrollArea::horizontal()
                                .auto_shrink([false, true])
                                .show(ui, |ui| {
                                    tab_button(
                                        ui,
                                        &mut self.tab,
                                        Tab::Repositories,
                                        "REPOSITORIES",
                                        theme::orange(),
                                        self.tooltip_verbosity,
                                        "Add, update, and manage repository links",
                                    );
                                    tab_button(
                                        ui,
                                        &mut self.tab,
                                        Tab::Duplicates,
                                        "DUPLICATES",
                                        theme::lilac(),
                                        self.tooltip_verbosity,
                                        "Find and review exact or perceptually similar duplicates",
                                    );
                                    tab_button(
                                        ui,
                                        &mut self.tab,
                                        Tab::Transfer,
                                        "TRANSFER",
                                        theme::blue(),
                                        self.tooltip_verbosity,
                                        "Copy, move, sync or push a backup group between \
                                         repositories by content",
                                    );
                                    tab_button(
                                        ui,
                                        &mut self.tab,
                                        Tab::Grooming,
                                        "GROOMING",
                                        theme::tan(),
                                        self.tooltip_verbosity,
                                        "Prune and reorganize repositories (coming soon)",
                                    );
                                    tab_button(
                                        ui,
                                        &mut self.tab,
                                        Tab::Browse,
                                        "BROWSE",
                                        theme::amber(),
                                        self.tooltip_verbosity,
                                        "Browse a repo's files by directory, from the index",
                                    );
                                });
                        });
                    });
                });
            });
    }

    fn repositories_view(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        ui.add_space(6.0);
        ui.label(
            RichText::new("REPOSITORY MANAGEMENT")
                .color(theme::amber())
                .size(18.0)
                .strong(),
        );
        ui.add_space(4.0);

        if let Some(err) = &self.load_error {
            ui.colored_label(theme::red(), err);
        }
        if let Some(notice) = &self.notice {
            ui.colored_label(theme::amber(), notice);
        }

        // The registry is locked while any repo is updating, so adding a repo
        // (which reads every repo's stats) must wait until scans finish.
        let busy = self.worker.active_count() > 0;
        crate::lcars::section_lcars(
            ui,
            "MANAGE — ADD & UPDATE REPOSITORIES",
            theme::blue(),
            |ui| {
                ui.horizontal(|ui| {
                    let add = egui::Button::new(
                        RichText::new(format!("{} ADD REPOSITORY", icon::PLUS))
                            .color(theme::ink_on(theme::blue())),
                    )
                    .fill(theme::blue());
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
                        RichText::new(format!("{} UPDATE ALL", icon::REFRESH))
                            .color(theme::black()),
                    )
                    .fill(theme::orange());
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
                        RichText::new(format!("{} REFRESH STATUS", icon::REFRESH))
                            .color(theme::ink_on(theme::lilac())),
                    )
                    .fill(theme::lilac());
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
                                .color(theme::tan())
                                .size(12.0),
                        );
                    }
                });
            },
        );
        ui.add_space(4.0);

        let rows = self.repos.clone();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if rows.is_empty() {
                    ui.add_space(8.0);
                    ui.colored_label(theme::text(), "No repositories yet — use ADD REPOSITORY.");
                }
                for row in &rows {
                    // A sink is shown inside its group's section, not as a
                    // top-level repo.
                    if self.sink_of(&row.name).is_some() {
                        continue;
                    }
                    // A group main and its sinks are framed together by one LCARS
                    // elbow rail, so a group reads as a single block and an
                    // ungrouped repo as a bare card.
                    match self.group_of_main(&row.name) {
                        Some(group_name) => {
                            let name = group_name.clone();
                            // Folded by default: the repo list is about your
                            // originals, so a group's backups stay out of the way
                            // until you ask for them.
                            crate::lcars::section_lcars_collapsible(
                                ui,
                                &name,
                                theme::green(),
                                false,
                                |ui| {
                                    self.repo_card(ui, row, actions);
                                    self.group_section(ui, &row.name, &rows, actions);
                                },
                            );
                        }
                        None => self.repo_card(ui, row, actions),
                    }
                }
            });
    }

    /// The group a repo is a *sink* of, if any (a main is never collapsed).
    fn sink_of(&self, repo: &str) -> Option<&(String, dedup_core::store::SyncGroup)> {
        self.groups
            .iter()
            .find(|(_, g)| g.sinks.iter().any(|s| s.repo == repo))
    }

    /// The name of the group `repo` is the *main* of, if any — the title of the
    /// LCARS section that frames the group.
    fn group_of_main(&self, repo: &str) -> Option<&String> {
        self.groups
            .iter()
            .find(|(_, g)| g.main == repo)
            .map(|(n, _)| n)
    }

    /// The body of a group's LCARS section, drawn under its main's card: the
    /// group controls (ADD REPO, UPDATE ALL, UNGROUP), shown for any main — even
    /// one with no sinks yet — then each sink's own card.
    ///
    /// There is no chevron here: the enclosing section's caret is the single
    /// control that folds the whole group away.
    fn group_section(
        &mut self,
        ui: &mut egui::Ui,
        main: &str,
        rows: &[RepoRow],
        actions: &mut Vec<Action>,
    ) {
        let Some((group_name, group)) = self
            .groups
            .iter()
            .find(|(_, g)| g.main == main)
            .map(|(n, g)| (n.clone(), g.clone()))
        else {
            return;
        };
        let verbosity = self.tooltip_verbosity;
        ui.horizontal(|ui| {
            ui.add_space(16.0);
            if crate::lcars::action_button(
                ui,
                &format!("{} ADD REPO", icon::PLUS),
                true,
                theme::blue(),
            )
            .explain(
                verbosity,
                "Add a backup repository to this group",
                "Add a backup to this group: a clone of the main's index, pointed at a new \
                 folder.",
            )
            .clicked()
            {
                actions.push(Action::BeginAddSink {
                    group: group_name.clone(),
                    main: main.to_string(),
                });
            }
            if crate::lcars::action_button(
                ui,
                &format!("{} UPDATE ALL", icon::REFRESH),
                true,
                theme::amber(),
            )
            .explain(
                verbosity,
                "Scan the whole group",
                "Queue an UPDATE / SCAN for the main and every backup in this group.",
            )
            .clicked()
            {
                actions.push(Action::UpdateGroup(group_name.clone()));
            }
            if crate::lcars::action_button(ui, "UNGROUP", true, theme::lilac())
                .explain(
                    verbosity,
                    "Disband this group",
                    "Disband this group. Every repository stays; they are just no longer \
                     linked as main and backups.",
                )
                .clicked()
            {
                actions.push(Action::Ungroup(group_name.clone()));
            }
        });
        if group.sinks.is_empty() {
            return;
        }
        // No chevron here: the enclosing LCARS section's own caret folds the
        // whole group away, and two competing collapse controls read as a bug.
        for sink in &group.sinks {
            if let Some(row) = rows.iter().find(|r| r.name == sink.repo) {
                self.repo_card(ui, row, actions);
            }
        }
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
            .fill(theme::panel())
            .corner_radius(theme::PILL)
            .stroke(egui::Stroke::new(1.5, theme::orange()))
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
                            .color(theme::amber())
                            .size(17.0)
                            .strong(),
                    );
                    // Inside a group section both the main and its sinks are
                    // cards; the badge is what tells them apart.
                    if self.group_of_main(&row.name).is_some() {
                        main_pill(ui, self.tooltip_verbosity);
                    }
                    status_pills(ui, row, self.tooltip_verbosity);
                    // MIME breakdown, share-sorted, pinned to the top-right. It is
                    // reserved first (right-to-left) so the path — added inside,
                    // filling the gap between the status pills and the tags — can
                    // truncate to fit instead of running under the tags on a narrow
                    // window (the full path stays on hover). Both share one row.
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        mime_tags(ui, row, self.tooltip_verbosity);
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(&row.path).color(theme::text()).size(12.0),
                                )
                                .truncate(),
                            )
                            .explain(
                                self.tooltip_verbosity,
                                &row.path,
                                &format!("On-disk folder this repository indexes: {}", row.path),
                            );
                        });
                    });
                });
                ui.horizontal(|ui| {
                    stat(
                        ui,
                        "FILES",
                        &row.stats.file_count.to_string(),
                        theme::orange(),
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
                        theme::blue(),
                        "Total size of indexed files",
                        "Sum of the on-disk size of every indexed (non-missing) file in this \
                         repository.",
                        self.tooltip_verbosity,
                    );
                    stat(
                        ui,
                        "MISSING",
                        &row.stats.missing_count.to_string(),
                        theme::lilac(),
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
                        theme::tan(),
                        "When this repository was last scanned",
                        "Date and time of the most recent completed UPDATE / SCAN of this \
                         repository. \"never\" means it hasn't been scanned yet.",
                        self.tooltip_verbosity,
                    );
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
                                .color(theme::tan()),
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
                            ui.add(egui::Spinner::new().color(theme::amber()));
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
                                        .color(theme::amber()),
                                    );
                                }
                                other => {
                                    ui.label(
                                        RichText::new(progress_line(other)).color(theme::amber()),
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
                        ui.label(RichText::new(timing).color(theme::tan()).size(12.0));
                    }
                    None => {
                        self.card_controls(ui, row, actions);
                        if let Some(last) = &row.last {
                            ui.label(RichText::new(last).color(theme::tan()).size(12.0));
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
                    ui.label(RichText::new("RENAME →").color(theme::lilac()));
                    ui.text_edit_singleline(buf).explain(
                        verbosity,
                        "New name",
                        "The new name for this repository (its registry entry only — the \
                         on-disk folder it points at is unchanged).",
                    );
                    if ui
                        .button(RichText::new(format!("{} OK", icon::CHECK)).color(theme::black()))
                        .explain(verbosity, "Confirm rename", "Apply the new name.")
                        .clicked()
                    {
                        actions.push(Action::CommitRename(name.clone(), buf.trim().to_string()));
                    }
                    if ui
                        .button(RichText::new(icon::X).color(theme::black()))
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
                    ui.label(RichText::new("RELOCATE →").color(theme::lilac()));
                    if ui
                        .button(
                            RichText::new(format!("{} CHOOSE…", icon::FOLDER_OPEN))
                                .color(theme::black()),
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
                        .button(RichText::new(format!("{} OK", icon::CHECK)).color(theme::black()))
                        .explain(verbosity, "Confirm relocate", "Apply the new folder path.")
                        .clicked()
                    {
                        actions.push(Action::CommitRelocate(name.clone(), buf.trim().to_string()));
                    }
                    if ui
                        .button(RichText::new(icon::X).color(theme::black()))
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
                    ui.label(RichText::new("COPY → NAME").color(theme::lilac()));
                    ui.add(egui::TextEdit::singleline(dest).desired_width(140.0))
                        .explain(
                            verbosity,
                            "New repository's name",
                            "Name for the new repository the index is copied into.",
                        );
                    ui.label(RichText::new("PATH").color(theme::lilac()));
                    if ui
                        .button(
                            RichText::new(format!("{} CHOOSE…", icon::FOLDER_OPEN))
                                .color(theme::black()),
                        )
                        .explain(
                            verbosity,
                            "Pick a folder",
                            "Open a native folder picker to choose the on-disk folder the \
                             new repository will point at.",
                        )
                        .clicked()
                    {
                        actions.push(Action::ChooseFolder(FolderTarget::Duplicate));
                    }
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
                        .button(RichText::new(format!("{} OK", icon::CHECK)).color(theme::black()))
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
                        .button(RichText::new(icon::X).color(theme::black()))
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
            Some(Edit::AddSink {
                main,
                group,
                dest,
                path,
            }) if *main == row.name => {
                let verbosity = self.tooltip_verbosity;
                ui.horizontal(|ui| {
                    ui.label(RichText::new("ADD SINK → NAME").color(theme::green()));
                    ui.add(egui::TextEdit::singleline(dest).desired_width(140.0))
                        .explain(
                            verbosity,
                            "New repository's name",
                            "Name for the new backup repository. It starts as a copy of this \
                             group's main index, pointed at the folder you choose.",
                        );
                    ui.label(RichText::new("PATH").color(theme::green()));
                    if ui
                        .button(
                            RichText::new(format!("{} CHOOSE…", icon::FOLDER_OPEN))
                                .color(theme::black()),
                        )
                        .explain(
                            verbosity,
                            "Pick a folder",
                            "Open a native folder picker to choose where the new backup \
                             repository's files live.",
                        )
                        .clicked()
                    {
                        actions.push(Action::ChooseFolder(FolderTarget::AddSink));
                    }
                    ui.add(
                        egui::TextEdit::singleline(path)
                            .desired_width(240.0)
                            .hint_text("/backup/repo/path"),
                    )
                    .explain(
                        verbosity,
                        "New repository's folder",
                        "On-disk folder the new backup repository will point at. The main \
                         is left completely unchanged.",
                    );
                    if ui
                        .button(RichText::new(format!("{} OK", icon::CHECK)).color(theme::black()))
                        .explain(
                            verbosity,
                            "Add this backup",
                            "Clone the main's index into the new repository at the chosen \
                             path and add it to this group as a sink.",
                        )
                        .clicked()
                    {
                        actions.push(Action::CommitAddSink {
                            group: group.clone(),
                            main: main.clone(),
                            dest: dest.trim().to_string(),
                            path: path.trim().to_string(),
                        });
                    }
                    if ui
                        .button(RichText::new(icon::X).color(theme::black()))
                        .explain(verbosity, "Cancel", "Discard and close the editor.")
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
                    ui.colored_label(theme::red(), format!("Delete '{name}' and its index?"));
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new("DELETE").color(theme::ink_on(theme::red())),
                            )
                            .fill(theme::red()),
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
                        .button(RichText::new("KEEP").color(theme::black()))
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
                RichText::new(format!("{} UPDATE / SCAN", icon::REFRESH)).color(theme::black()),
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
                RichText::new(format!("{} CHECK", icon::SEARCH)).color(theme::black()),
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
                .button(RichText::new(format!("{} RENAME", icon::PENCIL)).color(theme::black()))
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
                .button(RichText::new(format!("{} RELOCATE", icon::RELOCATE)).color(theme::black()))
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
                .button(RichText::new(format!("{} DUPLICATE", icon::COPY)).color(theme::black()))
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
                        RichText::new(format!("{} DELETE", icon::TRASH))
                            .color(theme::ink_on(theme::red())),
                    )
                    .fill(theme::red()),
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

        // Sync-group membership actions. A main's group controls live on its
        // group section (below the card), so only sinks and ungrouped repos get a
        // button here.
        let is_main = self.groups.iter().any(|(_, g)| g.main == row.name);
        let sink_group = self.sink_of(&row.name).map(|(n, _)| n.clone());
        // This repo's own push mode when it is a sink, for its mode pill.
        let sink_mode = self
            .sink_of(&row.name)
            .and_then(|(_, g)| g.sinks.iter().find(|s| s.repo == row.name).map(|s| s.mode));
        // Every group as (group name, main), to offer as SINK INTO targets.
        let group_targets: Vec<(String, String)> = self
            .groups
            .iter()
            .map(|(n, g)| (n.clone(), g.main.clone()))
            .collect();
        let verbosity = self.tooltip_verbosity;
        if !is_main {
            ui.horizontal(|ui| {
                if let Some(group) = &sink_group {
                    // This backup's own push mode — MIRROR deletes what the main
                    // dropped; ADD ONLY only copies. Click to flip.
                    let is_mirror = sink_mode == Some(dedup_core::store::SyncMode::Mirror);
                    let mode_label = if is_mirror {
                        "MODE: MIRROR"
                    } else {
                        "MODE: ADD ONLY"
                    };
                    if crate::lcars::toggle_button(ui, mode_label, is_mirror, theme::orange())
                        .explain(
                            verbosity,
                            "How this backup is pushed — click to flip",
                            "How this backup is pushed. ADD ONLY copies what it lacks; MIRROR \
                             also deletes from it what the main no longer has. Click to flip.",
                        )
                        .clicked()
                    {
                        let mode = if is_mirror {
                            dedup_core::store::SyncMode::AddOnly
                        } else {
                            dedup_core::store::SyncMode::Mirror
                        };
                        actions.push(Action::SetSinkMode {
                            group: group.clone(),
                            repo: row.name.clone(),
                            mode,
                        });
                    }
                    if ui
                        .button(
                            RichText::new(format!("{} SINK OUT", icon::X)).color(theme::black()),
                        )
                        .explain(
                            verbosity,
                            "Take this backup out of its group",
                            "Remove this repository from its sync group. Both repositories \
                             stay; they are just no longer linked as main and backup.",
                        )
                        .clicked()
                    {
                        actions.push(Action::RemoveSink {
                            group: group.clone(),
                            repo: row.name.clone(),
                        });
                    }
                } else {
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new(format!("{} MAKE MAIN", icon::STAR))
                                    .color(theme::ink_on(theme::green())),
                            )
                            .fill(theme::green()),
                        )
                        .explain(
                            verbosity,
                            "Make this repository a sync-group main",
                            "Turn this repository into the main of a new sync group. You can \
                             then add backup repositories (sinks) that it is pushed to.",
                        )
                        .clicked()
                    {
                        actions.push(Action::MakeMain(row.name.clone()));
                    }
                    if !group_targets.is_empty() {
                        ui.menu_button(
                            RichText::new(format!("{} SINK INTO", icon::ARROW_RIGHT))
                                .color(theme::text()),
                            |ui| {
                                for (group, main) in &group_targets {
                                    if ui.button(format!("{} {main}", icon::STAR)).clicked() {
                                        actions.push(Action::SinkInto {
                                            repo: row.name.clone(),
                                            group: group.clone(),
                                        });
                                        ui.close();
                                    }
                                }
                            },
                        )
                        .response
                        .explain(
                            verbosity,
                            "Add this repository to a group as a backup",
                            "Add this repository to an existing sync group as a backup (sink) \
                             of that group's main.",
                        );
                    }
                }
            });
        }
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
                    .color(theme::amber())
                    .size(18.0)
                    .strong(),
            );
            ui.add_space(8.0);

            ui.horizontal(|ui| {
                ui.label(RichText::new("FOLDER").color(theme::text()).size(12.0));
                if ui
                    .button(
                        RichText::new(format!("{} CHOOSE…", icon::FOLDER_OPEN))
                            .color(theme::black()),
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
                ui.label(RichText::new("NAME  ").color(theme::text()).size(12.0));
                let mut name_edit = egui::TextEdit::singleline(&mut self.new_name)
                    .desired_width(300.0)
                    .hint_text("defaults to the folder name");
                if clashes {
                    name_edit = name_edit.text_color(theme::red());
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
                    theme::red(),
                    format!("A repository named '{effective}' already exists."),
                );
            } else if let Some(err) = &self.form_error {
                ui.add_space(4.0);
                ui.colored_label(theme::red(), err);
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let add =
                    egui::Button::new(RichText::new("ADD").color(theme::ink_on(theme::blue())))
                        .fill(theme::blue());
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
                    .button(RichText::new("CANCEL").color(theme::black()))
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
                    .color(theme::amber())
                    .size(18.0)
                    .strong(),
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Hashing threads").color(theme::text()));
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
                    .color(theme::tan())
                    .size(12.0),
            );
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Tooltips").color(theme::text()));
                let short = self.tooltip_verbosity == TooltipVerbosity::Short;
                egui::Frame::new()
                    .stroke(egui::Stroke::new(1.0, theme::blue()))
                    .corner_radius(6)
                    .inner_margin(egui::Margin::symmetric(4, 2))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let (short_fill, short_text) = if short {
                                (theme::blue(), theme::black())
                            } else {
                                (theme::panel(), theme::blue())
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
                                (theme::panel(), theme::lilac())
                            } else {
                                (theme::lilac(), theme::black())
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
                    .color(theme::tan())
                    .size(12.0),
            );
            ui.add_space(12.0);

            ui.horizontal(|ui| {
                ui.label(RichText::new("Appearance").color(theme::text()));
                egui::Frame::new()
                    .stroke(egui::Stroke::new(1.0, theme::amber()))
                    .corner_radius(6)
                    .inner_margin(egui::Margin::symmetric(4, 2))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            for (choice, label, short_help, long_help) in [
                                (
                                    ThemeChoice::System,
                                    "SYSTEM",
                                    "Follow your desktop",
                                    "Follows your desktop's own light or dark setting and \
                                     switches whenever that does.",
                                ),
                                (
                                    ThemeChoice::Light,
                                    "LIGHT",
                                    "Always light",
                                    "Uses the light appearance regardless of your desktop \
                                     setting.",
                                ),
                                (
                                    ThemeChoice::Dark,
                                    "DARK",
                                    "Always dark",
                                    "Uses the dark appearance regardless of your desktop \
                                     setting.",
                                ),
                            ] {
                                let on = self.theme == choice;
                                let (fill, txt) = if on {
                                    (theme::amber(), theme::black())
                                } else {
                                    (theme::panel(), theme::amber())
                                };
                                if ui
                                    .add(
                                        egui::Button::new(RichText::new(label).color(txt))
                                            .fill(fill),
                                    )
                                    .explain(self.tooltip_verbosity, short_help, long_help)
                                    .clicked()
                                {
                                    // Apply immediately so the change is visible
                                    // this frame; persistence happens in `ui`.
                                    self.theme = choice;
                                    ctx.set_theme(choice.preference());
                                    theme::sync_active(ctx);
                                }
                            }
                        });
                    });
            });
            ui.label(
                RichText::new(
                    "Dark is the default; System follows your desktop's light/dark setting",
                )
                .color(theme::tan())
                .size(12.0),
            );
            ui.add_space(12.0);

            ui.label(RichText::new("DIAGNOSTICS").color(theme::tan()).size(13.0));
            ui.add_space(4.0);
            if ui
                .add(egui::Button::new(
                    RichText::new("OPEN LOG FOLDER").color(theme::black()),
                ))
                .explain(
                    self.tooltip_verbosity,
                    "Open the folder holding this app's logs",
                    "Opens the folder where dedup records what each run did. The last few \
                     sessions are kept; attach the newest file when reporting a problem.",
                )
                .clicked()
                && let Err(e) = crate::external::open(&dedup_core::logging::log_dir())
            {
                log::error!("could not open the log folder: {e}");
                self.notice = Some(format!(
                    "Could not open the log folder ({}): {e}",
                    dedup_core::logging::log_dir().display()
                ));
            }
            ui.label(
                RichText::new(match dedup_core::logging::current_log() {
                    Some(path) => format!("This session: {}", path.display()),
                    None => format!(
                        "No log this session — {} could not be opened.",
                        dedup_core::logging::log_dir().display()
                    ),
                })
                .color(theme::tan())
                .size(11.0),
            );
            ui.add_space(12.0);

            if ui
                .add(egui::Button::new(
                    RichText::new("CLOSE").color(theme::black()),
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

    /// Confirmation for a scan that walked empty over a non-empty index.
    ///
    /// Refusing is the default reading of the situation — an unmounted drive
    /// scans as an empty directory, and if this repo is a sync group's main the
    /// next MIRROR push would carry the emptiness to every sink. Emptying a repo
    /// on purpose is still supported; it costs this one confirmation.
    fn empty_scan_modal(&mut self, ctx: &egui::Context) {
        let Some((repo, entries)) = self.empty_scan_confirm.clone() else {
            return;
        };
        let mut decision: Option<bool> = None;
        let response = egui::Modal::new(Id::new("empty-scan-confirm")).show(ctx, |ui| {
            ui.set_width(420.0);
            ui.label(
                RichText::new("SCAN FOUND NO FILES")
                    .color(theme::amber())
                    .size(18.0)
                    .strong(),
            );
            ui.add_space(8.0);
            ui.label(
                RichText::new(format!(
                    "Scanning '{repo}' found no files at all, but its index holds {entries}. \
                 Continuing marks every one of them missing."
                ))
                .color(theme::text()),
            );
            ui.add_space(6.0);
            ui.label(
                RichText::new(
                    "If this drive should not be empty, check that it is mounted and scan \
                     again. Nothing has been changed yet.",
                )
                .color(theme::tan())
                .size(12.0),
            );
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new("SCAN ANYWAY").color(theme::ink_on(theme::red())),
                        )
                        .fill(theme::red()),
                    )
                    .explain(
                        self.tooltip_verbosity,
                        "Mark every entry missing",
                        "Run the scan and mark all indexed files missing, because this \
                         repository really is empty now.",
                    )
                    .clicked()
                {
                    decision = Some(true);
                }
                if ui
                    .add(
                        egui::Button::new(RichText::new("CANCEL").color(theme::text()))
                            .fill(theme::panel()),
                    )
                    .explain(
                        self.tooltip_verbosity,
                        "Leave the index alone",
                        "Close without scanning. The index keeps every entry it has.",
                    )
                    .clicked()
                {
                    decision = Some(false);
                }
            });
        });
        if let Some(go) = decision {
            self.empty_scan_confirm = None;
            if go {
                self.enqueue(repo, JobKind::UpdateForced);
            }
        } else if response.should_close() {
            self.empty_scan_confirm = None;
        }
    }

    /// Startup health check: probe the audio device and external tools, record
    /// the system fingerprint for bug reports, and file a Warning for anything
    /// unavailable so the Status button can surface it.
    ///
    /// The probe blocks (audio device init, one process spawn per tool), so it
    /// runs on a background thread — never in a paint frame — and requests one
    /// repaint when it finishes so the Status badge reflects the result. Each
    /// tool is spawned exactly once (both the availability check and the
    /// fingerprint reuse the same result).
    fn probe_environment(&self, ctx: &egui::Context) {
        use crate::diagnostics::Severity::Warning;
        let diag = self.diag.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let probe = crate::diagnostics::probe_system();
            diag.set_fingerprint(probe.fingerprint);
            if !probe.audio_ok {
                diag.push(
                    Warning,
                    "audio-device",
                    "No audio output device",
                    "Playback is disabled. On Linux this usually means ALSA/PipeWire isn't \
                     running, or the machine has no audio device.",
                );
            }
            if !probe.ffmpeg_ok {
                diag.push(
                    Warning,
                    "ffmpeg",
                    "ffmpeg not found on PATH",
                    "Video frames, soundtrack extraction and pitch-preserving playback rates all \
                     need ffmpeg / ffprobe. Install ffmpeg to enable them.",
                );
            }
            if !probe.pdftoppm_ok {
                diag.push(
                    Warning,
                    "pdftoppm",
                    "pdftoppm not found on PATH",
                    "The viewer's PDF Render tab needs pdftoppm (from poppler-utils).",
                );
            }
            // The Render gate reads this flag per file; office documents
            // offer the tab only once the probe has said soffice runs.
            crate::lightbox::set_soffice_available(probe.soffice_ok);
            if !probe.soffice_ok {
                diag.push(
                    Warning,
                    "soffice",
                    "LibreOffice (soffice) not found on PATH",
                    "Office and legacy documents (Word, spreadsheets, .doc, .rtf) can't be \
                     shown as rendered pages without it. Install LibreOffice to enable them.",
                );
            }
            ctx.request_repaint();
        });
    }

    /// The Status panel: health Warnings (each copyable for a bug report) and a
    /// Copy-full-report button. Activity is added in a later pass.
    fn status_panel(&mut self, ctx: &egui::Context) {
        let mut open = self.show_status;
        egui::Window::new(
            RichText::new("STATUS")
                .color(theme::amber())
                .size(16.0)
                .strong(),
        )
        .id(Id::new("status-panel"))
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_width(560.0)
        .show(ctx, |ui| {
            let events = self.diag.events();
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("WARNINGS")
                        .color(theme::tan())
                        .size(13.0)
                        .strong(),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .button(RichText::new("COPY FULL REPORT").color(theme::text()))
                        .on_hover_text(
                            "Copy every warning plus system info and the recent log — \
                                        paste it into a bug report. Nothing is sent anywhere.",
                        )
                        .clicked()
                    {
                        let tail = Self::recent_log_tail();
                        ctx.copy_text(self.diag.full_report(tail.as_deref()));
                    }
                    if !events.is_empty()
                        && ui
                            .button(RichText::new("CLEAR ALL").color(theme::text()))
                            .on_hover_text(
                                "Dismiss every warning. Anything still wrong will \
                                 reappear when it is detected again.",
                            )
                            .clicked()
                    {
                        self.diag.clear_all();
                    }
                });
            });
            ui.add_space(4.0);
            if events.is_empty() {
                ui.label(
                    RichText::new(
                        "No warnings — audio, ffmpeg, pdftoppm and LibreOffice are all available.",
                    )
                    .color(theme::green())
                    .size(12.0),
                );
            }
            for e in &events {
                let color = match e.severity {
                    crate::diagnostics::Severity::Critical => theme::red(),
                    crate::diagnostics::Severity::Warning => theme::amber(),
                };
                egui::Frame::new()
                    .fill(theme::panel())
                    .stroke(egui::Stroke::new(1.0, color))
                    .corner_radius(6)
                    .inner_margin(8)
                    .outer_margin(egui::Margin {
                        top: 0,
                        bottom: 6,
                        left: 0,
                        right: 0,
                    })
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let heading = if e.count > 1 {
                                format!("{} {}  (×{})", e.severity.label(), e.title, e.count)
                            } else {
                                format!("{} {}", e.severity.label(), e.title)
                            };
                            ui.label(RichText::new(heading).color(color).size(13.0).strong());
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui
                                    .button(RichText::new(crate::icon::TRASH).color(theme::text()))
                                    .on_hover_text(
                                        "Dismiss this warning. If it is detected again it \
                                         will come back.",
                                    )
                                    .clicked()
                                {
                                    self.diag.clear(&e.key);
                                }
                                if ui
                                    .button(RichText::new("COPY").color(theme::text()))
                                    .on_hover_text(
                                        "Copy this warning + system info for a bug report",
                                    )
                                    .clicked()
                                {
                                    ctx.copy_text(self.diag.copy_text(&e.title, &e.detail));
                                }
                            });
                        });
                        // When it started, not "right now": a drive that has
                        // been gone since Tuesday should say so.
                        ui.label(
                            RichText::new(format!(
                                "since {}",
                                crate::util::format_mtime(e.since_ms)
                            ))
                            .color(theme::grey())
                            .size(11.0),
                        );
                        ui.label(RichText::new(&e.detail).color(theme::text()).size(12.0));
                    });
            }

            // Activity: the heavy background work you can watch and stop — the
            // answer to "why are the fans blasting?" and the way to end a
            // runaway scan without killing the whole app.
            ui.add_space(12.0);
            ui.label(
                RichText::new("ACTIVITY")
                    .color(theme::tan())
                    .size(13.0)
                    .strong(),
            );
            ui.add_space(4.0);
            let jobs = self.worker.jobs();
            if jobs.is_empty() {
                ui.label(
                    RichText::new("No background work running.")
                        .color(theme::grey())
                        .size(12.0),
                );
            } else {
                let mut cancel: Option<String> = None;
                for (repo, status, kind) in &jobs {
                    let verb = match kind {
                        JobKind::Check => "Checking",
                        _ => "Scanning",
                    };
                    let state = if *status == RepoStatus::Running {
                        "running"
                    } else {
                        "queued"
                    };
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!("{verb} '{repo}' — {state}"))
                                .color(theme::text())
                                .size(12.0),
                        );
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui
                                .button(RichText::new("CANCEL").color(theme::text()))
                                .on_hover_text("Stop this job. Anything already scanned stays.")
                                .clicked()
                            {
                                cancel = Some(repo.clone());
                            }
                        });
                    });
                }
                if let Some(name) = cancel {
                    // Same path as the repository card's CANCEL: signal a running
                    // job to stop cooperatively, or drop a still-queued one.
                    if let Some(token) = self.cancels.get(&name) {
                        token.cancel();
                    } else {
                        self.queue.retain(|(n, _)| n != &name);
                        self.worker.remove(&name);
                    }
                }
            }
        });
        self.show_status = open;
    }

    /// The last lines of the current session log, for the Copy-full-report
    /// payload (bounded so a huge log doesn't swamp the clipboard).
    fn recent_log_tail() -> Option<String> {
        let path = dedup_core::logging::current_log()?;
        let text = std::fs::read_to_string(&path).ok()?;
        let tail: Vec<&str> = text.lines().rev().take(60).collect();
        Some(tail.into_iter().rev().collect::<Vec<_>>().join("\n"))
    }

    fn about_modal(&mut self, ctx: &egui::Context) {
        let response = egui::Modal::new(Id::new("about")).show(ctx, |ui| {
            ui.set_width(320.0);
            ui.label(
                RichText::new("ABOUT")
                    .color(theme::amber())
                    .size(18.0)
                    .strong(),
            );
            ui.add_space(8.0);
            ui.label(
                RichText::new(format!("DEDUP  v{}", env!("CARGO_PKG_VERSION")))
                    .color(theme::text()),
            );
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("License:").color(theme::tan()).size(12.0));
                ui.hyperlink_to(
                    RichText::new("MIT").color(theme::lilac()).size(12.0),
                    "https://opensource.org/license/mit",
                );
            });
            ui.label(
                RichText::new("© 2026 Patrick Zimmer")
                    .color(theme::tan())
                    .size(12.0),
            );
            ui.hyperlink_to(
                RichText::new("dedup@tuta.io")
                    .color(theme::lilac())
                    .size(12.0),
                "mailto:dedup@tuta.io",
            );
            ui.add_space(12.0);
            if ui
                .add(egui::Button::new(
                    RichText::new("CLOSE").color(theme::black()),
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

    /// A real second OS window (not a modal) describing the current tab's
    /// purpose and controls, so it can sit beside the main window instead of
    /// blocking it. Content tracks `self.tab` live, so switching tabs while
    /// it's open updates what's shown.
    fn help_window(&mut self, ctx: &egui::Context) {
        let tab_label = match self.tab {
            Tab::Repositories => "REPOSITORIES",
            Tab::Duplicates => "DUPLICATES",
            Tab::Transfer => "TRANSFER",
            Tab::Grooming => "GROOMING",
            Tab::Browse => "BROWSE",
        };
        let text = crate::help_content::help_text(self.tab);
        // Park it just to the right of the main window when its position is
        // knowable (not on Wayland, where inner/outer rect is always None); a
        // fixed fallback otherwise. This is only honored by the backend when
        // the window is first created, so it never fights the user dragging
        // the help window elsewhere afterwards.
        let pos = ctx
            .input(|i| i.viewport().outer_rect)
            .map(|r| r.right_top())
            .unwrap_or(egui::pos2(120.0, 120.0));

        let close_requested = ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("dedup_help"),
            egui::ViewportBuilder::default()
                .with_title(format!("dedup help — {tab_label}"))
                .with_inner_size([420.0, 600.0])
                .with_position(pos),
            |ui, _class| {
                let mut close_clicked = false;
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(tab_label)
                                .color(theme::amber())
                                .size(18.0)
                                .strong(),
                        );
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui
                                .add(egui::Button::new(
                                    RichText::new("CLOSE").color(theme::black()),
                                ))
                                .clicked()
                            {
                                close_clicked = true;
                            }
                        });
                    });
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.label(RichText::new(text).color(theme::text()));
                        });
                });
                close_clicked || ui.input(|i| i.viewport().close_requested())
            },
        );
        if close_requested {
            self.show_help = false;
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
            ui.label(RichText::new(text).color(theme::black()).size(11.0))
        })
        .inner
}

/// The "main of a sync group" pill next to a repo's name on its card. Matches
/// the star badge the shared repo chip draws on every other tab, so a main is
/// recognisable in one glance wherever it appears.
fn main_pill(ui: &mut egui::Ui, verbosity: TooltipVerbosity) {
    // Built from the shared `pill` helper, like every other pill on this card,
    // rather than a second hand-rolled one.
    let resp = pill(ui, &format!("{} MAIN", icon::STAR), theme::amber());
    // Announced as the bare word, not glyph-plus-word, so it reads the same as
    // the chip badge everywhere else.
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, "MAIN"));
    resp.explain(
        verbosity,
        "The original this group is backed up from",
        "This repository is the main of a sync group: the original that GROUP SYNC pushes \
         out to the backup repositories listed under it.",
    );
}

/// Render a repo's location + freshness as chips next to its name. Anything
/// still `Unknown` (not yet probed/checked) draws nothing.
fn status_pills(ui: &mut egui::Ui, row: &RepoRow, verbosity: TooltipVerbosity) {
    match row.location {
        Some(Location::Local) => {
            pill(ui, "LOCAL", theme::blue()).explain(
                verbosity,
                "On this machine",
                "The repository's folder is on a local disk of this machine.",
            );
        }
        Some(Location::Remote) => {
            pill(ui, "REMOTE", theme::lilac()).explain(
                verbosity,
                "Network mount",
                "The repository's folder is on a reachable network mount (e.g. NFS/SMB).",
            );
        }
        Some(Location::Offline) => {
            pill(ui, "OFFLINE", theme::amber()).explain(
                verbosity,
                "Network mount is not reachable right now",
                "This repository's network mount is not reachable right now — scans and \
                 checks will fail until it's back online.",
            );
        }
        Some(Location::Missing) => {
            pill(ui, "MISSING", theme::red()).explain(
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
            pill(ui, "UP TO DATE", theme::tan()).explain(
                verbosity,
                "No changes since the last scan",
                "The last CHECK found no new, changed, or missing files since the last scan.",
            );
        }
        Freshness::Stale { changed, missing } => {
            pill(ui, "UPDATE REQUIRED", theme::orange()).explain(
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
        egui::Button::new(
            RichText::new(format!("{} CANCEL", icon::X)).color(theme::ink_on(theme::red())),
        )
        .fill(theme::red()),
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
    if crate::lcars::toggle_button(ui, label, selected, color)
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
            .color(theme::text())
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
                .color(theme::text())
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
                        .color(theme::black())
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
    theme::hsl((theme::name_hash(mime) % 360) as f32, 0.50, 0.74)
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

/// Derive a filesystem-safe repository name from a folder path's basename (the
/// name becomes a directory under the config dir). Path separators and control
/// characters are replaced with `_`; an empty result falls back to `repo`.
fn sanitize_repo_name(path: &std::path::Path) -> String {
    let base = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() {
        "repo".to_string()
    } else {
        cleaned
    }
}

/// Return `base` if it isn't already `taken`, else the first `base-2`, `base-3`,
/// … that is free — so a batch of dropped folders with clashing names (or names
/// clashing with existing repos) all get distinct repositories.
fn unique_repo_name(base: &str, taken: &HashSet<String>) -> String {
    if !taken.contains(base) {
        return base.to_string();
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|c| !taken.contains(c))
        .expect("an unbounded range always yields a free name")
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
    use super::{effective_name, sanitize_repo_name, unique_repo_name};
    use std::collections::HashSet;
    use std::path::Path;

    #[test]
    fn sanitize_repo_name_from_folder_basename() {
        assert_eq!(
            sanitize_repo_name(Path::new("/data/Holiday 2019")),
            "Holiday 2019"
        );
        // Hidden folders keep their leading dot (a valid dir name).
        assert_eq!(sanitize_repo_name(Path::new("/home/x/.config")), ".config");
        // A path with no basename falls back.
        assert_eq!(sanitize_repo_name(Path::new("/")), "repo");
    }

    #[test]
    fn unique_repo_name_suffixes_on_clash() {
        let taken: HashSet<String> = ["photos".to_string(), "photos-2".to_string()]
            .into_iter()
            .collect();
        // Free name is used as-is.
        assert_eq!(unique_repo_name("docs", &taken), "docs");
        // Clash skips past every taken suffix.
        assert_eq!(unique_repo_name("photos", &taken), "photos-3");
    }

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
    use dedup_core::store::SyncMode;
    use dedup_core::update::{NoProgress, update_repo};
    use egui_kittest::Harness;

    /// A temp store with two scanned repos, so the Repository Management
    /// A scan that walked empty over a non-empty index must ask before marking
    /// everything missing, and declining must leave the index untouched.
    #[test]
    fn an_emptying_scan_asks_first_and_declining_changes_nothing() {
        let (tmp, mut app) = sample_app();
        let repo = "Automatic Upload";

        // Empty the directory, as an unmounted drive would appear.
        let dir = tmp.path().join(repo.replace(' ', "_"));
        for entry in std::fs::read_dir(&dir).expect("read repo dir") {
            std::fs::remove_file(entry.expect("dir entry").path()).expect("remove file");
        }

        // The core refuses and writes nothing.
        let refused = update_repo(&app.store, repo, 1, &NoProgress, &CancellationToken::new());
        assert!(
            matches!(
                refused,
                Err(dedup_core::update::UpdateError::WouldEmptyIndex { entries: 5, .. })
            ),
            "the scan is refused rather than emptying the index"
        );

        // The UI turns that into a confirmation rather than an error.
        app.empty_scan_confirm = Some((repo.to_string(), 5));
        assert!(
            app.empty_scan_confirm.is_some(),
            "a confirmation is pending"
        );

        // Declining leaves every entry indexed.
        app.empty_scan_confirm = None;
        app.tab = Tab::Repositories;
        app.sync_shown_tab();
        let count = app
            .repos
            .iter()
            .find(|r| r.name == repo)
            .map(|r| r.stats.file_count)
            .expect("repo row");
        assert_eq!(count, 5, "declining keeps all five entries");
    }

    /// cards show real stats instead of all-zero placeholders.
    fn sample_app() -> (tempfile::TempDir, DedupApp) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        // With real doc media (DEDUP_DOC_MEDIA), the two repos hold a believable
        // mix of photos, video and documents so the cards show genuine
        // thumbnails and a real MIME breakdown; otherwise fall back to the
        // synthetic blobs the ordinary test suite uses.
        let layouts: [(&str, &[&str]); 2] = [
            (
                "Automatic Upload",
                &[
                    "IMG_2019_field.jpg",
                    "wallpaper_spacehulk.jpg",
                    "bebop_blue.jpg",
                    "mewtwo.png",
                    "kitten.mp4",
                    "menu.pdf",
                    "visa_contract.pdf",
                ],
            ),
            ("Videos", &["lynx.webm", "machine.mp4", "bebop_sepia.jpg"]),
        ];
        for (name, assets) in layouts {
            let dir = tmp.path().join(name.replace(' ', "_"));
            std::fs::create_dir_all(&dir).unwrap();
            let mut placed = 0usize;
            if crate::doc_media::available() {
                for asset in assets {
                    if crate::doc_media::place(asset, &dir.join(asset)) {
                        placed += 1;
                    }
                }
            }
            if placed == 0 {
                let files = if name == "Automatic Upload" { 5 } else { 2 };
                for i in 0..files {
                    std::fs::write(dir.join(format!("f{i}.bin")), format!("sample data {i}"))
                        .unwrap();
                }
            }
            store.create_repo(name, &dir.to_string_lossy()).unwrap();
            update_repo(&store, name, 1, &NoProgress, &CancellationToken::new()).unwrap();
        }
        (tmp, DedupApp::new(store))
    }

    /// Returning to the Repositories tab must re-read the registry, so counts
    /// reflect deletions made on another tab. Before this, the numbers stayed
    /// stale until the user refreshed by hand.
    #[test]
    fn switching_to_the_repositories_tab_refreshes_its_stats() {
        let (_tmp, mut app) = sample_app();
        app.tab = Tab::Repositories;
        app.sync_shown_tab();
        let before = app
            .repos
            .iter()
            .find(|r| r.name == "Automatic Upload")
            .map(|r| r.stats.file_count)
            .expect("repo row");
        assert_eq!(before, 5, "sample repo starts with five files");

        // Change the store behind the app's back, as a delete on another tab would.
        app.store
            .remove_file_entry("Automatic Upload", "f0.bin")
            .expect("remove entry");

        // Leaving and returning is what triggers the re-read.
        app.tab = Tab::Duplicates;
        app.sync_shown_tab();
        app.tab = Tab::Repositories;
        app.sync_shown_tab();

        let after = app
            .repos
            .iter()
            .find(|r| r.name == "Automatic Upload")
            .map(|r| r.stats.file_count)
            .expect("repo row");
        assert_eq!(
            after, 4,
            "returning to the tab picks up the change without a manual refresh"
        );
    }

    /// The re-read happens on the transition only, not every frame — otherwise a
    /// visible tab would reopen every repo db continuously.
    #[test]
    fn staying_on_the_repositories_tab_does_not_re_read_each_frame() {
        let (_tmp, mut app) = sample_app();
        app.tab = Tab::Repositories;
        app.sync_shown_tab();
        assert!(app.synced_tab == Some(Tab::Repositories));

        // Change the store, then run more frames *without* leaving the tab.
        app.store
            .create_repo("Later", &_tmp.path().join("Later").to_string_lossy())
            .ok();
        for _ in 0..3 {
            app.sync_shown_tab();
        }
        assert!(
            !app.repos.iter().any(|r| r.name == "Later"),
            "no re-read while the tab stays shown; only a switch refreshes"
        );
    }

    /// A sync group is framed by one LCARS section titled with the group name,
    /// holding its main and its sinks; the section's own caret folds the whole
    /// group away. Ungrouped repos stay bare cards outside any section.
    #[test]
    fn a_group_is_framed_by_one_section_that_folds_it_away() {
        use egui_kittest::kittest::Queryable;
        let (_tmp, mut app) = sample_app();
        app.store
            .create_sync_group("offsite", "Automatic Upload")
            .expect("create group");
        app.store
            .add_sync_sink("offsite", "Videos", SyncMode::AddOnly)
            .expect("add sink");
        app.reload_all();

        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1000.0, 900.0))
            .build_ui_state(
                move |ui, app: &mut DedupApp| {
                    if !init {
                        icon::install(ui.ctx());
                        theme::apply(ui.ctx(), theme::DARK);
                        init = true;
                    }
                    let mut actions = Vec::new();
                    app.repositories_view(ui, &mut actions);
                },
                app,
            );
        harness.run();
        // Folded by default: the group's title bar is all that shows, so the
        // repo list stays about your originals.
        assert!(
            harness.query_by_label_contains("offsite").is_some(),
            "the group's section is titled with the group name"
        );
        assert!(
            harness.query_all_by_label_contains("Videos").count() == 0,
            "a folded group hides its sinks"
        );
        // The section's caret is the one collapse control: no second chevron.
        assert!(
            harness.query_by_label_contains("SINK(S) IN").is_none(),
            "the old sink-count chevron is gone — one collapse affordance only"
        );

        // Expanding it brings the whole group — main and sinks — into view.
        harness.get_by_label_contains("offsite").click();
        harness.run();
        harness.run();
        assert!(
            harness
                .query_by_label_contains("Automatic Upload")
                .is_some(),
            "expanding shows the main's card"
        );
        assert!(
            harness.query_all_by_label_contains("Videos").count() > 0,
            "expanding shows the sink's card"
        );
        assert!(
            harness.query_by_label("MAIN").is_some(),
            "the main is badged so it is distinguishable from its sinks"
        );
    }

    /// A repo that belongs to no group is a bare card: no section rail, no badge.
    #[test]
    fn an_ungrouped_repo_has_no_section_and_no_badge() {
        use egui_kittest::kittest::Queryable;
        let (_tmp, app) = sample_app();
        let harness = render_repos(app);
        assert!(
            harness
                .query_by_label_contains("Automatic Upload")
                .is_some(),
            "the repo is listed"
        );
        assert!(
            harness.query_by_label("MAIN").is_none(),
            "an ungrouped repo carries no MAIN badge"
        );
    }

    /// Render the Repositories tab and run a frame. The harness collects (and
    /// discards) the deferred `Action`s, so this asserts on what is *shown* for a
    /// given store state, which is the group-management UI's real surface.
    fn render_repos(app: DedupApp) -> Harness<'static, DedupApp> {
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1200.0, 1000.0))
            .build_ui_state(
                move |ui, app: &mut DedupApp| {
                    if !init {
                        icon::install(ui.ctx());
                        theme::apply(ui.ctx(), theme::DARK);
                        init = true;
                    }
                    let mut actions = Vec::new();
                    app.repositories_view(ui, &mut actions);
                },
                app,
            );
        harness.run();
        harness
    }

    /// An ungrouped repo offers MAKE MAIN; with no groups yet, no group controls
    /// (UNGROUP / MODE pill) are shown anywhere.
    #[test]
    fn ungrouped_repos_offer_make_main_and_no_group_controls() {
        use egui_kittest::kittest::Queryable;
        let (_tmp, app) = sample_app();
        let harness = render_repos(app);
        assert!(
            harness.query_all_by_label_contains("MAKE MAIN").count() >= 1,
            "an ungrouped repo can be made a group main"
        );
        assert!(
            harness.query_by_label_contains("UNGROUP").is_none(),
            "no group controls without a group"
        );
    }

    /// A group's main shows the group controls (UNGROUP) and never MAKE MAIN;
    /// its sink carries its own mode pill and SINK OUT (once expanded).
    #[test]
    fn group_main_shows_controls_and_sink_shows_mode_and_sink_out() {
        use egui_kittest::kittest::Queryable;
        let (_tmp, mut app) = sample_app();
        app.store
            .create_sync_group("Automatic Upload", "Automatic Upload")
            .expect("create group");
        app.store
            .add_sync_sink("Automatic Upload", "Videos", SyncMode::AddOnly)
            .expect("add sink");
        app.reload_all();
        let mut harness = render_repos(app);
        // Groups are folded by default; open this one to reach its contents.
        harness.get_by_label_contains("Automatic Upload").click();
        harness.run();
        harness.run();

        assert!(
            harness.query_by_label_contains("UNGROUP").is_some(),
            "the main shows the UNGROUP control"
        );
        // Both repos are grouped, so nothing offers MAKE MAIN.
        assert!(
            harness.query_by_label_contains("MAKE MAIN").is_none(),
            "a grouped repo is not offered as a new main"
        );

        // The group section starts open, so the sink's card is already drawn.
        assert!(
            harness.query_by_label_contains("SINK OUT").is_some(),
            "the expanded sink offers SINK OUT"
        );
        assert!(
            harness.query_by_label_contains("MODE: ADD ONLY").is_some(),
            "the sink carries its own mode pill"
        );
    }

    /// A sink's mode pill reflects its own stored mode.
    #[test]
    fn a_mirror_sink_shows_a_mirror_pill() {
        use egui_kittest::kittest::Queryable;
        let (_tmp, mut app) = sample_app();
        app.store
            .create_sync_group("Automatic Upload", "Automatic Upload")
            .expect("create group");
        app.store
            .add_sync_sink("Automatic Upload", "Videos", SyncMode::Mirror)
            .expect("add sink");
        app.reload_all();
        let mut harness = render_repos(app);
        // Groups are folded by default; open this one to reach its contents.
        harness.get_by_label_contains("Automatic Upload").click();
        harness.run();
        harness.run();
        assert!(
            harness.query_by_label_contains("MODE: MIRROR").is_some(),
            "a MIRROR sink's pill reads MIRROR"
        );
    }

    /// The DUPLICATE editor offers a folder picker (CHOOSE…), like RELOCATE and
    /// ADD — so the new repo's path can be browsed, not just typed.
    #[test]
    fn duplicate_editor_has_a_browse_button() {
        use egui_kittest::kittest::Queryable;
        let (_tmp, mut app) = sample_app();
        app.edit = Some(Edit::Duplicate {
            name: "Automatic Upload".into(),
            dest: String::new(),
            path: String::new(),
        });
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1000.0, 400.0))
            .build_ui_state(
                move |ui, app: &mut DedupApp| {
                    if !init {
                        icon::install(ui.ctx());
                        theme::apply(ui.ctx(), theme::DARK);
                        init = true;
                    }
                    let mut actions = Vec::new();
                    app.repositories_view(ui, &mut actions);
                },
                app,
            );
        harness.run();
        assert!(
            harness.query_by_label_contains("CHOOSE").is_some(),
            "the DUPLICATE editor should offer a folder-picker button"
        );
    }

    /// The HELP window shows the current tab's help copy, and its in-window
    /// CLOSE button clears `show_help`. This headless harness has no native
    /// eframe integration, so `Context::embed_viewports` stays at its default
    /// `true` and `show_viewport_immediate` renders the content as a regular
    /// embedded `Window` instead of a real second OS window — this exercises
    /// the content and the CLOSE path, but not real cross-window placement or
    /// the native OS close button (only the running app can show those).
    #[test]
    fn help_window_shows_tab_copy_and_close_clears_flag() {
        use egui_kittest::kittest::Queryable;
        let (_tmp, mut app) = sample_app();
        app.show_help = true;
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1000.0, 700.0))
            .build_ui_state(
                move |ui, app: &mut DedupApp| {
                    if !init {
                        icon::install(ui.ctx());
                        theme::apply(ui.ctx(), theme::DARK);
                        init = true;
                    }
                    let ctx = ui.ctx().clone();
                    if app.show_help {
                        app.help_window(&ctx);
                    }
                },
                app,
            );
        harness.run();
        assert!(
            harness
                .query_by_label_contains("Register the folders you want to triage")
                .is_some(),
            "the HELP window shows the current (REPOSITORIES) tab's help copy"
        );
        harness.get_by_label("CLOSE").click();
        harness.run();
        assert!(!harness.state().show_help, "CLOSE clears show_help");
    }

    /// A temp store with one scanned repo whose on-disk path is very long, to
    /// exercise the narrow-window path/MIME overlap.
    fn app_with_long_path() -> (tempfile::TempDir, DedupApp, String) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let dir = tmp
            .path()
            .join("a/very/deeply/nested/photos/library/originals/2024/imports/raw");
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..4 {
            std::fs::write(dir.join(format!("f{i}.bin")), format!("data {i}")).unwrap();
        }
        store.create_repo("Photos", &dir.to_string_lossy()).unwrap();
        update_repo(&store, "Photos", 1, &NoProgress, &CancellationToken::new()).unwrap();
        let path = dir.to_string_lossy().into_owned();
        (tmp, DedupApp::new(store), path)
    }

    fn topbar_harness<'a>(app: DedupApp, width: f32) -> Harness<'a, DedupApp> {
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(width, 200.0))
            .build_ui_state(
                move |ui, app: &mut DedupApp| {
                    if !init {
                        icon::install(ui.ctx());
                        theme::apply(ui.ctx(), theme::DARK);
                        init = true;
                    }
                    app.top_bar(ui);
                },
                app,
            );
        harness.run();
        harness
    }

    /// At the minimum window width the five tabs no longer fit beside
    /// SETTINGS/ABOUT. Those two buttons must stay fully on-screen and pinned to
    /// the right (the tab strip scrolls, clipped, instead of a tab sliding behind
    /// ABOUT or SETTINGS spilling off the edge — the reported regression).
    #[test]
    fn top_bar_pins_settings_about_when_narrow() {
        use egui_kittest::kittest::Queryable;
        let width = 760.0;
        let (_tmp, app) = sample_app();
        let harness = topbar_harness(app, width);
        let settings = harness.get_by_label_contains("SETTINGS").rect();
        let about = harness.get_by_label("ABOUT").rect();
        let repos = harness.get_by_label("REPOSITORIES").rect();
        assert!(
            settings.right() <= width + 0.5,
            "SETTINGS spills off the right edge (right={} > {width})",
            settings.right()
        );
        assert!(
            about.right() <= settings.left() + 0.5,
            "ABOUT ({about:?}) overlaps SETTINGS ({settings:?})"
        );
        // The tab strip lives entirely to the left of ABOUT (it scrolls/clips
        // there), so no tab is drawn behind ABOUT.
        assert!(
            repos.left() < about.left(),
            "tab strip starts at/after ABOUT — it is not clipped to the left of it"
        );
    }

    /// Wide enough for every tab: all five are laid out (none clipped) and
    /// SETTINGS/ABOUT still sit to their right.
    #[test]
    fn top_bar_shows_all_tabs_when_wide() {
        use egui_kittest::kittest::Queryable;
        let (_tmp, app) = sample_app();
        let harness = topbar_harness(app, 1100.0);
        for tab in [
            "REPOSITORIES",
            "DUPLICATES",
            "TRANSFER",
            "GROOMING",
            "BROWSE",
        ] {
            assert!(
                harness.query_by_label(tab).is_some(),
                "tab {tab} missing at wide width"
            );
        }
        let browse = harness.get_by_label("BROWSE").rect();
        let about = harness.get_by_label("ABOUT").rect();
        assert!(
            browse.right() <= about.left() + 0.5,
            "BROWSE ({browse:?}) overlaps ABOUT ({about:?}) even when wide"
        );
    }

    /// A long repo path must truncate to the gap between the status pills and the
    /// MIME tags instead of running under the (right-pinned) MIME tags.
    #[test]
    fn repo_card_path_does_not_overlap_mime_tags() {
        use egui_kittest::kittest::Queryable;
        let (_tmp, app, _path) = app_with_long_path();
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(760.0, 300.0))
            .build_ui_state(
                move |ui, app: &mut DedupApp| {
                    if !init {
                        icon::install(ui.ctx());
                        theme::apply(ui.ctx(), theme::DARK);
                        init = true;
                    }
                    let mut actions = Vec::new();
                    app.repositories_view(ui, &mut actions);
                },
                app,
            );
        harness.run();
        // The lone repo is all one MIME type, so exactly one MIME pill renders.
        let mime = harness.get_by_label_contains("100%").rect();
        assert!(
            mime.right() <= 760.0 + 0.5,
            "MIME tag spills off the right edge (right={})",
            mime.right()
        );
        // The path label (truncated) must end at or before the MIME tag begins.
        let path = harness.get_by_label_contains("nested/photos").rect();
        assert!(
            path.right() <= mime.left() + 1.0,
            "path ({path:?}) runs under the MIME tag ({mime:?})"
        );
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
                        theme::apply(ui.ctx(), theme::DARK);
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

    /// Doc screenshot: a sync group framed by its LCARS elbow section — the
    /// badged main and its sink inside one rail, an ungrouped repo as a bare
    /// card outside it. Rendered rather than label-queried, because a label
    /// query passes even when the rail overlaps the cards it is meant to frame.
    #[test]
    #[ignore = "generates a doc screenshot (needs wgpu)"]
    fn doc_screenshot_repo_group_section() {
        let (tmp, mut app) = sample_app();
        app.store
            .create_sync_group("offsite", "Automatic Upload")
            .expect("create group");
        app.store
            .add_sync_sink("offsite", "Videos", SyncMode::Mirror)
            .expect("add sink");
        // A third, ungrouped repo: the point of the shot is the contrast between
        // a framed group and a bare card.
        let scratch = tmp.path().join("Scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::write(scratch.join("f0.bin"), "scratch").unwrap();
        app.store
            .create_repo("Scratch", &scratch.to_string_lossy())
            .unwrap();
        update_repo(
            &app.store,
            "Scratch",
            1,
            &NoProgress,
            &CancellationToken::new(),
        )
        .unwrap();
        app.reload_all();
        let _tmp = tmp;
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1120.0, 760.0))
            .wgpu()
            .build_ui_state(
                move |ui, app: &mut DedupApp| {
                    if !init {
                        icon::install(ui.ctx());
                        theme::apply(ui.ctx(), theme::DARK);
                        init = true;
                    }
                    let mut actions = Vec::new();
                    app.repositories_view(ui, &mut actions);
                },
                app,
            );
        harness.run();
        // Groups fold by default; the point of the shot is what a group holds,
        // so open it.
        {
            use egui_kittest::kittest::Queryable;
            harness.get_by_label_contains("offsite").click();
        }
        harness.run();
        harness.run();
        let img = harness.render().expect("wgpu render failed");
        let out = doc_screenshot_path("repo_group_section.png");
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());
    }

    /// The appearance control offers exactly System / Light / Dark, and picking
    /// one repaints in the same frame — the observable behaviour, not merely
    /// that a field was written.
    #[test]
    fn appearance_control_offers_three_options_and_applies_at_once() {
        use crate::settings::ThemeChoice;
        use egui_kittest::kittest::Queryable;
        let (_tmp, mut app) = sample_app();
        app.show_settings = true;
        assert_eq!(app.theme, ThemeChoice::Dark, "default is Dark");
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(460.0, 460.0))
            .build_ui_state(
                move |ui, app: &mut DedupApp| {
                    if !init {
                        icon::install(ui.ctx());
                        theme::register_themes(ui.ctx());
                        theme::apply(ui.ctx(), theme::DARK);
                        init = true;
                    }
                    app.settings_modal(&ui.ctx().clone());
                },
                app,
            );
        harness.run();
        for label in ["SYSTEM", "LIGHT", "DARK"] {
            assert!(
                harness.query_by_label(label).is_some(),
                "the appearance control offers {label}"
            );
        }

        harness.get_by_label("LIGHT").click();
        harness.run();
        assert_eq!(
            harness.state().theme,
            ThemeChoice::Light,
            "the choice is recorded"
        );
        assert_eq!(
            theme::text(),
            theme::LIGHT.text,
            "and the light palette is live in the same interaction — no restart"
        );
        theme::install(theme::DARK);
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
            .with_size(egui::vec2(420.0, 420.0))
            .wgpu()
            .build_ui_state(
                move |ui, app: &mut DedupApp| {
                    if !init {
                        icon::install(ui.ctx());
                        theme::apply(ui.ctx(), theme::DARK);
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

    /// Doc screenshot: the Status centre — health warnings (each copyable) and
    /// running background work with Cancel — to `docs/screenshots/status_panel.png`.
    /// `--ignored` (needs wgpu).
    #[test]
    #[ignore = "generates a doc screenshot (needs wgpu)"]
    fn doc_screenshot_status_panel() {
        use crate::diagnostics::Severity::{Critical, Warning};
        let (_tmp, mut app) = sample_app();
        app.diag.set_fingerprint(
            "dedup: 0.1.0\nos: linux x86_64\naudio output: none (playback disabled)\n\
             ffmpeg: not found\npdftoppm: present",
        );
        app.diag.push(
            Critical,
            "repo-unreachable:Old Laptop",
            "Repository 'Old Laptop' is unreachable",
            "Its folder no longer exists or can't be read — a disconnected drive or an unmounted \
             cloud folder looks exactly like this. Nothing has been deleted.",
        );
        app.diag.push(
            Warning,
            "audio-device",
            "No audio output device",
            "Playback is disabled. On Linux this usually means ALSA/PipeWire isn't running.",
        );
        app.diag.push(
            Warning,
            "ffmpeg",
            "ffmpeg not found on PATH",
            "Video frames, soundtrack extraction and playback rates need ffmpeg / ffprobe.",
        );
        // A running scan so the Activity section shows something to cancel.
        app.worker.mark_queued("Automatic Upload", JobKind::Update);
        app.worker.mark_running("Automatic Upload");
        app.show_status = true;

        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(620.0, 560.0))
            .wgpu()
            .build_ui_state(
                move |ui, app: &mut DedupApp| {
                    if !init {
                        icon::install(ui.ctx());
                        theme::apply(ui.ctx(), theme::DARK);
                        init = true;
                    }
                    // Paint the app background so the floating panel reads.
                    let screen = ui.ctx().content_rect();
                    ui.painter().rect_filled(screen, 0.0, theme::bg());
                    app.status_panel(&ui.ctx().clone());
                },
                app,
            );
        harness.run();
        let img = harness.render().expect("wgpu render failed");
        let out = doc_screenshot_path("status_panel.png");
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());
    }
}
