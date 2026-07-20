//! Session logging: one log file per run of the app, a handful kept.
//!
//! Beta users can only report what the app wrote down. Every run opens its own
//! file under [`log_dir`] and installs it behind the [`log`] facade, so
//! `log::error!` / `warn!` / `info!` anywhere in the workspace lands in it. The
//! newest [`SESSIONS_KEPT`] sessions are retained and older ones deleted, so the
//! directory cannot grow without bound.
//!
//! Logs are diagnostics, not state the app reads back: a failure to open one is
//! reported to the caller but must never stop the app from running.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// How many session logs to keep, newest first.
pub const SESSIONS_KEPT: usize = 10;

const PREFIX: &str = "session-";
const SUFFIX: &str = ".log";

/// The app's XDG state directory: `$XDG_STATE_HOME/dedup`, or
/// `~/.local/state/dedup` when that is unset.
///
/// State, not config: logs are regenerated data the user never edits. `/var/log`
/// would need root — this is a user-level desktop app.
pub fn state_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_STATE_HOME")
        && !dir.trim().is_empty()
    {
        return PathBuf::from(dir).join("dedup");
    }
    match std::env::var("HOME") {
        Ok(home) => PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("dedup"),
        Err(_) => PathBuf::from(".local").join("state").join("dedup"),
    }
}

/// Where session logs live.
pub fn log_dir() -> PathBuf {
    state_dir().join("logs")
}

/// The file this session is writing to, once [`init`] has succeeded.
pub fn current_log() -> Option<PathBuf> {
    LOGGER.get().and_then(|l| l.path.clone())
}

/// Open a log for this session, prune older ones, and route the [`log`] facade
/// into it. Returns the file being written.
///
/// Calling it more than once is a no-op that returns the first session's path —
/// the facade only accepts one logger per process.
pub fn init() -> std::io::Result<PathBuf> {
    init_in(&log_dir(), SESSIONS_KEPT)
}

/// [`init`] against an explicit directory, for tests.
pub fn init_in(dir: &Path, keep: usize) -> std::io::Result<PathBuf> {
    if let Some(existing) = current_log() {
        return Ok(existing);
    }
    let (path, file) = open_session(dir, keep)?;

    let logger = FileLogger {
        path: Some(path.clone()),
        sink: Mutex::new(Some(file)),
    };
    // A second caller loses the race and keeps its own file unused; harmless,
    // and `current_log` still names the one the facade actually writes to.
    if LOGGER.set(logger).is_err() {
        return Ok(current_log().unwrap_or(path));
    }
    if let Some(logger) = LOGGER.get()
        && log::set_logger(logger).is_ok()
    {
        log::set_max_level(log::LevelFilter::Info);
        capture_panics();
    }
    Ok(path)
}

/// Route panics into the session log, then hand off to the hook that was
/// installed before.
///
/// A panic on a worker thread otherwise dies with the thread and the user is
/// left with a window that simply stopped — "it froze" is not a bug report.
/// Chaining to the previous hook keeps the normal stderr backtrace.
pub fn capture_panics() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let where_ = match info.location() {
            Some(loc) => format!("{}:{}", loc.file(), loc.line()),
            None => "an unknown location".to_string(),
        };
        log::error!(
            "PANIC at {where_} on thread '{}': {}",
            std::thread::current().name().unwrap_or("unnamed"),
            payload_text(info.payload())
        );
        previous(info);
    }));
}

/// The human-readable part of a panic payload, which is a `&str` or a `String`
/// for every panic raised by `panic!`/`unwrap`/`expect`.
fn payload_text(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "panic with a non-string payload".to_string()
    }
}

/// Prune the directory to `keep - 1` logs, then create this session's file and
/// write its header. Split out from [`init_in`] so it can be tested without the
/// process-global logger.
fn open_session(dir: &Path, keep: usize) -> std::io::Result<(PathBuf, std::fs::File)> {
    std::fs::create_dir_all(dir)?;
    // Prune before opening so `keep` counts this session too.
    prune(dir, keep.saturating_sub(1));

    let started = chrono::Local::now();
    // Millisecond precision, so two runs starting in the same second get their
    // own files instead of the second truncating the first. Still sorts
    // chronologically, which is what `sessions` relies on for pruning.
    let path = dir.join(format!(
        "{PREFIX}{}{SUFFIX}",
        started.format("%Y%m%d-%H%M%S%.3f")
    ));
    let mut file = std::fs::File::create(&path)?;
    writeln!(
        file,
        "=== dedup {} — session started {} ===",
        env!("CARGO_PKG_VERSION"),
        started.format("%Y-%m-%d %H:%M:%S%.3f")
    )?;
    file.flush()?;
    Ok((path, file))
}

/// Session logs in the directory, oldest first. The timestamp in the name sorts
/// chronologically, so the filename order is the age order.
fn sessions(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut logs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(PREFIX) && n.ends_with(SUFFIX))
        })
        .collect();
    logs.sort();
    logs
}

/// Delete all but the newest `keep` session logs.
fn prune(dir: &Path, keep: usize) {
    let logs = sessions(dir);
    let excess = logs.len().saturating_sub(keep);
    for old in logs.into_iter().take(excess) {
        // Best effort: a log we cannot delete must not stop the session.
        let _ = std::fs::remove_file(old);
    }
}

static LOGGER: OnceLock<FileLogger> = OnceLock::new();

struct FileLogger {
    path: Option<PathBuf>,
    sink: Mutex<Option<std::fs::File>>,
}

