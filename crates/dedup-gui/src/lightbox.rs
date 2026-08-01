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
///
/// The ID3 text fields stay `None` here: reading them is disk I/O and
/// [`FileRepresentations::from_facts`] runs every frame, so the tab loads (and
/// caches) them lazily — this struct only says *that* a file has an editable
/// tag surface. The EXIF fields come straight from the index entry, so they are
/// populated eagerly.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MetadataRepresentation {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub year: Option<u32>,
    pub track: Option<u32>,
    pub comment: Option<String>,
    /// EXIF camera make/model (images).
    pub camera: Option<String>,
    /// EXIF capture time, naive-local epoch milliseconds (images).
    pub taken_ms: Option<i64>,
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

/// Whether a file falls back to the Text/hex representation — everything that
/// is not image, audio or video. Public because the Duplicate cards need the
/// same rule to decide whether a placeholder (a file with no thumbnail) is
/// still worth opening the lightbox for.
pub fn has_text_representation(facts: &FileFacts) -> bool {
    !facts.is_image() && !facts.is_audio() && !facts.is_video()
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

        // Metadata: an editable ID3 surface for the containers the tag writer
        // supports, or read-only EXIF capture facts for an image that has them.
        // Decided from the mime and the index entry only — no disk I/O here.
        let metadata = if is_aud && crate::id3tags::container_supported(facts.mime.as_deref()) {
            Some(MetadataRepresentation {
                can_save: can_write,
                ..Default::default()
            })
        } else if let (true, Some(ex)) = (is_img, facts.exif.as_ref()) {
            Some(MetadataRepresentation {
                camera: ex.camera.clone(),
                taken_ms: ex.taken_ms,
                // EXIF is shown, never rewritten — we have no EXIF writer.
                can_save: false,
                ..Default::default()
            })
        } else {
            None
        };

        // Text/hex: the fallback representation for everything that is not
        // image/audio/video. The preview itself is read lazily by the tab (I/O),
        // so only the capability is decided here; `is_text` is the mime's claim,
        // which the loader confirms or falls back to a hex dump.
        let text = if !has_text_representation(facts) {
            None
        } else {
            Some(TextBinaryRepresentation {
                text_preview: None,
                hex_dump: None,
                is_text: facts
                    .mime
                    .as_deref()
                    .is_some_and(|m| m.starts_with("text/")),
            })
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

/// The representation kinds offered for a Left/Right pair: every kind at least
/// one side supports (§1.3.1). Also the set the viewer dispatches over, so a tab
/// can never be selected without a column behind it.
pub fn tab_kinds(
    left_reps: &FileRepresentations,
    right_reps: Option<&FileRepresentations>,
) -> Vec<RepresentationKind> {
    let mut all_kinds = left_reps.available_kinds();
    for k in right_reps.map(|r| r.available_kinds()).unwrap_or_default() {
        if !all_kinds.contains(&k) {
            all_kinds.push(k);
        }
    }
    all_kinds.sort();
    all_kinds
}

/// Draw the top tab bar displaying representation tabs available across Left (A) and Right (B).
/// Takes the active tab by reference rather than a whole [`LightboxState`], so
/// any surface comparing two files can use it — the Duplicates lightbox and the
/// Transfer DIFF comparison both do.
pub fn draw_tab_bar(
    ui: &mut egui::Ui,
    active_tab: &mut RepresentationKind,
    left_reps: &FileRepresentations,
    right_reps: &FileRepresentations,
) {
    ui.horizontal(|ui| {
        for kind in tab_kinds(left_reps, Some(right_reps)) {
            let label = format!("{} {}", kind.icon(), kind.name());
            let selected = *active_tab == kind;
            let fill = if selected {
                theme::amber()
            } else {
                theme::panel()
            };
            let text_color = if selected {
                theme::black()
            } else {
                theme::text()
            };

            if ui
                .add(egui::Button::new(RichText::new(label).color(text_color)).fill(fill))
                .clicked()
            {
                *active_tab = kind;
            }
        }
    });
}

/// Gap between the lightbox's Left and Right columns.
const COLUMN_GAP: f32 = 16.0;
/// How much of a file's head the Text tab reads.
const TEXT_PREVIEW_BYTES: usize = 64 * 1024;
/// How many bytes a hex dump shows (a wall of hex helps nobody).
const HEX_DUMP_BYTES: usize = 2048;

/// One column's drawing callback: it gets the column's `Ui` and the shared
/// state [`draw_columns`] hands round (a cache, the open editor, or `()`).
pub type ColumnFn<'a, T> = Box<dyn FnOnce(&mut egui::Ui, &mut T) + 'a>;

/// Lay out the lightbox's columns side by side — strictly Left vs. Right, never
/// stacked (§1.3.2) — sizing each from the parent's own cursor. One entry draws
/// a single full-width column, which is what a representation only one side
/// supports must look like (§1.3.1).
///
/// Deliberately not `ui.columns`: that hardcodes `top_down_justified`, which
/// stretches every child widget to the column width instead of its natural size.
/// Equally not an absolute-rect split, which would overlap the columns inside a
/// flow layout and let later controls race them for the same row.
///
/// `shared` is handed to each column in turn — a cache both sides draw from
/// (thumbnails, previews) can only be borrowed by one column at a time, so it
/// travels as an argument rather than being captured twice. Pass `&mut ()` when
/// there is nothing to share.
pub fn draw_columns<T>(ui: &mut egui::Ui, shared: &mut T, cols: Vec<ColumnFn<'_, T>>) {
    let n = cols.len();
    if n == 0 {
        return;
    }
    let col_w = (ui.available_width() - COLUMN_GAP * (n as f32 - 1.0)) / n as f32;
    ui.horizontal_top(|ui| {
        for (i, col) in cols.into_iter().enumerate() {
            if i > 0 {
                ui.add_space(COLUMN_GAP);
            }
            ui.allocate_ui_with_layout(
                egui::vec2(col_w, 0.0),
                egui::Layout::top_down(egui::Align::Min),
                |ui| col(ui, shared),
            );
        }
    });
}

/// Identifying header of one lightbox column: which file, in which repo, and
/// whether that repo is read-only.
pub struct ColumnHead<'a> {
    pub file_name: &'a str,
    pub repo: &'a str,
    pub accent: egui::Color32,
    pub read_only: bool,
    /// This column's repo is the main of a sync group.
    pub is_main: bool,
    /// The file's absolute path — the column's identity for per-side widget
    /// state. Not the name or the hash: duplicates routinely share both, and
    /// two columns under one id share scroll position.
    pub source: &'a Path,
}

impl ColumnHead<'_> {
    fn draw(&self, ui: &mut egui::Ui) {
        crate::repo_chip::repo_chip(
            ui,
            self.repo,
            false,
            self.accent,
            self.is_main,
            Some(self.read_only),
        );
        ui.add_space(4.0);
        ui.label(
            RichText::new(self.file_name)
                .color(theme::text())
                .size(13.0),
        );
        ui.add_space(4.0);
    }
}

