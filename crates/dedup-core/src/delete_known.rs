//! DELETE (ADR 0006): delete from several repositories every file whose
//! content one of the reference repositories already holds. Content held
//! only by the repositories being deleted from stays — that is a duplicate
//! search's job.

use crate::diff::{
    DeleteStats, DiffError, DiffRun, collect_source_entries, diff_delete, open_repo, source_key,
};
use crate::filter::FileFilter;
use crate::store::{self, ContentKey, ContentState, Store};
use std::collections::{HashMap, HashSet};

/// One file a DELETE would remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteItem {
    pub source: String,
    pub rel: String,
    /// No reference holds the content any more: it was deleted there, and
    /// DELETE MISSING carries that deletion over.
    pub only_missing: bool,
}

/// Plan a DELETE without touching disk. A content counts when a reference
/// holds it; with `delete_missing` also when a reference only remembers it
/// as deleted (a missing entry). Accepted content is never a candidate.
pub fn plan_delete_known(
    store: &Store,
    sources: &[&str],
    references: &[&str],
    delete_missing: bool,
    filter: Option<&str>,
) -> Result<Vec<DeleteItem>, DiffError> {
    if references.is_empty() {
        return Err(DiffError::NoReference);
    }
    let filter = FileFilter::parse(filter)?;
    let mut known: HashMap<ContentKey, ContentState> = HashMap::new();
    for name in references {
        let repo = open_repo(store, name)?;
        for (key, state) in store::read_content_index(&repo.db)? {
            let slot = known.entry(key).or_default();
            slot.present |= state.present;
            slot.missing |= state.missing;
        }
    }
    let mut plan = Vec::new();
    for source in sources {
        let repo = open_repo(store, source)?;
        let accepted = store::accepted_of_db(&repo.db)?;
        for (rel, entry) in collect_source_entries(&repo.db, &filter, false)? {
            let key = (entry.size, entry.hash);
            let Some(state) = known.get(&key) else {
                continue;
            };
            if accepted.contains(&key) || !(state.present || (delete_missing && state.missing)) {
                continue;
            }
            plan.push(DeleteItem {
                source: (*source).to_string(),
                rel,
                only_missing: !state.present,
            });
        }
    }
    Ok(plan)
}

/// Carry out a planned DELETE: exactly the planned files, each source on its
/// own, its index marking them missing.
pub fn run_delete_known(
    store: &Store,
    plan: &[DeleteItem],
    references: &[&str],
    run: &DiffRun<'_>,
) -> Result<DeleteStats, DiffError> {
    let mut sources: Vec<&str> = Vec::new();
    for item in plan {
        if !sources.contains(&item.source.as_str()) {
            sources.push(&item.source);
        }
    }
    let mut total = DeleteStats::default();
    for source in sources {
        if run.cancel.is_cancelled() {
            total.cancelled = true;
            break;
        }
        let only: HashSet<String> = plan
            .iter()
            .filter(|i| i.source == source)
            .map(|i| source_key(&i.rel))
            .collect();
        let scoped = DiffRun::new(run.progress, run.cancel).with_selection(None, Some(&only));
        let stats = diff_delete(store, source, references, None, &scoped)?;
        total.deleted = total.deleted.saturating_add(stats.deleted);
        total.cancelled |= stats.cancelled;
    }
    Ok(total)
}
