//! Pieces every process page shares (ADR 0006): the frames repositories are
//! dropped into, and the preview window that locks the page until GO or
//! CANCEL.

use crate::theme;
use egui::RichText;

/// A frame repositories can be dropped onto: the panel's own colour, outlined
/// in `accent` while a repository is dragged over it. Returns the dropped
/// repository's name.
pub fn drop_zone(
    ui: &mut egui::Ui,
    accent: egui::Color32,
    add: impl FnOnce(&mut egui::Ui),
) -> Option<String> {
    let response = egui::Frame::new()
        .fill(theme::panel())
        .corner_radius(6.0)
        .inner_margin(6.0)
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            add(ui);
        })
        .response;
    if response.dnd_hover_payload::<String>().is_some() {
        ui.painter().rect_stroke(
            response.rect,
            6.0,
            egui::Stroke::new(2.0, accent),
            egui::StrokeKind::Inside,
        );
    }
    response
        .dnd_release_payload::<String>()
        .map(|name| (*name).clone())
}

/// Show `body` as a page's preview: a real window of its own, which locks the
/// main window until it closes. Returns whether the user closed the window,
/// which counts as CANCEL. A backend without separate windows draws it as a
/// window inside the main one, with no lock.
pub fn preview_window(
    ctx: &egui::Context,
    key: &str,
    page: &str,
    mut body: impl FnMut(&mut egui::Ui),
) -> bool {
    let closed = ctx.show_viewport_immediate(
        egui::ViewportId::from_hash_of(key),
        egui::ViewportBuilder::default()
            .with_title(format!("dedup — {page} preview"))
            .with_inner_size([1100.0, 800.0]),
        |ui, _class| {
            egui::CentralPanel::default().show(ui, |ui| body(ui));
            ui.input(|i| i.viewport().close_requested())
        },
    );
    if !ctx.embed_viewports() {
        egui::Modal::new(egui::Id::unique(("preview-lock", key))).show(ctx, |ui| {
            ui.label(
                RichText::new("The preview is open in its own window — GO or CANCEL there.")
                    .color(theme::text()),
            );
        });
    }
    closed
}
