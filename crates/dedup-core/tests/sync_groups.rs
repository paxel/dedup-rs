//! Integration tests for sync groups: the registry storage, the membership
//! rules, and the guards that keep a group's members from disappearing under
//! it. A group is one **main** repo plus the remote **sinks** it is pushed to.

use dedup_core::store::{Store, StoreError, SyncGroup, SyncMode};
use std::path::PathBuf;

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// A group's sink repositories, in order (dropping their per-sink modes).
fn sink_repos(group: &SyncGroup) -> Vec<&str> {
    group.sinks.iter().map(|s| s.repo.as_str()).collect()
}

struct Sandbox {
    _tempdir: tempfile::TempDir,
    store: Store,
    config: PathBuf,
}

impl Sandbox {
    /// A store with three registered (empty) repos: MAIN, SINK1, SINK2.
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let tempdir = tempfile::tempdir()?;
        let config = tempdir.path().join("config");
        let store = Store::open_at(config.clone())?;
        for name in ["MAIN", "SINK1", "SINK2"] {
            let dir = tempdir.path().join(name);
            std::fs::create_dir_all(&dir)?;
            store.create_repo(name, &dir.to_string_lossy())?;
        }
        Ok(Self {
            _tempdir: tempdir,
            store,
            config,
        })
    }
}

#[test]
fn a_group_stores_its_main_and_sinks_with_per_sink_modes() -> TestResult {
    let sb = Sandbox::new()?;
    sb.store.create_sync_group("offsite", "MAIN")?;
    sb.store
        .add_sync_sink("offsite", "SINK1", SyncMode::AddOnly)?;
    sb.store
        .add_sync_sink("offsite", "SINK2", SyncMode::Mirror)?;

    let group = sb.store.get_sync_group("offsite")?;
    assert_eq!(group.main, "MAIN");
    assert_eq!(sink_repos(&group), ["SINK1", "SINK2"]);
    // Each sink keeps its own push mode.
    assert_eq!(group.sinks[0].mode, SyncMode::AddOnly);
    assert_eq!(group.sinks[1].mode, SyncMode::Mirror);
    assert_eq!(
        group.members().collect::<Vec<_>>(),
        ["MAIN", "SINK1", "SINK2"],
        "the main leads its sinks"
    );

    // A sink's mode can be switched without touching the others.
    sb.store
        .set_sink_mode("offsite", "SINK1", SyncMode::Mirror)?;
    let group = sb.store.get_sync_group("offsite")?;
    assert_eq!(group.sinks[0].mode, SyncMode::Mirror);
    assert_eq!(group.sinks[1].mode, SyncMode::Mirror);

    // Groups survive reopening the store (they live in the registry). The
    // registry file is locked while a store holds it, so close this one first.
    let Sandbox {
        _tempdir,
        store,
        config,
    } = sb;
    drop(store);
    let reopened = Store::open_at(config)?;
    let listed = reopened.list_sync_groups()?;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].0, "offsite");
    assert_eq!(sink_repos(&listed[0].1), ["SINK1", "SINK2"]);
    assert_eq!(
        listed[0].1.sinks[1].mode,
        SyncMode::Mirror,
        "the per-sink mode round-trips through the registry"
    );
    Ok(())
}

#[test]
fn sink_repo_names_lists_only_sinks() -> TestResult {
    let sb = Sandbox::new()?;
    sb.store.create_sync_group("offsite", "MAIN")?;
    sb.store
        .add_sync_sink("offsite", "SINK1", SyncMode::AddOnly)?;
    sb.store
        .add_sync_sink("offsite", "SINK2", SyncMode::Mirror)?;

    let sinks = sb.store.sink_repo_names()?;
    assert_eq!(sinks.len(), 2);
    assert!(sinks.contains("SINK1") && sinks.contains("SINK2"));
    assert!(!sinks.contains("MAIN"), "a main is not a sink");
    Ok(())
}

#[test]
fn main_repo_names_lists_only_mains() -> TestResult {
    let sb = Sandbox::new()?;
    sb.store.create_sync_group("offsite", "MAIN")?;
    sb.store
        .add_sync_sink("offsite", "SINK1", SyncMode::AddOnly)?;
    // A second group with no sinks yet: its main still counts as a main, which
    // is what the MAIN badge keys off.
    sb.store.create_sync_group("local", "SINK2")?;

    let mains = sb.store.main_repo_names()?;
    assert_eq!(mains.len(), 2);
    assert!(mains.contains("MAIN"));
    assert!(
        mains.contains("SINK2"),
        "a group with no sinks still has a main"
    );
    assert!(!mains.contains("SINK1"), "a sink is not a main");
    Ok(())
}

