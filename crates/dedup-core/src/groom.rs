//! Grooming operations that tidy a single repository in place: bulk-delete
//! files matching a filter, and remove empty directories. Unlike [`crate::diff`]
//! these compare nothing between repos — they act on one repo's own contents.
//!
//! Both reuse the diff crate's progress/cancellation plumbing
//! ([`DiffRun`], [`DiffEvent`], [`DeleteStats`], [`DiffError`]) so the GUI can
//! drive them exactly like a copy/move/delete.

use crate::diff::{DiffAction, DiffError, DiffEvent, DiffRun};
use crate::filter::FileFilter;
use crate::store::{self, FileEntry, Store};
use std::path::PathBuf;

pub use crate::diff::DeleteStats;

/// Flush the mark-missing buffer this often, mirroring the diff pipeline.
const INDEX_BATCH: u64 = 200;

/// Delete every non-missing file in `repo` that matches `filter`, removing it
/// from disk and marking its index entry missing. Unlike a diff delete this
/// takes no reference repo — it deletes *all* matches — so callers must confirm
/// first. Index updates are batched (with a final flush applied even on cancel
/// or failure) and per-file progress is reported through `run`.
pub fn delete_by_filter(
    store: &Store,
    repo: &str,
    filter: Option<&str>,
    run: &DiffRun<'_>,
) -> Result<DeleteStats, DiffError> {
    let filter = FileFilter::parse(filter)?;
    let meta = store.get_repo(repo)?;
    let db = store.open_repo_db(repo)?;
    let root = PathBuf::from(&meta.abs_path);

    // Collect first so the read transaction isn't held during file I/O.
    let annotated = crate::filter::AnnotatedFilter::new(&db, &filter)?;
    let mut candidates: Vec<String> = Vec::new();
    store::for_each_file_entry(&db, |rel_path, entry: FileEntry| {
        if !entry.missing && annotated.matches(rel_path, &entry) {
            candidates.push(rel_path.to_string());
        }
        Ok(())
    })?;

    let total = candidates.len() as u64;
    let mut stats = DeleteStats::default();
    let mut deleted: Vec<String> = Vec::new();
    let mut since_flush = 0u64;
    let mut failure: Option<DiffError> = None;

    for rel_path in &candidates {
        if run.cancel.is_cancelled() {
            stats.cancelled = true;
            break;
        }
        let path = root.join(rel_path);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            // Already gone: just record it as missing.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                run.progress.on(DiffEvent::Error {
                    path: path.to_string_lossy().into_owned(),
                    message: err.to_string(),
                });
                failure = Some(DiffError::Io {
                    action: "delete",
                    path,
                    source: err,
                });
                break;
            }
        }
        deleted.push(rel_path.clone());
        stats.deleted += 1;
        since_flush += 1;
        run.progress.on(DiffEvent::Progress {
            action: DiffAction::Delete,
            done: stats.deleted,
            total,
            rel_path: rel_path.clone(),
        });
        if since_flush >= INDEX_BATCH {
            store::mark_missing(&db, deleted.iter().map(String::as_str))?;
            deleted.clear();
            since_flush = 0;
        }
    }

    // Files already removed from disk must be marked missing, even on cancel or
    // failure.
    if !deleted.is_empty() {
        store::mark_missing(&db, deleted.iter().map(String::as_str))?;
    }

    match failure {
        Some(err) => Err(err),
        None => Ok(stats),
    }
}

/// The non-missing relative paths in `repo` that match `filter`: the first
/// `limit` of them (for a preview list) plus the total match count. Streams the
/// index without deleting anything.
pub fn preview_by_filter(
    store: &Store,
    repo: &str,
    filter: Option<&str>,
    limit: usize,
) -> Result<(Vec<String>, usize), DiffError> {
    let filter = FileFilter::parse(filter)?;
    let db = store.open_repo_db(repo)?;
    let annotated = crate::filter::AnnotatedFilter::new(&db, &filter)?;
    let mut sample: Vec<String> = Vec::new();
    let mut total = 0usize;
    store::for_each_file_entry(&db, |rel_path, entry: FileEntry| {
        if !entry.missing && annotated.matches(rel_path, &entry) {
            total += 1;
            if sample.len() < limit {
                sample.push(rel_path.to_string());
            }
        }
        Ok(())
    })?;
    Ok((sample, total))
}

