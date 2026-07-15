//! Lossless-where-possible flip/rotate for the image lightbox's Edit control
//! (Phase 6.4). Non-JPEG formats round-trip bit-exact; JPEG is re-encoded at
//! high quality — a small, unavoidable loss without DCT-domain transforms, which
//! would need a C dependency. Saving is opt-in and confirmed (Phase 6.6): the
//! user picks overwrite-in-place or a `_rot` sibling copy each time.

use image::DynamicImage;
use std::path::{Path, PathBuf};

/// A single 90°-step rotation or mirror applied to the shown image.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Orient {
    RotateCw,
    RotateCcw,
    FlipH,
    FlipV,
}

/// JPEG re-encode quality for edited saves (near-lossless).
const JPEG_QUALITY: u8 = 95;

/// Apply `ops` in order to `img`.
pub fn apply_ops(mut img: DynamicImage, ops: &[Orient]) -> DynamicImage {
    for op in ops {
        img = match op {
            Orient::RotateCw => img.rotate90(),
            Orient::RotateCcw => img.rotate270(),
            Orient::FlipH => img.fliph(),
            Orient::FlipV => img.flipv(),
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

/// Load `original`, apply `ops`, and write the result. `overwrite` replaces the
/// original atomically (temp file + rename, so a failure can't truncate it);
/// otherwise a `_rot` sibling copy is written. Returns the path written.
pub fn save_edited(original: &Path, ops: &[Orient], overwrite: bool) -> Result<PathBuf, String> {
    if ops.is_empty() {
        return Err("no edits to save".into());
    }
    let format = image::ImageFormat::from_path(original).map_err(|e| e.to_string())?;
    let img = image::open(original).map_err(|e| e.to_string())?;
    let out = apply_ops(img, ops);

    if overwrite {
        let mut tmp_name = original.file_name().unwrap_or_default().to_os_string();
        tmp_name.push(".rot.tmp");
        let tmp = original.with_file_name(tmp_name);
        write(&out, &tmp, format)?;
        std::fs::rename(&tmp, original).map_err(|e| e.to_string())?;
        Ok(original.to_path_buf())
    } else {
        let target = copy_path(original);
        write(&out, &target, format)?;
        Ok(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, Rgb, RgbImage};

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

        let out = save_edited(&orig, &[Orient::RotateCw], false).unwrap();
        assert_ne!(out, orig, "a copy is a new file");
        assert!(orig.exists(), "the original is left untouched");
        let (ow, oh) = image::image_dimensions(&orig).unwrap();
        assert_eq!((ow, oh), (40, 20), "original dimensions unchanged");
        let (nw, nh) = image::image_dimensions(&out).unwrap();
        assert_eq!((nw, nh), (20, 40), "a 90° rotation swaps width/height");
    }

    #[test]
    fn overwrite_replaces_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let orig = tmp.path().join("p.png");
        RgbImage::from_fn(30, 10, |_, _| Rgb([1, 2, 3]))
            .save(&orig)
            .unwrap();

        let out = save_edited(&orig, &[Orient::RotateCw], true).unwrap();
        assert_eq!(out, orig, "overwrite returns the original path");
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
        let out = save_edited(&orig, &[Orient::FlipH], false).unwrap();
        // Re-encoded JPEG still decodes to the rotated dimensions.
        assert_eq!(image::image_dimensions(&out).unwrap(), (24, 12));
    }
}
