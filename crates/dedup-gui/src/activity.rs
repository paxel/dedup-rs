//! The one owner of everything the user is told about what the app is doing
//! (ADR 0003): long-running operations run alone behind the **activity
//! modal**, quick **row actions** report through **notification cards**, and
//! every change the app makes to the filesystem lands in the **event log**.
//!
//! A view hands its long work to [`Activity::start`] with a title and a
//! closure; the closure runs on a worker, reports phases through
//! [`ActivityProgress`], and returns the [`RunReport`] the finished modal
//! shows. The view still receives its own result over its own channel — this
//! module never sees result data, only progress and outcome. While an
//! operation runs, [`Activity::start`] refuses a second one, and a view must
//! not begin a row action; while a row action is in flight, no operation may
//! start. The refusal always names what is still running.

use crate::run_result::{ResultModal, RunReport};
use crate::theme;
use crate::util::ExplainExt;
use crossbeam_channel::{Receiver, Sender};
use dedup_core::diff::{DiffAction, DiffEvent, DiffProgress, PlanPhase, PlanProgress};
use dedup_core::update::{CancellationToken, Progress, ProgressEvent};
use egui::{Id, RichText};
use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// The one [`Activity`] of the app, shared with every view the way the lock
/// registry is: the root draws it, the views start work and post cards
/// through it.
pub type Shared = Arc<Mutex<Activity>>;

/// A new shared owner whose event log lives under `config_dir`.
/// An owner for a view built without the app root (tests, previews): its
/// event log goes to a fresh scratch directory under the system temp dir, so
/// no two such views — or test processes — share a log.
pub fn scratch() -> Shared {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    shared(
        &std::env::temp_dir()
            .join(format!("dedup-rs-{}", std::process::id()))
            .join(n.to_string()),
    )
}

pub fn shared(config_dir: &Path) -> Shared {
    Arc::new(Mutex::new(Activity::new(config_dir)))
}

/// Borrow the shared owner. A poisoned lock (a panic while it was held) is
/// recovered rather than propagated: the owner's state is plain data and the
/// app must keep drawing.
pub fn lock(shared: &Shared) -> MutexGuard<'_, Activity> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

/// How long a notification card stays before it slides out on its own.
const CARD_LIFETIME: Duration = Duration::from_secs(8);
/// How long a new card takes to slide in from the right edge.
const CARD_SLIDE: Duration = Duration::from_millis(250);
/// How many cards stack before the oldest is pushed out early.
const MAX_CARDS: usize = 4;
/// The event log's file name under the configuration directory.
pub const EVENT_LOG_FILE: &str = "events.jsonl";

/// What a long-running operation is, for the modal's header.
pub struct Spec {
    /// What runs, e.g. `"FIND similar files"`.
    pub title: String,
    /// The repositories it works on, listed under the title.
    pub repos: Vec<String>,
}

/// Why an operation or a row action could not start: what is still running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Busy(pub String);

impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Not now — {} is still running.", self.0)
    }
}

/// One row action's outcome, for a card and (when it changed the filesystem)
/// an event-log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    /// The verb, e.g. `"Deleted"`, `"Copied"`, `"Accepted"`.
    pub action: String,
    pub repo: String,
    /// The file, or a short description for an action on several.
    pub path: String,
    /// `Ok` or the error message.
    pub outcome: Result<(), String>,
    /// Whether the action changed something on disk — the event log records
    /// exactly those; marks and other in-memory choices get a card only.
    pub changed_disk: bool,
}

impl Notification {
    /// A successful change to the filesystem.
    pub fn changed(action: &str, repo: &str, path: &str) -> Self {
        Self {
            action: action.to_string(),
            repo: repo.to_string(),
            path: path.to_string(),
            outcome: Ok(()),
            changed_disk: true,
        }
    }

    /// A change to the filesystem that failed.
    pub fn failed(action: &str, repo: &str, path: &str, error: &str) -> Self {
        Self {
            action: action.to_string(),
            repo: repo.to_string(),
            path: path.to_string(),
            outcome: Err(error.to_string()),
            changed_disk: true,
        }
    }

