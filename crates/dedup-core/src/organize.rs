//! Stage-4 ordering: browse the sanitized corpus by *when it happened* and
//! export it into a dated `<year>/<month>/` tree. Dates use the best-known
//! signal — EXIF capture time, falling back to file mtime — so it works
//! (degraded) even without EXIF. Export copies, never moves (v1).

use crate::filter::{self, FileFilter};
use crate::store::{self, Store, StoreError};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One year/month bucket with its file count and total bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bucket {
    pub year: i64,
    pub month: u32,
    pub count: u64,
    pub bytes: u64,
}

/// Bucket every non-missing file passing `filter` by (year, month) of its
/// best-known date. Streams the index; buckets come back chronologically.
pub fn timeline_buckets(
    store: &Store,
    repo_names: &[String],
    filter: Option<&str>,
) -> Result<Vec<Bucket>, StoreError> {
    let filter = FileFilter::parse(filter).map_err(|e| StoreError::Serialization(e.to_string()))?;
    let mut map: BTreeMap<(i64, u32), (u64, u64)> = BTreeMap::new();
    for name in repo_names {
        let db = store.open_repo_db(name)?;
        store::for_each_file_entry(&db, |rel_path, entry| {
            if entry.missing || !filter.matches(rel_path, &entry) {
                return Ok(());
            }
            let (y, m, _) = filter::ms_to_ymd(filter::best_date_ms(&entry));
            let slot = map.entry((y, m)).or_default();
            slot.0 += 1;
            slot.1 += entry.size;
            Ok(())
        })?;
    }
    Ok(map
        .into_iter()
        .map(|((year, month), (count, bytes))| Bucket {
            year,
            month,
            count,
            bytes,
        })
        .collect())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExportStats {
    pub copied: u64,
    /// Files whose content already exists at a destination (a re-run).
    pub skipped: u64,
    pub errors: u64,
}

/// Copy every non-missing file passing `filter` into
/// `target_dir/<year>/<month>/<filename>`, using the best-known date. Names
/// that collide get a numeric suffix; a destination that already holds the
/// same content is skipped, so re-running an export is idempotent. Existing
/// files are never overwritten. Never moves.
pub fn export_by_date(
    store: &Store,
    repo_names: &[String],
    target_dir: &Path,
    filter: Option<&str>,
) -> Result<ExportStats, StoreError> {
    let filter = FileFilter::parse(filter).map_err(|e| StoreError::Serialization(e.to_string()))?;
    let mut stats = ExportStats::default();
    for name in repo_names {
        let root = PathBuf::from(&store.get_repo(name)?.abs_path);
        let db = store.open_repo_db(name)?;
        // Collect first so the read transaction isn't held during file I/O.
        let mut files: Vec<(String, i64, u64, [u8; 32])> = Vec::new();
        store::for_each_file_entry(&db, |rel_path, entry| {
            if !entry.missing && filter.matches(rel_path, &entry) {
                files.push((
                    rel_path.to_string(),
                    filter::best_date_ms(&entry),
                    entry.size,
                    entry.hash,
                ));
            }
            Ok(())
        })?;

        for (rel_path, date_ms, size, hash) in files {
            let (year, month, _) = filter::ms_to_ymd(date_ms);
            let dir = target_dir
                .join(year.to_string())
                .join(format!("{month:02}"));
            if std::fs::create_dir_all(&dir).is_err() {
                stats.errors += 1;
                continue;
            }
            let file_name = Path::new(&rel_path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| rel_path.replace('/', "_"));
            match dest_for(&dir, &file_name, size, &hash) {
                Dest::Copy(dest) => match std::fs::copy(root.join(&rel_path), &dest) {
                    Ok(_) => stats.copied += 1,
                    Err(_) => stats.errors += 1,
                },
                Dest::AlreadyThere => stats.skipped += 1,
                Dest::Exhausted => stats.errors += 1,
            }
        }
    }
    Ok(stats)
}

enum Dest {
    /// Copy to this free path.
    Copy(PathBuf),
    /// A destination already holds this exact content (size + BLAKE3).
    AlreadyThere,
    /// No free candidate name found; nothing is ever overwritten.
    Exhausted,
}

/// Pick a destination in `dir` for `file_name`: the plain name, else ` (2)`,
/// ` (3)`… before the extension. An existing candidate with the same content
/// short-circuits to [`Dest::AlreadyThere`] so re-runs don't duplicate.
fn dest_for(dir: &Path, file_name: &str, size: u64, hash: &[u8; 32]) -> Dest {
    let (stem, ext) = match file_name.rsplit_once('.') {
        Some((s, e)) => (s.to_string(), format!(".{e}")),
        None => (file_name.to_string(), String::new()),
    };
    for n in 1..10_000 {
        let candidate = if n == 1 {
            dir.join(file_name)
        } else {
            dir.join(format!("{stem} ({n}){ext}"))
        };
        if !candidate.exists() {
            return Dest::Copy(candidate);
        }
        if is_same_content(&candidate, size, hash) {
            return Dest::AlreadyThere;
        }
    }
    Dest::Exhausted
}

/// Whether the file at `path` has exactly this size and BLAKE3 hash.
fn is_same_content(path: &Path, size: u64, hash: &[u8; 32]) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if meta.len() != size {
        return false;
    }
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut hasher = blake3::Hasher::new();
    if hasher.update_reader(file).is_err() {
        return false;
    }
    hasher.finalize().as_bytes() == hash
}
