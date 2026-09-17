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
//!   overwriting an occupied path) and delete target content per a
//!   [`SyncDelete`] mode (nothing / the source's own deletions / everything the
//!   source lacks, i.e. a content-mirror); best effort, errors are counted.
//! - [`plan_sync`]: preview a [`diff_sync`] (the copies and deletes it would
//!   make) without touching disk.
//! - [`export_to_folder`]: copy (or move) a deduplicated selection of the
//!   source into a plain folder (not a repo), keeping source-relative paths.

use crate::filter::{FileFilter, FilterError};
use crate::store::{self, ContentKey, ContentState, FileEntry, RepoMeta, Store, StoreError};
use crate::update::CancellationToken;
use std::collections::{HashMap, HashSet};
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

    #[error("at least one reference repo is required")]
    NoReference,

    #[error(
        "Sync group main '{main}' has no indexed files, so a MIRROR push would delete \
         everything in its sink(s). Scan '{main}' first — a drive that failed to mount \
         scans as an empty directory."
    )]
    EmptyMirrorSource { main: String },
}

/// Where a copy/move should place files: a target directory and an optional
/// relative `subdir` inside it. Files keep their source-relative path under
/// `dir`/`subdir`.
#[derive(Debug, Clone, Copy)]
pub struct CopyDest<'a> {
    pub dir: &'a Path,
    pub subdir: Option<&'a str>,
}

