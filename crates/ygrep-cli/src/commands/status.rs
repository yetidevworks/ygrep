use anyhow::Result;
use std::path::Path;
use ygrep_core::{Config, Workspace, YgrepError};

use crate::commands::progress;

pub fn run(workspace_path: &Path, detailed: bool) -> Result<()> {
    println!("ygrep status");
    println!("============");
    println!();
    println!("Workspace: {}", workspace_path.display());

    // Try to open workspace read-only — status never modifies the index
    match Workspace::open_readonly(workspace_path) {
        Ok(workspace) => {
            println!("Index path: {}", workspace.index_path().display());
            println!("Indexed: yes");

            if let Some(when) = progress::indexed_at(workspace.index_path()) {
                println!("Last indexed: {}", when.to_rfc3339());
            }
            if let Some(note) = progress::staleness_note(workspace.index_path()) {
                println!("{}", note);
            }

            // Show index type
            let index_type = match workspace.stored_semantic_flag() {
                Some(true) => "semantic",
                Some(false) => "text",
                None => "text (legacy)",
            };
            println!("Index type: {}", index_type);

            // Show semantic index availability
            #[cfg(feature = "embeddings")]
            if workspace.has_semantic_index() {
                println!("Semantic search: available");
            }

            if detailed {
                println!();
                println!("Index details:");
                // TODO: Add more detailed stats from index
                println!("  (detailed stats coming in future version)");
            }
        }
        Err(YgrepError::WorkspaceNotIndexed(_)) => {
            // A build may be running right now, e.g. started by an editor session hook.
            if let Some(running) = Workspace::resolve_index_path(workspace_path, &Config::load())
                .ok()
                .as_deref()
                .and_then(progress::indexing_in_progress)
            {
                println!(
                    "Indexed: building now (running {})",
                    progress::format_duration(running.elapsed())
                );
                return Ok(());
            }

            println!("Indexed: no");
            println!();
            println!("To index this workspace, run:");
            println!("  ygrep index              # Text-only (fast)");
            println!("  ygrep index --semantic   # With semantic search");
        }
        Err(e) => {
            println!("Indexed: yes (but the index could not be opened)");
            println!();
            println!("Error: {}", e);
        }
    }

    Ok(())
}
