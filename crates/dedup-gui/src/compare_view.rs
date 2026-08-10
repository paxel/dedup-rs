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
    ComparePointer, CompareState, FileRepresentations, RepresentationKind, compare_split,
    draw_columns, draw_compare, draw_in_pane, draw_tab_bar, draw_text_column, load_text_preview,
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

/// Height of the Video tab's filmstrip row.
const FILMSTRIP_H: f32 = 72.0;

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

    /// Whether this side's file is actually on disk right now. A repo on an
    /// unmounted drive (a closed pcloud, an ejected disk) still has index
    /// entries, but the paths 404 — so guard before decode/preview/open. This
    /// does a filesystem `stat`, so it is probed **once** (at decode time and
    /// cached), never per render frame — a `stat` on a hung mount would freeze
    /// the UI thread every frame.
    pub fn present(&self) -> bool {
        self.facts.abs_path.exists()
    }

    /// What to say when there is no picture to show — the type this pane can't
    /// preview (a document on the Image tab). The distinct "file is gone" note
    /// is chosen by the viewer from a cached presence flag, not here, so this
    /// never touches the filesystem.
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
    /// This side's file was rewritten in place (a turned image saved over the
    /// original). The caller must refresh its index entry — the save may have
    /// deliberately preserved the file's timestamp, which a rescan's
    /// (size, mtime) skip would never notice. Does not close the viewer.
    Edited { on_left: bool },
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

/// Video-side media arriving from worker threads: filmstrip stills, the
/// extracted soundtrack (plus its spectrogram), scrubbed frames, and finished
/// pitch-preserving rate renders.
enum MediaMsg {
    /// One decoded filmstrip still.
    FilmFrame {
        left: bool,
        idx: usize,
        image: ColorImage,
    },
    /// The side's probed duration, sent once before its stills.
    FilmMeta {
        left: bool,
        duration_secs: Option<f64>,
    },
    /// The side's soundtrack: the cached WAV and its length once extracted
    /// (`None` = no track / no ffmpeg), plus the spectrogram when it decoded.
    Soundtrack {
        left: bool,
        wav: Option<(std::path::PathBuf, u64)>,
        spec: Option<ColorImage>,
    },
    /// The frame decoded at a playhead position. `fraction` identifies which
    /// click it answers, so a stale decode never overwrites a newer one.
    ScrubFrame {
        left: bool,
        fraction: f32,
        image: Option<ColorImage>,
    },
    /// An `atempo` rate render finished (or failed) for this cache path.
    RateWav { path: std::path::PathBuf, ok: bool },
    /// A side's document rasterized to its first page (`None` = it couldn't be
    /// rendered). `path` is the source it was rendered from, so a result that
    /// arrives after the side was swapped is dropped instead of shown.
    RenderPage {
        left: bool,
        path: std::path::PathBuf,
        image: Option<ColorImage>,
    },
}

/// Everything the viewer holds for one *video* side: the filmstrip, the
/// extracted soundtrack, and the playhead-scrubbed frame. Reset whenever the
/// side is pointed at another file.
#[derive(Default)]
struct VideoSideState {
    /// Filmstrip textures by slot; `None` while that still decodes (or when it
    /// never will — no ffmpeg — which draws as a placeholder slot).
    film: Vec<Option<TextureHandle>>,
    /// The clip's probed duration, for the playhead's fraction→timestamp map.
    duration_secs: Option<f64>,
    /// Soundtrack: `None` = still probing; `Some(None)` = no track (or no
    /// ffmpeg); `Some(Some((wav, ms)))` = extracted and ready to play.
    audio: Option<Option<(std::path::PathBuf, u64)>>,
    /// Spectrogram of the extracted soundtrack — the Audio tab's texture.
    spec_tex: Option<TextureHandle>,
    /// The soundtrack worker has reported (successfully or not).
    spec_settled: bool,
    /// The decoded frame at the current playhead, if any.
    scrub_tex: Option<TextureHandle>,
    /// The playhead fraction `scrub_tex` (or the decode in flight) answers.
    scrub_frac: Option<f32>,
    /// The scrub decode has reported (successfully or not).
    scrub_settled: bool,
}

/// A transport action deferred until its audio sources finish rendering (a
/// video soundtrack still extracting, an `atempo` rate render in flight).
/// Positions travel as a fraction so re-anchoring works across rate changes,
/// where the rendered runtimes differ.
#[derive(Clone, Copy, Debug, PartialEq)]
enum PendingPlay {
    Pair {
        audible_b: bool,
        fraction: f32,
        paused: bool,
    },
    Single {
        left: bool,
        fraction: f32,
        paused: bool,
    },
}

/// What the transport can play for one side right now, at the chosen rate.
#[derive(Clone, Debug, PartialEq)]
enum AudioSrc {
    /// A playable file: the track itself, a video's extracted soundtrack, or
    /// the pitch-preserving rate render of either.
    Ready {
        hex: String,
        path: std::path::PathBuf,
        total_ms: u64,
    },
    /// Still extracting or rendering — playable soon.
    Rendering,
    /// This side has no sound to play.
    None,
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
    /// Whether each side's file was on disk at decode time — probed once (a
    /// `stat`), never per frame, so a hung/disconnected mount can't freeze the
    /// UI. Drives the "this file isn't present" note.
    present: [bool; 2],
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
    /// Full EXIF field list per side, read once from the file — the Metadata
    /// tab shows everything, not only the camera and date the index keeps.
    exif_all: [Option<Vec<(String, String)>>; 2],
    /// Per-side deletion-mark state, when the caller's actions are marks rather
    /// than the DIFF board's commands. Set each frame via [`Self::set_marks`] —
    /// the caller owns the marks, the viewer only shows and reports them.
    marks: [Option<MarkPill>; 2],
    /// Which side's copy is audible (0 = A, 1 = B) once playback was started
    /// from this viewer — the side the playback cursor belongs to. Exact
    /// duplicates share a content hash, so the hash alone cannot say.
    pub audio_active: Option<usize>,
    /// The Text tab's shared scroll position: both panes are locked to the
    /// same offset, so a byte comparison always looks at the same place.
    text_scroll: egui::Vec2,
    /// The Text tab's aligned hex diff (two-sided), built from the two files'
    /// bytes and cached under their content hashes so it rebuilds only when a
    /// side changes. `hex_page` is the current page within it.
    hexdiff: Option<crate::hexdiff::HexDiff>,
    hexdiff_key: Option<(String, String)>,
    hex_page: usize,
    /// The Text tab's aligned content diff (two documents), built from both
    /// sides' extracted text and cached under their content hashes so it
    /// rebuilds only when a side changes.
    textdiff: Option<crate::textdiff::TextDiff>,
    textdiff_key: Option<(String, String)>,
    /// The Strings tab's per-side printable runs (joined, one per line) and, when
    /// comparing, their aligned diff — cached like the text diff.
    strings: [Option<String>; 2],
    stringsdiff: Option<crate::textdiff::TextDiff>,
    stringsdiff_key: Option<(String, String)>,
    /// The Render tab's first-page texture per side, rasterized once via
    /// `pdftoppm`. `render_tried` guards against re-running the tool every frame
    /// when it fails (missing/unrenderable).
    render_tex: [Option<TextureHandle>; 2],
    render_tried: [bool; 2],
    /// Set while a side's first page is rasterizing on a worker thread, so the
    /// pane shows "Rendering…" instead of a blank wait. Rendering runs off the UI
    /// thread because a large PDF takes seconds (and used to freeze the app).
    render_pending: [bool; 2],
    /// The Hex tab's single-file forced hex dump per side, cached like the text
    /// preview. The two-sided view is the paginated `hexdiff`.
    hex_head: [Option<crate::lightbox::TextPreview>; 2],
    /// The open ID3 tag editor (Metadata tab), if any. Only a writable audio
    /// side ever opens one.
    pub(crate) tag_edit: Option<TagEdit>,
    /// Why the last tag save failed, shown on the Metadata tab until the next
    /// attempt succeeds.
    tag_error: Option<String>,
    /// The caller's own headline for the viewer — what these two files are to
    /// the surface that opened it.
    title: String,
    /// The current pair's perceptual similarity (0–1), when the caller is a
    /// SIMILAR duplicate search — shown as `A ↔ B N%` so a loosely-matched pair
    /// explains itself rather than posing as an exact duplicate. `None` for
    /// exact-duplicate and DIFF callers, which have no similarity score.
    similarity: Option<f32>,
    /// The open save-confirmation dialog for a turned image, if any: which
    /// side is being saved.
    save_confirm: Option<bool>,
    /// In the save dialog: stamp the file's modified time from the EXIF
    /// capture date instead of keeping the original's.
    save_exif_date: bool,
    /// Why the last save failed, shown in the dialog until one succeeds.
    save_error: Option<String>,
    /// The Archive tab's member list for the current left-side archive, loaded
    /// lazily on demand (cheap — names/sizes only). `None` until first shown or
    /// after the left side changes.
    archive_entries: Option<Vec<dedup_core::archive::ArchiveEntry>>,
    /// When viewing a member opened from inside an archive: the archive side to
    /// return to, and the temp dir holding the extracted member (kept alive so
    /// the file isn't deleted while the viewer shows it).
    member_return: Option<Box<DiffSide>>,
    member_tempdir: Option<std::sync::Arc<tempfile::TempDir>>,
    /// Result of the last extraction, shown on the Archive tab.
    extract_status: Option<String>,
    /// The verified password for the current locked archive, held in memory for
    /// this session only (never written here). Once set, locked members open
    /// and extract with it.
    unlock_password: Option<String>,
    /// The unlock text field's buffer, and whether the last try was wrong.
    unlock_input: String,
    unlock_failed: bool,
    /// A background password-recovery attempt in flight: its result channel and
    /// a note (progress / outcome) shown on the Archive tab.
    recover_rx: Option<crossbeam_channel::Receiver<Option<String>>>,
    recover_note: Option<String>,
    /// Per-side video media (filmstrip, soundtrack, scrubbed frame) and the
    /// channel its workers report on.
    video: [VideoSideState; 2],
    media_tx: Sender<MediaMsg>,
    media_rx: Receiver<MediaMsg>,
    /// The shared proportional playhead across both filmstrips — a fraction of
    /// each side's *own* duration, so unequal clips stay aligned at the same
    /// relative moment.
    playhead: Option<f32>,
    /// The filmstrip slot rects laid out this frame, per side — the Video
    /// tab's testable geometry.
    film_rects: [Vec<Rect>; 2],
    /// The transport's play-rate stop (1× default). Non-1× plays the cached
    /// pitch-preserving `atempo` renders at 1×, never rodio's pitch-shifting
    /// `set_speed`.
    rate: f32,
    /// The rate the currently loaded playback was started at — part of the
    /// pair's identity, since the same hexes at another rate are other files.
    loaded_rate: f32,
    /// Rate renders confirmed on disk / in flight, so the per-frame source
    /// resolution neither re-stats nor re-spawns.
    rate_ready: std::collections::HashSet<std::path::PathBuf>,
    rate_pending: std::collections::HashSet<std::path::PathBuf>,
    /// A transport action waiting for its sources to finish rendering.
    pending_play: Option<PendingPlay>,
}

