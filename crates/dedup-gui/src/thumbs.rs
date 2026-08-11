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
                    let result = match req.job {
                        Job::Image => dedup_core::thumbnail::get_rgba(&req.source, &req.hex),
                        Job::VideoFrame { idx, count } => dedup_core::thumbnail::video_frame_rgba(
                            &req.source,
                            &req.hex,
                            idx,
                            count,
                        ),
                        Job::TextHead => unreachable!("handled above"),
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
