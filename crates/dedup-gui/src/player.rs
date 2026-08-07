//! Single global audio player for previewing duplicate audio files: confirm two
//! "similar" tracks are the same recording (and which sounds better) without
//! leaving the app.
//!
//! One background thread owns the `rodio` output; the UI drives it with commands
//! over a channel and reads playback position from shared atomics. Normally
//! exactly one file plays at a time. For the lightbox's A/B flicker there is a
//! **paired** mode: two files are loaded into two sinks and played in sync with
//! only one audible, so swapping which you hear is instant and gap-free. If no
//! audio device is available the controls still render (playback is a no-op), so
//! the UI never depends on audio hardware.

use crossbeam_channel::Receiver;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

enum Cmd {
    Play {
        path: PathBuf,
        total_ms: u64,
        start_ms: u64,
        /// Load the file but hold it paused, so stepping through a group's
        /// copies while paused swaps which file is loaded without starting
        /// playback the user had deliberately stopped.
        paused: bool,
    },
    /// Load two files into two sinks, played in sync from `start_ms`; only the
    /// one selected by `active_b` is audible.
    PlayPair {
        path_a: PathBuf,
        path_b: PathBuf,
        total_ms: u64,
        start_ms: u64,
    },
    /// Swap which sink of a pair is audible (instant, gap-free).
    Flip,
    TogglePause,
    Seek(f32),
    Stop,
}

/// Playback state shared between the audio thread and the UI.
///
/// Playback is always at normal speed: the viewer's rate stops play a
/// pitch-preserving `atempo` pre-render at 1× instead of resampling here.
#[derive(Default)]
struct Shared {
    /// Content-hash hex of the A (and, paired, B) channel currently loaded.
    hex_a: Mutex<Option<String>>,
    hex_b: Mutex<Option<String>>,
    /// Two channels are loaded (A/B flicker); else a single file plays.
    paired: AtomicBool,
    /// In paired mode, whether the B channel (rather than A) is the audible one.
    active_b: AtomicBool,
    pos_ms: AtomicU64,
    total_ms: AtomicU64,
    /// A file is loaded and not paused.
    playing: AtomicBool,
    /// A file is loaded (playing or paused).
    loaded: AtomicBool,
}

/// A cheap snapshot of the player state for one UI frame.
pub struct PlayerSnapshot {
    /// Hex of the currently audible file (the A or B channel).
    pub hex: Option<String>,
    pub hex_a: Option<String>,
    pub hex_b: Option<String>,
    pub paired: bool,
    pub pos_ms: u64,
    pub total_ms: u64,
    pub playing: bool,
    pub loaded: bool,
}

pub struct Player {
    tx: crossbeam_channel::Sender<Cmd>,
    shared: Arc<Shared>,
}

