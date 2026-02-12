use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod commands;
mod output;

#[derive(Parser)]
#[command(name = "ygrep")]
#[command(about = "Fast indexed code search with optional semantic search")]
#[command(
    long_about = "ygrep - Fast indexed code search with optional semantic search\n\n\
Uses literal text matching by default. Special characters work:\n\
  $variable, ->get(, {% block, @decorator\n\n\
Subtoken matching: camelCase and snake_case identifiers are split into\n\
subtokens, so searching \"send\" also finds sendCampaign, send_email, etc.\n\n\
Multi-word queries: \"campaign sending\" returns results where ALL terms\n\
appear in the file (AND logic), not just exact adjacent phrases.\n\n\
Use -r/--regex for regex patterns: ygrep \"fn\\\\s+main\" -r\n\
Use -s/--case-sensitive for exact case matching (default: case-insensitive)\n\
Use -A/-B/-K to control context lines around matches\n\n\
Output formats:\n\
  (default)  AI-optimized: path:line (score%) with match indicators\n\
  --json     Full JSON with metadata\n\
  --pretty   Human-readable with line numbers and context\n\n\
Match indicators in default output:\n\
  +  hybrid match (text AND semantic)\n\
  ~  semantic only (conceptual match)\n\
  (none) text match only"
)]
#[command(version)]
#[command(after_help = "EXAMPLES:\n\
    ygrep index                     Index current directory (text-only)\n\
    ygrep index --semantic          Index with semantic search (slower)\n\
    ygrep \"search query\"            Search with default AI output\n\
    ygrep \"fn main\" -n 10           Limit to 10 results\n\
    ygrep \"->get(\" -e php           Search PHP files only\n\
    ygrep \"fn\\\\s+main\" -r            Regex search\n\
    ygrep \"Config\" -s               Case-sensitive search\n\
    ygrep \"error\" -A 3              Show 3 lines after each match\n\
    ygrep \"error\" -B 2              Show 2 lines before each match\n\
    ygrep \"error\" -K 3              Show 3 lines before and after\n\
    ygrep search \"api\" --json       JSON output\n\
    ygrep install claude-code       Install for Claude Code\n\n\
For more info: https://github.com/yetidevworks/ygrep")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,

    /// Search query (shorthand for `ygrep search <QUERY>`)
    pub query: Option<String>,

    /// Maximum results
    #[arg(short = 'n', long, default_value = "100")]
    pub limit: usize,

    /// Workspace root (default: current directory)
    #[arg(short = 'C', long, global = true)]
    pub workspace: Option<PathBuf>,

    /// Output as JSON
    #[arg(long, global = true, conflicts_with = "pretty")]
    pub json: bool,

    /// Output in human-readable format (more context)
    #[arg(long, global = true, conflicts_with = "json")]
    pub pretty: bool,

    /// Verbose output
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// Treat query as regex pattern
    #[arg(short = 'r', long)]
    pub regex: bool,

    /// Filter by file extension (e.g., -e rs -e ts)
    #[arg(short = 'e', long = "ext")]
    pub extensions: Vec<String>,

    /// Filter by path pattern
    #[arg(short = 'p', long = "path")]
    pub paths: Vec<String>,

    /// Text-only search (disable semantic search)
    #[arg(long)]
    pub text_only: bool,

    /// Case-sensitive search (default is case-insensitive)
    #[arg(short = 's', long)]
    pub case_sensitive: bool,

    /// Lines of context after each match
    #[arg(short = 'A', long)]
    pub after_context: Option<usize>,

    /// Lines of context before each match
    #[arg(short = 'B', long)]
    pub before_context: Option<usize>,

    /// Lines of context before and after each match
    #[arg(short = 'K', long)]
    pub context: Option<usize>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Search indexed codebase (literal matching by default, like grep)
    Search {
        /// Search query (literal text or regex with --regex)
        query: String,

        /// Maximum results
        #[arg(short = 'n', long, default_value = "100")]
        limit: usize,

        /// Filter by file extension (e.g., -e rs -e ts)
        #[arg(short = 'e', long = "ext")]
        extensions: Vec<String>,

        /// Filter by path pattern
        #[arg(short = 'p', long = "path")]
        paths: Vec<String>,

        /// Treat query as regex pattern instead of literal text
        #[arg(short = 'r', long)]
        regex: bool,

        /// Show relevance scores
        #[arg(long)]
        scores: bool,

        /// Text-only search (disable semantic search)
        #[arg(long)]
        text_only: bool,

        /// Case-sensitive search (default is case-insensitive)
        #[arg(short = 's', long)]
        case_sensitive: bool,

        /// Lines of context after each match
        #[arg(short = 'A', long)]
        after_context: Option<usize>,

        /// Lines of context before each match
        #[arg(short = 'B', long)]
        before_context: Option<usize>,

        /// Lines of context before and after each match
        #[arg(short = 'K', long)]
        context: Option<usize>,
    },

    /// Build search index for a workspace (run before searching)
    Index {
        /// Workspace path (default: current directory)
        path: Option<PathBuf>,

        /// Force complete rebuild (clears existing index)
        #[arg(long)]
        rebuild: bool,

        /// Build semantic index for natural language queries (slower, ~25MB model)
        #[arg(long, conflicts_with = "text")]
        semantic: bool,

        /// Build text-only index (fast, default). Converts semantic to text-only.
        #[arg(long, conflicts_with = "semantic")]
        text: bool,
    },

    /// Show index status for current workspace
    Status {
        /// Show detailed statistics
        #[arg(long)]
        detailed: bool,
    },

    /// Watch for file changes and update index automatically
    Watch {
        /// Workspace path (default: current directory)
        path: Option<PathBuf>,
    },

    /// Install ygrep integration for AI coding tools
    #[command(subcommand)]
    Install(InstallTarget),

    /// Remove ygrep integration from AI coding tools
    #[command(subcommand)]
    Uninstall(InstallTarget),

    /// Manage stored indexes (list, clean, remove)
    #[command(subcommand)]
    Indexes(IndexesCommand),

    /// Start MCP server (stdio transport) for AI assistant integration
    #[cfg(feature = "mcp")]
    Mcp {
        /// Enable semantic search (downloads embedding model on first use)
        #[arg(long)]
        semantic: bool,
    },
}

