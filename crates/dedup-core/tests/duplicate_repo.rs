//! `duplicate_repo` (the ported `repo cp`): the destination inherits every
//! index entry at a new path, and the source is left untouched.

use dedup_core::store::Store;
use dedup_core::update::{CancellationToken, NoProgress, update_repo};
use std::fs;

#[test]
fn duplicate_copies_index_to_new_path_and_leaves_source_untouched() {
    let config = tempfile::tempdir().expect("config");
    let src_dir = tempfile::tempdir().expect("src dir");
    let dst_dir = tempfile::tempdir().expect("dst dir");
    let store = Store::open_at(config.path().to_path_buf()).expect("store");

    fs::write(src_dir.path().join("a.txt"), b"alpha").expect("write a");
    fs::write(src_dir.path().join("b.txt"), b"beta").expect("write b");

    store
        .create_repo("src", &src_dir.path().to_string_lossy())
        .expect("create src");
    update_repo(&store, "src", 1, &NoProgress, &CancellationToken::new()).expect("update src");
    let src_stats = store.get_repo_stats("src").expect("src stats");
    assert_eq!(src_stats.file_count, 2);

    // Duplicate into a new repo pointing at a different directory.
    store
        .duplicate_repo("src", "dst", &dst_dir.path().to_string_lossy())
        .expect("duplicate");

    // Destination carries the same entries and stats...
    let dst_stats = store.get_repo_stats("dst").expect("dst stats");
    assert_eq!(dst_stats, src_stats);
    let a_src = store
        .get_file_entry("src", "a.txt")
        .expect("q")
        .expect("a in src");
    let a_dst = store
        .get_file_entry("dst", "a.txt")
        .expect("q")
        .expect("a in dst");
    assert_eq!(a_src, a_dst, "entry copied verbatim");

    // ...at the new path, with the source registration unchanged.
    let dst_meta = store.get_repo("dst").expect("dst meta");
    assert_eq!(dst_meta.abs_path, dst_dir.path().to_string_lossy());
    let src_meta = store.get_repo("src").expect("src meta");
    assert_eq!(src_meta.abs_path, src_dir.path().to_string_lossy());

    // The copy is independent: removing it leaves the source intact.
    store.remove_repo("dst").expect("remove dst");
    assert_eq!(
        store.get_repo_stats("src").expect("src stats").file_count,
        2
    );
    assert!(store.get_repo("dst").is_err());
}

#[test]
fn duplicate_rejects_same_name_and_existing_destination() {
    let config = tempfile::tempdir().expect("config");
    let dir = tempfile::tempdir().expect("dir");
    let store = Store::open_at(config.path().to_path_buf()).expect("store");
    store
        .create_repo("one", &dir.path().to_string_lossy())
        .expect("create one");
    store
        .create_repo("two", &dir.path().to_string_lossy())
        .expect("create two");

    assert!(store.duplicate_repo("one", "one", "/tmp/x").is_err());
    assert!(
        store.duplicate_repo("one", "two", "/tmp/x").is_err(),
        "must not clobber an existing repo"
    );
    // The clash left 'two' as it was.
    assert_eq!(
        store.get_repo("two").expect("two meta").abs_path,
        dir.path().to_string_lossy()
    );
}
