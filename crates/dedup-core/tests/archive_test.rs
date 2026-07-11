//! Archive member indexing + coverage: a zip whose members all exist loose is
//! reported 100% redundant.

use dedup_core::store::Store;
use dedup_core::update::{CancellationToken, NoProgress, update_repo};
use std::io::Write;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn write_zip(path: &std::path::Path, entries: &[(&str, &[u8])]) {
    let file = std::fs::File::create(path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let opts: zip::write::FileOptions<'_, ()> =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, data) in entries {
        zip.start_file(*name, opts).unwrap();
        zip.write_all(data).unwrap();
    }
    zip.finish().unwrap();
}

#[test]
fn fully_redundant_archive_reports_full_coverage() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let store = Store::open_at(tmp.path().join("cfg"))?;

    // "loose" repo holds two files; "arch" repo holds a zip of the same two
    // plus one extra file not present loose.
    let loose_dir = tmp.path().join("loose");
    std::fs::create_dir_all(&loose_dir)?;
    std::fs::write(loose_dir.join("a.txt"), b"alpha")?;
    std::fs::write(loose_dir.join("b.txt"), b"beta")?;
    store.create_repo("loose", &loose_dir.to_string_lossy())?;
    update_repo(&store, "loose", 1, &NoProgress, &CancellationToken::new())?;

    let arch_dir = tmp.path().join("arch");
    std::fs::create_dir_all(&arch_dir)?;
    write_zip(
        &arch_dir.join("all.zip"),
        &[("a.txt", b"alpha"), ("b.txt", b"beta")],
    );
    write_zip(
        &arch_dir.join("partial.zip"),
        &[("a.txt", b"alpha"), ("new.txt", b"unique content")],
    );
    store.create_repo("arch", &arch_dir.to_string_lossy())?;
    update_repo(&store, "arch", 1, &NoProgress, &CancellationToken::new())?;

    let n = dedup_core::archive::index_repo_archives(&store, "arch")?;
    assert_eq!(n, 2, "both zips indexed");

    let report = dedup_core::archive::repo_archive_coverage(&store, "arch", &["loose"])?;
    let all = report.iter().find(|c| c.rel_path == "all.zip").unwrap();
    assert_eq!((all.members, all.present), (2, 2));
    assert!(all.redundant, "all.zip is fully redundant");

    let partial = report.iter().find(|c| c.rel_path == "partial.zip").unwrap();
    assert_eq!((partial.members, partial.present), (2, 1));
    assert!(!partial.redundant, "partial.zip keeps unique content");
    Ok(())
}
