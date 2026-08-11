//! Thumbnail service: a small background pool decodes/generates thumbnails via
//! `dedup_core::thumbnail`, and the UI uploads results into an LRU of GPU
//! textures. The UI thread never blocks on image work; it asks for a texture by
//! content hash and gets `None` (draw a placeholder) until one is ready.

use crossbeam_channel::{Receiver, Sender};
use egui::{ColorImage, Context, TextureHandle, TextureOptions};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Max number of live GPU textures kept at once (LRU-evicted beyond this).
const TEXTURE_CAPACITY: usize = 200;

/// Max cached text heads (tiny strings, but a huge result list shouldn't hoard).
const HEAD_CAPACITY: usize = 512;
/// How much of a text file's head is read for its card preview.
const HEAD_BYTES: usize = 4096;
/// How much of a file's head feeds the byte-view bitmap, and its fixed grid
/// (4:3, one byte per pixel; identical content → identical pattern).
const BYTE_VIEW_BYTES: usize = BYTE_VIEW_W * BYTE_VIEW_H;
const BYTE_VIEW_W: usize = 128;
const BYTE_VIEW_H: usize = 96;
/// How many lines of that head a card preview keeps.
const HEAD_LINES: usize = 12;

/// What a worker should generate for a request.
enum Job {
    /// A ≤512px image thumbnail keyed by content hash.
    Image,
    /// Video still `idx` of `count` evenly spaced frames.
    VideoFrame { idx: usize, count: usize },
    /// The first lines of a text file, for the card preview.
    TextHead,
    /// The file's head bytes as a greyscale bitmap — the universal fallback
    /// preview for files with no dedicated renderer.
    ByteView,
    /// A PDF's first page as a small image (async pdftoppm; fails without
    /// poppler, and the caller falls back to the byte view).
    PdfPage,
}

struct Request {
    /// Cache/texture key: the content hash for images, `<hash>-v<idx>` for
    /// video stills.
    key: String,
    hex: String,
    source: PathBuf,
    job: Job,
}

enum Decoded {
    Ready(String, ColorImage),
    ReadyText(String, String),
    Failed(String),
}

pub struct ThumbCache {
    requests: Sender<Request>,
    decoded: Receiver<Decoded>,
    textures: HashMap<String, TextureHandle>,
    /// Text-head previews (first lines of a text file), keyed like textures.
    heads: HashMap<String, String>,
    /// Insertion order of `heads`, oldest first (FIFO-evicted at capacity).
    head_order: Vec<String>,
    /// LRU order, least-recently-used first.
    order: Vec<String>,
    pending: HashSet<String>,
    failed: HashSet<String>,
    /// Cumulative count of generation requests sent (test-only diagnostic).
    #[cfg(test)]
    sent: usize,
}

