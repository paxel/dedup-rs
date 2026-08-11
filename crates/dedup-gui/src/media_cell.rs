//! A shared, per-kind **media cell**: given a file's [`FileFacts`] and the
//! [`ThumbCache`], it draws an image thumbnail, a video still, an audio
//! fingerprint glyph, or a typed placeholder — the same rendering the Duplicate
//! cards and the review board both use, so neither grows a downgraded variant of
//! the other (`AGENTS.md`, memory `ui-consistency: one app`).
//!
//! The cell is presentation only: it returns the click [`egui::Response`] (for
//! the image/video/audio kinds) and lets the caller decide what a click means
//! and which tooltip to attach. A placeholder (or an off-screen / not-yet-decoded
//! thumbnail) returns `None`.

use crate::icon;
use crate::theme;
use crate::thumbs::ThumbCache;
use dedup_core::store::{ExifInfo, FileEntry, Store};
use dedup_core::thumbnail::hash_hex;
use egui::{Response, RichText};
use std::path::PathBuf;
use std::sync::Arc;

/// Evenly spaced stills sampled per video: the lightbox filmstrip's cells, and
/// the grid the card/row preview samples from (frame `VIDEO_STRIP / 2`), so the
/// still is reused by the filmstrip instead of extracted twice.
pub(crate) const VIDEO_STRIP: usize = 10;

/// Format a millisecond duration as `h:mm:ss` (or `m:ss` under an hour).
pub(crate) fn fmt_ms(ms: u64) -> String {
    let secs = ms / 1000;
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// The display facts about one file, decoupled from its store [`FileEntry`] so
/// they can travel a preview worker channel and be rendered anywhere. Everything
/// here is `Send`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileFacts {
    pub size: u64,
    pub modified_ms: i64,
    /// The index says this file's path no longer holds it (a tombstone) — the
    /// cell veils itself MISSING without any caller wiring.
    pub missing: bool,
    pub mime: Option<String>,
    pub img_size: Option<(u32, u32)>,
    pub audio_ms: Option<u32>,
    /// First audio chunk hash, the seed for the deterministic glyph.
    pub audio_seed: Option<[u8; 32]>,
    /// Content hash as hex — the thumbnail cache key.
    pub hash_hex: String,
    /// On-disk source, the thumbnail generator's input.
    pub abs_path: PathBuf,
    pub origin: Option<String>,
    /// Capture metadata for images, carried along so the lightbox's Metadata
    /// tab can show it without re-reading the file.
    pub exif: Option<ExifInfo>,
}

impl FileFacts {
    /// Build the display facts for `entry`, whose file lives at `abs_path`.
    pub fn from_entry(entry: &FileEntry, abs_path: PathBuf) -> Self {
        Self {
            size: entry.size,
            modified_ms: entry.modified_ms,
            missing: entry.missing,
            mime: entry.mime.clone(),
            img_size: entry.img_size,
            audio_ms: entry.audio.as_ref().map(|a| a.duration_ms),
            audio_seed: entry
                .audio
                .as_ref()
                .and_then(|a| a.chunk_hashes.first().copied()),
            hash_hex: hash_hex(&entry.hash),
            abs_path,
            origin: entry.origin.clone(),
            exif: entry.exif.clone(),
        }
    }

    pub fn is_image(&self) -> bool {
        self.mime
            .as_deref()
            .is_some_and(|m| m.starts_with("image/"))
    }

    pub fn is_video(&self) -> bool {
        self.mime
            .as_deref()
            .is_some_and(|m| m.starts_with("video/"))
    }

    pub fn is_audio(&self) -> bool {
        self.mime
            .as_deref()
            .is_some_and(dedup_core::fingerprint::is_audio_mime)
    }

    /// Plain readable text (`text/*`) — the kind whose card preview shows its
    /// first lines.
    pub fn is_textual(&self) -> bool {
        self.mime.as_deref().is_some_and(|m| m.starts_with("text/"))
    }

