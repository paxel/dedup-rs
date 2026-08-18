//! dedup-gui
//! LCARS-themed eframe/egui desktop shell for dedup.

mod app;
mod board;
mod browse_view;
mod compare_view;
mod diagnostics;
mod diff_board;
mod dupes_view;
mod external;
mod filter_ui;
mod grooming_view;
mod help_content;
mod hexdiff;
mod icon;
mod id3tags;
mod imgedit;
mod lcars;
pub mod lightbox;
mod locks;
mod media_cell;
#[cfg(target_os = "linux")]
mod menu_entry;
mod player;
mod repo_chip;
mod run_result;
mod scrub;
mod settings;
mod status;
mod textdiff;
mod theme;
mod thumbs;
mod transfer_view;
mod util;
mod waveform;
mod worker;

#[cfg(test)]
pub(crate) mod doc_media;

use dedup_core::store::Store;
use std::sync::Arc;

/// Open the desktop window. Blocks until the user closes it.
///
/// `ui_scale` (from `--ui-scale`) multiplies the interface size; `None` keeps
/// the default.
///
/// Call this before the process spawns any thread: on Linux it edits the
/// process environment (see [`prefer_x11_for_drag_and_drop`]), which is only
/// sound while the process is single-threaded.
pub fn run(ui_scale: Option<f32>) -> Result<(), String> {
    // Must be the very first thing: it edits the process environment, which
    // is only safe while no other thread exists.
    #[cfg(target_os = "linux")]
    prefer_x11_for_drag_and_drop();

    // Diagnostics only: a log that cannot be opened must not stop the app.
    match dedup_core::logging::init() {
        Ok(path) => log::info!("dedup GUI started; logging to {}", path.display()),
        Err(e) => eprintln!(
            "warning: could not open a session log in {}: {e}",
            dedup_core::logging::log_dir().display()
        ),
    }
    let store = Arc::new(Store::open().map_err(|e| e.to_string())?);

    // A brew/tarball/AppImage install has no menu entry or Wayland icon until
    // something writes them; do it in the background, never blocking startup.
    #[cfg(target_os = "linux")]
    std::thread::spawn(menu_entry::register);

    // Load the persisted settings once: the last window size (so the app opens
    // where it was left) and the appearance preference (so the very first frame
    // already resolves to the chosen theme — no one-frame flash of the wrong
    // appearance on a desktop whose system theme differs from the setting).
    let settings = settings::Settings::load(store.config_dir());
    let size = settings
        .window_size
        .map(|[w, h]| [w.clamp(760.0, 10_000.0), h.clamp(480.0, 10_000.0)])
        .unwrap_or([1100.0, 720.0]);
    let theme_pref = settings.theme.preference();
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
            // Register a style per theme and resolve to the persisted
            // preference before the first frame; the app then drives it live
            // each frame from its own copy of the setting (default Dark).
            theme::register_themes(&cc.egui_ctx);
            cc.egui_ctx.set_theme(theme_pref);
            theme::sync_active(&cc.egui_ctx);
            if let Some(scale) = ui_scale {
                cc.egui_ctx.set_zoom_factor(scale.clamp(0.5, 3.0));
            }
            Ok(Box::new(app::DedupApp::new(store)))
        }),
    )
    .map_err(|e| e.to_string())
}

/// Steer winit onto the X11 backend when a Wayland session also offers an X11
/// display (XWayland). winit 0.30 implements no drag-and-drop on Wayland at
/// all — dropped folders never reach the app — while XWayland's XDnD path
/// works (the compositor bridges native drags). Setting `DEDUP_WAYLAND` to a
/// non-empty value keeps the native Wayland backend (e.g. for crisper
/// fractional scaling) at the cost of drag-and-drop. Drop this workaround
/// once eframe ships a winit with Wayland drag-and-drop
/// (rust-windowing/winit#4571).
#[cfg(target_os = "linux")]
fn prefer_x11_for_drag_and_drop() {
    let set = |name| std::env::var_os(name).is_some_and(|v| !v.is_empty());
    if !force_x11(set("WAYLAND_DISPLAY"), set("DISPLAY"), set("DEDUP_WAYLAND")) {
        return;
    }
    // SAFETY: `run` requires (and documents) that it is called before the
    // process spawns any thread, and calls this first — so no concurrent read
    // or write of the environment is possible.
    unsafe { std::env::remove_var("WAYLAND_DISPLAY") };
}

/// Whether to drop the Wayland display in favour of X11: only when both
/// displays are available and the user didn't opt back into Wayland.
#[cfg(target_os = "linux")]
fn force_x11(wayland: bool, x11: bool, keep_wayland: bool) -> bool {
    wayland && x11 && !keep_wayland
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

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::force_x11;

    /// X11 wins only when a Wayland session also offers an XWayland display
    /// and the user didn't opt back into Wayland via `DEDUP_WAYLAND`.
    #[test]
    fn x11_is_forced_only_with_both_displays_and_no_opt_out() {
        assert!(
            force_x11(true, true, false),
            "Wayland + XWayland: force X11"
        );
        assert!(!force_x11(true, false, false), "pure Wayland: keep it");
        assert!(!force_x11(false, true, false), "plain X11: nothing to do");
        assert!(!force_x11(true, true, true), "opt-out keeps Wayland");
    }
}
