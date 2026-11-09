//! COPY (ADR 0006): several sources into one target, against references,
//! each content once, under a subfolder and a template, never overwriting.

use dedup_core::copy_into::{CopySpec, plan_copy_into, run_copy_into};
use dedup_core::diff::{DiffRun, NoDiffProgress};
use dedup_core::store::Store;
use dedup_core::update::{CancellationToken, NoProgress, update_repo};
use std::path::{Path, PathBuf};

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Repositories `a`, `b` (sources), `t` (target) and `r` (reference), each
/// holding `files` and scanned.
fn setup(
    files: &[(&str, &str, &[u8])],
) -> Result<(tempfile::TempDir, Store), Box<dyn std::error::Error>> {
    let tmp = tempfile::tempdir()?;
    let store = Store::open_at(tmp.path().join("cfg"))?;
    for name in ["a", "b", "t", "r"] {
        std::fs::create_dir_all(tmp.path().join(name))?;
        store.create_repo(name, &tmp.path().join(name).to_string_lossy())?;
    }
    for (repo, rel, bytes) in files {
        let path = tmp.path().join(repo).join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, bytes)?;
    }
    for name in ["a", "b", "t", "r"] {
        update_repo(&store, name, 1, &NoProgress, &CancellationToken::new())?;
    }
    Ok((tmp, store))
}

fn spec<'a>(
    sources: &'a [&'a str],
    subdir: Option<&'a str>,
    template: Option<&'a str>,
) -> CopySpec<'a> {
    CopySpec {
        sources,
        target: "t",
        references: &["r"],
        subdir,
        template,
        filter: None,
    }
}

fn run(store: &Store, plan: &[dedup_core::copy_into::CopyItem]) -> TestResult {
    let cancel = CancellationToken::new();
    run_copy_into(store, "t", plan, &DiffRun::new(&NoDiffProgress, &cancel))?;
    Ok(())
}

fn t(tmp: &Path, rel: &str) -> PathBuf {
    tmp.join("t").join(rel)
}

#[test]
fn content_in_two_sources_is_copied_once_and_known_content_not_at_all() -> TestResult {
    let (tmp, store) = setup(&[
        ("a", "x.jpg", b"same"),
        ("b", "y.jpg", b"same"),
        ("a", "old.jpg", b"kept"),
        ("r", "old.jpg", b"kept"),
        ("b", "new.jpg", b"fresh"),
    ])?;
    let plan = plan_copy_into(&store, &spec(&["a", "b"], None, None))?;
    let rels: Vec<&str> = plan.iter().map(|i| i.target_rel.as_str()).collect();
    assert_eq!(rels, vec!["x.jpg", "new.jpg"], "{plan:?}");
    assert!(!t(tmp.path(), "x.jpg").exists(), "planning copies nothing");
    run(&store, &plan)?;
    assert_eq!(std::fs::read(t(tmp.path(), "x.jpg"))?, b"same");
    assert!(
        !t(tmp.path(), "y.jpg").exists(),
        "the second copy of a content stays behind"
    );
    assert!(
        !t(tmp.path(), "old.jpg").exists(),
        "the reference already holds it"
    );
    assert!(
        store.get_file_entry("t", "new.jpg")?.is_some(),
        "the copy is indexed in the target"
    );
    Ok(())
}

#[test]
fn subfolder_and_template_place_the_copy() -> TestResult {
    let (tmp, store) = setup(&[("a", "deep/photo.jpg", b"pic")])?;
    let plan = plan_copy_into(
        &store,
        &spec(&["a"], Some("imports"), Some("sorted/{o-name}")),
    )?;
    assert_eq!(plan[0].target_rel, "imports/sorted/photo.jpg");
    run(&store, &plan)?;
    assert!(t(tmp.path(), "imports/sorted/photo.jpg").is_file());
    let plain = plan_copy_into(&store, &spec(&["a"], Some("../escape"), None));
    assert!(plain.is_err(), "a subfolder cannot leave the target");
    Ok(())
}

#[test]
fn an_occupied_name_gets_a_suffix_and_the_existing_file_stays() -> TestResult {
    let (tmp, store) = setup(&[("a", "x.jpg", b"incoming"), ("t", "x.jpg", b"original")])?;
    let plan = plan_copy_into(&store, &spec(&["a"], None, None))?;
    assert_eq!(plan[0].target_rel, "x (1).jpg");
    assert!(plan[0].suffixed);
    run(&store, &plan)?;
    assert_eq!(
        std::fs::read(t(tmp.path(), "x.jpg"))?,
        b"original",
        "never overwritten"
    );
    assert_eq!(std::fs::read(t(tmp.path(), "x (1).jpg"))?, b"incoming");
    Ok(())
}