/// Promoting a sink swaps which repo the badge is drawn on: the old main
/// becomes an ordinary sink and drops out of the set.
#[test]
fn main_repo_names_follows_set_sync_main() -> TestResult {
    let sb = Sandbox::new()?;
    sb.store.create_sync_group("offsite", "MAIN")?;
    sb.store
        .add_sync_sink("offsite", "SINK1", SyncMode::AddOnly)?;

    sb.store.set_sync_main("offsite", "SINK1")?;

    let mains = sb.store.main_repo_names()?;
    assert!(mains.contains("SINK1"), "the promoted sink is now the main");
    assert!(!mains.contains("MAIN"), "the demoted main is now a sink");
    Ok(())
}

#[test]
fn a_repo_belongs_to_at_most_one_group() -> TestResult {
    let sb = Sandbox::new()?;
    sb.store.create_sync_group("offsite", "MAIN")?;
    sb.store
        .add_sync_sink("offsite", "SINK1", SyncMode::AddOnly)?;
    sb.store.create_sync_group("other", "SINK2")?;

    // Neither as a second group's main…
    let err = sb.store.create_sync_group("third", "SINK1").unwrap_err();
    assert!(
        matches!(err, StoreError::AlreadyGrouped { .. }),
        "got {err:?}"
    );
    // …nor as another group's sink.
    let err = sb
        .store
        .add_sync_sink("other", "SINK1", SyncMode::AddOnly)
        .unwrap_err();
    assert!(
        matches!(err, StoreError::AlreadyGrouped { .. }),
        "got {err:?}"
    );

    // Once it leaves, it is free again.
    sb.store.remove_sync_sink("offsite", "SINK1")?;
    assert!(sb.store.sync_group_of("SINK1")?.is_none());
    sb.store
        .add_sync_sink("other", "SINK1", SyncMode::AddOnly)?;
    assert_eq!(
        sb.store.sync_group_of("SINK1")?.map(|(name, _)| name),
        Some("other".to_string())
    );
    Ok(())
}

#[test]
fn promoting_a_sink_demotes_the_old_main() -> TestResult {
    let sb = Sandbox::new()?;
    sb.store.create_sync_group("offsite", "MAIN")?;
    sb.store
        .add_sync_sink("offsite", "SINK1", SyncMode::AddOnly)?;

    sb.store.set_sync_main("offsite", "SINK1")?;
    let group = sb.store.get_sync_group("offsite")?;
    assert_eq!(group.main, "SINK1");
    assert_eq!(
        sink_repos(&group),
        ["MAIN"],
        "the old main stays in the group as a sink"
    );
    assert_eq!(
        group.sinks[0].mode,
        SyncMode::AddOnly,
        "the demoted main defaults to ADD ONLY"
    );

    // The main is not a sink, so it cannot be removed as one.
    let err = sb.store.remove_sync_sink("offsite", "SINK1").unwrap_err();
    assert!(matches!(err, StoreError::NotInGroup { .. }), "got {err:?}");
    // Nor can a repo that was never in the group be promoted.
    let err = sb.store.set_sync_main("offsite", "SINK2").unwrap_err();
    assert!(matches!(err, StoreError::NotInGroup { .. }), "got {err:?}");
    Ok(())
}

#[test]
fn members_cannot_be_renamed_or_removed_out_from_under_their_group() -> TestResult {
    let sb = Sandbox::new()?;
    sb.store.create_sync_group("offsite", "MAIN")?;
    sb.store
        .add_sync_sink("offsite", "SINK1", SyncMode::AddOnly)?;

    for repo in ["MAIN", "SINK1"] {
        let err = sb.store.remove_repo(repo).unwrap_err();
        assert!(
            matches!(err, StoreError::InSyncGroup { .. }),
            "removing {repo}: got {err:?}"
        );
        let err = sb.store.rename_repo(repo, "NEWNAME").unwrap_err();
        assert!(
            matches!(err, StoreError::InSyncGroup { .. }),
            "renaming {repo}: got {err:?}"
        );
    }
    // A repo outside every group is untouched by the guard.
    sb.store.rename_repo("SINK2", "ELSEWHERE")?;
    sb.store.remove_repo("ELSEWHERE")?;

    // Taking a repo out of its group lifts the guard.
    sb.store.remove_sync_sink("offsite", "SINK1")?;
    sb.store.rename_repo("SINK1", "FREE")?;
    Ok(())
}

