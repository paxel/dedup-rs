//! Background audio-waveform extraction for the audio lightbox: decode a file
//! to a small amplitude envelope (one normalized peak per bucket) so a group's
//! "similar" copies can be compared visually and their differences spotted.
//!
//! Mirrors the `lightbox::FullResCache` worker/cache pattern — decoding never
//! blocks the UI, a finished envelope wakes the UI at rest, and a tiny LRU keeps
//! only the few copies on screen resident.

use crossbeam_channel::{Receiver, Sender};
use egui::Context;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Peak buckets per waveform. Fixed so any two copies share an x-axis and line
/// up column-for-column when stacked in the lightbox.
pub const WAVE_BUCKETS: usize = 480;
/// How many decoded vizualizations stay resident (a group's copies + slack).
const CACHE_CAP: usize = 16;

/// STFT window size (power of two for the radix-2 FFT) and hop between frames.
const FFT_N: usize = 512;
const HOP: usize = 256;
/// Spectrogram frequency bins (`FFT_N / 2`) and time columns.
const SPEC_BINS: usize = FFT_N / 2;
const SPEC_COLS: usize = 480;
/// Displayed spectrogram dynamic range, in dB below the loudest cell.
const SPEC_DB_RANGE: f32 = 70.0;

/// Decoded visual summary of an audio file: a time-domain amplitude envelope
/// (loud/compressed music reads as a flat block, hence the spectrogram too) and
/// a frequency-vs-time spectrogram whose values are log-magnitude, normalized
/// to 0..=1. `spec` is row-major `[bin * SPEC_COLS + col]`, bin 0 = lowest freq.
pub struct AudioViz {
    pub envelope: Vec<f32>,
    pub spec: Vec<f32>,
    pub spec_w: usize,
    pub spec_h: usize,
}

struct Request {
    hex: String,
    source: PathBuf,
}

enum Decoded {
    Ready(String, Arc<AudioViz>),
    Failed(String),
}

/// Tiny audio-visualization cache backed by a background decode pool.
pub struct WaveCache {
    requests: Sender<Request>,
    decoded: Receiver<Decoded>,
    waves: HashMap<String, Arc<AudioViz>>,
    order: Vec<String>,
    pending: HashSet<String>,
    failed: HashSet<String>,
    /// The UI context, so a finished decode can wake the UI at rest (the
    /// lightbox does not spin repaints while idle).
    ctx: Arc<Mutex<Option<Context>>>,
}

