use parking_lot::RwLock;
use std::path::Path;
use std::sync::Arc;
use tantivy::{Index, IndexWriter, TantivyDocument, Term};
use xxhash_rust::xxh3::xxh3_64;

use super::schema::SchemaFields;
#[cfg(feature = "embeddings")]
use super::VectorIndex;
use crate::config::IndexerConfig;
#[cfg(feature = "embeddings")]
use crate::embeddings::{EmbeddingCache, EmbeddingModel};
use crate::error::{Result, YgrepError};

/// Handles indexing of files and content
pub struct Indexer {
    config: IndexerConfig,
    index: Index,
    writer: Arc<RwLock<IndexWriter>>,
    fields: SchemaFields,
    workspace_root: String,
    /// Optional vector index for semantic search
    #[cfg(feature = "embeddings")]
    vector_index: Option<Arc<VectorIndex>>,
    /// Optional embedding model
    #[cfg(feature = "embeddings")]
    embedding_model: Option<Arc<EmbeddingModel>>,
    /// Optional embedding cache
    #[cfg(feature = "embeddings")]
    embedding_cache: Option<Arc<EmbeddingCache>>,
}

impl Indexer {
    /// Create a new indexer for a workspace (text search only)
    pub fn new(config: IndexerConfig, index: Index, workspace_root: &Path) -> Result<Self> {
        let writer = index.writer(50_000_000)?; // 50MB heap
        let schema = index.schema();
        let fields = SchemaFields::new(&schema);

        Ok(Self {
            config,
            index,
            writer: Arc::new(RwLock::new(writer)),
            fields,
            workspace_root: workspace_root.to_string_lossy().to_string(),
            #[cfg(feature = "embeddings")]
            vector_index: None,
            #[cfg(feature = "embeddings")]
            embedding_model: None,
            #[cfg(feature = "embeddings")]
            embedding_cache: None,
        })
    }

    /// Create a new indexer with semantic search support
    #[cfg(feature = "embeddings")]
    pub fn with_semantic(
        config: IndexerConfig,
        index: Index,
        workspace_root: &Path,
        vector_index: Arc<VectorIndex>,
        embedding_model: Arc<EmbeddingModel>,
        embedding_cache: Arc<EmbeddingCache>,
    ) -> Result<Self> {
        let writer = index.writer(50_000_000)?; // 50MB heap
        let schema = index.schema();
        let fields = SchemaFields::new(&schema);

        Ok(Self {
            config,
            index,
            writer: Arc::new(RwLock::new(writer)),
            fields,
            workspace_root: workspace_root.to_string_lossy().to_string(),
            vector_index: Some(vector_index),
            embedding_model: Some(embedding_model),
            embedding_cache: Some(embedding_cache),
        })
    }