#[test]
fn deleting_a_group_frees_its_members() -> TestResult {
    let sb = Sandbox::new()?;
    sb.store.create_sync_group("offsite", "MAIN")?;
    sb.store
        .add_sync_sink("offsite", "SINK1", SyncMode::AddOnly)?;

    sb.store.delete_sync_group("offsite")?;
    assert!(sb.store.list_sync_groups()?.is_empty());
    assert!(sb.store.sync_group_of("MAIN")?.is_none());
    sb.store.rename_repo("MAIN", "PLAIN")?;

    let err = sb.store.delete_sync_group("offsite").unwrap_err();
    assert!(matches!(err, StoreError::GroupNotFound(_)), "got {err:?}");
    Ok(())
}

#[test]
fn duplicate_group_names_and_unknown_repos_are_rejected() -> TestResult {
    let sb = Sandbox::new()?;
    sb.store.create_sync_group("offsite", "MAIN")?;

    let err = sb.store.create_sync_group("offsite", "SINK1").unwrap_err();
    assert!(matches!(err, StoreError::GroupExists(_)), "got {err:?}");

    let err = sb
        .store
        .create_sync_group("ghost", "NOSUCHREPO")
        .unwrap_err();
    assert!(matches!(err, StoreError::NotFound(_)), "got {err:?}");

    let err = sb
        .store
        .add_sync_sink("offsite", "NOSUCHREPO", SyncMode::AddOnly)
        .unwrap_err();
    assert!(matches!(err, StoreError::NotFound(_)), "got {err:?}");
    Ok(())
}

/// A registry written before sync groups existed has no `sync_groups` table at
/// all. Opening it must keep working and simply report no groups — the
/// standing rule for every store-format change.
#[test]
fn a_pre_sync_group_registry_still_opens_and_has_no_groups() -> TestResult {
    let tempdir = tempfile::tempdir()?;
    let config = tempdir.path().join("config");
    std::fs::create_dir_all(&config)?;

    // Build a registry the old way: the `repos` table only.
    {
        let db = redb::Database::create(config.join("repos.redb"))?;
        let write = db.begin_write()?;
        {
            let _repos = write.open_table(redb::TableDefinition::<&str, &[u8]>::new("repos"))?;
        }
        write.commit()?;
    }

    let store = Store::open_at(config.clone())?;
    assert!(
        store.list_sync_groups()?.is_empty(),
        "an old registry reports no sync groups instead of failing"
    );
    assert!(store.sync_group_of("ANY")?.is_none());

    // And it can be upgraded in place: repos and groups both work afterwards.
    let dir = tempdir.path().join("data");
    std::fs::create_dir_all(&dir)?;
    store.create_repo("MAIN", &dir.to_string_lossy())?;
    store.create_sync_group("offsite", "MAIN")?;
    assert_eq!(store.get_sync_group("offsite")?.main, "MAIN");
    Ok(())
}

// --- pushing a group out ---------------------------------------------------

use dedup_core::diff::{DiffRun, NoDiffProgress};
use dedup_core::sync_group::{SinkOutcome, diff_overview, plan_group_sync, run_group_sync};
use dedup_core::update::{CancellationToken, NoProgress, update_repo};
use std::path::Path;

/// Write a file into one of the sandbox's repo directories.
fn write(root: &Path, rel: &str, content: &[u8]) -> TestResult {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)?;
    Ok(())
}

/// The stats of the first sink's push, if it actually ran.
fn first_stats(results: Vec<(String, SinkOutcome)>) -> Option<dedup_core::diff::SyncStats> {
    match results.into_iter().next() {
        Some((_, SinkOutcome::Pushed(stats))) => Some(stats),
        _ => None,
    }
}

/// A repo's live (non-missing) relative paths, sorted.
fn live_paths(store: &Store, repo: &str) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let mut paths = Vec::new();
    let db = store.open_repo_db(repo)?;
    dedup_core::store::for_each_file_entry(&db, |rel, entry| {
        if !entry.missing {
            paths.push(rel.to_string());
        }
        Ok(())
    })?;
    paths.sort();
    Ok(paths)
}

