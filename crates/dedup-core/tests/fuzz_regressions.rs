//! Inputs fuzzing found (fuzz/, docs/security/threat-model.md threat 6). Each
//! once crashed or hung a parser; each must now be handled like any other bad
//! file. Fixtures live in tests/fixtures/fuzz/.

use dedup_core::store::Store;
use dedup_core::update::{CancellationToken, NoProgress, update_repo};
use std::path::Path;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/fuzz")
        .join(name)
}

/// symphonia's MKV reader panics on this file ("attempt to shift left with
/// overflow" in its variable-length integer). The fingerprint gives up, the
/// MIME type stays.
#[test]
fn a_panicking_audio_parser_costs_only_the_fingerprint() {
    let fp = dedup_core::fingerprint::compute(&fixture("mkv-vint-panic.ogg"), false);
    assert!(fp.audio.is_none());
    assert!(fp.mime.is_some(), "the file keeps its type: {fp:?}");
}

/// The same file in a scanned repository: the scan finishes and indexes it.
#[test]
fn a_scan_survives_a_panicking_parser() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo)?;
    std::fs::copy(fixture("mkv-vint-panic.ogg"), repo.join("track.ogg"))?;
    std::fs::write(repo.join("note.txt"), "still indexed")?;
    let store = Store::open_at(tmp.path().join("cfg"))?;
    store.create_repo("r", &repo.to_string_lossy())?;
    update_repo(&store, "r", 1, &NoProgress, &CancellationToken::new())?;
    let db = store.open_repo_db("r")?;
    let track = dedup_core::store::get_entry(&db, "track.ogg")?.ok_or("track.ogg indexed")?;
    assert!(track.audio.is_none());
    assert!(dedup_core::store::get_entry(&db, "note.txt")?.is_some());
    Ok(())
}