impl ThumbCache {
    pub fn new(workers: usize) -> Self {
        let (req_tx, req_rx) = crossbeam_channel::unbounded::<Request>();
        let (dec_tx, dec_rx) = crossbeam_channel::unbounded::<Decoded>();
        for _ in 0..workers.max(1) {
            let req_rx = req_rx.clone();
            let dec_tx = dec_tx.clone();
            std::thread::spawn(move || {
                while let Ok(req) = req_rx.recv() {
                    if let Job::TextHead = req.job {
                        match read_text_head(&req.source) {
                            Some(head) => {
                                let _ = dec_tx.send(Decoded::ReadyText(req.key, head));
                            }
                            None => {
                                let _ = dec_tx.send(Decoded::Failed(req.key));
                            }
                        }
                        continue;
                    }
                    if let Job::ByteView = req.job {
                        match byte_view_image(&req.source) {
                            Some(img) => {
                                let _ = dec_tx.send(Decoded::Ready(req.key, img));
                            }
                            None => {
                                let _ = dec_tx.send(Decoded::Failed(req.key));
                            }
                        }
                        continue;
                    }
                    if let Job::PdfPage = req.job {
                        match pdf_page_image(&req.source) {
                            Some(img) => {
                                let _ = dec_tx.send(Decoded::Ready(req.key, img));
                            }
                            None => {
                                let _ = dec_tx.send(Decoded::Failed(req.key));
                            }
                        }
                        continue;
                    }
                    let result = match req.job {
                        Job::Image => dedup_core::thumbnail::get_rgba(&req.source, &req.hex),
                        Job::VideoFrame { idx, count } => dedup_core::thumbnail::video_frame_rgba(
                            &req.source,
                            &req.hex,
                            idx,
                            count,
                        ),
                        Job::TextHead | Job::ByteView | Job::PdfPage => {
                            unreachable!("handled above")
                        }
                    };
                    match result {
                        Ok((w, h, rgba)) => {
                            let img =
                                ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
                            let _ = dec_tx.send(Decoded::Ready(req.key, img));
                        }
                        Err(_) => {
                            let _ = dec_tx.send(Decoded::Failed(req.key));
                        }
                    }
                }
            });
        }
        Self {
            requests: req_tx,
            decoded: dec_rx,
            textures: HashMap::new(),
            heads: HashMap::new(),
            head_order: Vec::new(),
            order: Vec::new(),
            pending: HashSet::new(),
            failed: HashSet::new(),
            #[cfg(test)]
            sent: 0,
        }
    }

    /// Cumulative number of generation requests sent since creation.
    #[cfg(test)]
    pub fn requests_sent(&self) -> usize {
        self.sent
    }

    /// Upload any freshly decoded thumbnails into textures. Call once per frame.
    /// Returns true if anything changed (so the caller can request a repaint).
    pub fn poll(&mut self, ctx: &Context) -> bool {
        let mut changed = false;
        while let Ok(decoded) = self.decoded.try_recv() {
            changed = true;
            match decoded {
                Decoded::Ready(hex, img) => {
                    let handle = ctx.load_texture(&hex, img, TextureOptions::LINEAR);
                    self.pending.remove(&hex);
                    self.touch(&hex);
                    self.textures.insert(hex, handle);
                    self.evict();
                }
                Decoded::ReadyText(key, head) => {
                    self.pending.remove(&key);
                    self.heads.insert(key.clone(), head);
                    self.head_order.push(key);
                    while self.head_order.len() > HEAD_CAPACITY {
                        let old = self.head_order.remove(0);
                        self.heads.remove(&old);
                    }
                }
                Decoded::Failed(hex) => {
                    self.pending.remove(&hex);
                    self.failed.insert(hex);
                }
            }
        }
        changed
    }

    /// Fetch the image thumbnail texture for `hex`, requesting generation from
    /// `source` if not cached. `None` while pending or failed.
    pub fn get(&mut self, hex: &str, source: &Path) -> Option<TextureHandle> {
        let key = hex.to_string();
        self.get_keyed(key, hex, source, Job::Image)
    }

    /// Fetch video still `idx` (of `count`) for `hex`, requesting extraction if
    /// not cached. `None` while pending or failed (e.g. no ffmpeg). `count` is
    /// part of the key: it decides where in the timeline still `idx` is
    /// sampled, so the same `idx` under a different grid is a different frame.
    pub fn get_video(
        &mut self,
        hex: &str,
        source: &Path,
        idx: usize,
        count: usize,
    ) -> Option<TextureHandle> {
        let key = format!("{hex}-v{idx}of{count}");
        self.get_keyed(key, hex, source, Job::VideoFrame { idx, count })
    }

