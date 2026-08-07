//! The Duplicate Management tab: choose repos (optionally read-only), find exact
//! duplicates or perceptual similars, review paged groups with thumbnails, and
//! delete the worse copies — batched per repo, never without a confirmation.

use crate::filter_ui::FilterBuilder;
use crate::icon;
use crate::lightbox::has_text_representation;
use crate::media_cell::{FileFacts, MediaStyle, fmt_ms, media_cell};
use crate::player::Player;
use crate::settings::TooltipVerbosity;
use crate::theme;
use crate::thumbs::ThumbCache;
use crate::util::{ExplainExt, format_mtime, format_size};
use crossbeam_channel::{Receiver, Sender};
use dedup_core::dupes::{
    DupeDeleteStats, DupeFile, DupeGroup, DupeGroupKey, delete_paths, load_groups,
    plan_exact_duplicates, retain_matching_keys, wasted_bytes,
};
use dedup_core::filter::FileFilter;
use dedup_core::similar::find_similar;
use dedup_core::store::Store;
use dedup_core::thumbnail::hash_hex;
use egui::{Id, RichText};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

const PAGE_SIZE: usize = 50;
/// Load groups from the DB in batches of this many during auto-resolve.
const AUTO_BATCH: usize = 128;

/// The current result set: exact duplicates are a lightweight *plan* of
/// descriptors (members loaded a page at a time), while similar results are the
/// full (small) group list held in memory.
enum Results {
    Exact(Vec<DupeGroupKey>),
    Similar(Vec<DupeGroup>),
}

impl Results {
    fn len(&self) -> usize {
        match self {
            Results::Exact(plan) => plan.len(),
            Results::Similar(groups) => groups.len(),
        }
    }
}

/// A background operation in flight (drives the spinner and disables actions).
enum Op {
    Find(usize),
    AutoResolve { done: usize, total: usize },
    Delete,
}

/// Messages from background operation threads back to the UI.
enum Msg {
    FindProgress(usize),
    FindDone(Result<Results, String>),
    AutoProgress { done: usize, total: usize },
    AutoDone(Result<Vec<FileKey>, String>),
    DeleteDone(Result<DupeDeleteStats, String>, DeleteFollow),
}

/// What to do after a background delete finishes.
#[derive(Clone, Copy)]
enum DeleteFollow {
    /// Global delete: clear marks and re-run the search.
    Refind,
    /// Per-group delete: drop this group's marks and collapse it (no re-plan).
    Resolve(usize),
}

/// The action a pending confirmation applies when accepted.
#[derive(Clone, Copy)]
enum ConfirmAction {
    EnableQuickDelete,
    DeleteAll,
}

#[derive(PartialEq, Clone, Copy)]
enum Mode {
    Exact,
    Similar,
}

/// A repo's participation in the current search.
struct RepoSel {
    name: String,
    included: bool,
    /// Read-only repos are never selected for deletion.
    read_only: bool,
    /// This repo is the main of a sync group — badged wherever it is named.
    is_main: bool,
}

/// How long a press must be held (mouse or touch) to count as a long-press.
const LONG_PRESS_SECS: f64 = 0.5;

/// Whether `resp` was long-pressed: a touchscreen long-touch, or the primary
/// mouse button held down on it for [`LONG_PRESS_SECS`]. Fires once per press
/// (tracked in egui memory by the widget id) rather than every frame while held.
fn long_pressed(ui: &egui::Ui, resp: &egui::Response) -> bool {
    if resp.long_touched() {
        return true;
    }
    if !resp.is_pointer_button_down_on() {
        return false;
    }
    let (start, now) = ui.input(|i| (i.pointer.press_start_time(), i.time));
    let Some(start) = start else {
        return false;
    };
    // Keep re-evaluating while the button is held so the threshold is noticed.
    ui.ctx().request_repaint();
    // `insert_temp(id, start)` marks this specific press handled.
    let already = ui.data(|d| d.get_temp::<f64>(resp.id)) == Some(start);
    if now - start >= LONG_PRESS_SECS && !already {
        ui.data_mut(|d| d.insert_temp(resp.id, start));
        return true;
    }
    false
}

/// Unique key for a file across repos.
type FileKey = (String, String);

fn key(file: &DupeFile) -> FileKey {
    (file.repo.clone(), file.rel_path.clone())
}

/// Deferred UI actions, applied after rendering to avoid double borrows.
enum Act {
    ToggleInclude(usize),
    ToggleRo(usize),
    Find,
    ToggleMark(FileKey),
    Unlock(FileKey),
    Relock(FileKey),
    Open(PathBuf),
    Reveal(PathBuf),
    /// Open the image lightbox at (group index, member index).
    OpenLightbox(usize, usize),
    /// Play an audio file: (content-hash hex, absolute path, total ms).
    PlayAudio(String, PathBuf, u64),
    /// Seek the current audio to a fraction [0,1] of its length.
    SeekAudio(f32),
    ToggleQuickDelete,
    AutoResolve,
    DeleteGroup(usize),
    /// Mark every markable (non-protected) file in group `gi` for deletion.
    MarkGroup(usize),
    /// Clear the marks on every file in group `gi` (keep them all).
    UnmarkGroup(usize),
    /// Dismiss group `gi` from the list until the next FIND.
    HideGroup(usize),
    AskDelete,
    ConfirmDelete,
    CancelDelete,
    SetPage(usize),
}

pub struct DupesView {
    repos: Vec<RepoSel>,
    repos_loaded: bool,
    mode: Mode,
    threshold: f64,
    /// The result set (plan for exact, full groups for similar), if a search has
    /// run. `None` before the first FIND.
    results: Option<Results>,
    /// Repo names the current `results` were computed for (used to load pages).
    result_names: Vec<String>,
    /// The current page's materialized groups, and which page they are.
    page_groups: Vec<DupeGroup>,
    cached_page: Option<usize>,
    /// Cached rendered height per group index (absolute); `0.0` = not measured.
    group_heights: Vec<f32>,
    marked: HashSet<FileKey>,
    /// Per-file read-only overrides: files explicitly unlocked (via the
    /// read-only badge's context menu / long press) so a single worse copy in
    /// an otherwise protected repo can be marked. Deliberately inconvenient —
    /// never bulk-set, ignored by auto-resolve, and reset on every FIND.
    unlocked: HashSet<FileKey>,
    /// Pages whose non-best copies have already been marked by default, so
    /// revisiting a page doesn't clobber the user's manual KEEP/DELETE choices.
    preselected_pages: HashSet<usize>,
    /// Group indices deleted this session (rendered collapsed). Reset on FIND.
    resolved: HashSet<usize>,
    /// Group indices the user dismissed with HIDE GROUP; skipped from the list
    /// until the next FIND (a triage aid, not a delete). Reset on FIND.
    hidden: HashSet<usize>,
    /// When on, per-group DELETE NOW buttons appear and delete immediately.
    quick_delete: bool,
    /// Keys handed to the in-flight delete, applied to `marked` on completion.
    delete_batch: Vec<FileKey>,
    page: usize,
    /// A background operation in flight, if any.
    busy: Option<Op>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    status: Option<String>,
    error: Option<String>,
    confirm: Option<(String, ConfirmAction)>,
    thumbs: ThumbCache,
    /// The open shared viewer ([`crate::compare_view::DiffCompare`]), if any —
    /// the same full-window surface every other caller opens.
    lightbox: Option<crate::compare_view::DiffCompare>,
    /// Global audio preview player (one file at a time).
    player: Player,
    /// Tooltip wording for this frame, set at the top of [`Self::show`] from
    /// the app-wide setting (not persisted here; `app.rs` owns that).
    verbosity: TooltipVerbosity,
    /// The shared FILTER wizard: FIND keeps only groups with at least one member
    /// matching this (the same widget used by Transfer/Grooming/Browse).
    filter: FilterBuilder,
    /// Content identity → the archives (across the searched repos) that contain
    /// it, built on FIND. Drives the read-only "evidence rows" (this content
    /// also lives inside a zip) and the tiered delete-safety warning.
    archive_evidence: HashMap<(u64, [u8; 32]), Vec<dedup_core::archive::ArchiveOccurrence>>,
}

