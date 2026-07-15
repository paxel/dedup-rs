//! Integration tests for `dedup_core::groom` — bulk delete-by-filter and
//! empty-directory pruning against a real repo in a tempdir.

use dedup_core::diff::{DiffRun, NoDiffProgress};
use dedup_core::groom::{delete_by_filter, delete_empty_dirs};
use dedup_core::update::{CancellationToken, NoProgress, update_repo};
use std::path::PathBuf;

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct Sandbox {
    _tempdir: tempfile::TempDir,
    store: dedup_core::store::Store,
    root: PathBuf,
}

impl Sandbox {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let tempdir = tempfile::tempdir()?;
        let store = dedup_core::store::Store::open_at(tempdir.path().join("config"))?;
        let root = tempdir.path().join("Rdata");
        std::fs::create_dir_all(&root)?;
        store.create_repo("R", &root.to_string_lossy())?;
        Ok(Self {
            _tempdir: tempdir,
            store,
            root,
        })
    }

    fn write(&self, rel: &str, content: &[u8]) -> TestResult {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, content)?;
        Ok(())
    }

    fn update(&self) -> TestResult {
        update_repo(&self.store, "R", 1, &NoProgress, &CancellationToken::new())?;
        Ok(())
    }
}

#[test]
fn delete_by_filter_removes_matches_and_marks_missing() -> TestResult {
    let sb = Sandbox::new()?;
    sb.write("keep.txt", b"keep")?;
    sb.write("cache/a.db", b"db-a")?;
    sb.write("cache/b.db", b"db-b")?;
    sb.update()?;

    let cancel = CancellationToken::new();
    let run = DiffRun::new(&NoDiffProgress, &cancel);
    // Wildcard: everything ending in .db.
    let stats = delete_by_filter(&sb.store, "R", Some("name:*.db"), &run)?;

    assert_eq!(stats.deleted, 2, "both .db files deleted");
    assert!(!sb.root.join("cache/a.db").exists());
    assert!(!sb.root.join("cache/b.db").exists());
    assert!(sb.root.join("keep.txt").exists(), "non-matching file kept");

    // Deleted entries are marked missing in the index.
    assert!(sb.store.get_file_entry("R", "cache/a.db")?.unwrap().missing);
    assert!(!sb.store.get_file_entry("R", "keep.txt")?.unwrap().missing);
    Ok(())
}

#[test]
fn delete_by_filter_with_no_matches_is_a_noop() -> TestResult {
    let sb = Sandbox::new()?;
    sb.write("a.txt", b"a")?;
    sb.update()?;

    let cancel = CancellationToken::new();
    let run = DiffRun::new(&NoDiffProgress, &cancel);
    let stats = delete_by_filter(&sb.store, "R", Some("name:*.nope"), &run)?;
    assert_eq!(stats.deleted, 0);
    assert!(sb.root.join("a.txt").exists());
    Ok(())
}

#[test]
fn delete_empty_dirs_prunes_bottom_up_but_keeps_root() -> TestResult {
    let sb = Sandbox::new()?;
    // A file keeps `full/` alive; `empty/deep/` is empty all the way up.
    sb.write("full/keep.txt", b"x")?;
    std::fs::create_dir_all(sb.root.join("empty/deep"))?;
    std::fs::create_dir_all(sb.root.join("solo"))?;

    let removed = delete_empty_dirs(&sb.store, "R")?;
    assert_eq!(removed, 3, "empty, empty/deep, and solo are all removed");
    assert!(!sb.root.join("empty").exists(), "empty tree pruned");
    assert!(!sb.root.join("solo").exists());
    assert!(sb.root.join("full").exists(), "dir with a file is kept");
    assert!(sb.root.exists(), "the repo root itself is never removed");
    Ok(())
}

/// The deleted files are honored as a real path, guarding the join logic.
#[test]
fn delete_by_filter_respects_size_filter() -> TestResult {
    let sb = Sandbox::new()?;
    sb.write("small.bin", b"ab")?; // 2 bytes
    sb.write("big.bin", &vec![0u8; 5000])?;
    sb.update()?;

    let cancel = CancellationToken::new();
    let run = DiffRun::new(&NoDiffProgress, &cancel);
    let stats = delete_by_filter(&sb.store, "R", Some("size:>=1000"), &run)?;
    assert_eq!(stats.deleted, 1);
    assert!(!sb.root.join("big.bin").exists());
    assert!(sb.root.join("small.bin").exists());
    Ok(())
}
