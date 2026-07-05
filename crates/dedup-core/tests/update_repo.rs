//! Integration tests for `dedup_core::update::update_repo` (Phase 2 acceptance):
//! create/modify/delete files in a tempdir and assert the index state,
//! re-running on an unchanged tree hashes nothing, and cancellation stops
//! cleanly while leaving the index consistent.

use dedup_core::store::Store;
use dedup_core::update::{
    CancellationToken, NoProgress, Progress, ProgressEvent, UpdateError, update_repo,
};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Records every progress event for later assertions.
#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<ProgressEvent>>,
}

impl Recorder {
    fn hashing_events(&self) -> usize {
        self.events
            .lock()
            .map(|events| {
                events
                    .iter()
                    .filter(|e| matches!(e, ProgressEvent::Hashing { .. }))
                    .count()
            })
            .unwrap_or(usize::MAX)
    }
}

impl Progress for Recorder {
    fn on(&self, event: ProgressEvent) {
        if let Ok(mut events) = self.events.lock() {
            events.push(event);
        }
    }
}

/// Cancels the token as soon as the first `Hashing` event arrives.
struct CancelOnFirstHash {
    token: CancellationToken,
}

impl Progress for CancelOnFirstHash {
    fn on(&self, event: ProgressEvent) {
        if matches!(event, ProgressEvent::Hashing { .. }) {
            self.token.cancel();
        }
    }
}

fn setup(tempdir: &Path) -> Result<(Store, PathBuf), Box<dyn std::error::Error>> {
    let store = Store::open_at(tempdir.join("config"))?;
    let data = tempdir.join("data");
    std::fs::create_dir_all(&data)?;
    store.create_repo("test", &data.to_string_lossy())?;
    Ok((store, data))
}

fn write(data: &Path, rel: &str, content: &[u8]) -> TestResult {
    let path = data.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)?;
    Ok(())
}

/// File mtimes are stored with millisecond precision; consecutive writes to
/// the same file within one millisecond would look unchanged. Tests sleep
/// briefly before rewriting a file to guarantee a distinct mtime.
fn tick() {
    std::thread::sleep(std::time::Duration::from_millis(20));
}

#[test]
fn update_lifecycle_add_modify_delete_restore() -> TestResult {
    let tempdir = tempfile::tempdir()?;
    let (store, data) = setup(tempdir.path())?;
    let cancel = CancellationToken::new();

    // Initial state: three files, two of them duplicates by content.
    write(&data, "a.txt", b"hello")?;
    write(&data, "sub/b.txt", b"world")?;
    write(&data, "dup.txt", b"hello")?;

    let stats = update_repo(&store, "test", 1, &NoProgress, &cancel)?;
    assert_eq!(stats.added, 3);
    assert_eq!(stats.updated, 0);
    assert_eq!(stats.unchanged, 0);
    assert_eq!(stats.marked_missing, 0);
    assert_eq!(stats.errors, 0);
    assert_eq!(stats.hashed_bytes, 15);
    assert!(!stats.cancelled);

    // Index entries carry the real BLAKE3 hash and metadata.
    let entry = store
        .get_file_entry("test", "a.txt")?
        .ok_or("a.txt not indexed")?;
    assert_eq!(entry.size, 5);
    assert_eq!(entry.hash, *blake3::hash(b"hello").as_bytes());
    assert!(!entry.missing);

    let repo_stats = store.get_repo_stats("test")?;
    assert_eq!(repo_stats.file_count, 3);
    assert_eq!(repo_stats.total_size, 15);
    assert_eq!(repo_stats.missing_count, 0);

    // The duplicate pair is visible via the size+hash index.
    let groups = store.get_duplicate_groups("test")?;
    assert_eq!(groups.len(), 1);
    assert!(groups[0].2.contains(&"a.txt".to_string()));
    assert!(groups[0].2.contains(&"dup.txt".to_string()));

    // Re-running on an unchanged tree hashes nothing.
    let recorder = Recorder::default();
    let stats = update_repo(&store, "test", 1, &recorder, &cancel)?;
    assert_eq!(stats.added, 0);
    assert_eq!(stats.updated, 0);
    assert_eq!(stats.unchanged, 3);
    assert_eq!(stats.hashed_bytes, 0);
    assert_eq!(recorder.hashing_events(), 0);

    // Modifying a file re-hashes exactly that file.
    tick();
    write(&data, "a.txt", b"HELLO++")?;
    let stats = update_repo(&store, "test", 1, &NoProgress, &cancel)?;
    assert_eq!(stats.updated, 1);
    assert_eq!(stats.unchanged, 2);
    let entry = store
        .get_file_entry("test", "a.txt")?
        .ok_or("a.txt not indexed")?;
    assert_eq!(entry.size, 7);
    assert_eq!(entry.hash, *blake3::hash(b"HELLO++").as_bytes());
    // a.txt and dup.txt no longer share size+hash.
    assert!(store.get_duplicate_groups("test")?.is_empty());

    // Deleting a file marks it missing but keeps its history entry.
    std::fs::remove_file(data.join("sub/b.txt"))?;
    let stats = update_repo(&store, "test", 1, &NoProgress, &cancel)?;
    assert_eq!(stats.marked_missing, 1);
    assert_eq!(stats.unchanged, 2);
    let entry = store
        .get_file_entry("test", "sub/b.txt")?
        .ok_or("sub/b.txt entry dropped")?;
    assert!(entry.missing);
    let repo_stats = store.get_repo_stats("test")?;
    assert_eq!(repo_stats.file_count, 2);
    assert_eq!(repo_stats.missing_count, 1);

    // Restoring the file re-hashes it and clears the missing state.
    write(&data, "sub/b.txt", b"world")?;
    let stats = update_repo(&store, "test", 1, &NoProgress, &cancel)?;
    assert_eq!(stats.updated, 1);
    assert_eq!(stats.marked_missing, 0);
    let entry = store
        .get_file_entry("test", "sub/b.txt")?
        .ok_or("sub/b.txt not indexed")?;
    assert!(!entry.missing);
    let repo_stats = store.get_repo_stats("test")?;
    assert_eq!(repo_stats.file_count, 3);
    assert_eq!(repo_stats.missing_count, 0);

    Ok(())
}