/// Whether a relative path stays inside its root: every component is an ordinary
/// name or `.`, so joining it onto a root cannot escape (no absolute prefix, no
/// drive/root, no `..`). The single place the repo-escape rule is defined, so
/// [`resolve_subdir`] and [`resolve_in_repo`] cannot drift apart.
fn stays_within_root(rel: &Path) -> bool {
    rel.components().all(|c| {
        matches!(
            c,
            std::path::Component::Normal(_) | std::path::Component::CurDir
        )
    })
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
    if !stays_within_root(rel) {
        return Err(DiffError::InvalidSubdir {
            subdir: raw.to_string(),
        });
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
/// report live progress, how to observe cancellation, and (optionally) which
/// review-board rows to act on. Grouping these keeps the operation signatures
/// compact.
///
/// Row selection uses namespaced keys, because a source-side row and a
/// target-side row can share the same relative path (a mirror can delete the
/// target's `a.txt` and then copy the source's `a.txt`): `s:<rel>` identifies
/// a source-side row (copy/move/delete-from-source/organize), `t:<rel>` a
/// target-side row (a sync/mirror deletion in the target).
#[derive(Clone, Copy)]
pub struct DiffRun<'a> {
    pub progress: &'a dyn DiffProgress,
    pub cancel: &'a CancellationToken,
    /// Namespaced row keys the user rejected in review; the op skips them.
    exclude: Option<&'a HashSet<String>>,
    /// When set, the op acts *only* on these namespaced row keys (a
    /// single-row apply is the batch op with a one-element allowlist).
    only: Option<&'a HashSet<String>>,
}

/// Build the namespaced selection key for a source-side row.
pub fn source_key(rel: &str) -> String {
    format!("s:{rel}")
}

/// Build the namespaced selection key for a target-side row.
pub fn target_key(rel: &str) -> String {
    format!("t:{rel}")
}

impl<'a> DiffRun<'a> {
    pub fn new(progress: &'a dyn DiffProgress, cancel: &'a CancellationToken) -> Self {
        Self {
            progress,
            cancel,
            exclude: None,
            only: None,
        }
    }

    /// Restrict this run to the review-board selection: skip `exclude`d rows,
    /// and when `only` is set act on those rows alone. Keys are namespaced via
    /// [`source_key`] / [`target_key`].
    pub fn with_selection(
        mut self,
        exclude: Option<&'a HashSet<String>>,
        only: Option<&'a HashSet<String>>,
    ) -> Self {
        self.exclude = exclude;
        self.only = only;
        self
    }

    /// Whether the op should act on the source-side row for `rel`.
    pub fn selected_source(&self, rel: &str) -> bool {
        self.selected(&source_key(rel))
    }

    /// Whether the op should act on the target-side row for `rel`.
    pub fn selected_target(&self, rel: &str) -> bool {
        self.selected(&target_key(rel))
    }

    fn selected(&self, key: &str) -> bool {
        self.only.is_none_or(|s| s.contains(key)) && self.exclude.is_none_or(|s| !s.contains(key))
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

/// Why a sink file is a candidate to pull back into its main (GROUP SYNC BACK).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullKind {
    /// Content the main has never had — a direct edit to the backup, promoted
    /// with no risk.
    New,
    /// Content the main once had and **deleted** (a tombstone) that the sink
    /// still holds. Bringing it back undoes the main's deletion — which may be a
    /// recovered mistake or an unwanted resurrection of a deliberate cleanup — so
    /// it is always the user's explicit choice, never automatic.
    Resurrection,
}

/// One sink file GROUP SYNC BACK could bring into the main, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullItem {
    pub rel_path: String,
    pub kind: PullKind,
}

/// Classify a sink's files against its main for a pull back: the content the
/// main never had ([`PullKind::New`]) and the content the main deleted but the
/// sink still holds ([`PullKind::Resurrection`]). Content the main already has is
/// omitted (nothing to pull). Reuses [`diff_print`] (source = sink, reference =
/// main), so a file is classified by its **content**, never its path, and the
/// order follows `diff_print`'s. The new/resurrection split is the whole point —
/// a naive "copy what the main lacks" cannot make it, because a tombstone and a
/// never-seen file both look like "the main lacks this content".
pub fn plan_sync_back(
    store: &Store,
    sink: &str,
    main: &str,
    filter: Option<&str>,
) -> Result<Vec<PullItem>, DiffError> {
    Ok(diff_print(store, sink, &[main], filter)?
        .into_iter()
        .filter_map(|item| match item {
            DiffItem::New { rel_path } => Some(PullItem {
                rel_path,
                kind: PullKind::New,
            }),
            DiffItem::DeletedInReference { rel_path } => Some(PullItem {
                rel_path,
                kind: PullKind::Resurrection,
            }),
            DiffItem::Equal { .. } => None,
        })
        .collect())
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
    /// Target files deleted by the delete phase (per the [`SyncDelete`] mode).
    pub deleted: u64,
    /// Errors encountered (best effort: the sync continues).
    pub errors: u64,
    pub cancelled: bool,
}

struct OpenRepo {
    meta: RepoMeta,
    db: std::sync::Arc<redb::Database>,
}

fn open_repo(store: &Store, name: &str) -> Result<OpenRepo, StoreError> {
    let meta = store.get_repo(name)?;
    let db = store.open_repo_db(name)?;
    Ok(OpenRepo { meta, db })
}

/// Open every reference repo once and merge their content indexes into one
/// presence map. A content key is `present` if any reference has a live copy
/// and `missing` if any reference marks it missing — so "unique" means unique
/// against every reference, not just one.
///
/// The primary (`repos[0]`) is special: it is the repo whose index `diff_copy`
/// writes copies back into, and whose paths `diff_print` prefers for `Equal`.
/// Callers must pass at least one reference. The whole vec is returned so
/// per-file lookups can reuse the open handles instead of reopening repos.
fn open_references(
    store: &Store,
    references: &[&str],
) -> Result<(Vec<OpenRepo>, HashMap<ContentKey, ContentState>), DiffError> {
    if references.is_empty() {
        return Err(DiffError::NoReference);
    }
    let mut repos = Vec::with_capacity(references.len());
    for name in references {
        repos.push(open_repo(store, name)?);
    }
    let mut merged: HashMap<ContentKey, ContentState> = HashMap::new();
    for repo in &repos {
        for (key, state) in store::read_content_index(&repo.db)? {
            let slot = merged.entry(key).or_default();
            slot.present |= state.present;
            slot.missing |= state.missing;
        }
    }
    Ok((repos, merged))
}

/// Collect the source entries (rel path + entry) that pass the filter.
/// `include_missing` controls whether missing entries are streamed too.
fn collect_source_entries(
    db: &redb::Database,
    filter: &FileFilter,
    include_missing: bool,
) -> Result<Vec<(String, FileEntry)>, StoreError> {
    let annotated = crate::filter::AnnotatedFilter::new(db, filter)?;
    let mut entries = Vec::new();
    store::for_each_file_entry(db, |rel_path, entry| {
        if (include_missing || !entry.missing) && annotated.matches(rel_path, &entry) {
            entries.push((rel_path.to_string(), entry));
        }
        Ok(())
    })?;
    Ok(entries)
}

/// A source repository's filtered index, collected once.
///
/// Pushing a sync group syncs one main to several sinks. Done naively that
/// re-opens the main and re-streams its whole index once per sink, to produce
/// the same data every time — so [`plan_group_sync`](crate::sync_group::plan_group_sync)
/// and [`run_group_sync`](crate::sync_group::run_group_sync) build one of these
/// up front and hand it to every sink's plan or run.
///
/// It holds everything the sync path needs from the source: the entries the
/// filter admitted (missing ones included — a `Missing` delete needs them), the
/// repository's root on disk, and its name for provenance.
pub struct SourceView {
    name: String,
    root: PathBuf,
    entries: Vec<(String, FileEntry)>,
}

impl SourceView {
    /// Open `source` and collect the entries `filter` admits.
    pub fn collect(store: &Store, source: &str, filter: &FileFilter) -> Result<Self, DiffError> {
        let repo = open_repo(store, source)?;
        Ok(Self {
            name: source.to_string(),
            root: PathBuf::from(&repo.meta.abs_path),
            entries: collect_source_entries(&repo.db, filter, true)?,
        })
    }

    /// The content this source currently holds (non-missing): the reference set
    /// an `Absent` (mirror) delete removes target content outside of.
    fn present_content(&self) -> HashSet<ContentKey> {
        self.entries
            .iter()
            .filter(|(_, e)| !e.missing)
            .map(|(_, e)| (e.size, e.hash))
            .collect()
    }
}

/// Classify every non-missing source file against the union of the reference
/// repos. `Equal`'s `reference_path` is taken from the primary reference when it
/// holds the content, else from the first reference that does.
pub fn diff_print(
    store: &Store,
    source: &str,
    references: &[&str],
    filter: Option<&str>,
) -> Result<Vec<DiffItem>, DiffError> {
    let filter = FileFilter::parse(filter)?;
    let source = open_repo(store, source)?;
    let (refs, ref_index) = open_references(store, references)?;

    let mut items = Vec::new();
    for (rel_path, entry) in collect_source_entries(&source.db, &filter, false)? {
        match ref_index.get(&(entry.size, entry.hash)) {
            None => items.push(DiffItem::New { rel_path }),
            Some(state) if state.present => {
                // Prefer a path from the primary; fall back to any reference
                // (all handles are already open — no per-file reopening).
                let mut reference_path =
                    store::get_paths_by_size_hash(&refs[0].db, entry.size, &entry.hash)?
                        .into_iter()
                        .next()
                        .unwrap_or_default();
                if reference_path.is_empty() {
                    for extra in &refs[1..] {
                        if let Some(p) =
                            store::get_paths_by_size_hash(&extra.db, entry.size, &entry.hash)?
                                .into_iter()
                                .next()
                        {
                            reference_path = p;
                            break;
                        }
                    }
                }
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
    references: &[&str],
    dest: CopyDest<'_>,
    move_files: bool,
    filter: Option<&str>,
    run: &DiffRun<'_>,
) -> Result<CopyStats, DiffError> {
    let dest_root = resolve_subdir(dest.dir, dest.subdir)?;
    let filter = FileFilter::parse(filter)?;
    let source_name = source.to_string();
    let source = open_repo(store, source)?;
    // The primary reference is the copy-back target; the merged index is the
    // union of all references (a file is "new" only if no reference has it).
    let (mut refs, ref_index) = open_references(store, references)?;
    let reference = refs.swap_remove(0);
    drop(refs); // the extras were only needed to build the merged index

    let candidates: Vec<(String, FileEntry)> = collect_source_entries(&source.db, &filter, false)?
        .into_iter()
        .filter(|(rel, entry)| {
            !ref_index.contains_key(&(entry.size, entry.hash)) && run.selected_source(rel)
        })
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
        // lands inside that repo's directory. This relies on `dest_root`
        // resolving under the reference repo's registered `abs_path` as an
        // exact path-string prefix, which is always true from the GUI (both
        // derive from the same repo metadata). A CLI destination spelled
        // differently (relative, trailing-slash or symlinked form) will not
        // match here, so the target index is left untouched until the next
        // scan rather than risking a wrong entry.
        if let Ok(target_rel) = to.strip_prefix(&reference_root) {
            let target_rel = target_rel.to_string_lossy().replace('\\', "/");
            to_index.push((target_rel, entry_for_copied_file(entry, &to, &source_name)));
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
/// Contents *accepted* in the source (see [`Store::accept_content`]) are
/// never candidates — they are allowed to exist there.
///
/// The source index is kept in sync as the run proceeds: deleted paths are
/// marked missing in periodic batches (plus a final flush, applied even on
/// cancel or failure). Per-file progress is reported through `progress`.
pub fn diff_delete(
    store: &Store,
    source: &str,
    references: &[&str],
    filter: Option<&str>,
    run: &DiffRun<'_>,
) -> Result<DeleteStats, DiffError> {
    let filter = FileFilter::parse(filter)?;
    let source = open_repo(store, source)?;
    let (_refs, ref_index) = open_references(store, references)?;

    let accepted = store::accepted_of_db(&source.db)?;
    let candidates: Vec<(String, FileEntry)> = collect_source_entries(&source.db, &filter, false)?
        .into_iter()
        .filter(|(rel, entry)| {
            ref_index.contains_key(&(entry.size, entry.hash))
                && !accepted.contains(&(entry.size, entry.hash))
                && run.selected_source(rel)
        })
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

/// What [`diff_sync`] deletes in the target (its copy behaviour is separate,
/// controlled by `copy_new`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncDelete {
    /// Delete nothing in the target.
    None,
    /// Delete target content the source marks *missing* — propagate the
    /// source's own deletions into the target. Never touches content the
    /// source simply never had.
    Missing,
    /// Delete any target content not currently in the source: make the target a
    /// content-mirror of the source. This runs *before* the copy so a copy can
    /// reclaim a path a delete frees. It is a mirror by *content*, so identical
    /// content living at a different path in the target is kept, not relocated.
    Absent,
}

/// Sync the source repo into the target repo (best effort):
/// - `copy_new`: copy content that exists in source but not in target to the
///   same relative path; never overwrite an occupied path (counts as skipped);
///   successful copies are added to the target index.
/// - `delete`: what to remove from the target — see [`SyncDelete`]. A
///   [`SyncDelete::Absent`] (mirror) delete runs before the copy so a freed
///   path can be reused; the milder [`SyncDelete::Missing`] runs after.
pub fn diff_sync(
    store: &Store,
    source: &str,
    target: &str,
    copy_new: bool,
    delete: SyncDelete,
    filter: Option<&str>,
    run: &DiffRun<'_>,
) -> Result<SyncStats, DiffError> {
    let filter = FileFilter::parse(filter)?;
    let source = SourceView::collect(store, source, &filter)?;
    diff_sync_from(store, &source, target, copy_new, delete, &filter, run)
}

/// [`diff_sync`] against an already-collected source, so a multi-sink push
/// reads the main once instead of once per sink.
pub fn diff_sync_from(
    store: &Store,
    source: &SourceView,
    target: &str,
    copy_new: bool,
    delete: SyncDelete,
    filter: &FileFilter,
    run: &DiffRun<'_>,
) -> Result<SyncStats, DiffError> {
    let target = open_repo(store, target)?;
    let mut target_index = store::read_content_index(&target.db)?;

    let source_entries = &source.entries;
    let source_root = source.root.clone();
    let source_name = source.name.clone();
    let target_root = PathBuf::from(&target.meta.abs_path);
    let mut stats = SyncStats::default();

    // Content the source currently holds (non-missing, filter-matched): the
    // reference set for an Absent (mirror) delete, which removes any target
    // content outside it.
    let source_present = source.present_content();
    // Live target files (filter-matched) — only an Absent delete needs them.
    let target_entries = if delete == SyncDelete::Absent {
        collect_source_entries(&target.db, filter, false)?
    } else {
        Vec::new()
    };

    // Exact denominator for the live progress: how many copies + deletes the
    // chosen mode will attempt (equal/occupied entries are excluded).
    let copy_total = if copy_new {
        source_entries
            .iter()
            .filter(|(rel, e)| {
                !e.missing
                    && !target_index
                        .get(&(e.size, e.hash))
                        .is_some_and(|s| s.present)
                    && run.selected_source(rel)
            })
            .count()
    } else {
        0
    };
    let delete_total = match delete {
        SyncDelete::None => 0,
        SyncDelete::Missing => source_entries
            .iter()
            .filter(|(_, e)| {
                e.missing
                    && target_index
                        .get(&(e.size, e.hash))
                        .is_some_and(|s| s.present)
            })
            .count(),
        SyncDelete::Absent => target_entries
            .iter()
            .filter(|(rel, e)| {
                !source_present.contains(&(e.size, e.hash)) && run.selected_target(rel)
            })
            .count(),
    };
    let total = (copy_total + delete_total) as u64;
    let mut done = 0u64;

    // Mirror deletes first so a copy can reclaim a path a delete frees (the
    // target may hold different content at a source path); other modes' deletes
    // never free a path a copy wants, so they run after the copy.
    if delete == SyncDelete::Absent {
        delete_absent(
            &target,
            &target_root,
            &target_entries,
            &source_present,
            &mut stats,
            &mut done,
            total,
            run,
        )?;
    }

    if copy_new && !stats.cancelled {
        for (rel_path, entry) in source_entries {
            if entry.missing || !run.selected_source(rel_path) {
                continue;
            }
            if run.cancel.is_cancelled() {
                stats.cancelled = true;
                break;
            }
            if sync_copy(
                &source_root,
                &source_name,
                &target,
                &target_root,
                &mut target_index,
                rel_path,
                entry,
                &mut stats,
                run,
            )? {
                done += 1;
                run.progress.on(DiffEvent::Progress {
                    action: DiffAction::Copy,
                    done,
                    total,
                    rel_path: rel_path.clone(),
                });
            }
        }
    }

    if delete == SyncDelete::Missing && !stats.cancelled {
        for (_, entry) in source_entries.iter().filter(|(_, e)| e.missing) {
            if run.cancel.is_cancelled() {
                stats.cancelled = true;
                break;
            }
            let deleted = sync_delete(
                &target,
                &target_root,
                &mut target_index,
                entry,
                &mut stats,
                run,
            )?;
            // One progress step per acting entry (not per deleted path), so
            // `done` never overshoots `total`.
            if let Some(first) = deleted.first() {
                done += 1;
                run.progress.on(DiffEvent::Progress {
                    action: DiffAction::Delete,
                    done,
                    total,
                    rel_path: first.clone(),
                });
            }
        }
    }

    Ok(stats)
}

/// Delete every live target file whose content is not in `source_present` (the
/// source's current content set) — the [`SyncDelete::Absent`] mirror delete.
/// Best effort: failures are counted and reported, the mirror continues.
#[allow(clippy::too_many_arguments)]
fn delete_absent(
    target: &OpenRepo,
    target_root: &Path,
    target_entries: &[(String, FileEntry)],
    source_present: &HashSet<ContentKey>,
    stats: &mut SyncStats,
    done: &mut u64,
    total: u64,
    run: &DiffRun<'_>,
) -> Result<(), StoreError> {
    for (rel_path, entry) in target_entries {
        if run.cancel.is_cancelled() {
            stats.cancelled = true;
            break;
        }
        if source_present.contains(&(entry.size, entry.hash)) || !run.selected_target(rel_path) {
            continue;
        }
        let path = target_root.join(rel_path);
        let removed = match std::fs::remove_file(&path) {
            Ok(()) => true,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
            Err(err) => {
                stats.errors += 1;
                run.progress.on(DiffEvent::Error {
                    path: path.to_string_lossy().into_owned(),
                    message: err.to_string(),
                });
                false
            }
        };
        if removed {
            store::mark_missing(&target.db, std::iter::once(rel_path.as_str()))?;
            stats.deleted += 1;
            *done += 1;
            run.progress.on(DiffEvent::Progress {
                action: DiffAction::Delete,
                done: *done,
                total,
                rel_path: rel_path.clone(),
            });
        }
    }
    Ok(())
}

/// A preview of what [`diff_sync`] would do, without touching disk: the
/// source-relative paths to copy into the target and the target-relative paths
/// to delete. Classification mirrors [`diff_sync`] (content compared by
/// size + hash), except the copy list cannot foresee a runtime skip when the
/// target path is already occupied by *different* content.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncPlan {
    /// Source-relative paths whose content the target lacks (copied to the
    /// same relative path).
    pub copies: Vec<String>,
    /// Target-relative paths the delete phase would remove (per the chosen
    /// [`SyncDelete`] mode).
    pub deletes: Vec<String>,
}

/// Plan a [`diff_sync`] without changing anything on disk (for a preview /
/// confirmation count). See [`SyncPlan`] and [`SyncDelete`] for the semantics.
pub fn plan_sync(
    store: &Store,
    source: &str,
    target: &str,
    copy_new: bool,
    delete: SyncDelete,
    filter: Option<&str>,
) -> Result<SyncPlan, DiffError> {
    let filter = FileFilter::parse(filter)?;
    let source = SourceView::collect(store, source, &filter)?;
    plan_sync_from(store, &source, target, copy_new, delete, &filter)
}

/// [`plan_sync`] against an already-collected source, so a multi-sink push
/// reads the main once instead of once per sink.
pub fn plan_sync_from(
    store: &Store,
    source: &SourceView,
    target: &str,
    copy_new: bool,
    delete: SyncDelete,
    filter: &FileFilter,
) -> Result<SyncPlan, DiffError> {
    let target = open_repo(store, target)?;
    let target_index = store::read_content_index(&target.db)?;

    let source_entries = &source.entries;
    let mut plan = SyncPlan::default();

    if copy_new {
        for (rel_path, entry) in source_entries {
            if entry.missing {
                continue;
            }
            let present = target_index
                .get(&(entry.size, entry.hash))
                .is_some_and(|s| s.present);
            if !present {
                plan.copies.push(rel_path.clone());
            }
        }
    }

    match delete {
        SyncDelete::None => {}
        SyncDelete::Missing => {
            for (_, entry) in source_entries.iter().filter(|(_, e)| e.missing) {
                let present = target_index
                    .get(&(entry.size, entry.hash))
                    .is_some_and(|s| s.present);
                if present {
                    for p in store::get_paths_by_size_hash(&target.db, entry.size, &entry.hash)? {
                        plan.deletes.push(p);
                    }
                }
            }
        }
        SyncDelete::Absent => {
            let source_present = source.present_content();
            for (rel_path, entry) in collect_source_entries(&target.db, filter, false)? {
                if !source_present.contains(&(entry.size, entry.hash)) {
                    plan.deletes.push(rel_path);
                }
            }
        }
    }
    Ok(plan)
}

/// Copy one source file into the target if the target lacks its content and
/// its path is free. Returns `true` when a file was actually copied (so the
/// caller can emit one progress step); `equal`/`skipped`/`errors` are folded
/// into `stats`, and any I/O failure is reported through `run` best-effort
/// (the sync continues).
#[allow(clippy::too_many_arguments)]
fn sync_copy(
    source_root: &Path,
    source_name: &str,
    target: &OpenRepo,
    target_root: &Path,
    target_index: &mut HashMap<ContentKey, ContentState>,
    rel_path: &str,
    entry: &FileEntry,
    stats: &mut SyncStats,
    run: &DiffRun<'_>,
) -> Result<bool, StoreError> {
    let key = (entry.size, entry.hash);
    if target_index.get(&key).is_some_and(|state| state.present) {
        stats.equal += 1;
        return Ok(false);
    }

    let target_file = target_root.join(rel_path);
    if target_file.exists() {
        stats.skipped += 1;
        return Ok(false);
    }
    if let Some(parent) = target_file.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        stats.errors += 1;
        run.progress.on(DiffEvent::Error {
            path: target_file.to_string_lossy().into_owned(),
            message: err.to_string(),
        });
        return Ok(false);
    }
    let source_file = source_root.join(rel_path);
    if let Err(err) = std::fs::copy(&source_file, &target_file) {
        stats.errors += 1;
        run.progress.on(DiffEvent::Error {
            path: target_file.to_string_lossy().into_owned(),
            message: err.to_string(),
        });
        return Ok(false);
    }
    // Best effort, before the entry below reads the mtime back off disk.
    let _ = crate::update::copy_mtime(&source_file, &target_file);
    stats.copied += 1;

    // Index the copy with the mtime the file actually has on the target so
    // the next update run sees it as unchanged.
    let new_entry = entry_for_copied_file(entry, &target_file, source_name);
    store::apply_entries(&target.db, std::iter::once((rel_path, &new_entry)))?;
    target_index.entry(key).or_default().present = true;
    Ok(true)
}

/// Delete every target file whose content matches a missing source entry.
/// Returns the target-relative paths actually removed (folded into
/// `stats.deleted`); failures are reported through `run` best-effort.
fn sync_delete(
    target: &OpenRepo,
    target_root: &Path,
    target_index: &mut HashMap<ContentKey, ContentState>,
    entry: &FileEntry,
    stats: &mut SyncStats,
    run: &DiffRun<'_>,
) -> Result<Vec<String>, StoreError> {
    let key = (entry.size, entry.hash);
    if !target_index.get(&key).is_some_and(|state| state.present) {
        return Ok(Vec::new());
    }
    let mut removed_paths = Vec::new();
    // Paths the review selection excludes stay on disk; if any remain, the
    // content is still present in the target and the index must say so.
    let mut kept = 0usize;
    for rel_path in store::get_paths_by_size_hash(&target.db, entry.size, &entry.hash)? {
        if !run.selected_target(&rel_path) {
            kept += 1;
            continue;
        }
        let path = target_root.join(&rel_path);
        let removed = match std::fs::remove_file(&path) {
            Ok(()) => true,
            // NotFound counts as deleted: the file is gone either way.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
            Err(err) => {
                stats.errors += 1;
                run.progress.on(DiffEvent::Error {
                    path: path.to_string_lossy().into_owned(),
                    message: err.to_string(),
                });
                false
            }
        };
        if removed {
            store::mark_missing(&target.db, std::iter::once(rel_path.as_str()))?;
            stats.deleted += 1;
            removed_paths.push(rel_path);
        }
    }
    if kept == 0
        && let Some(state) = target_index.get_mut(&key)
    {
        state.present = false;
        state.missing = true;
    }
    Ok(removed_paths)
}

/// Build the index entry for a file that was just copied to `path`: the
/// original content entry with its `missing` flag cleared and `modified_ms`
/// set to the file's real on-disk mtime, so a later scan sees the copy as
/// unchanged. Falls back to the source entry's mtime if the target's cannot
/// be read.
fn entry_for_copied_file(entry: &FileEntry, path: &Path, origin: &str) -> FileEntry {
    let modified_ms = std::fs::metadata(path)
        .and_then(|md| md.modified())
        .map(crate::update::system_time_to_ms)
        .unwrap_or(entry.modified_ms);
    let mut new_entry = entry.clone();
    new_entry.missing = false;
    new_entry.modified_ms = modified_ms;
    // Provenance: record which repo the file came from (unless the source
    // already carried an origin, which we preserve through further copies).
    if new_entry.origin.is_none() {
        new_entry.origin = Some(origin.to_string());
    }
    new_entry
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
            // Cross-device move: copy, then remove the source. (A plain rename
            // keeps the timestamps; a copy does not, hence `copy_mtime`.)
            std::fs::copy(from, to).map_err(|e| io_err("move", e))?;
            // Best effort: a filesystem that refuses the timestamp must not
            // fail an otherwise-complete transfer — the index records the
            // file's real on-disk mtime either way.
            let _ = crate::update::copy_mtime(from, to);
            std::fs::remove_file(from).map_err(|e| io_err("move", e))?;
        }
    } else {
        std::fs::copy(from, to).map_err(|e| io_err("copy", e))?;
        let _ = crate::update::copy_mtime(from, to);
    }
    Ok(())
}

/// How a folder export groups the source to decide which copies are redundant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FolderMode {
    /// Group by exact content (size + BLAKE3 hash).
    Exact,
    /// Group by perceptual similarity at the given threshold percentage
    /// (matching the Duplicates tab's similarity slider).
    Similar { threshold: f64 },
}

/// Plan a folder export: the source-relative paths that would be copied/moved
/// into an export folder, in deterministic (rel-path) order.
///
/// The candidate set is the source's non-missing files that pass `filter` and
/// whose content none of the `references` already holds (present *or* missing,
/// matching [`diff_copy`]'s notion of "already known"). Those candidates are
/// then split by exact/similar grouping of the *source repo*: with
/// `invert == false` the export keeps the **unique** files — each group's best
/// copy plus every ungrouped singleton; with `invert == true` it keeps the
/// **redundant** copies — every non-best member of a group.
pub fn plan_folder_export(
    store: &Store,
    source: &str,
    references: &[&str],
    mode: FolderMode,
    invert: bool,
    filter: Option<&str>,
) -> Result<Vec<String>, DiffError> {
    let filter = FileFilter::parse(filter)?;
    let source_open = open_repo(store, source)?;

    // Content the reference repos already hold is excluded up front.
    let ref_index = if references.is_empty() {
        HashMap::new()
    } else {
        open_references(store, references)?.1
    };
    let mut candidates: Vec<String> = collect_source_entries(&source_open.db, &filter, false)?
        .into_iter()
        .filter(|(_, entry)| !ref_index.contains_key(&(entry.size, entry.hash)))
        .map(|(rel_path, _)| rel_path)
        .collect();

    // The non-best members of each exact/similar group within the source repo
    // are the "redundant" copies; every group is sorted best-copy-first.
    let source_names = [source.to_string()];
    let groups = match mode {
        FolderMode::Exact => crate::dupes::find_exact_duplicates(store, &source_names)?,
        FolderMode::Similar { threshold } => {
            crate::similar::find_similar(store, &source_names, threshold, None)?
        }
    };
    let redundant: HashSet<String> = groups
        .iter()
        .flat_map(|group| group.iter().skip(1))
        .map(|file| file.rel_path.clone())
        .collect();

    // Uniques keep the candidates that are not a redundant copy; the inverted
    // export keeps only the redundant copies.
    candidates.retain(|rel_path| redundant.contains(rel_path) == invert);
    Ok(candidates)
}

/// Copy (or move) the source's exported files (see [`plan_folder_export`]) into
/// `dest_dir`, preserving each file's source-relative path. Nothing is indexed
/// into a repo — the destination is a plain directory — but a move marks the
/// exported source entries missing (batched, with a final flush applied even on
/// cancel or failure). Per-file progress is reported through `run`.
#[allow(clippy::too_many_arguments)]
pub fn export_to_folder(
    store: &Store,
    source: &str,
    references: &[&str],
    dest_dir: &Path,
    mode: FolderMode,
    invert: bool,
    move_files: bool,
    filter: Option<&str>,
    run: &DiffRun<'_>,
) -> Result<CopyStats, DiffError> {
    let exports: Vec<String> = plan_folder_export(store, source, references, mode, invert, filter)?
        .into_iter()
        .filter(|rel| run.selected_source(rel))
        .collect();
    let source_open = open_repo(store, source)?;
    let source_root = PathBuf::from(&source_open.meta.abs_path);
    let total = exports.len() as u64;
    let action = if move_files {
        DiffAction::Move
    } else {
        DiffAction::Copy
    };
    let mut stats = CopyStats::default();
    let mut moved: Vec<String> = Vec::new();
    let mut since_flush = 0u64;
    let mut failure: Option<DiffError> = None;

    for rel_path in &exports {
        if run.cancel.is_cancelled() {
            stats.cancelled = true;
            break;
        }
        let from = source_root.join(rel_path);
        let to = dest_dir.join(rel_path);
        if let Err(err) = transfer_file(&from, &to, move_files) {
            run.progress.on(DiffEvent::Error {
                path: from.to_string_lossy().into_owned(),
                message: err.to_string(),
            });
            failure = Some(err);
            break;
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
            flush_moved(&source_open.db, &mut moved)?;
            since_flush = 0;
        }
    }

    // Reflect any moves already done on disk, even on cancel or failure.
    flush_moved(&source_open.db, &mut moved)?;

    match failure {
        Some(err) => Err(err),
        None => Ok(stats),
    }
}

/// Mark the buffered moved source paths missing in one write transaction, then
/// clear the buffer.
fn flush_moved(source_db: &redb::Database, moved: &mut Vec<String>) -> Result<(), StoreError> {
    if !moved.is_empty() {
        store::mark_missing(source_db, moved.iter().map(String::as_str))?;
        moved.clear();
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Manual two-repo diff (the Transfer tab's DIFF command)
// ---------------------------------------------------------------------------

/// How a manual two-repo diff pairs the two sides' files up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffPairing {
    /// Pair by content identity (size + BLAKE3 hash) — paths never matter, so
    /// the same photo under two names is one row.
    ByHash,
    /// Pair by repo-relative path — the same name on both sides is one row,
    /// even when the contents differ.
    ByPath,
}

/// One file on one side of a [`RepoDiffRow`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffFile {
    pub rel_path: String,
    pub size: u64,
    pub modified_ms: i64,
}

/// What the two sides of a [`RepoDiffRow`] say about each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffRelation {
    /// Both sides hold this file, under the same name(s): nothing to do.
    Equal,
    /// Both sides hold the same content under different names (hash pairing) —
    /// resolved by renaming one side to the other's name.
    Renamed,
    /// Both sides hold different content under the same name (path pairing) —
    /// resolved by overwriting one side with the other, or deleting one.
    Conflict,
    /// Only the left side has it: copy it right, or delete it left.
    OnlyLeft,
    /// Only the right side has it: copy it left, or delete it right.
    OnlyRight,
    /// A guess: only-left and only-right files whose *names* look like the
    /// same thing (see [`merge_by_name`]), offered so a re-encoded or renamed
    /// twin can be settled like a conflict. `score` is the name similarity in
    /// percent. Never acted on in bulk — every row is judged by eye.
    Probable { score: u8 },
}

/// One row of a manual two-repo diff: everything each side holds for one
/// pairing key. With [`DiffPairing::ByHash`] a side can hold the same content
/// under several names (all listed, so the UI can narrow them down); with
/// [`DiffPairing::ByPath`] a side holds at most one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoDiffRow {
    pub relation: DiffRelation,
    pub left: Vec<DiffFile>,
    pub right: Vec<DiffFile>,
    /// An `OnlyLeft` row's content survives in the *right* repo only as
    /// tombstones — the right side once held exactly this and deleted it, so
    /// copying it right would resurrect a deletion.
    pub deleted_in_right: bool,
    /// The mirror for `OnlyRight` rows against the left repo.
    pub deleted_in_left: bool,
}

