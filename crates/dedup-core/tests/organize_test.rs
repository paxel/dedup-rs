//! Timeline bucketing + dated export by best-known date.

use dedup_core::filter::ymd_to_ms;
use dedup_core::store::{FileEntry, Store};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn entry(rel: &str, ms: i64) -> FileEntry {
    let mut hash = [0u8; 32];
    hash[0] = rel.len() as u8;
    hash[1] = rel.bytes().next().unwrap_or(0);
    FileEntry {
        size: 100,
        hash,
        modified_ms: ms,
        missing: false,
        mime: Some("image/jpeg".into()),
        img_fingerprint: None,
        video_hash: None,
        pdf_hash: None,
        audio: None,
        img_size: None,
        origin: None,
        exif: None,
    }
}

#[test]
fn buckets_and_export_group_by_date() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let store = Store::open_at(tmp.path().join("cfg"))?;
    let dir = tmp.path().join("repo");
    std::fs::create_dir_all(&dir)?;
    store.create_repo("r", &dir.to_string_lossy())?;

    // Two files in 2021-03, one in 2020-07. Real files on disk for export.
    for (rel, y, m) in [("a.jpg", 2021, 3), ("b.jpg", 2021, 3), ("c.jpg", 2020, 7)] {
        std::fs::write(dir.join(rel), rel.as_bytes())?;
        store.update_file_entry("r", rel, &entry(rel, ymd_to_ms(y, m, 10)))?;
    }

    let buckets = dedup_core::organize::timeline_buckets(&store, &["r".to_string()], None)?;
    assert_eq!(buckets.len(), 2);
    // BTreeMap order: 2020-07 then 2021-03.
    assert_eq!(
        (buckets[0].year, buckets[0].month, buckets[0].count),
        (2020, 7, 1)
    );
    assert_eq!(
        (buckets[1].year, buckets[1].month, buckets[1].count),
        (2021, 3, 2)
    );

    // Filtered bucketing.
    let only_2021 =
        dedup_core::organize::timeline_buckets(&store, &["r".to_string()], Some("date:2021"))?;
    assert_eq!(only_2021.len(), 1);
    assert_eq!(only_2021[0].count, 2);

    // Export into a dated tree.
    let out = tmp.path().join("out");
    let stats = dedup_core::organize::export_by_date(&store, &["r".to_string()], &out, None)?;
    assert_eq!(stats.copied, 3);
    assert!(out.join("2020").join("07").join("c.jpg").exists());
    assert!(out.join("2021").join("03").join("a.jpg").exists());
    assert!(out.join("2021").join("03").join("b.jpg").exists());
    Ok(())
}
