//! Integration tests for `dedup_core::update::check_repo`: the dry-run change
//! detection that reports new/changed and vanished counts without hashing or
//! writing the index. The key case is a file synced in with a *preserved*
//! (older-than-last-scan) timestamp — a naive "mtime > last_scan" check would
//! miss it, but diffing against the stored record catches it.

use dedup_core::store::Store;
use dedup_core::update::{CancellationToken, NoProgress, check_repo, update_repo};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

type TestResult = Result<(), Box<dyn std::error::Error>>;

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

/// Force a file's mtime, simulating an rsync/BeyondCompare copy that preserved
/// the source timestamp.
fn set_mtime(data: &Path, rel: &str, time: SystemTime) -> TestResult {
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(data.join(rel))?;
    f.set_modified(time)?;
    Ok(())
}

#[test]
fn check_reports_clean_tree_as_up_to_date() -> TestResult {
    let tempdir = tempfile::tempdir()?;
    let (store, data) = setup(tempdir.path())?;
    let cancel = CancellationToken::new();

    write(&data, "a.txt", b"hello")?;
    write(&data, "sub/b.txt", b"world")?;
    update_repo(&store, "test", 1, &NoProgress, &cancel)?;

    let stats = check_repo(&store, "test", &NoProgress, &cancel)?;
    assert_eq!(stats.changed, 0);
    assert_eq!(stats.missing, 0);
    assert_eq!(stats.unchanged, 2);
    assert!(stats.up_to_date());
    assert!(!stats.cancelled);
    Ok(())
}

#[test]
fn check_flags_new_and_vanished_without_touching_index() -> TestResult {
    let tempdir = tempfile::tempdir()?;
    let (store, data) = setup(tempdir.path())?;
    let cancel = CancellationToken::new();

    write(&data, "a.txt", b"hello")?;
    write(&data, "gone.txt", b"bye")?;
    update_repo(&store, "test", 1, &NoProgress, &cancel)?;
    let before = store.get_repo_stats("test")?;

    // One new file appears; one indexed file vanishes.
    write(&data, "new.txt", b"fresh")?;
    std::fs::remove_file(data.join("gone.txt"))?;

    let stats = check_repo(&store, "test", &NoProgress, &cancel)?;
    assert_eq!(stats.changed, 1, "new.txt is new");
    assert_eq!(stats.missing, 1, "gone.txt vanished");
    assert_eq!(stats.unchanged, 1, "a.txt unchanged");
    assert!(!stats.up_to_date());

    // The check must not have written anything: stats and index are unchanged,
    // and gone.txt is not (yet) marked missing.
    let after = store.get_repo_stats("test")?;
    assert_eq!(after.file_count, before.file_count);
    assert_eq!(after.missing_count, before.missing_count);
    assert!(store.get_file_entry("test", "new.txt")?.is_none());
    assert!(
        !store
            .get_file_entry("test", "gone.txt")?
            .ok_or("dropped")?
            .missing
    );
    Ok(())
}

#[test]
fn check_detects_same_size_file_with_preserved_older_mtime() -> TestResult {
    let tempdir = tempfile::tempdir()?;
    let (store, data) = setup(tempdir.path())?;
    let cancel = CancellationToken::new();

    // Index the file at its current (recent) mtime.
    write(&data, "a.txt", b"hello")?;
    update_repo(&store, "test", 1, &NoProgress, &cancel)?;

    // Simulate a sync: same byte length, but content changed and the mtime set
    // to an old value that is *earlier* than the last scan. A "mtime > last
    // scan" heuristic would treat this as unchanged; diffing against the stored
    // record does not, because the stored mtime differs.
    write(&data, "a.txt", b"world")?; // same 5-byte length
    let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    set_mtime(&data, "a.txt", old)?;

    let stats = check_repo(&store, "test", &NoProgress, &cancel)?;
    assert_eq!(
        stats.changed, 1,
        "changed mtime is detected despite old value"
    );
    assert_eq!(stats.unchanged, 0);
    assert!(!stats.up_to_date());
    Ok(())
}

#[test]
fn check_on_cancel_reports_cancelled() -> TestResult {
    let tempdir = tempfile::tempdir()?;
    let (store, data) = setup(tempdir.path())?;
    write(&data, "a.txt", b"hello")?;

    let cancel = CancellationToken::new();
    cancel.cancel();
    let stats = check_repo(&store, "test", &NoProgress, &cancel)?;
    assert!(stats.cancelled);
    assert_eq!(
        stats.missing, 0,
        "cancelled walk never concludes files vanished"
    );
    Ok(())
}
