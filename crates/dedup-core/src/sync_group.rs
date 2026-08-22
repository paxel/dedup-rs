//! Pushing a **sync group** out: one main repository copied to each of its
//! remote sinks, each in its own mode.
//!
//! This is the native replacement for the manual "duplicate the repo, relocate
//! the copy, rescan it" backup dance. The heavy lifting is the existing
//! content sync ([`crate::diff::diff_sync`]) run once per sink — what this
//! module adds is the group's semantics:
//!
//! - [`SyncMode::AddOnly`] — copy content the sink lacks and never delete
//!   anything, so a sink can also hold things the main no longer does.
//! - [`SyncMode::ApplyChanges`] — additionally delete sink content the main
//!   itself deleted (its tombstones), so the sink follows the main's edits
//!   while keeping anything the main never had.
//! - [`SyncMode::Mirror`] — additionally delete sink content the main does not
//!   have, so the sink converges on exactly the main's content.
//!
//! Sinks are pushed in order and independently: one unreachable sink does not
//! stop the others, and every sink's outcome is reported.

use crate::diff::{
    DiffError, DiffItem, DiffRun, SourceView, SyncDelete, SyncPlan, SyncStats, diff_print,
    diff_sync_from, plan_sync_from,
};
use crate::filter::FileFilter;
use crate::store::{Store, SyncGroup, SyncMode};

/// One repository's content overlap with a reference repository, compared by
/// content (paths ignored). No current GUI caller (the many-repo overview it
/// powered was removed with the Repo Sync tab); kept as tested library API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoOverview {
    pub repo: String,
    /// Content only this repo has (the reference lacks it).
    pub unique: usize,
    /// Content both this repo and the reference have.
    pub shared: usize,
    /// Reference content this repo lacks.
    pub missing: usize,
}

/// Compare every repo in `repos` against `reference` by content; `reference`
/// itself is skipped. A repo with many `unique` files holds a lot the reference
/// lacks; many `missing` means it is behind the reference.
pub fn diff_overview(
    store: &Store,
    reference: &str,
    repos: &[String],
) -> Result<Vec<RepoOverview>, DiffError> {
    let mut out = Vec::new();
    for repo in repos {
        if repo == reference {
            continue;
        }
        // Diffing the repo against the reference classifies the repo's own files:
        // New = content the reference lacks (unique), Equal = shared.
        let (mut unique, mut shared) = (0usize, 0usize);
        for item in diff_print(store, repo, &[reference], None)? {
            match item {
                DiffItem::New { .. } => unique += 1,
                DiffItem::Equal { .. } => shared += 1,
                // The reference has this content but only as missing entries; not
                // a "shared" hit and not what `missing` (below) measures.
                DiffItem::DeletedInReference { .. } => {}
            }
        }
        // Reference content the repo lacks is the *reverse* diff's New count.
        let missing = diff_print(store, reference, &[repo.as_str()], None)?
            .iter()
            .filter(|i| matches!(i, DiffItem::New { .. }))
            .count();
        out.push(RepoOverview {
            repo: repo.clone(),
            unique,
            shared,
            missing,
        });
    }
    Ok(out)
}

/// The delete policy a group's mode implies for each sink.
pub fn delete_mode(mode: SyncMode) -> SyncDelete {
    match mode {
        SyncMode::AddOnly => SyncDelete::None,
        // Follow the main's edits: only content the main tombstoned goes.
        SyncMode::ApplyChanges => SyncDelete::Missing,
        // A content mirror: whatever the main does not have goes.
        SyncMode::Mirror => SyncDelete::Absent,
    }
}

