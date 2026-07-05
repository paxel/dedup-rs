//! Integration tests for `dedup_core::dupes` — ports of the exact-duplicate
//! scenarios from the legacy `DuplicateRepoProcessTest` (sorting, deletion,
//! cross-repo grouping). Similarity search is Phase 4 and not covered here.

use dedup_core::dupes::{delete_duplicates, find_exact_duplicates, wasted_bytes};
use dedup_core::store::{FileEntry, Store};
use dedup_core::update::{CancellationToken, NoProgress, update_repo};
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

#[test]
fn sorts_groups_by_wasted_bytes_and_files_by_area_time_path() -> TestResult {
    let tempdir = tempfile::tempdir()?;
    let store = Store::open_at(tempdir.path().join("config"))?;
    setup_repo(&store, tempdir.path(), "repo")?;

    // Group 1: wasted bytes = (2 - 1) * 100 = 100.
    store.update_file_entry("repo", "g1_f1.jpg", &entry(100, 1, 1000, None))?;
    store.update_file_entry("repo", "g1_f2.jpg", &entry(100, 1, 2000, None))?;

    // Group 2: wasted bytes = (4 - 1) * 500 = 1500. One file per criterion:
    // z_img has an image area (best), a_img is oldest, b_img beats c_img by path.
    store.update_file_entry("repo", "z_img.jpg", &entry(500, 2, 2000, Some((10, 10))))?;
    store.update_file_entry("repo", "a_img.jpg", &entry(500, 2, 1000, None))?;
    store.update_file_entry("repo", "b_img.jpg", &entry(500, 2, 2000, None))?;
    store.update_file_entry("repo", "c_img.jpg", &entry(500, 2, 2000, None))?;

    let groups = find_exact_duplicates(&store, &["repo".to_string()])?;
    assert_eq!(groups.len(), 2);

    // The group wasting more bytes comes first.
    assert_eq!(wasted_bytes(&groups[0]), 1500);
    assert_eq!(wasted_bytes(&groups[1]), 100);

    let paths: Vec<&str> = groups[0].iter().map(|f| f.rel_path.as_str()).collect();
    assert_eq!(paths, ["z_img.jpg", "a_img.jpg", "b_img.jpg", "c_img.jpg"]);

    let paths: Vec<&str> = groups[1].iter().map(|f| f.rel_path.as_str()).collect();
    assert_eq!(paths, ["g1_f1.jpg", "g1_f2.jpg"]);
    Ok(())
}

#[test]
fn orders_equal_size_by_oldest_first() -> TestResult {
    let tempdir = tempfile::tempdir()?;
    let store = Store::open_at(tempdir.path().join("config"))?;
    setup_repo(&store, tempdir.path(), "repo")?;

    store.update_file_entry("repo", "newer.jpg", &entry(100, 1, 2000, None))?;
    store.update_file_entry("repo", "older.jpg", &entry(100, 1, 1000, None))?;

    let groups = find_exact_duplicates(&store, &["repo".to_string()])?;
    assert_eq!(groups.len(), 1);
    let paths: Vec<&str> = groups[0].iter().map(|f| f.rel_path.as_str()).collect();
    assert_eq!(paths, ["older.jpg", "newer.jpg"]);
    Ok(())
}

#[test]
fn deletes_duplicates_keeping_the_best_copy() -> TestResult {
    let tempdir = tempfile::tempdir()?;
    let store = Store::open_at(tempdir.path().join("config"))?;
    let root = tempdir.path().join("repo");
    std::fs::create_dir_all(&root)?;
    store.create_repo("repo", &root.to_string_lossy())?;

    // Two real files with identical content; keep.jpg is older.
    std::fs::write(root.join("keep.jpg"), b"content")?;
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(root.join("delete.jpg"), b"content")?;
    update_repo(&store, "repo", 1, &NoProgress, &CancellationToken::new())?;

    let groups = find_exact_duplicates(&store, &["repo".to_string()])?;
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0][0].rel_path, "keep.jpg");

    let stats = delete_duplicates(&store, &groups)?;
    assert_eq!(stats.deleted, 1);
    assert_eq!(stats.errors, 0);
    assert!(root.join("keep.jpg").exists());
    assert!(!root.join("delete.jpg").exists());

    let deleted = store
        .get_file_entry("repo", "delete.jpg")?
        .ok_or("delete.jpg entry dropped")?;
    assert!(deleted.missing);
    let kept = store
        .get_file_entry("repo", "keep.jpg")?
        .ok_or("keep.jpg not indexed")?;
    assert!(!kept.missing);
    Ok(())
}

