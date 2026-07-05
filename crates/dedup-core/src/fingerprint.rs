//! Perceptual fingerprints, computed per file during `update` based on MIME.
//!
//! Semantics are ported from the legacy Java fingerprinters:
//! - **Images**: 64-bit dHash of a 9×9 bilinear grayscale, canonicalized over
//!   the eight dihedral orientations so the hash is rotation/mirror invariant.
//!   The original pixel dimensions are recorded alongside.
//! - **Video**: temporal hash — dHash of frames sampled at 10/50/90 % of the
//!   duration, three `u64`s (192 bits). Requires `ffmpeg`/`ffprobe` on PATH;
//!   absent, video files degrade to content hash only.
//! - **PDF**: BLAKE3 of the normalized (lowercased, whitespace-stripped) text.
//! - **Audio**: duration plus a BLAKE3 chunk hash of the raw stream after any
//!   ID3v2 tag, matching the Java chunk scheme.
//!
//! Every step is best-effort: a decode or tool failure yields `None` for that
//! field, never an error — the content hash already identifies the file.

use crate::store::AudioFp;
use std::path::Path;
use std::process::Command;

/// The perceptual fields derived from a single file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fingerprints {
    pub mime: Option<String>,
    pub img_fingerprint: Option<u64>,
    pub img_size: Option<(u32, u32)>,
    pub video_hash: Option<[u64; 3]>,
    pub pdf_hash: Option<[u8; 32]>,
    pub audio: Option<AudioFp>,
}

/// Detect a file's MIME type: magic bytes first (`infer`), then extension
/// (`mime_guess`) as a fallback.
pub fn detect_mime(path: &Path) -> Option<String> {
    if let Ok(Some(kind)) = infer::get_from_path(path) {
        return Some(kind.mime_type().to_string());
    }
    mime_guess::from_path(path).first().map(|m| m.to_string())
}

/// Compute all applicable fingerprints for `path`, dispatching on MIME.
///
/// `ffmpeg_available` gates the (external) video path; when false, video files
/// yield no temporal hash. Pass the result of [`ffmpeg_available`], probed once.
pub fn compute(path: &Path, ffmpeg_available: bool) -> Fingerprints {
    let mime = detect_mime(path);
    let mut fp = Fingerprints {
        mime: mime.clone(),
        ..Default::default()
    };
    let mime = match mime {
        Some(m) => m,
        None => return fp,
    };

    if mime.starts_with("image/") {
        if let Some((hash, size)) = image_dhash(path) {
            fp.img_fingerprint = Some(hash);
            fp.img_size = Some(size);
        }
    } else if mime.starts_with("video/") {
        if ffmpeg_available {
            fp.video_hash = video_temporal_hash(path);
        }
    } else if mime == "application/pdf" {
        fp.pdf_hash = pdf_text_hash(path);
    } else if mime.starts_with("audio/") {
        fp.audio = audio_fingerprint(path);
    }

    fp
}

/// Is `ffmpeg` runnable on this system? Probed by invoking `ffmpeg -version`.
pub fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// --- Images -----------------------------------------------------------------

/// 64-bit dHash plus `(width, height)` of the image at `path`, or `None` if it
/// cannot be decoded.
pub fn image_dhash(path: &Path) -> Option<(u64, (u32, u32))> {
    let img = image::open(path).ok()?;
    let size = (img.width(), img.height());
    Some((dhash_from_image(&img), size))
}

/// dHash of an already-decoded image. Shared by the image and video paths.
pub fn dhash_from_image(img: &image::DynamicImage) -> u64 {
    // 9×9 bilinear grayscale, matching the Java normalization grid.
    let small =
        image::imageops::resize(&img.to_luma8(), 9, 9, image::imageops::FilterType::Triangle);
    let mut pixels = [0u8; 81];
    for (i, p) in small.pixels().enumerate() {
        pixels[i] = p.0[0];
    }
    let pixels = canonicalize(pixels);

    // 8×8 horizontal difference hash.
    let mut hash = 0u64;
    for y in 0..8 {
        for x in 0..8 {
            let left = pixels[y * 9 + x];
            let right = pixels[y * 9 + (x + 1)];
            if left > right {
                hash |= 1u64 << (y * 8 + x);
            }
        }
    }
    hash
}

/// Pick the lexicographically greatest of the eight dihedral orientations so
/// the fingerprint is invariant to rotation and mirroring.
fn canonicalize(img: [u8; 81]) -> [u8; 81] {
    let mut best = img;
    let mut current = img;
    for r in 0..4 {
        if current > best {
            best = current;
        }
        let flipped = flip_diagonal(&current);
        if flipped > best {
            best = flipped;
        }
        if r < 3 {
            current = rotate90(&current);
        }
    }
    best
}

fn rotate90(img: &[u8; 81]) -> [u8; 81] {
    let mut rotated = [0u8; 81];
    for y in 0..9 {
        for x in 0..9 {
            rotated[x * 9 + (8 - y)] = img[y * 9 + x];
        }
    }
    rotated
}

fn flip_diagonal(img: &[u8; 81]) -> [u8; 81] {
    let mut flipped = [0u8; 81];
    for y in 0..9 {
        for x in 0..9 {
            flipped[x * 9 + y] = img[y * 9 + x];
        }
    }
    flipped
}

// --- Video ------------------------------------------------------------------

/// Temporal hash: dHash of frames at 10/50/90 % of the video's duration.
/// Any frame that cannot be extracted contributes a zero hash. Returns `None`
/// only if the duration cannot be probed at all.
pub fn video_temporal_hash(path: &Path) -> Option<[u64; 3]> {
    let duration = probe_duration_secs(path)?;
    if duration <= 0.0 {
        return None;
    }
    let mut hashes = [0u64; 3];
    for (i, pct) in [0.1, 0.5, 0.9].into_iter().enumerate() {
        if let Some(frame) = extract_frame(path, duration * pct) {
            hashes[i] = dhash_from_image(&frame);
        }
    }
    Some(hashes)
}

