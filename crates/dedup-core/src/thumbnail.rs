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

/// Decode an image to `(width, height, rgba8)` at full resolution, downscaling
/// only when its longest edge exceeds `max_edge` (to stay within GPU texture
/// limits — a 50 MP photo is ~200 MB RGBA). Aspect ratio is preserved. Used by
/// the lightbox's full-resolution viewer, so `max_edge` is large (e.g. 8192).
pub fn load_full_rgba(path: &Path, max_edge: u32) -> Result<(u32, u32, Vec<u8>), ThumbError> {
    let img = image::open(path).map_err(|e| ThumbError::Image(e.to_string()))?;
    let max_edge = max_edge.max(1);
    let img = if img.width().max(img.height()) > max_edge {
        img.thumbnail(max_edge, max_edge)
    } else {
        img
    };
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

/// Cache path for still `idx` of a video (`<hex>-v<idx>.jpg`).
pub fn video_thumb_path(hash_hex: &str, idx: usize) -> PathBuf {
    cache_dir().join(format!("{hash_hex}-v{idx}.jpg"))
}

/// Ensure still `idx` (of `count` evenly spaced stills) for the video `source`
/// exists as a cached JPEG, extracting it with ffmpeg if missing. The still is
/// sampled at the midpoint of its slice of the timeline.
pub fn ensure_video_frame(
    source: &Path,
    hash_hex: &str,
    idx: usize,
    count: usize,
) -> Result<PathBuf, ThumbError> {
    let out = video_thumb_path(hash_hex, idx);
    if out.exists() {
        return Ok(out);
    }
    let count = count.max(1);
    let duration = crate::fingerprint::media_duration_secs(source).unwrap_or(0.0);
    let at = if duration > 0.0 {
        duration * (idx as f64 + 0.5) / count as f64
    } else {
        0.0
    };
    let frame = crate::fingerprint::video_frame(source, at)
        .ok_or_else(|| ThumbError::Image("video frame extraction failed".into()))?;
    let thumb = frame.thumbnail(MAX_EDGE, MAX_EDGE);
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    thumb
        .to_rgb8()
        .save(&out)
        .map_err(|e| ThumbError::Image(e.to_string()))?;
    Ok(out)
}

/// Ensure and decode video still `idx` (of `count`) as `(w, h, rgba8)` — the
/// one call a GUI worker needs for the video filmstrip / card frame. Requires
/// ffmpeg; errors (no ffmpeg, unreadable video) propagate so the caller shows a
/// placeholder.
pub fn video_frame_rgba(
    source: &Path,
    hash_hex: &str,
    idx: usize,
    count: usize,
) -> Result<(u32, u32, Vec<u8>), ThumbError> {
    let out = ensure_video_frame(source, hash_hex, idx, count)?;
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

    #[test]
    fn load_full_rgba_keeps_small_images_and_caps_large_ones() {
        let dir = tempfile::tempdir().expect("dir");

        // Below the cap: returned at native resolution.
        let small = dir.path().join("small.png");
        image::RgbImage::from_fn(300, 200, |_, _| image::Rgb([10, 20, 30]))
            .save(&small)
            .expect("write small");
        let (w, h, rgba) = load_full_rgba(&small, 8192).expect("load small");
        assert_eq!((w, h), (300, 200));
        assert_eq!(rgba.len() as u32, w * h * 4);

        // Above the cap: longest edge scaled down to the cap, aspect preserved.
        let big = dir.path().join("big.png");
        image::RgbImage::from_fn(2000, 1000, |_, _| image::Rgb([1, 2, 3]))
            .save(&big)
            .expect("write big");
        let (w, h, _) = load_full_rgba(&big, 512).expect("load big");
        assert_eq!(w, 512);
        assert_eq!(h, 256);
    }

    #[test]
    fn video_frame_extracts_and_caches_a_still() {
        if !crate::fingerprint::ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().expect("dir");
        let video = dir.path().join("clip.mp4");
        let ok = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg("testsrc=duration=2:size=128x96:rate=10")
            .args(["-pix_fmt", "yuv420p"])
            .arg(&video)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            eprintln!("skipping: ffmpeg could not generate the test clip");
            return;
        }

        // Redirect the cache into the tempdir so the test is hermetic.
        // SAFETY: single-threaded test; no other thread reads HOME concurrently.
        unsafe { std::env::set_var("HOME", dir.path()) };

        let hex = "deadbeef";
        let (w, h, rgba) = video_frame_rgba(&video, hex, 2, 10).expect("extract frame");
        assert!(w > 0 && h > 0);
        assert_eq!(rgba.len() as u32, w * h * 4);
        // The still is cached where the GUI worker expects it.
        assert!(video_thumb_path(hex, 2).exists(), "still is cached on disk");
    }
}
