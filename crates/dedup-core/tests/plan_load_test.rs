//! Tests for the streamed/paged exact-duplicate API: `plan_exact_duplicates`
//! (lightweight descriptors, no file entries), `load_groups`/`load_group`
//! (materialize a page on demand), and `delete_paths` (delete by key). The plan
//! + load path must reproduce `find_exact_duplicates` exactly.

use dedup_core::dupes::{
    delete_paths, find_exact_duplicates, load_group, load_groups, plan_exact_duplicates,
};
use dedup_core::store::{FileEntry, Store};
use std::path::Path;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn entry(size: u64, hash_byte: u8, modified_ms: i64, img_size: Option<(u32, u32)>) -> FileEntry {
    FileEntry {
        size,
        hash: [hash_byte; 32],
        modified_ms,
        missing: false,
        mime: None,
        img_fingerprint: None,
        video_hash: None,
        pdf_hash: None,
        audio: None,
        img_size,
    }
}

fn setup_repo(store: &Store, tempdir: &Path, name: &str) -> TestResult {
    let root = tempdir.join(name);
    std::fs::create_dir_all(&root)?;
    store.create_repo(name, &root.to_string_lossy())?;
    Ok(())
}

/// Seed the sorting fixture: a 4-member group (waste 1500), a 2-member group
/// (waste 100), and a unique file that must not appear in any group.
fn seed_two_groups(store: &Store) -> TestResult {
    store.update_file_entry("repo", "g1_f1.jpg", &entry(100, 1, 1000, None))?;
    store.update_file_entry("repo", "g1_f2.jpg", &entry(100, 1, 2000, None))?;
    store.update_file_entry("repo", "z_img.jpg", &entry(500, 2, 2000, Some((10, 10))))?;
    store.update_file_entry("repo", "a_img.jpg", &entry(500, 2, 1000, None))?;
    store.update_file_entry("repo", "b_img.jpg", &entry(500, 2, 2000, None))?;
    store.update_file_entry("repo", "c_img.jpg", &entry(500, 2, 2000, None))?;
    store.update_file_entry("repo", "unique.jpg", &entry(77, 9, 3000, None))?;
    Ok(())
}

#[test]
fn plan_lists_multi_member_groups_ordered_by_waste() -> TestResult {
    let tempdir = tempfile::tempdir()?;
    let store = Store::open_at(tempdir.path().join("config"))?;
    setup_repo(&store, tempdir.path(), "repo")?;
    seed_two_groups(&store)?;

    let plan = plan_exact_duplicates(&store, &["repo".to_string()], |_| {})?;
    // Two groups; the unique file is excluded.
    assert_eq!(plan.len(), 2);
    // Ordered by wasted bytes descending.
    assert_eq!(plan[0].size, 500);
    assert_eq!(plan[0].count, 4);
    assert_eq!(plan[0].wasted_bytes(), 1500);
    assert_eq!(plan[1].size, 100);
    assert_eq!(plan[1].count, 2);
    assert_eq!(plan[1].wasted_bytes(), 100);
    Ok(())
}

#[test]
fn plan_then_load_reproduces_find_exactly() -> TestResult {
    let tempdir = tempfile::tempdir()?;
    let store = Store::open_at(tempdir.path().join("config"))?;
    setup_repo(&store, tempdir.path(), "repo")?;
    seed_two_groups(&store)?;

    let repos = ["repo".to_string()];
    let via_find = find_exact_duplicates(&store, &repos)?;

    // Paging: plan, then load each descriptor.
    let plan = plan_exact_duplicates(&store, &repos, |_| {})?;
    let via_plan = load_groups(&store, &repos, &plan)?;
    assert_eq!(
        via_plan, via_find,
        "plan+load must match find output & order"
    );

    // load_group (single) matches the batch result, incl. within-group order.
    let first = load_group(&store, &repos, &plan[0])?;
    assert_eq!(first, via_find[0]);
    let order: Vec<&str> = first.iter().map(|f| f.rel_path.as_str()).collect();
    assert_eq!(order, ["z_img.jpg", "a_img.jpg", "b_img.jpg", "c_img.jpg"]);
    Ok(())
}

#[test]
fn plan_counts_and_loads_cross_repo_duplicates() -> TestResult {
    let tempdir = tempfile::tempdir()?;
    let store = Store::open_at(tempdir.path().join("config"))?;
    setup_repo(&store, tempdir.path(), "one")?;
    setup_repo(&store, tempdir.path(), "two")?;
    // Same content (size, hash) in both repos, different relative paths.
    store.update_file_entry("one", "a.bin", &entry(42, 7, 100, None))?;
    store.update_file_entry("two", "b.bin", &entry(42, 7, 200, None))?;

    let repos = ["one".to_string(), "two".to_string()];
    let plan = plan_exact_duplicates(&store, &repos, |_| {})?;
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].count, 2, "cross-repo members are counted");

    let group = load_group(&store, &repos, &plan[0])?;
    let got_repos: Vec<&str> = group.iter().map(|f| f.repo.as_str()).collect();
    assert!(got_repos.contains(&"one") && got_repos.contains(&"two"));
    Ok(())
}

#[test]
fn delete_paths_removes_files_and_marks_missing() -> TestResult {
    let tempdir = tempfile::tempdir()?;
    let store = Store::open_at(tempdir.path().join("config"))?;
    let root = tempdir.path().join("repo");
    std::fs::create_dir_all(&root)?;
    store.create_repo("repo", &root.to_string_lossy())?;

    // Three identical files on disk + index.
    for name in ["a.jpg", "b.jpg", "c.jpg"] {
        std::fs::write(root.join(name), b"same bytes")?;
        store.update_file_entry("repo", name, &entry(10, 5, 1000, None))?;
    }

    // Delete b and c by key; keep a.
    let stats = delete_paths(
        &store,
        &[
            ("repo".to_string(), "b.jpg".to_string()),
            ("repo".to_string(), "c.jpg".to_string()),
        ],
    )?;
    assert_eq!(stats.deleted, 2);
    assert_eq!(stats.errors, 0);

    assert!(root.join("a.jpg").exists());
    assert!(!root.join("b.jpg").exists());
    assert!(!root.join("c.jpg").exists());
    assert!(!store.get_file_entry("repo", "a.jpg")?.ok_or("a")?.missing);
    assert!(store.get_file_entry("repo", "b.jpg")?.ok_or("b")?.missing);
    // Missing entries drop out of the index, so no duplicates remain.
    assert!(plan_exact_duplicates(&store, &["repo".to_string()], |_| {})?.is_empty());
    Ok(())
}