/// What a Metadata column's controls asked the caller to do. The caller owns the
/// tag state and the disk write, so the column only reports the intent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetaAction {
    None,
    Edit,
    Save,
    Cancel,
}

/// The body of one Metadata column: an open ID3 editor, a file's stored tags, or
/// an image's EXIF capture facts.
pub enum MetaBody<'a> {
    /// The working copy of the tags being edited, plus the distinct values the
    /// other copies in the group offer per field (Title/Artist/Album/Year/Track/
    /// Genre) so the best one can be adopted.
    Editing {
        tags: &'a mut crate::id3tags::Tags,
        options: &'a [Vec<String>; 6],
    },
    /// The file's tags as stored (`None` when it carries none), read-only.
    /// `can_edit` is false for a read-only repo, which hides the EDIT button
    /// rather than offering a save that cannot land (§1.3.5).
    Stored {
        tags: Option<&'a crate::id3tags::Tags>,
        can_edit: bool,
    },
    /// Read-only EXIF capture facts (images have no tag writer here).
    Exif {
        camera: Option<&'a str>,
        taken: Option<String>,
    },
}

/// One column of the Metadata tab: repo badge + file name, then the file's tag
/// surface — the ID3 editor for the copy being edited, the stored tags for every
/// other copy, or an image's EXIF facts.
pub fn draw_metadata_column(
    ui: &mut egui::Ui,
    head: &ColumnHead<'_>,
    body: MetaBody<'_>,
) -> MetaAction {
    head.draw(ui);
    let mut action = MetaAction::None;
    match body {
        MetaBody::Editing { tags, options } => {
            let fields: [(&str, &mut String); 6] = [
                ("Title", &mut tags.title),
                ("Artist", &mut tags.artist),
                ("Album", &mut tags.album),
                ("Year", &mut tags.year),
                ("Track", &mut tags.track),
                ("Genre", &mut tags.genre),
            ];
            for (i, (label, val)) in fields.into_iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.add_sized(
                        [56.0, 18.0],
                        egui::Label::new(RichText::new(label).color(theme::tan()).size(12.0)),
                    );
                    let w = (ui.available_width() - 32.0).max(80.0);
                    ui.add(egui::TextEdit::singleline(val).desired_width(w));
                    // Adopt a value from another copy in the group.
                    if !options[i].is_empty() {
                        ui.menu_button(icon::CARET_RIGHT, |ui| {
                            for o in &options[i] {
                                if ui.button(RichText::new(o).color(theme::text())).clicked() {
                                    *val = o.clone();
                                }
                            }
                        })
                        .response
                        .on_hover_text("Pick a value from another copy in this group");
                    }
                });
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui
                    .add(
                        egui::Button::new(RichText::new("SAVE TAGS").color(theme::black()))
                            .fill(theme::amber()),
                    )
                    .clicked()
                {
                    action = MetaAction::Save;
                }
                if ui
                    .button(RichText::new("CANCEL").color(theme::text()))
                    .clicked()
                {
                    action = MetaAction::Cancel;
                }
            });
            ui.add_space(4.0);
            ui.label(
                RichText::new(
                    "Saving writes the tags to the file on disk; the audio is unchanged.",
                )
                .color(theme::lilac())
                .size(11.0),
            );
        }
        MetaBody::Stored { tags, can_edit } => {
            match tags {
                Some(t) => {
                    let rows: [(&str, &str); 6] = [
                        ("Title", &t.title),
                        ("Artist", &t.artist),
                        ("Album", &t.album),
                        ("Year", &t.year),
                        ("Track", &t.track),
                        ("Genre", &t.genre),
                    ];
                    for (label, val) in rows {
                        fact_row(ui, label, if val.is_empty() { "—" } else { val });
                    }
                }
                None => {
                    ui.label(
                        RichText::new("No ID3 tags in this file")
                            .color(theme::grey())
                            .size(12.0),
                    );
                }
            }
            ui.add_space(8.0);
            if can_edit {
                if ui
                    .button(
                        RichText::new(format!("{} EDIT TAGS", icon::PENCIL)).color(theme::text()),
                    )
                    .clicked()
                {
                    action = MetaAction::Edit;
                }
            } else {
                ui.label(
                    RichText::new("Read-only repository — tags cannot be changed")
                        .color(theme::grey())
                        .size(11.0),
                );
            }
        }
        MetaBody::Exif { camera, taken } => {
            fact_row(ui, "Camera", camera.unwrap_or("—"));
            fact_row(ui, "Taken", taken.as_deref().unwrap_or("—"));
            ui.add_space(8.0);
            ui.label(
                RichText::new("Capture metadata is shown as recorded and is not edited here.")
                    .color(theme::grey())
                    .size(11.0),
            );
        }
    }
    action
}

