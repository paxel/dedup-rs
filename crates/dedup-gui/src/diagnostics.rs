//! In-memory diagnostics: a thread-safe registry of **Critical** and **Warning**
//! events plus a one-time **system fingerprint**, surfaced by the Status button.
//!
//! This is deliberately *not* a log — the on-disk session log
//! ([`dedup_core::logging`]) already keeps a diary. Every entry here is
//! actionable: a capability is unavailable (no audio device, no ffmpeg) or an
//! operation failed (a file went missing, a decode broke). Entries are deduped
//! by a key so a flapping mount bumps a count rather than flooding the list, and
//! held only for the session. The registry is cloneable (an `Arc` inside), so
//! background workers and the UI share one instance; snapshots are cloned out so
//! the UI never renders while holding the lock.

use std::sync::{Arc, Mutex};

/// How loud an event is. There is no `Info` — this surface is only for things
/// that need attention.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// A capability is unavailable or degraded (no audio device, no ffmpeg).
    Warning,
    /// An operation actually failed or data is at risk (a file went missing).
    Critical,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Severity::Warning => "WARNING",
            Severity::Critical => "CRITICAL",
        }
    }
}

/// One diagnostic entry as the UI sees it (a cloned snapshot).
#[derive(Clone, Debug)]
pub struct Event {
    pub severity: Severity,
    pub title: String,
    pub detail: String,
    /// How many times this same event has fired (a deduped repeat bumps it).
    pub count: u32,
}

struct Stored {
    severity: Severity,
    key: String,
    title: String,
    detail: String,
    count: u32,
    unread: bool,
}

#[derive(Default)]
struct Inner {
    events: Vec<Stored>,
    fingerprint: String,
}

/// The shared, cloneable diagnostics registry.
#[derive(Clone, Default)]
pub struct Diagnostics(Arc<Mutex<Inner>>);

impl Diagnostics {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Record the system fingerprint (gathered once at startup).
    pub fn set_fingerprint(&self, fingerprint: impl Into<String>) {
        self.lock().fingerprint = fingerprint.into();
    }

    /// File an event. `key` deduplicates: a repeat with the same key bumps the
    /// existing entry's count and re-flags it unread instead of appending, so a
    /// dropped mount or a retrying decode never floods the list. The newest
    /// title/detail win (state may have moved on).
    pub fn push(
        &self,
        severity: Severity,
        key: impl Into<String>,
        title: impl Into<String>,
        detail: impl Into<String>,
    ) {
        let key = key.into();
        let mut inner = self.lock();
        if let Some(e) = inner.events.iter_mut().find(|e| e.key == key) {
            e.severity = severity;
            e.title = title.into();
            e.detail = detail.into();
            e.count += 1;
            e.unread = true;
        } else {
            inner.events.push(Stored {
                severity,
                key,
                title: title.into(),
                detail: detail.into(),
                count: 1,
                unread: true,
            });
        }
    }

    /// Clear an event by key (e.g. a repo root came back). No-op if absent.
    pub fn clear(&self, key: &str) {
        self.lock().events.retain(|e| e.key != key);
    }

    /// A snapshot of all events, Critical first, for the UI to render.
    pub fn events(&self) -> Vec<Event> {
        let inner = self.lock();
        let mut out: Vec<Event> = inner
            .events
            .iter()
            .map(|e| Event {
                severity: e.severity,
                title: e.title.clone(),
                detail: e.detail.clone(),
                count: e.count,
            })
            .collect();
        // Critical before Warning; stable within a severity (insertion order).
        out.sort_by_key(|e| match e.severity {
            Severity::Critical => 0,
            Severity::Warning => 1,
        });
        out
    }

    /// Count of unread events — what the Status badge shows.
    pub fn unread_count(&self) -> usize {
        self.lock().events.iter().filter(|e| e.unread).count()
    }

    /// Mark every event read (called when the panel is opened).
    pub fn mark_all_read(&self) {
        for e in self.lock().events.iter_mut() {
            e.unread = false;
        }
    }

    /// The clipboard text for one event: its message plus the system
    /// fingerprint, ready to paste into a bug report.
    pub fn copy_text(&self, title: &str, detail: &str) -> String {
        format!(
            "{title}\n{detail}\n\n--- system ---\n{}",
            self.lock().fingerprint
        )
    }

    /// The clipboard text for the whole panel: every event plus the fingerprint
    /// and (when supplied) the tail of the session log.
    pub fn full_report(&self, log_tail: Option<&str>) -> String {
        let inner = self.lock();
        let mut s = String::from("dedup diagnostics report\n========================\n\n");
        // Critical first, matching the on-screen order (`events`), so the pasted
        // report reads the same way the user saw it.
        let mut ordered: Vec<&Stored> = inner.events.iter().collect();
        ordered.sort_by_key(|e| match e.severity {
            Severity::Critical => 0,
            Severity::Warning => 1,
        });
        for e in ordered {
            let n = if e.count > 1 {
                format!(" (x{})", e.count)
            } else {
                String::new()
            };
            s.push_str(&format!(
                "[{}] {}{n}\n    {}\n",
                e.severity.label(),
                e.title,
                e.detail
            ));
        }
        if inner.events.is_empty() {
            s.push_str("(no warnings)\n");
        }
        s.push_str("\n--- system ---\n");
        s.push_str(&inner.fingerprint);
        if let Some(tail) = log_tail {
            s.push_str("\n\n--- recent log ---\n");
            s.push_str(tail);
        }
        s
    }
}

