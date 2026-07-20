//! End-to-end check of the session log, in its own process.
//!
//! The logger and the panic hook are process-global, so this lives in its own
//! integration-test binary rather than the unit tests: it needs a process where
//! nothing else has installed either.

use std::path::PathBuf;

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// A session records what the app did, and — the reason the hook exists — a
/// panic on a worker thread lands in the file instead of dying with the thread.
#[test]
fn a_session_records_messages_and_panics() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path().join("logs");
    let path: PathBuf = dedup_core::logging::init_in(&dir, 10)?;
    assert_eq!(
        dedup_core::logging::current_log().as_deref(),
        Some(path.as_path()),
        "the session names the file it is writing"
    );

    log::info!("scan of 'PHOTOS': added 3, updated 0");
    log::error!("sink 'BACKUP1' failed: drive gone");

    // Keep the chained hook quiet so the panic doesn't spam the test output;
    // `capture_panics` calls whatever hook it replaced.
    std::panic::set_hook(Box::new(|_| {}));
    dedup_core::logging::capture_panics();
    let caught = std::panic::catch_unwind(|| {
        panic!("worker thread fell over");
    });
    assert!(caught.is_err(), "the panic really happened");

    let written = std::fs::read_to_string(&path)?;
    assert!(
        written.contains("scan of 'PHOTOS': added 3, updated 0"),
        "ordinary operations leave a trail: {written}"
    );
    assert!(
        written.contains("ERROR") && written.contains("sink 'BACKUP1' failed"),
        "failures are recorded with their level: {written}"
    );
    assert!(
        written.contains("PANIC") && written.contains("worker thread fell over"),
        "a panic reaches the log rather than vanishing: {written}"
    );
    assert!(
        written.contains("logging_session.rs"),
        "with the source location, so it can be chased: {written}"
    );
    Ok(())
}