/// Refuse the whole push when the main holds no indexed files *and any sink's
/// mode deletes* (MIRROR or APPLY CHANGES).
///
/// A mirror deletes sink content whose hash the main does not have, so an empty
/// main means "delete everything"; an APPLY CHANGES push from a main that lost
/// its files to a failed mount could likewise carry a flood of fresh
/// tombstones. That is almost never what the user wants: it happens when the
/// main was never scanned, or when its drive failed to mount and scanned as an
/// empty directory. A group whose sinks are all ADD ONLY is exempt.
///
/// This fails safe at the group level: a mixed group with even one deleting
/// sink refuses the *entire* push (including its add-only sinks) rather than
/// push some sinks from a main that looks broken. An empty main is a strong
/// "stop and look" signal, so the whole run waits until the main is scanned.
///
/// Public so a caller that plans/pushes a *subset* of a group's sinks directly
/// (bypassing [`plan_group_sync`]/[`run_group_sync`], e.g. to thread through a
/// filter those two don't accept) can still not lose this refusal.
pub fn guard_mirror_source(store: &Store, group: &SyncGroup) -> Result<(), DiffError> {
    if !group.sinks.iter().any(|s| s.mode.deletes_in_sink()) {
        return Ok(());
    }
    // `file_count` is maintained per live entry, so this is a META read rather
    // than a scan of the whole index.
    if store.get_repo_stats(&group.main)?.file_count == 0 {
        log::error!(
            "refusing to MIRROR from '{}': it has no indexed files, which would delete \
             everything in {} sink(s)",
            group.main,
            group.sinks.len()
        );
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
    // Every sink is planned against the same main, so its index is streamed once
    // here rather than re-read inside each `plan_sync`.
    let filter = FileFilter::All;
    let main = SourceView::collect(store, &group.main, &filter)?;
    let mut plans = Vec::with_capacity(group.sinks.len());
    for sink in &group.sinks {
        let plan = plan_sync_from(
            store,
            &main,
            &sink.repo,
            true,
            delete_mode(sink.mode),
            &filter,
        )?;
        plans.push((sink.repo.clone(), plan));
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
    log::info!("pushing '{}' to {} sink(s)", group.main, group.sinks.len(),);
    // The main is the same for every sink, so its index is streamed once here
    // instead of being re-read inside each `diff_sync`. Collected after the
    // guard, so an empty-main mirror is still refused before any work.
    let filter = FileFilter::All;
    let main = SourceView::collect(store, &group.main, &filter)?;
    let mut results = Vec::with_capacity(group.sinks.len());
    for sink in &group.sinks {
        let repo = sink.repo.as_str();
        // Sinks the cancel cut short are still reported, so the caller can say
        // which backups are now stale instead of silently dropping them.
        let outcome = if run.cancel.is_cancelled() {
            SinkOutcome::Skipped
        } else {
            match diff_sync_from(
                store,
                &main,
                repo,
                true,
                delete_mode(sink.mode),
                &filter,
                run,
            ) {
                Ok(stats) => {
                    log::info!(
                        "sink '{repo}': copied {}, deleted {}, {} error(s){}",
                        stats.copied,
                        stats.deleted,
                        stats.errors,
                        if stats.cancelled { ", cancelled" } else { "" }
                    );
                    SinkOutcome::Pushed(stats)
                }
                Err(e) => {
                    log::error!("sink '{repo}' failed: {e}");
                    SinkOutcome::Failed(e)
                }
            }
        };
        results.push((sink.repo.clone(), outcome));
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_only_never_deletes_and_mirror_does() {
        assert_eq!(delete_mode(SyncMode::AddOnly), SyncDelete::None);
        assert_eq!(delete_mode(SyncMode::ApplyChanges), SyncDelete::Missing);
        assert_eq!(delete_mode(SyncMode::Mirror), SyncDelete::Absent);
    }

    /// The empty-main guard covers every mode whose push can delete in the
    /// sink — MIRROR and APPLY CHANGES alike — and exempts pure ADD ONLY.
    #[test]
    fn deleting_modes_are_guarded_add_only_is_not() {
        assert!(!SyncMode::AddOnly.deletes_in_sink());
        assert!(SyncMode::ApplyChanges.deletes_in_sink());
        assert!(SyncMode::Mirror.deletes_in_sink());
    }
}