impl Sandbox {
    fn dir(&self, repo: &str) -> PathBuf {
        self._tempdir.path().join(repo)
    }

    fn scan(&self, repos: &[&str]) -> TestResult {
        for repo in repos {
            update_repo(&self.store, repo, 1, &NoProgress, &CancellationToken::new())?;
        }
        Ok(())
    }
}

/// A group with MAIN pushed to SINK1, in the given mode, with the main holding
/// `a.txt`/`b.txt` and the sink holding `a.txt` plus its own `extra.txt`.
fn seeded_group(mode: SyncMode) -> Result<Sandbox, Box<dyn std::error::Error>> {
    let sb = Sandbox::new()?;
    write(&sb.dir("MAIN"), "a.txt", b"alpha")?;
    write(&sb.dir("MAIN"), "b.txt", b"beta")?;
    write(&sb.dir("SINK1"), "a.txt", b"alpha")?;
    write(&sb.dir("SINK1"), "extra.txt", b"only in the sink")?;
    sb.scan(&["MAIN", "SINK1"])?;
    sb.store.create_sync_group("offsite", "MAIN")?;
    sb.store.add_sync_sink("offsite", "SINK1", mode)?;
    Ok(sb)
}

#[test]
fn add_only_copies_what_the_sink_lacks_and_deletes_nothing() -> TestResult {
    let sb = seeded_group(SyncMode::AddOnly)?;
    let group = sb.store.get_sync_group("offsite")?;

    let plans = plan_group_sync(&sb.store, &group)?;
    assert_eq!(plans.len(), 1, "one plan per sink");
    assert_eq!(plans[0].0, "SINK1");
    assert_eq!(plans[0].1.copies, ["b.txt"], "only the missing content");
    assert!(plans[0].1.deletes.is_empty(), "AddOnly never deletes");

    let results = run_group_sync(
        &sb.store,
        &group,
        &DiffRun::new(&NoDiffProgress, &CancellationToken::new()),
    )?;
    let stats = first_stats(results).ok_or("sync failed")?;
    assert_eq!((stats.copied, stats.deleted), (1, 0));
    assert_eq!(
        live_paths(&sb.store, "SINK1")?,
        ["a.txt", "b.txt", "extra.txt"],
        "the sink keeps what only it has"
    );
    assert_eq!(std::fs::read(sb.dir("SINK1").join("b.txt"))?, b"beta");
    Ok(())
}

#[test]
fn mirror_converges_the_sink_on_the_main() -> TestResult {
    let sb = seeded_group(SyncMode::Mirror)?;
    let group = sb.store.get_sync_group("offsite")?;

    let plans = plan_group_sync(&sb.store, &group)?;
    assert_eq!(plans[0].1.copies, ["b.txt"]);
    assert_eq!(
        plans[0].1.deletes,
        ["extra.txt"],
        "Mirror also drops what the main does not have"
    );

    let results = run_group_sync(
        &sb.store,
        &group,
        &DiffRun::new(&NoDiffProgress, &CancellationToken::new()),
    )?;
    let stats = first_stats(results).ok_or("sync failed")?;
    assert_eq!((stats.copied, stats.deleted), (1, 1));
    assert_eq!(
        live_paths(&sb.store, "SINK1")?,
        ["a.txt", "b.txt"],
        "the sink now holds exactly the main's content"
    );
    assert!(!sb.dir("SINK1").join("extra.txt").exists());
    // The main is never changed by a push.
    assert_eq!(live_paths(&sb.store, "MAIN")?, ["a.txt", "b.txt"]);
    Ok(())
}

