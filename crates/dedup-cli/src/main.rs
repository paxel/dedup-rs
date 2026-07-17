use clap::{Parser, Subcommand};
use dedup_core::diff::{
    CopyDest, DiffItem, DiffRun, NoDiffProgress, diff_copy, diff_delete, diff_print, diff_sync,
};
use dedup_core::dupes::{DupeGroup, delete_duplicates, find_exact_duplicates, wasted_bytes};
use dedup_core::similar::find_similar;
use dedup_core::store::Store;
use dedup_core::update::{CancellationToken, Progress, ProgressEvent, update_repo};

#[derive(Parser)]
#[command(name = "dedup")]
#[command(version = env!("CARGO_PKG_VERSION"))]
#[command(about = "File deduplication tool in Rust", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
    /// GUI only: scale the interface by this factor (0.5–3.0)
    #[arg(long, global = true)]
    ui_scale: Option<f32>,
}

#[derive(Subcommand)]
enum Commands {
    /// Repository management commands
    Repo {
        #[command(subcommand)]
        command: RepoCommands,
    },
    /// Compare a source repo against a reference repo (by content, not path)
    Diff {
        #[command(subcommand)]
        command: DiffCommands,
    },
    /// Browse or export files by date (EXIF capture time, else mtime)
    Timeline {
        /// Repositories to organize
        names: Vec<String>,
        /// Use all registered repositories
        #[arg(long)]
        all: bool,
        /// Copy matching files into <dir>/<year>/<month>/ instead of listing buckets
        #[arg(long, value_name = "DIR")]
        export: Option<String>,
        /// Filter: date:/before:/after: (YYYY[-MM[-DD]]) plus mime:/name:/size:
        #[arg(short, long)]
        filter: Option<String>,
    },
    /// Print a Markdown triage report (stats, duplicates, flagged files)
    Report {
        /// Repositories to report on
        names: Vec<String>,
        /// Report on all registered repositories
        #[arg(long)]
        all: bool,
    },
    /// Scan repos for likely-critical files (wallets, keys, vaults, docs)
    Scan {
        /// Repositories to scan
        names: Vec<String>,
        /// Scan all registered repositories
        #[arg(long)]
        all: bool,
    },
    /// Index archive members and report how redundant archives are
    Archive {
        #[command(subcommand)]
        command: ArchiveCommands,
    },
    /// Triage a disk: scan it, copy its unique content into a sanitized repo
    /// (diffing against the sanitized repo and any extra references), then mark
    /// the source repo triage-done. The one-shot disk-inheritance workflow.
    Sanitize {
        /// Source repository (the disk to triage)
        source: String,
        /// Sanitized repository to copy unique content into (also a reference)
        sanitized: String,
        /// Additional already-processed reference repos; repeatable
        #[arg(long = "ref", value_name = "REPO")]
        refs: Vec<String>,
        /// Relative subfolder inside the sanitized repo to place files under
        #[arg(short = 'i', long)]
        into: Option<String>,
        /// Move instead of copy (marks source entries missing)
        #[arg(long)]
        move_files: bool,
        /// Skip the initial scan of the source (use the existing index)
        #[arg(long)]
        no_scan: bool,
        /// Filter: mime:/name:/size:/origin:
        #[arg(short, long)]
        filter: Option<String>,
    },
}

