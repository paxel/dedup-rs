//! Repository update: walk the tree, hash new/changed files with BLAKE3,
//! and mark vanished files missing.
//!
//! Change detection follows the legacy semantics: a file whose (path, size,
//! mtime) matches a non-missing index entry is considered unchanged and is
//! not re-hashed. Everything else — new, changed, or previously missing —
//! gets hashed. Index writes are batched (~1000 entries per transaction) to
//! keep write transactions short.

use crate::fingerprint;
use crate::store::{self, FileEntry, Store, StoreError};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

const BATCH_SIZE: usize = 1000;

/// Cooperative cancellation flag shared between the caller and the update.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Progress events emitted during long-running operations.
///
/// Events are emitted freely (one per file); coalescing is the consumer's job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgressEvent {
    Scanning {
        files: u64,
        dirs: u64,
    },
    Hashing {
        done: u64,
        total: u64,
        /// Bytes processed so far (summed file sizes, whether hashed or errored).
        done_bytes: u64,
        /// Total bytes to process this run (summed sizes of all files to hash).
        total_bytes: u64,
        current: String,
    },
    Error {
        path: String,
        message: String,
    },
    Finished {
        stats: UpdateStats,
    },
}

/// Callback for progress reporting. Implementations must be cheap and
/// non-blocking; they may be called from worker threads.
pub trait Progress: Send + Sync {
    fn on(&self, event: ProgressEvent);
}

/// A [`Progress`] implementation that discards all events.
pub struct NoProgress;

impl Progress for NoProgress {
    fn on(&self, _event: ProgressEvent) {}
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UpdateStats {
    /// Files hashed that had no index entry before.
    pub added: u64,
    /// Files re-hashed because size/mtime changed or they had been missing.
    pub updated: u64,
    /// Files skipped because (size, mtime) matched the index.
    pub unchanged: u64,
    /// Index entries marked missing because the file vanished from disk.
    pub marked_missing: u64,
    /// Files or directories that could not be read.
    pub errors: u64,
    /// Total bytes hashed.
    pub hashed_bytes: u64,
    /// True if the update was cancelled before completion.
    pub cancelled: bool,
    /// True when the walk found no file at all over an index that held some.
    /// Usually a drive that failed to mount rather than a repo somebody
    /// emptied; the entries are still marked missing, but callers should say so
    /// loudly — an emptied main is what makes a MIRROR sync a wipe.
    pub empty_walk: bool,
}

#[derive(thiserror::Error, Debug)]
pub enum UpdateError {
    #[error(transparent)]
    Store(#[from] StoreError),

    #[error("Repository directory does not exist: {0}")]
    RootMissing(String),

    #[error("Failed to build hashing thread pool: {0}")]
    ThreadPool(String),

    #[error(
        "Scanning '{repo}' found no files at all, but its index holds {entries}. Marking every \
         entry missing would empty the index — and if this repo is a sync group's main, the next \
         MIRROR push would then wipe its sinks. If the drive should not be empty, check it is \
         mounted. To scan it anyway, re-run with --force."
    )]
    WouldEmptyIndex { repo: String, entries: u64 },

    #[error("Could not read {path}: {source}")]
    FileUnreadable {
        path: String,
        source: std::io::Error,
    },
}

struct WalkedFile {
    rel: String,
    abs: PathBuf,
    size: u64,
    modified_ms: i64,
}

/// Outcome of walking the tree and diffing it against the stored index. Shared
/// by [`update_repo`] (which then hashes `to_hash`) and [`check_repo`] (which
/// only reports the counts).
struct WalkSplit {
    /// Files that are new, whose (size, mtime) differ from the index, or whose
    /// stored entry predates the current fingerprint format.
    to_hash: Vec<WalkedFile>,
    /// Non-missing index entries not seen on disk this walk (vanished).
    vanished: Vec<String>,
    /// Files whose (size, mtime) matched the index.
    unchanged: u64,
    /// Files or directories that could not be read.
    errors: u64,
    /// True if the walk was cancelled before it completed.
    cancelled: bool,
}

