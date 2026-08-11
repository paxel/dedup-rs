//! Integration tests for the manual two-repo diff (`plan_repo_diff`) that
//! backs the Transfer tab's DIFF command: real files, real indexes, both
//! pairings.

use dedup_core::diff::{
    DiffOpError, DiffPairing, DiffRelation, PullKind, RepoDiffRow, copy_file_between, delete_file,
    overwrite_file, plan_repo_diff, plan_sync_back, rename_file,
};
use dedup_core::store::Store;
use dedup_core::update::{CancellationToken, NoProgress, update_repo};
use std::path::{Path, PathBuf};

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct Sandbox {
    _tempdir: tempfile::TempDir,
    store: Store,
    left: PathBuf,
    right: PathBuf,
}

impl Sandbox {
    /// Two registered repos LEFT and RIGHT with real data directories.
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let tempdir = tempfile::tempdir()?;
        let store = Store::open_at(tempdir.path().join("config"))?;
        let left = tempdir.path().join("left");
        let right = tempdir.path().join("right");
        std::fs::create_dir_all(&left)?;
        std::fs::create_dir_all(&right)?;
        store.create_repo("LEFT", &left.to_string_lossy())?;
        store.create_repo("RIGHT", &right.to_string_lossy())?;
        Ok(Self {
            _tempdir: tempdir,
            store,
            left,
            right,
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

    /// Scan both repos so the indexes match what is on disk.
    fn update_both(&self) -> TestResult {
        for name in ["LEFT", "RIGHT"] {
            update_repo(&self.store, name, 1, &NoProgress, &CancellationToken::new())?;
        }
        Ok(())
    }

    fn diff(&self, pairing: DiffPairing) -> Result<Vec<RepoDiffRow>, Box<dyn std::error::Error>> {
        Ok(plan_repo_diff(&self.store, "LEFT", "RIGHT", pairing)?)
    }
}

/// The paths one side of a row offers, for terse assertions.
fn paths(files: &[dedup_core::diff::DiffFile]) -> Vec<&str> {
    files.iter().map(|f| f.rel_path.as_str()).collect()
}

#[test]
fn by_hash_pairs_content_regardless_of_path() -> TestResult {
    let sb = Sandbox::new()?;
    // Same content, same name → equal. Same content, other name → rename.
    // Content only one side has → one-sided.
    Sandbox::write(&sb.left, "same.txt", b"same")?;
    Sandbox::write(&sb.right, "same.txt", b"same")?;
    Sandbox::write(&sb.left, "old-name.txt", b"renamed")?;
    Sandbox::write(&sb.right, "new-name.txt", b"renamed")?;
    Sandbox::write(&sb.left, "only-left.txt", b"left")?;
    Sandbox::write(&sb.right, "only-right.txt", b"right")?;
    sb.update_both()?;

    let rows = sb.diff(DiffPairing::ByHash)?;
    assert_eq!(rows.len(), 4, "one row per content key: {rows:#?}");

    let equal = rows
        .iter()
        .find(|r| r.relation == DiffRelation::Equal)
        .ok_or("no equal row")?;
    assert_eq!(paths(&equal.left), ["same.txt"]);
    assert_eq!(paths(&equal.right), ["same.txt"]);

    let renamed = rows
        .iter()
        .find(|r| r.relation == DiffRelation::Renamed)
        .ok_or("no renamed row")?;
    assert_eq!(paths(&renamed.left), ["old-name.txt"]);
    assert_eq!(paths(&renamed.right), ["new-name.txt"]);

    let only_left = rows
        .iter()
        .find(|r| r.relation == DiffRelation::OnlyLeft)
        .ok_or("no left-only row")?;
    assert_eq!(paths(&only_left.left), ["only-left.txt"]);
    assert!(only_left.right.is_empty());

    let only_right = rows
        .iter()
        .find(|r| r.relation == DiffRelation::OnlyRight)
        .ok_or("no right-only row")?;
    assert!(only_right.left.is_empty());
    assert_eq!(paths(&only_right.right), ["only-right.txt"]);
    Ok(())
}

#[test]
fn by_hash_lists_every_duplicate_path_per_side() -> TestResult {
    let sb = Sandbox::new()?;
    // The left side holds the same content three times, the right side once
    // under a different name: one row the UI narrows down step by step.
    Sandbox::write(&sb.left, "a.txt", b"dupe")?;
    Sandbox::write(&sb.left, "b.txt", b"dupe")?;
    Sandbox::write(&sb.left, "sub/c.txt", b"dupe")?;
    Sandbox::write(&sb.right, "z.txt", b"dupe")?;
    sb.update_both()?;

    let rows = sb.diff(DiffPairing::ByHash)?;
    assert_eq!(rows.len(), 1, "one content key, one row");
    assert_eq!(rows[0].relation, DiffRelation::Renamed);
    assert_eq!(
        paths(&rows[0].left),
        ["a.txt", "b.txt", "sub/c.txt"],
        "every path the left side holds, sorted"
    );
    assert_eq!(paths(&rows[0].right), ["z.txt"]);
    Ok(())
}

#[test]
fn by_path_pairs_names_and_flags_differing_content() -> TestResult {
    let sb = Sandbox::new()?;
    Sandbox::write(&sb.left, "same.txt", b"same")?;
    Sandbox::write(&sb.right, "same.txt", b"same")?;
    Sandbox::write(&sb.left, "clash.txt", b"left version")?;
    Sandbox::write(&sb.right, "clash.txt", b"right version")?;
    Sandbox::write(&sb.left, "only-left.txt", b"left")?;
    sb.update_both()?;

    let rows = sb.diff(DiffPairing::ByPath)?;
    assert_eq!(rows.len(), 3, "one row per path: {rows:#?}");
    assert_eq!(
        rows.iter().map(|r| r.relation).collect::<Vec<_>>(),
        [
            DiffRelation::Conflict, // clash.txt
            DiffRelation::OnlyLeft, // only-left.txt
            DiffRelation::Equal,    // same.txt
        ],
        "rows come back ordered by path"
    );
    let conflict = &rows[0];
    assert_eq!(paths(&conflict.left), ["clash.txt"]);
    assert_eq!(paths(&conflict.right), ["clash.txt"]);
    assert_ne!(
        conflict.left[0].size, conflict.right[0].size,
        "the two sides really are different files"
    );
    Ok(())
}

#[test]
fn by_path_treats_a_rename_as_two_one_sided_rows() -> TestResult {
    let sb = Sandbox::new()?;
    // The same content under two names is one row by hash, but two rows by
    // path — that is the whole point of offering both pairings.
    Sandbox::write(&sb.left, "old-name.txt", b"renamed")?;
    Sandbox::write(&sb.right, "new-name.txt", b"renamed")?;
    sb.update_both()?;

    assert_eq!(sb.diff(DiffPairing::ByHash)?.len(), 1, "one row by hash");
    let rows = sb.diff(DiffPairing::ByPath)?;
    assert_eq!(
        rows.iter().map(|r| r.relation).collect::<Vec<_>>(),
        [DiffRelation::OnlyRight, DiffRelation::OnlyLeft],
        "new-name.txt then old-name.txt, each one-sided"
    );
    Ok(())
}

#[test]
fn deleted_files_drop_out_of_the_diff() -> TestResult {
    let sb = Sandbox::new()?;
    Sandbox::write(&sb.left, "gone.txt", b"gone")?;
    Sandbox::write(&sb.left, "stays.txt", b"stays")?;
    sb.update_both()?;
    // Remove the file and rescan: its entry is now missing, and a diff
    // describes what is on disk, not what once was.
    std::fs::remove_file(sb.left.join("gone.txt"))?;
    sb.update_both()?;

    for pairing in [DiffPairing::ByHash, DiffPairing::ByPath] {
        let rows = sb.diff(pairing)?;
        assert_eq!(rows.len(), 1, "{pairing:?}: only the live file shows");
        assert_eq!(paths(&rows[0].left), ["stays.txt"]);
    }
    Ok(())
}

/// An only-on-one-side row whose content the *other* repo once held and
/// deleted is flagged — the DIFF board paints it blue WAS DELETED instead of
/// green NEW, because copying it across would resurrect a deletion.
#[test]
fn one_sided_rows_flag_content_the_other_side_deleted() -> TestResult {
    let sb = Sandbox::new()?;
    Sandbox::write(&sb.left, "kept-only-here.txt", b"never-on-right")?;
    Sandbox::write(&sb.left, "they-deleted-it.txt", b"tombstoned-content")?;
    Sandbox::write(&sb.right, "they-deleted-it.txt", b"tombstoned-content")?;
    // A survivor on the right, so the rescan after the deletion doesn't trip
    // the "would empty the index" unmounted-drive guard.
    Sandbox::write(&sb.right, "survivor.txt", b"still-here")?;
    sb.update_both()?;
    // The right side deletes its copy and rescans: a tombstone remains.
    std::fs::remove_file(sb.right.join("they-deleted-it.txt"))?;
    sb.update_both()?;

    for pairing in [DiffPairing::ByHash, DiffPairing::ByPath] {
        let rows = sb.diff(pairing)?;
        let row = |name: &str| {
            rows.iter()
                .find(|r| paths(&r.left) == [name])
                .unwrap_or_else(|| panic!("{pairing:?}: no row for {name}"))
        };
        let fresh = row("kept-only-here.txt");
        assert_eq!(fresh.relation, DiffRelation::OnlyLeft);
        assert!(
            !fresh.deleted_in_right,
            "{pairing:?}: content the right never had is plain only-left"
        );
        let ghost = row("they-deleted-it.txt");
        assert_eq!(ghost.relation, DiffRelation::OnlyLeft);
        assert!(
            ghost.deleted_in_right,
            "{pairing:?}: the right holds a tombstone of this content"
        );
    }
    Ok(())
}

#[test]
fn plan_sync_back_separates_new_from_resurrection() -> TestResult {
    // LEFT is the sink, RIGHT is the main.
    let sb = Sandbox::new()?;
    // A file both hold; a file the main once had and will delete (its content
    // lingers on the sink); a file only the sink has.
    Sandbox::write(&sb.right, "shared.txt", b"shared")?;
    Sandbox::write(&sb.right, "deleted-in-main.txt", b"deleted-content")?;
    Sandbox::write(&sb.left, "shared.txt", b"shared")?;
    Sandbox::write(&sb.left, "deleted-in-main.txt", b"deleted-content")?;
    Sandbox::write(&sb.left, "new-on-sink.txt", b"brand-new")?;
    sb.update_both()?;
    // The main deletes its copy and rescans → a tombstone for that content,
    // which the sink still holds.
    std::fs::remove_file(sb.right.join("deleted-in-main.txt"))?;
    update_repo(
        &sb.store,
        "RIGHT",
        1,
        &NoProgress,
        &CancellationToken::new(),
    )?;

    let plan = plan_sync_back(&sb.store, "LEFT", "RIGHT", None)?;
    let by_path: std::collections::HashMap<&str, PullKind> =
        plan.iter().map(|i| (i.rel_path.as_str(), i.kind)).collect();

    assert_eq!(
        by_path.get("new-on-sink.txt"),
        Some(&PullKind::New),
        "content the main never saw is New (promote)"
    );
    assert_eq!(
        by_path.get("deleted-in-main.txt"),
        Some(&PullKind::Resurrection),
        "content the main deleted but the sink still holds is a Resurrection"
    );
    assert!(
        !by_path.contains_key("shared.txt"),
        "content the main already has is omitted — nothing to pull"
    );
    Ok(())
}

#[test]
fn two_empty_repos_diff_to_nothing() -> TestResult {
    let sb = Sandbox::new()?;
    sb.update_both()?;
    assert!(sb.diff(DiffPairing::ByHash)?.is_empty());
    assert!(sb.diff(DiffPairing::ByPath)?.is_empty());
    Ok(())
}

// --- row actions -----------------------------------------------------------

/// A repo's live (non-missing) relative paths, sorted.
fn live_paths(store: &Store, repo: &str) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let mut paths: Vec<String> = Vec::new();
    let db = store.open_repo_db(repo)?;
    dedup_core::store::for_each_file_entry(&db, |rel, entry| {
        if !entry.missing {
            paths.push(rel.to_string());
        }
        Ok(())
    })?;
    paths.sort();
    Ok(paths)
}

#[test]
fn rename_moves_the_file_and_its_index_entry() -> TestResult {
    let sb = Sandbox::new()?;
    Sandbox::write(&sb.left, "old-name.txt", b"content")?;
    sb.update_both()?;

    rename_file(&sb.store, "LEFT", "old-name.txt", "sub/new-name.txt")?;

    assert!(!sb.left.join("old-name.txt").exists(), "old name is gone");
    assert_eq!(std::fs::read(sb.left.join("sub/new-name.txt"))?, b"content");
    assert_eq!(live_paths(&sb.store, "LEFT")?, ["sub/new-name.txt"]);
    let entry = sb
        .store
        .get_file_entry("LEFT", "sub/new-name.txt")?
        .ok_or("renamed file not in index")?;
    assert_eq!(entry.hash, *blake3::hash(b"content").as_bytes());
    assert!(
        sb.store.get_file_entry("LEFT", "old-name.txt")?.is_none(),
        "the old path is dropped, not left behind as missing"
    );
    // A follow-up scan finds nothing to do: index and disk agree.
    let stats = update_repo(&sb.store, "LEFT", 1, &NoProgress, &CancellationToken::new())?;
    assert_eq!((stats.unchanged, stats.added, stats.updated), (1, 0, 0));
    Ok(())
}

/// A rename must leave the content indexed under exactly one path — never both.
/// If the insert-new and drop-old writes were separate transactions, a crash
/// between them would list the same (size, hash) twice: a phantom duplicate and
/// inflated counts. Here we assert the post-condition the single transaction
/// guarantees.
#[test]
fn rename_leaves_the_content_under_one_path_only() -> TestResult {
    let sb = Sandbox::new()?;
    Sandbox::write(&sb.left, "old.txt", b"content")?;
    sb.update_both()?;
    let before = sb.store.get_repo_stats("LEFT")?;

    rename_file(&sb.store, "LEFT", "old.txt", "new.txt")?;

    let after = sb.store.get_repo_stats("LEFT")?;
    assert_eq!(
        (after.file_count, after.total_size),
        (before.file_count, before.total_size),
        "a rename changes neither the file count nor the total size"
    );
    assert_eq!(after.file_count, 1, "still exactly one file");
    // The content-hash index lists the new path and only the new path, so the
    // single copy is never reported as a duplicate group.
    assert!(
        sb.store.get_duplicate_groups("LEFT")?.is_empty(),
        "one path per content: no phantom duplicate group"
    );
    assert_eq!(live_paths(&sb.store, "LEFT")?, ["new.txt"]);
    Ok(())
}

#[test]
fn rename_refuses_to_overwrite_an_existing_name() -> TestResult {
    let sb = Sandbox::new()?;
    Sandbox::write(&sb.left, "a.txt", b"a")?;
    Sandbox::write(&sb.left, "b.txt", b"b")?;
    sb.update_both()?;

    let err = rename_file(&sb.store, "LEFT", "a.txt", "b.txt").unwrap_err();
    assert!(
        matches!(err, DiffOpError::AlreadyExists { .. }),
        "got {err:?}"
    );
    assert_eq!(
        std::fs::read(sb.left.join("b.txt"))?,
        b"b",
        "b is untouched"
    );
    assert_eq!(live_paths(&sb.store, "LEFT")?, ["a.txt", "b.txt"]);
    Ok(())
}

#[test]
fn copy_lands_in_the_other_repo_and_its_index() -> TestResult {
    let sb = Sandbox::new()?;
    Sandbox::write(&sb.left, "photo.jpg", b"pixels")?;
    sb.update_both()?;

    copy_file_between(&sb.store, "LEFT", "photo.jpg", "RIGHT", "photo.jpg")?;

    assert_eq!(std::fs::read(sb.right.join("photo.jpg"))?, b"pixels");
    let entry = sb
        .store
        .get_file_entry("RIGHT", "photo.jpg")?
        .ok_or("copy not indexed in RIGHT")?;
    assert!(!entry.missing);
    assert_eq!(entry.hash, *blake3::hash(b"pixels").as_bytes());
    assert_eq!(entry.origin.as_deref(), Some("LEFT"), "provenance recorded");
    assert_eq!(
        std::fs::metadata(sb.right.join("photo.jpg"))?.modified()?,
        std::fs::metadata(sb.left.join("photo.jpg"))?.modified()?,
        "the copy keeps the source's date"
    );
    // Both sides now hold the same content at the same path: an equal row.
    let rows = sb.diff(DiffPairing::ByHash)?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].relation, DiffRelation::Equal);
    Ok(())
}