/// The point of per-sink modes: one group, mixed. The MIRROR sink drops what
/// the main lacks; the ADD ONLY sink keeps its own extra file in the same push.
#[test]
fn a_group_can_mirror_one_sink_and_only_add_to_another() -> TestResult {
    let sb = Sandbox::new()?;
    write(&sb.dir("MAIN"), "a.txt", b"alpha")?;
    write(&sb.dir("SINK1"), "a.txt", b"alpha")?;
    write(&sb.dir("SINK1"), "extra1.txt", b"only in sink1")?;
    write(&sb.dir("SINK2"), "a.txt", b"alpha")?;
    write(&sb.dir("SINK2"), "extra2.txt", b"only in sink2")?;
    sb.scan(&["MAIN", "SINK1", "SINK2"])?;
    sb.store.create_sync_group("offsite", "MAIN")?;
    sb.store
        .add_sync_sink("offsite", "SINK1", SyncMode::Mirror)?;
    sb.store
        .add_sync_sink("offsite", "SINK2", SyncMode::AddOnly)?;
    let group = sb.store.get_sync_group("offsite")?;

    let plans = plan_group_sync(&sb.store, &group)?;
    assert_eq!(plans.len(), 2);
    // The MIRROR sink drops the content the main does not have…
    assert_eq!(plans[0].0, "SINK1");
    assert_eq!(
        plans[0].1.deletes,
        ["extra1.txt"],
        "the mirror sink drops what the main lacks"
    );
    // …while the ADD ONLY sink keeps its own, in the very same push.
    assert_eq!(plans[1].0, "SINK2");
    assert!(
        plans[1].1.deletes.is_empty(),
        "the add-only sink keeps its extra"
    );
    Ok(())
}

#[test]
fn every_sink_is_pushed_independently() -> TestResult {
    let sb = Sandbox::new()?;
    write(&sb.dir("MAIN"), "a.txt", b"alpha")?;
    sb.scan(&["MAIN", "SINK1", "SINK2"])?;
    sb.store.create_sync_group("offsite", "MAIN")?;
    sb.store
        .add_sync_sink("offsite", "SINK1", SyncMode::AddOnly)?;
    sb.store
        .add_sync_sink("offsite", "SINK2", SyncMode::AddOnly)?;
    let group = sb.store.get_sync_group("offsite")?;

    let results = run_group_sync(
        &sb.store,
        &group,
        &DiffRun::new(&NoDiffProgress, &CancellationToken::new()),
    )?;
    assert_eq!(
        results
            .iter()
            .map(|(sink, _)| sink.as_str())
            .collect::<Vec<_>>(),
        ["SINK1", "SINK2"],
        "one result per sink, in group order"
    );
    for (sink, outcome) in results {
        let SinkOutcome::Pushed(stats) = outcome else {
            return Err(format!("{sink} was not pushed").into());
        };
        assert_eq!(stats.copied, 1, "{sink} got the file");
        assert_eq!(live_paths(&sb.store, &sink)?, ["a.txt"]);
    }
    Ok(())
}

#[test]
fn a_second_push_has_nothing_left_to_do() -> TestResult {
    let sb = seeded_group(SyncMode::Mirror)?;
    let group = sb.store.get_sync_group("offsite")?;
    let cancel = CancellationToken::new();
    run_group_sync(&sb.store, &group, &DiffRun::new(&NoDiffProgress, &cancel))?;

    // Re-planning right after a push: the sink already matches the main, so a
    // push is idempotent (no re-copying, no re-deleting).
    let plans = plan_group_sync(&sb.store, &group)?;
    assert!(plans[0].1.copies.is_empty(), "nothing left to copy");
    assert!(plans[0].1.deletes.is_empty(), "nothing left to delete");
    Ok(())
}

// ---------------------------------------------------------------------------
// The empty-main guard: a MIRROR push must never read "delete everything"
// ---------------------------------------------------------------------------

/// A main that was never scanned holds no content, so mirroring from it would
/// classify every sink file as "absent from the main" and delete the lot.
#[test]
fn mirror_refuses_a_main_that_was_never_scanned() -> TestResult {
    let sb = Sandbox::new()?;
    write(&sb.dir("MAIN"), "a.txt", b"alpha")?;
    write(&sb.dir("SINK1"), "precious.txt", b"the only copy")?;
    // Only the sink is scanned: the main's index stays empty.
    sb.scan(&["SINK1"])?;
    sb.store.create_sync_group("offsite", "MAIN")?;
    sb.store
        .add_sync_sink("offsite", "SINK1", SyncMode::Mirror)?;
    let group = sb.store.get_sync_group("offsite")?;

    assert!(
        matches!(
            plan_group_sync(&sb.store, &group),
            Err(dedup_core::diff::DiffError::EmptyMirrorSource { .. })
        ),
        "planning is refused before it can propose deletions"
    );
    assert!(
        matches!(
            run_group_sync(
                &sb.store,
                &group,
                &DiffRun::new(&NoDiffProgress, &CancellationToken::new()),
            ),
            Err(dedup_core::diff::DiffError::EmptyMirrorSource { .. })
        ),
        "and so is the push"
    );
    assert_eq!(
        live_paths(&sb.store, "SINK1")?,
        ["precious.txt"],
        "the sink still holds its content"
    );
    assert!(sb.dir("SINK1").join("precious.txt").exists());
    Ok(())
}

