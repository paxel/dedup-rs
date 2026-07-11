//! Perceptual fingerprints, computed per file during `update` based on MIME.
//!
//! - **Images**: 512-bit gradient hash of a 17×17 bilinear grayscale,
//!   canonicalized over the eight dihedral orientations so the hash is
//!   rotation/mirror invariant; one bit per horizontal and per vertical
//!   neighbor comparison (16×16 each). The original pixel dimensions are
//!   recorded alongside.
//! - **Video**: temporal hash — 64-bit dHash of frames sampled at 10/50/90 %
//!   of the duration, three `u64`s (192 bits). Requires `ffmpeg`/`ffprobe` on
//!   PATH; absent, video files degrade to content hash only.
//! - **PDF**: BLAKE3 of the normalized (lowercased, whitespace-stripped) text.
//! - **Audio**: duration plus a BLAKE3 chunk hash of the raw stream after any
//!   ID3v2 tag, matching the Java chunk scheme.
//!
//! Every step is best-effort: a decode or tool failure yields `None` for that
//! field, never an error — the content hash already identifies the file.

use crate::store::{AudioFp, ImgHash};
use std::path::Path;
use std::process::Command;

/// The perceptual fields derived from a single file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fingerprints {
    pub mime: Option<String>,
    pub img_fingerprint: Option<ImgHash>,
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

/// Grid side of the image hash: 17×17 grayscale → 16×16 gradient bits per
/// direction, 512 bits total.
const IMG_GRID: usize = 17;

/// Image hash plus `(width, height)` of the image at `path`, or `None` if it
/// cannot be decoded.
pub fn image_dhash(path: &Path) -> Option<(ImgHash, (u32, u32))> {
    let img = image::open(path).ok()?;
    let size = (img.width(), img.height());
    Some((image_hash(&img), size))
}

/// 512-bit gradient hash of an already-decoded image: a 17×17 bilinear
/// grayscale, canonicalized by brightness moments, then one bit per horizontal
/// and per vertical neighbor comparison (16×16 each). Hashing both gradient
/// directions on a fine grid keeps smooth low-contrast photos (skies, sunsets,
/// documents) from collapsing onto a handful of shared values the way a 64-bit
/// horizontal-only dHash did.
pub fn image_hash(img: &image::DynamicImage) -> ImgHash {
    let n = IMG_GRID;
    let pixels = canonicalize_moments(gray_grid(img, n), n);
    let mut hash = [0u64; 8];
    let mut bit = 0usize;
    for y in 0..n - 1 {
        for x in 0..n - 1 {
            if pixels[y * n + x] > pixels[y * n + x + 1] {
                hash[bit / 64] |= 1u64 << (bit % 64);
            }
            bit += 1;
        }
    }
    for y in 0..n - 1 {
        for x in 0..n - 1 {
            if pixels[y * n + x] > pixels[(y + 1) * n + x] {
                hash[bit / 64] |= 1u64 << (bit % 64);
            }
            bit += 1;
        }
    }
    hash
}