/// A `Label   value` row, the read-only counterpart of the editor's fields.
fn fact_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.add_sized(
            [56.0, 18.0],
            egui::Label::new(RichText::new(label).color(theme::tan()).size(12.0)),
        );
        ui.label(RichText::new(value).color(theme::text()).size(12.0));
    });
}

/// A file's head, rendered as text when it decodes as UTF-8 and as a hex dump
/// when it does not — the Text/binary representation's content, loaded on
/// demand (this is disk I/O; callers cache it by content hash).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextPreview {
    pub body: String,
    /// Whether `body` is the file's text (rather than a hex dump).
    pub is_text: bool,
    /// The file is longer than what `body` shows.
    pub truncated: bool,
    /// Why nothing could be read, when that is the case.
    pub error: Option<String>,
}

/// Read at most [`TEXT_PREVIEW_BYTES`] from `path` and render it as text, or as
/// a hex dump of the first [`HEX_DUMP_BYTES`] when it is not valid UTF-8.
pub fn load_text_preview(path: &Path) -> TextPreview {
    use std::io::Read;

    let mut buf = Vec::new();
    let read = std::fs::File::open(path)
        .and_then(|f| f.take(TEXT_PREVIEW_BYTES as u64 + 1).read_to_end(&mut buf));
    if let Err(e) = read {
        return TextPreview {
            body: String::new(),
            is_text: false,
            truncated: false,
            error: Some(e.to_string()),
        };
    }
    let truncated = buf.len() > TEXT_PREVIEW_BYTES;
    buf.truncate(TEXT_PREVIEW_BYTES);

    // A NUL byte means binary even when the sample happens to decode: office
    // and archive containers are full of decodable runs, and rendering those as
    // "text" hides what the file actually is.
    match std::str::from_utf8(&buf)
        .map_err(|_| ())
        .and_then(|s| if buf.contains(&0) { Err(()) } else { Ok(s) })
    {
        Ok(s) => TextPreview {
            body: s.to_string(),
            is_text: true,
            truncated,
            error: None,
        },
        Err(_) => {
            let shown = buf.len().min(HEX_DUMP_BYTES);
            TextPreview {
                body: hex_dump(&buf[..shown]),
                is_text: false,
                truncated: truncated || shown < buf.len(),
                error: None,
            }
        }
    }
}

