//! Diff operations between a source repo and a reference repo, ported from
//! the legacy `DiffProcess`. Two files are considered the same content when
//! (size, hash) match — paths never matter for existence checks.
//!
//! Operations:
//! - [`diff_print`]: classify each source file as new / equal / deleted-in-reference.
//! - [`diff_copy`]: copy (or move) files whose content the reference has never
//!   seen into a target directory; a move marks the source entries missing.
//! - [`diff_delete`]: delete source files whose content the reference knows
//!   (present or missing) and mark them missing in the source index.
//! - [`diff_sync`]: sync source into target — copy new content (never
//!   overwriting an occupied path), optionally delete target content the
//!   source marks missing; best effort, errors are counted.

use crate::filter::{FileFilter, FilterError};
use crate::store::{self, ContentKey, ContentState, FileEntry, RepoMeta, Store, StoreError};
use crate::update::CancellationToken;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(thiserror::Error, Debug)]
pub enum DiffError {
    #[error(transparent)]
    Store(#[from] StoreError),

    #[error(transparent)]
    Filter(#[from] FilterError),

    #[error("Could not {action} '{path}': {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("invalid target subdirectory: {subdir}")]
    InvalidSubdir { subdir: String },
}

/// Where a copy/move should place files: a target directory and an optional
/// relative `subdir` inside it. Files keep their source-relative path under
/// `dir`/`subdir`.
#[derive(Debug, Clone, Copy)]
pub struct CopyDest<'a> {
    pub dir: &'a Path,
    pub subdir: Option<&'a str>,
}

/// Resolve the destination root for a copy/move: `target_dir` optionally
/// prefixed by a relative `subdir` inside it. An empty/blank subdir means the
/// target root itself. A subdir that would escape the target root (absolute
/// components, a prefix/root, or any `..`) is rejected as [`DiffError::InvalidSubdir`].
fn resolve_subdir(target_dir: &Path, subdir: Option<&str>) -> Result<PathBuf, DiffError> {
    let raw = subdir.unwrap_or("").trim();
    if raw.is_empty() {
        return Ok(target_dir.to_path_buf());
    }
    let rel = Path::new(raw);
    for component in rel.components() {
        match component {
            std::path::Component::Normal(_) | std::path::Component::CurDir => {}
            _ => {
                return Err(DiffError::InvalidSubdir {
                    subdir: raw.to_string(),
                });
            }
        }
    }
    Ok(target_dir.join(rel))
}

/// Flush accumulated index changes to disk every this many processed files,
/// so both repo indexes stay close to real time without a redb write
/// transaction per file (mirrors the scan pipeline's batching).
const INDEX_BATCH: u64 = 200;

/// The kind of file operation a [`DiffEvent`] refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffAction {
    Copy,
    Move,
    Delete,
}

/// A live progress event emitted by [`diff_copy`] / [`diff_delete`] while they
/// run, so a caller (e.g. the GUI) can render the current file, a running
/// count and a last-N action log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffEvent {
    /// A file has just been processed. `done` is the number of files handled so
    /// far, `total` the size of the candidate set, `rel_path` the file touched.
    Progress {
        action: DiffAction,
        done: u64,
        total: u64,
        rel_path: String,
    },
    /// A file could not be processed; the operation is about to stop.
    Error { path: String, message: String },
}

/// Callback trait used by the diff operations to report per-file progress
/// across the crate boundary (the core crate must not depend on the GUI/CLI).
pub trait DiffProgress: Send + Sync {
    fn on(&self, event: DiffEvent);
}

/// A no-op [`DiffProgress`] for callers that do not render live progress
/// (e.g. the CLI).
pub struct NoDiffProgress;

impl DiffProgress for NoDiffProgress {
    fn on(&self, _event: DiffEvent) {}
}

/// The execution context shared by the mutating diff operations: where to
/// report live progress and how to observe cancellation. Grouping the two
/// keeps the operation signatures compact.
#[derive(Clone, Copy)]
pub struct DiffRun<'a> {
    pub progress: &'a dyn DiffProgress,
    pub cancel: &'a CancellationToken,
}