#[derive(Subcommand)]
enum DiffCommands {
    /// Print differences between source and reference(s)
    Print {
        /// Source repository
        source: String,
        /// Reference repository (content already known)
        reference: String,
        /// Additional reference repos; repeatable. A file counts as "new" only
        /// when none of the references (positional or `--ref`) has its content.
        #[arg(long = "ref", value_name = "REPO")]
        refs: Vec<String>,
        /// Filter: mime:<substring>, name:<substring>, or size:<expr>
        #[arg(short, long)]
        filter: Option<String>,
    },
    /// Copy files in source whose content no reference knows to a target directory
    Cp {
        /// Source repository
        source: String,
        /// Reference repository (content already known)
        reference: String,
        /// Target directory
        target: String,
        /// Additional reference repos; repeatable (see `diff print`).
        #[arg(long = "ref", value_name = "REPO")]
        refs: Vec<String>,
        /// Relative parent directory inside the target to place files under
        #[arg(short = 'i', long)]
        into: Option<String>,
        /// Filter: mime:<substring>, name:<substring>, or size:<expr>
        #[arg(short, long)]
        filter: Option<String>,
    },
    /// Move files in source whose content no reference knows to a target directory
    Mv {
        /// Source repository
        source: String,
        /// Reference repository (content already known)
        reference: String,
        /// Target directory
        target: String,
        /// Additional reference repos; repeatable (see `diff print`).
        #[arg(long = "ref", value_name = "REPO")]
        refs: Vec<String>,
        /// Relative parent directory inside the target to place files under
        #[arg(short = 'i', long)]
        into: Option<String>,
        /// Filter: mime:<substring>, name:<substring>, or size:<expr>
        #[arg(short, long)]
        filter: Option<String>,
    },
    /// Delete files in source whose content any reference already knows
    Rm {
        /// Source repository
        source: String,
        /// Reference repository (content already known)
        reference: String,
        /// Additional reference repos; repeatable (see `diff print`).
        #[arg(long = "ref", value_name = "REPO")]
        refs: Vec<String>,
        /// Filter: mime:<substring>, name:<substring>, or size:<expr>
        #[arg(short, long)]
        filter: Option<String>,
    },
    /// Sync target repo with source: copy new contents, optionally delete missing
    Sync {
        /// Source repository (A)
        source: String,
        /// Target repository (B)
        target: String,
        /// Copy contents that exist in A but not in B
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        copy_new: bool,
        /// Delete in B contents that A marks as missing
        #[arg(long)]
        delete_missing: bool,
        /// Equivalent to --copy-new true --delete-missing
        #[arg(long)]
        mirror: bool,
        /// Filter: mime:<substring>, name:<substring>, or size:<expr>
        #[arg(short, long)]
        filter: Option<String>,
    },
}

#[derive(Subcommand)]
enum ArchiveCommands {
    /// Index every archive's members in a repo (opt-in; reads each archive)
    Index {
        /// Repository holding the archives
        repo: String,
    },
    /// Report how much of each archive already exists as loose content
    Coverage {
        /// Repository holding the (already indexed) archives
        repo: String,
        /// Extra repos to count as "already have it"; repeatable
        #[arg(long = "ref", value_name = "REPO")]
        refs: Vec<String>,
        /// Only list archives that are fully redundant (100% covered)
        #[arg(long)]
        redundant_only: bool,
    },
}

