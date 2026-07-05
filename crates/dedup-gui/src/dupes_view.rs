//! The Duplicate Management tab: choose repos (optionally read-only), find exact
//! duplicates or perceptual similars, review paged groups with thumbnails, and
//! delete the worse copies — batched per repo, never without a confirmation.

use crate::theme;
use crate::thumbs::ThumbCache;
use crate::util::{format_mtime, format_size};
use dedup_core::dupes::{DupeFile, DupeGroup, delete_files, find_exact_duplicates, wasted_bytes};
use dedup_core::similar::find_similar;
use dedup_core::store::Store;
use dedup_core::thumbnail::hash_hex;
use egui::{Id, RichText};
use std::collections::HashSet;

const PAGE_SIZE: usize = 50;

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
    AutoResolve,
    AskDelete,
    ConfirmDelete,
    CancelDelete,
    SetPage(usize),
}

pub struct DupesView {
    repos: Vec<RepoSel>,
    repos_loaded: bool,
    mode: Mode,
    threshold: u32,
    groups: Vec<DupeGroup>,
    marked: HashSet<FileKey>,
    page: usize,
    status: Option<String>,
    error: Option<String>,
    confirm: Option<String>,
    thumbs: ThumbCache,
}

impl DupesView {
    pub fn new() -> Self {
        Self {
            repos: Vec::new(),
            repos_loaded: false,
            mode: Mode::Exact,
            threshold: 90,
            groups: Vec::new(),
            marked: HashSet::new(),
            page: 0,
            status: None,
            error: None,
            confirm: None,
            thumbs: ThumbCache::new(3),
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, store: &Store) {
        if self.thumbs.poll(&ui.ctx().clone()) {
            ui.ctx().request_repaint();
        }
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
        self.results(ui, &mut acts);

        if let Some(prompt) = self.confirm.clone() {
            self.confirm_modal(ui, &prompt, &mut acts);
        }

        for act in acts {
            self.apply(store, act);
        }
    }

    fn load_repos(&mut self, store: &Store) {
        match store.list_repos() {
            Ok(list) => {
                let ro: HashSet<String> = self
                    .repos
                    .iter()
                    .filter(|r| r.read_only)
                    .map(|r| r.name.clone())
                    .collect();
                let excluded: HashSet<String> = self
                    .repos
                    .iter()
                    .filter(|r| !r.included)
                    .map(|r| r.name.clone())
                    .collect();
                self.repos = list
                    .into_iter()
                    .map(|(name, _, _)| RepoSel {
                        included: !excluded.contains(&name),
                        read_only: ro.contains(&name),
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
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("REPOS").color(theme::TEXT).size(12.0));
            for (i, repo) in self.repos.iter().enumerate() {
                let fill = if repo.included {
                    theme::ORANGE
                } else {
                    theme::PANEL
                };
                let text = if repo.included {
                    theme::BLACK
                } else {
                    theme::TEXT
                };
                if ui
                    .add(egui::Button::new(RichText::new(&repo.name).color(text)).fill(fill))
                    .on_hover_text("Toggle whether this repo is searched")
                    .clicked()
                {
                    acts.push(Act::ToggleInclude(i));
                }
                let ro_fill = if repo.read_only {
                    theme::BLUE
                } else {
                    theme::PANEL
                };
                let ro_text = if repo.read_only {
                    theme::BLACK
                } else {
                    theme::BLUE
                };
                if ui
                    .add(egui::Button::new(RichText::new("RO").color(ro_text)).fill(ro_fill))
                    .on_hover_text("Read-only: files here are never selected for deletion")
                    .clicked()
                {
                    acts.push(Act::ToggleRo(i));
                }
                ui.add_space(8.0);
            }
            if ui.button(RichText::new("↻").color(theme::BLACK)).clicked() {
                acts.push(Act::ReloadRepos);
            }
        });
    }

    fn controls(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        ui.horizontal(|ui| {
            let exact = self.mode == Mode::Exact;
            if ui
                .add(
                    egui::Button::new(RichText::new("DUPLICATES").color(theme::BLACK))
                        .fill(if exact { theme::ORANGE } else { theme::PANEL }),
                )
                .clicked()
            {
                self.mode = Mode::Exact;
            }
            if ui
                .add(
                    egui::Button::new(RichText::new("SIMILAR").color(theme::BLACK))
                        .fill(if exact { theme::PANEL } else { theme::LILAC }),
                )
                .clicked()
            {
                self.mode = Mode::Similar;
            }
            if self.mode == Mode::Similar {
                ui.label(RichText::new("threshold").color(theme::TEXT).size(12.0));
                ui.add(egui::Slider::new(&mut self.threshold, 50..=100).suffix("%"));
            }
            if ui
                .add(
                    egui::Button::new(RichText::new("FIND").color(theme::BLACK)).fill(theme::AMBER),
                )
                .clicked()
            {
                acts.push(Act::Find);
            }
        });

        if !self.groups.is_empty() {
            ui.horizontal(|ui| {
                let n = self.marked.len();
                if ui
                    .button(RichText::new("AUTO-RESOLVE REST").color(theme::BLACK))
                    .on_hover_text("Mark every non-best copy in a deletable repo")
                    .clicked()
                {
                    acts.push(Act::AutoResolve);
                }
                let del = egui::Button::new(
                    RichText::new(format!("DELETE MARKED ({n})")).color(theme::BLACK),
                )
                .fill(theme::RED);
                if ui.add_enabled(n > 0, del).clicked() {
                    acts.push(Act::AskDelete);
                }
            });
        }
    }

    fn results(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        if self.groups.is_empty() {
            ui.add_space(8.0);
            ui.colored_label(theme::TEXT, "No groups. Pick repos and press FIND.");
            return;
        }

        let pages = self.groups.len().div_ceil(PAGE_SIZE);
        let page = self.page.min(pages.saturating_sub(1));
        ui.horizontal(|ui| {
            if ui.add_enabled(page > 0, egui::Button::new("◀")).clicked() {
                acts.push(Act::SetPage(page - 1));
            }
            ui.label(
                RichText::new(format!(
                    "page {}/{} · {} groups",
                    page + 1,
                    pages,
                    self.groups.len()
                ))
                .color(theme::TAN),
            );
            if ui
                .add_enabled(page + 1 < pages, egui::Button::new("▶"))
                .clicked()
            {
                acts.push(Act::SetPage(page + 1));
            }
        });

        let start = page * PAGE_SIZE;
        let end = (start + PAGE_SIZE).min(self.groups.len());
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for gi in start..end {
                    self.group_card(ui, gi, acts);
                }
            });
    }