impl<'a> DiffRun<'a> {
    pub fn new(progress: &'a dyn DiffProgress, cancel: &'a CancellationToken) -> Self {
        Self { progress, cancel }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffItem {
    /// Content exists in source but the reference has never seen it.
    New { rel_path: String },
    /// Content is present in the reference (at any path).
    Equal {
        rel_path: String,
        reference_path: String,
    },
    /// The reference knew this content but all its entries are missing.
    DeletedInReference { rel_path: String },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CopyStats {
    pub copied: u64,
    pub cancelled: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeleteStats {
    pub deleted: u64,
    pub cancelled: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncStats {
    /// Files copied to the target.
    pub copied: u64,
    /// Source files whose content the target already has.
    pub equal: u64,
    /// Copies skipped because the target path is occupied by other content.
    pub skipped: u64,
    /// Target files deleted because the source marks their content missing.
    pub deleted: u64,
    /// Errors encountered (best effort: the sync continues).
    pub errors: u64,
    pub cancelled: bool,
}

struct OpenRepo {
    meta: RepoMeta,
    db: redb::Database,
}

fn open_repo(store: &Store, name: &str) -> Result<OpenRepo, StoreError> {
    let meta = store.get_repo(name)?;
    let db = store.open_repo_db(name)?;
    Ok(OpenRepo { meta, db })
}

/// Collect the source entries (rel path + entry) that pass the filter.
/// `include_missing` controls whether missing entries are streamed too.
fn collect_source_entries(
    db: &redb::Database,
    filter: &FileFilter,
    include_missing: bool,
) -> Result<Vec<(String, FileEntry)>, StoreError> {
    let mut entries = Vec::new();
    store::for_each_file_entry(db, |rel_path, entry| {
        if (include_missing || !entry.missing) && filter.matches(rel_path, &entry) {
            entries.push((rel_path.to_string(), entry));
        }
        Ok(())
    })?;
    Ok(entries)
}

/// Classify every non-missing source file against the reference repo.
pub fn diff_print(
    store: &Store,
    source: &str,
    reference: &str,
    filter: Option<&str>,
) -> Result<Vec<DiffItem>, DiffError> {
    let filter = FileFilter::parse(filter)?;
    let source = open_repo(store, source)?;
    let reference = open_repo(store, reference)?;
    let ref_index = store::read_content_index(&reference.db)?;

    let mut items = Vec::new();
    for (rel_path, entry) in collect_source_entries(&source.db, &filter, false)? {
        match ref_index.get(&(entry.size, entry.hash)) {
            None => items.push(DiffItem::New { rel_path }),
            Some(state) if state.present => {
                let reference_path =
                    store::get_paths_by_size_hash(&reference.db, entry.size, &entry.hash)?
                        .into_iter()
                        .next()
                        .unwrap_or_default();
                items.push(DiffItem::Equal {
                    rel_path,
                    reference_path,
                });
            }
            Some(_) => items.push(DiffItem::DeletedInReference { rel_path }),
        }
    }
    Ok(items)
}

/// Copy (or move) every source file whose content the reference has never
/// seen — not even as a missing entry — into `target_dir`, preserving the
/// relative path.
///
/// Both repo indexes are kept in sync as the run proceeds: each copied file
/// that lands inside the reference (target) repo's directory is added to the
/// reference index with its on-disk mtime, and a move marks the source entries
/// missing. Index changes are flushed in periodic batches (plus a final flush,
/// applied even when the operation fails or is cancelled midway) so the indexes
/// always match what is actually on disk. Per-file progress is reported through
/// `progress`.
pub fn diff_copy(
    store: &Store,
    source: &str,
    reference: &str,
    dest: CopyDest<'_>,
    move_files: bool,
    filter: Option<&str>,
    run: &DiffRun<'_>,
) -> Result<CopyStats, DiffError> {
    let dest_root = resolve_subdir(dest.dir, dest.subdir)?;
    let filter = FileFilter::parse(filter)?;
    let source = open_repo(store, source)?;
    let reference = open_repo(store, reference)?;
    let ref_index = store::read_content_index(&reference.db)?;

    let candidates: Vec<(String, FileEntry)> = collect_source_entries(&source.db, &filter, false)?
        .into_iter()
        .filter(|(_, entry)| !ref_index.contains_key(&(entry.size, entry.hash)))
        .collect();

    let source_root = PathBuf::from(&source.meta.abs_path);
    let reference_root = PathBuf::from(&reference.meta.abs_path);
    let total = candidates.len() as u64;
    let action = if move_files {
        DiffAction::Move
    } else {
        DiffAction::Copy
    };
    let mut stats = CopyStats::default();
    // Entries to add to the reference (target) index and source paths to mark
    // missing, buffered until the next batch flush.
    let mut to_index: Vec<(String, FileEntry)> = Vec::new();
    let mut moved: Vec<String> = Vec::new();
    let mut since_flush = 0u64;
    let mut failure: Option<DiffError> = None;

    for (rel_path, entry) in &candidates {
        if run.cancel.is_cancelled() {
            stats.cancelled = true;
            break;
        }
        let from = source_root.join(rel_path);
        let to = dest_root.join(rel_path);
        if let Err(err) = transfer_file(&from, &to, move_files) {
            run.progress.on(DiffEvent::Error {
                path: from.to_string_lossy().into_owned(),
                message: err.to_string(),
            });
            failure = Some(err);
            break;
        }
        // Index the copy into the reference (target) repo when it actually
        // lands inside that repo's directory (always true from the GUI).
        if let Ok(target_rel) = to.strip_prefix(&reference_root) {
            let target_rel = target_rel.to_string_lossy().replace('\\', "/");
            let modified_ms = std::fs::metadata(&to)
                .and_then(|md| md.modified())
                .map(crate::update::system_time_to_ms)
                .unwrap_or(entry.modified_ms);
            let mut new_entry = entry.clone();
            new_entry.missing = false;
            new_entry.modified_ms = modified_ms;
            to_index.push((target_rel, new_entry));
        }
        if move_files {
            moved.push(rel_path.clone());
        }
        stats.copied += 1;
        since_flush += 1;
        run.progress.on(DiffEvent::Progress {
            action,
            done: stats.copied,
            total,
            rel_path: rel_path.clone(),
        });
        if since_flush >= INDEX_BATCH {
            flush_copy(&reference.db, &source.db, &mut to_index, &mut moved)?;
            since_flush = 0;
        }
    }

    // Final flush: anything already transferred on disk must be reflected in
    // the indexes, even on cancel or failure.
    flush_copy(&reference.db, &source.db, &mut to_index, &mut moved)?;

    match failure {
        Some(err) => Err(err),
        None => Ok(stats),
    }
}

/// Apply the buffered target-index additions and source missing-marks in a
/// single pair of write transactions, then clear the buffers.
fn flush_copy(
    reference_db: &redb::Database,
    source_db: &redb::Database,
    to_index: &mut Vec<(String, FileEntry)>,
    moved: &mut Vec<String>,
) -> Result<(), StoreError> {
    if !to_index.is_empty() {
        store::apply_entries(reference_db, to_index.iter().map(|(p, e)| (p.as_str(), e)))?;
        to_index.clear();
    }
    if !moved.is_empty() {
        store::mark_missing(source_db, moved.iter().map(String::as_str))?;
        moved.clear();
    }
    Ok(())
}

/// Delete every source file whose content the reference knows about (present
/// or missing) and mark the deleted entries missing in the source index.
///
/// The source index is kept in sync as the run proceeds: deleted paths are
/// marked missing in periodic batches (plus a final flush, applied even on
/// cancel or failure). Per-file progress is reported through `progress`.
pub fn diff_delete(
    store: &Store,
    source: &str,
    reference: &str,
    filter: Option<&str>,
    run: &DiffRun<'_>,
) -> Result<DeleteStats, DiffError> {
    let filter = FileFilter::parse(filter)?;
    let source = open_repo(store, source)?;
    let reference = open_repo(store, reference)?;
    let ref_index = store::read_content_index(&reference.db)?;

    let candidates: Vec<(String, FileEntry)> = collect_source_entries(&source.db, &filter, false)?
        .into_iter()
        .filter(|(_, entry)| ref_index.contains_key(&(entry.size, entry.hash)))
        .collect();

    let source_root = PathBuf::from(&source.meta.abs_path);
    let total = candidates.len() as u64;
    let mut stats = DeleteStats::default();
    let mut deleted: Vec<String> = Vec::new();
    let mut since_flush = 0u64;
    let mut failure: Option<DiffError> = None;

    for (rel_path, _) in &candidates {
        if run.cancel.is_cancelled() {
            stats.cancelled = true;
            break;
        }
        let path = source_root.join(rel_path);
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
            store::mark_missing(&source.db, deleted.iter().map(String::as_str))?;
            deleted.clear();
            since_flush = 0;
        }
    }

    // Final flush: files already removed from disk must be marked missing,
    // even on cancel or failure.
    if !deleted.is_empty() {
        store::mark_missing(&source.db, deleted.iter().map(String::as_str))?;
    }

    match failure {
        Some(err) => Err(err),
        None => Ok(stats),
    }
}

/// Sync the source repo into the target repo (best effort):
/// - `copy_new`: copy content that exists in source but not in target to the
///   same relative path; never overwrite an occupied path (counts as skipped);
///   successful copies are added to the target index.
/// - `delete_missing`: when the source marks content missing and the target
///   still has it, delete those target files and mark them missing in the
///   target index.
pub fn diff_sync(
    store: &Store,
    source: &str,
    target: &str,
    copy_new: bool,
    delete_missing: bool,
    filter: Option<&str>,
    cancel: &CancellationToken,
) -> Result<SyncStats, DiffError> {
    let filter = FileFilter::parse(filter)?;
    let source = open_repo(store, source)?;
    let target = open_repo(store, target)?;
    let mut target_index = store::read_content_index(&target.db)?;

    let entries = collect_source_entries(&source.db, &filter, true)?;
    let source_root = PathBuf::from(&source.meta.abs_path);
    let target_root = PathBuf::from(&target.meta.abs_path);
    let mut stats = SyncStats::default();

    for (rel_path, entry) in entries {
        if cancel.is_cancelled() {
            stats.cancelled = true;
            break;
        }
        if entry.missing {
            if delete_missing {
                sync_delete(&target, &target_root, &mut target_index, &entry, &mut stats)?;
            }
        } else if copy_new {
            sync_copy(
                &source_root,
                &target,
                &target_root,
                &mut target_index,
                &rel_path,
                &entry,
                &mut stats,
            )?;
        }
    }
    Ok(stats)
}

fn sync_copy(
    source_root: &Path,
    target: &OpenRepo,
    target_root: &Path,
    target_index: &mut HashMap<ContentKey, ContentState>,
    rel_path: &str,
    entry: &FileEntry,
    stats: &mut SyncStats,
) -> Result<(), StoreError> {
    let key = (entry.size, entry.hash);
    if target_index.get(&key).is_some_and(|state| state.present) {
        stats.equal += 1;
        return Ok(());
    }

    let target_file = target_root.join(rel_path);
    if target_file.exists() {
        stats.skipped += 1;
        return Ok(());
    }
    if let Some(parent) = target_file.parent()
        && std::fs::create_dir_all(parent).is_err()
    {
        stats.errors += 1;
        return Ok(());
    }
    if std::fs::copy(source_root.join(rel_path), &target_file).is_err() {
        stats.errors += 1;
        return Ok(());
    }
    stats.copied += 1;

    // Index the copy with the mtime the file actually has on the target so
    // the next update run sees it as unchanged.
    let modified_ms = std::fs::metadata(&target_file)
        .and_then(|md| md.modified())
        .map(crate::update::system_time_to_ms)
        .unwrap_or(entry.modified_ms);
    let mut new_entry = entry.clone();
    new_entry.missing = false;
    new_entry.modified_ms = modified_ms;
    store::apply_entries(&target.db, std::iter::once((rel_path, &new_entry)))?;
    target_index.entry(key).or_default().present = true;
    Ok(())
}

fn sync_delete(
    target: &OpenRepo,
    target_root: &Path,
    target_index: &mut HashMap<ContentKey, ContentState>,
    entry: &FileEntry,
    stats: &mut SyncStats,
) -> Result<(), StoreError> {
    let key = (entry.size, entry.hash);
    if !target_index.get(&key).is_some_and(|state| state.present) {
        return Ok(());
    }
    for rel_path in store::get_paths_by_size_hash(&target.db, entry.size, &entry.hash)? {
        let path = target_root.join(&rel_path);
        let removed = match std::fs::remove_file(&path) {
            Ok(()) => true,
            // NotFound counts as deleted: the file is gone either way.
            Err(err) => err.kind() == std::io::ErrorKind::NotFound,
        };
        if removed {
            store::mark_missing(&target.db, std::iter::once(rel_path.as_str()))?;
            stats.deleted += 1;
        } else {
            stats.errors += 1;
        }
    }
    if let Some(state) = target_index.get_mut(&key) {
        state.present = false;
        state.missing = true;
    }
    Ok(())
}

/// Move a file, falling back to copy+delete across filesystems; parent
/// directories of the destination are created as needed.
fn transfer_file(from: &Path, to: &Path, move_file: bool) -> Result<(), DiffError> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).map_err(|source| DiffError::Io {
            action: "create directory",
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let io_err = |action: &'static str, source: std::io::Error| DiffError::Io {
        action,
        path: from.to_path_buf(),
        source,
    };
    if move_file {
        if std::fs::rename(from, to).is_err() {
            // Cross-device move: copy, then remove the source.
            std::fs::copy(from, to).map_err(|e| io_err("move", e))?;
            std::fs::remove_file(from).map_err(|e| io_err("move", e))?;
        }
    } else {
        std::fs::copy(from, to).map_err(|e| io_err("copy", e))?;
    }
    Ok(())
}
