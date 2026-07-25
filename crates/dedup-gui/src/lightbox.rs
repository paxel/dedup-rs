//! Full-window image viewer (lightbox): click a duplicate's thumbnail to judge
//! it at pixel level — wheel to zoom around the cursor, drag to pan, arrow keys
//! to step through the group, and mark/close without leaving the app.
//!
//! Full-resolution decoding must never block the UI, so it reuses the
//! `thumbs.rs` worker pattern with a tiny, aggressively-evicted cache (a 50 MP
//! photo is ~200 MB of RGBA — only the current image and its neighbours stay
//! resident). While a decode is in flight the caller draws the 512-px thumbnail
//! scaled up.

use crate::icon;
use crate::media_cell::FileFacts;
use crate::settings::TooltipVerbosity;
use crate::theme;
use crate::thumbs::ThumbCache;
use crate::util::ExplainExt;
use crossbeam_channel::{Receiver, Sender};
use egui::{ColorImage, Context, Rect, RichText, TextureHandle, TextureOptions, Vec2};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Longest texture edge uploaded to the GPU; larger images are downscaled by
/// the decoder to stay within driver limits (commonly 8192 px).
const MAX_TEXTURE_EDGE: u32 = 8192;
/// How many full-resolution textures stay resident (current + a few neighbours).
const FULL_CACHE_CAP: usize = 3;

const MIN_SCALE: f32 = 0.02;
const MAX_SCALE: f32 = 32.0;

/// Classification of file representation tabs available in the Lightbox.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RepresentationKind {
    Overview,
    Metadata,
    Image,
    Audio,
    Video,
    Text,
}

impl RepresentationKind {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Metadata => "Metadata",
            Self::Image => "Image",
            Self::Audio => "Audio",
            Self::Video => "Video",
            Self::Text => "Text",
        }
    }

    pub fn icon(&self) -> &'static str {
        match self {
            Self::Overview => icon::STAR,
            Self::Metadata => icon::PENCIL,
            Self::Image => icon::IMAGE,
            Self::Audio => icon::LIGHTNING,
            Self::Video => icon::IMAGE,
            Self::Text => icon::SEARCH,
        }
    }
}

/// Base deduplication metadata representation of a file instance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DedupDataRepresentation {
    pub rel_path: String,
    pub repo_name: String,
    pub size: u64,
    pub modified_ms: i64,
    pub mime: Option<String>,
    pub read_only: bool,
    pub hash_hex: String,
    pub abs_path: PathBuf,
}

/// Image media representation facts and capability flags.
#[derive(Clone)]
pub struct ImageRepresentation {
    pub dimensions: Option<(u32, u32)>,
    pub texture: Option<TextureHandle>,
    pub supports_flicker: bool,
    pub can_rotate: bool,
    pub can_crop: bool,
    pub can_save: bool,
}

impl std::fmt::Debug for ImageRepresentation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageRepresentation")
            .field("dimensions", &self.dimensions)
            .field("has_texture", &self.texture.is_some())
            .field("supports_flicker", &self.supports_flicker)
            .field("can_rotate", &self.can_rotate)
            .field("can_crop", &self.can_crop)
            .field("can_save", &self.can_save)
            .finish()
    }
}

/// Audio media representation facts and capability flags.
#[derive(Clone)]
pub struct AudioRepresentation {
    pub duration_ms: Option<u32>,
    pub spectrogram_texture: Option<TextureHandle>,
    pub is_playing: bool,
    pub seek_position_ms: u32,
    pub can_play: bool,
}

impl std::fmt::Debug for AudioRepresentation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioRepresentation")
            .field("duration_ms", &self.duration_ms)
            .field("has_spectrogram", &self.spectrogram_texture.is_some())
            .field("is_playing", &self.is_playing)
            .field("seek_position_ms", &self.seek_position_ms)
            .field("can_play", &self.can_play)
            .finish()
    }
}

