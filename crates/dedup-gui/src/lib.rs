//! dedup-gui
//! LCARS-themed eframe/egui desktop shell for dedup.

mod app;
mod browse_view;
mod diff_board;
mod diff_inspect;
mod dupes_view;
mod external;
mod filter_ui;
mod grooming_view;
mod help_content;
mod icon;
mod id3tags;
mod imgedit;
mod lcars;
mod lightbox;
mod media_cell;
mod player;
mod repo_chip;
mod review;
mod run_result;
mod settings;
mod status;
mod sync_view;
mod theme;
mod thumbs;
mod transfer_view;
mod util;
mod waveform;
mod worker;

use dedup_core::store::Store;
use std::sync::Arc;

/// Open the desktop window. Blocks until the user closes it.
///
/// `ui_scale` (from `--ui-scale`) multiplies the interface size; `None` keeps
/// the default.
pub fn run(ui_scale: Option<f32>) -> Result<(), String> {
    // Diagnostics only: a log that cannot be opened must not stop the app.
    match dedup_core::logging::init() {
        Ok(path) => log::info!("dedup GUI started; logging to {}", path.display()),
        Err(e) => eprintln!(
            "warning: could not open a session log in {}: {e}",
            dedup_core::logging::log_dir().display()
        ),
    }
    let store = Arc::new(Store::open().map_err(|e| e.to_string())?);

    // Restore the last window size (clamped to something sane), so the app opens
    // where it was left instead of a fixed default.
    let size = settings::Settings::load(store.config_dir())
        .window_size
        .map(|[w, h]| [w.clamp(760.0, 10_000.0), h.clamp(480.0, 10_000.0)])
        .unwrap_or([1100.0, 720.0]);
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size(size)
        .with_min_inner_size([760.0, 480.0])
        .with_title("dedup")
        // On Wayland the compositor ignores `with_icon` and instead matches the
        // window's app_id to an installed `<app_id>.desktop` file for the
        // taskbar/titlebar icon; keep this stable and matching `dedup.desktop`.
        .with_app_id("dedup");
    if let Some(icon) = load_icon() {
        viewport = viewport.with_icon(Arc::new(icon));
    }
    let options = eframe::NativeOptions {
        viewport,
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

/// Decode the embedded app icon (two cat heads, one crossed through) into the
/// RGBA form eframe wants for the window/taskbar icon. Returns `None` if the
/// bundled PNG ever fails to decode, so a bad asset never blocks startup.
fn load_icon() -> Option<egui::IconData> {
    let image = image::load_from_memory(include_bytes!("../assets/icon.png"))
        .ok()?
        .into_rgba8();
    let (width, height) = image.dimensions();
    Some(egui::IconData {
        rgba: image.into_raw(),
        width,
        height,
    })
}
