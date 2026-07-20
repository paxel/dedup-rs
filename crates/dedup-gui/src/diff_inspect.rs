//! Side-by-side inspection of one diff row: the two versions of a file that
//! share a path but not their content (the DIFF board's BY PATH conflicts).
//!
//! It answers the question the table cannot — *which of these two do I want?* —
//! by showing both versions full-window with a preview appropriate to the file
//! type (image, video still) plus the numbers that decide it (size, date,
//! type), and it offers the same actions the row does, so the decision can be
//! made right where it is being judged.
//!
//! Previews decode off the UI thread (a large photo must never freeze the
//! window); until one lands the pane says so.

use crate::settings::TooltipVerbosity;
use crate::theme;
use crate::util::{ExplainExt, format_mtime, format_size};
use crossbeam_channel::{Receiver, Sender};
use egui::{ColorImage, Context, Rect, RichText, TextureHandle, TextureOptions, Vec2};
use std::path::PathBuf;

/// Longest edge uploaded to the GPU, matching the lightbox's limit.
const MAX_TEXTURE_EDGE: u32 = 8192;

/// One side of the comparison: which repo the file lives in and everything
/// needed to preview and judge it.
pub struct InspectSide {
    pub repo: String,
    pub rel_path: String,
    pub abs_path: PathBuf,
    pub size: u64,
    pub modified_ms: i64,
    pub mime: Option<String>,
    /// Content hash (hex), used as the video still's cache key.
    pub hash_hex: String,
}

impl InspectSide {
    fn is_image(&self) -> bool {
        self.mime
            .as_deref()
            .is_some_and(|m| m.starts_with("image/"))
    }

    fn is_video(&self) -> bool {
        self.mime
            .as_deref()
            .is_some_and(|m| m.starts_with("video/"))
    }

    /// What to say when there is no picture to show.
    fn placeholder(&self) -> String {
        match self.mime.as_deref() {
            Some(mime) => format!("no preview for {mime}"),
            None => "no preview for this file type".to_string(),
        }
    }
}

/// What the user decided in the inspector.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InspectOutcome {
    /// Delete this side's file.
    Delete { on_left: bool },
    /// Replace the other side's file with this side's content.
    Overwrite { from_left: bool },
    /// Leave both alone.
    Close,
}

/// A decoded preview arriving from a worker thread.
struct Loaded {
    left: bool,
    image: Option<ColorImage>,
}

/// The open inspector: both sides plus their (asynchronously decoded)
/// previews.
pub struct Inspect {
    pub left: InspectSide,
    pub right: InspectSide,
    tex: [Option<TextureHandle>; 2],
    /// Whether that side's decode has come back (successfully or not).
    settled: [bool; 2],
    tx: Sender<Loaded>,
    rx: Receiver<Loaded>,
    started: bool,
}

impl Inspect {
    pub fn new(left: InspectSide, right: InspectSide) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        Self {
            left,
            right,
            tex: [None, None],
            settled: [false, false],
            tx,
            rx,
            started: false,
        }
    }

    /// Kick off both decodes once, off the UI thread.
    fn start(&mut self, ctx: &Context) {
        if self.started {
            return;
        }
        self.started = true;
        for (is_left, side) in [(true, &self.left), (false, &self.right)] {
            if !side.is_image() && !side.is_video() {
                self.settled[usize::from(!is_left)] = true;
                continue;
            }
            let tx = self.tx.clone();
            let ctx = ctx.clone();
            let path = side.abs_path.clone();
            let hex = side.hash_hex.clone();
            let video = side.is_video();
            std::thread::spawn(move || {
                let decoded = if video {
                    // One still is enough to tell two clips apart at a glance.
                    dedup_core::thumbnail::video_frame_rgba(&path, &hex, 0, 1).ok()
                } else {
                    dedup_core::thumbnail::load_full_rgba(&path, MAX_TEXTURE_EDGE).ok()
                };
                let image = decoded.map(|(w, h, rgba)| {
                    ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba)
                });
                let _ = tx.send(Loaded {
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
                    "diff-inspect-left"
                } else {
                    "diff-inspect-right"
                };
                self.tex[slot] = Some(ctx.load_texture(name, image, TextureOptions::LINEAR));
            }
        }
    }

    /// Draw the inspector over the whole window. Returns the user's decision,
    /// or `None` while they are still looking.
    pub fn view(&mut self, ctx: &Context, verbosity: TooltipVerbosity) -> Option<InspectOutcome> {
        self.start(ctx);
        self.poll(ctx);

        let mut outcome = None;
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            return Some(InspectOutcome::Close);
        }
        egui::Area::new(egui::Id::new("diff-inspect"))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::Pos2::ZERO)
            .show(ctx, |ui| {
                let screen = ctx.content_rect();
                ui.allocate_rect(screen, egui::Sense::click());
                // Nearly opaque: this is a judgement call about two files, so
                // the board behind must not compete for attention.
                ui.painter()
                    .rect_filled(screen, 0.0, egui::Color32::from_black_alpha(252));
                let mut child = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(screen.shrink(12.0))
                        .layout(egui::Layout::top_down(egui::Align::Min)),
                );
                let ui = &mut child;
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("COMPARE — SAME PATH, DIFFERENT CONTENT")
                            .color(theme::TAN)
                            .size(16.0)
                            .strong(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .button(RichText::new("CLOSE").color(theme::BLACK))
                            .explain(
                                verbosity,
                                "Close the comparison",
                                "Close this view and go back to the diff board. Nothing is \
                                 changed.",
                            )
                            .clicked()
                        {
                            outcome = Some(InspectOutcome::Close);
                        }
                    });
                });
                ui.add_space(6.0);

                // Two equal panes, each with its preview above its facts.
                let avail = ui.available_size();
                let pane_width = (avail.x - 16.0) * 0.5;
                let pane_height = avail.y - 8.0;
                ui.horizontal_top(|ui| {
                    for is_left in [true, false] {
                        let picked = pane(
                            ui,
                            self,
                            is_left,
                            Vec2::new(pane_width, pane_height),
                            verbosity,
                        );
                        if picked.is_some() {
                            outcome = picked;
                        }
                        ui.add_space(16.0);
                    }
                });
            });
        outcome
    }
}

