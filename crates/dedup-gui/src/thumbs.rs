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

struct Request {
    hex: String,
    source: PathBuf,
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
                    match dedup_core::thumbnail::get_rgba(&req.source, &req.hex) {
                        Ok((w, h, rgba)) => {
                            let img =
                                ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
                            let _ = dec_tx.send(Decoded::Ready(req.hex, img));
                        }
                        Err(_) => {
                            let _ = dec_tx.send(Decoded::Failed(req.hex));
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

    /// Fetch the texture for `hex`, requesting generation from `source` if it is
    /// not cached yet. Returns `None` while the thumbnail is pending or failed.
    pub fn get(&mut self, hex: &str, source: &Path) -> Option<TextureHandle> {
        if self.textures.contains_key(hex) {
            self.touch(hex);
            return self.textures.get(hex).cloned();
        }
        if self.failed.contains(hex) {
            return None;
        }
        if self.pending.insert(hex.to_string()) {
            #[cfg(test)]
            {
                self.sent += 1;
            }
            let _ = self.requests.send(Request {
                hex: hex.to_string(),
                source: source.to_path_buf(),
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
