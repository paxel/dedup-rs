use clap::{Parser, Subcommand};
use dedup_core::store::Store;
use dedup_core::update::{CancellationToken, Progress, ProgressEvent, update_repo};

#[derive(Parser)]
#[command(name = "dedup")]
#[command(version = env!("CARGO_PKG_VERSION"))]
#[command(about = "File deduplication tool in Rust", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Repository management commands
    Repo {
        #[command(subcommand)]
        command: RepoCommands,
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
                RepoCommands::Update {
                    names,
                    all,
                    threads,
                } => {
                    update_repos(&store, names, all, threads)?;
                }
            }
        }
        None => {
            println!("Starting GUI...");
            if let Err(e) = dedup_gui::run() {
                eprintln!("GUI Error: {}", e);
                std::process::exit(1);
            }
        }
    }

    Ok(())
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
