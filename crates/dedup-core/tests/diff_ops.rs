//! Integration tests for `dedup_core::diff` — ports of the legacy
//! `DiffProcessSyncTest` and `DiffProcessMoveTest` scenarios, plus coverage
//! for print/cp/rm. Real files and real indices in a tempdir replace the
//! Java in-memory mock filesystem.

use dedup_core::diff::{CopyDest, DiffItem, diff_copy, diff_delete, diff_print, diff_sync};
use dedup_core::store::{FileEntry, Store};
use dedup_core::update::{CancellationToken, NoProgress, update_repo};
use std::path::{Path, PathBuf};

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct Sandbox {
    _tempdir: tempfile::TempDir,
    store: Store,
    a_root: PathBuf,
    b_root: PathBuf,
}

impl Sandbox {
    /// Two registered repos A and B with real data directories.
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let tempdir = tempfile::tempdir()?;
        let store = Store::open_at(tempdir.path().join("config"))?;
        let a_root = tempdir.path().join("Adata");
        let b_root = tempdir.path().join("Bdata");
        std::fs::create_dir_all(&a_root)?;
        std::fs::create_dir_all(&b_root)?;
        store.create_repo("A", &a_root.to_string_lossy())?;
        store.create_repo("B", &b_root.to_string_lossy())?;
        Ok(Self {
            _tempdir: tempdir,
            store,
            a_root,
            b_root,
        })
    }

    fn write(root: &Path, rel: &str, content: &[u8]) -> TestResult {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, content)?;
        Ok(())
    }

    fn update(&self, name: &str) -> TestResult {
        update_repo(&self.store, name, 1, &NoProgress, &CancellationToken::new())?;
        Ok(())
    }
}

/// A handcrafted index entry for tests that need MIME types (real scans
/// don't detect MIME until Phase 4).
fn entry(size: u64, hash_byte: u8, mime: &str, missing: bool) -> FileEntry {
    FileEntry {
        size,
        hash: [hash_byte; 32],
        modified_ms: 1,
        missing,
        mime: Some(mime.to_string()),
        img_fingerprint: None,
        video_hash: None,
        pdf_hash: None,
        audio: None,
        img_size: None,
    }
}

#[test]
fn sync_does_not_copy_when_content_already_present_in_b() -> TestResult {
    let sb = Sandbox::new()?;
    Sandbox::write(&sb.a_root, "x.txt", b"hello")?;
    Sandbox::write(&sb.b_root, "y.txt", b"hello")?;
    sb.update("A")?;
    sb.update("B")?;

    let stats = diff_sync(
        &sb.store,
        "A",
        "B",
        true,
        false,
        None,
        &CancellationToken::new(),
    )?;
    assert_eq!(stats.copied, 0);
    assert_eq!(stats.equal, 1);
    assert!(!sb.b_root.join("x.txt").exists());
    Ok(())
}

#[test]
fn sync_copies_when_missing_in_b_and_updates_index() -> TestResult {
    let sb = Sandbox::new()?;
    Sandbox::write(&sb.a_root, "dir/a.txt", b"hello")?;
    sb.update("A")?;

    let stats = diff_sync(
        &sb.store,
        "A",
        "B",
        true,
        false,
        None,
        &CancellationToken::new(),
    )?;
    assert_eq!(stats.copied, 1);
    assert_eq!(std::fs::read(sb.b_root.join("dir/a.txt"))?, b"hello");

    let in_b = sb
        .store
        .get_file_entry("B", "dir/a.txt")?
        .ok_or("copied file not in B index")?;
    assert!(!in_b.missing);
    assert_eq!(in_b.hash, *blake3::hash(b"hello").as_bytes());
    assert_eq!(in_b.size, 5);

    // The indexed mtime matches the file on disk: a subsequent update run
    // must not re-hash the copy.
    let update_stats = update_repo(&sb.store, "B", 1, &NoProgress, &CancellationToken::new())?;
    assert_eq!(update_stats.unchanged, 1);
    assert_eq!(update_stats.added + update_stats.updated, 0);
    Ok(())
}