/// ADD ONLY never deletes, so an empty main is harmless there and must not be
/// refused — it simply has nothing to copy.
#[test]
fn add_only_still_runs_with_an_empty_main() -> TestResult {
    let sb = Sandbox::new()?;
    write(&sb.dir("SINK1"), "precious.txt", b"the only copy")?;
    sb.scan(&["SINK1"])?;
    sb.store.create_sync_group("offsite", "MAIN")?;
    sb.store
        .add_sync_sink("offsite", "SINK1", SyncMode::AddOnly)?;
    let group = sb.store.get_sync_group("offsite")?;

    let plans = plan_group_sync(&sb.store, &group)?;
    assert!(plans[0].1.copies.is_empty(), "an empty main copies nothing");
    assert!(plans[0].1.deletes.is_empty(), "and AddOnly never deletes");
    assert_eq!(live_paths(&sb.store, "SINK1")?, ["precious.txt"]);
    Ok(())
}

/// The dangerous variant of an empty main: the drive failed to mount, so the
/// root is still a directory (passing the `RootMissing` check) but holds
/// nothing. The scan flags it, and — the part that actually protects the sink —
/// MIRROR then refuses to push from the emptied main.
#[test]
fn an_empty_walk_is_flagged_and_disarms_mirror() -> TestResult {
    let sb = seeded_group(SyncMode::Mirror)?;
    assert_eq!(live_paths(&sb.store, "MAIN")?, ["a.txt", "b.txt"]);

    // Simulate the unmounted mountpoint: the directory is there, empty.
    std::fs::remove_file(sb.dir("MAIN").join("a.txt"))?;
    std::fs::remove_file(sb.dir("MAIN").join("b.txt"))?;

    // Unauthorised, this is refused outright and the index is left intact —
    // an unmounted drive must not be able to empty it.
    assert!(
        matches!(
            update_repo(&sb.store, "MAIN", 1, &NoProgress, &CancellationToken::new()),
            Err(dedup_core::update::UpdateError::WouldEmptyIndex { entries: 2, .. })
        ),
        "a walk that would empty the index is refused without authorisation"
    );
    assert_eq!(
        live_paths(&sb.store, "MAIN")?,
        ["a.txt", "b.txt"],
        "the refusal leaves every entry exactly as it was"
    );

    // Authorised (the GUI confirmation / CLI --force), it proceeds as before.
    let stats = dedup_core::update::update_repo_authorized(
        &sb.store,
        "MAIN",
        1,
        &NoProgress,
        &CancellationToken::new(),
        true,
    )?;

    assert!(stats.empty_walk, "the scan flags a walk that found nothing");
    assert_eq!(stats.marked_missing, 2);

    // The main now looks empty — which is exactly when a mirror must not run.
    let group = sb.store.get_sync_group("offsite")?;
    assert!(
        matches!(
            plan_group_sync(&sb.store, &group),
            Err(dedup_core::diff::DiffError::EmptyMirrorSource { .. })
        ),
        "MIRROR refuses an emptied main instead of wiping the sink"
    );
    assert_eq!(
        live_paths(&sb.store, "SINK1")?,
        ["a.txt", "extra.txt"],
        "the sink is untouched"
    );
    Ok(())
}

/// A repo that lost only some of its files is an ordinary scan: nothing is
/// flagged, and the vanished entry is marked missing as always.
#[test]
fn a_partial_deletion_is_not_flagged() -> TestResult {
    let sb = seeded_group(SyncMode::Mirror)?;
    std::fs::remove_file(sb.dir("MAIN").join("b.txt"))?;
    let stats = update_repo(&sb.store, "MAIN", 1, &NoProgress, &CancellationToken::new())?;

    assert_eq!(stats.marked_missing, 1);
    assert!(!stats.empty_walk, "some files were still found");
    assert_eq!(live_paths(&sb.store, "MAIN")?, ["a.txt"]);
    Ok(())
}