/// Metadata (ID3/EXIF) representation facts and edit capabilities.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MetadataRepresentation {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub year: Option<u32>,
    pub track: Option<u32>,
    pub comment: Option<String>,
    pub can_save: bool,
}

/// Video media representation facts and capability flags.
#[derive(Clone)]
pub struct VideoRepresentation {
    pub duration_ms: Option<u32>,
    pub dimensions: Option<(u32, u32)>,
    pub filmstrip_textures: Vec<TextureHandle>,
    pub selected_frame: Option<usize>,
    pub is_playing: bool,
}

impl std::fmt::Debug for VideoRepresentation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoRepresentation")
            .field("duration_ms", &self.duration_ms)
            .field("dimensions", &self.dimensions)
            .field("filmstrip_count", &self.filmstrip_textures.len())
            .field("selected_frame", &self.selected_frame)
            .field("is_playing", &self.is_playing)
            .finish()
    }
}

/// Text or raw binary preview representation facts.
#[derive(Clone, Debug)]
pub struct TextBinaryRepresentation {
    pub text_preview: Option<String>,
    pub hex_dump: Option<String>,
    pub is_text: bool,
}

/// Deletion mark state for a file in a duplicate group or comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkState {
    Unmarked,
    Delete,
    DeleteA,
    DeleteB,
    Protected,
}

impl MarkState {
    pub fn is_protected(&self) -> bool {
        matches!(self, Self::Protected)
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Unmarked => "UNMARKED",
            Self::Delete => "DELETE",
            Self::DeleteA => "DELETE A",
            Self::DeleteB => "DELETE B",
            Self::Protected => "DELETE (Protected)",
        }
    }
}

/// Aggregated representations for a single file instance.
#[derive(Clone, Debug)]
pub struct FileRepresentations {
    pub dedup: DedupDataRepresentation,
    pub image: Option<ImageRepresentation>,
    pub audio: Option<AudioRepresentation>,
    pub metadata: Option<MetadataRepresentation>,
    pub video: Option<VideoRepresentation>,
    pub text: Option<TextBinaryRepresentation>,
    pub mark: MarkState,
}

impl FileRepresentations {
    /// Construct representations from generic [`FileFacts`].
    pub fn from_facts(
        facts: &FileFacts,
        repo_name: String,
        read_only: bool,
        mark: MarkState,
    ) -> Self {
        let is_img = facts.is_image();
        let is_aud = facts.is_audio();
        let is_vid = facts.is_video();
        let can_write = !read_only;

        let image = if is_img {
            Some(ImageRepresentation {
                dimensions: facts.img_size,
                texture: None,
                supports_flicker: true,
                can_rotate: can_write,
                can_crop: can_write,
                can_save: can_write,
            })
        } else {
            None
        };

        let audio = if is_aud {
            Some(AudioRepresentation {
                duration_ms: facts.audio_ms,
                spectrogram_texture: None,
                is_playing: false,
                seek_position_ms: 0,
                can_play: true,
            })
        } else {
            None
        };

        let video = if is_vid {
            Some(VideoRepresentation {
                duration_ms: facts.audio_ms,
                dimensions: facts.img_size,
                filmstrip_textures: Vec::new(),
                selected_frame: None,
                is_playing: false,
            })
        } else {
            None
        };

        let metadata = if is_aud || is_img {
            Some(MetadataRepresentation {
                title: None,
                artist: None,
                album: None,
                year: None,
                track: None,
                comment: None,
                can_save: can_write,
            })
        } else {
            None
        };

        let text = if !is_img && !is_aud && !is_vid {
            Some(TextBinaryRepresentation {
                text_preview: None,
                hex_dump: None,
                is_text: true,
            })
        } else {
            None
        };

        Self {
            dedup: DedupDataRepresentation {
                rel_path: facts
                    .abs_path
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default(),
                repo_name,
                size: facts.size,
                modified_ms: facts.modified_ms,
                mime: facts.mime.clone(),
                read_only,
                hash_hex: facts.hash_hex.clone(),
                abs_path: facts.abs_path.clone(),
            },
            image,
            audio,
            metadata,
            video,
            text,
            mark,
        }
    }