#[derive(Subcommand)]
enum RepoCommands {
    /// Create a new repository link
    Create {
        /// Unique name of the repository
        name: String,
        /// Absolute or relative path to the repository directory
        path: String,
    },
    /// List all registered repositories and their metadata/statistics
    Ls,
    /// Remove a repository registry link and delete its local database
    Rm {
        /// Name of the repository to remove
        name: String,
    },
    /// Rename a registered repository (moves its database directory name)
    Mv {
        /// Current name of the repository
        name: String,
        /// New name of the repository
        new_name: String,
    },
    /// Relocate the target path of a repository
    Rel {
        /// Name of the repository
        name: String,
        /// New target directory path
        new_path: String,
    },
    /// Copy a repository's index into a new one at a new path (source unchanged)
    Cp {
        /// Source repository to copy from
        source: String,
        /// Name of the new repository
        dest: String,
        /// Target directory path for the new repository
        path: String,
    },
    /// Scan repository directories and update their indices
    Update {
        /// Names of the repositories to update
        #[arg(required_unless_present = "all")]
        names: Vec<String>,
        /// Update all registered repositories
        #[arg(short, long)]
        all: bool,
        /// Number of hashing threads (0 = one per CPU core)
        #[arg(short, long, default_value_t = 0)]
        threads: usize,
    },
    /// Find exact duplicates (or, with --threshold, similar files) in repositories
    Dupes {
        /// Names of the repositories to search
        #[arg(required_unless_present = "all")]
        names: Vec<String>,
        /// Search all registered repositories
        #[arg(short, long)]
        all: bool,
        /// Delete all but the best copy of each group
        #[arg(long)]
        delete: bool,
        /// Similarity search: group perceptually similar files at >= this percent (1-100)
        #[arg(long)]
        threshold: Option<u32>,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Repo { command }) => {
            let store = Store::open()?;
            match command {
                RepoCommands::Create { name, path } => {
                    store.create_repo(&name, &path)?;
                    println!(
                        "Successfully created repository '{}' linked to path '{}'.",
                        name, path
                    );
                }
                RepoCommands::Ls => {
                    let repos = store.list_repos()?;
                    if repos.is_empty() {
                        println!(
                            "No repositories registered. Use 'dedup repo create <name> <path>' to register one."
                        );
                    } else {
                        println!(
                            "{:<15} {:<30} {:<20} {:<10} {:<12} {:<10}",
                            "Name", "Path", "Created At", "Files", "Size", "Missing"
                        );
                        println!("{}", "-".repeat(97));
                        for (name, meta, stats) in repos {
                            let created_str = format_time(meta.created);
                            let size_str = format_size(stats.total_size);
                            println!(
                                "{:<15} {:<30} {:<20} {:<10} {:<12} {:<10}",
                                name,
                                truncate_path(&meta.abs_path, 30),
                                created_str,
                                stats.file_count,
                                size_str,
                                stats.missing_count
                            );
                        }
                    }
                }
                RepoCommands::Rm { name } => {
                    store.remove_repo(&name)?;
                    println!("Successfully removed repository '{}'.", name);
                }
                RepoCommands::Mv { name, new_name } => {
                    store.rename_repo(&name, &new_name)?;
                    println!(
                        "Successfully renamed repository '{}' to '{}'.",
                        name, new_name
                    );
                }
                RepoCommands::Rel { name, new_path } => {
                    store.relocate_repo(&name, &new_path)?;
                    println!(
                        "Successfully relocated repository '{}' to path '{}'.",
                        name, new_path
                    );
                }
                RepoCommands::Cp { source, dest, path } => {
                    store.duplicate_repo(&source, &dest, &path)?;
                    println!(
                        "Successfully copied repository '{}' to '{}' at path '{}'.",
                        source, dest, path
                    );
                }
                RepoCommands::Update {
                    names,
                    all,
                    threads,
                } => {
                    update_repos(&store, names, all, threads)?;
                }
                RepoCommands::Dupes {
                    names,
                    all,
                    delete,
                    threshold,
                } => {
                    dupes(&store, names, all, delete, threshold)?;
                }
            }
        }
        Some(Commands::Diff { command }) => {
            let store = Store::open()?;
            run_diff(&store, command)?;
        }
        Some(Commands::Timeline {
            names,
            all,
            export,
            filter,
        }) => {
            let store = Store::open()?;
            run_timeline(&store, names, all, export, filter)?;
        }
        Some(Commands::Report { names, all }) => {
            let store = Store::open()?;
            run_report(&store, names, all)?;
        }
        Some(Commands::Scan { names, all }) => {
            let store = Store::open()?;
            run_scan(&store, names, all)?;
        }
        Some(Commands::Archive { command }) => {
            let store = Store::open()?;
            run_archive(&store, command)?;
        }
        Some(Commands::Sanitize {
            source,
            sanitized,
            refs,
            into,
            move_files,
            no_scan,
            filter,
        }) => {
            let store = Store::open()?;
            run_sanitize(
                &store, &source, &sanitized, &refs, into, move_files, no_scan, filter,
            )?;
        }
        None => {
            println!("Starting GUI...");
            if let Err(e) = dedup_gui::run(cli.ui_scale) {
                eprintln!("GUI Error: {}", e);
                std::process::exit(1);
            }
        }
    }

    Ok(())
}

/// Build a human-readable destination label, appending the relative subdir
/// (when set) to the target for the copy/move success messages.
fn destination(target: &str, into: &Option<String>) -> String {
    match into.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(sub) => format!("{target}/{sub}"),
        None => target.to_string(),
    }
}

/// Combine the positional reference with any repeatable `--ref` repos into the
/// reference list the diff ops expect (primary first).
fn diff_refs<'a>(reference: &'a str, extra: &'a [String]) -> Vec<&'a str> {
    std::iter::once(reference)
        .chain(extra.iter().map(String::as_str))
        .collect()
}

