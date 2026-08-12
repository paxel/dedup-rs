//! The shared building blocks of the one full-window viewer
//! ([`crate::compare_view::DiffCompare`]): the representation model
//! ([`FileRepresentations`] and its per-kind facts), the tab bar, the
//! column/metadata/text drawing helpers, the mark pill, and the A/B compare
//! transform. Every surface that shows or compares a file draws through these,
//! so each piece exists exactly once.

use crate::icon;
use crate::media_cell::FileFacts;
use crate::settings::TooltipVerbosity;
use crate::theme;
use crate::util::ExplainExt;
use egui::{Rect, RichText, TextureHandle, Vec2};
use std::path::{Path, PathBuf};

const MAX_SCALE: f32 = 32.0;

/// Classification of file representation tabs available in the Lightbox.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RepresentationKind {
    Overview,
    // Archive sorts right after Overview so it is the natural landing tab for a
    // container file, ahead of the Text/hex fallback every file also offers.
    Archive,
    Metadata,
    Image,
    Audio,
    // The audio spectrogram as a zoomable image — the visual fingerprint
    // comparison, split out so the Audio tab stays a listening transport.
    Spectrum,
    Video,
    Text,
    // A document rasterized to page images — how it looks. Offered for PDFs,
    // beside Text.
    Render,
    // The printable runs embedded in any file's bytes — offered everywhere,
    // after Text.
    Strings,
    // The raw bytes of any file — always available, its own tab, independent of
    // Text (which is now readable text only).
    Hex,
}

