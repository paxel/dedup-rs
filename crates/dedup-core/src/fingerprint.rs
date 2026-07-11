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

use crate::store::{AudioFp, ExifInfo, ImgHash};
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
    pub exif: Option<ExifInfo>,
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
        fp.exif = read_exif(path);
    } else if mime.starts_with("video/") {
        if ffmpeg_available {
            fp.video_hash = video_temporal_hash(path);
        }
    } else if mime == "application/pdf" {
        fp.pdf_hash = pdf_text_hash(path);
    } else if is_office_doc(&mime) {
        // Office documents share the PDF text-hash slot: text identity groups
        // them together (and with matching PDFs) regardless of container.
        fp.pdf_hash = doc_text_hash(path, &mime);
    } else if mime.starts_with("text/") {
        // Plain text / CSV: group exports and logs that differ only by BOM,
        // line endings or trailing whitespace.
        fp.pdf_hash = text_file_hash(path);
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

// --- EXIF -------------------------------------------------------------------

/// Read capture date and camera from an image's EXIF, if present. Best-effort:
/// any parse failure yields `None` (like all fingerprints). The capture time is
/// stored as naive-local epoch milliseconds — EXIF carries no timezone.
pub fn read_exif(path: &Path) -> Option<ExifInfo> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let exif = exif::Reader::new().read_from_container(&mut reader).ok()?;

    let text = |tag: exif::Tag| -> Option<String> {
        exif.get_field(tag, exif::In::PRIMARY).map(|f| {
            f.display_value()
                .to_string()
                .trim_matches('"')
                .trim()
                .to_string()
        })
    };
    let make = text(exif::Tag::Make).filter(|s| !s.is_empty());
    let model = text(exif::Tag::Model).filter(|s| !s.is_empty());
    let camera = match (make, model) {
        (Some(mk), Some(md)) if md.starts_with(&mk) => Some(md),
        (Some(mk), Some(md)) => Some(format!("{mk} {md}")),
        (Some(mk), None) => Some(mk),
        (None, Some(md)) => Some(md),
        (None, None) => None,
    };

    let taken_ms = exif
        .get_field(exif::Tag::DateTimeOriginal, exif::In::PRIMARY)
        .or_else(|| exif.get_field(exif::Tag::DateTime, exif::In::PRIMARY))
        .and_then(|f| match &f.value {
            exif::Value::Ascii(vec) if !vec.is_empty() => exif::DateTime::from_ascii(&vec[0]).ok(),
            _ => None,
        })
        .map(|dt| exif_datetime_to_ms(&dt));

    if taken_ms.is_none() && camera.is_none() {
        return None;
    }
    Some(ExifInfo { taken_ms, camera })
}