/// Walk `root`, skipping unreadable entries (reported, then continue), and
/// split what is found against `existing` into unchanged vs new/changed, plus
/// the index entries that have vanished from disk. Emits `Scanning` progress.
fn walk_and_split(
    root: &Path,
    existing: &std::collections::HashMap<String, store::ScanEntry>,
    progress: &dyn Progress,
    cancel: &CancellationToken,
) -> WalkSplit {
    let mut walked: Vec<WalkedFile> = Vec::new();
    let mut dirs = 0u64;
    let mut errors = 0u64;
    for item in walkdir::WalkDir::new(root).follow_links(false) {
        if cancel.is_cancelled() {
            return WalkSplit {
                to_hash: Vec::new(),
                vanished: Vec::new(),
                unchanged: 0,
                errors,
                cancelled: true,
            };
        }
        let entry = match item {
            Ok(entry) => entry,
            Err(err) => {
                errors += 1;
                progress.on(ProgressEvent::Error {
                    path: err
                        .path()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default(),
                    message: err.to_string(),
                });
                continue;
            }
        };
        if entry.file_type().is_dir() {
            dirs += 1;
            continue;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(err) => {
                errors += 1;
                progress.on(ProgressEvent::Error {
                    path: entry.path().display().to_string(),
                    message: err.to_string(),
                });
                continue;
            }
        };
        // Rel paths are stored with `/` separators on every OS — the index is
        // portable, and every lookup/join in the codebase assumes it. Joined
        // per component, NOT via a blanket `\ → /` replace: on Unix a file
        // may legally be *named* `a\b.txt`, and rewriting that would corrupt
        // its index key.
        let rel = match entry.path().strip_prefix(root) {
            Ok(rel) => {
                let mut s = String::new();
                for part in rel.components() {
                    if !s.is_empty() {
                        s.push('/');
                    }
                    s.push_str(&part.as_os_str().to_string_lossy());
                }
                s
            }
            Err(_) => continue,
        };
        let modified_ms = metadata.modified().ok().map(system_time_to_ms).unwrap_or(0);
        walked.push(WalkedFile {
            rel,
            abs: entry.into_path(),
            size: metadata.len(),
            modified_ms,
        });
        progress.on(ProgressEvent::Scanning {
            files: walked.len() as u64,
            dirs,
        });
    }

    // Split into unchanged and to-hash; whatever non-missing entry is not
    // seen on disk afterwards has vanished.
    let mut remaining: std::collections::HashMap<&str, ()> = existing
        .iter()
        .filter(|(_, entry)| !entry.missing)
        .map(|(rel, _)| (rel.as_str(), ()))
        .collect();
    let mut to_hash: Vec<WalkedFile> = Vec::new();
    let mut unchanged = 0u64;
    for file in walked {
        remaining.remove(file.rel.as_str());
        match existing.get(&file.rel) {
            Some(entry)
                if !entry.missing
                    && !entry.stale
                    && entry.size == file.size
                    && entry.modified_ms == file.modified_ms =>
            {
                unchanged += 1;
            }
            _ => to_hash.push(file),
        }
    }
    let vanished: Vec<String> = remaining.into_keys().map(|s| s.to_string()).collect();

    WalkSplit {
        to_hash,
        vanished,
        unchanged,
        errors,
        cancelled: false,
    }
}

/// Scan the repository directory of `name` and bring its index up to date.
///
/// `threads` is the hashing thread count; `0` uses one thread per CPU core.
/// The operation checks `cancel` per file and stops cleanly mid-hash: entries
/// already hashed are committed, vanished files are only marked missing on a
/// complete, uncancelled walk.
pub fn update_repo(
    store: &Store,
    name: &str,
    threads: usize,
    progress: &dyn Progress,
    cancel: &CancellationToken,
) -> Result<UpdateStats, UpdateError> {
    update_repo_authorized(store, name, threads, progress, cancel, false)
}