fn probe_duration_secs(path: &Path) -> Option<f64> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.trim().parse::<f64>().ok()
}

fn extract_frame(path: &Path, at_secs: f64) -> Option<image::DynamicImage> {
    let output = Command::new("ffmpeg")
        .args(["-v", "error", "-ss"])
        .arg(format!("{at_secs}"))
        .arg("-i")
        .arg(path)
        .args(["-frames:v", "1", "-f", "image2pipe", "-c:v", "png", "-"])
        .output()
        .ok()?;
    if !output.status.success() || output.stdout.is_empty() {
        return None;
    }
    image::load_from_memory(&output.stdout).ok()
}

// --- PDF --------------------------------------------------------------------

/// BLAKE3 of the normalized text of a PDF, or `None` if it has no extractable
/// text. Normalization lowercases and strips all whitespace, matching Java.
pub fn pdf_text_hash(path: &Path) -> Option<[u8; 32]> {
    let doc = lopdf::Document::load(path).ok()?;
    let pages: Vec<u32> = doc.get_pages().keys().copied().collect();
    let text = doc.extract_text(&pages).ok()?;
    let normalized: String = text
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(|c| c.to_lowercase())
        .collect();
    if normalized.is_empty() {
        return None;
    }
    Some(*blake3::hash(normalized.as_bytes()).as_bytes())
}

// --- Audio ------------------------------------------------------------------

const AUDIO_CHUNK_SIZE: usize = 100 * 1024;

/// Duration (via symphonia) plus a BLAKE3 chunk hash of the raw stream after
/// any ID3v2 tag. Returns `None` if no content chunk can be read.
pub fn audio_fingerprint(path: &Path) -> Option<AudioFp> {
    let chunk_hash = audio_chunk_hash(path)?;
    let duration_ms = probe_audio_duration_ms(path).unwrap_or(0);
    Some(AudioFp {
        duration_ms,
        chunk_hashes: vec![chunk_hash],
    })
}

fn audio_chunk_hash(path: &Path) -> Option<[u8; 32]> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;

    // Skip an ID3v2 tag if present so metadata edits don't change the hash.
    let mut header = [0u8; 10];
    if file.read_exact(&mut header).is_ok() && &header[0..3] == b"ID3" {
        let size = (u32::from(header[6] & 0x7f) << 21)
            | (u32::from(header[7] & 0x7f) << 14)
            | (u32::from(header[8] & 0x7f) << 7)
            | u32::from(header[9] & 0x7f);
        let _ = std::io::copy(
            &mut file.by_ref().take(u64::from(size)),
            &mut std::io::sink(),
        );
    } else {
        // Not ID3v2: rewind so the header bytes count as content.
        use std::io::Seek;
        file.seek(std::io::SeekFrom::Start(0)).ok()?;
    }

    let mut chunk = vec![0u8; AUDIO_CHUNK_SIZE];
    let mut read = 0usize;
    while read < AUDIO_CHUNK_SIZE {
        match file.read(&mut chunk[read..]) {
            Ok(0) => break,
            Ok(n) => read += n,
            Err(_) => break,
        }
    }
    if read == 0 {
        return None;
    }
    Some(*blake3::hash(&chunk[..read]).as_bytes())
}

fn probe_audio_duration_ms(path: &Path) -> Option<u32> {
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let file = std::fs::File::open(path).ok()?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .ok()?;
    let track = probed.format.default_track()?;
    let params = &track.codec_params;
    let n_frames = params.n_frames?;
    let sample_rate = params.sample_rate?;
    if sample_rate == 0 {
        return None;
    }
    let ms = n_frames as f64 / f64::from(sample_rate) * 1000.0;
    Some(ms as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, Rgb, RgbImage};

    /// An asymmetric "L" shape, echoing the Java invariance fixture.
    fn l_shape() -> DynamicImage {
        let mut img = RgbImage::from_pixel(100, 100, Rgb([0, 0, 0]));
        for y in 10..90 {
            for x in 10..30 {
                img.put_pixel(x, y, Rgb([255, 255, 255]));
            }
        }
        for y in 70..90 {
            for x in 10..70 {
                img.put_pixel(x, y, Rgb([255, 255, 255]));
            }
        }
        DynamicImage::ImageRgb8(img)
    }

    fn rotate180(img: &DynamicImage) -> DynamicImage {
        DynamicImage::ImageRgb8(image::imageops::rotate180(&img.to_rgb8()))
    }

    #[test]
    fn dhash_is_invariant_to_rotation_and_mirroring() {
        let base = dhash_from_image(&l_shape());
        let rotated = dhash_from_image(&DynamicImage::ImageRgb8(image::imageops::rotate90(
            &l_shape().to_rgb8(),
        )));
        let flipped = dhash_from_image(&DynamicImage::ImageRgb8(image::imageops::flip_horizontal(
            &l_shape().to_rgb8(),
        )));
        let upside = dhash_from_image(&rotate180(&l_shape()));
        assert_eq!(base, rotated);
        assert_eq!(base, flipped);
        assert_eq!(base, upside);
    }

    #[test]
    fn dhash_differs_for_distinct_images() {
        let l = dhash_from_image(&l_shape());
        let solid = dhash_from_image(&DynamicImage::ImageRgb8(RgbImage::from_pixel(
            100,
            100,
            Rgb([128, 128, 128]),
        )));
        assert_ne!(l, solid);
    }
}
