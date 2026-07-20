//! Pushing a **sync group** out: one main repository copied to each of its
//! remote sinks, in the group's own mode.
//!
//! This is the native replacement for the manual "duplicate the repo, relocate
//! the copy, rescan it" backup dance. The heavy lifting is the existing
//! content sync ([`crate::diff::diff_sync`]) run once per sink — what this
//! module adds is the group's semantics:
//!
//! - [`SyncMode::AddOnly`] — copy content the sink lacks and never delete
//!   anything, so a sink can also hold things the main no longer does.
//! - [`SyncMode::Mirror`] — additionally delete sink content the main does not
//!   have, so the sink converges on exactly the main's content.
//!
//! Sinks are pushed in order and independently: one unreachable sink does not
//! stop the others, and every sink's outcome is reported.

use crate::diff::{DiffError, DiffRun, SyncDelete, SyncPlan, SyncStats, diff_sync, plan_sync};
use crate::store::{Store, SyncGroup, SyncMode};

/// The delete policy a group's mode implies for each sink.
fn delete_mode(mode: SyncMode) -> SyncDelete {
    match mode {
        SyncMode::AddOnly => SyncDelete::None,
        // A content mirror: whatever the main does not have goes.
        SyncMode::Mirror => SyncDelete::Absent,
    }
}

/// What a push would do to each sink, without touching disk. Sinks come back
/// in group order, each with its own plan.
pub fn plan_group_sync(
    store: &Store,
    group: &SyncGroup,
) -> Result<Vec<(String, SyncPlan)>, DiffError> {
    let mut plans = Vec::with_capacity(group.sinks.len());
    for sink in &group.sinks {
        let plan = plan_sync(
            store,
            &group.main,
            sink,
            true,
            delete_mode(group.mode),
            None,
        )?;
        plans.push((sink.clone(), plan));
    }
    Ok(plans)
}

/// Push the main to every sink. Each sink is synced independently: a sink that
/// fails outright is reported as an error against its own name and the rest
/// still run, because a group is a set of backups, not a transaction.
pub fn run_group_sync(
    store: &Store,
    group: &SyncGroup,
    run: &DiffRun<'_>,
) -> Vec<(String, Result<SyncStats, DiffError>)> {
    let mut results = Vec::with_capacity(group.sinks.len());
    for sink in &group.sinks {
        if run.cancel.is_cancelled() {
            break;
        }
        let outcome = diff_sync(
            store,
            &group.main,
            sink,
            true,
            delete_mode(group.mode),
            None,
            run,
        );
        results.push((sink.clone(), outcome));
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_only_never_deletes_and_mirror_does() {
        assert_eq!(delete_mode(SyncMode::AddOnly), SyncDelete::None);
        assert_eq!(delete_mode(SyncMode::Mirror), SyncDelete::Absent);
    }
}