impl DiffCompare {
    pub fn new(left: DiffSide, right: DiffSide) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        let (media_tx, media_rx) = crossbeam_channel::unbounded();
        let compare = CompareState::new(right.facts.clone());
        Self {
            left,
            right,
            compare,
            tex: [None, None],
            settled: [false, false],
            present: [true, true],
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
            exif_all: [None, None],
            marks: [None, None],
            audio_active: None,
            text_scroll: egui::Vec2::ZERO,
            hexdiff: None,
            hexdiff_key: None,
            hex_page: 0,
            textdiff: None,
            textdiff_key: None,
            strings: [None, None],
            stringsdiff: None,
            stringsdiff_key: None,
            render_tex: [None, None],
            render_tried: [false, false],
            render_pending: [false, false],
            hex_head: [None, None],
            tag_edit: None,
            tag_error: None,
            title: "COMPARE — SAME PATH, DIFFERENT CONTENT".into(),
            similarity: None,
            save_confirm: None,
            save_exif_date: false,
            save_error: None,
            archive_entries: None,
            member_return: None,
            member_tempdir: None,
            extract_status: None,
            unlock_password: None,
            unlock_input: String::new(),
            unlock_failed: false,
            recover_rx: None,
            recover_note: None,
            video: [VideoSideState::default(), VideoSideState::default()],
            media_tx,
            media_rx,
            playhead: None,
            film_rects: [Vec::new(), Vec::new()],
            rate: 1.0,
            loaded_rate: 1.0,
            rate_ready: std::collections::HashSet::new(),
            rate_pending: std::collections::HashSet::new(),
            pending_play: None,
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

    /// Supply the current pair's perceptual similarity (0–1) for a SIMILAR
    /// search, shown as `A ↔ B N%`. `None` clears it (exact / DIFF have none).
    pub fn set_similarity(&mut self, similarity: Option<f32>) {
        self.similarity = similarity;
    }

    /// The content hashes of the two files currently shown side by side, or
    /// `None` when only one side is visible — so a caller can score the pair
    /// and feed it back via [`Self::set_similarity`].
    pub fn shown_pair(&self) -> Option<(String, String)> {
        (self.two_sided() && self.left.facts.abs_path != self.right.facts.abs_path).then(|| {
            (
                self.left.facts.hash_hex.clone(),
                self.right.facts.hash_hex.clone(),
            )
        })
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
    /// distinct copies. With the second side hidden, `other` is only a
    /// placeholder and masks nothing — the whole pool is free.
    fn has_free_candidate(&self, from: &DiffSide, other: &DiffSide) -> bool {
        let mask = self.two_sided();
        self.pool.iter().any(|c| {
            c.facts.abs_path != from.facts.abs_path
                && (!mask || c.facts.abs_path != other.facts.abs_path)
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
        let mask = self.two_sided();
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
            if !mask || candidate.facts.abs_path != other.facts.abs_path {
                return Some(candidate.clone());
            }
        }
        None
    }

    /// The representations each side offers, for the tab bar and the dispatch.
    /// Marks belong to the caller; the only write a side can carry is its ID3
    /// tags, and only when its repository is writable.
    pub fn reps(&self) -> (FileRepresentations, FileRepresentations) {
        let make = |side: &DiffSide, vid: &VideoSideState| {
            let mut reps = FileRepresentations::from_facts(
                &side.facts,
                side.repo.clone(),
                side.read_only,
                crate::lightbox::MarkState::Protected,
            );
            if side.facts.is_video() {
                // The clip's extracted soundtrack: the Audio representation
                // appears exactly when a track is present (probed off-thread;
                // no ffmpeg / silent clip → never offered).
                if let Some(Some((_, ms))) = &vid.audio {
                    reps.audio = Some(crate::lightbox::AudioRepresentation {
                        duration_ms: u32::try_from(*ms).ok(),
                        spectrogram_texture: vid.spec_tex.clone(),
                        is_playing: false,
                        seek_position_ms: 0,
                        can_play: true,
                    });
                }
                if let Some(v) = reps.video.as_mut() {
                    v.filmstrip_textures = vid.film.iter().flatten().cloned().collect();
                    v.selected_frame = self
                        .playhead
                        .map(|f| crate::scrub::filmstrip_slot(f, crate::scrub::FILMSTRIP_FRAMES));
                    if let Some(d) = vid.duration_secs {
                        v.duration_ms = Some((d * 1000.0) as u32);
                    }
                }
            }
            reps
        };
        (
            make(&self.left, &self.video[0]),
            make(&self.right, &self.video[1]),
        )
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

    /// Write one side's metadata to a human-readable sidecar in a folder the user
    /// picks — salvaging it before a copy is deleted. A new file, so it is
    /// allowed even for a locked repo (it only adds). Cancelling the folder
    /// picker is a no-op.
    fn export_metadata(&mut self, is_left: bool) {
        let slot = usize::from(!is_left);
        let (abs, rel) = {
            let side = if is_left { &self.left } else { &self.right };
            (side.facts.abs_path.clone(), side.rel_path.clone())
        };
        let fields = self.exif_all[slot]
            .clone()
            .unwrap_or_else(|| dedup_core::fingerprint::exif_fields(&abs));
        let base = std::path::Path::new(&rel)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "metadata".to_string());
        let Some(dir) = rfd::FileDialog::new().pick_folder() else {
            return;
        };
        let text = metadata_sidecar(&base, &fields);
        let dest = non_colliding_sidecar(&dir, &format!("{base}.metadata.txt"));
        match std::fs::write(&dest, text) {
            Ok(()) => self.tag_error = None,
            Err(e) => self.tag_error = Some(format!("Metadata export failed: {e}")),
        }
    }

    /// Render the Text tab's aligned content diff for two documents: build (and
    /// cache under the two content hashes) the line-aligned diff of both sides'
    /// extracted text, then show it. No verdict is drawn — two identical
    /// documents simply show no marks.
    fn render_text_diff(&mut self, ui: &mut egui::Ui) {
        for slot in [0usize, 1usize] {
            if self.text[slot].is_none() {
                let side = if slot == 0 { &self.left } else { &self.right };
                self.text[slot] = Some(document_preview(&side.facts));
            }
        }
        let key = (
            self.left.facts.hash_hex.clone(),
            self.right.facts.hash_hex.clone(),
        );
        if self.textdiff_key.as_ref() != Some(&key) {
            let td = {
                let a = self.text[0]
                    .as_ref()
                    .map(|t| t.body.as_str())
                    .unwrap_or_default();
                let b = self.text[1]
                    .as_ref()
                    .map(|t| t.body.as_str())
                    .unwrap_or_default();
                crate::textdiff::TextDiff::build(a, b)
            };
            self.textdiff = Some(td);
            self.textdiff_key = Some(key);
        }
        if let Some(td) = &self.textdiff {
            td.show(ui);
        }
    }

    /// Render the Strings tab: the printable runs embedded in each file's bytes,
    /// one file's runs scrollable, or — comparing two — their runs aligned as a
    /// content diff (green/amber). No verdict.
    fn render_strings(&mut self, ui: &mut egui::Ui, two_sided: bool) {
        for slot in [0usize, 1usize] {
            if self.strings[slot].is_none() {
                let side = if slot == 0 { &self.left } else { &self.right };
                self.strings[slot] = Some(strings_body(&side.facts));
            }
        }
        if two_sided {
            let key = (
                self.left.facts.hash_hex.clone(),
                self.right.facts.hash_hex.clone(),
            );
            if self.stringsdiff_key.as_ref() != Some(&key) {
                let td = {
                    let a = self.strings[0].as_deref().unwrap_or_default();
                    let b = self.strings[1].as_deref().unwrap_or_default();
                    crate::textdiff::TextDiff::build(a, b)
                };
                self.stringsdiff = Some(td);
                self.stringsdiff_key = Some(key);
            }
            if let Some(td) = &self.stringsdiff {
                td.show(ui);
            }
        } else if let Some(body) = &self.strings[0] {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(body)
                                .monospace()
                                .size(12.0)
                                .color(theme::text()),
                        )
                        .wrap(),
                    );
                });
        }
    }

    /// Start rasterizing a side's document to its first page, once. `pdftoppm`
    /// runs on a **worker thread** — a large PDF takes seconds to render, and
    /// doing it inline froze the whole app with no feedback — sending the page
    /// back as [`MediaMsg::RenderPage`]. `render_tried` guards against re-spawning
    /// every frame; `render_pending` drives the "Rendering…" note until it lands.
    fn ensure_render(&mut self, ctx: &Context, slot: usize) {
        if self.render_tried[slot] {
            return;
        }
        self.render_tried[slot] = true;
        self.render_pending[slot] = true;
        let left = slot == 0;
        let path = if left {
            self.left.facts.abs_path.clone()
        } else {
            self.right.facts.abs_path.clone()
        };
        let tx = self.media_tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let image = (|| {
                let dir = tempfile::tempdir().ok()?;
                let first = dedup_core::render::render_pdf_first_page(&path, dir.path())?;
                let (w, h, rgba) = dedup_core::thumbnail::load_full_rgba(&first, 2000).ok()?;
                Some(ColorImage::from_rgba_unmultiplied(
                    [w as usize, h as usize],
                    &rgba,
                ))
            })();
            let _ = tx.send(MediaMsg::RenderPage { left, path, image });
            ctx.request_repaint();
        });
    }

    /// Draw one side's rendered page fit within `area`; a "Rendering…" note while
    /// the worker runs, or a "couldn't be drawn" note when it finished empty.
    fn draw_render_page(&self, ui: &mut egui::Ui, area: Rect, slot: usize) {
        if let Some(tex) = &self.render_tex[slot] {
            let sz = tex.size();
            let (w, h) = (sz[0] as f32, sz[1] as f32);
            let scale = (area.width() / w).min(area.height() / h).min(4.0);
            let fit = Rect::from_center_size(area.center(), egui::vec2(w * scale, h * scale));
            draw_in_pane(ui, area, fit, &self.render_tex[slot]);
            return;
        }
        let (note, color) = if self.render_pending[slot] {
            // Keep painting until the page lands (the worker also requests a
            // repaint, but a dropped wake-up shouldn't strand the note).
            ui.ctx().request_repaint();
            (
                "Rendering the first page… a large document can take a moment.".to_string(),
                theme::tan(),
            )
        } else {
            (
                "This document could not be drawn to pages — the page renderer \
                 (poppler) may be missing."
                    .to_string(),
                theme::hairline(),
            )
        };
        ui.painter().text(
            area.center(),
            Align2::CENTER_CENTER,
            note,
            FontId::proportional(13.0),
            color,
        );
    }

    /// Render the Text tab's aligned hex diff for a two-sided comparison: build
    /// (and cache under the two content hashes) the alignment, then a row of
    /// controls — page navigation, jump-to-difference, and any degrade or
    /// truncation notice — over the current page.
    fn render_hex_diff(&mut self, ui: &mut egui::Ui) {
        let key = (
            self.left.facts.hash_hex.clone(),
            self.right.facts.hash_hex.clone(),
        );
        if self.hexdiff_key.as_ref() != Some(&key) {
            let a = crate::hexdiff::read_capped(&self.left.facts.abs_path);
            let b = crate::hexdiff::read_capped(&self.right.facts.abs_path);
            self.hexdiff = Some(crate::hexdiff::HexDiff::build(
                &a.bytes,
                &b.bytes,
                a.truncated || b.truncated,
                !a.read_ok || !b.read_ok,
            ));
            self.hexdiff_key = Some(key);
            self.hex_page = 0;
        }
        let (pages, identical, degraded, truncated, read_failed) = match &self.hexdiff {
            Some(d) => (
                d.pages(),
                d.is_identical(),
                d.degraded,
                d.truncated,
                d.read_failed,
            ),
            None => return,
        };
        self.hex_page = self.hex_page.min(pages.saturating_sub(1));

        let mut go: Option<usize> = None;
        let mut jump_next = false;
        let mut jump_prev = false;
        // Where the viewer's A|B divider crosses this pane: the page controls
        // stay in the left half and the diff jumps start in the right half, so
        // the divider passes between the two groups, never through a button.
        let mid_x = ui.max_rect().center().x;
        ui.horizontal(|ui| {
            if ui.button("< PREV PAGE").clicked() {
                go = Some(self.hex_page.saturating_sub(1));
            }
            // The page number is a control, not just a caption: type or drag it
            // to land on an exact page.
            let mut page1 = self.hex_page + 1;
            ui.label(RichText::new("page").color(theme::tan()));
            let typed = ui
                .add(egui::DragValue::new(&mut page1).range(1..=pages))
                .on_hover_text("Type or drag to jump straight to a page.");
            ui.label(RichText::new(format!("/ {pages}")).color(theme::tan()));
            if typed.changed() {
                go = Some(page1.saturating_sub(1));
            }
            if ui.button("NEXT PAGE >").clicked() && self.hex_page + 1 < pages {
                go = Some(self.hex_page + 1);
            }
            // A slider for fast, coarse scrolling through a big file — dragging
            // it sweeps the pages far quicker than stepping.
            if pages > 1 {
                ui.add_space(8.0);
                ui.spacing_mut().slider_width = 160.0;
                let slid = ui
                    .add(egui::Slider::new(&mut page1, 1..=pages).show_value(false))
                    .on_hover_text("Drag to sweep quickly through the whole file.");
                if slid.changed() {
                    go = Some(page1.saturating_sub(1));
                }
            }
            // Into the right half (unless a narrow window already pushed the
            // cursor past it — then just flow on).
            let cur = ui.cursor().min.x;
            ui.add_space((mid_x + 8.0 - cur).max(12.0));
            if identical {
                ui.label(RichText::new("The shown bytes are identical.").color(theme::green()));
            } else {
                if ui
                    .button(format!("{} PREV DIFF", crate::icon::CARET_LEFT))
                    .clicked()
                {
                    jump_prev = true;
                }
                if ui
                    .button(format!("NEXT DIFF {}", crate::icon::CARET_RIGHT))
                    .clicked()
                {
                    jump_next = true;
                }
            }
        });
        if degraded {
            ui.label(
                RichText::new("File too large for exact alignment — showing a block-level match.")
                    .color(theme::red())
                    .size(11.0),
            );
        }
        if truncated {
            ui.label(
                RichText::new("Large file — only the first part is shown.")
                    .color(theme::tan())
                    .size(11.0),
            );
        }
        if read_failed {
            ui.label(
                RichText::new(
                    "A file could not be read — the diff below treats it as empty and may be \
                     misleading.",
                )
                .color(theme::red())
                .size(11.0),
            );
        }
        if let Some(d) = &self.hexdiff {
            if jump_next {
                go = d.next_diff_page(self.hex_page);
            }
            if jump_prev {
                go = d.prev_diff_page(self.hex_page);
            }
        }
        if let Some(p) = go {
            self.hex_page = p;
        }
        if let Some(d) = &self.hexdiff {
            d.show_page(ui, self.hex_page);
        }
    }

    /// What one side's pane should draw right now: its decoded image, an
    /// in-flight "decoding…" note, or a settled "no preview" note. A side is
    /// `NoPreview` both when it can never have a visual (a document) and when its
    /// decode came back empty (e.g. a video with no ffmpeg) — settled with no
    /// texture. This is what keeps a failed decode from spinning "decoding…"
    /// forever (the distinction the old two-pane `settled[]` flags carried).
    fn slot_state(&self, slot: usize) -> SlotState {
        if self.tab_tex(slot).is_some() {
            SlotState::Image
        } else if self.tab_settled(slot) {
            SlotState::NoPreview
        } else {
            SlotState::Decoding
        }
    }

    /// The note for a pane with nothing to show. A file that was gone at decode
    /// time (cached presence, never re-statted) reads as "isn't present"; every
    /// other case names the type this pane can't preview.
    fn no_preview_text(&self, slot: usize) -> String {
        if !self.present[slot] {
            return "This file isn't present — its drive may be disconnected.".to_string();
        }
        let side = if slot == 0 { &self.left } else { &self.right };
        side.placeholder()
    }

    /// The texture behind `slot` on the *current tab*. The Audio tab of a
    /// video side compares the soundtrack's spectrogram; every other case uses
    /// the side's primary texture (image, audio-file spectrogram).
    fn tab_tex(&self, slot: usize) -> Option<&TextureHandle> {
        if self.spec_mode(slot) {
            self.video[slot].spec_tex.as_ref()
        } else {
            self.tex[slot].as_ref()
        }
    }

    /// Whether `slot`'s decode for the current tab has come back (successfully
    /// or not) — the tab-aware counterpart of `settled`.
    fn tab_settled(&self, slot: usize) -> bool {
        if self.spec_mode(slot) {
            self.video[slot].spec_settled
        } else {
            self.settled[slot]
        }
    }

    /// This slot shows a video's extracted-soundtrack spectrogram right now.
    fn spec_mode(&self, slot: usize) -> bool {
        let side = if slot == 0 { &self.left } else { &self.right };
        self.tab == RepresentationKind::Audio && side.facts.is_video()
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
        // A file whose drive has gone (unmounted mount, ejected disk) has no
        // bytes to decode — settle immediately so the pane shows the "not
        // present" note instead of spinning a decode that will only fail. Probe
        // presence once here and cache it; the render path must never `stat`.
        let present = side.present();
        self.present[slot] = present;
        if !present {
            self.settled[slot] = true;
            return;
        }
        let side = if is_left { &self.left } else { &self.right };
        if side.facts.is_video() {
            // A video has no single primary still any more — the Video tab
            // draws the filmstrip, the Audio tab the extracted soundtrack's
            // spectrogram, each produced by its own workers.
            self.settled[slot] = true;
            self.spawn_video_side(ctx, slot);
            return;
        }
        if !side.previewable() {
            self.settled[slot] = true;
            return;
        }
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        let path = side.facts.abs_path.clone();
        let audio = side.facts.is_audio();
        std::thread::spawn(move || {
            // Audio has no frame to show, so it is compared as a spectrogram
            // — the same rendering the Duplicates player uses, from the same
            // shared `waveform` module rather than a second implementation.
            let image = if audio {
                crate::waveform::spec_rgba(&path)
            } else {
                dedup_core::thumbnail::load_full_rgba(&path, MAX_TEXTURE_EDGE)
                    .ok()
                    .map(|(w, h, rgba)| {
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

    /// Start a video side's workers: the filmstrip stills (sent one by one, so
    /// the strip fills in as they decode) and the soundtrack probe → extract →
    /// spectrogram chain. Both degrade to nothing without ffmpeg — placeholder
    /// slots, no Audio tab — never an error.
    fn spawn_video_side(&mut self, ctx: &Context, slot: usize) {
        let is_left = slot == 0;
        let side = if is_left { &self.left } else { &self.right };
        let n = crate::scrub::FILMSTRIP_FRAMES;
        self.video[slot] = VideoSideState {
            film: vec![None; n],
            ..VideoSideState::default()
        };

        let tx = self.media_tx.clone();
        let ctx2 = ctx.clone();
        let path = side.facts.abs_path.clone();
        let hex = side.facts.hash_hex.clone();
        std::thread::spawn(move || {
            let duration_secs = dedup_core::fingerprint::media_duration_secs(&path);
            let _ = tx.send(MediaMsg::FilmMeta {
                left: is_left,
                duration_secs,
            });
            for idx in 0..n {
                if let Ok((w, h, rgba)) =
                    dedup_core::thumbnail::video_frame_rgba(&path, &hex, idx, n)
                {
                    let image = ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
                    let _ = tx.send(MediaMsg::FilmFrame {
                        left: is_left,
                        idx,
                        image,
                    });
                    ctx2.request_repaint();
                }
            }
            ctx2.request_repaint();
        });

        let tx = self.media_tx.clone();
        let ctx2 = ctx.clone();
        let path = side.facts.abs_path.clone();
        let hex = side.facts.hash_hex.clone();
        std::thread::spawn(move || {
            let wav = dedup_core::fingerprint::has_audio_track(&path)
                .then(|| dedup_core::thumbnail::ensure_video_audio(&path, &hex).ok())
                .flatten();
            let msg = match wav {
                Some(wav) => {
                    let ms = dedup_core::fingerprint::media_duration_secs(&wav)
                        .map(|s| (s * 1000.0) as u64)
                        .unwrap_or(0);
                    let spec = crate::waveform::spec_rgba(&wav);
                    MediaMsg::Soundtrack {
                        left: is_left,
                        wav: Some((wav, ms)),
                        spec,
                    }
                }
                None => MediaMsg::Soundtrack {
                    left: is_left,
                    wav: None,
                    spec: None,
                },
            };
            let _ = tx.send(msg);
            ctx2.request_repaint();
        });

        // A playhead already dropped keeps pointing at the same relative
        // moment of whatever file the side now shows.
        if let Some(f) = self.playhead {
            self.spawn_scrub(ctx, slot, f);
        }
    }

    /// Decode the frame at playhead `fraction` of this side's own timeline,
    /// off the UI thread (the `spawn_decode` pattern). The result is shown
    /// enlarged as one half of `A@t | B@t`.
    fn spawn_scrub(&mut self, ctx: &Context, slot: usize, fraction: f32) {
        let is_left = slot == 0;
        let side = if is_left { &self.left } else { &self.right };
        if !side.facts.is_video() {
            return;
        }
        let st = &mut self.video[slot];
        st.scrub_frac = Some(fraction);
        st.scrub_tex = None;
        st.scrub_settled = false;
        let known_duration = st.duration_secs;
        let tx = self.media_tx.clone();
        let ctx2 = ctx.clone();
        let path = side.facts.abs_path.clone();
        std::thread::spawn(move || {
            let duration =
                known_duration.or_else(|| dedup_core::fingerprint::media_duration_secs(&path));
            let image = duration
                .and_then(|d| {
                    let at = crate::scrub::scrub_timestamp_secs(fraction, d);
                    dedup_core::fingerprint::video_frame(&path, at)
                })
                .map(|img| {
                    let rgba = img.to_rgba8();
                    ColorImage::from_rgba_unmultiplied(
                        [rgba.width() as usize, rgba.height() as usize],
                        rgba.as_raw(),
                    )
                });
            let _ = tx.send(MediaMsg::ScrubFrame {
                left: is_left,
                fraction,
                image,
            });
            ctx2.request_repaint();
        });
    }

    /// One side of the Video tab: the filmstrip across the top of `pane`, the
    /// shared playhead marker over it, and the enlarged frame at the playhead
    /// below. Returns the fraction clicked this frame, if any. A side that is
    /// no video (a mixed pair) keeps its "no preview" note instead.
    fn draw_video_side(
        &mut self,
        ui: &mut egui::Ui,
        pane: Rect,
        slot: usize,
        verbosity: TooltipVerbosity,
    ) -> Option<f32> {
        let side = if slot == 0 { &self.left } else { &self.right };
        if !side.facts.is_video() {
            self.film_rects[slot].clear();
            ui.painter().text(
                pane.center(),
                Align2::CENTER_CENTER,
                self.no_preview_text(slot),
                FontId::proportional(14.0),
                theme::tan(),
            );
            return None;
        }
        let n = crate::scrub::FILMSTRIP_FRAMES;
        let strip = Rect::from_min_size(pane.min, egui::vec2(pane.width(), FILMSTRIP_H));
        let slot_w = strip.width() / n as f32;
        let uv = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
        let mut rects = Vec::with_capacity(n);
        for idx in 0..n {
            let r = Rect::from_min_size(
                egui::pos2(strip.min.x + idx as f32 * slot_w, strip.min.y),
                egui::vec2(slot_w, FILMSTRIP_H),
            )
            .shrink(1.0);
            rects.push(r);
            match self.video[slot].film.get(idx).and_then(Option::as_ref) {
                Some(tex) => {
                    let fitted = crate::lightbox::fit_rect(r, tex.size_vec2());
                    ui.painter_at(r)
                        .image(tex.id(), fitted, uv, egui::Color32::WHITE);
                }
                None => {
                    // Still decoding — or never will (no ffmpeg): a quiet
                    // placeholder slot, not an error.
                    ui.painter().rect_filled(r, 2.0, theme::panel());
                }
            }
        }
        self.film_rects[slot] = rects;
        // The shared playhead: one fraction, each side's own timeline.
        if let Some(f) = self.playhead {
            let x = strip.min.x + f.clamp(0.0, 1.0) * strip.width();
            ui.painter().line_segment(
                [egui::pos2(x, strip.min.y), egui::pos2(x, strip.max.y)],
                egui::Stroke::new(2.0, theme::amber()),
            );
        }
        let resp = ui
            .interact(
                strip,
                ui.id().with(("video-filmstrip", slot)),
                egui::Sense::click(),
            )
            .explain(
                verbosity,
                "Inspect a moment",
                "Click a spot on the filmstrip to see the frame at that moment of both \
                 clips, enlarged below. The spot is a fraction of each clip's own length, \
                 so copies of different length stay aligned.",
            );
        let clicked = resp
            .clicked()
            .then(|| resp.interact_pointer_pos())
            .flatten()
            .map(|p| ((p.x - strip.min.x) / strip.width()).clamp(0.0, 1.0));

        // The enlarged frame at the playhead, in the space below the strip.
        let below = Rect::from_min_max(egui::pos2(pane.min.x, strip.max.y + 6.0), pane.max);
        if below.height() < 20.0 {
            return clicked;
        }
        match self.playhead {
            None => {
                ui.painter().text(
                    below.center(),
                    Align2::CENTER_CENTER,
                    "Click the filmstrip to inspect a moment",
                    FontId::proportional(13.0),
                    theme::hairline(),
                );
            }
            Some(f) => {
                let st = &self.video[slot];
                match (&st.scrub_tex, st.scrub_settled) {
                    (Some(tex), _) => {
                        let fitted = crate::lightbox::fit_rect(below, tex.size_vec2());
                        draw_in_pane(ui, below, fitted, &Some(tex.clone()));
                    }
                    (None, false) => {
                        ui.painter().text(
                            below.center(),
                            Align2::CENTER_CENTER,
                            "decoding…",
                            FontId::proportional(14.0),
                            theme::tan(),
                        );
                    }
                    (None, true) => {
                        ui.painter().text(
                            below.center(),
                            Align2::CENTER_CENTER,
                            "No frame could be read at this moment",
                            FontId::proportional(13.0),
                            theme::tan(),
                        );
                    }
                }
                // `A @ 1:00` — the moment in this clip's own timeline.
                let tag = if slot == 0 { "A" } else { "B" };
                let label = match self.video[slot].duration_secs {
                    Some(d) => format!(
                        "{tag} @ {}",
                        crate::scrub::format_secs(crate::scrub::scrub_timestamp_secs(f, d))
                    ),
                    None => tag.to_string(),
                };
                ui.painter().text(
                    below.min + egui::vec2(6.0, 6.0),
                    Align2::LEFT_TOP,
                    label,
                    FontId::proportional(16.0),
                    theme::amber(),
                );
            }
        }
        clicked
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
        self.strings[slot] = None;
        self.render_tex[slot] = None;
        self.render_tried[slot] = false;
        self.render_pending[slot] = false;
        self.hex_head[slot] = None;
        self.tags[slot] = None;
        self.exif_all[slot] = None;
        // The video media behind this side is stale too; a deferred transport
        // action would play the file this side no longer shows.
        self.video[slot] = VideoSideState::default();
        self.pending_play = None;
        // The Archive tab lists the *left* side's members; a changed left side
        // invalidates that list and its extraction status.
        if slot == 0 {
            self.archive_entries = None;
            self.extract_status = None;
            self.unlock_password = None;
            self.unlock_input.clear();
            self.unlock_failed = false;
            self.recover_rx = None;
            self.recover_note = None;
        }
        self.spawn_decode(ctx, slot);
    }

    /// Open a member of the current left-side archive: extract it to a temp dir
    /// and show it in place (rendered by its own type), remembering the archive
    /// to return to. Ephemeral — the source archive is never modified, and the
    /// extracted file lives only as long as the viewer shows it. `password`
    /// decrypts a locked member (supplied by the caller's unlock flow).
    fn open_archive_member(
        &mut self,
        ctx: &Context,
        entry: &dedup_core::archive::ArchiveEntry,
        password: Option<&str>,
    ) {
        let archive_name = self
            .left
            .facts
            .abs_path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let Ok(tmp) = tempfile::tempdir() else {
            return;
        };
        let flat = entry
            .name
            .rsplit(['/', '\\'])
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("member");
        let dest = tmp.path().join(flat);
        if dedup_core::archive::extract_member(
            &self.left.facts.abs_path,
            &archive_name,
            self.left.facts.mime.as_deref(),
            &entry.name,
            &dest,
            password,
        )
        .is_err()
        {
            return;
        }
        let size = std::fs::metadata(&dest)
            .map(|m| m.len())
            .unwrap_or(entry.size);
        // Detect the member's type so the viewer dispatches to the right
        // representation (image as image, audio as audio, …).
        let fp = dedup_core::fingerprint::compute(&dest, false);
        let facts = crate::media_cell::FileFacts {
            size,
            modified_ms: 0,
            mime: fp.mime,
            img_size: fp.img_size,
            audio_ms: fp.audio.as_ref().map(|a| a.duration_ms),
            audio_seed: fp
                .audio
                .as_ref()
                .and_then(|a| a.chunk_hashes.first().copied()),
            hash_hex: String::new(),
            abs_path: dest,
            origin: None,
            exif: fp.exif,
        };
        let member_side = DiffSide {
            repo: self.left.repo.clone(),
            rel_path: entry.name.clone(),
            read_only: true,
            facts,
        };
        let archive_side = std::mem::replace(&mut self.left, member_side);
        self.member_return = Some(Box::new(archive_side));
        self.member_tempdir = Some(std::sync::Arc::new(tmp));
        // Let the landing-tab logic pick the member's own representation.
        self.tab = RepresentationKind::Overview;
        self.refresh_side(ctx, 0);
    }

    /// Go back from an opened member to the archive's member list.
    fn return_to_archive(&mut self, ctx: &Context) {
        if let Some(archive) = self.member_return.take() {
            self.left = *archive;
            self.member_tempdir = None;
            self.tab = RepresentationKind::Archive;
            self.refresh_side(ctx, 0);
        }
    }

    /// Extract the whole archive (`member` = `None`) or one member into a folder
    /// the user picks. Extract into a repository to have the contents indexed on
    /// the next scan. The source archive is never modified; collisions never
    /// overwrite. Records a status line for the Archive tab.
    fn extract_to_picked_folder(&mut self, member: Option<dedup_core::archive::ArchiveEntry>) {
        let Some(dir) = rfd::FileDialog::new().pick_folder() else {
            return;
        };
        let f = &self.left.facts;
        let name = f
            .abs_path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mime = f.mime.clone();
        let archive_path = f.abs_path.clone();
        let pw = self.unlock_password.clone();
        self.extract_status = Some(match member {
            None => match dedup_core::archive::extract_all(
                &archive_path,
                &name,
                mime.as_deref(),
                &dir,
                pw.as_deref(),
            ) {
                Ok(n) => format!("Extracted {n} member(s) to {}", dir.display()),
                Err(e) => format!("Extract failed: {e}"),
            },
            Some(entry) => {
                let flat = entry
                    .name
                    .rsplit(['/', '\\'])
                    .next()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("member");
                let dest = dir.join(flat);
                let dest = if dest.exists() {
                    // Never overwrite: fall back to a temp-style suffixed name.
                    let stem = std::path::Path::new(flat)
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "member".into());
                    let ext = std::path::Path::new(flat)
                        .extension()
                        .map(|e| format!(".{}", e.to_string_lossy()))
                        .unwrap_or_default();
                    let mut i = 1;
                    loop {
                        let c = dir.join(format!("{stem}_{i}{ext}"));
                        if !c.exists() {
                            break c;
                        }
                        i += 1;
                    }
                } else {
                    dest
                };
                match dedup_core::archive::extract_member(
                    &archive_path,
                    &name,
                    mime.as_deref(),
                    &entry.name,
                    &dest,
                    pw.as_deref(),
                ) {
                    Ok(()) => format!("Extracted {} to {}", entry.name, dir.display()),
                    Err(e) => format!("Extract failed: {e}"),
                }
            }
        });
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
        while let Ok(msg) = self.media_rx.try_recv() {
            match msg {
                MediaMsg::FilmFrame { left, idx, image } => {
                    let slot = usize::from(!left);
                    let tex = ctx.load_texture(
                        format!("diff-film-{slot}-{idx}"),
                        image,
                        TextureOptions::LINEAR,
                    );
                    if let Some(f) = self.video[slot].film.get_mut(idx) {
                        *f = Some(tex);
                    }
                }
                MediaMsg::FilmMeta {
                    left,
                    duration_secs,
                } => {
                    self.video[usize::from(!left)].duration_secs = duration_secs;
                }
                MediaMsg::Soundtrack { left, wav, spec } => {
                    let slot = usize::from(!left);
                    let st = &mut self.video[slot];
                    st.audio = Some(wav);
                    st.spec_tex = spec.map(|image| {
                        ctx.load_texture(format!("diff-spec-{slot}"), image, TextureOptions::LINEAR)
                    });
                    st.spec_settled = true;
                }
                MediaMsg::ScrubFrame {
                    left,
                    fraction,
                    image,
                } => {
                    let slot = usize::from(!left);
                    let st = &mut self.video[slot];
                    // A decode for an older playhead position must not
                    // overwrite the one answering the current click.
                    if st.scrub_frac == Some(fraction) {
                        st.scrub_tex = image.map(|image| {
                            ctx.load_texture(
                                format!("diff-scrub-{slot}"),
                                image,
                                TextureOptions::LINEAR,
                            )
                        });
                        st.scrub_settled = true;
                    }
                }
                MediaMsg::RateWav { path, ok } => {
                    self.rate_pending.remove(&path);
                    if ok {
                        self.rate_ready.insert(path);
                    } else {
                        // The render failed (realistically: no ffmpeg). Snap
                        // back to 1× so the transport's label and its sound
                        // agree, and let whatever waited play at normal speed.
                        self.rate = 1.0;
                    }
                }
                MediaMsg::RenderPage { left, path, image } => {
                    let slot = usize::from(!left);
                    let side = if slot == 0 { &self.left } else { &self.right };
                    // A page that finished rendering after the side was swapped
                    // belongs to a file no longer shown — drop it.
                    if side.facts.abs_path != path {
                        continue;
                    }
                    self.render_pending[slot] = false;
                    if let Some(image) = image {
                        self.render_tex[slot] = Some(ctx.load_texture(
                            format!("render{slot}"),
                            image,
                            TextureOptions::LINEAR,
                        ));
                    }
                }
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
        let tex = self.tab_tex(slot).cloned();
        // A video's soundtrack spectrogram has nothing to do with the clip's
        // pixel dimensions — its own texture size is its layout.
        if self.spec_mode(slot) {
            let img = tex
                .as_ref()
                .map(|t| t.size_vec2())
                .unwrap_or(egui::vec2(1.0, 1.0));
            return (tex, img);
        }
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

    /// Whether one side has (or is still resolving) a soundtrack the transport
    /// could play: an audio file, or a video whose track probe hasn't said no.
    fn side_has_sound(&self, slot: usize) -> bool {
        let side = if slot == 0 { &self.left } else { &self.right };
        side.facts.is_audio()
            || (side.facts.is_video() && !matches!(self.video[slot].audio, Some(None)))
    }

    /// Resolve what the transport plays for one side at the current rate stop:
    /// the file itself (audio), its extracted soundtrack (video), or the
    /// pitch-preserving `atempo` pre-render of either when the rate isn't 1×.
    /// A missing rate render is kicked off here, once, off the UI thread.
    fn audio_src(&mut self, ctx: &Context, slot: usize) -> AudioSrc {
        let side = if slot == 0 { &self.left } else { &self.right };
        let hex = side.facts.hash_hex.clone();
        let (path, total_ms) = if side.facts.is_audio() {
            (
                side.facts.abs_path.clone(),
                u64::from(side.facts.audio_ms.unwrap_or(0)),
            )
        } else if side.facts.is_video() {
            match &self.video[slot].audio {
                Some(Some((wav, ms))) => (wav.clone(), *ms),
                Some(None) => return AudioSrc::None,
                None => return AudioSrc::Rendering,
            }
        } else {
            return AudioSrc::None;
        };
        if (self.rate - 1.0).abs() < 0.01 {
            return AudioSrc::Ready {
                hex,
                path,
                total_ms,
            };
        }
        let pct = (self.rate * 100.0).round() as u32;
        let out = dedup_core::thumbnail::rate_wav_path(&hex, pct);
        let total_ms = crate::scrub::rated_total_ms(total_ms, self.rate);
        if self.rate_ready.contains(&out) || out.exists() {
            self.rate_ready.insert(out.clone());
            return AudioSrc::Ready {
                hex,
                path: out,
                total_ms,
            };
        }
        if !self.rate_pending.contains(&out) {
            self.rate_pending.insert(out.clone());
            let tx = self.media_tx.clone();
            let ctx2 = ctx.clone();
            std::thread::spawn(move || {
                let ok = dedup_core::thumbnail::ensure_audio_rate(&path, &hex, pct).is_ok();
                let _ = tx.send(MediaMsg::RateWav { path: out, ok });
                ctx2.request_repaint();
            });
        }
        AudioSrc::Rendering
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
        // working copy), then an opened archive member (back to the member
        // list), then flicker, then the viewer itself.
        if esc {
            if self.tag_edit.is_some() {
                self.tag_edit = None;
            } else if self.member_return.is_some() {
                self.return_to_archive(ctx);
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
        // other transport actions after drawing. A hidden second side has
        // nothing to flicker against.
        let mut swapped = false;
        if space && ready && self.two_sided() {
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
        let mut open_save: Option<bool> = None;
        let mut enter_flicker = false;
        let mut leave_flicker = false;
        let mut do_swap = false;
        let mut play: Option<bool> = None;
        let mut pause = false;
        let mut cycle_speed = false;
        let mut show = false;
        // Archive browsing: the member clicked to open, a request to go back
        // from an opened member, and extraction requests (whole / one member).
        let mut open_member: Option<dedup_core::archive::ArchiveEntry> = None;
        let mut back_to_archive = false;
        let mut extract_all_req = false;
        let mut extract_member_req: Option<dedup_core::archive::ArchiveEntry> = None;
        let mut unlock_req = false;
        let mut recover_req = false;
        let mut export_hash_req = false;
        let viewing_member = self.member_return.is_some();

        // Poll a running recovery attempt.
        if let Some(rx) = &self.recover_rx {
            if let Ok(result) = rx.try_recv() {
                self.recover_rx = None;
                match result {
                    Some(pw) => {
                        self.recover_note = Some("Recovered the password.".into());
                        self.unlock_password = Some(pw);
                    }
                    None => {
                        self.recover_note = Some(
                            "No luck — try a wordlist, or export the hash for hashcat.".into(),
                        );
                    }
                }
            } else {
                ctx.request_repaint_after(std::time::Duration::from_millis(200));
            }
        }
        let recovering = self.recover_rx.is_some();
        let (mut meta_l, mut meta_r) = (
            crate::lightbox::MetaAction::None,
            crate::lightbox::MetaAction::None,
        );
        let mut picked = egui::Area::new(Id::new("diff-compare"))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::Pos2::ZERO)
            .show(ctx, |ui| {
                let screen = ctx.content_rect();
                let bg = ui.allocate_rect(screen, egui::Sense::click_and_drag());
                // Fully opaque, and following the palette (black under dark, the
                // light backdrop under light): this is a judgement call about two
                // files, and a translucent backdrop let the tab underneath read
                // through the photographs.
                ui.painter().rect_filled(screen, 0.0, theme::bg());
                let inner = screen.shrink(12.0);
                let mut pick = None;
                let two_sided = self.two_sided();
                let two_sided_now = two_sided;
                let tab_is_image = self.tab == RepresentationKind::Image;
                let tab_is_audio = self.tab == RepresentationKind::Audio;
                // Flicker is a media-only mode (image/audio/video); its controls
                // and single-file chrome must never appear on the Text/hex tab.
                let tab_is_media = matches!(
                    self.tab,
                    RepresentationKind::Image
                        | RepresentationKind::Audio
                        | RepresentationKind::Video
                );
                let speed_label = {
                    let s = format!("{:.2}", self.rate);
                    s.trim_end_matches('0').trim_end_matches('.').to_string()
                };

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

                // Three horizontal bands, top to bottom: a read-only facts strip
                // (repo chip + path + size/date/type — *what the file is*), the
                // content viewport (the picture, filmstrip, spectrogram or text),
                // and a fixed action bar (*what you can do*). Controls live only
                // in the bottom bar, in fixed slots, so a filename's length never
                // shoves a button under the cursor. The bar gets a second row on
                // tabs that carry tools (image rotate/mirror/save, audio
                // transport), so the tools never crowd the navigate/delete row —
                // the clipping the old stacked layout was written to avoid.
                // Two rows per side — repo chip + facts inline, then the file
                // name — so the strip stays shallow and the content gets the
                // height (it was 142 when the facts stacked one per line).
                const TITLE_H: f32 = 78.0;
                const ACTION_ROW: f32 = 32.0;
                const ACTION_GAP: f32 = 6.0;
                const HINT_H: f32 = 14.0;
                let has_tools = tab_is_image || (tab_is_audio && player.is_some());
                let action_h = if has_tools {
                    ACTION_ROW * 2.0
                } else {
                    ACTION_ROW
                };
                let tab_h = 30.0;
                // A clear gap below the tab pills so the repo chip doesn't touch
                // them (the tabs end at inner.min.y + 30 + tab_h).
                let titles_top = inner.min.y + 30.0 + tab_h + 12.0;
                // The action bar is anchored to the window bottom (above the
                // one-line hint), so it can never be pushed off-screen by a short
                // window; the viewport fills whatever is left between the facts
                // and the bar.
                let action_band = Rect::from_min_max(
                    egui::pos2(inner.min.x, inner.max.y - HINT_H - action_h),
                    egui::pos2(inner.max.x, inner.max.y - HINT_H),
                );
                let viewport = Rect::from_min_max(
                    egui::pos2(inner.min.x, titles_top + TITLE_H),
                    egui::pos2(
                        inner.max.x,
                        (action_band.min.y - ACTION_GAP).max(titles_top + TITLE_H + 40.0),
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
                    // Preference order, not enum order: a video's own
                    // representation is Video — its soundtrack tab sits
                    // *beside* it, and Audio would otherwise sort first and
                    // land a clip on its soundtrack.
                    [
                        RepresentationKind::Archive,
                        RepresentationKind::Image,
                        RepresentationKind::Video,
                        RepresentationKind::Audio,
                    ]
                    .into_iter()
                    .find(|k| offered.contains(k))
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
                // screen for close analysis. Beside it, once both sides have
                // decoded, flicker gets buttons — space alone was a mystery.
                if two_sided_now {
                    ui.scope_builder(
                        UiBuilder::new()
                            .max_rect(tab_rect)
                            .layout(Layout::right_to_left(Align::Center)),
                        |ui| {
                            if ui
                                .button("HIDE B")
                                .explain(
                                    verbosity,
                                    "Show only the first file",
                                    "Hide the second side so the first file gets the whole \
                                     screen for close analysis.",
                                )
                                .clicked()
                            {
                                hide = true;
                            }
                            if ready && tab_is_media {
                                if self.compare.flicker {
                                    if ui
                                        .button("SIDE BY SIDE")
                                        .explain(
                                            verbosity,
                                            "Both files at once",
                                            "Leave flicker and show A and B next to each other \
                                             again.",
                                        )
                                        .clicked()
                                    {
                                        leave_flicker = true;
                                    }
                                    if ui
                                        .button("SWAP")
                                        .explain(
                                            verbosity,
                                            "Show the other file",
                                            "Swap which file fills the screen (Space does the \
                                             same).",
                                        )
                                        .clicked()
                                    {
                                        do_swap = true;
                                    }
                                } else if ui
                                    .button("FLICKER")
                                    .explain(
                                        verbosity,
                                        "Overlay the two files",
                                        "Show one file at a time in the full pane and swap \
                                         between them in place — a subtle difference stands \
                                         out immediately (Space does the same).",
                                    )
                                    .clicked()
                                {
                                    enter_flicker = true;
                                }
                            }
                        },
                    );
                } else {
                    ui.scope_builder(
                        UiBuilder::new()
                            .max_rect(tab_rect)
                            .layout(Layout::right_to_left(Align::Center)),
                        |ui| {
                            // Inside an opened archive member, the right-hand
                            // control goes back to the member list rather than
                            // offering a second side.
                            if viewing_member {
                                if ui
                                    .button(format!("{} BACK", crate::icon::CARET_LEFT))
                                    .explain(
                                        verbosity,
                                        "Back to the archive",
                                        "Return to the archive's member list (Esc does the \
                                         same).",
                                    )
                                    .clicked()
                                {
                                    back_to_archive = true;
                                }
                            } else if ui
                                .button("SHOW B")
                                .explain(
                                    verbosity,
                                    "Compare against another file",
                                    "Show a second file beside this one to compare them.",
                                )
                                .clicked()
                            {
                                show = true;
                            }
                        },
                    );
                }

                // Archive: the left side's member list. A readable member opens
                // in place (rendered by its own type); a locked member is shown
                // disabled until the archive is unlocked.
                if self.tab == RepresentationKind::Archive {
                    if self.archive_entries.is_none() {
                        let f = &self.left.facts;
                        let name = f
                            .abs_path
                            .file_name()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        self.archive_entries = Some(
                            dedup_core::archive::list_entries(
                                &f.abs_path,
                                &name,
                                f.mime.as_deref(),
                            )
                            .unwrap_or_default(),
                        );
                    }
                    let entries = self.archive_entries.clone().unwrap_or_default();
                    ui.scope_builder(
                        UiBuilder::new()
                            .max_rect(viewport)
                            .layout(Layout::top_down(Align::Min)),
                        |ui| {
                            // Extract the whole archive into a folder you pick.
                            ui.horizontal(|ui| {
                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new("EXTRACT ALL").color(theme::black()),
                                        )
                                        .fill(theme::amber()),
                                    )
                                    .explain(
                                        verbosity,
                                        "Extract every member to a folder",
                                        "Choose a folder and write every readable member into \
                                         it. Extract into a repository to have the contents \
                                         indexed on the next scan. The archive is never changed.",
                                    )
                                    .clicked()
                                {
                                    extract_all_req = true;
                                }
                                if let Some(s) = &self.extract_status {
                                    ui.label(RichText::new(s).color(theme::lilac()).size(11.0));
                                }
                            });
                            // Unlock: a locked archive with no session password
                            // yet offers a password field. Once verified, locked
                            // members open and extract.
                            let has_locked = entries.iter().any(|e| e.locked);
                            if has_locked && self.unlock_password.is_none() {
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new(format!("{}  LOCKED", crate::icon::LOCK))
                                            .color(theme::amber())
                                            .size(12.0),
                                    );
                                    let field = egui::TextEdit::singleline(&mut self.unlock_input)
                                        .password(true)
                                        .hint_text("password")
                                        .desired_width(160.0);
                                    let resp = ui.add(field);
                                    let entered = resp.lost_focus()
                                        && ui.input(|i| i.key_pressed(egui::Key::Enter));
                                    if ui
                                        .add(
                                            egui::Button::new(
                                                RichText::new("UNLOCK").color(theme::black()),
                                            )
                                            .fill(theme::amber()),
                                        )
                                        .clicked()
                                        || entered
                                    {
                                        unlock_req = true;
                                    }
                                    // Built-in recovery: try the easy possibilities.
                                    let recover = ui.add_enabled(
                                        !recovering,
                                        egui::Button::new(
                                            RichText::new(if recovering {
                                                "RECOVERING…"
                                            } else {
                                                "RECOVER"
                                            })
                                            .color(theme::text()),
                                        ),
                                    );
                                    if recover
                                        .on_hover_text(
                                            "Try a built-in list of common passwords. Weak \
                                             passwords may fall; a strong one will not — export \
                                             the hash for hashcat instead.",
                                        )
                                        .clicked()
                                    {
                                        recover_req = true;
                                    }
                                    if ui
                                        .button("EXPORT HASH")
                                        .on_hover_text(
                                            "Copy this archive's hash in hashcat's $zip2$ format \
                                             (mode 13600) to the clipboard, to crack it with \
                                             hashcat or John where the real GPU power is.",
                                        )
                                        .clicked()
                                    {
                                        export_hash_req = true;
                                    }
                                });
                                if self.unlock_failed {
                                    ui.label(
                                        RichText::new("Wrong password.")
                                            .color(theme::red())
                                            .size(11.0),
                                    );
                                }
                                if let Some(note) = &self.recover_note {
                                    ui.label(RichText::new(note).color(theme::lilac()).size(11.0));
                                }
                                ui.add_space(4.0);
                            } else if has_locked && self.unlock_password.is_some() {
                                ui.label(
                                    RichText::new(format!(
                                        "{}  Unlocked for this session",
                                        crate::icon::LOCK_OPEN
                                    ))
                                    .color(theme::green())
                                    .size(11.0),
                                );
                                ui.add_space(4.0);
                            }
                            let unlocked = self.unlock_password.is_some();

                            if entries.is_empty() {
                                ui.label(
                                    RichText::new(
                                        "This archive has no readable members (or could not be \
                                         opened).",
                                    )
                                    .color(theme::grey())
                                    .size(12.0),
                                );
                                return;
                            }
                            egui::ScrollArea::vertical()
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    for entry in &entries {
                                        ui.horizontal(|ui| {
                                            if entry.locked && !unlocked {
                                                ui.add_enabled(
                                                    false,
                                                    egui::Button::new(
                                                        RichText::new(format!(
                                                            "{}  {}",
                                                            crate::icon::LOCK,
                                                            entry.name
                                                        ))
                                                        .color(theme::hairline()),
                                                    ),
                                                );
                                                ui.label(
                                                    RichText::new("LOCKED")
                                                        .color(theme::hairline())
                                                        .size(11.0),
                                                );
                                            } else {
                                                if ui
                                                    .add(
                                                        egui::Button::new(
                                                            RichText::new(&entry.name)
                                                                .color(theme::text()),
                                                        )
                                                        .fill(theme::panel()),
                                                    )
                                                    .clicked()
                                                {
                                                    open_member = Some(entry.clone());
                                                }
                                                if ui
                                                    .button(crate::icon::ARROW_RIGHT)
                                                    .on_hover_text(
                                                        "Extract this member to a folder",
                                                    )
                                                    .clicked()
                                                {
                                                    extract_member_req = Some(entry.clone());
                                                }
                                            }
                                            ui.label(
                                                RichText::new(crate::util::format_size(entry.size))
                                                    .color(theme::lilac())
                                                    .size(11.0),
                                            );
                                        });
                                    }
                                });
                        },
                    );
                } else if self.tab == RepresentationKind::Metadata {
                    for slot in [0usize, 1usize] {
                        let side = if slot == 0 { &self.left } else { &self.right };
                        if self.tags[slot].is_none() {
                            self.tags[slot] = Some(
                                crate::id3tags::container_supported(side.facts.mime.as_deref())
                                    .then(|| crate::id3tags::read(&side.facts.abs_path))
                                    .flatten(),
                            );
                        }
                        // The full EXIF listing, read from the file once. When
                        // the file itself yields nothing, the two indexed
                        // facts still show.
                        if self.exif_all[slot].is_none() {
                            let fields = if side.facts.is_image() {
                                let mut f =
                                    dedup_core::fingerprint::exif_fields(&side.facts.abs_path);
                                if f.is_empty()
                                    && let Some(ex) = side.facts.exif.as_ref()
                                {
                                    if let Some(c) = &ex.camera {
                                        f.push(("Camera".into(), c.clone()));
                                    }
                                    if let Some(ms) = ex.taken_ms {
                                        f.push(("Taken".into(), crate::util::format_mtime(ms)));
                                    }
                                }
                                f
                            } else {
                                Vec::new()
                            };
                            self.exif_all[slot] = Some(fields);
                        }
                    }
                    let (lt, rt) = (
                        self.tags[0].clone().flatten(),
                        self.tags[1].clone().flatten(),
                    );
                    let (lx, rx) = (
                        self.exif_all[0].clone().unwrap_or_default(),
                        self.exif_all[1].clone().unwrap_or_default(),
                    );
                    // Tags that differ between the two sides drive the Metadata
                    // tab's highlight — the whole point of comparing them.
                    let differing = differing_exif_tags(&lx, &rx);
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
                        exif: Vec<(String, String)>,
                        differing: std::collections::HashSet<String>,
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
                                fields: exif,
                                differing,
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
                                &l.facts.abs_path,
                                meta_body(l, lt.as_ref(), lx.clone(), differing.clone(), te),
                            );
                        }));
                    }
                    if two_sided_now && rreps.metadata.is_some() {
                        cols.push(Box::new(|ui: &mut egui::Ui, te: &mut Option<TagEdit>| {
                            meta_r = crate::lightbox::draw_metadata_column(
                                ui,
                                &r.facts.abs_path,
                                meta_body(r, rt.as_ref(), rx.clone(), differing.clone(), te),
                            );
                        }));
                    }
                    draw_columns(&mut child, &mut tag_edit, cols);
                    self.tag_edit = tag_edit;
                } else if self.tab == RepresentationKind::Text {
                    // Readable text: a document's extracted words, or a plain-text
                    // file's raw text — one file, or an aligned content diff when
                    // comparing two. Raw bytes live on the Hex tab now, never here.
                    if two_sided_now {
                        // Below the per-side action strip (which extends past the
                        // titles into the viewport top) so the diff's own controls
                        // can't collide — plus the switcher row when pooled.
                        let has_switcher = self.can_step_left() || self.can_step_right();
                        let clearance = if has_switcher { 58.0 } else { 30.0 };
                        let diff_rect = Rect::from_min_max(
                            egui::pos2(viewport.min.x, viewport.min.y + clearance),
                            viewport.max,
                        );
                        let mut child = ui.new_child(
                            UiBuilder::new()
                                .max_rect(diff_rect)
                                .layout(Layout::top_down(Align::Min)),
                        );
                        self.render_text_diff(&mut child);
                    } else {
                        for slot in [0usize, 1usize] {
                            if self.text[slot].is_none() {
                                let side = if slot == 0 { &self.left } else { &self.right };
                                self.text[slot] = Some(document_preview(&side.facts));
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
                        // Both panes are locked to one scroll offset: each column
                        // is drawn at the shared position and reports back where
                        // it ended up, so whichever pane the user scrolled becomes
                        // the new shared position (§ story 31 — comparing bytes
                        // means looking at the same offset in both).
                        type ScrollSync = (egui::Vec2, Option<egui::Vec2>);
                        let mut sync: ScrollSync = (self.text_scroll, None);
                        let mut cols: Vec<crate::lightbox::ColumnFn<'_, ScrollSync>> =
                            vec![Box::new(move |ui: &mut egui::Ui, sync: &mut ScrollSync| {
                                if let Some(p) = &lt {
                                    let (_, off) = draw_text_column(
                                        ui,
                                        &l.facts.abs_path,
                                        p,
                                        height,
                                        Some(sync.0),
                                    );
                                    if off != sync.0 {
                                        sync.1 = Some(off);
                                    }
                                }
                            })];
                        if two_sided_now {
                            cols.push(Box::new(move |ui: &mut egui::Ui, sync: &mut ScrollSync| {
                                if let Some(p) = &rt {
                                    let (_, off) = draw_text_column(
                                        ui,
                                        &r.facts.abs_path,
                                        p,
                                        height,
                                        Some(sync.0),
                                    );
                                    if off != sync.0 {
                                        sync.1 = Some(off);
                                    }
                                }
                            }));
                        }
                        draw_columns(&mut child, &mut sync, cols);
                        self.text_scroll = sync.1.unwrap_or(sync.0);
                    }
                } else if self.tab == RepresentationKind::Strings {
                    // Printable runs below the per-side action strip, so its
                    // controls can't collide — plus the switcher row when pooled.
                    let has_switcher = self.can_step_left() || self.can_step_right();
                    let clearance = if has_switcher { 58.0 } else { 30.0 };
                    let rect = Rect::from_min_max(
                        egui::pos2(viewport.min.x, viewport.min.y + clearance),
                        viewport.max,
                    );
                    let mut child = ui.new_child(
                        UiBuilder::new()
                            .max_rect(rect)
                            .layout(Layout::top_down(Align::Min)),
                    );
                    self.render_strings(&mut child, two_sided_now);
                } else if self.tab == RepresentationKind::Render {
                    // The document rasterized to its first page, shown as it
                    // looks — one file, or both side by side. Judged by eye; no
                    // pixel diff, no verdict. (Page navigation and flicker land
                    // with multi-page support.)
                    let ctx = ui.ctx().clone();
                    self.ensure_render(&ctx, 0);
                    if two_sided_now {
                        self.ensure_render(&ctx, 1);
                        let (left_pane, right_pane) = compare_split(viewport);
                        self.draw_render_page(ui, left_pane, 0);
                        self.draw_render_page(ui, right_pane, 1);
                    } else {
                        self.draw_render_page(ui, viewport, 0);
                    }
                } else if self.tab == RepresentationKind::Hex {
                    // Raw bytes, always: a hex dump of one file's head, or the
                    // full-file aligned hex diff when comparing two.
                    if two_sided_now {
                        let has_switcher = self.can_step_left() || self.can_step_right();
                        let clearance = if has_switcher { 58.0 } else { 30.0 };
                        let rect = Rect::from_min_max(
                            egui::pos2(viewport.min.x, viewport.min.y + clearance),
                            viewport.max,
                        );
                        let mut child = ui.new_child(
                            UiBuilder::new()
                                .max_rect(rect)
                                .layout(Layout::top_down(Align::Min)),
                        );
                        self.render_hex_diff(&mut child);
                    } else {
                        if self.hex_head[0].is_none() {
                            self.hex_head[0] =
                                Some(crate::lightbox::hex_head_preview(&self.left.facts.abs_path));
                        }
                        if let Some(p) = &self.hex_head[0] {
                            let body = p.body.clone();
                            // Draw into a viewport-clipped child, not the whole
                            // Area ui — otherwise the dump paints up into the
                            // tabs and facts strip (the single-view "rendered
                            // into the headers" bug).
                            let mut child = ui.new_child(
                                UiBuilder::new()
                                    .max_rect(viewport)
                                    .layout(Layout::top_down(Align::Min)),
                            );
                            child.set_clip_rect(viewport);
                            egui::ScrollArea::both().auto_shrink([false, false]).show(
                                &mut child,
                                |ui| {
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(&body)
                                                .monospace()
                                                .size(12.0)
                                                .color(theme::text()),
                                        )
                                        .wrap_mode(egui::TextWrapMode::Extend),
                                    );
                                },
                            );
                        }
                    }
                } else if self.tab == RepresentationKind::Video {
                    // The Video representation: an aligned filmstrip per side
                    // — each clip's whole shape at a glance — with a shared
                    // proportional playhead dropped by clicking, enlarged as
                    // A@t | B@t below. Below the per-side action strip (which
                    // extends past the titles into the viewport top), like the
                    // other tabs with their own controls.
                    let has_switcher = self.can_step_left() || self.can_step_right();
                    let clearance = if has_switcher { 58.0 } else { 30.0 };
                    let viewport = Rect::from_min_max(
                        egui::pos2(viewport.min.x, viewport.min.y + clearance),
                        viewport.max,
                    );
                    let mut clicked: Option<f32> = None;
                    if two_sided_now {
                        let (left_pane, right_pane) = compare_split(viewport);
                        for (slot, pane) in [(0usize, left_pane), (1usize, right_pane)] {
                            if let Some(f) = self.draw_video_side(ui, pane, slot, verbosity) {
                                clicked = Some(f);
                            }
                        }
                    } else {
                        if let Some(f) = self.draw_video_side(ui, viewport, 0, verbosity) {
                            clicked = Some(f);
                        }
                        self.film_rects[1].clear();
                    }
                    if let Some(f) = clicked {
                        self.playhead = Some(f);
                        self.spawn_scrub(ctx, 0, f);
                        if two_sided_now {
                            self.spawn_scrub(ctx, 1, f);
                        }
                    }
                } else if !two_sided_now {
                    // One file, the whole viewport — the hidden side must not
                    // paint a second copy of the same picture.
                    match self.slot_state(0) {
                        SlotState::Image => {
                            let (a_tex, a_img) = self.sized(0);
                            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
                            if bg.dragged() {
                                self.compare.pan_by(bg.drag_delta());
                            }
                            if scroll != 0.0
                                && ctx
                                    .pointer_hover_pos()
                                    .is_some_and(|c| viewport.contains(c))
                            {
                                self.compare.zoom_by((scroll * 0.005).exp());
                            }
                            draw_in_pane(
                                ui,
                                viewport,
                                self.compare.pane_rect(viewport, a_img),
                                &a_tex,
                            );
                            ui.painter().text(
                                egui::pos2(inner.min.x + 4.0, action_band.max.y + 1.0),
                                Align2::LEFT_TOP,
                                "wheel: zoom · drag: pan · Esc close",
                                FontId::proportional(11.0),
                                theme::hairline(),
                            );
                        }
                        note => {
                            let text = if note == SlotState::Decoding {
                                "decoding…".to_string()
                            } else {
                                self.no_preview_text(0)
                            };
                            ui.painter().text(
                                viewport.center(),
                                Align2::CENTER_CENTER,
                                text,
                                FontId::proportional(14.0),
                                theme::tan(),
                            );
                        }
                    }
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
                        egui::pos2(inner.min.x + 4.0, action_band.max.y + 1.0),
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
                                let text = if note == SlotState::Decoding {
                                    "decoding…".to_string()
                                } else {
                                    self.no_preview_text(slot)
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

                // The read-only facts strip up top, then the fixed action bar
                // along the bottom. Each side keeps a *fixed* half (not flow
                // layout), so B's block never drifts when A's is short.
                let strip = Rect::from_min_max(
                    egui::pos2(inner.min.x, titles_top),
                    egui::pos2(inner.max.x, titles_top + TITLE_H),
                );
                let col_w = (strip.width() - 16.0) * 0.5;
                // In flicker the viewer is single-file: only the shown side (A or
                // B) carries chrome, full-width, and SWAP flips it across — so
                // there is no hidden-side control to click by accident.
                let flicker_active = self.compare.flicker && two_sided && tab_is_media;
                let shown_is_left = !self.compare.show_b;
                let sides: &[bool] = if !two_sided {
                    &[true]
                } else if flicker_active {
                    if shown_is_left { &[true] } else { &[false] }
                } else {
                    &[true, false]
                };
                // One side's fixed half within a band (the top strip or the
                // bottom action bar), aligned with the image pane between them.
                let side_half = |band: Rect, is_left: bool| -> Rect {
                    let full = !two_sided || flicker_active;
                    let half = if full { band.width() } else { col_w };
                    let x0 = if full || is_left {
                        band.min.x
                    } else {
                        band.min.x + col_w + 16.0
                    };
                    Rect::from_min_size(egui::pos2(x0, band.min.y), egui::vec2(half, band.height()))
                };

                // A thin dark rule down the centre gutter — the "this is A, this
                // is B" helper — from the facts strip through the content to the
                // action bar. Only when both sides actually show side by side
                // (a single/flicker view is one file, so no divide).
                if two_sided && !flicker_active {
                    let x = inner.center().x;
                    ui.painter().vline(
                        x,
                        titles_top..=action_band.min.y,
                        egui::Stroke::new(1.0, theme::hairline()),
                    );
                }

                // Top: read-only identity, one block per side.
                for &is_left in sides {
                    let (side, other) = if is_left {
                        (&self.left, &self.right)
                    } else {
                        (&self.right, &self.left)
                    };
                    ui.scope_builder(
                        UiBuilder::new()
                            .max_rect(side_half(strip, is_left))
                            .layout(Layout::top_down(Align::Min)),
                        |ui| side_strip(ui, side, other, is_left),
                    );
                }

                // Bottom: the fixed action bar per side — navigate · tools ·
                // delete. Navigation is left-edge-anchored and the destructive
                // action is inset from the right edge, so the two controls you
                // must not confuse sit at opposite ends and never move under the
                // cursor, whatever the filename's length.
                for &is_left in sides {
                    // The Archive tab owns the whole viewport with its own
                    // controls (extract / unlock); it carries no per-side action
                    // bar, matching the pre-refactor behavior.
                    if self.tab == RepresentationKind::Archive {
                        continue;
                    }
                    let bar = side_half(action_band, is_left);
                    let side = if is_left { &self.left } else { &self.right };
                    let label = if is_left { "A" } else { "B" };
                    let can_step = if is_left {
                        self.can_step_left()
                    } else {
                        self.can_step_right()
                    };
                    // Row 1 holds the two controls that must never be confused:
                    // navigation (left edge) and the destructive action (its own
                    // right-hand region). The tab's tools get their own row 2, so
                    // a long tool set can never crowd or clip the delete control —
                    // the clipping the old stacked layout was written to avoid.
                    let row1 =
                        Rect::from_min_max(bar.min, egui::pos2(bar.max.x, bar.min.y + ACTION_ROW));
                    let del_w = (bar.width() * 0.55).min(260.0);
                    let nav_rect = Rect::from_min_max(
                        row1.min,
                        egui::pos2(row1.max.x - del_w - 8.0, row1.max.y),
                    );
                    let del_rect =
                        Rect::from_min_max(egui::pos2(row1.max.x - del_w, row1.min.y), row1.max);

                    // Row 1 left: the compact switcher and the pair's similarity.
                    ui.scope_builder(
                        UiBuilder::new()
                            .max_rect(nav_rect)
                            .layout(Layout::left_to_right(Align::Center)),
                        |ui| {
                            if can_step {
                                if ui
                                    .button(
                                        RichText::new(crate::icon::CARET_LEFT).color(theme::text()),
                                    )
                                    .on_hover_text("Previous candidate for this side")
                                    .clicked()
                                {
                                    step = Some((is_left, -1));
                                }
                                if let Some(pos) = self.switcher_label(is_left) {
                                    ui.label(RichText::new(pos).color(theme::tan()).size(12.0));
                                }
                                if ui
                                    .button(
                                        RichText::new(crate::icon::CARET_RIGHT)
                                            .color(theme::text()),
                                    )
                                    .on_hover_text("Next candidate for this side")
                                    .clicked()
                                {
                                    step = Some((is_left, 1));
                                }
                            }
                            // The pair's perceptual similarity, shown once (on
                            // A's bar): why two different-looking files were
                            // grouped, and how loosely.
                            if is_left && let Some(sim) = self.similarity {
                                ui.label(
                                    RichText::new(format!("A ↔ B  {}", similarity_label(sim)))
                                        .color(theme::lilac())
                                        .size(12.0),
                                );
                            }
                        },
                    );

                    // Row 1 right: the destructive action in its own region — the
                    // caller's mark pill, or DIFF's OVERWRITE OTHER / DELETE.
                    let mark = self.marks[usize::from(!is_left)];
                    let mark_label = if two_sided && !flicker_active {
                        if is_left { "DELETE A" } else { "DELETE B" }
                    } else {
                        "DELETE"
                    };
                    let picked = ui
                        .scope_builder(
                            UiBuilder::new()
                                .max_rect(del_rect)
                                .layout(Layout::right_to_left(Align::Center)),
                            |ui| {
                                // Inset the destructive action from the very
                                // edge — the edge is the effortless "slam to
                                // it" target, which delete must not be.
                                ui.add_space(8.0);
                                side_actions(ui, is_left, verbosity, mark.map(|m| (mark_label, m)))
                            },
                        )
                        .inner;
                    if picked.is_some() {
                        pick = picked;
                    }

                    // Row 2: the tab's own tools (image rotate/mirror/save, audio
                    // transport), in their own full-width row so they never crowd
                    // the navigate/delete row above.
                    if has_tools {
                        let tools_row = Rect::from_min_max(
                            egui::pos2(bar.min.x, bar.min.y + ACTION_ROW),
                            bar.max,
                        );
                        ui.scope_builder(
                            UiBuilder::new()
                                .max_rect(tools_row)
                                .layout(Layout::left_to_right(Align::Center)),
                            |ui| {
                                if tab_is_image {
                                    let slot = usize::from(!is_left);
                                    let turned = !self.ops[slot].is_empty();
                                    if ui.button(format!("ROTATE {label}")).clicked() {
                                        turn = Some((is_left, Orient::RotateCw));
                                    }
                                    if ui.button(format!("MIRROR {label}")).clicked() {
                                        turn = Some((is_left, Orient::FlipH));
                                    }
                                    let long_help = if side.read_only {
                                        "Save this side's rotation/mirror as a new copy beside the \
                                         original. This repo is locked, so the original itself \
                                         can't be overwritten."
                                    } else {
                                        "Save this side's rotation/mirror to the file — \
                                         overwriting it in place or as a new copy; you choose next."
                                    };
                                    if turned
                                        && ui
                                            .add(
                                                egui::Button::new(
                                                    RichText::new(format!("SAVE {label}"))
                                                        .color(theme::ink_on(theme::amber())),
                                                )
                                                .fill(theme::amber()),
                                            )
                                            .explain(
                                                verbosity,
                                                "Write the turned image to disk",
                                                long_help,
                                            )
                                            .clicked()
                                    {
                                        open_save = Some(is_left);
                                    }
                                }
                                if tab_is_audio && player.is_some() {
                                    let slot = usize::from(!is_left);
                                    let has_sound = self.side_has_sound(slot);
                                    if ui
                                        .add_enabled(
                                            has_sound,
                                            egui::Button::new(format!("PLAY {label}")),
                                        )
                                        .on_disabled_hover_text("This clip has no audio track.")
                                        .clicked()
                                    {
                                        play = Some(is_left);
                                    }
                                    if ui.button("PAUSE").clicked() {
                                        pause = true;
                                    }
                                    if ui
                                        .button(format!("SPEED {}×", speed_label))
                                        .explain(
                                            verbosity,
                                            "Change the playback speed",
                                            "Step through the playback speeds (0.25× – 2×). The \
                                             pitch is preserved, so a slowed track still sounds \
                                             like itself.",
                                        )
                                        .clicked()
                                    {
                                        cycle_speed = true;
                                    }
                                    if self.pending_play.is_some() {
                                        ui.label(
                                            RichText::new("Preparing audio…")
                                                .color(theme::lilac())
                                                .size(11.0),
                                        );
                                    }
                                }
                            },
                        );
                    }
                }
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
                (MetaAction::ExportMetadata, _) => self.export_metadata(true),
                (_, MetaAction::ExportMetadata) => self.export_metadata(false),
                (MetaAction::Cancel, _) | (_, MetaAction::Cancel) => self.tag_edit = None,
                (MetaAction::None, MetaAction::None) => {}
            }
        }
        // The flicker buttons mirror the space bar exactly; a button swap
        // flips the audio with the picture, settled in the player block below.
        if enter_flicker {
            self.compare.flicker = true;
        }
        if leave_flicker {
            self.compare.flicker = false;
        }
        if do_swap && self.compare.flicker {
            self.compare.show_b = !self.compare.show_b;
            swapped = true;
        }
        if let Some((is_left, op)) = turn {
            self.rotate(is_left, op);
            self.upload(ctx, usize::from(!is_left));
        }
        // Export the hashcat $zip2$ hash to the clipboard for external cracking.
        if export_hash_req {
            let f = &self.left.facts;
            let name = f
                .abs_path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            self.recover_note = Some(
                match dedup_core::archive::export_hashcat_hash(
                    &f.abs_path,
                    &name,
                    f.mime.as_deref(),
                ) {
                    Some(hash) => {
                        ctx.copy_text(hash);
                        "Hash copied — run: hashcat -m 13600 <hash> <wordlist>".to_string()
                    }
                    None => "No exportable hash (only WinZip-AES zips are supported).".to_string(),
                },
            );
        }
        // Recover: run a built-in wordlist attempt on a background thread.
        if recover_req && self.recover_rx.is_none() {
            let f = &self.left.facts;
            let name = f
                .abs_path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let (path, mime) = (f.abs_path.clone(), f.mime.clone());
            let (tx, rx) = crossbeam_channel::bounded(1);
            self.recover_rx = Some(rx);
            self.recover_note = Some("Trying common passwords…".into());
            let ctx2 = ctx.clone();
            std::thread::spawn(move || {
                let found = dedup_core::archive::recover_password(
                    &path,
                    &name,
                    mime.as_deref(),
                    &[],
                    &dedup_core::archive::builtin_wordlist(),
                    || false,
                );
                let _ = tx.send(found);
                ctx2.request_repaint();
            });
        }
        // Unlock: verify the typed password against the archive. On success it
        // is held for this session so locked members open and extract.
        if unlock_req {
            let f = &self.left.facts;
            let name = f
                .abs_path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let candidate = self.unlock_input.clone();
            if dedup_core::archive::verify_password(
                &f.abs_path,
                &name,
                f.mime.as_deref(),
                &candidate,
            ) {
                self.unlock_password = Some(candidate);
                self.unlock_failed = false;
                self.unlock_input.clear();
            } else {
                self.unlock_failed = true;
            }
        }
        // Archive browsing: open a clicked member, or go back to the list.
        let pw = self.unlock_password.clone();
        if let Some(entry) = open_member {
            self.open_archive_member(ctx, &entry, pw.as_deref());
        }
        if back_to_archive {
            self.return_to_archive(ctx);
        }
        // Extraction: pick a destination folder and write the member(s) there.
        // Extract into a repository to have the contents indexed on next scan;
        // the source archive is never modified.
        if extract_all_req {
            self.extract_to_picked_folder(None);
        }
        if let Some(entry) = extract_member_req {
            self.extract_to_picked_folder(Some(entry));
        }
        // The save dialog for a turned image: overwrite in place or write a
        // `_rot` sibling, in either case carrying the date the user chose —
        // the original's, or (opt-in) the EXIF capture date.
        if let Some(is_left) = open_save {
            self.save_confirm = Some(is_left);
            self.save_exif_date = false;
            self.save_error = None;
        }
        if let Some(is_left) = self.save_confirm {
            let slot = usize::from(!is_left);
            let (name, path, taken, read_only) = {
                let side = if is_left { &self.left } else { &self.right };
                (
                    side.rel_path.clone(),
                    side.facts.abs_path.clone(),
                    side.facts.exif.as_ref().and_then(|e| e.taken_ms),
                    side.read_only,
                )
            };
            let actions = save_actions(read_only, true);
            let mut do_save: Option<bool> = None; // Some(overwrite)
            let mut cancel = false;
            let mut exif_date = self.save_exif_date;
            let save_error = self.save_error.clone();
            egui::Modal::new(Id::new("viewer-save")).show(ctx, |ui| {
                ui.set_width(440.0);
                ui.label(
                    RichText::new("WRITE THE TURNED IMAGE")
                        .color(theme::amber())
                        .size(16.0)
                        .strong(),
                );
                ui.add_space(6.0);
                ui.colored_label(theme::text(), &name);
                let blurb = if actions.overwrite {
                    "Overwriting replaces the file in place; saving a copy writes a \
                     new `_rot` file beside it and leaves the original untouched. \
                     Either way the file keeps its modified time."
                } else {
                    "This repo is locked, so the original can't be overwritten — but \
                     saving a copy is fine, since it only adds a new `_rot` file beside \
                     the original. Unlock the repo in Duplicates to overwrite in place. \
                     The copy keeps the file's modified time."
                };
                ui.label(RichText::new(blurb).color(theme::lilac()).size(11.0));
                if let Some(ms) = taken {
                    ui.add_space(4.0);
                    ui.checkbox(
                        &mut exif_date,
                        RichText::new(format!(
                            "Set the file date to the EXIF capture date ({})",
                            crate::util::format_mtime(ms)
                        ))
                        .color(theme::text()),
                    );
                }
                if let Some(err) = &save_error {
                    ui.add_space(4.0);
                    ui.colored_label(theme::red(), err);
                }
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if actions.overwrite
                        && ui
                            .add(
                                egui::Button::new(
                                    RichText::new("OVERWRITE").color(theme::ink_on(theme::red())),
                                )
                                .fill(theme::red()),
                            )
                            .explain(
                                verbosity,
                                "Replace the file in place",
                                "Write the turned image over the original. The previous pixels \
                                 are gone afterwards; the file's date is kept.",
                            )
                            .clicked()
                    {
                        do_save = Some(true);
                    }
                    if ui
                        .add(
                            egui::Button::new(RichText::new("SAVE COPY").color(theme::black()))
                                .fill(theme::tan()),
                        )
                        .explain(
                            verbosity,
                            "Write a new file beside the original",
                            "Write the turned image as a new `_rot` file next to the \
                             original, which stays exactly as it was.",
                        )
                        .clicked()
                    {
                        do_save = Some(false);
                    }
                    if ui
                        .button(RichText::new("CANCEL").color(theme::text()))
                        .clicked()
                    {
                        cancel = true;
                    }
                });
            });
            self.save_exif_date = exif_date;
            if cancel {
                self.save_confirm = None;
            }
            if let Some(overwrite) = do_save {
                let time = match taken {
                    Some(ms) if self.save_exif_date => crate::imgedit::SavedTime::At(ms),
                    _ => crate::imgedit::SavedTime::Original,
                };
                let ops = self.ops.get(slot).cloned().unwrap_or_default();
                match crate::imgedit::save_edited(&path, &ops, overwrite, time) {
                    Ok(_) => {
                        self.save_confirm = None;
                        if overwrite {
                            // The bytes behind this side changed: re-decode
                            // them, and tell the caller to refresh its index.
                            self.refresh_side(ctx, slot);
                            picked = picked.or(Some(DiffPick::Edited { on_left: is_left }));
                        }
                    }
                    Err(e) => self.save_error = Some(format!("Save failed: {e}")),
                }
            }
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

        // Arrow keys: on a two-sided pair with sound on both sides (bare audio
        // or a video's soundtrack) they flip which copy is audible (below,
        // gap-free on the loaded pair — re-pointing a side instead would force
        // a reloading pause, the bug the user originally hit); with the second
        // side hidden they step the shown file through the pool.
        let sound_pair = self.two_sided() && self.side_has_sound(0) && self.side_has_sound(1);
        let flip = (arrow_l || arrow_r) && sound_pair;
        if (arrow_l || arrow_r) && !sound_pair && !self.two_sided() && step.is_none() {
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
        // viewer. `snap` is the transport as this frame found it. Each side's
        // source is resolved at the chosen rate stop — the file itself, a
        // video's extracted soundtrack, or their pitch-preserving `atempo`
        // renders — and an action whose source is still rendering waits as
        // `pending_play`, firing the moment it is ready.
        if let Some(p) = player {
            let snap = p.snapshot();
            // Where playback stands, as a fraction — positions travel as
            // fractions across rate changes, where the runtimes differ.
            let frac = if snap.total_ms > 0 {
                (snap.pos_ms as f32 / snap.total_ms as f32).clamp(0.0, 1.0)
            } else {
                0.0
            };
            // Whether the player already holds exactly this (A, B) pair *at
            // the current rate* — then a swap is an instant, gap-free volume
            // flip rather than a reload. The same hexes at another rate are
            // other files, hence the rate in the identity.
            let pair_loaded = snap.paired
                && (self.loaded_rate - self.rate).abs() < 0.01
                && snap.hex_a.as_deref() == Some(self.left.facts.hash_hex.as_str())
                && snap.hex_b.as_deref() == Some(self.right.facts.hash_hex.as_str());

            // PLAY A / PLAY B: on a pair with sound both copies load in sync
            // with the asked-for side audible, so a later flip is gap-free.
            if let Some(is_left) = play {
                self.pending_play = Some(if sound_pair {
                    PendingPlay::Pair {
                        audible_b: !is_left,
                        fraction: frac,
                        paused: false,
                    }
                } else {
                    PendingPlay::Single {
                        left: is_left,
                        fraction: 0.0,
                        paused: false,
                    }
                });
            }
            if pause {
                p.toggle_pause();
            }
            // P: pause/resume what is loaded, else start playing — the pair
            // when comparing, the shown file alone otherwise.
            if key_p {
                if snap.loaded {
                    p.toggle_pause();
                } else if sound_pair {
                    self.pending_play = Some(PendingPlay::Pair {
                        audible_b: false,
                        fraction: frac,
                        paused: false,
                    });
                } else if self.side_has_sound(0) {
                    self.pending_play = Some(PendingPlay::Single {
                        left: true,
                        fraction: frac,
                        paused: false,
                    });
                }
            }
            // SPEED: step to the next stop. Whatever is loaded re-anchors at
            // the same fraction once the pitch-preserving render is ready —
            // in a pair both sides render at the chosen rate, staying aligned.
            if cycle_speed {
                self.rate = crate::scrub::next_rate(self.rate);
                if snap.loaded {
                    self.pending_play = Some(if snap.paired {
                        PendingPlay::Pair {
                            audible_b: self.audio_active == Some(1),
                            fraction: frac,
                            paused: !snap.playing,
                        }
                    } else {
                        PendingPlay::Single {
                            left: self.audio_active != Some(1),
                            fraction: frac,
                            paused: !snap.playing,
                        }
                    });
                }
            }
            // Arrows on the pair: flip the audible copy, gap-free.
            if flip && snap.loaded {
                let want_b = self.audio_active != Some(1);
                if pair_loaded {
                    let target = if want_b {
                        self.right.facts.hash_hex.as_str()
                    } else {
                        self.left.facts.hash_hex.as_str()
                    };
                    if snap.hex.as_deref() != Some(target) {
                        p.flip();
                    }
                    self.audio_active = Some(usize::from(want_b));
                } else {
                    self.pending_play = Some(PendingPlay::Pair {
                        audible_b: want_b,
                        fraction: frac,
                        paused: !snap.playing,
                    });
                }
                if self.compare.flicker {
                    self.compare.show_b = want_b;
                }
            }
            // A flicker swap flips the audio with the picture, gap-free.
            if swapped && sound_pair && snap.loaded {
                let want_b = self.compare.show_b;
                if pair_loaded {
                    let target = if want_b {
                        self.right.facts.hash_hex.as_str()
                    } else {
                        self.left.facts.hash_hex.as_str()
                    };
                    if snap.hex.as_deref() != Some(target) {
                        p.flip();
                    }
                    self.audio_active = Some(usize::from(want_b));
                } else {
                    self.pending_play = Some(PendingPlay::Pair {
                        audible_b: want_b,
                        fraction: frac,
                        paused: !snap.playing,
                    });
                }
            }
            // Stepping the shown file follows with whatever transport state it
            // was in: playing keeps playing the new copy, a deliberate pause
            // stays paused with the new copy loaded — leaving the previous file
            // loaded would show one copy and resume another. (The step itself
            // cleared any stale deferred action via `refresh_side`.)
            if let Some((true, _)) = stepped
                && !self.two_sided()
                && snap.loaded
                && self.side_has_sound(0)
            {
                self.pending_play = Some(PendingPlay::Single {
                    left: true,
                    fraction: frac,
                    paused: !snap.playing,
                });
            }
            // Keep the synced pair loaded whenever the pair plays, so flips
            // stay instant even after a side was stepped to another file —
            // and re-anchor it after a rate change, both sides at the new rate.
            if sound_pair && snap.playing && !pair_loaded && self.pending_play.is_none() {
                self.pending_play = Some(PendingPlay::Pair {
                    audible_b: self.audio_active == Some(1),
                    fraction: frac,
                    paused: false,
                });
            }
            // Fire the deferred action the moment its sources are ready
            // (resolved *after* the actions above, so a rate change this frame
            // resolves at the new rate). Dropped if a side turns out to have
            // no sound after all; kept waiting while anything still renders.
            if let Some(pend) = self.pending_play.take() {
                let src_a = self.audio_src(ctx, 0);
                let src_b = self.audio_src(ctx, 1);
                match pend {
                    PendingPlay::Pair {
                        audible_b,
                        fraction,
                        paused,
                    } => match (&src_a, &src_b) {
                        (
                            AudioSrc::Ready {
                                hex: ha,
                                path: pa,
                                total_ms,
                            },
                            AudioSrc::Ready {
                                hex: hb, path: pb, ..
                            },
                        ) => {
                            let start = (fraction * *total_ms as f32) as u64;
                            p.play_pair(ha, pa, hb, pb, *total_ms, start, audible_b);
                            if paused {
                                p.toggle_pause();
                            }
                            self.loaded_rate = self.rate;
                            self.audio_active = Some(usize::from(audible_b));
                        }
                        (AudioSrc::None, _) | (_, AudioSrc::None) => {}
                        _ => {
                            self.pending_play = Some(pend);
                            ctx.request_repaint_after(std::time::Duration::from_millis(150));
                        }
                    },
                    PendingPlay::Single {
                        left,
                        fraction,
                        paused,
                    } => {
                        let src = if left { &src_a } else { &src_b };
                        match src {
                            AudioSrc::Ready {
                                hex,
                                path,
                                total_ms,
                            } => {
                                let start = (fraction * *total_ms as f32) as u64;
                                if paused {
                                    p.load_paused(hex, path, *total_ms, start);
                                } else {
                                    p.play(hex, path, *total_ms, start);
                                }
                                self.loaded_rate = self.rate;
                                self.audio_active = Some(usize::from(!left));
                            }
                            AudioSrc::None => {}
                            AudioSrc::Rendering => {
                                self.pending_play = Some(pend);
                                ctx.request_repaint_after(std::time::Duration::from_millis(150));
                            }
                        }
                    }
                }
            }
        }
        picked
    }
}

/// Which save actions the viewer offers for a turned image on one side. A locked
/// repo protects its *existing* files but permits adding *new* ones, so writing a
/// copy is always available once there are edits; only overwriting the original
/// in place is gated by the lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SaveActions {
    copy: bool,
    overwrite: bool,
}

