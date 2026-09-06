//! Exact duplicate detection within and across repos, ported from the legacy
//! `DuplicateRepoProcess`. Files are duplicates when (size, hash) match.
//!
//! Group ordering (the fixed "B3" behavior — sorted from day one):
//! - within a group: image area desc, size desc, oldest mtime first,
//!   relative path (case-insensitive) as tie-breaker;
//! - groups: wasted bytes (`(len - 1) * size`) descending.
//!
//! The first file of each sorted group is the "best" copy — deletion keeps it.

use crate::filter::{AnnotatedFilter, FileFilter};
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

/// A lightweight descriptor of one exact-duplicate group — its content key and
/// member count, but none of the (potentially many) file entries. This is what
/// a streamed "plan" holds, so peak memory is O(number of duplicate groups)
/// rather than O(number of files).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DupeGroupKey {
    pub size: u64,
    pub hash: [u8; 32],
    pub count: u32,
}

impl DupeGroupKey {
    /// Bytes reclaimable by keeping a single copy of this group.
    pub fn wasted_bytes(&self) -> u64 {
        (u64::from(self.count) - 1) * self.size
    }
}

/// Enumerate every exact-duplicate group across `repo_names` as lightweight
/// descriptors, ordered by wasted bytes descending (ties broken deterministically
/// by size then content hash). Streams from the DB index without materializing
/// file entries — see [`store::Store::plan_duplicate_group_keys`]. `progress`
/// receives the running group count during the scan.
pub fn plan_exact_duplicates(
    store: &Store,
    repo_names: &[String],
    progress: impl FnMut(usize),
) -> Result<Vec<DupeGroupKey>, StoreError> {
    let mut plan: Vec<DupeGroupKey> = store
        .plan_duplicate_group_keys(repo_names, progress)?
        .into_iter()
        .map(|(size, hash, count)| DupeGroupKey { size, hash, count })
        .collect();
    plan.sort_by(|a, b| {
        b.wasted_bytes()
            .cmp(&a.wasted_bytes())
            .then(b.size.cmp(&a.size))
            .then(a.hash.cmp(&b.hash))
    });
    Ok(plan)
}

/// Materialize the full [`DupeFile`]s for a batch of group descriptors. Opens
/// each repo database once, so loading a page of groups is cheap. Members are
/// deduplicated by absolute path across repos and sorted best-copy-first.
pub fn load_groups(
    store: &Store,
    repo_names: &[String],
    keys: &[DupeGroupKey],
) -> Result<Vec<DupeGroup>, StoreError> {
    let mut roots: Vec<String> = Vec::with_capacity(repo_names.len());
    let mut dbs: Vec<std::sync::Arc<redb::Database>> = Vec::with_capacity(repo_names.len());
    for name in repo_names {
        roots.push(store.get_repo(name)?.abs_path);
        dbs.push(store.open_repo_db(name)?);
    }

    let mut out = Vec::with_capacity(keys.len());
    for key in keys {
        let mut seen: HashSet<PathBuf> = HashSet::new();
        let mut files: DupeGroup = Vec::new();
        for (i, name) in repo_names.iter().enumerate() {
            for rel in store::get_paths_by_size_hash(&dbs[i], key.size, &key.hash)? {
                let abs = Path::new(&roots[i]).join(&rel);
                if seen.insert(abs)
                    && let Some(entry) = store::get_entry(&dbs[i], &rel)?
                {
                    files.push(DupeFile {
                        repo: name.clone(),
                        repo_root: roots[i].clone(),
                        rel_path: rel,
                        entry,
                    });
                }
            }
        }
        sort_group_members(&mut files);
        out.push(files);
    }
    Ok(out)
}

/// Materialize a single group's [`DupeFile`]s (see [`load_groups`]).
pub fn load_group(
    store: &Store,
    repo_names: &[String],
    key: &DupeGroupKey,
) -> Result<DupeGroup, StoreError> {
    Ok(load_groups(store, repo_names, std::slice::from_ref(key))?
        .pop()
        .unwrap_or_default())
}

/// Find all exact duplicate groups across the given repos, sorted.
/// A file registered under the same absolute path in several repos is
/// counted once.
///
/// This is now a thin wrapper over [`plan_exact_duplicates`] + [`load_groups`],
/// so it no longer holds every (unique) file in memory. Callers that only show
/// a window of results should use the plan + load directly.
pub fn find_exact_duplicates(
    store: &Store,
    repo_names: &[String],
) -> Result<Vec<DupeGroup>, StoreError> {
    let plan = plan_exact_duplicates(store, repo_names, |_| {})?;
    load_groups(store, repo_names, &plan)
}

