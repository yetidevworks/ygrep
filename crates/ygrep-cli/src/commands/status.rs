use anyhow::{Context, Result};
use std::path::Path;
use ygrep_core::Workspace;

pub fn run(workspace_path: &Path, detailed: bool) -> Result<()> {
    // Open workspace
    let workspace = Workspace::open(workspace_path)
        .context("Failed to open workspace")?;

    println!("ygrep status");
    println!("============");
    println!();
    println!("Workspace: {}", workspace.root().display());
    println!("Index path: {}", workspace.index_path().display());
    println!("Indexed: {}", if workspace.is_indexed() { "yes" } else { "no" });

    if detailed && workspace.is_indexed() {
        println!();
        println!("Index details:");
        // TODO: Add more detailed stats from index
        println!("  (detailed stats coming in future version)");
    }

    Ok(())
}