#[test]
fn update_with_multiple_threads_indexes_everything() -> TestResult {
    let tempdir = tempfile::tempdir()?;
    let (store, data) = setup(tempdir.path())?;

    for i in 0..50 {
        write(&data, &format!("dir{}/f{}.bin", i % 5, i), &[i as u8; 100])?;
    }

    let stats = update_repo(&store, "test", 4, &NoProgress, &CancellationToken::new())?;
    assert_eq!(stats.added, 50);
    assert_eq!(stats.errors, 0);
    assert_eq!(store.get_repo_stats("test")?.file_count, 50);
    Ok(())
}

#[test]
fn cancelled_before_start_hashes_nothing() -> TestResult {
    let tempdir = tempfile::tempdir()?;
    let (store, data) = setup(tempdir.path())?;
    write(&data, "a.txt", b"hello")?;

    let cancel = CancellationToken::new();
    cancel.cancel();
    let stats = update_repo(&store, "test", 1, &NoProgress, &cancel)?;
    assert!(stats.cancelled);
    assert_eq!(stats.added, 0);
    assert!(store.get_file_entry("test", "a.txt")?.is_none());
    Ok(())
}

#[test]
fn cancel_mid_hash_stops_cleanly_and_keeps_index_consistent() -> TestResult {
    let tempdir = tempfile::tempdir()?;
    let (store, data) = setup(tempdir.path())?;
    for i in 0..20 {
        write(&data, &format!("f{i}.bin"), &[i as u8; 1000])?;
    }

    let cancel = CancellationToken::new();
    let progress = CancelOnFirstHash {
        token: cancel.clone(),
    };
    let stats = update_repo(&store, "test", 1, &progress, &cancel)?;
    assert!(stats.cancelled);
    assert!(stats.added < 20, "cancellation should stop hashing early");
    // Whatever was committed is consistent: META matches the entries.
    assert_eq!(store.get_repo_stats("test")?.file_count, stats.added);
    // Nothing is marked missing on a cancelled run.
    assert_eq!(stats.marked_missing, 0);
    Ok(())
}

#[test]
fn update_missing_root_fails() -> TestResult {
    let tempdir = tempfile::tempdir()?;
    let store = Store::open_at(tempdir.path().join("config"))?;
    store.create_repo("ghost", "/nonexistent/dedup-test-root")?;

    match update_repo(&store, "ghost", 1, &NoProgress, &CancellationToken::new()) {
        Err(UpdateError::RootMissing(path)) => {
            assert_eq!(path, "/nonexistent/dedup-test-root");
        }
        other => return Err(format!("expected RootMissing, got {other:?}").into()),
    }
    Ok(())
}

#[test]
fn update_unknown_repo_fails() -> TestResult {
    let tempdir = tempfile::tempdir()?;
    let store = Store::open_at(tempdir.path().join("config"))?;

    assert!(update_repo(&store, "nope", 1, &NoProgress, &CancellationToken::new()).is_err());
    Ok(())
}

#[test]
fn unreadable_files_are_reported_and_skipped() -> TestResult {
    let tempdir = tempfile::tempdir()?;
    let (store, data) = setup(tempdir.path())?;
    write(&data, "ok.txt", b"fine")?;
    write(&data, "secret.txt", b"locked")?;

    // Remove read permission; the walk sees the file but hashing fails.
    let secret = data.join("secret.txt");
    let mut perms = std::fs::metadata(&secret)?.permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o000);
    std::fs::set_permissions(&secret, perms)?;

    let recorder = Recorder::default();
    let stats = update_repo(&store, "test", 1, &recorder, &CancellationToken::new())?;

    // Restore permissions so the tempdir can be cleaned up.
    let mut perms = std::fs::metadata(&secret)?.permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o644);
    std::fs::set_permissions(&secret, perms)?;

    assert_eq!(stats.added, 1);
    assert_eq!(stats.errors, 1);
    assert!(store.get_file_entry("test", "ok.txt")?.is_some());
    assert!(store.get_file_entry("test", "secret.txt")?.is_none());
    let errors = recorder
        .events
        .lock()
        .map_err(|e| e.to_string())?
        .iter()
        .filter(|e| matches!(e, ProgressEvent::Error { .. }))
        .count();
    assert_eq!(errors, 1);
    Ok(())
}
