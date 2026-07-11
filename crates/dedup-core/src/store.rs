//! Storage implementation using redb for registry and repo-specific databases.

use redb::{ReadableMultimapTable, ReadableTable};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

const SCHEMA_VERSION: u8 = 1;
/// Version byte of serialized [`FileEntry`] values. v2 grew the image
/// fingerprint from a 64-bit dHash to the 512-bit [`ImgHash`] (v1 entries decode
/// but drop the fingerprint and are flagged stale for re-hashing); v3 added
/// `origin` provenance, a decode-compatible change (old entries get `origin =
/// None` and are NOT re-flagged stale). See [`decode_entry`].
const ENTRY_VERSION: u8 = 3;

// Registry table definition
const REPOS: redb::TableDefinition<&str, &[u8]> = redb::TableDefinition::new("repos");

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

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct RepoMeta {
    pub abs_path: String,
    pub created: u64,
    pub hash_algo: String,
    pub schema_ver: u8,
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
    pub video_hash: Option<[u64; 3]>,     // temporal hash
    pub pdf_hash: Option<[u8; 32]>,       // blake3 of normalized text
    pub audio: Option<AudioFp>,           // duration_ms + chunk hashes
    pub img_size: Option<(u32, u32)>,
    /// Provenance: the source repo a file was copied/synced from (set by
    /// `diff_copy`/`sync_copy` when the target is a repo). `None` for scanned
    /// files and pre-provenance entries. A display/filter hint, never an
    /// identity input.
    pub origin: Option<String>,
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

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct AudioFp {
    pub duration_ms: u32,
    pub chunk_hashes: Vec<[u8; 32]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepoStats {
    pub file_count: u64,
    pub total_size: u64,
    pub missing_count: u64,
    /// Epoch milliseconds of the last completed scan; 0 if never scanned.
    pub last_scan_ms: u64,
    /// Epoch milliseconds when this repo was marked triage-done (its unique
    /// content copied into a sanitized dir); 0 if not yet done.
    pub triage_done_ms: u64,
}

pub type DuplicateGroup = (u64, [u8; 32], Vec<String>);

/// The head of one `BY_SIZE_HASH` cursor during a k-way merge: the smallest
/// not-yet-consumed `(size, hash)` and its member rel-paths, or `None` at end.
type MergeHead = Option<(u64, [u8; 32], Vec<String>)>;

/// The cache of shared repo index handles, keyed by repo name.
type RepoDbMap = HashMap<String, Arc<redb::Database>>;

pub struct Store {
    config_dir: PathBuf,
    registry: redb::Database,
    /// One shared handle per repo index. redb permits a single live
    /// [`redb::Database`] per file (it holds an exclusive file lock), but that
    /// one instance supports any number of concurrent readers plus one writer
    /// (MVCC) — so every user must share the cached handle instead of
    /// re-opening the file, or concurrent operations fail with
    /// "database already open".
    repo_dbs: Mutex<RepoDbMap>,
}

fn get_config_dir() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".config").join("dedup")
    } else {
        PathBuf::from(".config").join("dedup")
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
        Some(2) => {
            let v2: FileEntryV2 = deserialize_value(2, bytes)?;
            let entry = FileEntry {
                size: v2.size,
                hash: v2.hash,
                modified_ms: v2.modified_ms,
                missing: v2.missing,
                mime: v2.mime,
                img_fingerprint: v2.img_fingerprint,
                video_hash: v2.video_hash,
                pdf_hash: v2.pdf_hash,
                audio: v2.audio,
                img_size: v2.img_size,
                origin: None,
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
                video_hash: v1.video_hash,
                pdf_hash: v1.pdf_hash,
                audio: v1.audio,
                img_size: v1.img_size,
                origin: None,
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
        }
        write_txn.commit()?;

        Ok(Self {
            config_dir,
            registry,
            repo_dbs: Mutex::new(HashMap::new()),
        })
    }

    /// Lock the repo handle map. A poisoned lock only means another thread
    /// panicked while holding it; the map itself (plain inserts/removes of
    /// `Arc`s) is always consistent, so recover instead of propagating panics.
    fn repo_dbs(&self) -> MutexGuard<'_, RepoDbMap> {
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
    /// (readers never block; writers serialize per batch).
    pub fn open_repo_db(&self, name: &str) -> Result<Arc<redb::Database>, StoreError> {
        let mut dbs = self.repo_dbs();
        self.open_repo_db_in(&mut dbs, name)
    }

    /// [`Self::open_repo_db`] against an already-locked handle map, so callers
    /// that must hold the lock across a file operation can open without
    /// re-locking (the map mutex is not reentrant).
    fn open_repo_db_in(
        &self,
        dbs: &mut RepoDbMap,
        name: &str,
    ) -> Result<Arc<redb::Database>, StoreError> {
        if let Some(db) = dbs.get(name) {
            return Ok(Arc::clone(db));
        }

        let path = self.get_repo_db_path(name);
        if let Some(parent) = path.parent()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent)?;
        }
        let db = redb::Database::create(&path)?;

        // Ensure tables exist
        let write_txn = db.begin_write()?;
        {
            let _files = write_txn.open_table(FILES)?;
            let _by_size_hash = write_txn.open_multimap_table(BY_SIZE_HASH)?;
            let _by_fprint = write_txn.open_multimap_table(BY_FPRINT)?;
            let _meta = write_txn.open_table(META)?;
            let _mime_stats = write_txn.open_table(MIME_STATS)?;
            // Drop the pre-ImgHash fingerprint index if this repo predates it.
            let _ = write_txn.delete_multimap_table(BY_FPRINT_LEGACY);
        }
        write_txn.commit()?;

        let db = Arc::new(db);
        dbs.insert(name.to_string(), Arc::clone(&db));
        Ok(db)
    }

    /// Remove `name`'s cached handle so its index file may be deleted, renamed
    /// or copied. Fails with [`StoreError::Busy`] while any operation still
    /// holds the handle (e.g. a running scan). The caller must keep holding
    /// `dbs` across the following file operation so no thread re-opens the
    /// index mid-change.
    fn evict_repo_db(&self, dbs: &mut RepoDbMap, name: &str) -> Result<(), StoreError> {
        if let Some(db) = dbs.get(name) {
            if Arc::strong_count(db) > 1 {
                return Err(StoreError::Busy(name.to_string()));
            }
            dbs.remove(name);
        }
        Ok(())
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
        };
        let serialized = serialize_value(SCHEMA_VERSION, &meta)?;

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
            Some(guard) => deserialize_value(SCHEMA_VERSION, guard.value()),
            None => Err(StoreError::NotFound(name.to_string())),
        }
    }

    pub fn list_repos(&self) -> Result<Vec<(String, RepoMeta, RepoStats)>, StoreError> {
        let read_txn = self.registry.begin_read()?;
        let table = read_txn.open_table(REPOS)?;

        let mut repos = Vec::new();
        for item in table.iter()? {
            let (name_guard, val_guard) = item?;
            let name = name_guard.value().to_string();
            let meta: RepoMeta = deserialize_value(SCHEMA_VERSION, val_guard.value())?;
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
                triage_done_ms: 0,
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
            triage_done_ms: get("triage_done_ms")?,
        })
    }

    /// Mark a repo triage-done now (or clear it with `done == false`). Records an
    /// epoch-millisecond timestamp in the repo's `META` table.
    pub fn set_triage_done(&self, name: &str, done: bool) -> Result<(), StoreError> {
        let db = self.open_repo_db(name)?;
        let ms = if done {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64
        } else {
            0
        };
        let write_txn = db.begin_write()?;
        {
            let mut meta = write_txn.open_table(META)?;
            meta.insert("triage_done_ms", ms)?;
        }
        write_txn.commit()?;
        Ok(())
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

    pub fn remove_repo(&self, name: &str) -> Result<(), StoreError> {
        // Held across the registry removal and the directory delete so no
        // thread can re-open the index mid-removal.
        let mut dbs = self.repo_dbs();
        self.evict_repo_db(&mut dbs, name)?;

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
        // Held across the registry update and the directory rename so no
        // thread can re-open either index mid-rename.
        let mut dbs = self.repo_dbs();
        self.evict_repo_db(&mut dbs, name)?;

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
            deserialize_value::<RepoMeta>(SCHEMA_VERSION, &meta_bytes)?
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
        let serialized = serialize_value(SCHEMA_VERSION, &meta)?;

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
        // index, so evict its handle (fails while e.g. a scan holds it) and
        // keep the map locked until the copy is done.
        let mut dbs = self.repo_dbs();
        self.evict_repo_db(&mut dbs, source)?;

        let source_meta = self.get_repo(source)?;

        let meta = RepoMeta {
            abs_path: canonicalize_path(new_path),
            created: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            hash_algo: source_meta.hash_algo,
            schema_ver: source_meta.schema_ver,
        };
        let serialized = serialize_value(SCHEMA_VERSION, &meta)?;

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
                // Source was never scanned: start the copy with empty tables.
                self.open_repo_db_in(&mut dbs, dest)?;
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
        let (entry, version) = decode_entry(val_guard.value())?;
        // Only the v1→v2 image-hash upgrade forces a rescan. Later bumps (v3
        // added `origin`, which defaults to `None`) are decode-compatible and
        // must not re-mark images stale.
        let stale = version < 2
            && entry
                .mime
                .as_deref()
                .is_some_and(|m| m.starts_with("image/"));
        index.insert(
            key_guard.value().to_string(),
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

    /// Version-2 entries (512-bit image hash, pre-provenance) decode unchanged:
    /// `origin` defaults to `None` and — crucially — the v3 bump does NOT
    /// re-flag images stale (it is a decode-compatible change, no rescan).
    #[test]
    fn v2_entries_decode_with_none_origin_and_no_restale() -> Result<(), Box<dyn std::error::Error>>
    {
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

        let entry = get_entry(&db, "photo.jpg")?.expect("v2 entry decodes");
        assert_eq!(
            entry.img_fingerprint,
            Some([7; 8]),
            "512-bit hash preserved"
        );
        assert_eq!(entry.origin, None, "origin defaults to None");

        let index = read_scan_index(&db)?;
        assert!(
            !index["photo.jpg"].stale,
            "v2 image is not re-marked stale by the v3 provenance bump"
        );
        Ok(())
    }
}
