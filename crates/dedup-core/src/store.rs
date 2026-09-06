//! Storage implementation using redb for registry and repo-specific databases.

use redb::{ReadableMultimapTable, ReadableTable};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

const SCHEMA_VERSION: u8 = 1;
/// Version byte of serialized [`SyncGroup`] values. v1 stored a single
/// group-wide `mode`; v2 moved the mode onto each sink ([`SyncSink`]). v1 values
/// decode with every sink inheriting the old group mode. Scoped to sync groups
/// so it can advance independently of [`SCHEMA_VERSION`] (which still governs
/// `RepoMeta` in the same registry).
const SYNC_GROUP_VERSION: u8 = 2;
/// Version byte of serialized [`FileEntry`] values. v2 grew the image
/// fingerprint from a 64-bit dHash to the 512-bit [`ImgHash`] (v1 entries decode
/// but drop the fingerprint and are flagged stale for re-hashing); v3 added
/// `origin` provenance, a decode-compatible change (old entries get `origin =
/// None` and are NOT re-flagged stale); v4 added image `exif`, which requires
/// re-reading image files, so images below v4 are flagged stale; v5 added
/// office-document text hashes (reusing the `pdf_hash` slot), so document files
/// below v5 are flagged stale; v6 added text/CSV hashes, so text files below v6
/// are flagged stale; v7 added `.eml` email hashes (the v4→v7 layout is
/// unchanged); v8 grew the video temporal hash from 64 to 512 bits per frame,
/// so video files below v8 are flagged stale; v9 reclassified MP4-container
/// audio (`.m4b`/`.m4a` sniffed as plain "video/mp4") as audio — the layout is
/// unchanged, and only audio-named "video/mp4" entries below v9 are flagged
/// stale so they re-fingerprint as audio. See [`decode_entry`].
const ENTRY_VERSION: u8 = 9;

// Registry table definition
const REPOS: redb::TableDefinition<&str, &[u8]> = redb::TableDefinition::new("repos");
/// Registry table of sync groups: group name → postcard-encoded [`SyncGroup`].
/// Added after the first release, so a registry written before it simply has
/// no such table and reads back as "no groups".
const SYNC_GROUPS: redb::TableDefinition<&str, &[u8]> = redb::TableDefinition::new("sync_groups");

// Repo-specific table definitions
const FILES: redb::TableDefinition<&str, &[u8]> = redb::TableDefinition::new("files");
const BY_SIZE_HASH: redb::MultimapTableDefinition<(u64, &[u8]), &str> =
    redb::MultimapTableDefinition::new("by_size_hash");
const BY_FPRINT: redb::MultimapTableDefinition<ImgHash, &str> =
    redb::MultimapTableDefinition::new("by_fprint2");
/// Pre-[`ImgHash`] fingerprint index; dropped on repo open.
const BY_FPRINT_LEGACY: redb::MultimapTableDefinition<u64, &str> =
    redb::MultimapTableDefinition::new("by_fprint");
const META: redb::TableDefinition<&str, u64> = redb::TableDefinition::new("meta");
const MIME_STATS: redb::TableDefinition<&str, u64> = redb::TableDefinition::new("mime_stats");
/// Archive rel-path → postcard-encoded `Vec<ArchiveMember>` (opt-in index).
const ARCHIVE_MEMBERS: redb::TableDefinition<&str, &[u8]> =
    redb::TableDefinition::new("archive_members");
/// Archive rel-path → the archive's working password, encrypted at rest behind
/// the app passphrase (see [`crate::secret`]). Never the plaintext.
const ARCHIVE_PASSWORDS: redb::TableDefinition<&str, &[u8]> =
    redb::TableDefinition::new("archive_passwords");
/// File rel-path → encoded `Vec<String>` of free-form user annotation tags
/// (Browse tab). Kept out of `FileEntry` since it's mutable user metadata, not
/// content identity; a file with no tags has no row.
const ANNOTATIONS: redb::TableDefinition<&str, &[u8]> = redb::TableDefinition::new("annotations");
/// Content `(size, hash)` → nothing: contents the user *accepted* as allowed
/// to repeat inside this repo (a podcast's per-folder `cover.jpg`). Keyed like
/// `BY_SIZE_HASH` so the mark follows the content, never a path; an accepted
/// content that later disappears keeps its row (a future copy is accepted
/// again — the whole point). Lazily created; absent → nothing accepted.
const ACCEPTED: redb::TableDefinition<(u64, &[u8]), ()> = redb::TableDefinition::new("accepted");

#[derive(thiserror::Error, Debug)]
pub enum StoreError {
    #[error("Database error: {0}")]
    Database(Box<redb::Error>),

    #[error("Database open error: {0}")]
    DatabaseOpen(Box<redb::DatabaseError>),

    #[error("Database transaction error: {0}")]
    Transaction(Box<redb::TransactionError>),

    #[error("Table error: {0}")]
    Table(Box<redb::TableError>),

    #[error("Storage error: {0}")]
    Storage(Box<redb::StorageError>),

    #[error("Commit error: {0}")]
    Commit(Box<redb::CommitError>),

    #[error("Compaction error: {0}")]
    Compaction(Box<redb::CompactionError>),

    #[error("Storage I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("Deserialization error: {0}")]
    Deserialization(String),

    #[error("Schema version mismatch: expected {expected}, found {found}")]
    SchemaVersionMismatch { expected: u8, found: u8 },

    #[error("Repository '{0}' already exists")]
    AlreadyExists(String),

    #[error("Repository '{0}' not found")]
    NotFound(String),

    #[error("Repository '{0}' is in use by another operation")]
    Busy(String),

    #[error("Sync group '{0}' already exists")]
    GroupExists(String),

    #[error("Sync group '{0}' not found")]
    GroupNotFound(String),

    #[error("Repository '{repo}' is already in sync group '{group}'")]
    AlreadyGrouped { repo: String, group: String },

    #[error("Repository '{repo}' belongs to sync group '{group}' — take it out of the group first")]
    InSyncGroup { repo: String, group: String },

    #[error("Repository '{repo}' is not a member of sync group '{group}'")]
    NotInGroup { repo: String, group: String },
}

impl From<redb::Error> for StoreError {
    fn from(err: redb::Error) -> Self {
        StoreError::Database(Box::new(err))
    }
}

impl From<redb::DatabaseError> for StoreError {
    fn from(err: redb::DatabaseError) -> Self {
        StoreError::DatabaseOpen(Box::new(err))
    }
}

impl From<redb::TransactionError> for StoreError {
    fn from(err: redb::TransactionError) -> Self {
        StoreError::Transaction(Box::new(err))
    }
}

impl From<redb::TableError> for StoreError {
    fn from(err: redb::TableError) -> Self {
        StoreError::Table(Box::new(err))
    }
}

impl From<redb::StorageError> for StoreError {
    fn from(err: redb::StorageError) -> Self {
        StoreError::Storage(Box::new(err))
    }
}

impl From<redb::CommitError> for StoreError {
    fn from(err: redb::CommitError) -> Self {
        StoreError::Commit(Box::new(err))
    }
}

impl From<redb::CompactionError> for StoreError {
    fn from(err: redb::CompactionError) -> Self {
        StoreError::Compaction(Box::new(err))
    }
}

/// Registry value version for [`RepoMeta`]. Postcard is positional, so adding
/// a field is a new version with a [`decode_repo_meta`] migration — exactly
/// the [`decode_sync_group`] discipline. Version 2 added `remote`.
const REPO_META_VERSION: u8 = 2;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct RepoMeta {
    pub abs_path: String,
    pub created: u64,
    pub hash_algo: String,
    pub schema_ver: u8,
    /// User-flagged: this repo's root lives on a slow/remote mount (cloud,
    /// NFS) — bulk actions like "scan all local" skip it. Never auto-detected;
    /// the flag is the user's own judgement.
    pub remote: bool,
}

/// Version-1 [`RepoMeta`] layout (no `remote` flag). Kept so pre-upgrade
/// registries stay readable; see [`decode_repo_meta`].
#[derive(Serialize, Deserialize)]
struct RepoMetaV1 {
    abs_path: String,
    created: u64,
    hash_algo: String,
    schema_ver: u8,
}

/// Decode a stored [`RepoMeta`], migrating a version-1 value (which predates
/// the `remote` flag) to `remote: false`. Re-saving writes the current
/// [`REPO_META_VERSION`].
fn decode_repo_meta(bytes: &[u8]) -> Result<RepoMeta, StoreError> {
    match bytes.first() {
        Some(&REPO_META_VERSION) => deserialize_value(REPO_META_VERSION, bytes),
        Some(&SCHEMA_VERSION) => {
            let v1: RepoMetaV1 = deserialize_value(SCHEMA_VERSION, bytes)?;
            Ok(RepoMeta {
                abs_path: v1.abs_path,
                created: v1.created,
                hash_algo: v1.hash_algo,
                schema_ver: v1.schema_ver,
                remote: false,
            })
        }
        other => Err(StoreError::SchemaVersionMismatch {
            expected: REPO_META_VERSION,
            found: other.copied().unwrap_or(0),
        }),
    }
}

/// How the main is pushed to one sink. Chosen per sink, so a group can mirror
/// some backups and only-add to others.
/// Stored via postcard (variant *index*), so new variants are appended at the
/// end — inserting or reordering would silently re-mode every stored sink.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SyncMode {
    /// Copy content the sink lacks; never delete anything in the sink.
    #[default]
    AddOnly,
    /// Copy what the sink lacks *and* delete what the main no longer has, so
    /// the sink ends up holding exactly the main's content.
    Mirror,
    /// Copy what the sink lacks *and* delete sink content the main itself
    /// deleted (its tombstones), so the sink follows the main's edits.
    /// Content the main never had stays untouched.
    ApplyChanges,
}

impl SyncMode {
    /// Whether a push in this mode can delete the sink's existing files —
    /// what the session locks and the empty-main guard care about.
    pub fn deletes_in_sink(self) -> bool {
        !matches!(self, SyncMode::AddOnly)
    }
}

/// One sink of a group: the backup repository and how the main is pushed to it.
/// The mode is per sink, so a group can mirror some backups and only-add to others.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SyncSink {
    pub repo: String,
    pub mode: SyncMode,
}

/// One backup group: a **main** repository plus the remote **sinks** it is
/// pushed to. A repository belongs to at most one group.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SyncGroup {
    pub main: String,
    pub sinks: Vec<SyncSink>,
}

impl SyncGroup {
    /// The main repo and its sinks' repos, in display order.
    pub fn members(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.main.as_str()).chain(self.sinks.iter().map(|s| s.repo.as_str()))
    }

    pub fn has_member(&self, repo: &str) -> bool {
        self.members().any(|m| m == repo)
    }
}

/// Version-1 [`SyncGroup`] layout: a single group-wide `mode`. Kept so
/// pre-upgrade groups stay readable; see [`decode_sync_group`].
#[derive(Serialize, Deserialize)]
struct SyncGroupV1 {
    main: String,
    sinks: Vec<String>,
    mode: SyncMode,
}

