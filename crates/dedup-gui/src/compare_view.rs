//! The two-file comparison surface shared by the Transfer DIFF board and any
//! other view that needs to put two files side by side.
//!
//! It renders through the shared [`crate::lightbox`] helpers — `tab_kinds`,
//! `draw_tab_bar`, `draw_columns`, `draw_metadata_column`, `draw_text_column` —
//! so each representation has exactly one implementation. This module owns only
//! what is specific to comparing an arbitrary *pair*: decoding each side to a
//! texture, and the A/B zoom / pan / flicker transform.
//!
//! It lived inside `transfer_view` as a private type until 2026-07-31, which is
//! what made DIFF a downgraded compare surface — audio, metadata and text were
//! unreachable there. Actions stay caller-supplied: the viewer is shared, the
//! decisions are not.

use crate::imgedit::Orient;
use crate::lightbox::{
    ColumnHead, ComparePointer, CompareState, FileRepresentations, RepresentationKind,
    compare_split, draw_columns, draw_compare, draw_in_pane, draw_tab_bar, draw_text_column,
    load_text_preview,
};
use crate::media_cell::FileFacts;
use crate::settings::TooltipVerbosity;
use crate::theme;
use crate::util::{ExplainExt, format_mtime, format_size};
use crossbeam_channel::{Receiver, Sender};
use egui::{
    Align, Align2, ColorImage, Context, FontId, Id, Layout, Rect, RichText, TextureHandle,
    TextureOptions, UiBuilder, Vec2,
};

/// Largest texture edge uploaded for a full-resolution preview.
const MAX_TEXTURE_EDGE: u32 = 8192;

/// One side of a DIFF comparison: its action identity (`repo` + `rel_path`, which
/// the resulting [`crate::diff_board::BoardAction`] needs) alongside the
/// viewer-agnostic [`FileFacts`] used to preview and describe it.
#[derive(Clone)]
pub(crate) struct DiffSide {
    pub repo: String,
    pub rel_path: String,
    pub facts: FileFacts,
    /// Whether this side's repository is read-only. A writable audio side may
    /// edit its ID3 tags in the viewer; everything else is presentation only.
    pub read_only: bool,
}

impl DiffSide {
    /// Whether this side can produce a visual to compare (image or video still).
    /// Text / binary / audio cannot, so compare disables itself for the pair.
    /// Whether this side can produce a visual to compare against.
    ///
    /// Audio counts: it is compared as a spectrogram, which is a texture like
    /// any other. Before this, DIFF said "no preview for audio/mpeg" and two
    /// MP3s could not be compared at all.
    pub fn previewable(&self) -> bool {
        self.facts.is_image() || self.facts.is_video() || self.facts.is_audio()
    }

    /// What to say when there is no picture to show.
    pub fn placeholder(&self) -> String {
        match self.facts.mime.as_deref() {
            Some(mime) => format!("no preview for {mime}"),
            None => "no preview for this file type".to_string(),
        }
    }
}

/// The user's decision in the DIFF comparison, mapped by the caller onto the same
/// `BoardAction`s the diff board row offers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DiffPick {
    /// Delete this side's file.
    Delete { on_left: bool },
    /// Replace the other side's file with this side's content.
    Overwrite { from_left: bool },
    /// Toggle this side's deletion mark (callers that supplied marks via
    /// [`DiffCompare::set_marks`]). Unlike the commands above, acting on it
    /// does not close the viewer — marking is part of looking.
    ToggleMark { on_left: bool },
    /// Leave both alone.
    Close,
}

/// One side's deletion-mark state, supplied per frame by a caller whose actions
/// are marks (the Duplicates tab). `markable: false` renders the pill disabled
/// and struck through — the protected (read-only repo) presentation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct MarkPill {
    pub marked: bool,
    pub markable: bool,
}

/// A decoded preview arriving from a worker thread.
struct DiffLoaded {
    left: bool,
    image: Option<ColorImage>,
}

/// The open ID3 tag editor: which file is being edited (by content hash), the
/// working copy of its tags bound to the editor's fields, and the distinct
/// values every pool candidate offers per field (Title/Artist/Album/Year/Track/
/// Genre) so the best one can be adopted.
pub(crate) struct TagEdit {
    hex: String,
    path: std::path::PathBuf,
    pub tags: crate::id3tags::Tags,
    pub(crate) options: [Vec<String>; 6],
}

/// What one side's preview pane should draw this frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SlotState {
    /// The decoded image (or video still) is ready.
    Image,
    /// The decode is still in flight.
    Decoding,
    /// Settled with nothing to show — a non-previewable type, or a decode that
    /// came back empty (e.g. video with no ffmpeg).
    NoPreview,
}

/// The open side-by-side comparison of one BY PATH conflict — two versions of the
/// same path in two repos — rendered through the shared lightbox viewer
/// ([`draw_compare`]): the same zoom / pan / flicker the Duplicate lightbox has,
/// with DIFF's own per-side actions. Previews decode off the UI thread (a large
/// photo must never freeze the window) and handle both images and video stills;
/// a side that can produce no visual keeps a "no preview" note and disables
/// compare (roadmap: "if a side has no visual, compare disables itself").
pub(crate) struct DiffCompare {
    pub left: DiffSide,
    pub right: DiffSide,
    /// Shared A/B view transform (zoom / pan / flicker). Its `b` carries the right
    /// side's facts, though [`draw_compare`] reads only the transform.
    compare: CompareState,
    tex: [Option<TextureHandle>; 2],
    /// Whether that side's decode has come back (successfully or not).
    settled: [bool; 2],
    tx: Sender<DiffLoaded>,
    rx: Receiver<DiffLoaded>,
    started: bool,
    /// Which representation is on screen. DIFF dispatches on this exactly as the
    /// Duplicates lightbox does, through the same shared `lightbox` helpers, so
    /// a document or a tagged track is comparable here too.
    pub(crate) tab: crate::lightbox::RepresentationKind,
    /// Text previews, read once per side and kept for as long as the comparison
    /// is open (a 64 KB head read per frame would be absurd).
    text: [Option<crate::lightbox::TextPreview>; 2],
    /// The candidates each side can be switched between — a duplicate group's
    /// members, a Browse listing, or (for a review row) just the pair itself.
    /// The viewer knows nothing about where they came from.
    pool: Vec<DiffSide>,
    /// Orientation operations applied per side, in order. Held here rather than
    /// baked into the texture so they survive entering and leaving comparison —
    /// a rotation made while inspecting one file is still there when the pair is
    /// aligned against each other.
    ops: [Vec<crate::imgedit::Orient>; 2],
    /// Each side's decoded pixel size, before orientation.
    base_size: [Option<(u32, u32)>; 2],
    /// The decoded pixels per side, kept so a turn can be re-applied without
    /// decoding the file again.
    base_image: [Option<ColorImage>; 2],
    /// Whether the second side is hidden, giving the first the whole screen for
    /// close analysis. A viewer opened with one file starts this way.
    second_hidden: bool,
    /// Stored ID3 tags per side, read once. `Some(None)` means "read, carries
    /// none" — distinct from "not read yet".
    tags: [Option<Option<crate::id3tags::Tags>>; 2],
    /// Per-side deletion-mark state, when the caller's actions are marks rather
    /// than the DIFF board's commands. Set each frame via [`Self::set_marks`] —
    /// the caller owns the marks, the viewer only shows and reports them.
    marks: [Option<MarkPill>; 2],
    /// Which side's copy is audible (0 = A, 1 = B) once playback was started
    /// from this viewer — the side the playback cursor belongs to. Exact
    /// duplicates share a content hash, so the hash alone cannot say.
    pub audio_active: Option<usize>,
    /// The open ID3 tag editor (Metadata tab), if any. Only a writable audio
    /// side ever opens one.
    pub(crate) tag_edit: Option<TagEdit>,
    /// Why the last tag save failed, shown on the Metadata tab until the next
    /// attempt succeeds.
    tag_error: Option<String>,
    /// The caller's own headline for the viewer — what these two files are to
    /// the surface that opened it.
    title: String,
}

