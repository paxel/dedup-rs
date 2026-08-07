//! Shared test helper for the `doc_screenshot_*` render tests: when the
//! `DEDUP_DOC_MEDIA` environment variable points at a folder of real media, the
//! screenshot fixtures populate their repos from it, so the documentation shows
//! genuine thumbnails, MIME breakdowns and duplicate groups instead of synthetic
//! `f0.bin` blobs. Absent the variable the fixtures fall back to their synthetic
//! seeding, so the ordinary test suite (and CI) is unaffected.
//!
//! The media folder is never committed — only the rendered PNGs are — so the
//! curated set stays out of the repository and no machine path is hard-coded.
//! The expected asset names are the ones the doc-refresh staging folder uses
//! (see `docs/gui/` regeneration notes); a folder missing an asset simply
//! yields no file for it and the fixture skips it.

use std::path::{Path, PathBuf};

/// The curated media folder, if `DEDUP_DOC_MEDIA` is set and is a directory.
pub fn dir() -> Option<PathBuf> {
    std::env::var_os("DEDUP_DOC_MEDIA")
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
}

/// Whether real doc media is available this run.
pub fn available() -> bool {
    dir().is_some()
}

/// Copy asset `name` from the media folder to `dest`, returning `true` on
/// success. `false` when no media folder is configured or the asset is absent —
/// the caller then leaves that slot to its synthetic fallback.
pub fn place(name: &str, dest: &Path) -> bool {
    let Some(d) = dir() else {
        return false;
    };
    let src = d.join(name);
    src.is_file() && std::fs::copy(&src, dest).is_ok()
}

// The staging folder is expected to hold these curated assets (each fixture
// asks for the ones it needs by name):
//   images: IMG_2019_field.jpg, wallpaper_spacehulk.jpg, bebop_blue.jpg,
//           bebop_sepia.jpg, mewtwo.png
//   video:  kitten.mp4, lynx.webm, machine.mp4
//   docs:   menu.pdf, visa_contract.pdf