impl Player {
    pub fn new() -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        let shared = Arc::new(Shared::default());
        let worker_shared = Arc::clone(&shared);
        std::thread::spawn(move || audio_thread(rx, worker_shared));
        Self { tx, shared }
    }

    fn set_hex(&self, a: Option<&str>, b: Option<&str>) {
        *self.shared.hex_a.lock().unwrap_or_else(|e| e.into_inner()) = a.map(str::to_string);
        *self.shared.hex_b.lock().unwrap_or_else(|e| e.into_inner()) = b.map(str::to_string);
    }

    /// Start playing `path` (identified by its content-hash `hex`) from
    /// `start_ms` into the track. UI state is updated optimistically so the
    /// controls respond instantly even before the decoder starts. A non-zero
    /// `start_ms` is how switching between a group's copies keeps the offset.
    pub fn play(&self, hex: &str, path: &Path, total_ms: u64, start_ms: u64) {
        self.load(hex, path, total_ms, start_ms, false);
    }

    /// Load `path` and hold it paused at `start_ms`.
    ///
    /// Stepping to another copy while playback is paused must swap *which* file
    /// is loaded — otherwise the lightbox shows one copy while the player still
    /// holds the previous one, and pressing play resumes the wrong file.
    pub fn load_paused(&self, hex: &str, path: &Path, total_ms: u64, start_ms: u64) {
        self.load(hex, path, total_ms, start_ms, true);
    }

    fn load(&self, hex: &str, path: &Path, total_ms: u64, start_ms: u64, paused: bool) {
        let start = start_ms.min(total_ms);
        self.set_hex(Some(hex), None);
        self.shared.paired.store(false, Ordering::Relaxed);
        self.shared.active_b.store(false, Ordering::Relaxed);
        self.shared.total_ms.store(total_ms, Ordering::Relaxed);
        self.shared.pos_ms.store(start, Ordering::Relaxed);
        self.shared.playing.store(!paused, Ordering::Relaxed);
        self.shared.loaded.store(true, Ordering::Relaxed);
        let _ = self.tx.send(Cmd::Play {
            path: path.to_path_buf(),
            total_ms,
            start_ms: start,
            paused,
        });
    }

    /// Load two copies into two sinks, played in sync from `start_ms`; only the
    /// channel chosen by `audible_b` is audible. Powers gap-free A/B flicker.
    #[allow(clippy::too_many_arguments)]
    pub fn play_pair(
        &self,
        hex_a: &str,
        path_a: &Path,
        hex_b: &str,
        path_b: &Path,
        total_ms: u64,
        start_ms: u64,
        audible_b: bool,
    ) {
        let start = start_ms.min(total_ms);
        self.set_hex(Some(hex_a), Some(hex_b));
        self.shared.paired.store(true, Ordering::Relaxed);
        self.shared.active_b.store(audible_b, Ordering::Relaxed);
        self.shared.total_ms.store(total_ms, Ordering::Relaxed);
        self.shared.pos_ms.store(start, Ordering::Relaxed);
        self.shared.playing.store(true, Ordering::Relaxed);
        self.shared.loaded.store(true, Ordering::Relaxed);
        let _ = self.tx.send(Cmd::PlayPair {
            path_a: path_a.to_path_buf(),
            path_b: path_b.to_path_buf(),
            total_ms,
            start_ms: start,
        });
    }

    /// Swap which channel of a loaded pair is audible — instant and gap-free.
    pub fn flip(&self) {
        let now = self.shared.active_b.load(Ordering::Relaxed);
        self.shared.active_b.store(!now, Ordering::Relaxed);
        let _ = self.tx.send(Cmd::Flip);
    }

    pub fn toggle_pause(&self) {
        let now = self.shared.playing.load(Ordering::Relaxed);
        self.shared.playing.store(!now, Ordering::Relaxed);
        let _ = self.tx.send(Cmd::TogglePause);
    }

    pub fn stop(&self) {
        self.set_hex(None, None);
        self.shared.paired.store(false, Ordering::Relaxed);
        self.shared.loaded.store(false, Ordering::Relaxed);
        self.shared.playing.store(false, Ordering::Relaxed);
        self.shared.pos_ms.store(0, Ordering::Relaxed);
        let _ = self.tx.send(Cmd::Stop);
    }

    pub fn seek_fraction(&self, fraction: f32) {
        let f = fraction.clamp(0.0, 1.0);
        let total = self.shared.total_ms.load(Ordering::Relaxed);
        self.shared
            .pos_ms
            .store((f as f64 * total as f64) as u64, Ordering::Relaxed);
        let _ = self.tx.send(Cmd::Seek(f));
    }

    pub fn snapshot(&self) -> PlayerSnapshot {
        let hex_a = self
            .shared
            .hex_a
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let hex_b = self
            .shared
            .hex_b
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let active_b = self.shared.active_b.load(Ordering::Relaxed);
        let hex = if active_b {
            hex_b.clone()
        } else {
            hex_a.clone()
        };
        PlayerSnapshot {
            hex,
            hex_a,
            hex_b,
            paired: self.shared.paired.load(Ordering::Relaxed),
            pos_ms: self.shared.pos_ms.load(Ordering::Relaxed),
            total_ms: self.shared.total_ms.load(Ordering::Relaxed),
            playing: self.shared.playing.load(Ordering::Relaxed),
            loaded: self.shared.loaded.load(Ordering::Relaxed),
        }
    }

    /// Whether any file is currently loaded (used to keep repainting the seek
    /// bar and to know if a tab switch should stop playback).
    pub fn is_active(&self) -> bool {
        self.shared.loaded.load(Ordering::Relaxed)
    }
}

impl Default for Player {
    fn default() -> Self {
        Self::new()
    }
}

/// Append a decoded file to `sink`, seeking to `start_ms`. Returns whether a
/// source was actually loaded.
fn load(sink: &rodio::Sink, path: &Path, start_ms: u64) -> bool {
    sink.clear();
    if let Ok(file) = std::fs::File::open(path)
        && let Ok(decoder) = rodio::Decoder::new(BufReader::new(file))
    {
        sink.append(decoder);
        if start_ms > 0 {
            let _ = sink.try_seek(Duration::from_millis(start_ms));
        }
        true
    } else {
        false
    }
}

