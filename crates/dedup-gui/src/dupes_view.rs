//! The Duplicate Management tab: choose repos (optionally read-only), find exact
//! duplicates or perceptual similars, review paged groups with thumbnails, and
//! delete the worse copies — batched per repo, never without a confirmation.

use crate::icon;
use crate::id3tags::{self, Tags};
use crate::imgedit::{self, Orient};
use crate::lightbox::{CompareState, FullResCache, LightboxState};
use crate::player::Player;
use crate::settings::TooltipVerbosity;
use crate::theme;
use crate::thumbs::ThumbCache;
use crate::util::{ExplainExt, format_mtime, format_size};
use crate::waveform::WaveCache;
use crossbeam_channel::{Receiver, Sender};
use dedup_core::dupes::{
    DupeDeleteStats, DupeFile, DupeGroup, DupeGroupKey, delete_paths, load_groups,
    plan_exact_duplicates, wasted_bytes,
};
use dedup_core::similar::find_similar;
use dedup_core::store::Store;
use dedup_core::thumbnail::hash_hex;
use egui::{Color32, Id, RichText};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const PAGE_SIZE: usize = 50;
/// Load groups from the DB in batches of this many during auto-resolve.
const AUTO_BATCH: usize = 128;
/// Evenly spaced stills sampled per video: the lightbox filmstrip's cells, and
/// the grid the card preview samples from (frame `VIDEO_STRIP / 2`), so the
/// card's still is reused by the filmstrip instead of extracted twice.
const VIDEO_STRIP: usize = 10;
/// Longest edge for the lightbox edit preview (matches the full-res decoder).
const EDIT_MAX_EDGE: u32 = 8192;

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

/// Format milliseconds as `m:ss` (or `h:mm:ss` past an hour) for the seek bar.
fn fmt_ms(ms: u64) -> String {
    let secs = ms / 1000;
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Paint a deterministic "fingerprint" glyph for an audio file: a waveform whose
/// bar heights and accent colour come from the audio chunk hash. Every bar is an
/// independent hash byte (no forced mirror symmetry, which would make different
/// files look alike to the eye). Identical content yields an identical glyph —
/// BLAKE3's avalanche means it signals *identity*, not gradations of similarity.
/// It replaces the generic broken-image placeholder so audio cards read as audio.
fn paint_audio_glyph(painter: &egui::Painter, rect: egui::Rect, fp: &dedup_core::store::AudioFp) {
    let seed = fp.chunk_hashes.first().copied().unwrap_or([0u8; 32]);
    let palette = [
        theme::AMBER,
        theme::TAN,
        theme::LILAC,
        theme::BLUE,
        theme::ORANGE,
    ];
    let accent = palette[seed[0] as usize % palette.len()];
    let bars = 15usize;
    let gap = 3.0;
    let bar_w = ((rect.width() - gap * (bars as f32 - 1.0)) / bars as f32).max(1.0);
    let mid_y = rect.center().y;
    let max_amp = rect.height() * 0.45;
    for i in 0..bars {
        // Each bar is its own hash byte — no mirror — so distinct audio yields
        // visibly distinct glyphs instead of similar symmetric ones.
        let amp = (0.15 + (seed[i % seed.len()] as f32 / 255.0) * 0.85) * max_amp;
        let x = rect.left() + i as f32 * (bar_w + gap);
        painter.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(x, mid_y - amp),
                egui::pos2(x + bar_w, mid_y + amp),
            ),
            1.0,
            accent,
        );
    }
}

/// Magma-ish heat ramp (black → purple → orange → white) for spectrogram cells:
/// `v` in 0..=1 maps to brightness, so louder frequencies read brighter.
fn spec_color(v: f32) -> egui::Color32 {
    const STOPS: [(f32, f32, f32, f32); 5] = [
        (0.00, 0.0, 0.0, 4.0),
        (0.25, 60.0, 15.0, 110.0),
        (0.50, 165.0, 45.0, 110.0),
        (0.75, 235.0, 105.0, 60.0),
        (1.00, 252.0, 255.0, 200.0),
    ];
    let v = v.clamp(0.0, 1.0);
    let mut i = 0;
    while i + 1 < STOPS.len() && v > STOPS[i + 1].0 {
        i += 1;
    }
    let (v0, r0, g0, b0) = STOPS[i];
    let (v1, r1, g1, b1) = STOPS[(i + 1).min(STOPS.len() - 1)];
    let t = if v1 > v0 { (v - v0) / (v1 - v0) } else { 0.0 };
    let lerp = |a: f32, b: f32| (a + (b - a) * t) as u8;
    egui::Color32::from_rgb(lerp(r0, r1), lerp(g0, g1), lerp(b0, b1))
}

/// Build a spectrogram image (time on x, frequency on y with bass at the
/// bottom) from a decoded [`waveform::AudioViz`].
fn spec_image(viz: &crate::waveform::AudioViz) -> egui::ColorImage {
    let (w, h) = (viz.spec_w, viz.spec_h);
    let mut rgba = vec![0u8; w * h * 4];
    for y in 0..h {
        let bin = h - 1 - y; // row 0 (top) = highest freq
        for x in 0..w {
            let c = spec_color(viz.spec[bin * w + x]);
            let i = (y * w + x) * 4;
            rgba[i] = c.r();
            rgba[i + 1] = c.g();
            rgba[i + 2] = c.b();
            rgba[i + 3] = 255;
        }
    }
    egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba)
}

/// One-line `path · size · WxH · mtime` description used by the lightbox.
fn lightbox_meta(file: &DupeFile) -> String {
    format!(
        "{} · {} · {} · {}",
        file.rel_path,
        format_size(file.entry.size),
        file.entry
            .img_size
            .map(|(w, h)| format!("{w}×{h}"))
            .unwrap_or_else(|| "—".into()),
        format_mtime(file.entry.modified_ms),
    )
}

/// Deferred UI actions, applied after rendering to avoid double borrows.
enum Act {
    ToggleInclude(usize),
    ToggleRo(usize),
    ReloadRepos,
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
    /// Open image lightbox (full-window zoom viewer), if any.
    lightbox: Option<LightboxState>,
    /// Full-resolution texture cache backing the lightbox.
    full_res: FullResCache,
    /// Global audio preview player (one file at a time).
    player: Player,
    /// Audio-visualization cache backing the audio lightbox's waveforms/spectra.
    waves: WaveCache,
    /// GPU textures for spectrograms, keyed by content hash (built lazily from
    /// `waves`, cleared when the audio lightbox closes).
    spec_tex: HashMap<String, egui::TextureHandle>,
    /// In-progress lossless rotate/flip of the lightbox's current image, if any.
    edit: Option<EditState>,
    /// The save-confirmation modal (overwrite vs copy) is open.
    edit_save: bool,
    /// Cached ID3 tags per audio file (hex → tags, or `None` if none/unsupported).
    tags_cache: HashMap<String, Option<Tags>>,
    /// The ID3 tag editor (audio lightbox), if open.
    tag_edit: Option<TagEdit>,
    /// Tooltip wording for this frame, set at the top of [`Self::show`] from
    /// the app-wide setting (not persisted here; `app.rs` owns that).
    verbosity: TooltipVerbosity,
}

/// A pending rotate/flip edit of the lightbox's current image. The `base` pixels
/// are decoded once; `tex`/`dims` are the live preview with `ops` applied.
struct EditState {
    hex: String,
    path: PathBuf,
    base: image::RgbaImage,
    ops: Vec<Orient>,
    tex: egui::TextureHandle,
    dims: egui::Vec2,
}

/// Open ID3 tag editor: the audio file being edited plus a working copy of its
/// tags, bound to the modal's text fields. `options` holds the distinct values
/// seen across every copy in the group, per field (Title/Artist/Album/Year/
/// Track/Genre), so the user can adopt the best value from any similar file.
struct TagEdit {
    hex: String,
    path: PathBuf,
    tags: Tags,
    options: [Vec<String>; 6],
}