    /// List representation kinds supported by this file instance.
    pub fn available_kinds(&self) -> Vec<RepresentationKind> {
        let mut kinds = vec![RepresentationKind::Overview];
        if self.metadata.is_some() {
            kinds.push(RepresentationKind::Metadata);
        }
        if self.image.is_some() {
            kinds.push(RepresentationKind::Image);
        }
        if self.audio.is_some() {
            kinds.push(RepresentationKind::Audio);
        }
        if self.video.is_some() {
            kinds.push(RepresentationKind::Video);
        }
        if self.text.is_some() {
            kinds.push(RepresentationKind::Text);
        }
        kinds
    }

    pub fn mark_state(&self) -> MarkState {
        self.mark
    }

    pub fn dedup_data(&self) -> &DedupDataRepresentation {
        &self.dedup
    }
}

/// Given total group members $N$ and the left file index `left_idx` ($0 \le \text{left\_idx} < N$),
/// return the list of valid "other" member indices (length $N - 1$).
pub fn other_member_indices(total_group_len: usize, left_idx: usize) -> Vec<usize> {
    (0..total_group_len).filter(|&i| i != left_idx).collect()
}

/// Format the Right-side switcher label for `other_sel` index within `others` list.
/// Returns `<current / total_others>` e.g. `<1 / 3>` for a 4-file group with 3 other files.
pub fn format_other_switcher_label(other_sel: usize, total_others: usize) -> String {
    if total_others == 0 {
        "0/0".to_string()
    } else {
        let current = (other_sel % total_others) + 1;
        format!("<{current} / {total_others}>")
    }
}

/// Draw the top tab bar displaying representation tabs available across Left (A) and Right (B).
pub fn draw_tab_bar(
    ui: &mut egui::Ui,
    state: &mut LightboxState,
    left_reps: &FileRepresentations,
    right_reps: &FileRepresentations,
) {
    let left_kinds = left_reps.available_kinds();
    let right_kinds = right_reps.available_kinds();
    let mut all_kinds = left_kinds;
    for k in right_kinds {
        if !all_kinds.contains(&k) {
            all_kinds.push(k);
        }
    }
    all_kinds.sort();

    ui.horizontal(|ui| {
        for kind in all_kinds {
            let label = format!("{} {}", kind.icon(), kind.name());
            let selected = state.active_tab == kind;
            let fill = if selected { theme::AMBER } else { theme::PANEL };
            let text_color = if selected { theme::BLACK } else { theme::TEXT };

            if ui
                .add(egui::Button::new(RichText::new(label).color(text_color)).fill(fill))
                .clicked()
            {
                state.active_tab = kind;
            }
        }
    });
}

/// Render the Overview tab side-by-side view for Left and Right [`FileRepresentations`].
pub fn draw_overview_mode(
    ui: &mut egui::Ui,
    left_reps: &FileRepresentations,
    right_reps: &FileRepresentations,
    other_sel: usize,
    total_others: usize,
) -> Option<RepresentationKind> {
    let mut switch_to_kind = None;
    let (left_pane, right_pane) = compare_split(ui.available_rect_before_wrap());

    // Left Column
    ui.scope_builder(egui::UiBuilder::new().max_rect(left_pane), |ui| {
        ui.heading(&left_reps.dedup_data().rel_path);
        ui.label(format!("Repo: {}", left_reps.dedup_data().repo_name));
        ui.label(format!("Size: {} bytes", left_reps.dedup_data().size));
        ui.label(format!("Mark: {}", left_reps.mark_state().label()));
    });

    // Right Column
    ui.scope_builder(egui::UiBuilder::new().max_rect(right_pane), |ui| {
        ui.heading(&right_reps.dedup_data().rel_path);
        ui.label(format!("Repo: {}", right_reps.dedup_data().repo_name));
        ui.label(format!("Size: {} bytes", right_reps.dedup_data().size));
        ui.label(format!("Mark: {}", right_reps.mark_state().label()));

        if total_others > 0 {
            ui.label(format_other_switcher_label(other_sel, total_others));
        }
    });

    // If both Left and Right support a media representation (Image, Audio, etc.), render a Compare button
    let common_kinds: Vec<RepresentationKind> = left_reps
        .available_kinds()
        .into_iter()
        .filter(|k| *k != RepresentationKind::Overview && right_reps.available_kinds().contains(k))
        .collect();

    if let Some(&first_kind) = common_kinds.first()
        && ui
            .button(format!("Compare {}", first_kind.name()))
            .clicked()
    {
        switch_to_kind = Some(first_kind);
    }

    switch_to_kind
}