/// The result of the one-time environment probe: availability flags for the
/// startup warnings, plus a bug-report fingerprint.
pub struct SystemProbe {
    pub fingerprint: String,
    pub audio_ok: bool,
    pub ffmpeg_ok: bool,
    pub pdftoppm_ok: bool,
}

/// Probe the environment **once** — the audio device and each external tool
/// spawned a single time — and build both the availability flags and the
/// one-line-per-fact fingerprint. Does blocking device init and process spawns,
/// so call it **off the UI thread** (it must never run in a paint frame).
pub fn probe_system() -> SystemProbe {
    // Run a tool once; `Some(version-line)` means available.
    let tool = |name: &str, arg: &str| -> Option<String> {
        let out = std::process::Command::new(name).arg(arg).output().ok()?;
        if !out.status.success() {
            return None;
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let line = stdout
            .lines()
            .next()
            .or_else(|| stderr.lines().next())
            .unwrap_or("")
            .trim();
        Some(if line.is_empty() {
            "present".to_string()
        } else {
            line.to_string()
        })
    };
    let audio_ok = rodio::OutputStream::try_default().is_ok();
    let ffmpeg = tool("ffmpeg", "-version");
    let ffprobe = tool("ffprobe", "-version");
    let pdftoppm = tool("pdftoppm", "-v");
    let line = |o: &Option<String>| o.clone().unwrap_or_else(|| "not found".to_string());
    let fingerprint = format!(
        "dedup: {}\nos: {} {}\naudio output: {}\nffmpeg: {}\nffprobe: {}\npdftoppm: {}\n",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        if audio_ok {
            "available"
        } else {
            "none (playback disabled)"
        },
        line(&ffmpeg),
        line(&ffprobe),
        line(&pdftoppm),
    );
    SystemProbe {
        fingerprint,
        audio_ok,
        ffmpeg_ok: ffmpeg.is_some(),
        pdftoppm_ok: pdftoppm.is_some(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedupes_by_key_and_counts_repeats() {
        let d = Diagnostics::new();
        d.push(Severity::Warning, "no-ffmpeg", "No ffmpeg", "video off");
        d.push(
            Severity::Critical,
            "missing:RepoA",
            "Files missing",
            "1 gone",
        );
        // A repeat of the same key bumps the count, does not add a row.
        d.push(
            Severity::Critical,
            "missing:RepoA",
            "Files missing",
            "3 gone",
        );
        let events = d.events();
        assert_eq!(events.len(), 2, "one row per key");
        let missing = events.iter().find(|e| e.title == "Files missing").unwrap();
        assert_eq!(missing.count, 2, "the repeat bumped the count");
        assert_eq!(missing.detail, "3 gone", "newest detail wins");
        // Critical sorts before Warning.
        assert_eq!(events[0].severity, Severity::Critical);
    }

    #[test]
    fn unread_badge_and_mark_read() {
        let d = Diagnostics::new();
        d.push(Severity::Warning, "a", "A", "");
        d.push(Severity::Warning, "b", "B", "");
        assert_eq!(d.unread_count(), 2);
        d.mark_all_read();
        assert_eq!(d.unread_count(), 0);
        // A fresh repeat re-flags unread.
        d.push(Severity::Warning, "a", "A", "again");
        assert_eq!(d.unread_count(), 1);
    }

    #[test]
    fn clear_removes_by_key() {
        let d = Diagnostics::new();
        d.push(Severity::Critical, "missing:R", "gone", "");
        assert_eq!(d.events().len(), 1);
        d.clear("missing:R");
        assert!(
            d.events().is_empty(),
            "the root came back, the event clears"
        );
    }

    #[test]
    fn copy_and_report_carry_the_fingerprint() {
        let d = Diagnostics::new();
        d.set_fingerprint("dedup: 0.1.0\naudio output: none");
        d.push(
            Severity::Warning,
            "audio",
            "No audio output",
            "playback disabled",
        );
        let one = d.copy_text("No audio output", "playback disabled");
        assert!(one.contains("No audio output") && one.contains("audio output: none"));
        let report = d.full_report(Some("...log line..."));
        assert!(report.contains("[WARNING] No audio output"));
        assert!(report.contains("--- system ---") && report.contains("dedup: 0.1.0"));
        assert!(report.contains("--- recent log ---") && report.contains("...log line..."));
    }
}