impl RepoDiffRow {
    /// The row's sort key: the first path either side offers.
    fn sort_key(&self) -> &str {
        self.left
            .first()
            .or_else(|| self.right.first())
            .map(|f| f.rel_path.as_str())
            .unwrap_or_default()
    }
}

/// Compare two repos file by file, in one pass over each index, and return the
/// rows a manual diff shows — both directions at once, ordered by path.
///
/// Missing entries are ignored: the diff describes what is on disk right now.
/// Equal rows are included (the UI hides them by default) so the caller can
/// report true totals.
pub fn plan_repo_diff(
    store: &Store,
    left: &str,
    right: &str,
    pairing: DiffPairing,
) -> Result<Vec<RepoDiffRow>, DiffError> {
    let left_repo = open_repo(store, left)?;
    let right_repo = open_repo(store, right)?;
    let mut rows = match pairing {
        DiffPairing::ByHash => rows_by_hash(&left_repo.db, &right_repo.db)?,
        DiffPairing::ByPath => rows_by_path(&left_repo.db, &right_repo.db)?,
    };
    rows.sort_by(|a, b| {
        a.sort_key()
            .to_lowercase()
            .cmp(&b.sort_key().to_lowercase())
            .then_with(|| a.sort_key().cmp(b.sort_key()))
    });
    Ok(rows)
}

