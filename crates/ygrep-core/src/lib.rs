//! ygrep-core - Core library for ygrep semantic code search
//!
//! This crate provides the core functionality for indexing and searching code:
//! - Tantivy-based full-text indexing
//! - File system walking with symlink handling
//! - BM25 text search + semantic vector search
//! - Hybrid search with Reciprocal Rank Fusion
//! - Configuration management

pub mod config;
pub mod embeddings;
pub mod error;
pub mod fs;
pub mod index;
pub mod search;
pub mod watcher;

pub use config::Config;
pub use error::{Result, YgrepError};
pub use watcher::{FileWatcher, WatchEvent};

use std::path::Path;
use std::sync::Arc;
use tantivy::Index;

use embeddings::{EmbeddingModel, EmbeddingCache};
use index::VectorIndex;

/// Embedding dimension for bge-small-en-v1.5
const EMBEDDING_DIM: usize = 384;

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
    /// Vector index for semantic search
    vector_index: Arc<VectorIndex>,
    /// Embedding model
    embedding_model: Arc<EmbeddingModel>,
    /// Embedding cache
    embedding_cache: Arc<EmbeddingCache>,
}

impl Workspace {
    /// Open or create a workspace for the given directory
    pub fn open(root: &Path) -> Result<Self> {
        let config = Config::load();
        Self::open_with_config(root, config)
    }

    /// Open or create a workspace with custom config
    pub fn open_with_config(root: &Path, config: Config) -> Result<Self> {
        let root = std::fs::canonicalize(root)?;

        // Create index directory based on workspace path hash
        let workspace_hash = hash_path(&root);
        let index_path = config.indexer.data_dir.join("indexes").join(&workspace_hash);
        std::fs::create_dir_all(&index_path)?;

        // Open or create Tantivy index
        let schema = index::build_document_schema();
        let index = if index_path.join("meta.json").exists() {
            Index::open_in_dir(&index_path)?
        } else {
            Index::create_in_dir(&index_path, schema)?
        };

        // Register our custom code tokenizer
        index::register_tokenizers(index.tokenizers());

        // Create vector index path
        let vector_path = index_path.join("vectors");

        // Load or create vector index
        let vector_index = if VectorIndex::exists(&vector_path) {
            Arc::new(VectorIndex::load(vector_path)?)
        } else {
            Arc::new(VectorIndex::new(vector_path, EMBEDDING_DIM)?)
        };

        // Create embedding model (lazy-loaded on first use)
        let embedding_model = Arc::new(EmbeddingModel::default()); // Uses bge-small-en-v1.5

        // Create embedding cache (100MB cache, 384 dimensions)
        let embedding_cache = Arc::new(EmbeddingCache::new(100, EMBEDDING_DIM));

        Ok(Self {
            root,
            config,
            index,
            index_path,
            vector_index,
            embedding_model,
            embedding_cache,
        })
    }

    /// Index all files in the workspace (text-only by default, fast)
    pub fn index_all(&self) -> Result<IndexStats> {
        self.index_all_with_options(false)
    }