/// A/B compare overlaid on the lightbox. `b` is the abstract B side — the
/// *rendering source* it compares A against, as viewer-agnostic [`FileFacts`]
/// (A is the lightbox's current `index`). Today the Duplicate lightbox points it
/// at another group member; generalising it off the group index lets a later
/// slice point B at a file in another repo (DIFF / cross-type compare). Zoom/pan
/// are shared by both panes and normalized to each image's fit, so differing
/// resolutions line up.
pub struct CompareState {
    pub b: FileFacts,
    pub flicker: bool,
    /// In flicker mode, whether B (rather than A) is currently shown.
    pub show_b: bool,
    zoom: f32,
    pan: Vec2,
}

impl CompareState {
    pub fn new(b: FileFacts) -> Self {
        Self {
            b,
            flicker: false,
            show_b: false,
            zoom: 1.0,
            pan: Vec2::ZERO,
        }
    }

    pub fn zoom_by(&mut self, factor: f32) {
        self.zoom = (self.zoom * factor).clamp(1.0, MAX_SCALE);
    }

    pub fn pan_by(&mut self, delta: Vec2) {
        self.pan += delta;
    }

    /// Screen rectangle for `img` fitted into `pane`, then scaled by the shared
    /// zoom and shifted by the shared pan (so both panes track together).
    pub fn pane_rect(&self, pane: Rect, img: Vec2) -> Rect {
        let fit = (pane.width() / img.x).min(pane.height() / img.y);
        let size = img * (fit * self.zoom);
        Rect::from_center_size(pane.center() + self.pan, size)
    }
}

/// Live state of an open lightbox. `group`/`index` address a member of the
/// current page's groups; `scale`/`pan` are the view transform. In `fit` mode
/// the scale is recomputed from the viewport each frame (so window resizes stay
/// fitted) and the pan is ignored.
pub struct LightboxState {
    pub group: usize,
    pub index: usize,
    pub active_tab: RepresentationKind,
    scale: f32,
    pan: Vec2,
    fit: bool,
    /// Active A/B compare, if the user pressed `C`.
    pub compare: Option<CompareState>,
    /// Audio lightbox only: the group index whose playback cursor is shown (the
    /// copy the user last started). Needed because exact-duplicate copies share
    /// a content hash, so the hash alone can't say which row is playing.
    pub audio_active: Option<usize>,
    /// Audio lightbox only: show spectrograms instead of amplitude waveforms.
    pub spectrogram: bool,
    /// Video lightbox only: the filmstrip still the user pinned (clicked) to show
    /// enlarged. `None` until they click one — then the middle frame is shown.
    /// Reset when navigating to another copy.
    pub video_frame: Option<usize>,
}

impl LightboxState {
    pub fn new(group: usize, index: usize) -> Self {
        Self {
            group,
            index,
            active_tab: RepresentationKind::Overview,
            scale: 1.0,
            pan: Vec2::ZERO,
            fit: true,
            compare: None,
            audio_active: None,
            spectrogram: false,
            video_frame: None,
        }
    }

    /// Reset to fit-to-window (used when switching to another image).
    pub fn reset_view(&mut self) {
        self.scale = 1.0;
        self.pan = Vec2::ZERO;
        self.fit = true;
    }