/// 64-bit horizontal dHash of a 9×9 grid, canonicalized lexicographically;
/// used per video frame (images use the stronger [`image_hash`]).
/// Bit-compatible with the stored version-1 video hashes, which are not
/// rescanned — do not change its semantics.
pub fn dhash_from_image(img: &image::DynamicImage) -> u64 {
    let pixels = canonicalize_lex(gray_grid(img, 9), 9);
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

/// `n`×`n` bilinear grayscale of `img`, row-major.
fn gray_grid(img: &image::DynamicImage, n: usize) -> Vec<u8> {
    let small = image::imageops::resize(
        &img.to_luma8(),
        n as u32,
        n as u32,
        image::imageops::FilterType::Triangle,
    );
    small.pixels().map(|p| p.0[0]).collect()
}

/// Orient the grid by its brightness moments so the fingerprint is invariant
/// to rotation and mirroring: flip so the centroid falls in the top-left
/// quadrant (brightness decreasing rightward/downward keeps the strict `>`
/// gradient bits set), then transpose so the horizontal moment dominates.
/// Unlike picking the lexicographically greatest orientation (see
/// [`canonicalize_lex`]), these statistics move smoothly with pixel values, so
/// a tiny luma change in a near-duplicate cannot snap it to a completely
/// different orientation; when a moment is near zero, the orientations it
/// distinguishes are near-symmetric and hash close together anyway. Requires
/// odd `n` so the center is a cell.
fn canonicalize_moments(img: Vec<u8>, n: usize) -> Vec<u8> {
    debug_assert!(n % 2 == 1);
    let c = ((n - 1) / 2) as i64;
    let moment = |img: &[u8], horizontal: bool| -> i64 {
        let mut m = 0i64;
        for y in 0..n {
            for x in 0..n {
                let w = if horizontal { x as i64 } else { y as i64 } - c;
                m += i64::from(img[y * n + x]) * w;
            }
        }
        m
    };
    let mut out = img;
    if moment(&out, true) > 0 {
        out = flip_horizontal(&out, n);
    }
    if moment(&out, false) > 0 {
        out = flip_vertical(&out, n);
    }
    // Transposing swaps the two (now non-positive) moments.
    if moment(&out, true) > moment(&out, false) {
        out = flip_diagonal(&out, n);
    }
    out
}

/// Pick the lexicographically greatest of the eight dihedral orientations so
/// the fingerprint is invariant to rotation and mirroring. Only the video
/// path still uses this; its orientation choice is unstable for near-twins,
/// but stored video hashes depend on it.
fn canonicalize_lex(img: Vec<u8>, n: usize) -> Vec<u8> {
    let mut best = img.clone();
    let mut current = img;
    for r in 0..4 {
        if current > best {
            best = current.clone();
        }
        let flipped = flip_diagonal(&current, n);
        if flipped > best {
            best = flipped;
        }
        if r < 3 {
            current = rotate90(&current, n);
        }
    }
    best
}

fn rotate90(img: &[u8], n: usize) -> Vec<u8> {
    let mut rotated = vec![0u8; n * n];
    for y in 0..n {
        for x in 0..n {
            rotated[x * n + (n - 1 - y)] = img[y * n + x];
        }
    }
    rotated
}

fn flip_horizontal(img: &[u8], n: usize) -> Vec<u8> {
    let mut flipped = vec![0u8; n * n];
    for y in 0..n {
        for x in 0..n {
            flipped[y * n + (n - 1 - x)] = img[y * n + x];
        }
    }
    flipped
}

fn flip_vertical(img: &[u8], n: usize) -> Vec<u8> {
    let mut flipped = vec![0u8; n * n];
    for y in 0..n {
        for x in 0..n {
            flipped[(n - 1 - y) * n + x] = img[y * n + x];
        }
    }
    flipped
}

fn flip_diagonal(img: &[u8], n: usize) -> Vec<u8> {
    let mut flipped = vec![0u8; n * n];
    for y in 0..n {
        for x in 0..n {
            flipped[x * n + y] = img[y * n + x];
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

/// Extract a single video frame at `at_secs` as an image (needs `ffmpeg`).
/// Shared with the GUI's video preview (`thumbnail::video_frame_rgba`).
pub fn video_frame(path: &Path, at_secs: f64) -> Option<image::DynamicImage> {
    extract_frame(path, at_secs)
}

/// Probe a media file's duration in seconds (needs `ffprobe`).
pub fn media_duration_secs(path: &Path) -> Option<f64> {
    probe_duration_secs(path)
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
    fn image_hash_is_invariant_to_rotation_and_mirroring() {
        let base = image_hash(&l_shape());
        let rotated = image_hash(&DynamicImage::ImageRgb8(image::imageops::rotate90(
            &l_shape().to_rgb8(),
        )));
        let flipped = image_hash(&DynamicImage::ImageRgb8(image::imageops::flip_horizontal(
            &l_shape().to_rgb8(),
        )));
        let upside = image_hash(&rotate180(&l_shape()));
        assert_eq!(base, rotated);
        assert_eq!(base, flipped);
        assert_eq!(base, upside);
    }

    #[test]
    fn video_dhash_is_invariant_to_rotation() {
        let base = dhash_from_image(&l_shape());
        let rotated = dhash_from_image(&DynamicImage::ImageRgb8(image::imageops::rotate90(
            &l_shape().to_rgb8(),
        )));
        assert_eq!(base, rotated);
    }

    #[test]
    fn image_hash_differs_for_distinct_images() {
        let l = image_hash(&l_shape());
        let solid = image_hash(&DynamicImage::ImageRgb8(RgbImage::from_pixel(
            100,
            100,
            Rgb([128, 128, 128]),
        )));
        assert_ne!(l, solid);
    }

    /// Regression: lexicographic canonicalization let a tiny luma change flip
    /// a near-duplicate into a different orientation, hashing it far away.
    /// Moment-based orientation must keep near-twins close.
    #[test]
    fn image_hash_is_stable_for_near_duplicates() {
        let base = l_shape();
        let mut near = l_shape().to_rgb8();
        for y in 2..6 {
            for x in 92..96 {
                near.put_pixel(x, y, Rgb([40, 40, 40]));
            }
        }
        let a = image_hash(&base);
        let b = image_hash(&DynamicImage::ImageRgb8(near));
        let distance: u32 = (0..8).map(|k| (a[k] ^ b[k]).count_ones()).sum();
        assert!(distance <= 8, "near-twin distance {distance} of 512");
    }

    /// Regression: smooth low-contrast images collapsed onto shared values
    /// under the old 64-bit horizontal dHash (e.g. every monotonic gradient
    /// hashed identically), falsely grouping unrelated photos at 100 %.
    #[test]
    fn image_hash_separates_smooth_gradients() {
        let gradient = |f: fn(u32, u32) -> u8| {
            let mut img = RgbImage::new(100, 100);
            for y in 0..100 {
                for x in 0..100 {
                    let v = f(x, y);
                    img.put_pixel(x, y, Rgb([v, v, v]));
                }
            }
            DynamicImage::ImageRgb8(img)
        };
        let vertical = gradient(|_, y| (y * 2) as u8);
        let diagonal = gradient(|x, y| (x + y) as u8);
        assert_ne!(image_hash(&vertical), image_hash(&diagonal));
    }
}
