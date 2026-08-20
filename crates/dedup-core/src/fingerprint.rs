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
    pub video_hash: Option<[ImgHash; 3]>,
    pub pdf_hash: Option<[u8; 32]>,
    pub audio: Option<AudioFp>,
    pub exif: Option<ExifInfo>,
}

/// Detect a file's MIME type: magic bytes first (`infer`), then extension
/// (`mime_guess`) as a fallback.
pub fn detect_mime(path: &Path) -> Option<String> {
    if let Ok(Some(kind)) = infer::get_from_path(path) {
        let sniffed = kind.mime_type();
        // The MP4 container carries audio too: an audiobook/.m4a whose major
        // brand is plain `isom` sniffs as "video/mp4" (so does `file`). When
        // the container is ambiguous and the *name* says audio, trust the
        // name — the audio treatment beats a video still that cannot exist.
        if sniffed == "video/mp4"
            && let Some(named) = audio_mime_by_name(path)
        {
            return Some(named);
        }
        return Some(sniffed.to_string());
    }
    mime_guess::from_path(path).first().map(|m| m.to_string())
}

/// The `audio/*` MIME the file's extension implies (`.m4b` → `audio/m4b`),
/// `None` for anything not named as audio. The tie-breaker for audio in an
/// MP4 container, which content-sniffing alone reports as video.
pub fn audio_mime_by_name(path: &Path) -> Option<String> {
    mime_guess::from_path(path)
        .first()
        .filter(|m| m.type_() == mime_guess::mime::AUDIO)
        .map(|m| m.to_string())
}

/// Playlist MIME types that `mime_guess` reports under `audio/` (`.m3u`, `.pls`,
/// …) even though they're text playlists, not audio media. They must not get the
/// audio treatment (fingerprint, waveform, id3).
pub fn is_playlist_mime(mime: &str) -> bool {
    matches!(
        mime,
        "audio/x-mpegurl"
            | "audio/mpegurl"
            | "application/x-mpegurl"
            | "application/vnd.apple.mpegurl"
            | "audio/x-scpls"
    )
}

