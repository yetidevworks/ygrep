//! ygrep-core - Core library for ygrep semantic code search
//!
//! This crate provides the core functionality for indexing and searching code:
//! - Tantivy-based full-text indexing
//! - File system walking with symlink handling
//! - BM25 text search + semantic vector search (with `embeddings` feature)
//! - Hybrid search with Reciprocal Rank Fusion
//! - Configuration management

pub mod config;
pub mod dashboard;
#[cfg(feature = "embeddings")]
pub mod embeddings;
pub mod error;
pub mod fs;
pub mod index;
pub mod search;
pub mod watcher;

pub use config::Config;
pub use error::{Result, YgrepError};
pub use watcher::{FileWatcher, WatchEvent};

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tantivy::Index;

#[cfg(feature = "embeddings")]
use embeddings::{EmbeddingCache, EmbeddingModel};
#[cfg(feature = "embeddings")]
use index::VectorIndex;
#[cfg(feature = "embeddings")]
use std::sync::Arc;

/// Embedding dimension for all-MiniLM-L6-v2
#[cfg(feature = "embeddings")]
const EMBEDDING_DIM: usize = 384;

/// Sender for routing log messages (e.g. to dashboard TUI instead of stderr)
pub type LogSender = std::sync::mpsc::Sender<String>;

/// High-level workspace for indexing and searching
pub struct Workspace {
    /// Workspace root directory
    root: std::path::PathBuf,
    /// Configuration
    config: Config,
    /// Tantivy index
    index: Index,
    /// Index directory path
    index_path: std::path::PathBuf,
    /// Optional log channel — messages go here instead of stderr when set
    log_tx: Option<LogSender>,
    /// Vector index for semantic search
    #[cfg(feature = "embeddings")]
    vector_index: Arc<VectorIndex>,
    /// Embedding model
    #[cfg(feature = "embeddings")]
    embedding_model: Arc<EmbeddingModel>,
    /// Embedding cache
    #[cfg(feature = "embeddings")]
    embedding_cache: Arc<EmbeddingCache>,
}

/// How the workspace index will be used, which determines what filesystem
/// access `open_internal` requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenMode {
    /// Create the index if it doesn't exist (indexing)
    Create,
    /// Open an existing index with write access (watch, incremental updates)
    Write,
    /// Open an existing index for search/status only. Never writes to the
    /// index directory, so it works when the directory is read-only (issue #12)
    ReadOnly,
}

impl Workspace {
    /// Open an existing workspace for updates (fails if not indexed)
    pub fn open(root: &Path) -> Result<Self> {
        let config = Config::load();
        Self::open_internal(root, config, OpenMode::Write)
    }

    /// Open an existing workspace for updates with custom config (fails if not indexed)
    pub fn open_with_config(root: &Path, config: Config) -> Result<Self> {
        Self::open_internal(root, config, OpenMode::Write)
    }

    /// Open an existing workspace read-only (fails if not indexed).
    /// Works when the index directory is readable but not writable, e.g. a
    /// sandboxed process consuming a centrally-maintained index (issue #12).
    pub fn open_readonly(root: &Path) -> Result<Self> {
        let config = Config::load();
        Self::open_internal(root, config, OpenMode::ReadOnly)
    }

    /// Open an existing workspace read-only with custom config (fails if not indexed)
    pub fn open_readonly_with_config(root: &Path, config: Config) -> Result<Self> {
        Self::open_internal(root, config, OpenMode::ReadOnly)
    }

    /// Create or open a workspace for indexing
    pub fn create(root: &Path) -> Result<Self> {
        let config = Config::load();
        Self::open_internal(root, config, OpenMode::Create)
    }

    /// Create or open a workspace with custom config for indexing
    pub fn create_with_config(root: &Path, config: Config) -> Result<Self> {
        Self::open_internal(root, config, OpenMode::Create)
    }

    /// Resolve the index directory for a workspace root without opening or creating it.
    ///
    /// Uses the same resolution as `open_internal`, so callers that need the index
    /// location before an index exists (progress reporting, auto-indexing) stay in step
    /// with where the index will actually be written.
    pub fn resolve_index_path(root: &Path, config: &Config) -> Result<PathBuf> {
        let root = std::fs::canonicalize(root)?;

        // Resolve data directory:
        // 1. Auto-detect: .ygrep/ directory in workspace root
        // 2. Relative data_dir in config: resolve against workspace root
        // 3. Absolute data_dir from config: use as-is
        let local_ygrep = root.join(".ygrep");
        let data_dir = if local_ygrep.is_dir() {
            local_ygrep
        } else if config.indexer.data_dir.is_relative() {
            root.join(&config.indexer.data_dir)
        } else {
            config.indexer.data_dir.clone()
        };

        Ok(data_dir.join("indexes").join(hash_path(&root)))
    }

    /// Open or create a workspace with custom config.
    /// Unless the mode is `Create`, returns an error if the index doesn't exist.
    fn open_internal(root: &Path, config: Config, mode: OpenMode) -> Result<Self> {
        let index_path = Self::resolve_index_path(root, &config)?;
        let root = std::fs::canonicalize(root)?;

        // Check if workspace has been properly indexed (workspace.json is written after indexing)
        let workspace_indexed = index_path.join("workspace.json").exists();
        // Check if Tantivy files exist (meta.json is created by Tantivy)
        let tantivy_exists = index_path.join("meta.json").exists();

        // If not creating and workspace not indexed, return error
        if mode != OpenMode::Create && !workspace_indexed {
            return Err(YgrepError::WorkspaceNotIndexed(root));
        }

        // Open or create Tantivy index
        let schema = index::build_document_schema();

        // Early check: verify index directory is writable (issue #7).
        // On macOS, the default data path (~/Library/Application Support/ygrep/) may not
        // be writable in sandboxed environments, causing cryptic lockfile PermissionDenied
        // errors.  Detect this upfront and suggest XDG_DATA_HOME.
        // Skipped for read-only opens, which never write to the index directory.
        if mode != OpenMode::ReadOnly && index_path.exists() {
            let probe = index_path.join(".ygrep-write-probe");
            match std::fs::write(&probe, b"") {
                Ok(()) => {
                    let _ = std::fs::remove_file(&probe);
                }
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    return Err(YgrepError::Config(format!(
                        "Index directory is not writable: {}\n\n\
                         Hint: Create a .ygrep/ directory in your project root for local indexes,\n\
                         or set YGREP_HOME to a writable location.",
                        index_path.display()
                    )));
                }
                Err(_) => {} // Other errors (e.g. disk full) — let Tantivy handle them
            }
        }