/// [`update_repo`], with explicit authorisation for the destructive-empty case.
///
/// A walk that finds **no file at all** over an index that still holds entries
/// marks every one of them missing. That is far more often an unmounted drive
/// than a repo somebody emptied — an unmounted mountpoint is still a directory,
/// so the `is_dir` check below lets it through — and an emptied main turns the
/// next MIRROR push into a wipe of every sink. So it is refused with
/// [`UpdateError::WouldEmptyIndex`] unless `allow_empty` is set, and the index is
/// left untouched. Emptying a repo on purpose stays supported; it costs one
/// explicit confirmation (GUI) or `--force` (CLI).
pub fn update_repo_authorized(
    store: &Store,
    name: &str,
    threads: usize,
    progress: &dyn Progress,
    cancel: &CancellationToken,
    allow_empty: bool,
) -> Result<UpdateStats, UpdateError> {
    let meta = store.get_repo(name)?;
    let root = PathBuf::from(&meta.abs_path);
    if !root.is_dir() {
        return Err(UpdateError::RootMissing(meta.abs_path.clone()));
    }

    let db = store.open_repo_db(name)?;
    let existing = store::read_scan_index(&db)?;
    let mut stats = UpdateStats::default();

    // Walk the tree and diff it against the stored index.
    let split = walk_and_split(&root, &existing, progress, cancel);
    stats.errors = split.errors;
    stats.unchanged = split.unchanged;
    if split.cancelled {
        stats.cancelled = true;
        progress.on(ProgressEvent::Finished { stats });
        return Ok(stats);
    }
    let to_hash = split.to_hash;
    let remaining = split.vanished;

    // Hash in parallel; a single consumer batches index writes.
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .map_err(|e| UpdateError::ThreadPool(e.to_string()))?;
    let total = to_hash.len() as u64;
    let total_bytes: u64 = to_hash.iter().map(|f| f.size).sum();
    let (sender, receiver) = mpsc::channel::<(&WalkedFile, std::io::Result<HashedFile>)>();

    // Perceptual fingerprints (images/video/pdf/audio) are computed alongside
    // the content hash. Probe ffmpeg once and warn (once) if videos will go
    // unfingerprinted for lack of it.
    let ffmpeg_available = fingerprint::ffmpeg_available();
    if !ffmpeg_available && to_hash.iter().any(|f| is_video_ext(&f.rel)) {
        progress.on(ProgressEvent::Error {
            path: String::new(),
            message: "ffmpeg not found on PATH; video fingerprints are disabled".to_string(),
        });
    }

    let write_result = std::thread::scope(|scope| -> Result<(), StoreError> {
        scope.spawn(|| {
            pool.install(|| {
                use rayon::prelude::*;
                to_hash.par_iter().for_each_with(sender, |sender, file| {
                    if cancel.is_cancelled() {
                        return;
                    }
                    let result = hash_file(&file.abs).map(|hash| {
                        let fingerprints = fingerprint::compute(&file.abs, ffmpeg_available);
                        // A changed archive has its members read in the same
                        // pass; an unchanged one never reaches here, so the
                        // heavy re-read is paid once per change, like the hash.
                        let archive_members = if crate::archive::is_archive(
                            &file.rel,
                            fingerprints.mime.as_deref(),
                        ) {
                            Some(
                                crate::archive::list_members(
                                    &file.abs,
                                    &file.rel,
                                    fingerprints.mime.as_deref(),
                                )
                                .unwrap_or_default(),
                            )
                        } else {
                            None
                        };
                        HashedFile {
                            hash,
                            fingerprints,
                            archive_members,
                        }
                    });
                    // Receiver gone means the writer failed; just stop sending.
                    let _ = sender.send((file, result));
                });
            });
        });

        let mut batch: Vec<(&str, FileEntry)> = Vec::new();
        // Archive member lists to write once the file batch is flushed. Rare
        // relative to loose files, so a per-archive write afterward is fine.
        let mut archive_members: Vec<(&str, Vec<store::ArchiveMember>)> = Vec::new();
        let mut done_bytes: u64 = 0;
        for (index, (file, result)) in receiver.iter().enumerate() {
            let done = index as u64 + 1;
            // Count every file's size toward progress, hashed or errored, so the
            // byte counter reaches `total_bytes` exactly at completion.
            done_bytes += file.size;
            match result {
                Ok(hashed) => {
                    if existing.contains_key(&file.rel) {
                        stats.updated += 1;
                    } else {
                        stats.added += 1;
                    }
                    stats.hashed_bytes += file.size;
                    if let Some(members) = hashed.archive_members {
                        archive_members.push((file.rel.as_str(), members));
                    }
                    let fp = hashed.fingerprints;
                    batch.push((
                        file.rel.as_str(),
                        FileEntry {
                            size: file.size,
                            hash: hashed.hash,
                            modified_ms: file.modified_ms,
                            missing: false,
                            mime: fp.mime,
                            img_fingerprint: fp.img_fingerprint,
                            video_hash: fp.video_hash,
                            pdf_hash: fp.pdf_hash,
                            audio: fp.audio,
                            img_size: fp.img_size,
                            origin: None,
                            exif: fp.exif,
                        },
                    ));
                    if batch.len() >= BATCH_SIZE {
                        store::apply_entries(&db, batch.iter().map(|(rel, e)| (*rel, e)))?;
                        batch.clear();
                    }
                }
                Err(err) => {
                    stats.errors += 1;
                    progress.on(ProgressEvent::Error {
                        path: file.abs.display().to_string(),
                        message: err.to_string(),
                    });
                }
            }
            progress.on(ProgressEvent::Hashing {
                done,
                total,
                done_bytes,
                total_bytes,
                current: file.rel.clone(),
            });
        }
        store::apply_entries(&db, batch.iter().map(|(rel, e)| (*rel, e)))?;
        // Replace each changed archive's member list wholesale.
        for (rel, members) in &archive_members {
            store::set_archive_members(&db, rel, members)?;
        }
        Ok(())
    });
    write_result?;

    // A walk that saw no file at all, over an index that held some, is more
    // often an unmounted drive than a repo somebody emptied: an unmounted
    // mountpoint is still a directory, so the `is_dir` check above lets it
    // through. Emptying a repo really is legitimate, so the entries are still
    // marked missing — but the caller is told, because an emptied main is what
    // turns a MIRROR sync into a wipe.
    stats.empty_walk = total == 0 && stats.unchanged == 0 && !remaining.is_empty();

    // Refuse before anything is marked, unless the caller authorised it. The
    // walk found no files, so nothing has been written yet and the index is
    // exactly as it was.
    if stats.empty_walk && !allow_empty && !cancel.is_cancelled() {
        log::error!(
            "refusing to scan '{}': it walked empty while its index holds {} entries",
            meta.abs_path,
            remaining.len()
        );
        return Err(UpdateError::WouldEmptyIndex {
            repo: name.to_string(),
            entries: remaining.len() as u64,
        });
    }

    // Only a complete, uncancelled walk proves a file vanished.
    if cancel.is_cancelled() {
        stats.cancelled = true;
    } else {
        if stats.empty_walk {
            log::warn!(
                "scan of '{}' found no files at all, but the index held {} — marking them \
                 missing; check the directory is mounted",
                meta.abs_path,
                remaining.len()
            );
            progress.on(ProgressEvent::Error {
                path: meta.abs_path.clone(),
                message: format!(
                    "Found no files at all, but the index held {}. If this drive should not \
                     be empty, check it is mounted — every entry is now marked missing.",
                    remaining.len()
                ),
            });
        }
        stats.marked_missing = remaining.len() as u64;
        store::mark_missing(&db, remaining.iter().map(|s| s.as_str()))?;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        store::set_last_scan(&db, now_ms)?;
    }

    progress.on(ProgressEvent::Finished { stats });
    Ok(stats)
}

