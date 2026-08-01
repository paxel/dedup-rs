//! 90°-step rotation and mirroring for the shared viewer's image tools, and
//! saving the turned result back to disk. Non-JPEG formats round-trip
//! bit-exact; JPEG is re-encoded at high quality — a small, unavoidable loss
//! without DCT-domain transforms, which would need a C dependency.
//!
//! Saving never silently changes the file's date: a turned scan is still the
//! same photograph from the same day, and the timeline/best-copy machinery
//! ranks by that date. The caller chooses [`SavedTime::Original`] (keep the
//! file's modified time) or [`SavedTime::At`] (stamp an explicit time — the
//! EXIF capture date, typically).

use image::DynamicImage;
use std::path::{Path, PathBuf};

/// A single 90°-step rotation or mirror applied to the shown image.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Orient {
    RotateCw,
    FlipH,
}

/// Which modified time a saved file carries.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SavedTime {
    /// Keep the original file's modified time.
    Original,
    /// Stamp an explicit time (naive epoch milliseconds) — e.g. the EXIF
    /// capture date.
    At(i64),
}

/// JPEG re-encode quality for edited saves (near-lossless).
const JPEG_QUALITY: u8 = 95;

/// Apply `ops` in order to `img`.
pub fn apply_ops(mut img: DynamicImage, ops: &[Orient]) -> DynamicImage {
    for op in ops {
        img = match op {
            Orient::RotateCw => img.rotate90(),
            Orient::FlipH => img.fliph(),
        };
    }
    img
}

/// A non-colliding `<stem>_rot<.ext>` sibling path, so a saved copy never
/// overwrites an existing file (forensic rule: never lose data).
pub fn copy_path(original: &Path) -> PathBuf {
    let stem = original
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("image");
    let ext = original.extension().and_then(|e| e.to_str());
    let dir = original.parent().unwrap_or_else(|| Path::new("."));
    let make = |suffix: &str| {
        let name = match ext {
            Some(e) => format!("{stem}{suffix}.{e}"),
            None => format!("{stem}{suffix}"),
        };
        dir.join(name)
    };
    let mut p = make("_rot");
    let mut i = 1;
    while p.exists() {
        p = make(&format!("_rot_{i}"));
        i += 1;
    }
    p
}

fn write(img: &DynamicImage, path: &Path, format: image::ImageFormat) -> Result<(), String> {
    let mut out = std::fs::File::create(path).map_err(|e| e.to_string())?;
    if format == image::ImageFormat::Jpeg {
        let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY);
        img.write_with_encoder(enc).map_err(|e| e.to_string())
    } else {
        img.write_to(&mut out, format).map_err(|e| e.to_string())
    }
}

/// Set `path`'s modified time to naive epoch milliseconds `ms`.
fn stamp_mtime(path: &Path, ms: i64) -> Result<(), String> {
    let time = if ms >= 0 {
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms as u64)
    } else {
        std::time::UNIX_EPOCH - std::time::Duration::from_millis(ms.unsigned_abs())
    };
    std::fs::File::options()
        .write(true)
        .open(path)
        .and_then(|f| f.set_modified(time))
        .map_err(|e| e.to_string())
}