    /// A browsable container (zip/tar/tar.gz), detected by name or MIME.
    pub fn is_archive(&self) -> bool {
        let name = self
            .abs_path
            .file_name()
            .map(|s| s.to_string_lossy())
            .unwrap_or_default();
        dedup_core::archive::is_archive(&name, self.mime.as_deref())
    }

    /// Pixel dimensions (`W×H`) for images, a duration for audio, else `—`.
    pub fn dims_or_duration(&self) -> String {
        if let Some((w, h)) = self.img_size {
            format!("{w}×{h}")
        } else if let Some(ms) = self.audio_ms {
            fmt_ms(ms as u64)
        } else {
            "—".to_string()
        }
    }
}

/// Open a repo's index and resolve its base path once, so a preview builder can
/// look up many rows' facts without reopening per row. Either half is `None`
/// when the repo is unavailable (e.g. a folder-export target that is not a repo).
pub fn open_facts(store: &Store, repo: &str) -> (Option<Arc<redb::Database>>, Option<String>) {
    (
        store.open_repo_db(repo).ok(),
        store.get_repo(repo).map(|m| m.abs_path).ok(),
    )
}

/// Look up the display facts for `rel` in an already-opened repo `db` rooted at
/// `base` (both `None` when the repo could not be opened). `None` when the entry
/// is absent or unreadable — a preview row on the side where the file exists
/// populates this; the other side passes `None`.
pub fn facts_for(db: Option<&redb::Database>, base: Option<&str>, rel: &str) -> Option<FileFacts> {
    let (db, base) = (db?, base?);
    let entry = dedup_core::store::get_entry(db, rel).ok().flatten()?;
    Some(FileFacts::from_entry(
        &entry,
        std::path::Path::new(base).join(rel),
    ))
}

/// A status veil painted over a cell's preview: a translucent wash in the
/// status colour with the state in big bold letters — deletion, absence and
/// resurrection read at a glance without hiding *what* file they concern.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellOverlay {
    /// A plan will delete this file.
    WillDelete,
    /// The file is gone from disk (its index entry is a tombstone).
    Missing,
    /// The receiving repo once held exactly this content and deleted it —
    /// copying it would resurrect a deletion.
    WasDeleted,
    /// Content only on this side / about to be added.
    New,
}

impl CellOverlay {
    fn label(self) -> &'static str {
        match self {
            Self::WillDelete => "WILL DELETE",
            Self::Missing => "MISSING",
            Self::WasDeleted => "WAS DELETED",
            Self::New => "NEW",
        }
    }

    fn color(self) -> egui::Color32 {
        match self {
            Self::WillDelete => theme::red(),
            Self::Missing => theme::amber(),
            Self::WasDeleted => theme::blue(),
            Self::New => theme::green(),
        }
    }
}

/// How large to draw a media cell and whether it captions itself.
#[derive(Clone, Copy)]
pub struct MediaStyle {
    pub width: f32,
    pub height: f32,
    /// Draw the mime under the placeholder and the duration under the audio
    /// glyph. The Duplicate cards do; the compact review rows carry those in
    /// their own facts line instead.
    pub captions: bool,
    /// A status veil to paint over whatever the cell shows (see
    /// [`CellOverlay`]). A missing file veils itself regardless.
    pub overlay: Option<CellOverlay>,
}

impl MediaStyle {
    /// The Duplicate tab's card preview.
    pub fn card() -> Self {
        Self {
            width: 160.0,
            height: 120.0,
            captions: true,
            overlay: None,
        }
    }

    /// A small square cell for a table row.
    pub fn row(edge: f32) -> Self {
        Self {
            width: edge,
            height: edge,
            captions: false,
            overlay: None,
        }
    }

    /// The same style with a status veil.
    pub fn with_overlay(mut self, overlay: Option<CellOverlay>) -> Self {
        self.overlay = overlay;
        self
    }
}

