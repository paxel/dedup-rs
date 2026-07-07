//! The Duplicate Management tab: choose repos (optionally read-only), find exact
//! duplicates or perceptual similars, review paged groups with thumbnails, and
//! delete the worse copies — batched per repo, never without a confirmation.

use crate::icon;
use crate::theme;
use crate::thumbs::ThumbCache;
use crate::util::{format_mtime, format_size};
use crossbeam_channel::{Receiver, Sender};
use dedup_core::dupes::{
    DupeDeleteStats, DupeFile, DupeGroup, DupeGroupKey, delete_paths, load_groups,
    plan_exact_duplicates, wasted_bytes,
};
use dedup_core::similar::find_similar;
use dedup_core::store::Store;
use dedup_core::thumbnail::hash_hex;
use egui::{Color32, Id, RichText};
use std::collections::HashSet;
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

/// A bold-bordered LCARS section container in the given accent color, used to
/// group a row of related controls.
fn section(color: Color32) -> egui::Frame {
    egui::Frame::new()
        .fill(theme::PANEL)
        .corner_radius(theme::PILL)
        .stroke(egui::Stroke::new(2.0, color))
        .inner_margin(8.0)
        .outer_margin(egui::Margin {
            left: 0,
            right: 0,
            top: 0,
            bottom: 8,
        })
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
    ReloadRepos,
    Find,
    ToggleMark(FileKey),
    ToggleQuickDelete,
    AutoResolve,
    DeleteGroup(usize),
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
    /// Pages whose non-best copies have already been marked by default, so
    /// revisiting a page doesn't clobber the user's manual KEEP/DELETE choices.
    preselected_pages: HashSet<usize>,
    /// Group indices deleted this session (rendered collapsed). Reset on FIND.
    resolved: HashSet<usize>,
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
}