#[test]
fn sync_deletes_when_marked_missing_in_a_and_updates_index() -> TestResult {
    let sb = Sandbox::new()?;
    // A once had the content, then it vanished (missing entry).
    Sandbox::write(&sb.a_root, "somewhere.txt", b"xyz")?;
    sb.update("A")?;
    std::fs::remove_file(sb.a_root.join("somewhere.txt"))?;
    sb.update("A")?;
    // B still has that content at another path.
    Sandbox::write(&sb.b_root, "del.txt", b"xyz")?;
    sb.update("B")?;

    let stats = diff_sync(
        &sb.store,
        "A",
        "B",
        false,
        true,
        None,
        &CancellationToken::new(),
    )?;
    assert_eq!(stats.deleted, 1);
    assert!(!sb.b_root.join("del.txt").exists());

    let in_b = sb
        .store
        .get_file_entry("B", "del.txt")?
        .ok_or("del.txt entry dropped")?;
    assert!(in_b.missing);
    assert_eq!(in_b.hash, *blake3::hash(b"xyz").as_bytes());
    Ok(())
}

#[test]
fn sync_skips_copy_when_target_path_already_occupied_by_different_content() -> TestResult {
    let sb = Sandbox::new()?;
    Sandbox::write(&sb.a_root, "x.txt", b"hello")?;
    Sandbox::write(&sb.b_root, "x.txt", b"different")?;
    sb.update("A")?;
    sb.update("B")?;

    let stats = diff_sync(
        &sb.store,
        "A",
        "B",
        true,
        false,
        None,
        &CancellationToken::new(),
    )?;
    assert_eq!(stats.copied, 0);
    assert_eq!(stats.skipped, 1);
    assert_eq!(std::fs::read(sb.b_root.join("x.txt"))?, b"different");

    let in_b = sb
        .store
        .get_file_entry("B", "x.txt")?
        .ok_or("x.txt not in B index")?;
    assert_eq!(in_b.hash, *blake3::hash(b"different").as_bytes());
    Ok(())
}

#[test]
fn sync_obeys_mime_filter() -> TestResult {
    let sb = Sandbox::new()?;
    Sandbox::write(&sb.a_root, "img.png", b"imagedata")?;
    Sandbox::write(&sb.a_root, "doc.txt", b"textdata")?;
    sb.store
        .update_file_entry("A", "img.png", &entry(9, 1, "image/png", false))?;
    sb.store
        .update_file_entry("A", "doc.txt", &entry(8, 2, "text/plain", false))?;

    let stats = diff_sync(
        &sb.store,
        "A",
        "B",
        true,
        false,
        Some("mime:image"),
        &CancellationToken::new(),
    )?;
    assert_eq!(stats.copied, 1);
    assert!(sb.b_root.join("img.png").exists());
    assert!(!sb.b_root.join("doc.txt").exists());
    assert!(sb.store.get_file_entry("B", "img.png")?.is_some());
    assert!(sb.store.get_file_entry("B", "doc.txt")?.is_none());
    Ok(())
}

#[test]
fn sync_obeys_mime_filter_for_delete() -> TestResult {
    let sb = Sandbox::new()?;
    // B has an image and a text file; A marks both contents missing.
    Sandbox::write(&sb.b_root, "img.png", b"imagedata!")?;
    Sandbox::write(&sb.b_root, "doc.txt", b"textdata!doc")?;
    sb.store
        .update_file_entry("B", "img.png", &entry(10, 1, "image/png", false))?;
    sb.store
        .update_file_entry("B", "doc.txt", &entry(12, 2, "text/plain", false))?;
    sb.store
        .update_file_entry("A", "img.png", &entry(10, 1, "image/png", true))?;
    sb.store
        .update_file_entry("A", "doc.txt", &entry(12, 2, "text/plain", true))?;

    let stats = diff_sync(
        &sb.store,
        "A",
        "B",
        false,
        true,
        Some("mime:image"),
        &CancellationToken::new(),
    )?;
    assert_eq!(stats.deleted, 1);
    assert!(!sb.b_root.join("img.png").exists());
    assert!(sb.b_root.join("doc.txt").exists());

    let img = sb
        .store
        .get_file_entry("B", "img.png")?
        .ok_or("img.png entry dropped")?;
    assert!(img.missing, "img.png should be missing");
    let doc = sb
        .store
        .get_file_entry("B", "doc.txt")?
        .ok_or("doc.txt entry dropped")?;
    assert!(!doc.missing, "doc.txt should NOT be missing");
    Ok(())
}