/// Resolve a repo-name list, expanding `--all` to every registered repo.
fn resolve_repo_names(store: &Store, names: Vec<String>, all: bool) -> anyhow::Result<Vec<String>> {
    let names = if all {
        store
            .list_repos()?
            .into_iter()
            .map(|(name, _, _)| name)
            .collect()
    } else {
        names
    };
    if names.is_empty() {
        anyhow::bail!("No repositories. Pass repo names or --all.");
    }
    Ok(names)
}

fn run_timeline(
    store: &Store,
    names: Vec<String>,
    all: bool,
    export: Option<String>,
    filter: Option<String>,
) -> anyhow::Result<()> {
    let names = resolve_repo_names(store, names, all)?;
    match export {
        Some(dir) => {
            let stats = dedup_core::organize::export_by_date(
                store,
                &names,
                std::path::Path::new(&dir),
                filter.as_deref(),
            )?;
            println!(
                "Exported {} file(s) into '{}' ({} already present, {} error(s)).",
                stats.copied, dir, stats.skipped, stats.errors
            );
        }
        None => {
            let buckets = dedup_core::organize::timeline_buckets(store, &names, filter.as_deref())?;
            if buckets.is_empty() {
                println!("No files match.");
                return Ok(());
            }
            for b in &buckets {
                println!(
                    "{}-{:02}  {:>6} files  {}",
                    b.year,
                    b.month,
                    b.count,
                    format_size(b.bytes)
                );
            }
        }
    }
    Ok(())
}

fn run_report(store: &Store, names: Vec<String>, all: bool) -> anyhow::Result<()> {
    let names = resolve_repo_names(store, names, all)?;
    let reports = dedup_core::report::build_report(store, &names)?;
    println!("# dedup triage report\n");
    for r in &reports {
        println!("## {}\n", r.name);
        println!("- Files: {} ({})", r.files, format_size(r.bytes));
        println!("- Missing (indexed, gone from disk): {}", r.missing);
        println!(
            "- Exact-duplicate groups: {} · reclaimable {}",
            r.dup_groups,
            format_size(r.reclaimable)
        );
        let triaged = if r.triage_done_ms > 0 { "yes" } else { "no" };
        println!("- Triaged: {triaged}");
        if r.flags.is_empty() {
            println!("- Flagged critical files: none");
        } else {
            let parts: Vec<String> = r
                .flags
                .iter()
                .map(|(c, n)| format!("{} {}", n, c.label()))
                .collect();
            println!("- Flagged critical files: {}", parts.join(", "));
        }
        if !r.top_mimes.is_empty() {
            println!("- Top types:");
            for (mime, count) in &r.top_mimes {
                println!("  - {mime}: {count}");
            }
        }
        println!();
    }
    Ok(())
}

fn run_scan(store: &Store, names: Vec<String>, all: bool) -> anyhow::Result<()> {
    let names = resolve_repo_names(store, names, all)?;

    let flags = dedup_core::scan::scan_repos(store, &names)?;
    if flags.is_empty() {
        println!("No critical files flagged.");
        return Ok(());
    }
    // Group by category for a reviewable report.
    use dedup_core::scan::Category;
    for category in [
        Category::Wallet,
        Category::Key,
        Category::Vault,
        Category::Identity,
        Category::Financial,
    ] {
        let group: Vec<_> = flags.iter().filter(|f| f.category == category).collect();
        if group.is_empty() {
            continue;
        }
        println!("\n{} ({})", category.label(), group.len());
        for f in group {
            println!("  {}/{}  — {}", f.repo, f.rel_path, f.reason);
        }
    }
    println!(
        "\n{} file(s) flagged (advisory — nothing was modified).",
        flags.len()
    );
    Ok(())
}