fn save_actions(read_only: bool, has_edits: bool) -> SaveActions {
    SaveActions {
        copy: has_edits,
        overwrite: has_edits && !read_only,
    }
}

/// The Text tab's single-file body for one side: a document's *extracted* words
/// when the file is one we can read (PDF/office/email), a short note when such a
/// document yields nothing, and otherwise the raw head or hex dump via
/// [`load_text_preview`]. The extracted text is the words as written, not the
/// normalized form used for dedup hashing.
fn document_preview(facts: &FileFacts) -> crate::lightbox::TextPreview {
    use crate::lightbox::TextPreview;
    let doc_mime = facts
        .mime
        .as_deref()
        .filter(|m| dedup_core::fingerprint::is_extractable_document(m));
    if let Some(mime) = doc_mime {
        return match dedup_core::fingerprint::extract_document_text(&facts.abs_path, mime) {
            Some(body) => TextPreview {
                body,
                is_text: true,
                truncated: false,
                error: None,
            },
            None => TextPreview {
                body: "Nothing readable here — this document may be scanned, encrypted, \
                       or empty."
                    .to_string(),
                is_text: true,
                truncated: false,
                error: None,
            },
        };
    }
    load_text_preview(&facts.abs_path)
}

/// The Strings tab's body for one side: the printable runs (≥4 chars) embedded
/// in the file's bytes, one per line, or a short note when there are none. Reads
/// a bounded head so a huge file can't exhaust memory.
fn strings_body(facts: &FileFacts) -> String {
    let capped = crate::hexdiff::read_capped(&facts.abs_path);
    let runs = dedup_core::strings::printable_strings(&capped.bytes, 4, 4000);
    if runs.is_empty() {
        "No readable runs found in this file's bytes.".to_string()
    } else {
        runs.join("\n")
    }
}