/// Classic `offset  16 hex bytes  |ascii|` dump.
fn hex_dump(bytes: &[u8]) -> String {
    let mut out = String::new();
    for (i, chunk) in bytes.chunks(16).enumerate() {
        out.push_str(&format!("{:08x}  ", i * 16));
        for (j, b) in chunk.iter().enumerate() {
            out.push_str(&format!("{b:02x} "));
            if j == 7 {
                out.push(' ');
            }
        }
        for j in chunk.len()..16 {
            out.push_str("   ");
            if j == 7 {
                out.push(' ');
            }
        }
        out.push_str(" |");
        for b in chunk {
            out.push(if b.is_ascii_graphic() || *b == b' ' {
                *b as char
            } else {
                '.'
            });
        }
        out.push_str("|\n");
    }
    out
}

/// One column of the Text tab: repo badge + file name, then the file's head as
/// monospaced text or a hex dump, in its own scroll area. Returns the viewport
/// the preview was drawn into — exactly `height` tall however long the file is,
/// because a file longer than the window has to scroll, not overflow it.
pub fn draw_text_column(
    ui: &mut egui::Ui,
    head: &ColumnHead<'_>,
    preview: &TextPreview,
    height: f32,
) -> egui::Rect {
    head.draw(ui);
    let note = match (&preview.error, preview.is_text, preview.truncated) {
        (Some(e), _, _) => format!("Could not read this file: {e}"),
        (None, true, true) => "First 64 KB, as text".to_string(),
        (None, true, false) => "Full contents, as text".to_string(),
        (None, false, _) => "Not text — showing the first bytes as hex".to_string(),
    };
    ui.label(RichText::new(note).color(theme::lilac()).size(11.0));
    ui.add_space(4.0);
    // The column itself is laid out top-down from a zero-height cursor, so the
    // scroll viewport has to be given its size explicitly — left to
    // `available_height` it would collapse to a couple of lines.
    let viewport = egui::Rect::from_min_size(
        ui.next_widget_position(),
        egui::vec2(ui.available_width(), height),
    );
    ui.allocate_rect(viewport, egui::Sense::hover());
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(viewport)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    {
        let ui = &mut child;
        egui::ScrollArea::both()
            .id_salt(head.source)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let text = RichText::new(&preview.body)
                    .color(theme::text())
                    .monospace()
                    .size(12.0);
                // Prose wraps to the column; a hex dump must not — its columns only
                // line up if long lines scroll sideways instead of folding.
                let label = egui::Label::new(text).wrap_mode(if preview.is_text {
                    egui::TextWrapMode::Wrap
                } else {
                    egui::TextWrapMode::Extend
                });
                ui.add(label);
            });
    }
    viewport
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
            // Overview is the intro: a freshly-opened lightbox shows facts +
            // repo + mark pill first, not native content straight away
            // (improvements.md: "the tab on top should always be the intro to
            // comparison").
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
            theme::amber(),
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
                theme::tan(),
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
                        theme::amber(),
                        theme::black(),
                        "Close the viewer",
                        "Close the image viewer (Esc does the same).",
                    ) {
                        close = true;
                    }
                    if pill(
                        ui,
                        "FIT",
                        theme::panel(),
                        theme::text(),
                        "Fit to window",
                        "Scale the image to fit the viewport (F does the same).",
                    ) {
                        do_fit = true;
                    }
                    if pill(
                        ui,
                        "1:1",
                        theme::panel(),
                        theme::text(),
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
                theme::tan(),
            );
            p.text(
                egui::pos2(screen.min.x + 10.0, screen.max.y - 10.0),
                egui::Align2::LEFT_BOTTOM,
                "wheel: zoom · drag: pan · F fit · 1 100% · Esc close",
                egui::FontId::proportional(12.0),
                theme::hairline(),
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
            exif: None,
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
        assert!(!kinds.contains(&RepresentationKind::Audio));
        // An image without EXIF has nothing to put on a Metadata tab, and a
        // visual is never offered as text.
        assert!(!kinds.contains(&RepresentationKind::Metadata));
        assert!(!kinds.contains(&RepresentationKind::Text));

        // The same image *with* EXIF does offer Metadata — read-only, since
        // there is no EXIF writer.
        let with_exif = FileFacts {
            exif: Some(dedup_core::store::ExifInfo {
                taken_ms: Some(1_600_000_000_000),
                camera: Some("Canon EOS 5D".into()),
            }),
            ..facts.clone()
        };
        let reps = FileRepresentations::from_facts(
            &with_exif,
            "MainRepo".into(),
            false,
            MarkState::Unmarked,
        );
        assert!(
            reps.available_kinds()
                .contains(&RepresentationKind::Metadata)
        );
        assert_eq!(
            reps.metadata.as_ref().map(|m| m.can_save),
            Some(false),
            "EXIF is shown, never written"
        );
    }

    #[test]
    fn metadata_is_offered_for_id3_containers_only() {
        let audio = |mime: &str| FileFacts {
            size: 417,
            modified_ms: 0,
            mime: Some(mime.to_string()),
            img_size: None,
            audio_ms: Some(1000),
            audio_seed: None,
            hash_hex: "abcd".to_string(),
            abs_path: PathBuf::from("/tmp/song"),
            origin: None,
            exif: None,
        };

        let mp3 = FileRepresentations::from_facts(
            &audio("audio/mpeg"),
            "r".into(),
            false,
            MarkState::Unmarked,
        );
        assert!(
            mp3.available_kinds()
                .contains(&RepresentationKind::Metadata)
        );
        assert_eq!(
            mp3.metadata.as_ref().map(|m| m.can_save),
            Some(true),
            "a writable repo can save tags"
        );

        // A read-only repo still shows the tab, but not as a savable one.
        let ro = FileRepresentations::from_facts(
            &audio("audio/mpeg"),
            "r".into(),
            true,
            MarkState::Unmarked,
        );
        assert_eq!(ro.metadata.as_ref().map(|m| m.can_save), Some(false));

        // FLAC/OGG carry no ID3 tag the writer understands.
        let flac = FileRepresentations::from_facts(
            &audio("audio/flac"),
            "r".into(),
            false,
            MarkState::Unmarked,
        );
        assert!(
            !flac
                .available_kinds()
                .contains(&RepresentationKind::Metadata)
        );
    }

    #[test]
    fn text_preview_reads_text_and_falls_back_to_hex() {
        let dir = tempfile::tempdir().unwrap();

        let txt = dir.path().join("notes.txt");
        std::fs::write(&txt, "hello alpha\n").unwrap();
        let prev = load_text_preview(&txt);
        assert!(prev.is_text, "valid UTF-8 is shown as text");
        assert_eq!(prev.body, "hello alpha\n");
        assert!(!prev.truncated);
        assert!(prev.error.is_none());

        // Decodable but binary: a NUL byte alone forces the hex view.
        let nul = dir.path().join("blob.dat");
        std::fs::write(&nul, b"%PDF-1.4\x00\x01stream").unwrap();
        assert!(
            !load_text_preview(&nul).is_text,
            "a NUL byte marks the file binary even though it decodes"
        );

        let bin = dir.path().join("blob.bin");
        std::fs::write(&bin, [0xff, 0xfe, 0x00, 0x41]).unwrap();
        let prev = load_text_preview(&bin);
        assert!(!prev.is_text, "invalid UTF-8 falls back to a hex dump");
        assert!(
            prev.body.starts_with("00000000  ff fe 00 41"),
            "dump starts at offset 0 with the file's bytes: {:?}",
            prev.body
        );
        assert!(prev.body.contains("|...A|"), "and carries an ASCII gutter");

        // Longer than the window: the body is capped and flagged.
        let big = dir.path().join("big.txt");
        std::fs::write(&big, "x".repeat(TEXT_PREVIEW_BYTES + 10)).unwrap();
        let prev = load_text_preview(&big);
        assert_eq!(prev.body.len(), TEXT_PREVIEW_BYTES);
        assert!(prev.truncated, "the user is told there is more");

        let prev = load_text_preview(&dir.path().join("nope.txt"));
        assert!(prev.error.is_some(), "an unreadable file reports why");
    }

    /// A file far longer than its column scrolls inside the viewport it was
    /// given instead of pushing the rest of the screen down.
    #[test]
    fn text_column_scrolls_rather_than_overflowing() {
        let preview = TextPreview {
            body: (0..400).map(|i| format!("line {i}\n")).collect(),
            is_text: true,
            truncated: false,
            error: None,
        };
        let head = ColumnHead {
            file_name: "long.txt",
            repo: "r",
            accent: theme::blue(),
            read_only: false,
            is_main: false,
            source: Path::new("/tmp/r/long.txt"),
        };

        let mut viewport = None;
        {
            let mut harness = egui_kittest::Harness::builder()
                .with_size(egui::vec2(600.0, 400.0))
                .build_ui(|ui| {
                    viewport = Some(draw_text_column(ui, &head, &preview, 300.0));
                });
            harness.run();
        }

        let viewport = viewport.expect("column drawn");
        assert_eq!(
            viewport.height(),
            300.0,
            "the preview takes the height it was given, not the file's length"
        );
        assert!(
            viewport.bottom() <= 400.0,
            "and stays inside the window: {viewport:?}"
        );
    }
}
