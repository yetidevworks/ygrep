use anyhow::{Context, Result};
use std::path::Path;
use ygrep_core::{WatchEvent, Workspace};

pub fn run(workspace_path: &Path) -> Result<()> {
    eprintln!("Opening workspace {}...", workspace_path.display());

    // Open existing workspace (fails if not indexed)
    let workspace = match Workspace::open(workspace_path) {
        Ok(ws) => ws,
        Err(_) => {
            eprintln!("Workspace not indexed: {}", workspace_path.display());
            eprintln!();
            eprintln!("To watch this workspace, first index it:");
            eprintln!("  ygrep index              # Text-only (fast)");
            eprintln!("  ygrep index --semantic   # With semantic search (slower, better results)");
            std::process::exit(1);
        }
    };

    // Read the stored semantic flag
    let use_semantic = workspace.stored_semantic_flag().unwrap_or(false);

    let mode = if use_semantic { "semantic" } else { "text" };
    eprintln!("Starting file watcher (mode: {})...", mode);
    eprintln!("Press Ctrl+C to stop.\n");

    let mut watcher = workspace
        .create_watcher()
        .context("Failed to create file watcher")?;

    watcher.start().context("Failed to start file watcher")?;

    // Create tokio runtime for async event handling
    let rt = tokio::runtime::Runtime::new().context("Failed to create async runtime")?;

    rt.block_on(async {
        let mut changed_count = 0u64;
        let mut deleted_count = 0u64;
        let mut error_count = 0u64;

        loop {
            match watcher.next_event().await {
                Some(WatchEvent::Changed(path)) => {
                    // Check if it's a text file we should index
                    if is_indexable(&path) {
                        match workspace.index_file_with_options(&path, use_semantic) {
                            Ok(()) => {
                                changed_count += 1;
                                eprintln!("  [+] {}", path.display());
                            }
                            Err(e) => {
                                error_count += 1;
                                eprintln!("  [!] {} - {}", path.display(), e);
                            }
                        }
                    }
                }
                Some(WatchEvent::Deleted(path)) => {
                    match workspace.delete_file(&path) {
                        Ok(()) => {
                            deleted_count += 1;
                            eprintln!("  [-] {}", path.display());
                        }
                        Err(e) => {
                            // File might not have been in index, that's OK
                            tracing::debug!("Delete error for {}: {}", path.display(), e);
                        }
                    }
                }
                Some(WatchEvent::DirCreated(path)) => {
                    eprintln!("  [d] {} (new directory)", path.display());
                }
                Some(WatchEvent::DirDeleted(path)) => {
                    eprintln!("  [d] {} (directory removed)", path.display());
                }
                Some(WatchEvent::Error(e)) => {
                    error_count += 1;
                    eprintln!("  [!] Watch error: {}", e);
                }
                None => {
                    // Channel closed, exit
                    break;
                }
            }

            // Print periodic stats
            if (changed_count + deleted_count) % 100 == 0 && (changed_count + deleted_count) > 0 {
                eprintln!(
                    "\n--- Stats: {} indexed, {} deleted, {} errors ---\n",
                    changed_count, deleted_count, error_count
                );
            }
        }

        eprintln!(
            "\nWatch stopped. {} indexed, {} deleted, {} errors.",
            changed_count, deleted_count, error_count
        );
    });

    Ok(())
}

/// Check if a file should be indexed (simple extension check)
fn is_indexable(path: &Path) -> bool {
    const TEXT_EXTENSIONS: &[&str] = &[
        "rs",
        "py",
        "js",
        "ts",
        "jsx",
        "tsx",
        "mjs",
        "mts",
        "cjs",
        "cts",
        "go",
        "rb",
        "php",
        "java",
        "c",
        "cpp",
        "cc",
        "h",
        "hpp",
        "hh",
        "cs",
        "swift",
        "kt",
        "scala",
        "clj",
        "ex",
        "exs",
        "erl",
        "hs",
        "ml",
        "fs",
        "r",
        "jl",
        "lua",
        "pl",
        "pm",
        "sh",
        "bash",
        "zsh",
        "fish",
        "ps1",
        "bat",
        "cmd",
        "html",
        "htm",
        "css",
        "scss",
        "sass",
        "less",
        "xml",
        "json",
        "yaml",
        "yml",
        "toml",
        "twig",
        "blade",
        "ejs",
        "hbs",
        "handlebars",
        "mustache",
        "pug",
        "jade",
        "erb",
        "haml",
        "njk",
        "nunjucks",
        "jinja",
        "jinja2",
        "liquid",
        "eta",
        "md",
        "markdown",
        "rst",
        "txt",
        "csv",
        "sql",
        "graphql",
        "gql",
        "dockerfile",
        "makefile",
        "cmake",
        "gradle",
        "pom",
        "ini",
        "conf",
        "cfg",
        "vue",
        "svelte",
        "astro",
        "tf",
        "hcl",
        "nix",
        "proto",
        "thrift",
        "avsc",
        "gitignore",
        "gitattributes",
        "editorconfig",
        "env",
    ];

    if let Some(ext) = path.extension() {
        let ext_lower = ext.to_string_lossy().to_lowercase();
        TEXT_EXTENSIONS.contains(&ext_lower.as_str())
    } else {
        false
    }
}