    /// A choice that changed nothing on disk (a mark, an acceptance).
    pub fn noted(action: &str, repo: &str, path: &str) -> Self {
        Self {
            action: action.to_string(),
            repo: repo.to_string(),
            path: path.to_string(),
            outcome: Ok(()),
            changed_disk: false,
        }
    }

    /// An action that did not start because something else is still running.
    pub fn refused(action: &str, busy: &Busy) -> Self {
        Self {
            action: action.to_string(),
            repo: String::new(),
            path: "did not start".to_string(),
            outcome: Err(format!("{} is still running", busy.0)),
            changed_disk: false,
        }
    }

    /// The card's first line: the verb and the file.
    pub fn headline(&self) -> String {
        match &self.outcome {
            Ok(()) => format!("{} {}", self.action, self.path),
            Err(_) => format!("FAILED: {} {}", self.action.to_lowercase(), self.path),
        }
    }
}

/// What a running operation tells the modal.
enum Msg {
    Phase {
        name: String,
        done: u64,
        total: Option<u64>,
    },
    Problem(String),
    /// The operation ended; `None` closes the modal without a report (a
    /// preview whose result is the board it fills).
    Finished(Option<RunReport>),
}

/// The progress side of a running operation: what the worker closure reports
/// through. Also a core [`Progress`], so a repository scan's events feed the
/// same modal.
#[derive(Clone)]
pub struct ActivityProgress {
    tx: Sender<Msg>,
    ctx: egui::Context,
    log: EventLog,
}

impl ActivityProgress {
    /// Append one filesystem change to the event log from the worker — for
    /// an operation that touches many files and summarises them in its
    /// report. Nothing is drawn; the card, if any, is the caller's.
    pub fn record(&self, note: &Notification) {
        self.log.append(&LogEntry::from_note(note));
    }

    /// Ask the window to redraw — for a worker that has just sent its view a
    /// message over the view's own channel.
    pub fn repaint(&self) {
        self.ctx.request_repaint();
    }

    /// The current phase: a short present-tense line, and how far along it is.
    /// `total: None` is an indeterminate phase (a spinner, no percentage).
    pub fn phase(&self, name: impl Into<String>, done: u64, total: Option<u64>) {
        let _ = self.tx.send(Msg::Phase {
            name: name.into(),
            done,
            total,
        });
        self.ctx.request_repaint();
    }

    /// A failure on one item, listed live in the modal and again in the report.
    pub fn problem(&self, text: impl Into<String>) {
        let _ = self.tx.send(Msg::Problem(text.into()));
        self.ctx.request_repaint();
    }
}

impl Progress for ActivityProgress {
    fn on(&self, event: ProgressEvent) {
        match event {
            ProgressEvent::Scanning { files, dirs } => {
                self.phase(
                    format!("walking — {files} files in {dirs} folders"),
                    0,
                    None,
                );
            }
            ProgressEvent::Hashing {
                done,
                total,
                current,
                ..
            } => self.phase(format!("hashing {current}"), done, Some(total)),
            ProgressEvent::Error { path, message } => self.problem(format!("{path}: {message}")),
            ProgressEvent::Finished { .. } => {}
        }
    }
}

/// [`DiffProgress`] adapter for a run behind the activity modal or a row
/// action: each file becomes the modal's phase line and an event-log line,
/// each failure a live problem and a failed log line. `repo` is where the
/// change lands (the target, the sink, the main, or an export folder).
pub struct RunProgress {
    pub activity: ActivityProgress,
    pub repo: String,
}

impl DiffProgress for RunProgress {
    fn on(&self, event: DiffEvent) {
        match event {
            DiffEvent::Progress {
                action,
                done,
                total,
                rel_path,
            } => {
                let (doing, did) = match action {
                    DiffAction::Copy => ("copying", "Copied"),
                    DiffAction::Move => ("moving", "Moved"),
                    DiffAction::Delete => ("deleting", "Deleted"),
                };
                self.activity
                    .phase(format!("{doing} {rel_path}"), done, Some(total));
                self.activity
                    .record(&Notification::changed(did, &self.repo, &rel_path));
            }
            DiffEvent::Error { path, message } => {
                log::warn!("transfer error: {path}: {message}");
                self.activity.problem(format!("{path}: {message}"));
                self.activity.record(&Notification::failed(
                    "Transfer", &self.repo, &path, &message,
                ));
            }
        }
    }
}

