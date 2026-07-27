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

/// Count the segments in an index by reading Tantivy's metadata.
///
/// Deliberately avoids opening the index: this runs after every index build to decide
/// whether compaction is due, so it has to cost a single small file read.
///
/// Returns `None` when the index has no readable metadata yet.
pub fn segment_count(index_path: &Path) -> Option<usize> {
    let json = std::fs::read_to_string(index_path.join("meta.json")).ok()?;
    let meta: serde_json::Value = serde_json::from_str(&json).ok()?;

    meta.get("segments")?
        .as_array()
        .map(|segments| segments.len())
}

/// Whether an index has accumulated enough segments to be worth compacting.
///
/// Cheap enough to call after every watch-mode commit: it reads one small metadata file
/// and never opens the index.
pub fn compaction_due(index_path: &Path, threshold: usize) -> bool {
    if threshold == 0 {
        return false;
    }

    segment_count(index_path)
        .map(|segments| segments > threshold)
        .unwrap_or(false)
}

/// Compact an index once it has accumulated more segments than `threshold`.
///
/// Editing a file leaves its previous document behind as a tombstone in the old segment,
/// and watch mode commits with merging disabled, so segments only ever accumulate.
/// Returns `None` when compaction wasn't due or couldn't run — an index that stayed
/// fragmented is merely larger than it needs to be, never a reason to fail the caller.
///
/// The caller must not be holding a writer on this index: compaction opens its own.
pub fn auto_compact(index_path: &Path, threshold: usize) -> Option<CompactStats> {
    if !compaction_due(index_path, threshold) {
        return None;
    }

    match compact_index(index_path) {
        Ok(stats) => {
            tracing::debug!(
                "Auto-compacted {} -> {} segments",
                stats.segments_before,
                stats.segments_after
            );
            Some(stats)
        }
        Err(e) => {
            tracing::warn!("Auto-compaction failed for {}: {e}", index_path.display());
            None
        }
    }
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

    let mut writer: tantivy::IndexWriter<tantivy::TantivyDocument> =
        writer::open_index_writer(&index, 50_000_000)?;
    if segment_ids.len() > 1 {
        writer.merge(&segment_ids).wait()?;
    }

    // Let every merge thread finish before collecting anything, including merges an
    // earlier commit in this process scheduled. Collecting while one is still in flight
    // drops its output files from `.managed.json` without deleting them, stranding them
    // on disk where no later garbage collection will ever look again.
    writer.wait_merging_threads()?;

    // A fresh writer now that the index is quiesced, purely to collect what the merges
    // superseded.
    let writer: tantivy::IndexWriter<tantivy::TantivyDocument> =
        writer::open_index_writer(&index, 50_000_000)?;
    writer.garbage_collect_files().wait()?;
    writer.wait_merging_threads()?;

    // Recover anything stranded by a previous run. Tantivy only deletes files it still
    // manages, so files orphaned this way are invisible to it forever.
    remove_orphaned_segment_files(index_path)?;

    let reader = index.reader()?;
    let segments_after = reader.searcher().segment_readers().len();

    Ok(CompactStats {
        segments_before,
        segments_after,
    })
}