/// Cancelling mid-push must not look like a completed one: the sinks that were
/// never reached are reported as skipped, so the caller can say which backups
/// are now stale instead of silently dropping them from the results.
#[test]
fn a_cancelled_push_reports_the_sinks_it_never_reached() -> TestResult {
    let sb = Sandbox::new()?;
    write(&sb.dir("MAIN"), "a.txt", b"alpha")?;
    sb.scan(&["MAIN", "SINK1", "SINK2"])?;
    sb.store.create_sync_group("offsite", "MAIN")?;
    sb.store
        .add_sync_sink("offsite", "SINK1", SyncMode::AddOnly)?;
    sb.store
        .add_sync_sink("offsite", "SINK2", SyncMode::AddOnly)?;
    let group = sb.store.get_sync_group("offsite")?;

    // Cancelled before anything ran: both sinks are still accounted for.
    let cancel = CancellationToken::new();
    cancel.cancel();
    let results = run_group_sync(&sb.store, &group, &DiffRun::new(&NoDiffProgress, &cancel))?;

    assert_eq!(results.len(), 2, "every sink is still reported");
    assert!(
        results
            .iter()
            .all(|(_, o)| matches!(o, SinkOutcome::Skipped)),
        "a cancelled run marks untouched sinks skipped, not done"
    );
    assert_eq!(
        results
            .iter()
            .map(|(sink, _)| sink.as_str())
            .collect::<Vec<_>>(),
        ["SINK1", "SINK2"],
        "named, so the caller can list the stale backups"
    );
    assert!(
        live_paths(&sb.store, "SINK1")?.is_empty(),
        "and nothing was actually pushed"
    );
    Ok(())
}

/// `diff_overview`: every repo diffed against one reference by content.
/// `unique` = content only that repo has, `shared` = in both, `missing` =
/// reference content that repo lacks. The reference is skipped.
#[test]
fn diff_overview_counts_unique_shared_and_missing_per_repo() -> TestResult {
    let sb = Sandbox::new()?;
    // MAIN (the reference) holds a shared file and one only it has.
    write(&sb.dir("MAIN"), "shared.txt", b"both have this")?;
    write(&sb.dir("MAIN"), "ref_only.txt", b"only the reference")?;
    // SINK1 shares one, has one of its own, and lacks the reference's other.
    write(&sb.dir("SINK1"), "shared.txt", b"both have this")?;
    write(&sb.dir("SINK1"), "sink_only.txt", b"only the sink")?;
    sb.scan(&["MAIN", "SINK1", "SINK2"])?;

    let repos = vec!["MAIN".to_string(), "SINK1".to_string(), "SINK2".to_string()];
    let overview = diff_overview(&sb.store, "MAIN", &repos)?;

    // The reference itself is not in the overview.
    assert!(
        !overview.iter().any(|o| o.repo == "MAIN"),
        "the reference is skipped"
    );
    let sink1 = overview
        .iter()
        .find(|o| o.repo == "SINK1")
        .ok_or("SINK1 missing")?;
    assert_eq!(sink1.unique, 1, "sink_only.txt is content MAIN lacks");
    assert_eq!(sink1.shared, 1, "shared.txt is in both");
    assert_eq!(sink1.missing, 1, "ref_only.txt is MAIN content SINK1 lacks");

    // An empty repo shares nothing and is missing everything the reference has.
    let sink2 = overview
        .iter()
        .find(|o| o.repo == "SINK2")
        .ok_or("SINK2 missing")?;
    assert_eq!((sink2.unique, sink2.shared, sink2.missing), (0, 0, 2));
    Ok(())
}

// --- one main, several sinks ------------------------------------------------