/// Outcome of a [`prune`] run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneStats {
    /// Missing (deleted-from-disk) index records dropped from the database.
    pub pruned: u64,
    /// Whether the follow-up compaction actually reclaimed disk space (redb
    /// reports `false` when there was nothing to reclaim).
    pub compacted: bool,
    /// The run was cancelled before every tombstone was removed.
    pub cancelled: bool,
}

/// The first `limit` missing (deleted-from-disk) record paths in `repo`, plus
/// the total count of them — the records [`prune`] would drop. Streams the index
/// without changing anything.
pub fn preview_prune(
    store: &Store,
    repo: &str,
    limit: usize,
) -> Result<(Vec<String>, usize), DiffError> {
    let db = store.open_repo_db(repo)?;
    let mut sample: Vec<String> = Vec::new();
    let mut total = 0usize;
    store::for_each_file_entry(&db, |rel_path, entry: FileEntry| {
        if entry.missing {
            total += 1;
            if sample.len() < limit {
                sample.push(rel_path.to_string());
            }
        }
        Ok(())
    })?;
    Ok((sample, total))
}

/// Drop every *missing* index record (a tombstone left by [`delete_by_filter`],
/// a diff delete, or a scan that saw the file vanish) from `repo`'s database,
/// then compact the file to reclaim the freed space. Missing records carry the
/// "content was here, now gone" signal the mirror/diff planner reads, so this is
/// destructive of that history and callers must confirm first. Removals are
/// batched and cancellable (with a final flush applied even on cancel);
/// compaction runs only on a full, uncancelled pass. Per-record progress is
/// reported through `run`.
pub fn prune(store: &Store, repo: &str, run: &DiffRun<'_>) -> Result<PruneStats, DiffError> {
    let db = store.open_repo_db(repo)?;

    // Collect first so the read transaction isn't held during the removals.
    let mut tombstones: Vec<String> = Vec::new();
    store::for_each_file_entry(&db, |rel_path, entry: FileEntry| {
        if entry.missing {
            tombstones.push(rel_path.to_string());
        }
        Ok(())
    })?;

    let total = tombstones.len() as u64;
    let mut stats = PruneStats::default();
    let mut batch: Vec<String> = Vec::new();

    for rel_path in &tombstones {
        if run.cancel.is_cancelled() {
            stats.cancelled = true;
            break;
        }
        batch.push(rel_path.clone());
        stats.pruned += 1;
        run.progress.on(DiffEvent::Progress {
            action: DiffAction::Delete,
            done: stats.pruned,
            total,
            rel_path: rel_path.clone(),
        });
        if batch.len() as u64 >= INDEX_BATCH {
            store::remove_entries(&db, batch.iter().map(String::as_str))?;
            batch.clear();
        }
    }
    if !batch.is_empty() {
        store::remove_entries(&db, batch.iter().map(String::as_str))?;
    }

    // Release the shared handle before compaction, which needs exclusive access.
    drop(db);
    if !stats.cancelled {
        stats.compacted = store.compact_repo(repo)?;
    }
    Ok(stats)
}

/// Remove every empty directory under `repo`'s root (bottom-up, so a directory
/// left empty only after its empty children are removed is also pruned). The
/// repo root itself is never removed. Returns the number of directories
/// deleted. The index is untouched — directories are not indexed.
pub fn delete_empty_dirs(store: &Store, repo: &str) -> Result<u64, DiffError> {
    let meta = store.get_repo(repo)?;
    let root = PathBuf::from(&meta.abs_path);

    // Deepest paths first so a parent is visited only after its children have
    // had their chance to be removed.
    let mut dirs: Vec<PathBuf> = walkdir::WalkDir::new(&root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_dir() && e.path() != root)
        .map(|e| e.into_path())
        .collect();
    dirs.sort_by_key(|p| std::cmp::Reverse(p.components().count()));

    let mut removed = 0u64;
    for dir in dirs {
        // Only remove a directory that is actually empty now; ignore errors
        // (a non-empty dir errors, which is exactly what we want to skip).
        let is_empty = std::fs::read_dir(&dir)
            .map(|mut it| it.next().is_none())
            .unwrap_or(false);
        if is_empty && std::fs::remove_dir(&dir).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}
