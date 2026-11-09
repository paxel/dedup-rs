//! COPY (ADR 0006): copy into one target repository every file of several
//! sources whose content the target and the reference repositories have
//! never held, each content once. Files land under an optional subfolder,
//! at their source path or at a path template's, and never overwrite: an
//! occupied name gets a numeric suffix.

use crate::diff::{
    DiffAction, DiffError, DiffEvent, DiffRun, collect_source_entries, entry_for_copied_file,
    open_repo, transfer_file,
};
use crate::filter::FileFilter;
use crate::organize::{normalize_rel, render_template};
use crate::store::{self, ContentKey};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// One file a COPY would make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyItem {
    /// The source repository and the file's path in it.
    pub source: String,
    pub rel: String,
    /// Where it lands in the target.
    pub target_rel: String,
    /// The planned name was taken, so a numeric suffix was added.
    pub suffixed: bool,
}

/// What COPY copies from where, and where to.
pub struct CopySpec<'a> {
    pub sources: &'a [&'a str],
    pub target: &'a str,
    /// Further repositories whose content counts as already there.
    pub references: &'a [&'a str],
    /// A folder inside the target the files land under.
    pub subdir: Option<&'a str>,
    /// A path template (as ORGANIZE uses) for each file's path under
    /// `subdir`; `None` keeps the source path.
    pub template: Option<&'a str>,
    pub filter: Option<&'a str>,
}

/// Counts of a finished COPY.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CopyIntoStats {
    pub copied: u64,
    /// Copies that landed under a suffixed name.
    pub suffixed: u64,
    pub cancelled: bool,
}

/// Plan a COPY without touching disk.
///
/// A content counts as known when the target or a reference holds it in any
/// state, missing included: a file deleted there on purpose is not brought
/// back. Sources are taken in order, and a content planned from one source is
/// not copied again from a later one.
pub fn plan_copy_into(
    store: &crate::store::Store,
    spec: &CopySpec<'_>,
) -> Result<Vec<CopyItem>, DiffError> {
    let filter = FileFilter::parse(spec.filter)?;
    let subdir = match spec.subdir.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => Some(normalize_rel(s).ok_or_else(|| DiffError::InvalidSubdir {
            subdir: s.to_string(),
        })?),
        None => None,
    };
    let target = open_repo(store, spec.target)?;
    let target_root = PathBuf::from(&target.meta.abs_path);
    let mut known: HashSet<ContentKey> =
        store::read_content_index(&target.db)?.into_keys().collect();
    for name in spec.references {
        let repo = open_repo(store, name)?;
        known.extend(store::read_content_index(&repo.db)?.into_keys());
    }
    let mut claimed: HashSet<String> = HashSet::new();
    let mut plan = Vec::new();
    for source in spec.sources {
        let repo = open_repo(store, source)?;
        for (rel, entry) in collect_source_entries(&repo.db, &filter, false)? {
            if !known.insert((entry.size, entry.hash)) {
                continue;
            }
            let path = match spec.template {
                Some(t) => normalize_rel(&render_template(t, &entry, &rel)).unwrap_or(rel.clone()),
                None => rel.clone(),
            };
            let path = match &subdir {
                Some(dir) => format!("{dir}/{path}"),
                None => path,
            };
            let (target_rel, suffixed) = free_name(&target_root, &path, &claimed);
            claimed.insert(target_rel.clone());
            plan.push(CopyItem {
                source: (*source).to_string(),
                rel,
                target_rel,
                suffixed,
            });
        }
    }
    Ok(plan)
}

/// Carry out a planned COPY: copy each file, never over an existing one (a
/// name taken since planning gets a suffix too), and index each copy in the
/// target.
pub fn run_copy_into(
    store: &crate::store::Store,
    target: &str,
    plan: &[CopyItem],
    run: &DiffRun<'_>,
) -> Result<CopyIntoStats, DiffError> {
    let target = open_repo(store, target)?;
    let target_root = PathBuf::from(&target.meta.abs_path);
    let mut stats = CopyIntoStats::default();
    let mut claimed: HashSet<String> = HashSet::new();
    let mut indexed = Vec::new();
    let total = plan.len() as u64;
    let mut result = Ok(());
    for item in plan {
        if run.cancel.is_cancelled() {
            stats.cancelled = true;
            break;
        }
        let source = open_repo(store, &item.source)?;
        let Some(entry) = store::get_entry(&source.db, &item.rel)? else {
            continue;
        };
        let from = Path::new(&source.meta.abs_path).join(&item.rel);
        let (target_rel, suffixed) = free_name(&target_root, &item.target_rel, &claimed);
        claimed.insert(target_rel.clone());
        let to = target_root.join(&target_rel);
        if let Err(err) = transfer_file(&from, &to, false) {
            run.progress.on(DiffEvent::Error {
                path: from.to_string_lossy().into_owned(),
                message: err.to_string(),
            });
            result = Err(err);
            break;
        }
        indexed.push((
            target_rel.clone(),
            entry_for_copied_file(&entry, &to, &item.source),
        ));
        stats.copied = stats.copied.saturating_add(1);
        if suffixed || item.suffixed {
            stats.suffixed = stats.suffixed.saturating_add(1);
        }
        run.progress.on(DiffEvent::Progress {
            action: DiffAction::Copy,
            done: stats.copied,
            total,
            rel_path: target_rel,
        });
    }
    // What landed on disk is in the index, even after a failure or a cancel.
    store::apply_entries(&target.db, indexed.iter().map(|(p, e)| (p.as_str(), e)))?;
    result.map(|()| stats)
}

/// `path` if nothing occupies it on disk or in `claimed`, else the first free
/// `name (n).ext` beside it. Returns the name and whether it was suffixed.
fn free_name(root: &Path, path: &str, claimed: &HashSet<String>) -> (String, bool) {
    let taken = |p: &str| claimed.contains(p) || root.join(p).exists();
    if !taken(path) {
        return (path.to_string(), false);
    }
    let (dir, file) = match path.rsplit_once('/') {
        Some((d, f)) => (format!("{d}/"), f),
        None => (String::new(), path),
    };
    let (stem, ext) = match file.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, format!(".{e}")),
        _ => (file, String::new()),
    };
    let mut n = 1u32;
    loop {
        let candidate = format!("{dir}{stem} ({n}){ext}");
        if !taken(&candidate) {
            return (candidate, true);
        }
        n = n.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_taken_name_gets_the_first_free_suffix() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("a")).expect("mkdir");
        std::fs::write(dir.path().join("a/x.jpg"), b"1").expect("write");
        let claimed = HashSet::from(["a/x (1).jpg".to_string()]);
        assert_eq!(
            free_name(dir.path(), "a/y.jpg", &claimed),
            ("a/y.jpg".into(), false)
        );
        assert_eq!(
            free_name(dir.path(), "a/x.jpg", &claimed),
            ("a/x (2).jpg".into(), true)
        );
        assert_eq!(
            free_name(dir.path(), "a", &HashSet::new()),
            ("a (1)".into(), true)
        );
    }
}
