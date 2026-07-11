//! Full-window image viewer (lightbox): click a duplicate's thumbnail to judge
//! it at pixel level — wheel to zoom around the cursor, drag to pan, arrow keys
//! to step through the group, and mark/close without leaving the app.
//!
//! Full-resolution decoding must never block the UI, so it reuses the
//! `thumbs.rs` worker pattern with a tiny, aggressively-evicted cache (a 50 MP
//! photo is ~200 MB of RGBA — only the current image and its neighbours stay
//! resident). While a decode is in flight the caller draws the 512-px thumbnail
//! scaled up.

use crossbeam_channel::{Receiver, Sender};
use egui::{ColorImage, Context, Rect, TextureHandle, TextureOptions, Vec2};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Longest texture edge uploaded to the GPU; larger images are downscaled by
/// the decoder to stay within driver limits (commonly 8192 px).
const MAX_TEXTURE_EDGE: u32 = 8192;
/// How many full-resolution textures stay resident (current + a few neighbours).
const FULL_CACHE_CAP: usize = 3;

const MIN_SCALE: f32 = 0.02;
const MAX_SCALE: f32 = 32.0;

/// Live state of an open lightbox. `group`/`index` address a member of the
/// current page's groups; `scale`/`pan` are the view transform. In `fit` mode
/// the scale is recomputed from the viewport each frame (so window resizes stay
/// fitted) and the pan is ignored.
pub struct LightboxState {
    pub group: usize,
    pub index: usize,
    scale: f32,
    pan: Vec2,
    fit: bool,
}

impl LightboxState {
    pub fn new(group: usize, index: usize) -> Self {
        Self {
            group,
            index,
            scale: 1.0,
            pan: Vec2::ZERO,
            fit: true,
        }
    }

    /// Reset to fit-to-window (used when switching to another image).
    pub fn reset_view(&mut self) {
        self.scale = 1.0;
        self.pan = Vec2::ZERO;
        self.fit = true;
    }

    /// Effective pixels-per-image-pixel for the current mode and viewport.
    fn effective_scale(&self, view: Rect, img: Vec2) -> f32 {
        if self.fit {
            (view.width() / img.x)
                .min(view.height() / img.y)
                .clamp(MIN_SCALE, MAX_SCALE)
        } else {
            self.scale
        }
    }

    /// Screen rectangle the image occupies inside `view`.
    pub fn image_rect(&self, view: Rect, img: Vec2) -> Rect {
        let scale = self.effective_scale(view, img);
        let size = img * scale;
        let pan = if self.fit { Vec2::ZERO } else { self.pan };
        Rect::from_center_size(view.center() + pan, size)
    }

    /// Enter fit mode.
    pub fn fit(&mut self) {
        self.fit = true;
    }

    /// Enter 1:1 (true pixels) mode, keeping the image centred.
    pub fn one_to_one(&mut self) {
        self.scale = 1.0;
        self.pan = Vec2::ZERO;
        self.fit = false;
    }

    /// Pan by a screen-space delta (from a drag). No-op in fit mode until the
    /// user has zoomed.
    pub fn pan_by(&mut self, delta: Vec2, view: Rect, img: Vec2) {
        self.leave_fit(view, img);
        self.pan += delta;
    }

    /// Zoom by `factor` keeping the image point under `cursor` fixed.
    pub fn zoom_at(&mut self, cursor: egui::Pos2, factor: f32, view: Rect, img: Vec2) {
        self.leave_fit(view, img);
        let new_scale = (self.scale * factor).clamp(MIN_SCALE, MAX_SCALE);
        let ratio = new_scale / self.scale;
        // Keep `cursor` anchored: center' = cursor - (cursor - center) * ratio.
        let center = view.center() + self.pan;
        let new_center = cursor + (center - cursor) * ratio;
        self.pan = new_center - view.center();
        self.scale = new_scale;
    }

    /// Materialize the current fit scale into an explicit scale so subsequent
    /// zoom/pan operate from what the user currently sees.
    fn leave_fit(&mut self, view: Rect, img: Vec2) {
        if self.fit {
            self.scale = self.effective_scale(view, img);
            self.pan = Vec2::ZERO;
            self.fit = false;
        }
    }
}

struct Request {
    hex: String,
    source: PathBuf,
}

enum Decoded {
    Ready(String, ColorImage),
    Failed(String),
}

/// Tiny full-resolution texture cache backed by a background decode pool.
pub struct FullResCache {
    requests: Sender<Request>,
    decoded: Receiver<Decoded>,
    textures: HashMap<String, TextureHandle>,
    order: Vec<String>,
    pending: HashSet<String>,
    failed: HashSet<String>,
    /// The UI context, so a finished decode can wake the UI at rest (the
    /// lightbox does not spin repaints while idle).
    ctx: Arc<Mutex<Option<Context>>>,
}

impl FullResCache {
    pub fn new(workers: usize) -> Self {
        let (req_tx, req_rx) = crossbeam_channel::unbounded::<Request>();
        let (dec_tx, dec_rx) = crossbeam_channel::unbounded::<Decoded>();
        let ctx: Arc<Mutex<Option<Context>>> = Arc::new(Mutex::new(None));
        for _ in 0..workers.max(1) {
            let req_rx = req_rx.clone();
            let dec_tx = dec_tx.clone();
            let ctx = Arc::clone(&ctx);
            std::thread::spawn(move || {
                while let Ok(req) = req_rx.recv() {
                    match dedup_core::thumbnail::load_full_rgba(&req.source, MAX_TEXTURE_EDGE) {
                        Ok((w, h, rgba)) => {
                            let img =
                                ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
                            let _ = dec_tx.send(Decoded::Ready(req.hex, img));
                        }
                        Err(_) => {
                            let _ = dec_tx.send(Decoded::Failed(req.hex));
                        }
                    }
                    if let Some(ctx) = ctx.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                        ctx.request_repaint();
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
            ctx,
        }
    }

    /// Upload freshly decoded images into textures. Returns whether anything
    /// changed (so the caller can repaint).
    pub fn poll(&mut self, ctx: &Context) -> bool {
        *self.ctx.lock().unwrap_or_else(|e| e.into_inner()) = Some(ctx.clone());
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

    /// Texture for `hex`, requesting a full-resolution decode of `source` if it
    /// is not resident yet. `None` while pending or failed (caller shows the
    /// upscaled thumbnail meanwhile).
    pub fn get(&mut self, hex: &str, source: &Path) -> Option<TextureHandle> {
        if self.textures.contains_key(hex) {
            self.touch(hex);
            return self.textures.get(hex).cloned();
        }
        if self.failed.contains(hex) {
            return None;
        }
        if self.pending.insert(hex.to_string()) {
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
        while self.order.len() > FULL_CACHE_CAP {
            let old = self.order.remove(0);
            self.textures.remove(&old);
        }
    }
}