impl DupesView {
    pub fn new() -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        Self {
            repos: Vec::new(),
            repos_loaded: false,
            mode: Mode::Exact,
            threshold: 99.0,
            results: None,
            result_names: Vec::new(),
            page_groups: Vec::new(),
            cached_page: None,
            group_heights: Vec::new(),
            marked: HashSet::new(),
            unlocked: HashSet::new(),
            preselected_pages: HashSet::new(),
            resolved: HashSet::new(),
            hidden: HashSet::new(),
            quick_delete: false,
            delete_batch: Vec::new(),
            page: 0,
            busy: None,
            tx,
            rx,
            status: None,
            error: None,
            confirm: None,
            thumbs: ThumbCache::new(3),
            lightbox: None,
            player: Player::new(),
            verbosity: TooltipVerbosity::default(),
            filter: FilterBuilder::new(),
            archive_evidence: HashMap::new(),
        }
    }

    /// The similarity slider position (persisted across launches).
    pub fn threshold(&self) -> f64 {
        self.threshold
    }

    /// Restore the similarity slider position from persisted settings.
    pub fn set_threshold(&mut self, threshold: f64) {
        self.threshold = threshold.clamp(50.0, 100.0);
    }

    /// Stop any audio preview (called when leaving the tab).
    pub fn stop_audio(&self) {
        if self.player.is_active() {
            self.player.stop();
        }
    }

    /// Total number of result groups (0 if no search yet).
    fn total_groups(&self) -> usize {
        self.results.as_ref().map_or(0, Results::len)
    }

    pub fn show(&mut self, ui: &mut egui::Ui, store: &Arc<Store>, verbosity: TooltipVerbosity) {
        self.verbosity = verbosity;
        let ctx = ui.ctx().clone();
        if self.thumbs.poll(&ctx) {
            ctx.request_repaint();
        }
        self.drain_messages(store, &ctx);
        if !self.repos_loaded {
            self.sync_repos(store);
        }

        let mut acts: Vec<Act> = Vec::new();

        // Grid keyboard shortcuts — skipped while a lightbox or confirm modal
        // owns the keyboard, or a text field is focused.
        if self.lightbox.is_none() && self.confirm.is_none() && !ctx.egui_wants_keyboard_input() {
            let pages = self.total_groups().div_ceil(PAGE_SIZE);
            let cur = self.page.min(pages.saturating_sub(1));
            ctx.input(|i| {
                if i.key_pressed(egui::Key::F) && self.busy.is_none() {
                    acts.push(Act::Find);
                }
                if i.key_pressed(egui::Key::M) {
                    self.mode = if self.mode == Mode::Exact {
                        Mode::Similar
                    } else {
                        Mode::Exact
                    };
                }
                if i.key_pressed(egui::Key::ArrowLeft) && cur > 0 {
                    acts.push(Act::SetPage(cur - 1));
                }
                if i.key_pressed(egui::Key::ArrowRight) && cur + 1 < pages {
                    acts.push(Act::SetPage(cur + 1));
                }
            });
        }

        ui.add_space(6.0);
        ui.label(
            RichText::new("DUPLICATE MANAGEMENT")
                .color(theme::lilac())
                .size(18.0)
                .strong(),
        );
        self.repo_bar(ui, &mut acts);
        // Shared FILTER wizard: FIND keeps only groups with ≥1 member matching
        // it. The first included repo backs the MIME/TAG pick-lists.
        let sugg = self.suggestion_repo();
        let outcome = self.filter.ui(ui, store, sugg.as_deref(), self.verbosity);
        if outcome.status.is_some() {
            self.status = outcome.status;
        }
        if outcome.error.is_some() {
            self.error = outcome.error;
        }
        self.controls(ui, &mut acts);
        crate::util::shortcut_bar(
            ui,
            &format!(
                "F find · {}/{} page · M mode",
                icon::CARET_LEFT,
                icon::CARET_RIGHT
            ),
        );
        if let Some(err) = &self.error {
            ui.colored_label(theme::red(), err);
        }
        if let Some(status) = &self.status {
            ui.label(RichText::new(status).color(theme::tan()).size(13.0));
        }
        ui.separator();
        self.results(ui, store, &mut acts);

        if let Some((prompt, action)) = self.confirm.clone() {
            let verb = match action {
                ConfirmAction::EnableQuickDelete => "ENABLE",
                ConfirmAction::DeleteAll => "DELETE",
            };
            self.confirm_modal(ui, &prompt, verb, &mut acts);
        }

        // The shared viewer overlays everything else when open.
        self.viewer_modal(&ctx, store, &mut acts);

        for act in acts {
            self.apply(&ctx, store, act);
        }

        // Keep the spinner/progress ticking while a background op runs, and the
        // seek bar moving while audio plays. The full-res cache wakes the UI
        // itself when a decode lands.
        if self.busy.is_some() || self.player.is_active() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

    /// Apply results/marks from finished background operations.
    fn drain_messages(&mut self, store: &Arc<Store>, ctx: &egui::Context) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::FindProgress(n) => self.busy = Some(Op::Find(n)),
                Msg::FindDone(result) => {
                    self.busy = None;
                    match result {
                        Ok(results) => {
                            self.group_heights = vec![0.0; results.len()];
                            self.status = Some(format!("{} group(s)", results.len()));
                            self.results = Some(results);
                            self.marked.clear();
                            self.unlocked.clear();
                            self.preselected_pages.clear();
                            self.resolved.clear();
                            self.hidden.clear();
                            self.page = 0;
                            self.cached_page = None;
                            self.error = None;
                            // The archives across the searched repos that contain
                            // the same content as loose files — for evidence rows
                            // and the tiered delete-safety warning.
                            let refs: Vec<&str> =
                                self.result_names.iter().map(String::as_str).collect();
                            self.archive_evidence =
                                dedup_core::archive::members_by_content(store, &refs)
                                    .unwrap_or_default();
                        }
                        Err(e) => self.error = Some(e),
                    }
                }
                Msg::AutoProgress { done, total } => {
                    self.busy = Some(Op::AutoResolve { done, total });
                }
                Msg::AutoDone(result) => {
                    self.busy = None;
                    match result {
                        Ok(keys) => {
                            for k in keys {
                                self.marked.insert(k);
                            }
                            self.status =
                                Some(format!("{} marked for deletion", self.marked.len()));
                        }
                        Err(e) => self.error = Some(e),
                    }
                }
                Msg::DeleteDone(result, follow) => {
                    self.busy = None;
                    let batch = std::mem::take(&mut self.delete_batch);
                    match result {
                        Ok(stats) => match follow {
                            DeleteFollow::Refind => {
                                self.status = Some(format!(
                                    "Deleted {} file(s), {} error(s). Re-running search…",
                                    stats.deleted, stats.errors
                                ));
                                self.marked.clear();
                                self.start_find(store, ctx);
                            }
                            DeleteFollow::Resolve(gi) => {
                                self.status = Some(format!("Deleted {} file(s)", stats.deleted));
                                for k in &batch {
                                    self.marked.remove(k);
                                }
                                self.resolved.insert(gi);
                            }
                        },
                        Err(e) => self.error = Some(e),
                    }
                }
            }
        }
    }

    /// Sync the repo list with the store, non-destructively: existing repos keep
    /// their include + read-only state, newly-registered repos default to
    /// *excluded* + read-only (you opt in the repos to search via the chips or
    /// MARK ALL; read-only stays the safe default — deleting is a deliberate
    /// unlock), and removed repos drop out. Called on first show and whenever the
    /// tab is re-shown, so the list stays fresh without a manual refresh button.
    pub fn sync_repos(&mut self, store: &Store) {
        match store.list_repos() {
            Ok(list) => {
                // Sinks are searched through their group's main, not directly.
                let sinks = store.sink_repo_names().unwrap_or_default();
                let mains = store.main_repo_names().unwrap_or_default();
                let prev = std::mem::take(&mut self.repos);
                self.repos = list
                    .into_iter()
                    .filter(|(name, _, _)| !sinks.contains(name))
                    .map(|(name, _, _)| {
                        let old = prev.iter().find(|r| r.name == name);
                        RepoSel {
                            included: old.map(|r| r.included).unwrap_or(false),
                            read_only: old.map(|r| r.read_only).unwrap_or(true),
                            is_main: mains.contains(&name),
                            name,
                        }
                    })
                    .collect();
                self.repos_loaded = true;
                self.error = None;
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    fn repo_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        crate::lcars::section_lcars(
            ui,
            "REPOS — WHERE TO LOOK FOR DUPLICATES",
            theme::lilac(),
            |ui| {
                // Bulk MARK ALL / NONE (repos start excluded, so this is the quick way
                // to include/clear all of them at once).
                ui.horizontal(|ui| {
                    if crate::lcars::toggle_button(ui, "ALL", false, theme::orange())
                        .explain(
                            self.verbosity,
                            "Include every repo in the search",
                            "Include (check) every repository so FIND searches them all.",
                        )
                        .clicked()
                    {
                        self.repos.iter_mut().for_each(|r| r.included = true);
                    }
                    if crate::lcars::toggle_button(ui, "NONE", false, theme::orange())
                        .explain(
                            self.verbosity,
                            "Exclude every repo",
                            "Exclude (uncheck) every repository. FIND needs at least one included.",
                        )
                        .clicked()
                    {
                        self.repos.iter_mut().for_each(|r| r.included = false);
                    }
                });
                // The shared wrapping chip row (see `repo_chip::chip_row`) breaks onto
                // multiple lines when the window is narrow.
                crate::repo_chip::chip_row(ui, "dupes_repos", "", self.repos.len(), |ui, i| {
                    self.repo_chip(ui, i, acts)
                });
            },
        );
    }

    /// One repo chip — the shared identicon + name include-toggle plus the
    /// read-only padlock (the "3 in the group" variant). Returns the chip frame
    /// response (its rect feeds the wrap packing in [`repo_chip::chip_row`]).
    fn repo_chip(&self, ui: &mut egui::Ui, i: usize, acts: &mut Vec<Act>) -> egui::Response {
        let repo = &self.repos[i];
        let chip = crate::repo_chip::repo_chip(
            ui,
            &repo.name,
            repo.included,
            theme::orange(),
            repo.is_main,
            Some(repo.read_only),
        );
        if chip
            .name
            .explain(
                self.verbosity,
                "Toggle whether this repo is searched",
                "Include or exclude this repository from FIND results. Excluded repos \
                 are skipped entirely — their files won't appear as duplicates or as \
                 candidates.",
            )
            .clicked()
        {
            acts.push(Act::ToggleInclude(i));
        }
        if let Some(lock) = chip.lock {
            // Closed padlock = read-only (protected); open padlock = deletable.
            let (hover, hover_verbose) = if repo.read_only {
                (
                    "Locked: files here are protected from deletion — click to allow deleting",
                    "This repo is read-only: none of its files are ever preselected or \
                     deletable, even by auto-resolve. Click to unlock the whole repo for \
                     deletion.",
                )
            } else {
                (
                    "Unlocked: files here can be deleted — click to protect",
                    "This repo is unlocked: its files can be marked and deleted like any \
                     other. Click to protect it (read-only) again.",
                )
            };
            if lock.explain(self.verbosity, hover, hover_verbose).clicked() {
                acts.push(Act::ToggleRo(i));
            }
        }
        chip.outer
    }

    fn controls(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        crate::lcars::section_lcars(
            ui,
            "MODE — WHAT COUNTS AS A DUPLICATE",
            theme::amber(),
            |ui| {
                ui.horizontal(|ui| {
                    let exact = self.mode == Mode::Exact;
                    // The two match modes: DUPLICATES (orange) / SIMILAR (lilac).
                    if crate::lcars::toggle_button(ui, "DUPLICATES", exact, theme::orange())
                        .explain(
                            self.verbosity,
                            "Exact byte-for-byte duplicates",
                            "Find files whose content is byte-for-byte identical \
                         (same size and BLAKE3 hash). Fast, no false positives.",
                        )
                        .clicked()
                    {
                        self.mode = Mode::Exact;
                    }
                    if crate::lcars::toggle_button(ui, "SIMILAR", !exact, theme::lilac())
                        .explain(
                            self.verbosity,
                            "Perceptually similar images/videos",
                            "Find images and videos that look alike even when their \
                         bytes differ — re-saves, re-encodes, or crops — using a \
                         perceptual hash and the similarity threshold below.",
                        )
                        .clicked()
                    {
                        self.mode = Mode::Similar;
                    }
                    if crate::lcars::action_button(
                        ui,
                        &format!("{} FIND", icon::SEARCH),
                        self.busy.is_none(),
                        theme::amber(),
                    )
                    .explain(
                        self.verbosity,
                        "Search the included repos",
                        "Search every included (checked) repository for duplicates or \
                     similars per the selected mode. Excluded repos are skipped.",
                    )
                    .clicked()
                    {
                        acts.push(Act::Find);
                    }
                    // Progress while a background op runs.
                    if let Some(op) = &self.busy {
                        ui.add(egui::Spinner::new().color(theme::amber()));
                        let text = match op {
                            Op::Find(n) => format!("searching… {n} groups"),
                            Op::AutoResolve { done, total } => {
                                format!("auto-resolving… {done}/{total}")
                            }
                            Op::Delete => "deleting…".to_string(),
                        };
                        ui.label(RichText::new(text).color(theme::amber()).size(12.0));
                    }
                });

                // The similarity threshold gets its own row so the slider has room
                // to read as a slider (cramming it into the button row hid the track
                // behind the value box). The value box still accepts typed floats.
                if self.mode == Mode::Similar {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        crate::util::similarity_slider(ui, &mut self.threshold, self.verbosity);
                    });
                }

                // Quick Delete: gives each group a DELETE NOW button that removes
                // its marked files instantly (no per-group confirmation).
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                // A proper bordered toggle now (filled red when on), so it's
                // clearly a clickable control even when off.
                let label = format!("{} QUICK DELETE", icon::LIGHTNING);
                if crate::lcars::toggle_button(ui, &label, self.quick_delete, theme::red())
                    .explain(
                        self.verbosity,
                        "Show a DELETE NOW button on each group that deletes its marked files immediately, no confirmation",
                        "When on, every group gets a DELETE NOW button that deletes its \
                         currently marked files immediately, skipping the usual \
                         confirmation dialog. Turn off to go back to confirming every batch.",
                    )
                    .clicked()
                {
                    acts.push(Act::ToggleQuickDelete);
                }
                if self.quick_delete {
                    ui.label(
                        RichText::new("on — DELETE NOW removes files instantly")
                            .color(theme::red())
                            .size(12.0),
                    );
                }
            });
            },
        );

        if self.total_groups() > 0 {
            ui.horizontal(|ui| {
                let n = self.marked.len();
                let idle = self.busy.is_none();
                if crate::lcars::action_button(ui, "AUTO-RESOLVE REST", idle, theme::orange())
                    .explain(
                        self.verbosity,
                        "Mark every non-best copy in a deletable repo",
                        "Across every result group, mark every copy except the best one for \
                         deletion — but only in repos that aren't read-only. Review the \
                         marks before deleting; nothing is deleted by this button alone.",
                    )
                    .clicked()
                {
                    acts.push(Act::AutoResolve);
                }
                if crate::lcars::action_button(
                    ui,
                    &format!("DELETE MARKED ({n})"),
                    idle && n > 0,
                    theme::red(),
                )
                .explain(
                    self.verbosity,
                    "Delete every marked file, with confirmation",
                    "Delete every currently marked file across all groups, batched per \
                     repo in one transaction. Always asks for confirmation first — use \
                     QUICK DELETE if you want per-group deletes without asking.",
                )
                .clicked()
                {
                    acts.push(Act::AskDelete);
                }
            });
        }
    }

    fn results(&mut self, ui: &mut egui::Ui, store: &Arc<Store>, acts: &mut Vec<Act>) {
        let total = self.total_groups();
        if total == 0 {
            ui.add_space(8.0);
            let msg = if self.busy.is_some() {
                "Searching…"
            } else {
                "No groups. Pick repos and press FIND."
            };
            ui.colored_label(theme::text(), msg);
            return;
        }

        let pages = total.div_ceil(PAGE_SIZE);
        let page = self.page.min(pages.saturating_sub(1));
        ui.horizontal(|ui| {
            if ui
                .add_enabled(page > 0, egui::Button::new(icon::CARET_LEFT))
                .explain(
                    self.verbosity,
                    "Previous page",
                    "Go to the previous page of groups.",
                )
                .clicked()
            {
                acts.push(Act::SetPage(page - 1));
            }
            ui.label(
                RichText::new(format!("page {}/{} · {} groups", page + 1, pages, total))
                    .color(theme::tan()),
            );
            if ui
                .add_enabled(page + 1 < pages, egui::Button::new(icon::CARET_RIGHT))
                .explain(
                    self.verbosity,
                    "Next page",
                    "Go to the next page of groups.",
                )
                .clicked()
            {
                acts.push(Act::SetPage(page + 1));
            }
        });

        let start = page * PAGE_SIZE;
        let end = (start + PAGE_SIZE).min(total);
        if self.group_heights.len() != total {
            self.group_heights = vec![0.0; total];
        }
        // Materialize just this page's groups (exact loads from the DB).
        self.ensure_page(store, page);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let avail_w = ui.available_width();
                let spacing = ui.spacing().item_spacing.y;
                for gi in start..end {
                    // HIDE GROUP dismisses a group from the list until the next
                    // FIND — skip it entirely (no card, no reserved space).
                    if self.hidden.contains(&gi) {
                        continue;
                    }
                    // Virtualize: a group we've measured before and that lies
                    // outside the viewport just reserves its known height — we
                    // skip building (and cloning) its widgets entirely. Unmeasured
                    // groups always render once so their height is recorded.
                    let known = self.group_heights[gi];
                    let top = ui.next_widget_position();
                    let visible = known <= 0.0
                        || ui.is_rect_visible(egui::Rect::from_min_size(
                            top,
                            egui::vec2(avail_w, known),
                        ));
                    if visible {
                        let before = top.y;
                        self.group_card(ui, gi, start, acts);
                        self.group_heights[gi] = ui.next_widget_position().y - before;
                    } else {
                        // `allocate_space` adds a trailing item-spacing itself, so
                        // reserve the slot minus that to match the rendered advance.
                        ui.allocate_space(egui::vec2(avail_w, (known - spacing).max(0.0)));
                    }
                }
            });
    }

    /// Load the current page's groups into `page_groups` if not already cached.
    fn ensure_page(&mut self, store: &Arc<Store>, page: usize) {
        if self.cached_page == Some(page) {
            return;
        }
        let total = self.total_groups();
        let start = page * PAGE_SIZE;
        let end = (start + PAGE_SIZE).min(total);
        let loaded = match &self.results {
            Some(Results::Exact(plan)) => {
                load_groups(store, &self.result_names, &plan[start..end]).map_err(|e| e.to_string())
            }
            Some(Results::Similar(groups)) => Ok(groups[start..end].to_vec()),
            None => Ok(Vec::new()),
        };
        match loaded {
            Ok(groups) => self.page_groups = groups,
            Err(e) => {
                self.error = Some(e);
                self.page_groups = Vec::new();
            }
        }
        self.cached_page = Some(page);

        // Promote protected (read-only repo) copies to best within each group,
        // so a protected original is the kept copy and its writable duplicate
        // falls to the deletable tail (the default mark below then targets the
        // writable copy, not the protected one).
        let ro = self.read_only_names();
        dedup_core::dupes::promote_protected_first(&mut self.page_groups, |f| ro.contains(&f.repo));

        // Default-mark this page's worse (non-best) copies once, so the extras
        // show DELETE by default. Read-only repos are never marked, and a page
        // is only preselected once so manual KEEP choices survive a revisit.
        if self.preselected_pages.insert(page) {
            let keys: Vec<FileKey> = self
                .page_groups
                .iter()
                .flat_map(|g| g.iter().skip(1))
                .filter(|f| !ro.contains(&f.repo))
                .map(key)
                .collect();
            self.marked.extend(keys);
        }
    }

    fn group_card(&mut self, ui: &mut egui::Ui, gi: usize, page_start: usize, acts: &mut Vec<Act>) {
        // A group deleted this session collapses to a one-line note.
        if self.resolved.contains(&gi) {
            egui::Frame::new()
                .fill(theme::panel())
                .corner_radius(theme::PILL)
                .stroke(egui::Stroke::new(1.0, theme::tan()))
                .inner_margin(10.0)
                .outer_margin(egui::Margin {
                    left: 0,
                    right: 0,
                    top: 0,
                    bottom: 8,
                })
                .show(ui, |ui| {
                    ui.label(
                        RichText::new(format!("{} deleted", icon::CHECK))
                            .color(theme::tan())
                            .strong(),
                    );
                });
            return;
        }

        // The page's groups are already materialized; clone the (small) one so the
        // render closure can borrow `self` mutably for thumbnails/mark state.
        let group: DupeGroup = self
            .page_groups
            .get(gi - page_start)
            .cloned()
            .unwrap_or_default();
        let count = group.len();
        let wasted = wasted_bytes(&group);
        // Exact copies share one size; similar members don't, so their header
        // shows the combined size instead of a per-copy one.
        let header = if matches!(self.results, Some(Results::Similar(_))) {
            let total: u64 = group.iter().map(|f| f.entry.size).sum();
            format!(
                "{count} similar · {} total · {} reclaimable",
                format_size(total),
                format_size(wasted)
            )
        } else {
            let size = group.first().map(|f| f.entry.size).unwrap_or(0);
            format!(
                "{count} copies · {} each · {} reclaimable",
                format_size(size),
                format_size(wasted)
            )
        };
        // Flags read before the render closure (which borrows `self` mutably).
        let quick = self.quick_delete;
        let idle = self.busy.is_none();
        let has_marked = group.iter().any(|f| self.marked.contains(&key(f)));
        egui::Frame::new()
            .fill(theme::panel())
            .corner_radius(theme::PILL)
            .stroke(egui::Stroke::new(1.5, theme::orange()))
            .inner_margin(10.0)
            .outer_margin(egui::Margin {
                left: 0,
                right: 0,
                top: 0,
                bottom: 8,
            })
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(header).color(theme::amber()).strong());
                    // Group-level bulk actions: mark every copy, keep every copy,
                    // or dismiss the whole group — no need to touch each card.
                    if crate::repo_chip::small_button(ui, "MARK ALL", theme::red())
                        .explain(
                            self.verbosity,
                            "Mark every copy in this group for deletion",
                            "Mark all deletable copies in this group for deletion. \
                             Protected (locked) copies are left untouched, and nothing is \
                             removed until you run DELETE.",
                        )
                        .clicked()
                    {
                        acts.push(Act::MarkGroup(gi));
                    }
                    if crate::repo_chip::small_button(ui, "MARK NONE", theme::tan())
                        .explain(
                            self.verbosity,
                            "Keep every copy in this group",
                            "Clear all deletion marks in this group, so every copy is kept.",
                        )
                        .clicked()
                    {
                        acts.push(Act::UnmarkGroup(gi));
                    }
                    if crate::repo_chip::small_button(ui, "HIDE", theme::blue())
                        .explain(
                            self.verbosity,
                            "Hide this group until the next search",
                            "Dismiss this group from the list until the next FIND. It isn't \
                             deleted or changed — just hidden to keep your review focused.",
                        )
                        .clicked()
                    {
                        acts.push(Act::HideGroup(gi));
                    }
                    // Quick Delete: one-click removal of this group's marked files.
                    if quick && has_marked {
                        let del = egui::Button::new(
                            RichText::new(format!("{} DELETE NOW", icon::TRASH))
                                .color(theme::ink_on(theme::red())),
                        )
                        .fill(theme::red());
                        if ui
                            .add_enabled(idle, del)
                            .explain(
                                self.verbosity,
                                "Delete this group's marked files now",
                                "Delete this group's marked files immediately, no \
                                 confirmation — QUICK DELETE is on. Files still marked \
                                 KEEP are untouched.",
                            )
                            .clicked()
                        {
                            acts.push(Act::DeleteGroup(gi));
                        }
                    }
                });
                // One row per group; wide groups scroll horizontally with their
                // own (solid, always-allocated) scrollbar instead of wrapping.
                egui::ScrollArea::horizontal()
                    .id_salt(("group_row", gi))
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            for (fi, file) in group.iter().enumerate() {
                                self.file_card(ui, gi, fi, file, fi == 0, acts);
                            }
                        });
                    });

                // Evidence rows: archives that also contain this group's
                // content. Read-only — they inform the keep/delete decision
                // (the loose copy is also archived; or the whole zip is that
                // much more redundant) but carry no action of their own.
                let mut occ: Vec<&dedup_core::archive::ArchiveOccurrence> = Vec::new();
                let mut seen = HashSet::new();
                for f in &group {
                    if let Some(list) = self.archive_evidence.get(&(f.entry.size, f.entry.hash)) {
                        for o in list {
                            if seen.insert((
                                o.repo.clone(),
                                o.archive_rel.clone(),
                                o.member_name.clone(),
                            )) {
                                occ.push(o);
                            }
                        }
                    }
                }
                if !occ.is_empty() {
                    ui.add_space(4.0);
                    for o in occ {
                        ui.label(
                            RichText::new(format!(
                                "{}  in archive:  {} › {}  ({})",
                                icon::FOLDER_OPEN,
                                o.archive_rel,
                                o.member_name,
                                o.repo,
                            ))
                            .color(theme::lilac())
                            .size(11.0),
                        )
                        .on_hover_text(
                            "This content also lives inside this archive. It cannot be marked \
                             here — delete a loose copy, or the whole archive.",
                        );
                    }
                }
            });
    }

    fn file_card(
        &mut self,
        ui: &mut egui::Ui,
        gi: usize,
        fi: usize,
        file: &DupeFile,
        is_best: bool,
        acts: &mut Vec<Act>,
    ) {
        let k = key(file);
        let marked = self.marked.contains(&k);
        let repo_ro = self.repo_is_ro(&file.repo);
        let unlocked = repo_ro && self.unlocked.contains(&k);
        let ro = repo_ro && !unlocked;
        egui::Frame::new()
            .fill(theme::bg())
            .corner_radius(theme::PILL)
            .inner_margin(8.0)
            .outer_margin(egui::Margin::same(4))
            .show(ui, |ui| {
                // The whole card senses clicks *behind* its children (a
                // trailing `interact` would sit on top and swallow the KEEP
                // button and badge menus), carrying the external-open menu.
                let card =
                    ui.scope_builder(egui::UiBuilder::new().sense(egui::Sense::click()), |ui| {
                        ui.vertical(|ui| {
                            ui.set_width(200.0);
                            // Selectable labels sense clicks and would swallow
                            // right-clicks over the card's text, so the menu
                            // would only open over the sparse non-text areas.
                            // The read-only/unlocked badges keep their own
                            // menus via an explicit click sense.
                            ui.style_mut().interaction.selectable_labels = false;
                            self.thumbnail(ui, gi, fi, file, acts);
                            ui.label(
                                RichText::new(&file.rel_path)
                                    .color(theme::text())
                                    .size(12.0)
                                    .strong(),
                            );
                            ui.label(
                                RichText::new(format!(
                                    "{} · {}",
                                    file.repo,
                                    format_size(file.entry.size)
                                ))
                                .color(theme::tan())
                                .size(11.0),
                            );
                            let dims = file
                                .entry
                                .img_size
                                .map(|(w, h)| format!("{w}×{h}"))
                                .unwrap_or_else(|| "—".into());
                            ui.label(
                                RichText::new(format!(
                                    "{dims} · {}",
                                    format_mtime(file.entry.modified_ms)
                                ))
                                .color(theme::tan())
                                .size(11.0),
                            );

                            if let Some(origin) = &file.entry.origin {
                                ui.label(
                                    RichText::new(format!("from {origin}"))
                                        .color(theme::lilac())
                                        .size(11.0),
                                );
                            }

                            self.audio_controls(ui, file, acts);

                            if is_best {
                                ui.label(
                                    RichText::new(format!("{} BEST", icon::STAR))
                                        .color(theme::blue())
                                        .size(12.0)
                                        .strong(),
                                );
                            }
                            if ro {
                                // A worse copy inside a protected repo can still be
                                // unlocked one file at a time, via its context menu
                                // or a long press (never a plain click).
                                let resp = ui
                                    .add(
                                        egui::Label::new(
                                            RichText::new("read-only")
                                                .color(theme::blue())
                                                .size(11.0),
                                        )
                                        .sense(egui::Sense::click()),
                                    )
                                    .explain(
                                        self.verbosity,
                                        "Right-click or long-press to unlock this file",
                                        "This file is protected by its repo's read-only lock, \
                                         so it can't be marked for deletion. Right-click or \
                                         long-press to unlock just this one file. Running FIND \
                                         again re-locks it.",
                                    );
                                if long_pressed(ui, &resp) {
                                    acts.push(Act::Unlock(k.clone()));
                                }
                                resp.context_menu(|ui| {
                                    if ui
                                        .button(format!("{} UNLOCK for deletion", icon::LOCK_OPEN))
                                        .explain(
                                            self.verbosity,
                                            "Unlock this file only",
                                            "Unlock just this file for deletion, without \
                                             unlocking the whole repo. Reset the next time \
                                             you run FIND.",
                                        )
                                        .clicked()
                                    {
                                        acts.push(Act::Unlock(k.clone()));
                                        ui.close();
                                    }
                                });
                            } else {
                                if unlocked {
                                    let resp = ui
                                .add(
                                    egui::Label::new(
                                        RichText::new(format!("{} unlocked", icon::LOCK_OPEN))
                                            .color(theme::red())
                                            .size(11.0),
                                    )
                                    .sense(egui::Sense::click()),
                                )
                                .explain(
                                    self.verbosity,
                                    "Read-only override for this file — right-click to re-lock",
                                    "This file was individually unlocked from its repo's \
                                     read-only protection. Right-click to re-lock it (or FIND \
                                     again, which resets all per-file unlocks).",
                                );
                                    resp.context_menu(|ui| {
                                        if ui
                                            .button(format!("{} RE-LOCK", icon::LOCK))
                                            .explain(
                                                self.verbosity,
                                                "Restore read-only protection",
                                                "Re-lock this file, restoring its repo's \
                                                 read-only protection and clearing any \
                                                 pending mark.",
                                            )
                                            .clicked()
                                        {
                                            acts.push(Act::Relock(k.clone()));
                                            ui.close();
                                        }
                                    });
                                }
                                let (label, fill) = if marked {
                                    (format!("{} DELETE", icon::CHECK), theme::red())
                                } else {
                                    ("KEEP".to_string(), theme::panel())
                                };
                                let color = if marked {
                                    theme::black()
                                } else {
                                    theme::text()
                                };
                                if ui
                                    .add(
                                        egui::Button::new(RichText::new(label).color(color))
                                            .fill(fill),
                                    )
                                    .explain(
                                        self.verbosity,
                                        "Toggle this copy's mark",
                                        "Toggle whether this copy is marked for deletion. \
                                         Nothing is deleted until you press DELETE MARKED (or \
                                         DELETE NOW under Quick Delete).",
                                    )
                                    .clicked()
                                {
                                    acts.push(Act::ToggleMark(k.clone()));
                                }
                            }
                        });
                    });
                // Full-fidelity escape hatch: hand the file to the system's
                // default app.
                card.response.context_menu(|ui| {
                    if ui
                        .button(format!("{} OPEN", icon::ARROW_RIGHT))
                        .explain(
                            self.verbosity,
                            "Open with the system default app",
                            "Hand this file to the operating system's default application \
                             for its type — the full-fidelity escape hatch for anything the \
                             in-app preview can't show.",
                        )
                        .clicked()
                    {
                        acts.push(Act::Open(file.absolute_path()));
                        ui.close();
                    }
                    if ui
                        .button(format!("{} SHOW IN FOLDER", icon::FOLDER_OPEN))
                        .explain(
                            self.verbosity,
                            "Reveal in the file manager",
                            "Open this file's containing folder in the system file manager.",
                        )
                        .clicked()
                    {
                        acts.push(Act::Reveal(file.absolute_path()));
                        ui.close();
                    }
                });
            });
    }

    /// Play/pause + seek bar for audio files. One file plays at a time; the
    /// controls reflect the global player and survive scrolling (the player is
    /// not per-card). No-op for non-audio files.
    fn audio_controls(&mut self, ui: &mut egui::Ui, file: &DupeFile, acts: &mut Vec<Act>) {
        let is_audio = file
            .entry
            .mime
            .as_deref()
            .is_some_and(dedup_core::fingerprint::is_audio_mime);
        if !is_audio {
            return;
        }
        let hex = hash_hex(&file.entry.hash);
        let total_ms = file
            .entry
            .audio
            .as_ref()
            .map(|a| a.duration_ms as u64)
            .unwrap_or(0);
        let snap = self.player.snapshot();
        let is_current = snap.loaded && snap.hex.as_deref() == Some(hex.as_str());
        let playing = is_current && snap.playing;

        ui.horizontal(|ui| {
            let label = if playing { "PAUSE" } else { "PLAY" };
            let fill = if playing {
                theme::amber()
            } else {
                theme::panel()
            };
            let col = if playing {
                theme::black()
            } else {
                theme::text()
            };
            if ui
                .add(egui::Button::new(RichText::new(label).color(col)).fill(fill))
                .explain(
                    self.verbosity,
                    "Play/pause this track",
                    "Play this file in the built-in preview player. Only one file plays at \
                     a time — starting another stops this one. Click again to pause/resume.",
                )
                .clicked()
            {
                acts.push(Act::PlayAudio(hex.clone(), file.absolute_path(), total_ms));
            }
            let (pos, total) = if is_current {
                (snap.pos_ms, snap.total_ms.max(total_ms))
            } else {
                (0, total_ms)
            };
            ui.label(
                RichText::new(format!("{} / {}", fmt_ms(pos), fmt_ms(total)))
                    .color(theme::tan())
                    .size(11.0),
            );
        });

        // Seek bar (only meaningful for the currently-loaded file).
        let total = snap.total_ms.max(total_ms);
        if is_current && total > 0 {
            let mut frac = (snap.pos_ms as f32 / total as f32).clamp(0.0, 1.0);
            if ui
                .add(egui::Slider::new(&mut frac, 0.0..=1.0).show_value(false))
                .explain(
                    self.verbosity,
                    "Seek",
                    "Drag to seek to a position in this track.",
                )
                .changed()
            {
                acts.push(Act::SeekAudio(frac));
            }
        }
    }

    fn thumbnail(
        &mut self,
        ui: &mut egui::Ui,
        gi: usize,
        fi: usize,
        file: &DupeFile,
        acts: &mut Vec<Act>,
    ) {
        // The shared media cell draws the image / video still / audio glyph /
        // placeholder; the card keeps the click meaning (open the lightbox) and
        // its own tooltip. A placeholder or not-yet-decoded thumbnail returns
        // `None` — but a *typed* placeholder (a document, an archive) still has
        // a Text representation to open, so the card makes the placeholder
        // itself the click target rather than leaving those groups with no way
        // into the lightbox at all.
        let facts = FileFacts::from_entry(&file.entry, file.absolute_path());
        let thumbs = &mut self.thumbs;
        let cell = ui.scope(|ui| media_cell(ui, thumbs, &facts, MediaStyle::card()));
        let resp = match cell.inner {
            Some(resp) => resp,
            None if has_text_representation(&facts) => {
                let hit = ui.interact(
                    cell.response.rect,
                    ui.id().with(("dupe-open", gi, fi)),
                    egui::Sense::click(),
                );
                // The placeholder is a picture of nothing, so name the target —
                // otherwise this click area has no accessible label at all.
                hit.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "OPEN PREVIEW")
                });
                hit.on_hover_cursor(egui::CursorIcon::PointingHand).explain(
                    self.verbosity,
                    "Open the text preview",
                    "Open the full-window lightbox: this file has no picture, so its Text tab \
                     shows the start of its contents — as text, or as hex when it is not text \
                     — beside the copy you compare it with.",
                )
            }
            None => return,
        };
        let resp = if facts.is_audio() {
            resp.explain(
                self.verbosity,
                "Open the audio lightbox",
                "Open the full-window audio view: compare this group's copies as waveforms and \
                 switch playback between them without losing your place in the track.",
            )
        } else {
            resp.explain(
                self.verbosity,
                "Click to open the lightbox",
                "Click to open the full-window lightbox: zoom, pan, step through this group's \
                 copies, and (for images) A/B compare against the best copy.",
            )
        };
        if resp.clicked() {
            acts.push(Act::OpenLightbox(gi, fi));
        }
    }

    /// The one shared viewer ([`crate::compare_view::DiffCompare`]), overlaid
    /// while open. The Duplicates tab supplies what is its own: the group as
    /// the pool, its player (one audio device — the cards behind the overlay
    /// share it), and its deletion marks as the per-side actions.
    fn viewer_modal(&mut self, ctx: &egui::Context, store: &Arc<Store>, acts: &mut Vec<Act>) {
        // Marks are resolved through `self` before the viewer is borrowed.
        let marks = self.lightbox.as_ref().map(|lb| {
            let mark = |side: &crate::compare_view::DiffSide| {
                let k = (side.repo.clone(), side.rel_path.clone());
                let markable = !self.repo_is_ro(&side.repo) || self.unlocked.contains(&k);
                crate::compare_view::MarkPill {
                    marked: self.marked.contains(&k),
                    markable,
                }
            };
            (mark(&lb.left), mark(&lb.right))
        });
        let verbosity = self.verbosity;
        let Some((l_mark, r_mark)) = marks else {
            return;
        };
        let Some(lb) = self.lightbox.as_mut() else {
            return;
        };
        lb.set_marks(Some(l_mark), Some(r_mark));
        match lb.view(ctx, verbosity, Some(&self.player)) {
            Some(crate::compare_view::DiffPick::ToggleMark { on_left }) => {
                let side = if on_left { &lb.left } else { &lb.right };
                acts.push(Act::ToggleMark((side.repo.clone(), side.rel_path.clone())));
            }
            Some(crate::compare_view::DiffPick::Edited { on_left }) => {
                // A file was rewritten in place, possibly keeping its
                // timestamp — the index must follow the bytes now, not wait
                // for a rescan that would skip an unchanged (size, mtime).
                let side = if on_left { &lb.left } else { &lb.right };
                let (repo, rel) = (side.repo.clone(), side.rel_path.clone());
                match dedup_core::update::refresh_file_entry(store, &repo, &rel) {
                    Ok(_) => {
                        self.status = Some(format!("Saved {rel}"));
                        // The cards show sizes from the loaded page — reload it.
                        self.cached_page = None;
                    }
                    Err(e) => {
                        self.error = Some(format!("Saved, but re-indexing failed: {e}"));
                    }
                }
            }
            Some(_) => {
                // Closing the viewer also silences what it was playing; the
                // cards' own playback (started outside it) is left alone.
                if lb.audio_active.is_some() {
                    self.player.stop();
                }
                self.lightbox = None;
            }
            None => {}
        }
    }

    fn confirm_modal(&mut self, ui: &mut egui::Ui, prompt: &str, verb: &str, acts: &mut Vec<Act>) {
        egui::Modal::new(Id::new("dupes-confirm")).show(&ui.ctx().clone(), |ui| {
            ui.set_width(360.0);
            ui.label(
                RichText::new("CONFIRM")
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
                        egui::Button::new(RichText::new(verb).color(theme::ink_on(theme::red())))
                            .fill(theme::red()),
                    )
                    .clicked()
                {
                    acts.push(Act::ConfirmDelete);
                }
                if ui
                    .button(RichText::new("CANCEL").color(theme::black()))
                    .clicked()
                {
                    acts.push(Act::CancelDelete);
                }
            });
        });
    }

    fn apply(&mut self, ctx: &egui::Context, store: &Arc<Store>, act: Act) {
        match act {
            Act::ToggleInclude(i) => {
                if let Some(r) = self.repos.get_mut(i) {
                    r.included = !r.included;
                }
            }
            Act::ToggleRo(i) => {
                if let Some(r) = self.repos.get_mut(i) {
                    r.read_only = !r.read_only;
                    if r.read_only {
                        let name = r.name.clone();
                        self.marked.retain(|(repo, _)| repo != &name);
                        // A repo turned read-only starts fully locked again.
                        self.unlocked.retain(|(repo, _)| repo != &name);
                    }
                }
            }
            Act::Find => self.start_find(store, ctx),
            Act::ToggleMark(k) => {
                if !self.marked.remove(&k) {
                    self.marked.insert(k);
                }
            }
            Act::Unlock(k) => {
                self.unlocked.insert(k);
            }
            Act::Relock(k) => {
                self.unlocked.remove(&k);
                self.marked.remove(&k);
            }
            Act::Open(path) => {
                if let Err(e) = crate::external::open(&path) {
                    self.error = Some(format!("Open failed: {e}"));
                }
            }
            Act::Reveal(path) => {
                if let Err(e) = crate::external::reveal(&path) {
                    self.error = Some(format!("Show in folder failed: {e}"));
                }
            }
            Act::OpenLightbox(gi, fi) => {
                // The clicked member opens alone (the second side hidden); the
                // whole group travels as the pool either side steps through.
                let page_start = self.cached_page.unwrap_or(0) * PAGE_SIZE;
                if let Some(group) = self
                    .page_groups
                    .get(gi.wrapping_sub(page_start))
                    .filter(|g| !g.is_empty())
                {
                    let side = |f: &DupeFile| crate::compare_view::DiffSide {
                        repo: f.repo.clone(),
                        rel_path: f.rel_path.clone(),
                        read_only: self.repo_is_ro(&f.repo),
                        facts: FileFacts::from_entry(&f.entry, f.absolute_path()),
                    };
                    let pool: Vec<crate::compare_view::DiffSide> = group.iter().map(side).collect();
                    let fi = fi.min(group.len() - 1);
                    let mut lb = crate::compare_view::DiffCompare::new_with_pool(
                        side(&group[fi]),
                        None,
                        pool,
                    );
                    lb.hide_second();
                    lb.set_title("COMPARE — DUPLICATE GROUP");
                    self.lightbox = Some(lb);
                }
            }
            Act::PlayAudio(hex, path, total_ms) => {
                let snap = self.player.snapshot();
                // Clicking the playing file toggles pause; another file starts it.
                if snap.loaded && snap.hex.as_deref() == Some(hex.as_str()) {
                    self.player.toggle_pause();
                } else {
                    // Switching between a group's copies keeps the current
                    // offset, so you hear the same moment in each — the openings
                    // are byte-identical, so any difference is later in the track.
                    let start_ms = if snap.loaded { snap.pos_ms } else { 0 };
                    self.player.play(&hex, &path, total_ms, start_ms);
                }
            }
            Act::SeekAudio(f) => self.player.seek_fraction(f),
            Act::ToggleQuickDelete => {
                if self.quick_delete {
                    self.quick_delete = false;
                } else {
                    self.confirm = Some((
                        "Quick Delete removes a group's marked files immediately, \
                         with no further confirmation. Enable?"
                            .into(),
                        ConfirmAction::EnableQuickDelete,
                    ));
                }
            }
            Act::AutoResolve => self.start_auto_resolve(store, ctx),
            Act::DeleteGroup(gi) => self.delete_group(store, ctx, gi),
            Act::MarkGroup(gi) => self.set_group_mark(gi, true),
            Act::UnmarkGroup(gi) => self.set_group_mark(gi, false),
            Act::HideGroup(gi) => {
                self.hidden.insert(gi);
            }
            Act::SetPage(p) => self.page = p,
            Act::AskDelete => {
                let n = self.marked.len();
                if n > 0 {
                    let mut prompt = format!(
                        "Delete {n} marked file{} from disk? This cannot be undone.",
                        if n == 1 { "" } else { "s" }
                    );
                    // Tiered safety: warn (don't block) when a delete would
                    // leave some content surviving only inside an archive — a
                    // weaker tier that needs extraction (and maybe a password)
                    // to read, and may itself be deleted later.
                    let survivors = self.archive_only_survivors();
                    if !survivors.is_empty() {
                        prompt.push_str(&format!(
                            "\n\n⚠ {} file(s) will then survive only inside an archive \
                             (extract to keep a loose copy): {}",
                            survivors.len(),
                            survivors.join(", "),
                        ));
                    }
                    self.confirm = Some((prompt, ConfirmAction::DeleteAll));
                }
            }
            Act::CancelDelete => self.confirm = None,
            Act::ConfirmDelete => {
                if let Some((_, action)) = self.confirm.take() {
                    match action {
                        ConfirmAction::EnableQuickDelete => self.quick_delete = true,
                        ConfirmAction::DeleteAll => {
                            let keys: Vec<FileKey> = self.marked.iter().cloned().collect();
                            self.start_delete(store, ctx, keys, DeleteFollow::Refind);
                        }
                    }
                }
            }
        }
    }

    /// Delete one group's marked files immediately (Quick Delete path).
    fn delete_group(&mut self, store: &Arc<Store>, ctx: &egui::Context, gi: usize) {
        let keys: Vec<FileKey> = {
            let Some(page) = self.cached_page else { return };
            let page_start = page * PAGE_SIZE;
            let Some(group) = self.page_groups.get(gi.wrapping_sub(page_start)) else {
                return;
            };
            group
                .iter()
                .map(key)
                .filter(|k| self.marked.contains(k))
                .collect()
        };
        self.start_delete(store, ctx, keys, DeleteFollow::Resolve(gi));
    }

    /// MARK ALL / MARK NONE for a group: set (`mark`) or clear the deletion mark
    /// on its files. MARK ALL only touches *markable* copies — protected
    /// (read-only, not individually unlocked) copies are never marked.
    fn set_group_mark(&mut self, gi: usize, mark: bool) {
        let keys: Vec<FileKey> = {
            let Some(page) = self.cached_page else { return };
            let page_start = page * PAGE_SIZE;
            let Some(group) = self.page_groups.get(gi.wrapping_sub(page_start)) else {
                return;
            };
            if mark {
                group
                    .iter()
                    .filter(|f| !self.repo_is_ro(&f.repo) || self.unlocked.contains(&key(f)))
                    .map(key)
                    .collect()
            } else {
                group.iter().map(key).collect()
            }
        };
        for k in keys {
            if mark {
                self.marked.insert(k);
            } else {
                self.marked.remove(&k);
            }
        }
    }

    fn included_names(&self) -> Vec<String> {
        self.repos
            .iter()
            .filter(|r| r.included)
            .map(|r| r.name.clone())
            .collect()
    }

    /// The repo whose MIME/tag pick-lists back the FILTER wizard: the first
    /// included repo (the wizard stays usable-but-unassisted when none is).
    fn suggestion_repo(&self) -> Option<String> {
        self.included_names().into_iter().next()
    }

    fn read_only_names(&self) -> HashSet<String> {
        self.repos
            .iter()
            .filter(|r| r.read_only)
            .map(|r| r.name.clone())
            .collect()
    }

    /// Run the search on a background thread; results/progress arrive via `rx`.
    fn start_find(&mut self, store: &Arc<Store>, ctx: &egui::Context) {
        if self.busy.is_some() {
            return;
        }
        let names = self.included_names();
        if names.is_empty() {
            self.error = Some("Select at least one repo.".into());
            return;
        }
        // Parse the FILTER once, up front, so a bad expression is reported here
        // rather than on the worker thread. `None` (no conditions) skips the
        // per-member matching entirely.
        let filter_str = self.filter.filter_string();
        let has_filter = filter_str.is_some();
        let filter = match FileFilter::parse(filter_str.as_deref()) {
            Ok(f) => f,
            Err(e) => {
                self.error = Some(e.to_string());
                return;
            }
        };
        self.result_names = names.clone();
        self.busy = Some(Op::Find(0));
        self.status = None;
        self.error = None;
        let store = Arc::clone(store);
        let tx = self.tx.clone();
        let mode = self.mode;
        let threshold = self.threshold;
        let repaint = ctx.clone();
        std::thread::spawn(move || {
            let fref = has_filter.then_some(&filter);
            let result = match mode {
                Mode::Exact => {
                    let tx2 = tx.clone();
                    let r = repaint.clone();
                    plan_exact_duplicates(&store, &names, move |n| {
                        let _ = tx2.send(Msg::FindProgress(n));
                        r.request_repaint();
                    })
                    .and_then(|plan| {
                        // Keep only groups with ≥1 matching member (streams each
                        // key's members; reports keys examined as progress).
                        let tx2 = tx.clone();
                        let r = repaint.clone();
                        retain_matching_keys(&store, &names, plan, fref, move |n| {
                            let _ = tx2.send(Msg::FindProgress(n));
                            r.request_repaint();
                        })
                    })
                    .map(Results::Exact)
                    .map_err(|e| e.to_string())
                }
                Mode::Similar => find_similar(&store, &names, threshold, fref)
                    .map(Results::Similar)
                    .map_err(|e| e.to_string()),
            };
            let _ = tx.send(Msg::FindDone(result));
            repaint.request_repaint();
        });
    }

    /// Mark every non-best copy in a non-read-only repo. Similar results are in
    /// memory (marked inline); exact streams the plan on a background thread.
    fn start_auto_resolve(&mut self, store: &Arc<Store>, ctx: &egui::Context) {
        if self.busy.is_some() {
            return;
        }
        let ro = self.read_only_names();
        match &self.results {
            Some(Results::Similar(groups)) => {
                for group in groups {
                    for file in group.iter().skip(1) {
                        if !ro.contains(&file.repo) {
                            self.marked.insert(key(file));
                        }
                    }
                }
                self.status = Some(format!("{} marked for deletion", self.marked.len()));
            }
            Some(Results::Exact(plan)) => {
                let plan = plan.clone();
                let names = self.result_names.clone();
                let total = plan.len();
                self.busy = Some(Op::AutoResolve { done: 0, total });
                let store = Arc::clone(store);
                let tx = self.tx.clone();
                let repaint = ctx.clone();
                std::thread::spawn(move || {
                    let mut marks: Vec<FileKey> = Vec::new();
                    let mut done = 0;
                    for chunk in plan.chunks(AUTO_BATCH) {
                        match load_groups(&store, &names, chunk) {
                            Ok(groups) => {
                                for group in &groups {
                                    for file in group.iter().skip(1) {
                                        if !ro.contains(&file.repo) {
                                            marks.push((file.repo.clone(), file.rel_path.clone()));
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                let _ = tx.send(Msg::AutoDone(Err(e.to_string())));
                                repaint.request_repaint();
                                return;
                            }
                        }
                        done += chunk.len();
                        let _ = tx.send(Msg::AutoProgress { done, total });
                        repaint.request_repaint();
                    }
                    let _ = tx.send(Msg::AutoDone(Ok(marks)));
                    repaint.request_repaint();
                });
            }
            None => {}
        }
    }

    /// Delete `keys` on a background thread; `follow` decides the aftermath.
    fn start_delete(
        &mut self,
        store: &Arc<Store>,
        ctx: &egui::Context,
        keys: Vec<FileKey>,
        follow: DeleteFollow,
    ) {
        if self.busy.is_some() || keys.is_empty() {
            return;
        }
        self.delete_batch = keys.clone();
        self.busy = Some(Op::Delete);
        let store = Arc::clone(store);
        let tx = self.tx.clone();
        let repaint = ctx.clone();
        std::thread::spawn(move || {
            let result = delete_paths(&store, &keys).map_err(|e| e.to_string());
            let _ = tx.send(Msg::DeleteDone(result, follow));
            repaint.request_repaint();
        });
    }

    fn repo_is_ro(&self, name: &str) -> bool {
        self.repos.iter().any(|r| r.name == name && r.read_only)
    }

    /// Contents (named by a loose file's path) that a delete of the marked set
    /// would leave surviving *only* inside an archive: a loaded group whose
    /// every copy is marked, and whose content is present in an archive. Used
    /// for the tiered delete-safety warning. Best-effort over the loaded pages.
    fn archive_only_survivors(&self) -> Vec<String> {
        let mut out = Vec::new();
        for g in &self.page_groups {
            let Some(first) = g.first() else { continue };
            // Every copy of this content marked ⇒ no loose copy would survive.
            if !g.iter().all(|f| self.marked.contains(&key(f))) {
                continue;
            }
            if let Some(occ) = self
                .archive_evidence
                .get(&(first.entry.size, first.entry.hash))
                .filter(|v| !v.is_empty())
            {
                out.push(format!("{} (in {})", first.rel_path, occ[0].archive_rel));
            }
        }
        out
    }
}

#[cfg(test)]
mod ui_tests {
    use super::*;
    use crate::id3tags::Tags;
    use dedup_core::store::Store;
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;
    use std::path::Path;
    use tempfile::TempDir;

    const SAMPLE_REPOS: [&str; 5] = [
        "Automatic Upload",
        "Videos",
        "data",
        "entertainment_media",
        "private_media",
    ];

    /// A temp store pre-populated with `names` as (empty) repos.
    fn sample_store(names: &[&str]) -> (TempDir, Arc<Store>) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open_at(tmp.path().join("cfg")).unwrap();
        for n in names {
            let dir = tmp.path().join(n);
            std::fs::create_dir_all(&dir).unwrap();
            store.create_repo(n, &dir.to_string_lossy()).unwrap();
        }
        (tmp, Arc::new(store))
    }

    /// Build a driven harness showing the Duplicates view for `store`. The
    /// closure owns `view`/`store`; the theme + icon font are installed once so
    /// glyph metrics match the real app.
    fn dupes_harness<'a>(store: Arc<Store>) -> Harness<'a> {
        let mut view = DupesView::new();
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1120.0, 360.0))
            .build_ui(move |ui| {
                if !init {
                    crate::icon::install(ui.ctx());
                    crate::theme::apply(ui.ctx(), crate::theme::DARK);
                    init = true;
                }
                view.show(ui, &store, TooltipVerbosity::default());
            });
        harness.run();
        harness
    }

    /// Regression test for the recurring "first repo sits higher" bug: every
    /// repo chip on a row must share one top edge.
    #[test]
    fn repo_row_is_aligned() {
        let (_tmp, store) = sample_store(&SAMPLE_REPOS);
        let harness = dupes_harness(store);

        let tops: Vec<f32> = SAMPLE_REPOS
            .iter()
            .map(|n| harness.get_by_label(n).rect().top())
            .collect();
        let base = tops[0];
        for (name, top) in SAMPLE_REPOS.iter().zip(&tops) {
            assert!(
                (top - base).abs() < 0.75,
                "repo '{name}' top {top} != first repo top {base} — row misaligned (tops: {tops:?})"
            );
        }
    }

    /// Auto-refresh: re-syncing the repo list keeps existing repos' include and
    /// read-only state, adds newly-registered repos with the default (excluded +
    /// locked), so a repo added elsewhere shows up without resetting choices.
    #[test]
    fn sync_repos_preserves_state_and_adds_new() {
        let (tmp, store) = sample_store(&["A", "B"]);
        let mut view = DupesView::new();
        view.sync_repos(&store);
        assert_eq!(
            view.repos
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
            ["A", "B"]
        );
        assert!(
            view.repos.iter().all(|r| !r.included && r.read_only),
            "repos default to excluded + read-only"
        );
        // The user includes A and unlocks it (a non-default state to preserve).
        let a = view.repos.iter_mut().find(|r| r.name == "A").unwrap();
        a.included = true;
        a.read_only = false;
        // A new repo C is registered, then the tab is re-synced.
        let dir = tmp.path().join("C");
        std::fs::create_dir_all(&dir).unwrap();
        store.create_repo("C", &dir.to_string_lossy()).unwrap();
        view.sync_repos(&store);
        let get = |n: &str| {
            let r = view.repos.iter().find(|r| r.name == n).unwrap();
            (r.included, r.read_only)
        };
        assert_eq!(
            get("A"),
            (true, false),
            "A's include + unlock survive the re-sync"
        );
        assert_eq!(get("B"), (false, true), "B keeps the default");
        assert_eq!(
            get("C"),
            (false, true),
            "the new repo C appears with the default (excluded + locked)"
        );
    }

    /// MARK ALL / MARK NONE bulk-toggle every repo's include state (repos start
    /// excluded by default).
    #[test]
    fn mark_all_none_toggles_include() {
        let (_tmp, store) = sample_store(&SAMPLE_REPOS);
        let store_ui = Arc::clone(&store);
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1120.0, 360.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    view.show(ui, &store_ui, TooltipVerbosity::default());
                },
                DupesView::new(),
            );
        harness.run();
        assert!(
            harness.state().repos.iter().all(|r| !r.included),
            "repos start excluded by default"
        );
        harness.get_by_label("ALL").click();
        harness.run();
        assert!(
            harness.state().repos.iter().all(|r| r.included),
            "MARK ALL includes every repo"
        );
        harness.get_by_label("NONE").click();
        harness.run();
        assert!(
            harness.state().repos.iter().all(|r| !r.included),
            "MARK NONE excludes every repo"
        );
    }

    /// Many repos in a narrow window must **wrap** onto several rows (rather
    /// than scroll off-screen), and each wrapped row must stay top-aligned.
    #[test]
    fn repo_chips_wrap_when_narrow() {
        let names: Vec<String> = (0..12).map(|i| format!("repo{i:02}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let (_tmp, store) = sample_store(&refs);

        let mut view = DupesView::new();
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(420.0, 500.0))
            .build_ui(move |ui| {
                if !init {
                    crate::icon::install(ui.ctx());
                    crate::theme::apply(ui.ctx(), crate::theme::DARK);
                    init = true;
                }
                view.show(ui, &store, TooltipVerbosity::default());
            });
        harness.run();

        let tops: Vec<f32> = names
            .iter()
            .map(|n| harness.get_by_label(n).rect().top())
            .collect();

        // Cluster tops into rows (0.75 px tolerance, matching the alignment
        // test). Wrapping means more than one row; top-alignment means the
        // first row holds several chips that share a top edge.
        let mut rows: Vec<f32> = Vec::new();
        for &t in &tops {
            if !rows.iter().any(|&r| (r - t).abs() < 0.75) {
                rows.push(t);
            }
        }
        assert!(
            rows.len() >= 2,
            "repo chips did not wrap: all {} chips share one row (tops: {tops:?})",
            tops.len()
        );
        let first = tops.iter().cloned().fold(f32::INFINITY, f32::min);
        let first_row = tops.iter().filter(|&&t| (t - first).abs() < 0.75).count();
        assert!(
            first_row >= 2,
            "first wrapped row is not top-aligned: only {first_row} chip(s) at top {first} (tops: {tops:?})"
        );
    }

    /// Guards against the REPOS section expanding to fill the viewport (a real
    /// regression we hit): the MODE row's FIND button must stay near the top,
    /// not be pushed hundreds of px down by an over-tall section above it.
    #[test]
    fn sections_stay_compact() {
        let (_tmp, store) = sample_store(&SAMPLE_REPOS);
        let harness = dupes_harness(store);
        // The FILTER wizard sits directly below the REPOS section, so its top
        // reflects the REPOS height — a good guard against an expanded repos bar
        // (the FIND button now sits below FILTER, so it's no longer a tight
        // proxy for the repos height). The REPOS section is the MARK ALL/NONE
        // header line plus one chip row here; an over-expansion regression pushed
        // it hundreds of px down, so a ~200px ceiling still catches that.
        let filter_top = harness.get_by_label_contains("FILTER — ").rect().top();
        assert!(
            filter_top < 200.0,
            "FILTER section at y={filter_top}; the REPOS section is too tall (expanded?)"
        );
    }

    /// The shared FILTER wizard is present on the Duplicates tab (its FILTER
    /// label and the `+` add-condition button), so FIND can be narrowed.
    #[test]
    fn filter_wizard_is_present() {
        let (_tmp, store) = sample_store(&SAMPLE_REPOS);
        let harness = dupes_harness(store);
        assert!(
            harness.query_by_label_contains("FILTER — ").is_some(),
            "the shared FILTER wizard should render on the Duplicates tab"
        );
        assert!(
            harness.query_by_label("+").is_some(),
            "the FILTER wizard's add-condition button should be present"
        );
    }

    /// In SIMILAR mode a threshold control appears in the MODE row. It must not
    /// drift the row vertically: the DUPLICATES and FIND buttons stay aligned.
    #[test]
    fn similar_mode_row_is_aligned() {
        let (_tmp, store) = sample_store(&SAMPLE_REPOS);
        let mut view = DupesView::new();
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1120.0, 360.0))
            .build_ui(move |ui| {
                if !init {
                    crate::icon::install(ui.ctx());
                    crate::theme::apply(ui.ctx(), crate::theme::DARK);
                    init = true;
                }
                view.show(ui, &store, TooltipVerbosity::default());
            });
        harness.run();
        harness.get_by_label("SIMILAR").click();
        harness.run();

        let dup_top = harness.get_by_label("DUPLICATES").rect().top();
        let find_top = harness
            .get_by_label(&format!("{} FIND", icon::SEARCH))
            .rect()
            .top();
        assert!(
            (dup_top - find_top).abs() < 0.75,
            "SIMILAR row misaligned: DUPLICATES top {dup_top} vs FIND top {find_top}"
        );
    }

    /// A duplicate file with a chosen content identity (so two can share one).
    fn content_file(rel: &str, hash0: u8) -> DupeFile {
        let mut hash = [0u8; 32];
        hash[0] = hash0;
        DupeFile {
            repo: "r".into(),
            repo_root: "/nonexistent-dedup-test".into(),
            rel_path: rel.into(),
            entry: dedup_core::store::FileEntry {
                size: 500,
                hash,
                modified_ms: 0,
                missing: false,
                mime: Some("image/png".into()),
                img_fingerprint: None,
                video_hash: None,
                pdf_hash: None,
                audio: None,
                img_size: Some((10, 10)),
                origin: None,
                exif: None,
            },
        }
    }

    fn occ(archive_rel: &str, member: &str) -> dedup_core::archive::ArchiveOccurrence {
        dedup_core::archive::ArchiveOccurrence {
            repo: "r".into(),
            archive_rel: archive_rel.into(),
            member_name: member.into(),
        }
    }

    /// A duplicate group whose content also lives inside an archive shows a
    /// read-only evidence row naming that archive.
    #[test]
    fn archive_evidence_rows_name_the_containing_zip() {
        let group: DupeGroup = vec![content_file("a.png", 7), content_file("b.png", 7)];
        let ck = (group[0].entry.size, group[0].entry.hash);
        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.results = Some(Results::Similar(vec![group]));
        view.archive_evidence
            .insert(ck, vec![occ("backup_2019.zip", "a.png")]);

        let (_t, store) = sample_store(&[]);
        let mut init = false;
        let mut h = Harness::builder()
            .with_size(egui::vec2(1000.0, 700.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        h.run();
        assert!(
            h.query_all_by_label_contains("backup_2019.zip").count() > 0,
            "the evidence row names the archive that contains this content"
        );
        assert!(
            h.query_all_by_label_contains("in archive").count() > 0,
            "and marks it as living inside an archive"
        );
    }

    /// Deleting every loose copy of content that also lives in an archive warns
    /// (does not block): the content would survive only inside the archive.
    #[test]
    fn deleting_all_loose_copies_warns_when_content_survives_only_in_an_archive() {
        let group: DupeGroup = vec![content_file("a.png", 9), content_file("b.png", 9)];
        let ck = (group[0].entry.size, group[0].entry.hash);
        let (ka, kb) = (key(&group[0]), key(&group[1]));
        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.results = Some(Results::Similar(vec![group]));
        view.archive_evidence
            .insert(ck, vec![occ("backup.zip", "a.png")]);
        view.marked.insert(ka);
        view.marked.insert(kb);

        let (_t, store) = sample_store(&[]);
        let store2 = Arc::clone(&store);
        let mut init = false;
        let mut h = Harness::builder()
            .with_size(egui::vec2(1000.0, 700.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        h.run(); // populates the page's materialized groups

        let ctx = egui::Context::default();
        h.state_mut().apply(&ctx, &store2, Act::AskDelete);
        let prompt = h
            .state()
            .confirm
            .as_ref()
            .map(|(p, _)| p.clone())
            .unwrap_or_default();
        assert!(
            prompt.contains("survive only inside an archive"),
            "the delete is warned, not blocked: {prompt:?}"
        );
        assert!(prompt.contains("a.png"), "and names the file: {prompt:?}");
    }

    /// A dummy image-type duplicate file with a unique hash (→ unique thumbnail).
    fn image_file(i: usize) -> DupeFile {
        let mut hash = [0u8; 32];
        hash[0] = i as u8;
        hash[1] = (i >> 8) as u8;
        DupeFile {
            repo: "r".into(),
            repo_root: "/nonexistent-dedup-test".into(),
            rel_path: format!("img{i}.png"),
            entry: dedup_core::store::FileEntry {
                size: 1000,
                hash,
                modified_ms: 0,
                missing: false,
                mime: Some("image/png".into()),
                img_fingerprint: None,
                video_hash: None,
                pdf_hash: None,
                audio: None,
                img_size: Some((100, 100)),
                origin: None,
                exif: None,
            },
        }
    }

    /// Regression test for the SIMILAR results melting the CPU: the results list
    /// isn't virtualized, so a page can lay out far more thumbnails than the
    /// texture cache holds. Requesting them all every frame thrashes the LRU and
    /// spins repaints. The fix only fetches textures for on-screen cards — so
    /// with many off-screen cards, only the visible handful should be requested.
    #[test]
    fn offscreen_thumbnails_are_not_requested() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let total = 40usize;
        let groups: Vec<DupeGroup> = (0..total).map(|i| vec![image_file(i)]).collect();

        let mut view = DupesView::new();
        view.repos_loaded = true; // no repos needed; render fabricated groups
        view.results = Some(Results::Similar(groups));

        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(600.0, 520.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();

        let sent = harness.state().thumbs.requests_sent();
        assert!(sent > 0, "expected on-screen cards to request thumbnails");
        assert!(
            sent <= 8,
            "requested {sent} of {total} thumbnails in a 400px viewport; \
             off-screen cards should be skipped (not virtualized → cache thrash)"
        );
    }

    /// Off-screen groups must not be laid out at all (virtualization): their
    /// widgets should be absent from the tree, so rendering cost tracks the
    /// visible groups rather than the whole page.
    #[test]
    fn offscreen_groups_are_virtualized() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let groups: Vec<DupeGroup> = (0..40).map(|i| vec![image_file(i)]).collect();

        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.results = Some(Results::Similar(groups));

        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(600.0, 520.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        // First frame renders all to measure heights; later frames virtualize.
        harness.run();
        harness.run();

        // A top group is on-screen and rendered; a bottom group is off-screen
        // and must have been skipped (its file label is absent from the tree).
        assert!(
            harness.query_by_label("img0.png").is_some(),
            "top group should be rendered"
        );
        assert!(
            harness.query_by_label("img39.png").is_none(),
            "bottom group should be virtualized (not laid out)"
        );
    }

    /// Seed `repo` with `n` distinct 2-member exact-duplicate groups (metadata
    /// only — no files on disk, which plan/load don't need). Batched into one
    /// write transaction so seeding many groups stays fast.
    fn seed_groups(store: &Store, n: usize) {
        let mut entries: Vec<(String, dedup_core::store::FileEntry)> = Vec::with_capacity(n * 2);
        for g in 0..n {
            let mut hash = [0u8; 32];
            hash[0] = g as u8;
            hash[1] = (g >> 8) as u8;
            let e = dedup_core::store::FileEntry {
                size: 1000,
                hash,
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
            for c in 0..2 {
                entries.push((format!("g{g}_c{c}.bin"), e.clone()));
            }
        }
        let db = store.open_repo_db("repo").unwrap();
        dedup_core::store::apply_entries(&db, entries.iter().map(|(p, e)| (p.as_str(), e)))
            .unwrap();
    }

    fn seeded_store(n: usize) -> (TempDir, Arc<Store>) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open_at(tmp.path().join("cfg")).unwrap();
        let root = tmp.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        store.create_repo("repo", &root.to_string_lossy()).unwrap();
        seed_groups(&store, n);
        (tmp, Arc::new(store))
    }

    /// A real repo of exact-duplicate groups built from doc media, scanned so
    /// entries carry genuine hashes/mime/thumbnails — the duplicates cards then
    /// show real photos, not placeholders. The headline pair is the identical
    /// contract PDF hiding among the photos. Falls back to `None` when no doc
    /// media is configured, so callers keep their synthetic path.
    fn seeded_media_store() -> Option<(TempDir, Arc<Store>)> {
        if !crate::doc_media::available() {
            return None;
        }
        let tmp = tempfile::tempdir().unwrap();
        dedup_core::thumbnail::set_cache_dir(tmp.path().join("thumbs"));
        let store = Store::open_at(tmp.path().join("cfg")).unwrap();
        let root = tmp.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        // Each pair is one source copied under two names → one exact-dup group.
        let pairs: [(&str, &str, &str); 4] = [
            (
                "IMG_2019_field.jpg",
                "IMG_2019_field.jpg",
                "IMG_2019_field (1).jpg",
            ),
            (
                "wallpaper_spacehulk.jpg",
                "wallpaper_spacehulk.jpg",
                "spacehulk_backup.jpg",
            ),
            (
                "visa_contract.pdf",
                "Vertragsangebot.pdf",
                "Vertragsangebot (1).pdf",
            ),
            ("mewtwo.png", "mewtwo.png", "mewtwo_copy.png"),
        ];
        let mut any = false;
        for (asset, a, b) in pairs {
            if crate::doc_media::place(asset, &root.join(a)) {
                crate::doc_media::place(asset, &root.join(b));
                any = true;
            }
        }
        if !any {
            return None;
        }
        store
            .create_repo("Automatic Upload", &root.to_string_lossy())
            .unwrap();
        dedup_core::update::update_repo(
            &store,
            "Automatic Upload",
            2,
            &dedup_core::update::NoProgress,
            &dedup_core::update::CancellationToken::new(),
        )
        .unwrap();
        Some((tmp, Arc::new(store)))
    }

    /// The whole point of the redesign: with many exact-duplicate groups, only
    /// the current page's members are materialized in memory (the rest stay as
    /// lightweight plan descriptors).
    #[test]
    fn exact_results_load_only_the_current_page() {
        let (_tmp, store) = seeded_store(120);
        let plan = plan_exact_duplicates(&store, &["repo".to_string()], |_| {}).unwrap();
        assert_eq!(plan.len(), 120);

        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.results = Some(Results::Exact(plan));
        view.result_names = vec!["repo".to_string()];

        let store_ui = Arc::clone(&store);
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(600.0, 520.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
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

        let loaded = harness.state().page_groups.len();
        assert!(loaded > 0, "current page should be materialized");
        assert!(
            loaded <= PAGE_SIZE,
            "only one page of {PAGE_SIZE} groups should be in memory, got {loaded}"
        );
        assert_eq!(harness.state().total_groups(), 120);
    }

    /// FIND runs on a background thread and its result lands via the channel.
    #[test]
    fn find_runs_async_and_populates_results() {
        let (_tmp, store) = seeded_store(5);
        let store_ui = Arc::clone(&store);
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(700.0, 400.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    view.show(ui, &store_ui, TooltipVerbosity::default());
                },
                DupesView::new(),
            );
        harness.run(); // sync_repos (all excluded by default), initial render
        // Repos start excluded; opt them all in before searching.
        harness
            .state_mut()
            .repos
            .iter_mut()
            .for_each(|r| r.included = true);

        harness
            .get_by_label(&format!("{} FIND", icon::SEARCH))
            .click();

        // Use `step` (single frame) not `run`: the spinner requests continuous
        // repaints while the op is in flight, which trips `run`'s step cap.
        let mut done = false;
        for _ in 0..200 {
            harness.step();
            if harness.state().results.is_some() {
                done = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            done,
            "async FIND never populated results (error={:?})",
            harness.state().error
        );
        assert_eq!(harness.state().total_groups(), 5);
        assert!(harness.state().busy.is_none(), "op should have settled");
    }

    /// FIND respects the FILTER: with `name:g0_` set, only group 0 survives.
    #[test]
    fn find_applies_the_filter() {
        let (_tmp, store) = seeded_store(5);
        let store_ui = Arc::clone(&store);
        let mut view = DupesView::new();
        // seed_groups names copies g{g}_c{c}.bin, so this matches only group 0.
        view.filter.set_expression("name:g0_");
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(700.0, 400.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    view.show(ui, &store_ui, TooltipVerbosity::default());
                },
                view,
            );
        // The FILTER's live match-count keeps requesting repaints, so `run`
        // (step-capped) would overflow — step manually to load repos.
        for _ in 0..5 {
            harness.step();
        }
        // Repos start excluded; opt them all in before searching.
        harness
            .state_mut()
            .repos
            .iter_mut()
            .for_each(|r| r.included = true);

        harness
            .get_by_label(&format!("{} FIND", icon::SEARCH))
            .click();
        let mut done = false;
        for _ in 0..200 {
            harness.step();
            if harness.state().results.is_some() {
                done = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            done,
            "async FIND never populated (error={:?})",
            harness.state().error
        );
        assert_eq!(
            harness.state().total_groups(),
            1,
            "the filter should keep only group 0 (down from 5)"
        );
    }

    /// A duplicate file in `repo` with rel path `rel` (metadata only).
    fn dfile(repo: &str, rel: &str) -> DupeFile {
        DupeFile {
            repo: repo.into(),
            repo_root: "/nonexistent-dedup-test".into(),
            rel_path: rel.into(),
            entry: dedup_core::store::FileEntry {
                size: 10,
                hash: [0u8; 32],
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
            },
        }
    }

    fn similar_harness<'a>(view: DupesView) -> Harness<'a, DupesView> {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(700.0, 500.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    // keep tmp alive for the store's lifetime
                    let _ = &tmp;
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();
        harness
    }

    /// Worse (non-best) copies are marked DELETE by default, but never in a
    /// read-only repo (fixes the "everything is KEEP" regression).
    #[test]
    fn default_marks_worse_copies_excluding_read_only() {
        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.repos = vec![
            RepoSel {
                name: "w".into(),
                included: true,
                read_only: false,
                is_main: false,
            },
            RepoSel {
                name: "ro".into(),
                included: true,
                read_only: true,
                is_main: false,
            },
        ];
        view.results = Some(Results::Similar(vec![
            vec![dfile("w", "a"), dfile("w", "b")], // worse "b" is writable → marked
            vec![dfile("w", "c"), dfile("ro", "d")], // worse "d" is read-only → not marked
        ]));

        let harness = similar_harness(view);
        let m = &harness.state().marked;
        assert!(
            m.contains(&("w".into(), "b".into())),
            "worse writable copy marked"
        );
        assert!(
            !m.contains(&("ro".into(), "d".into())),
            "read-only copy is never marked"
        );
        assert!(!m.contains(&("w".into(), "a".into())), "best copy is kept");
    }

    /// The group-level bulk buttons: MARK NONE clears the group, MARK ALL marks
    /// every *writable* copy (never the read-only one), and HIDE dismisses the
    /// whole group from the list.
    #[test]
    fn group_bulk_buttons_mark_and_hide() {
        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.repos = vec![
            RepoSel {
                name: "w".into(),
                included: true,
                read_only: false,
                is_main: false,
            },
            RepoSel {
                name: "ro".into(),
                included: true,
                read_only: true,
                is_main: false,
            },
        ];
        view.results = Some(Results::Similar(vec![vec![
            dfile("w", "a"),
            dfile("w", "b"),
            dfile("ro", "c"),
        ]]));
        // Taller than `similar_harness` so the group header buttons clear the
        // (now taller) LCARS section chrome and are clickable.
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(700.0, 760.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    let _ = &tmp;
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();

        harness.get_by_label("MARK NONE").click();
        harness.run();
        assert!(
            harness.state().marked.is_empty(),
            "MARK NONE clears every mark in the group"
        );

        harness.get_by_label("MARK ALL").click();
        harness.run();
        let m = &harness.state().marked;
        assert!(
            m.contains(&("w".into(), "a".into())) && m.contains(&("w".into(), "b".into())),
            "MARK ALL marks every writable copy (including the best)"
        );
        assert!(
            !m.contains(&("ro".into(), "c".into())),
            "MARK ALL never marks a protected read-only copy"
        );

        harness.get_by_label("HIDE").click();
        harness.run();
        assert!(
            harness.state().hidden.contains(&0),
            "HIDE dismisses the group"
        );
        assert!(
            harness.query_by_label("MARK ALL").is_none(),
            "a hidden group renders no card (and no buttons)"
        );
    }

    /// A per-file unlock lets one read-only copy be marked: the locked card
    /// shows only the "read-only" badge, while the unlocked card gets the
    /// "unlocked" badge plus a working KEEP/DELETE toggle.
    #[test]
    fn unlocking_a_read_only_file_makes_it_markable() {
        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.repos = vec![RepoSel {
            name: "ro".into(),
            included: true,
            read_only: true,
            is_main: false,
        }];
        view.results = Some(Results::Similar(vec![vec![
            dfile("ro", "best"),
            dfile("ro", "worse"),
        ]]));
        view.unlocked.insert(("ro".into(), "worse".into()));

        // Taller than `similar_harness`: the KEEP toggle sits near the bottom
        // of the card and pointer clicks need it inside the viewport.
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(700.0, 900.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    let _ = &tmp;
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();
        assert!(
            harness.query_by_label("read-only").is_some(),
            "locked copy still shows the read-only badge"
        );
        assert!(
            harness
                .query_by_label(&format!("{} unlocked", icon::LOCK_OPEN))
                .is_some(),
            "unlocked copy shows the unlocked badge"
        );

        // Exactly one KEEP toggle (the locked best has none); clicking marks it.
        harness.get_by_label("KEEP").click();
        let mut marked = false;
        for _ in 0..50 {
            harness.step();
            if harness
                .state()
                .marked
                .contains(&("ro".into(), "worse".into()))
            {
                marked = true;
                break;
            }
        }
        assert!(marked, "unlocked copy can be marked for deletion");
    }

    /// The real interaction: right-clicking the read-only badge opens a
    /// context menu whose UNLOCK entry lifts the per-file lock.
    #[test]
    fn right_click_menu_unlocks_a_read_only_file() {
        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.repos = vec![
            RepoSel {
                name: "w".into(),
                included: true,
                read_only: false,
                is_main: false,
            },
            RepoSel {
                name: "ro".into(),
                included: true,
                read_only: true,
                is_main: false,
            },
        ];
        view.results = Some(Results::Similar(vec![vec![
            dfile("w", "best"),
            dfile("ro", "worse"),
        ]]));

        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(700.0, 900.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    let _ = &tmp;
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();

        harness.get_by_label("read-only").click_secondary();
        harness.run();
        harness
            .get_by_label(&format!("{} UNLOCK for deletion", icon::LOCK_OPEN))
            .click();
        harness.run();
        assert!(
            harness
                .state()
                .unlocked
                .contains(&("ro".into(), "worse".into())),
            "context-menu UNLOCK lifts the per-file lock"
        );
    }

    /// Every file card is an external escape hatch: right-clicking it offers
    /// OPEN (system default app) and SHOW IN FOLDER entries.
    #[test]
    fn right_click_card_offers_open_and_show_in_folder() {
        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.results = Some(Results::Similar(vec![vec![
            dfile("w", "best"),
            dfile("w", "worse"),
        ]]));

        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(700.0, 900.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    let _ = &tmp;
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();

        // Card labels are non-selectable (they must not sense clicks), so the
        // right-click falls through to the card's own interact response.
        harness.get_by_label("best").click_secondary();
        harness.run();
        assert!(
            harness
                .query_by_label(&format!("{} OPEN", icon::ARROW_RIGHT))
                .is_some(),
            "card context menu should offer OPEN"
        );
        assert!(
            harness
                .query_by_label(&format!("{} SHOW IN FOLDER", icon::FOLDER_OPEN))
                .is_some(),
            "card context menu should offer SHOW IN FOLDER"
        );
    }

    /// The law reaches its last caller: opening a duplicate card lands in the
    /// one shared viewer, with the group as the pool and the caller's own
    /// marks as the actions. Toggling a mark acts on the Duplicates tab's mark
    /// set and keeps the viewer open — marking is part of looking.
    #[test]
    fn a_card_opens_the_shared_viewer_with_the_group_as_pool_and_marks() {
        let group: DupeGroup = (0..3).map(image_file).collect();

        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.results = Some(Results::Similar(vec![group.clone()]));

        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let store2 = Arc::clone(&store);
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1200.0, 800.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    let _ = &tmp;
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();

        // What a card click pushes: the group and member the card shows.
        let ctx = egui::Context::default();
        harness
            .state_mut()
            .apply(&ctx, &store2, Act::OpenLightbox(0, 0));
        harness.run();

        {
            let lb = harness.state().lightbox.as_ref().expect("viewer open");
            assert_eq!(
                lb.left.rel_path, "img0.png",
                "the clicked member is the one shown"
            );
            assert_eq!(
                lb.pool_len(),
                3,
                "the whole group is the pool the sides step through"
            );
        }

        // The caller's actions are marks: reveal B, then DELETE A toggles the
        // Duplicates tab's own mark for that copy — and the viewer stays open.
        harness.get_by_label_contains("SHOW B").click();
        harness.run();
        let a_key = key(&group[0]);
        let before = harness.state().marked.contains(&a_key);
        harness.get_by_label_contains("DELETE A").click();
        harness.run();
        assert_eq!(
            harness.state().marked.contains(&a_key),
            !before,
            "DELETE A toggles that copy's mark in the caller's own mark set"
        );
        assert!(
            harness.state().lightbox.is_some(),
            "toggling a mark keeps the viewer open"
        );
        assert_ne!(
            harness
                .state()
                .lightbox
                .as_ref()
                .map(|lb| lb.right.rel_path.clone()),
            Some("img0.png".into()),
            "revealing B picks another member, never the file A shows"
        );
    }

    /// The viewer opens over a group, steps through its members with the arrow
    /// keys (the position counter following), and closes on `Esc`. Marking from
    /// the viewer is covered by
    /// [`a_card_opens_the_shared_viewer_with_the_group_as_pool_and_marks`].
    #[test]
    fn lightbox_opens_navigates_marks_and_closes() {
        let group: DupeGroup = (0..3).map(image_file).collect();

        let mut view = DupesView::new();
        view.repos_loaded = true; // fabricated groups, no repos needed
        view.results = Some(Results::Similar(vec![group]));

        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let store2 = Arc::clone(&store);
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1200.0, 800.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    let _ = &tmp;
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();

        // Open the viewer on the first (best) member.
        let ctx = egui::Context::default();
        harness
            .state_mut()
            .apply(&ctx, &store2, Act::OpenLightbox(0, 0));
        harness.run();
        assert!(
            harness.query_by_label("<1 / 3>").is_some(),
            "the viewer shows the position among the group's members"
        );
        assert!(
            harness.query_by_label_contains("CLOSE").is_some(),
            "the viewer shows a CLOSE control"
        );

        // Step to the next member.
        harness.key_press(egui::Key::ArrowRight);
        harness.run();
        assert_eq!(
            harness
                .state()
                .lightbox
                .as_ref()
                .map(|l| l.left.rel_path.clone()),
            Some("img1.png".into()),
            "ArrowRight advances to the second member"
        );
        assert!(
            harness.query_by_label("<2 / 3>").is_some(),
            "counter follows navigation"
        );

        // Close.
        harness.key_press(egui::Key::Escape);
        harness.run();
        assert!(
            harness.state().lightbox.is_none(),
            "Escape closes the lightbox"
        );
    }

    /// An audio dummy file (unique hash, audio mime, a stored duration).
    fn audio_file(i: usize) -> DupeFile {
        let mut hash = [0u8; 32];
        hash[0] = 0xA0 | i as u8;
        DupeFile {
            repo: "r".into(),
            repo_root: "/nonexistent-dedup-test".into(),
            rel_path: format!("track{i}.mp3"),
            entry: dedup_core::store::FileEntry {
                size: 5_000,
                hash,
                modified_ms: 0,
                missing: false,
                mime: Some("audio/mpeg".into()),
                img_fingerprint: None,
                video_hash: None,
                pdf_hash: None,
                audio: Some(dedup_core::store::AudioFp {
                    duration_ms: 185_000,
                    chunk_hashes: Vec::new(),
                }),
                img_size: None,
                origin: None,
                exif: None,
            },
        }
    }

    /// Audio cards show a PLAY control; clicking it loads that file into the
    /// single global player (which then shows PAUSE and a seek bar).
    #[test]
    fn audio_card_plays_and_reflects_player_state() {
        let group: DupeGroup = (0..2).map(audio_file).collect();
        let target_hex = dedup_core::thumbnail::hash_hex(&group[0].entry.hash);

        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.results = Some(Results::Similar(vec![group]));

        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(900.0, 820.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    let _ = &tmp;
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();

        let plays: Vec<_> = harness.get_all_by_label("PLAY").collect();
        assert_eq!(plays.len(), 2, "each audio card shows a PLAY control");

        // Click the first card's PLAY → that file becomes the loaded one.
        // A playing card keeps repainting (seek bar), so step a fixed number of
        // frames instead of running to a settled state.
        plays[0].click();
        harness.step();
        harness.step();
        let snap = harness.state().player.snapshot();
        assert_eq!(
            snap.hex.as_deref(),
            Some(target_hex.as_str()),
            "clicking PLAY loads that file into the global player"
        );
        assert!(snap.loaded, "player reports a loaded track");
    }

    /// Audio cards render a fingerprint-glyph tile (with the duration) instead
    /// of the generic broken-image placeholder, which used to show the raw mime
    /// string in place of a thumbnail.
    #[test]
    fn audio_card_shows_fingerprint_tile_not_placeholder() {
        let group: DupeGroup = (0..2).map(audio_file).collect();

        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.results = Some(Results::Similar(vec![group]));

        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(900.0, 700.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    let _ = &tmp;
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();

        assert!(
            harness.query_by_label("track0.mp3").is_some(),
            "the audio card renders"
        );
        assert!(
            harness.query_by_label("audio/mpeg").is_none(),
            "the audio tile replaces the broken-image placeholder (which showed the raw mime)"
        );
    }

    /// Switching to another copy in a group keeps the playback offset, so you
    /// hear the same moment in each. Clicking the currently-playing file instead
    /// toggles pause (offset untouched).
    #[test]
    fn switching_audio_copies_keeps_offset() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let ctx = egui::Context::default();
        let mut view = DupesView::new();

        let f0 = audio_file(0);
        let f1 = audio_file(1);
        let hex0 = hash_hex(&f0.entry.hash);
        let hex1 = hash_hex(&f1.entry.hash);
        let total = f0.entry.audio.as_ref().unwrap().duration_ms as u64;

        // Start copy 0 from the beginning, then seek into the middle.
        view.apply(
            &ctx,
            &store,
            Act::PlayAudio(hex0, f0.absolute_path(), total),
        );
        view.apply(&ctx, &store, Act::SeekAudio(0.5));
        let mid = view.player.snapshot().pos_ms;
        assert!(mid > 0, "seeking advanced the position");

        // Switch to copy 1 → the offset carries over.
        view.apply(
            &ctx,
            &store,
            Act::PlayAudio(hex1.clone(), f1.absolute_path(), total),
        );
        let snap = view.player.snapshot();
        assert_eq!(
            snap.hex.as_deref(),
            Some(hex1.as_str()),
            "switched to the other copy"
        );
        assert_eq!(
            snap.pos_ms, mid,
            "playback offset carried over to the new copy"
        );
    }

    /// Stepping to another copy while paused must swap which file is loaded and
    /// stay paused. Before this, a paused nav left the *previous* file loaded, so
    /// the lightbox showed one copy while play would resume another.
    #[test]
    fn stepping_while_paused_loads_the_new_copy_without_resuming() {
        let group: DupeGroup = (0..3).map(audio_file).collect();
        let a_hex = hash_hex(&group[0].entry.hash);
        let b_hex = hash_hex(&group[1].entry.hash);

        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.results = Some(Results::Similar(vec![group]));

        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let store2 = Arc::clone(&store);
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1000.0, 700.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    let _ = &tmp;
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();
        let ctx = egui::Context::default();
        harness
            .state_mut()
            .apply(&ctx, &store2, Act::OpenLightbox(0, 0));
        harness.run();

        // Establish the state the reporter described: a copy loaded and
        // deliberately paused. Driving this through the player API rather than a
        // key press keeps it deterministic — the audio thread corrects `playing`
        // from the real sink, which a headless run does not have.
        let path = std::path::PathBuf::from("/nonexistent-dedup-test/track0.mp3");
        harness.state().player.load_paused(&a_hex, &path, 5_000, 0);
        harness.step();
        harness.step();
        let snap = harness.state().player.snapshot();
        assert!(
            snap.loaded && !snap.playing,
            "set up: copy A loaded and paused"
        );
        assert_eq!(snap.hex.as_deref(), Some(a_hex.as_str()));

        // Step to the next copy while paused.
        harness.key_press(egui::Key::ArrowRight);
        harness.step();
        harness.step();

        let snap = harness.state().player.snapshot();
        assert!(
            !snap.playing,
            "a deliberate pause survives the step - it must not resume on its own"
        );
        assert_eq!(
            snap.hex.as_deref(),
            Some(b_hex.as_str()),
            "the newly shown copy is the one now loaded, so play resumes the right file"
        );
    }

    /// Clicking an audio card opens the shared viewer on the Audio tab with a
    /// working transport: `P` plays the shown copy, revealing B gives a
    /// transport per side, and `Esc` closes. The pair/flip mechanics are pinned
    /// at the viewer's own seam
    /// (`compare_view::tests::p_plays_the_gapless_pair_and_arrows_flip_the_audible_copy`).
    #[test]
    fn the_audio_viewer_opens_plays_and_escapes() {
        let group: DupeGroup = (0..2).map(audio_file).collect();
        let a_hex = hash_hex(&group[0].entry.hash);

        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.results = Some(Results::Similar(vec![group]));

        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let store2 = Arc::clone(&store);
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1200.0, 800.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    let _ = &tmp;
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();
        let ctx = egui::Context::default();
        harness
            .state_mut()
            .apply(&ctx, &store2, Act::OpenLightbox(0, 0));
        harness.run();

        // The shared viewer, on the audio group's own representation.
        assert!(
            harness.query_by_label_contains("CLOSE").is_some(),
            "the viewer shows a CLOSE control"
        );
        assert_eq!(
            harness.state().lightbox.as_ref().map(|lb| lb.tab),
            Some(crate::lightbox::RepresentationKind::Audio),
            "an audio group opens on the Audio tab"
        );

        // P plays the shown copy. Playback keeps repainting, so step a fixed
        // number of frames rather than running to a settled state.
        harness.key_press(egui::Key::P);
        harness.step();
        harness.step();
        assert_eq!(
            harness.state().player.snapshot().hex.as_deref(),
            Some(a_hex.as_str()),
            "P plays the shown copy"
        );

        // Revealing B gives each side its own transport. Playback keeps the UI
        // repainting, so step fixed frames rather than running to settled.
        harness.get_by_label_contains("SHOW B").click();
        harness.step();
        harness.step();
        assert!(
            harness.query_by_label_contains("PLAY A").is_some()
                && harness.query_by_label_contains("PLAY B").is_some(),
            "comparing offers a transport per side"
        );

        // Esc closes, and what the viewer was playing falls silent.
        harness.key_press(egui::Key::Escape);
        harness.step();
        harness.step();
        assert!(harness.state().lightbox.is_none(), "Esc closes the viewer");
        assert!(
            !harness.state().player.snapshot().loaded,
            "closing the viewer silences what it started"
        );
    }

    /// `T` opens the audio lightbox's ID3 editor pre-filled with the file's
    /// tags; editing a field and clicking SAVE TAGS writes only the tags to
    /// disk, preserves the others, and keeps the lightbox open (6.5/6.6).
    #[test]
    fn the_audio_viewer_edits_and_saves_id3_tags() {
        let tmp = tempfile::tempdir().unwrap();
        let mp3 = tmp.path().join("song.mp3");
        crate::id3tags::write_bare_mp3(&mp3);
        crate::id3tags::write(
            &mp3,
            &Tags {
                title: "Old".into(),
                artist: "Cohen".into(),
                ..Default::default()
            },
        )
        .unwrap();

        let mut hash = [0u8; 32];
        hash[0] = 0xC3;
        let file = DupeFile {
            repo: "r".into(),
            repo_root: tmp.path().to_string_lossy().into_owned(),
            rel_path: "song.mp3".into(),
            entry: dedup_core::store::FileEntry {
                size: 417,
                hash,
                modified_ms: 0,
                missing: false,
                mime: Some("audio/mpeg".into()),
                img_fingerprint: None,
                video_hash: None,
                pdf_hash: None,
                audio: Some(dedup_core::store::AudioFp {
                    duration_ms: 1000,
                    chunk_hashes: Vec::new(),
                }),
                img_size: None,
                origin: None,
                exif: None,
            },
        };

        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.results = Some(Results::Similar(vec![vec![file]]));

        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let store2 = Arc::clone(&store);
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1200.0, 800.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();
        let ctx = egui::Context::default();
        harness
            .state_mut()
            .apply(&ctx, &store2, Act::OpenLightbox(0, 0));
        for _ in 0..4 {
            harness.step();
        }

        // T opens the editor, pre-filled from the file.
        harness.key_press(egui::Key::T);
        for _ in 0..4 {
            harness.step();
        }
        assert_eq!(
            harness
                .state()
                .lightbox
                .as_ref()
                .and_then(|lb| lb.tag_edit.as_ref())
                .map(|t| t.tags.title.as_str()),
            Some("Old"),
            "editor opens pre-filled with the current title"
        );

        // Type a new title, then SAVE TAGS.
        harness
            .state_mut()
            .lightbox
            .as_mut()
            .unwrap()
            .tag_edit
            .as_mut()
            .unwrap()
            .tags
            .title = "New Title".into();
        harness.run();
        harness.get_by_label("SAVE TAGS").click();
        harness.run();

        let saved = crate::id3tags::read(&mp3).expect("tags still readable");
        assert_eq!(saved.title, "New Title", "the new title is written to disk");
        assert_eq!(saved.artist, "Cohen", "other tags are preserved");
        assert!(
            harness
                .state()
                .lightbox
                .as_ref()
                .is_some_and(|lb| lb.tag_edit.is_none()),
            "the editor closes on save"
        );
        assert!(
            harness.state().lightbox.is_some(),
            "saving keeps the lightbox open (6.6)"
        );
    }

    /// In side-by-side compare, `←`/`→` flip which copy is audible (gap-free via
    /// the loaded pair) and move the cursor — not re-index A (which used to
    /// pause and strand the cursor on the top row).
    #[test]
    fn audio_compare_arrow_flips_audible_copy() {
        let group: DupeGroup = (0..2).map(audio_file).collect();
        let a_hex = hash_hex(&group[0].entry.hash);
        let b_hex = hash_hex(&group[1].entry.hash);

        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.results = Some(Results::Similar(vec![group]));

        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let store2 = Arc::clone(&store);
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1200.0, 800.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    let _ = &tmp;
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();
        let ctx = egui::Context::default();
        harness
            .state_mut()
            .apply(&ctx, &store2, Act::OpenLightbox(0, 0));
        harness.run();

        // Enter compare, then play → the synced pair loads (A audible).
        harness.get_by_label_contains("SHOW B").click();
        harness.run();
        harness.run();
        harness.key_press(egui::Key::P);
        harness.step();
        harness.step();
        let snap = harness.state().player.snapshot();
        assert!(snap.paired, "compare + play loads the pair");
        assert_eq!(snap.hex.as_deref(), Some(a_hex.as_str()), "A audible first");

        // → flips audible to B, gap-free (still paired), cursor follows.
        harness.key_press(egui::Key::ArrowRight);
        harness.step();
        harness.step();
        let snap = harness.state().player.snapshot();
        assert_eq!(
            snap.hex.as_deref(),
            Some(b_hex.as_str()),
            "→ flips audible to B"
        );
        assert!(snap.paired, "still paired — no reload, no gap");
        assert!(
            snap.loaded && snap.playing,
            "still playing after the flip — so the transport button stays PAUSE (not PLAY)"
        );
        assert_eq!(
            harness.state().lightbox.as_ref().unwrap().audio_active,
            Some(1),
            "the playback cursor moves to B"
        );

        // → again flips back to A.
        harness.key_press(egui::Key::ArrowRight);
        harness.step();
        harness.step();
        assert_eq!(
            harness.state().player.snapshot().hex.as_deref(),
            Some(a_hex.as_str()),
            "→ flips back to A"
        );
    }

    /// A four-copy audio group: stepping B's switcher must walk it through each
    /// of the three *others* in turn. The report was that tags repeated every
    /// second click, as if four members mapped onto two files.
    #[test]
    fn a_four_copy_audio_group_cycles_b_through_three_distinct_others() {
        let tmp = tempfile::tempdir().unwrap();
        let mk = |i: u8| -> DupeFile {
            let name = format!("track{i}.mp3");
            let path = tmp.path().join(&name);
            crate::id3tags::write_bare_mp3(&path);
            crate::id3tags::write(
                &path,
                &Tags {
                    title: format!("Title{i}"),
                    artist: format!("Artist{i}"),
                    ..Default::default()
                },
            )
            .unwrap();
            let mut hash = [0u8; 32];
            hash[0] = 0xB0 | i;
            DupeFile {
                repo: "r".into(),
                repo_root: tmp.path().to_string_lossy().into_owned(),
                rel_path: name,
                entry: dedup_core::store::FileEntry {
                    size: 417,
                    hash,
                    modified_ms: 0,
                    missing: false,
                    mime: Some("audio/mpeg".into()),
                    img_fingerprint: None,
                    video_hash: None,
                    pdf_hash: None,
                    audio: Some(dedup_core::store::AudioFp {
                        duration_ms: 1000,
                        chunk_hashes: Vec::new(),
                    }),
                    img_size: None,
                    origin: None,
                    exif: None,
                },
            }
        };
        let group: DupeGroup = (0..4u8).map(mk).collect();

        // The index mapping the cycler is built on: with A fixed, there are
        // exactly three others and none of them is A.
        for left in 0..4usize {
            let others = crate::lightbox::other_member_indices(4, left);
            assert_eq!(others.len(), 3, "a 4-copy group has 3 others of A={left}");
            assert!(!others.contains(&left), "A is never its own B");
            let mut seen = others.clone();
            seen.sort();
            seen.dedup();
            assert_eq!(
                seen.len(),
                3,
                "the three others are distinct, not a 1..2 cycle"
            );
        }

        // ...and the label never reports the group size where the count of
        // others belongs: a 4-copy group must never read "/ 4".
        for sel in 0..3usize {
            let label = crate::lightbox::format_other_switcher_label(sel, 3);
            assert_eq!(label, format!("<{} / 3>", sel + 1));
            assert!(
                !label.contains("/ 4"),
                "must never show the member count as the others count: {label}"
            );
        }

        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.results = Some(Results::Similar(vec![group]));
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let store2 = Arc::clone(&store);
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1200.0, 800.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();
        let ctx = egui::Context::default();
        harness
            .state_mut()
            .apply(&ctx, &store2, Act::OpenLightbox(0, 0));
        for _ in 0..4 {
            harness.step();
        }
        // The cycler is B's own switcher, offered once B is revealed.
        harness.get_by_label_contains("SHOW B").click();
        for _ in 0..4 {
            harness.step();
        }

        // The switcher counts the three *others* — never the four members.
        assert!(
            harness.query_all_by_label("<1 / 3>").count() > 0,
            "the switcher counts the three others, not the four members"
        );

        // Walk it: each step must land on a new label and wrap after the third,
        // rather than repeating every second click as reported.
        for expected in ["<2 / 3>", "<3 / 3>", "<1 / 3>"] {
            harness.get_by_label_contains("NEXT B").click();
            // Settle until the new candidate fully resolves — the target label
            // present and the member-count labels gone — rather than a fixed
            // step count: under parallel decode load a handful of frames isn't
            // enough, and breaking on a transient frame catches a stale count.
            for _ in 0..200 {
                let settled = harness.query_all_by_label(expected).count() > 0
                    && harness.query_all_by_label("<1 / 4>").count() == 0
                    && harness.query_all_by_label("<4 / 4>").count() == 0;
                if settled {
                    break;
                }
                harness.step();
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert!(
                harness.query_all_by_label(expected).count() > 0,
                "cycling B should reach {expected}"
            );
            assert!(
                harness.query_all_by_label("<1 / 4>").count() == 0
                    && harness.query_all_by_label("<4 / 4>").count() == 0,
                "the member count must never appear in the others slot"
            );
        }
    }

    /// Each copy's own ID3 tags must be the ones shown for it. The report was
    /// Playback rate is offered in the audio header and takes effect — slowing a
    /// passage is how two takes of one recording are told apart by ear.
    #[test]
    fn the_audio_header_offers_playback_speed() {
        let tmp = tempfile::tempdir().unwrap();
        let group: DupeGroup = (0..2).map(audio_file).collect();
        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.results = Some(Results::Similar(vec![group]));
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let store2 = Arc::clone(&store);
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1200.0, 800.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    let _ = &tmp;
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();
        let ctx = egui::Context::default();
        harness
            .state_mut()
            .apply(&ctx, &store2, Act::OpenLightbox(0, 0));
        harness.run();

        harness.get_by_label_contains("SPEED 1×");
        harness.get_by_label_contains("SPEED").click();
        harness.run();
        // The next stop up from the 1× default. The player itself stays at
        // normal speed — a non-1× stop plays a pitch-preserving pre-render.
        harness.get_by_label_contains("SPEED 1.5×");
    }

    /// that copies 1 and 3 showed identical tags after editing only one.
    #[test]
    fn each_audio_copy_shows_its_own_tags_not_every_second_one() {
        let tmp = tempfile::tempdir().unwrap();
        let mk = |i: u8| -> DupeFile {
            let name = format!("copy{i}.mp3");
            let path = tmp.path().join(&name);
            crate::id3tags::write_bare_mp3(&path);
            crate::id3tags::write(
                &path,
                &Tags {
                    title: format!("Title{i}"),
                    artist: format!("Artist{i}"),
                    ..Default::default()
                },
            )
            .unwrap();
            let mut hash = [0u8; 32];
            hash[0] = 0xC0 | i;
            DupeFile {
                repo: "r".into(),
                repo_root: tmp.path().to_string_lossy().into_owned(),
                rel_path: name,
                entry: dedup_core::store::FileEntry {
                    size: 417,
                    hash,
                    modified_ms: 0,
                    missing: false,
                    mime: Some("audio/mpeg".into()),
                    img_fingerprint: None,
                    video_hash: None,
                    pdf_hash: None,
                    audio: Some(dedup_core::store::AudioFp {
                        duration_ms: 1000,
                        chunk_hashes: Vec::new(),
                    }),
                    img_size: None,
                    origin: None,
                    exif: None,
                },
            }
        };
        let group: Vec<DupeFile> = (0..4u8).map(mk).collect();

        // Read back what each copy holds on disk, through the same reader the
        // lightbox uses. Copy 1 and copy 3 must differ — the exact symptom.
        let tags: Vec<Tags> = group
            .iter()
            .map(|f| crate::id3tags::read(&f.absolute_path()).unwrap_or_default())
            .collect();
        for (i, t) in tags.iter().enumerate() {
            assert_eq!(t.title, format!("Title{i}"), "copy {i} keeps its own title");
            assert_eq!(
                t.artist,
                format!("Artist{i}"),
                "copy {i} keeps its own artist"
            );
        }
        assert_ne!(
            tags[1].title, tags[3].title,
            "copies 1 and 3 must not collapse onto the same tags"
        );
    }

    /// The native audio compare header carries its own DELETE A / DELETE B pills,
    /// so marking does not depend on which media type is being compared (the
    /// image compare header already had them). qa.md: "no mark buttons for mp3s".
    #[test]
    fn audio_compare_header_marks_each_copy_independently() {
        let tmp = tempfile::tempdir().unwrap();
        let group: DupeGroup = (0..2).map(audio_file).collect();
        let a_key = key(&group[0]);
        let b_key = key(&group[1]);

        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.results = Some(Results::Similar(vec![group]));
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let store2 = Arc::clone(&store);
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1200.0, 800.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    let _ = &tmp;
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();
        let ctx = egui::Context::default();
        harness
            .state_mut()
            .apply(&ctx, &store2, Act::OpenLightbox(0, 0));
        harness.run();

        // Not comparing: one DELETE for the copy on screen.
        assert!(
            harness.query_all_by_label_contains("DELETE").count() > 0,
            "a single copy offers one DELETE pill"
        );

        harness.get_by_label_contains("SHOW B").click();
        harness.run();
        harness.run();

        // Comparing: an independent pill per copy.
        assert!(
            harness.query_all_by_label_contains("DELETE A").count() > 0,
            "compare offers DELETE A"
        );
        assert!(
            harness.query_all_by_label_contains("DELETE B").count() > 0,
            "compare offers DELETE B"
        );

        // Toggling one pill must move that copy's mark only. Assert the
        // *transition*, not absolute membership: the Duplicates view auto-marks
        // the copies it did not pick as best, so B already carries a mark here.
        let a0 = harness.state().marked.contains(&a_key);
        let b0 = harness.state().marked.contains(&b_key);

        harness.get_by_label_contains("DELETE A").click();
        harness.run();
        assert_eq!(
            harness.state().marked.contains(&a_key),
            !a0,
            "DELETE A toggles A's mark"
        );
        assert_eq!(
            harness.state().marked.contains(&b_key),
            b0,
            "DELETE A must leave B's mark exactly as it was"
        );

        harness.get_by_label_contains("DELETE B").click();
        harness.run();
        assert_eq!(
            harness.state().marked.contains(&b_key),
            !b0,
            "DELETE B toggles B's mark"
        );
        assert_eq!(
            harness.state().marked.contains(&a_key),
            !a0,
            "and leaves A's mark as the previous click set it"
        );
    }

    /// The added pills must not push the header's controls out of the window.
    /// A label query passes even when a widget is clipped, so assert rectangles.
    #[test]
    fn audio_compare_header_controls_stay_inside_a_narrow_window() {
        let tmp = tempfile::tempdir().unwrap();
        let group: DupeGroup = (0..2).map(audio_file).collect();
        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.results = Some(Results::Similar(vec![group]));
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let store2 = Arc::clone(&store);
        let mut init = false;

        let width = 900.0;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(width, 700.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    let _ = &tmp;
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();
        let ctx = egui::Context::default();
        harness
            .state_mut()
            .apply(&ctx, &store2, Act::OpenLightbox(0, 0));
        harness.run();
        harness.get_by_label_contains("SHOW B").click();
        harness.run();
        harness.run();

        for label in ["DELETE A", "DELETE B"] {
            let rect = harness.get_by_label_contains(label).rect();
            assert!(
                rect.max.x <= width,
                "'{label}' escapes the {width}px window: {rect:?}"
            );
            assert!(rect.min.x >= 0.0, "'{label}' starts off-screen: {rect:?}");
        }
        // One SPEED control per side; each stays inside the window too.
        let speeds: Vec<_> = harness
            .query_all_by_label_contains("SPEED")
            .map(|n| n.rect())
            .collect();
        assert!(!speeds.is_empty(), "the transport offers SPEED");
        for rect in speeds {
            assert!(
                rect.max.x <= width && rect.min.x >= 0.0,
                "'SPEED' must stay inside the {width}px window: {rect:?}"
            );
        }
    }

    /// The Metadata tab shows a tag panel per side while comparing — A's tags
    /// and B's tags both on screen (symmetric), each with its own EDIT control.
    #[test]
    fn audio_compare_shows_symmetric_tag_panels() {
        let tmp = tempfile::tempdir().unwrap();
        let mk = |name: &str, artist: &str, i: u8| -> DupeFile {
            let path = tmp.path().join(name);
            crate::id3tags::write_bare_mp3(&path);
            crate::id3tags::write(
                &path,
                &Tags {
                    title: "Song".into(),
                    artist: artist.into(),
                    ..Default::default()
                },
            )
            .unwrap();
            let mut hash = [0u8; 32];
            hash[0] = 0xE0 | i;
            DupeFile {
                repo: "r".into(),
                repo_root: tmp.path().to_string_lossy().into_owned(),
                rel_path: name.into(),
                entry: dedup_core::store::FileEntry {
                    size: 417,
                    hash,
                    modified_ms: 0,
                    missing: false,
                    mime: Some("audio/mpeg".into()),
                    img_fingerprint: None,
                    video_hash: None,
                    pdf_hash: None,
                    audio: Some(dedup_core::store::AudioFp {
                        duration_ms: 1000,
                        chunk_hashes: Vec::new(),
                    }),
                    img_size: None,
                    origin: None,
                    exif: None,
                },
            }
        };
        let group: DupeGroup = vec![mk("a.mp3", "Alpha", 0), mk("b.mp3", "Beta", 1)];

        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.results = Some(Results::Similar(vec![group]));

        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let store2 = Arc::clone(&store);
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1200.0, 800.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();
        let ctx = egui::Context::default();
        harness
            .state_mut()
            .apply(&ctx, &store2, Act::OpenLightbox(0, 0));
        for _ in 0..4 {
            harness.step();
        }
        harness.get_by_label_contains("SHOW B").click();
        for _ in 0..4 {
            harness.step();
        }
        harness.get_by_label_contains("Metadata").click();
        for _ in 0..4 {
            harness.step();
        }

        // A's value shows in A's panel and B's value in B's (not blank).
        assert!(
            harness.query_by_label("Alpha").is_some(),
            "A's tags render in its panel"
        );
        assert!(
            harness.query_by_label("Beta").is_some(),
            "B's tags render in its panel (the regression the user hit)"
        );
        // One EDIT control per panel.
        let edits = harness
            .get_all_by_label(&format!("{} EDIT TAGS", icon::PENCIL))
            .count();
        assert_eq!(edits, 2, "an EDIT control per copy");
    }

    /// Opening the tag editor gathers the distinct value of each field from
    /// every copy in the group, so the user can adopt the best one.
    #[test]
    fn tag_editor_offers_values_from_all_copies() {
        let tmp = tempfile::tempdir().unwrap();
        let mk = |name: &str, title: &str, i: u8| -> DupeFile {
            let path = tmp.path().join(name);
            crate::id3tags::write_bare_mp3(&path);
            crate::id3tags::write(
                &path,
                &Tags {
                    title: title.into(),
                    artist: "The Band".into(),
                    ..Default::default()
                },
            )
            .unwrap();
            let mut hash = [0u8; 32];
            hash[0] = 0xD0 | i;
            DupeFile {
                repo: "r".into(),
                repo_root: tmp.path().to_string_lossy().into_owned(),
                rel_path: name.into(),
                entry: dedup_core::store::FileEntry {
                    size: 417,
                    hash,
                    modified_ms: 0,
                    missing: false,
                    mime: Some("audio/mpeg".into()),
                    img_fingerprint: None,
                    video_hash: None,
                    pdf_hash: None,
                    audio: Some(dedup_core::store::AudioFp {
                        duration_ms: 1000,
                        chunk_hashes: Vec::new(),
                    }),
                    img_size: None,
                    origin: None,
                    exif: None,
                },
            }
        };
        let group: DupeGroup = vec![mk("one.mp3", "Take One", 0), mk("two.mp3", "Take Two", 1)];

        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.results = Some(Results::Similar(vec![group]));

        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let store2 = Arc::clone(&store);
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1200.0, 800.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();
        let ctx = egui::Context::default();
        harness
            .state_mut()
            .apply(&ctx, &store2, Act::OpenLightbox(0, 0));
        // The audio decode lands on its own schedule and wakes the UI, so step
        // a fixed number of frames rather than running to a settled state.
        for _ in 0..4 {
            harness.step();
        }
        harness.key_press(egui::Key::T);
        for _ in 0..4 {
            harness.step();
        }

        let te = harness.state();
        let te = te
            .lightbox
            .as_ref()
            .and_then(|lb| lb.tag_edit.as_ref())
            .expect("editor open");
        assert!(
            te.options[0].contains(&"Take One".to_string())
                && te.options[0].contains(&"Take Two".to_string()),
            "title options gather both copies' values: {:?}",
            te.options[0]
        );
        assert_eq!(
            te.options[1],
            vec!["The Band".to_string()],
            "the shared artist is deduplicated to one option"
        );
    }

    /// Revealing B exposes the pair — B defaulting to the next member, never
    /// A's own file — with a DELETE B pill for the candidate, and HIDE B
    /// returns to the single view.
    #[test]
    fn lightbox_compare_enters_marks_b_and_exits() {
        let group: DupeGroup = (0..3).map(image_file).collect();
        let b_key = key(&group[1]); // A is index 0 → B defaults to index 1

        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.results = Some(Results::Similar(vec![group]));

        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let store2 = Arc::clone(&store);
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1200.0, 800.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    let _ = &tmp;
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();
        let ctx = egui::Context::default();
        harness
            .state_mut()
            .apply(&ctx, &store2, Act::OpenLightbox(0, 0));
        harness.run();

        // Reveal the pair.
        harness.get_by_label_contains("SHOW B").click();
        harness.run();
        assert_eq!(
            harness
                .state()
                .lightbox
                .as_ref()
                .map(|lb| lb.right.rel_path.clone()),
            Some("img1.png".into()),
            "B defaults to the next member, never A's own file"
        );

        // Clear preselected marks so B shows the unmarked DELETE B control.
        harness.state_mut().marked.clear();
        harness.run();
        harness.get_by_label_contains("DELETE B").click();
        harness.run();
        assert!(
            harness.state().marked.contains(&b_key),
            "DELETE B marks the candidate"
        );

        // Exit back to the single view.
        harness.get_by_label_contains("HIDE B").click();
        harness.run();
        assert!(
            harness.query_by_label_contains("SHOW B").is_some(),
            "HIDE B returns to the single view"
        );
    }

    /// `M` toggles the match mode from the grid (no lightbox/modal open).
    #[test]
    fn grid_shortcut_toggles_match_mode() {
        let mut view = DupesView::new();
        view.repos_loaded = true;

        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(900.0, 600.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    let _ = &tmp;
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();
        assert!(
            harness.state().mode == Mode::Exact,
            "starts in exact/DUPLICATES"
        );

        harness.key_press(egui::Key::M);
        harness.run();
        harness.run();
        assert!(
            harness.state().mode == Mode::Similar,
            "M switches to SIMILAR"
        );
        harness.key_press(egui::Key::M);
        harness.run();
        harness.run();
        assert!(harness.state().mode == Mode::Exact, "M toggles back");
    }

    /// Re-locking removes the override and any pending mark, and turning a
    /// repo read-only clears its per-file unlocks.
    #[test]
    fn relock_and_repo_ro_toggle_clear_unlocks() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let ctx = egui::Context::default();
        let k: FileKey = ("r".into(), "f".into());

        let mut view = DupesView::new();
        view.unlocked.insert(k.clone());
        view.marked.insert(k.clone());
        view.apply(&ctx, &store, Act::Relock(k.clone()));
        assert!(!view.unlocked.contains(&k), "relock removes the override");
        assert!(!view.marked.contains(&k), "relock unmarks the file");

        view.repos = vec![RepoSel {
            name: "r".into(),
            included: true,
            read_only: false,
            is_main: false,
        }];
        view.unlocked.insert(k.clone());
        view.apply(&ctx, &store, Act::ToggleRo(0));
        assert!(
            !view.unlocked.contains(&k),
            "turning a repo read-only relocks its files"
        );
    }

    /// Default marking is bounded to the current page, not the whole result.
    #[test]
    fn marking_is_bounded_to_the_current_page() {
        let groups: Vec<DupeGroup> = (0..60)
            .map(|i| {
                vec![
                    dfile("w", &format!("best{i}")),
                    dfile("w", &format!("worse{i}")),
                ]
            })
            .collect();
        let mut view = DupesView::new();
        view.repos_loaded = true; // repos empty → nothing read-only → all worse markable
        view.results = Some(Results::Similar(groups));

        let harness = similar_harness(view);
        let n = harness.state().marked.len();
        assert!(n > 0, "page 0's worse copies should be marked");
        assert!(
            n <= PAGE_SIZE,
            "only the current page (≤{PAGE_SIZE}) should be marked, got {n} of 60"
        );
    }

    /// Similar groups' members differ in size, so the header must show the
    /// combined size and the summed reclaimable bytes — not the exact-dupe
    /// "X each" wording, which assumed byte-identical copies.
    #[test]
    fn similar_header_shows_totals_not_per_copy_size() {
        let mut best = dfile("w", "a");
        best.entry.size = 3000;
        let mut worse = dfile("w", "b");
        worse.entry.size = 1000;
        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.results = Some(Results::Similar(vec![vec![best, worse]]));

        let harness = similar_harness(view);
        let expected = format!(
            "2 similar · {} total · {} reclaimable",
            format_size(4000),
            format_size(1000)
        );
        assert!(
            harness.query_by_label(&expected).is_some(),
            "similar group header should read \"{expected}\""
        );
    }

    /// Quick Delete's per-group DELETE NOW removes that group's marked files and
    /// collapses the group, without wiping the (paged) plan.
    #[test]
    fn quick_delete_now_removes_files_and_resolves_group() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let root = tmp.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        store.create_repo("repo", &root.to_string_lossy()).unwrap();
        for name in ["a.bin", "b.bin"] {
            std::fs::write(root.join(name), b"dup").unwrap();
            store
                .update_file_entry(
                    "repo",
                    name,
                    &dedup_core::store::FileEntry {
                        size: 3,
                        hash: [7u8; 32],
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
                    },
                )
                .unwrap();
        }
        let plan = plan_exact_duplicates(&store, &["repo".to_string()], |_| {}).unwrap();
        assert_eq!(plan.len(), 1);

        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.repos = vec![RepoSel {
            name: "repo".into(),
            included: true,
            read_only: false,
            is_main: false,
        }];
        view.result_names = vec!["repo".to_string()];
        view.results = Some(Results::Exact(plan));
        view.quick_delete = true;

        let store_ui = Arc::clone(&store);
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(800.0, 600.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
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
            .get_by_label(&format!("{} DELETE NOW", icon::TRASH))
            .click();

        let mut resolved = false;
        for _ in 0..200 {
            harness.step();
            if harness.state().resolved.contains(&0) {
                resolved = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(resolved, "group was not resolved after DELETE NOW");
        // The best copy (a.bin, alphabetically first) stays; the worse is gone.
        assert!(root.join("a.bin").exists(), "best copy kept");
        assert!(!root.join("b.bin").exists(), "worse copy deleted");
        assert!(
            harness.state().results.is_some(),
            "per-group delete must not wipe the plan"
        );
    }

    /// Renders the audio lightbox's ID3 tag editor to `target/dupes_tags.png`.
    #[test]
    #[ignore = "renders a PNG for manual inspection"]
    fn render_audio_tags() {
        let tmp = tempfile::tempdir().unwrap();
        let mk = |name: &str, tags: Tags, i: u8| -> DupeFile {
            let path = tmp.path().join(name);
            crate::id3tags::write_bare_mp3(&path);
            crate::id3tags::write(&path, &tags).unwrap();
            let mut hash = [0u8; 32];
            hash[0] = 0xC0 | i;
            DupeFile {
                repo: "r".into(),
                repo_root: tmp.path().to_string_lossy().into_owned(),
                rel_path: name.into(),
                entry: dedup_core::store::FileEntry {
                    size: 417,
                    hash,
                    modified_ms: 0,
                    missing: false,
                    mime: Some("audio/mpeg".into()),
                    img_fingerprint: None,
                    video_hash: None,
                    pdf_hash: None,
                    audio: Some(dedup_core::store::AudioFp {
                        duration_ms: 1000,
                        chunk_hashes: Vec::new(),
                    }),
                    img_size: None,
                    origin: None,
                    exif: None,
                },
            }
        };
        // Two copies that agree on most tags but differ on album/track.
        let a = mk(
            "a.mp3",
            Tags {
                title: "Chelsea Hotel #2".into(),
                artist: "Leonard Cohen".into(),
                album: "New Skin".into(),
                year: "1974".into(),
                track: "5".into(),
                genre: "Folk".into(),
            },
            0,
        );
        let b = mk(
            "b.mp3",
            Tags {
                title: "Chelsea Hotel #2".into(),
                artist: "Leonard Cohen".into(),
                album: "New Skin for the Old Ceremony".into(),
                year: "1974".into(),
                track: "05".into(),
                genre: "Folk".into(),
            },
            1,
        );
        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.results = Some(Results::Similar(vec![vec![a, b]]));
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let store2 = Arc::clone(&store);
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1000.0, 640.0))
            .wgpu()
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();
        let ctx = egui::Context::default();
        harness
            .state_mut()
            .apply(&ctx, &store2, Act::OpenLightbox(0, 0));
        for _ in 0..4 {
            harness.step();
        }
        // Both sides on the Metadata tab → the symmetric tag panels.
        harness.get_by_label_contains("SHOW B").click();
        for _ in 0..4 {
            harness.step();
        }
        harness.get_by_label_contains("Metadata").click();
        for _ in 0..4 {
            harness.step();
        }
        let img = harness.render().expect("wgpu render failed");
        let out =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/dupes_tags.png");
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());
    }

    /// Not run by default: renders the view to `target/dupes_view.png` for a
    /// human to eyeball. Needs a wgpu backend (lavapipe works headless):
    ///   cargo test -p dedup-gui render_dupes_view -- --ignored --nocapture
    #[test]
    #[ignore = "renders a PNG for manual inspection"]
    fn render_dupes_view() {
        let (_tmp, store) = sample_store(&SAMPLE_REPOS);
        let mut view = DupesView::new();
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1120.0, 360.0))
            .wgpu()
            .build_ui(move |ui| {
                if !init {
                    crate::icon::install(ui.ctx());
                    crate::theme::apply(ui.ctx(), crate::theme::DARK);
                    init = true;
                }
                view.show(ui, &store, TooltipVerbosity::default());
            });
        harness.run();
        let img = harness.render().expect("wgpu render failed");
        let out =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/dupes_view.png");
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());
    }

    /// Renders a group of audio cards (fingerprint-glyph tiles) to
    /// `target/dupes_audio.png` for manual inspection. `--ignored`.
    #[test]
    #[ignore = "renders a PNG for manual inspection"]
    fn render_audio_cards() {
        let group: DupeGroup = (0..4u8)
            .map(|i| {
                let mut f = audio_file(i as usize);
                // Jagged per-byte values (mimicking a real BLAKE3 hash) so the
                // rendered glyphs look representative, not like smooth ramps.
                let mut h = [0u8; 32];
                for (j, b) in h.iter_mut().enumerate() {
                    let v = (i as u32 + 1).wrapping_mul(2_654_435_761)
                        ^ (j as u32).wrapping_mul(2_246_822_519);
                    *b = (v >> ((j as u32 % 6) * 4 + 3)) as u8;
                }
                f.entry.audio = Some(dedup_core::store::AudioFp {
                    duration_ms: 60_000 + i as u32 * 45_000,
                    chunk_hashes: vec![h],
                });
                f
            })
            .collect();

        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.results = Some(Results::Similar(vec![group]));

        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1120.0, 700.0))
            .wgpu()
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    let _ = &tmp;
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();
        let img = harness.render().expect("wgpu render failed");
        let out =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/dupes_audio.png");
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());
    }

    /// Renders the audio lightbox (two copies' waveforms, A/B compare) to
    /// `target/dupes_audio_viewer.png` for manual inspection. `--ignored`.
    #[test]
    #[ignore = "renders a PNG for manual inspection"]
    fn render_audio_viewer() {
        use std::f32::consts::PI;
        let tmp = tempfile::tempdir().unwrap();
        // Two real WAVs with opposite frequency sweeps (chirps) → the
        // spectrograms show diagonals running in opposite directions.
        let wav = |i: usize, up: bool| -> DupeFile {
            // Long enough (~45 s) that the STFT column-fold runs several times.
            let (sr, secs) = (16000u32, 45usize);
            let n = sr as usize * secs;
            let (f0, f1) = if up {
                (200.0f32, 1200.0)
            } else {
                (1200.0f32, 200.0)
            };
            let tt = secs as f32;
            let samples: Vec<i16> = (0..n)
                .map(|k| {
                    let t = k as f32 / sr as f32;
                    // Swept fundamental + two harmonics + a DC offset — enough
                    // structure and DC to exercise the dB/DC-drop spectrogram fix.
                    let phase = f0 * t + (f1 - f0) * t * t / (2.0 * tt);
                    let s = (2.0 * PI * phase).sin()
                        + 0.5 * (2.0 * PI * 2.0 * phase).sin()
                        + 0.3 * (2.0 * PI * 3.0 * phase).sin();
                    ((s * 0.3 + 0.2) * 24_000.0) as i16
                })
                .collect();
            let rel = format!("track{i}.wav");
            crate::waveform::write_wav(&tmp.path().join(&rel), sr, &samples);
            let mut hash = [0u8; 32];
            hash[0] = 0xB0 | i as u8;
            DupeFile {
                repo: "r".into(),
                repo_root: tmp.path().to_string_lossy().into_owned(),
                rel_path: rel,
                entry: dedup_core::store::FileEntry {
                    size: (n * 2) as u64,
                    hash,
                    modified_ms: 0,
                    missing: false,
                    mime: Some("audio/x-wav".into()),
                    img_fingerprint: None,
                    video_hash: None,
                    pdf_hash: None,
                    audio: Some(dedup_core::store::AudioFp {
                        duration_ms: (secs as u32) * 1000,
                        chunk_hashes: vec![hash],
                    }),
                    img_size: None,
                    origin: None,
                    exif: None,
                },
            }
        };
        let group: DupeGroup = vec![wav(0, true), wav(1, false)];

        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.results = Some(Results::Similar(vec![group]));

        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let store2 = Arc::clone(&store);
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1120.0, 640.0))
            .wgpu()
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    let _ = &tmp;
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();
        let ctx = egui::Context::default();
        harness
            .state_mut()
            .apply(&ctx, &store2, Act::OpenLightbox(0, 0));
        harness.step();
        harness.get_by_label_contains("SHOW B").click();
        // Give the background workers time to decode both WAVs into spectrograms.
        for _ in 0..60 {
            std::thread::sleep(std::time::Duration::from_millis(10));
            harness.step();
        }
        let img = harness.render().expect("wgpu render failed");
        let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/dupes_audio_viewer.png");
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());
    }

    /// Renders the view in SIMILAR mode (threshold slider visible) to
    /// `target/dupes_similar.png`. Run with `--ignored`.
    #[test]
    #[ignore = "renders a PNG for manual inspection"]
    fn render_dupes_similar() {
        let (_tmp, store) = sample_store(&SAMPLE_REPOS);
        let mut view = DupesView::new();
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1120.0, 360.0))
            .wgpu()
            .build_ui(move |ui| {
                if !init {
                    crate::icon::install(ui.ctx());
                    crate::theme::apply(ui.ctx(), crate::theme::DARK);
                    init = true;
                }
                view.show(ui, &store, TooltipVerbosity::default());
            });
        harness.run();
        harness.get_by_label("SIMILAR").click();
        harness.run();
        let img = harness.render().expect("wgpu render failed");
        let out =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/dupes_similar.png");
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());
    }

    /// Renders populated results with worse copies pre-marked and Quick Delete
    /// on (so DELETE NOW shows) to `target/dupes_populated.png`. `--ignored`.
    #[test]
    #[ignore = "renders a PNG for manual inspection"]
    fn render_dupes_populated() {
        let (_tmp, store) = seeded_store(3);
        let plan = plan_exact_duplicates(&store, &["repo".to_string()], |_| {}).unwrap();
        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.repos = vec![RepoSel {
            name: "repo".into(),
            included: true,
            read_only: false,
            is_main: false,
        }];
        view.result_names = vec!["repo".to_string()];
        view.results = Some(Results::Exact(plan));
        view.quick_delete = true;

        let store_ui = Arc::clone(&store);
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1120.0, 620.0))
            .wgpu()
            .build_ui_state(
                move |ui, view: &mut DupesView| {
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
        let img = harness.render().expect("wgpu render failed");
        let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/dupes_populated.png");
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());
    }

    /// A read-only repo's file shows the protected mark pill — disabled and
    /// struck through — in the shared viewer, exactly as the cards do.
    #[test]
    fn overview_mark_pill_is_protected_for_a_read_only_repo() {
        use egui_kittest::Harness;
        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.repos = vec![RepoSel {
            name: "ro".into(),
            included: true,
            read_only: true,
            is_main: false,
        }];
        view.results = Some(Results::Similar(vec![vec![
            dfile("ro", "best.png"),
            dfile("ro", "worse.png"),
        ]]));

        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let store2 = Arc::clone(&store);
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1200.0, 800.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    let _ = &tmp;
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();
        let ctx = egui::Context::default();
        harness
            .state_mut()
            .apply(&ctx, &store2, Act::OpenLightbox(0, 0));
        harness.run();
        assert!(
            harness.query_by_label("DELETE (Protected)").is_some(),
            "a read-only repo's file shows the protected mark label in the viewer"
        );
    }

    /// Renders the open lightbox over a real on-disk image to
    /// `target/lightbox.png` for manual inspection. `--ignored` (needs wgpu).
    #[test]
    #[ignore = "renders a PNG for manual inspection"]
    fn render_lightbox() {
        // Real images on disk so the thumb/full-res pipeline has something to
        // decode (the lightbox draws the actual pixels).
        let dir = tempfile::tempdir().unwrap();
        // Dedicated cache so the test never writes into the user's real one.
        dedup_core::thumbnail::set_cache_dir(dir.path().join("thumbs"));
        // Real photos when doc media is available (the A/B compare then shows
        // genuine images, not gradients); synthetic gradients otherwise.
        let real: [&str; 3] = [
            "IMG_2019_field.jpg",
            "wallpaper_spacehulk.jpg",
            "bebop_blue.jpg",
        ];
        let mut group: DupeGroup = Vec::new();
        for i in 0..3u8 {
            let (rel, real_ok) = if crate::doc_media::available() {
                let name = real[i as usize];
                (
                    name.to_string(),
                    crate::doc_media::place(name, &dir.path().join(name)),
                )
            } else {
                (format!("photo{i}.png"), false)
            };
            let path = dir.path().join(&rel);
            if !real_ok {
                image::RgbImage::from_fn(640, 480, |x, y| {
                    image::Rgb([x as u8, y as u8, (i as u32 * 60) as u8])
                })
                .save(&path)
                .unwrap();
            }
            let mut hash = [0u8; 32];
            hash[0] = i;
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(1000);
            group.push(DupeFile {
                repo: "r".into(),
                repo_root: dir.path().to_string_lossy().into_owned(),
                rel_path: rel,
                entry: dedup_core::store::FileEntry {
                    size,
                    hash,
                    modified_ms: 0,
                    missing: false,
                    mime: Some(if real_ok { "image/jpeg" } else { "image/png" }.into()),
                    img_fingerprint: None,
                    video_hash: None,
                    pdf_hash: None,
                    audio: None,
                    // Real media: let the decoded texture set the aspect.
                    img_size: (!real_ok).then_some((640, 480)),
                    origin: None,
                    exif: None,
                },
            });
        }

        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.results = Some(Results::Similar(vec![group]));

        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let store2 = Arc::clone(&store);
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1000.0, 720.0))
            .wgpu()
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    let _ = (&tmp, &dir);
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();
        let ctx = egui::Context::default();
        harness
            .state_mut()
            .apply(&ctx, &store2, Act::OpenLightbox(0, 0));
        // Several frames with pauses so the background decode lands; first the
        // single (hidden-B) view, which must show exactly one pane.
        for _ in 0..12 {
            harness.run();
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        let img = harness.render().expect("wgpu render failed");
        let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/lightbox_single.png");
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());

        harness.get_by_label_contains("SHOW B").click(); // render A/B side-by-side
        for _ in 0..12 {
            harness.run();
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        harness.run();
        let img = harness.render().expect("wgpu render failed");
        let out =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/lightbox.png");
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());
    }

    /// Renders the video lightbox (filmstrip + enlarged scrubbed frame) over a
    /// real ffmpeg-generated clip to `target/lightbox_video.png`. `--ignored`.
    #[test]
    #[ignore = "renders a PNG for manual inspection (needs ffmpeg + wgpu)"]
    fn render_video_lightbox() {
        if !dedup_core::fingerprint::ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        // Dedicated cache so the test never writes into the user's real one.
        dedup_core::thumbnail::set_cache_dir(dir.path().join("thumbs"));
        let clip = dir.path().join("clip.mp4");
        let ok = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg("testsrc=duration=4:size=320x240:rate=15")
            .args(["-pix_fmt", "yuv420p"])
            .arg(&clip)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            eprintln!("skipping: ffmpeg could not generate the clip");
            return;
        }

        let mut hash = [0u8; 32];
        hash[0] = 0x5a;
        let file = DupeFile {
            repo: "r".into(),
            repo_root: dir.path().to_string_lossy().into_owned(),
            rel_path: "clip.mp4".into(),
            entry: dedup_core::store::FileEntry {
                size: 1000,
                hash,
                modified_ms: 0,
                missing: false,
                mime: Some("video/mp4".into()),
                img_fingerprint: None,
                video_hash: None,
                pdf_hash: None,
                audio: None,
                img_size: None,
                origin: None,
                exif: None,
            },
        };

        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.results = Some(Results::Similar(vec![vec![file]]));

        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let store2 = Arc::clone(&store);
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1000.0, 720.0))
            .wgpu()
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    let _ = (&tmp, &dir);
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();
        let ctx = egui::Context::default();
        harness
            .state_mut()
            .apply(&ctx, &store2, Act::OpenLightbox(0, 0));
        // Give the ffmpeg extraction workers time to produce the still.
        for _ in 0..30 {
            harness.step();
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
        let img = harness.render().expect("wgpu render failed");
        let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/lightbox_video.png");
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());
    }

    /// Absolute path to `docs/screenshots/<name>`, creating the directory if
    /// needed. Kept separate from the `render_*`/snapshot tests above (which
    /// are for manual inspection/regression) — these are the doc screenshots
    /// referenced from `docs/gui/*.md`.
    fn doc_screenshot_path(name: &str) -> PathBuf {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/screenshots");
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    /// Doc screenshot: the Duplicate Management tab with a populated result
    /// set (worse copies pre-marked, Quick Delete on) to
    /// `docs/screenshots/duplicates_tab.png`. Run with `--ignored`.
    #[test]
    #[ignore = "generates a doc screenshot (needs wgpu)"]
    fn doc_screenshot_duplicates_tab() {
        let (repo, (_tmp, store)) = match seeded_media_store() {
            Some(s) => ("Automatic Upload".to_string(), s),
            None => ("repo".to_string(), seeded_store(3)),
        };
        let plan = plan_exact_duplicates(&store, std::slice::from_ref(&repo), |_| {}).unwrap();
        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.repos = vec![RepoSel {
            name: repo.clone(),
            included: true,
            read_only: false,
            is_main: false,
        }];
        view.result_names = vec![repo];
        view.results = Some(Results::Exact(plan));
        view.quick_delete = true;

        let store_ui = Arc::clone(&store);
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1120.0, 900.0))
            .wgpu()
            .build_ui_state(
                move |ui, view: &mut DupesView| {
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
        // Card thumbnails decode on background workers and upload over frames;
        // pump the harness so the grid shows real photos, not placeholders.
        for _ in 0..40 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            harness.step();
        }
        let img = harness.render().expect("wgpu render failed");
        let out = doc_screenshot_path("duplicates_tab.png");
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());
    }

    /// Doc screenshot: the lightbox in A/B compare (side-by-side) mode to
    /// `docs/screenshots/lightbox_compare.png`. Run with `--ignored`.
    #[test]
    #[ignore = "generates a doc screenshot (needs wgpu)"]
    fn doc_screenshot_lightbox_compare() {
        let dir = tempfile::tempdir().unwrap();
        dedup_core::thumbnail::set_cache_dir(dir.path().join("thumbs"));
        // Real photos when doc media is available (the A/B compare then shows
        // genuine images, not gradients); synthetic gradients otherwise.
        let real: [&str; 3] = [
            "IMG_2019_field.jpg",
            "wallpaper_spacehulk.jpg",
            "bebop_blue.jpg",
        ];
        let mut group: DupeGroup = Vec::new();
        for i in 0..3u8 {
            let (rel, real_ok) = if crate::doc_media::available() {
                let name = real[i as usize];
                (
                    name.to_string(),
                    crate::doc_media::place(name, &dir.path().join(name)),
                )
            } else {
                (format!("photo{i}.png"), false)
            };
            let path = dir.path().join(&rel);
            if !real_ok {
                image::RgbImage::from_fn(640, 480, |x, y| {
                    image::Rgb([x as u8, y as u8, (i as u32 * 60) as u8])
                })
                .save(&path)
                .unwrap();
            }
            let mut hash = [0u8; 32];
            hash[0] = i;
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(1000);
            group.push(DupeFile {
                repo: "r".into(),
                repo_root: dir.path().to_string_lossy().into_owned(),
                rel_path: rel,
                entry: dedup_core::store::FileEntry {
                    size,
                    hash,
                    modified_ms: 0,
                    missing: false,
                    mime: Some(if real_ok { "image/jpeg" } else { "image/png" }.into()),
                    img_fingerprint: None,
                    video_hash: None,
                    pdf_hash: None,
                    audio: None,
                    // Real media: let the decoded texture set the aspect.
                    img_size: (!real_ok).then_some((640, 480)),
                    origin: None,
                    exif: None,
                },
            });
        }

        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.results = Some(Results::Similar(vec![group]));

        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let store2 = Arc::clone(&store);
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1000.0, 720.0))
            .wgpu()
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    let _ = (&tmp, &dir);
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();
        let ctx = egui::Context::default();
        harness
            .state_mut()
            .apply(&ctx, &store2, Act::OpenLightbox(0, 0));
        harness.run();
        harness.get_by_label_contains("SHOW B").click();
        // Both sides decode off-thread; pump enough for the larger side too.
        for _ in 0..40 {
            harness.run();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        harness.run();
        let img = harness.render().expect("wgpu render failed");
        let out = doc_screenshot_path("lightbox_compare.png");
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());
    }

    /// Doc screenshot: two clips A/B compared with the shared frame scrubber, to
    /// `docs/screenshots/video_compare.png`. Builds real clips with ffmpeg and
    /// extracts their stills, so it needs ffmpeg on PATH. Run with `--ignored`.
    #[test]
    #[ignore = "generates a doc screenshot (needs wgpu + ffmpeg)"]
    fn doc_screenshot_video_compare() {
        let dir = tempfile::tempdir().unwrap();
        dedup_core::thumbnail::set_cache_dir(dir.path().join("thumbs"));

        // Two short, animated test-pattern clips (a near-duplicate feel), created
        // with ffmpeg's lavfi sources.
        let make = |rel: &str, src: &str| {
            let out = dir.path().join(rel);
            let status = std::process::Command::new("ffmpeg")
                .args([
                    "-y",
                    "-loglevel",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    src,
                    "-pix_fmt",
                    "yuv420p",
                    out.to_str().unwrap(),
                ])
                .status()
                .expect("run ffmpeg");
            assert!(status.success(), "ffmpeg failed to build {rel}");
        };
        // Real clips when doc media is available (genuine filmstrips and
        // scrubbed frames); animated test patterns otherwise.
        let clips: Vec<(String, &str)> = if crate::doc_media::available()
            && crate::doc_media::place("kitten.mp4", &dir.path().join("kitten.mp4"))
            && crate::doc_media::place("lynx.webm", &dir.path().join("lynx.webm"))
        {
            vec![
                ("kitten.mp4".into(), "video/mp4"),
                ("lynx.webm".into(), "video/webm"),
            ]
        } else {
            make("clip0.mp4", "testsrc=duration=1:size=480x360:rate=8");
            make("clip1.mp4", "testsrc2=duration=1:size=480x360:rate=8");
            vec![
                ("clip0.mp4".into(), "video/mp4"),
                ("clip1.mp4".into(), "video/mp4"),
            ]
        };

        let mut group: DupeGroup = Vec::new();
        for (i, (rel, mime)) in clips.iter().enumerate() {
            let mut hash = [0u8; 32];
            hash[0] = i as u8;
            let size = std::fs::metadata(dir.path().join(rel))
                .map(|m| m.len())
                .unwrap_or(1000);
            group.push(DupeFile {
                repo: "r".into(),
                repo_root: dir.path().to_string_lossy().into_owned(),
                rel_path: rel.to_string(),
                entry: dedup_core::store::FileEntry {
                    size,
                    hash,
                    modified_ms: 0,
                    missing: false,
                    mime: Some((*mime).into()),
                    img_fingerprint: None,
                    video_hash: None,
                    pdf_hash: None,
                    audio: None,
                    img_size: None,
                    origin: None,
                    exif: None,
                },
            });
        }

        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.results = Some(Results::Similar(vec![group]));

        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let store2 = Arc::clone(&store);
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1000.0, 760.0))
            .wgpu()
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    let _ = (&tmp, &dir);
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();
        let ctx = egui::Context::default();
        harness
            .state_mut()
            .apply(&ctx, &store2, Act::OpenLightbox(0, 0));
        harness.run();
        harness.get_by_label_contains("SHOW B").click();
        // Stills extract on worker threads (an ffmpeg call each), so step and
        // wait until both panes have filled in.
        for _ in 0..120 {
            harness.step();
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
        harness.step();
        let img = harness.render().expect("wgpu render failed");
        let out = doc_screenshot_path("video_compare.png");
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());
    }

    // ---- Metadata and Text tabs (roadmap §1.3.1–1.3.2: the viewer dispatches
    // on the selected representation, not on the file's mime) -----------------

    /// A harness over `view` with its own throwaway store, the setup every
    /// lightbox test repeats.
    fn lightbox_harness(
        view: DupesView,
        size: egui::Vec2,
    ) -> egui_kittest::Harness<'static, DupesView> {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let mut init = false;
        egui_kittest::Harness::builder()
            .with_size(size)
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    let _ = &tmp;
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            )
    }

    /// Open the shared viewer on group `gi`, member `fi`, through the same act
    /// a card click pushes. Steps fixed frames rather than running to a settled
    /// state: the viewer's background decodes wake the UI on their own schedule.
    fn open_viewer(harness: &mut egui_kittest::Harness<'static, DupesView>, gi: usize, fi: usize) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let ctx = egui::Context::default();
        harness
            .state_mut()
            .apply(&ctx, &store, Act::OpenLightbox(gi, fi));
        for _ in 0..4 {
            harness.step();
        }
    }

    /// Click the (unique) control containing `label` and settle a few frames.
    fn click_and_step(harness: &mut egui_kittest::Harness<'static, DupesView>, label: &str) {
        harness.get_by_label_contains(label).click();
        for _ in 0..4 {
            harness.step();
        }
    }

    /// A real (tiny) MP3 at `dir/rel` carrying `title`, plus the `DupeFile` that
    /// addresses it.
    fn tagged_mp3(dir: &Path, rel: &str, i: u8, title: &str) -> DupeFile {
        let path = dir.join(rel);
        crate::id3tags::write_bare_mp3(&path);
        crate::id3tags::write(
            &path,
            &Tags {
                title: title.into(),
                artist: "Cohen".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let mut hash = [0u8; 32];
        hash[0] = i;
        DupeFile {
            repo: "r".into(),
            repo_root: dir.to_string_lossy().into_owned(),
            rel_path: rel.into(),
            entry: dedup_core::store::FileEntry {
                size: 417,
                hash,
                modified_ms: 0,
                missing: false,
                mime: Some("audio/mpeg".into()),
                img_fingerprint: None,
                video_hash: None,
                pdf_hash: None,
                audio: Some(dedup_core::store::AudioFp {
                    duration_ms: 1000,
                    chunk_hashes: Vec::new(),
                }),
                img_size: None,
                origin: None,
                exif: None,
            },
        }
    }

    /// A file of `bytes` at `dir/rel` with `mime`, plus its `DupeFile`.
    fn plain_file(dir: &Path, rel: &str, i: u8, mime: &str, bytes: &[u8]) -> DupeFile {
        std::fs::write(dir.join(rel), bytes).unwrap();
        let mut hash = [0u8; 32];
        hash[0] = i;
        DupeFile {
            repo: "r".into(),
            repo_root: dir.to_string_lossy().into_owned(),
            rel_path: rel.into(),
            entry: dedup_core::store::FileEntry {
                size: bytes.len() as u64,
                hash,
                modified_ms: 0,
                missing: false,
                mime: Some(mime.into()),
                img_fingerprint: None,
                video_hash: None,
                pdf_hash: None,
                audio: None,
                img_size: None,
                origin: None,
                exif: None,
            },
        }
    }

    /// Reaching the tag editor the way a user does — the `Metadata` tab →
    /// EDIT TAGS → SAVE — writes the file. The tab is *clicked*, not set on the
    /// state, so this covers the dispatch as well as the screen.
    #[test]
    fn metadata_tab_is_clicked_into_and_saves_tags() {
        let dir = tempfile::tempdir().unwrap();
        let group: DupeGroup = vec![
            tagged_mp3(dir.path(), "a.mp3", 1, "Old"),
            tagged_mp3(dir.path(), "b.mp3", 2, "Other"),
        ];
        let mp3 = dir.path().join("a.mp3");

        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.results = Some(Results::Similar(vec![group]));
        let mut harness = lightbox_harness(view, egui::vec2(1200.0, 800.0));
        harness.run();
        open_viewer(&mut harness, 0, 0);

        click_and_step(&mut harness, "Metadata");
        assert_eq!(
            harness.state().lightbox.as_ref().map(|l| l.tab),
            Some(crate::lightbox::RepresentationKind::Metadata),
            "clicking the Metadata tab selects it"
        );
        assert!(
            harness.query_all_by_label("Old").count() > 0,
            "the tab shows the stored title before any editing"
        );

        click_and_step(&mut harness, "EDIT TAGS");
        assert!(
            harness
                .state()
                .lightbox
                .as_ref()
                .is_some_and(|lb| lb.tag_edit.is_some()),
            "EDIT TAGS opens the editor on this copy"
        );
        harness
            .state_mut()
            .lightbox
            .as_mut()
            .unwrap()
            .tag_edit
            .as_mut()
            .unwrap()
            .tags
            .title = "New Title".into();
        for _ in 0..2 {
            harness.step();
        }
        click_and_step(&mut harness, "SAVE TAGS");

        let saved = crate::id3tags::read(&mp3).expect("tags still readable");
        assert_eq!(saved.title, "New Title", "the edit is written to disk");
        assert_eq!(saved.artist, "Cohen", "other tags are preserved");
        assert!(
            harness
                .state()
                .lightbox
                .as_ref()
                .is_some_and(|lb| lb.tag_edit.is_none()),
            "the editor closes on save"
        );
        assert!(
            harness.state().lightbox.is_some(),
            "saving keeps the lightbox open"
        );
    }

    /// §1.3.1: only the column(s) that support the representation are drawn.
    /// Two ID3 containers give two side-by-side (non-overlapping) columns; an
    /// untagged FLAC as B leaves A alone on screen instead of an empty half.
    #[test]
    fn metadata_tab_draws_only_the_sides_that_have_metadata() {
        let dir = tempfile::tempdir().unwrap();

        // Both sides ID3-capable → two columns, laid out left | right.
        let group: DupeGroup = vec![
            tagged_mp3(dir.path(), "a.mp3", 1, "Alpha"),
            tagged_mp3(dir.path(), "b.mp3", 2, "Beta"),
        ];
        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.results = Some(Results::Similar(vec![group]));
        let mut harness = lightbox_harness(view, egui::vec2(1000.0, 720.0));
        harness.run();
        open_viewer(&mut harness, 0, 0);
        click_and_step(&mut harness, "SHOW B");
        click_and_step(&mut harness, "Metadata");

        let rects: Vec<egui::Rect> = harness
            .query_all_by_label_contains("EDIT TAGS")
            .map(|n| n.rect())
            .collect();
        assert_eq!(rects.len(), 2, "both tagged sides get their own column");
        let (left, right) = if rects[0].left() <= rects[1].left() {
            (rects[0], rects[1])
        } else {
            (rects[1], rects[0])
        };
        assert!(
            left.right() <= right.left(),
            "columns sit side by side without overlapping: {left:?} vs {right:?}"
        );
        assert!(
            right.right() <= 1000.0,
            "the right column stays inside the window: {right:?}"
        );

        // B is a FLAC: no ID3 container, so no B column at all.
        let flac = plain_file(dir.path(), "b.flac", 3, "audio/flac", b"fLaC\0\0\0\0");
        let group: DupeGroup = vec![tagged_mp3(dir.path(), "a.mp3", 1, "Alpha"), flac];
        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.results = Some(Results::Similar(vec![group]));
        let mut harness = lightbox_harness(view, egui::vec2(1000.0, 720.0));
        harness.run();
        open_viewer(&mut harness, 0, 0);
        click_and_step(&mut harness, "SHOW B");
        click_and_step(&mut harness, "Metadata");
        assert_eq!(
            harness.query_all_by_label_contains("EDIT TAGS").count(),
            1,
            "a side without metadata contributes no column"
        );
    }

    /// A read-only repo gets no edit affordance on the Metadata tab (§1.3.5):
    /// no EDIT TAGS button, and the reason is spelled out instead.
    #[test]
    fn metadata_tab_offers_no_editing_in_a_read_only_repo() {
        let dir = tempfile::tempdir().unwrap();
        let group: DupeGroup = vec![tagged_mp3(dir.path(), "a.mp3", 1, "Alpha")];
        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.repos = vec![RepoSel {
            name: "r".into(),
            included: true,
            read_only: true,
            is_main: false,
        }];
        view.results = Some(Results::Similar(vec![group]));
        let mut harness = lightbox_harness(view, egui::vec2(1000.0, 720.0));
        harness.run();
        open_viewer(&mut harness, 0, 0);
        click_and_step(&mut harness, "Metadata");

        assert_eq!(
            harness.query_all_by_label_contains("EDIT TAGS").count(),
            0,
            "a read-only repo offers no tag editing"
        );
        assert!(
            harness
                .query_all_by_label_contains("Read-only repository")
                .count()
                > 0,
            "and says why"
        );
    }

    /// Non-media duplicates (documents, archives) reach a Text tab — the
    /// representation that gives them a lightbox at all. A single file previews
    /// its head as decoded text (or a hex dump); revealing the second side turns
    /// the tab into the full-file, aligned hex diff of the two.
    #[test]
    fn text_tab_shows_words_and_hex_tab_shows_the_byte_diff() {
        let dir = tempfile::tempdir().unwrap();
        let group: DupeGroup = vec![
            plain_file(dir.path(), "notes.txt", 1, "text/plain", b"hello alpha"),
            plain_file(
                dir.path(),
                "doc.pdf",
                2,
                "application/pdf",
                &[0x25, 0x50, 0x44, 0x46, 0xff, 0xfe, 0x00, 0x01],
            ),
        ];
        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.results = Some(Results::Similar(vec![group]));
        let mut harness = lightbox_harness(view, egui::vec2(1000.0, 720.0));
        harness.run();
        open_viewer(&mut harness, 0, 0);

        harness.get_by_label_contains("Text").click();
        harness.run();
        assert!(
            harness.query_all_by_label_contains("hello alpha").count() > 0,
            "A's readable text is previewed on the Text tab"
        );

        // Raw bytes live on the Hex tab now: with B revealed it is the aligned,
        // paginated hex diff of the two files — equal bytes lined up, marked.
        harness.get_by_label_contains("SHOW B").click();
        harness.run();
        harness.get_by_label_contains("Hex").click();
        harness.run();
        assert!(
            harness.query_all_by_label_contains("page 1 /").count() > 0,
            "the Hex tab is the paginated hex diff of the two files"
        );
        assert!(
            harness.query_all_by_label_contains("NEXT DIFF").count() > 0,
            "and it offers to jump to the difference"
        );
    }

    /// A group of documents has no thumbnail to click, so the card's typed
    /// placeholder is the way in: clicking it opens the lightbox, which offers
    /// the Text tab. Without this the Text representation would have no entry
    /// point at all.
    #[test]
    fn a_document_cards_placeholder_opens_the_lightbox() {
        let dir = tempfile::tempdir().unwrap();
        let group: DupeGroup = vec![
            plain_file(
                dir.path(),
                "a.pdf",
                1,
                "application/pdf",
                b"%PDF-1.4\x00 one",
            ),
            plain_file(
                dir.path(),
                "b.pdf",
                2,
                "application/pdf",
                b"%PDF-1.4\x00 two",
            ),
        ];
        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.results = Some(Results::Similar(vec![group]));
        let mut harness = lightbox_harness(view, egui::vec2(1000.0, 720.0));
        harness.run();
        assert!(
            harness.state().lightbox.is_none(),
            "no lightbox open to start with"
        );

        // Both copies offer one; the first is A's card.
        let open_a = harness
            .query_all_by_label_contains("OPEN PREVIEW")
            .next()
            .expect("a document card offers a way into the lightbox");
        open_a.click();
        harness.run();
        assert!(
            harness.state().lightbox.is_some(),
            "clicking a document's placeholder opens the lightbox"
        );

        harness.get_by_label_contains("Text").click();
        harness.run();
        assert_eq!(
            harness.state().lightbox.as_ref().map(|l| l.tab),
            Some(crate::lightbox::RepresentationKind::Text),
            "and the Text tab is offered there"
        );
        assert!(
            harness
                .query_all_by_label_contains("Nothing readable")
                .count()
                > 0,
            "an unreadable PDF shows the empty-state note here, not its raw bytes \
             (the byte view lives on its own tab)"
        );
    }

    /// A stepped side can leave the selected tab with no file behind it.
    /// Rather than a blank overlay, the viewer falls back to the pair's own
    /// (native) representation.
    #[test]
    fn an_unsupported_tab_falls_back_to_overview() {
        let group: DupeGroup = (0..2u8)
            .map(|i| {
                let mut f = dfile("r", &format!("photo{i}.png"));
                f.entry.hash[0] = i;
                f.entry.mime = Some("image/png".into());
                f.entry.img_size = Some((640, 480));
                f
            })
            .collect();
        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.results = Some(Results::Similar(vec![group]));
        let mut harness = lightbox_harness(view, egui::vec2(1000.0, 720.0));
        harness.run();
        open_viewer(&mut harness, 0, 0);
        // Audio is a tab an image pair does not offer.
        harness.state_mut().lightbox.as_mut().unwrap().tab =
            crate::lightbox::RepresentationKind::Audio;
        harness.run();

        assert_eq!(
            harness.state().lightbox.as_ref().map(|l| l.tab),
            Some(crate::lightbox::RepresentationKind::Image),
            "a tab the pair does not offer falls back to its native representation"
        );
    }

    /// Doc screenshots of the two new tabs — rendered, not just label-queried,
    /// because a label query cannot see a column overlapping its neighbour.
    /// Run with `--ignored`.
    #[test]
    #[ignore = "generates a doc screenshot (needs wgpu)"]
    fn doc_screenshot_lightbox_metadata_and_text() {
        let dir = tempfile::tempdir().unwrap();
        let group: DupeGroup = vec![
            tagged_mp3(dir.path(), "chelsea-1974.mp3", 1, "Chelsea Hotel"),
            tagged_mp3(dir.path(), "chelsea-remaster.mp3", 2, "Chelsea Hotel #2"),
        ];
        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.results = Some(Results::Similar(vec![group]));

        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1000.0, 720.0))
            .wgpu()
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    let _ = &tmp;
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();
        open_viewer(&mut harness, 0, 0);
        harness.get_by_label_contains("SHOW B").click();
        harness.run();
        harness.get_by_label_contains("Metadata").click();
        for _ in 0..6 {
            harness.run();
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        // With one side's editor open, so the screenshot shows both the
        // read-only and the editable state of the same tab (both columns offer
        // EDIT TAGS; the first is A's).
        if let Some(edit_a) = harness.query_all_by_label_contains("EDIT TAGS").next() {
            edit_a.click();
        }
        for _ in 0..6 {
            harness.step();
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        let img = harness.render().expect("wgpu render failed");
        let out = doc_screenshot_path("lightbox_metadata.png");
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());

        // The Text tab, on a text/binary pair.
        let text_dir = tempfile::tempdir().unwrap();
        let group: DupeGroup = vec![
            plain_file(
                text_dir.path(),
                "readme.md",
                1,
                "text/markdown",
                b"# Inheritance notes\n\nTwo copies of this file were found.\n",
            ),
            plain_file(
                text_dir.path(),
                "scan.pdf",
                2,
                "application/pdf",
                b"%PDF-1.4\x00\x01\x02 stream ... binary payload ...",
            ),
        ];
        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.results = Some(Results::Similar(vec![group]));

        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1000.0, 720.0))
            .wgpu()
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    let _ = (&tmp, &text_dir);
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();
        open_viewer(&mut harness, 0, 0);
        harness.get_by_label_contains("SHOW B").click();
        harness.run();
        harness.get_by_label_contains("Text").click();
        for _ in 0..6 {
            harness.run();
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        let img = harness.render().expect("wgpu render failed");
        let out = doc_screenshot_path("lightbox_text.png");
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());
    }
}