    /// Fetch the first lines of text file `hex` for its card preview,
    /// requesting a background read if not cached. `None` while pending or
    /// failed — reads happen on the worker pool only, so a hung network mount
    /// can never stall a paint frame.
    pub fn get_text_head(&mut self, hex: &str, source: &Path) -> Option<String> {
        let key = format!("{hex}-t");
        if let Some(head) = self.heads.get(&key) {
            return Some(head.clone());
        }
        if self.failed.contains(&key) {
            return None;
        }
        if self.pending.insert(key.clone()) {
            #[cfg(test)]
            {
                self.sent += 1;
            }
            let _ = self.requests.send(Request {
                key,
                hex: hex.to_string(),
                source: source.to_path_buf(),
                job: Job::TextHead,
            });
        }
        None
    }

    /// Fetch the byte-view bitmap (head bytes as greyscale) for `hex`,
    /// requesting generation if not cached. `None` while pending or failed.
    pub fn get_byte_view(&mut self, hex: &str, source: &Path) -> Option<TextureHandle> {
        let key = format!("{hex}-b");
        self.get_keyed(key, hex, source, Job::ByteView)
    }

    /// Fetch the PDF first-page mini-render for `hex`. `None` while pending —
    /// and forever when poppler is absent or the file is unrenderable; the
    /// caller simply falls through to the byte view either way.
    pub fn get_pdf_page(&mut self, hex: &str, source: &Path) -> Option<TextureHandle> {
        let key = format!("{hex}-p");
        self.get_keyed(key, hex, source, Job::PdfPage)
    }

    fn get_keyed(
        &mut self,
        key: String,
        hex: &str,
        source: &Path,
        job: Job,
    ) -> Option<TextureHandle> {
        if self.textures.contains_key(&key) {
            self.touch(&key);
            return self.textures.get(&key).cloned();
        }
        if self.failed.contains(&key) {
            return None;
        }
        if self.pending.insert(key.clone()) {
            #[cfg(test)]
            {
                self.sent += 1;
            }
            let _ = self.requests.send(Request {
                key,
                hex: hex.to_string(),
                source: source.to_path_buf(),
                job,
            });
        }
        None
    }

    fn touch(&mut self, hex: &str) {
        if self.order.last().map(String::as_str) != Some(hex) {
            self.order.retain(|h| h != hex);
            self.order.push(hex.to_string());
        }
    }

    fn evict(&mut self) {
        while self.order.len() > TEXTURE_CAPACITY {
            let old = self.order.remove(0);
            self.textures.remove(&old); // dropping the handle frees the GPU texture
        }
    }
}

/// Render a file's head bytes as a greyscale bitmap: one byte per pixel on a
/// fixed 128×96 grid, row-major. Identical content yields an identical pattern
/// — two duplicates *look* the same — and different formats show their
/// characteristic texture (compressed noise, structured records, padding runs).
/// A short file leaves the tail dark; `None` when the file cannot be read.
fn byte_view_image(path: &Path) -> Option<ColorImage> {
    use std::io::Read;
    let mut buf = vec![0u8; BYTE_VIEW_BYTES];
    let mut f = std::fs::File::open(path).ok()?;
    let mut filled = 0usize;
    while filled < buf.len() {
        match f.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(_) => return None,
        }
    }
    if filled == 0 {
        return None;
    }
    let mut rgba = vec![0u8; BYTE_VIEW_W * BYTE_VIEW_H * 4];
    for (i, px) in rgba.chunks_exact_mut(4).enumerate() {
        // Bytes past the end stay near-black, reading as "the file ends here".
        let v = if i < filled { buf[i] } else { 8 };
        // Lift the floor a touch so a zero-heavy header still shows structure
        // against the pure-black card background.
        let g = 24u8.saturating_add((v as u16 * 200 / 255) as u8);
        px[0] = g;
        px[1] = g;
        px[2] = g;
        px[3] = 255;
    }
    Some(ColorImage::from_rgba_unmultiplied(
        [BYTE_VIEW_W, BYTE_VIEW_H],
        &rgba,
    ))
}