impl DiffCompare {
    pub fn new(left: DiffSide, right: DiffSide) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        let compare = CompareState::new(right.facts.clone());
        Self {
            left,
            right,
            compare,
            tex: [None, None],
            settled: [false, false],
            tx,
            rx,
            started: false,
            pool: Vec::new(),
            second_hidden: false,
            ops: [Vec::new(), Vec::new()],
            base_size: [None, None],
            base_image: [None, None],
            tab: crate::lightbox::RepresentationKind::Overview,
            text: [None, None],
            tags: [None, None],
            marks: [None, None],
            audio_active: None,
            tag_edit: None,
            tag_error: None,
            title: "COMPARE — SAME PATH, DIFFERENT CONTENT".into(),
        }
    }

    /// Supply the sides' deletion-mark state for this frame. A side given
    /// `Some` shows a mark pill as its action; the DIFF commands are replaced —
    /// actions belong to the caller, and a caller that marks does not overwrite.
    pub fn set_marks(&mut self, left: Option<MarkPill>, right: Option<MarkPill>) {
        self.marks = [left, right];
    }

    /// Replace the headline — what these two files are to the caller.
    pub fn set_title(&mut self, title: impl Into<String>) {
        self.title = title.into();
    }

    /// [`Self::new`] with the candidates each side may be switched between.
    pub fn new_with_pool(left: DiffSide, right: Option<DiffSide>, pool: Vec<DiffSide>) -> Self {
        // A viewer with no second side still shows the first, full width.
        let right = right.unwrap_or_else(|| left.clone());
        let mut me = Self::new(left, right);
        me.pool = pool;
        me
    }

    /// Record a side's decoded pixel size.
    #[cfg(test)]
    pub fn set_base_size(&mut self, slot: usize, size: (u32, u32)) {
        if let Some(s) = self.base_size.get_mut(slot) {
            *s = Some(size);
        }
    }

    /// Turn or mirror one side. Applies to that side alone, so one copy can be
    /// brought into alignment with the other.
    pub fn rotate(&mut self, is_left: bool, op: crate::imgedit::Orient) {
        let slot = usize::from(!is_left);
        if let Some(ops) = self.ops.get_mut(slot) {
            ops.push(op);
        }
    }

    /// How many orientation operations a side carries.
    #[cfg(test)]
    pub fn ops_len(&self, slot: usize) -> usize {
        self.ops.get(slot).map_or(0, Vec::len)
    }

    /// A side's dimensions after its orientation operations — a quarter turn
    /// swaps them, which is what the comparison lays out against.
    pub fn oriented_size(&self, slot: usize) -> Option<(u32, u32)> {
        let (w, h) = (*self.base_size.get(slot)?)?;
        let quarter_turns = self
            .ops
            .get(slot)?
            .iter()
            .filter(|o| matches!(o, crate::imgedit::Orient::RotateCw))
            .count();
        Some(if quarter_turns % 2 == 1 {
            (h, w)
        } else {
            (w, h)
        })
    }

    /// Hide the second side so the first fills the screen.
    pub fn hide_second(&mut self) {
        self.second_hidden = true;
    }

    /// Whether both sides are on screen.
    fn two_sided(&self) -> bool {
        !self.second_hidden
    }

    /// Whether the left side has anywhere to go — false for a pair, where the
    /// only other candidate is already on the right. The switcher is rendered
    /// only when this is true, so a pair shows none at all.
    pub fn can_step_left(&self) -> bool {
        self.has_free_candidate(&self.left, &self.right)
    }

    /// Whether the right side has anywhere to go.
    pub fn can_step_right(&self) -> bool {
        self.has_free_candidate(&self.right, &self.left)
    }

    /// How many candidates the sides can be switched between.
    #[cfg(test)]
    pub fn pool_len(&self) -> usize {
        self.pool.len()
    }

    /// Whether the pool holds a candidate that is neither `from` nor `other`.
    /// Identity is the absolute path, not the relative one: duplicates across
    /// two repositories routinely share their relative path while being
    /// distinct copies.
    fn has_free_candidate(&self, from: &DiffSide, other: &DiffSide) -> bool {
        self.pool.iter().any(|c| {
            c.facts.abs_path != from.facts.abs_path && c.facts.abs_path != other.facts.abs_path
        })
    }

    /// Where a side stands among *its* candidates — the pool minus the file the
    /// other side shows. `<1 / 3>` in a four-file pool: the count of candidates,
    /// never the pool size (the four-copy cycler defect). With the second side
    /// hidden nothing is masked, so the label counts the whole pool. `None`
    /// when the side's file is not in the pool.
    fn switcher_label(&self, is_left: bool) -> Option<String> {
        let (from, other) = if is_left {
            (&self.left, &self.right)
        } else {
            (&self.right, &self.left)
        };
        let masked = self.two_sided().then_some(other.facts.abs_path.as_path());
        let candidates: Vec<&DiffSide> = self
            .pool
            .iter()
            .filter(|c| Some(c.facts.abs_path.as_path()) != masked)
            .collect();
        let pos = candidates
            .iter()
            .position(|c| c.facts.abs_path == from.facts.abs_path)?;
        Some(crate::lightbox::format_other_switcher_label(
            pos,
            candidates.len(),
        ))
    }

    /// Move the left side `dir` places through the pool.
    pub fn step_left(&mut self, dir: isize) {
        if let Some(next) = self.stepped(&self.left, &self.right, dir) {
            self.left = next;
        }
    }

    /// Move the right side `dir` places through the pool.
    pub fn step_right(&mut self, dir: isize) {
        if let Some(next) = self.stepped(&self.right, &self.left, dir) {
            self.right = next;
        }
    }

    /// The candidate `dir` places along from `from`, skipping the one `other` is
    /// showing.
    ///
    /// Skipping is what makes comparing a file with itself impossible. It leaves
    /// a hole in the sequence — in a group of three, stepping from the first
    /// goes to the third when the second is taken — which is the accepted cost
    /// of never colliding.
    fn stepped(&self, from: &DiffSide, other: &DiffSide, dir: isize) -> Option<DiffSide> {
        let at = self
            .pool
            .iter()
            .position(|c| c.facts.abs_path == from.facts.abs_path)?;
        let n = self.pool.len() as isize;
        let step = if dir >= 0 { 1 } else { -1 };
        // Walk at most once round: every other candidate is tried before giving
        // up, so a pool where only `other` is free simply does not move.
        let mut probe = at as isize;
        for _ in 0..n {
            probe = (probe + step).rem_euclid(n);
            let candidate = self.pool.get(probe as usize)?;
            if candidate.facts.abs_path != other.facts.abs_path {
                return Some(candidate.clone());
            }
        }
        None
    }

    /// The representations each side offers, for the tab bar and the dispatch.
    /// Marks belong to the caller; the only write a side can carry is its ID3
    /// tags, and only when its repository is writable.
    pub fn reps(&self) -> (FileRepresentations, FileRepresentations) {
        let make = |side: &DiffSide| {
            FileRepresentations::from_facts(
                &side.facts,
                side.repo.clone(),
                side.read_only,
                crate::lightbox::MarkState::Protected,
            )
        };
        (make(&self.left), make(&self.right))
    }

    /// Open the ID3 editor on one side: its stored tags as the working copy,
    /// plus the distinct value each field takes across the pool's candidates
    /// (the pair itself when there is no pool), so the best one can be adopted.
    fn open_tag_editor(&mut self, is_left: bool) {
        let side = if is_left { &self.left } else { &self.right };
        let tags = crate::id3tags::read(&side.facts.abs_path).unwrap_or_default();
        let mut options: [Vec<String>; 6] = std::array::from_fn(|_| Vec::new());
        let candidates: Vec<&DiffSide> = if self.pool.is_empty() {
            vec![&self.left, &self.right]
        } else {
            self.pool.iter().collect()
        };
        for c in candidates {
            if let Some(t) = crate::id3tags::read(&c.facts.abs_path) {
                let vals = [&t.title, &t.artist, &t.album, &t.year, &t.track, &t.genre];
                for (i, v) in vals.into_iter().enumerate() {
                    if !v.is_empty() && !options[i].iter().any(|o| o == v) {
                        options[i].push(v.clone());
                    }
                }
            }
        }
        self.tag_edit = Some(TagEdit {
            hex: side.facts.hash_hex.clone(),
            path: side.facts.abs_path.clone(),
            tags,
            options,
        });
    }

    /// Write the open editor's tags to disk (tags only — the audio is
    /// untouched) and close it. A failure keeps the editor open and says why.
    fn save_tag_edit(&mut self) {
        let Some(te) = self.tag_edit.take() else {
            return;
        };
        match crate::id3tags::write(&te.path, &te.tags) {
            Ok(()) => {
                self.tag_error = None;
                // The stored-tags read is stale for this file now.
                for (slot, side) in [(0, &self.left), (1, &self.right)] {
                    if side.facts.hash_hex == te.hex {
                        self.tags[slot] = None;
                    }
                }
            }
            Err(e) => {
                self.tag_error = Some(format!("Tag save failed: {e}"));
                self.tag_edit = Some(te);
            }
        }
    }

    /// What one side's pane should draw right now: its decoded image, an
    /// in-flight "decoding…" note, or a settled "no preview" note. A side is
    /// `NoPreview` both when it can never have a visual (a document) and when its
    /// decode came back empty (e.g. a video with no ffmpeg) — settled with no
    /// texture. This is what keeps a failed decode from spinning "decoding…"
    /// forever (the distinction the old two-pane `settled[]` flags carried).
    fn slot_state(&self, slot: usize) -> SlotState {
        if self.tex[slot].is_some() {
            SlotState::Image
        } else if self.settled[slot] {
            SlotState::NoPreview
        } else {
            SlotState::Decoding
        }
    }

    /// A/B compare (zoom / pan / flicker) is available only once *both* sides
    /// have actually produced a texture — before then, or if either failed,
    /// there is nothing to compare, so the panes stay static.
    fn compare_ready(&self) -> bool {
        matches!(
            (self.slot_state(0), self.slot_state(1)),
            (SlotState::Image, SlotState::Image)
        )
    }

    /// Kick off both decodes once, off the UI thread. A non-previewable side is
    /// settled immediately with no decode.
    fn start(&mut self, ctx: &Context) {
        if self.started {
            return;
        }
        self.started = true;
        self.spawn_decode(ctx, 0);
        self.spawn_decode(ctx, 1);
    }

    /// Decode one side's preview off the UI thread (or settle it immediately
    /// when it can produce no visual).
    fn spawn_decode(&mut self, ctx: &Context, slot: usize) {
        let is_left = slot == 0;
        let side = if is_left { &self.left } else { &self.right };
        if !side.previewable() {
            self.settled[slot] = true;
            return;
        }
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        let path = side.facts.abs_path.clone();
        let hex = side.facts.hash_hex.clone();
        let video = side.facts.is_video();
        let audio = side.facts.is_audio();
        std::thread::spawn(move || {
            // Audio has no frame to show, so it is compared as a spectrogram
            // — the same rendering the Duplicates player uses, from the same
            // shared `waveform` module rather than a second implementation.
            let image = if audio {
                crate::waveform::spec_rgba(&path)
            } else {
                let decoded = if video {
                    // One still is enough to tell two clips apart at a glance.
                    dedup_core::thumbnail::video_frame_rgba(&path, &hex, 0, 1).ok()
                } else {
                    dedup_core::thumbnail::load_full_rgba(&path, MAX_TEXTURE_EDGE).ok()
                };
                decoded.map(|(w, h, rgba)| {
                    ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba)
                })
            };
            // Only a successful decode has something new to show, so only
            // that wakes the UI; waking on failure would spin repaints for
            // files with nothing to draw.
            let wake = image.is_some();
            let _ = tx.send(DiffLoaded {
                left: is_left,
                image,
            });
            if wake {
                ctx.request_repaint();
            }
        });
    }

    /// Forget everything cached about one side — its texture, decoded pixels,
    /// orientation, text preview and stored tags — and decode the (new) file
    /// behind it. Called whenever a side is pointed at another file: without
    /// this a stepped side keeps showing its predecessor.
    fn refresh_side(&mut self, ctx: &Context, slot: usize) {
        self.tex[slot] = None;
        self.settled[slot] = false;
        self.base_size[slot] = None;
        self.base_image[slot] = None;
        if let Some(ops) = self.ops.get_mut(slot) {
            ops.clear();
        }
        self.text[slot] = None;
        self.tags[slot] = None;
        self.spawn_decode(ctx, slot);
    }

    /// Upload any freshly decoded previews.
    fn poll(&mut self, ctx: &Context) {
        while let Ok(loaded) = self.rx.try_recv() {
            let slot = usize::from(!loaded.left);
            self.settled[slot] = true;
            if let Some(image) = loaded.image {
                let [w, h] = image.size;
                self.base_size[slot] = Some((w as u32, h as u32));
                self.base_image[slot] = Some(image);
                self.upload(ctx, slot);
            }
        }
    }

    /// Re-apply a side's orientation to its decoded pixels and upload the result.
    /// Called on decode and again whenever the side is turned, so what is on
    /// screen always matches the recorded operations.
    fn upload(&mut self, ctx: &Context, slot: usize) {
        let Some(base) = self.base_image.get(slot).and_then(Option::as_ref) else {
            return;
        };
        let ops = self.ops.get(slot).cloned().unwrap_or_default();
        let name = if slot == 0 {
            "diff-compare-left"
        } else {
            "diff-compare-right"
        };
        let image = if ops.is_empty() {
            base.clone()
        } else {
            // Round-trip through `image` so the shared orientation code is the
            // one that runs — the same operations the single-file editor applies.
            let [w, h] = base.size;
            let rgba: Vec<u8> = base.as_raw().to_vec();
            match image::RgbaImage::from_raw(w as u32, h as u32, rgba) {
                Some(buf) => {
                    let turned =
                        crate::imgedit::apply_ops(image::DynamicImage::ImageRgba8(buf), &ops)
                            .to_rgba8();
                    ColorImage::from_rgba_unmultiplied(
                        [turned.width() as usize, turned.height() as usize],
                        turned.as_raw(),
                    )
                }
                None => base.clone(),
            }
        };
        self.tex[slot] = Some(ctx.load_texture(name, image, TextureOptions::LINEAR));
    }

    /// `(texture, pixel-size)` for one side, in the shape [`draw_compare`] wants.
    /// The size comes from the indexed image dimensions, else the decoded texture
    /// (video stills carry no stored dimensions), else a 1×1 fallback while pending.
    fn sized(&self, slot: usize) -> (Option<TextureHandle>, Vec2) {
        let tex = self.tex[slot].clone();
        let facts = if slot == 0 {
            &self.left.facts
        } else {
            &self.right.facts
        };
        // Oriented dimensions, so a turned side lays out at its new aspect
        // rather than its stored one.
        let img = self
            .oriented_size(slot)
            .map(|(w, h)| egui::vec2(w as f32, h as f32))
            .or_else(|| facts.img_size.map(|(w, h)| egui::vec2(w as f32, h as f32)))
            .or_else(|| tex.as_ref().map(|t| t.size_vec2()))
            .unwrap_or(egui::vec2(1.0, 1.0));
        (tex, img)
    }

    /// Draw the comparison over the whole window. Returns the user's decision, or
    /// `None` while they are still looking.
    pub fn view(
        &mut self,
        ctx: &Context,
        verbosity: TooltipVerbosity,
        player: Option<&crate::player::Player>,
    ) -> Option<DiffPick> {
        self.start(ctx);
        self.poll(ctx);

        let ready = self.compare_ready();
        // Esc steps back (flicker → side-by-side → closed); Space drives flicker
        // (enter it, then swap A/B), both only once both sides have decoded.
        // P and the arrows drive the audio transport (guarded so typing in the
        // tag editor never plays or steps anything).
        let (esc, space, key_p, key_t, arrow_l, arrow_r) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::Escape),
                i.key_pressed(egui::Key::Space),
                i.key_pressed(egui::Key::P),
                i.key_pressed(egui::Key::T),
                i.key_pressed(egui::Key::ArrowLeft),
                i.key_pressed(egui::Key::ArrowRight),
            )
        });
        let typing = ctx.egui_wants_keyboard_input();
        let (key_p, key_t, arrow_l, arrow_r) = (
            key_p && !typing,
            key_t && !typing,
            arrow_l && !typing,
            arrow_r && !typing,
        );
        // Esc backs out one level: an open tag editor first (abandoning its
        // working copy), then flicker, then the viewer itself.
        if esc {
            if self.tag_edit.is_some() {
                self.tag_edit = None;
            } else if ready && self.compare.flicker {
                self.compare.flicker = false;
            } else {
                return Some(DiffPick::Close);
            }
        }
        // T: the shown copy's tags, ready to edit — writable audio only.
        if key_t
            && self.left.facts.is_audio()
            && !self.left.read_only
            && crate::id3tags::container_supported(self.left.facts.mime.as_deref())
        {
            self.open_tag_editor(true);
            self.tab = RepresentationKind::Metadata;
        }
        // Space swaps within flicker; whether audio follows is settled with the
        // other transport actions after drawing.
        let mut swapped = false;
        if space && ready {
            if self.compare.flicker {
                self.compare.show_b = !self.compare.show_b;
                swapped = true;
            } else {
                self.compare.flicker = true;
            }
        }

        let mut step: Option<(bool, isize)> = None;
        let mut hide = false;
        let mut turn: Option<(bool, Orient)> = None;
        let mut play: Option<bool> = None;
        let mut pause = false;
        let mut cycle_speed = false;
        let mut show = false;
        let (mut meta_l, mut meta_r) = (
            crate::lightbox::MetaAction::None,
            crate::lightbox::MetaAction::None,
        );
        let picked = egui::Area::new(Id::new("diff-compare"))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::Pos2::ZERO)
            .show(ctx, |ui| {
                let screen = ctx.content_rect();
                let bg = ui.allocate_rect(screen, egui::Sense::click_and_drag());
                // Fully opaque: this is a judgement call about two files, and a
                // translucent backdrop let the tab underneath read through the
                // photographs.
                ui.painter().rect_filled(screen, 0.0, theme::black());
                let inner = screen.shrink(12.0);
                let mut pick = None;
                let two_sided = self.two_sided();
                let two_sided_now = two_sided;
                let tab_is_image = self.tab == RepresentationKind::Image;
                let tab_is_audio = self.tab == RepresentationKind::Audio;
                let speed_label = player
                    .map(|p| {
                        let r = p.snapshot().speed;
                        let s = format!("{r:.1}");
                        s.trim_end_matches('0').trim_end_matches('.').to_string()
                    })
                    .unwrap_or_else(|| "1".to_string());

                // Title + CLOSE.
                let top =
                    Rect::from_min_max(inner.min, egui::pos2(inner.max.x, inner.min.y + 26.0));
                let close = ui
                    .scope_builder(
                        UiBuilder::new()
                            .max_rect(top)
                            .layout(Layout::left_to_right(Align::Center)),
                        |ui| {
                            ui.label(
                                RichText::new(&self.title)
                                    .color(theme::tan())
                                    .size(16.0)
                                    .strong(),
                            );
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.button(RichText::new("CLOSE").color(theme::black()))
                                    .explain(
                                        verbosity,
                                        "Close the comparison",
                                        "Close this view and go back to the diff board. Nothing \
                                         is changed.",
                                    )
                                    .clicked()
                            })
                            .inner
                        },
                    )
                    .inner;
                if close {
                    pick = Some(DiffPick::Close);
                }

                // Preview viewport on top, facts / action strip along the bottom.
                // Guard the viewport bottom so a very short window never inverts
                // the rect (a full-window modal, but cheap to keep well-formed).
                // The identifying facts sit with their side, above the images —
                // so the images get the space the old bottom legend occupied.
                const TITLE_H: f32 = 96.0;
                let tab_h = 30.0;
                let titles_top = inner.min.y + 32.0 + tab_h;
                let viewport = Rect::from_min_max(
                    egui::pos2(inner.min.x, titles_top + TITLE_H),
                    egui::pos2(
                        inner.max.x,
                        (inner.max.y - 18.0).max(titles_top + TITLE_H + 40.0),
                    ),
                );

                // The representation tabs, from the same helper the Duplicates
                // lightbox uses — so a pair offering Text or Metadata is
                // comparable here too, rather than only images and video.
                let (lreps, rreps) = self.reps();
                let offered = crate::lightbox::tab_kinds(&lreps, Some(&rreps));
                let tab_rect = Rect::from_min_max(
                    egui::pos2(inner.min.x, inner.min.y + 30.0),
                    egui::pos2(inner.max.x, inner.min.y + 30.0 + tab_h),
                );
                // Open on the pair's own representation, not Overview: DIFF has
                // no Overview screen — the facts for both sides are always on the
                // strip below — so selecting it would label the view wrongly. A
                // tab the pair no longer offers cannot stay selected either.
                // "The pair's own representation" is its media kind when it has
                // one — an mp3's is Audio, not the Metadata tab that happens to
                // sort first.
                let native = || {
                    offered
                        .iter()
                        .copied()
                        .find(|k| {
                            matches!(
                                k,
                                RepresentationKind::Image
                                    | RepresentationKind::Audio
                                    | RepresentationKind::Video
                            )
                        })
                        .or_else(|| {
                            offered
                                .iter()
                                .copied()
                                .find(|k| *k != RepresentationKind::Overview)
                        })
                        .or_else(|| offered.first().copied())
                        .unwrap_or(RepresentationKind::Image)
                };
                if self.tab == RepresentationKind::Overview || !offered.contains(&self.tab) {
                    self.tab = native();
                }
                let mut tab = self.tab;
                ui.scope_builder(
                    UiBuilder::new()
                        .max_rect(tab_rect)
                        .layout(Layout::left_to_right(Align::Center)),
                    |ui| draw_tab_bar(ui, &mut tab, &lreps, &rreps),
                );
                self.tab = tab;
                // Hiding the second side is how a single file gets the whole
                // screen for close analysis.
                if two_sided_now {
                    ui.scope_builder(
                        UiBuilder::new()
                            .max_rect(tab_rect)
                            .layout(Layout::right_to_left(Align::Center)),
                        |ui| {
                            if ui.button("HIDE B").clicked() {
                                hide = true;
                            }
                        },
                    );
                } else {
                    ui.scope_builder(
                        UiBuilder::new()
                            .max_rect(tab_rect)
                            .layout(Layout::right_to_left(Align::Center)),
                        |ui| {
                            if ui.button("SHOW B").clicked() {
                                show = true;
                            }
                        },
                    );
                }

                // Metadata: each side's tag surface — the ID3 editor for the
                // copy being edited (only a writable audio side offers one),
                // the stored tags for the others, an image's EXIF facts. Marks
                // stay the caller's; tags are the one write a side carries.
                if self.tab == RepresentationKind::Metadata {
                    for slot in [0usize, 1usize] {
                        if self.tags[slot].is_none() {
                            let side = if slot == 0 { &self.left } else { &self.right };
                            self.tags[slot] = Some(
                                crate::id3tags::container_supported(side.facts.mime.as_deref())
                                    .then(|| crate::id3tags::read(&side.facts.abs_path))
                                    .flatten(),
                            );
                        }
                    }
                    let (lt, rt) = (
                        self.tags[0].clone().flatten(),
                        self.tags[1].clone().flatten(),
                    );
                    // The editor travels out of `self` for the frame so the
                    // column closures can bind text fields to it.
                    let mut tag_edit = self.tag_edit.take();
                    let (l, r) = (&self.left, &self.right);
                    let mut child = ui.new_child(
                        UiBuilder::new()
                            .max_rect(viewport)
                            .layout(Layout::top_down(Align::Min)),
                    );
                    if let Some(err) = &self.tag_error {
                        child.colored_label(theme::red(), err);
                    }
                    fn meta_body<'a>(
                        side: &'a DiffSide,
                        stored: Option<&'a crate::id3tags::Tags>,
                        te: &'a mut Option<TagEdit>,
                    ) -> crate::lightbox::MetaBody<'a> {
                        match te {
                            Some(TagEdit {
                                hex, tags, options, ..
                            }) if *hex == side.facts.hash_hex => {
                                crate::lightbox::MetaBody::Editing { tags, options }
                            }
                            _ if side.facts.is_audio() => crate::lightbox::MetaBody::Stored {
                                tags: stored,
                                can_edit: !side.read_only,
                            },
                            _ => crate::lightbox::MetaBody::Exif {
                                camera: side.facts.exif.as_ref().and_then(|e| e.camera.as_deref()),
                                taken: side
                                    .facts
                                    .exif
                                    .as_ref()
                                    .and_then(|e| e.taken_ms)
                                    .map(crate::util::format_mtime),
                            },
                        }
                    }
                    // Only the side(s) that actually carry metadata get a
                    // column — a representation only one side supports draws
                    // as a single full-width column (§1.3.1).
                    let mut cols: Vec<crate::lightbox::ColumnFn<'_, Option<TagEdit>>> = Vec::new();
                    if lreps.metadata.is_some() {
                        cols.push(Box::new(|ui: &mut egui::Ui, te: &mut Option<TagEdit>| {
                            meta_l = crate::lightbox::draw_metadata_column(
                                ui,
                                &ColumnHead {
                                    file_name: &l.rel_path,
                                    repo: &l.repo,
                                    accent: theme::blue(),
                                    read_only: l.read_only,
                                    is_main: false,
                                    source: &l.facts.abs_path,
                                },
                                meta_body(l, lt.as_ref(), te),
                            );
                        }));
                    }
                    if two_sided_now && rreps.metadata.is_some() {
                        cols.push(Box::new(|ui: &mut egui::Ui, te: &mut Option<TagEdit>| {
                            meta_r = crate::lightbox::draw_metadata_column(
                                ui,
                                &ColumnHead {
                                    file_name: &r.rel_path,
                                    repo: &r.repo,
                                    accent: theme::tan(),
                                    is_main: false,
                                    read_only: r.read_only,
                                    source: &r.facts.abs_path,
                                },
                                meta_body(r, rt.as_ref(), te),
                            );
                        }));
                    }
                    draw_columns(&mut child, &mut tag_edit, cols);
                    self.tag_edit = tag_edit;
                } else if self.tab == RepresentationKind::Text {
                    for slot in [0usize, 1usize] {
                        if self.text[slot].is_none() {
                            let side = if slot == 0 { &self.left } else { &self.right };
                            self.text[slot] = Some(load_text_preview(&side.facts.abs_path));
                        }
                    }
                    let (lt, rt) = (
                        self.text[0].as_ref().cloned(),
                        self.text[1].as_ref().cloned(),
                    );
                    let (l, r) = (&self.left, &self.right);
                    let height = viewport.height().max(80.0);
                    let mut child = ui.new_child(
                        UiBuilder::new()
                            .max_rect(viewport)
                            .layout(Layout::top_down(Align::Min)),
                    );
                    let mut cols: Vec<crate::lightbox::ColumnFn<'_, ()>> =
                        vec![Box::new(move |ui: &mut egui::Ui, _: &mut ()| {
                            if let Some(p) = &lt {
                                draw_text_column(
                                    ui,
                                    &ColumnHead {
                                        file_name: &l.rel_path,
                                        repo: &l.repo,
                                        accent: theme::blue(),
                                        read_only: l.read_only,
                                        is_main: false,
                                        source: &l.facts.abs_path,
                                    },
                                    p,
                                    height,
                                );
                            }
                        })];
                    if two_sided_now {
                        cols.push(Box::new(move |ui: &mut egui::Ui, _: &mut ()| {
                            if let Some(p) = &rt {
                                draw_text_column(
                                    ui,
                                    &ColumnHead {
                                        file_name: &r.rel_path,
                                        repo: &r.repo,
                                        accent: theme::tan(),
                                        read_only: r.read_only,
                                        is_main: false,
                                        source: &r.facts.abs_path,
                                    },
                                    p,
                                    height,
                                );
                            }
                        }));
                    }
                    draw_columns(&mut child, &mut (), cols);
                } else if ready {
                    // Both sides decoded: the shared A/B viewer — zoom / pan /
                    // flicker across both panes.
                    let (a_tex, a_img) = self.sized(0);
                    let (b_tex, b_img) = self.sized(1);
                    let scroll = ui.input(|i| i.smooth_scroll_delta.y);
                    draw_compare(
                        ui,
                        &mut self.compare,
                        viewport,
                        (&a_tex, a_img),
                        (&b_tex, b_img),
                        ComparePointer {
                            drag: bg.dragged().then(|| bg.drag_delta()),
                            scroll,
                            cursor: ctx.pointer_hover_pos(),
                        },
                    );
                    ui.painter().text(
                        egui::pos2(inner.min.x + 4.0, viewport.max.y + 2.0),
                        Align2::LEFT_TOP,
                        "wheel: zoom · drag: pan · Space flicker/swap · Esc close",
                        FontId::proportional(11.0),
                        theme::hairline(),
                    );
                } else {
                    // Not both decoded yet (or one produced no visual): static
                    // side-by-side, each pane its image, an in-flight "decoding…"
                    // note, or a settled "no preview" note. Compare stays disabled
                    // until both sides yield a texture.
                    let (left_pane, right_pane) = compare_split(viewport);
                    for (slot, pane) in [(0usize, left_pane), (1usize, right_pane)] {
                        match self.slot_state(slot) {
                            SlotState::Image => {
                                let (tex, img) = self.sized(slot);
                                let rect = crate::lightbox::fit_rect(pane, img);
                                draw_in_pane(ui, pane, rect, &tex);
                            }
                            note => {
                                let side = if slot == 0 { &self.left } else { &self.right };
                                let text = if note == SlotState::Decoding {
                                    "decoding…".to_string()
                                } else {
                                    side.placeholder()
                                };
                                ui.painter().text(
                                    pane.center(),
                                    Align2::CENTER_CENTER,
                                    text,
                                    FontId::proportional(14.0),
                                    theme::tan(),
                                );
                            }
                        }
                    }
                }

                // Facts + actions: one title per side, above its own image.
                let strip = Rect::from_min_max(
                    egui::pos2(inner.min.x, titles_top),
                    egui::pos2(inner.max.x, titles_top + TITLE_H),
                );
                let col_w = (strip.width() - 16.0) * 0.5;
                ui.scope_builder(
                    UiBuilder::new()
                        .max_rect(strip)
                        .layout(Layout::left_to_right(Align::Min)),
                    |ui| {
                        for is_left in [true, false] {
                            if !is_left && !two_sided {
                                continue;
                            }
                            let (side, other) = if is_left {
                                (&self.left, &self.right)
                            } else {
                                (&self.right, &self.left)
                            };
                            let can_step = if is_left {
                                self.can_step_left()
                            } else {
                                self.can_step_right()
                            };
                            let mark = self.marks[usize::from(!is_left)];
                            let mark_label = if two_sided {
                                if is_left { "DELETE A" } else { "DELETE B" }
                            } else {
                                "DELETE"
                            };
                            let picked = ui
                                .allocate_ui(egui::vec2(col_w, strip.height()), |ui| {
                                    let picked = side_strip(
                                        ui,
                                        side,
                                        other,
                                        is_left,
                                        verbosity,
                                        mark.map(|m| (mark_label, m)),
                                    );
                                    // The switcher belongs to its side, and is
                                    // offered only when that side has somewhere
                                    // to go — a pair shows none at all, so it can
                                    // never land on the file the other side has.
                                    let label = if is_left { "A" } else { "B" };
                                    // The control rows stack beside the facts:
                                    // one row chained after another was what
                                    // clipped the rightmost tool off the edge.
                                    ui.vertical(|ui| {
                                        if can_step {
                                            let position = self.switcher_label(is_left);
                                            ui.horizontal(|ui| {
                                                if ui.button(format!("< PREV {label}")).clicked() {
                                                    step = Some((is_left, -1));
                                                }
                                                if let Some(pos) = position {
                                                    ui.label(
                                                        RichText::new(pos).color(theme::tan()),
                                                    );
                                                }
                                                if ui.button(format!("NEXT {label} >")).clicked() {
                                                    step = Some((is_left, 1));
                                                }
                                            });
                                        }
                                        // Tools belong to the tab, and to the
                                        // side they act on. Available while
                                        // comparing — aligning a flipped copy
                                        // is done by looking at both.
                                        if tab_is_image {
                                            ui.horizontal(|ui| {
                                                if ui.button(format!("ROTATE {label}")).clicked() {
                                                    turn = Some((is_left, Orient::RotateCw));
                                                }
                                                if ui.button(format!("MIRROR {label}")).clicked() {
                                                    turn = Some((is_left, Orient::FlipH));
                                                }
                                            });
                                        }
                                        // Audio transport, per side. The caller
                                        // owns the player — one audio device,
                                        // and the tab it belongs to also drives
                                        // the cards behind.
                                        if tab_is_audio && player.is_some() {
                                            ui.horizontal(|ui| {
                                                if ui.button(format!("PLAY {label}")).clicked() {
                                                    play = Some(is_left);
                                                }
                                                if ui.button("PAUSE").clicked() {
                                                    pause = true;
                                                }
                                                if ui
                                                    .button(format!("SPEED {}×", speed_label))
                                                    .clicked()
                                                {
                                                    cycle_speed = true;
                                                }
                                            });
                                        }
                                    });
                                    picked
                                })
                                .inner;
                            if picked.is_some() {
                                pick = picked;
                            }
                            ui.add_space(16.0);
                        }
                    },
                );
                pick
            })
            .inner;

        // Applied after drawing: the closures above borrow both sides, and a
        // side that moves must not change under the frame that drew it.
        {
            use crate::lightbox::MetaAction;
            match (meta_l, meta_r) {
                (MetaAction::Edit, _) => self.open_tag_editor(true),
                (_, MetaAction::Edit) => self.open_tag_editor(false),
                (MetaAction::Save, _) | (_, MetaAction::Save) => self.save_tag_edit(),
                (MetaAction::Cancel, _) | (_, MetaAction::Cancel) => self.tag_edit = None,
                (MetaAction::None, MetaAction::None) => {}
            }
        }
        if let Some((is_left, op)) = turn {
            self.rotate(is_left, op);
            self.upload(ctx, usize::from(!is_left));
        }
        if hide {
            self.hide_second();
        }
        if show {
            self.second_hidden = false;
            // A viewer opened on one file carries a placeholder B (the same
            // file); revealing B must land on another candidate, never on the
            // file A shows — self-comparison is the defect this viewer exists
            // to make impossible.
            if self.right.facts.abs_path == self.left.facts.abs_path
                && let Some(next) = self.stepped(&self.right, &self.left, 1)
            {
                self.right = next;
                self.refresh_side(ctx, 1);
            }
        }

        // Arrow keys: on a two-sided audio pair they flip which copy is audible
        // (below, gap-free on the loaded pair — re-pointing a side instead would
        // force a reloading pause, the bug the user originally hit); with the
        // second side hidden they step the shown file through the pool.
        let audio_pair =
            self.two_sided() && self.left.facts.is_audio() && self.right.facts.is_audio();
        let flip = (arrow_l || arrow_r) && audio_pair;
        if (arrow_l || arrow_r) && !audio_pair && !self.two_sided() && step.is_none() {
            step = Some((true, if arrow_r { 1 } else { -1 }));
        }

        let stepped = step;
        if let Some((is_left, dir)) = step {
            let slot = usize::from(!is_left);
            let before = if is_left {
                self.left.facts.abs_path.clone()
            } else {
                self.right.facts.abs_path.clone()
            };
            if is_left {
                self.step_left(dir);
            } else {
                self.step_right(dir);
            }
            let moved = if is_left {
                self.left.facts.abs_path != before
            } else {
                self.right.facts.abs_path != before
            };
            if moved {
                self.refresh_side(ctx, slot);
            }
        }

        // The audio transport, driven strictly through the caller's player —
        // one audio device, owned by the caller whose surface sits behind this
        // viewer. `snap` is the transport as this frame found it.
        if let Some(p) = player {
            let snap = p.snapshot();
            let total = u64::from(self.left.facts.audio_ms.unwrap_or(0));
            let offset = snap.pos_ms.min(total);
            // Whether the player already holds exactly this (A, B) pair — then
            // a swap is an instant, gap-free volume flip rather than a reload.
            let pair_loaded = snap.paired
                && snap.hex_a.as_deref() == Some(self.left.facts.hash_hex.as_str())
                && snap.hex_b.as_deref() == Some(self.right.facts.hash_hex.as_str());
            let (l_hex, l_path) = (
                self.left.facts.hash_hex.clone(),
                self.left.facts.abs_path.clone(),
            );
            let (r_hex, r_path) = (
                self.right.facts.hash_hex.clone(),
                self.right.facts.abs_path.clone(),
            );
            // Load both copies into a synced pair, `want_b` audible.
            let start_pair =
                |want_b: bool| p.play_pair(&l_hex, &l_path, &r_hex, &r_path, total, offset, want_b);
            // Anything that already (re)starts playback this frame makes the
            // keep-pair maintenance below redundant.
            let mut busy = false;

            // PLAY A / PLAY B: on an audio pair both copies load in sync with
            // the asked-for side audible, so a later flip is gap-free.
            if let Some(is_left) = play {
                busy = true;
                if audio_pair {
                    start_pair(!is_left);
                } else {
                    let side = if is_left { &self.left } else { &self.right };
                    p.play(
                        &side.facts.hash_hex,
                        &side.facts.abs_path,
                        u64::from(side.facts.audio_ms.unwrap_or(0)),
                        0,
                    );
                }
                self.audio_active = Some(usize::from(!is_left));
            }
            if pause {
                busy = true;
                p.toggle_pause();
            }
            // P: pause/resume what is loaded, else start playing — the pair
            // when comparing, the shown file alone otherwise.
            if key_p {
                busy = true;
                if snap.loaded {
                    p.toggle_pause();
                } else if audio_pair {
                    start_pair(false);
                    self.audio_active = Some(0);
                } else if self.left.facts.is_audio() {
                    p.play(&l_hex, &l_path, total, offset);
                    self.audio_active = Some(0);
                }
            }
            // Arrows on the pair: flip the audible copy, gap-free.
            if flip && snap.loaded {
                busy = true;
                let want_b = self.audio_active != Some(1);
                if pair_loaded {
                    let target = if want_b {
                        r_hex.as_str()
                    } else {
                        l_hex.as_str()
                    };
                    if snap.hex.as_deref() != Some(target) {
                        p.flip();
                    }
                } else {
                    start_pair(want_b);
                }
                self.audio_active = Some(usize::from(want_b));
                if self.compare.flicker {
                    self.compare.show_b = want_b;
                }
            }
            // A flicker swap flips the audio with the picture, gap-free.
            if swapped && audio_pair && snap.loaded {
                busy = true;
                let want_b = self.compare.show_b;
                if pair_loaded {
                    let target = if want_b {
                        r_hex.as_str()
                    } else {
                        l_hex.as_str()
                    };
                    if snap.hex.as_deref() != Some(target) {
                        p.flip();
                    }
                } else {
                    start_pair(want_b);
                }
                self.audio_active = Some(usize::from(want_b));
            }
            // Stepping the shown file follows with whatever transport state it
            // was in: playing keeps playing the new copy, a deliberate pause
            // stays paused with the new copy loaded — leaving the previous file
            // loaded would show one copy and resume another.
            if let Some((true, _)) = stepped
                && !self.two_sided()
                && snap.loaded
                && self.left.facts.is_audio()
            {
                busy = true;
                let t = u64::from(self.left.facts.audio_ms.unwrap_or(0));
                let at = snap.pos_ms.min(t);
                let (hex, path) = (&self.left.facts.hash_hex, &self.left.facts.abs_path);
                if snap.playing {
                    p.play(hex, path, t, at);
                } else {
                    p.load_paused(hex, path, t, at);
                }
                self.audio_active = Some(0);
            }
            // Keep the synced pair loaded whenever the pair plays, so flips
            // stay instant even after a side was stepped to another file.
            if audio_pair && snap.playing && !pair_loaded && !busy {
                start_pair(self.audio_active == Some(1));
            }
            if cycle_speed {
                let r = snap.speed;
                p.set_speed(match r {
                    r if r < 0.9 => 1.0,
                    r if r < 1.9 => 2.0,
                    _ => 0.5,
                });
            }
        }
        picked
    }
}

