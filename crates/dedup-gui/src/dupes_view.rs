//! The Duplicate Management tab: choose repos (optionally read-only), find exact
//! duplicates or perceptual similars, review paged groups with thumbnails, and
//! delete the worse copies — batched per repo, never without a confirmation.

use crate::icon;
use crate::theme;
use crate::thumbs::ThumbCache;
use crate::util::{format_mtime, format_size};
use dedup_core::dupes::{DupeFile, DupeGroup, delete_files, find_exact_duplicates, wasted_bytes};
use dedup_core::similar::find_similar;
use dedup_core::store::Store;
use dedup_core::thumbnail::hash_hex;
use egui::{Color32, Id, RichText};
use std::collections::HashSet;

const PAGE_SIZE: usize = 50;

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
    threshold: f64,
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
            threshold: 90.0,
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
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new(format!("{} FIND", icon::SEARCH)).color(theme::BLACK),
                        )
                        .fill(theme::AMBER),
                    )
                    .clicked()
                {
                    acts.push(Act::Find);
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
            if ui
                .add_enabled(page > 0, egui::Button::new(icon::CARET_LEFT))
                .clicked()
            {
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
                .add_enabled(page + 1 < pages, egui::Button::new(icon::CARET_RIGHT))
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
            .stroke(egui::Stroke::new(1.5, theme::ORANGE))
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
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new(icon::IMAGE).color(theme::LILAC).size(28.0));
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
            Mode::Similar => find_similar(store, &names, self.threshold),
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
    fn sample_store(names: &[&str]) -> (TempDir, Store) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open_at(tmp.path().join("cfg")).unwrap();
        for n in names {
            let dir = tmp.path().join(n);
            std::fs::create_dir_all(&dir).unwrap();
            store.create_repo(n, &dir.to_string_lossy()).unwrap();
        }
        (tmp, store)
    }

    /// Build a driven harness showing the Duplicates view for `store`. The
    /// closure owns `view`/`store`; the theme + icon font are installed once so
    /// glyph metrics match the real app.
    fn dupes_harness<'a>(store: Store) -> Harness<'a> {
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
}