/// Draw the media cell for `facts`. Returns the clickable [`Response`] for the
/// image / video / audio kinds (the caller attaches meaning and tooltip); a
/// placeholder — or an off-screen or not-yet-decoded thumbnail — returns `None`.
pub fn media_cell(
    ui: &mut egui::Ui,
    thumbs: &mut ThumbCache,
    facts: &FileFacts,
    style: MediaStyle,
) -> Option<Response> {
    let (w, h) = (style.width, style.height);
    // A missing file veils itself; otherwise the caller's status wins. Painted
    // over whatever the cell shows — including a stale cached thumbnail, which
    // is deliberate: "this (red-veiled photo) is what the plan deletes".
    let overlay = if facts.missing {
        Some(CellOverlay::Missing)
    } else {
        style.overlay
    };
    if facts.is_image() || facts.is_video() {
        // Only fetch a texture for an on-screen cell: a virtualized table or a
        // long card list can lay out far more thumbnails than the GPU cache
        // holds, and requesting every one thrashes the LRU.
        let thumb_rect = egui::Rect::from_min_size(ui.next_widget_position(), egui::vec2(w, h));
        if ui.is_rect_visible(thumb_rect) {
            let source = facts.abs_path.as_path();
            // Videos show a mid-timeline still, sampled on the same grid as the
            // lightbox filmstrip so the frame is reused, not extracted twice.
            let tex = if facts.is_video() {
                thumbs.get_video(&facts.hash_hex, source, VIDEO_STRIP / 2, VIDEO_STRIP)
            } else {
                thumbs.get(&facts.hash_hex, source)
            };
            if let Some(tex) = tex {
                let mut image = egui::Image::new(egui::load::SizedTexture::from_handle(&tex))
                    .max_height(h)
                    .corner_radius(6)
                    .sense(egui::Sense::click());
                // A square row cell also bounds the width so a wide image fits its
                // box; the taller card constrains by height only, as it always has.
                if !style.captions {
                    image = image.max_width(w);
                }
                let resp = ui.add(image);
                // Hairline so dark photos stand off the dark panel.
                ui.painter().rect_stroke(
                    resp.rect,
                    6,
                    egui::Stroke::new(1.0, theme::hairline()),
                    egui::StrokeKind::Inside,
                );
                paint_overlay(ui.painter(), resp.rect, overlay, style.captions);
                return Some(resp);
            }
        }
    }

    // Audio: a deterministic fingerprint glyph (identical content → identical
    // glyph), so a cell reads as audio instead of a broken image. A file whose
    // fingerprint failed still carries a duration, so gate on that, not the seed
    // (which is absent when the fingerprint has no chunk hashes).
    if facts.is_audio() && facts.audio_ms.is_some() {
        let seed = facts.audio_seed.unwrap_or([0u8; 32]);
        // Leave room under the glyph for the duration caption when captioned.
        let bottom = if style.captions { 24.0 } else { 8.0 };
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::click());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 6.0, theme::panel());
        painter.rect_stroke(
            rect,
            6.0,
            egui::Stroke::new(1.0, theme::hairline()),
            egui::StrokeKind::Inside,
        );
        let glyph = egui::Rect::from_min_max(
            rect.min + egui::vec2(8.0, 8.0),
            egui::pos2(rect.max.x - 8.0, rect.max.y - bottom),
        );
        paint_audio_glyph(&painter, glyph, seed);
        if style.captions
            && let Some(ms) = facts.audio_ms
        {
            painter.text(
                egui::pos2(rect.center().x, rect.max.y - 13.0),
                egui::Align2::CENTER_CENTER,
                fmt_ms(ms as u64),
                egui::FontId::proportional(12.0),
                theme::tan(),
            );
        }
        paint_overlay(&painter, rect, overlay, style.captions);
        return Some(resp);
    }

    // Text files: the first lines of the file, drawn as a tiny page — far more
    // telling than a mime badge when the duplicates are notes, configs or code.
    // The head is read on the thumbnail worker pool and cached (`None` until it
    // lands), so the paint path never touches the disk — a hung network mount
    // cannot stall a frame.
    if facts.is_textual() {
        let thumb_rect = egui::Rect::from_min_size(ui.next_widget_position(), egui::vec2(w, h));
        if ui.is_rect_visible(thumb_rect)
            && let Some(head) = thumbs.get_text_head(&facts.hash_hex, facts.abs_path.as_path())
        {
            let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::click());
            let painter = ui.painter_at(rect);
            painter.rect_filled(rect, 6.0, theme::panel());
            painter.rect_stroke(
                rect,
                6.0,
                egui::Stroke::new(1.0, theme::hairline()),
                egui::StrokeKind::Inside,
            );
            let font = egui::FontId::monospace(9.0);
            let line_h = 11.0;
            let pad = 7.0;
            let mut y = rect.min.y + pad;
            for line in head.lines() {
                if y + line_h > rect.max.y - pad {
                    break;
                }
                painter.text(
                    egui::pos2(rect.min.x + pad, y),
                    egui::Align2::LEFT_TOP,
                    line,
                    font.clone(),
                    theme::text(),
                );
                y += line_h;
            }
            paint_overlay(&painter, rect, overlay, style.captions);
            // A small row cell fits only a corner of the head; hovering it
            // shows the whole preview at a readable size.
            let resp = if h < 60.0 {
                resp.on_hover_text(RichText::new(&head).monospace().size(11.0))
            } else {
                resp
            };
            // The same accessible name the placeholder's overlay carries, so
            // "the way into the viewer" reads consistently for tests and
            // screen readers whatever the cell happens to show.
            resp.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "OPEN PREVIEW")
            });
            return Some(resp);
        }
    }

    // PDFs: a real first-page mini-render via the async poppler pipeline.
    // While it renders (or forever, without poppler) they fall through to the
    // byte view below like every other opaque file.
    let is_pdf = facts.mime.as_deref() == Some("application/pdf");
    if is_pdf {
        let thumb_rect = egui::Rect::from_min_size(ui.next_widget_position(), egui::vec2(w, h));
        if ui.is_rect_visible(thumb_rect)
            && let Some(tex) = thumbs.get_pdf_page(&facts.hash_hex, facts.abs_path.as_path())
        {
            let mut image = egui::Image::new(egui::load::SizedTexture::from_handle(&tex))
                .max_height(h)
                .corner_radius(6)
                .sense(egui::Sense::click());
            if !style.captions {
                image = image.max_width(w);
            }
            let resp = ui.add(image);
            ui.painter().rect_stroke(
                resp.rect,
                6,
                egui::Stroke::new(1.0, theme::hairline()),
                egui::StrokeKind::Inside,
            );
            paint_overlay(ui.painter(), resp.rect, overlay, style.captions);
            resp.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "OPEN PREVIEW")
            });
            return Some(resp);
        }
    }

    // The universal fallback: the file's head bytes as a greyscale bitmap
    // (identical content → identical pattern) with the extension across the
    // middle, colour-hashed like the repo identicons so ".db" is the same hue
    // everywhere. Generated on the worker pool; until it lands (or when the
    // file cannot be read) the typed placeholder below stands in.
    if !facts.is_image() && !facts.is_video() && !facts.is_audio() && !facts.is_textual() {
        let thumb_rect = egui::Rect::from_min_size(ui.next_widget_position(), egui::vec2(w, h));
        if ui.is_rect_visible(thumb_rect)
            && let Some(tex) = thumbs.get_byte_view(&facts.hash_hex, facts.abs_path.as_path())
        {
            let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::click());
            let painter = ui.painter_at(rect);
            painter.image(
                tex.id(),
                rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
            painter.rect_stroke(
                rect,
                6.0,
                egui::Stroke::new(1.0, theme::hairline()),
                egui::StrokeKind::Inside,
            );
            // Under a status veil the extension yields the centre to the
            // status word and shrinks into the corner.
            paint_extension_badge(&painter, rect, facts, style.captions, overlay.is_none());
            paint_overlay(&painter, rect, overlay, style.captions);
            resp.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "OPEN PREVIEW")
            });
            return Some(resp);
        }
    }

    // Placeholder for non-media or not-yet-ready thumbnails.
    let label = facts.mime.clone().unwrap_or_else(|| "file".into());
    let frame = egui::Frame::new()
        .fill(theme::panel())
        .corner_radius(6)
        .inner_margin(if style.captions { 18.0 } else { 6.0 })
        .show(ui, |ui| {
            ui.set_width(w);
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new(icon::IMAGE)
                        .color(theme::lilac())
                        .size(if style.captions { 28.0 } else { 18.0 }),
                );
                if style.captions {
                    ui.label(RichText::new(label).color(theme::lilac()).size(11.0));
                }
            });
        });
    paint_overlay(ui.painter(), frame.response.rect, overlay, style.captions);
    None
}