/// Tag names whose value differs between the two sides: present on both with a
/// different value, or present on only one. Drives the Metadata tab's "differs"
/// highlight.
fn differing_exif_tags(
    a: &[(String, String)],
    b: &[(String, String)],
) -> std::collections::HashSet<String> {
    let amap: std::collections::HashMap<&str, &str> =
        a.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let bmap: std::collections::HashMap<&str, &str> =
        b.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let mut out = std::collections::HashSet::new();
    for (k, v) in a {
        if bmap.get(k.as_str()) != Some(&v.as_str()) {
            out.insert(k.clone());
        }
    }
    for (k, _) in b {
        if !amap.contains_key(k.as_str()) {
            out.insert(k.clone());
        }
    }
    out
}

/// A human-readable metadata sidecar: a header naming the file, then each field
/// as `Tag: value`. The goal is preserving the *information* before a copy is
/// deleted, not reconstructing the exact bytes.
fn metadata_sidecar(name: &str, fields: &[(String, String)]) -> String {
    let header = format!("Metadata for {name}");
    let mut out = String::new();
    out.push_str(&header);
    out.push('\n');
    out.push_str(&"=".repeat(header.len().min(72)));
    out.push('\n');
    if fields.is_empty() {
        out.push_str("(no metadata)\n");
    }
    for (tag, value) in fields {
        out.push_str(&format!("{tag}: {value}\n"));
    }
    out
}