#[test]
fn delete_files_removes_an_explicit_selection_and_marks_it_missing() -> TestResult {
    let tempdir = tempfile::tempdir()?;
    let store = Store::open_at(tempdir.path().join("config"))?;
    let root = tempdir.path().join("repo");
    std::fs::create_dir_all(&root)?;
    store.create_repo("repo", &root.to_string_lossy())?;

    // Three identical copies; the review UI marks a specific subset.
    for name in ["a.jpg", "b.jpg", "c.jpg"] {
        std::fs::write(root.join(name), b"same bytes")?;
    }
    update_repo(&store, "repo", 1, &NoProgress, &CancellationToken::new())?;
    let groups = find_exact_duplicates(&store, &["repo".to_string()])?;
    assert_eq!(groups.len(), 1);

    // Delete exactly the two files whose rel_path is b.jpg / c.jpg, keep a.jpg.
    let selection: Vec<&_> = groups[0]
        .iter()
        .filter(|f| f.rel_path == "b.jpg" || f.rel_path == "c.jpg")
        .collect();
    let stats = dedup_core::dupes::delete_files(&store, &selection)?;
    assert_eq!(stats.deleted, 2);
    assert_eq!(stats.errors, 0);

    assert!(root.join("a.jpg").exists());
    assert!(!root.join("b.jpg").exists());
    assert!(!root.join("c.jpg").exists());
    assert!(!store.get_file_entry("repo", "a.jpg")?.ok_or("a")?.missing);
    assert!(store.get_file_entry("repo", "b.jpg")?.ok_or("b")?.missing);
    // No duplicates remain (only a.jpg is present).
    assert!(find_exact_duplicates(&store, &["repo".to_string()])?.is_empty());
    Ok(())
}

#[test]
fn finds_duplicates_across_repos() -> TestResult {
    let tempdir = tempfile::tempdir()?;
    let store = Store::open_at(tempdir.path().join("config"))?;
    for name in ["one", "two"] {
        let root = tempdir.path().join(name);
        std::fs::create_dir_all(&root)?;
        store.create_repo(name, &root.to_string_lossy())?;
        std::fs::write(root.join(format!("{name}.bin")), b"same content")?;
        update_repo(&store, name, 1, &NoProgress, &CancellationToken::new())?;
    }

    let groups = find_exact_duplicates(&store, &["one".to_string(), "two".to_string()])?;
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].len(), 2);
    let repos: Vec<&str> = groups[0].iter().map(|f| f.repo.as_str()).collect();
    assert!(repos.contains(&"one") && repos.contains(&"two"));
    Ok(())
}

#[test]
fn same_absolute_path_in_overlapping_repos_counts_once() -> TestResult {
    let tempdir = tempfile::tempdir()?;
    let store = Store::open_at(tempdir.path().join("config"))?;
    let root = tempdir.path().join("shared");
    std::fs::create_dir_all(&root)?;
    // Two repos over the same directory.
    store.create_repo("first", &root.to_string_lossy())?;
    store.create_repo("second", &root.to_string_lossy())?;
    std::fs::write(root.join("x.txt"), b"data")?;
    update_repo(&store, "first", 1, &NoProgress, &CancellationToken::new())?;
    update_repo(&store, "second", 1, &NoProgress, &CancellationToken::new())?;

    // The same physical file must not form a duplicate group with itself.
    let groups = find_exact_duplicates(&store, &["first".to_string(), "second".to_string()])?;
    assert!(groups.is_empty());
    Ok(())
}

#[test]
fn missing_entries_never_join_groups() -> TestResult {
    let tempdir = tempfile::tempdir()?;
    let store = Store::open_at(tempdir.path().join("config"))?;
    let root = tempdir.path().join("repo");
    std::fs::create_dir_all(&root)?;
    store.create_repo("repo", &root.to_string_lossy())?;

    std::fs::write(root.join("a.txt"), b"payload")?;
    std::fs::write(root.join("b.txt"), b"payload")?;
    update_repo(&store, "repo", 1, &NoProgress, &CancellationToken::new())?;
    std::fs::remove_file(root.join("b.txt"))?;
    update_repo(&store, "repo", 1, &NoProgress, &CancellationToken::new())?;

    let groups = find_exact_duplicates(&store, &["repo".to_string()])?;
    assert!(groups.is_empty());
    Ok(())
}