/// Map a plan's phase onto the activity modal.
pub fn plan_phase(progress: &ActivityProgress, p: PlanProgress) {
    match p.phase {
        PlanPhase::Reading { repo } => progress.phase(format!("reading '{repo}'"), 0, None),
        PlanPhase::Pairing => progress.phase("pairing files", p.done, p.total),
        PlanPhase::Grouping => progress.phase("grouping duplicates", p.done, p.total),
    }
}

struct Running {
    title: String,
    repos: Vec<String>,
    started: Instant,
    phase: String,
    done: u64,
    total: Option<u64>,
    problems: Vec<String>,
    cancel: CancellationToken,
}

impl Running {
    fn fraction(&self) -> Option<f32> {
        match self.total {
            Some(total) if total > 0 => Some((self.done as f64 / total as f64) as f32),
            _ => None,
        }
    }

    fn eta(&self) -> Option<Duration> {
        let total = self.total?;
        if self.done == 0 || total <= self.done {
            return None;
        }
        let per_item = self.started.elapsed().as_secs_f64() / self.done as f64;
        Some(Duration::from_secs_f64(
            per_item * (total - self.done) as f64,
        ))
    }
}

struct Card {
    note: Notification,
    born: Instant,
}

/// One event-log line: a change the app made to the filesystem.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LogEntry {
    /// When, as RFC 3339 in UTC.
    pub at: String,
    pub action: String,
    pub repo: String,
    pub path: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl LogEntry {
    fn from_note(note: &Notification) -> Self {
        Self {
            at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            action: note.action.clone(),
            repo: note.repo.clone(),
            path: note.path.clone(),
            ok: note.outcome.is_ok(),
            error: note.outcome.clone().err(),
        }
    }

    fn matches(&self, filter: &str) -> bool {
        let f = filter.to_lowercase();
        f.is_empty()
            || self.at.to_lowercase().contains(&f)
            || self.action.to_lowercase().contains(&f)
            || self.repo.to_lowercase().contains(&f)
            || self.path.to_lowercase().contains(&f)
            || self
                .error
                .as_deref()
                .is_some_and(|e| e.to_lowercase().contains(&f))
    }
}

/// The append-only event log on disk. One JSON object per line; the app
/// never rewrites or truncates it.
#[derive(Clone)]
pub struct EventLog {
    path: PathBuf,
}

impl EventLog {
    pub fn at(config_dir: &Path) -> Self {
        Self {
            path: config_dir.join(EVENT_LOG_FILE),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn append(&self, entry: &LogEntry) {
        let Ok(line) = serde_json::to_string(entry) else {
            return;
        };
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let written = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .and_then(|mut f| writeln!(f, "{line}"));
        if let Err(e) = written {
            log::warn!("event log {}: {e}", self.path.display());
        }
    }

    /// Every entry, newest first. A line that does not parse is skipped.
    pub fn read(&self) -> Vec<LogEntry> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        let mut entries: Vec<LogEntry> = text
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        entries.reverse();
        entries
    }
}

/// The owner of the activity modal, the notification stack and the event log.
/// Lives in the application root; every view gets `&mut` access for the frame.
pub struct Activity {
    running: Option<Running>,
    report: ResultModal,
    cards: VecDeque<Card>,
    unread: usize,
    log: EventLog,
    show_log: bool,
    log_filter: String,
    log_entries: Vec<LogEntry>,
    /// Row actions in flight (a view's worker for one quick change).
    row_actions: usize,
    /// Something outside this owner is busy — the repository scan worker,
    /// until it too runs behind the modal. Set by the app each frame.
    external: Option<String>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
}

impl Activity {
    pub fn new(config_dir: &Path) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        Self {
            running: None,
            report: ResultModal::default(),
            cards: VecDeque::new(),
            unread: 0,
            log: EventLog::at(config_dir),
            show_log: false,
            log_filter: String::new(),
            log_entries: Vec::new(),
            row_actions: 0,
            external: None,
            tx,
            rx,
        }
    }

