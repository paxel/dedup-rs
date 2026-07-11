//! Triage report: the caretaker's audit trail — per repo, what was reduced,
//! what remains, and what's flagged. Read-only aggregation over existing
//! machinery (stats, exact-duplicate plans, the important-file scanner, and the
//! MIME histogram). The CLI renders these as Markdown.

use crate::dupes::plan_exact_duplicates;
use crate::scan::{self, Category};
use crate::store::{Store, StoreError};

/// The report for a single repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoReport {
    pub name: String,
    pub files: u64,
    pub bytes: u64,
    pub missing: u64,
    /// Epoch ms the repo was marked triage-done; 0 if not.
    pub triage_done_ms: u64,
    /// Number of exact-duplicate groups and the bytes reclaimable by keeping
    /// one copy of each.
    pub dup_groups: usize,
    pub reclaimable: u64,
    /// Flagged critical files per category (only non-zero categories).
    pub flags: Vec<(Category, usize)>,
    /// Top MIME types by count (largest first), capped.
    pub top_mimes: Vec<(String, u64)>,
}

/// Build a read-only triage report for each repo. Duplicate stats are computed
/// per repo (within-repo exact duplicates).
pub fn build_report(store: &Store, repo_names: &[String]) -> Result<Vec<RepoReport>, StoreError> {
    let mut reports = Vec::with_capacity(repo_names.len());
    for name in repo_names {
        let stats = store.get_repo_stats(name)?;

        let plan = plan_exact_duplicates(store, std::slice::from_ref(name), |_| {})?;
        let reclaimable = plan.iter().map(|k| k.wasted_bytes()).sum();

        let flags = scan::scan_repos(store, std::slice::from_ref(name))?;
        let mut flags_by_category = Vec::new();
        for category in [
            Category::Wallet,
            Category::Key,
            Category::Vault,
            Category::Identity,
            Category::Financial,
        ] {
            let count = flags.iter().filter(|f| f.category == category).count();
            if count > 0 {
                flags_by_category.push((category, count));
            }
        }

        let mut top_mimes = store.get_mime_stats(name)?;
        top_mimes.truncate(8);

        reports.push(RepoReport {
            name: name.clone(),
            files: stats.file_count,
            bytes: stats.total_size,
            missing: stats.missing_count,
            triage_done_ms: stats.triage_done_ms,
            dup_groups: plan.len(),
            reclaimable,
            flags: flags_by_category,
            top_mimes,
        });
    }
    Ok(reports)
}