/// One side's facts (repo, path, size / date / type with the bigger-or-newer
/// value highlighted so the difference reads without comparing both numbers) and
/// its actions — the caller's own: a deletion-mark pill when `mark` is supplied,
/// else the DIFF board's OVERWRITE / DELETE commands. Returns the chosen action,
/// if any.
fn side_strip(
    ui: &mut egui::Ui,
    side: &DiffSide,
    other: &DiffSide,
    is_left: bool,
    verbosity: TooltipVerbosity,
    mark: Option<(&str, MarkPill)>,
) -> Option<DiffPick> {
    let mut pick = None;
    ui.vertical(|ui| {
        ui.label(
            RichText::new(&side.repo)
                .color(if is_left {
                    theme::orange()
                } else {
                    theme::blue()
                })
                .size(14.0)
                .strong(),
        );
        ui.label(
            RichText::new(&side.rel_path)
                .color(theme::text())
                .size(12.0),
        );
        let size_color = if side.facts.size > other.facts.size {
            theme::green()
        } else {
            theme::text()
        };
        let date_color = if side.facts.modified_ms > other.facts.modified_ms {
            theme::green()
        } else {
            theme::text()
        };
        ui.add_space(4.0);
        ui.label(
            RichText::new(format_size(side.facts.size))
                .color(size_color)
                .size(13.0)
                .strong(),
        );
        ui.label(
            RichText::new(format_mtime(side.facts.modified_ms))
                .color(date_color)
                .size(13.0),
        );
        ui.label(
            RichText::new(
                side.facts
                    .mime
                    .clone()
                    .unwrap_or_else(|| "unknown type".into()),
            )
            .color(theme::grey())
            .size(12.0),
        );
        ui.add_space(6.0);
        // A caller that supplied marks acts through the pill alone.
        if let Some((label, pill)) = mark {
            if crate::lightbox::mark_pill(ui, verbosity, label, pill.marked, pill.markable) {
                pick = Some(DiffPick::ToggleMark { on_left: is_left });
            }
            return;
        }
        ui.horizontal(|ui| {
            if ui
                .add(
                    egui::Button::new(RichText::new("OVERWRITE OTHER").color(theme::black()))
                        .fill(theme::tan()),
                )
                .explain(
                    verbosity,
                    "Replace the other side with this version",
                    "Copy this version over the other repository's file, so both repositories \
                     hold this one. The other version is gone afterwards.",
                )
                .clicked()
            {
                pick = Some(DiffPick::Overwrite { from_left: is_left });
            }
            if ui
                .add(
                    egui::Button::new(RichText::new("DELETE").color(theme::black()))
                        .fill(theme::red()),
                )
                .explain(
                    verbosity,
                    "Delete this version",
                    "Delete this file from this repository. The other repository's version is \
                     left alone. This cannot be undone.",
                )
                .clicked()
            {
                pick = Some(DiffPick::Delete { on_left: is_left });
            }
        });
    });
    pick
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// An audio side with its own identity, for the transport tests.
    fn audio_side(name: &str, hex: &str) -> DiffSide {
        let mut side = diff_side(Some("audio/mpeg"));
        side.rel_path = name.to_string();
        side.facts.abs_path = PathBuf::from(format!("/tmp/{name}"));
        side.facts.hash_hex = hex.to_string();
        side.facts.audio_ms = Some(1000);
        side
    }

    /// A side identified by name, for the pool/switcher tests.
    fn named_side(name: &str) -> DiffSide {
        DiffSide {
            rel_path: name.to_string(),
            facts: FileFacts {
                abs_path: PathBuf::from(format!("/tmp/{name}")),
                ..diff_side(Some("image/jpeg")).facts
            },
            ..diff_side(Some("image/jpeg"))
        }
    }

    /// With more than two candidates, each side can be stepped through them —
    /// this is what replaces the single switcher that could walk A onto B.
    #[test]
    fn stepping_a_side_moves_it_to_the_next_candidate() {
        let pool = vec![
            named_side("a.jpg"),
            named_side("b.jpg"),
            named_side("c.jpg"),
        ];
        // B sits on the last candidate, so the one A steps onto is free — this
        // test is about stepping alone; skipping B is the next cycle.
        let mut cmp =
            DiffCompare::new_with_pool(named_side("a.jpg"), Some(named_side("c.jpg")), pool);

        cmp.step_left(1);
        assert_eq!(
            cmp.left.rel_path, "b.jpg",
            "A steps forward to the next candidate"
        );
    }

    /// The defect that started the redesign: the old switcher walked A through
    /// every member including B's, so a step could land A on the file B was
    /// already showing and you compared a file with itself.
    #[test]
    fn stepping_skips_the_file_the_other_side_shows() {
        let pool = vec![
            named_side("a.jpg"),
            named_side("b.jpg"),
            named_side("c.jpg"),
        ];
        let mut cmp =
            DiffCompare::new_with_pool(named_side("a.jpg"), Some(named_side("b.jpg")), pool);

        cmp.step_left(1);
        assert_eq!(
            cmp.left.rel_path, "c.jpg",
            "b is B's file, so A steps over it"
        );
        assert_ne!(
            cmp.left.rel_path, cmp.right.rel_path,
            "the two sides can never be the same file"
        );
    }

    /// Drive the viewer for a frame and return the harness, so tests can query
    /// what a user would actually see.
    fn rendered(cmp: DiffCompare) -> egui_kittest::Harness<'static, DiffCompare> {
        let mut init = false;
        let mut h = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1200.0, 800.0))
            .build_ui_state(
                move |ui, cmp: &mut DiffCompare| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    cmp.view(&ui.ctx().clone(), TooltipVerbosity::default(), None);
                },
                cmp,
            );
        h.run();
        h
    }

    /// With somewhere to go, each side gets its own switcher.
    #[test]
    fn three_candidates_render_a_switcher_per_side() {
        use egui_kittest::kittest::Queryable;
        let pool = vec![
            named_side("a.jpg"),
            named_side("b.jpg"),
            named_side("c.jpg"),
        ];
        let h = rendered(DiffCompare::new_with_pool(
            named_side("a.jpg"),
            Some(named_side("b.jpg")),
            pool,
        ));

        assert_eq!(
            h.query_all_by_label_contains("NEXT").count(),
            2,
            "one forward control per side"
        );
        assert_eq!(
            h.query_all_by_label_contains("PREV").count(),
            2,
            "and one back control per side"
        );
    }

    /// Presence is not function: the control must actually move its side, and
    /// only its side.
    #[test]
    fn clicking_next_moves_that_side_only() {
        use egui_kittest::kittest::Queryable;
        let pool = vec![
            named_side("a.jpg"),
            named_side("b.jpg"),
            named_side("c.jpg"),
        ];
        let mut h = rendered(DiffCompare::new_with_pool(
            named_side("a.jpg"),
            Some(named_side("b.jpg")),
            pool,
        ));

        h.get_by_label_contains("NEXT A").click();
        h.run();

        assert_eq!(
            h.state().left.rel_path,
            "c.jpg",
            "A moved forward, skipping B's file"
        );
        assert_eq!(h.state().right.rel_path, "b.jpg", "and B did not move");
    }

    /// Each side's identifying facts belong with that side, above its image —
    /// not in a shared legend at the bottom, and not in a distant top bar.
    #[test]
    fn each_side_has_its_own_title_above_the_images() {
        use egui_kittest::kittest::Queryable;
        let h = rendered(DiffCompare::new_with_pool(
            named_side("left.jpg"),
            Some(named_side("right.jpg")),
            vec![named_side("left.jpg"), named_side("right.jpg")],
        ));

        let a = h.get_by_label_contains("left.jpg").rect();
        let b = h.get_by_label_contains("right.jpg").rect();

        assert!(
            a.max.x <= b.min.x,
            "A's title is left of B's, not stacked: {a:?} vs {b:?}"
        );
        assert!(
            a.max.y < 400.0 && b.max.y < 400.0,
            "both titles sit in the top half, above the images: {a:?} {b:?}"
        );
    }

    /// Hiding the second side gives the first the whole screen — the single-file
    /// analysis case.
    #[test]
    fn hiding_the_second_side_leaves_only_the_first() {
        use egui_kittest::kittest::Queryable;
        let mut cmp = DiffCompare::new_with_pool(
            named_side("left.jpg"),
            Some(named_side("right.jpg")),
            vec![named_side("left.jpg"), named_side("right.jpg")],
        );
        cmp.hide_second();
        let h = rendered(cmp);

        assert!(
            h.query_all_by_label_contains("left.jpg").count() > 0,
            "the first side is still shown"
        );
        assert_eq!(
            h.query_all_by_label_contains("right.jpg").count(),
            0,
            "the second side is gone entirely, not merely narrowed"
        );
    }

    /// A label query passes on a clipped widget, so the controls are asserted
    /// geometrically. The reported defect was the rightmost control clipping off
    /// the edge, and controls drifting downward the further right they sat.
    #[test]
    fn header_controls_stay_inside_a_narrow_window_and_share_a_baseline() {
        use egui_kittest::kittest::Queryable;
        let width = 900.0;
        let mut init = false;
        let mut h = egui_kittest::Harness::builder()
            .with_size(egui::vec2(width, 700.0))
            .build_ui_state(
                move |ui, cmp: &mut DiffCompare| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    cmp.view(&ui.ctx().clone(), TooltipVerbosity::default(), None);
                },
                DiffCompare::new_with_pool(
                    named_side("left.jpg"),
                    Some(named_side("right.jpg")),
                    vec![named_side("left.jpg"), named_side("right.jpg")],
                ),
            );
        h.run();

        let close = h.get_by_label_contains("CLOSE").rect();
        assert!(
            close.max.x <= width && close.min.x >= 0.0,
            "CLOSE is inside the window: {close:?}"
        );

        // Controls *within the tab row* share a baseline — the reported defect
        // was them sinking the further right they sat. CLOSE lives in the title
        // row above and is deliberately not compared against.
        let first = h.get_by_label_contains("Image").rect();
        let other = h.get_by_label_contains("Text").rect();
        assert!(
            other.max.x <= width,
            "the tab escapes the {width}px window: {other:?}"
        );
        assert!(
            (other.center().y - first.center().y).abs() < 2.0,
            "tabs share a baseline rather than drifting: {other:?} vs {first:?}"
        );
    }

    /// A photograph's bytes are as legitimate a thing to compare as its pixels —
    /// reading a JPEG header is a forensic act. Today text is offered only to
    /// files that are not image, audio or video.
    #[test]
    fn an_image_pair_also_offers_text_and_bytes() {
        let cmp = DiffCompare::new(diff_side(Some("image/jpeg")), diff_side(Some("image/jpeg")));
        let (l, r) = cmp.reps();
        let kinds = crate::lightbox::tab_kinds(&l, Some(&r));
        assert!(
            kinds.contains(&RepresentationKind::Image),
            "still an image pair: {kinds:?}"
        );
        assert!(
            kinds.contains(&RepresentationKind::Text),
            "and its text/bytes are reachable: {kinds:?}"
        );
    }

    /// The law: any file opens, whatever it is. Previewability decides which
    /// tabs appear, not whether the viewer opens at all — a pair of unknown
    /// blobs still compares as bytes.
    #[test]
    fn any_file_opens_even_with_no_renderable_form() {
        for mime in [
            Some("application/pdf"),
            Some("application/octet-stream"),
            Some("application/vnd.oasis.opendocument.text"),
            None,
        ] {
            let cmp = DiffCompare::new(diff_side(mime), diff_side(mime));
            let (l, r) = cmp.reps();
            let kinds = crate::lightbox::tab_kinds(&l, Some(&r));
            assert!(
                kinds.contains(&RepresentationKind::Text),
                "{mime:?} is comparable as text/bytes: {kinds:?}"
            );
        }
    }

    /// Rotation is per side and survives into the comparison — finding the copy
    /// somebody flipped is the reason this tool has a compare view at all.
    #[test]
    fn rotating_a_side_turns_that_side_only() {
        let mut cmp = DiffCompare::new(named_side("left.jpg"), named_side("right.jpg"));
        // 635x465, as the user's scans are.
        cmp.set_base_size(0, (635, 465));
        cmp.set_base_size(1, (635, 465));

        cmp.rotate(true, crate::imgedit::Orient::RotateCw);

        assert_eq!(
            cmp.oriented_size(0),
            Some((465, 635)),
            "a quarter turn swaps that side's dimensions"
        );
        assert_eq!(
            cmp.oriented_size(1),
            Some((635, 465)),
            "and leaves the other side alone"
        );
    }

    /// The tools belong to the tab: the Image tab carries rotate and mirror, and
    /// they are reachable *while comparing*, not only before.
    #[test]
    fn the_image_tab_offers_rotation_while_comparing() {
        use egui_kittest::kittest::Queryable;
        let mut cmp = DiffCompare::new_with_pool(
            named_side("left.jpg"),
            Some(named_side("right.jpg")),
            vec![named_side("left.jpg"), named_side("right.jpg")],
        );
        cmp.tab = RepresentationKind::Image;
        let mut h = rendered(cmp);

        assert_eq!(
            h.query_all_by_label_contains("ROTATE").count(),
            2,
            "a rotate control per side"
        );

        h.get_by_label_contains("ROTATE A").click();
        h.run();
        assert_eq!(h.state().ops_len(0), 1, "clicking it turns that side");
        assert_eq!(h.state().ops_len(1), 0, "and not the other");
    }

    /// Metadata is a tab like any other now, so it is reachable *while*
    /// comparing — the user's report was that it vanished the moment you
    /// compared, because comparing used to be a different screen.
    #[test]
    fn metadata_is_reachable_while_comparing() {
        use egui_kittest::kittest::Queryable;
        let mut cmp = DiffCompare::new_with_pool(
            diff_side(Some("audio/mpeg")),
            Some(diff_side(Some("audio/mpeg"))),
            vec![],
        );
        cmp.tab = RepresentationKind::Audio;
        let mut h = rendered(cmp);

        // The tab row is present while comparing, and Metadata is on it.
        h.get_by_label_contains("Metadata").click();
        h.run();
        assert_eq!(
            h.state().tab,
            RepresentationKind::Metadata,
            "metadata is one click away without leaving the comparison"
        );
    }

    /// Images have no EXIF writer, so no edit control is offered for them.
    #[test]
    fn image_metadata_offers_no_edit_control() {
        let cmp = DiffCompare::new(diff_side(Some("image/jpeg")), diff_side(Some("image/jpeg")));
        let (l, _) = cmp.reps();
        assert!(
            l.metadata.as_ref().is_none_or(|m| !m.can_save),
            "an image's metadata is read-only"
        );
    }

    /// An audio pair opens on its own representation and `P` starts the synced
    /// A/B pair — both copies loaded, only one audible — so the later flip is
    /// gap-free. `→`/`←` flip which copy is audible on the loaded pair (no
    /// reload, no gap), and the cursor (`audio_active`) follows.
    #[test]
    fn p_plays_the_gapless_pair_and_arrows_flip_the_audible_copy() {
        let player = std::sync::Arc::new(crate::player::Player::new());
        let cmp = DiffCompare::new(audio_side("a.mp3", "aaaa"), audio_side("b.mp3", "bbbb"));

        let p = std::sync::Arc::clone(&player);
        let mut init = false;
        let mut h = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1200.0, 800.0))
            .build_ui_state(
                move |ui, cmp: &mut DiffCompare| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    cmp.view(&ui.ctx().clone(), TooltipVerbosity::default(), Some(&p));
                },
                cmp,
            );
        h.run();
        assert_eq!(
            h.state().tab,
            RepresentationKind::Audio,
            "an audio pair opens on the Audio representation, not Metadata"
        );

        h.key_press(egui::Key::P);
        h.step();
        h.step();
        let snap = player.snapshot();
        assert!(snap.paired, "P loads the synced A/B pair");
        assert_eq!(snap.hex.as_deref(), Some("aaaa"), "A is audible first");
        assert!(snap.playing, "and it is playing");

        h.key_press(egui::Key::ArrowRight);
        h.step();
        h.step();
        let snap = player.snapshot();
        assert!(snap.paired, "→ keeps the loaded pair — no reload, no gap");
        assert_eq!(snap.hex.as_deref(), Some("bbbb"), "→ flips audible to B");
        assert!(snap.playing, "still playing after the flip");
        assert_eq!(
            h.state().audio_active,
            Some(1),
            "the playback cursor moves to B"
        );

        h.key_press(egui::Key::ArrowLeft);
        h.step();
        h.step();
        let snap = player.snapshot();
        assert!(snap.paired, "← keeps the pair too");
        assert_eq!(snap.hex.as_deref(), Some("aaaa"), "← flips back to A");
        assert_eq!(h.state().audio_active, Some(0));
    }

    /// With the second side hidden, `→` steps the shown file through the pool —
    /// and a deliberately paused player loads the newly shown copy without
    /// resuming, so play never resumes a file that is no longer on screen.
    #[test]
    fn stepping_the_single_view_keeps_the_paused_transport() {
        let player = std::sync::Arc::new(crate::player::Player::new());
        let pool = vec![
            audio_side("t0.mp3", "aaaa"),
            audio_side("t1.mp3", "bbbb"),
            audio_side("t2.mp3", "cccc"),
        ];
        let mut cmp = DiffCompare::new_with_pool(audio_side("t0.mp3", "aaaa"), None, pool);
        cmp.hide_second();

        let p = std::sync::Arc::clone(&player);
        let mut init = false;
        let mut h = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1200.0, 800.0))
            .build_ui_state(
                move |ui, cmp: &mut DiffCompare| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    cmp.view(&ui.ctx().clone(), TooltipVerbosity::default(), Some(&p));
                },
                cmp,
            );
        h.run();

        player.load_paused(
            "aaaa",
            std::path::Path::new("/nonexistent-dedup-test/t0.mp3"),
            1000,
            500,
        );
        h.step();
        h.step();
        let snap = player.snapshot();
        assert!(
            snap.loaded && !snap.playing,
            "set up: copy A loaded, paused"
        );

        h.key_press(egui::Key::ArrowRight);
        h.step();
        h.step();
        assert_eq!(
            h.state().left.rel_path,
            "t1.mp3",
            "→ steps the shown file forward through the pool"
        );
        let snap = player.snapshot();
        assert!(
            !snap.playing,
            "a deliberate pause survives the step - it must not resume on its own"
        );
        assert_eq!(
            snap.hex.as_deref(),
            Some("bbbb"),
            "the newly shown copy is the one now loaded, so play resumes the right file"
        );
    }

    /// A writable audio side edits its ID3 tags on the Metadata tab: `T` opens
    /// the editor pre-filled from the file, SAVE TAGS writes only the tags to
    /// disk (the audio untouched, other fields preserved), the editor closes,
    /// and the viewer stays open — saving is part of looking, not a way out.
    #[test]
    fn a_writable_audio_side_edits_and_saves_its_tags() {
        use egui_kittest::kittest::Queryable;
        let tmp = tempfile::tempdir().unwrap();
        let mp3 = tmp.path().join("song.mp3");
        crate::id3tags::write_bare_mp3(&mp3);
        crate::id3tags::write(
            &mp3,
            &crate::id3tags::Tags {
                title: "Old".into(),
                artist: "Cohen".into(),
                ..Default::default()
            },
        )
        .unwrap();

        let mut left = audio_side("song.mp3", "aaaa");
        left.facts.abs_path = mp3.clone();
        left.read_only = false;
        let cmp = DiffCompare::new(left, audio_side("other.mp3", "bbbb"));

        let mut init = false;
        let mut h = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1200.0, 800.0))
            .build_ui_state(
                move |ui, state: &mut (DiffCompare, Vec<DiffPick>)| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    let (cmp, picks) = state;
                    if let Some(p) = cmp.view(&ui.ctx().clone(), TooltipVerbosity::default(), None)
                    {
                        picks.push(p);
                    }
                },
                (cmp, Vec::new()),
            );
        h.run();

        // T opens the editor, pre-filled from the file, on the Metadata tab.
        h.key_press(egui::Key::T);
        h.run();
        h.run();
        assert_eq!(
            h.state().0.tab,
            RepresentationKind::Metadata,
            "T lands on the Metadata tab"
        );
        assert_eq!(
            h.state().0.tag_edit.as_ref().map(|t| t.tags.title.as_str()),
            Some("Old"),
            "editor opens pre-filled with the current title"
        );

        // Type a new title, then SAVE TAGS.
        h.state_mut().0.tag_edit.as_mut().unwrap().tags.title = "New Title".into();
        h.run();
        h.get_by_label("SAVE TAGS").click();
        h.run();

        let saved = crate::id3tags::read(&mp3).expect("tags still readable");
        assert_eq!(saved.title, "New Title", "the new title is written to disk");
        assert_eq!(saved.artist, "Cohen", "other tags are preserved");
        assert!(h.state().0.tag_edit.is_none(), "the editor closes on save");
        assert!(
            h.state().1.is_empty(),
            "saving keeps the viewer open — no pick was reported"
        );
    }

    /// The shared viewer can drive audio, given the caller's player — one audio
    /// device, owned by the tab that also drives the cards behind the viewer.
    #[test]
    fn the_audio_tab_plays_the_side_asked_for() {
        use egui_kittest::kittest::Queryable;
        let player = std::sync::Arc::new(crate::player::Player::new());
        let mut cmp = DiffCompare::new(audio_side("a.mp3", "aaaa"), audio_side("b.mp3", "bbbb"));
        cmp.tab = RepresentationKind::Audio;

        let p = std::sync::Arc::clone(&player);
        let mut init = false;
        let mut h = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1200.0, 800.0))
            .build_ui_state(
                move |ui, cmp: &mut DiffCompare| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    cmp.view(&ui.ctx().clone(), TooltipVerbosity::default(), Some(&p));
                },
                cmp,
            );
        h.run();

        assert_eq!(
            h.query_all_by_label_contains("PLAY").count(),
            2,
            "a transport control per side"
        );
        h.get_by_label_contains("PLAY B").click();
        h.run();
        assert_eq!(
            player.snapshot().hex.as_deref(),
            Some("bbbb"),
            "the side asked for is the one loaded"
        );
    }

    /// The Duplicates caller supplies deletion *marks* as the per-side actions:
    /// a pill per side that toggles only that side's mark, and none of the DIFF
    /// board's OVERWRITE/DELETE commands — actions belong to the caller
    /// (spec §35). A protected side (read-only repo) renders its pill disabled.
    #[test]
    fn caller_supplied_marks_render_a_pill_per_side_and_report_toggles() {
        use egui_kittest::kittest::Queryable;
        let pool = vec![named_side("a.jpg"), named_side("b.jpg")];
        let mut cmp =
            DiffCompare::new_with_pool(named_side("a.jpg"), Some(named_side("b.jpg")), pool);
        cmp.set_marks(
            Some(MarkPill {
                marked: false,
                markable: true,
            }),
            Some(MarkPill {
                marked: true,
                markable: true,
            }),
        );

        let mut init = false;
        let mut h = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1200.0, 800.0))
            .build_ui_state(
                move |ui, state: &mut (DiffCompare, Vec<DiffPick>)| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::DARK);
                        init = true;
                    }
                    let (cmp, picks) = state;
                    if let Some(p) = cmp.view(&ui.ctx().clone(), TooltipVerbosity::default(), None)
                    {
                        picks.push(p);
                    }
                },
                (cmp, Vec::new()),
            );
        h.run();

        assert!(
            h.query_all_by_label_contains("DELETE A").count() > 0,
            "a mark pill for the left side"
        );
        assert!(
            h.query_all_by_label_contains("DELETE B").count() > 0,
            "and one for the right side"
        );
        assert_eq!(
            h.query_all_by_label_contains("OVERWRITE").count(),
            0,
            "the DIFF board's commands are not another caller's actions"
        );

        h.get_by_label_contains("DELETE A").click();
        h.run();
        assert_eq!(
            h.state().1.as_slice(),
            &[DiffPick::ToggleMark { on_left: true }],
            "clicking the pill reports a toggle for that side only, and does not close"
        );
    }

    /// The switcher says where a side stands among *its* candidates — the pool
    /// minus the file the other side shows. A four-file pool therefore reads
    /// `<1 / 3>`, never `/ 4`: the member count must not appear where the count
    /// of others belongs (the defect the four-copy cycler regression pinned).
    #[test]
    fn the_switcher_labels_the_position_among_that_sides_candidates() {
        use egui_kittest::kittest::Queryable;
        let pool = vec![
            named_side("a.jpg"),
            named_side("b.jpg"),
            named_side("c.jpg"),
            named_side("d.jpg"),
        ];
        let mut h = rendered(DiffCompare::new_with_pool(
            named_side("a.jpg"),
            Some(named_side("b.jpg")),
            pool,
        ));

        assert!(
            h.query_all_by_label("<1 / 3>").count() > 0,
            "each side stands first among its three candidates"
        );

        for expected in ["<2 / 3>", "<3 / 3>", "<1 / 3>"] {
            h.get_by_label_contains("NEXT B").click();
            h.run();
            assert!(
                h.query_all_by_label(expected).count() > 0,
                "stepping B reaches {expected}"
            );
            assert!(
                h.query_all_by_label("<1 / 4>").count() == 0
                    && h.query_all_by_label("<4 / 4>").count() == 0,
                "the pool size never appears in the candidates slot"
            );
        }
    }

    /// A pair has nothing to switch to — the other candidate is already on the
    /// other side. The switcher is therefore not offered at all, rather than
    /// offered and inert, which is what let the old one compare a file with
    /// itself.
    #[test]
    fn a_pair_offers_no_stepping() {
        let pool = vec![named_side("a.jpg"), named_side("b.jpg")];
        let cmp = DiffCompare::new_with_pool(named_side("a.jpg"), Some(named_side("b.jpg")), pool);

        assert!(!cmp.can_step_left(), "a pair leaves A nowhere to go");
        assert!(!cmp.can_step_right(), "nor B");
    }

    /// Three candidates give each side somewhere to go, so the switcher appears.
    #[test]
    fn three_candidates_offer_stepping_on_both_sides() {
        let pool = vec![
            named_side("a.jpg"),
            named_side("b.jpg"),
            named_side("c.jpg"),
        ];
        let cmp = DiffCompare::new_with_pool(named_side("a.jpg"), Some(named_side("b.jpg")), pool);

        assert!(cmp.can_step_left());
        assert!(cmp.can_step_right());
    }

    /// The completeness check this rewrite exists to make: the old viewers —
    /// the Duplicates tab's tabbed lightbox, its separate audio viewer, and the
    /// Browse single-image viewer — are deleted, and nothing in the crate
    /// references them. The identifiers are assembled at runtime so this
    /// file's own source cannot trip the scan.
    #[test]
    fn nothing_in_the_crate_references_the_deleted_viewers() {
        let name = |parts: &[&str]| parts.concat();
        let forbidden = [
            name(&["draw_", "lightbox_", "overview"]),
            name(&["draw_", "lightbox_", "metadata"]),
            name(&["draw_", "lightbox_", "text"]),
            name(&["audio_", "lightbox"]),
            name(&["lightbox_", "shell"]),
            name(&["Lightbox", "State"]),
            name(&["FullRes", "Cache"]),
            name(&["save_", "edited"]),
            name(&["draw_", "filmstrip"]),
            name(&["draw_", "overview_", "column"]),
            name(&["previewable_", "texture"]),
        ];
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        for entry in std::fs::read_dir(&src).expect("read src dir") {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let body = std::fs::read_to_string(&path).expect("read source file");
            for f in &forbidden {
                assert!(
                    !body.contains(f.as_str()),
                    "{} still references the deleted viewers ({f})",
                    path.display()
                );
            }
        }
    }

    /// A DIFF side carrying just a mime, for the previewability unit test.
    fn diff_side(mime: Option<&str>) -> DiffSide {
        DiffSide {
            repo: "r".into(),
            rel_path: "a.bin".into(),
            read_only: true,
            facts: FileFacts {
                size: 1,
                modified_ms: 1,
                mime: mime.map(str::to_string),
                img_size: None,
                audio_ms: None,
                audio_seed: None,
                hash_hex: "deadbeef".into(),
                abs_path: PathBuf::from("/tmp/a.bin"),
                origin: None,
                exif: None,
            },
        }
    }

    /// A side with a visual (image / video) is compared through the shared
    /// viewer; one without (a document) shows a "no preview" note naming the type
    /// — not a stuck "decoding…" — and disables compare for the pair.
    #[test]
    fn diff_previewability_follows_mime_and_placeholder_names_the_type() {
        assert!(diff_side(Some("image/jpeg")).previewable());
        assert!(diff_side(Some("video/mp4")).previewable());
        let doc = diff_side(Some("application/pdf"));
        assert!(!doc.previewable(), "a document has no visual to compare");
        assert!(
            doc.placeholder().contains("application/pdf"),
            "the pane names the type it cannot preview"
        );
        assert!(diff_side(None).placeholder().contains("file type"));
    }

    /// Two tagged tracks offer the **Metadata** representation from a DIFF row.
    /// It is read-only here: DIFF's row commands are how a file is acted on, so
    /// there is deliberately no second tag-editing surface.
    #[test]
    fn two_tagged_tracks_offer_metadata_from_a_diff_row() {
        let cmp = DiffCompare::new(diff_side(Some("audio/mpeg")), diff_side(Some("audio/mpeg")));
        let (l, r) = cmp.reps();
        let kinds = crate::lightbox::tab_kinds(&l, Some(&r));
        assert!(
            kinds.contains(&RepresentationKind::Metadata),
            "an ID3-capable pair offers Metadata, got {kinds:?}"
        );
        // Both sides are built read-only, which is what hides the EDIT button.
        assert!(
            l.metadata.as_ref().is_none_or(|m| !m.can_save),
            "DIFF never offers tag writing"
        );
    }

    /// Two documents now offer the **Text** representation from a DIFF row, using
    /// the same `lightbox` helpers the Duplicates viewer uses. Before, DIFF could
    /// only ever show a picture, so a pair of PDFs had nothing at all.
    #[test]
    fn two_documents_offer_the_text_representation_from_a_diff_row() {
        let cmp = DiffCompare::new(
            diff_side(Some("application/pdf")),
            diff_side(Some("application/pdf")),
        );
        let (l, r) = cmp.reps();
        let kinds = crate::lightbox::tab_kinds(&l, Some(&r));
        assert!(
            kinds.contains(&RepresentationKind::Text),
            "a document pair offers Text, got {kinds:?}"
        );
        assert!(
            !kinds.contains(&RepresentationKind::Image),
            "and not Image, which it cannot produce"
        );
    }

    /// An image pair keeps its Image representation — the regression risk when
    /// adding the tab dispatch.
    #[test]
    fn an_image_pair_still_offers_image_compare_from_a_diff_row() {
        let cmp = DiffCompare::new(diff_side(Some("image/jpeg")), diff_side(Some("image/jpeg")));
        let (l, r) = cmp.reps();
        let kinds = crate::lightbox::tab_kinds(&l, Some(&r));
        assert!(
            kinds.contains(&RepresentationKind::Image),
            "images still compare as images, got {kinds:?}"
        );
        // Text/bytes for images is deliberate as of the lightbox redesign — see
        // `an_image_pair_also_offers_text_and_bytes`. This test is about the
        // Image tab surviving, which was the regression risk when tabs arrived.
    }

    /// Audio offers its own representation rather than falling through to Text.
    #[test]
    fn an_audio_pair_offers_the_audio_representation() {
        let cmp = DiffCompare::new(diff_side(Some("audio/mpeg")), diff_side(Some("audio/mpeg")));
        let (l, r) = cmp.reps();
        let kinds = crate::lightbox::tab_kinds(&l, Some(&r));
        assert!(
            kinds.contains(&RepresentationKind::Audio),
            "audio is its own representation, got {kinds:?}"
        );
    }

    /// The reported failure: "compare of two audio is completely broken".
    /// DIFF treated previewable as image-or-video, so two MP3s produced
    /// `no preview for audio/mpeg` and could not be compared at all. Audio is now
    /// compared as a spectrogram — a texture like any other — using the same
    /// shared `waveform` rendering the Duplicates player uses.
    #[test]
    fn two_audio_files_can_be_compared_from_a_diff_row() {
        let mp3 = diff_side(Some("audio/mpeg"));
        assert!(
            mp3.previewable(),
            "audio must be comparable, not 'no preview for audio/mpeg'"
        );
        assert!(diff_side(Some("audio/flac")).previewable());
        assert!(diff_side(Some("audio/x-wav")).previewable());

        // A pair of audio sides is a comparable pair on both halves.
        let other = diff_side(Some("audio/mpeg"));
        assert!(
            mp3.previewable() && other.previewable(),
            "both sides yield a visual, so compare is offered for the pair"
        );

        // Still nothing to compare where there genuinely is no visual.
        assert!(!diff_side(Some("application/pdf")).previewable());
    }

    /// A previewable pair whose decode came back empty (e.g. two videos with no
    /// ffmpeg) settles to "no preview" and keeps compare disabled — it must not
    /// spin "decoding…" forever, and CLAUDE.md promises video degrades gracefully.
    #[test]
    fn a_failed_decode_settles_to_no_preview_not_a_stuck_decode() {
        let mut dc = DiffCompare::new(diff_side(Some("video/mp4")), diff_side(Some("video/mp4")));
        // Before decode: both in flight, compare not yet available.
        assert_eq!(dc.slot_state(0), SlotState::Decoding);
        assert!(!dc.compare_ready());
        // Decode came back with no texture on both sides.
        dc.settled = [true, true];
        assert_eq!(dc.slot_state(0), SlotState::NoPreview);
        assert_eq!(dc.slot_state(1), SlotState::NoPreview);
        assert!(
            !dc.compare_ready(),
            "no textures ⇒ compare stays disabled, panes show 'no preview'"
        );
    }
}