/// Write `dir/name`, appending `.N` before it collides with an existing file, so
/// a sidecar export never clobbers something already there.
fn non_colliding_sidecar(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let mut dest = dir.join(name);
    let mut n = 1;
    while dest.exists() {
        dest = dir.join(format!("{name}.{n}"));
        n += 1;
    }
    dest
}

/// Thickness of the filename frame's right cap and its bottom bar.
const FRAME_CAP_W: f32 = 8.0;
const FRAME_FOOT_H: f32 = 4.0;

/// The LCARS "elbow" that underlines a filename: a bar along the bottom that
/// rises, at the right, into a matching cap — the classic elbow corner. It
/// carries no title (it is chrome, not a label) and is painted behind the name,
/// in the side's own accent colour. `rect` is the framed area (the name row plus
/// the bottom bar).
fn filename_frame(rect: Rect, accent: egui::Color32) -> Vec<egui::Shape> {
    const R: u8 = 5; // outer corner radius
    let right = Rect::from_min_max(egui::pos2(rect.max.x - FRAME_CAP_W, rect.min.y), rect.max);
    let foot = Rect::from_min_max(egui::pos2(rect.min.x, rect.max.y - FRAME_FOOT_H), rect.max);
    vec![
        // Bottom bar: rounded on its free left end; the right end runs under the
        // cap, which overpaints it, so the join reads as a single elbow.
        egui::Shape::rect_filled(
            foot,
            egui::CornerRadius {
                nw: R,
                ne: 0,
                sw: R,
                se: 0,
            },
            accent,
        ),
        // Right cap: rounded top-right (free) and bottom-right (the outer elbow).
        egui::Shape::rect_filled(
            right,
            egui::CornerRadius {
                nw: 0,
                ne: R,
                sw: 0,
                se: R,
            },
            accent,
        ),
    ]
}

/// One side's read-only identity block: a bordered repo chip, the file name, and
/// its size / date / type — with the bigger-or-older value highlighted so the
/// difference reads without comparing both numbers. This is *what the file is*;
/// every action lives in the fixed bottom bar ([`side_actions`]), so the top
/// strip never carries a button that a long filename could shove.
fn side_strip(ui: &mut egui::Ui, side: &DiffSide, other: &DiffSide, is_left: bool) {
    ui.vertical(|ui| {
        // The bordered repo chip — the same widget every other tab's repo
        // selector uses, so one repo reads with one identity everywhere (the
        // bare identicon+label here used to be the odd one out, unbordered).
        // The file's facts (size · date · type) ride on the same row behind the
        // chip: they are short, so stacking them each on their own line only
        // ate viewport height.
        let accent = if is_left {
            theme::orange()
        } else {
            theme::blue()
        };
        let size_color = if side.facts.size > other.facts.size {
            theme::green()
        } else {
            theme::text()
        };
        // Prefer the *older* copy: in inheritance triage the earlier file is the
        // more original, so age (not recency) is the "better" cue. Equal dates
        // green neither side.
        let date_color = if side.facts.modified_ms < other.facts.modified_ms {
            theme::green()
        } else {
            theme::text()
        };
        ui.horizontal(|ui| {
            crate::repo_chip::repo_chip(ui, &side.repo, false, accent, false, Some(side.read_only));
            ui.add_space(6.0);
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
        });
        ui.add_space(3.0);
        // The file name is the headline of the identity block — which file am I
        // about to act on — bold and a step larger than the facts below,
        // underlined by an LCARS elbow in the side's colour (a bottom bar rising
        // into a cap on the right). A very long name never grows the layout: it
        // lives in a fixed-width horizontal scroll — with the right cap reserved
        // so a long name can't shove it off — that sticks to the *end* (the
        // filename + extension you care about), and the label is selectable so
        // you can drag to the front and copy the whole path.
        let bg = ui.painter().add(egui::Shape::Noop);
        let full_w = ui.available_width();
        let row = ui.horizontal(|ui| {
            ui.add_space(2.0);
            let name_w = (full_w - FRAME_CAP_W - 10.0).max(40.0);
            egui::ScrollArea::horizontal()
                .id_salt(("filename", is_left))
                .max_width(name_w)
                .stick_to_right(true)
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(&side.rel_path)
                                .color(theme::text())
                                .size(15.0)
                                .strong(),
                        )
                        .wrap_mode(egui::TextWrapMode::Extend)
                        .selectable(true),
                    );
                });
        });
        // Span the frame across the whole strip (a short name leaves the row
        // narrow) and extend it below the text for the bottom bar, then paint it
        // behind the name.
        let mut frame = row.response.rect;
        frame.max.x = frame.min.x + full_w;
        frame.max.y += FRAME_FOOT_H;
        ui.painter()
            .set(bg, egui::Shape::Vec(filename_frame(frame, accent)));
        ui.add_space(FRAME_FOOT_H + 2.0);
    });
}

