//! Integration tests for `dedup_core::diff` — ports of the legacy
//! `DiffProcessSyncTest` and `DiffProcessMoveTest` scenarios, plus coverage
//! for print/cp/rm. Real files and real indices in a tempdir replace the
//! Java in-memory mock filesystem.

use dedup_core::diff::{
    CopyDest, DiffAction, DiffEvent, DiffItem, DiffProgress, DiffRun, FolderMode, NoDiffProgress,
    SyncDelete, diff_copy, diff_delete, diff_print, diff_sync, export_to_folder,
    plan_folder_export, plan_sync,
};
use dedup_core::store::{FileEntry, Store};
use dedup_core::update::{CancellationToken, NoProgress, update_repo};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// 2000-01-01T00:00:00Z — far enough in the past that a fresh copy's mtime
/// can never coincide with it.
fn old_mtime() -> std::time::SystemTime {
    std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(946_684_800)
}

fn mtime_of(path: &Path) -> Result<std::time::SystemTime, Box<dyn std::error::Error>> {
    Ok(std::fs::metadata(path)?.modified()?)
}

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

    /// Like [`Sandbox::write`], but backdates the file so a copy that fails to
    /// carry the timestamp over is obvious.
    fn write_dated(root: &Path, rel: &str, content: &[u8]) -> TestResult {
        Sandbox::write(root, rel, content)?;
        std::fs::File::options()
            .write(true)
            .open(root.join(rel))?
            .set_modified(old_mtime())?;
        Ok(())
    }

    fn update(&self, name: &str) -> TestResult {
        update_repo(&self.store, name, 1, &NoProgress, &CancellationToken::new())?;
        Ok(())
    }

    /// A scan that is *allowed* to empty the index, for the cases that
    /// deliberately remove every file from a repo.
    fn update_emptying(&self, name: &str) -> TestResult {
        dedup_core::update::update_repo_authorized(
            &self.store,
            name,
            1,
            &NoProgress,
            &CancellationToken::new(),
            true,
        )?;
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
        origin: None,
        exif: None,
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
        SyncDelete::None,
        None,
        &DiffRun::new(&NoDiffProgress, &CancellationToken::new()),
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
        SyncDelete::None,
        None,
        &DiffRun::new(&NoDiffProgress, &CancellationToken::new()),
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
    // That was A's only file, so this scan empties A's index on purpose.
    sb.update_emptying("A")?;
    // B still has that content at another path.
    Sandbox::write(&sb.b_root, "del.txt", b"xyz")?;
    sb.update("B")?;

    let stats = diff_sync(
        &sb.store,
        "A",
        "B",
        false,
        SyncDelete::Missing,
        None,
        &DiffRun::new(&NoDiffProgress, &CancellationToken::new()),
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
        SyncDelete::None,
        None,
        &DiffRun::new(&NoDiffProgress, &CancellationToken::new()),
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
        SyncDelete::None,
        Some("mime:image"),
        &DiffRun::new(&NoDiffProgress, &CancellationToken::new()),
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
        SyncDelete::Missing,
        Some("mime:image"),
        &DiffRun::new(&NoDiffProgress, &CancellationToken::new()),
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
        SyncDelete::None,
        Some("size:5"),
        &DiffRun::new(&NoDiffProgress, &CancellationToken::new()),
    )?;
    assert_eq!(stats.copied, 1);
    assert!(sb.b_root.join("small.txt").exists());
    assert!(!sb.b_root.join("large.txt").exists());
    Ok(())
}

#[test]
fn plan_sync_lists_copies_and_deletes() -> TestResult {
    let sb = Sandbox::new()?;
    // A: "new" is unknown to B; "gone" was indexed then removed (missing in A).
    Sandbox::write(&sb.a_root, "new.txt", b"new")?;
    Sandbox::write(&sb.a_root, "gone.txt", b"gone")?;
    sb.update("A")?;
    std::fs::remove_file(sb.a_root.join("gone.txt"))?;
    sb.update("A")?;
    // B still holds the "gone" content (at another path).
    Sandbox::write(&sb.b_root, "kept.txt", b"gone")?;
    sb.update("B")?;

    let plan = plan_sync(&sb.store, "A", "B", true, SyncDelete::Missing, None)?;
    assert_eq!(plan.copies, vec!["new.txt".to_string()]);
    assert_eq!(plan.deletes, vec!["kept.txt".to_string()]);

    // With delete_missing off, only copies are planned.
    let copy_only = plan_sync(&sb.store, "A", "B", true, SyncDelete::None, None)?;
    assert_eq!(copy_only.copies, vec!["new.txt".to_string()]);
    assert!(copy_only.deletes.is_empty());

    // Nothing on disk changed (plan is read-only).
    assert!(sb.b_root.join("kept.txt").exists());
    assert!(!sb.b_root.join("new.txt").exists());
    Ok(())
}

