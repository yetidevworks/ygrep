use anyhow::Result;
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    schemars, tool, tool_handler, tool_router, ServerHandler,
    transport::stdio,
    ServiceExt,
};
use serde::Deserialize;
use std::path::Path;
use ygrep_core::{WatchEvent, Workspace};

/// MCP server wrapping the ygrep Workspace API.
pub struct YgrepMcp {
    workspace: Workspace,
    tool_router: ToolRouter<Self>,
}

impl std::fmt::Debug for YgrepMcp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("YgrepMcp")
            .field("root", &self.workspace.root())
            .finish()
    }
}

// --- Tool parameter structs ---

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchParams {
    #[schemars(description = "Search query (literal text or regex pattern)")]
    pub query: String,

    #[schemars(description = "Maximum results to return (default: 50)")]
    pub limit: Option<u32>,

    #[schemars(description = "Filter by file extensions (e.g. [\"rs\", \"ts\"])")]
    pub extensions: Option<Vec<String>>,

    #[schemars(description = "Filter by path patterns (e.g. [\"src/\", \"tests/\"])")]
    pub paths: Option<Vec<String>>,

    #[schemars(description = "Treat query as a regex pattern (default: false)")]
    pub regex: Option<bool>,

    #[schemars(description = "Case-sensitive search (default: false = case-insensitive)")]
    pub case_sensitive: Option<bool>,

    #[schemars(description = "Disable semantic search, use text-only (default: false)")]
    pub text_only: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct IndexParams {
    #[schemars(description = "Force a full rebuild of the index (default: false = incremental)")]
    pub rebuild: Option<bool>,

    #[schemars(description = "Enable semantic/embedding index (default: false = text-only)")]
    pub semantic: Option<bool>,
}

// --- Tool implementations ---

#[tool_router]
impl YgrepMcp {
    fn new(workspace: Workspace) -> Self {
        Self {
            workspace,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Search indexed codebase using fast full-text search with optional semantic search. Returns file paths, line numbers, and code snippets. Automatically indexes the workspace on first use."
    )]
    async fn ygrep_search(
        &self,
        Parameters(params): Parameters<SearchParams>,
    ) -> Result<CallToolResult, ErrorData> {
        // Auto-index if workspace isn't indexed yet, or rebuild if schema is outdated
        if self.workspace.needs_schema_rebuild() {
            let index_path = self.workspace.index_path().to_path_buf();
            if index_path.exists() {
                std::fs::remove_dir_all(&index_path)
                    .map_err(|e| ErrorData::internal_error(format!("Failed to clear stale index: {}", e), None))?;
            }
            // Re-open workspace after clearing index and do full rebuild
            let ws = Workspace::create(self.workspace.root())
                .map_err(|e| ErrorData::internal_error(format!("Failed to reopen workspace: {}", e), None))?;
            ws.index_all_with_options(false)
                .map_err(|e| ErrorData::internal_error(format!("Schema rebuild failed: {}", e), None))?;
        } else if !self.workspace.is_indexed() {
            self.workspace
                .index_incremental_with_options(false)
                .map_err(|e| ErrorData::internal_error(format!("Auto-index failed: {}", e), None))?;
        }

        let limit = params.limit.unwrap_or(50) as usize;
        let use_regex = params.regex.unwrap_or(false);
        let case_sensitive = params.case_sensitive.unwrap_or(false);
        let text_only = params.text_only.unwrap_or(false);

        let ext_filter = params.extensions.filter(|v: &Vec<String>| !v.is_empty());
        let path_filter = params.paths.filter(|v: &Vec<String>| !v.is_empty());

        // Use hybrid search if semantic index is available and not disabled
        #[cfg(feature = "embeddings")]
        let use_hybrid = !text_only && !use_regex && self.workspace.has_semantic_index();
        #[cfg(not(feature = "embeddings"))]
        let use_hybrid = false;

        let result = if use_hybrid {
            #[cfg(feature = "embeddings")]
            {
                self.workspace
                    .search_hybrid(&params.query, Some(limit))
                    .map_err(|e| ErrorData::internal_error(format!("Search failed: {}", e), None))?
            }
            #[cfg(not(feature = "embeddings"))]
            unreachable!()
        } else {
            self.workspace
                .search_filtered(
                    &params.query,
                    Some(limit),
                    ext_filter,
                    path_filter,
                    use_regex,
                    case_sensitive,
                    None,
                    None,
                )
                .map_err(|e| ErrorData::internal_error(format!("Search failed: {}", e), None))?
        };

        let output = result.format_ai();
        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    #[tool(
        description = "Build or update the search index for the workspace. Incremental by default (only re-indexes changed files). Use rebuild=true to force a full re-index."
    )]
    async fn ygrep_index(
        &self,
        Parameters(params): Parameters<IndexParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let rebuild = params.rebuild.unwrap_or(false);
        let semantic = params.semantic.unwrap_or(false);

        let stats = if rebuild {
            self.workspace
                .index_all_with_options(semantic)
                .map_err(|e| ErrorData::internal_error(format!("Index failed: {}", e), None))?
        } else {
            self.workspace
                .index_incremental_with_options(semantic)
                .map_err(|e| ErrorData::internal_error(format!("Index failed: {}", e), None))?
        };

        let mode = if rebuild { "rebuild" } else { "incremental" };
        let semantic_str = if semantic { " (semantic)" } else { "" };
        let output = format!(
            "Index {}{}: {} indexed, {} unchanged, {} removed, {} skipped, {} errors",
            mode, semantic_str, stats.indexed, stats.unchanged, stats.removed, stats.skipped, stats.errors
        );

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    #[tool(description = "Show workspace index status: root path, index state, semantic availability.")]
    async fn ygrep_status(&self) -> Result<CallToolResult, ErrorData> {
        let root = self.workspace.root().display().to_string();
        let indexed = self.workspace.is_indexed();
        let index_path = self.workspace.index_path().display().to_string();

        let index_type = match self.workspace.stored_semantic_flag() {
            Some(true) => "semantic",
            Some(false) => "text",
            None if indexed => "text (legacy)",
            None => "none",
        };

        let has_semantic = self.workspace.has_semantic_index();

        let output = format!(
            "Workspace: {}\nIndexed: {}\nIndex path: {}\nIndex type: {}\nSemantic search: {}",
            root,
            if indexed { "yes" } else { "no" },
            index_path,
            index_type,
            if has_semantic { "available" } else { "not available" },
        );

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }
}

// --- ServerHandler ---

#[tool_handler]
impl ServerHandler for YgrepMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::V_2024_11_05,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: "ygrep".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                ..Default::default()
            },
            instructions: Some(
                "ygrep is a fast indexed code search tool. Use ygrep_search to find code \
                 by keyword, identifier, or regex. The workspace is automatically indexed \
                 on first search. Use ygrep_status to check index state."
                    .into(),
            ),
        }
    }
}