/// The status veil: a translucent wash of the status colour over the whole
/// cell with the state word big and unmissable across the middle — visible from
/// across the room, exactly because it sits on top of the file's own preview.
fn paint_overlay(
    painter: &egui::Painter,
    rect: egui::Rect,
    overlay: Option<CellOverlay>,
    large: bool,
) {
    let Some(overlay) = overlay else {
        return;
    };
    let color = overlay.color();
    painter.rect_filled(rect, 6.0, color.gamma_multiply(0.22));
    painter.rect_stroke(
        rect,
        6.0,
        egui::Stroke::new(2.0, color),
        egui::StrokeKind::Inside,
    );
    let font = egui::FontId::proportional(if large { 17.0 } else { 10.0 });
    let galley = painter.layout_no_wrap(overlay.label().to_string(), font, color);
    let pos = rect.center() - galley.size() / 2.0;
    let chip = egui::Rect::from_min_size(pos, galley.size()).expand(if large { 5.0 } else { 2.0 });
    painter.rect_filled(chip, 4.0, egui::Color32::from_black_alpha(170));
    painter.galley(pos, galley, color);
}

/// The big centred extension over a byte-view bitmap (".db", ".exe"), in a hue
/// hashed from the extension itself — the same extension is the same colour on
/// every card — on a dark chip so it reads over the byte noise.
fn paint_extension_badge(
    painter: &egui::Painter,
    rect: egui::Rect,
    facts: &FileFacts,
    large: bool,
    centered: bool,
) {
    let ext = facts
        .abs_path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()))
        .unwrap_or_else(|| "?".to_string());
    let hue = (theme::name_hash(&ext) % 360) as f32;
    let (sat, light) = if theme::is_dark() {
        (0.55, 0.70)
    } else {
        (0.60, 0.42)
    };
    let color = theme::hsl(hue, sat, light);
    let size = match (large, centered) {
        (true, true) => 26.0,
        (true, false) => 13.0,
        (false, true) => 13.0,
        (false, false) => 9.0,
    };
    let font = egui::FontId::proportional(size);
    let galley = painter.layout_no_wrap(ext, font, color);
    let pos = if centered {
        rect.center() - galley.size() / 2.0
    } else {
        // Tucked into the top-left, leaving the centre to the status word.
        rect.min + egui::vec2(6.0, 5.0)
    };
    let chip = egui::Rect::from_min_size(pos, galley.size()).expand(if large { 6.0 } else { 3.0 });
    painter.rect_filled(chip, 4.0, egui::Color32::from_black_alpha(150));
    painter.galley(pos, galley, color);
}