#[derive(Subcommand, Clone)]
pub enum IndexesCommand {
    /// List all indexes with size and type (text/semantic)
    List,
    /// Remove orphaned indexes for workspaces that no longer exist
    Clean,
    /// Remove a specific index by hash or workspace path
    Remove {
        /// Index hash (from `ygrep indexes list`) or workspace path
        identifier: String,
    },
}

#[derive(Subcommand, Clone)]
pub enum InstallTarget {
    /// Claude Code - Installs plugin with skill and auto-index hook
    ClaudeCode,
    /// OpenCode - Installs skill to ~/.config/opencode/skills/ygrep/
    Opencode,
    /// Codex - Installs skill to ~/.agents/skills/ygrep/
    Codex,
    /// MCP - Print MCP server configuration for Claude Desktop and other MCP clients
    #[cfg(feature = "mcp")]
    Mcp {
        /// Include --semantic flag in generated config (enables semantic search)
        #[arg(long)]
        semantic: bool,
    },
}

/// Output format determined by --json or --pretty flags
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum OutputFormat {
    /// AI-optimized minimal output (default)
    #[default]
    Ai,
    /// JSON output
    Json,
    /// Human-readable formatted output
    Pretty,
}

impl OutputFormat {
    pub fn from_flags(json: bool, pretty: bool) -> Self {
        if json {
            OutputFormat::Json
        } else if pretty {
            OutputFormat::Pretty
        } else {
            OutputFormat::Ai
        }
    }
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
    let workspace = cli
        .workspace
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    // Determine output format from flags
    let format = OutputFormat::from_flags(cli.json, cli.pretty);

    // Handle command
    match cli.command {
        Some(Commands::Search {
            query,
            limit,
            extensions,
            paths,
            regex,
            scores,
            text_only,
            case_sensitive,
            after_context,
            before_context,
            context,
        }) => {
            let ctx_before = context.or(before_context);
            let ctx_after = context.or(after_context);
            commands::search::run(
                &workspace,
                &query,
                limit,
                extensions,
                paths,
                regex,
                scores,
                text_only,
                case_sensitive,
                ctx_before,
                ctx_after,
                format,
            )?;
        }
        Some(Commands::Index {
            path,
            rebuild,
            semantic,
            text,
        }) => {
            let target = path.unwrap_or(workspace);
            commands::index::run(&target, rebuild, semantic, text)?;
        }
        Some(Commands::Status { detailed }) => {
            commands::status::run(&workspace, detailed)?;
        }
        Some(Commands::Watch { path }) => {
            let target = path.unwrap_or(workspace);
            commands::watch::run(&target)?;
        }
        Some(Commands::Install(target)) => match target {
            InstallTarget::ClaudeCode => commands::install::install_claude_code()?,
            InstallTarget::Opencode => commands::install::install_opencode()?,
            InstallTarget::Codex => commands::install::install_codex()?,
            #[cfg(feature = "mcp")]
            InstallTarget::Mcp { semantic } => {
                commands::install::install_mcp(&workspace, semantic)?
            }
        },
        Some(Commands::Uninstall(target)) => match target {
            InstallTarget::ClaudeCode => commands::install::uninstall_claude_code()?,
            InstallTarget::Opencode => commands::install::uninstall_opencode()?,
            InstallTarget::Codex => commands::install::uninstall_codex()?,
            #[cfg(feature = "mcp")]
            InstallTarget::Mcp { .. } => commands::install::uninstall_mcp()?,
        },
        Some(Commands::Indexes(cmd)) => match cmd {
            IndexesCommand::List => commands::indexes::list()?,
            IndexesCommand::Clean => commands::indexes::clean()?,
            IndexesCommand::Remove { identifier } => commands::indexes::remove(&identifier)?,
        },
        #[cfg(feature = "mcp")]
        Some(Commands::Mcp { semantic }) => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(commands::mcp::run(&workspace, semantic))?;
        }
        None => {
            // Default: treat as search if query provided
            if let Some(query) = cli.query {
                let ctx_before = cli.context.or(cli.before_context);
                let ctx_after = cli.context.or(cli.after_context);
                commands::search::run(
                    &workspace,
                    &query,
                    cli.limit,
                    cli.extensions,
                    cli.paths,
                    cli.regex,
                    false,
                    cli.text_only,
                    cli.case_sensitive,
                    ctx_before,
                    ctx_after,
                    format,
                )?;
            } else {
                // No query, show help
                use clap::CommandFactory;
                Cli::command().print_help()?;
                println!();
            }
        }
    }

    Ok(())
}