// --- Entry point ---

pub async fn run(workspace_path: &Path, semantic: bool) -> Result<()> {
    let mut workspace = Workspace::create(workspace_path)?;

    // Use --semantic flag, or preserve existing semantic setting
    let use_semantic = semantic || workspace.stored_semantic_flag().unwrap_or(false);

    // Rebuild index if schema version is outdated
    if workspace.needs_schema_rebuild() {
        eprintln!("Schema version changed, rebuilding index...");
        let index_path = workspace.index_path().to_path_buf();
        drop(workspace);
        if index_path.exists() {
            std::fs::remove_dir_all(&index_path)?;
            eprintln!("  Cleared old index at {}", index_path.display());
        }
        // Re-create workspace and do full rebuild
        let workspace_rebuilt = Workspace::create(workspace_path)?;
        let stats = workspace_rebuilt.index_all_with_options(use_semantic)?;
        eprintln!(
            "  Indexed {} files ({} skipped, {} errors)",
            stats.indexed, stats.skipped, stats.errors
        );
        workspace = workspace_rebuilt;
    } else if !workspace.is_indexed() {
        eprintln!("Auto-indexing workspace...");
        let stats = workspace.index_incremental_with_options(use_semantic)?;
        eprintln!(
            "Indexed {} files ({} skipped, {} errors)",
            stats.indexed, stats.skipped, stats.errors
        );
    } else if semantic && !workspace.has_semantic_index() {
        eprintln!("Rebuilding index with semantic search enabled...");
        let stats = workspace.index_all_with_options(true)?;
        eprintln!(
            "Indexed {} files ({} skipped, {} errors)",
            stats.indexed, stats.skipped, stats.errors
        );
    }

    // Start file watcher in background
    let mut watcher = workspace.create_watcher()?;
    watcher.start()?;

    // Spawn watcher event loop
    let watcher_workspace_root = workspace.root().to_path_buf();
    let watcher_handle = tokio::spawn(async move {
        // Open a separate workspace handle for the watcher task
        let watcher_ws = match Workspace::open(&watcher_workspace_root) {
            Ok(ws) => ws,
            Err(e) => {
                eprintln!("Watcher: failed to open workspace: {}", e);
                return;
            }
        };
        loop {
            match watcher.next_event().await {
                Some(WatchEvent::Changed(path)) => {
                    if let Err(e) = watcher_ws.index_file_with_options(&path, use_semantic) {
                        tracing::debug!("Watcher index error for {}: {}", path.display(), e);
                    }
                }
                Some(WatchEvent::Deleted(path)) => {
                    if let Err(e) = watcher_ws.delete_file(&path) {
                        tracing::debug!("Watcher delete error for {}: {}", path.display(), e);
                    }
                }
                Some(WatchEvent::Error(e)) => {
                    tracing::debug!("Watcher error: {}", e);
                }
                Some(_) => {} // DirCreated, DirDeleted — no action needed
                None => break,
            }
        }
    });

    // Serve MCP over stdio
    let server = YgrepMcp::new(workspace);
    let peer = server.serve(stdio()).await?;
    peer.waiting().await?;

    // Clean up watcher
    watcher_handle.abort();

    Ok(())
}