/// Load `original`, apply `ops`, and write the result carrying the modified
/// time the caller chose. `overwrite` replaces the original atomically (temp
/// file + rename, so a failure can't truncate it); otherwise a `_rot` sibling
/// copy is written and the original left untouched. Returns the path written.
pub fn save_edited(
    original: &Path,
    ops: &[Orient],
    overwrite: bool,
    time: SavedTime,
) -> Result<PathBuf, String> {
    if ops.is_empty() {
        return Err("no edits to save".into());
    }
    let format = image::ImageFormat::from_path(original).map_err(|e| e.to_string())?;
    // The time to stamp is read before the original is replaced.
    let stamp = match time {
        SavedTime::At(ms) => Some(ms),
        SavedTime::Original => std::fs::metadata(original)
            .and_then(|m| m.modified())
            .ok()
            .map(|t| match t.duration_since(std::time::UNIX_EPOCH) {
                Ok(d) => d.as_millis() as i64,
                Err(e) => -(e.duration().as_millis() as i64),
            }),
    };
    let img = image::open(original).map_err(|e| e.to_string())?;
    let out = apply_ops(img, ops);

    let target = if overwrite {
        let mut tmp_name = original.file_name().unwrap_or_default().to_os_string();
        tmp_name.push(".rot.tmp");
        let tmp = original.with_file_name(tmp_name);
        write(&out, &tmp, format)?;
        std::fs::rename(&tmp, original).map_err(|e| e.to_string())?;
        original.to_path_buf()
    } else {
        let target = copy_path(original);
        write(&out, &target, format)?;
        target
    };
    if let Some(ms) = stamp {
        stamp_mtime(&target, ms)?;
    }
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, Rgb, RgbImage};

    fn mtime_ms(path: &Path) -> i64 {
        let t = std::fs::metadata(path).unwrap().modified().unwrap();
        t.duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
    }

    #[test]
    fn apply_ops_compose_to_identity() {
        let img =
            DynamicImage::ImageRgb8(RgbImage::from_fn(4, 6, |x, y| Rgb([x as u8, y as u8, 7])));
        // Two horizontal flips cancel; four CW rotations return to start.
        assert_eq!(
            apply_ops(img.clone(), &[Orient::FlipH, Orient::FlipH]).to_rgb8(),
            img.to_rgb8()
        );
        assert_eq!(
            apply_ops(
                img.clone(),
                &[
                    Orient::RotateCw,
                    Orient::RotateCw,
                    Orient::RotateCw,
                    Orient::RotateCw
                ]
            )
            .to_rgb8(),
            img.to_rgb8()
        );
    }

    #[test]
    fn save_copy_leaves_original_and_rotates_dims() {
        let tmp = tempfile::tempdir().unwrap();
        let orig = tmp.path().join("photo.png");
        RgbImage::from_fn(40, 20, |x, _| Rgb([x as u8, 0, 0]))
            .save(&orig)
            .unwrap();

        let out = save_edited(&orig, &[Orient::RotateCw], false, SavedTime::Original).unwrap();
        assert_ne!(out, orig, "a copy is a new file");
        assert!(orig.exists(), "the original is left untouched");
        let (ow, oh) = image::image_dimensions(&orig).unwrap();
        assert_eq!((ow, oh), (40, 20), "original dimensions unchanged");
        let (nw, nh) = image::image_dimensions(&out).unwrap();
        assert_eq!((nw, nh), (20, 40), "a 90° rotation swaps width/height");
    }

    /// The turned file is still the same photograph from the same date — an
    /// overwrite must not silently move it to "today", and a copy inherits the
    /// original's date for the same reason.
    #[test]
    fn saving_keeps_the_original_modified_time() {
        let tmp = tempfile::tempdir().unwrap();
        let orig = tmp.path().join("scan.png");
        RgbImage::from_fn(30, 10, |_, _| Rgb([1, 2, 3]))
            .save(&orig)
            .unwrap();
        // Give the original a distinctly old date.
        let old_ms = 1_000_000_000_000i64; // 2001-09-09
        stamp_mtime(&orig, old_ms).unwrap();

        save_edited(&orig, &[Orient::RotateCw], true, SavedTime::Original).unwrap();
        assert_eq!(
            mtime_ms(&orig),
            old_ms,
            "an overwrite keeps the file's modified time"
        );

        let copy = save_edited(&orig, &[Orient::FlipH], false, SavedTime::Original).unwrap();
        assert_eq!(
            mtime_ms(&copy),
            old_ms,
            "a saved copy inherits the original's modified time"
        );
    }

    /// The caller can stamp an explicit time instead — the EXIF capture date,
    /// for scans whose file date is meaningless.
    #[test]
    fn saving_can_stamp_an_explicit_time() {
        let tmp = tempfile::tempdir().unwrap();
        let orig = tmp.path().join("p.png");
        RgbImage::from_fn(30, 10, |_, _| Rgb([1, 2, 3]))
            .save(&orig)
            .unwrap();

        let taken_ms = 869_037_150_000i64; // 1997-07-16 05:12:30
        let out = save_edited(&orig, &[Orient::RotateCw], true, SavedTime::At(taken_ms)).unwrap();
        assert_eq!(out, orig, "overwrite returns the original path");
        assert_eq!(
            mtime_ms(&orig),
            taken_ms,
            "the file now carries the EXIF capture date"
        );
        let (w, h) = image::image_dimensions(&orig).unwrap();
        assert_eq!((w, h), (10, 30), "the original file is now rotated");
    }

    #[test]
    fn jpeg_saves_and_reloads() {
        let tmp = tempfile::tempdir().unwrap();
        let orig = tmp.path().join("j.jpg");
        RgbImage::from_fn(24, 12, |x, y| Rgb([x as u8, y as u8, 128]))
            .save(&orig)
            .unwrap();
        let out = save_edited(&orig, &[Orient::FlipH], false, SavedTime::Original).unwrap();
        // Re-encoded JPEG still decodes to the same dimensions.
        assert_eq!(image::image_dimensions(&out).unwrap(), (24, 12));
    }
}
