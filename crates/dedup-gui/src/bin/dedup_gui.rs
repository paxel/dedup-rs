//! GUI-only launcher — the double-click target on Windows, where the console
//! subsystem of the CLI-capable `dedup` binary would flash a console window
//! behind the app. Built with the windows subsystem there (no console at all);
//! on other platforms it is simply the GUI without the CLI. All command-line
//! work belongs to `dedup`.
#![cfg_attr(windows, windows_subsystem = "windows")]
#![forbid(unsafe_code)]
// Shipped code may not panic: no unwrap, expect or panic, no unreachable, no
// indexing or slicing that can go out of bounds, no arithmetic that can overflow.
// Unit tests are exempt via `cfg(test)`; integration tests are separate crates.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::todo,
        clippy::unimplemented,
        clippy::unreachable,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects
    )
)]

fn main() {
    if let Err(e) = dedup_gui::run(None) {
        eprintln!("dedup-gui: {e}");
        std::process::exit(1);
    }
}