/// Collect one repo's live files, keyed by content, as diff-ready files.
fn live_files_by_content(
    db: &redb::Database,
) -> Result<HashMap<ContentKey, Vec<DiffFile>>, StoreError> {
    let mut by_content: HashMap<ContentKey, Vec<DiffFile>> = HashMap::new();
    store::for_each_file_entry(db, |rel_path, entry| {
        if !entry.missing {
            by_content
                .entry((entry.size, entry.hash))
                .or_default()
                .push(DiffFile {
                    rel_path: rel_path.to_string(),
                    size: entry.size,
                    modified_ms: entry.modified_ms,
                });
        }
        Ok(())
    })?;
    for paths in by_content.values_mut() {
        paths.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    }
    Ok(by_content)
}

/// Pair by content: one row per content key either side holds.
fn rows_by_hash(
    left_db: &redb::Database,
    right_db: &redb::Database,
) -> Result<Vec<RepoDiffRow>, StoreError> {
    let mut left = live_files_by_content(left_db)?;
    let right = live_files_by_content(right_db)?;
    // Tombstone knowledge for the one-sided rows (one pass per side).
    let left_idx = store::read_content_index(left_db)?;
    let right_idx = store::read_content_index(right_db)?;
    let mut rows = Vec::new();
    for (key, right_files) in right {
        let left_files = left.remove(&key).unwrap_or_default();
        let mut row = hash_row(left_files, right_files);
        if row.relation == DiffRelation::OnlyRight {
            row.deleted_in_left = store::content_tombstoned(&left_idx, key.0, &key.1);
        }
        rows.push(row);
    }
    // Whatever the right side never had is left-only — or had and deleted.
    for (key, left_files) in left {
        let mut row = hash_row(left_files, Vec::new());
        row.deleted_in_right = store::content_tombstoned(&right_idx, key.0, &key.1);
        rows.push(row);
    }
    Ok(rows)
}