    /// Index a single file
    /// Returns (doc_id, content) so callers can reuse the content without re-reading.
    pub fn index_file(&self, path: &Path) -> Result<(String, String)> {
        // Read file content
        let content = std::fs::read_to_string(path)?;
        let metadata = std::fs::metadata(path)?;

        // Check file size
        let size = metadata.len();
        if size > self.config.max_file_size {
            return Err(YgrepError::FileTooLarge {
                path: path.to_path_buf(),
                size,
                max: self.config.max_file_size,
            });
        }

        // Generate content hash for deduplication and doc_id
        let content_hash = xxh3_64(content.as_bytes());
        let doc_id = format!("{:016x}", content_hash);

        // Get relative path
        let rel_path = path
            .strip_prefix(&self.workspace_root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();

        // Get file extension
        let extension = path
            .extension()
            .map(|e| e.to_string_lossy().to_string())
            .unwrap_or_default();

        // Get modification time
        let mtime = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let line_count = content.lines().count() as u64;

        // Build the document
        let mut doc = TantivyDocument::new();
        doc.add_text(self.fields.doc_id, &doc_id);
        doc.add_text(self.fields.path, &rel_path);
        doc.add_text(self.fields.workspace, &self.workspace_root);
        doc.add_text(self.fields.content, &content);
        doc.add_u64(self.fields.mtime, mtime);
        doc.add_u64(self.fields.size, size);
        doc.add_text(self.fields.extension, &extension);
        doc.add_u64(self.fields.line_start, 1);
        doc.add_u64(self.fields.line_end, line_count);
        doc.add_text(self.fields.chunk_id, ""); // Not a chunk
        doc.add_text(self.fields.parent_doc, ""); // Not a chunk

        // Delete any existing document with same path
        self.delete_by_path(&rel_path)?;

        // Add the document
        let mut writer = self.writer.write();
        writer.add_document(doc)?;

        // Also create chunks for the file
        #[cfg(feature = "embeddings")]
        let chunk_ids = self.index_chunks(&content, &doc_id, &rel_path, &mut writer)?;
        #[cfg(not(feature = "embeddings"))]
        let _ = self.index_chunks(&content, &doc_id, &rel_path, &mut writer)?;

        // Release the writer lock before embedding generation
        drop(writer);

        // Generate embeddings if semantic search is enabled
        #[cfg(feature = "embeddings")]
        if let (Some(vector_index), Some(model), Some(cache)) = (
            &self.vector_index,
            &self.embedding_model,
            &self.embedding_cache,
        ) {
            // Embed the full document
            let embedding = cache.get_or_insert(&content, || {
                model.embed(&content).unwrap_or_else(|_| vec![0.0; 384])
            });
            vector_index.insert(&doc_id, &embedding)?;

            // Embed chunks
            for (chunk_id, chunk_content) in chunk_ids {
                let chunk_embedding = cache.get_or_insert(&chunk_content, || {
                    model
                        .embed(&chunk_content)
                        .unwrap_or_else(|_| vec![0.0; 384])
                });
                vector_index.insert(&chunk_id, &chunk_embedding)?;
            }
        }

        Ok((doc_id, content))
    }

    /// Index chunks of a file for more granular search
    /// Returns a list of (chunk_id, chunk_content) tuples for embedding generation
    fn index_chunks(
        &self,
        content: &str,
        parent_doc_id: &str,
        path: &str,
        writer: &mut IndexWriter,
    ) -> Result<Vec<(String, String)>> {
        let lines: Vec<&str> = content.lines().collect();
        let chunk_size = self.config.chunk_size;
        let overlap = self.config.chunk_overlap;

        if lines.len() <= chunk_size {
            // File is small enough, no need for chunks
            return Ok(vec![]);
        }

        let mut chunks = Vec::new();
        let mut start = 0;
        let mut chunk_num = 0;

        while start < lines.len() {
            let end = (start + chunk_size).min(lines.len());
            let chunk_content = lines[start..end].join("\n");
            let chunk_id = format!("{}:{}", parent_doc_id, chunk_num);

            let mut doc = TantivyDocument::new();
            doc.add_text(self.fields.doc_id, &chunk_id);
            doc.add_text(self.fields.path, path);
            doc.add_text(self.fields.workspace, &self.workspace_root);
            doc.add_text(self.fields.content, &chunk_content);
            doc.add_u64(self.fields.mtime, 0);
            doc.add_u64(self.fields.size, chunk_content.len() as u64);
            doc.add_text(self.fields.extension, "");
            doc.add_u64(self.fields.line_start, (start + 1) as u64);
            doc.add_u64(self.fields.line_end, end as u64);
            doc.add_text(self.fields.chunk_id, &chunk_id);
            doc.add_text(self.fields.parent_doc, parent_doc_id);

            writer.add_document(doc)?;

            // Store chunk info for embedding
            chunks.push((chunk_id, chunk_content));

            chunk_num += 1;
            start += chunk_size - overlap;
        }

        Ok(chunks)
    }

    /// Delete a document by path
    pub fn delete_by_path(&self, path: &str) -> Result<()> {
        let term = Term::from_field_text(self.fields.path, path);
        let writer = self.writer.write();
        writer.delete_term(term);
        Ok(())
    }

    /// Delete a document by doc_id
    pub fn delete_by_id(&self, doc_id: &str) -> Result<()> {
        let term = Term::from_field_text(self.fields.doc_id, doc_id);
        let writer = self.writer.write();
        writer.delete_term(term);
        Ok(())
    }

    /// Commit pending changes to the index
    pub fn commit(&self) -> Result<()> {
        let mut writer = self.writer.write();
        writer.commit()?;

        // Also save the vector index if present
        #[cfg(feature = "embeddings")]
        if let Some(vector_index) = &self.vector_index {
            vector_index.save()?;
        }

        Ok(())
    }

    /// Get the underlying index
    pub fn index(&self) -> &Index {
        &self.index
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::schema::build_document_schema;
    use tempfile::tempdir;

    /// Helper: create an index in a temp directory
    fn create_test_indexer(temp_dir: &std::path::Path) -> (Indexer, std::path::PathBuf) {
        let index_path = temp_dir.join("index");
        std::fs::create_dir_all(&index_path).unwrap();

        let schema = build_document_schema();
        let index = Index::create_in_dir(&index_path, schema).unwrap();
        crate::index::register_tokenizers(index.tokenizers());

        let config = IndexerConfig::default();
        let indexer = Indexer::new(config, index, temp_dir).unwrap();
        (indexer, index_path)
    }

    #[test]
    fn test_index_file() -> Result<()> {
        let temp_dir = tempdir().unwrap();
        let (indexer, _) = create_test_indexer(temp_dir.path());

        // Create test file
        let test_file = temp_dir.path().join("test.rs");
        std::fs::write(&test_file, "fn main() {\n    println!(\"hello\");\n}").unwrap();

        // Index the file
        let (doc_id, content) = indexer.index_file(&test_file)?;
        indexer.commit()?;

        assert!(!doc_id.is_empty());
        assert!(content.contains("hello"));
        Ok(())
    }

    #[test]
    fn test_content_hash_deduplication() -> Result<()> {
        let temp_dir = tempdir().unwrap();
        let (indexer, _) = create_test_indexer(temp_dir.path());

        let content = "fn duplicate() {}";
        let file1 = temp_dir.path().join("file1.rs");
        let file2 = temp_dir.path().join("file2.rs");
        std::fs::write(&file1, content).unwrap();
        std::fs::write(&file2, content).unwrap();

        let (doc_id1, _) = indexer.index_file(&file1)?;
        let (doc_id2, _) = indexer.index_file(&file2)?;

        // Same content should produce the same doc_id (content hash)
        assert_eq!(doc_id1, doc_id2);
        Ok(())
    }

    #[test]
    fn test_file_too_large() {
        let temp_dir = tempdir().unwrap();

        let index_path = temp_dir.path().join("index");
        std::fs::create_dir_all(&index_path).unwrap();
        let schema = build_document_schema();
        let index = Index::create_in_dir(&index_path, schema).unwrap();
        crate::index::register_tokenizers(index.tokenizers());

        let mut config = IndexerConfig::default();
        config.max_file_size = 10; // 10 bytes max

        let indexer = Indexer::new(config, index, temp_dir.path()).unwrap();

        // Create a file larger than the limit
        let large_file = temp_dir.path().join("large.rs");
        std::fs::write(
            &large_file,
            "this content is definitely longer than 10 bytes",
        )
        .unwrap();

        let result = indexer.index_file(&large_file);
        assert!(matches!(
            result,
            Err(crate::error::YgrepError::FileTooLarge { .. })
        ));
    }

    #[test]
    fn test_chunking_large_file() -> Result<()> {
        let temp_dir = tempdir().unwrap();

        let index_path = temp_dir.path().join("index");
        std::fs::create_dir_all(&index_path).unwrap();
        let schema = build_document_schema();
        let index = Index::create_in_dir(&index_path, schema).unwrap();
        crate::index::register_tokenizers(index.tokenizers());

        let mut config = IndexerConfig::default();
        config.chunk_size = 5; // 5 lines per chunk
        config.chunk_overlap = 1;

        let indexer = Indexer::new(config, index.clone(), temp_dir.path()).unwrap();

        // Create a file with 15 lines (should produce chunks)
        let lines: Vec<String> = (1..=15).map(|i| format!("line {} content", i)).collect();
        let content = lines.join("\n");
        let test_file = temp_dir.path().join("big.rs");
        std::fs::write(&test_file, &content).unwrap();

        indexer.index_file(&test_file)?;
        indexer.commit()?;

        // Verify chunks were created by searching the index
        let reader = index.reader()?;
        let searcher = reader.searcher();
        let all_query = tantivy::query::AllQuery;
        let top_docs =
            searcher.search(&all_query, &tantivy::collector::TopDocs::with_limit(100))?;

        // Should have 1 parent doc + multiple chunks
        assert!(
            top_docs.len() > 1,
            "Expected chunks, got {} docs",
            top_docs.len()
        );
        Ok(())
    }

    #[test]
    fn test_delete_by_path() -> Result<()> {
        let temp_dir = tempdir().unwrap();
        let (indexer, _) = create_test_indexer(temp_dir.path());

        let test_file = temp_dir.path().join("deleteme.rs");
        std::fs::write(&test_file, "fn to_delete() {}").unwrap();

        indexer.index_file(&test_file)?;
        indexer.commit()?;

        // Delete by relative path
        indexer.delete_by_path("deleteme.rs")?;
        indexer.commit()?;

        // Verify deletion by searching
        let reader = indexer.index().reader()?;
        let searcher = reader.searcher();
        let all_query = tantivy::query::AllQuery;
        let top_docs =
            searcher.search(&all_query, &tantivy::collector::TopDocs::with_limit(100))?;

        assert_eq!(top_docs.len(), 0, "Document should have been deleted");
        Ok(())
    }
}