impl RepresentationKind {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Archive => "Archive",
            Self::Metadata => "Metadata",
            Self::Image => "Image",
            Self::Audio => "Audio",
            Self::Spectrum => "Spectrum",
            Self::Video => "Video",
            Self::Text => "Text",
            Self::Render => "Render",
            Self::Strings => "Strings",
            Self::Hex => "Hex",
        }
    }

    pub fn icon(&self) -> &'static str {
        match self {
            Self::Overview => icon::STAR,
            Self::Archive => icon::FOLDER_OPEN,
            Self::Metadata => icon::PENCIL,
            Self::Image => icon::IMAGE,
            Self::Audio => icon::LIGHTNING,
            Self::Spectrum => icon::IMAGE,
            Self::Video => icon::IMAGE,
            Self::Text => icon::SEARCH,
            Self::Render => icon::IMAGE,
            Self::Strings => icon::SEARCH,
            Self::Hex => icon::SEARCH,
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

/// A file is a browsable archive (zip/tar/tar.gz). The member list itself is
/// loaded lazily by the viewer (disk I/O), so this only records the capability.
#[derive(Clone, Debug)]
pub struct ArchiveRepresentation {}

/// A document (PDF directly; office and legacy formats via LibreOffice) can be
/// rasterized to page images and looked at as it renders. The pages are
/// produced lazily by the viewer (external tools), so this only records the
/// capability.
#[derive(Clone, Debug)]
pub struct RenderRepresentation {}

/// Aggregated representations for a single file instance.
#[derive(Clone, Debug)]
pub struct FileRepresentations {
    pub dedup: DedupDataRepresentation,
    pub image: Option<ImageRepresentation>,
    pub audio: Option<AudioRepresentation>,
    pub metadata: Option<MetadataRepresentation>,
    pub video: Option<VideoRepresentation>,
    pub text: Option<TextBinaryRepresentation>,
    pub archive: Option<ArchiveRepresentation>,
    pub render: Option<RenderRepresentation>,
    pub mark: MarkState,
}

/// Whether a file falls back to the Text/hex representation — everything that
/// is not image, audio or video. Public because the Duplicate cards need the
/// same rule to decide whether a placeholder (a file with no thumbnail) is
/// still worth opening the lightbox for.
pub fn has_text_representation(_facts: &FileFacts) -> bool {
    // Every file has bytes, and reading them is a forensic act in its own right:
    // a JPEG's header, a document's magic number, the tail of an unknown format.
    // Previously restricted to files that were not image/audio/video, which left
    // exactly the cases where "is this really the same file?" needed an answer
    // with no way to look.
    true
}

/// Whether a file has *readable text* — a text-purpose document we can extract
/// (PDF/office/email) or a plain-text file. Drives the viewer's **Text** tab; the
/// always-present **Hex** tab covers the raw bytes of everything else.
pub fn has_readable_text(facts: &FileFacts) -> bool {
    facts.mime.as_deref().is_some_and(|m| {
        dedup_core::fingerprint::is_extractable_document(m) || m.starts_with("text/")
    })
}

/// Whether headless LibreOffice was found by the startup probe. Office and
/// legacy documents offer a Render tab only when it can actually rasterize
/// them; a PDF's Render tab is independent of this. Written once by the probe
/// thread — reading it never blocks a paint frame. Tri-state, **optimistic
/// while unknown**: a viewer opened in the seconds before the probe answers
/// still offers the tab (a conversion attempt then fails gracefully with a
/// note), instead of silently withholding it for the session's first file.
static SOFFICE_STATE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(UNKNOWN);
const UNKNOWN: u8 = 0;
const PRESENT: u8 = 1;
const ABSENT: u8 = 2;

/// Record the startup probe's `soffice` verdict (see [`soffice_available`]).
pub fn set_soffice_available(ok: bool) {
    SOFFICE_STATE.store(
        if ok { PRESENT } else { ABSENT },
        std::sync::atomic::Ordering::Relaxed,
    );
}

/// Whether office/legacy documents can offer rendering: the probe found
/// `soffice`, or it simply hasn't answered yet (optimistic).
pub fn soffice_available() -> bool {
    SOFFICE_STATE.load(std::sync::atomic::Ordering::Relaxed) != ABSENT
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

        // Text: readable words — an extractable document (PDF/office/email) or a
        // plain-text file. The raw-bytes view lives on the always-present Hex tab,
        // not here. The preview is read lazily by the tab (I/O), so only the
        // capability is decided; `is_text` is the mime's claim.
        let text = if !has_readable_text(facts) {
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

        // A container file (zip/tar/tar.gz) can be looked inside.
        let archive = if facts.is_archive() {
            Some(ArchiveRepresentation {})
        } else {
            None
        };

        // A PDF can be rasterized to page images directly; an office or
        // legacy document (.doc, RTF) via LibreOffice, when the probe found
        // it. Render deliberately covers formats Text can't extract — a 1998
        // .doc is exactly the file that must be judged by eye.
        let render = match facts.mime.as_deref() {
            Some("application/pdf") => Some(RenderRepresentation {}),
            Some(m) if dedup_core::render::office_renderable(m) && soffice_available() => {
                Some(RenderRepresentation {})
            }
            _ => None,
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
            archive,
            render,
            mark,
        }
    }

    /// List representation kinds supported by this file instance.
    pub fn available_kinds(&self) -> Vec<RepresentationKind> {
        let mut kinds = vec![RepresentationKind::Overview];
        if self.archive.is_some() {
            kinds.push(RepresentationKind::Archive);
        }
        if self.metadata.is_some() {
            kinds.push(RepresentationKind::Metadata);
        }
        if self.image.is_some() {
            kinds.push(RepresentationKind::Image);
        }
        if self.audio.is_some() {
            kinds.push(RepresentationKind::Audio);
            kinds.push(RepresentationKind::Spectrum);
        }
        if self.video.is_some() {
            kinds.push(RepresentationKind::Video);
        }
        if self.text.is_some() {
            kinds.push(RepresentationKind::Text);
        }
        if self.render.is_some() {
            kinds.push(RepresentationKind::Render);
        }
        // Strings and Hex are offered for every file — any bytes may hold
        // embedded text, and the raw bytes are always worth a look.
        kinds.push(RepresentationKind::Strings);
        kinds.push(RepresentationKind::Hex);
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
/// Takes the active tab by reference rather than any larger viewer state, so
/// any surface comparing two files can use it.
pub fn draw_tab_bar(
    ui: &mut egui::Ui,
    active_tab: &mut RepresentationKind,
    left_reps: &FileRepresentations,
    right_reps: &FileRepresentations,
) {
    ui.horizontal(|ui| {
        // No Overview tab: its content lives in the per-side titles and the
        // Metadata tab, so a button for it would select nothing.
        for kind in tab_kinds(left_reps, Some(right_reps))
            .into_iter()
            .filter(|k| *k != RepresentationKind::Overview)
        {
            let label = format!("{} {}", kind.icon(), kind.name());
            let selected = *active_tab == kind;
            let fill = if selected {
                theme::amber()
            } else {
                theme::panel()
            };
            let text_color = if selected {
                theme::ink_on(theme::amber())
            } else {
                theme::text()
            };

            let mut button = egui::Button::new(RichText::new(label).color(text_color)).fill(fill);
            // The selected tab carries a bright outline so which representation is
            // active reads at a glance, not just from the fill.
            if selected {
                button = button.stroke(egui::Stroke::new(2.0, theme::orange()));
            }
            if ui.add(button).clicked() {
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
/// stacked (§1.3.2). One entry draws a single full-width column, which is what a
/// representation only one side supports must look like (§1.3.1).
///
/// Each column is drawn into a **fixed, clipped absolute rect**: column `i` always
/// sits at the same x, whatever the others contain, and content that would exceed
/// its half is clipped rather than shoving the next column rightward (and, with a
/// long path or a wide document line, eventually off-screen — the bug this
/// replaced). Columns that need to show more than fits scroll within their own
/// rect. `shared` is handed to each column in turn (a cache both sides draw from
/// can only be borrowed by one at a time). Pass `&mut ()` when there is nothing
/// to share.
pub fn draw_columns<T>(ui: &mut egui::Ui, shared: &mut T, cols: Vec<ColumnFn<'_, T>>) {
    let n = cols.len();
    if n == 0 {
        return;
    }
    // Start from the current cursor (not the top of the ui), so anything drawn
    // before the columns — e.g. the Metadata tab's tag-save error — stays above
    // them instead of being painted over.
    let full = ui.available_rect_before_wrap();
    let col_w = (full.width() - COLUMN_GAP * (n as f32 - 1.0)) / n as f32;
    for (i, col) in cols.into_iter().enumerate() {
        let x0 = full.min.x + i as f32 * (col_w + COLUMN_GAP);
        let rect =
            Rect::from_min_size(egui::pos2(x0, full.min.y), egui::vec2(col_w, full.height()));
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(rect)
                .layout(egui::Layout::top_down(egui::Align::Min)),
        );
        // Clip to the column so nothing a column draws can escape into its
        // neighbour or push the layout — the guarantee the flow layout lacked.
        child.set_clip_rect(rect.intersect(ui.clip_rect()));
        col(&mut child, shared);
    }
    ui.allocate_rect(full, egui::Sense::hover());
}

/// What a Metadata column's controls asked the caller to do. The caller owns the
/// tag state and the disk write, so the column only reports the intent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetaAction {
    None,
    Edit,
    Save,
    Cancel,
    /// Write this side's metadata to a human-readable sidecar file.
    ExportMetadata,
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
    /// Read-only EXIF facts (images have no tag writer here) — the full field
    /// list as `(tag, value)` pairs, not only camera and date. `differing` names
    /// the tags whose value differs from the other side, highlighted so the
    /// difference reads at a glance.
    Exif {
        fields: Vec<(String, String)>,
        differing: std::collections::HashSet<String>,
    },
}

/// One column of the Metadata tab: repo badge + file name, then the file's tag
/// surface — the ID3 editor for the copy being edited, the stored tags for every
/// other copy, or an image's EXIF facts.
pub fn draw_metadata_column(ui: &mut egui::Ui, source: &Path, body: MetaBody<'_>) -> MetaAction {
    // No per-column identity header: the viewer's top strip already names the
    // side (repo chip + path + facts), so a head here would just duplicate it
    // and collide with the strip above. `source` is only the scroll id.
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
            // One row geometry for all six rows, computed once: label column,
            // field column, kebab slot. Sizing the field per row from
            // `available_width` left each row a slightly different width (and
            // let the kebab overflow the column clip, cutting the pill) — the
            // fields must line up like a form, so their width is fixed here and
            // forced with `add_sized`, and the kebab slot ends inside the clip.
            const LABEL_W: f32 = 56.0;
            const KEBAB_W: f32 = 36.0;
            let gap = ui.spacing().item_spacing.x;
            let field_w = (ui.available_width() - LABEL_W - KEBAB_W - 2.0 * gap - 4.0).max(60.0);
            for (i, (label, val)) in fields.into_iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.add_sized(
                        [LABEL_W, 18.0],
                        egui::Label::new(RichText::new(label).color(theme::tan()).size(12.0)),
                    );
                    ui.add_sized([field_w, 18.0], egui::TextEdit::singleline(val));
                    // Adopt a value from another copy in the group. A "more
                    // options for this field" kebab (⋮), not a directional
                    // caret — the value is pulled *into* this field, never
                    // pushed to the other side.
                    if !options[i].is_empty() {
                        ui.menu_button("⋮", |ui| {
                            for o in &options[i] {
                                if ui.button(RichText::new(o).color(theme::text())).clicked() {
                                    *val = o.clone();
                                }
                            }
                        })
                        .response
                        .on_hover_text("Use a value from another copy in this group");
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
        MetaBody::Exif { fields, differing } => {
            if fields.is_empty() {
                ui.label(
                    RichText::new("No EXIF metadata in this file")
                        .color(theme::grey())
                        .size(12.0),
                );
            } else {
                // The full field list can be long; it scrolls in its column
                // rather than pushing the note (and the strip below) away.
                egui::ScrollArea::vertical()
                    .id_salt(source)
                    .max_height((ui.available_height() - 64.0).max(80.0))
                    .show(ui, |ui| {
                        for (tag, value) in &fields {
                            // A field that differs from the other side is the
                            // point of comparison — mark it (tag and value) in
                            // the "differs" amber.
                            let differs = differing.contains(tag);
                            let tag_color = if differs {
                                theme::amber()
                            } else {
                                theme::tan()
                            };
                            let val_color = if differs {
                                theme::amber()
                            } else {
                                theme::text()
                            };
                            ui.horizontal(|ui| {
                                ui.add_sized(
                                    [140.0, 18.0],
                                    egui::Label::new(
                                        RichText::new(tag).color(tag_color).size(12.0),
                                    ),
                                );
                                ui.label(RichText::new(value).color(val_color).size(12.0));
                            });
                        }
                    });
            }
            ui.add_space(6.0);
            if ui
                .add(
                    egui::Button::new(
                        RichText::new("SAVE METADATA").color(theme::ink_on(theme::tan())),
                    )
                    .fill(theme::tan()),
                )
                .on_hover_text(
                    "Write this file's metadata to a text file in a folder you pick, so it is \
                     preserved before you delete a copy.",
                )
                .clicked()
            {
                action = MetaAction::ExportMetadata;
            }
            ui.add_space(6.0);
            ui.label(
                RichText::new("Capture metadata is shown as recorded and is not edited here.")
                    .color(theme::grey())
                    .size(11.0),
            );
        }
    }
    action
}

/// A DELETE mark pill (`DELETE` / `DELETE A` / `DELETE B`): filled red when
/// marked; when the repo is read-only the file is *protected*, so the pill shows
/// `… (Protected)`, is disabled, and struck through. Returns `true` when a
/// markable pill was clicked. Shared so the labels and the protected state
/// cannot drift between surfaces (`qa.md`: standardize DELETE / DELETE A /
/// DELETE B).
pub fn mark_pill(
    ui: &mut egui::Ui,
    verbosity: TooltipVerbosity,
    base: &str,
    marked: bool,
    markable: bool,
) -> bool {
    let label = if marked {
        format!("{base} {}", icon::CHECK)
    } else if !markable {
        format!("{base} (Protected)")
    } else {
        base.to_string()
    };
    let fill = if marked { theme::red() } else { theme::panel() };
    let col = if marked {
        theme::black()
    } else if !markable {
        theme::hairline()
    } else {
        theme::text()
    };
    let mut rt = RichText::new(label).color(col);
    if !markable {
        rt = rt.strikethrough();
    }
    let btn = egui::Button::new(rt).fill(fill);
    if !markable {
        ui.add_enabled(false, btn).on_disabled_hover_text(
            "This copy is protected: its repository is locked. Unlock it in the Duplicates \
             tab to mark this copy for deletion.",
        );
        false
    } else {
        ui.add(btn)
            .explain(
                verbosity,
                "Toggle deletion mark",
                "Toggle whether this copy is marked for deletion.",
            )
            .clicked()
    }
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

/// The Hex tab's single-file view: a forced hex dump of the file's head, always
/// bytes even for a text file (unlike [`load_text_preview`], which renders a
/// text file as text). Reuses the one hex formatter, [`hex_dump`].
pub(crate) fn hex_head_preview(path: &Path) -> TextPreview {
    use std::io::Read;
    let mut buf = Vec::new();
    let read = std::fs::File::open(path)
        .and_then(|f| f.take(HEX_DUMP_BYTES as u64 + 1).read_to_end(&mut buf));
    if let Err(e) = read {
        return TextPreview {
            body: String::new(),
            is_text: false,
            truncated: false,
            error: Some(e.to_string()),
        };
    }
    let truncated = buf.len() > HEX_DUMP_BYTES;
    let shown = buf.len().min(HEX_DUMP_BYTES);
    TextPreview {
        body: hex_dump(&buf[..shown]),
        is_text: false,
        truncated,
        error: None,
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
/// What a text/hex column says about what it is showing.
///
/// A sampled view has to *say* it is sampled: two files whose first 64 KB match
/// are not thereby identical, and the conclusion drawn from this pane can end in
/// a deletion.
fn preview_note(preview: &TextPreview) -> String {
    match (&preview.error, preview.is_text, preview.truncated) {
        (Some(e), _, _) => format!("Could not read this file: {e}"),
        (None, true, true) => "As text — only the first 64 KB, not the whole file".to_string(),
        (None, true, false) => "Full contents, as text".to_string(),
        (None, false, true) => {
            "Not text — hex of only the first bytes, not the whole file".to_string()
        }
        (None, false, false) => "Not text — full contents as hex".to_string(),
    }
}

/// `sync` scroll-locks the column to a shared offset: comparing two files as
/// bytes only means anything when both panes show the same offset (§ story
/// 31). The column applies the given offset and returns where it actually is
/// after this frame's input, so the caller can adopt whichever pane the user
/// scrolled as the new shared position.
pub fn draw_text_column(
    ui: &mut egui::Ui,
    source: &Path,
    preview: &TextPreview,
    height: f32,
    sync: Option<Vec2>,
) -> (egui::Rect, Vec2) {
    // No per-column identity header — the viewer's top strip names the side.
    // `source` is only the scroll id.
    let note = preview_note(preview);
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
    let offset = {
        let ui = &mut child;
        let mut area = egui::ScrollArea::both()
            .id_salt(source)
            .auto_shrink([false, false]);
        if let Some(o) = sync {
            area = area.scroll_offset(o);
        }
        area.show(ui, |ui| {
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
        })
        .state
        .offset
    };
    (viewport, offset)
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

#[cfg(test)]
mod tests {

    /// A sampled view must say it is a sample. Concluding two files are
    /// identical from their first kilobyte is the mistake this prevents, and it
    /// is a mistake with deletions on the other side of it.
    #[test]
    fn a_sampled_preview_discloses_that_it_is_a_sample() {
        let sampled_text = TextPreview {
            body: String::new(),
            is_text: true,
            truncated: true,
            error: None,
        };
        let note = preview_note(&sampled_text);
        assert!(
            note.contains("64 KB") && note.to_lowercase().contains("only"),
            "a truncated text view says how much it read and that it is partial: {note:?}"
        );

        let hex = TextPreview {
            is_text: false,
            ..sampled_text.clone()
        };
        assert!(
            preview_note(&hex).to_lowercase().contains("only"),
            "so does the hex view: {:?}",
            preview_note(&hex)
        );

        let whole = TextPreview {
            truncated: false,
            ..sampled_text.clone()
        };
        assert!(
            !preview_note(&whole).to_lowercase().contains("only"),
            "a complete view does not warn about sampling"
        );
    }
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
            missing: false,
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
        // An image without EXIF has nothing to put on a Metadata tab.
        assert!(!kinds.contains(&RepresentationKind::Metadata));
        // A PNG has no readable text, so no Text tab — but its raw bytes are
        // always on Hex, and any embedded runs on Strings.
        assert!(!kinds.contains(&RepresentationKind::Text));
        assert!(kinds.contains(&RepresentationKind::Hex));
        assert!(kinds.contains(&RepresentationKind::Strings));

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
            missing: false,
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

    /// A column given a shared scroll offset lands on it — the mechanism that
    /// keeps two byte panes locked to the same offset (§ story 31).
    #[test]
    fn text_columns_can_be_scroll_locked() {
        let preview = TextPreview {
            body: (0..400).map(|i| format!("line {i}\n")).collect(),
            is_text: true,
            truncated: false,
            error: None,
        };
        let mut offset = None;
        {
            let mut harness = egui_kittest::Harness::builder()
                .with_size(egui::vec2(600.0, 400.0))
                .build_ui(|ui| {
                    offset = Some(
                        draw_text_column(
                            ui,
                            Path::new("/tmp/r/long.txt"),
                            &preview,
                            300.0,
                            Some(egui::vec2(0.0, 120.0)),
                        )
                        .1,
                    );
                });
            harness.run();
        }
        assert_eq!(
            offset.expect("column drawn").y,
            120.0,
            "the column sits at the shared offset it was given"
        );
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
        let mut viewport = None;
        {
            let mut harness = egui_kittest::Harness::builder()
                .with_size(egui::vec2(600.0, 400.0))
                .build_ui(|ui| {
                    viewport = Some(
                        draw_text_column(ui, Path::new("/tmp/r/long.txt"), &preview, 300.0, None).0,
                    );
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
