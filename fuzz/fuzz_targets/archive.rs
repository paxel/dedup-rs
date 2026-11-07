//! An archive's member listing (what a scan indexes) and its browse listing,
//! for zip, tar and tar.gz: the first byte picks the format.
#![no_main]

use libfuzzer_sys::fuzz_target;

const NAMES: [&str; 3] = ["input.zip", "input.tar", "input.tar.gz"];

fuzz_target!(|data: &[u8]| {
    let Some((&pick, body)) = data.split_first() else {
        return;
    };
    let name = NAMES
        .get(usize::from(pick) % NAMES.len())
        .copied()
        .unwrap_or("input.zip");
    let Ok(dir) = tempfile::tempdir() else {
        return;
    };
    let path = dir.path().join(name);
    if std::fs::write(&path, body).is_err() {
        return;
    }
    let _ = dedup_core::archive::list_members(&path, name, None);
    let _ = dedup_core::archive::list_entries(&path, name, None);
});
