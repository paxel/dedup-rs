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
    Align, Align2, Color32, ColorImage, Context, FontId, Id, Layout, Rect, RichText, TextureHandle,
    TextureOptions, UiBuilder, Vec2,
};

/// Largest texture edge uploaded for a full-resolution preview.
const MAX_TEXTURE_EDGE: u32 = 8192;

/// One side of a DIFF comparison: its action identity (`repo` + `rel_path`, which
/// the resulting [`crate::diff_board::BoardAction`] needs) alongside the
/// viewer-agnostic [`FileFacts`] used to preview and describe it.
pub(crate) struct DiffSide {
    pub repo: String,
    pub rel_path: String,
    pub facts: FileFacts,
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
    /// Leave both alone.
    Close,
}

/// A decoded preview arriving from a worker thread.
struct DiffLoaded {
    left: bool,
    image: Option<ColorImage>,
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
    tab: crate::lightbox::RepresentationKind,
    /// Text previews, read once per side and kept for as long as the comparison
    /// is open (a 64 KB head read per frame would be absurd).
    text: [Option<crate::lightbox::TextPreview>; 2],
    /// Stored ID3 tags per side, read once. `Some(None)` means "read, carries
    /// none" — distinct from "not read yet".
    tags: [Option<Option<crate::id3tags::Tags>>; 2],
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
            tab: crate::lightbox::RepresentationKind::Overview,
            text: [None, None],
            tags: [None, None],
        }
    }

    /// The representations each side offers, for the tab bar and the dispatch.
    /// Marks and writes belong to the caller, so both sides are read-only here:
    /// DIFF's own row commands are how a file is acted on.
    pub fn reps(&self) -> (FileRepresentations, FileRepresentations) {
        let make = |side: &DiffSide| {
            FileRepresentations::from_facts(
                &side.facts,
                side.repo.clone(),
                true,
                crate::lightbox::MarkState::Protected,
            )
        };
        (make(&self.left), make(&self.right))
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
        for (is_left, side) in [(true, &self.left), (false, &self.right)] {
            let slot = usize::from(!is_left);
            if !side.previewable() {
                self.settled[slot] = true;
                continue;
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
                let _ = tx.send(DiffLoaded {
                    left: is_left,
                    image,
                });
                ctx.request_repaint();
            });
        }
    }

    /// Upload any freshly decoded previews.
    fn poll(&mut self, ctx: &Context) {
        while let Ok(loaded) = self.rx.try_recv() {
            let slot = usize::from(!loaded.left);
            self.settled[slot] = true;
            if let Some(image) = loaded.image {
                let name = if loaded.left {
                    "diff-compare-left"
                } else {
                    "diff-compare-right"
                };
                self.tex[slot] = Some(ctx.load_texture(name, image, TextureOptions::LINEAR));
            }
        }
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
        let img = facts
            .img_size
            .map(|(w, h)| egui::vec2(w as f32, h as f32))
            .or_else(|| tex.as_ref().map(|t| t.size_vec2()))
            .unwrap_or(egui::vec2(1.0, 1.0));
        (tex, img)
    }

    /// Draw the comparison over the whole window. Returns the user's decision, or
    /// `None` while they are still looking.
    pub fn view(&mut self, ctx: &Context, verbosity: TooltipVerbosity) -> Option<DiffPick> {
        self.start(ctx);
        self.poll(ctx);

        let ready = self.compare_ready();
        // Esc steps back (flicker → side-by-side → closed); Space drives flicker
        // (enter it, then swap A/B), both only once both sides have decoded.
        let (esc, space) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::Escape),
                i.key_pressed(egui::Key::Space),
            )
        });
        if esc {
            if ready && self.compare.flicker {
                self.compare.flicker = false;
            } else {
                return Some(DiffPick::Close);
            }
        }
        if space && ready {
            if self.compare.flicker {
                self.compare.show_b = !self.compare.show_b;
            } else {
                self.compare.flicker = true;
            }
        }

        egui::Area::new(Id::new("diff-compare"))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::Pos2::ZERO)
            .show(ctx, |ui| {
                let screen = ctx.content_rect();
                let bg = ui.allocate_rect(screen, egui::Sense::click_and_drag());
                // Nearly opaque: this is a judgement call about two files, so the
                // board behind must not compete for attention.
                ui.painter()
                    .rect_filled(screen, 0.0, Color32::from_black_alpha(252));
                let inner = screen.shrink(12.0);
                let mut pick = None;

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
                                RichText::new("COMPARE — SAME PATH, DIFFERENT CONTENT")
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
                const STRIP_H: f32 = 150.0;
                let tab_h = 30.0;
                let viewport = Rect::from_min_max(
                    egui::pos2(inner.min.x, inner.min.y + 32.0 + tab_h),
                    egui::pos2(
                        inner.max.x,
                        (inner.max.y - STRIP_H).max(inner.min.y + 112.0 + tab_h),
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
                let native = || {
                    offered
                        .iter()
                        .copied()
                        .find(|k| *k != RepresentationKind::Overview)
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

                // Metadata, read-only: DIFF's row commands are how a file is
                // acted on, so there is no editor here and no second tag-writing
                // surface. `can_edit: false` is what hides the EDIT button.
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
                    let (l, r) = (&self.left, &self.right);
                    let mut child = ui.new_child(
                        UiBuilder::new()
                            .max_rect(viewport)
                            .layout(Layout::top_down(Align::Min)),
                    );
                    fn meta_body<'a>(
                        side: &'a DiffSide,
                        stored: Option<&'a crate::id3tags::Tags>,
                    ) -> crate::lightbox::MetaBody<'a> {
                        if side.facts.is_audio() {
                            crate::lightbox::MetaBody::Stored {
                                tags: stored,
                                can_edit: false,
                            }
                        } else {
                            crate::lightbox::MetaBody::Exif {
                                camera: side.facts.exif.as_ref().and_then(|e| e.camera.as_deref()),
                                taken: side
                                    .facts
                                    .exif
                                    .as_ref()
                                    .and_then(|e| e.taken_ms)
                                    .map(crate::util::format_mtime),
                            }
                        }
                    }
                    let cols: Vec<crate::lightbox::ColumnFn<'_, ()>> = vec![
                        Box::new(|ui: &mut egui::Ui, _: &mut ()| {
                            crate::lightbox::draw_metadata_column(
                                ui,
                                &ColumnHead {
                                    file_name: &l.rel_path,
                                    repo: &l.repo,
                                    accent: theme::blue(),
                                    read_only: true,
                                    is_main: false,
                                    source: &l.facts.abs_path,
                                },
                                meta_body(l, lt.as_ref()),
                            );
                        }),
                        Box::new(|ui: &mut egui::Ui, _: &mut ()| {
                            crate::lightbox::draw_metadata_column(
                                ui,
                                &ColumnHead {
                                    file_name: &r.rel_path,
                                    repo: &r.repo,
                                    accent: theme::tan(),
                                    read_only: true,
                                    is_main: false,
                                    source: &r.facts.abs_path,
                                },
                                meta_body(r, rt.as_ref()),
                            );
                        }),
                    ];
                    draw_columns(&mut child, &mut (), cols);
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
                    let cols: Vec<crate::lightbox::ColumnFn<'_, ()>> = vec![
                        Box::new(move |ui: &mut egui::Ui, _: &mut ()| {
                            if let Some(p) = &lt {
                                draw_text_column(
                                    ui,
                                    &ColumnHead {
                                        file_name: &l.rel_path,
                                        repo: &l.repo,
                                        accent: theme::blue(),
                                        read_only: true,
                                        is_main: false,
                                        source: &l.facts.abs_path,
                                    },
                                    p,
                                    height,
                                );
                            }
                        }),
                        Box::new(move |ui: &mut egui::Ui, _: &mut ()| {
                            if let Some(p) = &rt {
                                draw_text_column(
                                    ui,
                                    &ColumnHead {
                                        file_name: &r.rel_path,
                                        repo: &r.repo,
                                        accent: theme::tan(),
                                        read_only: true,
                                        is_main: false,
                                        source: &r.facts.abs_path,
                                    },
                                    p,
                                    height,
                                );
                            }
                        }),
                    ];
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

                // Facts + actions: two columns below the preview.
                let strip = Rect::from_min_max(
                    egui::pos2(inner.min.x, inner.max.y - STRIP_H + 10.0),
                    inner.max,
                );
                let col_w = (strip.width() - 16.0) * 0.5;
                ui.scope_builder(
                    UiBuilder::new()
                        .max_rect(strip)
                        .layout(Layout::left_to_right(Align::Min)),
                    |ui| {
                        for is_left in [true, false] {
                            let (side, other) = if is_left {
                                (&self.left, &self.right)
                            } else {
                                (&self.right, &self.left)
                            };
                            let picked = ui
                                .allocate_ui(egui::vec2(col_w, strip.height()), |ui| {
                                    side_strip(ui, side, other, is_left, verbosity)
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
            .inner
    }
}

/// One side's facts (repo, path, size / date / type with the bigger-or-newer
/// value highlighted so the difference reads without comparing both numbers) and
/// its OVERWRITE / DELETE actions. Returns the chosen action, if any.
fn side_strip(
    ui: &mut egui::Ui,
    side: &DiffSide,
    other: &DiffSide,
    is_left: bool,
    verbosity: TooltipVerbosity,
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

    /// A DIFF side carrying just a mime, for the previewability unit test.
    fn diff_side(mime: Option<&str>) -> DiffSide {
        DiffSide {
            repo: "r".into(),
            rel_path: "a.bin".into(),
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
        assert!(
            !kinds.contains(&RepresentationKind::Text),
            "an image is not offered as text"
        );
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
