//! Hand files to the system's default applications — the full-fidelity escape
//! hatch for every file type (and the designated video player).

use std::io;
use std::path::Path;

/// Open `path` with the system's default application, without blocking the UI.
pub fn open(path: &Path) -> io::Result<()> {
    ::open::that_detached(path)
}

/// Reveal `path` in the file manager by opening its parent directory — the
/// portable lowest common denominator (no per-platform "select file" APIs).
pub fn reveal(path: &Path) -> io::Result<()> {
    ::open::that_detached(path.parent().unwrap_or(path))
}
