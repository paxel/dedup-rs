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

/// Refuse a MIRROR push whose main holds no indexed files.
///
/// A mirror deletes sink content whose hash the main does not have, so an empty
/// main means "delete everything". That is almost never what the user wants: it
/// happens when the main was never scanned, or when its drive failed to mount
/// and scanned as an empty directory. `AddOnly` never deletes, so it is exempt.
fn guard_mirror_source(store: &Store, group: &SyncGroup) -> Result<(), DiffError> {
    if group.mode != SyncMode::Mirror {
        return Ok(());
    }
    // `file_count` is maintained per live entry, so this is a META read rather
    // than a scan of the whole index.
    if store.get_repo_stats(&group.main)?.file_count == 0 {
        return Err(DiffError::EmptyMirrorSource {
            main: group.main.clone(),
        });
    }
    Ok(())
}

/// What a push would do to each sink, without touching disk. Sinks come back
/// in group order, each with its own plan.
pub fn plan_group_sync(
    store: &Store,
    group: &SyncGroup,
) -> Result<Vec<(String, SyncPlan)>, DiffError> {
    guard_mirror_source(store, group)?;
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

/// A sink's outcome: the stats of its push, or why it did not happen.
pub enum SinkOutcome {
    Pushed(SyncStats),
    Failed(DiffError),
    /// The run was cancelled before this sink was reached.
    Skipped,
}

/// Push the main to every sink. Each sink is synced independently: a sink that
/// fails outright is reported as [`SinkOutcome::Failed`] against its own name
/// and the rest still run, because a group is a set of backups, not a
/// transaction. Cancelling reports the untouched sinks as
/// [`SinkOutcome::Skipped`] rather than dropping them, so the caller can say
/// which backups are now stale. The whole push is refused up front if it would
/// mirror from an empty main.
pub fn run_group_sync(
    store: &Store,
    group: &SyncGroup,
    run: &DiffRun<'_>,
) -> Result<Vec<(String, SinkOutcome)>, DiffError> {
    // A group-level refusal is not a per-sink failure: nothing is attempted.
    guard_mirror_source(store, group)?;
    let mut results = Vec::with_capacity(group.sinks.len());
    for sink in &group.sinks {
        // Sinks the cancel cut short are still reported, so the caller can say
        // which backups are now stale instead of silently dropping them.
        let outcome = if run.cancel.is_cancelled() {
            SinkOutcome::Skipped
        } else {
            match diff_sync(
                store,
                &group.main,
                sink,
                true,
                delete_mode(group.mode),
                None,
                run,
            ) {
                Ok(stats) => SinkOutcome::Pushed(stats),
                Err(e) => SinkOutcome::Failed(e),
            }
        };
        results.push((sink.clone(), outcome));
    }
    Ok(results)
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