/// Keep only the group keys whose group has at least one member matching
/// `filter` — the Duplicates tab's "keep a whole group when any copy matches"
/// (a group is shown with *all* its copies, filtered only at the group level).
/// `None` returns the plan unchanged.
///
/// Members are streamed per key (like [`load_groups`]) so the memory-light paged
/// plan is preserved; each repo's annotations are loaded once up front. The
/// scan short-circuits on the first matching member of a group. `progress`
/// receives the running count of keys examined.
pub fn retain_matching_keys(
    store: &Store,
    repo_names: &[String],
    keys: Vec<DupeGroupKey>,
    filter: Option<&FileFilter>,
    mut progress: impl FnMut(usize),
) -> Result<Vec<DupeGroupKey>, StoreError> {
    let Some(filter) = filter else {
        return Ok(keys);
    };

    let mut dbs: Vec<std::sync::Arc<redb::Database>> = Vec::with_capacity(repo_names.len());
    for name in repo_names {
        dbs.push(store.open_repo_db(name)?);
    }
    // One annotated matcher per repo (tags are per repo+path); built once.
    let mut matchers: Vec<AnnotatedFilter> = Vec::with_capacity(dbs.len());
    for db in &dbs {
        matchers.push(AnnotatedFilter::new(db, filter)?);
    }

    let mut kept = Vec::with_capacity(keys.len());
    for (i, key) in keys.into_iter().enumerate() {
        let mut matched = false;
        'members: for j in 0..dbs.len() {
            for rel in store::get_paths_by_size_hash(&dbs[j], key.size, &key.hash)? {
                if let Some(entry) = store::get_entry(&dbs[j], &rel)?
                    && matchers[j].matches(&rel, &entry)
                {
                    matched = true;
                    break 'members;
                }
            }
        }
        if matched {
            kept.push(key);
        }
        progress(i + 1);
    }
    Ok(kept)
}

/// Sort files within each group (best copy first) and order the groups by
/// wasted bytes descending.
pub fn sort_groups(groups: &mut [DupeGroup]) {
    for group in groups.iter_mut() {
        sort_group_members(group);
    }
    groups.sort_by_key(|group| std::cmp::Reverse(wasted_bytes(group)));
}

/// Within each group, stably move the files the caller marks as protected
/// (read-only) ahead of the rest, so a protected copy becomes the kept "best"
/// and any writable duplicate falls to the deletable tail. The content order
/// from [`sort_groups`] is preserved within the protected and writable
/// partitions (a stable sort), so among equally-(un)protected copies the best
/// content still wins.
pub fn promote_protected_first(groups: &mut [DupeGroup], is_protected: impl Fn(&DupeFile) -> bool) {
    for group in groups.iter_mut() {
        // false (0) sorts first, so protected files lead; stable → ties keep
        // their existing best-first order.
        group.sort_by_key(|f| !is_protected(f));
    }
}

/// Order one group's files best-copy-first: image area desc, then (for
/// pixel-equal copies) the one with EXIF and the earliest capture date — an
/// original beats a re-save that stripped its metadata — then size desc, oldest
/// mtime first, then relative path (case-insensitive).
fn sort_group_members(group: &mut DupeGroup) {
    group.sort_by(|a, b| {
        image_area(&b.entry)
            .cmp(&image_area(&a.entry))
            .then_with(|| has_exif(&b.entry).cmp(&has_exif(&a.entry)))
            .then_with(|| taken_or_max(&a.entry).cmp(&taken_or_max(&b.entry)))
            .then(b.entry.size.cmp(&a.entry.size))
            .then(a.entry.modified_ms.cmp(&b.entry.modified_ms))
            .then_with(|| a.rel_path.to_lowercase().cmp(&b.rel_path.to_lowercase()))
    });
}

/// Whether an entry carries any EXIF (capture date or camera).
fn has_exif(entry: &FileEntry) -> bool {
    entry
        .exif
        .as_ref()
        .is_some_and(|e| e.taken_ms.is_some() || e.camera.is_some())
}

/// EXIF capture time, or `i64::MAX` when absent (so undated copies sort last).
fn taken_or_max(entry: &FileEntry) -> i64 {
    entry
        .exif
        .as_ref()
        .and_then(|e| e.taken_ms)
        .unwrap_or(i64::MAX)
}