    /// Effective pixels-per-image-pixel for the current mode and viewport.
    fn effective_scale(&self, view: Rect, img: Vec2) -> f32 {
        if self.fit {
            (view.width() / img.x)
                .min(view.height() / img.y)
                .clamp(MIN_SCALE, MAX_SCALE)
        } else {
            self.scale
        }
    }

    /// Screen rectangle the image occupies inside `view`.
    pub fn image_rect(&self, view: Rect, img: Vec2) -> Rect {
        let scale = self.effective_scale(view, img);
        let size = img * scale;
        let pan = if self.fit { Vec2::ZERO } else { self.pan };
        Rect::from_center_size(view.center() + pan, size)
    }

    /// Enter fit mode.
    pub fn fit(&mut self) {
        self.fit = true;
    }

    /// Enter 1:1 (true pixels) mode, keeping the image centred.
    pub fn one_to_one(&mut self) {
        self.scale = 1.0;
        self.pan = Vec2::ZERO;
        self.fit = false;
    }

    /// Pan by a screen-space delta (from a drag). No-op in fit mode until the
    /// user has zoomed.
    pub fn pan_by(&mut self, delta: Vec2, view: Rect, img: Vec2) {
        self.leave_fit(view, img);
        self.pan += delta;
    }

    /// Zoom by `factor` keeping the image point under `cursor` fixed.
    pub fn zoom_at(&mut self, cursor: egui::Pos2, factor: f32, view: Rect, img: Vec2) {
        self.leave_fit(view, img);
        let new_scale = (self.scale * factor).clamp(MIN_SCALE, MAX_SCALE);
        let ratio = new_scale / self.scale;
        // Keep `cursor` anchored: center' = cursor - (cursor - center) * ratio.
        let center = view.center() + self.pan;
        let new_center = cursor + (center - cursor) * ratio;
        self.pan = new_center - view.center();
        self.scale = new_scale;
    }

    /// Materialize the current fit scale into an explicit scale so subsequent
    /// zoom/pan operate from what the user currently sees.
    fn leave_fit(&mut self, view: Rect, img: Vec2) {
        if self.fit {
            self.scale = self.effective_scale(view, img);
            self.pan = Vec2::ZERO;
            self.fit = false;
        }
    }
}

/// Resolve a previewable file's full-resolution image texture (upscaled thumbnail
/// while the full decode is in flight) and its pixel size, from the shared
/// caches. The viewer-agnostic generalisation of the Duplicate tab's
/// `lightbox_texture`: it takes [`FileFacts`] rather than a `DupeFile`, so any
/// side — a duplicate, a DIFF file, a cross-repo file — resolves the same way.
pub fn full_texture(
    facts: &FileFacts,
    full: &mut FullResCache,
    thumbs: &mut ThumbCache,
) -> (Option<TextureHandle>, Vec2) {
    let source = facts.abs_path.as_path();
    let tex = full
        .get(&facts.hash_hex, source)
        .or_else(|| thumbs.get(&facts.hash_hex, source));
    let img = facts
        .img_size
        .map(|(w, h)| egui::vec2(w as f32, h as f32))
        .or_else(|| tex.as_ref().map(|t| t.size_vec2()))
        .unwrap_or(egui::vec2(1.0, 1.0));
    (tex, img)
}

/// Split `viewport` into the two equal panes of a side-by-side compare, with a
/// fixed gutter between them. Pure geometry, so the layout is unit-testable and
/// shared by every caller of [`draw_compare`].
pub fn compare_split(viewport: Rect) -> (Rect, Rect) {
    const GAP: f32 = 6.0;
    let half = (viewport.width() - GAP) / 2.0;
    let left = Rect::from_min_size(viewport.min, egui::vec2(half, viewport.height()));
    let right = Rect::from_min_size(
        egui::pos2(viewport.min.x + half + GAP, viewport.min.y),
        egui::vec2(half, viewport.height()),
    );
    (left, right)
}