#[test]
fn sync_obeys_size_filter() -> TestResult {
    let sb = Sandbox::new()?;
    Sandbox::write(&sb.a_root, "small.txt", b"small")?;
    Sandbox::write(&sb.a_root, "large.txt", b"very large content")?;
    sb.update("A")?;

    let stats = diff_sync(
        &sb.store,
        "A",
        "B",
        true,
        false,
        Some("size:5"),
        &CancellationToken::new(),
    )?;
    assert_eq!(stats.copied, 1);
    assert!(sb.b_root.join("small.txt").exists());
    assert!(!sb.b_root.join("large.txt").exists());
    Ok(())
}

#[test]
fn move_updates_source_index_to_missing() -> TestResult {
    let sb = Sandbox::new()?;
    Sandbox::write(&sb.a_root, "to_move.txt", b"content")?;
    sb.update("A")?;
    let target_dir = sb._tempdir.path().join("move-target");

    let stats = diff_copy(
        &sb.store,
        "A",
        "B",
        CopyDest {
            dir: &target_dir,
            subdir: None,
        },
        true,
        None,
        &CancellationToken::new(),
    )?;
    assert_eq!(stats.copied, 1);
    assert!(!sb.a_root.join("to_move.txt").exists());
    assert_eq!(std::fs::read(target_dir.join("to_move.txt"))?, b"content");

    let in_a = sb
        .store
        .get_file_entry("A", "to_move.txt")?
        .ok_or("to_move.txt entry dropped")?;
    assert!(
        in_a.missing,
        "Source file should be marked as missing after move"
    );
    Ok(())
}

#[test]
fn copy_only_transfers_content_the_reference_has_never_seen() -> TestResult {
    let sb = Sandbox::new()?;
    Sandbox::write(&sb.a_root, "unknown.txt", b"aaa")?;
    Sandbox::write(&sb.a_root, "present.txt", b"bbb")?;
    Sandbox::write(&sb.a_root, "was_there.txt", b"ccc")?;
    // B has "bbb" present and once had "ccc" (now missing).
    Sandbox::write(&sb.b_root, "b.txt", b"bbb")?;
    Sandbox::write(&sb.b_root, "c.txt", b"ccc")?;
    sb.update("A")?;
    sb.update("B")?;
    std::fs::remove_file(sb.b_root.join("c.txt"))?;
    sb.update("B")?;

    let target_dir = sb._tempdir.path().join("copy-target");
    let stats = diff_copy(
        &sb.store,
        "A",
        "B",
        CopyDest {
            dir: &target_dir,
            subdir: None,
        },
        false,
        None,
        &CancellationToken::new(),
    )?;
    // Even a missing reference entry blocks the copy (legacy semantics).
    assert_eq!(stats.copied, 1);
    assert!(target_dir.join("unknown.txt").exists());
    assert!(!target_dir.join("present.txt").exists());
    assert!(!target_dir.join("was_there.txt").exists());
    // Plain copy leaves the source untouched.
    assert!(sb.a_root.join("unknown.txt").exists());
    Ok(())
}

#[test]
fn copy_into_subdir_preserves_relative_paths() -> TestResult {
    let sb = Sandbox::new()?;
    Sandbox::write(&sb.a_root, "photos/2020/a.jpg", b"img")?;
    sb.update("A")?;
    let target_dir = sb._tempdir.path().join("subdir-copy-target");

    let stats = diff_copy(
        &sb.store,
        "A",
        "B",
        CopyDest {
            dir: &target_dir,
            subdir: Some("imports/batch1"),
        },
        false,
        None,
        &CancellationToken::new(),
    )?;
    assert_eq!(stats.copied, 1);
    assert_eq!(
        std::fs::read(target_dir.join("imports/batch1/photos/2020/a.jpg"))?,
        b"img"
    );
    // Nothing landed directly at the target root.
    assert!(!target_dir.join("photos/2020/a.jpg").exists());
    Ok(())
}