#[test]
fn a_push_to_several_sinks_plans_and_runs_each_independently() -> TestResult {
    // The main's index is collected once and shared across every sink's plan and
    // run. This pins the behaviour that sharing must not change: each sink is
    // still planned against its own contents and its own mode.
    let sb = Sandbox::new()?;
    write(&sb.dir("MAIN"), "a.txt", b"alpha")?;
    write(&sb.dir("MAIN"), "b.txt", b"beta")?;
    // SINK1 already has one of the two and an extra of its own.
    write(&sb.dir("SINK1"), "a.txt", b"alpha")?;
    write(&sb.dir("SINK1"), "extra.txt", b"only in sink1")?;
    // SINK2 is empty, so it needs both.
    sb.scan(&["MAIN", "SINK1", "SINK2"])?;
    sb.store.create_sync_group("offsite", "MAIN")?;
    sb.store
        .add_sync_sink("offsite", "SINK1", SyncMode::AddOnly)?;
    sb.store
        .add_sync_sink("offsite", "SINK2", SyncMode::Mirror)?;
    let group = sb.store.get_sync_group("offsite")?;

    let plans = plan_group_sync(&sb.store, &group)?;
    assert_eq!(plans.len(), 2, "one plan per sink, in group order");
    assert_eq!(plans[0].0, "SINK1");
    assert_eq!(plans[0].1.copies, ["b.txt"], "SINK1 only lacks b.txt");
    assert!(
        plans[0].1.deletes.is_empty(),
        "AddOnly leaves extra.txt alone"
    );
    assert_eq!(plans[1].0, "SINK2");
    let mut sink2_copies = plans[1].1.copies.clone();
    sink2_copies.sort();
    assert_eq!(sink2_copies, ["a.txt", "b.txt"], "SINK2 is empty");

    let results = run_group_sync(
        &sb.store,
        &group,
        &DiffRun::new(&NoDiffProgress, &CancellationToken::new()),
    )?;
    assert_eq!(results.len(), 2);
    for (repo, outcome) in &results {
        assert!(
            matches!(outcome, SinkOutcome::Pushed(_)),
            "sink '{repo}' should have been pushed"
        );
    }

    // Both sinks now hold the main's content; only the AddOnly sink kept its own.
    assert_eq!(
        live_paths(&sb.store, "SINK1")?,
        ["a.txt", "b.txt", "extra.txt"]
    );
    assert_eq!(live_paths(&sb.store, "SINK2")?, ["a.txt", "b.txt"]);
    Ok(())
}

#[test]
fn a_shared_main_view_still_refuses_an_empty_mirror() -> TestResult {
    // The guard runs before the main is collected, so the refusal survives the
    // shared-source path exactly as before.
    let sb = Sandbox::new()?;
    write(&sb.dir("SINK1"), "keep.txt", b"precious")?;
    sb.scan(&["MAIN", "SINK1", "SINK2"])?;
    sb.store.create_sync_group("offsite", "MAIN")?;
    sb.store
        .add_sync_sink("offsite", "SINK1", SyncMode::Mirror)?;
    sb.store
        .add_sync_sink("offsite", "SINK2", SyncMode::Mirror)?;
    let group = sb.store.get_sync_group("offsite")?;

    assert!(
        plan_group_sync(&sb.store, &group).is_err(),
        "planning a mirror from an empty main is refused"
    );
    assert!(
        run_group_sync(
            &sb.store,
            &group,
            &DiffRun::new(&NoDiffProgress, &CancellationToken::new()),
        )
        .is_err(),
        "so is running it"
    );
    assert_eq!(
        live_paths(&sb.store, "SINK1")?,
        ["keep.txt"],
        "nothing was deleted"
    );
    Ok(())
}

#[test]
fn a_cancelled_multi_sink_push_still_reports_every_sink() -> TestResult {
    // Cancelling must not drop sinks from the report, so the caller can say which
    // backups are now stale — unchanged by sharing the main's view.
    let sb = Sandbox::new()?;
    write(&sb.dir("MAIN"), "a.txt", b"alpha")?;
    sb.scan(&["MAIN", "SINK1", "SINK2"])?;
    sb.store.create_sync_group("offsite", "MAIN")?;
    sb.store
        .add_sync_sink("offsite", "SINK1", SyncMode::AddOnly)?;
    sb.store
        .add_sync_sink("offsite", "SINK2", SyncMode::AddOnly)?;
    let group = sb.store.get_sync_group("offsite")?;

    let cancel = CancellationToken::new();
    cancel.cancel();
    let results = run_group_sync(&sb.store, &group, &DiffRun::new(&NoDiffProgress, &cancel))?;

    assert_eq!(results.len(), 2, "every sink is still reported");
    for (repo, outcome) in &results {
        assert!(
            matches!(outcome, SinkOutcome::Skipped),
            "sink '{repo}' should be reported as skipped"
        );
    }
    Ok(())
}