/// One frame's pointer input over the compare viewport, so [`draw_compare`]
/// stays a handful of arguments: the background drag delta (`None` when not
/// dragging), the smooth scroll amount, and the hover position.
pub struct ComparePointer {
    pub drag: Option<Vec2>,
    pub scroll: f32,
    pub cursor: Option<egui::Pos2>,
}

/// Render an A/B compare into `viewport`: in flicker mode one side fills the
/// whole viewport (A or B per `state.show_b`), otherwise the two panes sit side
/// by side. Applies the shared drag-pan and cursor-anchored scroll-zoom so both
/// panes track together, then labels each pane. `a`/`b` are each a
/// `(texture, pixel-size)`. The viewer mechanics only — the caller owns the
/// surrounding chrome and the action strip.
pub fn draw_compare(
    ui: &egui::Ui,
    state: &mut CompareState,
    viewport: Rect,
    a: (&Option<TextureHandle>, Vec2),
    b: (&Option<TextureHandle>, Vec2),
    input: ComparePointer,
) {
    let (a_tex, a_img) = a;
    let (b_tex, b_img) = b;
    if let Some(delta) = input.drag {
        state.pan_by(delta);
    }
    if input.scroll != 0.0 && input.cursor.is_some_and(|c| viewport.contains(c)) {
        state.zoom_by((input.scroll * 0.005).exp());
    }
    let tag = |ui: &egui::Ui, pane: Rect, text: &str| {
        ui.painter().text(
            pane.min + egui::vec2(6.0, 6.0),
            egui::Align2::LEFT_TOP,
            text,
            egui::FontId::proportional(18.0),
            theme::AMBER,
        );
    };
    if state.flicker {
        // Overlay: show A or B in the whole viewport.
        let (tex, img) = if state.show_b {
            (b_tex, b_img)
        } else {
            (a_tex, a_img)
        };
        draw_in_pane(ui, viewport, state.pane_rect(viewport, img), tex);
        tag(ui, viewport, if state.show_b { "B" } else { "A" });
    } else {
        let (left, right) = compare_split(viewport);
        draw_in_pane(ui, left, state.pane_rect(left, a_img), a_tex);
        draw_in_pane(ui, right, state.pane_rect(right, b_img), b_tex);
        tag(ui, left, "A");
        tag(ui, right, "B");
    }
}

/// Draw `tex` stretched to `rect`, clipped to `pane` — or a "decoding…" note
/// while the texture is still being produced.
pub fn draw_in_pane(ui: &egui::Ui, pane: Rect, rect: Rect, tex: &Option<TextureHandle>) {
    let uv = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
    match tex {
        Some(tex) => {
            ui.painter_at(pane)
                .image(tex.id(), rect, uv, egui::Color32::WHITE);
        }
        None => {
            ui.painter().text(
                pane.center(),
                egui::Align2::CENTER_CENTER,
                "decoding…",
                egui::FontId::proportional(16.0),
                theme::TAN,
            );
        }
    }
}

/// `size` fitted into `target`, centred.
pub fn fit_rect(target: Rect, size: Vec2) -> Rect {
    let s = (target.width() / size.x).min(target.height() / size.y);
    Rect::from_center_size(target.center(), size * s)
}