/// Decode a stored [`SyncGroup`], migrating a version-1 (group-wide mode) value
/// so every sink inherits that mode. Re-saving writes it back as the current
/// [`SYNC_GROUP_VERSION`].
fn decode_sync_group(bytes: &[u8]) -> Result<SyncGroup, StoreError> {
    match bytes.first() {
        Some(&SYNC_GROUP_VERSION) => deserialize_value(SYNC_GROUP_VERSION, bytes),
        Some(&SCHEMA_VERSION) => {
            let v1: SyncGroupV1 = deserialize_value(SCHEMA_VERSION, bytes)?;
            Ok(SyncGroup {
                main: v1.main,
                sinks: v1
                    .sinks
                    .into_iter()
                    .map(|repo| SyncSink {
                        repo,
                        mode: v1.mode,
                    })
                    .collect(),
            })
        }
        other => Err(StoreError::SchemaVersionMismatch {
            expected: SYNC_GROUP_VERSION,
            found: other.copied().unwrap_or(0),
        }),
    }
}

/// 512-bit perceptual image hash; see `fingerprint::image_hash`.
pub type ImgHash = [u64; 8];

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub size: u64,
    pub hash: [u8; 32], // blake3
    pub modified_ms: i64,
    pub missing: bool,
    pub mime: Option<String>,
    pub img_fingerprint: Option<ImgHash>, // gradient hash
    pub video_hash: Option<[ImgHash; 3]>, // 512-bit temporal hash (3 frames)
    pub pdf_hash: Option<[u8; 32]>,       // blake3 of normalized text
    pub audio: Option<AudioFp>,           // duration_ms + chunk hashes
    pub img_size: Option<(u32, u32)>,
    /// Provenance: the source repo a file was copied/synced from (set by
    /// `diff_copy`/`sync_copy` when the target is a repo). `None` for scanned
    /// files and pre-provenance entries. A display/filter hint, never an
    /// identity input.
    pub origin: Option<String>,
    /// EXIF metadata for images (capture date, camera). `None` when absent or
    /// unreadable. Used to rank best copies and to order by real capture time.
    pub exif: Option<ExifInfo>,
}

/// Capture metadata extracted from an image's EXIF, if present.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct ExifInfo {
    /// Capture time as naive-local epoch milliseconds (EXIF has no timezone).
    pub taken_ms: Option<i64>,
    /// Camera make/model, e.g. "Canon EOS 5D".
    pub camera: Option<String>,
}

/// Version-1 [`FileEntry`] layout, whose image fingerprint was a 64-bit dHash.
/// Kept so pre-upgrade indexes stay readable; see [`decode_entry`].
#[derive(Serialize, Deserialize)]
struct FileEntryV1 {
    size: u64,
    hash: [u8; 32],
    modified_ms: i64,
    missing: bool,
    mime: Option<String>,
    img_fingerprint: Option<u64>,
    video_hash: Option<[u64; 3]>,
    pdf_hash: Option<[u8; 32]>,
    audio: Option<AudioFp>,
    img_size: Option<(u32, u32)>,
}

/// Version-2 [`FileEntry`] layout: the 512-bit image hash but no `origin`.
/// Kept so pre-provenance indexes decode unchanged (origin → `None`).
#[derive(Serialize, Deserialize)]
struct FileEntryV2 {
    size: u64,
    hash: [u8; 32],
    modified_ms: i64,
    missing: bool,
    mime: Option<String>,
    img_fingerprint: Option<ImgHash>,
    video_hash: Option<[u64; 3]>,
    pdf_hash: Option<[u8; 32]>,
    audio: Option<AudioFp>,
    img_size: Option<(u32, u32)>,
}

/// Version-3 [`FileEntry`] layout: has `origin` but no `exif`. Kept so
/// pre-EXIF indexes decode unchanged (exif → `None`).
#[derive(Serialize, Deserialize)]
struct FileEntryV3 {
    size: u64,
    hash: [u8; 32],
    modified_ms: i64,
    missing: bool,
    mime: Option<String>,
    img_fingerprint: Option<ImgHash>,
    video_hash: Option<[u64; 3]>,
    pdf_hash: Option<[u8; 32]>,
    audio: Option<AudioFp>,
    img_size: Option<(u32, u32)>,
    origin: Option<String>,
}

/// Version-4..7 [`FileEntry`] layout: has `origin` and `exif` but the old
/// 64-bit-per-frame video hash. Kept so pre-v8 indexes decode; the incompatible
/// video hash comes back `None` and video files are flagged stale for re-hash.
#[derive(Serialize, Deserialize)]
struct FileEntryV7 {
    size: u64,
    hash: [u8; 32],
    modified_ms: i64,
    missing: bool,
    mime: Option<String>,
    img_fingerprint: Option<ImgHash>,
    video_hash: Option<[u64; 3]>,
    pdf_hash: Option<[u8; 32]>,
    audio: Option<AudioFp>,
    img_size: Option<(u32, u32)>,
    origin: Option<String>,
    exif: Option<ExifInfo>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct AudioFp {
    pub duration_ms: u32,
    pub chunk_hashes: Vec<[u8; 32]>,
}

/// One file inside an archive: its path within the archive plus, when the
/// content could be read, its content identity (size + BLAKE3). A **locked**
/// member is one whose contents are encrypted: its name and size are known
/// (they live unencrypted in the archive's directory) but its `hash` is `None`
/// until the archive is unlocked. A locked member never matches loose content.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ArchiveMember {
    pub rel_path: String,
    pub size: u64,
    /// Content hash, or `None` when the member is locked (or otherwise
    /// unreadable) so its content identity is not yet known.
    pub hash: Option<[u8; 32]>,
    /// The member is encrypted and could not be read without a password.
    pub locked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepoStats {
    pub file_count: u64,
    pub total_size: u64,
    pub missing_count: u64,
    /// Epoch milliseconds of the last completed scan; 0 if never scanned.
    pub last_scan_ms: u64,
}

pub type DuplicateGroup = (u64, [u8; 32], Vec<String>);

/// The head of one `BY_SIZE_HASH` cursor during a k-way merge: the smallest
/// not-yet-consumed `(size, hash)` and its member rel-paths, or `None` at end.
type MergeHead = Option<(u64, [u8; 32], Vec<String>)>;

/// The cache of shared repo index handles, keyed by repo name.
type RepoDbMap = HashMap<String, Arc<redb::Database>>;

/// Shared repo handles plus the names currently frozen by an identity
/// operation (remove/rename/duplicate). Opening a frozen name fails
/// [`StoreError::Busy`]; the file work itself runs *outside* the mutex, so
/// unrelated repos never block on another repo's file I/O.
#[derive(Default)]
struct RepoDbCache {
    map: RepoDbMap,
    frozen: std::collections::HashSet<String>,
}

/// Unfreezes its repo names on drop, so every exit path (including `?`) of an
/// identity operation releases them.
struct FreezeGuard<'a> {
    store: &'a Store,
    names: Vec<String>,
}

impl Drop for FreezeGuard<'_> {
    fn drop(&mut self) {
        let mut cache = self.store.repo_dbs();
        for name in &self.names {
            cache.frozen.remove(name);
        }
    }
}

pub struct Store {
    config_dir: PathBuf,
    registry: redb::Database,
    /// One shared handle per repo index. redb permits a single live
    /// [`redb::Database`] per file (it holds an exclusive file lock), but that
    /// one instance supports any number of concurrent readers plus one writer
    /// (MVCC) — so every user must share the cached handle instead of
    /// re-opening the file, or concurrent operations fail with
    /// "database already open".
    repo_dbs: Mutex<RepoDbCache>,
}

/// Create (or open) a repo index file at `path` and ensure its tables exist.
fn create_db_file(path: &std::path::Path) -> Result<redb::Database, StoreError> {
    if let Some(parent) = path.parent()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent)?;
    }
    let db = redb::Database::create(path)?;

    // Ensure tables exist
    let write_txn = db.begin_write()?;
    {
        let _files = write_txn.open_table(FILES)?;
        let _by_size_hash = write_txn.open_multimap_table(BY_SIZE_HASH)?;
        let _by_fprint = write_txn.open_multimap_table(BY_FPRINT)?;
        let _meta = write_txn.open_table(META)?;
        let _mime_stats = write_txn.open_table(MIME_STATS)?;
        let _archive_members = write_txn.open_table(ARCHIVE_MEMBERS)?;
        let _archive_passwords = write_txn.open_table(ARCHIVE_PASSWORDS)?;
        // Drop the pre-ImgHash fingerprint index if this repo predates it.
        let _ = write_txn.delete_multimap_table(BY_FPRINT_LEGACY);
    }
    write_txn.commit()?;
    Ok(db)
}

/// The registry's directory: `$XDG_CONFIG_HOME/dedup`, else `~/.config/dedup`.
/// Honouring the XDG variable keeps it consistent with
/// [`crate::logging::state_dir`], which reads `$XDG_STATE_HOME`.
fn get_config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME")
        && !dir.trim().is_empty()
    {
        return PathBuf::from(dir).join("dedup");
    }
    match std::env::var("HOME") {
        Ok(home) => PathBuf::from(home).join(".config").join("dedup"),
        Err(_) => PathBuf::from(".config").join("dedup"),
    }
}

/// Absolute paths are kept verbatim; relative ones are canonicalized against
/// the current directory, falling back to the original string on failure.
fn canonicalize_path(path: &str) -> String {
    let p = std::path::Path::new(path);
    if p.is_absolute() {
        path.to_string()
    } else {
        std::fs::canonicalize(p)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| path.to_string())
    }
}

fn serialize_value<T: Serialize>(schema_ver: u8, value: &T) -> Result<Vec<u8>, StoreError> {
    let mut bytes = vec![schema_ver];
    let serialized =
        postcard::to_allocvec(value).map_err(|e| StoreError::Serialization(e.to_string()))?;
    bytes.extend_from_slice(&serialized);
    Ok(bytes)
}

fn deserialize_value<'a, T: Deserialize<'a>>(
    schema_ver: u8,
    bytes: &'a [u8],
) -> Result<T, StoreError> {
    if bytes.is_empty() {
        return Err(StoreError::Deserialization("Empty bytes".to_string()));
    }
    if bytes[0] != schema_ver {
        return Err(StoreError::SchemaVersionMismatch {
            expected: schema_ver,
            found: bytes[0],
        });
    }
    postcard::from_bytes(&bytes[1..]).map_err(|e| StoreError::Deserialization(e.to_string()))
}

