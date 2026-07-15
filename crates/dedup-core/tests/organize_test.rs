//! Timeline bucketing + dated export by best-known date, plus rule-based
//! in-repo organize.

use dedup_core::diff::{DiffRun, NoDiffProgress};
use dedup_core::filter::ymd_to_ms;
use dedup_core::organize::{OrganizeRule, organize_apply, plan_organize};
use dedup_core::store::{ExifInfo, FileEntry, Store};
use dedup_core::update::CancellationToken;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn rule(filter: Option<&str>, template: &str) -> OrganizeRule {
    OrganizeRule {
        filter: filter.map(str::to_string),
        template: template.to_string(),
    }
}

fn entry(rel: &str, ms: i64) -> FileEntry {
    // Size and hash match the file content the test writes (the file name),
    // so export's already-present check sees the real identity.
    FileEntry {
        size: rel.len() as u64,
        hash: *blake3::hash(rel.as_bytes()).as_bytes(),
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

    // Re-running the export is idempotent: identical destinations are skipped,
    // nothing is copied again, and no " (2)" duplicates appear.
    let again = dedup_core::organize::export_by_date(&store, &["r".to_string()], &out, None)?;
    assert_eq!((again.copied, again.skipped, again.errors), (0, 3, 0));
    assert!(
        !out.join("2021").join("03").join("a (2).jpg").exists(),
        "re-run must not duplicate exports"
    );

    // Same name, different content → the numeric suffix is used (never an
    // overwrite, never a false skip).
    std::fs::write(dir.join("a.jpg"), b"different pixels")?;
    let mut changed = entry("a.jpg", ymd_to_ms(2021, 3, 10));
    changed.size = 16;
    changed.hash = *blake3::hash(b"different pixels").as_bytes();
    store.update_file_entry("r", "a.jpg", &changed)?;
    let third = dedup_core::organize::export_by_date(&store, &["r".to_string()], &out, None)?;
    assert_eq!((third.copied, third.skipped), (1, 2));
    assert!(out.join("2021").join("03").join("a (2).jpg").exists());
    assert_eq!(
        std::fs::read(out.join("2021").join("03").join("a.jpg"))?,
        b"a.jpg",
        "the original export is untouched"
    );
    Ok(())
}

/// A repo with real files on disk and matching index entries.
fn organize_repo()
-> Result<(tempfile::TempDir, Store, std::path::PathBuf), Box<dyn std::error::Error>> {
    let tmp = tempfile::tempdir()?;
    let store = Store::open_at(tmp.path().join("cfg"))?;
    let dir = tmp.path().join("repo");
    std::fs::create_dir_all(&dir)?;
    store.create_repo("r", &dir.to_string_lossy())?;
    Ok((tmp, store, dir))
}

fn write_indexed(store: &Store, dir: &std::path::Path, rel: &str, e: &FileEntry) -> TestResult {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, format!("content:{rel}").as_bytes())?;
    let mut e = e.clone();
    e.size = std::fs::metadata(&path)?.len();
    e.hash = *blake3::hash(format!("content:{rel}").as_bytes()).as_bytes();
    store.update_file_entry("r", rel, &e)?;
    Ok(())
}