        // Clean up stale lockfiles that may block readers on macOS (issue #7).
        // Tantivy's reader acquires META_LOCK via flock(); on macOS Intel this can
        // fail with EPERM if a stale lockfile inode still has an unreleased flock.
        // Removing the file forces the next acquire_lock() to create a fresh inode.
        // Read-only opens use no-op locks instead, so stale lockfiles can't block them.
        if mode == OpenMode::Write {
            let _ = std::fs::remove_file(index_path.join(".tantivy-meta.lock"));
            let _ = std::fs::remove_file(index_path.join(".tantivy-writer.lock"));
        }

        let index = if tantivy_exists {
            if mode == OpenMode::ReadOnly {
                // Open through a directory wrapper that never takes filesystem
                // locks, so search works on a non-writable index directory
                let mmap = tantivy::directory::MmapDirectory::open(&index_path)
                    .map_err(tantivy::TantivyError::from)?;
                Index::open(index::ReadOnlyDirectory::new(mmap))?
            } else {
                match Index::open_in_dir(&index_path) {
                    Ok(idx) => idx,
                    Err(_e) if mode == OpenMode::Create => {
                        // Corrupt index detected, silently recreate
                        // Remove corrupted index and create fresh
                        std::fs::remove_dir_all(&index_path)?;
                        std::fs::create_dir_all(&index_path)?;
                        Index::create_in_dir(&index_path, schema)?
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        } else if mode == OpenMode::ReadOnly {
            return Err(YgrepError::Config(format!(
                "Index at {} is incomplete (missing Tantivy metadata).\n\
                 Re-run 'ygrep index' to rebuild it.",
                index_path.display()
            )));
        } else {
            // Create directory only when explicitly creating the index
            std::fs::create_dir_all(&index_path)?;
            Index::create_in_dir(&index_path, schema)?
        };

        // Register our custom code tokenizer
        index::register_tokenizers(index.tokenizers());

        #[cfg(feature = "embeddings")]
        let (vector_index, embedding_model, embedding_cache) = {
            // Create vector index path
            let vector_path = index_path.join("vectors");

            // Load or create vector index
            let vector_index = if VectorIndex::exists(&vector_path) {
                match VectorIndex::load(vector_path.clone()) {
                    Ok(vi) => Arc::new(vi),
                    Err(_e) if mode == OpenMode::ReadOnly => {
                        // Corrupt vector index, but we can't rewrite it —
                        // fall back to text-only search
                        Arc::new(VectorIndex::empty(vector_path, EMBEDDING_DIM))
                    }
                    Err(_e) => {
                        // Corrupt vector index detected, silently recreate
                        // Remove corrupted vector files and create fresh
                        if vector_path.exists() {
                            let _ = std::fs::remove_dir_all(&vector_path);
                        }
                        Arc::new(VectorIndex::new(vector_path, EMBEDDING_DIM)?)
                    }
                }
            } else if mode == OpenMode::ReadOnly {
                // No semantic index on disk; don't create its directory
                Arc::new(VectorIndex::empty(vector_path, EMBEDDING_DIM))
            } else {
                Arc::new(VectorIndex::new(vector_path, EMBEDDING_DIM)?)
            };

            // Create embedding model (lazy-loaded on first use)
            let embedding_model = Arc::new(EmbeddingModel::default()); // Uses all-MiniLM-L6-v2

            // Create embedding cache (100MB cache, 384 dimensions)
            let embedding_cache = Arc::new(EmbeddingCache::new(100, EMBEDDING_DIM));

            (vector_index, embedding_model, embedding_cache)
        };

        Ok(Self {
            root,
            config,
            index,
            index_path,
            log_tx: None,
            #[cfg(feature = "embeddings")]
            vector_index,
            #[cfg(feature = "embeddings")]
            embedding_model,
            #[cfg(feature = "embeddings")]
            embedding_cache,
        })
    }

    /// Set a log channel — all progress/warning output goes here instead of stderr
    pub fn set_log_tx(&mut self, tx: LogSender) {
        self.log_tx = Some(tx);
        #[cfg(feature = "embeddings")]
        self.embedding_model.set_quiet(true);
    }

    /// Route a message to the log channel or stderr
    fn log(&self, msg: impl std::fmt::Display) {
        if let Some(ref tx) = self.log_tx {
            let _ = tx.send(msg.to_string());
        } else {
            eprintln!("{}", msg);
        }
    }

    /// Route a partial-line message (no newline) to the log channel or stderr
    fn log_inline(&self, msg: impl std::fmt::Display) {
        if let Some(ref tx) = self.log_tx {
            let _ = tx.send(msg.to_string());
        } else {
            eprint!("{}", msg);
        }
    }

    /// Index all files in the workspace (text-only by default, fast)
    pub fn index_all(&self) -> Result<IndexStats> {
        self.index_all_with_options(false)
    }

    /// Index all files with options
    #[allow(unused_variables)]
    pub fn index_all_with_options(&self, with_embeddings: bool) -> Result<IndexStats> {
        // Clear vector index for fresh re-index
        #[cfg(feature = "embeddings")]
        self.vector_index.clear();

        // Phase 1: Index all files with BM25 (fast)
        let indexer =
            index::Indexer::new(self.config.indexer.clone(), self.index.clone(), &self.root)?;

        let mut walker = fs::FileWalker::new(self.root.clone(), self.config.indexer.clone())?;

        let mut indexed = 0;
        let mut skipped = 0;
        let mut errors = 0;

        // Collect content for batch embedding
        #[cfg(feature = "embeddings")]
        let mut embedding_batch: Vec<(String, String)> = Vec::new(); // (doc_id, content)
                                                                     // Larger batch size = more efficient SIMD/vectorization in ONNX Runtime
        #[cfg(feature = "embeddings")]
        const BATCH_SIZE: usize = 64;

        for entry in walker.walk() {
            match indexer.index_file_with_chunks(&entry.path, should_index_chunks(with_embeddings))
            {
                Ok((doc_id, content)) => {
                    indexed += 1;
                    if indexed % 500 == 0 {
                        self.log_inline(format!("\r  Indexed {} files...          ", indexed));
                    }

                    // Collect for embedding if enabled (reuse content from indexer)
                    #[cfg(feature = "embeddings")]
                    if with_embeddings {
                        embedding_batch.push((doc_id, content));
                    }
                    #[cfg(not(feature = "embeddings"))]
                    {
                        let _ = doc_id;
                        let _ = content;
                    }
                }
                Err(YgrepError::FileTooLarge { .. }) => {
                    skipped += 1;
                }
                Err(e) => {
                    tracing::debug!("Error indexing {}: {}", entry.path.display(), e);
                    errors += 1;
                }
            }
        }

        self.log(format!("\r  Indexed {} files.              ", indexed));
        indexer.commit()?;

        // Track embedded count
        #[cfg(feature = "embeddings")]
        let mut total_embedded = 0usize;
        #[cfg(not(feature = "embeddings"))]
        let total_embedded = 0usize;

        // Phase 2: Generate embeddings in batches (if enabled)
        #[cfg(feature = "embeddings")]
        if with_embeddings && !embedding_batch.is_empty() {
            // Filter out very short content (< 50 chars) and very long content (> 50KB)
            // These don't embed well or are too slow
            let filtered_batch: Vec<_> = embedding_batch
                .into_iter()
                .filter(|(_, content)| {
                    let len = content.len();
                    len >= 50 && len <= 50_000
                })
                .collect();

            if filtered_batch.is_empty() {
                self.log("No documents suitable for semantic indexing.");
            } else {
                use indicatif::{ProgressBar, ProgressStyle};

                let total_docs = filtered_batch.len() as u64;
                self.log(format!(
                    "Building semantic index for {} documents...",
                    total_docs
                ));

                // Pre-load the semantic model before starting progress bar
                self.embedding_model.preload()?;

                let pb = if self.log_tx.is_some() {
                    ProgressBar::hidden()
                } else {
                    ProgressBar::new(total_docs)
                };
                pb.set_style(
                    ProgressStyle::default_bar()
                        .template("  [{bar:40.cyan/blue}] {pos}/{len} ({percent}%)")
                        .unwrap()
                        .progress_chars("━╸─"),
                );
                pb.enable_steady_tick(std::time::Duration::from_millis(100));

                for chunk in filtered_batch.chunks(BATCH_SIZE) {
                    // Truncate to ~4KB for embedding - sufficient context for code, faster tokenization
                    // Use floor_char_boundary to avoid slicing in the middle of multi-byte UTF-8 characters
                    const EMBED_TRUNCATE: usize = 4096;
                    let texts: Vec<&str> = chunk
                        .iter()
                        .map(|(_, content)| {
                            if content.len() > EMBED_TRUNCATE {
                                let boundary = content.floor_char_boundary(EMBED_TRUNCATE);
                                &content[..boundary]
                            } else {
                                content.as_str()
                            }
                        })
                        .collect();

                    match self.embedding_model.embed_batch(&texts) {
                        Ok(embeddings) => {
                            for ((doc_id, _), embedding) in chunk.iter().zip(embeddings) {
                                if let Err(e) = self.vector_index.insert(doc_id, &embedding) {
                                    tracing::debug!(
                                        "Failed to insert embedding for {}: {}",
                                        doc_id,
                                        e
                                    );
                                }
                            }
                            total_embedded += chunk.len();
                            pb.set_position(total_embedded as u64);
                        }
                        Err(e) => {
                            tracing::warn!("Batch embedding failed: {}", e);
                            pb.inc(chunk.len() as u64);
                        }
                    }
                }

                pb.finish_and_clear();
                self.log(format!("  Indexed {} documents.", total_embedded));
                self.vector_index.save()?;
            }
        }

        #[cfg(not(feature = "embeddings"))]
        if with_embeddings {
            self.log("Warning: Semantic search feature not available in this build.");
        }

        let stats = walker.stats();

        // Save workspace metadata for index management
        let metadata = serde_json::json!({
            "workspace": self.root.to_string_lossy(),
            "indexed_at": chrono::Utc::now().to_rfc3339(),
            "files_indexed": indexed,
            "semantic": with_embeddings,
            "schema_version": index::SCHEMA_VERSION,
        });
        let metadata_path = self.index_path.join("workspace.json");
        if let Err(e) = std::fs::write(
            &metadata_path,
            serde_json::to_string_pretty(&metadata).unwrap_or_default(),
        ) {
            tracing::warn!("Failed to save workspace metadata: {}", e);
        }

        Ok(IndexStats {
            indexed,
            embedded: total_embedded,
            skipped,
            errors,
            unique_paths: stats.visited_paths,
            unchanged: 0,
            removed: 0,
        })
    }

    /// Build a map of all indexed files: relative_path -> (mtime, doc_id)
    /// Uses fast fields for efficient columnar reads, skipping chunk documents.
    /// Returns an empty map if the index is empty or unreadable.
    pub fn build_indexed_files_map(&self) -> HashMap<String, (u64, String)> {
        let mut map = HashMap::new();

        let reader = match self.index.reader() {
            Ok(r) => r,
            Err(_) => return map,
        };

        let searcher = reader.searcher();

        for segment_reader in searcher.segment_readers() {
            let alive_bitset = segment_reader.alive_bitset();
            let fast_fields = segment_reader.fast_fields();

            // Get fast field columns for path, mtime, chunk_id, doc_id
            let path_col = match fast_fields.str("path") {
                Ok(Some(col)) => col,
                _ => continue,
            };
            let mtime_col = match fast_fields.u64("mtime") {
                Ok(col) => col,
                Err(_) => continue,
            };
            let chunk_id_col = match fast_fields.str("chunk_id") {
                Ok(Some(col)) => col,
                _ => continue,
            };
            let doc_id_col = match fast_fields.str("doc_id") {
                Ok(Some(col)) => col,
                _ => continue,
            };

            let mut path_buf = String::new();
            let mut chunk_id_buf = String::new();
            let mut doc_id_buf = String::new();

            for row_id in 0..segment_reader.max_doc() {
                // Skip deleted docs
                if let Some(bitset) = &alive_bitset {
                    if !bitset.is_alive(row_id) {
                        continue;
                    }
                }

                // Skip chunk documents (chunk_id is non-empty for chunks)
                chunk_id_buf.clear();
                let mut is_chunk = false;
                for ord in chunk_id_col.term_ords(row_id) {
                    let _ = chunk_id_col.ord_to_str(ord, &mut chunk_id_buf);
                    if !chunk_id_buf.is_empty() {
                        is_chunk = true;
                        break;
                    }
                }
                if is_chunk {
                    continue;
                }

                // Read path
                path_buf.clear();
                for ord in path_col.term_ords(row_id) {
                    let _ = path_col.ord_to_str(ord, &mut path_buf);
                }
                if path_buf.is_empty() {
                    continue;
                }

                // Read mtime from fast field
                let mtime_val = mtime_col.values_for_doc(row_id).next().unwrap_or(0);

                // Read doc_id
                doc_id_buf.clear();
                for ord in doc_id_col.term_ords(row_id) {
                    let _ = doc_id_col.ord_to_str(ord, &mut doc_id_buf);
                }

                map.insert(path_buf.clone(), (mtime_val, doc_id_buf.clone()));
            }
        }

        map
    }

    /// Incremental index: only re-index files that changed since last index
    #[allow(unused_variables)]
    pub fn index_incremental_with_options(&self, with_embeddings: bool) -> Result<IndexStats> {
        // Build map of currently indexed files
        let mut indexed_map = self.build_indexed_files_map();

        // Create indexer (does NOT clear vector index)
        let indexer =
            index::Indexer::new(self.config.indexer.clone(), self.index.clone(), &self.root)?;

        let mut walker = fs::FileWalker::new(self.root.clone(), self.config.indexer.clone())?;

        let mut indexed = 0;
        let mut skipped = 0;
        let mut errors = 0;
        let mut unchanged = 0;

        #[cfg(feature = "embeddings")]
        let mut embedding_batch: Vec<(String, String)> = Vec::new();
        #[cfg(feature = "embeddings")]
        const BATCH_SIZE: usize = 64;

        for entry in walker.walk() {
            // Get relative path for this file
            let rel_path = entry
                .path
                .strip_prefix(&self.root)
                .unwrap_or(&entry.path)
                .to_string_lossy()
                .to_string();

            // Get current file mtime
            let current_mtime = std::fs::metadata(&entry.path)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);

            // Check if file is unchanged
            if let Some((stored_mtime, _stored_doc_id)) = indexed_map.remove(&rel_path) {
                if stored_mtime == current_mtime {
                    unchanged += 1;
                    continue;
                }
                // mtime differs - re-index below
            }
            // else: new file, not in map

            match indexer.index_file_with_chunks(&entry.path, should_index_chunks(with_embeddings))
            {
                Ok((doc_id, content)) => {
                    indexed += 1;
                    if indexed % 500 == 0 {
                        self.log_inline(format!("\r  Indexed {} files...          ", indexed));
                    }

                    #[cfg(feature = "embeddings")]
                    if with_embeddings {
                        embedding_batch.push((doc_id, content));
                    }
                    #[cfg(not(feature = "embeddings"))]
                    {
                        let _ = doc_id;
                        let _ = content;
                    }
                }
                Err(YgrepError::FileTooLarge { .. }) => {
                    skipped += 1;
                }
                Err(e) => {
                    tracing::debug!("Error indexing {}: {}", entry.path.display(), e);
                    errors += 1;
                }
            }
        }

        if indexed > 0 {
            self.log(format!("\r  Indexed {} files.              ", indexed));
        }

        // Remove files that no longer exist on disk
        // Any paths remaining in indexed_map are deleted files
        let removed = indexed_map.len();
        for (deleted_path, (_mtime, doc_id)) in &indexed_map {
            indexer.delete_by_path(deleted_path)?;

            // Also remove stale embeddings from vector index
            #[cfg(feature = "embeddings")]
            if !doc_id.is_empty() {
                self.vector_index.mark_deleted(doc_id);
            }
        }

        // Save vector index if we removed any embeddings
        #[cfg(feature = "embeddings")]
        if removed > 0 {
            if let Err(e) = self.vector_index.save() {
                tracing::debug!("Failed to save vector index after removals: {}", e);
            }
        }

        indexer.commit()?;

        // Track embedded count
        #[cfg(feature = "embeddings")]
        let mut total_embedded = 0usize;
        #[cfg(not(feature = "embeddings"))]
        let total_embedded = 0usize;

        // Generate embeddings for changed files
        #[cfg(feature = "embeddings")]
        if with_embeddings && !embedding_batch.is_empty() {
            let filtered_batch: Vec<_> = embedding_batch
                .into_iter()
                .filter(|(_, content)| {
                    let len = content.len();
                    len >= 50 && len <= 50_000
                })
                .collect();

            if !filtered_batch.is_empty() {
                use indicatif::{ProgressBar, ProgressStyle};

                let total_docs = filtered_batch.len() as u64;
                self.log(format!(
                    "Building semantic index for {} changed documents...",
                    total_docs
                ));

                self.embedding_model.preload()?;

                let pb = if self.log_tx.is_some() {
                    ProgressBar::hidden()
                } else {
                    ProgressBar::new(total_docs)
                };
                pb.set_style(
                    ProgressStyle::default_bar()
                        .template("  [{bar:40.cyan/blue}] {pos}/{len} ({percent}%)")
                        .unwrap()
                        .progress_chars("━╸─"),
                );
                pb.enable_steady_tick(std::time::Duration::from_millis(100));

                for chunk in filtered_batch.chunks(BATCH_SIZE) {
                    const EMBED_TRUNCATE: usize = 4096;
                    let texts: Vec<&str> = chunk
                        .iter()
                        .map(|(_, content)| {
                            if content.len() > EMBED_TRUNCATE {
                                let boundary = content.floor_char_boundary(EMBED_TRUNCATE);
                                &content[..boundary]
                            } else {
                                content.as_str()
                            }
                        })
                        .collect();

                    match self.embedding_model.embed_batch(&texts) {
                        Ok(embeddings) => {
                            for ((doc_id, _), embedding) in chunk.iter().zip(embeddings) {
                                if let Err(e) = self.vector_index.insert(doc_id, &embedding) {
                                    tracing::debug!(
                                        "Failed to insert embedding for {}: {}",
                                        doc_id,
                                        e
                                    );
                                }
                            }
                            total_embedded += chunk.len();
                            pb.set_position(total_embedded as u64);
                        }
                        Err(e) => {
                            tracing::warn!("Batch embedding failed: {}", e);
                            pb.inc(chunk.len() as u64);
                        }
                    }
                }

                pb.finish_and_clear();
                self.log(format!("  Indexed {} documents.", total_embedded));
                self.vector_index.save()?;
            }
        }

        #[cfg(not(feature = "embeddings"))]
        if with_embeddings {
            self.log("Warning: Semantic search feature not available in this build.");
        }

        let walk_stats = walker.stats();

        // Save workspace metadata
        let total_files = unchanged + indexed;
        let metadata = serde_json::json!({
            "workspace": self.root.to_string_lossy(),
            "indexed_at": chrono::Utc::now().to_rfc3339(),
            "files_indexed": total_files,
            "semantic": with_embeddings,
            "schema_version": index::SCHEMA_VERSION,
        });
        let metadata_path = self.index_path.join("workspace.json");
        if let Err(e) = std::fs::write(
            &metadata_path,
            serde_json::to_string_pretty(&metadata).unwrap_or_default(),
        ) {
            tracing::warn!("Failed to save workspace metadata: {}", e);
        }

        Ok(IndexStats {
            indexed,
            embedded: total_embedded,
            skipped,
            errors,
            unique_paths: walk_stats.visited_paths,
            unchanged,
            removed,
        })
    }

    /// Search the workspace
    pub fn search(&self, query: &str, limit: Option<usize>) -> Result<search::SearchResult> {
        let searcher = search::Searcher::new(self.config.search.clone(), self.index.clone());
        searcher.search(query, limit, false, None, None)
    }

    /// Search with filters
    pub fn search_filtered(
        &self,
        query: &str,
        limit: Option<usize>,
        extensions: Option<Vec<String>>,
        paths: Option<Vec<String>>,
        use_regex: bool,
        case_sensitive: bool,
        context_before: Option<usize>,
        context_after: Option<usize>,
        verbose: bool,
    ) -> Result<search::SearchResult> {
        let searcher = search::Searcher::new(self.config.search.clone(), self.index.clone());
        let filters = search::SearchFilters { extensions, paths };
        searcher.search_filtered(
            query,
            limit,
            filters,
            use_regex,
            case_sensitive,
            context_before,
            context_after,
            verbose,
        )
    }

    /// Hybrid search combining BM25 and vector search
    #[cfg(feature = "embeddings")]
    pub fn search_hybrid(&self, query: &str, limit: Option<usize>) -> Result<search::SearchResult> {
        let searcher = search::HybridSearcher::new(
            self.config.search.clone(),
            self.index.clone(),
            self.vector_index.clone(),
            self.embedding_model.clone(),
            self.embedding_cache.clone(),
        );
        searcher.search(query, limit)
    }

    /// Check if semantic search is available (vector index has data)
    #[cfg(feature = "embeddings")]
    pub fn has_semantic_index(&self) -> bool {
        !self.vector_index.is_empty()
    }

    /// Check if semantic search is available (always false without embeddings feature)
    #[cfg(not(feature = "embeddings"))]
    pub fn has_semantic_index(&self) -> bool {
        false
    }

    /// Get the workspace root
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Get the index path
    pub fn index_path(&self) -> &Path {
        &self.index_path
    }

    /// Check if the workspace has been indexed
    /// (workspace.json is only created after actual indexing, not just opening)
    pub fn is_indexed(&self) -> bool {
        self.index_path.join("workspace.json").exists()
    }

    /// Index or re-index a single file (for incremental updates)
    /// Note: path can be under workspace root OR under a symlink target
    pub fn index_file(&self, path: &Path) -> Result<()> {
        // Create indexer and index the file
        let indexer =
            index::Indexer::new(self.config.indexer.clone(), self.index.clone(), &self.root)?;

        match indexer.index_file_with_chunks(path, false) {
            Ok((_doc_id, _content)) => {
                indexer.commit()?;
                tracing::debug!("Indexed: {}", path.display());
                Ok(())
            }
            Err(YgrepError::FileTooLarge { .. }) => {
                tracing::debug!("Skipped (too large): {}", path.display());
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Delete a file from the index (for incremental updates)
    pub fn delete_file(&self, path: &Path) -> Result<()> {
        use tantivy::Term;

        // Get the relative path as doc_id
        let relative_path = path
            .strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy();

        let schema = self.index.schema();
        let path_field = schema
            .get_field("path")
            .map_err(|_| YgrepError::Config("path field not found in schema".to_string()))?;

        let term = Term::from_field_text(path_field, &relative_path);

        let mut writer = self.index.writer::<tantivy::TantivyDocument>(50_000_000)?;
        writer.delete_term(term);
        writer.commit()?;

        tracing::debug!("Deleted from index: {}", path.display());
        Ok(())
    }

    /// Create a file watcher for this workspace
    pub fn create_watcher(&self) -> Result<FileWatcher> {
        FileWatcher::new(self.root.clone(), self.config.indexer.clone())
    }

    /// Get the indexer config
    pub fn indexer_config(&self) -> &config::IndexerConfig {
        &self.config.indexer
    }

    /// Read the stored semantic flag from workspace.json metadata
    /// Returns None if no metadata exists or flag is not set
    pub fn stored_semantic_flag(&self) -> Option<bool> {
        self.read_metadata()
            .and_then(|v| v.get("semantic").and_then(|s| s.as_bool()))
    }

    /// Read the stored schema version from workspace.json metadata
    /// Returns None if no metadata exists or version is not set
    pub fn stored_schema_version(&self) -> Option<u32> {
        self.read_metadata()
            .and_then(|v| v.get("schema_version").and_then(|s| s.as_u64()))
            .map(|v| v as u32)
    }

    /// Read workspace.json metadata
    fn read_metadata(&self) -> Option<serde_json::Value> {
        let metadata_path = self.index_path.join("workspace.json");
        if metadata_path.exists() {
            std::fs::read_to_string(&metadata_path)
                .ok()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        } else {
            None
        }
    }

    /// Index or re-index a single file with optional semantic indexing (for incremental updates)
    #[allow(unused_variables)]
    pub fn index_file_with_options(&self, path: &Path, with_embeddings: bool) -> Result<()> {
        // Create indexer and index the file
        let indexer =
            index::Indexer::new(self.config.indexer.clone(), self.index.clone(), &self.root)?;
        self.index_file_with_indexer(&indexer, path, with_embeddings)
    }

    /// Index or re-index a single file using an existing Indexer (avoids lock churn)
    #[allow(unused_variables)]
    pub fn index_file_with_indexer(
        &self,
        indexer: &index::Indexer,
        path: &Path,
        with_embeddings: bool,
    ) -> Result<()> {
        match indexer.index_file_with_chunks(path, should_index_chunks(with_embeddings)) {
            Ok((doc_id, content)) => {
                indexer.commit()?;
                tracing::debug!("Indexed: {}", path.display());

                // Generate embedding if semantic indexing is enabled (reuse content from indexer)
                #[cfg(feature = "embeddings")]
                if with_embeddings {
                    // Only embed files within size bounds
                    let len = content.len();
                    if (50..=50_000).contains(&len) {
                        // Truncate for embedding
                        const EMBED_TRUNCATE: usize = 4096;
                        let text = if content.len() > EMBED_TRUNCATE {
                            let boundary = content.floor_char_boundary(EMBED_TRUNCATE);
                            &content[..boundary]
                        } else {
                            content.as_str()
                        };

                        match self.embedding_model.embed(text) {
                            Ok(embedding) => {
                                if let Err(e) = self.vector_index.insert(&doc_id, &embedding) {
                                    tracing::debug!(
                                        "Failed to insert embedding for {}: {}",
                                        doc_id,
                                        e
                                    );
                                } else {
                                    // Save vector index after each file (incremental)
                                    if let Err(e) = self.vector_index.save() {
                                        tracing::debug!("Failed to save vector index: {}", e);
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::debug!(
                                    "Failed to generate embedding for {}: {}",
                                    doc_id,
                                    e
                                );
                            }
                        }
                    }
                }

                #[cfg(not(feature = "embeddings"))]
                {
                    let _ = doc_id;
                    let _ = content;
                }

                Ok(())
            }
            Err(YgrepError::FileTooLarge { .. }) => {
                tracing::debug!("Skipped (too large): {}", path.display());
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Create a persistent Indexer for this workspace (holds a single writer lock)
    pub fn create_indexer(&self) -> Result<index::Indexer> {
        index::Indexer::new(self.config.indexer.clone(), self.index.clone(), &self.root)
    }

    /// Create a persistent Indexer with NoMergePolicy (for watch mode)
    /// Prevents background merge threads from racing with commits.
    /// Segments accumulate but are consolidated on next incremental index.
    pub fn create_watch_indexer(&self) -> Result<index::Indexer> {
        index::Indexer::new_no_merge(self.config.indexer.clone(), self.index.clone(), &self.root)
    }

    /// Index a single file without committing (for batched watch operations)
    #[allow(unused_variables)]
    pub fn index_file_no_commit(
        &self,
        indexer: &index::Indexer,
        path: &Path,
        with_embeddings: bool,
    ) -> Result<()> {
        match indexer.index_file_with_chunks(path, should_index_chunks(with_embeddings)) {
            Ok((doc_id, content)) => {
                tracing::debug!("Staged: {}", path.display());

                #[cfg(feature = "embeddings")]
                if with_embeddings {
                    let len = content.len();
                    if (50..=50_000).contains(&len) {
                        const EMBED_TRUNCATE: usize = 4096;
                        let text = if content.len() > EMBED_TRUNCATE {
                            let boundary = content.floor_char_boundary(EMBED_TRUNCATE);
                            &content[..boundary]
                        } else {
                            content.as_str()
                        };

                        match self.embedding_model.embed(text) {
                            Ok(embedding) => {
                                if let Err(e) = self.vector_index.insert(&doc_id, &embedding) {
                                    tracing::debug!(
                                        "Failed to insert embedding for {}: {}",
                                        doc_id,
                                        e
                                    );
                                }
                            }
                            Err(e) => {
                                tracing::debug!(
                                    "Failed to generate embedding for {}: {}",
                                    doc_id,
                                    e
                                );
                            }
                        }
                    }
                }

                #[cfg(not(feature = "embeddings"))]
                {
                    let _ = doc_id;
                    let _ = content;
                }

                Ok(())
            }
            Err(YgrepError::FileTooLarge { .. }) => {
                tracing::debug!("Skipped (too large): {}", path.display());
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Delete a file from the index without committing (for batched watch operations)
    pub fn delete_file_no_commit(&self, indexer: &index::Indexer, path: &Path) -> Result<()> {
        let relative_path = path
            .strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy();

        indexer.delete_by_path(&relative_path)?;
        tracing::debug!("Staged delete: {}", path.display());
        Ok(())
    }

    /// Commit all pending indexer changes
    pub fn commit_indexer(&self, indexer: &index::Indexer) -> Result<()> {
        indexer.commit()
    }

    /// Delete a file from the index using an existing Indexer (avoids lock churn)
    pub fn delete_file_with_indexer(&self, indexer: &index::Indexer, path: &Path) -> Result<()> {
        let relative_path = path
            .strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy();

        indexer.delete_by_path(&relative_path)?;
        indexer.commit()?;

        tracing::debug!("Deleted from index: {}", path.display());
        Ok(())
    }
}

/// Statistics from an indexing operation
#[derive(Debug, Clone, Default)]
pub struct IndexStats {
    pub indexed: usize,
    pub embedded: usize,
    pub skipped: usize,
    pub errors: usize,
    pub unique_paths: usize,
    pub unchanged: usize,
    pub removed: usize,
}

/// Hash a path to create a unique identifier
fn hash_path(path: &Path) -> String {
    use xxhash_rust::xxh3::xxh3_64;
    let hash = xxh3_64(path.to_string_lossy().as_bytes());
    format!("{:016x}", hash)
}

fn should_index_chunks(with_embeddings: bool) -> bool {
    cfg!(feature = "embeddings") && with_embeddings
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_workspace_open() -> Result<()> {
        let temp_dir = tempdir().unwrap();

        // Create a test file
        std::fs::write(temp_dir.path().join("test.rs"), "fn main() {}").unwrap();

        let workspace = Workspace::create(temp_dir.path())?;
        assert!(workspace.root().exists());

        Ok(())
    }

    #[test]
    fn test_workspace_index_and_search() -> Result<()> {
        let temp_dir = tempdir().unwrap();

        // Create a workspace subdirectory to avoid the walker's hardcoded
        // ignore list (which includes "tmp", "var" — common tempdir components)
        let workspace_dir = temp_dir.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        // Create test files
        std::fs::write(
            workspace_dir.join("hello.rs"),
            "fn hello_world() { println!(\"Hello!\"); }",
        )
        .unwrap();
        std::fs::write(
            workspace_dir.join("goodbye.rs"),
            "fn goodbye_world() { println!(\"Bye!\"); }",
        )
        .unwrap();

        let mut config = Config::default();
        config.indexer.data_dir = temp_dir.path().join("data");
        config.indexer.ignore_patterns = vec![];

        let workspace = Workspace::create_with_config(&workspace_dir, config)?;

        // Index
        let stats = workspace.index_all()?;
        assert!(
            stats.indexed >= 2,
            "Expected at least 2 indexed files, got {}",
            stats.indexed
        );

        // Search
        let result = workspace.search("hello", None)?;
        assert!(!result.is_empty());
        assert!(result.hits.iter().any(|h| h.path.contains("hello")));

        Ok(())
    }

    /// Recursively set or clear write permission on a directory tree
    #[cfg(unix)]
    fn set_tree_writable(path: &Path, writable: bool) {
        use std::os::unix::fs::PermissionsExt;
        let mode = if path.is_dir() {
            if writable {
                0o755
            } else {
                0o555
            }
        } else if writable {
            0o644
        } else {
            0o444
        };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        if path.is_dir() {
            for entry in std::fs::read_dir(path).unwrap() {
                set_tree_writable(&entry.unwrap().path(), writable);
            }
        }
    }

    /// Read-only search on a non-writable index directory (issue #12):
    /// a sandboxed process must be able to search an index it cannot write to.
    #[cfg(unix)]
    #[test]
    fn test_readonly_open_and_search_on_unwritable_index() -> Result<()> {
        let temp_dir = tempdir().unwrap();
        let workspace_dir = temp_dir.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();
        std::fs::write(
            workspace_dir.join("hello.rs"),
            "fn hello_world() { println!(\"Hello!\"); }",
        )
        .unwrap();

        let data_dir = temp_dir.path().join("data");
        let mut config = Config::default();
        config.indexer.data_dir = data_dir.clone();
        config.indexer.ignore_patterns = vec![];

        // Index normally, then drop the workspace to release locks
        {
            let workspace = Workspace::create_with_config(&workspace_dir, config.clone())?;
            let stats = workspace.index_all()?;
            assert!(stats.indexed >= 1);
        }

        // Make the entire data directory read-only
        set_tree_writable(&data_dir, false);

        let result = (|| -> Result<()> {
            let workspace = Workspace::open_readonly_with_config(&workspace_dir, config.clone())?;
            let result = workspace.search("hello", None)?;
            assert!(!result.is_empty());
            assert!(result.hits.iter().any(|h| h.path.contains("hello")));

            // Status-style metadata reads must also work
            assert_eq!(workspace.stored_semantic_flag(), Some(false));

            // A writable open of the same index must fail with the
            // writability hint, not something cryptic
            let err = Workspace::open_with_config(&workspace_dir, config.clone())
                .err()
                .expect("writable open of a read-only index should fail");
            assert!(
                err.to_string().contains("not writable"),
                "unexpected error: {err}"
            );
            Ok(())
        })();

        // Restore permissions so the tempdir can be cleaned up
        set_tree_writable(&data_dir, true);
        result
    }

    /// Opening an unindexed workspace must return the WorkspaceNotIndexed
    /// variant so the CLI can distinguish it from other open failures (issue #12)
    #[test]
    fn test_open_unindexed_returns_not_indexed_variant() {
        let temp_dir = tempdir().unwrap();
        let workspace_dir = temp_dir.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let mut config = Config::default();
        config.indexer.data_dir = temp_dir.path().join("data");

        for result in [
            Workspace::open_with_config(&workspace_dir, config.clone()),
            Workspace::open_readonly_with_config(&workspace_dir, config.clone()),
        ] {
            match result {
                Err(YgrepError::WorkspaceNotIndexed(_)) => {}
                Err(e) => panic!("expected WorkspaceNotIndexed, got: {e}"),
                Ok(_) => panic!("expected WorkspaceNotIndexed, got Ok"),
            }
        }
    }

    #[test]
    fn test_shared_indexer_multiple_files() -> Result<()> {
        let temp_dir = tempdir().unwrap();
        let workspace_dir = temp_dir.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        // Create initial file and index
        std::fs::write(workspace_dir.join("initial.rs"), "fn initial() {}").unwrap();

        let mut config = Config::default();
        config.indexer.data_dir = temp_dir.path().join("data");
        config.indexer.ignore_patterns = vec![];

        let workspace = Workspace::create_with_config(&workspace_dir, config)?;
        workspace.index_all()?;

        // Create a shared indexer and use it for multiple file operations
        let indexer = workspace.create_indexer()?;

        // Index several files rapidly with the same indexer (simulates watch loop)
        for i in 0..20 {
            let filename = format!("file_{}.rs", i);
            let content = format!("fn func_{}() {{ /* content {} */ }}", i, i);
            std::fs::write(workspace_dir.join(&filename), &content).unwrap();
            workspace.index_file_with_indexer(&indexer, &workspace_dir.join(&filename), false)?;
        }

        // Verify all files are searchable
        for i in 0..20 {
            let query = format!("func_{}", i);
            let result = workspace.search(&query, None)?;
            assert!(
                !result.is_empty(),
                "File {} should be searchable after indexing with shared indexer",
                i
            );
        }

        // Delete some files with the same indexer
        for i in 0..5 {
            let path = workspace_dir.join(format!("file_{}.rs", i));
            workspace.delete_file_with_indexer(&indexer, &path)?;
        }

        // Verify deleted files are gone, others remain
        let result = workspace.search("func_0", None)?;
        assert!(
            result.is_empty(),
            "Deleted file should not appear in search"
        );

        let result = workspace.search("func_10", None)?;
        assert!(
            !result.is_empty(),
            "Non-deleted file should still be searchable"
        );

        Ok(())
    }

    #[test]
    fn test_shared_indexer_no_lock_contention() -> Result<()> {
        // This test verifies that creating a single indexer and reusing it
        // for rapid sequential operations doesn't cause LockBusy errors,
        // which was the bug when creating a new indexer per file event.
        let temp_dir = tempdir().unwrap();
        let workspace_dir = temp_dir.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        std::fs::write(workspace_dir.join("seed.rs"), "fn seed() {}").unwrap();

        let mut config = Config::default();
        config.indexer.data_dir = temp_dir.path().join("data");
        config.indexer.ignore_patterns = vec![];

        let workspace = Workspace::create_with_config(&workspace_dir, config)?;
        workspace.index_all()?;

        let indexer = workspace.create_indexer()?;

        // Simulate heavy churn: rapidly create, index, modify, re-index, delete
        for i in 0..50 {
            let path = workspace_dir.join(format!("churn_{}.rs", i));

            // Create and index
            std::fs::write(&path, format!("fn v1_{}() {{}}", i)).unwrap();
            workspace.index_file_with_indexer(&indexer, &path, false)?;

            // Modify and re-index (simulates rapid file changes)
            std::fs::write(&path, format!("fn v2_{}() {{}}", i)).unwrap();
            workspace.index_file_with_indexer(&indexer, &path, false)?;

            // Delete half of them
            if i % 2 == 0 {
                workspace.delete_file_with_indexer(&indexer, &path)?;
            }
        }

        // Verify odd-numbered files are still searchable with updated content
        let result = workspace.search("v2_1", None)?;
        assert!(
            !result.is_empty(),
            "Surviving file should have latest content"
        );

        // Verify even-numbered (deleted) files are gone
        let result = workspace.search("v2_0", None)?;
        assert!(result.is_empty(), "Deleted file should not appear");

        Ok(())
    }

    #[test]
    fn test_per_file_indexer_still_works() -> Result<()> {
        // Ensure the original index_file_with_options path (one indexer per call)
        // still works correctly for non-watch usage.
        let temp_dir = tempdir().unwrap();
        let workspace_dir = temp_dir.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        std::fs::write(workspace_dir.join("base.rs"), "fn base() {}").unwrap();

        let mut config = Config::default();
        config.indexer.data_dir = temp_dir.path().join("data");
        config.indexer.ignore_patterns = vec![];

        let workspace = Workspace::create_with_config(&workspace_dir, config)?;
        workspace.index_all()?;

        // Use the per-call indexer path (index_file_with_options)
        let path = workspace_dir.join("standalone.rs");
        std::fs::write(&path, "fn standalone_function() {}").unwrap();
        workspace.index_file_with_options(&path, false)?;

        let result = workspace.search("standalone_function", None)?;
        assert!(
            !result.is_empty(),
            "File indexed via index_file_with_options should be searchable"
        );

        Ok(())
    }

    #[test]
    fn test_batched_commit_multiple_files() -> Result<()> {
        // Simulates the watch loop pattern: stage many files, commit once.
        // This is the pattern that prevents segment merge warnings under churn.
        let temp_dir = tempdir().unwrap();
        let workspace_dir = temp_dir.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        std::fs::write(workspace_dir.join("seed.rs"), "fn seed() {}").unwrap();

        let mut config = Config::default();
        config.indexer.data_dir = temp_dir.path().join("data");
        config.indexer.ignore_patterns = vec![];

        let workspace = Workspace::create_with_config(&workspace_dir, config)?;
        workspace.index_all()?;

        let indexer = workspace.create_indexer()?;

        // Stage 30 file additions without committing (simulates a git branch switch)
        for i in 0..30 {
            let path = workspace_dir.join(format!("batch_{}.rs", i));
            std::fs::write(&path, format!("fn batch_func_{}() {{}}", i)).unwrap();
            workspace.index_file_no_commit(&indexer, &path, false)?;
        }

        // Single commit for all 30 files
        workspace.commit_indexer(&indexer)?;

        // All files should be searchable
        for i in 0..30 {
            let result = workspace.search(&format!("batch_func_{}", i), None)?;
            assert!(
                !result.is_empty(),
                "File {} should be searchable after batched commit",
                i
            );
        }

        // Now stage mixed deletes and adds without committing
        for i in 0..10 {
            let path = workspace_dir.join(format!("batch_{}.rs", i));
            workspace.delete_file_no_commit(&indexer, &path)?;
        }
        for i in 30..40 {
            let path = workspace_dir.join(format!("batch_{}.rs", i));
            std::fs::write(&path, format!("fn batch_func_{}() {{}}", i)).unwrap();
            workspace.index_file_no_commit(&indexer, &path, false)?;
        }

        // Single commit for mixed batch
        workspace.commit_indexer(&indexer)?;

        // Deleted files gone
        let result = workspace.search("batch_func_0", None)?;
        assert!(result.is_empty(), "Deleted file should not appear");

        // Surviving and new files present
        let result = workspace.search("batch_func_15", None)?;
        assert!(!result.is_empty(), "Surviving file should be searchable");

        let result = workspace.search("batch_func_35", None)?;
        assert!(!result.is_empty(), "Newly added file should be searchable");

        Ok(())
    }

    #[test]
    fn test_batched_commit_heavy_churn() -> Result<()> {
        // Simulates git-style churn: many files created, modified, and deleted
        // in rapid succession with only periodic commits (not per-file).
        let temp_dir = tempdir().unwrap();
        let workspace_dir = temp_dir.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        std::fs::write(workspace_dir.join("seed.rs"), "fn seed() {}").unwrap();

        let mut config = Config::default();
        config.indexer.data_dir = temp_dir.path().join("data");
        config.indexer.ignore_patterns = vec![];

        let workspace = Workspace::create_with_config(&workspace_dir, config)?;
        workspace.index_all()?;

        let indexer = workspace.create_indexer()?;

        // Simulate 5 rapid batches (like 5 debounce windows during git checkout)
        for batch in 0..5 {
            // Each batch: create/modify/delete 20 files, commit once
            for i in 0..20 {
                let idx = batch * 20 + i;
                let path = workspace_dir.join(format!("churn_{}.rs", idx));

                std::fs::write(&path, format!("fn churn_v{}_{} () {{}}", batch, idx)).unwrap();
                workspace.index_file_no_commit(&indexer, &path, false)?;
            }

            // Delete files from previous batch
            if batch > 0 {
                for i in 0..10 {
                    let idx = (batch - 1) * 20 + i;
                    let path = workspace_dir.join(format!("churn_{}.rs", idx));
                    workspace.delete_file_no_commit(&indexer, &path)?;
                }
            }

            // One commit per batch
            workspace.commit_indexer(&indexer)?;
        }

        // Verify last batch's files are searchable
        let result = workspace.search("churn_v4_80", None)?;
        assert!(!result.is_empty(), "Latest batch file should be searchable");

        // Verify early deleted files are gone
        let result = workspace.search("churn_v0_0", None)?;
        assert!(
            result.is_empty(),
            "Deleted file from early batch should be gone"
        );

        Ok(())
    }
}