/// Classify one content key's two path lists.
fn hash_row(left: Vec<DiffFile>, right: Vec<DiffFile>) -> RepoDiffRow {
    let relation = match (left.is_empty(), right.is_empty()) {
        (false, true) => DiffRelation::OnlyLeft,
        (true, false) => DiffRelation::OnlyRight,
        // Identical name(s) on both sides: there is nothing left to reconcile.
        // Any difference (a rename, or a duplicate on one side) is a Renamed
        // row the UI narrows down action by action.
        _ if left
            .iter()
            .map(|f| &f.rel_path)
            .eq(right.iter().map(|f| &f.rel_path)) =>
        {
            DiffRelation::Equal
        }
        _ => DiffRelation::Renamed,
    };
    RepoDiffRow {
        relation,
        left,
        right,
        deleted_in_right: false,
        deleted_in_left: false,
    }
}

/// Pair by path: one row per relative path either side holds.
fn rows_by_path(
    left_db: &redb::Database,
    right_db: &redb::Database,
) -> Result<Vec<RepoDiffRow>, StoreError> {
    // Path → (file, content key) per side; the key decides equal vs conflict.
    let live =
        |db: &redb::Database| -> Result<HashMap<String, (DiffFile, ContentKey)>, StoreError> {
            let mut files = HashMap::new();
            store::for_each_file_entry(db, |rel_path, entry| {
                if !entry.missing {
                    files.insert(
                        rel_path.to_string(),
                        (
                            DiffFile {
                                rel_path: rel_path.to_string(),
                                size: entry.size,
                                modified_ms: entry.modified_ms,
                            },
                            (entry.size, entry.hash),
                        ),
                    );
                }
                Ok(())
            })?;
            Ok(files)
        };
    let mut left = live(left_db)?;
    let right = live(right_db)?;
    let left_idx = store::read_content_index(left_db)?;
    let right_idx = store::read_content_index(right_db)?;
    let mut rows = Vec::new();
    for (path, (right_file, right_key)) in right {
        match left.remove(&path) {
            Some((left_file, left_key)) => rows.push(RepoDiffRow {
                relation: if left_key == right_key {
                    DiffRelation::Equal
                } else {
                    DiffRelation::Conflict
                },
                left: vec![left_file],
                right: vec![right_file],
                deleted_in_right: false,
                deleted_in_left: false,
            }),
            None => rows.push(RepoDiffRow {
                relation: DiffRelation::OnlyRight,
                left: Vec::new(),
                right: vec![right_file],
                deleted_in_right: false,
                deleted_in_left: store::content_tombstoned(&left_idx, right_key.0, &right_key.1),
            }),
        }
    }
    for (_, (left_file, left_key)) in left {
        rows.push(RepoDiffRow {
            relation: DiffRelation::OnlyLeft,
            left: vec![left_file],
            right: Vec::new(),
            deleted_in_right: store::content_tombstoned(&right_idx, left_key.0, &left_key.1),
            deleted_in_left: false,
        });
    }
    Ok(rows)
}