#[test]
fn copy_refuses_an_occupied_path_but_overwrite_replaces_it() -> TestResult {
    let sb = Sandbox::new()?;
    Sandbox::write(&sb.left, "notes.txt", b"the good version")?;
    Sandbox::write(&sb.right, "notes.txt", b"stale")?;
    sb.update_both()?;

    let err = copy_file_between(&sb.store, "LEFT", "notes.txt", "RIGHT", "notes.txt").unwrap_err();
    assert!(
        matches!(err, DiffOpError::AlreadyExists { .. }),
        "got {err:?}"
    );
    assert_eq!(std::fs::read(sb.right.join("notes.txt"))?, b"stale");

    overwrite_file(&sb.store, "LEFT", "notes.txt", "RIGHT", "notes.txt")?;
    assert_eq!(
        std::fs::read(sb.right.join("notes.txt"))?,
        b"the good version"
    );
    let entry = sb
        .store
        .get_file_entry("RIGHT", "notes.txt")?
        .ok_or("overwritten file not in index")?;
    assert_eq!(entry.hash, *blake3::hash(b"the good version").as_bytes());
    // The conflict is resolved: by path, the row is equal now.
    let rows = sb.diff(DiffPairing::ByPath)?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].relation, DiffRelation::Equal);
    Ok(())
}

