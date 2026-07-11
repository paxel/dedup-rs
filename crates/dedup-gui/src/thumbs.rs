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

/// What a worker should generate for a request.
enum Job {
    /// A ≤512px image thumbnail keyed by content hash.
    Image,
    /// Video still `idx` of `count` evenly spaced frames.
    VideoFrame { idx: usize, count: usize },
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
    Failed(String),
}

pub struct ThumbCache {
    requests: Sender<Request>,
    decoded: Receiver<Decoded>,
    textures: HashMap<String, TextureHandle>,
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
                    let result = match req.job {
                        Job::Image => dedup_core::thumbnail::get_rgba(&req.source, &req.hex),
                        Job::VideoFrame { idx, count } => dedup_core::thumbnail::video_frame_rgba(
                            &req.source,
                            &req.hex,
                            idx,
                            count,
                        ),
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
    /// not cached. `None` while pending or failed (e.g. no ffmpeg).
    pub fn get_video(
        &mut self,
        hex: &str,
        source: &Path,
        idx: usize,
        count: usize,
    ) -> Option<TextureHandle> {
        let key = format!("{hex}-v{idx}");
        self.get_keyed(key, hex, source, Job::VideoFrame { idx, count })
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