#[test]
fn organize_moves_by_template_and_updates_index() -> TestResult {
    let (_tmp, store, dir) = organize_repo()?;
    let mut e = entry("x", ymd_to_ms(2021, 3, 10));
    e.exif = Some(ExifInfo {
        taken_ms: Some(ymd_to_ms(2019, 12, 25)),
        camera: Some("Canon EOS 5D".into()),
    });
    write_indexed(&store, &dir, "loose/photo.jpg", &e)?;

    // Rename into <year>/<stem>-<camera>.<ext> using the EXIF date + camera.
    let rules = vec![rule(
        Some("mime:image"),
        "{year}/{o-stem}-{camera|\"nocam\"}.{o-ext}",
    )];

    // Preview matches what apply does.
    let plan = plan_organize(&store, "r", &rules)?;
    assert_eq!(
        plan,
        vec![(
            "loose/photo.jpg".to_string(),
            "2019/photo-Canon EOS 5D.jpg".to_string()
        )]
    );

    let cancel = CancellationToken::new();
    let run = DiffRun::new(&NoDiffProgress, &cancel);
    let stats = organize_apply(&store, "r", &rules, &run)?;
    assert_eq!((stats.moved, stats.skipped, stats.errors), (1, 0, 0));

    // File moved on disk; index reflects the new path and drops the old.
    assert!(dir.join("2019/photo-Canon EOS 5D.jpg").exists());
    assert!(!dir.join("loose/photo.jpg").exists());
    assert!(
        store
            .get_file_entry("r", "2019/photo-Canon EOS 5D.jpg")?
            .is_some()
    );
    assert!(store.get_file_entry("r", "loose/photo.jpg")?.is_none());
    Ok(())
}

#[test]
fn organize_default_template_is_identity_noop() -> TestResult {
    let (_tmp, store, dir) = organize_repo()?;
    write_indexed(&store, &dir, "a/b.txt", &entry("x", 1))?;

    let rules = vec![rule(None, dedup_core::organize::DEFAULT_TEMPLATE)];
    assert!(
        plan_organize(&store, "r", &rules)?.is_empty(),
        "the identity template plans no moves"
    );
    let cancel = CancellationToken::new();
    let run = DiffRun::new(&NoDiffProgress, &cancel);
    let stats = organize_apply(&store, "r", &rules, &run)?;
    assert_eq!(stats.moved, 0);
    assert!(dir.join("a/b.txt").exists(), "file stays put");
    Ok(())
}

#[test]
fn organize_leaves_unmatched_files_untouched() -> TestResult {
    let (_tmp, store, dir) = organize_repo()?;
    let mut img = entry("x", ymd_to_ms(2021, 3, 10));
    img.mime = Some("image/jpeg".into());
    write_indexed(&store, &dir, "pic.jpg", &img)?;
    let mut doc = entry("x", ymd_to_ms(2021, 3, 10));
    doc.mime = Some("text/plain".into());
    write_indexed(&store, &dir, "notes.txt", &doc)?;

    // Only images are organized; the text file matches no rule.
    let rules = vec![rule(Some("mime:image"), "photos/{o-name}")];
    let cancel = CancellationToken::new();
    let run = DiffRun::new(&NoDiffProgress, &cancel);
    let stats = organize_apply(&store, "r", &rules, &run)?;
    assert_eq!(stats.moved, 1);
    assert!(dir.join("photos/pic.jpg").exists());
    assert!(dir.join("notes.txt").exists(), "unmatched file untouched");
    assert!(store.get_file_entry("r", "notes.txt")?.is_some());
    Ok(())
}

#[test]
fn organize_never_overwrites_different_content() -> TestResult {
    let (_tmp, store, dir) = organize_repo()?;
    // Two different-content files that both render to the same target name.
    write_indexed(&store, &dir, "one/report.pdf", &entry("x", 1))?;
    write_indexed(&store, &dir, "two/report.pdf", &entry("x", 1))?;

    // Flatten everything to archive/<name>; the two collide by name.
    let rules = vec![rule(None, "archive/{o-name}")];
    let cancel = CancellationToken::new();
    let run = DiffRun::new(&NoDiffProgress, &cancel);
    let stats = organize_apply(&store, "r", &rules, &run)?;
    assert_eq!(stats.moved, 2, "both move, one gets a suffix");

    assert!(dir.join("archive/report.pdf").exists());
    assert!(
        dir.join("archive/report (2).pdf").exists(),
        "the colliding file is suffixed, never overwritten"
    );
    // Both distinct contents survive.
    let a = std::fs::read(dir.join("archive/report.pdf"))?;
    let b = std::fs::read(dir.join("archive/report (2).pdf"))?;
    assert_ne!(a, b, "distinct contents preserved");
    Ok(())
}