/// Decode a stored [`FileEntry`] value, returning it with its version byte.
/// Version-1 entries convert losslessly except for the image fingerprint,
/// which used an incompatible 64-bit format and comes back as `None`; callers
/// that drive rescans use the version to flag such entries stale.
fn decode_entry(bytes: &[u8]) -> Result<(FileEntry, u8), StoreError> {
    match bytes.first() {
        Some(&ENTRY_VERSION) => Ok((deserialize_value(ENTRY_VERSION, bytes)?, ENTRY_VERSION)),
        // v8 shares the current layout byte-for-byte; only the mime
        // misclassification of MP4-container audio separates it (the per-mime
        // stale rules in `read_scan_index` re-scan exactly those files).
        Some(&8) => Ok((deserialize_value(8, bytes)?, 8)),
        // v4..v7 share one layout with the old 64-bit video hash; decode via
        // FileEntryV7 and drop the incompatible video hash (video → stale).
        Some(&v @ 4..=7) => {
            let old: FileEntryV7 = deserialize_value(v, bytes)?;
            let entry = FileEntry {
                size: old.size,
                hash: old.hash,
                modified_ms: old.modified_ms,
                missing: old.missing,
                mime: old.mime,
                img_fingerprint: old.img_fingerprint,
                video_hash: None,
                pdf_hash: old.pdf_hash,
                audio: old.audio,
                img_size: old.img_size,
                origin: old.origin,
                exif: old.exif,
            };
            Ok((entry, v))
        }
        Some(3) => {
            let v3: FileEntryV3 = deserialize_value(3, bytes)?;
            let entry = FileEntry {
                size: v3.size,
                hash: v3.hash,
                modified_ms: v3.modified_ms,
                missing: v3.missing,
                mime: v3.mime,
                img_fingerprint: v3.img_fingerprint,
                video_hash: None, // old 64-bit hash dropped; video re-scans
                pdf_hash: v3.pdf_hash,
                audio: v3.audio,
                img_size: v3.img_size,
                origin: v3.origin,
                exif: None,
            };
            Ok((entry, 3))
        }
        Some(2) => {
            let v2: FileEntryV2 = deserialize_value(2, bytes)?;
            let entry = FileEntry {
                size: v2.size,
                hash: v2.hash,
                modified_ms: v2.modified_ms,
                missing: v2.missing,
                mime: v2.mime,
                img_fingerprint: v2.img_fingerprint,
                video_hash: None,
                pdf_hash: v2.pdf_hash,
                audio: v2.audio,
                img_size: v2.img_size,
                origin: None,
                exif: None,
            };
            Ok((entry, 2))
        }
        Some(1) => {
            let v1: FileEntryV1 = deserialize_value(1, bytes)?;
            let entry = FileEntry {
                size: v1.size,
                hash: v1.hash,
                modified_ms: v1.modified_ms,
                missing: v1.missing,
                mime: v1.mime,
                img_fingerprint: None,
                video_hash: None,
                pdf_hash: v1.pdf_hash,
                audio: v1.audio,
                img_size: v1.img_size,
                origin: None,
                exif: None,
            };
            Ok((entry, 1))
        }
        Some(&found) => Err(StoreError::SchemaVersionMismatch {
            expected: ENTRY_VERSION,
            found,
        }),
        None => Err(StoreError::Deserialization("Empty bytes".to_string())),
    }
}

impl Store {
    pub fn open() -> Result<Self, StoreError> {
        Self::open_at(get_config_dir())
    }

    pub fn open_at(config_dir: PathBuf) -> Result<Self, StoreError> {
        std::fs::create_dir_all(&config_dir)?;

        let registry_path = config_dir.join("repos.redb");
        let registry = redb::Database::create(&registry_path)?;

        // Ensure registry table exists
        let write_txn = registry.begin_write()?;
        {
            let _table = write_txn.open_table(REPOS)?;
            let _groups = write_txn.open_table(SYNC_GROUPS)?;
        }
        write_txn.commit()?;

        Ok(Self {
            config_dir,
            registry,
            repo_dbs: Mutex::new(RepoDbCache::default()),
        })
    }

    /// Lock the repo handle cache. A poisoned lock only means another thread
    /// panicked while holding it; the cache itself (plain inserts/removes of
    /// `Arc`s and names) is always consistent, so recover instead of
    /// propagating panics.
    fn repo_dbs(&self) -> MutexGuard<'_, RepoDbCache> {
        self.repo_dbs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The directory where dedup stores its configuration and repo databases
    /// (e.g. `~/.config/dedup`).
    pub fn config_dir(&self) -> &std::path::Path {
        &self.config_dir
    }

    pub fn get_repo_db_path(&self, name: &str) -> PathBuf {
        self.config_dir.join("repos").join(name).join("index.redb")
    }

    /// The shared handle for a repo's index database, creating the database
    /// (and its tables) on first use. All callers — scans, diff ops, views —
    /// get the same instance, so they can run concurrently under redb's MVCC
    /// (readers never block; writers serialize per batch). Fails with
    /// [`StoreError::Busy`] while an identity operation has the name frozen.
    pub fn open_repo_db(&self, name: &str) -> Result<Arc<redb::Database>, StoreError> {
        let mut cache = self.repo_dbs();
        if cache.frozen.contains(name) {
            return Err(StoreError::Busy(name.to_string()));
        }
        if let Some(db) = cache.map.get(name) {
            return Ok(Arc::clone(db));
        }

        let db = Arc::new(create_db_file(&self.get_repo_db_path(name))?);
        cache.map.insert(name.to_string(), Arc::clone(&db));
        Ok(db)
    }