impl DupesView {
    pub fn new() -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        Self {
            repos: Vec::new(),
            repos_loaded: false,
            mode: Mode::Exact,
            threshold: 90.0,
            results: None,
            result_names: Vec::new(),
            page_groups: Vec::new(),
            cached_page: None,
            group_heights: Vec::new(),
            marked: HashSet::new(),
            preselected_pages: HashSet::new(),
            resolved: HashSet::new(),
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
        }
    }

    /// Total number of result groups (0 if no search yet).
    fn total_groups(&self) -> usize {
        self.results.as_ref().map_or(0, Results::len)
    }

    pub fn show(&mut self, ui: &mut egui::Ui, store: &Arc<Store>) {
        let ctx = ui.ctx().clone();
        if self.thumbs.poll(&ctx) {
            ctx.request_repaint();
        }
        self.drain_messages(store, &ctx);
        if !self.repos_loaded {
            self.load_repos(store);
        }

        let mut acts: Vec<Act> = Vec::new();

        ui.add_space(6.0);
        ui.label(
            RichText::new("DUPLICATE MANAGEMENT")
                .color(theme::LILAC)
                .size(18.0)
                .strong(),
        );
        self.repo_bar(ui, &mut acts);
        self.controls(ui, &mut acts);
        if let Some(err) = &self.error {
            ui.colored_label(theme::RED, err);
        }
        if let Some(status) = &self.status {
            ui.label(RichText::new(status).color(theme::TAN).size(13.0));
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

        for act in acts {
            self.apply(&ctx, store, act);
        }

        // Keep the spinner/progress ticking while a background op runs.
        if self.busy.is_some() {
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
                            self.preselected_pages.clear();
                            self.resolved.clear();
                            self.page = 0;
                            self.cached_page = None;
                            self.error = None;
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

    fn load_repos(&mut self, store: &Store) {
        match store.list_repos() {
            Ok(list) => {
                let excluded: HashSet<String> = self
                    .repos
                    .iter()
                    .filter(|r| !r.included)
                    .map(|r| r.name.clone())
                    .collect();
                // Every (re)load re-locks all repos: read-only is the safe
                // default, so deleting duplicates is always a deliberate unlock.
                self.repos = list
                    .into_iter()
                    .map(|(name, _, _)| RepoSel {
                        included: !excluded.contains(&name),
                        read_only: true,
                        name,
                    })
                    .collect();
                self.repos_loaded = true;
                self.error = None;
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    fn repo_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        section(theme::LILAC).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("REPOS").color(theme::TEXT).size(12.0));
                // Top-align the chips. A centered row (`horizontal`/
                // `horizontal_wrapped`) places earlier items progressively higher
                // as the row height converges, leaving the first repo a few px
                // above the rest (see the `repo_row_is_aligned` test). Top-align
                // pins every chip to one line. It stays bounded because it's
                // nested inside this outer `horizontal`.
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Min), |ui| {
                    for (i, repo) in self.repos.iter().enumerate() {
                    // Name + lock read as one bordered unit per repo, with room
                    // between the border and the buttons.
                    egui::Frame::new()
                        .stroke(egui::Stroke::new(1.0, theme::BLUE))
                        .corner_radius(8)
                        .inner_margin(egui::Margin::symmetric(10, 6))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                let (fill, text) = if repo.included {
                                    (theme::ORANGE, theme::BLACK)
                                } else {
                                    (theme::PANEL, theme::TEXT)
                                };
                                if ui
                                    .add(
                                        egui::Button::new(RichText::new(&repo.name).color(text))
                                            .fill(fill),
                                    )
                                    .on_hover_text("Toggle whether this repo is searched")
                                    .clicked()
                                {
                                    acts.push(Act::ToggleInclude(i));
                                }
                                // Closed padlock = read-only (protected); open
                                // padlock = deletable.
                                let (glyph, ro_fill, ro_text, hover) = if repo.read_only {
                                    (
                                        icon::LOCK,
                                        theme::BLUE,
                                        theme::BLACK,
                                        "Locked: files here are protected from deletion — click to allow deleting",
                                    )
                                } else {
                                    (
                                        icon::LOCK_OPEN,
                                        theme::PANEL,
                                        theme::BLUE,
                                        "Unlocked: files here can be deleted — click to protect",
                                    )
                                };
                                if ui
                                    .add(
                                        egui::Button::new(RichText::new(glyph).color(ro_text))
                                            .fill(ro_fill),
                                    )
                                    .on_hover_text(hover)
                                    .clicked()
                                {
                                    acts.push(Act::ToggleRo(i));
                                }
                            });
                        });
                    ui.add_space(8.0);
                }
                // Inset the refresh button by the chips' frame margin so its top
                // lines up with the (inset) repo name buttons, not the chip tops.
                egui::Frame::new()
                    .inner_margin(egui::Margin {
                        left: 0,
                        right: 0,
                        top: 7,
                        bottom: 7,
                    })
                    .show(ui, |ui| {
                        let refresh = egui::Button::new(
                            RichText::new(format!("{} REFRESH", icon::REFRESH))
                                .color(theme::BLACK),
                        )
                        .fill(theme::LILAC);
                        if ui
                            .add(refresh)
                            .on_hover_text("Reload the repository list")
                            .clicked()
                        {
                            acts.push(Act::ReloadRepos);
                        }
                    });
                });
            });
        });
    }

    fn controls(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        section(theme::AMBER).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("MODE").color(theme::TEXT).size(12.0));
                let exact = self.mode == Mode::Exact;
                // The two match modes form one segmented toggle.
                egui::Frame::new()
                    .stroke(egui::Stroke::new(1.0, theme::BLUE))
                    .corner_radius(6)
                    .inner_margin(egui::Margin::symmetric(4, 2))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            // Selected = filled accent + black text; unselected =
                            // panel fill with accent-colored text (an outline),
                            // so both stay readable instead of black-on-black.
                            let (dup_fill, dup_text) = if exact {
                                (theme::ORANGE, theme::BLACK)
                            } else {
                                (theme::PANEL, theme::ORANGE)
                            };
                            if ui
                                .add(
                                    egui::Button::new(RichText::new("DUPLICATES").color(dup_text))
                                        .fill(dup_fill),
                                )
                                .on_hover_text("Exact byte-for-byte duplicates")
                                .clicked()
                            {
                                self.mode = Mode::Exact;
                            }
                            let (sim_fill, sim_text) = if exact {
                                (theme::PANEL, theme::LILAC)
                            } else {
                                (theme::LILAC, theme::BLACK)
                            };
                            if ui
                                .add(
                                    egui::Button::new(RichText::new("SIMILAR").color(sim_text))
                                        .fill(sim_fill),
                                )
                                .on_hover_text("Perceptually similar images/videos")
                                .clicked()
                            {
                                self.mode = Mode::Similar;
                            }
                        });
                    });
                let find = egui::Button::new(
                    RichText::new(format!("{} FIND", icon::SEARCH)).color(theme::BLACK),
                )
                .fill(theme::AMBER);
                if ui.add_enabled(self.busy.is_none(), find).clicked() {
                    acts.push(Act::Find);
                }
                // Progress while a background op runs.
                if let Some(op) = &self.busy {
                    ui.add(egui::Spinner::new().color(theme::AMBER));
                    let text = match op {
                        Op::Find(n) => format!("searching… {n} groups"),
                        Op::AutoResolve { done, total } => {
                            format!("auto-resolving… {done}/{total}")
                        }
                        Op::Delete => "deleting…".to_string(),
                    };
                    ui.label(RichText::new(text).color(theme::AMBER).size(12.0));
                }
            });

            // The similarity threshold gets its own row so the slider has room
            // to read as a slider (cramming it into the button row hid the track
            // behind the value box). The value box still accepts typed floats.
            if self.mode == Mode::Similar {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("similarity").color(theme::TEXT).size(12.0));
                    ui.add(
                        egui::Slider::new(&mut self.threshold, 50.0..=100.0)
                            .suffix("%")
                            .max_decimals(1),
                    );
                });
            }

            // Quick Delete: gives each group a DELETE NOW button that removes
            // its marked files instantly (no per-group confirmation).
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let (fill, text) = if self.quick_delete {
                    (theme::RED, theme::BLACK)
                } else {
                    (theme::PANEL, theme::RED)
                };
                let glyph = if self.quick_delete { icon::CHECK } else { icon::X };
                let qd = egui::Button::new(
                    RichText::new(format!("{glyph} QUICK DELETE")).color(text),
                )
                .fill(fill);
                if ui
                    .add(qd)
                    .on_hover_text(
                        "Show a DELETE NOW button on each group that deletes its marked files immediately, no confirmation",
                    )
                    .clicked()
                {
                    acts.push(Act::ToggleQuickDelete);
                }
                if self.quick_delete {
                    ui.label(
                        RichText::new("on — DELETE NOW removes files instantly")
                            .color(theme::RED)
                            .size(12.0),
                    );
                }
            });
        });

        if self.total_groups() > 0 {
            ui.horizontal(|ui| {
                let n = self.marked.len();
                let idle = self.busy.is_none();
                let auto =
                    egui::Button::new(RichText::new("AUTO-RESOLVE REST").color(theme::BLACK));
                if ui
                    .add_enabled(idle, auto)
                    .on_hover_text("Mark every non-best copy in a deletable repo")
                    .clicked()
                {
                    acts.push(Act::AutoResolve);
                }
                let del = egui::Button::new(
                    RichText::new(format!("DELETE MARKED ({n})")).color(theme::BLACK),
                )
                .fill(theme::RED);
                if ui.add_enabled(idle && n > 0, del).clicked() {
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
            ui.colored_label(theme::TEXT, msg);
            return;
        }

        let pages = total.div_ceil(PAGE_SIZE);
        let page = self.page.min(pages.saturating_sub(1));
        ui.horizontal(|ui| {
            if ui
                .add_enabled(page > 0, egui::Button::new(icon::CARET_LEFT))
                .clicked()
            {
                acts.push(Act::SetPage(page - 1));
            }
            ui.label(
                RichText::new(format!("page {}/{} · {} groups", page + 1, pages, total))
                    .color(theme::TAN),
            );
            if ui
                .add_enabled(page + 1 < pages, egui::Button::new(icon::CARET_RIGHT))
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

        // Default-mark this page's worse (non-best) copies once, so the extras
        // show DELETE by default. Read-only repos are never marked, and a page
        // is only preselected once so manual KEEP choices survive a revisit.
        if self.preselected_pages.insert(page) {
            let ro = self.read_only_names();
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
                .fill(theme::PANEL)
                .corner_radius(theme::PILL)
                .stroke(egui::Stroke::new(1.0, theme::TAN))
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
                            .color(theme::TAN)
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
        let size = group.first().map(|f| f.entry.size).unwrap_or(0);
        let wasted = wasted_bytes(&group);
        // Flags read before the render closure (which borrows `self` mutably).
        let quick = self.quick_delete;
        let idle = self.busy.is_none();
        let has_marked = group.iter().any(|f| self.marked.contains(&key(f)));
        egui::Frame::new()
            .fill(theme::PANEL)
            .corner_radius(theme::PILL)
            .stroke(egui::Stroke::new(1.5, theme::ORANGE))
            .inner_margin(10.0)
            .outer_margin(egui::Margin {
                left: 0,
                right: 0,
                top: 0,
                bottom: 8,
            })
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!(
                            "{count} copies · {} each · {} reclaimable",
                            format_size(size),
                            format_size(wasted)
                        ))
                        .color(theme::AMBER)
                        .strong(),
                    );
                    // Quick Delete: one-click removal of this group's marked files.
                    if quick && has_marked {
                        let del = egui::Button::new(
                            RichText::new(format!("{} DELETE NOW", icon::TRASH))
                                .color(theme::BLACK),
                        )
                        .fill(theme::RED);
                        if ui
                            .add_enabled(idle, del)
                            .on_hover_text("Delete this group's marked files now")
                            .clicked()
                        {
                            acts.push(Act::DeleteGroup(gi));
                        }
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    for (fi, file) in group.iter().enumerate() {
                        self.file_card(ui, file, fi == 0, acts);
                    }
                });
            });
    }

    fn file_card(
        &mut self,
        ui: &mut egui::Ui,
        file: &DupeFile,
        is_best: bool,
        acts: &mut Vec<Act>,
    ) {
        let k = key(file);
        let marked = self.marked.contains(&k);
        let ro = self.repo_is_ro(&file.repo);
        egui::Frame::new()
            .fill(theme::BLACK)
            .corner_radius(theme::PILL)
            .inner_margin(8.0)
            .outer_margin(egui::Margin::same(4))
            .show(ui, |ui| {
                ui.vertical(|ui| {
                    ui.set_width(200.0);
                    self.thumbnail(ui, file);
                    ui.label(
                        RichText::new(&file.rel_path)
                            .color(theme::TEXT)
                            .size(12.0)
                            .strong(),
                    );
                    ui.label(
                        RichText::new(format!("{} · {}", file.repo, format_size(file.entry.size)))
                            .color(theme::TAN)
                            .size(11.0),
                    );
                    let dims = file
                        .entry
                        .img_size
                        .map(|(w, h)| format!("{w}×{h}"))
                        .unwrap_or_else(|| "—".into());
                    ui.label(
                        RichText::new(format!("{dims} · {}", format_mtime(file.entry.modified_ms)))
                            .color(theme::TAN)
                            .size(11.0),
                    );

                    if is_best {
                        ui.label(
                            RichText::new(format!("{} BEST", icon::STAR))
                                .color(theme::BLUE)
                                .size(12.0)
                                .strong(),
                        );
                    }
                    if ro {
                        ui.label(RichText::new("read-only").color(theme::BLUE).size(11.0));
                    } else {
                        let (label, fill) = if marked {
                            ("DELETE ✓", theme::RED)
                        } else {
                            ("KEEP", theme::PANEL)
                        };
                        let color = if marked { theme::BLACK } else { theme::TEXT };
                        if ui
                            .add(egui::Button::new(RichText::new(label).color(color)).fill(fill))
                            .clicked()
                        {
                            acts.push(Act::ToggleMark(k.clone()));
                        }
                    }
                });
            });
    }

    fn thumbnail(&mut self, ui: &mut egui::Ui, file: &DupeFile) {
        let is_image = file
            .entry
            .mime
            .as_deref()
            .is_some_and(|m| m.starts_with("image/"));
        if is_image {
            // Only fetch a texture for on-screen cards. The results list is not
            // virtualized, so a page can lay out far more thumbnails than the GPU
            // texture cache holds; requesting every one each frame thrashes the
            // LRU (evict → re-decode → repaint), which spikes CPU and makes the
            // images flicker. Off-screen cards fall through to the placeholder.
            let thumb_rect =
                egui::Rect::from_min_size(ui.next_widget_position(), egui::vec2(160.0, 120.0));
            if ui.is_rect_visible(thumb_rect) {
                let hex = hash_hex(&file.entry.hash);
                let source = file.absolute_path();
                if let Some(tex) = self.thumbs.get(&hex, &source) {
                    ui.add(
                        egui::Image::new(egui::load::SizedTexture::from_handle(&tex))
                            .max_height(120.0)
                            .corner_radius(6),
                    );
                    return;
                }
            }
        }
        // Placeholder for non-images or not-yet-ready thumbnails.
        let label = file.entry.mime.clone().unwrap_or_else(|| "file".into());
        egui::Frame::new()
            .fill(theme::PANEL)
            .corner_radius(6)
            .inner_margin(18.0)
            .show(ui, |ui| {
                ui.set_width(160.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new(icon::IMAGE).color(theme::LILAC).size(28.0));
                    ui.label(RichText::new(label).color(theme::LILAC).size(11.0));
                });
            });
    }

    fn confirm_modal(&mut self, ui: &mut egui::Ui, prompt: &str, verb: &str, acts: &mut Vec<Act>) {
        egui::Modal::new(Id::new("dupes-confirm")).show(&ui.ctx().clone(), |ui| {
            ui.set_width(360.0);
            ui.label(
                RichText::new("CONFIRM")
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
                        egui::Button::new(RichText::new(verb).color(theme::BLACK)).fill(theme::RED),
                    )
                    .clicked()
                {
                    acts.push(Act::ConfirmDelete);
                }
                if ui
                    .button(RichText::new("CANCEL").color(theme::BLACK))
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
                    }
                }
            }
            Act::ReloadRepos => self.load_repos(store),
            Act::Find => self.start_find(store, ctx),
            Act::ToggleMark(k) => {
                if !self.marked.remove(&k) {
                    self.marked.insert(k);
                }
            }
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
            Act::SetPage(p) => self.page = p,
            Act::AskDelete => {
                let n = self.marked.len();
                if n > 0 {
                    self.confirm = Some((
                        format!(
                            "Delete {n} marked file{} from disk? This cannot be undone.",
                            if n == 1 { "" } else { "s" }
                        ),
                        ConfirmAction::DeleteAll,
                    ));
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

    fn included_names(&self) -> Vec<String> {
        self.repos
            .iter()
            .filter(|r| r.included)
            .map(|r| r.name.clone())
            .collect()
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
            let result = match mode {
                Mode::Exact => {
                    let tx2 = tx.clone();
                    let r = repaint.clone();
                    plan_exact_duplicates(&store, &names, move |n| {
                        let _ = tx2.send(Msg::FindProgress(n));
                        r.request_repaint();
                    })
                    .map(Results::Exact)
                    .map_err(|e| e.to_string())
                }
                Mode::Similar => find_similar(&store, &names, threshold)
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
}

#[cfg(test)]
mod ui_tests {
    use super::*;
    use dedup_core::store::Store;
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;
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
                    crate::theme::apply(ui.ctx());
                    init = true;
                }
                view.show(ui, &store);
            });
        harness.run();
        harness
    }

    /// Regression test for the recurring "first repo sits higher" bug: every
    /// repo's name button — and the REFRESH button — must share one top edge.
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
        let refresh_top = harness.get_by_label_contains("REFRESH").rect().top();
        assert!(
            (refresh_top - base).abs() < 0.75,
            "REFRESH top {refresh_top} != repo name-button top {base}"
        );
    }

    /// Guards against the REPOS section expanding to fill the viewport (a real
    /// regression we hit): the MODE row's FIND button must stay near the top,
    /// not be pushed hundreds of px down by an over-tall section above it.
    #[test]
    fn sections_stay_compact() {
        let (_tmp, store) = sample_store(&SAMPLE_REPOS);
        let harness = dupes_harness(store);
        // Exact label (the help text also contains "FIND").
        let find_label = format!("{} FIND", icon::SEARCH);
        let find_top = harness.get_by_label(&find_label).rect().top();
        assert!(
            find_top < 160.0,
            "FIND button at y={find_top}; the REPOS section is too tall (expanded?)"
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
                    crate::theme::apply(ui.ctx());
                    init = true;
                }
                view.show(ui, &store);
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
            .with_size(egui::vec2(600.0, 400.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx());
                        init = true;
                    }
                    view.show(ui, &store);
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
            .with_size(egui::vec2(600.0, 400.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx());
                        init = true;
                    }
                    view.show(ui, &store);
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
            .with_size(egui::vec2(600.0, 400.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx());
                        init = true;
                    }
                    view.show(ui, &store_ui);
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
                        crate::theme::apply(ui.ctx());
                        init = true;
                    }
                    view.show(ui, &store_ui);
                },
                DupesView::new(),
            );
        harness.run(); // load_repos (all included), initial render

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
                        crate::theme::apply(ui.ctx());
                        init = true;
                    }
                    // keep tmp alive for the store's lifetime
                    let _ = &tmp;
                    view.show(ui, &store);
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
            },
            RepoSel {
                name: "ro".into(),
                included: true,
                read_only: true,
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
                        crate::theme::apply(ui.ctx());
                        init = true;
                    }
                    view.show(ui, &store_ui);
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

    /// Image-diff regression test against `tests/snapshots/dupes_view.png`.
    /// Rendered with wgpu (lavapipe headless). Regenerate the baseline after an
    /// intentional visual change with:
    ///   UPDATE_SNAPSHOTS=1 cargo test -p dedup-gui dupes_view_snapshot -- --ignored
    /// Ignored by default because the baseline is renderer-specific (commit the
    /// baseline produced on your machine).
    #[test]
    #[ignore = "renderer-specific image snapshot; run explicitly"]
    fn dupes_view_snapshot() {
        let (_tmp, store) = sample_store(&SAMPLE_REPOS);
        let mut view = DupesView::new();
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1120.0, 260.0))
            .wgpu()
            .build_ui(move |ui| {
                if !init {
                    crate::icon::install(ui.ctx());
                    crate::theme::apply(ui.ctx());
                    init = true;
                }
                view.show(ui, &store);
            });
        harness.run();
        harness.snapshot("dupes_view");
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
                    crate::theme::apply(ui.ctx());
                    init = true;
                }
                view.show(ui, &store);
            });
        harness.run();
        let img = harness.render().expect("wgpu render failed");
        let out =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/dupes_view.png");
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
                    crate::theme::apply(ui.ctx());
                    init = true;
                }
                view.show(ui, &store);
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
                        crate::theme::apply(ui.ctx());
                        init = true;
                    }
                    view.show(ui, &store_ui);
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
}