/// One side's pane: preview, facts, actions.
fn pane(
    ui: &mut egui::Ui,
    inspect: &Inspect,
    is_left: bool,
    size: Vec2,
    verbosity: TooltipVerbosity,
) -> Option<InspectOutcome> {
    let slot = usize::from(!is_left);
    let side = if is_left {
        &inspect.left
    } else {
        &inspect.right
    };
    let mut outcome = None;
    ui.allocate_ui(size, |ui| {
        ui.vertical(|ui| {
            ui.label(
                RichText::new(&side.repo)
                    .color(if is_left { theme::ORANGE } else { theme::BLUE })
                    .size(14.0)
                    .strong(),
            );
            ui.label(RichText::new(&side.rel_path).color(theme::TEXT).size(12.0));
            ui.add_space(4.0);

            // Preview area: the image (or video still) fitted, or a word about
            // why there is none. The rest of the pane is reserved for the
            // facts and the buttons, which must never fall off the bottom.
            let preview_height = (size.y - 190.0).max(80.0);
            let (rect, _) =
                ui.allocate_exact_size(Vec2::new(size.x, preview_height), egui::Sense::hover());
            match &inspect.tex[slot] {
                Some(tex) => {
                    let fitted = crate::lightbox::fit_rect(rect, tex.size_vec2());
                    ui.painter().image(
                        tex.id(),
                        fitted,
                        Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        egui::Color32::WHITE,
                    );
                }
                None => {
                    let text = if inspect.settled[slot] {
                        side.placeholder()
                    } else {
                        "decoding…".to_string()
                    };
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        text,
                        egui::FontId::proportional(14.0),
                        theme::TAN,
                    );
                }
            }

            // The facts that decide it. The bigger/newer value is highlighted,
            // so the difference is visible without reading both numbers.
            let other = if is_left {
                &inspect.right
            } else {
                &inspect.left
            };
            let size_color = if side.size > other.size {
                theme::GREEN
            } else {
                theme::TEXT
            };
            let date_color = if side.modified_ms > other.modified_ms {
                theme::GREEN
            } else {
                theme::TEXT
            };
            ui.add_space(4.0);
            ui.label(
                RichText::new(format_size(side.size))
                    .color(size_color)
                    .size(13.0)
                    .strong(),
            );
            ui.label(
                RichText::new(format_mtime(side.modified_ms))
                    .color(date_color)
                    .size(13.0),
            );
            ui.label(
                RichText::new(side.mime.clone().unwrap_or_else(|| "unknown type".into()))
                    .color(theme::GREY)
                    .size(12.0),
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui
                    .add(
                        egui::Button::new(RichText::new("OVERWRITE OTHER").color(theme::BLACK))
                            .fill(theme::TAN),
                    )
                    .explain(
                        verbosity,
                        "Replace the other side with this version",
                        "Copy this version over the other repository's file, so both \
                         repositories hold this one. The other version is gone afterwards.",
                    )
                    .clicked()
                {
                    outcome = Some(InspectOutcome::Overwrite { from_left: is_left });
                }
                if ui
                    .add(
                        egui::Button::new(RichText::new("DELETE").color(theme::BLACK))
                            .fill(theme::RED),
                    )
                    .explain(
                        verbosity,
                        "Delete this version",
                        "Delete this file from this repository. The other repository's \
                         version is left alone. This cannot be undone.",
                    )
                    .clicked()
                {
                    outcome = Some(InspectOutcome::Delete { on_left: is_left });
                }
            });
        });
    });
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    fn side(mime: Option<&str>) -> InspectSide {
        InspectSide {
            repo: "r".into(),
            rel_path: "a.bin".into(),
            abs_path: PathBuf::from("/tmp/a.bin"),
            size: 1,
            modified_ms: 1,
            mime: mime.map(str::to_string),
            hash_hex: "deadbeef".into(),
        }
    }

    #[test]
    fn preview_kind_follows_the_mime_type() {
        assert!(side(Some("image/jpeg")).is_image());
        assert!(side(Some("video/mp4")).is_video());
        let doc = side(Some("application/pdf"));
        assert!(!doc.is_image() && !doc.is_video());
        assert!(
            doc.placeholder().contains("application/pdf"),
            "the pane says which type it cannot preview"
        );
        assert!(side(None).placeholder().contains("file type"));
    }
}
