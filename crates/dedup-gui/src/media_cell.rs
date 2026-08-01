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

/// How large to draw a media cell and whether it captions itself.
#[derive(Clone, Copy)]
pub struct MediaStyle {
    pub width: f32,
    pub height: f32,
    /// Draw the mime under the placeholder and the duration under the audio
    /// glyph. The Duplicate cards do; the compact review rows carry those in
    /// their own facts line instead.
    pub captions: bool,
}

impl MediaStyle {
    /// The Duplicate tab's card preview.
    pub fn card() -> Self {
        Self {
            width: 160.0,
            height: 120.0,
            captions: true,
        }
    }

    /// A small square cell for a table row.
    pub fn row(edge: f32) -> Self {
        Self {
            width: edge,
            height: edge,
            captions: false,
        }
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
        return Some(resp);
    }

    // Placeholder for non-media or not-yet-ready thumbnails.
    let label = facts.mime.clone().unwrap_or_else(|| "file".into());
    egui::Frame::new()
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
    None
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