    /// What is running, if anything — a long-running operation, a row
    /// action, or work outside this owner.
    pub fn busy(&self) -> Option<Busy> {
        if let Some(r) = &self.running {
            return Some(Busy(r.title.clone()));
        }
        if self.row_actions > 0 {
            return Some(Busy("a file action".to_string()));
        }
        self.external.clone().map(Busy)
    }

    /// Whether a long-running operation is on the modal right now.
    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// Whether work outside this owner (the scan worker) must hold off: an
    /// operation is on the modal or a row action is in flight.
    pub fn blocks_external(&self) -> bool {
        self.running.is_some() || self.row_actions > 0
    }

    /// Tell the owner about work it does not run itself (the scan worker), so
    /// "one at a time" holds across both. `None` when that work is idle.
    pub fn set_external(&mut self, what: Option<String>) {
        self.external = what;
    }

    /// Start a long-running operation. `work` runs on a worker thread,
    /// reports through the progress handle, watches the token, and returns
    /// the report the finished modal shows. Refused, naming what runs, while
    /// anything else is in flight.
    pub fn start<F>(&mut self, ctx: &egui::Context, spec: Spec, work: F) -> Result<(), Busy>
    where
        F: FnOnce(&ActivityProgress, &CancellationToken) -> RunReport + Send + 'static,
    {
        self.launch(ctx, spec, move |progress, cancel| {
            Some(work(progress, cancel))
        })
    }

    /// Start a long-running operation whose result is what it fills in
    /// (a preview's board), not a report: the modal shows its phases and
    /// closes by itself when `work` returns `Ok`. An `Err` becomes a
    /// one-problem report, unless the operation was cancelled.
    pub fn start_quiet<F>(&mut self, ctx: &egui::Context, spec: Spec, work: F) -> Result<(), Busy>
    where
        F: FnOnce(&ActivityProgress, &CancellationToken) -> Result<(), String> + Send + 'static,
    {
        let title = spec.title.clone();
        self.launch(ctx, spec, move |progress, cancel| {
            match work(progress, cancel) {
                Ok(()) => None,
                Err(_) if cancel.is_cancelled() => None,
                Err(e) => {
                    let mut report = RunReport::new(title);
                    report.problem(e);
                    Some(report)
                }
            }
        })
    }

    fn launch<F>(&mut self, ctx: &egui::Context, spec: Spec, work: F) -> Result<(), Busy>
    where
        F: FnOnce(&ActivityProgress, &CancellationToken) -> Option<RunReport> + Send + 'static,
    {
        // A finished operation may not have been drawn yet (a view starting
        // work before the root's next frame); fold it in before judging.
        self.drain();
        if let Some(busy) = self.busy() {
            return Err(busy);
        }
        self.report.close();
        let cancel = CancellationToken::new();
        self.running = Some(Running {
            title: spec.title,
            repos: spec.repos,
            started: Instant::now(),
            phase: "starting".to_string(),
            done: 0,
            total: None,
            problems: Vec::new(),
            cancel: cancel.clone(),
        });
        let progress = self.progress_handle(ctx);
        std::thread::spawn(move || {
            let report = work(&progress, &cancel);
            let _ = progress.tx.send(Msg::Finished(report));
            progress.ctx.request_repaint();
        });
        Ok(())
    }

    /// A view is about to run a row action on a worker. Refused while a
    /// long-running operation is up; otherwise counted until
    /// [`Self::end_row_action`].
    pub fn begin_row_action(&mut self) -> Result<(), Busy> {
        self.drain();
        if let Some(r) = &self.running {
            return Err(Busy(r.title.clone()));
        }
        if let Some(what) = &self.external {
            return Err(Busy(what.clone()));
        }
        self.row_actions += 1;
        Ok(())
    }

    pub fn end_row_action(&mut self) {
        self.row_actions = self.row_actions.saturating_sub(1);
    }

    /// Log a change without a card — for each file of a batch whose card is
    /// one summary line.
    pub fn record(&mut self, note: &Notification) {
        self.log.append(&LogEntry::from_note(note));
    }

