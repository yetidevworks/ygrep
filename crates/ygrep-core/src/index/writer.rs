use parking_lot::RwLock;
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use tantivy::merge_policy::NoMergePolicy;
use tantivy::{Index, IndexWriter, TantivyDocument, Term};
use xxhash_rust::xxh3::xxh3_64;

use super::schema::SchemaFields;
#[cfg(feature = "embeddings")]
use super::VectorIndex;
use crate::config::IndexerConfig;
#[cfg(feature = "embeddings")]
use crate::embeddings::{EmbeddingCache, EmbeddingModel};
use crate::error::{Result, YgrepError};
use crate::fs::classify;

/// Writer heap for a bulk index build, when the config asks for nothing else
pub const DEFAULT_WRITER_HEAP_MB: usize = 50;

/// Writer heap for indexers that handle one file at a time.
///
/// A watch session holds its writer open for as long as the workspace is watched, and
/// the dashboard holds one per watched workspace, so the build-sized heap was charged
/// to every idle repository: eight watched repositories reserved 400MB to index a file
/// at a time. Tantivy's own minimum is 15MB.
pub const SINGLE_FILE_WRITER_HEAP_BYTES: usize = 15_000_000;

/// The writer heap a bulk build gets, from config, never below tantivy's own minimum
pub(crate) fn bulk_writer_heap(config: &IndexerConfig) -> usize {
    (config.writer_heap_mb * 1_000_000).max(SINGLE_FILE_WRITER_HEAP_BYTES)
}

/// Open an index writer, reporting a held lock as contention rather than a raw error.
///
/// Tantivy's writer lock is what keeps two processes from writing the same index at
/// once, so a busy lock means another build, watch, or dashboard session owns it. The
/// caller has to back off: stealing the lock puts two live writers on one index.
pub(crate) fn open_index_writer(index: &Index, heap_size: usize) -> Result<IndexWriter> {
    index.writer(heap_size).map_err(|e| {
        if matches!(
            e,
            tantivy::TantivyError::LockFailure(tantivy::directory::error::LockError::LockBusy, _)
        ) {
            YgrepError::IndexLocked
        } else {
            e.into()
        }
    })
}

/// Size and modification time a caller already read, so the indexer doesn't stat again
#[derive(Debug, Clone, Copy)]
pub struct FileMeta {
    pub size: u64,
    pub mtime: u64,
}

impl FileMeta {
    /// Read a file's size and mtime
    pub fn read(path: &Path) -> Result<Self> {
        let metadata = std::fs::metadata(path)?;
        let mtime = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);

        Ok(Self {
            size: metadata.len(),
            mtime,
        })
    }
}

