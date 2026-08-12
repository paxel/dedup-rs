//! GUI-only launcher — the double-click target on Windows, where the console
//! subsystem of the CLI-capable `dedup` binary would flash a console window
//! behind the app. Built with the windows subsystem there (no console at all);
//! on other platforms it is simply the GUI without the CLI. All command-line
//! work belongs to `dedup`.
#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    if let Err(e) = dedup_gui::run(None) {
        eprintln!("dedup-gui: {e}");
        std::process::exit(1);
    }
}