/// Single-file operations the manual diff's row actions execute. Each one
/// touches disk *and* both repo indexes, so the diff can be re-planned right
/// after without a rescan.
///
/// They are deliberately strict: nothing is overwritten unless the caller says
/// so explicitly ([`overwrite_file`]), and an operation that cannot be carried
/// out exactly as asked fails instead of guessing.
#[derive(thiserror::Error, Debug)]
pub enum DiffOpError {
    #[error(transparent)]
    Store(#[from] StoreError),

    #[error("Could not {action} '{path}': {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("'{path}' does not exist in repository '{repo}'")]
    NoSuchFile { repo: String, path: String },

    #[error("'{path}' already exists in repository '{repo}'")]
    AlreadyExists { repo: String, path: String },

    #[error("'{path}' is not a valid repository-relative path")]
    InvalidPath { path: String },
}

/// Resolve a repo-relative path to an absolute one, refusing anything that
/// would escape the repo root (absolute components or `..`) or is empty. Shares
/// its escape rule with [`resolve_subdir`] via [`stays_within_root`].
fn resolve_in_repo(root: &Path, rel_path: &str) -> Result<PathBuf, DiffOpError> {
    let rel = Path::new(rel_path);
    if rel_path.trim().is_empty() || !stays_within_root(rel) {
        return Err(DiffOpError::InvalidPath {
            path: rel_path.to_string(),
        });
    }
    Ok(root.join(rel))
}

/// Rename one file inside a repo — on disk and in the index — keeping its
/// content identity (and therefore its fingerprints) untouched.
///
/// Fails if the source is unknown or the destination path is already taken,
/// so a rename can never silently swallow another file.
pub fn rename_file(
    store: &Store,
    repo: &str,
    from_rel: &str,
    to_rel: &str,
) -> Result<(), DiffOpError> {
    if from_rel == to_rel {
        return Ok(());
    }
    let open = open_repo(store, repo)?;
    let root = PathBuf::from(&open.meta.abs_path);
    let from = resolve_in_repo(&root, from_rel)?;
    let to = resolve_in_repo(&root, to_rel)?;
    let Some(entry) = store::get_entry(&open.db, from_rel)?.filter(|e| !e.missing) else {
        return Err(DiffOpError::NoSuchFile {
            repo: repo.to_string(),
            path: from_rel.to_string(),
        });
    };
    if to.exists() {
        return Err(DiffOpError::AlreadyExists {
            repo: repo.to_string(),
            path: to_rel.to_string(),
        });
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).map_err(|source| DiffOpError::Io {
            action: "create directory",
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::rename(&from, &to).map_err(|source| DiffOpError::Io {
        action: "rename",
        path: from.clone(),
        source,
    })?;
    // The content did not change, so the entry moves over as it is; the old
    // path is dropped outright rather than left behind as missing (the file
    // was not lost, it just has another name now). Both index writes happen in
    // one transaction, so a crash can never leave the content under both names.
    store::rename_entry(&open.db, from_rel, to_rel, &entry)?;
    Ok(())
}

/// Copy one file from one repo into another, at `to_rel`, and index it in the
/// target. The copy keeps the source's modification time, and records the
/// source repo as its origin (like every other transfer).
///
/// Refuses to touch an occupied destination — use [`overwrite_file`] to
/// replace one deliberately.
pub fn copy_file_between(
    store: &Store,
    from_repo: &str,
    from_rel: &str,
    to_repo: &str,
    to_rel: &str,
) -> Result<(), DiffOpError> {
    copy_into(store, from_repo, from_rel, to_repo, to_rel, false)
}

/// Replace the target file with the other side's content: the same as
/// [`copy_file_between`], except an existing destination is overwritten.
pub fn overwrite_file(
    store: &Store,
    from_repo: &str,
    from_rel: &str,
    to_repo: &str,
    to_rel: &str,
) -> Result<(), DiffOpError> {
    copy_into(store, from_repo, from_rel, to_repo, to_rel, true)
}

fn copy_into(
    store: &Store,
    from_repo: &str,
    from_rel: &str,
    to_repo: &str,
    to_rel: &str,
    overwrite: bool,
) -> Result<(), DiffOpError> {
    let source = open_repo(store, from_repo)?;
    let target = open_repo(store, to_repo)?;
    let from = resolve_in_repo(&PathBuf::from(&source.meta.abs_path), from_rel)?;
    let to = resolve_in_repo(&PathBuf::from(&target.meta.abs_path), to_rel)?;
    // A copy onto itself is complete before it starts — and must not reach
    // `fs::copy`, which would truncate the file it is about to read.
    if from == to {
        return Ok(());
    }
    let Some(entry) = store::get_entry(&source.db, from_rel)?.filter(|e| !e.missing) else {
        return Err(DiffOpError::NoSuchFile {
            repo: from_repo.to_string(),
            path: from_rel.to_string(),
        });
    };
    if !overwrite && to.exists() {
        return Err(DiffOpError::AlreadyExists {
            repo: to_repo.to_string(),
            path: to_rel.to_string(),
        });
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).map_err(|source| DiffOpError::Io {
            action: "create directory",
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::copy(&from, &to).map_err(|source| DiffOpError::Io {
        action: "copy",
        path: from.clone(),
        source,
    })?;
    let _ = crate::update::copy_mtime(&from, &to);
    let new_entry = entry_for_copied_file(&entry, &to, from_repo);
    store::apply_entries(&target.db, std::iter::once((to_rel, &new_entry)))?;
    Ok(())
}

/// Delete one file from a repo: remove it from disk and mark its index entry
/// missing (the repo still remembers it once held that content, exactly like a
/// batch delete).
pub fn delete_file(store: &Store, repo: &str, rel_path: &str) -> Result<(), DiffOpError> {
    let open = open_repo(store, repo)?;
    let path = resolve_in_repo(&PathBuf::from(&open.meta.abs_path), rel_path)?;
    if store::get_entry(&open.db, rel_path)?.is_none() {
        return Err(DiffOpError::NoSuchFile {
            repo: repo.to_string(),
            path: rel_path.to_string(),
        });
    }
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        // Already gone counts as deleted — the index still has to catch up.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(DiffOpError::Io {
                action: "delete",
                path,
                source,
            });
        }
    }
    store::mark_missing(&open.db, std::iter::once(rel_path))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// MERGE REST BY NAME: pairing the unmatched leftovers of a diff by filename.

/// The least name similarity (percent) at which two leftovers are paired.
/// Below it a guess is noise.
const NAME_MATCH_FLOOR: u8 = 60;

/// One normalized filename token. `weight` is 3 for the tokens that carry the
/// identity of a numbered or catalogued file — digit runs and mixed
/// letter-digit codes such as an ASIN or ISBN — and 1 for plain words, so
/// "chapter 5" and "chapter 6" are far apart however long their shared title.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NameToken {
    pub text: String,
    pub weight: u8,
}

/// Fold a character towards ASCII: fullwidth forms (`Ａ`, `１`, `：`) and the
/// modifier colon `꞉` (U+A789) that appears in filenames where a real colon
/// is not allowed.
fn fold_char(c: char) -> char {
    match c {
        '\u{FF01}'..='\u{FF5E}' => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
        '\u{A789}' => ':',
        _ => c,
    }
}

/// Break a filename stem into comparable tokens: lowercased, folded to ASCII
/// where a lookalike exists, split at every non-alphanumeric character (so
/// `꞉`, `:`, `-`, `_` and spaces all separate alike), digit runs stripped of
/// leading zeros (`005` is `5`). The extension is not stripped here — pass a
/// stem.
pub fn normalize_stem(stem: &str) -> Vec<NameToken> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let flush = |current: &mut String, tokens: &mut Vec<NameToken>| {
        if current.is_empty() {
            return;
        }
        let all_digits = current.chars().all(|c| c.is_ascii_digit());
        let has_digit = current.chars().any(|c| c.is_ascii_digit());
        let text = if all_digits {
            let trimmed = current.trim_start_matches('0');
            if trimmed.is_empty() { "0" } else { trimmed }.to_string()
        } else {
            std::mem::take(current)
        };
        current.clear();
        tokens.push(NameToken {
            weight: if has_digit { 3 } else { 1 },
            text,
        });
    };
    for c in stem.chars().map(fold_char) {
        if c.is_alphanumeric() {
            current.extend(c.to_lowercase());
        } else {
            flush(&mut current, &mut tokens);
        }
    }
    flush(&mut current, &mut tokens);
    tokens
}

