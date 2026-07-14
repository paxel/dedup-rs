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

/// A/B compare overlaid on the lightbox: `other` is the B member's index in the
/// same group (A is the lightbox's current `index`). Zoom/pan are shared by both
/// panes and normalized to each image's fit, so differing resolutions line up.
pub struct CompareState {
    pub other: usize,
    pub flicker: bool,
    /// In flicker mode, whether B (rather than A) is currently shown.
    pub show_b: bool,
    zoom: f32,
    pan: Vec2,
}

impl CompareState {
    pub fn new(other: usize) -> Self {
        Self {
            other,
            flicker: false,
            show_b: false,
            zoom: 1.0,
            pan: Vec2::ZERO,
        }
    }

    pub fn zoom_by(&mut self, factor: f32) {
        self.zoom = (self.zoom * factor).clamp(1.0, MAX_SCALE);
    }

    pub fn pan_by(&mut self, delta: Vec2) {
        self.pan += delta;
    }

    /// Screen rectangle for `img` fitted into `pane`, then scaled by the shared
    /// zoom and shifted by the shared pan (so both panes track together).
    pub fn pane_rect(&self, pane: Rect, img: Vec2) -> Rect {
        let fit = (pane.width() / img.x).min(pane.height() / img.y);
        let size = img * (fit * self.zoom);
        Rect::from_center_size(pane.center() + self.pan, size)
    }
}

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
    /// Active A/B compare, if the user pressed `C`.
    pub compare: Option<CompareState>,
    /// Audio lightbox only: the group index whose playback cursor is shown (the
    /// copy the user last started). Needed because exact-duplicate copies share
    /// a content hash, so the hash alone can't say which row is playing.
    pub audio_active: Option<usize>,
    /// Audio lightbox only: show spectrograms instead of amplitude waveforms.
    pub spectrogram: bool,
}

impl LightboxState {
    pub fn new(group: usize, index: usize) -> Self {
        Self {
            group,
            index,
            scale: 1.0,
            pan: Vec2::ZERO,
            fit: true,
            compare: None,
            audio_active: None,
            spectrogram: false,
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
                    // Only a successful decode has something new to show, so
                    // only that wakes the UI; waking on failure would spin
                    // repaints for missing files (and never settle).
                    match dedup_core::thumbnail::load_full_rgba(&req.source, MAX_TEXTURE_EDGE) {
                        Ok((w, h, rgba)) => {
                            let img =
                                ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
                            let _ = dec_tx.send(Decoded::Ready(req.hex, img));
                            if let Some(ctx) =
                                ctx.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
                            {
                                ctx.request_repaint();
                            }
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
