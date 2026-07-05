use clap::{Parser, Subcommand};

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
    /// Placeholder subcommand for Phase 0
    Placeholder,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Commands::Placeholder) => {
            println!("Placeholder subcommand");
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