    /// A progress handle for a row action's worker thread, so it can
    /// [`ActivityProgress::record`] the files it changes. Phase reports
    /// through it are dropped: no operation is on the modal.
    pub fn progress_handle(&self, ctx: &egui::Context) -> ActivityProgress {
        ActivityProgress {
            tx: self.tx.clone(),
            ctx: ctx.clone(),
            log: self.log.clone(),
        }
    }

    /// Show a card without logging — the summary of a batch whose files were
    /// each [`Self::record`]ed.
    pub fn card(&mut self, ctx: &egui::Context, note: Notification) {
        self.cards.push_front(Card {
            note,
            born: Instant::now(),
        });
        while self.cards.len() > MAX_CARDS {
            self.cards.pop_back();
        }
        self.unread += 1;
        ctx.request_repaint();
    }

    /// The card text currently on screen, newest first.
    #[cfg(test)]
    pub fn card_lines(&self) -> Vec<String> {
        self.cards.iter().map(|c| c.note.headline()).collect()
    }

    /// Whether a finished operation's report is up (folding in a finish
    /// the root has not drawn yet).
    #[cfg(test)]
    pub fn has_report(&mut self) -> bool {
        self.drain();
        self.report.is_open()
    }

    /// Every event-log line, newest first.
    #[cfg(test)]
    pub fn logged(&self) -> Vec<LogEntry> {
        self.log.read()
    }