/// One side's actions for the fixed bottom bar: the caller's own — a deletion
/// mark pill when `mark` is supplied (Duplicates), else the DIFF board's
/// OVERWRITE OTHER / DELETE commands. Drawn in whatever layout the caller sets
/// (a right-to-left parent right-aligns it against the pane edge). Returns the
/// chosen action, if any.
fn side_actions(
    ui: &mut egui::Ui,
    is_left: bool,
    verbosity: TooltipVerbosity,
    mark: Option<(&str, MarkPill)>,
) -> Option<DiffPick> {
    let mut pick = None;
    // A caller that supplied marks acts through the pill alone.
    if let Some((label, pill)) = mark {
        if crate::lightbox::mark_pill(ui, verbosity, label, pill.marked, pill.markable) {
            pick = Some(DiffPick::ToggleMark { on_left: is_left });
        }
        return pick;
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
                egui::Button::new(RichText::new("DELETE").color(theme::ink_on(theme::red())))
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
    pick
}

/// Format a 0–1 perceptual similarity as a compact percent label: `0.94 → "94%"`.
/// The ceiling reads `"99%+"`, never "identical" or "100%" — this readout exists
/// precisely to distinguish a *perceptual* match from a byte-for-byte duplicate,
/// so it must not borrow the duplicate vocabulary at the top of its range.
fn similarity_label(similarity: f32) -> String {
    let pct = (similarity.clamp(0.0, 1.0) * 100.0).round() as i32;
    if pct >= 100 {
        "99%+".to_string()
    } else {
        format!("{pct}%")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// The metadata diff names every tag that differs — changed on both sides,
    /// or present on only one — and nothing that matches.
    #[test]
    fn differing_exif_tags_names_only_the_differences() {
        let a = vec![
            ("Make".to_string(), "Canon".to_string()),
            ("Model".to_string(), "5D".to_string()),
            ("Title".to_string(), "Beach".to_string()),
        ];
        let b = vec![
            ("Make".to_string(), "Canon".to_string()), // same
            ("Model".to_string(), "6D".to_string()),   // changed
            ("Author".to_string(), "Sam".to_string()), // only on b
        ];
        let diff = differing_exif_tags(&a, &b);
        assert!(!diff.contains("Make"), "an identical field is not flagged");
        assert!(diff.contains("Model"), "a changed field is flagged");
        assert!(
            diff.contains("Title"),
            "a field only on the left is flagged"
        );
        assert!(
            diff.contains("Author"),
            "a field only on the right is flagged"
        );
        assert_eq!(diff.len(), 3);
    }

    /// The per-pair similarity reads as a whole percent, and the near-ceiling
    /// case as "identical" — the vocabulary that explains why a loosely matched
    /// pair is grouped without posing as an exact duplicate.
    #[test]
    fn similarity_label_reads_as_percent_never_identical() {
        assert_eq!(similarity_label(0.94), "94%");
        assert_eq!(similarity_label(0.601), "60%");
        // The ceiling never borrows the duplicate vocabulary ("identical" /
        // "100%") — it must not read as a byte-for-byte duplicate.
        assert_eq!(similarity_label(0.995), "99%+");
        assert_eq!(similarity_label(1.0), "99%+");
        // Out-of-range input clamps rather than printing nonsense.
        assert_eq!(similarity_label(1.4), "99%+");
        assert_eq!(similarity_label(-0.3), "0%");
    }

    /// The sidecar is human-readable: a header naming the file, then `Tag: value`
    /// lines carrying the information.
    #[test]
    fn metadata_sidecar_is_readable_and_names_the_file() {
        let fields = vec![
            ("Title".to_string(), "Beach".to_string()),
            ("Author".to_string(), "Sam".to_string()),
        ];
        let text = metadata_sidecar("photo.tif", &fields);
        assert!(text.contains("Metadata for photo.tif"));
        assert!(text.contains("Title: Beach"));
        assert!(text.contains("Author: Sam"));
        // Empty metadata is stated, not a blank file.
        assert!(metadata_sidecar("x.jpg", &[]).contains("(no metadata)"));
    }

    /// A locked repo protects its existing files but permits adding new ones, so
    /// a copy is always offered once there are edits; only overwriting in place
    /// is gated by the lock.
    #[test]
    fn save_actions_allow_a_copy_when_locked_but_not_an_overwrite() {
        // Nothing edited: nothing to save.
        assert_eq!(
            save_actions(false, false),
            SaveActions {
                copy: false,
                overwrite: false
            }
        );
        // Unlocked with edits: both a copy and an in-place overwrite.
        assert_eq!(
            save_actions(false, true),
            SaveActions {
                copy: true,
                overwrite: true
            }
        );
        // Locked with edits: a copy is fine (it only adds), overwrite is not.
        assert_eq!(
            save_actions(true, true),
            SaveActions {
                copy: true,
                overwrite: false
            }
        );
    }

    /// An audio side with its own identity, for the transport tests.
    fn audio_side(name: &str, hex: &str) -> DiffSide {
        let mut side = diff_side(Some("audio/mpeg"));
        side.rel_path = name.to_string();
        side.facts.abs_path = PathBuf::from(format!("/tmp/{name}"));
        side.facts.hash_hex = hex.to_string();
        side.facts.audio_ms = Some(1000);
        side
    }

    /// A video side with its own identity, for the Video tab tests. The path
    /// need not exist: without a decodable clip (or without ffmpeg) the
    /// filmstrip keeps placeholder slots — the no-crash fallback.
    fn video_side(name: &str, hex: &str) -> DiffSide {
        let mut side = diff_side(Some("video/mp4"));
        side.rel_path = name.to_string();
        side.facts.abs_path = PathBuf::from(format!("/tmp/{name}"));
        side.facts.hash_hex = hex.to_string();
        side
    }

    /// A video pair lands on the Video representation and lays out the full
    /// aligned filmstrip — N frame slots per side, in a row, side by side —
    /// even before (or without) any frame decoding. Asserted as rects, per the
    /// GUI convention that a label query passes even when clipped.
    #[test]
    fn a_video_pair_lands_on_video_with_a_filmstrip_per_side() {
        let h = rendered(DiffCompare::new(
            video_side("a.mp4", "aaaa"),
            video_side("b.mp4", "bbbb"),
        ));
        assert_eq!(
            h.state().tab,
            RepresentationKind::Video,
            "a clip's own representation is Video, not its soundtrack"
        );
        let n = crate::scrub::FILMSTRIP_FRAMES;
        let window = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1200.0, 800.0));
        for slot in [0, 1] {
            let rects = &h.state().film_rects[slot];
            assert_eq!(rects.len(), n, "side {slot} lays out {n} frame slots");
            for w in rects.windows(2) {
                assert!(
                    w[0].right() <= w[1].left() + 0.5,
                    "slots sit in a row without overlap: {w:?}"
                );
            }
            for r in rects {
                assert!(
                    r.width() > 20.0 && r.height() > 20.0,
                    "a slot is a visible frame, not a sliver: {r:?}"
                );
                assert!(window.contains_rect(*r), "slots stay on screen: {r:?}");
            }
        }
        let a_right = h.state().film_rects[0].last().expect("A strip").right();
        let b_left = h.state().film_rects[1].first().expect("B strip").left();
        assert!(
            a_right <= b_left,
            "the two strips sit side by side, A's before B's"
        );
    }

    /// Clicking the filmstrip drops the *shared* playhead: one fraction, which
    /// each side decodes against its own timeline — the proportional-alignment
    /// wiring on top of the unit-tested mapping in `scrub`.
    #[test]
    fn clicking_the_filmstrip_drops_the_shared_playhead() {
        let mut h = rendered(DiffCompare::new(
            video_side("a.mp4", "aaaa"),
            video_side("b.mp4", "bbbb"),
        ));
        // The centre of A's fourth slot is 3.5/8 of the strip.
        let pos = h.state().film_rects[0][3].center();
        h.event(egui::Event::PointerMoved(pos));
        h.event(egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        });
        h.step();
        h.event(egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        });
        h.step();
        h.step();
        let f = h
            .state()
            .playhead
            .expect("the click set the shared playhead");
        assert!(
            (f - 3.5 / 8.0).abs() < 0.02,
            "the fraction matches the clicked spot, got {f}"
        );
        // Both sides answer the same fraction — the alignment guarantee.
        assert_eq!(h.state().video[0].scrub_frac, Some(f));
        assert_eq!(h.state().video[1].scrub_frac, Some(f));
        // And the representation reports the slot under the playhead.
        let (l, _) = h.state().reps();
        assert_eq!(l.video.expect("video rep").selected_frame, Some(3));
    }

    /// With ffmpeg present, a clip that carries a soundtrack offers the Audio
    /// representation beside Video — and a silent clip never does. Gated like
    /// every other ffmpeg test: machines without it skip rather than fail.
    #[test]
    fn a_video_with_a_soundtrack_offers_the_audio_tab_and_a_silent_one_does_not() {
        use egui_kittest::kittest::Queryable;
        if !dedup_core::fingerprint::ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().expect("dir");
        let sound = dir.path().join("sound.mp4");
        let silent = dir.path().join("silent.mp4");
        let ok = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg("testsrc=duration=1:size=64x64:rate=5")
            .args(["-f", "lavfi", "-i"])
            .arg("sine=frequency=440:duration=1")
            .args(["-pix_fmt", "yuv420p", "-c:a", "aac", "-shortest"])
            .arg(&sound)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
            && std::process::Command::new("ffmpeg")
                .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
                .arg("testsrc=duration=1:size=64x64:rate=5")
                .args(["-pix_fmt", "yuv420p"])
                .arg(&silent)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
        if !ok {
            eprintln!("skipping: ffmpeg could not generate the test clips");
            return;
        }
        dedup_core::thumbnail::set_cache_dir(dir.path().join("thumbs"));
        let side = |path: &std::path::Path, hex: &str| {
            let mut s = diff_side(Some("video/mp4"));
            s.rel_path = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            s.facts.abs_path = path.to_path_buf();
            s.facts.hash_hex = hex.to_string();
            s
        };
        // The soundtrack probe and extraction run off-thread; step the UI
        // until they settle (bounded — this is not a hang-forever loop).
        let settle = |h: &mut egui_kittest::Harness<'static, DiffCompare>| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while h.state().video[0].audio.is_none() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the soundtrack probe never settled"
                );
                std::thread::sleep(std::time::Duration::from_millis(50));
                h.step();
            }
        };

        let mut h = rendered(DiffCompare::new(
            side(&sound, "feed01"),
            side(&sound, "feed02"),
        ));
        settle(&mut h);
        let (l, _) = h.state().reps();
        assert!(
            l.audio.is_some(),
            "a clip with a soundtrack offers the Audio representation"
        );
        assert_eq!(
            h.state().tab,
            RepresentationKind::Video,
            "Video stays the landing tab, Audio sits beside it"
        );
        h.run();
        // The tab is really offered on the bar, not merely present in a struct.
        h.get_by_label_contains("Audio");

        let mut h = rendered(DiffCompare::new(
            side(&silent, "feed03"),
            side(&silent, "feed04"),
        ));
        settle(&mut h);
        assert_eq!(
            h.state().video[0].audio,
            Some(None),
            "the probe found no track"
        );
        let (l, _) = h.state().reps();
        assert!(l.audio.is_none(), "a silent clip offers no Audio tab");
        assert_eq!(
            h.query_all_by_label_contains("Audio").count(),
            0,
            "and no Audio tab button is drawn"
        );
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

    /// Sync a fixture side's recorded size with the bytes actually on disk, so
    /// a doc screenshot shows the file's real size instead of the placeholder
    /// `1 B` the bare [`diff_side`] carries.
    fn sync_size(side: &mut DiffSide) {
        if let Ok(m) = std::fs::metadata(&side.facts.abs_path) {
            side.facts.size = m.len();
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

    /// A `.docx` side on disk whose single paragraph is `word`.
    fn docx_side(dir: &Path, name: &str, word: &str) -> DiffSide {
        use std::io::Write;
        let path = dir.join(name);
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
        zip.start_file("word/document.xml", opts).unwrap();
        zip.write_all(
            format!(
                "<w:document><w:body><w:p><w:r><w:t>{word}</w:t></w:r>\
                 </w:p></w:body></w:document>"
            )
            .as_bytes(),
        )
        .unwrap();
        zip.finish().unwrap();
        let mut side = named_side(name);
        side.facts.abs_path = path;
        side.facts.mime =
            Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document".into());
        side
    }

    /// Author a valid one-page PDF (a filled rectangle in the given colour) so
    /// `pdftoppm` accepts it — for the Render tab.
    fn tiny_pdf(path: &Path, r: f64, g: f64, b: f64) {
        use lopdf::content::{Content, Operation};
        use lopdf::{Document, Object, Stream, dictionary};
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let content = Content {
            operations: vec![
                Operation::new("rg", vec![r.into(), g.into(), b.into()]),
                Operation::new("re", vec![20.into(), 20.into(), 200.into(), 160.into()]),
                Operation::new("f", vec![]),
            ],
        };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "MediaBox" => vec![0.into(), 0.into(), 240.into(), 200.into()],
        });
        let pages = dictionary! {
            "Type" => "Pages",
            "Kids" => vec![page_id.into()],
            "Count" => 1,
        };
        doc.objects.insert(pages_id, Object::Dictionary(pages));
        let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog_id);
        doc.save(path).unwrap();
    }

    /// A PDF side on disk, a one-page document in the given colour.
    fn pdf_side(dir: &Path, name: &str, rgb: (f64, f64, f64)) -> DiffSide {
        let path = dir.join(name);
        tiny_pdf(&path, rgb.0, rgb.1, rgb.2);
        let mut side = named_side(name);
        side.facts.abs_path = path;
        side.facts.mime = Some("application/pdf".into());
        sync_size(&mut side);
        side
    }

    /// A DiffSide over a real document from the doc-media folder (a genuine
    /// page to rasterize / extract), falling back to the synthetic colored
    /// `pdf_side` when no doc media is configured.
    fn pdf_side_media(dir: &Path, asset: &str, fallback_rgb: (f64, f64, f64)) -> DiffSide {
        let dest = dir.join(asset);
        if crate::doc_media::available() && crate::doc_media::place(asset, &dest) {
            let mut side = named_side(asset);
            side.facts.abs_path = dest.clone();
            side.facts.mime = Some("application/pdf".into());
            side.facts.size = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(1);
            side
        } else {
            pdf_side(dir, asset, fallback_rgb)
        }
    }

    /// The Render tab is offered for a PDF and not for a file that cannot be
    /// rasterized — keyed on mime, so no valid document is needed here.
    #[test]
    fn a_pdf_offers_a_render_tab_and_an_image_does_not() {
        let mut pdf = named_side("doc.pdf");
        pdf.facts.mime = Some("application/pdf".into());
        let cmp = DiffCompare::new_with_pool(pdf, None, Vec::new());
        assert!(
            cmp.reps()
                .0
                .available_kinds()
                .contains(&RepresentationKind::Render),
            "a PDF offers the Render tab"
        );

        let mut jpg = named_side("photo.jpg");
        jpg.facts.mime = Some("image/jpeg".into());
        let cmp2 = DiffCompare::new_with_pool(jpg, None, Vec::new());
        assert!(
            !cmp2
                .reps()
                .0
                .available_kinds()
                .contains(&RepresentationKind::Render),
            "an image is not rasterized to pages — no Render tab"
        );
    }

    /// A plain-text side on disk.
    fn text_side(dir: &Path, name: &str, body: &[u8]) -> DiffSide {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        let mut side = named_side(name);
        side.facts.abs_path = path;
        side.facts.mime = Some("text/plain".into());
        sync_size(&mut side);
        side
    }

    /// After the tab split, a text file has *both* a Text tab (its words) and an
    /// always-present Hex tab (its raw bytes) — the byte view no longer hides
    /// inside Text.
    #[test]
    fn a_text_file_has_both_a_text_and_a_hex_view() {
        use egui_kittest::kittest::Queryable;
        let tmp = tempfile::tempdir().unwrap();
        let side = text_side(tmp.path(), "notes.txt", b"visible words here in this file");
        let mut cmp = DiffCompare::new_with_pool(side, None, Vec::new());
        cmp.hide_second();
        cmp.tab = RepresentationKind::Text;
        let mut h = rendered(cmp);
        assert!(
            h.query_all_by_label_contains("visible words here").count() > 0,
            "the Text tab shows the file's words"
        );
        h.get_by_label_contains("Hex").click();
        h.run();
        assert!(
            h.query_all_by_label_contains("00000000").count() > 0,
            "the Hex tab shows a byte dump (offsets), even for a text file"
        );
    }

    /// Two text files compared on the Text tab diff as *content* (their words,
    /// aligned), not as a hex dump — this falls out of the split for free.
    #[test]
    fn two_text_files_diff_as_content_not_bytes() {
        use egui_kittest::kittest::Queryable;
        let tmp = tempfile::tempdir().unwrap();
        let mut cmp = DiffCompare::new_with_pool(
            text_side(tmp.path(), "a.txt", b"alpha heading\nthe shared body line"),
            Some(text_side(
                tmp.path(),
                "b.txt",
                b"bravo heading\nthe shared body line",
            )),
            Vec::new(),
        );
        cmp.tab = RepresentationKind::Text;
        let h = rendered(cmp);
        assert!(
            h.query_all_by_label_contains("alpha heading").count() > 0,
            "left file's line is shown as text"
        );
        assert!(
            h.query_all_by_label_contains("bravo heading").count() > 0,
            "right file's line is shown as text, aligned beside it"
        );
    }

    /// A binary side on disk holding raw `bytes` — for the Strings tab.
    fn bytes_side(dir: &Path, name: &str, bytes: &[u8]) -> DiffSide {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        let mut side = named_side(name);
        side.facts.abs_path = path;
        side.facts.mime = Some("application/octet-stream".into());
        sync_size(&mut side);
        side
    }

    /// The Strings tab surfaces the printable runs embedded in a file's bytes.
    #[test]
    fn strings_tab_shows_embedded_printable_runs() {
        use egui_kittest::kittest::Queryable;
        let tmp = tempfile::tempdir().unwrap();
        let side = bytes_side(
            tmp.path(),
            "blob.bin",
            b"\x00\x01CONFIG_TOKEN_abcdef\x00\xff\x02",
        );
        let mut cmp = DiffCompare::new_with_pool(side, None, Vec::new());
        cmp.hide_second();
        cmp.tab = RepresentationKind::Strings;
        let h = rendered(cmp);
        assert!(
            h.query_all_by_label_contains("CONFIG_TOKEN_abcdef").count() > 0,
            "the embedded printable run is shown on the Strings tab"
        );
    }

    /// Comparing two files on the Strings tab aligns their runs and shows both
    /// sides' distinct embedded text.
    #[test]
    fn comparing_two_files_strings_diffs_their_runs() {
        use egui_kittest::kittest::Queryable;
        let tmp = tempfile::tempdir().unwrap();
        let a = bytes_side(
            tmp.path(),
            "a.bin",
            b"\x00SHARED_MARKER_xyz\x00ONLY_IN_A_123\x00",
        );
        let b = bytes_side(
            tmp.path(),
            "b.bin",
            b"\x00SHARED_MARKER_xyz\x00ONLY_IN_B_456\x00",
        );
        let mut cmp = DiffCompare::new_with_pool(a, Some(b), Vec::new());
        cmp.tab = RepresentationKind::Strings;
        let h = rendered(cmp);
        assert!(
            h.query_all_by_label_contains("ONLY_IN_A_123").count() > 0,
            "left file's distinct run is shown"
        );
        assert!(
            h.query_all_by_label_contains("ONLY_IN_B_456").count() > 0,
            "right file's distinct run is shown alongside"
        );
    }

    /// Stepping the shown file to another pool member refreshes the Strings tab
    /// to the new file's runs — the per-side strings cache invalidates on a step,
    /// the same way the Text and image caches do.
    #[test]
    fn stepping_refreshes_the_strings_tab() {
        use egui_kittest::kittest::Queryable;
        let tmp = tempfile::tempdir().unwrap();
        let pool = vec![
            bytes_side(tmp.path(), "a.bin", b"\x00ALPHA_RUN_marker\x00"),
            bytes_side(tmp.path(), "b.bin", b"\x00BRAVO_RUN_marker\x00"),
        ];
        let mut cmp = DiffCompare::new_with_pool(
            bytes_side(tmp.path(), "a.bin", b"\x00ALPHA_RUN_marker\x00"),
            None,
            pool,
        );
        cmp.hide_second();
        cmp.tab = RepresentationKind::Strings;
        let mut h = rendered(cmp);
        assert!(
            h.query_all_by_label_contains("ALPHA_RUN_marker").count() > 0,
            "the first file's run shows to start"
        );
        h.key_press(egui::Key::ArrowRight);
        h.run();
        assert!(
            h.query_all_by_label_contains("BRAVO_RUN_marker").count() > 0,
            "after stepping, the new file's run shows"
        );
        assert_eq!(
            h.query_all_by_label_contains("ALPHA_RUN_marker").count(),
            0,
            "the previous file's run is gone — the cache refreshed"
        );
    }

    /// An `.eml` side on disk with the given subject and (possibly multi-line)
    /// body — its extracted text keeps the body's line breaks.
    fn eml_side(dir: &Path, name: &str, subject: &str, body: &str) -> DiffSide {
        let content = format!("From: a@example.com\r\nSubject: {subject}\r\n\r\n{body}");
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        let mut side = named_side(name);
        side.facts.abs_path = path;
        side.facts.mime = Some("message/rfc822".into());
        sync_size(&mut side);
        side
    }

    /// Doc screenshot: two PDFs rendered to pages, side by side on the Render
    /// tab, to `docs/screenshots/render.png`. `--ignored` (needs wgpu + pdftoppm).
    #[test]
    #[ignore = "generates a doc screenshot (needs wgpu + pdftoppm)"]
    fn doc_screenshot_render() {
        if !dedup_core::render::pdftoppm_available() {
            eprintln!("skipping: pdftoppm not on PATH");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let mut cmp = DiffCompare::new_with_pool(
            pdf_side_media(tmp.path(), "menu.pdf", (0.20, 0.35, 0.70)),
            Some(pdf_side_media(
                tmp.path(),
                "visa_contract.pdf",
                (0.75, 0.30, 0.20),
            )),
            Vec::new(),
        );
        cmp.tab = RepresentationKind::Render;
        let mut init = false;
        let mut h = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1100.0, 620.0))
            .wgpu()
            .build_ui_state(
                move |ui, cmp: &mut DiffCompare| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::LIGHT);
                        init = true;
                    }
                    cmp.view(&ui.ctx().clone(), TooltipVerbosity::default(), None);
                },
                cmp,
            );
        // Both pages rasterize on worker threads; pump with `step()` (not
        // `run()` — the "Rendering…" note requests a repaint each frame, which
        // `run()` would treat as never settling) until both textures land.
        for _ in 0..100 {
            if h.state().render_tex.iter().all(Option::is_some) {
                break;
            }
            h.step();
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        h.step();
        let img = h.render().expect("wgpu render failed");
        let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/screenshots/render.png");
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());
    }

    /// Doc screenshot: two clips on the Video tab — a filmstrip per side with
    /// the shared playhead dropped and the frame at that moment enlarged as
    /// A@t | B@t — to `docs/screenshots/video-diff.png`. `--ignored` (needs
    /// wgpu + ffmpeg).
    #[test]
    #[ignore = "generates a doc screenshot (needs wgpu + ffmpeg)"]
    fn doc_screenshot_video_diff() {
        if !dedup_core::fingerprint::ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let make_clip = |name: &str, src: &str| {
            let path = tmp.path().join(name);
            let ok = std::process::Command::new("ffmpeg")
                .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
                .arg(src)
                .args(["-pix_fmt", "yuv420p"])
                .arg(&path)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            assert!(ok, "ffmpeg generated {name}");
            path
        };
        let a = make_clip("a.mp4", "testsrc=duration=4:size=320x180:rate=10");
        let b = make_clip("b.mp4", "testsrc2=duration=3:size=320x180:rate=10");
        dedup_core::thumbnail::set_cache_dir(tmp.path().join("thumbs"));
        let side = |path: &std::path::Path, hex: &str| {
            let mut s = diff_side(Some("video/mp4"));
            s.rel_path = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            s.facts.abs_path = path.to_path_buf();
            s.facts.hash_hex = hex.to_string();
            s
        };
        let mut cmp = DiffCompare::new(side(&a, "shot0a"), side(&b, "shot0b"));
        cmp.tab = RepresentationKind::Video;
        let mut init = false;
        let mut h = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1100.0, 620.0))
            .wgpu()
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
        // Let the filmstrip decodes land, then drop the playhead mid-strip.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while h.state().video[0].film.iter().any(Option::is_none)
            || h.state().video[1].film.iter().any(Option::is_none)
        {
            assert!(
                std::time::Instant::now() < deadline,
                "filmstrips never finished decoding"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
            h.step();
        }
        let pos = h.state().film_rects[0][5].center();
        h.event(egui::Event::PointerMoved(pos));
        h.event(egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        });
        h.step();
        h.event(egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !(h.state().video[0].scrub_settled && h.state().video[1].scrub_settled) {
            assert!(
                std::time::Instant::now() < deadline,
                "the scrubbed frames never decoded"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
            h.step();
        }
        h.run();
        let img = h.render().expect("wgpu render failed");
        let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/screenshots/video-diff.png");
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());
    }

    /// Doc screenshot: the Strings tab surfacing a binary's embedded runs, to
    /// `docs/screenshots/strings.png`. `--ignored` (needs wgpu).
    #[test]
    #[ignore = "generates a doc screenshot (needs wgpu)"]
    fn doc_screenshot_strings() {
        let tmp = tempfile::tempdir().unwrap();
        let side = bytes_side(
            tmp.path(),
            "firmware.bin",
            b"\x00\x01Copyright ACME 2021\x00\x00/usr/local/bin/agent\x00\xff\
              version 3.4.1 build 8892\x00\x02\x03config=/etc/agent.conf\x00",
        );
        let mut cmp = DiffCompare::new_with_pool(side, None, Vec::new());
        cmp.hide_second();
        cmp.tab = RepresentationKind::Strings;
        let mut init = false;
        let mut h = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1100.0, 620.0))
            .wgpu()
            .build_ui_state(
                move |ui, cmp: &mut DiffCompare| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::LIGHT);
                        init = true;
                    }
                    cmp.view(&ui.ctx().clone(), TooltipVerbosity::default(), None);
                },
                cmp,
            );
        h.run();
        let img = h.render().expect("wgpu render failed");
        let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/screenshots/strings.png");
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());
    }

    /// Doc screenshot: the aligned content diff of two documents on the Text tab,
    /// to `docs/screenshots/content_diff.png`. `--ignored` (needs wgpu).
    #[test]
    #[ignore = "generates a doc screenshot (needs wgpu)"]
    fn doc_screenshot_content_diff() {
        let tmp = tempfile::tempdir().unwrap();
        let a = eml_side(
            tmp.path(),
            "a.eml",
            "Invoice 4471",
            "Dear Bob,\r\nAmount due is 100.\r\nRegards, Acme.",
        );
        let b = eml_side(
            tmp.path(),
            "b.eml",
            "Invoice 4471",
            "Dear Alice,\r\nAmount due is 100.\r\nPlease remit by Friday.\r\nRegards, Acme.",
        );
        let mut cmp = DiffCompare::new_with_pool(a, Some(b), Vec::new());
        cmp.tab = RepresentationKind::Text;
        let mut init = false;
        let mut h = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1100.0, 620.0))
            .wgpu()
            .build_ui_state(
                move |ui, cmp: &mut DiffCompare| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::LIGHT);
                        init = true;
                    }
                    cmp.view(&ui.ctx().clone(), TooltipVerbosity::default(), None);
                },
                cmp,
            );
        h.run();
        let img = h.render().expect("wgpu render failed");
        let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/screenshots/content_diff.png");
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());
    }

    /// Opening a document lands the Text tab on its *extracted words*, not a hex
    /// dump of the container bytes.
    #[test]
    fn a_document_text_tab_shows_extracted_words() {
        use egui_kittest::kittest::Queryable;
        let tmp = tempfile::tempdir().unwrap();
        let side = docx_side(tmp.path(), "report.docx", "Quarterly revenue climbed");
        let mut cmp = DiffCompare::new_with_pool(side, None, Vec::new());
        cmp.hide_second(); // one file, full width — not the mirrored two-up view
        cmp.tab = RepresentationKind::Text;
        let h = rendered(cmp);
        assert_eq!(
            h.query_all_by_label_contains("Quarterly revenue climbed")
                .count(),
            1,
            "one document, shown once — its extracted words on the Text tab",
        );
    }

    /// Comparing two documents shows both their extracted texts side by side —
    /// the readable content — rather than routing to the byte-level hex diff.
    #[test]
    fn two_documents_compare_their_extracted_text() {
        use egui_kittest::kittest::Queryable;
        let tmp = tempfile::tempdir().unwrap();
        let mut cmp = DiffCompare::new_with_pool(
            docx_side(tmp.path(), "a.docx", "Alpha manifest"),
            Some(docx_side(tmp.path(), "b.docx", "Bravo manifest")),
            Vec::new(),
        );
        cmp.tab = RepresentationKind::Text;
        let h = rendered(cmp);
        assert!(
            h.query_by_label_contains("Alpha manifest").is_some(),
            "left document's words are shown"
        );
        assert!(
            h.query_by_label_contains("Bravo manifest").is_some(),
            "right document's words are shown side by side"
        );
    }

    /// A read-only zip side on disk, holding `entries`.
    fn archive_side(dir: &Path, name: &str, entries: &[(&str, &[u8])]) -> DiffSide {
        use std::io::Write;
        let path = dir.join(name);
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (n, d) in entries {
            zip.start_file(*n, opts).unwrap();
            zip.write_all(d).unwrap();
        }
        zip.finish().unwrap();
        let mut side = named_side(name);
        side.facts.abs_path = path;
        side.facts.mime = Some("application/zip".into());
        side
    }

    /// A zip opens on its Archive tab listing its members; clicking a readable
    /// member opens it in place (rendered by its own type), and BACK/Esc
    /// returns to the member list. The source archive is never modified.
    #[test]
    fn a_zip_opens_on_the_archive_tab_and_a_member_opens_in_place() {
        use egui_kittest::kittest::Queryable;
        let tmp = tempfile::tempdir().unwrap();
        let side = archive_side(
            tmp.path(),
            "backup.zip",
            &[("notes.txt", b"hello archive"), ("readme.md", b"# readme")],
        );
        let zip_bytes_before = std::fs::read(&side.facts.abs_path).unwrap();
        let mut cmp = DiffCompare::new_with_pool(side, None, Vec::new());
        cmp.hide_second();
        let mut h = rendered(cmp);

        // Landed on the Archive tab, with the members listed.
        assert_eq!(
            h.state().tab,
            RepresentationKind::Archive,
            "a zip opens on its Archive representation"
        );
        assert!(
            h.query_by_label_contains("notes.txt").is_some(),
            "members are listed"
        );
        assert!(h.query_by_label_contains("readme.md").is_some());

        // Click a member: it opens in place; the shown file becomes the member.
        h.get_by_label_contains("notes.txt").click();
        h.run();
        assert_eq!(
            h.state().left.rel_path,
            "notes.txt",
            "the member is now the shown file"
        );
        assert!(
            h.state().member_return.is_some(),
            "the archive is remembered so BACK can return to it"
        );

        // BACK returns to the member list.
        h.get_by_label_contains("BACK").click();
        h.run();
        assert!(
            h.state().member_return.is_none(),
            "BACK returns to the archive"
        );
        assert_eq!(h.state().tab, RepresentationKind::Archive);
        assert!(
            h.query_by_label_contains("readme.md").is_some(),
            "the member list is shown again"
        );

        // The source archive is byte-identical after all that browsing.
        let path = h.state().left.facts.abs_path.clone();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            zip_bytes_before,
            "browsing never modifies the source archive"
        );
    }

    /// A read-only zip side with one AES-encrypted member.
    fn encrypted_archive_side(
        dir: &Path,
        name: &str,
        member: &str,
        data: &[u8],
        password: &str,
    ) -> DiffSide {
        use std::io::Write;
        let path = dir.join(name);
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .with_aes_encryption(zip::AesMode::Aes256, password);
        zip.start_file(member, opts).unwrap();
        zip.write_all(data).unwrap();
        zip.finish().unwrap();
        let mut side = named_side(name);
        side.facts.abs_path = path;
        side.facts.mime = Some("application/zip".into());
        side
    }

    /// A locked zip lists its (locked) member and offers UNLOCK; the wrong
    /// password is rejected, the right one unlocks it for the session.
    #[test]
    fn a_locked_zip_unlocks_with_the_supplied_password() {
        use egui_kittest::kittest::Queryable;
        let tmp = tempfile::tempdir().unwrap();
        let side = encrypted_archive_side(
            tmp.path(),
            "locked.zip",
            "secret.txt",
            b"classified",
            "sesame",
        );
        let mut cmp = DiffCompare::new_with_pool(side, None, Vec::new());
        cmp.hide_second();
        let mut h = rendered(cmp);

        assert_eq!(h.state().tab, RepresentationKind::Archive);
        assert!(
            h.query_by_label_contains("secret.txt").is_some(),
            "a locked member's name is still listed"
        );
        assert!(
            h.query_by_label_contains("UNLOCK").is_some(),
            "a locked archive offers UNLOCK"
        );
        assert!(
            h.query_by_label_contains("RECOVER").is_some(),
            "and a built-in RECOVER attempt"
        );
        assert!(
            h.query_by_label_contains("EXPORT HASH").is_some(),
            "and a hashcat hash export"
        );

        // Exporting copies a $zip2$ hash to the clipboard.
        h.get_by_label_contains("EXPORT HASH").click();
        h.run();
        assert!(
            h.state()
                .recover_note
                .as_deref()
                .is_some_and(|n| n.contains("13600")),
            "the export reports the hashcat mode"
        );

        // Wrong password: rejected, still locked.
        h.state_mut().unlock_input = "wrong".into();
        h.get_by_label_contains("UNLOCK").click();
        h.run();
        assert!(h.state().unlock_password.is_none());
        assert!(h.state().unlock_failed, "the wrong password is reported");

        // Right password: unlocked for the session.
        h.state_mut().unlock_input = "sesame".into();
        h.get_by_label_contains("UNLOCK").click();
        h.run();
        assert_eq!(
            h.state().unlock_password.as_deref(),
            Some("sesame"),
            "the verified password is held for the session"
        );
    }

    /// The Archive tab offers extraction — a whole-archive EXTRACT ALL and a
    /// per-member extract control. (The folder picker and the extraction itself
    /// are exercised at the core level; here we assert the controls exist.)
    #[test]
    fn the_archive_tab_offers_extraction_controls() {
        use egui_kittest::kittest::Queryable;
        let tmp = tempfile::tempdir().unwrap();
        let side = archive_side(tmp.path(), "backup.zip", &[("notes.txt", b"hi")]);
        let mut cmp = DiffCompare::new_with_pool(side, None, Vec::new());
        cmp.hide_second();
        let h = rendered(cmp);
        assert_eq!(
            h.state().tab,
            RepresentationKind::Archive,
            "opens on the Archive tab"
        );
        assert!(
            h.query_by_label_contains("EXTRACT ALL").is_some(),
            "a whole-archive extract control is offered"
        );
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
            h.query_all_by_label(crate::icon::CARET_RIGHT).count(),
            2,
            "one compact forward caret per side"
        );
        assert_eq!(
            h.query_all_by_label(crate::icon::CARET_LEFT).count(),
            2,
            "and one back caret per side"
        );
        // The loud PREV/NEXT pills are gone.
        assert_eq!(h.query_all_by_label_contains("NEXT").count(), 0);
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

        // A's forward caret is the first one drawn (A's bar, left to right).
        {
            let carets: Vec<_> = h.query_all_by_label(crate::icon::CARET_RIGHT).collect();
            carets[0].click();
        }
        h.run();

        assert_eq!(
            h.state().left.rel_path,
            "c.jpg",
            "A moved forward, skipping B's file"
        );
        assert_eq!(h.state().right.rel_path, "b.jpg", "and B did not move");
    }

    /// Each side's identifying facts belong with that side, above its image —
    /// not in a shared legend at the bottom, and not in a distant top bar. B's
    /// block sits in the right half of the window, over B's own pane — it must
    /// not drift left when A's facts happen to be narrow (the user's screenshot
    /// showed B's title floating mid-window, nowhere near B's image).
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
        assert!(
            b.min.x >= 590.0,
            "B's title sits in the right half of a 1200px window, over B's own \
             pane: {b:?}"
        );
    }

    /// With the second side hidden, the switcher walks the *whole* pool — the
    /// hidden placeholder must not mask a candidate. The reported defect: in a
    /// two-copy group the switcher appeared, worked once, and then vanished,
    /// because the stale hidden side masked the file just stepped away from.
    #[test]
    fn a_single_view_steps_the_whole_pool_repeatedly() {
        use egui_kittest::kittest::Queryable;
        let pool = vec![named_side("a.jpg"), named_side("b.jpg")];
        let mut cmp = DiffCompare::new_with_pool(named_side("a.jpg"), None, pool);
        cmp.hide_second();
        let mut h = rendered(cmp);

        assert!(
            h.query_by_label("<1 / 2>").is_some(),
            "a single view of a two-copy group offers the switcher"
        );
        // Single view → exactly one forward caret, so it is unambiguous.
        h.get_by_label(crate::icon::CARET_RIGHT).click();
        h.run();
        assert_eq!(h.state().left.rel_path, "b.jpg", "the step lands");
        assert!(
            h.query_by_label("<2 / 2>").is_some(),
            "and the switcher is still there, at the new position"
        );
        h.get_by_label(crate::icon::CARET_RIGHT).click();
        h.run();
        assert_eq!(
            h.state().left.rel_path,
            "a.jpg",
            "stepping wraps instead of dying"
        );

        // Revealing B turns the pair two-sided: nothing left to switch to.
        h.get_by_label_contains("SHOW B").click();
        h.run();
        assert_eq!(
            h.query_all_by_label(crate::icon::CARET_RIGHT).count(),
            0,
            "a two-sided pair offers no switcher at all"
        );
    }

    /// Flicker has buttons, not only the space bar: once both sides are
    /// decoded, FLICKER overlays the panes, SWAP trades A for B, and
    /// SIDE BY SIDE returns. A single (hidden-B) view offers none of it —
    /// flickering a file against its own placeholder shows nothing.
    #[test]
    fn a_decoded_pair_offers_flicker_buttons() {
        use egui_kittest::kittest::Queryable;
        let tmp = tempfile::tempdir().unwrap();
        let a = writable_png_side(tmp.path(), "a.png", 40, 20);
        let b = writable_png_side(tmp.path(), "b.png", 40, 20);
        let cmp = DiffCompare::new_with_pool(a, Some(b), Vec::new());
        let mut h = save_harness(cmp);

        // The decodes land on worker threads; settle until the button shows.
        for _ in 0..200 {
            h.step();
            if h.query_by_label("FLICKER").is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            h.query_by_label("FLICKER").is_some(),
            "a decoded pair offers the FLICKER control"
        );

        h.get_by_label("FLICKER").click();
        h.run();
        assert!(
            h.state().0.compare.flicker,
            "clicking it enters flicker mode"
        );
        assert!(
            h.query_by_label("SWAP").is_some() && h.query_by_label("SIDE BY SIDE").is_some(),
            "flicker offers SWAP and the way back"
        );
        h.get_by_label("SWAP").click();
        h.run();
        assert!(h.state().0.compare.show_b, "SWAP shows the other side");
        h.get_by_label("SIDE BY SIDE").click();
        h.run();
        assert!(!h.state().0.compare.flicker, "SIDE BY SIDE leaves flicker");

        // A hidden second side has nothing to flicker against.
        h.get_by_label_contains("HIDE B").click();
        h.run();
        assert!(
            h.query_by_label("FLICKER").is_none(),
            "a single view offers no flicker"
        );
    }

    /// In flicker the viewer is single-file: only the shown side's facts are on
    /// screen, and SWAP flips *which* side's facts show along with the image —
    /// so there is no hidden-side control to click by accident.
    #[test]
    fn flicker_shows_only_the_visible_sides_chrome_and_swap_flips_it() {
        use egui_kittest::kittest::Queryable;
        let tmp = tempfile::tempdir().unwrap();
        let a = writable_png_side(tmp.path(), "alpha.png", 40, 20);
        let b = writable_png_side(tmp.path(), "beta.png", 40, 20);
        let cmp = DiffCompare::new_with_pool(a, Some(b), Vec::new());
        let mut h = save_harness(cmp);
        for _ in 0..200 {
            h.step();
            if h.query_by_label("FLICKER").is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        h.get_by_label("FLICKER").click();
        h.run();
        // show_b is false, so only side A's facts are present.
        assert!(
            h.query_by_label_contains("alpha.png").is_some(),
            "the shown side's facts are on screen"
        );
        assert!(
            h.query_by_label_contains("beta.png").is_none(),
            "the hidden side's facts (and its tools) are not"
        );
        h.get_by_label("SWAP").click();
        h.run();
        assert!(
            h.query_by_label_contains("beta.png").is_some(),
            "SWAP brings the other side's facts on screen"
        );
        assert!(
            h.query_by_label_contains("alpha.png").is_none(),
            "and takes the first side's away — the chrome flipped with the image"
        );
    }

    /// Selecting a representation tab switches to it, and the repo header (its
    /// identicon-decorated name) is present on both the Image and the Text tab —
    /// one repo reads with one identity across tabs, not decorated on one and
    /// plain on the other.
    #[test]
    fn selecting_a_tab_switches_it_and_the_repo_header_is_consistent() {
        use egui_kittest::kittest::Queryable;
        let tmp = tempfile::tempdir().unwrap();
        // Two decodable PNGs so both sides yield an Image tab.
        let mut a = writable_png_side(tmp.path(), "a.png", 40, 20);
        let mut b = writable_png_side(tmp.path(), "b.png", 30, 24);
        a.repo = "shoebox".into();
        b.repo = "shoebox".into();
        let cmp = DiffCompare::new_with_pool(a, Some(b), Vec::new());
        let mut h = save_harness(cmp);
        // Wait for the decode: the Image tab appears only once both sides decode
        // (Hex is offered immediately, so it can't be the settle signal).
        for _ in 0..200 {
            h.step();
            if h.query_by_label_contains("Image").is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            h.query_by_label_contains("Image").is_some(),
            "the Image tab is offered"
        );
        assert!(
            h.query_by_label_contains("Hex").is_some(),
            "the Hex tab is offered (an image has no Text tab)"
        );
        assert!(
            h.query_all_by_label_contains("shoebox").count() > 0,
            "the repo header shows on the image tab"
        );
        h.get_by_label_contains("Hex").click();
        h.run();
        assert_eq!(
            h.state().0.tab,
            RepresentationKind::Hex,
            "clicking Hex selects it"
        );
        assert!(
            h.query_all_by_label_contains("shoebox").count() > 0,
            "the repo header is still shown on the Hex tab — consistent across tabs"
        );
    }

    /// Build a two-sided, Text-tab compare over two real files whose only
    /// difference is an inserted header — the case the aligned hex diff exists
    /// for.
    fn hex_diff_pair(tmp: &Path) -> DiffCompare {
        // Big enough to paginate (8 pages at 40 rows × 16 bytes), so the page
        // controls — number, slider, diff jumps — have something real to do.
        let payload: Vec<u8> = (0..5_000u32).map(|i| (i % 251) as u8).collect();
        let pa = tmp.join("a.bin");
        let pb = tmp.join("b.bin");
        std::fs::write(&pa, &payload).unwrap();
        let mut bbytes = b"INSERTED-HEADER-BYTES".to_vec();
        bbytes.extend_from_slice(&payload);
        std::fs::write(&pb, &bbytes).unwrap();
        let mut a = named_side("a.bin");
        a.facts.abs_path = pa;
        a.facts.hash_hex = "hash-a".into();
        a.facts.mime = Some("application/octet-stream".into());
        sync_size(&mut a);
        let mut b = named_side("b.bin");
        b.facts.abs_path = pb;
        b.facts.hash_hex = "hash-b".into();
        b.facts.mime = Some("application/octet-stream".into());
        sync_size(&mut b);
        let mut cmp = DiffCompare::new(a, b);
        cmp.tab = RepresentationKind::Hex;
        cmp
    }

    /// The Text tab of a two-sided compare is the aligned, paginated hex diff:
    /// it paginates and offers to jump to the difference.
    #[test]
    fn the_hex_tab_is_a_paginated_hex_diff_with_a_jump_control() {
        use egui_kittest::kittest::Queryable;
        let tmp = tempfile::tempdir().unwrap();
        let h = rendered(hex_diff_pair(tmp.path()));
        // The page number is an editable DragValue between a "page" caption and
        // a "/ total" caption; the fixture spans 8 pages.
        assert!(
            h.query_by_label_contains("/ 8").is_some(),
            "the hex diff paginates, naming the page count"
        );
        assert!(
            h.query_by_label_contains("PREV PAGE").is_some()
                && h.query_by_label_contains("NEXT PAGE").is_some(),
            "page stepping is offered"
        );
        assert!(
            h.query_by_label_contains("NEXT DIFF").is_some(),
            "and offers jump-to-difference (the header insertion is a difference)"
        );
        assert!(
            h.query_by_label_contains("block-level").is_none(),
            "a small pair aligns exactly — no degrade notice"
        );
    }

    /// Doc screenshot: the aligned hex diff on the Text tab, to
    /// `docs/screenshots/hex_diff.png`. `--ignored` (needs wgpu).
    #[test]
    #[ignore = "generates a doc screenshot (needs wgpu)"]
    fn doc_screenshot_hex_diff() {
        let tmp = tempfile::tempdir().unwrap();
        let cmp = hex_diff_pair(tmp.path());
        let mut init = false;
        let mut h = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1100.0, 620.0))
            .wgpu()
            .build_ui_state(
                move |ui, cmp: &mut DiffCompare| {
                    if !init {
                        crate::icon::install(ui.ctx());
                        crate::theme::apply(ui.ctx(), crate::theme::LIGHT);
                        init = true;
                    }
                    cmp.view(&ui.ctx().clone(), TooltipVerbosity::default(), None);
                },
                cmp,
            );
        h.run();
        let img = h.render().expect("wgpu render failed");
        let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/screenshots/hex_diff.png");
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());
    }

    /// Render one file alone (the second side hidden) on `tab`, DARK, and save
    /// it — the shared body of the single-view doc screenshots.
    fn render_single_view(cmp: DiffCompare, name: &str) {
        let mut init = false;
        let mut h = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1000.0, 620.0))
            .wgpu()
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
        let img = h.render().expect("wgpu render failed");
        let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/screenshots")
            .join(name);
        img.save(&out).expect("save png");
        eprintln!("WROTE_SNAPSHOT {}", out.display());
    }

    /// Doc screenshot: a single file's Hex tab (no second side), to
    /// `docs/screenshots/single_hex.png`. Also the regression guard for the
    /// single-view clip fix — the dump must sit in the viewport, not the
    /// headers. `--ignored` (needs wgpu).
    #[test]
    #[ignore = "generates a doc screenshot (needs wgpu)"]
    fn doc_screenshot_single_hex() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bytes = b"BM\x00\x01config: theme=lcars build=2019 author=unknown\x00\x00".to_vec();
        bytes.extend((0..512u32).map(|i| (i % 251) as u8));
        let side = bytes_side(tmp.path(), "firmware.bin", &bytes);
        let mut cmp = DiffCompare::new_with_pool(side, None, Vec::new());
        cmp.hide_second();
        cmp.tab = RepresentationKind::Hex;
        render_single_view(cmp, "single_hex.png");
    }

    /// Doc screenshot: a single text file's Text tab (no second side), to
    /// `docs/screenshots/single_text.png`. `--ignored` (needs wgpu).
    #[test]
    #[ignore = "generates a doc screenshot (needs wgpu)"]
    fn doc_screenshot_single_text() {
        let tmp = tempfile::tempdir().unwrap();
        let body = b"# Inheritance notes\n\nThis drive came from the loft PC.\n\
                     - photos/ : holiday scans, 2003-2011\n\
                     - docs/   : contracts, warranties (some scanned, some PDF)\n\
                     - the Visa contract appears twice, byte-identical\n\n\
                     TODO: dedupe against the NAS before archiving.\n";
        let side = text_side(tmp.path(), "README.md", body);
        let mut cmp = DiffCompare::new_with_pool(side, None, Vec::new());
        cmp.hide_second();
        cmp.tab = RepresentationKind::Text;
        render_single_view(cmp, "single_text.png");
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
        let other = h.get_by_label_contains("Hex").rect();
        assert!(
            other.max.x <= width,
            "the tab escapes the {width}px window: {other:?}"
        );
        assert!(
            (other.center().y - first.center().y).abs() < 2.0,
            "tabs share a baseline rather than drifting: {other:?} vs {first:?}"
        );
    }

    /// The bottom action bar keeps navigation and the destructive action in
    /// separate regions: even in a narrow window the DELETE pill stays inside
    /// the window and never overlaps the side's navigation caret — the fixed
    /// layout that makes a mis-click between "next" and "delete" impossible.
    #[test]
    fn action_bar_keeps_every_control_inside_a_narrow_window() {
        use egui_kittest::kittest::Queryable;
        let width = 820.0;
        let height = 700.0;
        // The crowded worst case: a multi-candidate image pair (nav carets), a
        // *turned* side (so SAVE appears — the tool that shows exactly when the
        // row is busiest), and no marks (so the destructive control is the wider
        // DIFF pair, OVERWRITE OTHER + DELETE).
        let mut cmp = DiffCompare::new_with_pool(
            named_side("a.jpg"),
            Some(named_side("b.jpg")),
            vec![
                named_side("a.jpg"),
                named_side("b.jpg"),
                named_side("c.jpg"),
            ],
        );
        cmp.rotate(true, Orient::RotateCw);
        let mut init = false;
        let mut h = egui_kittest::Harness::builder()
            .with_size(egui::vec2(width, height))
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

        let inside = |r: egui::Rect| r.min.x >= 0.0 && r.max.x <= width && r.max.y <= height;
        let first = |q: &str| {
            h.query_all_by_label_contains(q)
                .next()
                .unwrap_or_else(|| panic!("no widget labelled {q:?}"))
                .rect()
        };
        // Every control the crowded row can hold stays inside the window — the
        // repo convention is to assert rects, since a label query passes even
        // when the widget is clipped off the edge.
        let save = first("SAVE A");
        let del = first("DELETE");
        let over = first("OVERWRITE OTHER");
        let caret = h
            .query_all_by_label(crate::icon::CARET_RIGHT)
            .next()
            .expect("A has a forward caret")
            .rect();
        for (name, r) in [("SAVE A", save), ("DELETE", del), ("OVERWRITE OTHER", over)] {
            assert!(
                r.max.x <= width,
                "{name} escapes the {width}px window: {r:?}"
            );
        }
        // The tools live on their own row, below the navigate/delete row, so a
        // tool can never overlap the destructive control.
        assert!(
            save.min.y >= del.max.y - 1.0,
            "the tools row sits below the delete row: SAVE {save:?} vs DELETE {del:?}"
        );
        // Navigation stays left of the destructive region.
        assert!(
            caret.max.x <= del.min.x,
            "the nav caret is clear of the destructive controls: {caret:?} vs {del:?}"
        );
        assert!(
            inside(caret),
            "the nav caret is inside the window: {caret:?}"
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
            kinds.contains(&RepresentationKind::Hex),
            "and its raw bytes are reachable on Hex (an image has no Text tab): {kinds:?}"
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
                kinds.contains(&RepresentationKind::Hex),
                "{mime:?} is always comparable by its raw bytes on Hex: {kinds:?}"
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
            // B's forward caret is the second one drawn (A's bar, then B's).
            {
                let carets: Vec<_> = h.query_all_by_label(crate::icon::CARET_RIGHT).collect();
                carets[1].click();
            }
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

    /// A real on-disk PNG as a writable viewer side, for the save tests.
    fn writable_png_side(dir: &Path, name: &str, w: u32, h: u32) -> DiffSide {
        let path = dir.join(name);
        image::RgbImage::from_fn(w, h, |x, y| image::Rgb([x as u8, y as u8, 60]))
            .save(&path)
            .unwrap();
        let mut side = named_side(name);
        side.read_only = false;
        side.facts.abs_path = path;
        side.facts.mime = Some("image/png".into());
        side.facts.img_size = Some((w, h));
        side
    }

    /// Drive the viewer with pick capture, for the save tests.
    fn save_harness(
        cmp: DiffCompare,
    ) -> egui_kittest::Harness<'static, (DiffCompare, Vec<DiffPick>)> {
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
        h
    }

    fn file_mtime_ms(path: &Path) -> i64 {
        std::fs::metadata(path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
    }

    /// A turned, writable image side offers SAVE; OVERWRITE in the confirm
    /// dialog writes the rotated pixels over the original — keeping its
    /// modified time — clears the pending turn, and reports `Edited` so the
    /// caller can refresh its index (a kept timestamp makes the change
    /// invisible to a rescan). The viewer stays open.
    #[test]
    fn a_turned_writable_image_saves_over_the_original_keeping_its_date() {
        use egui_kittest::kittest::Queryable;
        let tmp = tempfile::tempdir().unwrap();
        let side = writable_png_side(tmp.path(), "scan.png", 40, 20);
        let path = side.facts.abs_path.clone();
        // Give the scan a distinctly old date, as the real ones have.
        let old = std::time::UNIX_EPOCH + std::time::Duration::from_millis(1_000_000_000_000);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();

        let mut cmp = DiffCompare::new_with_pool(side, None, Vec::new());
        cmp.hide_second();
        let mut h = save_harness(cmp);

        // No pending turn → nothing to save yet.
        assert_eq!(h.query_all_by_label_contains("SAVE A").count(), 0);
        h.get_by_label_contains("ROTATE A").click();
        h.run();
        h.get_by_label_contains("SAVE A").click();
        h.run();
        h.get_by_label("OVERWRITE").click();
        h.run();

        assert_eq!(
            image::image_dimensions(&path).unwrap(),
            (20, 40),
            "the rotated pixels are on disk"
        );
        assert_eq!(
            file_mtime_ms(&path),
            1_000_000_000_000,
            "the file keeps its modified time"
        );
        assert_eq!(
            h.state().1.as_slice(),
            &[DiffPick::Edited { on_left: true }],
            "the caller is told the bytes changed, and the viewer stays open"
        );
        assert_eq!(
            h.state().0.ops_len(0),
            0,
            "the pending turn is consumed by the save"
        );
    }

    /// SAVE COPY writes a `_rot` sibling carrying the original's date and
    /// leaves the original untouched — the never-lose-data path.
    #[test]
    fn save_copy_writes_a_sibling_and_leaves_the_original() {
        use egui_kittest::kittest::Queryable;
        let tmp = tempfile::tempdir().unwrap();
        let side = writable_png_side(tmp.path(), "scan.png", 40, 20);
        let path = side.facts.abs_path.clone();

        let mut cmp = DiffCompare::new_with_pool(side, None, Vec::new());
        cmp.hide_second();
        let mut h = save_harness(cmp);

        h.get_by_label_contains("ROTATE A").click();
        h.run();
        h.get_by_label_contains("SAVE A").click();
        h.run();
        h.get_by_label_contains("SAVE COPY").click();
        h.run();

        let copy = tmp.path().join("scan_rot.png");
        assert_eq!(
            image::image_dimensions(&copy).unwrap(),
            (20, 40),
            "the sibling carries the rotated pixels"
        );
        assert_eq!(
            image::image_dimensions(&path).unwrap(),
            (40, 20),
            "the original is untouched"
        );
        assert!(
            h.state().1.is_empty(),
            "a copy changes no indexed file, so no event is reported"
        );
    }

    /// When the image carries an EXIF capture date, the save dialog can stamp
    /// the file's modified time from it — a scan's file date is often the copy
    /// date, and the capture date is the honest one.
    #[test]
    fn saving_can_stamp_the_exif_capture_date() {
        use egui_kittest::kittest::Queryable;
        let tmp = tempfile::tempdir().unwrap();
        let mut side = writable_png_side(tmp.path(), "scan.png", 40, 20);
        let taken_ms = 869_037_150_000i64; // 1997-07-16
        side.facts.exif = Some(dedup_core::store::ExifInfo {
            taken_ms: Some(taken_ms),
            camera: None,
        });
        let path = side.facts.abs_path.clone();

        let mut cmp = DiffCompare::new_with_pool(side, None, Vec::new());
        cmp.hide_second();
        let mut h = save_harness(cmp);

        h.get_by_label_contains("ROTATE A").click();
        h.run();
        h.get_by_label_contains("SAVE A").click();
        h.run();
        h.get_by_label_contains("EXIF capture date").click();
        h.run();
        h.get_by_label("OVERWRITE").click();
        h.run();

        assert_eq!(
            file_mtime_ms(&path),
            taken_ms,
            "the saved file carries the EXIF capture date"
        );
    }

    /// The Metadata tab lists *every* EXIF field read from the file — EXIF
    /// carries far more than the camera and date the index keeps.
    #[test]
    fn the_metadata_tab_lists_every_exif_field() {
        use egui_kittest::kittest::Queryable;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("shot.jpg");
        // A minimal JPEG whose APP1 carries a hand-built little-endian TIFF
        // with Make "Fuj", Model "X" and a DateTime (same fixture the core
        // `exif_fields` test builds).
        {
            let mut tiff: Vec<u8> = Vec::new();
            tiff.extend_from_slice(b"II");
            tiff.extend_from_slice(&0x2Au16.to_le_bytes());
            tiff.extend_from_slice(&8u32.to_le_bytes());
            tiff.extend_from_slice(&3u16.to_le_bytes());
            let entry = |tiff: &mut Vec<u8>, tag: u16, count: u32, value: [u8; 4]| {
                tiff.extend_from_slice(&tag.to_le_bytes());
                tiff.extend_from_slice(&2u16.to_le_bytes());
                tiff.extend_from_slice(&count.to_le_bytes());
                tiff.extend_from_slice(&value);
            };
            entry(&mut tiff, 0x010F, 4, *b"Fuj\0");
            entry(&mut tiff, 0x0110, 2, *b"X\0\0\0");
            entry(&mut tiff, 0x0132, 20, 50u32.to_le_bytes());
            tiff.extend_from_slice(&0u32.to_le_bytes());
            tiff.extend_from_slice(b"2004:01:06 18:42:00\0");
            let mut app1: Vec<u8> = Vec::new();
            app1.extend_from_slice(b"Exif\0\0");
            app1.extend_from_slice(&tiff);
            let mut jpeg: Vec<u8> = vec![0xFF, 0xD8, 0xFF, 0xE1];
            jpeg.extend_from_slice(&((app1.len() as u16 + 2).to_be_bytes()));
            jpeg.extend_from_slice(&app1);
            jpeg.extend_from_slice(&[0xFF, 0xD9]);
            std::fs::write(&path, jpeg).unwrap();
        }

        let mut side = named_side("shot.jpg");
        side.facts.abs_path = path;
        side.facts.mime = Some("image/jpeg".into());
        // The indexed facts gate the Metadata tab's presence for an image.
        side.facts.exif = Some(dedup_core::store::ExifInfo {
            taken_ms: Some(1_073_413_320_000),
            camera: Some("Fuj X".into()),
        });

        let mut cmp = DiffCompare::new_with_pool(side, None, Vec::new());
        cmp.hide_second();
        let mut h = rendered(cmp);
        h.get_by_label_contains("Metadata").click();
        h.run();

        assert!(
            h.query_by_label("Make").is_some(),
            "the field's tag name is listed"
        );
        assert!(
            h.query_all_by_label_contains("Fuj").count() > 0,
            "with its value beside it"
        );
        assert!(
            h.query_by_label("DateTime").is_some()
                && h.query_all_by_label_contains("2004").count() > 0,
            "every field the file carries is shown, not only camera and date"
        );
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
                // A believable date (2019-05-10) so doc screenshots built from
                // this helper show a real date, not the epoch.
                modified_ms: 1_557_500_000_000,
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
    /// — not a stuck "decoding…" — and disables compare for the pair. A file
    /// whose drive has gone says so instead, distinct from "no preview".
    #[test]
    fn diff_previewability_follows_mime_and_placeholder_names_the_type() {
        // Previewability is a property of the mime, not the file's presence.
        assert!(diff_side(Some("image/jpeg")).previewable());
        assert!(diff_side(Some("video/mp4")).previewable());

        // The "no preview, here's the type" note is only right for a file that
        // *is* present — so the placeholder assertions use real files on disk.
        let tmp = tempfile::tempdir().unwrap();
        let present = |mime: Option<&str>, name: &str| {
            let path = tmp.path().join(name);
            std::fs::write(&path, b"x").unwrap();
            let mut s = diff_side(mime);
            s.facts.abs_path = path;
            s
        };
        let doc = present(Some("application/pdf"), "a.pdf");
        assert!(!doc.previewable(), "a document has no visual to compare");
        assert!(
            doc.placeholder().contains("application/pdf"),
            "the pane names the type it cannot preview"
        );
        assert!(present(None, "b.bin").placeholder().contains("file type"));

        // A file that isn't on disk (a disconnected drive) reads as gone, not
        // as an un-previewable type. The viewer probes presence once (in
        // `spawn_decode`) and caches it; the pane's note then comes from
        // `no_preview_text`, which says "isn't present" rather than naming a
        // type it could otherwise have previewed.
        let gone = diff_side(Some("application/pdf"));
        assert!(!gone.present(), "the default fixture path is not on disk");
        let mut cmp = DiffCompare::new(diff_side(Some("image/jpeg")), gone);
        cmp.present[1] = false;
        assert!(
            cmp.no_preview_text(1)
                .to_lowercase()
                .contains("isn't present"),
            "a missing file says so: {:?}",
            cmp.no_preview_text(1)
        );
        // A present-but-un-previewable file still names its type.
        cmp.present[1] = true;
        assert!(
            cmp.no_preview_text(1).contains("application/pdf"),
            "a present document names the type it cannot preview: {:?}",
            cmp.no_preview_text(1)
        );
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