fn run_archive(store: &Store, command: ArchiveCommands) -> anyhow::Result<()> {
    match command {
        ArchiveCommands::Index { repo } => {
            let n = dedup_core::archive::index_repo_archives(store, &repo)?;
            println!("Indexed {n} archive(s) in '{repo}'.");
        }
        ArchiveCommands::Coverage {
            repo,
            refs,
            redundant_only,
        } => {
            // Count against the repo's own loose content plus any extra refs.
            let references = diff_refs(&repo, &refs);
            let report = dedup_core::archive::repo_archive_coverage(store, &repo, &references)?;
            let mut redundant = 0;
            for cov in &report {
                if redundant_only && !cov.redundant {
                    continue;
                }
                if cov.redundant {
                    redundant += 1;
                }
                println!(
                    "{:>5.1}%  {}/{}  {}{}",
                    cov.percent(),
                    cov.present,
                    cov.members,
                    cov.rel_path,
                    if cov.redundant { "  [REDUNDANT]" } else { "" }
                );
            }
            println!(
                "{} archive(s), {} fully redundant.",
                report.len(),
                redundant
            );
        }
    }
    Ok(())
}

/// The one-shot disk-triage workflow: scan the source, copy its unique content
/// into the sanitized repo (diffing against the sanitized repo plus any extra
/// references), and mark the source triage-done.
#[allow(clippy::too_many_arguments)]
fn run_sanitize(
    store: &Store,
    source: &str,
    sanitized: &str,
    refs: &[String],
    into: Option<String>,
    move_files: bool,
    no_scan: bool,
    filter: Option<String>,
) -> anyhow::Result<()> {
    let cancel = CancellationToken::new();
    {
        let cancel = cancel.clone();
        ctrlc::set_handler(move || cancel.cancel())?;
    }

    if !no_scan {
        println!("Scanning '{source}'…");
        // Same live progress as `repo update` — a whole-disk scan can run for
        // hours and must not look like a hang.
        let progress = TerminalProgress::new();
        update_repo(store, source, 0, &progress, &cancel)?;
        progress.finish();
    }

    // The sanitized repo's directory receives the copies (and is a reference).
    let sanitized_dir = store.get_repo(sanitized)?.abs_path;
    let references = diff_refs(sanitized, refs);
    let stats = diff_copy(
        store,
        source,
        &references,
        CopyDest {
            dir: std::path::Path::new(&sanitized_dir),
            subdir: into.as_deref(),
        },
        move_files,
        filter.as_deref(),
        &DiffRun::new(&NoDiffProgress, &cancel),
    )?;

    let verb = if move_files { "Moved" } else { "Copied" };
    println!(
        "{verb} {} unique file(s) from '{source}' into '{}'.",
        stats.copied,
        destination(sanitized, &into)
    );
    if stats.cancelled {
        println!("Sanitize cancelled by user; source not marked done.");
        return Ok(());
    }

    store.set_triage_done(source, true)?;
    println!("Marked '{source}' triage-done.");
    Ok(())
}