/// Bytes that could be reclaimed by keeping only the first (best) copy of the
/// group. Sums the other members' sizes, which similar groups need — their
/// members are not byte-identical, so their sizes differ.
pub fn wasted_bytes(group: &DupeGroup) -> u64 {
    group.iter().skip(1).map(|f| f.entry.size).sum()
}

fn image_area(entry: &FileEntry) -> i64 {
    entry
        .img_size
        .map(|(w, h)| i64::from(w) * i64::from(h))
        .unwrap_or(-1)
}

/// The accepted contents of every searched repo, loaded once so a paged or
/// streamed caller can ask "is this copy protected?" without touching the
/// database per file. A member is accepted when its content is accepted in
/// **its own** repo — acceptance never crosses repos.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AcceptedIndex {
    by_repo: HashMap<String, HashSet<ContentKey>>,
}

impl AcceptedIndex {
    /// Load the accepted sets of `repo_names`.
    pub fn load(store: &Store, repo_names: &[String]) -> Result<Self, StoreError> {
        let mut by_repo = HashMap::with_capacity(repo_names.len());
        for name in repo_names {
            by_repo.insert(name.clone(), store.all_accepted(name)?);
        }
        Ok(Self { by_repo })
    }

    /// Whether `size`/`hash` is accepted in `repo`.
    pub fn contains(&self, repo: &str, size: u64, hash: &[u8; 32]) -> bool {
        self.by_repo
            .get(repo)
            .is_some_and(|set| set.contains(&(size, *hash)))
    }

    /// Whether this copy's content is accepted in its repo.
    pub fn is_accepted(&self, file: &DupeFile) -> bool {
        self.contains(&file.repo, file.entry.size, &file.entry.hash)
    }

    /// Whether every member of the group is accepted (nothing to triage).
    pub fn all_accepted(&self, group: &DupeGroup) -> bool {
        !group.is_empty() && group.iter().all(|f| self.is_accepted(f))
    }

    /// Whether any repo accepts this content (the cheap pre-check before a
    /// per-repo presence lookup).
    pub fn any_repo_accepts(&self, size: u64, hash: &[u8; 32]) -> bool {
        self.by_repo
            .values()
            .any(|set| set.contains(&(size, *hash)))
    }

    /// Record an accept in memory (mirror of [`Store::accept_content`]).
    pub fn insert(&mut self, repo: &str, size: u64, hash: &[u8; 32]) {
        self.by_repo
            .entry(repo.to_string())
            .or_default()
            .insert((size, *hash));
    }

    /// Drop an accept in memory (mirror of [`Store::unaccept_content`]).
    pub fn remove(&mut self, repo: &str, size: u64, hash: &[u8; 32]) {
        if let Some(set) = self.by_repo.get_mut(repo) {
            set.remove(&(size, *hash));
        }
    }
}