/// Handles indexing of files and content
pub struct Indexer {
    config: IndexerConfig,
    index: Index,
    writer: Arc<RwLock<IndexWriter>>,
    fields: SchemaFields,
    workspace_root: String,
    index_chunks_by_default: bool,
    seen_content_hashes: Arc<RwLock<HashSet<u64>>>,
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
        let heap = bulk_writer_heap(&config);
        Self::build(config, index, workspace_root, heap, false, false)
    }

    /// Create an indexer sized for one file at a time (watch events, single updates)
    pub fn new_single_file(
        config: IndexerConfig,
        index: Index,
        workspace_root: &Path,
    ) -> Result<Self> {
        Self::build(
            config,
            index,
            workspace_root,
            SINGLE_FILE_WRITER_HEAP_BYTES,
            false,
            false,
        )
    }

    /// Create a new indexer with NoMergePolicy (for watch mode — prevents segment merge races)
    pub fn new_no_merge(
        config: IndexerConfig,
        index: Index,
        workspace_root: &Path,
    ) -> Result<Self> {
        Self::build(
            config,
            index,
            workspace_root,
            SINGLE_FILE_WRITER_HEAP_BYTES,
            true,
            false,
        )
    }

    fn build(
        config: IndexerConfig,
        index: Index,
        workspace_root: &Path,
        heap_size: usize,
        no_merge: bool,
        index_chunks_by_default: bool,
    ) -> Result<Self> {
        let writer = open_index_writer(&index, heap_size)?;
        if no_merge {
            writer.set_merge_policy(Box::new(NoMergePolicy));
        }
        let schema = index.schema();
        let fields = SchemaFields::new(&schema);

        Ok(Self {
            config,
            index,
            writer: Arc::new(RwLock::new(writer)),
            fields,
            workspace_root: workspace_root.to_string_lossy().to_string(),
            index_chunks_by_default,
            seen_content_hashes: Arc::new(RwLock::new(HashSet::new())),
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
        let heap = bulk_writer_heap(&config);
        let mut indexer = Self::build(config, index, workspace_root, heap, false, true)?;
        indexer.vector_index = Some(vector_index);
        indexer.embedding_model = Some(embedding_model);
        indexer.embedding_cache = Some(embedding_cache);
        Ok(indexer)
    }

    /// Index a single file
    /// Returns (doc_id, content) so callers can reuse the content without re-reading.
    pub fn index_file(&self, path: &Path) -> Result<(String, String)> {
        self.index_file_with_chunks(path, self.index_chunks_by_default)
    }

    /// Index a single file, optionally adding chunk documents.
    /// Returns (doc_id, content) so callers can reuse the content without re-reading.
    pub fn index_file_with_chunks(
        &self,
        path: &Path,
        index_chunks: bool,
    ) -> Result<(String, String)> {
        self.index_entry(path, FileMeta::read(path)?, index_chunks)
    }

    /// Index a file whose size and mtime the caller already read during the walk.
    /// Returns (doc_id, content) so callers can reuse the content without re-reading.
    pub fn index_entry(
        &self,
        path: &Path,
        meta: FileMeta,
        index_chunks: bool,
    ) -> Result<(String, String)> {
        // Check the size before reading: a multi-gigabyte file rejected after reading it
        // has already cost us its full size in RAM.
        let size = meta.size;
        if size > self.config.max_file_size {
            return Err(YgrepError::FileTooLarge {
                path: path.to_path_buf(),
                size,
                max: self.config.max_file_size,
            });
        }

        let content = self.read_indexable(path, size)?;

        // Generate content hash for deduplication and doc_id
        let content_hash = xxh3_64(content.as_bytes());
        let doc_id = format!("{:016x}", content_hash);
        let is_duplicate_content = self.config.deduplicate && !self.mark_content_seen(content_hash);

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

        let mtime = meta.mtime;
        let line_count = content.lines().count() as u64;

        // Build the document
        let mut doc = TantivyDocument::new();
        doc.add_text(self.fields.doc_id, &doc_id);
        doc.add_text(self.fields.path, &rel_path);
        if let Some(filepath) = self.fields.filepath {
            doc.add_text(filepath, &rel_path);
        }
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

        // Adding a document only needs shared access — tantivy hands it to its own
        // indexing threads — so parallel walkers never queue behind each other. Only a
        // commit takes the writer exclusively.
        let writer = self.writer.read();
        writer.add_document(doc)?;

        // Also create chunks for the file
        let chunk_ids = if index_chunks && !is_duplicate_content {
            self.index_chunks(&content, &doc_id, &rel_path, &writer)?
        } else {
            Vec::new()
        };

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
        #[cfg(not(feature = "embeddings"))]
        {
            let _ = chunk_ids;
        }

        Ok((doc_id, content))
    }

    /// Read a file, rejecting generated content before the whole of it is in memory.
    ///
    /// The minified check runs on the head of the file, which is read first, so a 9MB
    /// bundle costs 64KB instead of 9MB. Files smaller than that head are already fully
    /// read by the time the check runs, so nothing reads twice.
    fn read_indexable(&self, path: &Path, size: u64) -> Result<String> {
        use std::io::Read;

        let head_limit = classify::MINIFIED_SNIFF_BYTES;
        let mut file = std::fs::File::open(path)?;
        let mut buf = Vec::with_capacity((size as usize).min(head_limit));
        file.by_ref()
            .take(head_limit as u64)
            .read_to_end(&mut buf)?;

        if classify::content_is_minified(&buf, self.config.max_avg_line_length) {
            tracing::debug!("Skipping minified/generated file: {}", path.display());
            return Err(YgrepError::GeneratedFile(path.to_path_buf()));
        }

        if buf.len() == head_limit {
            buf.reserve(size.saturating_sub(head_limit as u64) as usize);
            file.read_to_end(&mut buf)?;
        }

        String::from_utf8(buf).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, e.utf8_error()).into()
        })
    }

    fn mark_content_seen(&self, content_hash: u64) -> bool {
        let mut seen = self.seen_content_hashes.write();
        seen.insert(content_hash)
    }

    /// Index chunks of a file for more granular search
    /// Returns a list of (chunk_id, chunk_content) tuples for embedding generation
    fn index_chunks(
        &self,
        content: &str,
        parent_doc_id: &str,
        path: &str,
        writer: &IndexWriter,
    ) -> Result<Vec<(String, String)>> {
        let lines: Vec<&str> = content.lines().collect();
        let chunk_size = self.config.chunk_size;
        let overlap = self.config.chunk_overlap.min(chunk_size.saturating_sub(1));

        if chunk_size == 0 || lines.len() <= chunk_size {
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
            if let Some(filepath) = self.fields.filepath {
                doc.add_text(filepath, path);
            }
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
        let writer = self.writer.read();
        writer.delete_term(term);
        Ok(())
    }

    /// Delete a document by doc_id
    pub fn delete_by_id(&self, doc_id: &str) -> Result<()> {
        let term = Term::from_field_text(self.fields.doc_id, doc_id);
        let writer = self.writer.read();
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
    fn an_oversize_file_is_rejected_before_it_is_read() {
        let temp_dir = tempdir().unwrap();

        let index_path = temp_dir.path().join("index");
        std::fs::create_dir_all(&index_path).unwrap();
        let schema = build_document_schema();
        let index = Index::create_in_dir(&index_path, schema).unwrap();
        crate::index::register_tokenizers(index.tokenizers());

        let config = IndexerConfig {
            max_file_size: 10,
            ..Default::default()
        };
        let indexer = Indexer::new(config, index, temp_dir.path()).unwrap();

        // Not valid UTF-8, so reading it first would fail with an IO error instead of
        // reporting the size — which is how we know the size check ran first.
        let binary = temp_dir.path().join("blob.rs");
        std::fs::write(&binary, [0xffu8; 4096]).unwrap();

        assert!(matches!(
            indexer.index_file(&binary),
            Err(crate::error::YgrepError::FileTooLarge { .. })
        ));
    }

    #[test]
    fn a_generated_file_is_rejected_from_its_head_alone() {
        let temp_dir = tempdir().unwrap();
        let (indexer, _) = create_test_indexer(temp_dir.path());

        let bundle = temp_dir.path().join("app.bundle.js");
        std::fs::write(&bundle, format!("var a=1;{}\n", "x".repeat(200_000))).unwrap();

        assert!(matches!(
            indexer.index_file(&bundle),
            Err(crate::error::YgrepError::GeneratedFile(_))
        ));

        // Hand-written source of the same size is indexed.
        let source = temp_dir.path().join("main.rs");
        let normal: String = (0..8_000)
            .map(|i| format!("    let x{i} = {i};\n"))
            .collect();
        std::fs::write(&source, &normal).unwrap();

        let (_doc_id, content) = indexer.index_file(&source).unwrap();
        assert_eq!(content.len(), normal.len(), "the whole file must be read");
    }

    #[test]
    fn a_second_writer_reports_contention_instead_of_stealing_the_lock() {
        let temp_dir = tempdir().unwrap();
        let (first, index_path) = create_test_indexer(temp_dir.path());

        let index = Index::open_in_dir(&index_path).unwrap();
        crate::index::register_tokenizers(index.tokenizers());

        let second = Indexer::new(IndexerConfig::default(), index, temp_dir.path());
        assert!(
            matches!(second, Err(crate::error::YgrepError::IndexLocked)),
            "a second writer must back off, not take the lock"
        );

        // The lockfile the first writer holds must still be there, and that writer must
        // still work.
        assert!(index_path.join(".tantivy-writer.lock").exists());

        let test_file = temp_dir.path().join("test.rs");
        std::fs::write(&test_file, "fn main() {}").unwrap();
        first.index_file(&test_file).unwrap();
        first.commit().unwrap();

        // Once the first writer is gone the lock is free again.
        drop(first);
        let index = Index::open_in_dir(&index_path).unwrap();
        crate::index::register_tokenizers(index.tokenizers());
        assert!(Indexer::new(IndexerConfig::default(), index, temp_dir.path()).is_ok());
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

        indexer.index_file_with_chunks(&test_file, true)?;
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
    fn test_text_indexing_does_not_create_chunks() -> Result<()> {
        let temp_dir = tempdir().unwrap();

        let index_path = temp_dir.path().join("index");
        std::fs::create_dir_all(&index_path).unwrap();
        let schema = build_document_schema();
        let index = Index::create_in_dir(&index_path, schema).unwrap();
        crate::index::register_tokenizers(index.tokenizers());

        let mut config = IndexerConfig::default();
        config.chunk_size = 5;
        config.chunk_overlap = 1;

        let indexer = Indexer::new(config, index.clone(), temp_dir.path()).unwrap();

        let lines: Vec<String> = (1..=15).map(|i| format!("line {} content", i)).collect();
        let test_file = temp_dir.path().join("big.rs");
        std::fs::write(&test_file, lines.join("\n")).unwrap();

        indexer.index_file(&test_file)?;
        indexer.commit()?;

        let reader = index.reader()?;
        let searcher = reader.searcher();
        let all_query = tantivy::query::AllQuery;
        let top_docs =
            searcher.search(&all_query, &tantivy::collector::TopDocs::with_limit(100))?;

        assert_eq!(top_docs.len(), 1, "Text-only indexing should store one doc");
        Ok(())
    }

    #[test]
    fn test_chunk_overlap_cannot_stall_chunking() -> Result<()> {
        let temp_dir = tempdir().unwrap();

        let index_path = temp_dir.path().join("index");
        std::fs::create_dir_all(&index_path).unwrap();
        let schema = build_document_schema();
        let index = Index::create_in_dir(&index_path, schema).unwrap();
        crate::index::register_tokenizers(index.tokenizers());

        let mut config = IndexerConfig::default();
        config.chunk_size = 3;
        config.chunk_overlap = 3;

        let indexer = Indexer::new(config, index.clone(), temp_dir.path()).unwrap();

        let lines: Vec<String> = (1..=8).map(|i| format!("line {}", i)).collect();
        let test_file = temp_dir.path().join("overlap.rs");
        std::fs::write(&test_file, lines.join("\n")).unwrap();

        indexer.index_file_with_chunks(&test_file, true)?;
        indexer.commit()?;

        let reader = index.reader()?;
        let searcher = reader.searcher();
        let all_query = tantivy::query::AllQuery;
        let top_docs =
            searcher.search(&all_query, &tantivy::collector::TopDocs::with_limit(100))?;

        assert!(top_docs.len() > 1, "Expected parent document plus chunks");
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