    /// Index all files with options
    pub fn index_all_with_options(&self, with_embeddings: bool) -> Result<IndexStats> {
        // Clear vector index for fresh re-index
        self.vector_index.clear();

        // Phase 1: Index all files with BM25 (fast)
        let indexer = index::Indexer::new(
            self.config.indexer.clone(),
            self.index.clone(),
            &self.root,
        )?;

        let mut walker = fs::FileWalker::new(self.root.clone(), self.config.indexer.clone())?;

        let mut indexed = 0;
        let mut skipped = 0;
        let mut errors = 0;

        // Collect content for batch embedding
        let mut embedding_batch: Vec<(String, String)> = Vec::new(); // (doc_id, content)
        const BATCH_SIZE: usize = 32;

        for entry in walker.walk() {
            match indexer.index_file(&entry.path) {
                Ok(doc_id) => {
                    indexed += 1;
                    if indexed % 500 == 0 {
                        eprint!("\r  Indexed {} files...          ", indexed);
                    }

                    // Collect for embedding if enabled
                    if with_embeddings {
                        if let Ok(content) = std::fs::read_to_string(&entry.path) {
                            embedding_batch.push((doc_id, content));
                        }
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

        eprintln!("\r  Indexed {} files.              ", indexed);
        indexer.commit()?;

        // Phase 2: Generate embeddings in batches (if enabled)
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
                eprintln!("No documents suitable for embedding.");
            } else {
                eprintln!("Generating embeddings for {} documents (filtered from {})...",
                    filtered_batch.len(), indexed);

                let total_docs = filtered_batch.len();
                let mut embedded = 0;

                for chunk in filtered_batch.chunks(BATCH_SIZE) {
                    // Truncate very long content to first 8KB for embedding
                    let texts: Vec<&str> = chunk.iter()
                        .map(|(_, content)| {
                            if content.len() > 8192 {
                                &content[..8192]
                            } else {
                                content.as_str()
                            }
                        })
                        .collect();

                    match self.embedding_model.embed_batch(&texts) {
                        Ok(embeddings) => {
                            for ((doc_id, _), embedding) in chunk.iter().zip(embeddings) {
                                if let Err(e) = self.vector_index.insert(doc_id, &embedding) {
                                    tracing::debug!("Failed to insert embedding for {}: {}", doc_id, e);
                                }
                            }
                            embedded += chunk.len();
                            eprint!("\r  Embedded {}/{} documents...    ", embedded, total_docs);
                        }
                        Err(e) => {
                            tracing::warn!("Batch embedding failed: {}", e);
                        }
                    }
                }

                eprintln!("\r  Embedded {} documents.              ", embedded);
                self.vector_index.save()?;
            }
        }

        let stats = walker.stats();

        // Save workspace metadata for index management
        let metadata = serde_json::json!({
            "workspace": self.root.to_string_lossy(),
            "indexed_at": chrono::Utc::now().to_rfc3339(),
            "files_indexed": indexed,
        });
        let metadata_path = self.index_path.join("workspace.json");
        if let Err(e) = std::fs::write(&metadata_path, serde_json::to_string_pretty(&metadata).unwrap_or_default()) {
            tracing::warn!("Failed to save workspace metadata: {}", e);
        }

        Ok(IndexStats {
            indexed,
            skipped,
            errors,
            unique_paths: stats.visited_paths,
        })
    }

    /// Search the workspace
    pub fn search(&self, query: &str, limit: Option<usize>) -> Result<search::SearchResult> {
        let searcher = search::Searcher::new(self.config.search.clone(), self.index.clone());
        searcher.search(query, limit)
    }

    /// Search with filters
    pub fn search_filtered(
        &self,
        query: &str,
        limit: Option<usize>,
        extensions: Option<Vec<String>>,
        paths: Option<Vec<String>>,
    ) -> Result<search::SearchResult> {
        let searcher = search::Searcher::new(self.config.search.clone(), self.index.clone());
        let filters = search::SearchFilters { extensions, paths };
        searcher.search_filtered(query, limit, filters)
    }

    /// Hybrid search combining BM25 and vector search
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
    pub fn has_semantic_index(&self) -> bool {
        !self.vector_index.is_empty()
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
    pub fn is_indexed(&self) -> bool {
        self.index_path.join("meta.json").exists()
    }

    /// Index or re-index a single file (for incremental updates)
    /// Note: path can be under workspace root OR under a symlink target
    pub fn index_file(&self, path: &Path) -> Result<()> {
        // Create indexer and index the file
        let indexer = index::Indexer::new(
            self.config.indexer.clone(),
            self.index.clone(),
            &self.root,
        )?;

        match indexer.index_file(path) {
            Ok(_doc_id) => {
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
        let doc_id_field = schema.get_field("doc_id").map_err(|_| {
            YgrepError::Config("doc_id field not found in schema".to_string())
        })?;

        let term = Term::from_field_text(doc_id_field, &relative_path);

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
}

/// Statistics from an indexing operation
#[derive(Debug, Clone, Default)]
pub struct IndexStats {
    pub indexed: usize,
    pub skipped: usize,
    pub errors: usize,
    pub unique_paths: usize,
}

/// Hash a path to create a unique identifier
fn hash_path(path: &Path) -> String {
    use xxhash_rust::xxh3::xxh3_64;
    let hash = xxh3_64(path.to_string_lossy().as_bytes());
    format!("{:016x}", hash)
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

        let workspace = Workspace::open(temp_dir.path())?;
        assert!(workspace.root().exists());

        Ok(())
    }

    #[test]
    fn test_workspace_index_and_search() -> Result<()> {
        let temp_dir = tempdir().unwrap();

        // Create test files
        std::fs::write(temp_dir.path().join("hello.rs"), "fn hello_world() { println!(\"Hello!\"); }").unwrap();
        std::fs::write(temp_dir.path().join("goodbye.rs"), "fn goodbye_world() { println!(\"Bye!\"); }").unwrap();

        let mut config = Config::default();
        config.indexer.data_dir = temp_dir.path().join("data");

        let workspace = Workspace::open_with_config(temp_dir.path(), config)?;

        // Index
        let stats = workspace.index_all()?;
        assert!(stats.indexed >= 2);

        // Search
        let result = workspace.search("hello", None)?;
        assert!(!result.is_empty());
        assert!(result.hits.iter().any(|h| h.path.contains("hello")));

        Ok(())
    }
}