/// Paint a deterministic "fingerprint" glyph for an audio file from its first
/// chunk-hash `seed`: a waveform whose bar heights and accent colour come from
/// the hash. Every bar is an independent hash byte (no forced mirror symmetry,
/// which would make different files look alike). Identical content yields an
/// identical glyph — BLAKE3's avalanche means it signals *identity*, not degrees
/// of similarity.
pub(crate) fn paint_audio_glyph(painter: &egui::Painter, rect: egui::Rect, seed: [u8; 32]) {
    let palette = [
        theme::amber(),
        theme::tan(),
        theme::lilac(),
        theme::blue(),
        theme::orange(),
    ];
    let accent = palette[seed[0] as usize % palette.len()];
    let bars = 15usize;
    let gap = 3.0;
    let bar_w = ((rect.width() - gap * (bars as f32 - 1.0)) / bars as f32).max(1.0);
    let mid_y = rect.center().y;
    let max_amp = rect.height() * 0.45;
    for i in 0..bars {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(mime: &str) -> FileFacts {
        FileFacts {
            size: 4_200_000,
            modified_ms: 0,
            missing: false,
            mime: Some(mime.to_string()),
            img_size: None,
            audio_ms: None,
            audio_seed: None,
            hash_hex: "deadbeef".to_string(),
            abs_path: PathBuf::from("/x"),
            origin: None,
            exif: None,
        }
    }

    #[test]
    fn kind_helpers_classify_by_mime() {
        assert!(facts("image/jpeg").is_image());
        assert!(facts("video/mp4").is_video());
        assert!(facts("audio/mpeg").is_audio());
        let txt = facts("text/plain");
        assert!(!txt.is_image() && !txt.is_video() && !txt.is_audio());
        assert!(txt.is_textual() && facts("text/markdown").is_textual());
        assert!(!facts("application/pdf").is_textual());
    }

    /// An opaque file's card cell becomes the clickable byte-view bitmap once
    /// the background read lands (with the extension badge painted over it);
    /// until then it is the placeholder, which returns `None`.
    #[test]
    fn opaque_card_gets_a_byte_view_once_loaded() {
        struct State {
            thumbs: ThumbCache,
            facts: FileFacts,
            clickable: bool,
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        std::fs::write(&path, vec![0xA5u8; 4096]).unwrap();
        let mut f = facts("application/octet-stream");
        f.abs_path = path;
        let state = State {
            thumbs: ThumbCache::new(1),
            facts: f,
            clickable: false,
        };
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(400.0, 300.0))
            .build_ui_state(
                move |ui, state: &mut State| {
                    state.thumbs.poll(&ui.ctx().clone());
                    let resp = media_cell(ui, &mut state.thumbs, &state.facts, MediaStyle::card());
                    state.clickable = resp.is_some();
                },
                state,
            );
        for _ in 0..100 {
            harness.step();
            if harness.state().clickable {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            harness.state().clickable,
            "the byte view landed and the cell is clickable"
        );
    }

    /// A text file's card cell becomes a clickable first-lines preview once the
    /// background head-read lands; until then it stays the plain placeholder
    /// (which returns `None`) — the paint path never reads the file itself.
    #[test]
    fn text_card_previews_first_lines_once_loaded() {
        struct State {
            thumbs: ThumbCache,
            facts: FileFacts,
            clickable: bool,
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recipe.txt");
        std::fs::write(&path, "Fritata\n\n1. eggs\n2. pan\n").unwrap();
        let mut f = facts("text/plain");
        f.abs_path = path;
        let state = State {
            thumbs: ThumbCache::new(1),
            facts: f,
            clickable: false,
        };
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(400.0, 300.0))
            .build_ui_state(
                move |ui, state: &mut State| {
                    state.thumbs.poll(&ui.ctx().clone());
                    let resp = media_cell(ui, &mut state.thumbs, &state.facts, MediaStyle::card());
                    state.clickable = resp.is_some();
                },
                state,
            );
        // (No "still placeholder" assertion after the first frame — the worker
        // can win that race on a tiny local file, and that's fine.)
        for _ in 0..100 {
            harness.step();
            if harness.state().clickable {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            harness.state().clickable,
            "once the head-read lands the cell draws the preview and is clickable"
        );
    }

    #[test]
    fn dims_or_duration_prefers_image_size_then_audio_then_dash() {
        let mut f = facts("image/png");
        f.img_size = Some((4032, 3024));
        assert_eq!(f.dims_or_duration(), "4032×3024");

        let mut a = facts("audio/mpeg");
        a.audio_ms = Some(221_000);
        assert_eq!(a.dims_or_duration(), "3:41");

        assert_eq!(facts("application/pdf").dims_or_duration(), "—");
    }

    #[test]
    fn fmt_ms_switches_to_hours_past_one_hour() {
        assert_eq!(fmt_ms(3_000), "0:03");
        assert_eq!(fmt_ms(221_000), "3:41");
        assert_eq!(fmt_ms(3_661_000), "1:01:01");
    }
}