/// An overwrite whose source and destination are the same file must not touch
/// it: `fs::copy` onto oneself truncates the file it is about to read. Nothing
/// in the GUI can ask for this, but the API must survive a caller that does.
#[test]
fn overwrite_onto_the_same_file_leaves_it_intact() -> TestResult {
    let sb = Sandbox::new()?;
    Sandbox::write(&sb.left, "notes.txt", b"irreplaceable")?;
    sb.update_both()?;

    overwrite_file(&sb.store, "LEFT", "notes.txt", "LEFT", "notes.txt")?;
    assert_eq!(std::fs::read(sb.left.join("notes.txt"))?, b"irreplaceable");

    // A same-name rename is equally a no-op — the entry must survive it.
    rename_file(&sb.store, "LEFT", "notes.txt", "notes.txt")?;
    assert_eq!(live_paths(&sb.store, "LEFT")?, ["notes.txt"]);
    Ok(())
}

#[test]
fn delete_removes_the_file_and_marks_the_entry_missing() -> TestResult {
    let sb = Sandbox::new()?;
    Sandbox::write(&sb.left, "junk.tmp", b"junk")?;
    Sandbox::write(&sb.left, "keep.txt", b"keep")?;
    sb.update_both()?;

    delete_file(&sb.store, "LEFT", "junk.tmp")?;

    assert!(!sb.left.join("junk.tmp").exists());
    assert_eq!(live_paths(&sb.store, "LEFT")?, ["keep.txt"]);
    let entry = sb
        .store
        .get_file_entry("LEFT", "junk.tmp")?
        .ok_or("deleted entry should still be known, as missing")?;
    assert!(entry.missing);
    assert_eq!(sb.diff(DiffPairing::ByHash)?.len(), 1, "one live file left");
    Ok(())
}

#[test]
fn row_actions_reject_unknown_files_and_escaping_paths() -> TestResult {
    let sb = Sandbox::new()?;
    Sandbox::write(&sb.left, "here.txt", b"here")?;
    sb.update_both()?;

    assert!(matches!(
        rename_file(&sb.store, "LEFT", "nope.txt", "other.txt").unwrap_err(),
        DiffOpError::NoSuchFile { .. }
    ));
    assert!(matches!(
        delete_file(&sb.store, "LEFT", "nope.txt").unwrap_err(),
        DiffOpError::NoSuchFile { .. }
    ));
    assert!(matches!(
        copy_file_between(&sb.store, "LEFT", "here.txt", "RIGHT", "../escaped.txt").unwrap_err(),
        DiffOpError::InvalidPath { .. }
    ));
    assert!(matches!(
        rename_file(&sb.store, "LEFT", "here.txt", "/tmp/escaped.txt").unwrap_err(),
        DiffOpError::InvalidPath { .. }
    ));
    assert!(
        sb.left.join("here.txt").exists(),
        "nothing was touched by the rejected calls"
    );
    Ok(())
}
