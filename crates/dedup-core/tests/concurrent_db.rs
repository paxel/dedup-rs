//! Integration tests for the shared repo-database handles: `open_repo_db`
//! hands every caller the same cached `Arc<redb::Database>`, so concurrent
//! operations (a scan plus a view, two views, …) work under redb's MVCC
//! instead of failing with "database already open" — while repo-identity
//! operations (remove/rename/duplicate) refuse to touch an index that is
//! still in use.

use dedup_core::store::{Store, StoreError};
use dedup_core::update::{CancellationToken, NoProgress, update_repo};
use std::path::PathBuf;
use std::sync::Arc;

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct Sandbox {
    _tempdir: tempfile::TempDir,
    store: Store,
    root: PathBuf,
}

impl Sandbox {
    /// One registered repo R with a real data directory holding `files` small
    /// files.
    fn new(files: usize) -> Result<Self, Box<dyn std::error::Error>> {
        let tempdir = tempfile::tempdir()?;
        let store = Store::open_at(tempdir.path().join("config"))?;
        let root = tempdir.path().join("Rdata");
        std::fs::create_dir_all(&root)?;
        for i in 0..files {
            std::fs::write(root.join(format!("file{i}.txt")), format!("content {i}"))?;
        }
        store.create_repo("R", &root.to_string_lossy())?;
        Ok(Self {
            _tempdir: tempdir,
            store,
            root,
        })
    }

    fn scan(&self) -> TestResult {
        update_repo(&self.store, "R", 1, &NoProgress, &CancellationToken::new())?;
        Ok(())
    }
}

#[test]
fn open_repo_db_returns_the_same_shared_handle() -> TestResult {
    let sb = Sandbox::new(1)?;
    let first = sb.store.open_repo_db("R")?;
    let second = sb.store.open_repo_db("R")?;
    assert!(
        Arc::ptr_eq(&first, &second),
        "both callers must share one redb::Database instance"
    );
    Ok(())
}

#[test]
fn reads_work_while_another_holder_keeps_the_db_open() -> TestResult {
    let sb = Sandbox::new(3)?;
    sb.scan()?;

    // Simulates e.g. a scan or diff op holding the handle while a view reads
    // stats — previously the second open failed with "database already open".
    let _held = sb.store.open_repo_db("R")?;
    assert_eq!(sb.store.get_repo_stats("R")?.file_count, 3);
    assert!(!sb.store.get_mime_stats("R")?.is_empty());
    Ok(())
}

#[test]
fn concurrent_reader_and_writer_share_the_handle() -> TestResult {
    let sb = Sandbox::new(50)?;
    sb.scan()?;

    // A reader thread iterates the index over its own Arc clone while the
    // main thread re-scans (writes) — MVCC must let both finish cleanly.
    let db = sb.store.open_repo_db("R")?;
    let reader = std::thread::spawn(move || -> Result<usize, StoreError> {
        let mut seen = 0;
        for _ in 0..20 {
            let mut count = 0;
            dedup_core::store::for_each_file_entry(&db, |_, _| {
                count += 1;
                Ok(())
            })?;
            seen = count;
        }
        Ok(seen)
    });

    std::fs::write(sb.root.join("late.txt"), "late content")?;
    sb.scan()?;

    let seen = reader
        .join()
        .unwrap_or_else(|_| panic!("reader panicked"))?;
    assert!(seen >= 50, "reader saw a consistent snapshot, got {seen}");
    assert_eq!(sb.store.get_repo_stats("R")?.file_count, 51);
    Ok(())
}

#[test]
fn identity_ops_refuse_busy_repos_and_work_after_release() -> TestResult {
    let sb = Sandbox::new(1)?;
    sb.scan()?;

    let held = sb.store.open_repo_db("R")?;
    assert!(matches!(
        sb.store.remove_repo("R"),
        Err(StoreError::Busy(_))
    ));
    assert!(matches!(
        sb.store.rename_repo("R", "S"),
        Err(StoreError::Busy(_))
    ));
    let dest = sb.root.join("copy");
    assert!(matches!(
        sb.store.duplicate_repo("R", "R2", &dest.to_string_lossy()),
        Err(StoreError::Busy(_))
    ));

    drop(held);
    sb.store
        .duplicate_repo("R", "R2", &dest.to_string_lossy())?;
    assert_eq!(sb.store.get_repo_stats("R2")?.file_count, 1);
    sb.store.rename_repo("R", "S")?;
    assert_eq!(sb.store.get_repo_stats("S")?.file_count, 1);
    sb.store.remove_repo("S")?;
    assert!(matches!(
        sb.store.get_repo("S"),
        Err(StoreError::NotFound(_))
    ));
    Ok(())
}