/// A displayable ID3 field: its label and an accessor for its value.
type TagField = (&'static str, fn(&Tags) -> &str);

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
            full_res: FullResCache::new(2),
            player: Player::new(),
            waves: WaveCache::new(2),
            spec_tex: HashMap::new(),
            edit: None,
            edit_save: false,
            tags_cache: HashMap::new(),
            tag_edit: None,
            verbosity: TooltipVerbosity::default(),
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
        if self.full_res.poll(&ctx) {
            ctx.request_repaint();
        }
        if self.waves.poll(&ctx) {
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

        // The lightbox overlays everything else when open.
        self.lightbox_modal(&ctx, &mut acts);

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
                // nested inside this outer `horizontal`. A horizontal scroll
                // area keeps many repos on one line instead of overflowing the
                // window (solid scrollbar from the theme).
                egui::ScrollArea::horizontal()
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
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
                                    .explain(
                                        self.verbosity,
                                        "Toggle whether this repo is searched",
                                        "Include or exclude this repository from FIND results. \
                                         Excluded repos are skipped entirely — their files \
                                         won't appear as duplicates or as candidates.",
                                    )
                                    .clicked()
                                {
                                    acts.push(Act::ToggleInclude(i));
                                }
                                // Closed padlock = read-only (protected); open
                                // padlock = deletable.
                                let (glyph, ro_fill, ro_text, hover, hover_verbose) = if repo.read_only {
                                    (
                                        icon::LOCK,
                                        theme::BLUE,
                                        theme::BLACK,
                                        "Locked: files here are protected from deletion — click to allow deleting",
                                        "This repo is read-only: none of its files are ever \
                                         preselected or deletable, even by auto-resolve. Click \
                                         to unlock the whole repo for deletion.",
                                    )
                                } else {
                                    (
                                        icon::LOCK_OPEN,
                                        theme::PANEL,
                                        theme::BLUE,
                                        "Unlocked: files here can be deleted — click to protect",
                                        "This repo is unlocked: its files can be marked and \
                                         deleted like any other. Click to protect it (read-only) \
                                         again.",
                                    )
                                };
                                if ui
                                    .add(
                                        egui::Button::new(RichText::new(glyph).color(ro_text))
                                            .fill(ro_fill),
                                    )
                                    .explain(self.verbosity, hover, hover_verbose)
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
                            .explain(
                                self.verbosity,
                                "Reload the repository list",
                                "Reload the list of registered repositories (e.g. after \
                                 adding one in the Repositories tab). Include/read-only \
                                 choices for repos that still exist are kept.",
                            )
                            .clicked()
                        {
                            acts.push(Act::ReloadRepos);
                        }
                    });
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
                        });
                    });
                let find = egui::Button::new(
                    RichText::new(format!("{} FIND", icon::SEARCH)).color(theme::BLACK),
                )
                .fill(theme::AMBER);
                if ui
                    .add_enabled(self.busy.is_none(), find)
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
                    crate::util::similarity_slider(ui, &mut self.threshold, self.verbosity);
                });
            }

            // Quick Delete: gives each group a DELETE NOW button that removes
            // its marked files instantly (no per-group confirmation).
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                // Filled pill when on, text-only (frameless) when off — matches
                // the other LCARS toggles.
                let label = format!("{} QUICK DELETE", icon::LIGHTNING);
                let qd = if self.quick_delete {
                    egui::Button::new(RichText::new(label).color(theme::BLACK)).fill(theme::RED)
                } else {
                    egui::Button::new(RichText::new(label).color(theme::RED)).frame(false)
                };
                if ui
                    .add(qd)
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
                let del = egui::Button::new(
                    RichText::new(format!("DELETE MARKED ({n})")).color(theme::BLACK),
                )
                .fill(theme::RED);
                if ui
                    .add_enabled(idle && n > 0, del)
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
            ui.colored_label(theme::TEXT, msg);
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
                    .color(theme::TAN),
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
                    ui.label(RichText::new(header).color(theme::AMBER).strong());
                    // Quick Delete: one-click removal of this group's marked files.
                    if quick && has_marked {
                        let del = egui::Button::new(
                            RichText::new(format!("{} DELETE NOW", icon::TRASH))
                                .color(theme::BLACK),
                        )
                        .fill(theme::RED);
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
            .fill(theme::BLACK)
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
                                    .color(theme::TEXT)
                                    .size(12.0)
                                    .strong(),
                            );
                            ui.label(
                                RichText::new(format!(
                                    "{} · {}",
                                    file.repo,
                                    format_size(file.entry.size)
                                ))
                                .color(theme::TAN)
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
                                .color(theme::TAN)
                                .size(11.0),
                            );

                            if let Some(origin) = &file.entry.origin {
                                ui.label(
                                    RichText::new(format!("from {origin}"))
                                        .color(theme::LILAC)
                                        .size(11.0),
                                );
                            }

                            self.audio_controls(ui, file, acts);

                            if is_best {
                                ui.label(
                                    RichText::new(format!("{} BEST", icon::STAR))
                                        .color(theme::BLUE)
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
                                                .color(theme::BLUE)
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
                                            .color(theme::RED)
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
                                    (format!("{} DELETE", icon::CHECK), theme::RED)
                                } else {
                                    ("KEEP".to_string(), theme::PANEL)
                                };
                                let color = if marked { theme::BLACK } else { theme::TEXT };
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
            .is_some_and(|m| m.starts_with("audio/"));
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
            let fill = if playing { theme::AMBER } else { theme::PANEL };
            let col = if playing { theme::BLACK } else { theme::TEXT };
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
                    .color(theme::TAN)
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
        let mime = file.entry.mime.as_deref();
        let is_image = mime.is_some_and(|m| m.starts_with("image/"));
        let is_video = mime.is_some_and(|m| m.starts_with("video/"));
        let is_audio = mime.is_some_and(|m| m.starts_with("audio/"));
        if is_image || is_video {
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
                // Videos show a mid-timeline still (ffmpeg-extracted, cached);
                // absent ffmpeg the request fails and the placeholder shows.
                // Sampling on the same grid as the lightbox filmstrip means
                // the card's frame is reused there instead of extracted twice.
                let tex = if is_video {
                    self.thumbs
                        .get_video(&hex, &source, VIDEO_STRIP / 2, VIDEO_STRIP)
                } else {
                    self.thumbs.get(&hex, &source)
                };
                if let Some(tex) = tex {
                    let resp = ui
                        .add(
                            egui::Image::new(egui::load::SizedTexture::from_handle(&tex))
                                .max_height(120.0)
                                .corner_radius(6)
                                .sense(egui::Sense::click()),
                        )
                        .explain(
                            self.verbosity,
                            "Click to open the lightbox",
                            "Click to open the full-window lightbox: zoom, pan, step through \
                             this group's copies, and (for images) A/B compare against the \
                             best copy.",
                        );
                    // Hairline so dark photos stand off the dark panel.
                    ui.painter().rect_stroke(
                        resp.rect,
                        6,
                        egui::Stroke::new(1.0, theme::HAIRLINE),
                        egui::StrokeKind::Inside,
                    );
                    if resp.clicked() {
                        acts.push(Act::OpenLightbox(gi, fi));
                    }
                    return;
                }
            }
        }
        // Audio: a deterministic fingerprint glyph + duration, so cards read as
        // audio instead of a broken image and identical content shows the same
        // glyph. Clicking it opens the audio lightbox (waveform comparison).
        if is_audio && let Some(fp) = file.entry.audio.as_ref() {
            let (rect, resp) =
                ui.allocate_exact_size(egui::vec2(160.0, 112.0), egui::Sense::click());
            let painter = ui.painter_at(rect);
            painter.rect_filled(rect, 6.0, theme::PANEL);
            painter.rect_stroke(
                rect,
                6.0,
                egui::Stroke::new(1.0, theme::HAIRLINE),
                egui::StrokeKind::Inside,
            );
            let glyph = egui::Rect::from_min_max(
                rect.min + egui::vec2(8.0, 8.0),
                egui::pos2(rect.max.x - 8.0, rect.max.y - 24.0),
            );
            paint_audio_glyph(&painter, glyph, fp);
            painter.text(
                egui::pos2(rect.center().x, rect.max.y - 13.0),
                egui::Align2::CENTER_CENTER,
                fmt_ms(fp.duration_ms as u64),
                egui::FontId::proportional(12.0),
                theme::TAN,
            );
            let resp = resp.explain(
                self.verbosity,
                "Open the audio lightbox",
                "Open the full-window audio view: compare this group's copies as waveforms and \
                 switch playback between them without losing your place in the track.",
            );
            if resp.clicked() {
                acts.push(Act::OpenLightbox(gi, fi));
            }
            return;
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

    /// Full-resolution texture for a file (thumbnail upscaled while decoding),
    /// with the image's true pixel size (from the index, falling back to the
    /// texture) so transforms stay stable across the thumb→full-res swap.
    fn lightbox_texture(&mut self, file: &DupeFile) -> (Option<egui::TextureHandle>, egui::Vec2) {
        let hex = hash_hex(&file.entry.hash);
        let source = file.absolute_path();
        let full = self.full_res.get(&hex, &source);
        let tex = full.or_else(|| self.thumbs.get(&hex, &source));
        let img = file
            .entry
            .img_size
            .map(|(w, h)| egui::vec2(w as f32, h as f32))
            .or_else(|| tex.as_ref().map(|t| t.size_vec2()))
            .unwrap_or(egui::vec2(1.0, 1.0));
        (tex, img)
    }

    /// Apply a rotate/flip `op` to the lightbox's current image, decoding the
    /// base pixels on first use, and refresh the live preview texture.
    fn edit_apply(&mut self, ctx: &egui::Context, hex: &str, path: &Path, op: Orient) {
        if self.edit.as_ref().map(|e| e.hex.as_str()) != Some(hex) {
            let Ok((w, h, rgba)) = dedup_core::thumbnail::load_full_rgba(path, EDIT_MAX_EDGE)
            else {
                return;
            };
            let Some(base) = image::RgbaImage::from_raw(w, h, rgba) else {
                return;
            };
            let img =
                egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], base.as_raw());
            let tex = ctx.load_texture(format!("edit-{hex}"), img, egui::TextureOptions::LINEAR);
            self.edit = Some(EditState {
                hex: hex.to_string(),
                path: path.to_path_buf(),
                base,
                ops: Vec::new(),
                tex,
                dims: egui::vec2(w as f32, h as f32),
            });
        }
        if let Some(e) = self.edit.as_mut() {
            e.ops.push(op);
            let rgba = imgedit::apply_ops(image::DynamicImage::ImageRgba8(e.base.clone()), &e.ops)
                .to_rgba8();
            let (w, h) = rgba.dimensions();
            let img =
                egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], rgba.as_raw());
            e.tex = ctx.load_texture(format!("edit-{hex}"), img, egui::TextureOptions::LINEAR);
            e.dims = egui::vec2(w as f32, h as f32);
        }
    }

    /// Full-window image lightbox: wheel zoom (around cursor), drag pan, `F`
    /// fit / `1` 1:1, `←`/`→` step the group, `Del`/`K` toggle the mark, `C`
    /// A/B compare against the best copy (`space` enters flicker, then swaps
    /// A/B). `Esc` steps back one level — flicker → side-by-side → single →
    /// closed. Marking respects read-only exactly like the cards.
    fn lightbox_modal(&mut self, ctx: &egui::Context, acts: &mut Vec<Act>) {
        let verbosity = self.verbosity;
        // Take the state so `full_res`/`thumbs` can be borrowed mutably below;
        // it is put back at the end unless the lightbox was closed.
        let Some(mut state) = self.lightbox.take() else {
            return;
        };
        // Locate the addressed group on the current page.
        let page_start = self.cached_page.unwrap_or(0) * PAGE_SIZE;
        let Some(group) = self
            .page_groups
            .get(state.group.wrapping_sub(page_start))
            .filter(|g| !g.is_empty())
            .cloned()
        else {
            return; // group gone (page changed / resolved) → stay closed
        };
        let count = group.len();
        let mut idx = state.index.min(count - 1);

        // Audio files get a dedicated waveform lightbox, not the image viewer.
        if group[idx]
            .entry
            .mime
            .as_deref()
            .is_some_and(|m| m.starts_with("audio/"))
        {
            self.audio_lightbox(ctx, state, group, idx);
            return;
        }

        // Keyboard: navigation, view modes, mark, compare, close. Mode changes
        // are recorded as flags and applied after drawing (uniform one-frame
        // latency), so this frame draws a consistent state.
        let mut close = false;
        let mut new_idx = idx;
        let (mut do_fit, mut do_one, mut do_mark) = (false, false, false);
        let (mut toggle_compare, mut toggle_flicker, mut swap) = (false, false, false);
        let (mut esc, mut space) = (false, false);
        let (mut edit_op, mut reset_edit, mut open_save) = (None::<Orient>, false, false);
        ctx.input(|i| {
            if i.key_pressed(egui::Key::Escape) {
                esc = true;
            }
            if i.key_pressed(egui::Key::ArrowRight) {
                new_idx = (idx + 1) % count;
            }
            if i.key_pressed(egui::Key::ArrowLeft) {
                new_idx = (idx + count - 1) % count;
            }
            if i.key_pressed(egui::Key::F) {
                do_fit = true;
            }
            if i.key_pressed(egui::Key::Num1) {
                do_one = true;
            }
            if i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::K) {
                do_mark = true;
            }
            if i.key_pressed(egui::Key::C) {
                toggle_compare = true;
            }
            if i.key_pressed(egui::Key::Space) {
                space = true;
            }
        });
        // Escape is a universal "back": it pops one view level instead of
        // closing outright — flicker → side-by-side → single image → closed.
        // Space drives the flicker interaction: it enters flicker from
        // side-by-side, then swaps A/B once there. Both reuse the deferred
        // toggle flags so they apply after drawing like the button paths.
        if esc {
            match state.compare.as_ref() {
                Some(c) if c.flicker => toggle_flicker = true,
                Some(_) => toggle_compare = true,
                None => close = true,
            }
        }
        if space {
            match state.compare.as_ref() {
                Some(c) if c.flicker => swap = true,
                Some(_) => toggle_flicker = true,
                None => {}
            }
        }
        if close {
            self.edit = None;
            self.edit_save = false;
            return; // dropped state = closed
        }
        if new_idx != idx {
            idx = new_idx;
            state.index = idx;
            state.reset_view();
        }

        // The A file (always the current index) and its texture/metadata.
        let a = group[idx].clone();
        let a_key = key(&a);
        let a_markable = !self.repo_is_ro(&a.repo) || self.unlocked.contains(&a_key);
        let a_marked = self.marked.contains(&a_key);
        let (a_tex, a_img) = self.lightbox_texture(&a);
        let a_meta = lightbox_meta(&a);

        // The B file (compare target), if comparing.
        let b = state
            .compare
            .as_ref()
            .map(|c| c.other.min(count - 1))
            .map(|bi| group[bi].clone());
        let b_bundle = b.as_ref().map(|b| {
            let b_key = key(b);
            let b_markable = !self.repo_is_ro(&b.repo) || self.unlocked.contains(&b_key);
            let b_marked = self.marked.contains(&b_key);
            let (b_tex, b_img) = self.lightbox_texture(b);
            (b.clone(), b_key, b_markable, b_marked, b_tex, b_img)
        });

        // Del/K marks B when comparing (the candidate), else A.
        if do_mark {
            if let Some((_, b_key, b_markable, _, _, _)) = &b_bundle {
                if *b_markable {
                    acts.push(Act::ToggleMark(b_key.clone()));
                }
            } else if a_markable {
                acts.push(Act::ToggleMark(a_key.clone()));
            }
        }

        // "Better" (larger) size/area gets highlighted in the compare strip.
        let (a_size_col, b_size_col, a_dim_col, b_dim_col) = match &b_bundle {
            Some((bf, _, _, _, _, _)) => {
                let bigger = |x: u64, y: u64| {
                    if x > y { theme::BLUE } else { theme::TAN }
                };
                let area = |f: &DupeFile| {
                    f.entry
                        .img_size
                        .map(|(w, h)| w as u64 * h as u64)
                        .unwrap_or(0)
                };
                (
                    bigger(a.entry.size, bf.entry.size),
                    bigger(bf.entry.size, a.entry.size),
                    bigger(area(&a), area(bf)),
                    bigger(area(bf), area(&a)),
                )
            }
            None => (theme::TEXT, theme::TEXT, theme::TEXT, theme::TEXT),
        };

        // Destructure the B bundle into individual locals for the closure.
        let (b_key, b_markable, b_marked, b_tex, b_img) = match &b_bundle {
            Some((_, bk, bmk, bm, bt, bi)) => (Some(bk.clone()), *bmk, *bm, bt.clone(), Some(*bi)),
            None => (None, false, false, None, None),
        };
        let b_meta = b.as_ref().map(lightbox_meta);
        let flicker = state.compare.as_ref().is_some_and(|c| c.flicker);
        // Bottom strip height: compare stacks three lines (path A, path B, and
        // the hint) where the single view needs only two, so it grows and the
        // viewport shrinks to match — otherwise the hint line is pushed off the
        // bottom of the screen (which is why it looked like it vanished).
        let strip_h = if state.compare.is_some() { 74.0 } else { 50.0 };
        let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));

        // Video preview: a scrubbable filmstrip instead of a zoomable image.
        // Frames are extracted lazily by the thumb pool and fill in as they
        // land; the frame under the cursor's x fraction is shown enlarged.
        let a_is_video = a
            .entry
            .mime
            .as_deref()
            .is_some_and(|m| m.starts_with("video/"));

        // Rotate/flip editing applies only to a single, non-video image. Drop a
        // stale edit (and any open save modal) when we navigate to another image.
        let a_hex = hash_hex(&a.entry.hash);
        let a_is_image = a
            .entry
            .mime
            .as_deref()
            .is_some_and(|m| m.starts_with("image/"));
        if self.edit.as_ref().is_some_and(|e| e.hex != a_hex) {
            self.edit = None;
            self.edit_save = false;
        }
        let editing = a_is_image && !a_is_video && state.compare.is_none();
        let edited = editing && self.edit.as_ref().is_some_and(|e| !e.ops.is_empty());
        // The single-image view draws the live edit preview when there are edits.
        let (draw_tex, draw_img) = match &self.edit {
            Some(e) if edited && e.hex == a_hex => (Some(e.tex.clone()), e.dims),
            _ => (a_tex.clone(), a_img),
        };

        let vp_screen = ctx.content_rect();
        let vp = egui::Rect::from_min_max(
            egui::pos2(vp_screen.min.x + 8.0, vp_screen.min.y + 44.0),
            egui::pos2(vp_screen.max.x - 8.0, vp_screen.max.y - 62.0),
        );
        let video = if a_is_video && state.compare.is_none() {
            let strip_h = 92.0;
            let big =
                egui::Rect::from_min_max(vp.min, egui::pos2(vp.max.x, vp.max.y - strip_h - 6.0));
            let strip = egui::Rect::from_min_max(egui::pos2(vp.min.x, vp.max.y - strip_h), vp.max);
            let scrub = ctx
                .pointer_hover_pos()
                .filter(|c| vp.contains(*c))
                .map(|c| {
                    ((((c.x - vp.left()) / vp.width()) * VIDEO_STRIP as f32).floor() as i64)
                        .clamp(0, VIDEO_STRIP as i64 - 1) as usize
                })
                .unwrap_or(VIDEO_STRIP / 2);
            let hexa = hash_hex(&a.entry.hash);
            let srca = a.absolute_path();
            let big_tex = self.thumbs.get_video(&hexa, &srca, scrub, VIDEO_STRIP);
            let frames: Vec<Option<egui::TextureHandle>> = (0..VIDEO_STRIP)
                .map(|i| self.thumbs.get_video(&hexa, &srca, i, VIDEO_STRIP))
                .collect();
            Some((big, strip, scrub, big_tex, frames))
        } else {
            None
        };
        let video_pending = video
            .as_ref()
            .is_some_and(|(_, _, _, b, f)| b.is_none() || f.iter().any(Option::is_none));

        egui::Area::new(Id::new("lightbox"))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::Pos2::ZERO)
            .show(ctx, |ui| {
                let screen = ctx.content_rect();
                let bg = ui.allocate_rect(screen, egui::Sense::click_and_drag());
                ui.painter()
                    .rect_filled(screen, 0.0, egui::Color32::from_black_alpha(238));

                // Viewport = screen minus top control bar and bottom strip.
                let viewport = egui::Rect::from_min_max(
                    egui::pos2(screen.min.x + 8.0, screen.min.y + 44.0),
                    egui::pos2(screen.max.x - 8.0, screen.max.y - strip_h - 12.0),
                );
                let scroll = ui.input(|i| i.smooth_scroll_delta.y);
                let cursor = ctx.pointer_hover_pos();

                let draw = |ui: &egui::Ui, rect: egui::Rect, pane: egui::Rect, tex: &Option<egui::TextureHandle>| {
                    if let Some(tex) = tex {
                        ui.painter_at(pane).image(tex.id(), rect, uv, egui::Color32::WHITE);
                    } else {
                        ui.painter().text(
                            pane.center(),
                            egui::Align2::CENTER_CENTER,
                            "decoding…",
                            egui::FontId::proportional(16.0),
                            theme::TAN,
                        );
                    }
                };

                let fit = |target: egui::Rect, size: egui::Vec2| {
                    let s = (target.width() / size.x).min(target.height() / size.y);
                    egui::Rect::from_center_size(target.center(), size * s)
                };

                if let Some((big, strip, scrub, big_tex, frames)) = &video {
                    // Enlarged scrubbed frame.
                    if let Some(t) = big_tex {
                        let r = fit(*big, t.size_vec2());
                        ui.painter_at(*big).image(t.id(), r, uv, egui::Color32::WHITE);
                    } else {
                        ui.painter().text(
                            big.center(),
                            egui::Align2::CENTER_CENTER,
                            "decoding…",
                            egui::FontId::proportional(16.0),
                            theme::TAN,
                        );
                    }
                    ui.painter().text(
                        big.min + egui::vec2(6.0, 6.0),
                        egui::Align2::LEFT_TOP,
                        "VIDEO — hover to scrub",
                        egui::FontId::proportional(14.0),
                        theme::AMBER,
                    );
                    // Filmstrip of stills; the current one is outlined.
                    let n = frames.len().max(1);
                    let cell_w = strip.width() / n as f32;
                    for (i, f) in frames.iter().enumerate() {
                        let cell = egui::Rect::from_min_size(
                            egui::pos2(strip.left() + i as f32 * cell_w + 1.0, strip.top()),
                            egui::vec2(cell_w - 2.0, strip.height()),
                        );
                        if let Some(t) = f {
                            let r = fit(cell, t.size_vec2());
                            ui.painter_at(cell).image(t.id(), r, uv, egui::Color32::WHITE);
                        }
                        let (col, w) = if i == *scrub {
                            (theme::AMBER, 2.0)
                        } else {
                            (theme::HAIRLINE, 1.0)
                        };
                        ui.painter().rect_stroke(
                            cell,
                            0.0,
                            egui::Stroke::new(w, col),
                            egui::StrokeKind::Inside,
                        );
                    }
                } else if let Some(cmp) = state.compare.as_mut() {
                    // Shared zoom/pan across both panes.
                    if bg.dragged() {
                        cmp.pan_by(bg.drag_delta());
                    }
                    if scroll != 0.0
                        && cursor.is_some_and(|c| viewport.contains(c))
                    {
                        cmp.zoom_by((scroll * 0.005).exp());
                    }
                    if cmp.flicker {
                        // Overlay: show A or B in the whole viewport.
                        let (tex, img) = if cmp.show_b {
                            (&b_tex, b_img.unwrap_or(a_img))
                        } else {
                            (&a_tex, a_img)
                        };
                        let rect = cmp.pane_rect(viewport, img);
                        draw(ui, rect, viewport, tex);
                        let tag = if cmp.show_b { "B" } else { "A" };
                        ui.painter().text(
                            viewport.min + egui::vec2(6.0, 6.0),
                            egui::Align2::LEFT_TOP,
                            tag,
                            egui::FontId::proportional(18.0),
                            theme::AMBER,
                        );
                    } else {
                        // Side by side.
                        let gap = 6.0;
                        let half = (viewport.width() - gap) / 2.0;
                        let left = egui::Rect::from_min_size(
                            viewport.min,
                            egui::vec2(half, viewport.height()),
                        );
                        let right = egui::Rect::from_min_size(
                            egui::pos2(viewport.min.x + half + gap, viewport.min.y),
                            egui::vec2(half, viewport.height()),
                        );
                        draw(ui, cmp.pane_rect(left, a_img), left, &a_tex);
                        draw(
                            ui,
                            cmp.pane_rect(right, b_img.unwrap_or(a_img)),
                            right,
                            &b_tex,
                        );
                        for (pane, tag) in [(left, "A"), (right, "B")] {
                            ui.painter().text(
                                pane.min + egui::vec2(6.0, 6.0),
                                egui::Align2::LEFT_TOP,
                                tag,
                                egui::FontId::proportional(18.0),
                                theme::AMBER,
                            );
                        }
                    }
                } else {
                    // Single image: wheel zoom around cursor, drag pan.
                    if bg.dragged() {
                        state.pan_by(bg.drag_delta(), viewport, draw_img);
                    }
                    if scroll != 0.0
                        && let Some(c) = cursor
                        && viewport.contains(c)
                    {
                        state.zoom_at(c, (scroll * 0.005).exp(), viewport, draw_img);
                    }
                    let rect = state.image_rect(viewport, draw_img);
                    draw(ui, rect, viewport, &draw_tex);
                }

                // Top control bar.
                let top = egui::Rect::from_min_max(
                    egui::pos2(screen.min.x + 8.0, screen.min.y + 6.0),
                    egui::pos2(screen.max.x - 8.0, screen.min.y + 40.0),
                );
                ui.scope_builder(
                    egui::UiBuilder::new()
                        .max_rect(top)
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                    |ui| {
                        let pill = |ui: &mut egui::Ui,
                                    text: &str,
                                    fill: egui::Color32,
                                    col: egui::Color32,
                                    short: &str,
                                    verbose: &str| {
                            ui.add(egui::Button::new(RichText::new(text).color(col)).fill(fill))
                                .explain(verbosity, short, verbose)
                                .clicked()
                        };
                        if pill(
                            ui,
                            &format!("{} CLOSE", icon::CHECK),
                            theme::AMBER,
                            theme::BLACK,
                            "Close the lightbox",
                            "Close the lightbox and return to the group list (Esc does the same).",
                        ) {
                            close = true;
                        }
                        if pill(
                            ui,
                            icon::CARET_LEFT,
                            theme::PANEL,
                            theme::TEXT,
                            "Previous copy",
                            "Step to the previous copy in this group (← does the same).",
                        ) {
                            new_idx = (idx + count - 1) % count;
                        }
                        ui.label(
                            RichText::new(format!("{} / {count}", idx + 1))
                                .color(theme::TAN)
                                .strong(),
                        );
                        if pill(
                            ui,
                            icon::CARET_RIGHT,
                            theme::PANEL,
                            theme::TEXT,
                            "Next copy",
                            "Step to the next copy in this group (→ does the same).",
                        ) {
                            new_idx = (idx + 1) % count;
                        }
                        if state.compare.is_none() {
                            if pill(
                                ui,
                                "FIT",
                                theme::PANEL,
                                theme::TEXT,
                                "Fit to window",
                                "Scale the image to fit the viewport (F does the same).",
                            ) {
                                do_fit = true;
                            }
                            if pill(
                                ui,
                                "1:1",
                                theme::PANEL,
                                theme::TEXT,
                                "True pixels",
                                "Show the image at 100% — one screen pixel per image pixel \
                                 (1 does the same).",
                            ) {
                                do_one = true;
                            }
                            let (ml, mf) = if a_marked {
                                (format!("{} MARKED", icon::CHECK), theme::RED)
                            } else {
                                ("MARK".to_string(), theme::PANEL)
                            };
                            let mc = if a_marked { theme::BLACK } else { theme::TEXT };
                            if a_markable
                                && pill(
                                    ui,
                                    &ml,
                                    mf,
                                    mc,
                                    "Toggle this copy's mark",
                                    "Toggle whether the shown copy is marked for deletion \
                                     (Del/K does the same). Nothing deletes until you confirm \
                                     back in the group list.",
                                )
                            {
                                acts.push(Act::ToggleMark(a_key.clone()));
                            }
                            if count >= 2
                                && !a_is_video
                                && pill(
                                    ui,
                                    "COMPARE",
                                    theme::PANEL,
                                    theme::BLUE,
                                    "A/B compare with the best copy",
                                    "Enter A/B compare against the group's best copy, with a \
                                     shared zoom/pan (C does the same).",
                                )
                            {
                                toggle_compare = true;
                            }
                            if editing {
                                if pill(
                                    ui,
                                    "ROT L",
                                    theme::PANEL,
                                    theme::TEXT,
                                    "Rotate counter-clockwise",
                                    "Rotate 90° counter-clockwise. Lossless for PNG etc.; JPEG is \
                                     re-encoded at high quality when you save.",
                                ) {
                                    edit_op = Some(Orient::RotateCcw);
                                }
                                if pill(
                                    ui,
                                    "ROT R",
                                    theme::PANEL,
                                    theme::TEXT,
                                    "Rotate clockwise",
                                    "Rotate the image 90° clockwise.",
                                ) {
                                    edit_op = Some(Orient::RotateCw);
                                }
                                if pill(
                                    ui,
                                    "FLIP H",
                                    theme::PANEL,
                                    theme::TEXT,
                                    "Flip horizontally",
                                    "Mirror the image left-to-right.",
                                ) {
                                    edit_op = Some(Orient::FlipH);
                                }
                                if pill(
                                    ui,
                                    "FLIP V",
                                    theme::PANEL,
                                    theme::TEXT,
                                    "Flip vertically",
                                    "Mirror the image top-to-bottom.",
                                ) {
                                    edit_op = Some(Orient::FlipV);
                                }
                                if edited {
                                    if pill(
                                        ui,
                                        "RESET",
                                        theme::PANEL,
                                        theme::TAN,
                                        "Discard edits",
                                        "Discard the rotate/flip edits and show the original.",
                                    ) {
                                        reset_edit = true;
                                    }
                                    if pill(
                                        ui,
                                        &format!("{} SAVE", icon::CHECK),
                                        theme::AMBER,
                                        theme::BLACK,
                                        "Save the rotated image",
                                        "Write the rotated/flipped image to disk. You'll choose \
                                         overwrite or a new copy, and confirm first.",
                                    ) {
                                        open_save = true;
                                    }
                                }
                            }
                        } else {
                            if pill(
                                ui,
                                "EXIT COMPARE",
                                theme::PANEL,
                                theme::BLUE,
                                "Back to single view",
                                "Leave A/B compare and return to the single-image view \
                                 (C does the same).",
                            ) {
                                toggle_compare = true;
                            }
                            let mode = if flicker { "SIDE BY SIDE" } else { "FLICKER" };
                            let (mode_short, mode_verbose) = if flicker {
                                (
                                    "Switch to side-by-side",
                                    "Show A and B in two panes side by side instead of \
                                     overlaid.",
                                )
                            } else {
                                (
                                    "Switch to flicker mode",
                                    "Overlay A and B full-window; space enters flicker and then \
                                     swaps between them in place — the fastest way to spot \
                                     compression artifacts.",
                                )
                            };
                            if pill(ui, mode, theme::PANEL, theme::TEXT, mode_short, mode_verbose) {
                                toggle_flicker = true;
                            }
                            if flicker
                                && pill(
                                    ui,
                                    "SWAP",
                                    theme::PANEL,
                                    theme::TEXT,
                                    "Swap A/B",
                                    "Swap which of A or B is currently shown in flicker mode \
                                     (space does the same).",
                                )
                            {
                                swap = true;
                            }
                            // Mark A / Mark B.
                            let (al, af) = if a_marked {
                                (format!("A {}", icon::CHECK), theme::RED)
                            } else {
                                ("MARK A".to_string(), theme::PANEL)
                            };
                            let ac = if a_marked { theme::BLACK } else { theme::TEXT };
                            if a_markable
                                && pill(
                                    ui,
                                    &al,
                                    af,
                                    ac,
                                    "Toggle A's mark",
                                    "Toggle whether copy A (the shown file) is marked for \
                                     deletion.",
                                )
                            {
                                acts.push(Act::ToggleMark(a_key.clone()));
                            }
                            if let Some(bk) = &b_key {
                                let (bl, bf) = if b_marked {
                                    (format!("B {}", icon::CHECK), theme::RED)
                                } else {
                                    ("MARK B".to_string(), theme::PANEL)
                                };
                                let bc = if b_marked { theme::BLACK } else { theme::TEXT };
                                if b_markable
                                    && pill(
                                        ui,
                                        &bl,
                                        bf,
                                        bc,
                                        "Toggle B's mark",
                                        "Toggle whether copy B (the compare candidate) is \
                                         marked for deletion (Del/K does the same while \
                                         comparing).",
                                    )
                                {
                                    acts.push(Act::ToggleMark(bk.clone()));
                                }
                            }
                        }
                    },
                );

                // Bottom metadata + hint strip.
                let bottom = egui::Rect::from_min_max(
                    egui::pos2(screen.min.x + 8.0, screen.max.y - strip_h - 6.0),
                    egui::pos2(screen.max.x - 8.0, screen.max.y - 6.0),
                );
                ui.scope_builder(
                    egui::UiBuilder::new()
                        .max_rect(bottom)
                        .layout(egui::Layout::top_down(egui::Align::LEFT)),
                    |ui| {
                        if let Some(b_meta) = &b_meta {
                            let row = |ui: &mut egui::Ui, tag: &str, f: &DupeFile, sc: egui::Color32, dc: egui::Color32| {
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(tag).color(theme::AMBER).strong());
                                    ui.label(RichText::new(&f.rel_path).color(theme::TEXT).size(12.0));
                                    ui.label(RichText::new(format_size(f.entry.size)).color(sc).size(12.0));
                                    ui.label(
                                        RichText::new(
                                            f.entry
                                                .img_size
                                                .map(|(w, h)| format!("{w}×{h}"))
                                                .unwrap_or_else(|| "—".into()),
                                        )
                                        .color(dc)
                                        .size(12.0),
                                    );
                                    ui.label(
                                        RichText::new(format_mtime(f.entry.modified_ms))
                                            .color(theme::TAN)
                                            .size(12.0),
                                    );
                                });
                            };
                            row(ui, "A", &a, a_size_col, a_dim_col);
                            if let Some((bf, _, _, _, _, _)) = &b_bundle {
                                row(ui, "B", bf, b_size_col, b_dim_col);
                            }
                            let _ = b_meta;
                            let hint = if flicker {
                                "wheel zoom · drag pan · space: swap A/B · Del/K mark B · Esc: back to side-by-side · C: exit compare"
                            } else {
                                "wheel zoom · drag pan · space: flicker · Del/K mark B · Esc/C: back to single"
                            };
                            ui.label(RichText::new(hint).color(theme::LILAC).size(11.0));
                        } else {
                            ui.label(RichText::new(&a_meta).color(theme::TEXT).size(13.0));
                            let hint = if a_is_video {
                                format!(
                                    "hover: scrub · {}/{} step · Del/K mark · Esc close",
                                    icon::CARET_LEFT,
                                    icon::CARET_RIGHT,
                                )
                            } else {
                                format!(
                                    "wheel: zoom · drag: pan · F fit · 1 100% · {}/{} step · Del/K mark · C compare · Esc close",
                                    icon::CARET_LEFT,
                                    icon::CARET_RIGHT,
                                )
                            };
                            ui.label(RichText::new(hint).color(theme::LILAC).size(11.0));
                        }
                    },
                );
            });

        // Apply deferred view-mode / compare changes now that drawing is done.
        if do_fit {
            state.fit();
        }
        if do_one {
            state.one_to_one();
        }
        if toggle_compare {
            if state.compare.is_some() {
                state.compare = None;
                state.reset_view();
            } else if count >= 2 {
                let b_idx = if idx == 0 { 1 } else { 0 };
                state.compare = Some(CompareState::new(b_idx));
            }
        }
        if toggle_flicker && let Some(cmp) = state.compare.as_mut() {
            cmp.flicker = !cmp.flicker;
        }
        if swap
            && let Some(cmp) = state.compare.as_mut()
            && cmp.flicker
        {
            cmp.show_b = !cmp.show_b;
        }
        if new_idx != idx {
            state.index = new_idx;
            state.reset_view();
        }
        // Rotate/flip edits (deferred like the other controls).
        if let Some(op) = edit_op {
            self.edit_apply(ctx, &a_hex, &a.absolute_path(), op);
        }
        if reset_edit {
            self.edit = None;
        }
        if open_save {
            self.edit_save = true;
        }

        // Save-confirmation modal (Phase 6.6): overwrite in place or a `_rot`
        // copy, both explicitly confirmed. Saving does not close the lightbox.
        if self.edit_save {
            let (mut do_overwrite, mut do_copy, mut cancel) = (false, false, false);
            let is_jpeg = image::ImageFormat::from_path(a.absolute_path())
                .is_ok_and(|f| f == image::ImageFormat::Jpeg);
            egui::Modal::new(Id::new("edit-save")).show(&ctx.clone(), |ui| {
                ui.set_width(400.0);
                ui.label(
                    RichText::new("SAVE ROTATED IMAGE")
                        .color(theme::AMBER)
                        .size(16.0)
                        .strong(),
                );
                ui.add_space(6.0);
                ui.label(RichText::new(&a.rel_path).color(theme::TEXT).size(12.0));
                if is_jpeg {
                    ui.label(
                        RichText::new(
                            "JPEG will be re-encoded at high quality — a small, unavoidable loss.",
                        )
                        .color(theme::TAN)
                        .size(11.0),
                    );
                }
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new("OVERWRITE ORIGINAL").color(theme::BLACK),
                            )
                            .fill(theme::RED),
                        )
                        .clicked()
                    {
                        do_overwrite = true;
                    }
                    if ui
                        .add(
                            egui::Button::new(RichText::new("SAVE A COPY").color(theme::BLACK))
                                .fill(theme::BLUE),
                        )
                        .clicked()
                    {
                        do_copy = true;
                    }
                    if ui
                        .button(RichText::new("CANCEL").color(theme::TEXT))
                        .clicked()
                    {
                        cancel = true;
                    }
                });
                ui.add_space(6.0);
                ui.label(
                    RichText::new("Overwriting changes the file on disk and cannot be undone.")
                        .color(theme::LILAC)
                        .size(11.0),
                );
            });
            if cancel {
                self.edit_save = false;
            } else if do_overwrite || do_copy {
                let ops = self
                    .edit
                    .as_ref()
                    .map(|e| e.ops.clone())
                    .unwrap_or_default();
                let path = self.edit.as_ref().map(|e| e.path.clone());
                if let Some(path) = path {
                    match imgedit::save_edited(&path, &ops, do_overwrite) {
                        Ok(out) => {
                            self.status = Some(format!("Saved {}", out.display()));
                            self.error = None;
                        }
                        Err(e) => self.error = Some(format!("Save failed: {e}")),
                    }
                }
                self.edit_save = false;
                // Stay in the lightbox (6.6); keep the edit preview showing.
            }
        }

        if !close {
            self.lightbox = Some(state);
        } else {
            self.edit = None;
            self.edit_save = false;
        }

        // Keep polling while video stills are still being extracted so the
        // filmstrip fills in without needing mouse movement.
        if video_pending {
            ctx.request_repaint_after(std::time::Duration::from_millis(150));
        }
    }

    /// Full-window audio lightbox: each copy's decoded waveform, stacked for A/B
    /// compare so differences stand out; `space` play/pause, `←`/`→` switch which
    /// copy plays (keeping the offset, so you hear the same moment in each),
    /// clicking a waveform plays that copy from there, `C` toggles compare, `Esc`
    /// steps back (compare → single → closed).
    /// Lazily upload (and cache) a spectrogram texture for `hex`.
    fn spec_texture(
        &mut self,
        ctx: &egui::Context,
        hex: &str,
        viz: &crate::waveform::AudioViz,
    ) -> egui::TextureHandle {
        if let Some(t) = self.spec_tex.get(hex) {
            return t.clone();
        }
        let tex = ctx.load_texture(
            format!("spec-{hex}"),
            spec_image(viz),
            egui::TextureOptions::LINEAR,
        );
        self.spec_tex.insert(hex.to_string(), tex.clone());
        tex
    }

    fn audio_lightbox(
        &mut self,
        ctx: &egui::Context,
        mut state: LightboxState,
        group: DupeGroup,
        mut idx: usize,
    ) {
        let verbosity = self.verbosity;
        let count = group.len();
        let params = |f: &DupeFile| -> (String, PathBuf, u64) {
            (
                hash_hex(&f.entry.hash),
                f.absolute_path(),
                f.entry
                    .audio
                    .as_ref()
                    .map_or(0, |a| u64::from(a.duration_ms)),
            )
        };

        // A is the current copy; B (compare target) is another copy in the group.
        let a = group[idx].clone();
        let (a_hex, a_path, _a_total) = params(&a);
        let a_viz = self.waves.get(&a_hex, &a_path);
        let b_file = state
            .compare
            .as_ref()
            .map(|c| group[c.other.min(count - 1)].clone());
        let (b_hex, b_viz) = match &b_file {
            Some(f) => {
                let (h, p, _t) = params(f);
                let v = self.waves.get(&h, &p);
                (Some(h), v)
            }
            None => (None, None),
        };
        let comparing = b_file.is_some();
        let flicker = state.compare.as_ref().is_some_and(|c| c.flicker);
        let spectrogram = state.spectrogram;
        // Build/fetch spectrogram textures (only needed in spectrogram view).
        let a_tex = if spectrogram {
            a_viz.clone().map(|v| self.spec_texture(ctx, &a_hex, &v))
        } else {
            None
        };
        let b_tex = match (spectrogram, b_hex.as_ref(), b_viz.clone()) {
            (true, Some(h), Some(v)) => Some(self.spec_texture(ctx, h, &v)),
            _ => None,
        };

        // ID3 tags for A (and B, when comparing) — cached, read is file I/O.
        let a_tags = self
            .tags_cache
            .entry(a_hex.clone())
            .or_insert_with(|| id3tags::read(&a_path))
            .clone();
        let b_tags = b_file.as_ref().and_then(|f| {
            let (h, p, _t) = params(f);
            self.tags_cache
                .entry(h)
                .or_insert_with(|| id3tags::read(&p))
                .clone()
        });

        // Keyboard. `space` drives flicker (enter, then swap) exactly like the
        // image lightbox; playback is `P`, `S` toggles the spectrogram, `T` the
        // tag editor.
        let mut close = false;
        let mut new_idx = idx;
        let (mut toggle_play, mut toggle_compare, mut esc) = (false, false, false);
        let (mut space, mut toggle_flicker, mut swap, mut toggle_spec) =
            (false, false, false, false);
        // Which copy's tag editor to open (its group index), if any.
        let mut open_tags: Option<usize> = None;
        ctx.input(|i| {
            if i.key_pressed(egui::Key::Escape) {
                esc = true;
            }
            if i.key_pressed(egui::Key::ArrowRight) {
                new_idx = (idx + 1) % count;
            }
            if i.key_pressed(egui::Key::ArrowLeft) {
                new_idx = (idx + count - 1) % count;
            }
            if i.key_pressed(egui::Key::Space) {
                space = true;
            }
            if i.key_pressed(egui::Key::P) {
                toggle_play = true;
            }
            if i.key_pressed(egui::Key::S) {
                toggle_spec = true;
            }
            if i.key_pressed(egui::Key::T) {
                open_tags = Some(idx);
            }
            if i.key_pressed(egui::Key::C) {
                toggle_compare = true;
            }
        });

        // Player snapshot for the playback cursor. The cursor shows on exactly
        // one row — the copy the user last started (`audio_active`) — because
        // exact-duplicate copies share a content hash, so the hash alone can't
        // say which row is playing.
        let snap = self.player.snapshot();
        let b_idx = state.compare.as_ref().map(|c| c.other.min(count - 1));
        // Adopt an already-playing copy (e.g. started from a card) on open.
        if state.audio_active.is_none()
            && snap.loaded
            && snap.hex.as_deref() == Some(a_hex.as_str())
        {
            state.audio_active = Some(idx);
        }
        let active = state.audio_active;
        let cursor_at = |group_idx: usize, hex: &str| -> Option<f32> {
            (active == Some(group_idx)
                && snap.loaded
                && snap.hex.as_deref() == Some(hex)
                && snap.total_ms > 0)
                .then(|| (snap.pos_ms as f32 / snap.total_ms as f32).clamp(0.0, 1.0))
        };
        let a_cursor = cursor_at(idx, &a_hex);
        let b_cursor = b_file
            .as_ref()
            .zip(b_idx)
            .and_then(|(f, bi)| cursor_at(bi, &params(f).0));

        // Click on a waveform → play that copy from there. (row_is_b, fraction)
        let mut click_play: Option<(bool, f32)> = None;

        egui::Area::new(Id::new("audio-lightbox"))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::Pos2::ZERO)
            .show(ctx, |ui| {
                let screen = ctx.content_rect();
                // Absorb stray clicks so the cards behind stay inert.
                let _sink = ui.allocate_rect(screen, egui::Sense::click());
                ui.painter()
                    .rect_filled(screen, 0.0, egui::Color32::from_black_alpha(238));

                let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                let draw_row = |ui: &egui::Ui,
                                rect: egui::Rect,
                                viz: Option<&Arc<crate::waveform::AudioViz>>,
                                tex: Option<&egui::TextureHandle>,
                                color: egui::Color32,
                                tag: &str,
                                cursor: Option<f32>| {
                    let p = ui.painter_at(rect);
                    p.rect_filled(rect, 4.0, theme::PANEL);
                    let ready = if spectrogram {
                        if let Some(tex) = tex {
                            p.image(tex.id(), rect, uv, egui::Color32::WHITE);
                            true
                        } else {
                            false
                        }
                    } else if let Some(env) = viz.map(|v| &v.envelope).filter(|e| !e.is_empty()) {
                        let n = env.len();
                        let mid = rect.center().y;
                        let bw = rect.width() / n as f32;
                        for (i, &amp) in env.iter().enumerate() {
                            let h = amp * rect.height() * 0.46;
                            let x = rect.left() + i as f32 * bw;
                            p.rect_filled(
                                egui::Rect::from_min_max(
                                    egui::pos2(x, mid - h),
                                    egui::pos2(x + bw.max(1.0), mid + h),
                                ),
                                0.0,
                                color,
                            );
                        }
                        true
                    } else {
                        false
                    };
                    if !ready {
                        p.text(
                            rect.center(),
                            egui::Align2::CENTER_CENTER,
                            "analyzing…",
                            egui::FontId::proportional(16.0),
                            theme::TAN,
                        );
                    }
                    if let Some(f) = cursor {
                        let x = rect.left() + f * rect.width();
                        p.line_segment(
                            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
                            egui::Stroke::new(1.5, theme::AMBER),
                        );
                    }
                    p.text(
                        rect.min + egui::vec2(6.0, 4.0),
                        egui::Align2::LEFT_TOP,
                        tag,
                        egui::FontId::proportional(16.0),
                        theme::AMBER,
                    );
                };

                // View area. A right column always carries the read-only id3
                // tags, split to mirror the waveform rows — A's tags beside the A
                // wave, B's beside the B wave (symmetric), and flicker-aware.
                let area = egui::Rect::from_min_max(
                    egui::pos2(screen.min.x + 12.0, screen.min.y + 52.0),
                    egui::pos2(screen.max.x - 12.0, screen.max.y - 64.0),
                );
                let tags_w = (area.width() * 0.28).clamp(180.0, 270.0);
                let wave_area = egui::Rect::from_min_max(
                    area.min,
                    egui::pos2(area.max.x - tags_w - 12.0, area.max.y),
                );
                let tags_col = egui::Rect::from_min_max(
                    egui::pos2(area.max.x - tags_w, area.min.y),
                    area.max,
                );
                let frac_at = |rect: egui::Rect, resp: &egui::Response| {
                    resp.interact_pointer_pos()
                        .map(|p| ((p.x - rect.left()) / rect.width()).clamp(0.0, 1.0))
                };
                let fields: [TagField; 6] = [
                    ("Title", |t| &t.title),
                    ("Artist", |t| &t.artist),
                    ("Album", |t| &t.album),
                    ("Year", |t| &t.year),
                    ("Track", |t| &t.track),
                    ("Genre", |t| &t.genre),
                ];
                // Draw one copy's read-only tags into `rect`; values differing
                // from `other` (when comparing) are highlighted. Returns the
                // group index to edit if its EDIT button was clicked.
                let draw_tags = |ui: &mut egui::Ui,
                                 rect: egui::Rect,
                                 own: &Option<Tags>,
                                 other: &Option<Tags>,
                                 header: &str,
                                 header_col: egui::Color32,
                                 edit_idx: usize|
                 -> Option<usize> {
                    let mut edit = None;
                    ui.scope_builder(
                        egui::UiBuilder::new()
                            .max_rect(rect.shrink(6.0))
                            .layout(egui::Layout::top_down(egui::Align::LEFT)),
                        |ui| {
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(format!("{header}  ID3"))
                                        .color(header_col)
                                        .size(12.0)
                                        .strong(),
                                );
                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new(format!("{} EDIT", icon::PENCIL))
                                                .color(theme::TEXT),
                                        )
                                        .fill(theme::PANEL),
                                    )
                                    .clicked()
                                {
                                    edit = Some(edit_idx);
                                }
                            });
                            ui.add_space(2.0);
                            for (name, get) in fields {
                                let ov = own.as_ref().map(get).unwrap_or("");
                                let tv = other.as_ref().map(get).unwrap_or("");
                                let col = if own.is_some() && ov != tv {
                                    theme::AMBER
                                } else {
                                    theme::TEXT
                                };
                                ui.horizontal(|ui| {
                                    ui.add_sized(
                                        [46.0, 15.0],
                                        egui::Label::new(
                                            RichText::new(name).color(theme::LILAC).size(10.0),
                                        ),
                                    );
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(ov).color(col).size(11.0),
                                        )
                                        .truncate(),
                                    )
                                    .on_hover_text(ov);
                                });
                            }
                        },
                    );
                    edit
                };

                if comparing && flicker {
                    // Overlay: show A or B full-area (and its tags); space swaps.
                    let show_b = state.compare.as_ref().is_some_and(|c| c.show_b);
                    let ra = ui.allocate_rect(wave_area, egui::Sense::click());
                    if show_b {
                        draw_row(ui, wave_area, b_viz.as_ref(), b_tex.as_ref(), theme::TAN, "B", b_cursor);
                    } else {
                        draw_row(ui, wave_area, a_viz.as_ref(), a_tex.as_ref(), theme::BLUE, "A", a_cursor);
                    }
                    if ra.clicked()
                        && let Some(f) = frac_at(wave_area, &ra)
                    {
                        click_play = Some((show_b, f));
                    }
                    let hit = if show_b {
                        draw_tags(ui, tags_col, &b_tags, &a_tags, "B", theme::TAN, b_idx.unwrap_or(idx))
                    } else {
                        draw_tags(ui, tags_col, &a_tags, &b_tags, "A", theme::BLUE, idx)
                    };
                    open_tags = open_tags.or(hit);
                } else if comparing {
                    let gap = 12.0;
                    let half = (wave_area.height() - gap) / 2.0;
                    let top =
                        egui::Rect::from_min_size(wave_area.min, egui::vec2(wave_area.width(), half));
                    let bot = egui::Rect::from_min_size(
                        egui::pos2(wave_area.min.x, wave_area.min.y + half + gap),
                        egui::vec2(wave_area.width(), half),
                    );
                    let ttop =
                        egui::Rect::from_min_size(tags_col.min, egui::vec2(tags_col.width(), half));
                    let tbot = egui::Rect::from_min_size(
                        egui::pos2(tags_col.min.x, tags_col.min.y + half + gap),
                        egui::vec2(tags_col.width(), half),
                    );
                    let ra = ui.allocate_rect(top, egui::Sense::click());
                    draw_row(ui, top, a_viz.as_ref(), a_tex.as_ref(), theme::BLUE, "A", a_cursor);
                    if ra.clicked()
                        && let Some(f) = frac_at(top, &ra)
                    {
                        click_play = Some((false, f));
                    }
                    let rb = ui.allocate_rect(bot, egui::Sense::click());
                    draw_row(ui, bot, b_viz.as_ref(), b_tex.as_ref(), theme::TAN, "B", b_cursor);
                    if rb.clicked()
                        && let Some(f) = frac_at(bot, &rb)
                    {
                        click_play = Some((true, f));
                    }
                    // Symmetric tag panels: A beside the top row, B beside bottom.
                    let ha = draw_tags(ui, ttop, &a_tags, &b_tags, "A", theme::BLUE, idx);
                    let hb = draw_tags(
                        ui,
                        tbot,
                        &b_tags,
                        &a_tags,
                        "B",
                        theme::TAN,
                        b_idx.unwrap_or(idx),
                    );
                    open_tags = open_tags.or(ha).or(hb);
                } else {
                    let ra = ui.allocate_rect(wave_area, egui::Sense::click());
                    draw_row(ui, wave_area, a_viz.as_ref(), a_tex.as_ref(), theme::BLUE, "A", a_cursor);
                    if ra.clicked()
                        && let Some(f) = frac_at(wave_area, &ra)
                    {
                        click_play = Some((false, f));
                    }
                    let hit = draw_tags(ui, tags_col, &a_tags, &b_tags, "A", theme::BLUE, idx);
                    open_tags = open_tags.or(hit);
                }

                // Top control bar.
                let top_bar = egui::Rect::from_min_max(
                    egui::pos2(screen.min.x + 8.0, screen.min.y + 6.0),
                    egui::pos2(screen.max.x - 8.0, screen.min.y + 40.0),
                );
                // PLAY/PAUSE reflects whether *any* copy is playing, not just A —
                // in compare the audible copy switches, but the button must stay
                // PAUSE the whole time something is playing.
                let playing = snap.loaded && snap.playing;
                ui.scope_builder(
                    egui::UiBuilder::new()
                        .max_rect(top_bar)
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                    |ui| {
                        let pill = |ui: &mut egui::Ui,
                                    text: &str,
                                    fill: egui::Color32,
                                    col: egui::Color32,
                                    short: &str,
                                    verbose: &str| {
                            ui.add(egui::Button::new(RichText::new(text).color(col)).fill(fill))
                                .explain(verbosity, short, verbose)
                                .clicked()
                        };
                        if pill(
                            ui,
                            &format!("{} CLOSE", icon::CHECK),
                            theme::AMBER,
                            theme::BLACK,
                            "Close the audio lightbox",
                            "Close and return to the group list (Esc steps back one level).",
                        ) {
                            close = true;
                        }
                        if pill(
                            ui,
                            icon::CARET_LEFT,
                            theme::PANEL,
                            theme::TEXT,
                            "Previous copy",
                            "Switch to the previous copy, keeping the playback offset (← does \
                             the same).",
                        ) {
                            new_idx = (idx + count - 1) % count;
                        }
                        ui.label(
                            RichText::new(format!("{} / {count}", idx + 1))
                                .color(theme::TAN)
                                .strong(),
                        );
                        if pill(
                            ui,
                            icon::CARET_RIGHT,
                            theme::PANEL,
                            theme::TEXT,
                            "Next copy",
                            "Switch to the next copy, keeping the playback offset (→ does the \
                             same).",
                        ) {
                            new_idx = (idx + 1) % count;
                        }
                        let (pl, pf, pc) = if playing {
                            ("PAUSE", theme::AMBER, theme::BLACK)
                        } else {
                            ("PLAY", theme::PANEL, theme::TEXT)
                        };
                        if pill(
                            ui,
                            pl,
                            pf,
                            pc,
                            "Play/pause",
                            "Play or pause the current copy (P does the same).",
                        ) {
                            toggle_play = true;
                        }
                        // Waveform ↔ spectrogram view toggle.
                        let (vl, vshort, vverbose) = if spectrogram {
                            (
                                "WAVEFORM",
                                "Show the amplitude waveform",
                                "Switch back to the amplitude waveform (S toggles).",
                            )
                        } else {
                            (
                                "SPECTROGRAM",
                                "Show the spectrogram",
                                "Switch to a frequency-vs-time spectrogram: brightness is loudness \
                                 per frequency band — far more telling than the flat waveform for \
                                 loud music (S toggles).",
                            )
                        };
                        if pill(ui, vl, theme::PANEL, theme::LILAC, vshort, vverbose) {
                            toggle_spec = true;
                        }
                        if pill(
                            ui,
                            &format!("{} TAGS", icon::PENCIL),
                            theme::PANEL,
                            theme::TEXT,
                            "Edit ID3 tags",
                            "Open the ID3 tag editor for the current copy — the panels on the \
                             right show them read-only; saving writes only the tags, the audio \
                             is untouched (T does the same).",
                        ) {
                            open_tags = Some(idx);
                        }
                        if count >= 2 {
                            let (cl, cshort, cverbose) = if comparing {
                                (
                                    "EXIT COMPARE",
                                    "Back to a single copy",
                                    "Hide the B view and show only the current copy.",
                                )
                            } else {
                                (
                                    "COMPARE",
                                    "Compare against another copy",
                                    "Stack a second copy below this one so differences are \
                                     visible; click either to hear that spot.",
                                )
                            };
                            if pill(ui, cl, theme::PANEL, theme::BLUE, cshort, cverbose) {
                                toggle_compare = true;
                            }
                            if comparing {
                                let (ml, mshort, mverbose) = if flicker {
                                    (
                                        "SIDE BY SIDE",
                                        "Stack A and B",
                                        "Show A and B stacked instead of overlaid.",
                                    )
                                } else {
                                    (
                                        "FLICKER",
                                        "Overlay & flicker",
                                        "Overlay A and B in one pane; space enters flicker and \
                                         then swaps between them — flick A↔B to spot differences.",
                                    )
                                };
                                if pill(ui, ml, theme::PANEL, theme::TEXT, mshort, mverbose) {
                                    toggle_flicker = true;
                                }
                                if flicker
                                    && pill(
                                        ui,
                                        "SWAP",
                                        theme::PANEL,
                                        theme::TEXT,
                                        "Swap A/B",
                                        "Swap which copy is shown in flicker (space does the same).",
                                    )
                                {
                                    swap = true;
                                }
                            }
                        }
                    },
                );

                // Bottom metadata + hint strip.
                let bottom = egui::Rect::from_min_max(
                    egui::pos2(screen.min.x + 12.0, screen.max.y - 58.0),
                    egui::pos2(screen.max.x - 12.0, screen.max.y - 6.0),
                );
                ui.scope_builder(
                    egui::UiBuilder::new()
                        .max_rect(bottom)
                        .layout(egui::Layout::top_down(egui::Align::LEFT)),
                    |ui| {
                        let meta = |ui: &mut egui::Ui, tag: &str, f: &DupeFile| {
                            ui.label(
                                RichText::new(format!(
                                    "{tag}  {}  ·  {}  ·  {}",
                                    f.rel_path,
                                    format_size(f.entry.size),
                                    fmt_ms(
                                        f.entry
                                            .audio
                                            .as_ref()
                                            .map_or(0, |a| u64::from(a.duration_ms))
                                    ),
                                ))
                                .color(theme::TEXT)
                                .size(12.0),
                            );
                        };
                        meta(ui, "A", &a);
                        if let Some(bf) = &b_file {
                            meta(ui, "B", bf);
                        }
                        let view = if spectrogram { "waveform" } else { "spectrogram" };
                        let hint = if flicker {
                            format!(
                                "space: swap A/B · P play · {}/{} copy · S {view} · Esc back",
                                icon::CARET_LEFT,
                                icon::CARET_RIGHT,
                            )
                        } else if comparing {
                            format!(
                                "space: flicker · P play · click to play · {}/{} copy · S {view} · \
                                 C exit · Esc back",
                                icon::CARET_LEFT,
                                icon::CARET_RIGHT,
                            )
                        } else {
                            format!(
                                "P play/pause · click to play · {}/{} switch copy (keeps offset) · \
                                 C compare · S {view} · Esc back",
                                icon::CARET_LEFT,
                                icon::CARET_RIGHT,
                            )
                        };
                        ui.label(
                            RichText::new(hint)
                            .color(theme::LILAC)
                            .size(11.0),
                        );
                    },
                );
            });

        // Apply deferred actions now that drawing is done. Esc and space mirror
        // the image lightbox: Esc steps back one level (flicker → side-by-side →
        // single → closed); space enters flicker from side-by-side, then swaps.
        // Esc closes the tag editor first (if open), else backs out a level.
        if esc && self.tag_edit.is_some() {
            self.tag_edit = None;
            esc = false;
        }
        if esc {
            match state.compare.as_ref() {
                Some(c) if c.flicker => toggle_flicker = true,
                Some(_) => toggle_compare = true,
                None => close = true,
            }
        }
        if space {
            match state.compare.as_ref() {
                Some(c) if c.flicker => swap = true,
                Some(_) => toggle_flicker = true,
                None => {}
            }
        }
        if close {
            self.player.stop();
            self.spec_tex.clear();
            self.tag_edit = None;
            return; // dropped state = closed
        }
        // A tag EDIT button (or `T`) opens the editor for that copy; clicking it
        // again for the copy already open closes it (a toggle).
        if let Some(ei) = open_tags {
            let f = group[ei.min(count - 1)].clone();
            let (h, p, _t) = params(&f);
            if self.tag_edit.as_ref().map(|t| t.hex.as_str()) == Some(h.as_str()) {
                self.tag_edit = None;
            } else {
                let tags = self
                    .tags_cache
                    .entry(h.clone())
                    .or_insert_with(|| id3tags::read(&p))
                    .clone();
                // Collect the distinct value seen for each field across every
                // copy in the group, so the editor can offer them as options.
                let mut options: [Vec<String>; 6] = std::array::from_fn(|_| Vec::new());
                for gf in &group {
                    let (gh, gp, _) = params(gf);
                    if let Some(t) = self
                        .tags_cache
                        .entry(gh)
                        .or_insert_with(|| id3tags::read(&gp))
                        .clone()
                    {
                        let vals = [&t.title, &t.artist, &t.album, &t.year, &t.track, &t.genre];
                        for (i, v) in vals.into_iter().enumerate() {
                            if !v.is_empty() && !options[i].iter().any(|o| o == v) {
                                options[i].push(v.clone());
                            }
                        }
                    }
                }
                self.tag_edit = Some(TagEdit {
                    hex: h,
                    path: p,
                    tags: tags.unwrap_or_default(),
                    options,
                });
            }
        }
        if toggle_spec {
            state.spectrogram = !state.spectrogram;
        }

        let snap = self.player.snapshot();
        let cur_ms = snap.pos_ms;
        // Whether the player already holds exactly this (A, B) pair — if so, a
        // flicker swap is just an instant, gap-free volume flip.
        let paired_ab = comparing
            && snap.paired
            && snap.hex_a.as_deref() == Some(a_hex.as_str())
            && snap.hex_b.as_deref() == b_hex.as_deref();

        // Start (or re-target) playback at `offset`, making the chosen copy
        // audible. In compare mode both copies load into a synced pair so
        // flicker swaps are gap-free; otherwise a single file plays.
        let start_play = |me: &DupesView, want_b: bool, offset: u64| {
            if let (true, Some(bf)) = (comparing, b_file.as_ref()) {
                let (bh, bp, _bt) = params(bf);
                me.player
                    .play_pair(&a_hex, &a_path, &bh, &bp, _a_total, offset, want_b);
            } else {
                me.player.play(&a_hex, &a_path, _a_total, offset);
            }
        };

        // ←/→ : in single view, step which copy is A (keeping the offset). In
        // compare, *flip which copy is audible* instead — gap-free via the loaded
        // pair — and move the cursor with it. Re-indexing A while comparing would
        // collide it with B and force a reloading pause (the bug the user hit).
        let nav = new_idx != idx;
        if nav && comparing {
            if let Some(bi) = b_idx {
                let want_b = state.audio_active != Some(bi); // flip audible copy
                if snap.loaded {
                    let target = if want_b {
                        b_hex.as_deref()
                    } else {
                        Some(a_hex.as_str())
                    };
                    if paired_ab {
                        if snap.hex.as_deref() != target {
                            self.player.flip();
                        }
                    } else {
                        start_play(self, want_b, cur_ms);
                    }
                }
                state.audio_active = Some(if want_b { bi } else { idx });
                if let Some(c) = state.compare.as_mut()
                    && c.flicker
                {
                    c.show_b = want_b;
                }
            }
        } else if nav {
            idx = new_idx;
            state.index = idx;
            self.tag_edit = None; // tags belong to the copy we just left
            if snap.loaded {
                let (h, p, t) = params(&group[idx]);
                self.player.play(&h, &p, t, cur_ms.min(t));
                state.audio_active = Some(idx);
            }
        }
        if toggle_compare {
            if state.compare.is_some() {
                state.compare = None;
            } else if count >= 2 {
                let other = if idx == 0 { 1 } else { 0 };
                state.compare = Some(CompareState::new(other));
            }
        }
        if toggle_flicker && let Some(c) = state.compare.as_mut() {
            c.flicker = !c.flicker;
            // Entering flicker: show the copy that is currently audible.
            if c.flicker {
                c.show_b = state.audio_active.is_some() && state.audio_active == b_idx;
            }
        }
        // Keep the synced A/B pair loaded whenever comparing and playing, so both
        // flicker swaps *and* side-by-side clicks switch instantly (gap-free).
        // Skipped when another action this frame already (re)starts playback.
        let busy = nav || swap || toggle_play || click_play.is_some();
        if comparing && snap.playing && !paired_ab && !busy {
            start_play(self, state.audio_active == b_idx, cur_ms);
        }
        // space in flicker → swap the shown copy AND the audio, gap-free when the
        // pair is loaded (else load it), moving the cursor with it.
        if swap
            && let Some(c) = state.compare.as_mut()
            && c.flicker
        {
            c.show_b = !c.show_b;
        }
        if swap && state.compare.as_ref().is_some_and(|c| c.flicker) && snap.loaded {
            let show_b = state.compare.as_ref().is_some_and(|c| c.show_b);
            if paired_ab {
                self.player.flip();
            } else {
                start_play(self, show_b, cur_ms);
            }
            state.audio_active = if show_b { b_idx } else { Some(idx) };
        }
        if toggle_play {
            if snap.loaded {
                self.player.toggle_pause();
            } else {
                start_play(self, false, cur_ms.min(_a_total));
                state.audio_active = Some(idx);
            }
        }
        if let Some((is_b, frac)) = click_play {
            let want_b = is_b && comparing;
            if paired_ab && snap.playing {
                // Gap-free: flip to the clicked copy if it isn't already audible,
                // and seek only if the click actually moves the playhead — so
                // clicking the other copy at the same spot is an instant A/B swap.
                let wanted = if want_b {
                    b_hex.as_deref()
                } else {
                    Some(a_hex.as_str())
                };
                if snap.hex.as_deref() != wanted {
                    self.player.flip();
                }
                let cur = if snap.total_ms > 0 {
                    snap.pos_ms as f32 / snap.total_ms as f32
                } else {
                    0.0
                };
                if (cur - frac).abs() > 0.01 {
                    self.player.seek_fraction(frac);
                }
            } else {
                let offset = (f64::from(frac) * _a_total as f64) as u64;
                start_play(self, want_b, offset);
            }
            state.audio_active = if want_b { b_idx } else { Some(idx) };
            if let Some(c) = state.compare.as_mut()
                && c.flicker
            {
                c.show_b = want_b;
            }
        }

        // ID3 tag editor modal (Phase 6.5). Fields bind to the working copy;
        // SAVE writes tags only (audio untouched) and keeps the lightbox open.
        if self.tag_edit.is_some() {
            let (mut save, mut cancel) = (false, false);
            egui::Modal::new(Id::new("id3-edit")).show(&ctx.clone(), |ui| {
                let te = self.tag_edit.as_mut().unwrap();
                ui.set_width(440.0);
                ui.label(
                    RichText::new("EDIT ID3 TAGS")
                        .color(theme::AMBER)
                        .size(16.0)
                        .strong(),
                );
                ui.add_space(8.0);
                let field = |ui: &mut egui::Ui, label: &str, val: &mut String, opts: &[String]| {
                    ui.horizontal(|ui| {
                        ui.add_sized(
                            [56.0, 18.0],
                            egui::Label::new(RichText::new(label).color(theme::TAN).size(12.0)),
                        );
                        ui.add(egui::TextEdit::singleline(val).desired_width(300.0));
                        // Adopt a value from another copy in the group.
                        if !opts.is_empty() {
                            ui.menu_button(icon::CARET_RIGHT, |ui| {
                                for o in opts {
                                    if ui.button(RichText::new(o).color(theme::TEXT)).clicked() {
                                        *val = o.clone();
                                    }
                                }
                            })
                            .response
                            .on_hover_text("Pick a value from another copy in this group");
                        }
                    });
                };
                field(ui, "Title", &mut te.tags.title, &te.options[0]);
                field(ui, "Artist", &mut te.tags.artist, &te.options[1]);
                field(ui, "Album", &mut te.tags.album, &te.options[2]);
                field(ui, "Year", &mut te.tags.year, &te.options[3]);
                field(ui, "Track", &mut te.tags.track, &te.options[4]);
                field(ui, "Genre", &mut te.tags.genre, &te.options[5]);
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui
                        .add(
                            egui::Button::new(RichText::new("SAVE TAGS").color(theme::BLACK))
                                .fill(theme::AMBER),
                        )
                        .clicked()
                    {
                        save = true;
                    }
                    if ui
                        .button(RichText::new("CANCEL").color(theme::TEXT))
                        .clicked()
                    {
                        cancel = true;
                    }
                });
                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "Saving writes the tags to the file on disk; the audio is unchanged.",
                    )
                    .color(theme::LILAC)
                    .size(11.0),
                );
            });
            if cancel {
                self.tag_edit = None;
            } else if save && let Some(te) = self.tag_edit.take() {
                match id3tags::write(&te.path, &te.tags) {
                    Ok(()) => {
                        self.status = Some("Tags saved".into());
                        self.error = None;
                        self.tags_cache.insert(te.hex, Some(te.tags));
                    }
                    Err(e) => self.error = Some(format!("Tag save failed: {e}")),
                }
            }
        }

        self.lightbox = Some(state);
        // Keep repainting while a copy plays so the cursor advances smoothly.
        if self.player.is_active() {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
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
                        // A repo turned read-only starts fully locked again.
                        self.unlocked.retain(|(repo, _)| repo != &name);
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
                self.lightbox = Some(LightboxState::new(gi, fi));
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
                view.show(ui, &store, TooltipVerbosity::default());
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
            .with_size(egui::vec2(600.0, 400.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx());
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
            .with_size(egui::vec2(600.0, 400.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx());
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
                        crate::theme::apply(ui.ctx());
                        init = true;
                    }
                    view.show(ui, &store_ui, TooltipVerbosity::default());
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
                        crate::theme::apply(ui.ctx());
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
                        crate::theme::apply(ui.ctx());
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
            },
            RepoSel {
                name: "ro".into(),
                included: true,
                read_only: true,
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
                        crate::theme::apply(ui.ctx());
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
                        crate::theme::apply(ui.ctx());
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

    /// The lightbox opens over a group, steps through its members with the
    /// arrow keys, toggles the shown file's mark with `K`, and closes on `Esc`.
    #[test]
    fn lightbox_opens_navigates_marks_and_closes() {
        let group: DupeGroup = (0..3).map(image_file).collect();

        let mut view = DupesView::new();
        view.repos_loaded = true; // fabricated groups, no repos needed
        view.results = Some(Results::Similar(vec![group.clone()]));

        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
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
                    let _ = &tmp;
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();

        // Open the lightbox on the first (best) member.
        harness.state_mut().lightbox = Some(LightboxState::new(0, 0));
        harness.run();
        assert!(
            harness.query_by_label("1 / 3").is_some(),
            "lightbox shows the 1/3 position counter"
        );
        assert!(
            harness
                .query_by_label(&format!("{} CLOSE", icon::CHECK))
                .is_some(),
            "lightbox shows a CLOSE control"
        );

        // Mark the best copy (index 0 is never default-marked), via `K`.
        let best_key = key(&group[0]);
        harness.key_press(egui::Key::K);
        harness.run();
        assert!(
            harness.state().marked.contains(&best_key),
            "K marks the shown file"
        );

        // Step to the next member.
        harness.key_press(egui::Key::ArrowRight);
        harness.run();
        assert_eq!(
            harness.state().lightbox.as_ref().map(|l| l.index),
            Some(1),
            "ArrowRight advances to the second member"
        );
        assert!(
            harness.query_by_label("2 / 3").is_some(),
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
            .with_size(egui::vec2(900.0, 700.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx());
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
                        crate::theme::apply(ui.ctx());
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

    /// Clicking an audio card opens the dedicated audio lightbox (not the image
    /// viewer); `P` plays, `S` toggles the spectrogram, `C` compares, `space`
    /// drives flicker (enter then swap), and `Esc` steps back one level at a time.
    #[test]
    fn audio_lightbox_opens_compares_plays_and_escapes() {
        let group: DupeGroup = (0..2).map(audio_file).collect();
        let a_hex = hash_hex(&group[0].entry.hash);
        let b_hex = hash_hex(&group[1].entry.hash);

        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.results = Some(Results::Similar(vec![group]));

        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1000.0, 700.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx());
                        init = true;
                    }
                    let _ = &tmp;
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();
        harness.state_mut().lightbox = Some(LightboxState::new(0, 0));
        harness.run();

        // The audio lightbox shows its own controls (CLOSE + COMPARE are unique
        // to it; the image viewer's FIT/1:1 must be absent). PLAY is ambiguous
        // because the card behind the overlay also has one, so it isn't queried.
        assert!(
            harness
                .query_by_label(&format!("{} CLOSE", icon::CHECK))
                .is_some(),
            "audio lightbox shows a CLOSE control"
        );
        assert!(
            harness.query_by_label("COMPARE").is_some(),
            "audio lightbox offers A/B compare"
        );
        assert!(
            harness.query_by_label("FIT").is_none(),
            "audio lightbox is not the image viewer"
        );

        // P plays the current copy (A). Playback keeps repainting, so step a
        // fixed number of frames rather than running to a settled state.
        harness.key_press(egui::Key::P);
        harness.step();
        harness.step();
        assert_eq!(
            harness.state().player.snapshot().hex.as_deref(),
            Some(a_hex.as_str()),
            "P plays the current copy"
        );

        // S toggles the spectrogram view.
        harness.key_press(egui::Key::S);
        harness.step();
        harness.step();
        assert!(
            harness.state().lightbox.as_ref().unwrap().spectrogram,
            "S switches to the spectrogram view"
        );

        // C enters compare; space then enters flicker and swaps A/B — mirroring
        // the image lightbox.
        harness.key_press(egui::Key::C);
        harness.step();
        harness.step();
        assert!(
            harness.state().lightbox.as_ref().unwrap().compare.is_some(),
            "C enters compare"
        );
        // Comparing while playing loads the synced A/B pair even in side-by-side
        // (not just flicker), so a click on either copy switches gap-free.
        harness.step();
        assert!(
            harness.state().player.snapshot().paired,
            "side-by-side compare keeps the A/B pair loaded"
        );

        harness.key_press(egui::Key::Space);
        harness.step();
        harness.step();
        assert!(
            harness
                .state()
                .lightbox
                .as_ref()
                .unwrap()
                .compare
                .as_ref()
                .is_some_and(|c| c.flicker && !c.show_b),
            "space enters flicker showing A"
        );
        // Entering flicker while playing loads the synced A/B pair, still audible A.
        let snap = harness.state().player.snapshot();
        assert!(
            snap.paired,
            "flicker loads the A/B pair for gap-free swapping"
        );
        assert_eq!(
            snap.hex.as_deref(),
            Some(a_hex.as_str()),
            "A is audible first"
        );

        harness.key_press(egui::Key::Space);
        harness.step();
        harness.step();
        assert!(
            harness
                .state()
                .lightbox
                .as_ref()
                .unwrap()
                .compare
                .as_ref()
                .is_some_and(|c| c.flicker && c.show_b),
            "space swaps A/B within flicker"
        );
        // The swap flips the audio too (gap-free): B is now the audible channel.
        assert_eq!(
            harness.state().player.snapshot().hex.as_deref(),
            Some(b_hex.as_str()),
            "flicker swap makes B audible"
        );

        // Esc steps back: flicker → side-by-side → single → closed.
        for expect in ["flicker-off", "compare-off", "closed"] {
            harness.key_press(egui::Key::Escape);
            harness.step();
            harness.step();
            match expect {
                "flicker-off" => assert!(
                    harness
                        .state()
                        .lightbox
                        .as_ref()
                        .unwrap()
                        .compare
                        .as_ref()
                        .is_some_and(|c| !c.flicker),
                    "Esc leaves flicker back to side-by-side"
                ),
                "compare-off" => assert!(
                    harness.state().lightbox.as_ref().unwrap().compare.is_none(),
                    "Esc leaves compare back to the single view"
                ),
                _ => assert!(
                    harness.state().lightbox.is_none(),
                    "Esc from the single view closes the lightbox"
                ),
            }
        }
    }

    /// `T` opens the audio lightbox's ID3 editor pre-filled with the file's
    /// tags; editing a field and clicking SAVE TAGS writes only the tags to
    /// disk, preserves the others, and keeps the lightbox open (6.5/6.6).
    #[test]
    fn audio_lightbox_edits_and_saves_id3_tags() {
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
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1000.0, 700.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx());
                        init = true;
                    }
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();
        harness.state_mut().lightbox = Some(LightboxState::new(0, 0));
        harness.run();

        // T opens the editor, pre-filled from the file.
        harness.key_press(egui::Key::T);
        harness.run();
        harness.run();
        assert_eq!(
            harness
                .state()
                .tag_edit
                .as_ref()
                .map(|t| t.tags.title.as_str()),
            Some("Old"),
            "editor opens pre-filled with the current title"
        );

        // Type a new title, then SAVE TAGS.
        harness.state_mut().tag_edit.as_mut().unwrap().tags.title = "New Title".into();
        harness.run();
        harness.get_by_label("SAVE TAGS").click();
        harness.run();

        let saved = crate::id3tags::read(&mp3).expect("tags still readable");
        assert_eq!(saved.title, "New Title", "the new title is written to disk");
        assert_eq!(saved.artist, "Cohen", "other tags are preserved");
        assert!(
            harness.state().tag_edit.is_none(),
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
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1000.0, 700.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx());
                        init = true;
                    }
                    let _ = &tmp;
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();
        harness.state_mut().lightbox = Some(LightboxState::new(0, 0));
        harness.run();

        // Enter compare, then play → the synced pair loads (A audible).
        harness.key_press(egui::Key::C);
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

    /// Side-by-side compare shows a tag panel per row: A's tags beside the A
    /// wave, B's beside the B wave (symmetric), each with its own EDIT button.
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
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1000.0, 700.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx());
                        init = true;
                    }
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();
        harness.state_mut().lightbox = Some(LightboxState::new(0, 0));
        harness.run();
        harness.key_press(egui::Key::C);
        harness.run();
        harness.run();

        // A's value shows in the A row and B's value in the B row (not blank).
        assert!(
            harness.query_by_label("Alpha").is_some(),
            "A's tags render in the top panel"
        );
        assert!(
            harness.query_by_label("Beta").is_some(),
            "B's tags render in the B row (the regression the user hit)"
        );
        // One EDIT button per panel.
        let edits = harness
            .get_all_by_label(&format!("{} EDIT", icon::PENCIL))
            .count();
        assert_eq!(edits, 2, "an EDIT button per copy");
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
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1000.0, 700.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx());
                        init = true;
                    }
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();
        harness.state_mut().lightbox = Some(LightboxState::new(0, 0));
        harness.run();
        harness.key_press(egui::Key::T);
        harness.run();
        harness.run();

        let te = harness.state();
        let te = te.tag_edit.as_ref().expect("editor open");
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

    /// `C` enters A/B compare, which exposes MARK B and a FLICKER toggle, marks
    /// the B candidate, and exits back to single view.
    #[test]
    fn lightbox_compare_enters_marks_b_and_exits() {
        let group: DupeGroup = (0..3).map(image_file).collect();
        let b_key = key(&group[1]); // A is index 0 → B defaults to index 1

        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.results = Some(Results::Similar(vec![group]));

        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1000.0, 700.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx());
                        init = true;
                    }
                    let _ = &tmp;
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();
        harness.state_mut().lightbox = Some(LightboxState::new(0, 0));
        harness.run();

        // Enter compare (applied after the frame; drawn on the next).
        harness.key_press(egui::Key::C);
        harness.run();
        harness.run();
        assert!(
            harness.state().lightbox.as_ref().unwrap().compare.is_some(),
            "C enters compare mode"
        );
        assert!(
            harness.query_by_label("EXIT COMPARE").is_some(),
            "compare exposes an EXIT COMPARE control"
        );
        assert!(
            harness.query_by_label("FLICKER").is_some(),
            "compare exposes the FLICKER toggle"
        );

        // Compare stacks three bottom lines (path A, path B, hint) where the
        // single view needs only two. The strip must grow so the hint stays
        // inside it (above the 6px bottom margin of the 700px window) rather
        // than being pushed off the bottom edge — the reason it looked like the
        // hint "vanished" on entering compare.
        let hint_bottom = harness
            .get_by_label_contains("space: flicker")
            .rect()
            .bottom();
        assert!(
            hint_bottom <= 694.0,
            "compare hint stays inside the bottom strip, not off-screen: bottom {hint_bottom:.1}"
        );

        // Clear preselected marks so B shows the unmarked MARK B control.
        harness.state_mut().marked.clear();
        harness.run();
        assert!(
            harness.query_by_label("MARK B").is_some(),
            "compare exposes a MARK B control for the candidate"
        );

        // Del marks the B candidate.
        harness.key_press(egui::Key::Delete);
        harness.run();
        assert!(
            harness.state().marked.contains(&b_key),
            "Del marks the B candidate in compare mode"
        );

        // Exit compare.
        harness.key_press(egui::Key::C);
        harness.run();
        assert!(
            harness.state().lightbox.as_ref().unwrap().compare.is_none(),
            "C exits compare mode"
        );
    }

    /// Space enters flicker then swaps A/B; Escape is a hierarchical "back"
    /// that pops one view level per press: flicker → side-by-side → single →
    /// closed.
    #[test]
    fn lightbox_space_flicker_and_escape_back() {
        let group: DupeGroup = (0..3).map(image_file).collect();

        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.results = Some(Results::Similar(vec![group]));

        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1000.0, 700.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx());
                        init = true;
                    }
                    let _ = &tmp;
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();
        harness.state_mut().lightbox = Some(LightboxState::new(0, 0));
        harness.run();

        // Enter compare — starts in side-by-side (not flicker).
        harness.key_press(egui::Key::C);
        harness.run();
        harness.run();
        assert!(
            harness
                .state()
                .lightbox
                .as_ref()
                .unwrap()
                .compare
                .as_ref()
                .is_some_and(|c| !c.flicker),
            "C enters compare in side-by-side"
        );

        // Space enters flicker from side-by-side, showing A.
        harness.key_press(egui::Key::Space);
        harness.run();
        harness.run();
        assert!(
            harness
                .state()
                .lightbox
                .as_ref()
                .unwrap()
                .compare
                .as_ref()
                .is_some_and(|c| c.flicker && !c.show_b),
            "space enters flicker showing A"
        );

        // Space again swaps A/B within flicker.
        harness.key_press(egui::Key::Space);
        harness.run();
        harness.run();
        assert!(
            harness
                .state()
                .lightbox
                .as_ref()
                .unwrap()
                .compare
                .as_ref()
                .is_some_and(|c| c.flicker && c.show_b),
            "space swaps A/B within flicker"
        );

        // Escape steps back one level: flicker → side-by-side (still comparing).
        harness.key_press(egui::Key::Escape);
        harness.run();
        harness.run();
        assert!(
            harness
                .state()
                .lightbox
                .as_ref()
                .unwrap()
                .compare
                .as_ref()
                .is_some_and(|c| !c.flicker),
            "Escape leaves flicker back to side-by-side, staying in compare"
        );

        // Escape again: side-by-side → single image.
        harness.key_press(egui::Key::Escape);
        harness.run();
        harness.run();
        assert!(
            harness.state().lightbox.as_ref().unwrap().compare.is_none(),
            "Escape leaves compare back to the single image"
        );

        // Escape again: single image → closed.
        harness.key_press(egui::Key::Escape);
        harness.run();
        harness.run();
        assert!(
            harness.state().lightbox.is_none(),
            "Escape from the single image closes the lightbox"
        );
    }

    /// The image lightbox's Edit controls rotate the live preview and can save a
    /// `_rot` copy, leaving the original untouched and the lightbox open (6.4/6.6).
    #[test]
    fn lightbox_edit_rotates_and_saves_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let img_path = tmp.path().join("shot.png");
        image::RgbImage::from_fn(40, 20, |x, _| image::Rgb([x as u8, 0, 0]))
            .save(&img_path)
            .unwrap();

        let mut hash = [0u8; 32];
        hash[0] = 0x7E;
        let file = DupeFile {
            repo: "r".into(),
            repo_root: tmp.path().to_string_lossy().into_owned(),
            rel_path: "shot.png".into(),
            entry: dedup_core::store::FileEntry {
                size: 100,
                hash,
                modified_ms: 0,
                missing: false,
                mime: Some("image/png".into()),
                img_fingerprint: None,
                video_hash: None,
                pdf_hash: None,
                audio: None,
                img_size: Some((40, 20)),
                origin: None,
                exif: None,
            },
        };
        let group: DupeGroup = vec![file];

        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.results = Some(Results::Similar(vec![group]));

        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1100.0, 700.0))
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx());
                        init = true;
                    }
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();
        harness.state_mut().lightbox = Some(LightboxState::new(0, 0));
        harness.run();

        // Rotate clockwise → the preview's dimensions swap (40×20 → 20×40).
        harness.get_by_label("ROT R").click();
        harness.run();
        harness.run();
        let dims = harness.state().edit.as_ref().map(|e| e.dims);
        assert_eq!(
            dims,
            Some(egui::vec2(20.0, 40.0)),
            "rotate swaps preview dims"
        );

        // SAVE opens the confirm modal (does not write yet).
        harness
            .get_by_label(&format!("{} SAVE", icon::CHECK))
            .click();
        harness.run();
        harness.run();
        assert!(harness.state().edit_save, "SAVE opens the confirm modal");

        // Save a copy → a rotated sibling is written, the original untouched, and
        // the lightbox stays open.
        harness.get_by_label("SAVE A COPY").click();
        harness.run();
        let copy = tmp.path().join("shot_rot.png");
        assert!(copy.exists(), "a rotated copy is written");
        assert_eq!(
            image::image_dimensions(&copy).unwrap(),
            (20, 40),
            "the copy is rotated"
        );
        assert_eq!(
            image::image_dimensions(&img_path).unwrap(),
            (40, 20),
            "the original is left untouched"
        );
        assert!(!harness.state().edit_save, "the modal closes after saving");
        assert!(
            harness.state().lightbox.is_some(),
            "saving keeps the lightbox open (6.6)"
        );
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
                view.show(ui, &store, TooltipVerbosity::default());
            });
        harness.run();
        harness.snapshot("dupes_view");
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
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1000.0, 640.0))
            .wgpu()
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx());
                        init = true;
                    }
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();
        harness.state_mut().lightbox = Some(LightboxState::new(0, 0));
        harness.run();
        // Side-by-side compare → the read-only id3 diff table on the right.
        harness.key_press(egui::Key::C);
        harness.run();
        harness.run();
        let img = harness.render().expect("wgpu render failed");
        let out =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/dupes_tags.png");
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());
    }

    /// Renders the image lightbox mid-edit (rotated preview + Edit controls +
    /// the save-confirm modal) to `target/dupes_edit.png`. `--ignored`.
    #[test]
    #[ignore = "renders a PNG for manual inspection"]
    fn render_lightbox_edit() {
        let tmp = tempfile::tempdir().unwrap();
        let img_path = tmp.path().join("shot.png");
        // A directional gradient so a rotation is obvious.
        image::RgbImage::from_fn(400, 240, |x, y| image::Rgb([(x / 2) as u8, (y) as u8, 90]))
            .save(&img_path)
            .unwrap();
        let mut hash = [0u8; 32];
        hash[0] = 0x7E;
        let file = DupeFile {
            repo: "r".into(),
            repo_root: tmp.path().to_string_lossy().into_owned(),
            rel_path: "shot.png".into(),
            entry: dedup_core::store::FileEntry {
                size: 100,
                hash,
                modified_ms: 0,
                missing: false,
                mime: Some("image/png".into()),
                img_fingerprint: None,
                video_hash: None,
                pdf_hash: None,
                audio: None,
                img_size: Some((400, 240)),
                origin: None,
                exif: None,
            },
        };
        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.results = Some(Results::Similar(vec![vec![file]]));

        let store = Arc::new(Store::open_at(tmp.path().join("cfg")).unwrap());
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1100.0, 700.0))
            .wgpu()
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx());
                        init = true;
                    }
                    let _ = &tmp;
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();
        harness.state_mut().lightbox = Some(LightboxState::new(0, 0));
        harness.run();
        harness.get_by_label("ROT R").click();
        harness.run();
        harness.run();
        harness
            .get_by_label(&format!("{} SAVE", icon::CHECK))
            .click();
        harness.run();
        harness.run();
        let img = harness.render().expect("wgpu render failed");
        let out =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/dupes_edit.png");
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
                    crate::theme::apply(ui.ctx());
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
                        crate::theme::apply(ui.ctx());
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
    /// `target/dupes_audio_lightbox.png` for manual inspection. `--ignored`.
    #[test]
    #[ignore = "renders a PNG for manual inspection"]
    fn render_audio_lightbox() {
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
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1120.0, 640.0))
            .wgpu()
            .build_ui_state(
                move |ui, view: &mut DupesView| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx());
                        init = true;
                    }
                    let _ = &tmp;
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();
        harness.state_mut().lightbox = Some(LightboxState::new(0, 0));
        harness.step();
        {
            let lb = harness.state_mut().lightbox.as_mut().unwrap();
            lb.compare = Some(CompareState::new(1));
            lb.spectrogram = true;
        }
        // Give the background workers time to decode both WAVs into spectrograms.
        for _ in 0..60 {
            std::thread::sleep(std::time::Duration::from_millis(10));
            harness.step();
        }
        let img = harness.render().expect("wgpu render failed");
        let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/dupes_audio_lightbox.png");
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
        let mut group: DupeGroup = Vec::new();
        for i in 0..3u8 {
            let rel = format!("photo{i}.png");
            let path = dir.path().join(&rel);
            image::RgbImage::from_fn(640, 480, |x, y| {
                image::Rgb([x as u8, y as u8, (i as u32 * 60) as u8])
            })
            .save(&path)
            .unwrap();
            let mut hash = [0u8; 32];
            hash[0] = i;
            group.push(DupeFile {
                repo: "r".into(),
                repo_root: dir.path().to_string_lossy().into_owned(),
                rel_path: rel,
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
                    img_size: Some((640, 480)),
                    origin: None,
                    exif: None,
                },
            });
        }

        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.results = Some(Results::Similar(vec![group]));
        let mut lb = LightboxState::new(0, 0);
        lb.compare = Some(CompareState::new(1)); // render A/B side-by-side
        view.lightbox = Some(lb);

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
                        crate::theme::apply(ui.ctx());
                        init = true;
                    }
                    let _ = (&tmp, &dir);
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        // Several frames with pauses so the background decode lands.
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
        view.lightbox = Some(LightboxState::new(0, 0));

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
                        crate::theme::apply(ui.ctx());
                        init = true;
                    }
                    let _ = (&tmp, &dir);
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        // Give the ffmpeg extraction workers time to produce all stills.
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
                    view.show(ui, &store_ui, TooltipVerbosity::default());
                },
                view,
            );
        harness.run();
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
        let mut group: DupeGroup = Vec::new();
        for i in 0..3u8 {
            let rel = format!("photo{i}.png");
            let path = dir.path().join(&rel);
            image::RgbImage::from_fn(640, 480, |x, y| {
                image::Rgb([x as u8, y as u8, (i as u32 * 60) as u8])
            })
            .save(&path)
            .unwrap();
            let mut hash = [0u8; 32];
            hash[0] = i;
            group.push(DupeFile {
                repo: "r".into(),
                repo_root: dir.path().to_string_lossy().into_owned(),
                rel_path: rel,
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
                    img_size: Some((640, 480)),
                    origin: None,
                    exif: None,
                },
            });
        }

        let mut view = DupesView::new();
        view.repos_loaded = true;
        view.results = Some(Results::Similar(vec![group]));
        let mut lb = LightboxState::new(0, 0);
        lb.compare = Some(CompareState::new(1));
        view.lightbox = Some(lb);

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
                        crate::theme::apply(ui.ctx());
                        init = true;
                    }
                    let _ = (&tmp, &dir);
                    view.show(ui, &store, TooltipVerbosity::default());
                },
                view,
            );
        for _ in 0..12 {
            harness.run();
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        harness.run();
        let img = harness.render().expect("wgpu render failed");
        let out = doc_screenshot_path("lightbox_compare.png");
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());
    }
}
