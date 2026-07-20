//! Triage report aggregation: stats, exact-duplicate reclaimable bytes, and
//! flagged critical files come together per repo.

use dedup_core::scan::Category;
use dedup_core::store::Store;
use dedup_core::update::{CancellationToken, NoProgress, update_repo};

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[test]
fn report_aggregates_stats_dupes_and_flags() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let store = Store::open_at(tmp.path().join("cfg"))?;
    let dir = tmp.path().join("repo");
    std::fs::create_dir_all(&dir)?;

    // Two identical files (one exact-dup group) plus a flagged wallet file.
    std::fs::write(dir.join("a.txt"), b"same content")?;
    std::fs::write(dir.join("b.txt"), b"same content")?;
    std::fs::write(dir.join("wallet.dat"), b"whatever")?;
    store.create_repo("r", &dir.to_string_lossy())?;
    update_repo(&store, "r", 1, &NoProgress, &CancellationToken::new())?;

    let reports = dedup_core::report::build_report(&store, &["r".to_string()])?;
    assert_eq!(reports.len(), 1);
    let r = &reports[0];
    assert_eq!(r.files, 3);
    assert_eq!(r.dup_groups, 1, "the two identical files form one group");
    assert!(r.reclaimable > 0, "one copy is reclaimable");
    assert!(
        r.flags
            .iter()
            .any(|(c, n)| *c == Category::Wallet && *n == 1),
        "wallet.dat is flagged"
    );
    Ok(())
}