impl log::Log for FileLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Info
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        // Flushed per record rather than buffered: the sessions worth reading
        // are the ones that ended in a crash.
        let Ok(mut sink) = self.sink.lock() else {
            return;
        };
        if let Some(file) = sink.as_mut() {
            let _ = writeln!(
                file,
                "{} {:<5} [{}] {}",
                chrono::Local::now().format("%H:%M:%S%.3f"),
                record.level(),
                record.target(),
                record.args()
            );
            let _ = file.flush();
        }
    }

    fn flush(&self) {
        if let Ok(mut sink) = self.sink.lock()
            && let Some(file) = sink.as_mut()
        {
            let _ = file.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_dir_prefers_xdg_state_home() {
        // Not using the process env (tests share it); check the shape instead.
        let dir = state_dir();
        assert!(
            dir.ends_with("dedup"),
            "logs live under a dedup-owned directory, got {dir:?}"
        );
        assert!(
            !dir.starts_with("/var/log"),
            "a user-level app must not need root, got {dir:?}"
        );
        assert!(log_dir().ends_with("logs"));
    }

    #[test]
    fn pruning_keeps_the_newest_sessions() -> std::io::Result<()> {
        let tmp = tempfile::tempdir()?;
        for stamp in [
            "20260101-000000",
            "20260102-000000",
            "20260103-000000",
            "20260104-000000",
        ] {
            std::fs::write(tmp.path().join(format!("{PREFIX}{stamp}{SUFFIX}")), b"x")?;
        }
        // An unrelated file in the directory is never touched.
        std::fs::write(tmp.path().join("notes.txt"), b"keep me")?;

        prune(tmp.path(), 2);

        let left: Vec<String> = sessions(tmp.path())
            .iter()
            .filter_map(|p| p.file_name()?.to_str().map(str::to_string))
            .collect();
        assert_eq!(
            left,
            [
                format!("{PREFIX}20260103-000000{SUFFIX}"),
                format!("{PREFIX}20260104-000000{SUFFIX}"),
            ],
            "the two newest survive"
        );
        assert!(
            tmp.path().join("notes.txt").exists(),
            "foreign files remain"
        );
        Ok(())
    }

    #[test]
    fn pruning_an_empty_or_missing_directory_is_harmless() {
        let tmp = tempfile::tempdir().expect("tempdir");
        prune(tmp.path(), 5);
        prune(&tmp.path().join("nope"), 5);
        assert!(sessions(&tmp.path().join("nope")).is_empty());
    }

    /// A session opens its own file, writes a header, and leaves at most `keep`
    /// logs behind — the cap counts the session being opened.
    #[test]
    fn a_session_writes_a_header_and_respects_the_cap() -> std::io::Result<()> {
        let tmp = tempfile::tempdir()?;
        let dir = tmp.path().join("logs");
        std::fs::create_dir_all(&dir)?;
        for stamp in ["20260101-000000", "20260102-000000"] {
            std::fs::write(dir.join(format!("{PREFIX}{stamp}{SUFFIX}")), b"old")?;
        }

        let (path, _file) = open_session(&dir, 2)?;
        assert!(path.exists(), "the session log was created");
        let header = std::fs::read_to_string(&path)?;
        assert!(
            header.contains("session started"),
            "the log says when it began: {header}"
        );
        assert_eq!(
            sessions(&dir).len(),
            2,
            "the cap counts this session, so one old log was pruned"
        );
        assert!(
            !dir.join(format!("{PREFIX}20260101-000000{SUFFIX}"))
                .exists(),
            "the oldest went first"
        );
        Ok(())
    }

    /// A panic's message survives into the log whichever way it was raised —
    /// `panic!("literal")` gives a `&str`, `format!`-style gives a `String`.
    #[test]
    fn panic_payloads_are_readable() {
        let literal: Box<dyn std::any::Any + Send> = Box::new("index out of bounds");
        assert_eq!(payload_text(literal.as_ref()), "index out of bounds");

        let formatted: Box<dyn std::any::Any + Send> = Box::new("repo 'A' vanished".to_string());
        assert_eq!(payload_text(formatted.as_ref()), "repo 'A' vanished");

        let odd: Box<dyn std::any::Any + Send> = Box::new(42u8);
        assert!(
            payload_text(odd.as_ref()).contains("non-string"),
            "an exotic payload still says something"
        );
    }

    /// Records reach the file, tagged with their level — the whole point of the
    /// session log is that a bug report can carry what actually happened.
    #[test]
    fn records_are_written_with_their_level() -> std::io::Result<()> {
        use log::Log;
        let tmp = tempfile::tempdir()?;
        let (path, file) = open_session(tmp.path(), SESSIONS_KEPT)?;
        let logger = FileLogger {
            path: Some(path.clone()),
            sink: Mutex::new(Some(file)),
        };

        logger.log(
            &log::Record::builder()
                .args(format_args!("sink 'BACKUP1' failed: drive gone"))
                .level(log::Level::Error)
                .target("dedup_core::sync_group")
                .build(),
        );
        // Below the Info threshold: noise must stay out of a beta report.
        logger.log(
            &log::Record::builder()
                .args(format_args!("per-file chatter"))
                .level(log::Level::Debug)
                .target("noisy")
                .build(),
        );

        let written = std::fs::read_to_string(&path)?;
        assert!(
            written.contains("ERROR") && written.contains("sink 'BACKUP1' failed: drive gone"),
            "the error and its level are recorded: {written}"
        );
        assert!(
            written.contains("dedup_core::sync_group"),
            "and where it came from: {written}"
        );
        assert!(
            !written.contains("per-file chatter"),
            "debug records are filtered out: {written}"
        );
        Ok(())
    }
}