    fn group_card(&mut self, ui: &mut egui::Ui, gi: usize, acts: &mut Vec<Act>) {
        // Clone the (small) group so the render closure can borrow `self` mutably
        // for thumbnails and mark state without aliasing `self.groups`.
        let group: Vec<DupeFile> = self.groups[gi].clone();
        let count = group.len();
        let size = group.first().map(|f| f.entry.size).unwrap_or(0);
        let wasted = wasted_bytes(&group);
        egui::Frame::new()
            .fill(theme::PANEL)
            .corner_radius(theme::PILL)
            .inner_margin(10.0)
            .outer_margin(egui::Margin {
                left: 0,
                right: 0,
                top: 0,
                bottom: 8,
            })
            .show(ui, |ui| {
                ui.label(
                    RichText::new(format!(
                        "{count} copies · {} each · {} reclaimable",
                        format_size(size),
                        format_size(wasted)
                    ))
                    .color(theme::AMBER)
                    .strong(),
                );
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
                            RichText::new("★ BEST")
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
        // Placeholder for non-images or not-yet-ready thumbnails.
        let label = file.entry.mime.clone().unwrap_or_else(|| "file".into());
        egui::Frame::new()
            .fill(theme::PANEL)
            .corner_radius(6)
            .inner_margin(18.0)
            .show(ui, |ui| {
                ui.set_width(160.0);
                ui.centered_and_justified(|ui| {
                    ui.label(RichText::new(label).color(theme::LILAC).size(11.0));
                });
            });
    }

    fn confirm_modal(&mut self, ui: &mut egui::Ui, prompt: &str, acts: &mut Vec<Act>) {
        egui::Modal::new(Id::new("dupes-confirm")).show(&ui.ctx().clone(), |ui| {
            ui.set_width(360.0);
            ui.label(
                RichText::new("CONFIRM DELETE")
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
                        egui::Button::new(RichText::new("DELETE").color(theme::BLACK))
                            .fill(theme::RED),
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

    fn apply(&mut self, store: &Store, act: Act) {
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
            Act::Find => self.find(store),
            Act::ToggleMark(k) => {
                if !self.marked.remove(&k) {
                    self.marked.insert(k);
                }
            }
            Act::AutoResolve => self.preselect_worse(),
            Act::SetPage(p) => self.page = p,
            Act::AskDelete => {
                let n = self.marked.len();
                if n > 0 {
                    self.confirm = Some(format!(
                        "Delete {n} marked file{} from disk? This cannot be undone.",
                        if n == 1 { "" } else { "s" }
                    ));
                }
            }
            Act::CancelDelete => self.confirm = None,
            Act::ConfirmDelete => {
                self.confirm = None;
                self.delete_marked(store);
            }
        }
    }

    fn find(&mut self, store: &Store) {
        let names: Vec<String> = self
            .repos
            .iter()
            .filter(|r| r.included)
            .map(|r| r.name.clone())
            .collect();
        if names.is_empty() {
            self.error = Some("Select at least one repo.".into());
            return;
        }
        let result = match self.mode {
            Mode::Exact => find_exact_duplicates(store, &names),
            Mode::Similar => find_similar(store, &names, f64::from(self.threshold)),
        };
        match result {
            Ok(groups) => {
                self.groups = groups;
                self.marked.clear();
                self.page = 0;
                self.error = None;
                self.preselect_worse();
                self.status = Some(format!(
                    "{} group(s); {} preselected for deletion",
                    self.groups.len(),
                    self.marked.len()
                ));
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    /// Mark every non-best copy whose repo is not read-only.
    fn preselect_worse(&mut self) {
        for group in &self.groups {
            for file in group.iter().skip(1) {
                if !self.repo_is_ro(&file.repo) {
                    self.marked.insert(key(file));
                }
            }
        }
    }

    fn delete_marked(&mut self, store: &Store) {
        let targets: Vec<&DupeFile> = self
            .groups
            .iter()
            .flatten()
            .filter(|f| self.marked.contains(&key(f)))
            .collect();
        match delete_files(store, &targets) {
            Ok(stats) => {
                self.status = Some(format!(
                    "Deleted {} file(s), {} error(s). Re-running search…",
                    stats.deleted, stats.errors
                ));
                self.marked.clear();
                self.find(store);
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    fn repo_is_ro(&self, name: &str) -> bool {
        self.repos.iter().any(|r| r.name == name && r.read_only)
    }
}