/// Result of a dry-run [`check_repo`]: how the tree differs from the stored
/// index, computed without hashing file contents or writing anything.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CheckStats {
    /// Files that are new or whose (size, mtime) differ from the index.
    pub changed: u64,
    /// Non-missing index entries no longer present on disk.
    pub missing: u64,
    /// Files whose (size, mtime) match the index.
    pub unchanged: u64,
    /// Files or directories that could not be read.
    pub errors: u64,
    /// True if the check was cancelled before completion.
    pub cancelled: bool,
}

impl CheckStats {
    /// Whether an update would leave the index unchanged (nothing new, changed,
    /// or vanished).
    pub fn up_to_date(&self) -> bool {
        self.changed == 0 && self.missing == 0
    }
}

/// Dry-run the change detection for `name`: walk the tree and diff it against
/// the stored index, reporting new/changed and vanished counts **without
/// hashing file contents or writing to the index**.
///
/// Change detection compares each file's `(size, mtime)` against its stored
/// record — not against the last-scan wall-clock time — so a file synced in
/// with a preserved (older) timestamp is still flagged. It cannot detect a
/// content change that kept both size and mtime identical; only a full
/// [`update_repo`] re-hash catches that.
pub fn check_repo(
    store: &Store,
    name: &str,
    progress: &dyn Progress,
    cancel: &CancellationToken,
) -> Result<CheckStats, UpdateError> {
    let meta = store.get_repo(name)?;
    let root = PathBuf::from(&meta.abs_path);
    if !root.is_dir() {
        return Err(UpdateError::RootMissing(meta.abs_path.clone()));
    }

    let db = store.open_repo_db(name)?;
    let existing = store::read_scan_index(&db)?;
    let split = walk_and_split(&root, &existing, progress, cancel);

    Ok(CheckStats {
        changed: split.to_hash.len() as u64,
        missing: if split.cancelled {
            0
        } else {
            split.vanished.len() as u64
        },
        unchanged: split.unchanged,
        errors: split.errors,
        cancelled: split.cancelled,
    })
}