fn run_diff(store: &Store, command: DiffCommands) -> anyhow::Result<()> {
    let cancel = CancellationToken::new();
    {
        let cancel = cancel.clone();
        ctrlc::set_handler(move || cancel.cancel())?;
    }
    match command {
        DiffCommands::Print {
            source,
            reference,
            refs,
            filter,
        } => {
            let references = diff_refs(&reference, &refs);
            let items = diff_print(store, &source, &references, filter.as_deref())?;
            let mut new = 0u64;
            let mut equal = 0u64;
            let mut deleted = 0u64;
            for item in &items {
                match item {
                    DiffItem::New { rel_path } => {
                        new += 1;
                        println!("New: {}", rel_path);
                    }
                    DiffItem::Equal { .. } => equal += 1,
                    DiffItem::DeletedInReference { rel_path } => {
                        deleted += 1;
                        println!("Deleted in reference: {}", rel_path);
                    }
                }
            }
            println!(
                "{} new, {} equal, {} deleted in reference",
                new, equal, deleted
            );
        }
        DiffCommands::Cp {
            source,
            reference,
            target,
            refs,
            into,
            filter,
        } => {
            let references = diff_refs(&reference, &refs);
            let stats = diff_copy(
                store,
                &source,
                &references,
                CopyDest {
                    dir: std::path::Path::new(&target),
                    subdir: into.as_deref(),
                },
                false,
                filter.as_deref(),
                &DiffRun::new(&NoDiffProgress, &cancel),
            )?;
            println!(
                "Copied {} files to '{}'.",
                stats.copied,
                destination(&target, &into)
            );
            if stats.cancelled {
                println!("Copy cancelled by user.");
            }
        }
        DiffCommands::Mv {
            source,
            reference,
            target,
            refs,
            into,
            filter,
        } => {
            let references = diff_refs(&reference, &refs);
            let stats = diff_copy(
                store,
                &source,
                &references,
                CopyDest {
                    dir: std::path::Path::new(&target),
                    subdir: into.as_deref(),
                },
                true,
                filter.as_deref(),
                &DiffRun::new(&NoDiffProgress, &cancel),
            )?;
            println!(
                "Moved {} files to '{}'.",
                stats.copied,
                destination(&target, &into)
            );
            if stats.cancelled {
                println!("Move cancelled by user.");
            }
        }
        DiffCommands::Rm {
            source,
            reference,
            refs,
            filter,
        } => {
            let references = diff_refs(&reference, &refs);
            let stats = diff_delete(
                store,
                &source,
                &references,
                filter.as_deref(),
                &DiffRun::new(&NoDiffProgress, &cancel),
            )?;
            println!("Deleted {} files from '{}'.", stats.deleted, source);
            if stats.cancelled {
                println!("Delete cancelled by user.");
            }
        }
        DiffCommands::Sync {
            source,
            target,
            copy_new,
            delete_missing,
            mirror,
            filter,
        } => {
            let (copy_new, delete_missing) = if mirror {
                (true, true)
            } else {
                (copy_new, delete_missing)
            };
            let stats = diff_sync(
                store,
                &source,
                &target,
                copy_new,
                delete_missing,
                filter.as_deref(),
                &DiffRun::new(&NoDiffProgress, &cancel),
            )?;
            println!(
                "copied: {}, equal: {}, skipped: {}, deleted: {}, errors: {}",
                stats.copied, stats.equal, stats.skipped, stats.deleted, stats.errors
            );
            if stats.cancelled {
                println!("Sync cancelled by user.");
            }
        }
    }
    Ok(())
}