/// Delete segment files that no live segment refers to.
///
/// Every segment file is named `<segment-id>.<ext>`. Any such file whose id is absent
/// from `meta.json` belongs to a segment that has been merged away, so it holds no
/// reachable data. Only called with the writer lock released and the index settled.
fn remove_orphaned_segment_files(index_path: &Path) -> crate::Result<u64> {
    const SEGMENT_EXTENSIONS: &[&str] = &[
        "store",
        "pos",
        "idx",
        "term",
        "fast",
        "fieldnorm",
        "del",
        "positions",
    ];

    let Some(meta) = std::fs::read_to_string(index_path.join("meta.json"))
        .ok()
        .and_then(|json| serde_json::from_str::<serde_json::Value>(&json).ok())
    else {
        return Ok(0);
    };

    // Segment ids appear hyphenated in meta.json and unhyphenated in filenames.
    let live: std::collections::HashSet<String> = meta
        .get("segments")
        .and_then(|s| s.as_array())
        .map(|segments| {
            segments
                .iter()
                .filter_map(|s| s.get("segment_id")?.as_str())
                .map(|id| id.replace('-', "").to_lowercase())
                .collect()
        })
        .unwrap_or_default();

    let mut reclaimed = 0u64;

    for entry in std::fs::read_dir(index_path)?.flatten() {
        let path = entry.path();

        let Some(extension) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        if !SEGMENT_EXTENSIONS.contains(&extension) {
            continue;
        }

        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        // Segment ids are 32 hex characters; anything else isn't ours to delete.
        if stem.len() != 32 || !stem.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        if live.contains(&stem.to_lowercase()) {
            continue;
        }

        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        match std::fs::remove_file(&path) {
            Ok(()) => {
                tracing::debug!("Removed orphaned segment file {}", path.display());
                reclaimed += size;
            }
            Err(e) => tracing::warn!("Could not remove {}: {e}", path.display()),
        }
    }

    Ok(reclaimed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tantivy::doc;
    use tempfile::tempdir;

    #[test]
    fn segment_count_reads_metadata_without_opening_the_index() -> crate::Result<()> {
        let temp_dir = tempdir().unwrap();
        let schema = build_document_schema();
        let index = tantivy::Index::create_in_dir(temp_dir.path(), schema.clone())?;
        register_tokenizers(index.tokenizers());
        let fields = SchemaFields::new(&schema);

        // A fresh index has no segments yet.
        assert_eq!(segment_count(temp_dir.path()), Some(0));

        // Each separate writer commit produces its own segment.
        for i in 0..3 {
            let mut writer = index.writer(50_000_000)?;
            writer.set_merge_policy(Box::new(tantivy::merge_policy::NoMergePolicy));
            writer.add_document(doc!(
                fields.doc_id => format!("doc-{i}"),
                fields.path => format!("src/{i}.rs"),
                fields.filepath.unwrap() => format!("src/{i}.rs"),
                fields.workspace => "/test",
                fields.content => format!("fn seg_{i}() {{}}"),
                fields.mtime => 0u64,
                fields.size => 0u64,
                fields.extension => "rs",
                fields.line_start => 0u64,
                fields.line_end => 0u64,
                fields.chunk_id => "",
                fields.parent_doc => "",
            ))?;
            writer.commit()?;
        }

        assert_eq!(segment_count(temp_dir.path()), Some(3));

        Ok(())
    }

    #[test]
    fn compaction_removes_orphaned_segment_files_and_keeps_live_data() -> crate::Result<()> {
        let temp_dir = tempdir().unwrap();
        let schema = build_document_schema();
        let index = tantivy::Index::create_in_dir(temp_dir.path(), schema.clone())?;
        register_tokenizers(index.tokenizers());
        let fields = SchemaFields::new(&schema);

        for i in 0..3 {
            let mut writer = index.writer(50_000_000)?;
            writer.set_merge_policy(Box::new(tantivy::merge_policy::NoMergePolicy));
            writer.add_document(doc!(
                fields.doc_id => format!("doc-{i}"),
                fields.path => format!("src/{i}.rs"),
                fields.filepath.unwrap() => format!("src/{i}.rs"),
                fields.workspace => "/test",
                fields.content => format!("fn orphan_{i}() {{}}"),
                fields.mtime => 0u64,
                fields.size => 0u64,
                fields.extension => "rs",
                fields.line_start => 0u64,
                fields.line_end => 0u64,
                fields.chunk_id => "",
                fields.parent_doc => "",
            ))?;
            writer.commit()?;
        }

        // A merge that left its inputs behind: files named like a segment, referenced by
        // nothing. Tantivy's own collection can't see these once they leave .managed.json.
        let orphan = temp_dir
            .path()
            .join("ffffffffffffffffffffffffffffffff.store");
        std::fs::write(&orphan, vec![0u8; 4096])?;
        let orphan_term = temp_dir
            .path()
            .join("ffffffffffffffffffffffffffffffff.term");
        std::fs::write(&orphan_term, vec![0u8; 2048])?;

        // A file that merely looks similar must survive.
        let bystander = temp_dir.path().join("notasegment.store");
        std::fs::write(&bystander, b"keep me")?;

        compact_index(temp_dir.path())?;

        assert!(!orphan.exists(), "orphaned segment file must be removed");
        assert!(
            !orphan_term.exists(),
            "all orphan extensions must be removed"
        );
        assert!(bystander.exists(), "non-segment files must be left alone");

        // The documents themselves must still be there.
        let index = tantivy::Index::open_in_dir(temp_dir.path())?;
        register_tokenizers(index.tokenizers());
        let searcher = index.reader()?.searcher();
        let count = searcher.search(&tantivy::query::AllQuery, &tantivy::collector::Count)?;
        assert_eq!(count, 3, "compaction must not lose documents");

        Ok(())
    }

    #[test]
    fn segment_count_is_none_without_metadata() {
        let temp_dir = tempdir().unwrap();
        assert_eq!(segment_count(temp_dir.path()), None);
    }

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
