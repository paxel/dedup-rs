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

/// symphonia panics on these files: its MKV reader in a variable-length
/// integer, its AAC decoder in an ADTS header. The fingerprint gives up, the
/// MIME type stays.
#[test]
fn a_panicking_audio_parser_costs_only_the_fingerprint() {
    for name in ["mkv-vint-panic.ogg", "aac-adts-panic.ogg"] {
        let fp = dedup_core::fingerprint::compute(&fixture(name), false);
        assert!(fp.audio.is_none(), "{name}");
        assert!(fp.mime.is_some(), "{name} keeps its type: {fp:?}");
    }
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