/// How alike two filename stems are, in percent: 100 when they normalize to
/// the same token sequence, otherwise the weighted overlap of their token sets
/// (Jaccard, with [`NameToken::weight`]s). Folders and extensions are the
/// caller's business — pass bare stems.
pub fn name_similarity(a: &str, b: &str) -> u8 {
    similarity_tokens(&normalize_stem(a), &normalize_stem(b))
}

/// [`name_similarity`] over stems already normalized — the merge tokenizes
/// each name once and compares many times.
fn similarity_tokens(ta: &[NameToken], tb: &[NameToken]) -> u8 {
    if ta.is_empty() || tb.is_empty() {
        return 0;
    }
    if ta == tb {
        return 100;
    }
    let set_a: HashSet<&NameToken> = ta.iter().collect();
    let set_b: HashSet<&NameToken> = tb.iter().collect();
    let weight = |t: &&NameToken| u32::from(t.weight);
    let shared: u32 = set_a.intersection(&set_b).map(weight).sum();
    let all: u32 = set_a.union(&set_b).map(weight).sum();
    if all == 0 {
        return 0;
    }
    let score = (shared * 100 / all) as u8;
    // The numbers and codes in a name *are* its identity — the chapter, the
    // catalogue id, the counter. Two names that carry them but not the same
    // ones in the same order (chapter 5 of book 6 is not chapter 6 of book 5)
    // are different items however long the title they share, so they never
    // reach the pairing floor.
    if identity_key(ta) != identity_key(tb) {
        return score.min(NAME_MATCH_FLOOR - 10);
    }
    score
}

/// A name's identity tokens (numbers and codes, weight 3) in order, joined
/// into one key; empty for a name of plain words.
fn identity_key(tokens: &[NameToken]) -> String {
    let mut key = String::new();
    for t in tokens.iter().filter(|t| t.weight > 1) {
        key.push_str(&t.text);
        key.push('\u{1f}');
    }
    key
}

/// The filename stem of a repo-relative path: the last component without its
/// extension. The folder is deliberately ignored — two editions of the same
/// thing rarely share one.
fn stem_of(rel_path: &str) -> &str {
    let name = rel_path.rsplit('/').next().unwrap_or(rel_path);
    match name.rfind('.') {
        Some(dot) if dot > 0 => &name[..dot],
        _ => name,
    }
}

/// One side's names, tokenized once (a BY HASH side can hold several names
/// for one content).
type SideNames = Vec<Vec<NameToken>>;

fn side_names(files: &[DiffFile]) -> SideNames {
    files
        .iter()
        .map(|f| normalize_stem(stem_of(&f.rel_path)))
        .collect()
}

/// The best name similarity between any name on one side and any on the other.
fn side_similarity(left: &SideNames, right: &SideNames) -> u8 {
    let mut best = 0;
    for l in left {
        for r in right {
            best = best.max(similarity_tokens(l, r));
        }
    }
    best
}

