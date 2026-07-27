pub mod readonly_dir;
pub mod schema;
#[cfg(feature = "embeddings")]
pub mod vector;
pub mod writer;

use std::path::Path;

pub use readonly_dir::ReadOnlyDirectory;
pub use schema::{
    build_document_schema, fields, index_settings, register_tokenizers, SchemaFields,
    CODE_TOKENIZER, DEFAULT_DOCSTORE_COMPRESSION_LEVEL, MAX_DOCSTORE_COMPRESSION_LEVEL,
    MIN_DOCSTORE_COMPRESSION_LEVEL, SCHEMA_VERSION,
};
#[cfg(feature = "embeddings")]
pub use vector::VectorIndex;
pub use writer::Indexer;

#[derive(Debug, Clone, Copy)]
pub struct CompactStats {
    pub segments_before: usize,
    pub segments_after: usize,
}

pub fn compact_index(index_path: &Path) -> crate::Result<CompactStats> {
    let index = tantivy::Index::open_in_dir(index_path)?;
    register_tokenizers(index.tokenizers());

    let reader = index.reader()?;
    let searcher = reader.searcher();
    let segment_ids: Vec<_> = searcher
        .segment_readers()
        .iter()
        .map(|segment_reader| segment_reader.segment_id())
        .collect();
    let segments_before = segment_ids.len();
    drop(searcher);
    drop(reader);

    let mut writer: tantivy::IndexWriter<tantivy::TantivyDocument> = index.writer(50_000_000)?;
    if segment_ids.len() > 1 {
        writer.merge(&segment_ids).wait()?;
    }
    writer.garbage_collect_files().wait()?;
    writer.wait_merging_threads()?;

    let reader = index.reader()?;
    let segments_after = reader.searcher().segment_readers().len();

    Ok(CompactStats {
        segments_before,
        segments_after,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tantivy::doc;
    use tempfile::tempdir;

    #[test]
    fn test_compact_index_merges_segments() -> crate::Result<()> {
        let temp_dir = tempdir().unwrap();
        let schema = build_document_schema();
        let index = tantivy::Index::create_in_dir(temp_dir.path(), schema.clone())?;
        register_tokenizers(index.tokenizers());
        let fields = SchemaFields::new(&schema);

        for i in 0..2 {
            let mut writer = index.writer(50_000_000)?;
            writer.add_document(doc!(
                fields.doc_id => format!("doc-{i}"),
                fields.path => format!("src/{i}.rs"),
                fields.filepath.unwrap() => format!("src/{i}.rs"),
                fields.workspace => "/test",
                fields.content => format!("fn compact_{i}() {{}}"),
                fields.mtime => 0u64,
                fields.size => 0u64,
                fields.extension => "rs",
                fields.line_start => 1u64,
                fields.line_end => 1u64,
                fields.chunk_id => "",
                fields.parent_doc => ""
            ))?;
            writer.commit()?;
        }

        let stats = compact_index(temp_dir.path())?;
        assert!(stats.segments_before >= stats.segments_after);

        let index = tantivy::Index::open_in_dir(temp_dir.path())?;
        register_tokenizers(index.tokenizers());
        let reader = index.reader()?;
        let searcher = reader.searcher();
        let all_query = tantivy::query::AllQuery;
        let docs = searcher.search(&all_query, &tantivy::collector::Count)?;
        assert_eq!(docs, 2);

        Ok(())
    }
}