fn audio_thread(rx: Receiver<Cmd>, shared: Arc<Shared>) {
    // Open the default output once (kept alive for the thread's lifetime).
    // Absent a device, the sinks are None and every command is a no-op — the
    // optimistic UI state set by the caller stands.
    let stream = rodio::OutputStream::try_default().ok();
    let sink_a = stream
        .as_ref()
        .and_then(|(_, handle)| rodio::Sink::try_new(handle).ok());
    let sink_b = stream
        .as_ref()
        .and_then(|(_, handle)| rodio::Sink::try_new(handle).ok());
    // Whether a source was actually appended for the current A channel. Guards
    // end-of-track detection so a file that failed to decode (or a missing one)
    // is not immediately reported as "finished".
    let mut has_a = false;

    // Apply pair volumes from the shared `active_b` flag (only one audible).
    let apply_volumes = |shared: &Shared| {
        if let (Some(a), Some(b)) = (&sink_a, &sink_b) {
            let b_on = shared.active_b.load(Ordering::Relaxed);
            a.set_volume(if b_on { 0.0 } else { 1.0 });
            b.set_volume(if b_on { 1.0 } else { 0.0 });
        }
    };

    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Cmd::Play {
                path,
                total_ms,
                start_ms,
                paused,
            }) => {
                if let Some(b) = &sink_b {
                    b.clear();
                }
                has_a = false;
                if let Some(a) = &sink_a {
                    a.set_volume(1.0);
                    if load(a, &path, start_ms) {
                        // Always `play()` first: a rodio sink starts paused only
                        // if told to, and pausing after play leaves it primed at
                        // the right position for an instant resume.
                        a.play();
                        if paused {
                            a.pause();
                        }
                        has_a = true;
                    }
                }
                shared.total_ms.store(total_ms, Ordering::Relaxed);
                shared.pos_ms.store(start_ms, Ordering::Relaxed);
            }
            Ok(Cmd::PlayPair {
                path_a,
                path_b,
                total_ms,
                start_ms,
            }) => {
                has_a = false;
                if let (Some(a), Some(b)) = (&sink_a, &sink_b) {
                    has_a = load(a, &path_a, start_ms);
                    let _ = load(b, &path_b, start_ms);
                    apply_volumes(&shared);
                    a.play();
                    b.play();
                }
                shared.total_ms.store(total_ms, Ordering::Relaxed);
                shared.pos_ms.store(start_ms, Ordering::Relaxed);
            }
            Ok(Cmd::Flip) => apply_volumes(&shared),
            Ok(Cmd::TogglePause) => {
                for s in [&sink_a, &sink_b].into_iter().flatten() {
                    if s.is_paused() {
                        s.play();
                    } else {
                        s.pause();
                    }
                }
                if let Some(a) = &sink_a {
                    shared.playing.store(!a.is_paused(), Ordering::Relaxed);
                }
            }
            Ok(Cmd::Seek(f)) => {
                let total = shared.total_ms.load(Ordering::Relaxed);
                let pos = Duration::from_millis((f as f64 * total as f64) as u64);
                for s in [&sink_a, &sink_b].into_iter().flatten() {
                    let _ = s.try_seek(pos);
                }
            }
            Ok(Cmd::Stop) => {
                has_a = false;
                for s in [&sink_a, &sink_b].into_iter().flatten() {
                    s.clear();
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }

        // Track position and natural end-of-track off the A sink (the pair plays
        // in sync). Only meaningful with a real device.
        if let Some(a) = &sink_a
            && shared.loaded.load(Ordering::Relaxed)
        {
            shared
                .pos_ms
                .store(a.get_pos().as_millis() as u64, Ordering::Relaxed);
            if has_a && a.empty() {
                has_a = false;
                shared.loaded.store(false, Ordering::Relaxed);
                shared.playing.store(false, Ordering::Relaxed);
                shared.paired.store(false, Ordering::Relaxed);
                *shared.hex_a.lock().unwrap_or_else(|e| e.into_inner()) = None;
                *shared.hex_b.lock().unwrap_or_else(|e| e.into_inner()) = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The player always runs at normal speed — the viewer's rate stops swap
    /// *which file* plays (a pitch-preserving `atempo` render), never the
    /// sink's rate. Loading another copy must not disturb the transport state.
    #[test]
    fn loading_paused_holds_the_file_without_playing() {
        let player = Player::new();
        player.load_paused("abc", std::path::Path::new("/nonexistent.mp3"), 1000, 0);
        let snap = player.snapshot();
        assert!(snap.loaded, "the copy is loaded");
        assert!(!snap.playing, "and deliberately not playing");
        assert_eq!(snap.hex.as_deref(), Some("abc"));
    }
}
