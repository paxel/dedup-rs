//! Single global audio player for previewing duplicate audio files: confirm two
//! "similar" tracks are the same recording (and which sounds better) without
//! leaving the app.
//!
//! One background thread owns the `rodio` output; the UI drives it with
//! commands over a channel and reads playback position from shared atomics.
//! Exactly one file plays at a time — starting another replaces it. If no audio
//! device is available the controls still render (playback is simply a no-op),
//! so the UI never depends on audio hardware.

use crossbeam_channel::Receiver;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

enum Cmd {
    Play { path: PathBuf, total_ms: u64 },
    TogglePause,
    Seek(f32),
    Stop,
}

/// Playback state shared between the audio thread and the UI.
#[derive(Default)]
struct Shared {
    /// Content-hash hex of the file currently loaded, if any.
    hex: Mutex<Option<String>>,
    pos_ms: AtomicU64,
    total_ms: AtomicU64,
    /// A file is loaded and not paused.
    playing: AtomicBool,
    /// A file is loaded (playing or paused).
    loaded: AtomicBool,
}

/// A cheap snapshot of the player state for one UI frame.
pub struct PlayerSnapshot {
    pub hex: Option<String>,
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

    /// Start playing `path` (identified by its content-hash `hex`). The UI state
    /// is updated optimistically so the controls respond instantly even before
    /// the decoder starts (and so they still reflect intent with no device).
    pub fn play(&self, hex: &str, path: &Path, total_ms: u64) {
        *self.shared.hex.lock().unwrap_or_else(|e| e.into_inner()) = Some(hex.to_string());
        self.shared.total_ms.store(total_ms, Ordering::Relaxed);
        self.shared.pos_ms.store(0, Ordering::Relaxed);
        self.shared.playing.store(true, Ordering::Relaxed);
        self.shared.loaded.store(true, Ordering::Relaxed);
        let _ = self.tx.send(Cmd::Play {
            path: path.to_path_buf(),
            total_ms,
        });
    }

    pub fn toggle_pause(&self) {
        // Optimistic flip so the button label updates immediately.
        let now = self.shared.playing.load(Ordering::Relaxed);
        self.shared.playing.store(!now, Ordering::Relaxed);
        let _ = self.tx.send(Cmd::TogglePause);
    }

    pub fn stop(&self) {
        *self.shared.hex.lock().unwrap_or_else(|e| e.into_inner()) = None;
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
        PlayerSnapshot {
            hex: self
                .shared
                .hex
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
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

fn audio_thread(rx: Receiver<Cmd>, shared: Arc<Shared>) {
    // Open the default output once (kept alive for the thread's lifetime).
    // Absent a device, `player` is None and every command is a no-op — the
    // optimistic UI state set by the caller stands.
    let stream = rodio::OutputStream::try_default().ok();
    let player = stream
        .as_ref()
        .and_then(|(_, handle)| rodio::Sink::try_new(handle).ok());
    // Whether a source was actually appended for the current file. Guards the
    // end-of-track detection so a file that failed to decode (or a missing one)
    // is not immediately reported as "finished".
    let mut has_source = false;

    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Cmd::Play { path, total_ms }) => {
                has_source = false;
                if let Some(p) = &player {
                    p.clear();
                    if let Ok(file) = std::fs::File::open(&path)
                        && let Ok(decoder) = rodio::Decoder::new(BufReader::new(file))
                    {
                        p.append(decoder);
                        p.play();
                        has_source = true;
                    }
                }
                shared.total_ms.store(total_ms, Ordering::Relaxed);
                shared.pos_ms.store(0, Ordering::Relaxed);
            }
            Ok(Cmd::TogglePause) => {
                if let Some(p) = &player {
                    if p.is_paused() {
                        p.play();
                        shared.playing.store(true, Ordering::Relaxed);
                    } else {
                        p.pause();
                        shared.playing.store(false, Ordering::Relaxed);
                    }
                }
            }
            Ok(Cmd::Seek(f)) => {
                if let Some(p) = &player {
                    let total = shared.total_ms.load(Ordering::Relaxed);
                    let pos = Duration::from_millis((f as f64 * total as f64) as u64);
                    let _ = p.try_seek(pos);
                }
            }
            Ok(Cmd::Stop) => {
                has_source = false;
                if let Some(p) = &player {
                    p.clear();
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }

        // Track position and natural end-of-track (only meaningful with a real
        // device; without one the optimistic state is left as-is).
        if let Some(p) = &player
            && shared.loaded.load(Ordering::Relaxed)
        {
            shared
                .pos_ms
                .store(p.get_pos().as_millis() as u64, Ordering::Relaxed);
            // Only a track that actually started can "finish".
            if has_source && p.empty() {
                has_source = false;
                shared.loaded.store(false, Ordering::Relaxed);
                shared.playing.store(false, Ordering::Relaxed);
                *shared.hex.lock().unwrap_or_else(|e| e.into_inner()) = None;
            }
        }
    }
}