#[test]
fn move_into_subdir_places_files_and_marks_source_missing() -> TestResult {
    let sb = Sandbox::new()?;
    Sandbox::write(&sb.a_root, "docs/note.txt", b"hi")?;
    sb.update("A")?;
    let target_dir = sb._tempdir.path().join("subdir-move-target");

    let stats = diff_copy(
        &sb.store,
        "A",
        "B",
        CopyDest {
            dir: &target_dir,
            subdir: Some("archive"),
        },
        true,
        None,
        &CancellationToken::new(),
    )?;
    assert_eq!(stats.copied, 1);
    assert!(!sb.a_root.join("docs/note.txt").exists());
    assert_eq!(
        std::fs::read(target_dir.join("archive/docs/note.txt"))?,
        b"hi"
    );
    let in_a = sb
        .store
        .get_file_entry("A", "docs/note.txt")?
        .ok_or("docs/note.txt entry dropped")?;
    assert!(
        in_a.missing,
        "Source file should be marked missing after move"
    );
    Ok(())
}

#[test]
fn copy_with_escaping_subdir_is_rejected() -> TestResult {
    let sb = Sandbox::new()?;
    Sandbox::write(&sb.a_root, "file.txt", b"data")?;
    sb.update("A")?;
    let target_dir = sb._tempdir.path().join("escape-target");

    let result = diff_copy(
        &sb.store,
        "A",
        "B",
        CopyDest {
            dir: &target_dir,
            subdir: Some("../outside"),
        },
        false,
        None,
        &CancellationToken::new(),
    );
    assert!(matches!(
        result,
        Err(dedup_core::diff::DiffError::InvalidSubdir { .. })
    ));
    // No files were written anywhere under the target.
    assert!(!target_dir.exists());
    Ok(())
}

#[test]
fn print_classifies_new_equal_and_deleted_in_reference() -> TestResult {
    let sb = Sandbox::new()?;
    Sandbox::write(&sb.a_root, "new.txt", b"only in A")?;
    Sandbox::write(&sb.a_root, "equal.txt", b"in both")?;
    Sandbox::write(&sb.a_root, "gone.txt", b"was in B")?;
    Sandbox::write(&sb.b_root, "other_name.txt", b"in both")?;
    Sandbox::write(&sb.b_root, "deleted.txt", b"was in B")?;
    sb.update("A")?;
    sb.update("B")?;
    std::fs::remove_file(sb.b_root.join("deleted.txt"))?;
    sb.update("B")?;

    let mut items = diff_print(&sb.store, "A", "B", None)?;
    items.sort_by_key(|item| match item {
        DiffItem::New { rel_path } => rel_path.clone(),
        DiffItem::Equal { rel_path, .. } => rel_path.clone(),
        DiffItem::DeletedInReference { rel_path } => rel_path.clone(),
    });
    assert_eq!(
        items,
        vec![
            DiffItem::Equal {
                rel_path: "equal.txt".to_string(),
                reference_path: "other_name.txt".to_string(),
            },
            DiffItem::DeletedInReference {
                rel_path: "gone.txt".to_string(),
            },
            DiffItem::New {
                rel_path: "new.txt".to_string(),
            },
        ]
    );
    Ok(())
}

#[test]
fn delete_removes_source_files_known_to_reference_and_marks_them_missing() -> TestResult {
    let sb = Sandbox::new()?;
    Sandbox::write(&sb.a_root, "dupe.txt", b"shared")?;
    Sandbox::write(&sb.a_root, "unique.txt", b"only A")?;
    Sandbox::write(&sb.b_root, "somewhere.txt", b"shared")?;
    sb.update("A")?;
    sb.update("B")?;

    let stats = diff_delete(&sb.store, "A", "B", None, &CancellationToken::new())?;
    assert_eq!(stats.deleted, 1);
    assert!(!sb.a_root.join("dupe.txt").exists());
    assert!(sb.a_root.join("unique.txt").exists());

    let dupe = sb
        .store
        .get_file_entry("A", "dupe.txt")?
        .ok_or("dupe.txt entry dropped")?;
    assert!(dupe.missing);
    Ok(())
}