/// The shared full-window viewer for one image: wheel zoom around the cursor,
/// drag pan, FIT / 1:1, Esc or CLOSE to leave. Browse uses it as-is; the
/// Duplicates lightbox layers compare/mark/edit on the same [`LightboxState`].
/// Returns `true` when the viewer was closed this frame.
pub fn single_view(
    ctx: &Context,
    state: &mut LightboxState,
    tex: Option<TextureHandle>,
    img: Vec2,
    meta: &str,
    verbosity: TooltipVerbosity,
) -> bool {
    let mut close = false;
    let (mut do_fit, mut do_one) = (false, false);
    ctx.input(|i| {
        if i.key_pressed(egui::Key::Escape) {
            close = true;
        }
        if i.key_pressed(egui::Key::F) {
            do_fit = true;
        }
        if i.key_pressed(egui::Key::Num1) {
            do_one = true;
        }
    });
    egui::Area::new(egui::Id::new("single_lightbox"))
        .order(egui::Order::Foreground)
        .fixed_pos(egui::Pos2::ZERO)
        .show(ctx, |ui| {
            let screen = ctx.content_rect();
            let bg = ui.allocate_rect(screen, egui::Sense::click_and_drag());
            ui.painter()
                .rect_filled(screen, 0.0, egui::Color32::from_black_alpha(238));
            // Viewport = screen minus the top control bar and bottom meta strip.
            let viewport = Rect::from_min_max(
                egui::pos2(screen.min.x + 8.0, screen.min.y + 44.0),
                egui::pos2(screen.max.x - 8.0, screen.max.y - 50.0),
            );
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if bg.dragged() {
                state.pan_by(bg.drag_delta(), viewport, img);
            }
            if scroll != 0.0
                && let Some(c) = ctx.pointer_hover_pos()
                && viewport.contains(c)
            {
                state.zoom_at(c, (scroll * 0.005).exp(), viewport, img);
            }
            draw_in_pane(ui, viewport, state.image_rect(viewport, img), &tex);

            // Top control bar: CLOSE / FIT / 1:1.
            let top = Rect::from_min_max(
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
                        ui.add(egui::Button::new(egui::RichText::new(text).color(col)).fill(fill))
                            .explain(verbosity, short, verbose)
                            .clicked()
                    };
                    if pill(
                        ui,
                        &format!("{} CLOSE", icon::CHECK),
                        theme::AMBER,
                        theme::BLACK,
                        "Close the viewer",
                        "Close the image viewer (Esc does the same).",
                    ) {
                        close = true;
                    }
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
                },
            );

            // Bottom strip: file metadata plus the interaction hint.
            let p = ui.painter();
            p.text(
                egui::pos2(screen.min.x + 10.0, screen.max.y - 28.0),
                egui::Align2::LEFT_BOTTOM,
                meta,
                egui::FontId::proportional(13.0),
                theme::TAN,
            );
            p.text(
                egui::pos2(screen.min.x + 10.0, screen.max.y - 10.0),
                egui::Align2::LEFT_BOTTOM,
                "wheel: zoom · drag: pan · F fit · 1 100% · Esc close",
                egui::FontId::proportional(12.0),
                theme::HAIRLINE,
            );
        });
    if do_fit {
        state.fit();
    }
    if do_one {
        state.one_to_one();
    }
    close
}

struct Request {
    hex: String,
    source: PathBuf,
}

enum Decoded {
    Ready(String, ColorImage),
    Failed(String),
}

/// Tiny full-resolution texture cache backed by a background decode pool.
pub struct FullResCache {
    requests: Sender<Request>,
    decoded: Receiver<Decoded>,
    textures: HashMap<String, TextureHandle>,
    order: Vec<String>,
    pending: HashSet<String>,
    failed: HashSet<String>,
    /// The UI context, so a finished decode can wake the UI at rest (the
    /// lightbox does not spin repaints while idle).
    ctx: Arc<Mutex<Option<Context>>>,
}