    /// Freeze `names` for an identity operation (remove/rename/duplicate):
    /// their cached handles are evicted — failing with [`StoreError::Busy`]
    /// while any operation (e.g. a running scan) still holds one — and every
    /// open attempt fails `Busy` until the returned guard drops. The guard
    /// lets the registry/file work run *outside* the handle-map mutex, so a
    /// long copy or delete never stalls access to unrelated repos.
    fn freeze(&self, names: &[&str]) -> Result<FreezeGuard<'_>, StoreError> {
        let mut cache = self.repo_dbs();
        for name in names {
            if cache.frozen.contains(*name) {
                return Err(StoreError::Busy((*name).to_string()));
            }
            if let Some(db) = cache.map.get(*name) {
                if Arc::strong_count(db) > 1 {
                    return Err(StoreError::Busy((*name).to_string()));
                }
                cache.map.remove(*name);
            }
        }
        for name in names {
            cache.frozen.insert((*name).to_string());
        }
        Ok(FreezeGuard {
            store: self,
            names: names.iter().map(|n| (*n).to_string()).collect(),
        })
    }

    /// Rewrite a repo's index file to reclaim the disk space freed by removed
    /// entries (e.g. by [`remove_entries`] pruning missing tombstones). redb's
    /// [`compact`](redb::Database::compact) needs exclusive `&mut` access, so the
    /// name is [`freeze`](Self::freeze)d first — evicting the shared handle and
    /// failing [`StoreError::Busy`] while any operation still holds one — and the
    /// file is reopened privately for the rewrite. Returns whether compaction
    /// actually moved data (redb reports `false` when nothing could be reclaimed).
    /// The guard drops on return, so the next [`open_repo_db`](Self::open_repo_db)
    /// reopens the compacted file.
    pub fn compact_repo(&self, name: &str) -> Result<bool, StoreError> {
        let _frozen = self.freeze(&[name])?;
        let path = self.get_repo_db_path(name);
        if !path.exists() {
            return Ok(false);
        }
        let mut db = redb::Database::open(&path)?;
        Ok(db.compact()?)
    }

    pub fn create_repo(&self, name: &str, path: &str) -> Result<(), StoreError> {
        let abs_path = std::path::Path::new(path);
        let abs_path_str = if abs_path.is_absolute() {
            path.to_string()
        } else {
            std::fs::canonicalize(abs_path)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| path.to_string())
        };

        let created = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let meta = RepoMeta {
            abs_path: abs_path_str,
            created,
            hash_algo: "BLAKE3".to_string(),
            schema_ver: SCHEMA_VERSION,
            remote: false,
        };
        let serialized = serialize_value(REPO_META_VERSION, &meta)?;

        // Check and insert within one write transaction so a concurrent create
        // cannot slip in between; the check also runs before the repo database
        // is touched, so an existing repo's stats are never reset.
        let reg_write_txn = self.registry.begin_write()?;
        {
            let mut reg_table = reg_write_txn.open_table(REPOS)?;
            if reg_table.get(name)?.is_some() {
                return Err(StoreError::AlreadyExists(name.to_string()));
            }

            // Initialize the repository B-tree file and its meta stats
            let repo_db = self.open_repo_db(name)?;
            let write_txn = repo_db.begin_write()?;
            {
                let mut meta_table = write_txn.open_table(META)?;
                meta_table.insert("file_count", 0u64)?;
                meta_table.insert("total_size", 0u64)?;
                meta_table.insert("missing_count", 0u64)?;
            }
            write_txn.commit()?;

            reg_table.insert(name, serialized.as_slice())?;
        }
        reg_write_txn.commit()?;

        Ok(())
    }

    pub fn get_repo(&self, name: &str) -> Result<RepoMeta, StoreError> {
        let read_txn = self.registry.begin_read()?;
        let table = read_txn.open_table(REPOS)?;
        match table.get(name)? {
            Some(guard) => decode_repo_meta(guard.value()),
            None => Err(StoreError::NotFound(name.to_string())),
        }
    }

    /// Flip a repo's user-set **remote** flag (slow mount; bulk scans skip it).
    pub fn set_repo_remote(&self, name: &str, remote: bool) -> Result<(), StoreError> {
        let write_txn = self.registry.begin_write()?;
        {
            let mut table = write_txn.open_table(REPOS)?;
            let mut meta = match table.get(name)? {
                Some(guard) => decode_repo_meta(guard.value())?,
                None => return Err(StoreError::NotFound(name.to_string())),
            };
            meta.remote = remote;
            let serialized = serialize_value(REPO_META_VERSION, &meta)?;
            table.insert(name, serialized.as_slice())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    pub fn list_repos(&self) -> Result<Vec<(String, RepoMeta, RepoStats)>, StoreError> {
        let read_txn = self.registry.begin_read()?;
        let table = read_txn.open_table(REPOS)?;

        let mut repos = Vec::new();
        for item in table.iter()? {
            let (name_guard, val_guard) = item?;
            let name = name_guard.value().to_string();
            let meta: RepoMeta = decode_repo_meta(val_guard.value())?;
            let stats = self.get_repo_stats(&name)?;
            repos.push((name, meta, stats));
        }

        Ok(repos)
    }

    pub fn get_repo_stats(&self, name: &str) -> Result<RepoStats, StoreError> {
        let db_path = self.get_repo_db_path(name);
        if !db_path.exists() {
            return Ok(RepoStats {
                file_count: 0,
                total_size: 0,
                missing_count: 0,
                last_scan_ms: 0,
            });
        }

        let db = self.open_repo_db(name)?;
        let read_txn = db.begin_read()?;
        let meta_table = read_txn.open_table(META)?;

        let get = |key: &str| -> Result<u64, StoreError> {
            Ok(meta_table.get(key)?.map(|v| v.value()).unwrap_or(0))
        };

        Ok(RepoStats {
            file_count: get("file_count")?,
            total_size: get("total_size")?,
            missing_count: get("missing_count")?,
            last_scan_ms: get("last_scan_ms")?,
        })
    }

    /// MIME-type distribution for a repo (`mime → count`), sorted by count
    /// descending then name. Reads the maintained `MIME_STATS` table only — no
    /// scan of `FILES`.
    pub fn get_mime_stats(&self, name: &str) -> Result<Vec<(String, u64)>, StoreError> {
        let db_path = self.get_repo_db_path(name);
        if !db_path.exists() {
            return Ok(Vec::new());
        }
        let db = self.open_repo_db(name)?;
        let read_txn = db.begin_read()?;
        let table = read_txn.open_table(MIME_STATS)?;
        let mut stats = Vec::new();
        for item in table.iter()? {
            let (key, value) = item?;
            stats.push((key.value().to_string(), value.value()));
        }
        stats.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        Ok(stats)
    }

    // --- sync groups ------------------------------------------------------

    /// Every sync group, by name, in registry order. A registry written before
    /// sync groups existed simply has no such table and reports none.
    pub fn list_sync_groups(&self) -> Result<Vec<(String, SyncGroup)>, StoreError> {
        let read_txn = self.registry.begin_read()?;
        let table = match read_txn.open_table(SYNC_GROUPS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut groups = Vec::new();
        for item in table.iter()? {
            let (name_guard, val_guard) = item?;
            let group = decode_sync_group(val_guard.value())?;
            groups.push((name_guard.value().to_string(), group));
        }
        Ok(groups)
    }

    pub fn get_sync_group(&self, name: &str) -> Result<SyncGroup, StoreError> {
        let read_txn = self.registry.begin_read()?;
        let table = match read_txn.open_table(SYNC_GROUPS) {
            Ok(table) => table,
            // No table yet means no groups, so the name is simply not found.
            Err(redb::TableError::TableDoesNotExist(_)) => {
                return Err(StoreError::GroupNotFound(name.to_string()));
            }
            Err(e) => return Err(e.into()),
        };
        // A keyed lookup, not a scan of every group: the name is the table key.
        match table.get(name)? {
            Some(guard) => decode_sync_group(guard.value()),
            None => Err(StoreError::GroupNotFound(name.to_string())),
        }
    }

    /// The group a repository belongs to, if any — the lookup the repo lists
    /// use to collapse sinks under their main.
    pub fn sync_group_of(&self, repo: &str) -> Result<Option<(String, SyncGroup)>, StoreError> {
        Ok(self
            .list_sync_groups()?
            .into_iter()
            .find(|(_, group)| group.has_member(repo)))
    }

    /// The names of every repository that is a *sink* of some sync group. The
    /// operational tabs (Transfer, Grooming, Duplicates, Browse) hide these: a
    /// sink is acted on through its group's main, not directly.
    pub fn sink_repo_names(&self) -> Result<std::collections::HashSet<String>, StoreError> {
        let mut sinks = std::collections::HashSet::new();
        for (_, group) in self.list_sync_groups()? {
            for sink in group.sinks {
                sinks.insert(sink.repo);
            }
        }
        Ok(sinks)
    }

    /// The names of every repository that is the *main* of some sync group —
    /// including a group that has no sinks yet. The GUI marks these with a badge
    /// so a main is recognisable wherever a repo is named, rather than only by
    /// the group controls that appear under its card.
    pub fn main_repo_names(&self) -> Result<std::collections::HashSet<String>, StoreError> {
        Ok(self
            .list_sync_groups()?
            .into_iter()
            .map(|(_, group)| group.main)
            .collect())
    }

    /// Create a group around `main`. The repo must exist and must not already
    /// belong to another group. A new group has no sinks; each sink's push mode
    /// is chosen when it is added.
    pub fn create_sync_group(&self, name: &str, main: &str) -> Result<(), StoreError> {
        // Keyed existence check rather than a scan of every group.
        match self.get_sync_group(name) {
            Ok(_) => return Err(StoreError::GroupExists(name.to_string())),
            Err(StoreError::GroupNotFound(_)) => {}
            Err(e) => return Err(e),
        }
        self.get_repo(main)?;
        self.reject_if_grouped(main)?;
        self.put_sync_group(
            name,
            &SyncGroup {
                main: main.to_string(),
                sinks: Vec::new(),
            },
        )
    }

    pub fn delete_sync_group(&self, name: &str) -> Result<(), StoreError> {
        let write_txn = self.registry.begin_write()?;
        {
            let mut table = write_txn.open_table(SYNC_GROUPS)?;
            if table.remove(name)?.is_none() {
                return Err(StoreError::GroupNotFound(name.to_string()));
            }
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Add an existing repository to a group as a sink, pushed in `mode`.
    pub fn add_sync_sink(
        &self,
        group_name: &str,
        repo: &str,
        mode: SyncMode,
    ) -> Result<(), StoreError> {
        let mut group = self.get_sync_group(group_name)?;
        self.get_repo(repo)?;
        self.reject_if_grouped(repo)?;
        group.sinks.push(SyncSink {
            repo: repo.to_string(),
            mode,
        });
        self.put_sync_group(group_name, &group)
    }

    /// Take a sink out of its group. The main cannot be removed this way —
    /// promote another member first, or delete the group.
    pub fn remove_sync_sink(&self, group_name: &str, repo: &str) -> Result<(), StoreError> {
        let mut group = self.get_sync_group(group_name)?;
        if group.main == repo || !group.sinks.iter().any(|s| s.repo == repo) {
            return Err(StoreError::NotInGroup {
                repo: repo.to_string(),
                group: group_name.to_string(),
            });
        }
        group.sinks.retain(|s| s.repo != repo);
        self.put_sync_group(group_name, &group)
    }

    /// Promote a member to main; the previous main becomes a sink, so nothing
    /// leaves the group by switching which way it is pushed.
    pub fn set_sync_main(&self, group_name: &str, repo: &str) -> Result<(), StoreError> {
        let mut group = self.get_sync_group(group_name)?;
        if group.main == repo {
            return Ok(());
        }
        if !group.sinks.iter().any(|s| s.repo == repo) {
            return Err(StoreError::NotInGroup {
                repo: repo.to_string(),
                group: group_name.to_string(),
            });
        }
        // The promoted sink becomes the main (mains have no mode); the old main
        // becomes a sink in the default (ADD ONLY) mode.
        group.sinks.retain(|s| s.repo != repo);
        let old_main = std::mem::replace(&mut group.main, repo.to_string());
        group.sinks.push(SyncSink {
            repo: old_main,
            mode: SyncMode::default(),
        });
        self.put_sync_group(group_name, &group)
    }

    /// Set the push mode of one sink in a group.
    pub fn set_sink_mode(
        &self,
        group_name: &str,
        sink: &str,
        mode: SyncMode,
    ) -> Result<(), StoreError> {
        let mut group = self.get_sync_group(group_name)?;
        match group.sinks.iter_mut().find(|s| s.repo == sink) {
            Some(s) => s.mode = mode,
            None => {
                return Err(StoreError::NotInGroup {
                    repo: sink.to_string(),
                    group: group_name.to_string(),
                });
            }
        }
        self.put_sync_group(group_name, &group)
    }

    /// Refuse to touch a repo that is spoken for by a group.
    fn reject_if_grouped(&self, repo: &str) -> Result<(), StoreError> {
        match self.sync_group_of(repo)? {
            Some((group, _)) => Err(StoreError::AlreadyGrouped {
                repo: repo.to_string(),
                group,
            }),
            None => Ok(()),
        }
    }

    fn put_sync_group(&self, name: &str, group: &SyncGroup) -> Result<(), StoreError> {
        let bytes = serialize_value(SYNC_GROUP_VERSION, group)?;
        let write_txn = self.registry.begin_write()?;
        {
            let mut table = write_txn.open_table(SYNC_GROUPS)?;
            table.insert(name, bytes.as_slice())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    pub fn remove_repo(&self, name: &str) -> Result<(), StoreError> {
        // A repo a sync group is built on must not vanish under it.
        if let Some((group, _)) = self.sync_group_of(name)? {
            return Err(StoreError::InSyncGroup {
                repo: name.to_string(),
                group,
            });
        }
        // Frozen across the registry removal and the directory delete so no
        // thread can re-open the index mid-removal (without blocking access
        // to other repos while the delete runs).
        let _frozen = self.freeze(&[name])?;

        let reg_write_txn = self.registry.begin_write()?;
        {
            let mut reg_table = reg_write_txn.open_table(REPOS)?;
            if reg_table.remove(name)?.is_none() {
                return Err(StoreError::NotFound(name.to_string()));
            }
        }
        reg_write_txn.commit()?;

        let db_dir = self.config_dir.join("repos").join(name);
        if db_dir.exists() {
            std::fs::remove_dir_all(&db_dir)?;
        }

        Ok(())
    }

    pub fn rename_repo(&self, name: &str, new_name: &str) -> Result<(), StoreError> {
        if name == new_name {
            return Err(StoreError::AlreadyExists(new_name.to_string()));
        }
        // Group membership is by name, so a member cannot be renamed out from
        // under its group.
        if let Some((group, _)) = self.sync_group_of(name)? {
            return Err(StoreError::InSyncGroup {
                repo: name.to_string(),
                group,
            });
        }
        // Both names frozen across the registry update and the directory
        // rename: nobody may re-open the old index mid-rename, and nobody may
        // create the new name's index before the rename lands on it.
        let _frozen = self.freeze(&[name, new_name])?;

        let reg_write_txn = self.registry.begin_write()?;
        {
            let mut reg_table = reg_write_txn.open_table(REPOS)?;
            let bytes = match reg_table.get(name)? {
                Some(guard) => guard.value().to_vec(),
                None => return Err(StoreError::NotFound(name.to_string())),
            };
            if reg_table.get(new_name)?.is_some() {
                return Err(StoreError::AlreadyExists(new_name.to_string()));
            }
            reg_table.remove(name)?;
            reg_table.insert(new_name, bytes.as_slice())?;
        }
        reg_write_txn.commit()?;

        let old_db_dir = self.config_dir.join("repos").join(name);
        let new_db_dir = self.config_dir.join("repos").join(new_name);
        if old_db_dir.exists() {
            if let Some(parent) = new_db_dir.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(old_db_dir, new_db_dir)?;
        }

        Ok(())
    }

    pub fn relocate_repo(&self, name: &str, new_path: &str) -> Result<(), StoreError> {
        let reg_write_txn = self.registry.begin_write()?;
        let mut meta = {
            let reg_table = reg_write_txn.open_table(REPOS)?;
            let meta_bytes = match reg_table.get(name)? {
                Some(guard) => guard.value().to_vec(),
                None => return Err(StoreError::NotFound(name.to_string())),
            };
            decode_repo_meta(&meta_bytes)?
        };

        let abs_path = std::path::Path::new(new_path);
        let abs_path_str = if abs_path.is_absolute() {
            new_path.to_string()
        } else {
            std::fs::canonicalize(abs_path)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| new_path.to_string())
        };

        meta.abs_path = abs_path_str;
        let serialized = serialize_value(REPO_META_VERSION, &meta)?;

        {
            let mut reg_table = reg_write_txn.open_table(REPOS)?;
            reg_table.insert(name, serialized.as_slice())?;
        }
        reg_write_txn.commit()?;

        Ok(())
    }

    /// Duplicate `source` into a new repo `dest` pointing at `new_path`, keeping
    /// every index entry (and its stats) from the original. The source is left
    /// unmodified. Ported from the legacy `repo cp` / `CopyRepoProcess`.
    pub fn duplicate_repo(
        &self,
        source: &str,
        dest: &str,
        new_path: &str,
    ) -> Result<(), StoreError> {
        if source == dest {
            return Err(StoreError::AlreadyExists(dest.to_string()));
        }
        // A byte-copy is only consistent while nothing can write the source
        // index, so freeze it (fails while e.g. a scan holds it) — and the
        // destination, so nobody opens a half-copied index — until the copy
        // is done. The copy itself runs outside the handle-map mutex.
        let _frozen = self.freeze(&[source, dest])?;

        let source_meta = self.get_repo(source)?;

        let meta = RepoMeta {
            abs_path: canonicalize_path(new_path),
            created: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            hash_algo: source_meta.hash_algo,
            schema_ver: source_meta.schema_ver,
            // A duplicate points at a *new* path; whether that mount is slow
            // is a fresh judgement, so it starts unflagged.
            remote: false,
        };
        let serialized = serialize_value(REPO_META_VERSION, &meta)?;

        // Byte-copy the source index (a redb file at rest is self-consistent);
        // this carries FILES, the BY_* indexes, META, and MIME_STATS verbatim.
        let src_db = self.get_repo_db_path(source);
        let dst_db = self.get_repo_db_path(dest);
        if let Some(parent) = dst_db.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Reserve the destination name atomically before writing any files, so a
        // clash cannot leave an orphan index behind.
        let reg_write_txn = self.registry.begin_write()?;
        {
            let mut reg_table = reg_write_txn.open_table(REPOS)?;
            if reg_table.get(dest)?.is_some() {
                return Err(StoreError::AlreadyExists(dest.to_string()));
            }
            if src_db.exists() {
                std::fs::copy(&src_db, &dst_db)?;
            } else {
                // Source was never scanned: start the copy with empty tables
                // (dest is frozen, so the file can be created directly).
                create_db_file(&dst_db)?;
            }
            reg_table.insert(dest, serialized.as_slice())?;
        }
        reg_write_txn.commit()?;

        Ok(())
    }

    pub fn update_file_entry(
        &self,
        repo_name: &str,
        rel_path: &str,
        entry: &FileEntry,
    ) -> Result<(), StoreError> {
        let db = self.open_repo_db(repo_name)?;
        apply_entries(&db, std::iter::once((rel_path, entry)))
    }

    pub fn get_file_entry(
        &self,
        repo_name: &str,
        rel_path: &str,
    ) -> Result<Option<FileEntry>, StoreError> {
        let db = self.open_repo_db(repo_name)?;
        get_entry(&db, rel_path)
    }

    pub fn remove_file_entry(&self, repo_name: &str, rel_path: &str) -> Result<(), StoreError> {
        let db = self.open_repo_db(repo_name)?;
        let write_txn = db.begin_write()?;
        {
            let mut tables = RepoTables::open(&write_txn)?;
            tables.remove(rel_path)?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// The free-form annotation tags on one file (empty if none). Browse tab.
    pub fn get_annotations(
        &self,
        repo_name: &str,
        rel_path: &str,
    ) -> Result<Vec<String>, StoreError> {
        let db = self.open_repo_db(repo_name)?;
        let read_txn = db.begin_read()?;
        let table = match read_txn.open_table(ANNOTATIONS) {
            Ok(t) => t,
            // The table is created lazily on first write; absent → no tags.
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        match table.get(rel_path)? {
            Some(guard) => deserialize_value(SCHEMA_VERSION, guard.value()),
            None => Ok(Vec::new()),
        }
    }

    /// Replace one file's annotation tags (deduped, order preserved). An empty
    /// list removes the row entirely.
    pub fn set_annotations(
        &self,
        repo_name: &str,
        rel_path: &str,
        tags: &[String],
    ) -> Result<(), StoreError> {
        let mut clean: Vec<String> = Vec::new();
        for t in tags {
            let t = t.trim();
            if !t.is_empty() && !clean.iter().any(|c| c == t) {
                clean.push(t.to_string());
            }
        }
        let db = self.open_repo_db(repo_name)?;
        let write_txn = db.begin_write()?;
        {
            let mut table = write_txn.open_table(ANNOTATIONS)?;
            if clean.is_empty() {
                table.remove(rel_path)?;
            } else {
                let bytes = serialize_value(SCHEMA_VERSION, &clean)?;
                table.insert(rel_path, bytes.as_slice())?;
            }
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Every annotated file's tags, keyed by rel-path (for the Browse tab to
    /// load a repo's annotations in one pass and derive the used-tag list).
    pub fn all_annotations(
        &self,
        repo_name: &str,
    ) -> Result<std::collections::HashMap<String, Vec<String>>, StoreError> {
        let db = self.open_repo_db(repo_name)?;
        annotations_of_db(&db)
    }

    /// Mark one content as accepted in `repo_name`: it may repeat inside this
    /// repo, and every copy of it here is protected from duplicate deletion.
    /// Idempotent.
    pub fn accept_content(
        &self,
        repo_name: &str,
        size: u64,
        hash: &[u8; 32],
    ) -> Result<(), StoreError> {
        let db = self.open_repo_db(repo_name)?;
        let write_txn = db.begin_write()?;
        {
            let mut table = write_txn.open_table(ACCEPTED)?;
            table.insert((size, &hash[..]), ())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Drop the accepted mark on one content in `repo_name`. Returns whether a
    /// mark existed.
    pub fn unaccept_content(
        &self,
        repo_name: &str,
        size: u64,
        hash: &[u8; 32],
    ) -> Result<bool, StoreError> {
        let db = self.open_repo_db(repo_name)?;
        let write_txn = db.begin_write()?;
        let existed = {
            let mut table = write_txn.open_table(ACCEPTED)?;
            table.remove((size, &hash[..]))?.is_some()
        };
        write_txn.commit()?;
        Ok(existed)
    }

    /// Every accepted content key in `repo_name` (empty if none).
    pub fn all_accepted(&self, repo_name: &str) -> Result<HashSet<ContentKey>, StoreError> {
        let db = self.open_repo_db(repo_name)?;
        accepted_of_db(&db)
    }

    /// Rel-paths of the non-missing files in `repo_name` whose content is
    /// accepted there. Empty (without walking the index) when nothing is
    /// accepted, so callers can filter previews at no cost in the common case.
    pub fn accepted_paths(&self, repo_name: &str) -> Result<HashSet<String>, StoreError> {
        let db = self.open_repo_db(repo_name)?;
        let accepted = accepted_of_db(&db)?;
        let mut paths = HashSet::new();
        if accepted.is_empty() {
            return Ok(paths);
        }
        for_each_file_entry(&db, |rel, entry| {
            if !entry.missing && accepted.contains(&(entry.size, entry.hash)) {
                paths.insert(rel.to_string());
            }
            Ok(())
        })?;
        Ok(paths)
    }

    pub fn get_duplicate_groups(&self, repo_name: &str) -> Result<Vec<DuplicateGroup>, StoreError> {
        let db = self.open_repo_db(repo_name)?;
        let read_txn = db.begin_read()?;
        let by_size_hash = read_txn.open_multimap_table(BY_SIZE_HASH)?;

        let mut groups = Vec::new();
        for item in by_size_hash.iter()? {
            let (key_guard, val_iter) = item?;
            let (size, hash_slice) = key_guard.value();

            let mut paths = Vec::new();
            for path_res in val_iter {
                paths.push(path_res?.value().to_string());
            }

            if paths.len() > 1 {
                let mut hash = [0u8; 32];
                hash.copy_from_slice(hash_slice);
                groups.push((size, hash, paths));
            }
        }

        Ok(groups)
    }

    /// Enumerate exact-duplicate group descriptors `(size, hash, count)` across
    /// `repo_names`, streamed from each repo's sorted `BY_SIZE_HASH` index via a
    /// k-way merge. Members are deduplicated by absolute path across repos (like
    /// [`crate::dupes::find_exact_duplicates`]); only groups with `count > 1`
    /// are emitted. `progress` is called periodically with the running count.
    ///
    /// Peak memory is O(number of duplicate groups): unique files stream past
    /// without being retained.
    pub fn plan_duplicate_group_keys(
        &self,
        repo_names: &[String],
        mut progress: impl FnMut(usize),
    ) -> Result<Vec<(u64, [u8; 32], u32)>, StoreError> {
        // Keep every read layer alive for the whole merge. redb 2.x read handles
        // are owned (Arc-based), so these Vecs don't borrow one another; the
        // `iters` borrow `tables`, which must outlive them.
        let mut roots: Vec<String> = Vec::with_capacity(repo_names.len());
        let mut dbs: Vec<Arc<redb::Database>> = Vec::with_capacity(repo_names.len());
        for name in repo_names {
            roots.push(self.get_repo(name)?.abs_path);
            dbs.push(self.open_repo_db(name)?);
        }
        let mut txns = Vec::with_capacity(dbs.len());
        for db in &dbs {
            txns.push(db.begin_read()?);
        }
        let mut tables = Vec::with_capacity(txns.len());
        for txn in &txns {
            tables.push(txn.open_multimap_table(BY_SIZE_HASH)?);
        }
        let mut iters = Vec::with_capacity(tables.len());
        for table in &tables {
            iters.push(table.iter()?);
        }

        // Pull the next `(size, hash, member rel_paths)` from one cursor.
        macro_rules! pull {
            ($it:expr) => {{
                match $it.next() {
                    None => None,
                    Some(item) => {
                        let (key_guard, val_iter) = item?;
                        let (size, hash_slice) = key_guard.value();
                        let mut hash = [0u8; 32];
                        hash.copy_from_slice(hash_slice);
                        let mut members: Vec<String> = Vec::new();
                        for v in val_iter {
                            members.push(v?.value().to_string());
                        }
                        Some((size, hash, members))
                    }
                }
            }};
        }

        // Head of each cursor: the smallest not-yet-consumed key + its members.
        let mut heads: Vec<MergeHead> = Vec::with_capacity(iters.len());
        for it in &mut iters {
            heads.push(pull!(it));
        }

        let mut out: Vec<(u64, [u8; 32], u32)> = Vec::new();
        loop {
            let min = heads
                .iter()
                .flatten()
                .map(|(size, hash, _)| (*size, *hash))
                .min();
            let Some((msize, mhash)) = min else { break };

            // Union every cursor sitting on the min key, deduping by abs path.
            let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
            for i in 0..heads.len() {
                let on_min = matches!(&heads[i], Some((s, h, _)) if *s == msize && *h == mhash);
                if on_min {
                    if let Some((_, _, members)) = &heads[i] {
                        for rel in members {
                            seen.insert(std::path::Path::new(&roots[i]).join(rel));
                        }
                    }
                    heads[i] = pull!(iters[i]);
                }
            }

            if seen.len() > 1 {
                out.push((msize, mhash, seen.len() as u32));
                if out.len().is_multiple_of(1024) {
                    progress(out.len());
                }
            }
        }
        progress(out.len());
        Ok(out)
    }
}

/// The open tables of one repo database inside a single write transaction.
///
/// All index maintenance goes through this type so the invariant holds in one
/// place: every mutation of `FILES` updates `BY_SIZE_HASH`, `BY_FPRINT`,
/// `META`, and `MIME_STATS` in the same transaction, and `missing` entries are
/// excluded from index tables and stats.
struct RepoTables<'txn> {
    files: redb::Table<'txn, &'static str, &'static [u8]>,
    by_size_hash: redb::MultimapTable<'txn, (u64, &'static [u8]), &'static str>,
    by_fprint: redb::MultimapTable<'txn, ImgHash, &'static str>,
    meta: redb::Table<'txn, &'static str, u64>,
    mime_stats: redb::Table<'txn, &'static str, u64>,
}

impl<'txn> RepoTables<'txn> {
    fn open(txn: &'txn redb::WriteTransaction) -> Result<Self, StoreError> {
        Ok(Self {
            files: txn.open_table(FILES)?,
            by_size_hash: txn.open_multimap_table(BY_SIZE_HASH)?,
            by_fprint: txn.open_multimap_table(BY_FPRINT)?,
            meta: txn.open_table(META)?,
            mime_stats: txn.open_table(MIME_STATS)?,
        })
    }

    fn get_entry(&self, rel_path: &str) -> Result<Option<FileEntry>, StoreError> {
        match self.files.get(rel_path)? {
            Some(guard) => Ok(Some(decode_entry(guard.value())?.0)),
            None => Ok(None),
        }
    }

    /// Insert or replace one file entry, keeping index tables and stats consistent.
    fn upsert(&mut self, rel_path: &str, entry: &FileEntry) -> Result<(), StoreError> {
        if let Some(old) = self.get_entry(rel_path)? {
            self.unindex(rel_path, &old)?;
        }
        self.index(rel_path, entry)?;
        let serialized = serialize_value(ENTRY_VERSION, entry)?;
        self.files.insert(rel_path, serialized.as_slice())?;
        Ok(())
    }

    /// Remove one file entry and its index/stats contributions entirely.
    fn remove(&mut self, rel_path: &str) -> Result<(), StoreError> {
        if let Some(old) = self.get_entry(rel_path)? {
            self.unindex(rel_path, &old)?;
            self.files.remove(rel_path)?;
        }
        Ok(())
    }

    /// Undo the index/stats contributions of an existing entry.
    fn unindex(&mut self, rel_path: &str, old: &FileEntry) -> Result<(), StoreError> {
        if !old.missing {
            self.by_size_hash
                .remove((old.size, &old.hash[..]), rel_path)?;
            if let Some(fp) = old.img_fingerprint {
                self.by_fprint.remove(fp, rel_path)?;
            }
            self.bump_meta("file_count", -1)?;
            self.bump_meta_by("total_size", old.size, false)?;
            if let Some(ref mime) = old.mime {
                let count = self
                    .mime_stats
                    .get(mime.as_str())?
                    .map(|v| v.value())
                    .unwrap_or(0);
                if count <= 1 {
                    self.mime_stats.remove(mime.as_str())?;
                } else {
                    self.mime_stats.insert(mime.as_str(), count - 1)?;
                }
            }
        } else {
            self.bump_meta("missing_count", -1)?;
        }
        Ok(())
    }

    /// Apply the index/stats contributions of a new entry.
    fn index(&mut self, rel_path: &str, entry: &FileEntry) -> Result<(), StoreError> {
        if !entry.missing {
            self.by_size_hash
                .insert((entry.size, &entry.hash[..]), rel_path)?;
            if let Some(fp) = entry.img_fingerprint {
                self.by_fprint.insert(fp, rel_path)?;
            }
            self.bump_meta("file_count", 1)?;
            self.bump_meta_by("total_size", entry.size, true)?;
            if let Some(ref mime) = entry.mime {
                let count = self
                    .mime_stats
                    .get(mime.as_str())?
                    .map(|v| v.value())
                    .unwrap_or(0);
                self.mime_stats.insert(mime.as_str(), count + 1)?;
            }
        } else {
            self.bump_meta("missing_count", 1)?;
        }
        Ok(())
    }

    fn bump_meta(&mut self, key: &str, delta: i64) -> Result<(), StoreError> {
        let amount = delta.unsigned_abs();
        self.bump_meta_by(key, amount, delta >= 0)
    }

    fn bump_meta_by(&mut self, key: &str, amount: u64, add: bool) -> Result<(), StoreError> {
        let current = self.meta.get(key)?.map(|v| v.value()).unwrap_or(0);
        let next = if add {
            current.saturating_add(amount)
        } else {
            current.saturating_sub(amount)
        };
        self.meta.insert(key, next)?;
        Ok(())
    }
}

/// Insert or replace many file entries within a single write transaction.
pub fn apply_entries<'a, I>(db: &redb::Database, entries: I) -> Result<(), StoreError>
where
    I: IntoIterator<Item = (&'a str, &'a FileEntry)>,
{
    let write_txn = db.begin_write()?;
    {
        let mut tables = RepoTables::open(&write_txn)?;
        for (rel_path, entry) in entries {
            tables.upsert(rel_path, entry)?;
        }
    }
    write_txn.commit()?;
    Ok(())
}

/// Record the epoch-millisecond timestamp of a completed scan in `META`.
pub fn set_last_scan(db: &redb::Database, ms: u64) -> Result<(), StoreError> {
    let write_txn = db.begin_write()?;
    {
        let mut meta = write_txn.open_table(META)?;
        meta.insert("last_scan_ms", ms)?;
    }
    write_txn.commit()?;
    Ok(())
}

/// Store an archive's member list (replacing any existing) in `ARCHIVE_MEMBERS`.
pub fn set_archive_members(
    db: &redb::Database,
    rel_path: &str,
    members: &[ArchiveMember],
) -> Result<(), StoreError> {
    let bytes =
        postcard::to_allocvec(members).map_err(|e| StoreError::Serialization(e.to_string()))?;
    let write_txn = db.begin_write()?;
    {
        let mut table = write_txn.open_table(ARCHIVE_MEMBERS)?;
        table.insert(rel_path, bytes.as_slice())?;
    }
    write_txn.commit()?;
    Ok(())
}

/// Store an archive's working password **already encrypted** (see
/// [`crate::secret`]). The plaintext must never reach this function.
pub fn set_archive_password(
    db: &redb::Database,
    rel_path: &str,
    encrypted: &[u8],
) -> Result<(), StoreError> {
    let write_txn = db.begin_write()?;
    {
        let mut table = write_txn.open_table(ARCHIVE_PASSWORDS)?;
        table.insert(rel_path, encrypted)?;
    }
    write_txn.commit()?;
    Ok(())
}

/// The encrypted working password for an archive, if one is stored.
pub fn get_archive_password(
    db: &redb::Database,
    rel_path: &str,
) -> Result<Option<Vec<u8>>, StoreError> {
    let read_txn = db.begin_read()?;
    let table = match read_txn.open_table(ARCHIVE_PASSWORDS) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    Ok(table.get(rel_path)?.map(|v| v.value().to_vec()))
}

/// Iterate every indexed archive's `(rel_path, members)`.
pub fn for_each_archive_members<F>(db: &redb::Database, mut f: F) -> Result<(), StoreError>
where
    F: FnMut(&str, Vec<ArchiveMember>) -> Result<(), StoreError>,
{
    let read_txn = db.begin_read()?;
    let table = match read_txn.open_table(ARCHIVE_MEMBERS) {
        Ok(t) => t,
        // A repo whose db predates the table simply has no archives indexed.
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for item in table.iter()? {
        let (key, value) = item?;
        // A row that predates the current member format cannot be decoded;
        // skip it rather than failing the whole report — the next scan of that
        // archive repopulates it in the current format.
        let members: Vec<ArchiveMember> = match postcard::from_bytes(value.value()) {
            Ok(m) => m,
            Err(_) => continue,
        };
        f(key.value(), members)?;
    }
    Ok(())
}

/// Mark the given paths missing within a single write transaction.
/// Paths without an entry or already missing are left untouched.
pub fn mark_missing<'a, I>(db: &redb::Database, rel_paths: I) -> Result<(), StoreError>
where
    I: IntoIterator<Item = &'a str>,
{
    let write_txn = db.begin_write()?;
    {
        let mut tables = RepoTables::open(&write_txn)?;
        for rel_path in rel_paths {
            if let Some(mut entry) = tables.get_entry(rel_path)?
                && !entry.missing
            {
                entry.missing = true;
                tables.upsert(rel_path, &entry)?;
            }
        }
    }
    write_txn.commit()?;
    Ok(())
}

/// Remove the given relative paths from an open repo database in one write
/// transaction (a no-op for paths that aren't present). Mirrors [`apply_entries`]
/// / [`mark_missing`] for callers that move or drop entries in batches.
pub fn remove_entries<'a, I>(db: &redb::Database, rel_paths: I) -> Result<(), StoreError>
where
    I: IntoIterator<Item = &'a str>,
{
    let write_txn = db.begin_write()?;
    {
        let mut tables = RepoTables::open(&write_txn)?;
        for rel_path in rel_paths {
            tables.remove(rel_path)?;
        }
    }
    write_txn.commit()?;
    Ok(())
}

/// Move one entry from `from_rel` to `to_rel` in a single write transaction:
/// insert the entry at the new path and drop the old one together, so a crash
/// can never leave the same content indexed under both names (which would show
/// as a phantom duplicate and inflate `file_count`/`total_size`).
///
/// The entry is written verbatim — a rename keeps the content, and therefore
/// the fingerprints, untouched.
pub fn rename_entry(
    db: &redb::Database,
    from_rel: &str,
    to_rel: &str,
    entry: &FileEntry,
) -> Result<(), StoreError> {
    // Renaming onto the same name would upsert then remove the same key —
    // deleting the entry outright. There is nothing to move, so do nothing.
    if from_rel == to_rel {
        return Ok(());
    }
    let write_txn = db.begin_write()?;
    {
        let mut tables = RepoTables::open(&write_txn)?;
        tables.upsert(to_rel, entry)?;
        tables.remove(from_rel)?;
    }
    write_txn.commit()?;
    Ok(())
}

/// Read one file entry from an open repo database.
pub fn get_entry(db: &redb::Database, rel_path: &str) -> Result<Option<FileEntry>, StoreError> {
    let read_txn = db.begin_read()?;
    let files_table = read_txn.open_table(FILES)?;
    match files_table.get(rel_path)? {
        Some(guard) => Ok(Some(decode_entry(guard.value())?.0)),
        None => Ok(None),
    }
}

/// Stream every file entry (including missing ones) to a callback without
/// materializing the whole index.
pub fn for_each_file_entry<F>(db: &redb::Database, mut f: F) -> Result<(), StoreError>
where
    F: FnMut(&str, FileEntry) -> Result<(), StoreError>,
{
    let read_txn = db.begin_read()?;
    let files_table = read_txn.open_table(FILES)?;
    for item in files_table.iter()? {
        let (key_guard, val_guard) = item?;
        let (entry, _) = decode_entry(val_guard.value())?;
        f(key_guard.value(), entry)?;
    }
    Ok(())
}

/// Every annotated file's tags in a repo database, keyed by rel-path (files
/// with no tags have no entry). Shared by [`Store::all_annotations`] and by
/// filter matching that needs to resolve `tag:` conditions.
pub fn annotations_of_db(
    db: &redb::Database,
) -> Result<std::collections::HashMap<String, Vec<String>>, StoreError> {
    let read_txn = db.begin_read()?;
    let mut out = std::collections::HashMap::new();
    let table = match read_txn.open_table(ANNOTATIONS) {
        Ok(t) => t,
        // The table is created lazily on first write; absent → no annotations.
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(out),
        Err(e) => return Err(e.into()),
    };
    for item in table.iter()? {
        let (key, val) = item?;
        let tags: Vec<String> = deserialize_value(SCHEMA_VERSION, val.value())?;
        out.insert(key.value().to_string(), tags);
    }
    Ok(out)
}

/// Every accepted content key in a repo database. Shared by
/// [`Store::all_accepted`] and by filter matching that resolves `accepted:`.
pub fn accepted_of_db(db: &redb::Database) -> Result<HashSet<ContentKey>, StoreError> {
    let read_txn = db.begin_read()?;
    let mut out = HashSet::new();
    let table = match read_txn.open_table(ACCEPTED) {
        Ok(t) => t,
        // The table is created lazily on first write; absent → nothing accepted.
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(out),
        Err(e) => return Err(e.into()),
    };
    for item in table.iter()? {
        let (key, _) = item?;
        let (size, hash) = key.value();
        let hash: [u8; 32] = hash.try_into().map_err(|_| {
            StoreError::Deserialization("accepted key with a non-32-byte hash".into())
        })?;
        out.insert((size, hash));
    }
    Ok(out)
}

/// Presence state of one content key (size, hash) in a repo.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContentState {
    /// At least one non-missing entry has this content.
    pub present: bool,
    /// At least one missing entry had this content.
    pub missing: bool,
}

/// Content key of a file: equality is by size and hash, never by path.
pub type ContentKey = (u64, [u8; 32]);

/// Build a map of every content key in the repo to its presence state.
pub fn read_content_index(
    db: &redb::Database,
) -> Result<std::collections::HashMap<ContentKey, ContentState>, StoreError> {
    let mut index = std::collections::HashMap::new();
    for_each_file_entry(db, |_, entry| {
        let state: &mut ContentState = index.entry((entry.size, entry.hash)).or_default();
        if entry.missing {
            state.missing = true;
        } else {
            state.present = true;
        }
        Ok(())
    })?;
    Ok(index)
}

/// Paths of all non-missing entries with the given size and hash.
pub fn get_paths_by_size_hash(
    db: &redb::Database,
    size: u64,
    hash: &[u8; 32],
) -> Result<Vec<String>, StoreError> {
    let read_txn = db.begin_read()?;
    let by_size_hash = read_txn.open_multimap_table(BY_SIZE_HASH)?;
    let mut paths = Vec::new();
    for item in by_size_hash.get((size, &hash[..]))? {
        paths.push(item?.value().to_string());
    }
    Ok(paths)
}

/// Whether `state` says this content survives only as **tombstones** — the
/// index has seen it, but every path that ever held it is marked missing
/// (deleted). This is the "was deleted here" signal: copying such content
/// back in would resurrect a deletion. Probe with a [`read_content_index`]
/// map (`BY_SIZE_HASH` cannot answer this — it indexes live paths only).
pub fn content_tombstoned(
    index: &std::collections::HashMap<ContentKey, ContentState>,
    size: u64,
    hash: &[u8; 32],
) -> bool {
    index
        .get(&(size, *hash))
        .is_some_and(|s| s.missing && !s.present)
}

/// The subset of a [`FileEntry`] the update scan needs for change detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanEntry {
    pub size: u64,
    pub modified_ms: i64,
    pub missing: bool,
    /// Entry predates the current image-fingerprint format and must be
    /// re-hashed even if (size, mtime) still match.
    pub stale: bool,
}

/// Read the scan-relevant state of every indexed file.
pub fn read_scan_index(
    db: &redb::Database,
) -> Result<std::collections::HashMap<String, ScanEntry>, StoreError> {
    let read_txn = db.begin_read()?;
    let files_table = read_txn.open_table(FILES)?;
    let mut index = std::collections::HashMap::new();
    for item in files_table.iter()? {
        let (key_guard, val_guard) = item?;
        let rel = key_guard.value();
        let (entry, version) = decode_entry(val_guard.value())?;
        // Per-mime rescan policy: images below v4 (image-hash + EXIF), and
        // office documents below v5 (their text hash was added at v5). PDFs
        // already had a text hash, so they are not re-scanned.
        let stale = match entry.mime.as_deref() {
            Some(m) if m.starts_with("image/") => version < 4,
            Some(m) if crate::fingerprint::is_office_doc(m) => version < 5,
            Some(m) if m.starts_with("text/") => version < 6,
            Some("message/rfc822") => version < 7,
            // v9 reclassified MP4-container audio (.m4b/.m4a used to sniff as
            // plain "video/mp4") — only the audio-named files re-scan, never
            // the whole video corpus.
            Some("video/mp4") if version < 9 => {
                version < 8
                    || crate::fingerprint::audio_mime_by_name(std::path::Path::new(rel)).is_some()
            }
            Some(m) if m.starts_with("video/") => version < 8,
            _ => false,
        };
        index.insert(
            rel.to_string(),
            ScanEntry {
                size: entry.size,
                modified_ms: entry.modified_ms,
                missing: entry.missing,
                stale,
            },
        );
    }
    Ok(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A version-1 sync group stored one group-wide mode. Decoding it now must
    /// give every sink that same mode (the standing rule that store-format
    /// changes ship with a legacy-decode test).
    #[test]
    fn v1_sync_group_decodes_with_each_sink_inheriting_the_group_mode()
    -> Result<(), Box<dyn std::error::Error>> {
        // Encode exactly as the old code did: the shared version byte + a
        // postcard `SyncGroupV1`.
        let v1 = SyncGroupV1 {
            main: "MAIN".to_string(),
            sinks: vec!["SINK1".to_string(), "SINK2".to_string()],
            mode: SyncMode::Mirror,
        };
        let bytes = serialize_value(SCHEMA_VERSION, &v1)?;

        let group = decode_sync_group(&bytes)?;
        assert_eq!(group.main, "MAIN");
        assert_eq!(
            group.sinks,
            vec![
                SyncSink {
                    repo: "SINK1".to_string(),
                    mode: SyncMode::Mirror,
                },
                SyncSink {
                    repo: "SINK2".to_string(),
                    mode: SyncMode::Mirror,
                },
            ],
            "every legacy sink inherits the old group-wide mode"
        );

        // A current (v2) value round-trips unchanged.
        let bytes2 = serialize_value(SYNC_GROUP_VERSION, &group)?;
        assert_eq!(decode_sync_group(&bytes2)?, group);
        Ok(())
    }

    /// A version-1 repo meta predates the `remote` flag. Decoding it now must
    /// yield `remote: false` (the standing rule that store-format changes ship
    /// with a legacy-decode test), and the flag must survive a set/get.
    #[test]
    fn v1_repo_meta_decodes_as_local_and_the_flag_round_trips()
    -> Result<(), Box<dyn std::error::Error>> {
        let v1 = RepoMetaV1 {
            abs_path: "/data/pile".to_string(),
            created: 1_700_000_000,
            hash_algo: "BLAKE3".to_string(),
            schema_ver: SCHEMA_VERSION,
        };
        let bytes = serialize_value(SCHEMA_VERSION, &v1)?;
        let meta = decode_repo_meta(&bytes)?;
        assert_eq!(meta.abs_path, "/data/pile");
        assert!(!meta.remote, "a pre-flag registry entry reads as local");

        // A current value round-trips unchanged.
        let bytes2 = serialize_value(REPO_META_VERSION, &meta)?;
        assert_eq!(decode_repo_meta(&bytes2)?, meta);

        // And the flag persists through the store API.
        let tmp = tempfile::tempdir()?;
        let store = Store::open_at(tmp.path().to_path_buf())?;
        let data = tmp.path().join("repo");
        std::fs::create_dir_all(&data)?;
        store.create_repo("r", &data.to_string_lossy())?;
        assert!(!store.get_repo("r")?.remote, "new repos start local");
        store.set_repo_remote("r", true)?;
        assert!(store.get_repo("r")?.remote, "the flag sticks");
        store.set_repo_remote("r", false)?;
        assert!(!store.get_repo("r")?.remote, "and flips back");
        Ok(())
    }

    #[test]
    fn accepted_content_round_trips_and_resolves_paths() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let store = Store::open_at(tmp.path().to_path_buf())?;
        let repo_dir = tmp.path().join("r");
        std::fs::create_dir_all(&repo_dir)?;
        store.create_repo("r", &repo_dir.to_string_lossy())?;
        let entry = |hash: u8| FileEntry {
            size: 10,
            hash: [hash; 32],
            modified_ms: 0,
            missing: false,
            mime: None,
            img_fingerprint: None,
            video_hash: None,
            pdf_hash: None,
            audio: None,
            img_size: None,
            origin: None,
            exif: None,
        };
        store.update_file_entry("r", "a/cover.jpg", &entry(1))?;
        store.update_file_entry("r", "b/cover.jpg", &entry(1))?;
        store.update_file_entry("r", "other.jpg", &entry(2))?;

        // Nothing accepted yet (table not even created).
        assert!(store.all_accepted("r")?.is_empty());
        assert!(store.accepted_paths("r")?.is_empty());

        store.accept_content("r", 10, &[1; 32])?;
        store.accept_content("r", 10, &[1; 32])?; // idempotent
        assert_eq!(store.all_accepted("r")?, HashSet::from([(10, [1u8; 32])]));
        assert_eq!(
            store.accepted_paths("r")?,
            HashSet::from(["a/cover.jpg".to_string(), "b/cover.jpg".to_string()]),
            "every copy of the accepted content resolves, the other file does not"
        );

        // An orphan mark (content no longer indexed) is kept, never swept.
        store.accept_content("r", 99, &[9; 32])?;
        assert_eq!(store.all_accepted("r")?.len(), 2);

        assert!(store.unaccept_content("r", 10, &[1; 32])?);
        assert!(!store.unaccept_content("r", 10, &[1; 32])?, "already gone");
        assert!(store.accepted_paths("r")?.is_empty());
        Ok(())
    }

    #[test]
    fn annotations_round_trip_dedup_and_clear() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let store = Store::open_at(tmp.path().to_path_buf())?;
        let repo_dir = tmp.path().join("r");
        std::fs::create_dir_all(&repo_dir)?;
        store.create_repo("r", &repo_dir.to_string_lossy())?;

        // No annotations yet (table not even created).
        assert!(store.get_annotations("r", "a.jpg")?.is_empty());
        assert!(store.all_annotations("r")?.is_empty());

        // Set tags (with a blank + a duplicate that get cleaned out).
        store.set_annotations(
            "r",
            "a.jpg",
            &[
                "important".into(),
                " ".into(),
                "keep".into(),
                "important".into(),
            ],
        )?;
        assert_eq!(
            store.get_annotations("r", "a.jpg")?,
            vec!["important", "keep"]
        );

        store.set_annotations("r", "b.png", &["trash".into()])?;
        let all = store.all_annotations("r")?;
        assert_eq!(all.len(), 2);
        assert_eq!(all.get("b.png"), Some(&vec!["trash".to_string()]));

        // Modify, then clear.
        store.set_annotations("r", "a.jpg", &["review".into()])?;
        assert_eq!(store.get_annotations("r", "a.jpg")?, vec!["review"]);
        store.set_annotations("r", "a.jpg", &[])?;
        assert!(store.get_annotations("r", "a.jpg")?.is_empty());
        assert_eq!(store.all_annotations("r")?.len(), 1, "only b.png remains");
        Ok(())
    }

    #[test]
    fn test_store_invariants() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let store = Store::open_at(temp_dir.path().to_path_buf())?;

        // 1. Create a repository
        let repo_dir = temp_dir.path().join("mock_repo");
        std::fs::create_dir_all(&repo_dir)?;
        store.create_repo("test-repo", &repo_dir.to_string_lossy())?;

        // Check initial state
        let stats = store.get_repo_stats("test-repo")?;
        assert_eq!(stats.file_count, 0);
        assert_eq!(stats.total_size, 0);
        assert_eq!(stats.missing_count, 0);

        // 2. Insert some file entries
        let file1 = FileEntry {
            size: 100,
            hash: [1; 32],
            modified_ms: 123456,
            missing: false,
            mime: Some("image/png".to_string()),
            img_fingerprint: Some([42; 8]),
            video_hash: None,
            pdf_hash: None,
            audio: None,
            img_size: None,
            origin: None,
            exif: None,
        };

        let file2 = FileEntry {
            size: 200,
            hash: [2; 32],
            modified_ms: 123457,
            missing: false,
            mime: Some("image/png".to_string()),
            img_fingerprint: Some([43; 8]),
            video_hash: None,
            pdf_hash: None,
            audio: None,
            img_size: None,
            origin: None,
            exif: None,
        };

        store.update_file_entry("test-repo", "file1.png", &file1)?;
        store.update_file_entry("test-repo", "file2.png", &file2)?;

        // Verify stats updated correctly
        let stats = store.get_repo_stats("test-repo")?;
        assert_eq!(stats.file_count, 2);
        assert_eq!(stats.total_size, 300);
        assert_eq!(stats.missing_count, 0);

        // 3. Verify duplicate lookup (add file3.png with same size and hash as file1)
        let file3 = FileEntry {
            size: 100,
            hash: [1; 32], // Same as file1
            modified_ms: 123458,
            missing: false,
            mime: Some("image/png".to_string()),
            img_fingerprint: Some([42; 8]),
            video_hash: None,
            pdf_hash: None,
            audio: None,
            img_size: None,
            origin: None,
            exif: None,
        };
        store.update_file_entry("test-repo", "file3.png", &file3)?;

        let stats = store.get_repo_stats("test-repo")?;
        assert_eq!(stats.file_count, 3);
        assert_eq!(stats.total_size, 400);

        let dup_groups = store.get_duplicate_groups("test-repo")?;
        assert_eq!(dup_groups.len(), 1);
        assert_eq!(dup_groups[0].0, 100);
        assert_eq!(dup_groups[0].1, [1; 32]);
        assert!(dup_groups[0].2.contains(&"file1.png".to_string()));
        assert!(dup_groups[0].2.contains(&"file3.png".to_string()));

        // 4. Mark file3 missing, check it gets excluded from index & stats
        let mut file3_missing = file3.clone();
        file3_missing.missing = true;
        store.update_file_entry("test-repo", "file3.png", &file3_missing)?;

        let stats = store.get_repo_stats("test-repo")?;
        assert_eq!(stats.file_count, 2); // Decremented from 3 to 2
        assert_eq!(stats.total_size, 300); // Decremented from 400 to 300
        assert_eq!(stats.missing_count, 1);

        // Check it is excluded from duplicates
        let dup_groups = store.get_duplicate_groups("test-repo")?;
        assert!(dup_groups.is_empty()); // No duplicate groups now as file3 is missing

        // 5. Test repo list stats are loaded from META directly
        let list = store.list_repos()?;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].0, "test-repo");
        assert_eq!(list[0].2.file_count, 2);
        assert_eq!(list[0].2.total_size, 300);
        assert_eq!(list[0].2.missing_count, 1);

        // 6. Test rename and relocate
        store.rename_repo("test-repo", "renamed-repo")?;
        assert!(
            store.get_repo_stats("test-repo").is_err()
                || !store.get_repo_db_path("test-repo").exists()
        );
        let stats = store.get_repo_stats("renamed-repo")?;
        assert_eq!(stats.file_count, 2);

        store.relocate_repo("renamed-repo", "/mock/relocated/path")?;
        let repos = store.list_repos()?;
        assert_eq!(repos[0].0, "renamed-repo");
        assert_eq!(repos[0].1.abs_path, "/mock/relocated/path");

        // 7. Test remove
        store.remove_repo("renamed-repo")?;
        let repos = store.list_repos()?;
        assert!(repos.is_empty());

        Ok(())
    }

    /// Version-8 entries share the current layout and must stay readable
    /// as-is; only "video/mp4" entries whose *name* says audio (`.m4b`/`.m4a`
    /// — the mime misclassification v9 fixed) are flagged stale, never the
    /// whole video corpus.
    #[test]
    fn v8_entries_decode_and_only_audio_named_mp4_is_stale()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let store = Store::open_at(temp_dir.path().to_path_buf())?;
        let repo_dir = temp_dir.path().join("mock_repo");
        std::fs::create_dir_all(&repo_dir)?;
        store.create_repo("legacy", &repo_dir.to_string_lossy())?;

        let mp4 = |hash0: u8| FileEntry {
            size: 100,
            hash: [hash0; 32],
            modified_ms: 123456,
            missing: false,
            mime: Some("video/mp4".to_string()),
            img_fingerprint: None,
            video_hash: None,
            pdf_hash: None,
            audio: None,
            img_size: None,
            origin: None,
            exif: None,
        };
        let db = store.open_repo_db("legacy")?;
        let write_txn = db.begin_write()?;
        {
            let mut files = write_txn.open_table(FILES)?;
            files.insert("book.m4b", serialize_value(8, &mp4(1))?.as_slice())?;
            files.insert("clip.mp4", serialize_value(8, &mp4(2))?.as_slice())?;
        }
        write_txn.commit()?;

        let entry = get_entry(&db, "book.m4b")?.expect("v8 entry decodes");
        assert_eq!(entry.mime.as_deref(), Some("video/mp4"));

        let index = read_scan_index(&db)?;
        assert!(
            index["book.m4b"].stale,
            "an audio-named v8 mp4 entry re-scans to become audio"
        );
        assert!(!index["clip.mp4"].stale, "a real v8 video is left alone");
        Ok(())
    }

    /// Version-1 entries (64-bit image dHash) must stay readable: fields carry
    /// over, the incompatible fingerprint is dropped, and the scan index flags
    /// image entries stale so the next update re-fingerprints them.
    #[test]
    fn v1_entries_decode_and_flag_images_stale() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let store = Store::open_at(temp_dir.path().to_path_buf())?;
        let repo_dir = temp_dir.path().join("mock_repo");
        std::fs::create_dir_all(&repo_dir)?;
        store.create_repo("legacy", &repo_dir.to_string_lossy())?;

        let v1_image = FileEntryV1 {
            size: 100,
            hash: [1; 32],
            modified_ms: 123456,
            missing: false,
            mime: Some("image/jpeg".to_string()),
            img_fingerprint: Some(0xf8f0_f0f0_f0f0_f0f8),
            video_hash: None,
            pdf_hash: None,
            audio: None,
            img_size: Some((100, 100)),
        };
        let v1_pdf = FileEntryV1 {
            size: 200,
            hash: [2; 32],
            modified_ms: 123457,
            missing: false,
            mime: Some("application/pdf".to_string()),
            img_fingerprint: None,
            video_hash: None,
            pdf_hash: Some([3; 32]),
            audio: None,
            img_size: None,
        };

        let db = store.open_repo_db("legacy")?;
        let write_txn = db.begin_write()?;
        {
            let mut files = write_txn.open_table(FILES)?;
            files.insert("photo.jpg", serialize_value(1, &v1_image)?.as_slice())?;
            files.insert("doc.pdf", serialize_value(1, &v1_pdf)?.as_slice())?;
        }
        write_txn.commit()?;

        let entry = get_entry(&db, "photo.jpg")?.expect("v1 entry decodes");
        assert_eq!(entry.size, 100);
        assert_eq!(entry.mime.as_deref(), Some("image/jpeg"));
        assert_eq!(entry.img_fingerprint, None, "64-bit fingerprint dropped");
        assert_eq!(entry.img_size, Some((100, 100)));

        let index = read_scan_index(&db)?;
        assert!(index["photo.jpg"].stale, "v1 image entry is stale");
        assert!(!index["doc.pdf"].stale, "non-image v1 entry is not stale");

        // Rewriting the image entry at the current version clears staleness.
        let mut upgraded = entry;
        upgraded.img_fingerprint = Some([42; 8]);
        drop(db);
        store.update_file_entry("legacy", "photo.jpg", &upgraded)?;
        let db = store.open_repo_db("legacy")?;
        let index = read_scan_index(&db)?;
        assert!(!index["photo.jpg"].stale);

        Ok(())
    }

    /// Version-2 entries (512-bit image hash, pre-provenance/EXIF) decode
    /// unchanged: `origin` and `exif` default to `None`. The v4 EXIF bump does
    /// re-flag images stale (the file must be re-read for EXIF); non-images stay
    /// fresh.
    #[test]
    fn v2_entries_decode_with_none_extras_and_restale_images()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let store = Store::open_at(temp_dir.path().to_path_buf())?;
        let repo_dir = temp_dir.path().join("mock_repo");
        std::fs::create_dir_all(&repo_dir)?;
        store.create_repo("legacy", &repo_dir.to_string_lossy())?;

        let v2_image = FileEntryV2 {
            size: 100,
            hash: [1; 32],
            modified_ms: 123456,
            missing: false,
            mime: Some("image/jpeg".to_string()),
            img_fingerprint: Some([7; 8]),
            video_hash: None,
            pdf_hash: None,
            audio: None,
            img_size: Some((100, 100)),
        };

        let db = store.open_repo_db("legacy")?;
        let write_txn = db.begin_write()?;
        {
            let mut files = write_txn.open_table(FILES)?;
            files.insert("photo.jpg", serialize_value(2, &v2_image)?.as_slice())?;
        }
        write_txn.commit()?;

        // A v2 non-image (PDF) alongside, to confirm it stays fresh.
        let v2_pdf = FileEntryV2 {
            size: 200,
            hash: [2; 32],
            modified_ms: 1,
            missing: false,
            mime: Some("application/pdf".to_string()),
            img_fingerprint: None,
            video_hash: None,
            pdf_hash: Some([3; 32]),
            audio: None,
            img_size: None,
        };
        {
            let write_txn = db.begin_write()?;
            {
                let mut files = write_txn.open_table(FILES)?;
                files.insert("doc.pdf", serialize_value(2, &v2_pdf)?.as_slice())?;
            }
            write_txn.commit()?;
        }

        let entry = get_entry(&db, "photo.jpg")?.expect("v2 entry decodes");
        assert_eq!(
            entry.img_fingerprint,
            Some([7; 8]),
            "512-bit hash preserved"
        );
        assert_eq!(entry.origin, None, "origin defaults to None");
        assert_eq!(entry.exif, None, "exif defaults to None");

        let index = read_scan_index(&db)?;
        assert!(
            index["photo.jpg"].stale,
            "v2 image is re-flagged stale for EXIF re-read"
        );
        assert!(!index["doc.pdf"].stale, "v2 non-image stays fresh");
        Ok(())
    }
}