/// The keys a side is indexed under. A pair can only form between names with
/// the same [`identity_key`], so that key finds every candidate exactly; a
/// name of plain words (empty key) is indexed under each of its words
/// instead, so it is compared only with names sharing one.
fn index_keys(names: &SideNames) -> Vec<String> {
    let mut out = Vec::new();
    for tokens in names {
        let key = identity_key(tokens);
        if key.is_empty() {
            out.extend(tokens.iter().map(|t| t.text.clone()));
        } else {
            out.push(key);
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// The identity of a pair for the exclusion set: the first path on each side.
pub fn pair_key(row: &RepoDiffRow) -> (String, String) {
    pair_key_of(&row.left, &row.right)
}

fn pair_key_of(left: &[DiffFile], right: &[DiffFile]) -> (String, String) {
    (
        left.first().map(|f| f.rel_path.clone()).unwrap_or_default(),
        right
            .first()
            .map(|f| f.rel_path.clone())
            .unwrap_or_default(),
    )
}

/// Pair the leftovers of a diff by name: among the `eligible` rows, every
/// [`DiffRelation::OnlyLeft`] is scored against every [`DiffRelation::OnlyRight`]
/// sharing a strong token, and a pair forms when each is the other's single
/// best match at or above [`NAME_MATCH_FLOOR`] — mutual best, ties pair
/// nothing, so one library file is never handed to two orphans. A formed pair
/// becomes one [`DiffRelation::Probable`] row keeping both sides' files and
/// tombstone flags; `excluded` pairs (an UNPAIR the user made) stay split.
/// Every other row passes through untouched, in order.
pub fn merge_by_name(
    rows: Vec<RepoDiffRow>,
    eligible: &[bool],
    excluded: &HashSet<(String, String)>,
) -> Vec<RepoDiffRow> {
    let is = |i: usize, want: DiffRelation| {
        eligible.get(i).copied().unwrap_or(false) && rows[i].relation == want
    };
    let lefts: Vec<usize> = (0..rows.len())
        .filter(|&i| is(i, DiffRelation::OnlyLeft))
        .collect();
    let rights: Vec<usize> = (0..rows.len())
        .filter(|&i| is(i, DiffRelation::OnlyRight))
        .collect();
    if lefts.is_empty() || rights.is_empty() {
        return rows;
    }

    // Each name is tokenized once. The right side is indexed by identity key
    // (see [`index_keys`]), so a left is scored only against the handful of
    // rights it can pair with instead of all of them — tens of thousands a
    // side must stay interactive.
    let left_names: Vec<SideNames> = lefts.iter().map(|&l| side_names(&rows[l].left)).collect();
    let right_names: Vec<SideNames> = rights.iter().map(|&r| side_names(&rows[r].right)).collect();
    let mut by_token: HashMap<String, Vec<usize>> = HashMap::new();
    for (r_pos, names) in right_names.iter().enumerate() {
        for key in index_keys(names) {
            by_token.entry(key).or_default().push(r_pos);
        }
    }

    // Best partner per side: (position on the other side, score, tied).
    let mut best_left: Vec<Option<(usize, u8, bool)>> = vec![None; lefts.len()];
    let mut best_right: Vec<Option<(usize, u8, bool)>> = vec![None; rights.len()];
    let mut seen = vec![usize::MAX; rights.len()];
    for (l_pos, &l) in lefts.iter().enumerate() {
        for key in index_keys(&left_names[l_pos]) {
            let Some(candidates) = by_token.get(&key) else {
                continue;
            };
            for &r_pos in candidates {
                if seen[r_pos] == l_pos {
                    continue;
                }
                seen[r_pos] = l_pos;
                let r = rights[r_pos];
                if excluded.contains(&pair_key_of(&rows[l].left, &rows[r].right)) {
                    continue;
                }
                let score = side_similarity(&left_names[l_pos], &right_names[r_pos]);
                if score < NAME_MATCH_FLOOR {
                    continue;
                }
                for (slot, other) in [
                    (&mut best_left[l_pos], r_pos),
                    (&mut best_right[r_pos], l_pos),
                ] {
                    *slot = match *slot {
                        Some((_, s, _)) if s > score => *slot,
                        Some((_, s, _)) if s == score => Some((other, score, true)),
                        _ => Some((other, score, false)),
                    };
                }
            }
        }
    }

    let mut merged_into: HashMap<usize, usize> = HashMap::new(); // left row → right row
    let mut consumed: HashSet<usize> = HashSet::new(); // right rows folded away
    for (l_pos, &l) in lefts.iter().enumerate() {
        let Some((r_pos, score, false)) = best_left[l_pos] else {
            continue;
        };
        if best_right[r_pos] != Some((l_pos, score, false)) {
            continue;
        }
        merged_into.insert(l, rights[r_pos]);
        consumed.insert(rights[r_pos]);
    }
    if merged_into.is_empty() {
        return rows;
    }

    let mut rows = rows;
    let mut out = Vec::with_capacity(rows.len() - merged_into.len());
    let mut taken: Vec<Option<RepoDiffRow>> = rows.drain(..).map(Some).collect();
    for i in 0..taken.len() {
        if consumed.contains(&i) {
            continue;
        }
        let Some(mut row) = taken[i].take() else {
            continue;
        };
        if let Some(&r) = merged_into.get(&i)
            && let Some(right) = taken[r].take()
        {
            let score = side_similarity(&side_names(&row.left), &side_names(&right.right));
            row.relation = DiffRelation::Probable { score };
            row.right = right.right;
            row.deleted_in_left = right.deleted_in_left;
        }
        out.push(row);
    }
    out
}

/// Split a [`DiffRelation::Probable`] row back into the only-left and
/// only-right rows it was guessed from, in place of it. Any other row is
/// returned unchanged.
pub fn split_probable(row: RepoDiffRow) -> Vec<RepoDiffRow> {
    if !matches!(row.relation, DiffRelation::Probable { .. }) {
        return vec![row];
    }
    vec![
        RepoDiffRow {
            relation: DiffRelation::OnlyLeft,
            left: row.left,
            right: Vec::new(),
            deleted_in_right: row.deleted_in_right,
            deleted_in_left: false,
        },
        RepoDiffRow {
            relation: DiffRelation::OnlyRight,
            left: Vec::new(),
            right: row.right,
            deleted_in_right: false,
            deleted_in_left: row.deleted_in_left,
        },
    ]
}

#[cfg(test)]
mod name_merge_tests {
    use super::*;

    fn file(rel: &str) -> DiffFile {
        DiffFile {
            rel_path: rel.to_string(),
            size: 1,
            modified_ms: 0,
        }
    }

    fn only_left(rel: &str) -> RepoDiffRow {
        RepoDiffRow {
            relation: DiffRelation::OnlyLeft,
            left: vec![file(rel)],
            right: Vec::new(),
            deleted_in_right: false,
            deleted_in_left: false,
        }
    }

    fn only_right(rel: &str) -> RepoDiffRow {
        RepoDiffRow {
            relation: DiffRelation::OnlyRight,
            left: Vec::new(),
            right: vec![file(rel)],
            deleted_in_right: false,
            deleted_in_left: false,
        }
    }

    fn merge_all(rows: Vec<RepoDiffRow>) -> Vec<RepoDiffRow> {
        let eligible = vec![true; rows.len()];
        merge_by_name(rows, &eligible, &HashSet::new())
    }

    fn pairs(rows: &[RepoDiffRow]) -> Vec<(String, String, u8)> {
        rows.iter()
            .filter_map(|r| match r.relation {
                DiffRelation::Probable { score } => {
                    let (l, rr) = pair_key(r);
                    Some((l, rr, score))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn lookalike_punctuation_and_zero_padding_normalize_away() {
        assert_eq!(
            name_similarity(
                "Die Feinde der Zeit꞉ Die Zeit-Saga 3 [B0CNRWX1D8] - 05 - Kapitel 5",
                "Die Feinde der Zeit: Die Zeit-Saga 3 [B0CNRWX1D8] - 005 - Kapitel 5",
            ),
            100
        );
        assert_eq!(name_similarity("Ｔｒａｃｋ ０１", "track 1"), 100);
        assert_eq!(name_similarity("", "x"), 0);
    }

    #[test]
    fn neighbouring_chapters_score_far_below_the_floor_of_a_true_twin() {
        let same = name_similarity(
            "Ilium & Olympos 1 [B004UZGPBO] - 05 - Kapitel 5",
            "Ilium & Olympos 1 [B004UZGPBO] - 05 - Kapitel 5",
        );
        let next = name_similarity(
            "Ilium & Olympos 1 [B004UZGPBO] - 05 - Kapitel 5",
            "Ilium & Olympos 1 [B004UZGPBO] - 06 - Kapitel 6",
        );
        assert_eq!(same, 100);
        assert!(next < NAME_MATCH_FLOOR, "neighbour scored {next}");
        // The same numbers in another order are another item too: chapter 5
        // of volume 6 is not chapter 6 of volume 5, and another catalogue id
        // is another edition.
        let swapped = name_similarity(
            "Hyperion & Endymion 6 [B004V0DHPA] - 05 - Kapitel 5",
            "Hyperion & Endymion 5 [B004V0ABMW] - 06 - Kapitel 6",
        );
        assert!(swapped < NAME_MATCH_FLOOR, "swapped scored {swapped}");
    }

    #[test]
    fn merges_mutual_best_pairs_and_leaves_neighbours_alone() {
        let rows = vec![
            only_left("orphans/Book [B0X] - 05 - Kapitel 5.mp3"),
            only_left("orphans/Book [B0X] - 06 - Kapitel 6.mp3"),
            only_right("lib/Book [B0X] - 005 - Kapitel 5.m4b"),
            only_right("lib/Other [B0Y] - 01 - Chapter 1.m4b"),
            RepoDiffRow {
                relation: DiffRelation::Equal,
                left: vec![file("same.txt")],
                right: vec![file("same.txt")],
                deleted_in_right: false,
                deleted_in_left: false,
            },
        ];
        let merged = merge_all(rows);
        assert_eq!(
            pairs(&merged),
            vec![(
                "orphans/Book [B0X] - 05 - Kapitel 5.mp3".to_string(),
                "lib/Book [B0X] - 005 - Kapitel 5.m4b".to_string(),
                100
            )]
        );
        // Chapter 6 has no twin and must not be handed the chapter-5 file;
        // the unrelated right file and the equal row pass through.
        let relations: Vec<DiffRelation> = merged.iter().map(|r| r.relation).collect();
        assert_eq!(
            relations,
            vec![
                DiffRelation::Probable { score: 100 },
                DiffRelation::OnlyLeft,
                DiffRelation::OnlyRight,
                DiffRelation::Equal,
            ]
        );
    }

    #[test]
    fn a_tie_for_best_pairs_nothing() {
        // Two identical-looking orphans compete for one library file: neither
        // is *the* best, so the guess is withheld rather than made at random.
        let rows = vec![
            only_left("a/Book [B0X] - 05 - Kapitel 5.mp3"),
            only_left("b/Book [B0X] - 05 - Kapitel 5.mp3"),
            only_right("lib/Book [B0X] - 05 - Kapitel 5.m4b"),
        ];
        assert!(pairs(&merge_all(rows)).is_empty());
    }

    #[test]
    fn an_excluded_pair_stays_split_and_hidden_rows_are_untouched() {
        let rows = vec![
            only_left("Book [B0X] - 05 - Kapitel 5.mp3"),
            only_right("Book [B0X] - 05 - Kapitel 5.m4b"),
            only_left("Book [B0X] - 07 - Kapitel 7.mp3"),
            only_right("Book [B0X] - 07 - Kapitel 7.m4b"),
        ];
        let mut excluded = HashSet::new();
        excluded.insert((
            "Book [B0X] - 05 - Kapitel 5.mp3".to_string(),
            "Book [B0X] - 05 - Kapitel 5.m4b".to_string(),
        ));
        // Chapter 7's right row is hidden (not listed), so it is not a candidate.
        let eligible = vec![true, true, true, false];
        let merged = merge_by_name(rows, &eligible, &excluded);
        assert!(pairs(&merged).is_empty(), "{merged:?}");
        assert_eq!(merged.len(), 4);
    }

    #[test]
    fn a_weak_resemblance_is_below_the_floor() {
        let rows = vec![
            only_left("holiday/IMG_2041.jpg"),
            only_right("scans/IMG_2043.jpg"),
        ];
        assert!(pairs(&merge_all(rows)).is_empty());
    }

    #[test]
    fn a_probable_row_splits_back_into_its_two_sides() {
        let merged = merge_all(vec![
            only_left("Book [B0X] - 05 - Kapitel 5.mp3"),
            only_right("Book [B0X] - 05 - Kapitel 5.m4b"),
        ]);
        assert_eq!(merged.len(), 1);
        let back = split_probable(merged.into_iter().next().unwrap_or_else(|| unreachable!()));
        let relations: Vec<DiffRelation> = back.iter().map(|r| r.relation).collect();
        assert_eq!(
            relations,
            vec![DiffRelation::OnlyLeft, DiffRelation::OnlyRight]
        );
        assert_eq!(back[0].left[0].rel_path, "Book [B0X] - 05 - Kapitel 5.mp3");
        assert_eq!(back[1].right[0].rel_path, "Book [B0X] - 05 - Kapitel 5.m4b");
    }
}