impl FullResCache {
    pub fn new(workers: usize) -> Self {
        let (req_tx, req_rx) = crossbeam_channel::unbounded::<Request>();
        let (dec_tx, dec_rx) = crossbeam_channel::unbounded::<Decoded>();
        let ctx: Arc<Mutex<Option<Context>>> = Arc::new(Mutex::new(None));
        for _ in 0..workers.max(1) {
            let req_rx = req_rx.clone();
            let dec_tx = dec_tx.clone();
            let ctx = Arc::clone(&ctx);
            std::thread::spawn(move || {
                while let Ok(req) = req_rx.recv() {
                    // Only a successful decode has something new to show, so
                    // only that wakes the UI; waking on failure would spin
                    // repaints for missing files (and never settle).
                    match dedup_core::thumbnail::load_full_rgba(&req.source, MAX_TEXTURE_EDGE) {
                        Ok((w, h, rgba)) => {
                            let img =
                                ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
                            let _ = dec_tx.send(Decoded::Ready(req.hex, img));
                            if let Some(ctx) =
                                ctx.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
                            {
                                ctx.request_repaint();
                            }
                        }
                        Err(_) => {
                            let _ = dec_tx.send(Decoded::Failed(req.hex));
                        }
                    }
                }
            });
        }
        Self {
            requests: req_tx,
            decoded: dec_rx,
            textures: HashMap::new(),
            order: Vec::new(),
            pending: HashSet::new(),
            failed: HashSet::new(),
            ctx,
        }
    }

    /// Upload freshly decoded images into textures. Returns whether anything
    /// changed (so the caller can repaint).
    pub fn poll(&mut self, ctx: &Context) -> bool {
        *self.ctx.lock().unwrap_or_else(|e| e.into_inner()) = Some(ctx.clone());
        let mut changed = false;
        while let Ok(decoded) = self.decoded.try_recv() {
            changed = true;
            match decoded {
                Decoded::Ready(hex, img) => {
                    let handle = ctx.load_texture(&hex, img, TextureOptions::LINEAR);
                    self.pending.remove(&hex);
                    self.touch(&hex);
                    self.textures.insert(hex, handle);
                    self.evict();
                }
                Decoded::Failed(hex) => {
                    self.pending.remove(&hex);
                    self.failed.insert(hex);
                }
            }
        }
        changed
    }

    /// Texture for `hex`, requesting a full-resolution decode of `source` if it
    /// is not resident yet. `None` while pending or failed (caller shows the
    /// upscaled thumbnail meanwhile).
    pub fn get(&mut self, hex: &str, source: &Path) -> Option<TextureHandle> {
        if self.textures.contains_key(hex) {
            self.touch(hex);
            return self.textures.get(hex).cloned();
        }
        if self.failed.contains(hex) {
            return None;
        }
        if self.pending.insert(hex.to_string()) {
            let _ = self.requests.send(Request {
                hex: hex.to_string(),
                source: source.to_path_buf(),
            });
        }
        None
    }

    fn touch(&mut self, hex: &str) {
        if self.order.last().map(String::as_str) != Some(hex) {
            self.order.retain(|h| h != hex);
            self.order.push(hex.to_string());
        }
    }

    fn evict(&mut self) {
        while self.order.len() > FULL_CACHE_CAP {
            let old = self.order.remove(0);
            self.textures.remove(&old);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_other_member_indices() {
        let others = other_member_indices(4, 2);
        assert_eq!(others, vec![0, 1, 3]);
        assert_eq!(others.len(), 3);

        let label1 = format_other_switcher_label(0, others.len());
        assert_eq!(label1, "<1 / 3>");

        let label2 = format_other_switcher_label(2, others.len());
        assert_eq!(label2, "<3 / 3>");
    }

    #[test]
    fn test_file_representations_available_kinds() {
        let facts = FileFacts {
            size: 1024,
            modified_ms: 1000,
            mime: Some("image/png".to_string()),
            img_size: Some((800, 600)),
            audio_ms: None,
            audio_seed: None,
            hash_hex: "abcd".to_string(),
            abs_path: PathBuf::from("/tmp/test.png"),
            origin: None,
        };

        let reps = FileRepresentations::from_facts(
            &facts,
            "MainRepo".to_string(),
            false,
            MarkState::Unmarked,
        );
        let kinds = reps.available_kinds();
        assert!(kinds.contains(&RepresentationKind::Overview));
        assert!(kinds.contains(&RepresentationKind::Image));
        assert!(kinds.contains(&RepresentationKind::Metadata));
        assert!(!kinds.contains(&RepresentationKind::Audio));
    }
}