fn dupes(
    store: &Store,
    names: Vec<String>,
    all: bool,
    delete: bool,
    threshold: Option<u32>,
) -> anyhow::Result<()> {
    let names: Vec<String> = if all {
        store
            .list_repos()?
            .into_iter()
            .map(|(name, _, _)| name)
            .collect()
    } else {
        names
    };
    if names.is_empty() {
        anyhow::bail!("No repositories registered. Use 'dedup repo create <name> <path>' first.");
    }

    let groups: Vec<DupeGroup> = match threshold {
        Some(t) if t > 0 => {
            println!("Similarity search (threshold: {}%)", t);
            find_similar(store, &names, f64::from(t))?
        }
        _ => find_exact_duplicates(store, &names)?,
    };
    let mut total_wasted = 0u64;
    for group in &groups {
        let first = match group.first() {
            Some(first) => first,
            None => continue,
        };
        total_wasted += wasted_bytes(group);
        println!(
            "{} ({}, {} wasted)",
            hex(&first.entry.hash),
            format_size(first.entry.size),
            format_size(wasted_bytes(group))
        );
        for file in group {
            println!(
                "  {}: {}/{} (modified: {})",
                file.repo,
                file.repo_root,
                file.rel_path,
                format_time_ms(file.entry.modified_ms)
            );
        }
    }
    println!(
        "{} duplicate groups, {} wasted",
        groups.len(),
        format_size(total_wasted)
    );

    if delete {
        let stats = delete_duplicates(store, &groups)?;
        println!(
            "Deleted {} duplicate files (kept the best copy of each group), {} errors.",
            stats.deleted, stats.errors
        );
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn format_time_ms(ms: i64) -> String {
    use chrono::{Local, TimeZone};
    if let chrono::LocalResult::Single(dt) = Local.timestamp_millis_opt(ms) {
        dt.format("%Y-%m-%d %H:%M:%S").to_string()
    } else {
        "Unknown".to_string()
    }
}

fn update_repos(
    store: &Store,
    names: Vec<String>,
    all: bool,
    threads: usize,
) -> anyhow::Result<()> {
    let names: Vec<String> = if all {
        store
            .list_repos()?
            .into_iter()
            .map(|(name, _, _)| name)
            .collect()
    } else {
        names
    };
    if names.is_empty() {
        anyhow::bail!("No repositories registered. Use 'dedup repo create <name> <path>' first.");
    }

    let cancel = CancellationToken::new();
    {
        let cancel = cancel.clone();
        ctrlc::set_handler(move || cancel.cancel())?;
    }

    for name in names {
        println!("Updating '{}'...", name);
        let progress = TerminalProgress::new();
        let stats = update_repo(store, &name, threads, &progress, &cancel)?;
        progress.finish();
        println!(
            "  added: {}, updated: {}, unchanged: {}, missing: {}, errors: {}, hashed: {}",
            stats.added,
            stats.updated,
            stats.unchanged,
            stats.marked_missing,
            stats.errors,
            format_size(stats.hashed_bytes)
        );
        if stats.cancelled {
            println!("Update cancelled by user.");
            break;
        }
    }
    Ok(())
}

/// Renders core progress events as an indicatif bar: a spinner while
/// scanning, a progress bar once hashing starts.
struct TerminalProgress {
    bar: indicatif::ProgressBar,
}

impl TerminalProgress {
    fn new() -> Self {
        let bar = indicatif::ProgressBar::new_spinner();
        bar.enable_steady_tick(std::time::Duration::from_millis(100));
        Self { bar }
    }

    fn finish(&self) {
        self.bar.finish_and_clear();
    }
}

impl Progress for TerminalProgress {
    fn on(&self, event: ProgressEvent) {
        match event {
            ProgressEvent::Scanning { files, dirs } => {
                self.bar
                    .set_message(format!("Scanning: {} files, {} dirs", files, dirs));
            }
            ProgressEvent::Hashing {
                done,
                total,
                current,
                ..
            } => {
                if self.bar.length() != Some(total) {
                    self.bar.set_style(
                        indicatif::ProgressStyle::with_template("{wide_bar} {pos}/{len} {msg}")
                            .unwrap_or_else(|_| indicatif::ProgressStyle::default_bar()),
                    );
                    self.bar.set_length(total);
                }
                self.bar.set_position(done);
                self.bar.set_message(current);
            }
            ProgressEvent::Error { path, message } => {
                self.bar.println(format!("Error: {}: {}", path, message));
            }
            ProgressEvent::Finished { .. } => {}
        }
    }
}

fn truncate_path(path: &str, max_len: usize) -> String {
    let char_count = path.chars().count();
    if char_count <= max_len {
        return path.to_string();
    }
    let left = max_len / 2 - 2;
    let right = max_len - left - 3;
    let head: String = path.chars().take(left).collect();
    let tail: String = path.chars().skip(char_count - right).collect();
    format!("{}...{}", head, tail)
}

fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    const TB: u64 = GB * 1024;

    if bytes >= TB {
        format!("{:.2} TB", bytes as f64 / TB as f64)
    } else if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

fn format_time(timestamp: u64) -> String {
    use chrono::{Local, TimeZone};
    if let chrono::LocalResult::Single(dt) = Local.timestamp_opt(timestamp as i64, 0) {
        dt.format("%Y-%m-%d %H:%M:%S").to_string()
    } else {
        "Unknown".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_path_keeps_short_paths() {
        assert_eq!(truncate_path("/tmp/short", 30), "/tmp/short");
    }

    #[test]
    fn truncate_path_shortens_long_paths() {
        let truncated = truncate_path("/very/long/path/that/exceeds/the/limit", 30);
        assert_eq!(truncated.chars().count(), 30);
        assert!(truncated.contains("..."));
    }

    #[test]
    fn truncate_path_handles_multibyte_chars_on_boundaries() {
        // Byte-index slicing panicked here: byte 22 is inside 'Ä'.
        let truncated = truncate_path("/tmp/x/ÜrlaubsfötosÄÖfen-Übermu", 30);
        assert!(truncated.contains("..."));
    }
}