/// Whether an exact-duplicate group, known only by its content key, consists
/// of accepted copies alone: every searched repo that holds the content
/// accepts it. Cheap when nothing accepts the content; otherwise one index
/// lookup per repo.
pub fn key_all_accepted(
    store: &Store,
    repo_names: &[String],
    accepted: &AcceptedIndex,
    size: u64,
    hash: &[u8; 32],
) -> Result<bool, StoreError> {
    if !accepted.any_repo_accepts(size, hash) {
        return Ok(false);
    }
    for name in repo_names {
        if accepted.contains(name, size, hash) {
            continue;
        }
        let db = store.open_repo_db(name)?;
        if !store::get_paths_by_size_hash(&db, size, hash)?.is_empty() {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Delete all but the first (best) file of each group from disk and mark the
/// deleted entries missing — batched into one write transaction per repo.
/// Files already absent from disk are left untouched, matching the legacy
/// behavior. Best effort: failures are counted, not fatal.
///
/// Accepted copies are never deleted: they are promoted to the kept head of
/// their group first (see [`promote_protected_first`]), and any further
/// accepted members are skipped.
pub fn delete_duplicates(
    store: &Store,
    groups: &[DupeGroup],
) -> Result<DupeDeleteStats, StoreError> {
    let names: Vec<String> = {
        let mut seen: Vec<String> = Vec::new();
        for f in groups.iter().flatten() {
            if !seen.contains(&f.repo) {
                seen.push(f.repo.clone());
            }
        }
        seen
    };
    let accepted = AcceptedIndex::load(store, &names)?;
    let mut groups: Vec<DupeGroup> = groups.to_vec();
    promote_protected_first(&mut groups, |f| accepted.is_accepted(f));
    let extra: Vec<&DupeFile> = groups
        .iter()
        .flat_map(|g| g.iter().skip(1))
        .filter(|f| !accepted.is_accepted(f))
        .collect();
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

/// Delete files identified by `(repo_name, rel_path)` — the same behavior as
/// [`delete_files`] but without needing a materialized [`DupeFile`]. Lets a
/// paged UI delete its marked selection (which may span groups not currently
/// loaded) straight from the keys. Each repo's root is resolved once.
pub fn delete_paths(
    store: &Store,
    files: &[(String, String)],
) -> Result<DupeDeleteStats, StoreError> {
    let mut stats = DupeDeleteStats::default();
    let mut roots: HashMap<&str, String> = HashMap::new();
    let mut deleted_per_repo: HashMap<&str, Vec<&str>> = HashMap::new();

    for (repo, rel) in files {
        let root = match roots.get(repo.as_str()) {
            Some(root) => root.clone(),
            None => {
                let root = store.get_repo(repo)?.abs_path;
                roots.insert(repo.as_str(), root.clone());
                root
            }
        };
        let path = Path::new(&root).join(rel);
        if !path.exists() {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {
                stats.deleted += 1;
                deleted_per_repo
                    .entry(repo.as_str())
                    .or_default()
                    .push(rel.as_str());
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::ExifInfo;

    fn img_file(rel: &str, area: (u32, u32), exif: Option<ExifInfo>) -> DupeFile {
        DupeFile {
            repo: "r".into(),
            repo_root: "/x".into(),
            rel_path: rel.into(),
            entry: FileEntry {
                size: 100,
                hash: [7; 32],
                modified_ms: 0,
                missing: false,
                mime: Some("image/jpeg".into()),
                img_fingerprint: None,
                video_hash: None,
                pdf_hash: None,
                audio: None,
                img_size: Some(area),
                origin: None,
                exif,
            },
        }
    }

    /// Among pixel-equal copies, the one with EXIF (and the earlier capture
    /// date) ranks as the best copy — an original beats a metadata-stripped
    /// re-save.
    #[test]
    fn exif_original_beats_resave_when_pixel_equal() {
        let with_exif = img_file(
            "original.jpg",
            (4000, 3000),
            Some(ExifInfo {
                taken_ms: Some(1_000_000),
                camera: Some("Canon".into()),
            }),
        );
        let stripped = img_file("copy.jpg", (4000, 3000), None);

        let mut group = vec![stripped, with_exif];
        sort_group_members(&mut group);
        assert_eq!(group[0].rel_path, "original.jpg", "EXIF original is best");

        // Two dated copies: the earlier capture wins.
        let earlier = img_file(
            "a.jpg",
            (4000, 3000),
            Some(ExifInfo {
                taken_ms: Some(500),
                camera: None,
            }),
        );
        let later = img_file(
            "b.jpg",
            (4000, 3000),
            Some(ExifInfo {
                taken_ms: Some(9_000),
                camera: None,
            }),
        );
        let mut group = vec![later, earlier];
        sort_group_members(&mut group);
        assert_eq!(group[0].rel_path, "a.jpg", "earliest capture is best");
    }

    /// A protected (read-only) copy is promoted to best, so the writable
    /// duplicate becomes the deletable tail — even when the writable copy would
    /// otherwise win on content. Ties within a partition keep content order.
    #[test]
    fn protected_copy_is_promoted_to_best() {
        let file = |repo: &str, rel: &str| DupeFile {
            repo: repo.into(),
            ..img_file(rel, (4000, 3000), None)
        };
        // Content-best order would be writable-first (equal content → path).
        let mut groups = vec![vec![
            file("writable", "a_writable.jpg"),
            file("locked", "b_locked.jpg"),
        ]];
        let read_only = |f: &DupeFile| f.repo == "locked";
        promote_protected_first(&mut groups, read_only);
        assert_eq!(groups[0][0].repo, "locked", "protected copy is best");
        assert_eq!(groups[0][1].repo, "writable", "writable copy is deletable");

        // Two protected copies keep their content-best order between them.
        let mut groups = vec![vec![
            file("ro2", "z.jpg"),
            file("ro1", "a.jpg"),
            file("writable", "m.jpg"),
        ]];
        // Establish content order first (path asc among equal content).
        sort_groups(&mut groups);
        promote_protected_first(&mut groups, |f| f.repo.starts_with("ro"));
        let order: Vec<&str> = groups[0].iter().map(|f| f.repo.as_str()).collect();
        assert_eq!(
            order,
            ["ro1", "ro2", "writable"],
            "protected first, content-ordered"
        );
    }
}
