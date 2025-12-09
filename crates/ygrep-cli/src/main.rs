use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod commands;
mod output;

#[derive(Parser)]
#[command(name = "ygrep")]
#[command(about = "AI-optimized semantic code search", long_about = None)]
#[command(version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,

    /// Search query (shorthand for `ygrep search <QUERY>`)
    #[arg(trailing_var_arg = true, num_args = 0..)]
    pub query: Vec<String>,

    /// Workspace root (default: current directory)
    #[arg(short = 'C', long, global = true)]
    pub workspace: Option<PathBuf>,

    /// Output format: ai, json, pretty
    #[arg(short, long, default_value = "ai", global = true)]
    pub format: OutputFormat,

    /// Verbose output
    #[arg(short, long, global = true)]
    pub verbose: bool,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Search the codebase (default command)
    Search {
        /// Search query
        query: String,

        /// Maximum results
        #[arg(short = 'n', long, default_value = "10")]
        limit: usize,

        /// Filter by file extension (e.g., -e rs -e ts)
        #[arg(short = 'e', long = "ext")]
        extensions: Vec<String>,

        /// Filter by path pattern
        #[arg(short = 'p', long = "path")]
        paths: Vec<String>,

        /// Show relevance scores
        #[arg(long)]
        scores: bool,

        /// Text-only search (disable semantic/vector search)
        #[arg(long)]
        text_only: bool,
    },

    /// Index a workspace
    Index {
        /// Workspace path (default: current directory)
        path: Option<PathBuf>,

        /// Force complete rebuild
        #[arg(long)]
        rebuild: bool,

        /// Generate embeddings for semantic search (slower, but better results)
        #[arg(long)]
        embeddings: bool,
    },

    /// Show index and daemon status
    Status {
        /// Show detailed statistics
        #[arg(long)]
        detailed: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat {
    /// AI-optimized minimal output (default)
    Ai,
    /// JSON output
    Json,
    /// Human-readable formatted output
    Pretty,
}

fn main() -> Result<()> {
    // Initialize logging
    let filter = if std::env::var("YGREP_DEBUG").is_ok() {
        "debug"
    } else {
        "warn"
    };

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    // Determine workspace
    let workspace = cli.workspace.clone().unwrap_or_else(|| {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    });

    // Handle command
    match cli.command {
        Some(Commands::Search { query, limit, extensions, paths, scores, text_only }) => {
            commands::search::run(&workspace, &query, limit, extensions, paths, scores, text_only, cli.format)?;
        }
        Some(Commands::Index { path, rebuild, embeddings }) => {
            let target = path.unwrap_or(workspace);
            commands::index::run(&target, rebuild, embeddings)?;
        }
        Some(Commands::Status { detailed }) => {
            commands::status::run(&workspace, detailed)?;
        }
        None => {
            // Default: treat trailing args as search query
            if cli.query.is_empty() {
                // No query, show help
                use clap::CommandFactory;
                Cli::command().print_help()?;
                println!();
            } else {
                let query = cli.query.join(" ");
                commands::search::run(&workspace, &query, 10, vec![], vec![], false, false, cli.format)?;
            }
        }
    }

    Ok(())
}
