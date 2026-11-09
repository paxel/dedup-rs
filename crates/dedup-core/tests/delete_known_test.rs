//! DELETE (ADR 0006): several sources lose what the references hold; content
//! only among the sources stays; DELETE MISSING decides about content a
//! reference deleted.

use dedup_core::delete_known::{plan_delete_known, run_delete_known};
use dedup_core::diff::{DiffRun, NoDiffProgress};
use dedup_core::store::Store;
use dedup_core::update::{CancellationToken, NoProgress, update_repo};

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Sources `a` and `b`, reference `r`, holding `files`, all scanned.
fn setup(
    files: &[(&str, &str, &[u8])],
) -> Result<(tempfile::TempDir, Store), Box<dyn std::error::Error>> {
    let tmp = tempfile::tempdir()?;
    let store = Store::open_at(tmp.path().join("cfg"))?;
    for name in ["a", "b", "r"] {
        std::fs::create_dir_all(tmp.path().join(name))?;
        store.create_repo(name, &tmp.path().join(name).to_string_lossy())?;
    }
    for (repo, rel, bytes) in files {
        std::fs::write(tmp.path().join(repo).join(rel), bytes)?;
    }
    for name in ["a", "b", "r"] {
        update_repo(&store, name, 1, &NoProgress, &CancellationToken::new())?;
    }
    Ok((tmp, store))
}

fn scan(store: &Store, name: &str) -> TestResult {
    update_repo(store, name, 1, &NoProgress, &CancellationToken::new())?;
    Ok(())
}

#[test]
fn every_source_loses_what_the_reference_holds_and_shared_content_stays() -> TestResult {
    let (tmp, store) = setup(&[
        ("a", "known.jpg", b"known"),
        ("b", "known_too.jpg", b"known"),
        ("r", "orig.jpg", b"known"),
        ("a", "shared.jpg", b"only-in-sources"),
        ("b", "shared.jpg", b"only-in-sources"),
    ])?;
    let plan = plan_delete_known(&store, &["a", "b"], &["r"], false, None)?;
    assert_eq!(plan.len(), 2, "{plan:?}");
    assert!(
        tmp.path().join("a/known.jpg").exists(),
        "planning deletes nothing"
    );
    let cancel = CancellationToken::new();
    let stats = run_delete_known(
        &store,
        &plan,
        &["r"],
        &DiffRun::new(&NoDiffProgress, &cancel),
    )?;
    assert_eq!(stats.deleted, 2);
    assert!(!tmp.path().join("a/known.jpg").exists());
    assert!(!tmp.path().join("b/known_too.jpg").exists());
    assert!(
        tmp.path().join("a/shared.jpg").exists(),
        "content only among the sources stays"
    );
    assert!(tmp.path().join("b/shared.jpg").exists());
    assert!(
        tmp.path().join("r/orig.jpg").exists(),
        "the reference is untouched"
    );
    Ok(())
}

#[test]
fn delete_missing_decides_about_content_the_reference_deleted() -> TestResult {
    // `r` keeps another file, so its rescan is not refused as an emptied drive.
    let (tmp, store) = setup(&[
        ("a", "x.jpg", b"gone-there"),
        ("r", "x.jpg", b"gone-there"),
        ("r", "other.jpg", b"stays"),
    ])?;
    std::fs::remove_file(tmp.path().join("r/x.jpg"))?;
    scan(&store, "r")?;
    assert!(plan_delete_known(&store, &["a"], &["r"], false, None)?.is_empty());
    let plan = plan_delete_known(&store, &["a"], &["r"], true, None)?;
    assert_eq!(plan.len(), 1);
    assert!(plan[0].only_missing);
    Ok(())
}