/// A file's content hash together with its perceptual fingerprints, carried
/// from the parallel hashing stage to the single writer.
struct HashedFile {
    hash: [u8; 32],
    fingerprints: fingerprint::Fingerprints,
    /// When the file is an archive, its members read in the same pass — so
    /// indexing folds into the scan, gated by the same change-detection as the
    /// content hash. `None` for non-archives.
    archive_members: Option<Vec<store::ArchiveMember>>,
}

/// Cheap extension check used only to decide whether to warn about a missing
/// ffmpeg; authoritative MIME detection happens in [`fingerprint::compute`].
fn is_video_ext(rel: &str) -> bool {
    matches!(
        rel.rsplit('.')
            .next()
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("mp4" | "mkv" | "avi" | "mov" | "webm" | "wmv" | "flv" | "m4v" | "mpg" | "mpeg")
    )
}

fn hash_file(path: &Path) -> std::io::Result<[u8; 32]> {
    let mut hasher = blake3::Hasher::new();
    let mut file = std::fs::File::open(path)?;
    std::io::copy(&mut file, &mut hasher)?;
    Ok(*hasher.finalize().as_bytes())
}

/// Re-stat, re-hash and re-fingerprint a single file and rewrite its index
/// entry, preserving the entry's provenance (`origin`).
///
/// For a caller that has just changed the file's bytes **in place** — saving a
/// rotated image, say — possibly while deliberately preserving its modified
/// time, which would make the change invisible to the (size, mtime) skip of a
/// normal scan. Content identity is the hash, so the index must follow the
/// bytes immediately rather than wait for a rescan that may never notice.
pub fn refresh_file_entry(store: &Store, repo: &str, rel: &str) -> Result<FileEntry, UpdateError> {
    let meta = store.get_repo(repo)?;
    let abs = Path::new(&meta.abs_path).join(rel);
    let unreadable = |source: std::io::Error| UpdateError::FileUnreadable {
        path: abs.display().to_string(),
        source,
    };
    let md = std::fs::metadata(&abs).map_err(unreadable)?;
    let hash = hash_file(&abs).map_err(unreadable)?;
    let fp = fingerprint::compute(&abs, fingerprint::ffmpeg_available());
    let origin = store.get_file_entry(repo, rel)?.and_then(|e| e.origin);
    let entry = FileEntry {
        size: md.len(),
        hash,
        modified_ms: md.modified().ok().map(system_time_to_ms).unwrap_or(0),
        missing: false,
        mime: fp.mime,
        img_fingerprint: fp.img_fingerprint,
        video_hash: fp.video_hash,
        pdf_hash: fp.pdf_hash,
        audio: fp.audio,
        img_size: fp.img_size,
        origin,
        exif: fp.exif,
    };
    store.update_file_entry(repo, rel, &entry)?;
    Ok(entry)
}

/// Give `to` the modification time of `from`.
///
/// `std::fs::copy` does not carry timestamps over, so a copied file would
/// otherwise get a fresh mtime — losing the date the whole timeline/date
/// machinery relies on, and making the copy look changed against its source.
pub(crate) fn copy_mtime(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    let modified = std::fs::metadata(from)?.modified()?;
    std::fs::File::options()
        .write(true)
        .open(to)?
        .set_modified(modified)
}

pub(crate) fn system_time_to_ms(time: std::time::SystemTime) -> i64 {
    match time.duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_millis()).unwrap_or(i64::MAX),
        Err(err) => -i64::try_from(err.duration().as_millis()).unwrap_or(i64::MAX),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stale index entry (pre-upgrade fingerprint format) must be re-hashed
    /// even though its (size, mtime) still match the file on disk.
    #[test]
    fn walk_and_split_rehashes_stale_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("photo.jpg");
        std::fs::write(&path, b"not really a jpeg").expect("write");
        let meta = std::fs::metadata(&path).expect("metadata");
        let entry = |stale| store::ScanEntry {
            size: meta.len(),
            modified_ms: meta.modified().map(system_time_to_ms).unwrap_or(0),
            missing: false,
            stale,
        };

        let existing = std::collections::HashMap::from([("photo.jpg".to_string(), entry(false))]);
        let split = walk_and_split(
            dir.path(),
            &existing,
            &NoProgress,
            &CancellationToken::new(),
        );
        assert_eq!(split.unchanged, 1);
        assert!(split.to_hash.is_empty());

        let existing = std::collections::HashMap::from([("photo.jpg".to_string(), entry(true))]);
        let split = walk_and_split(
            dir.path(),
            &existing,
            &NoProgress,
            &CancellationToken::new(),
        );
        assert_eq!(split.unchanged, 0);
        assert_eq!(split.to_hash.len(), 1);
    }
}