impl WaveCache {
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
                    // Only a successful decode has something new to show, so only
                    // that wakes the UI; waking on failure would spin repaints
                    // for undecodable files (and never settle).
                    match extract_viz(&req.source) {
                        Some(viz) => {
                            let _ = dec_tx.send(Decoded::Ready(req.hex, Arc::new(viz)));
                            if let Some(ctx) =
                                ctx.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
                            {
                                ctx.request_repaint();
                            }
                        }
                        None => {
                            let _ = dec_tx.send(Decoded::Failed(req.hex));
                        }
                    }
                }
            });
        }
        Self {
            requests: req_tx,
            decoded: dec_rx,
            waves: HashMap::new(),
            order: Vec::new(),
            pending: HashSet::new(),
            failed: HashSet::new(),
            ctx,
        }
    }

    /// Drain freshly decoded envelopes into the cache. Returns whether anything
    /// changed (so the caller can repaint).
    pub fn poll(&mut self, ctx: &Context) -> bool {
        *self.ctx.lock().unwrap_or_else(|e| e.into_inner()) = Some(ctx.clone());
        let mut changed = false;
        while let Ok(decoded) = self.decoded.try_recv() {
            changed = true;
            match decoded {
                Decoded::Ready(hex, env) => {
                    self.pending.remove(&hex);
                    self.touch(&hex);
                    self.waves.insert(hex, env);
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

    /// Envelope for `hex`, requesting extraction of `source` if it is not
    /// resident yet. `None` while pending or failed (caller draws a placeholder
    /// meanwhile).
    pub fn get(&mut self, hex: &str, source: &Path) -> Option<Arc<AudioViz>> {
        if self.waves.contains_key(hex) {
            self.touch(hex);
            return self.waves.get(hex).map(Arc::clone);
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
        while self.order.len() > CACHE_CAP {
            let old = self.order.remove(0);
            self.waves.remove(&old);
        }
    }
}

/// Decode `path` into an [`AudioViz`] (amplitude envelope + spectrogram) in a
/// single streaming pass over mono-downmixed samples.
///
/// The decoder's reported duration is unreliable for some formats, so lengths
/// are discovered by streaming, not estimated: both the envelope's fine peaks
/// and the spectrogram's time columns fold in half (merging neighbours by max)
/// whenever they fill, so memory stays bounded for any track length. Returns
/// `None` if the file cannot be opened/decoded or is silent/empty.
fn extract_viz(path: &Path) -> Option<AudioViz> {
    use rodio::Source;
    let file = std::fs::File::open(path).ok()?;
    let decoder = rodio::Decoder::new(std::io::BufReader::new(file)).ok()?;
    let channels = decoder.channels().max(1) as usize;

    // Envelope: fine peaks that fold (halve resolution) when they hit the cap.
    let env_cap = WAVE_BUCKETS * 8;
    let mut fine: Vec<f32> = Vec::with_capacity(env_cap * 2);
    let mut step: u64 = 1;
    let mut in_step: u64 = 0;
    let mut peak = 0f32;

    // Spectrogram: a sliding STFT window, hop-spaced, columns folded like above.
    let hann: Vec<f32> = (0..FFT_N)
        .map(|k| {
            let x = std::f32::consts::PI * k as f32 / (FFT_N as f32 - 1.0);
            x.sin().powi(2)
        })
        .collect();
    let mut window = vec![0f32; FFT_N];
    let mut wpos = 0usize;
    let mut since_hop = 0usize;
    let mut cols: Vec<[f32; SPEC_BINS]> = Vec::with_capacity(SPEC_COLS * 2);
    let (mut re, mut im) = (vec![0f32; FFT_N], vec![0f32; FFT_N]);
    // Each output column is the peak over `col_step` STFT frames; `col_step`
    // doubles on every fold so columns keep a *uniform* time span (without this
    // the whole song collapses into the first few columns).
    let mut col_acc = [0f32; SPEC_BINS];
    let mut col_frames = 0usize;
    let mut col_step = 1usize;

    // Mono downmix state.
    let mut acc = 0f32;
    let mut ch_i = 0usize;
    let mut any = false;

    for sample in decoder {
        acc += f32::from(sample) * (1.0 / 32768.0);
        ch_i += 1;
        if ch_i < channels {
            continue;
        }
        let mono = acc / channels as f32;
        acc = 0.0;
        ch_i = 0;
        any = true;

        // --- envelope ---
        let amp = mono.abs();
        if amp > peak {
            peak = amp;
        }
        in_step += 1;
        if in_step == step {
            fine.push(peak);
            peak = 0.0;
            in_step = 0;
            if fine.len() >= env_cap * 2 {
                for j in 0..fine.len() / 2 {
                    fine[j] = fine[2 * j].max(fine[2 * j + 1]);
                }
                fine.truncate(fine.len() / 2);
                step *= 2;
            }
        }

        // --- spectrogram ---
        window[wpos] = mono;
        wpos = (wpos + 1) % FFT_N;
        since_hop += 1;
        if since_hop == HOP {
            since_hop = 0;
            for k in 0..FFT_N {
                re[k] = window[(wpos + k) % FFT_N] * hann[k];
                im[k] = 0.0;
            }
            fft(&mut re, &mut im);
            // Peak-accumulate this frame's linear magnitude into the column.
            for (b, slot) in col_acc.iter_mut().enumerate() {
                let mag = (re[b] * re[b] + im[b] * im[b]).sqrt();
                if mag > *slot {
                    *slot = mag;
                }
            }
            col_frames += 1;
            if col_frames == col_step {
                cols.push(col_acc);
                col_acc = [0f32; SPEC_BINS];
                col_frames = 0;
                if cols.len() >= SPEC_COLS * 2 {
                    for j in 0..cols.len() / 2 {
                        let (lo, hi) = (cols[2 * j], cols[2 * j + 1]);
                        cols[j] = std::array::from_fn(|k| lo[k].max(hi[k]));
                    }
                    cols.truncate(cols.len() / 2);
                    col_step *= 2;
                }
            }
        }
    }
    if col_frames > 0 {
        cols.push(col_acc);
    }
    if in_step > 0 {
        fine.push(peak);
    }
    if !any || fine.is_empty() {
        return None;
    }

    // Envelope → final buckets (peak within each), normalized to full height.
    let mut envelope = vec![0f32; WAVE_BUCKETS];
    let m = fine.len();
    for (i, &v) in fine.iter().enumerate() {
        let b = (i * WAVE_BUCKETS) / m;
        if v > envelope[b] {
            envelope[b] = v;
        }
    }
    let emax = envelope.iter().copied().fold(0f32, f32::max);
    if emax > 0.0 {
        for p in &mut envelope {
            *p /= emax;
        }
    }

    // Spectrogram → fixed grid (peak linear magnitude within each output column).
    let mut spec = vec![0f32; SPEC_BINS * SPEC_COLS];
    let nc = cols.len().max(1);
    for (i, col) in cols.iter().enumerate() {
        let x = (i * SPEC_COLS) / nc;
        for (b, &v) in col.iter().enumerate() {
            let idx = b * SPEC_COLS + x;
            if v > spec[idx] {
                spec[idx] = v;
            }
        }
    }
    // Drop the DC bin (row 0): it carries offset/leakage energy, not musical
    // content, and would otherwise dominate the whole image as one bright line.
    for v in spec.iter_mut().take(SPEC_COLS) {
        *v = 0.0;
    }
    // Map linear magnitude to dB with a fixed dynamic-range floor. A plain
    // max-normalize let a single loud cell wash everything else to black; a dB
    // floor spreads the visible energy across the brightness range instead.
    let peak = spec.iter().copied().fold(0f32, f32::max);
    if peak > 0.0 {
        let peak_db = 20.0 * peak.log10();
        let floor_db = peak_db - SPEC_DB_RANGE;
        let inv = 1.0 / (peak_db - floor_db);
        for v in &mut spec {
            *v = if *v > 0.0 {
                ((20.0 * v.log10() - floor_db) * inv).clamp(0.0, 1.0)
            } else {
                0.0
            };
        }
    }

    Some(AudioViz {
        envelope,
        spec,
        spec_w: SPEC_COLS,
        spec_h: SPEC_BINS,
    })
}

/// In-place iterative radix-2 Cooley–Tukey FFT (length must be a power of two).
/// Small and dependency-free; `FFT_N` is 512, so this is plenty fast off-thread.
fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    // Bit-reversal permutation.
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j ^= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    // Butterflies.
    let mut len = 2;
    while len <= n {
        let ang = -2.0 * std::f32::consts::PI / len as f32;
        let (wr, wi) = (ang.cos(), ang.sin());
        let mut i = 0;
        while i < n {
            let (mut cr, mut ci) = (1.0f32, 0.0f32);
            for k in 0..len / 2 {
                let a = i + k;
                let b = i + k + len / 2;
                let tr = cr * re[b] - ci * im[b];
                let ti = cr * im[b] + ci * re[b];
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
                let ncr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = ncr;
            }
            i += len;
        }
        len <<= 1;
    }
}

/// Magma-ish heat ramp (black → purple → orange → white) for spectrogram cells:
/// `v` in 0..=1 maps to brightness, so louder frequencies read brighter.
fn spec_color(v: f32) -> egui::Color32 {
    const STOPS: [(f32, f32, f32, f32); 5] = [
        (0.00, 0.0, 0.0, 4.0),
        (0.25, 60.0, 15.0, 110.0),
        (0.50, 165.0, 45.0, 110.0),
        (0.75, 235.0, 105.0, 60.0),
        (1.00, 252.0, 255.0, 200.0),
    ];
    let v = v.clamp(0.0, 1.0);
    let mut i = 0;
    while i + 1 < STOPS.len() && v > STOPS[i + 1].0 {
        i += 1;
    }
    let (v0, r0, g0, b0) = STOPS[i];
    let (v1, r1, g1, b1) = STOPS[(i + 1).min(STOPS.len() - 1)];
    let t = if v1 > v0 { (v - v0) / (v1 - v0) } else { 0.0 };
    let lerp = |a: f32, b: f32| (a + (b - a) * t) as u8;
    egui::Color32::from_rgb(lerp(r0, r1), lerp(g0, g1), lerp(b0, b1))
}

/// Build a spectrogram image (time on x, frequency on y with bass at the
/// bottom) from a decoded [`AudioViz`]. Shared by the Duplicates audio lightbox
/// and the Browse audio preview.
pub fn spec_image(viz: &AudioViz) -> egui::ColorImage {
    let (w, h) = (viz.spec_w, viz.spec_h);
    let mut rgba = vec![0u8; w * h * 4];
    for y in 0..h {
        let bin = h - 1 - y; // row 0 (top) = highest freq
        for x in 0..w {
            let c = spec_color(viz.spec[bin * w + x]);
            let i = (y * w + x) * 4;
            rgba[i] = c.r();
            rgba[i + 1] = c.g();
            rgba[i + 2] = c.b();
            rgba[i + 3] = 255;
        }
    }
    egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba)
}

/// Write a minimal PCM16 mono WAV (used by tests and the ignored render, since
/// no audio-writer crate is a dependency and rodio's symphonia-all decodes WAV).
#[cfg(test)]
pub(crate) fn write_wav(path: &Path, sample_rate: u32, samples: &[i16]) {
    use std::io::Write;
    let data_bytes = (samples.len() * 2) as u32;
    let byte_rate = sample_rate * 2; // mono, 16-bit
    let mut buf: Vec<u8> = Vec::with_capacity(44 + samples.len() * 2);
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    buf.extend_from_slice(b"WAVE");
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&16u32.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes()); // PCM
    buf.extend_from_slice(&1u16.to_le_bytes()); // mono
    buf.extend_from_slice(&sample_rate.to_le_bytes());
    buf.extend_from_slice(&byte_rate.to_le_bytes());
    buf.extend_from_slice(&2u16.to_le_bytes()); // block align
    buf.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&data_bytes.to_le_bytes());
    for &s in samples {
        buf.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::File::create(path)
        .unwrap()
        .write_all(&buf)
        .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_envelope_normalizes_and_tracks_amplitude() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("ramp.wav");
        let sr = 8000u32;
        let secs = 2usize;
        let n = sr as usize * secs;
        // A 220 Hz sine whose amplitude ramps from ~0 to full scale over time.
        let samples: Vec<i16> = (0..n)
            .map(|i| {
                let t = i as f32 / sr as f32;
                let amp = (i as f32 / n as f32) * 30_000.0;
                (amp * (2.0 * std::f32::consts::PI * 220.0 * t).sin()) as i16
            })
            .collect();
        write_wav(&path, sr, &samples);

        let _ = secs;
        let viz = extract_viz(&path).expect("wav decodes to a visualization");
        let env = &viz.envelope;
        assert_eq!(env.len(), WAVE_BUCKETS);
        let max = env.iter().copied().fold(0f32, f32::max);
        assert!(
            (max - 1.0).abs() < 1e-3,
            "envelope normalized to 1.0, got {max}"
        );
        // Amplitude ramps up, so late buckets are louder than early ones.
        let early = env[WAVE_BUCKETS / 10];
        let late = env[WAVE_BUCKETS - WAVE_BUCKETS / 10];
        assert!(
            late > early + 0.3,
            "amplitude ramp: late {late} should exceed early {early}"
        );

        // Spectrogram: right shape, normalized, and the 220 Hz tone's energy
        // sits in low frequency bins (well below the mid frequencies).
        assert_eq!(viz.spec.len(), viz.spec_w * viz.spec_h);
        let smax = viz.spec.iter().copied().fold(0f32, f32::max);
        assert!(
            (smax - 1.0).abs() < 1e-3,
            "spectrogram normalized to 1.0, got {smax}"
        );
        let bin_energy = |b: usize| -> f32 {
            (0..viz.spec_w)
                .map(|x| viz.spec[b * viz.spec_w + x])
                .fold(0f32, f32::max)
        };
        let low = (1..30).map(bin_energy).fold(0f32, f32::max);
        let high = (viz.spec_h / 2..viz.spec_h)
            .map(bin_energy)
            .fold(0f32, f32::max);
        assert!(
            low > high,
            "tone energy sits in low bins: low {low} vs high {high}"
        );
    }

    #[test]
    fn extract_viz_missing_file_is_none() {
        assert!(extract_viz(Path::new("/nonexistent-dedup.wav")).is_none());
    }

    /// A long steady tone must light its frequency bin across the *whole* width.
    /// Regression for the column-fold bug that crammed the entire song into the
    /// first few columns (bright left edge, everything else black).
    #[test]
    fn spectrogram_time_axis_spans_the_whole_input() {
        use std::f32::consts::PI;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("long.wav");
        let sr = 8000u32;
        // Long enough to trigger several column folds (≫ SPEC_COLS * 2 frames).
        let n = SPEC_COLS * 2 * HOP * 4;
        let samples: Vec<i16> = (0..n)
            .map(|k| {
                let t = k as f32 / sr as f32;
                (20_000.0 * (2.0 * PI * 440.0 * t).sin()) as i16
            })
            .collect();
        write_wav(&path, sr, &samples);

        let viz = extract_viz(&path).expect("wav decodes");
        let bin = (440.0 / (sr as f32 / FFT_N as f32)).round() as usize;
        let w = viz.spec_w;
        let lit = |x: usize| viz.spec[bin * w + x] > 0.25;
        assert!(lit(0), "tone lit at the start");
        assert!(lit(w / 2), "tone lit in the middle");
        assert!(
            lit(w - 1),
            "tone lit at the end — the time axis spans the whole file"
        );
    }
}
