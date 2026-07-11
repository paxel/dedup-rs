//! Archive member indexing and coverage. An inherited `backup_2019.zip` that
//! contains nothing you don't already have loose is the single biggest
//! redundancy class on old disks — but its bytes differ from the loose files,
//! so content dedup never sees it. This indexes each archive's members by
//! content identity (size + BLAKE3) into a side table, then reports what
//! fraction of an archive is already present elsewhere. Report-only: nothing
//! here deletes or extracts.
//!
//! Handled one level deep (zip, tar, tar.gz/tgz); nested archives are opaque
//! members. Encrypted or unreadable archives are skipped.

use crate::store::{self, ArchiveMember, ContentKey, ContentState, Store, StoreError};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

enum Kind {
    Zip,
    Tar,
    TarGz,
}

fn detect_kind(rel_path: &str, mime: Option<&str>) -> Option<Kind> {
    let lower = rel_path.to_lowercase();
    if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        return Some(Kind::TarGz);
    }
    if lower.ends_with(".tar") || mime == Some("application/x-tar") {
        return Some(Kind::Tar);
    }
    if lower.ends_with(".zip") || mime == Some("application/zip") {
        return Some(Kind::Zip);
    }
    None
}

/// Whether a file looks like a supported archive (by extension or MIME).
pub fn is_archive(rel_path: &str, mime: Option<&str>) -> bool {
    detect_kind(rel_path, mime).is_some()
}

/// Read `r` fully, returning `(size, blake3)` — the content identity used to
/// match a member against loose repo content.
fn hash_reader<R: Read>(mut r: R) -> Option<(u64, [u8; 32])> {
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let n = r.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    Some((size, *hasher.finalize().as_bytes()))
}

/// List an archive's file members with their content identity, or `None` when
/// the archive can't be read (encrypted, corrupt, unsupported).
pub fn list_members(path: &Path, rel_path: &str, mime: Option<&str>) -> Option<Vec<ArchiveMember>> {
    match detect_kind(rel_path, mime)? {
        Kind::Zip => zip_members(path),
        Kind::Tar => tar_members(std::fs::File::open(path).ok()?),
        Kind::TarGz => {
            let file = std::fs::File::open(path).ok()?;
            tar_members(flate2::read::GzDecoder::new(file))
        }
    }
}

fn zip_members(path: &Path) -> Option<Vec<ArchiveMember>> {
    let file = std::fs::File::open(path).ok()?;
    let mut archive = zip::ZipArchive::new(std::io::BufReader::new(file)).ok()?;
    let mut members = Vec::new();
    for i in 0..archive.len() {
        let mut entry = match archive.by_index(i) {
            Ok(e) => e,
            Err(_) => continue, // encrypted/corrupt member: skip
        };
        if !entry.is_file() {
            continue;
        }
        let name = entry.name().to_string();
        if let Some((size, hash)) = hash_reader(&mut entry) {
            members.push(ArchiveMember {
                rel_path: name,
                size,
                hash,
            });
        }
    }
    Some(members)
}

fn tar_members<R: Read>(reader: R) -> Option<Vec<ArchiveMember>> {
    let mut archive = tar::Archive::new(reader);
    let mut members = Vec::new();
    for entry in archive.entries().ok()? {
        let mut entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if entry.header().entry_type().is_dir() {
            continue;
        }
        let name = entry
            .path()
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        if let Some((size, hash)) = hash_reader(&mut entry) {
            members.push(ArchiveMember {
                rel_path: name,
                size,
                hash,
            });
        }
    }
    Some(members)
}

/// Index every archive in `repo` (found in its file index) into the
/// `ARCHIVE_MEMBERS` table. Opt-in and expensive — it reads each archive fully.
/// Returns the number of archives indexed.
pub fn index_repo_archives(store: &Store, repo: &str) -> Result<usize, StoreError> {
    let root = PathBuf::from(&store.get_repo(repo)?.abs_path);
    let db = store.open_repo_db(repo)?;

    let mut archives: Vec<(String, Option<String>)> = Vec::new();
    store::for_each_file_entry(&db, |rel_path, entry| {
        if !entry.missing && is_archive(rel_path, entry.mime.as_deref()) {
            archives.push((rel_path.to_string(), entry.mime.clone()));
        }
        Ok(())
    })?;

    let mut indexed = 0;
    for (rel, mime) in archives {
        if let Some(members) = list_members(&root.join(&rel), &rel, mime.as_deref()) {
            store::set_archive_members(&db, &rel, &members)?;
            indexed += 1;
        }
    }
    Ok(indexed)
}

/// Coverage of one archive: how many of its members' contents already exist in
/// the reference repos.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coverage {
    pub rel_path: String,
    pub members: usize,
    pub present: usize,
    /// Every member is present elsewhere (and there is at least one member):
    /// the archive is safe to delete.
    pub redundant: bool,
}

impl Coverage {
    /// Percent of members already present elsewhere (0 for an empty archive).
    pub fn percent(&self) -> f64 {
        if self.members == 0 {
            0.0
        } else {
            self.present as f64 / self.members as f64 * 100.0
        }
    }
}

/// Report coverage for every indexed archive in `archive_repo` against the
/// union of `references`' loose content. Archives must be indexed first
/// ([`index_repo_archives`]).
pub fn repo_archive_coverage(
    store: &Store,
    archive_repo: &str,
    references: &[&str],
) -> Result<Vec<Coverage>, StoreError> {
    let mut merged: HashMap<ContentKey, ContentState> = HashMap::new();
    for name in references {
        let db = store.open_repo_db(name)?;
        for (key, state) in store::read_content_index(&db)? {
            let slot = merged.entry(key).or_default();
            slot.present |= state.present;
            slot.missing |= state.missing;
        }
    }

    let db = store.open_repo_db(archive_repo)?;
    let mut out = Vec::new();
    store::for_each_archive_members(&db, |rel_path, members| {
        let total = members.len();
        let present = members
            .iter()
            .filter(|m| {
                merged
                    .get(&(m.size, m.hash))
                    .is_some_and(|state| state.present)
            })
            .count();
        out.push(Coverage {
            rel_path: rel_path.to_string(),
            members: total,
            present,
            redundant: total > 0 && present == total,
        });
        Ok(())
    })?;
    // Most-covered first, so fully-redundant archives surface at the top.
    out.sort_by(|a, b| {
        b.percent()
            .partial_cmp(&a.percent())
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.rel_path.cmp(&b.rel_path))
    });
    Ok(out)
}