/// Rasterize a PDF's first page to a small image for its card preview, via the
/// same poppler pipeline the Render tab uses. `None` without poppler or when
/// the file is not a renderable PDF.
fn pdf_page_image(path: &Path) -> Option<ColorImage> {
    let dir = tempfile::tempdir().ok()?;
    let png = dedup_core::render::render_pdf_page(path, dir.path(), 1)?;
    let (w, h, rgba) = dedup_core::thumbnail::load_full_rgba(&png, 512).ok()?;
    Some(ColorImage::from_rgba_unmultiplied(
        [w as usize, h as usize],
        &rgba,
    ))
}

/// Read the first lines of a text file for its card preview: up to
/// [`HEAD_BYTES`] from the head, lossy UTF-8, the first [`HEAD_LINES`] lines
/// with blank runs collapsed. `None` when the file cannot be read or holds no
/// printable line.
fn read_text_head(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut buf = vec![0u8; HEAD_BYTES];
    let mut f = std::fs::File::open(path).ok()?;
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<&str> = Vec::new();
    let mut last_blank = false;
    for line in text.lines() {
        let trimmed = line.trim_end();
        let blank = trimmed.trim().is_empty();
        if blank && (last_blank || lines.is_empty()) {
            continue;
        }
        last_blank = blank;
        lines.push(trimmed);
        if lines.len() >= HEAD_LINES {
            break;
        }
    }
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        return None;
    }
    Some(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The byte view is deterministic per content and marks EOF: two files
    /// with the same head render identical bitmaps, and a short file leaves
    /// the tail near-black instead of garbage.
    #[test]
    fn byte_view_is_deterministic_and_marks_eof() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.db");
        let b = dir.path().join("b.db");
        let bytes: Vec<u8> = (0..2048u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&a, &bytes).unwrap();
        std::fs::write(&b, &bytes).unwrap();
        let ia = byte_view_image(&a).expect("readable");
        let ib = byte_view_image(&b).expect("readable");
        assert_eq!(ia.pixels, ib.pixels, "same head bytes, same picture");
        assert_eq!(ia.size, [BYTE_VIEW_W, BYTE_VIEW_H]);
        // The written 2 KiB fill only the first pixels; the rest is the dark
        // EOF floor (uniform), visibly different from the data region.
        let tail = ia.pixels[BYTE_VIEW_BYTES - 1];
        assert_eq!(
            ia.pixels[BYTE_VIEW_BYTES - 100],
            tail,
            "the tail past EOF is a uniform floor"
        );
        assert!(
            byte_view_image(&dir.path().join("missing.bin")).is_none(),
            "an unreadable file yields no bitmap"
        );
    }

    /// The text-head pipeline: a request comes back through `poll` with the
    /// file's first lines, cached for every later frame; a missing file fails
    /// quietly and is not retried.
    #[test]
    fn text_head_round_trip_and_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, "# Shopping\n\n\n\n- eggs\n- milk\n").unwrap();
        let mut cache = ThumbCache::new(1);
        let ctx = Context::default();
        assert!(
            cache.get_text_head("aaaa", &path).is_none(),
            "first ask kicks off the background read"
        );
        for _ in 0..100 {
            cache.poll(&ctx);
            if let Some(head) = cache.get_text_head("aaaa", &path) {
                assert!(head.starts_with("# Shopping"), "head starts at line one");
                assert!(
                    head.contains("- eggs") && head.contains("- milk"),
                    "later lines follow"
                );
                assert!(
                    !head.contains("\n\n\n"),
                    "blank runs are collapsed: {head:?}"
                );
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(cache.get_text_head("aaaa", &path).is_some(), "cached now");

        let gone = dir.path().join("missing.txt");
        assert!(cache.get_text_head("bbbb", &gone).is_none());
        for _ in 0..100 {
            cache.poll(&ctx);
            if cache.failed.contains("bbbb-t") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let before = cache.requests_sent();
        assert!(cache.get_text_head("bbbb", &gone).is_none());
        assert_eq!(
            cache.requests_sent(),
            before,
            "a failed head is not re-requested every frame"
        );
    }
}
