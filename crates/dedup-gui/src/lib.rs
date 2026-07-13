//! dedup-gui
//! LCARS-themed eframe/egui desktop shell for dedup.

mod app;
mod dupes_view;
mod external;
mod icon;
mod lightbox;
mod player;
mod settings;
mod status;
mod theme;
mod thumbs;
mod transfer_view;
mod util;
mod worker;

use dedup_core::store::Store;
use std::sync::Arc;

/// Open the desktop window. Blocks until the user closes it.
///
/// `ui_scale` (from `--ui-scale`) multiplies the interface size; `None` keeps
/// the default.
pub fn run(ui_scale: Option<f32>) -> Result<(), String> {
    let store = Arc::new(Store::open().map_err(|e| e.to_string())?);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([760.0, 480.0])
            .with_title("dedup"),
        ..Default::default()
    };

    eframe::run_native(
        "dedup",
        options,
        Box::new(move |cc| {
            icon::install(&cc.egui_ctx);
            theme::apply(&cc.egui_ctx);
            if let Some(scale) = ui_scale {
                cc.egui_ctx.set_zoom_factor(scale.clamp(0.5, 3.0));
            }
            Ok(Box::new(app::DedupApp::new(store)))
        }),
    )
    .map_err(|e| e.to_string())
}