    fn drain(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Phase { name, done, total } => {
                    if let Some(r) = &mut self.running {
                        r.phase = name;
                        r.done = done;
                        r.total = total;
                    }
                }
                Msg::Problem(text) => {
                    if let Some(r) = &mut self.running {
                        r.problems.push(text);
                    }
                }
                Msg::Finished(report) => {
                    let running = self.running.take();
                    let Some(mut report) = report else {
                        continue;
                    };
                    if let Some(r) = running {
                        // Every problem the modal listed live belongs in the
                        // report too, whether or not the operation added
                        // some of its own.
                        for p in r.problems {
                            if !report.has_problem(&p) {
                                report.problem(p);
                            }
                        }
                        if r.cancel.is_cancelled() {
                            report = report.cancelled(true);
                        }
                    }
                    self.report.open(report);
                }
            }
        }
    }

    /// Draw the modal, the cards and the log viewer. Returns true while the
    /// modal blocks the app, so views skip their keyboard shortcuts.
    pub fn show(&mut self, ui: &mut egui::Ui) -> bool {
        self.drain();
        let ctx = ui.ctx().clone();
        self.expire_cards();
        self.draw_cards(&ctx);
        self.draw_corner(&ctx);
        if self.show_log {
            self.draw_log(&ctx);
        }
        let blocking = if self.running.is_some() {
            self.draw_running(&ctx);
            true
        } else {
            self.report.show(ui)
        };
        let sliding = self.cards.iter().any(|c| c.born.elapsed() < CARD_SLIDE);
        if sliding {
            ctx.request_repaint();
        } else if self.running.is_some() || !self.cards.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        blocking
    }

    fn expire_cards(&mut self) {
        while self
            .cards
            .back()
            .is_some_and(|c| c.born.elapsed() > CARD_LIFETIME)
        {
            self.cards.pop_back();
        }
    }

    fn draw_running(&mut self, ctx: &egui::Context) {
        let Some(r) = &self.running else {
            return;
        };
        let mut cancel = false;
        // A running operation ignores Escape and backdrop clicks: only CANCEL
        // ends it, and only the finished report closes on Escape.
        egui::Modal::new(Id::new("activity-modal")).show(ctx, |ui| {
            ui.set_width(560.0);
            ui.label(
                RichText::new(r.title.to_uppercase())
                    .color(theme::amber())
                    .size(16.0)
                    .strong(),
            );
            if !r.repos.is_empty() {
                ui.label(
                    RichText::new(r.repos.join(" · "))
                        .color(theme::tan())
                        .size(12.0),
                );
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().color(theme::amber()));
                ui.label(RichText::new(&r.phase).color(theme::text()).size(13.0));
            });
            ui.add_space(6.0);
            match r.fraction() {
                Some(f) => {
                    ui.add(egui::ProgressBar::new(f).fill(theme::amber()));
                    if let Some(total) = r.total {
                        ui.label(
                            RichText::new(format!(
                                "{} of {} · {:.0} %",
                                r.done,
                                total,
                                f64::from(f) * 100.0
                            ))
                            .color(theme::tan())
                            .size(12.0),
                        );
                    }
                }
                None => {
                    ui.add(egui::ProgressBar::new(0.0).animate(true));
                }
            }
            ui.add_space(6.0);
            let elapsed = r.started.elapsed();
            let mut timing = format!(
                "elapsed {}",
                crate::scrub::format_secs(elapsed.as_secs_f64())
            );
            if let Some(eta) = r.eta() {
                timing.push_str(&format!(
                    " · about {} left",
                    crate::scrub::format_secs(eta.as_secs_f64())
                ));
            }
            ui.label(RichText::new(timing).color(theme::tan()).size(12.0));

            if !r.problems.is_empty() {
                ui.add_space(8.0);
                ui.label(
                    RichText::new(format!("{} PROBLEM(S) SO FAR", r.problems.len()))
                        .color(theme::red())
                        .size(12.0)
                        .strong(),
                );
                egui::ScrollArea::vertical()
                    .max_height(140.0)
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for p in &r.problems {
                            ui.colored_label(theme::red(), p);
                        }
                    });
            }
            ui.add_space(12.0);
            let already = r.cancel.is_cancelled();
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        !already,
                        egui::Button::new(
                            RichText::new("CANCEL").color(theme::ink_on(theme::red())),
                        )
                        .fill(theme::red()),
                    )
                    .clicked()
                {
                    cancel = true;
                }
                if already {
                    ui.label(
                        RichText::new("cancelling — finishing the current step")
                            .color(theme::amber())
                            .size(12.0),
                    );
                }
            });
        });
        if cancel && let Some(r) = &self.running {
            r.cancel.cancel();
        }
    }

    fn draw_cards(&mut self, ctx: &egui::Context) {
        let mut y = 44.0;
        for card in &self.cards {
            // Slide in from the right edge over the first quarter second,
            // measured from the card's birth so the first frame starts off-screen.
            let t = (card.born.elapsed().as_secs_f32() / CARD_SLIDE.as_secs_f32()).clamp(0.0, 1.0);
            let slide = (1.0 - t).powi(2) * 320.0;
            let fade =
                1.0 - (card.born.elapsed().as_secs_f32() / CARD_LIFETIME.as_secs_f32()).powi(6);
            let accent = match card.note.outcome {
                Ok(()) if card.note.changed_disk => theme::green(),
                Ok(()) => theme::lilac(),
                Err(_) => theme::red(),
            };
            let response = egui::Area::new(Id::new(("activity-card-area", card.born)))
                .order(egui::Order::Foreground)
                .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-12.0 + slide, y))
                .interactable(false)
                .show(ctx, |ui| {
                    egui::Frame::new()
                        .fill(theme::panel().gamma_multiply(fade))
                        .stroke(egui::Stroke::new(1.5, accent.gamma_multiply(fade)))
                        .corner_radius(8.0)
                        .inner_margin(egui::Margin::symmetric(12, 8))
                        .show(ui, |ui| {
                            ui.set_max_width(320.0);
                            ui.label(
                                RichText::new(card.note.headline())
                                    .color(accent.gamma_multiply(fade))
                                    .size(13.0)
                                    .strong(),
                            );
                            let detail = match (&card.note.outcome, card.note.repo.as_str()) {
                                (Ok(()), repo) => format!("in {repo}"),
                                (Err(e), "") => e.clone(),
                                (Err(e), repo) => format!("in {repo} — {e}"),
                            };
                            ui.label(
                                RichText::new(detail)
                                    .color(theme::text().gamma_multiply(fade))
                                    .size(11.5),
                            );
                        });
                });
            y += response.response.rect.height() + 8.0;
        }
    }

    /// The always-present corner control: the LOG button with its unread
    /// badge, so the history is one click away whether or not a card is up.
    fn draw_corner(&mut self, ctx: &egui::Context) {
        let mut open = false;
        egui::Area::new(Id::new("activity-corner"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-12.0, 8.0))
            .show(ctx, |ui| {
                let label = if self.unread > 0 {
                    format!("LOG ({})", self.unread)
                } else {
                    "LOG".to_string()
                };
                let accent = if self.unread > 0 {
                    theme::amber()
                } else {
                    theme::grey()
                };
                if ui
                    .add(
                        egui::Button::new(RichText::new(label).color(accent).size(11.0))
                            .wrap_mode(egui::TextWrapMode::Extend)
                            .fill(theme::panel())
                            .stroke(egui::Stroke::new(1.0, accent)),
                    )
                    .explain(
                        crate::settings::TooltipVerbosity::default(),
                        "Every change the app made to your files",
                        "Open the event log: every file the app deleted, copied, moved, \
                         renamed or overwrote, newest first, kept across restarts.",
                    )
                    .clicked()
                {
                    open = true;
                }
            });
        if open {
            self.show_log = true;
            self.unread = 0;
            self.log_entries = self.log.read();
        }
    }

    fn draw_log(&mut self, ctx: &egui::Context) {
        let mut close = false;
        let response = egui::Modal::new(Id::new("activity-log")).show(ctx, |ui| {
            ui.set_width(720.0);
            ui.label(
                RichText::new("EVENT LOG")
                    .color(theme::lilac())
                    .size(16.0)
                    .strong(),
            );
            ui.label(
                RichText::new(format!(
                    "every change the app made to your files · {}",
                    self.log.path().display()
                ))
                .color(theme::tan())
                .size(11.0),
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("FILTER").color(theme::tan()).size(11.0));
                ui.add(
                    egui::TextEdit::singleline(&mut self.log_filter)
                        .desired_width(300.0)
                        .hint_text("path, repo, action, date…"),
                );
                let shown = self
                    .log_entries
                    .iter()
                    .filter(|e| e.matches(&self.log_filter))
                    .count();
                ui.label(
                    RichText::new(format!("{shown} of {} entries", self.log_entries.len()))
                        .color(theme::tan())
                        .size(11.0),
                );
            });
            ui.add_space(6.0);
            egui::ScrollArea::vertical()
                .max_height(420.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for e in self
                        .log_entries
                        .iter()
                        .filter(|e| e.matches(&self.log_filter))
                    {
                        let colour = if e.ok { theme::text() } else { theme::red() };
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&e.at).color(theme::tan()).size(11.0));
                            ui.label(RichText::new(&e.action).color(colour).size(11.5).strong());
                            ui.label(RichText::new(&e.repo).color(theme::lilac()).size(11.5));
                            ui.label(RichText::new(&e.path).color(colour).size(11.5));
                            if let Some(err) = &e.error {
                                ui.label(RichText::new(err).color(theme::red()).size(11.0));
                            }
                        });
                    }
                    if self.log_entries.is_empty() {
                        ui.colored_label(theme::tan(), "Nothing has been changed yet.");
                    }
                });
            ui.add_space(10.0);
            if ui
                .add(
                    egui::Button::new(RichText::new("CLOSE").color(theme::ink_on(theme::grey())))
                        .fill(theme::grey()),
                )
                .clicked()
            {
                close = true;
            }
        });
        if close || response.should_close() {
            self.show_log = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_change_is_logged_and_a_mark_is_not() {
        let dir = tempfile::tempdir().expect("dir");
        let ctx = egui::Context::default();
        let mut activity = Activity::new(dir.path());
        let deleted = Notification::changed("Deleted", "photos", "a/b.jpg");
        activity.record(&deleted);
        activity.card(&ctx, deleted);
        activity.card(&ctx, Notification::noted("Marked", "photos", "c.jpg"));
        let failed = Notification::failed("Deleted", "photos", "d.jpg", "permission denied");
        activity.record(&failed);
        activity.card(&ctx, failed);

        let entries = activity.log.read();
        assert_eq!(entries.len(), 2, "{entries:?}");
        assert_eq!(entries[0].path, "d.jpg");
        assert!(!entries[0].ok);
        assert_eq!(entries[0].error.as_deref(), Some("permission denied"));
        assert_eq!(entries[1].path, "a/b.jpg");
        assert!(entries[1].ok);
        assert_eq!(
            activity.card_lines(),
            vec!["FAILED: deleted d.jpg", "Marked c.jpg", "Deleted a/b.jpg"]
        );
        assert_eq!(activity.unread, 3);
    }

    #[test]
    fn a_second_operation_is_refused_while_one_runs_and_row_actions_gate_both_ways() {
        let dir = tempfile::tempdir().expect("dir");
        let ctx = egui::Context::default();
        let mut activity = Activity::new(dir.path());
        assert!(activity.begin_row_action().is_ok());
        let refused = activity.start(
            &ctx,
            Spec {
                title: "FIND".into(),
                repos: vec![],
            },
            |_, _| RunReport::new("never"),
        );
        assert_eq!(refused, Err(Busy("a file action".to_string())));
        activity.end_row_action();

        let (hold_tx, hold_rx) = crossbeam_channel::bounded::<()>(0);
        activity
            .start(
                &ctx,
                Spec {
                    title: "FIND duplicates".into(),
                    repos: vec!["photos".into()],
                },
                move |progress, cancel| {
                    progress.phase("grouping images", 1, Some(2));
                    let _ = hold_rx.recv();
                    RunReport::new("FIND duplicates")
                        .count("groups", 3)
                        .cancelled(cancel.is_cancelled())
                },
            )
            .expect("first start");
        assert!(activity.is_running());
        assert_eq!(
            activity.begin_row_action(),
            Err(Busy("FIND duplicates".to_string()))
        );
        let again = activity.start(
            &ctx,
            Spec {
                title: "RUN".into(),
                repos: vec![],
            },
            |_, _| RunReport::new("never"),
        );
        assert_eq!(again, Err(Busy("FIND duplicates".to_string())));

        // The worker finishes; the report replaces the running state.
        hold_tx.send(()).expect("release");
        for _ in 0..50 {
            activity.drain();
            if !activity.is_running() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!activity.is_running());
        assert!(activity.busy().is_none());
    }

    #[test]
    fn a_quiet_operation_closes_without_a_report_and_its_worker_can_log() {
        let dir = tempfile::tempdir().expect("dir");
        let ctx = egui::Context::default();
        let mut activity = Activity::new(dir.path());
        activity
            .start_quiet(
                &ctx,
                Spec {
                    title: "REVIEW".into(),
                    repos: vec!["photos".into()],
                },
                |progress, _| {
                    progress.phase("reading 'photos'", 0, None);
                    progress.record(&Notification::changed("Copied", "photos", "a.jpg"));
                    Ok(())
                },
            )
            .expect("start");
        for _ in 0..50 {
            activity.drain();
            if !activity.is_running() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!activity.is_running());
        assert!(!activity.report.is_open(), "a quiet finish shows no report");
        let logged = activity.log.read();
        assert_eq!(logged.len(), 1);
        assert_eq!((logged[0].action.as_str(), logged[0].ok), ("Copied", true));

        activity
            .start_quiet(
                &ctx,
                Spec {
                    title: "REVIEW".into(),
                    repos: vec![],
                },
                |_, _| Err("index unreadable".to_string()),
            )
            .expect("start");
        for _ in 0..50 {
            activity.drain();
            if !activity.is_running() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(activity.report.is_open(), "a failed quiet finish reports");
    }

    #[test]
    fn external_work_counts_as_busy() {
        let dir = tempfile::tempdir().expect("dir");
        let ctx = egui::Context::default();
        let mut activity = Activity::new(dir.path());
        activity.set_external(Some("scan of 'photos'".into()));
        assert_eq!(activity.busy(), Some(Busy("scan of 'photos'".into())));
        assert_eq!(
            activity.begin_row_action(),
            Err(Busy("scan of 'photos'".into()))
        );
        let refused = activity.start(
            &ctx,
            Spec {
                title: "FIND".into(),
                repos: vec![],
            },
            |_, _| RunReport::new("never"),
        );
        assert_eq!(refused, Err(Busy("scan of 'photos'".into())));
        activity.set_external(None);
        assert!(activity.busy().is_none());
    }
}
