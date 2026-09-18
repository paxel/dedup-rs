//! The `*_reporting` plan functions behind the Transfer tab's REVIEW: each
//! one walks its phases in order, reaches its totals, and stops on a
//! cancelled token with `DiffError::Cancelled`.

use dedup_core::diff::{
    DiffError, DiffPairing, FolderMode, PlanPhase, PlanProgress, SyncDelete,
    plan_folder_export_reporting, plan_repo_diff_reporting, plan_sync_back_reporting,
    plan_sync_reporting,
};
use dedup_core::store::Store;
use dedup_core::update::{CancellationToken, NoProgress, update_repo};
use std::path::Path;
use std::sync::Mutex;

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct Sandbox {
    _tempdir: tempfile::TempDir,
    store: Store,
}

impl Sandbox {
    /// Two scanned repos: LEFT holds three files, RIGHT shares one of them.
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let tempdir = tempfile::tempdir()?;
        let store = Store::open_at(tempdir.path().join("config"))?;
        let left = tempdir.path().join("left");
        let right = tempdir.path().join("right");
        write(&left, "a.txt", b"alpha")?;
        write(&left, "b.txt", b"beta")?;
        write(&left, "c.txt", b"gamma")?;
        write(&right, "a.txt", b"alpha")?;
        write(&right, "d.txt", b"delta")?;
        store.create_repo("LEFT", &left.to_string_lossy())?;
        store.create_repo("RIGHT", &right.to_string_lossy())?;
        for name in ["LEFT", "RIGHT"] {
            update_repo(&store, name, 1, &NoProgress, &CancellationToken::new())?;
        }
        Ok(Self {
            _tempdir: tempdir,
            store,
        })
    }
}

fn write(root: &Path, rel: &str, content: &[u8]) -> TestResult {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)?;
    Ok(())
}

fn reading(repo: &str) -> PlanPhase {
    PlanPhase::Reading {
        repo: repo.to_string(),
    }
}

/// The distinct phases in the order they were first reported.
fn phase_order(seen: &[PlanProgress]) -> Vec<PlanPhase> {
    let mut order: Vec<PlanPhase> = Vec::new();
    for p in seen {
        if order.last() != Some(&p.phase) {
            order.push(p.phase.clone());
        }
    }
    order
}

#[test]
fn repo_diff_reads_both_sides_then_pairs_and_honours_cancel() -> TestResult {
    let sb = Sandbox::new()?;
    for pairing in [DiffPairing::ByHash, DiffPairing::ByPath] {
        let seen = Mutex::new(Vec::new());
        let rows = plan_repo_diff_reporting(
            &sb.store,
            "LEFT",
            "RIGHT",
            pairing,
            &|p| seen.lock().map(|mut v| v.push(p)).unwrap_or(()),
            &CancellationToken::new(),
        )?;
        assert!(!rows.is_empty());
        let seen = seen.into_inner()?;
        assert_eq!(
            phase_order(&seen),
            vec![reading("LEFT"), reading("RIGHT"), PlanPhase::Pairing],
            "{pairing:?}: {seen:?}"
        );
    }

    let cancel = CancellationToken::new();
    cancel.cancel();
    let cancelled = plan_repo_diff_reporting(
        &sb.store,
        "LEFT",
        "RIGHT",
        DiffPairing::ByHash,
        &|_| {},
        &cancel,
    );
    assert!(
        matches!(cancelled, Err(DiffError::Cancelled)),
        "{cancelled:?}"
    );
    Ok(())
}

#[test]
fn sync_plan_reads_source_then_target_and_pairs_every_source_file() -> TestResult {
    let sb = Sandbox::new()?;
    let seen = Mutex::new(Vec::new());
    let plan = plan_sync_reporting(
        &sb.store,
        "LEFT",
        "RIGHT",
        true,
        SyncDelete::Absent,
        None,
        &|p| seen.lock().map(|mut v| v.push(p)).unwrap_or(()),
        &CancellationToken::new(),
    )?;
    assert_eq!(plan.copies.len(), 2, "b and c are new to RIGHT");
    assert_eq!(plan.deletes.len(), 1, "d is absent from LEFT");
    let seen = seen.into_inner()?;
    assert_eq!(
        phase_order(&seen),
        vec![
            reading("LEFT"),
            reading("RIGHT"),
            PlanPhase::Pairing,
            // An Absent (mirror) delete streams the target index once more.
            reading("RIGHT"),
        ],
        "{seen:?}"
    );
    let last_pairing = seen
        .iter()
        .rfind(|p| p.phase == PlanPhase::Pairing)
        .ok_or("pairing reported")?;
    assert_eq!(last_pairing.done, 3);
    assert_eq!(last_pairing.total, Some(3));

    let cancel = CancellationToken::new();
    cancel.cancel();
    let cancelled = plan_sync_reporting(
        &sb.store,
        "LEFT",
        "RIGHT",
        true,
        SyncDelete::None,
        None,
        &|_| {},
        &cancel,
    );
    assert!(
        matches!(cancelled, Err(DiffError::Cancelled)),
        "{cancelled:?}"
    );
    Ok(())
}

#[test]
fn sync_back_plan_reads_sink_then_main_and_pairs() -> TestResult {
    let sb = Sandbox::new()?;
    let seen = Mutex::new(Vec::new());
    let pull = plan_sync_back_reporting(
        &sb.store,
        "RIGHT",
        "LEFT",
        None,
        &|p| seen.lock().map(|mut v| v.push(p)).unwrap_or(()),
        &CancellationToken::new(),
    )?;
    assert_eq!(pull.len(), 1, "only d is new to LEFT");
    let seen = seen.into_inner()?;
    assert_eq!(
        phase_order(&seen),
        vec![reading("RIGHT"), reading("LEFT"), PlanPhase::Pairing],
        "{seen:?}"
    );
    let last = seen.last().ok_or("something reported")?;
    assert_eq!((last.done, last.total), (2, Some(2)));
    Ok(())
}

#[test]
fn folder_export_plan_reads_then_groups() -> TestResult {
    let sb = Sandbox::new()?;
    let seen = Mutex::new(Vec::new());
    let rels = plan_folder_export_reporting(
        &sb.store,
        "LEFT",
        &["RIGHT"],
        FolderMode::Exact,
        false,
        None,
        &|p| seen.lock().map(|mut v| v.push(p)).unwrap_or(()),
        &CancellationToken::new(),
    )?;
    assert_eq!(rels.len(), 2, "b and c are unique to LEFT");
    let seen = seen.into_inner()?;
    assert_eq!(
        phase_order(&seen),
        vec![reading("LEFT"), reading("RIGHT"), PlanPhase::Grouping],
        "{seen:?}"
    );

    let cancel = CancellationToken::new();
    cancel.cancel();
    let cancelled = plan_folder_export_reporting(
        &sb.store,
        "LEFT",
        &[],
        FolderMode::Exact,
        false,
        None,
        &|_| {},
        &cancel,
    );
    assert!(
        matches!(cancelled, Err(DiffError::Cancelled)),
        "{cancelled:?}"
    );
    Ok(())
}