/// Convert an EXIF `DateTime` (naive, no timezone) to epoch milliseconds,
/// treated as if UTC. Uses Howard Hinnant's days-from-civil algorithm.
fn exif_datetime_to_ms(dt: &exif::DateTime) -> i64 {
    let (y, m, d) = (dt.year as i64, dt.month as i64, dt.day as i64);
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let secs = days * 86400 + dt.hour as i64 * 3600 + dt.minute as i64 * 60 + dt.second as i64;
    secs * 1000
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

// --- Documents (PDF + office) -----------------------------------------------

/// BLAKE3 of normalized document text: lowercase, all whitespace stripped
/// (matching the Java scheme). `None` for empty text, so text-identity groups
/// documents regardless of container. Shared by PDF and office extraction.
fn text_hash(text: &str) -> Option<[u8; 32]> {
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

/// BLAKE3 of the normalized text of a PDF, or `None` if it has no extractable
/// text.
pub fn pdf_text_hash(path: &Path) -> Option<[u8; 32]> {
    let doc = lopdf::Document::load(path).ok()?;
    let pages: Vec<u32> = doc.get_pages().keys().copied().collect();
    text_hash(&doc.extract_text(&pages).ok()?)
}

/// MIME types handled by [`doc_text_hash`] (office documents). The same text
/// saved in any of these — or as a PDF — hashes identically.
pub fn is_office_doc(mime: &str) -> bool {
    matches!(
        mime,
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" // docx
            | "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" // xlsx
            | "application/vnd.openxmlformats-officedocument.presentationml.presentation" // pptx
            | "application/vnd.oasis.opendocument.text" // odt
            | "application/vnd.oasis.opendocument.spreadsheet" // ods
            | "application/vnd.oasis.opendocument.presentation" // odp
            | "application/vnd.ms-excel" // legacy xls
    )
}

/// BLAKE3 of the normalized text of an office document (docx/xlsx/pptx/odt/ods/
/// odp via zip+XML, legacy xls via calamine), or `None` if empty/unreadable or
/// encrypted. Legacy `.doc`/`.ppt` are out of scope.
pub fn doc_text_hash(path: &Path, mime: &str) -> Option<[u8; 32]> {
    let text = if mime == "application/vnd.ms-excel" {
        xls_text(path)?
    } else {
        zip_doc_text(path, mime)?
    };
    text_hash(&text)
}

/// Whether a zip entry holds body text for the given OOXML/ODF mime (styles,
/// metadata and relationships are skipped so the same text in different apps
/// hashes the same).
fn is_content_entry(name: &str, mime: &str) -> bool {
    match mime {
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => {
            name == "word/document.xml"
        }
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => {
            name == "xl/sharedStrings.xml"
        }
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => {
            name.starts_with("ppt/slides/slide") && name.ends_with(".xml")
        }
        // ODF containers keep all body text in content.xml.
        _ => name == "content.xml",
    }
}

/// Concatenated text nodes of a zip-based office document's content parts.
fn zip_doc_text(path: &Path, mime: &str) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut archive = zip::ZipArchive::new(std::io::BufReader::new(file)).ok()?;
    let names: Vec<String> = archive
        .file_names()
        .filter(|n| is_content_entry(n, mime))
        .map(String::from)
        .collect();

    let mut out = String::new();
    for name in names {
        use std::io::Read;
        let mut xml = String::new();
        if archive
            .by_name(&name)
            .ok()?
            .read_to_string(&mut xml)
            .is_err()
        {
            continue;
        }
        let mut reader = quick_xml::Reader::from_str(&xml);
        let mut buf = Vec::new();
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(quick_xml::events::Event::Text(t)) => {
                    if let Ok(text) = t.decode() {
                        out.push_str(&text);
                        out.push(' ');
                    }
                }
                Ok(quick_xml::events::Event::Eof) | Err(_) => break,
                _ => {}
            }
            buf.clear();
        }
    }
    Some(out)
}

/// Above this size a text file is hashed raw (no normalization) to bound cost.
const TEXT_NORMALIZE_CAP: u64 = 32 * 1024 * 1024;

/// BLAKE3 of a normalized text/CSV file so exports and logs that differ only by
/// BOM, line endings (CRLF vs LF) or trailing whitespace group together. Files
/// larger than [`TEXT_NORMALIZE_CAP`] are hashed raw (bounded cost); a
/// one-row/one-line difference still changes the hash.
pub fn text_file_hash(path: &Path) -> Option<[u8; 32]> {
    let meta = std::fs::metadata(path).ok()?;
    let bytes = std::fs::read(path).ok()?;
    if meta.len() > TEXT_NORMALIZE_CAP {
        return Some(*blake3::hash(&bytes).as_bytes());
    }
    let text = String::from_utf8_lossy(&bytes);
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(&text); // strip BOM
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let normalized = normalized.trim_end();
    if normalized.is_empty() {
        return None;
    }
    Some(*blake3::hash(normalized.as_bytes()).as_bytes())
}