#[test]
fn sync_emits_progress_for_copies_and_deletes() -> TestResult {
    let sb = Sandbox::new()?;
    Sandbox::write(&sb.a_root, "new.txt", b"new")?;
    Sandbox::write(&sb.a_root, "gone.txt", b"gone")?;
    sb.update("A")?;
    std::fs::remove_file(sb.a_root.join("gone.txt"))?;
    sb.update("A")?;
    Sandbox::write(&sb.b_root, "kept.txt", b"gone")?;
    sb.update("B")?;

    let progress = RecordingProgress::default();
    let stats = diff_sync(
        &sb.store,
        "A",
        "B",
        true,
        SyncDelete::Missing,
        None,
        &DiffRun::new(&progress, &CancellationToken::new()),
    )?;
    assert_eq!(stats.copied, 1);
    assert_eq!(stats.deleted, 1);
    // One progress step per acting entry (one copy + one delete), and `done`
    // never overshoots `total`.
    assert_eq!(progress.progress_count(), 2);
    assert_eq!(progress.last_progress(), Some((2, 2)));
    Ok(())
}

#[test]
fn mirror_deletes_target_content_absent_from_source() -> TestResult {
    let sb = Sandbox::new()?;
    // A holds "keep" and "new"; B holds "keep" (same content, different path)
    // and "extra" (content A never had).
    Sandbox::write(&sb.a_root, "keep.txt", b"keep")?;
    Sandbox::write(&sb.a_root, "new.txt", b"new")?;
    Sandbox::write(&sb.b_root, "same-content.txt", b"keep")?;
    Sandbox::write(&sb.b_root, "extra.txt", b"extra")?;
    sb.update("A")?;
    sb.update("B")?;

    let progress = RecordingProgress::default();
    let stats = diff_sync(
        &sb.store,
        "A",
        "B",
        true,
        SyncDelete::Absent,
        None,
        &DiffRun::new(&progress, &CancellationToken::new()),
    )?;
    // "new" is copied in; "extra" is deleted (A lacks it); "keep" content is
    // already present in B (at another path), so it is neither copied nor
    // deleted — the mirror is by content.
    assert_eq!(stats.copied, 1, "only 'new' is copied");
    assert_eq!(stats.deleted, 1, "only 'extra' is deleted");
    assert_eq!(std::fs::read(sb.b_root.join("new.txt"))?, b"new");
    assert!(!sb.b_root.join("extra.txt").exists(), "extra removed");
    assert!(
        sb.b_root.join("same-content.txt").exists(),
        "identical content kept in place (content-mirror, not path-mirror)"
    );

    // After the mirror, B's live content set equals A's.
    assert!(sb.store.get_file_entry("B", "extra.txt")?.unwrap().missing);
    assert!(!sb.store.get_file_entry("B", "new.txt")?.unwrap().missing);
    Ok(())
}

#[test]
fn mirror_deletes_first_so_a_copy_reclaims_the_freed_path() -> TestResult {
    let sb = Sandbox::new()?;
    // Same relative path holds different content in A and B: a true mirror must
    // end with A's content there (delete B's, then copy A's into the freed path).
    Sandbox::write(&sb.a_root, "clash.txt", b"from-A")?;
    Sandbox::write(&sb.b_root, "clash.txt", b"from-B")?;
    sb.update("A")?;
    sb.update("B")?;

    let stats = diff_sync(
        &sb.store,
        "A",
        "B",
        true,
        SyncDelete::Absent,
        None,
        &DiffRun::new(&NoDiffProgress, &CancellationToken::new()),
    )?;
    assert_eq!(stats.copied, 1);
    assert_eq!(stats.deleted, 1);
    assert_eq!(
        std::fs::read(sb.b_root.join("clash.txt"))?,
        b"from-A",
        "the occupied path ends up holding the source's content"
    );
    let in_b = sb
        .store
        .get_file_entry("B", "clash.txt")?
        .ok_or("clash.txt not in B index")?;
    assert!(!in_b.missing);
    assert_eq!(in_b.hash, *blake3::hash(b"from-A").as_bytes());
    Ok(())
}

