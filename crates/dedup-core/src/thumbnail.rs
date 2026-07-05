//! On-disk thumbnail cache. Thumbnails are keyed by a file's BLAKE3 content
//! hash, so byte-identical files share one thumbnail, and live under
//! `~/.cache/dedup/thumbs/<hex>.jpg` at ≤512 px on the long edge.
//!
//! The GUI decodes these into GPU textures; keeping generation here means the
//! `image` dependency (and the ≤512 px / JPEG policy) stays in the domain crate.

use std::path::{Path, PathBuf};

/// Longest edge of a generated thumbnail, in pixels.
pub const MAX_EDGE: u32 = 512;

#[derive(thiserror::Error, Debug)]
pub enum ThumbError {
    #[error("thumbnail I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("image error: {0}")]
    Image(String),
}

/// Directory holding the thumbnail cache (`~/.cache/dedup/thumbs`).
pub fn cache_dir() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home)
            .join(".cache")
            .join("dedup")
            .join("thumbs")
    } else {
        PathBuf::from(".cache").join("dedup").join("thumbs")
    }
}

/// Lowercase hex of a 32-byte content hash — the thumbnail cache key.
pub fn hash_hex(hash: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in hash {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
        s.push(char::from_digit((b & 0xf) as u32, 16).unwrap_or('0'));
    }
    s
}

/// Cache path for a given content-hash hex.
pub fn thumb_path(hash_hex: &str) -> PathBuf {
    cache_dir().join(format!("{hash_hex}.jpg"))
}

/// Generate the thumbnail JPEG for `source` at `out` if it does not exist yet.
/// The image is scaled to fit within [`MAX_EDGE`]² preserving aspect ratio.
pub fn ensure(source: &Path, out: &Path) -> Result<(), ThumbError> {
    if out.exists() {
        return Ok(());
    }
    let img = image::open(source).map_err(|e| ThumbError::Image(e.to_string()))?;
    let thumb = img.thumbnail(MAX_EDGE, MAX_EDGE);
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    thumb
        .to_rgb8()
        .save(out)
        .map_err(|e| ThumbError::Image(e.to_string()))?;
    Ok(())
}

/// Decode an image to `(width, height, rgba8)`.
pub fn load_rgba(path: &Path) -> Result<(u32, u32, Vec<u8>), ThumbError> {
    let img = image::open(path).map_err(|e| ThumbError::Image(e.to_string()))?;
    let rgba = img.to_rgba8();
    Ok((rgba.width(), rgba.height(), rgba.into_raw()))
}

/// Ensure a thumbnail for `source` (keyed by `hash_hex`) exists and return it
/// decoded as `(width, height, rgba8)`. This is the one call a GUI worker needs.
pub fn get_rgba(source: &Path, hash_hex: &str) -> Result<(u32, u32, Vec<u8>), ThumbError> {
    let out = thumb_path(hash_hex);
    ensure(source, &out)?;
    load_rgba(&out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips_known_bytes() {
        let mut h = [0u8; 32];
        h[0] = 0xde;
        h[1] = 0xad;
        h[31] = 0x0f;
        let hex = hash_hex(&h);
        assert!(hex.starts_with("dead"));
        assert!(hex.ends_with("0f"));
        assert_eq!(hex.len(), 64);
    }

    #[test]
    fn generates_and_loads_a_downscaled_thumbnail() {
        let dir = tempfile::tempdir().expect("dir");
        let src = dir.path().join("big.png");
        image::RgbImage::from_fn(1000, 600, |x, _| image::Rgb([(x % 256) as u8, 0, 0]))
            .save(&src)
            .expect("write src");

        let out = dir.path().join("thumb.jpg");
        ensure(&src, &out).expect("ensure");
        assert!(out.exists());

        let (w, h, rgba) = load_rgba(&out).expect("load");
        assert!(w <= MAX_EDGE && h <= MAX_EDGE, "scaled within bounds");
        assert_eq!(rgba.len() as u32, w * h * 4);
        // Long edge scaled to the cap, aspect preserved (1000x600 -> 512x307).
        assert_eq!(w, MAX_EDGE);
    }
}