/// Whether `mime` denotes an actual audio media file (so it gets the audio
/// treatment). True for `audio/*` except playlist formats — see
/// [`is_playlist_mime`].
pub fn is_audio_mime(mime: &str) -> bool {
    mime.starts_with("audio/") && !is_playlist_mime(mime)
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
    } else if mime == "message/rfc822" {
        // A single email (.eml): dedup the same message exported twice.
        fp.pdf_hash = eml_hash(path);
    } else if mime.starts_with("text/") {
        // Plain text / CSV: group exports and logs that differ only by BOM,
        // line endings or trailing whitespace.
        fp.pdf_hash = text_file_hash(path);
    } else if is_audio_mime(&mime) {
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

/// Every EXIF field of the primary image, as `(tag name, display value)`
/// pairs in file order — the Metadata tab's full listing. The index keeps
/// only camera and capture date ([`read_exif`]); everything else is read
/// from the file on demand. Best-effort like every fingerprint: a file with
/// no EXIF, or none at all, yields an empty list.
pub fn exif_fields(path: &Path) -> Vec<(String, String)> {
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let mut reader = std::io::BufReader::new(file);
    let Ok(exif) = exif::Reader::new().read_from_container(&mut reader) else {
        return Vec::new();
    };
    exif.fields()
        .filter(|f| f.ifd_num == exif::In::PRIMARY)
        .map(|f| {
            (
                f.tag.to_string(),
                f.display_value().with_unit(&exif).to_string(),
            )
        })
        .collect()
}

/// Convert an EXIF `DateTime` (naive, no timezone) to epoch milliseconds,
/// treated as if UTC (via the shared civil-date math in [`crate::filter`]).
fn exif_datetime_to_ms(dt: &exif::DateTime) -> i64 {
    crate::filter::ymd_to_ms(dt.year as i64, dt.month as i64, dt.day as i64)
        + (dt.hour as i64 * 3600 + dt.minute as i64 * 60 + dt.second as i64) * 1000
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

/// Temporal hash: the 512-bit moment-canonicalized [`image_hash`] of frames at
/// 10/50/90 % of the video's duration (three frames → 1536 bits). Any frame
/// that cannot be extracted contributes a zero hash. Returns `None` only if the
/// duration cannot be probed at all. The stronger per-frame hash avoids the
/// degenerate collisions the old 64-bit dHash had on smooth/dark frames.
pub fn video_temporal_hash(path: &Path) -> Option<[ImgHash; 3]> {
    let duration = probe_duration_secs(path)?;
    if duration <= 0.0 {
        return None;
    }
    let mut hashes = [[0u64; 8]; 3];
    for (i, pct) in [0.1, 0.5, 0.9].into_iter().enumerate() {
        if let Some(frame) = extract_frame(path, duration * pct) {
            hashes[i] = image_hash(&frame);
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

/// Whether a media file carries at least one audio stream (needs `ffprobe`).
/// `false` when ffprobe is unavailable or the probe fails — callers treat that
/// as "no soundtrack to offer", the same graceful degradation video
/// fingerprints have without ffmpeg.
pub fn has_audio_track(path: &Path) -> bool {
    let Ok(output) = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "a",
            "-show_entries",
            "stream=codec_type",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path)
        .output()
    else {
        return false;
    };
    output.status.success() && !String::from_utf8_lossy(&output.stdout).trim().is_empty()
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

/// Extract a document's readable text for display — the words as extracted,
/// spacing intact, *not* the whitespace-stripped form [`text_hash`] uses for
/// dedup identity. `None` when `mime` has no extractor, or the document yields
/// no text (empty, scanned, or encrypted). Reuses the same per-format extractors
/// that back the dedup text-hash, so what groups two documents is what you read.
pub fn extract_document_text(path: &Path, mime: &str) -> Option<String> {
    let text = if mime == "application/pdf" {
        pdf_text(path)?
    } else if mime == "application/vnd.ms-excel" {
        xls_text(path)?
    } else if is_office_doc(mime) {
        zip_doc_text(path, mime)?
    } else if mime == "message/rfc822" {
        eml_text(path)?
    } else {
        return None;
    };
    (!text.trim().is_empty()).then_some(text)
}

/// Whether `mime` is a document we can extract readable text from — the binary
/// containers (PDF, office, email). Plain `text/*` is excluded: it is already
/// its own text. The cheap per-frame predicate behind the viewer's readable-text
/// tab; [`extract_document_text`] does the actual read.
pub fn is_extractable_document(mime: &str) -> bool {
    mime == "application/pdf" || is_office_doc(mime) || mime == "message/rfc822"
}

/// The raw extracted text of a PDF, or `None` if it has none — the text half of
/// [`pdf_text_hash`], shared with [`extract_document_text`].
fn pdf_text(path: &Path) -> Option<String> {
    let doc = lopdf::Document::load(path).ok()?;
    let pages: Vec<u32> = doc.get_pages().keys().copied().collect();
    doc.extract_text(&pages).ok()
}

/// BLAKE3 of the normalized text of a PDF, or `None` if it has no extractable
/// text.
pub fn pdf_text_hash(path: &Path) -> Option<[u8; 32]> {
    text_hash(&pdf_text(path)?)
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
                // A paragraph (`w:p`/`a:p`/`text:p`) or spreadsheet shared string
                // (`si`) ends a line, so paragraph structure survives for the
                // content diff. Whitespace-neutral for `text_hash`.
                Ok(quick_xml::events::Event::End(e))
                    if matches!(e.local_name().as_ref(), b"p" | b"si") =>
                {
                    out.push('\n');
                }
                Ok(quick_xml::events::Event::Eof) | Err(_) => break,
                _ => {}
            }
            buf.clear();
        }
    }
    Some(out)
}

/// BLAKE3 identity of an `.eml` email: its `Message-ID` when present (so the
/// same mail exported twice dedups reliably), else a normalized subject+body
/// digest. `None` if unparseable. mbox stores are out of scope here.
pub fn eml_hash(path: &Path) -> Option<[u8; 32]> {
    let bytes = std::fs::read(path).ok()?;
    let msg = mail_parser::MessageParser::default().parse(&bytes)?;
    let basis = match msg.message_id() {
        Some(id) => format!("message-id:{id}"),
        None => {
            let subject = msg.subject().unwrap_or_default();
            let body = msg.body_text(0).unwrap_or_default();
            format!("{subject}\n{body}")
        }
    };
    text_hash(&basis)
}

/// The readable text of an `.eml` — its subject and body — for display. Distinct
/// from [`eml_hash`], which keys on Message-ID for dedup identity.
fn eml_text(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let msg = mail_parser::MessageParser::default().parse(&bytes)?;
    let subject = msg.subject().unwrap_or_default();
    let body = msg.body_text(0).unwrap_or_default();
    Some(format!("{subject}\n{body}"))
}

/// Above this size a text file gets no normalized hash, bounding both the read
/// and the in-memory copy. A raw hash would add nothing: it equals the entry's
/// BLAKE3 content hash, so exact-duplicate search already covers those files.
const TEXT_NORMALIZE_CAP: u64 = 32 * 1024 * 1024;

/// BLAKE3 of a normalized text/CSV file so exports and logs that differ only by
/// BOM, line endings (CRLF vs LF) or trailing whitespace group together. Files
/// larger than [`TEXT_NORMALIZE_CAP`] get `None` (their raw identity is the
/// content hash); a one-row/one-line difference still changes the hash.
pub fn text_file_hash(path: &Path) -> Option<[u8; 32]> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > TEXT_NORMALIZE_CAP {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
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

    /// A minimal JPEG whose APP1 segment carries a hand-built little-endian
    /// TIFF with three ASCII fields: Make "Fuj", Model "X", and a DateTime.
    fn jpeg_with_exif(path: &std::path::Path) {
        let mut tiff: Vec<u8> = Vec::new();
        tiff.extend_from_slice(b"II"); // little-endian
        tiff.extend_from_slice(&0x2Au16.to_le_bytes());
        tiff.extend_from_slice(&8u32.to_le_bytes()); // IFD0 at offset 8
        // IFD0: three entries, sorted by tag.
        tiff.extend_from_slice(&3u16.to_le_bytes());
        let entry = |tiff: &mut Vec<u8>, tag: u16, count: u32, value: [u8; 4]| {
            tiff.extend_from_slice(&tag.to_le_bytes());
            tiff.extend_from_slice(&2u16.to_le_bytes()); // ASCII
            tiff.extend_from_slice(&count.to_le_bytes());
            tiff.extend_from_slice(&value);
        };
        entry(&mut tiff, 0x010F, 4, *b"Fuj\0"); // Make, inline
        entry(&mut tiff, 0x0110, 2, *b"X\0\0\0"); // Model, inline
        // DateTime is 20 bytes, so it lives after the IFD: 8 + 2 + 36 + 4 = 50.
        entry(&mut tiff, 0x0132, 20, 50u32.to_le_bytes());
        tiff.extend_from_slice(&0u32.to_le_bytes()); // no next IFD
        tiff.extend_from_slice(b"2004:01:06 18:42:00\0");

        let mut app1: Vec<u8> = Vec::new();
        app1.extend_from_slice(b"Exif\0\0");
        app1.extend_from_slice(&tiff);

        let mut jpeg: Vec<u8> = vec![0xFF, 0xD8]; // SOI
        jpeg.extend_from_slice(&[0xFF, 0xE1]); // APP1
        jpeg.extend_from_slice(&((app1.len() as u16 + 2).to_be_bytes()));
        jpeg.extend_from_slice(&app1);
        jpeg.extend_from_slice(&[0xFF, 0xD9]); // EOI
        std::fs::write(path, jpeg).unwrap();
    }

    /// An audiobook (`.m4b`) and a plain `.m4a` live in the same MP4 container
    /// a video does — the sniffer alone says "video/mp4" for all of them. The
    /// audio-named ones must detect as audio; a real `.mp4` stays video.
    #[test]
    fn mp4_container_audio_detects_as_audio_by_name() {
        // A minimal `ftyp isom` header — exactly what an audible audiobook
        // starts with (M4A/M4B only appear among the *compatible* brands).
        let mut head: Vec<u8> = Vec::new();
        head.extend_from_slice(&[0x00, 0x00, 0x00, 0x20]);
        head.extend_from_slice(b"ftypisom");
        head.extend_from_slice(&[0x00, 0x00, 0x02, 0x00]);
        head.extend_from_slice(b"iso2mp41M4A M4B ");
        let tmp = tempfile::tempdir().unwrap();
        for (name, expect) in [
            ("book.m4b", "audio/m4b"),
            ("song.m4a", "audio/m4a"),
            ("clip.mp4", "video/mp4"),
        ] {
            let path = tmp.path().join(name);
            std::fs::write(&path, &head).unwrap();
            assert_eq!(
                detect_mime(&path).as_deref(),
                Some(expect),
                "{name} detects as {expect}"
            );
        }
    }

    /// The Metadata view lists *every* EXIF field, not only the two the index
    /// keeps — EXIF carries far more than camera and date.
    #[test]
    fn exif_fields_lists_every_primary_field() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("shot.jpg");
        jpeg_with_exif(&path);

        let fields = exif_fields(&path);
        let get = |tag: &str| -> Option<&str> {
            fields
                .iter()
                .find(|(t, _)| t == tag)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(fields.len(), 3, "all three fields are listed: {fields:?}");
        assert!(
            get("Make").is_some_and(|v| v.contains("Fuj")),
            "Make is listed with its value: {fields:?}"
        );
        assert!(
            get("DateTime").is_some_and(|v| v.contains("2004")),
            "so is the capture date: {fields:?}"
        );

        // Best-effort like every fingerprint: no EXIF (or no file) → empty.
        let plain = tmp.path().join("plain.png");
        RgbImage::from_fn(4, 4, |_, _| Rgb([1, 2, 3]))
            .save(&plain)
            .unwrap();
        assert!(exif_fields(&plain).is_empty());
        assert!(exif_fields(&tmp.path().join("missing.jpg")).is_empty());
    }

    #[test]
    fn playlists_are_not_audio_media() {
        // Real audio media gets the audio treatment.
        assert!(is_audio_mime("audio/mpeg"));
        assert!(is_audio_mime("audio/x-wav"));
        assert!(is_audio_mime("audio/flac"));
        // Playlists that mime_guess files under audio/ do not.
        assert!(!is_audio_mime("audio/x-mpegurl")); // .m3u
        assert!(!is_audio_mime("audio/mpegurl"));
        assert!(!is_audio_mime("audio/x-scpls")); // .pls
        assert!(is_playlist_mime("audio/x-mpegurl"));
        // .m3u8 isn't audio/* at all, so it was never treated as audio.
        assert!(!is_audio_mime("application/vnd.apple.mpegurl"));
    }

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

    /// A document's readable text is returned *as extracted* — the actual words,
    /// spacing and all — not the whitespace-stripped form [`text_hash`] uses for
    /// dedup identity. This is what the viewer shows.
    #[test]
    fn extract_document_text_returns_raw_words() {
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
        let text = extract_document_text(&docx, DOCX).expect("docx text");
        assert!(text.contains("Hello World"), "words present: {text:?}");
        assert!(text.contains("again"));
        // Un-normalized: spacing survives (the hashing form strips all of it).
        assert!(text.contains(' '), "spacing preserved, not stripped");
    }

    /// A mime with no document extractor (a JPEG) yields no text — the viewer
    /// uses that to decide the readable-text tab is absent.
    #[test]
    fn extract_document_text_is_none_for_non_documents() {
        let dir = tempfile::tempdir().unwrap();
        let jpg = dir.path().join("a.jpg");
        std::fs::write(&jpg, [0xff, 0xd8, 0xff, 0xe0]).unwrap();
        assert!(extract_document_text(&jpg, "image/jpeg").is_none());
    }

    /// An `.eml` extracts its subject and body as readable text — distinct from
    /// the Message-ID identity `eml_hash` keys on.
    #[test]
    fn extract_document_text_reads_eml_subject_and_body() {
        let dir = tempfile::tempdir().unwrap();
        let eml = dir.path().join("m.eml");
        std::fs::write(
            &eml,
            "From: a@example.com\r\nSubject: Quarterly Report\r\n\r\nRevenue was up.\r\n"
                .as_bytes(),
        )
        .unwrap();
        let text = extract_document_text(&eml, "message/rfc822").expect("eml text");
        assert!(text.contains("Quarterly Report"), "subject: {text:?}");
        assert!(text.contains("Revenue was up"), "body: {text:?}");
    }

    /// A document container with no body text yields None, so the viewer shows
    /// its empty state rather than a blank diff.
    #[test]
    fn extract_document_text_is_none_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        let docx = dir.path().join("empty.docx");
        write_zip(
            &docx,
            &[(
                "word/document.xml",
                "<w:document><w:body></w:body></w:document>",
            )],
        );
        assert!(extract_document_text(&docx, DOCX).is_none());
    }

    /// Paragraph boundaries survive extraction as line breaks, so a Word/Office
    /// document's content diffs line by line instead of as one wrapped run. This
    /// is hash-neutral — [`text_hash`] strips all whitespace anyway.
    #[test]
    fn extract_document_text_keeps_paragraph_line_breaks() {
        let dir = tempfile::tempdir().unwrap();
        let docx = dir.path().join("multi.docx");
        write_zip(
            &docx,
            &[(
                "word/document.xml",
                "<w:document><w:body>\
                 <w:p><w:r><w:t>First paragraph</w:t></w:r></w:p>\
                 <w:p><w:r><w:t>Second paragraph</w:t></w:r></w:p>\
                 </w:body></w:document>",
            )],
        );
        let text = extract_document_text(&docx, DOCX).expect("docx text");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "two paragraphs → two lines: {text:?}");
        assert!(lines[0].contains("First paragraph"));
        assert!(lines[1].contains("Second paragraph"));
    }

    /// The readable-text tab is offered only for the document formats we can
    /// extract — PDF, office, email — not plain text (already text) or binary.
    #[test]
    fn is_extractable_document_covers_pdf_office_email_only() {
        assert!(is_extractable_document("application/pdf"));
        assert!(is_extractable_document(DOCX));
        assert!(is_extractable_document("message/rfc822"));
        assert!(!is_extractable_document("text/plain"));
        assert!(!is_extractable_document("image/jpeg"));
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

    /// The same email exported twice (same Message-ID) hashes identically;
    /// a different message does not.
    #[test]
    fn eml_files_group_by_message_id() {
        let dir = tempfile::tempdir().unwrap();
        let mail = |mid: &str, body: &str| {
            format!(
                "From: a@example.com\r\nTo: b@example.com\r\nSubject: Hi\r\n\
                 Message-ID: <{mid}>\r\n\r\n{body}\r\n"
            )
        };

        let one = dir.path().join("one.eml");
        std::fs::write(&one, mail("abc@host", "Hello there").as_bytes()).unwrap();
        // Same Message-ID, trivially different body (a re-export).
        let copy = dir.path().join("copy.eml");
        std::fs::write(&copy, mail("abc@host", "Hello  there  ").as_bytes()).unwrap();
        // Different Message-ID.
        let other = dir.path().join("other.eml");
        std::fs::write(&other, mail("xyz@host", "Hello there").as_bytes()).unwrap();

        let h_one = eml_hash(&one).expect("one");
        assert_eq!(h_one, eml_hash(&copy).unwrap(), "same Message-ID groups");
        assert_ne!(
            h_one,
            eml_hash(&other).unwrap(),
            "different Message-ID differs"
        );
    }
}