/// Concatenated cell text of a legacy `.xls` workbook, via calamine.
fn xls_text(path: &Path) -> Option<String> {
    use calamine::Reader;
    let mut workbook: calamine::Xls<_> = calamine::open_workbook(path).ok()?;
    let mut out = String::new();
    let sheets = workbook.sheet_names().to_vec();
    for name in sheets {
        if let Ok(range) = workbook.worksheet_range(&name) {
            for row in range.rows() {
                for cell in row {
                    out.push_str(&cell.to_string());
                    out.push(' ');
                }
            }
        }
    }
    Some(out)
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

    #[test]
    fn exif_datetime_converts_to_epoch_ms() {
        // 2021-01-01 00:00:00 == 1609459200 s since the epoch (as UTC).
        let dt = exif::DateTime {
            year: 2021,
            month: 1,
            day: 1,
            hour: 0,
            minute: 0,
            second: 0,
            nanosecond: None,
            offset: None,
        };
        assert_eq!(exif_datetime_to_ms(&dt), 1_609_459_200_000);

        // A later timestamp is greater.
        let dt2 = exif::DateTime { second: 1, ..dt };
        assert_eq!(exif_datetime_to_ms(&dt2), 1_609_459_201_000);
    }

    /// Write a minimal zip with the given (name, xml) entries to `path`.
    fn write_zip(path: &std::path::Path, entries: &[(&str, &str)]) {
        use std::io::Write;
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (name, xml) in entries {
            zip.start_file(*name, opts).unwrap();
            zip.write_all(xml.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }

    const DOCX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
    const ODT: &str = "application/vnd.oasis.opendocument.text";

    /// The same text saved as .docx and .odt hashes identically (text identity,
    /// regardless of container), while different text does not.
    #[test]
    fn office_docs_group_by_text_across_containers() {
        let dir = tempfile::tempdir().unwrap();

        let docx = dir.path().join("a.docx");
        write_zip(
            &docx,
            &[(
                "word/document.xml",
                "<w:document><w:body><w:p><w:r><w:t>Hello World</w:t></w:r>\
                 <w:r><w:t> again</w:t></w:r></w:p></w:body></w:document>",
            )],
        );

        let odt = dir.path().join("a.odt");
        write_zip(
            &odt,
            &[(
                "content.xml",
                "<office:document-content><office:body><text:p>Hello World\
                 </text:p><text:p>again</text:p></office:body></office:document-content>",
            )],
        );

        let h_docx = doc_text_hash(&docx, DOCX).expect("docx text");
        let h_odt = doc_text_hash(&odt, ODT).expect("odt text");
        assert_eq!(h_docx, h_odt, "same text groups across containers");

        // A different document does not collide.
        let other = dir.path().join("b.docx");
        write_zip(
            &other,
            &[(
                "word/document.xml",
                "<w:document><w:body><w:p><w:r><w:t>Totally different</w:t></w:r>\
                 </w:p></w:body></w:document>",
            )],
        );
        assert_ne!(doc_text_hash(&other, DOCX).unwrap(), h_docx);
    }

    /// The same CSV with CRLF vs LF (and a trailing newline / BOM) groups; a
    /// one-row difference does not.
    #[test]
    fn text_files_group_across_line_endings() {
        let dir = tempfile::tempdir().unwrap();

        let lf = dir.path().join("a.csv");
        std::fs::write(&lf, b"id,name\n1,alice\n2,bob").unwrap();
        let crlf = dir.path().join("b.csv");
        std::fs::write(&crlf, "\u{FEFF}id,name\r\n1,alice\r\n2,bob\r\n".as_bytes()).unwrap();

        let h_lf = text_file_hash(&lf).expect("lf");
        let h_crlf = text_file_hash(&crlf).expect("crlf");
        assert_eq!(h_lf, h_crlf, "line-ending/BOM drift groups");

        let changed = dir.path().join("c.csv");
        std::fs::write(&changed, b"id,name\n1,alice\n2,carol").unwrap();
        assert_ne!(
            text_file_hash(&changed).unwrap(),
            h_lf,
            "row change differs"
        );
    }
}
