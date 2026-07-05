//! Exact duplicate detection within and across repos, ported from the legacy
//! `DuplicateRepoProcess`. Files are duplicates when (size, hash) match.
//!
//! Group ordering (the fixed "B3" behavior — sorted from day one):
//! - within a group: image area desc, size desc, oldest mtime first,
//!   relative path (case-insensitive) as tie-breaker;
//! - groups: wasted bytes (`(len - 1) * size`) descending.
//!
//! The first file of each sorted group is the "best" copy — deletion keeps it.

use crate::store::{self, ContentKey, FileEntry, Store, StoreError};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DupeFile {
    /// Name of the repo the file belongs to.
    pub repo: String,
    /// Absolute root directory of that repo.
    pub repo_root: String,
    /// Path relative to the repo root.
    pub rel_path: String,
    pub entry: FileEntry,
}

impl DupeFile {
    pub fn absolute_path(&self) -> PathBuf {
        Path::new(&self.repo_root).join(&self.rel_path)
    }
}

pub type DupeGroup = Vec<DupeFile>;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DupeDeleteStats {
    pub deleted: u64,
    pub errors: u64,
}

/// Find all exact duplicate groups across the given repos, sorted.
/// A file registered under the same absolute path in several repos is
/// counted once.
pub fn find_exact_duplicates(
    store: &Store,
    repo_names: &[String],
) -> Result<Vec<DupeGroup>, StoreError> {
    let mut by_content: HashMap<ContentKey, (Vec<DupeFile>, HashSet<PathBuf>)> = HashMap::new();

    for name in repo_names {
        let meta = store.get_repo(name)?;
        let db = store.open_repo_db(name)?;
        store::for_each_file_entry(&db, |rel_path, entry| {
            if entry.missing {
                return Ok(());
            }
            let (files, seen_paths) = by_content.entry((entry.size, entry.hash)).or_default();
            let abs = Path::new(&meta.abs_path).join(rel_path);
            if seen_paths.insert(abs) {
                files.push(DupeFile {
                    repo: name.clone(),
                    repo_root: meta.abs_path.clone(),
                    rel_path: rel_path.to_string(),
                    entry,
                });
            }
            Ok(())
        })?;
    }

    let mut groups: Vec<DupeGroup> = by_content
        .into_values()
        .map(|(files, _)| files)
        .filter(|group| group.len() > 1)
        .collect();
    sort_groups(&mut groups);
    Ok(groups)
}

/// Sort files within each group (best copy first) and order the groups by
/// wasted bytes descending.
pub fn sort_groups(groups: &mut [DupeGroup]) {
    for group in groups.iter_mut() {
        group.sort_by(|a, b| {
            image_area(&b.entry)
                .cmp(&image_area(&a.entry))
                .then(b.entry.size.cmp(&a.entry.size))
                .then(a.entry.modified_ms.cmp(&b.entry.modified_ms))
                .then_with(|| a.rel_path.to_lowercase().cmp(&b.rel_path.to_lowercase()))
        });
    }
    groups.sort_by_key(|group| std::cmp::Reverse(wasted_bytes(group)));
}

/// Bytes that could be reclaimed by keeping only one copy of the group.
pub fn wasted_bytes(group: &DupeGroup) -> u64 {
    match group.first() {
        Some(first) => (group.len() as u64 - 1) * first.entry.size,
        None => 0,
    }
}

fn image_area(entry: &FileEntry) -> i64 {
    entry
        .img_size
        .map(|(w, h)| i64::from(w) * i64::from(h))
        .unwrap_or(-1)
}

/// Delete all but the first (best) file of each group from disk and mark the
/// deleted entries missing — batched into one write transaction per repo.
/// Files already absent from disk are left untouched, matching the legacy
/// behavior. Best effort: failures are counted, not fatal.
pub fn delete_duplicates(
    store: &Store,
    groups: &[DupeGroup],
) -> Result<DupeDeleteStats, StoreError> {
    let extra: Vec<&DupeFile> = groups.iter().flat_map(|g| g.iter().skip(1)).collect();
    delete_files(store, &extra)
}

/// Delete an explicit set of files from disk and mark their index entries
/// missing — grouped so each repo's updates land in a single write transaction.
/// Files already absent from disk are skipped; failures are counted, not fatal.
pub fn delete_files(store: &Store, files: &[&DupeFile]) -> Result<DupeDeleteStats, StoreError> {
    let mut stats = DupeDeleteStats::default();
    let mut deleted_per_repo: HashMap<&str, Vec<&str>> = HashMap::new();

    for file in files {
        let path = file.absolute_path();
        if !path.exists() {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {
                stats.deleted += 1;
                deleted_per_repo
                    .entry(file.repo.as_str())
                    .or_default()
                    .push(file.rel_path.as_str());
            }
            Err(_) => stats.errors += 1,
        }
    }

    for (repo, rel_paths) in deleted_per_repo {
        let db = store.open_repo_db(repo)?;
        store::mark_missing(&db, rel_paths.iter().copied())?;
    }
    Ok(stats)
}