#[test]
fn plan_sync_absent_lists_mirror_deletes() -> TestResult {
    let sb = Sandbox::new()?;
    Sandbox::write(&sb.a_root, "new.txt", b"new")?;
    Sandbox::write(&sb.b_root, "extra.txt", b"extra")?;
    Sandbox::write(&sb.b_root, "shared.txt", b"new")?;
    sb.update("A")?;
    sb.update("B")?;

    let plan = plan_sync(&sb.store, "A", "B", true, SyncDelete::Absent, None)?;
    // "new" content already lives in B as shared.txt, so no copy; "extra" has
    // no counterpart in A, so it is the sole mirror delete.
    assert!(plan.copies.is_empty(), "content already present in B");
    assert_eq!(plan.deletes, vec!["extra.txt".to_string()]);
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
        &["B"],
        CopyDest {
            dir: &target_dir,
            subdir: None,
        },
        true,
        None,
        &DiffRun::new(&NoDiffProgress, &CancellationToken::new()),
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
        &["B"],
        CopyDest {
            dir: &target_dir,
            subdir: None,
        },
        false,
        None,
        &DiffRun::new(&NoDiffProgress, &CancellationToken::new()),
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
        &["B"],
        CopyDest {
            dir: &target_dir,
            subdir: Some("imports/batch1"),
        },
        false,
        None,
        &DiffRun::new(&NoDiffProgress, &CancellationToken::new()),
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
        &["B"],
        CopyDest {
            dir: &target_dir,
            subdir: Some("archive"),
        },
        true,
        None,
        &DiffRun::new(&NoDiffProgress, &CancellationToken::new()),
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
        &["B"],
        CopyDest {
            dir: &target_dir,
            subdir: Some("../outside"),
        },
        false,
        None,
        &DiffRun::new(&NoDiffProgress, &CancellationToken::new()),
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

    let mut items = diff_print(&sb.store, "A", &["B"], None)?;
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

    let stats = diff_delete(
        &sb.store,
        "A",
        &["B"],
        None,
        &DiffRun::new(&NoDiffProgress, &CancellationToken::new()),
    )?;
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

/// A `DiffProgress` double that records every event it receives so tests can
/// assert the live counts.
#[derive(Default)]
struct RecordingProgress {
    events: Mutex<Vec<DiffEvent>>,
}

impl RecordingProgress {
    /// The highest `done`/`total` seen across the recorded `Progress` events.
    fn last_progress(&self) -> Option<(u64, u64)> {
        self.events
            .lock()
            .ok()?
            .iter()
            .filter_map(|e| match e {
                DiffEvent::Progress { done, total, .. } => Some((*done, *total)),
                _ => None,
            })
            .next_back()
    }

    fn progress_count(&self) -> usize {
        self.events
            .lock()
            .map(|e| {
                e.iter()
                    .filter(|e| matches!(e, DiffEvent::Progress { .. }))
                    .count()
            })
            .unwrap_or(0)
    }
}

impl DiffProgress for RecordingProgress {
    fn on(&self, event: DiffEvent) {
        if let Ok(mut events) = self.events.lock() {
            events.push(event);
        }
    }
}

#[test]
fn copy_into_target_repo_updates_target_index() -> TestResult {
    let sb = Sandbox::new()?;
    Sandbox::write(&sb.a_root, "dir/a.txt", b"fresh")?;
    sb.update("A")?;

    // Copy straight into B's own data directory so the file lands inside the
    // reference (target) repo and must be indexed there.
    let stats = diff_copy(
        &sb.store,
        "A",
        &["B"],
        CopyDest {
            dir: &sb.b_root,
            subdir: None,
        },
        false,
        None,
        &DiffRun::new(&NoDiffProgress, &CancellationToken::new()),
    )?;
    assert_eq!(stats.copied, 1);
    assert_eq!(std::fs::read(sb.b_root.join("dir/a.txt"))?, b"fresh");

    // The target (B) index now knows the copied file at its relative path.
    let in_b = sb
        .store
        .get_file_entry("B", "dir/a.txt")?
        .ok_or("copied file not in B index")?;
    assert!(!in_b.missing);
    assert_eq!(in_b.hash, *blake3::hash(b"fresh").as_bytes());
    assert_eq!(in_b.size, 5);

    // The indexed mtime matches disk: a follow-up update must not re-add it.
    let update_stats = update_repo(&sb.store, "B", 1, &NoProgress, &CancellationToken::new())?;
    assert_eq!(update_stats.added, 0);

    // Plain copy leaves the source index untouched (still present).
    let in_a = sb
        .store
        .get_file_entry("A", "dir/a.txt")?
        .ok_or("dir/a.txt entry dropped from A")?;
    assert!(!in_a.missing);
    Ok(())
}

#[test]
fn copy_preserves_the_source_file_date() -> TestResult {
    let sb = Sandbox::new()?;
    Sandbox::write_dated(&sb.a_root, "dir/a.txt", b"fresh")?;
    sb.update("A")?;

    diff_copy(
        &sb.store,
        "A",
        &["B"],
        CopyDest {
            dir: &sb.b_root,
            subdir: None,
        },
        false,
        None,
        &DiffRun::new(&NoDiffProgress, &CancellationToken::new()),
    )?;
    assert_eq!(mtime_of(&sb.b_root.join("dir/a.txt"))?, old_mtime());

    // Index and disk agree, so a follow-up update re-hashes nothing.
    let update_stats = update_repo(&sb.store, "B", 1, &NoProgress, &CancellationToken::new())?;
    assert_eq!(update_stats.unchanged, 1);
    assert_eq!(update_stats.added + update_stats.updated, 0);
    Ok(())
}

#[test]
fn move_preserves_the_source_file_date() -> TestResult {
    let sb = Sandbox::new()?;
    Sandbox::write_dated(&sb.a_root, "note.txt", b"moved")?;
    sb.update("A")?;

    diff_copy(
        &sb.store,
        "A",
        &["B"],
        CopyDest {
            dir: &sb.b_root,
            subdir: None,
        },
        true,
        None,
        &DiffRun::new(&NoDiffProgress, &CancellationToken::new()),
    )?;
    assert_eq!(mtime_of(&sb.b_root.join("note.txt"))?, old_mtime());

    let update_stats = update_repo(&sb.store, "B", 1, &NoProgress, &CancellationToken::new())?;
    assert_eq!(update_stats.unchanged, 1);
    assert_eq!(update_stats.added + update_stats.updated, 0);
    Ok(())
}

#[test]
fn sync_preserves_the_source_file_date() -> TestResult {
    let sb = Sandbox::new()?;
    Sandbox::write_dated(&sb.a_root, "dir/a.txt", b"hello")?;
    sb.update("A")?;

    diff_sync(
        &sb.store,
        "A",
        "B",
        true,
        SyncDelete::None,
        None,
        &DiffRun::new(&NoDiffProgress, &CancellationToken::new()),
    )?;
    assert_eq!(mtime_of(&sb.b_root.join("dir/a.txt"))?, old_mtime());

    let update_stats = update_repo(&sb.store, "B", 1, &NoProgress, &CancellationToken::new())?;
    assert_eq!(update_stats.unchanged, 1);
    assert_eq!(update_stats.added + update_stats.updated, 0);
    Ok(())
}

#[test]
fn move_into_target_repo_updates_both_indexes() -> TestResult {
    let sb = Sandbox::new()?;
    Sandbox::write(&sb.a_root, "note.txt", b"moved")?;
    sb.update("A")?;

    let stats = diff_copy(
        &sb.store,
        "A",
        &["B"],
        CopyDest {
            dir: &sb.b_root,
            subdir: None,
        },
        true,
        None,
        &DiffRun::new(&NoDiffProgress, &CancellationToken::new()),
    )?;
    assert_eq!(stats.copied, 1);
    assert!(!sb.a_root.join("note.txt").exists());
    assert_eq!(std::fs::read(sb.b_root.join("note.txt"))?, b"moved");

    // Target index gains the file; source index marks it missing.
    let in_b = sb
        .store
        .get_file_entry("B", "note.txt")?
        .ok_or("moved file not in B index")?;
    assert!(!in_b.missing);
    let in_a = sb
        .store
        .get_file_entry("A", "note.txt")?
        .ok_or("note.txt entry dropped from A")?;
    assert!(in_a.missing);
    Ok(())
}

#[test]
fn copy_reports_progress_counts_matching_stats() -> TestResult {
    let sb = Sandbox::new()?;
    Sandbox::write(&sb.a_root, "one.txt", b"1")?;
    Sandbox::write(&sb.a_root, "two.txt", b"22")?;
    Sandbox::write(&sb.a_root, "three.txt", b"333")?;
    sb.update("A")?;
    let target_dir = sb._tempdir.path().join("progress-copy");

    let progress = RecordingProgress::default();
    let stats = diff_copy(
        &sb.store,
        "A",
        &["B"],
        CopyDest {
            dir: &target_dir,
            subdir: None,
        },
        false,
        None,
        &DiffRun::new(&progress, &CancellationToken::new()),
    )?;
    assert_eq!(stats.copied, 3);
    assert_eq!(progress.progress_count(), 3);
    let (done, total) = progress
        .last_progress()
        .ok_or("no progress events recorded")?;
    assert_eq!(total, 3);
    assert_eq!(done, stats.copied);
    let last_action = progress
        .events
        .lock()
        .map_err(|_| "progress mutex poisoned")?
        .iter()
        .rev()
        .find_map(|e| match e {
            DiffEvent::Progress { action, .. } => Some(*action),
            _ => None,
        })
        .ok_or("no progress action recorded")?;
    assert_eq!(last_action, DiffAction::Copy);
    Ok(())
}

#[test]
fn delete_reports_progress_counts_matching_stats() -> TestResult {
    let sb = Sandbox::new()?;
    Sandbox::write(&sb.a_root, "d1.txt", b"shared1")?;
    Sandbox::write(&sb.a_root, "d2.txt", b"shared2")?;
    Sandbox::write(&sb.b_root, "b1.txt", b"shared1")?;
    Sandbox::write(&sb.b_root, "b2.txt", b"shared2")?;
    sb.update("A")?;
    sb.update("B")?;

    let progress = RecordingProgress::default();
    let stats = diff_delete(
        &sb.store,
        "A",
        &["B"],
        None,
        &DiffRun::new(&progress, &CancellationToken::new()),
    )?;
    assert_eq!(stats.deleted, 2);
    assert_eq!(progress.progress_count(), 2);
    let (done, total) = progress
        .last_progress()
        .ok_or("no progress events recorded")?;
    assert_eq!(total, 2);
    assert_eq!(done, stats.deleted);
    Ok(())
}

#[test]
fn cancelled_delete_leaves_indexes_consistent_with_disk() -> TestResult {
    let sb = Sandbox::new()?;
    Sandbox::write(&sb.a_root, "gone.txt", b"shared")?;
    Sandbox::write(&sb.b_root, "keep.txt", b"shared")?;
    sb.update("A")?;
    sb.update("B")?;

    // A token cancelled up front: the loop breaks before touching any file.
    let cancel = CancellationToken::new();
    cancel.cancel();
    let stats = diff_delete(
        &sb.store,
        "A",
        &["B"],
        None,
        &DiffRun::new(&NoDiffProgress, &cancel),
    )?;
    assert_eq!(stats.deleted, 0);
    assert!(stats.cancelled);
    // Nothing was removed, so the source index still lists the file present.
    assert!(sb.a_root.join("gone.txt").exists());
    let in_a = sb
        .store
        .get_file_entry("A", "gone.txt")?
        .ok_or("gone.txt entry dropped from A")?;
    assert!(!in_a.missing);
    Ok(())
}

/// Multi-reference diff: a file counts as "new" only when *none* of the
/// references has its content. A file unique vs one reference but present in
/// another is not copied.
#[test]
fn copy_treats_union_of_references_as_known() -> TestResult {
    let sb = Sandbox::new()?;
    // A third reference repo C with its own data dir.
    let c_root = sb._tempdir.path().join("Cdata");
    std::fs::create_dir_all(&c_root)?;
    sb.store.create_repo("C", &c_root.to_string_lossy())?;

    // A has two distinct contents; B holds "one", C holds "two".
    Sandbox::write(&sb.a_root, "f1.txt", b"one")?;
    Sandbox::write(&sb.a_root, "f2.txt", b"two")?;
    Sandbox::write(&sb.b_root, "b.txt", b"one")?;
    Sandbox::write(&c_root, "c.txt", b"two")?;
    sb.update("A")?;
    sb.update("B")?;
    sb.update("C")?;

    let target = sb._tempdir.path().join("out");
    std::fs::create_dir_all(&target)?;

    // Against B and C together, both files are already known → nothing copied.
    let stats = diff_copy(
        &sb.store,
        "A",
        &["B", "C"],
        CopyDest {
            dir: &target,
            subdir: None,
        },
        false,
        None,
        &DiffRun::new(&NoDiffProgress, &CancellationToken::new()),
    )?;
    assert_eq!(stats.copied, 0, "both contents known across the references");
    assert!(!target.join("f1.txt").exists());
    assert!(!target.join("f2.txt").exists());

    // Against B alone, "two" (f2) is unique and gets copied; "one" (f1) does not.
    let stats = diff_copy(
        &sb.store,
        "A",
        &["B"],
        CopyDest {
            dir: &target,
            subdir: None,
        },
        false,
        None,
        &DiffRun::new(&NoDiffProgress, &CancellationToken::new()),
    )?;
    assert_eq!(stats.copied, 1, "only content absent from B is copied");
    assert_eq!(std::fs::read(target.join("f2.txt"))?, b"two");
    assert!(!target.join("f1.txt").exists());

    // diff_print agrees: with both refs, nothing is New.
    let items = diff_print(&sb.store, "A", &["B", "C"], None)?;
    let new = items
        .iter()
        .filter(|i| matches!(i, DiffItem::New { .. }))
        .count();
    assert_eq!(new, 0);
    Ok(())
}

/// A file copied into a repo carries provenance: its index entry records the
/// source repo it came from.
#[test]
fn copy_records_origin_in_target_index() -> TestResult {
    let sb = Sandbox::new()?;
    // Copy into B's own data dir so the copy lands inside the B repo and gets
    // indexed there (the GUI's usual case).
    Sandbox::write(&sb.a_root, "photo.txt", b"hello")?;
    sb.update("A")?;

    let stats = diff_copy(
        &sb.store,
        "A",
        &["B"],
        CopyDest {
            dir: &sb.b_root,
            subdir: None,
        },
        false,
        None,
        &DiffRun::new(&NoDiffProgress, &CancellationToken::new()),
    )?;
    assert_eq!(stats.copied, 1);

    let entry = sb
        .store
        .get_file_entry("B", "photo.txt")?
        .ok_or("copied file indexed in B")?;
    assert_eq!(
        entry.origin.as_deref(),
        Some("A"),
        "origin records source repo"
    );
    Ok(())
}

/// Count how many of the given source-relative paths landed under `dest`.
fn landed(dest: &Path, rels: &[&str]) -> u32 {
    rels.iter().filter(|rel| dest.join(rel).exists()).count() as u32
}

#[test]
fn folder_export_uniques_keeps_one_copy_per_content() -> TestResult {
    let sb = Sandbox::new()?;
    // Two byte-identical files (a duplicate group) plus one unique file.
    Sandbox::write(&sb.a_root, "dup1.txt", b"same")?;
    Sandbox::write(&sb.a_root, "sub/dup2.txt", b"same")?;
    Sandbox::write(&sb.a_root, "solo.txt", b"unique")?;
    sb.update("A")?;

    let dest = sb._tempdir.path().join("export");
    let cancel = CancellationToken::new();
    let run = DiffRun::new(&NoDiffProgress, &cancel);
    let stats = export_to_folder(
        &sb.store,
        "A",
        &[],
        &dest,
        FolderMode::Exact,
        false,
        false,
        None,
        &run,
    )?;

    assert_eq!(
        stats.copied, 2,
        "one copy of the duplicated content plus the unique file"
    );
    assert_eq!(
        landed(&dest, &["dup1.txt", "sub/dup2.txt"]),
        1,
        "exactly one of the two duplicate copies is exported"
    );
    assert!(
        dest.join("solo.txt").exists(),
        "the unique file is exported"
    );
    Ok(())
}

#[test]
fn folder_export_inverted_keeps_only_redundant_copies() -> TestResult {
    let sb = Sandbox::new()?;
    Sandbox::write(&sb.a_root, "dup1.txt", b"same")?;
    Sandbox::write(&sb.a_root, "sub/dup2.txt", b"same")?;
    Sandbox::write(&sb.a_root, "solo.txt", b"unique")?;
    sb.update("A")?;

    let dest = sb._tempdir.path().join("export");
    let cancel = CancellationToken::new();
    let run = DiffRun::new(&NoDiffProgress, &cancel);
    let stats = export_to_folder(
        &sb.store,
        "A",
        &[],
        &dest,
        FolderMode::Exact,
        true, // invert → redundant copies only
        false,
        None,
        &run,
    )?;

    assert_eq!(stats.copied, 1, "only the redundant duplicate copy");
    assert_eq!(
        landed(&dest, &["dup1.txt", "sub/dup2.txt"]),
        1,
        "exactly one duplicate copy exported"
    );
    assert!(
        !dest.join("solo.txt").exists(),
        "a unique file is never redundant, so it is not exported"
    );
    Ok(())
}

#[test]
fn folder_export_subtracts_reference_content() -> TestResult {
    let sb = Sandbox::new()?;
    Sandbox::write(&sb.a_root, "shared.txt", b"shared")?;
    Sandbox::write(&sb.a_root, "onlyA.txt", b"onlyA")?;
    Sandbox::write(&sb.b_root, "s.txt", b"shared")?; // B already holds "shared"
    sb.update("A")?;
    sb.update("B")?;

    let dest = sb._tempdir.path().join("export");
    let cancel = CancellationToken::new();
    let run = DiffRun::new(&NoDiffProgress, &cancel);
    let stats = export_to_folder(
        &sb.store,
        "A",
        &["B"],
        &dest,
        FolderMode::Exact,
        false,
        false,
        None,
        &run,
    )?;

    assert_eq!(stats.copied, 1, "content B already has is excluded");
    assert!(dest.join("onlyA.txt").exists());
    assert!(
        !dest.join("shared.txt").exists(),
        "shared content is already in the reference"
    );
    Ok(())
}

#[test]
fn folder_export_move_marks_source_missing() -> TestResult {
    let sb = Sandbox::new()?;
    Sandbox::write(&sb.a_root, "m.txt", b"movable")?;
    sb.update("A")?;

    let dest = sb._tempdir.path().join("export");
    let cancel = CancellationToken::new();
    let run = DiffRun::new(&NoDiffProgress, &cancel);
    let stats = export_to_folder(
        &sb.store,
        "A",
        &[],
        &dest,
        FolderMode::Exact,
        false,
        true, // move
        None,
        &run,
    )?;

    assert_eq!(stats.copied, 1);
    assert!(dest.join("m.txt").exists(), "moved into the export folder");
    assert!(
        !sb.a_root.join("m.txt").exists(),
        "removed from the source directory"
    );
    // The source entry is marked missing, so it is no longer a candidate.
    let again = plan_folder_export(&sb.store, "A", &[], FolderMode::Exact, false, None)?;
    assert!(again.is_empty(), "the moved file is no longer exportable");
    Ok(())
}

// --- Review-board selection (exclude / only) ---------------------------------

/// A rejected review row (exclude set) is skipped by `diff_copy`; the rest of
/// the batch proceeds.
#[test]
fn diff_copy_skips_excluded_rows() -> TestResult {
    let sb = Sandbox::new()?;
    Sandbox::write(&sb.a_root, "keep.txt", b"keep")?;
    Sandbox::write(&sb.a_root, "reject.txt", b"reject")?;
    sb.update("A")?;
    sb.update("B")?;

    let exclude: std::collections::HashSet<String> =
        [dedup_core::diff::source_key("reject.txt")].into();
    let cancel = CancellationToken::new();
    let run = DiffRun::new(&NoDiffProgress, &cancel).with_selection(Some(&exclude), None);
    let stats = diff_copy(
        &sb.store,
        "A",
        &["B"],
        CopyDest {
            dir: &sb.b_root,
            subdir: None,
        },
        false,
        None,
        &run,
    )?;
    assert_eq!(stats.copied, 1, "only the non-rejected file is copied");
    assert!(sb.b_root.join("keep.txt").exists());
    assert!(!sb.b_root.join("reject.txt").exists());
    Ok(())
}

/// An `only` set of one row applies exactly that action — the single-row
/// APPLY is the batch op with a one-element allowlist.
#[test]
fn diff_copy_only_applies_a_single_row() -> TestResult {
    let sb = Sandbox::new()?;
    Sandbox::write(&sb.a_root, "one.txt", b"one")?;
    Sandbox::write(&sb.a_root, "two.txt", b"two")?;
    sb.update("A")?;
    sb.update("B")?;

    let only: std::collections::HashSet<String> = [dedup_core::diff::source_key("two.txt")].into();
    let cancel = CancellationToken::new();
    let run = DiffRun::new(&NoDiffProgress, &cancel).with_selection(None, Some(&only));
    let stats = diff_copy(
        &sb.store,
        "A",
        &["B"],
        CopyDest {
            dir: &sb.b_root,
            subdir: None,
        },
        false,
        None,
        &run,
    )?;
    assert_eq!(stats.copied, 1);
    assert!(sb.b_root.join("two.txt").exists());
    assert!(!sb.b_root.join("one.txt").exists());
    Ok(())
}

/// A mirror's excluded target-side deletion stays on disk and its index entry
/// stays present; the selected deletion goes through.
#[test]
fn mirror_delete_respects_target_side_exclusion() -> TestResult {
    let sb = Sandbox::new()?;
    Sandbox::write(&sb.a_root, "common.txt", b"common")?;
    Sandbox::write(&sb.b_root, "common.txt", b"common")?;
    Sandbox::write(&sb.b_root, "extra1.txt", b"extra1")?;
    Sandbox::write(&sb.b_root, "extra2.txt", b"extra2")?;
    sb.update("A")?;
    sb.update("B")?;

    let exclude: std::collections::HashSet<String> =
        [dedup_core::diff::target_key("extra1.txt")].into();
    let cancel = CancellationToken::new();
    let run = DiffRun::new(&NoDiffProgress, &cancel).with_selection(Some(&exclude), None);
    let stats = diff_sync(&sb.store, "A", "B", true, SyncDelete::Absent, None, &run)?;
    assert_eq!(stats.deleted, 1, "only the non-rejected extra is deleted");
    assert!(sb.b_root.join("extra1.txt").exists(), "rejected row kept");
    assert!(!sb.b_root.join("extra2.txt").exists());
    assert!(
        !sb.store.get_file_entry("B", "extra1.txt")?.unwrap().missing,
        "the kept file's index entry stays present"
    );
    Ok(())
}

/// `organize_apply` with an `only` allowlist moves exactly that file.
#[test]
fn organize_only_moves_a_single_row() -> TestResult {
    let sb = Sandbox::new()?;
    Sandbox::write(&sb.a_root, "a.txt", b"a")?;
    Sandbox::write(&sb.a_root, "b.txt", b"b")?;
    sb.update("A")?;

    let rules = [dedup_core::organize::OrganizeRule {
        filter: None,
        template: "moved/{o-name}".into(),
    }];
    let only: std::collections::HashSet<String> = [dedup_core::diff::source_key("b.txt")].into();
    let cancel = CancellationToken::new();
    let run = DiffRun::new(&NoDiffProgress, &cancel).with_selection(None, Some(&only));
    let stats = dedup_core::organize::organize_apply(&sb.store, "A", &rules, &run)?;
    assert_eq!(stats.moved, 1);
    assert!(sb.a_root.join("moved/b.txt").exists());
    assert!(
        sb.a_root.join("a.txt").exists(),
        "unselected file untouched"
    );
    Ok(())
}
