//! dedup-gui
//! LCARS-themed eframe/egui desktop shell for dedup.

mod app;
mod theme;
mod worker;

use dedup_core::store::Store;
use std::sync::Arc;

/// Open the desktop window. Blocks until the user closes it.
pub fn run() -> Result<(), String> {
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
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(app::DedupApp::new(store)))
        }),
    )
    .map_err(|e| e.to_string())
}
