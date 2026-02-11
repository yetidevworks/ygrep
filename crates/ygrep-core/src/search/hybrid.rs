//! Hybrid search combining BM25 and vector search using Reciprocal Rank Fusion

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use tantivy::{collector::TopDocs, query::QueryParser, Index};

use super::results::{MatchType, SearchHit, SearchResult};
use crate::config::SearchConfig;
use crate::embeddings::{EmbeddingCache, EmbeddingModel};
use crate::error::Result;
use crate::index::schema::SchemaFields;
use crate::index::VectorIndex;

/// Hybrid searcher combining BM25 text search and vector similarity search
pub struct HybridSearcher {
    config: SearchConfig,
    index: Index,
    fields: SchemaFields,
    vector_index: Arc<VectorIndex>,
    embedding_model: Arc<EmbeddingModel>,
    embedding_cache: Arc<EmbeddingCache>,
}

impl HybridSearcher {
    /// Create a new hybrid searcher
    pub fn new(
        config: SearchConfig,
        index: Index,
        vector_index: Arc<VectorIndex>,
        embedding_model: Arc<EmbeddingModel>,
        embedding_cache: Arc<EmbeddingCache>,
    ) -> Self {
        let schema = index.schema();
        let fields = SchemaFields::new(&schema);

        Self {
            config,
            index,
            fields,
            vector_index,
            embedding_model,
            embedding_cache,
        }
    }

    /// Perform hybrid search combining BM25 and vector search
    pub fn search(&self, query: &str, limit: Option<usize>) -> Result<SearchResult> {
        let start = Instant::now();
        let limit = limit
            .unwrap_or(self.config.default_limit)
            .min(self.config.max_limit);

        // Fetch more results from each method for better fusion
        let fetch_limit = limit * 3;

        // Run BM25 search
        let bm25_results = self.bm25_search(query, fetch_limit)?;

        // Run vector search
        let vector_results = self.vector_search(query, fetch_limit)?;

        // Fuse results using Reciprocal Rank Fusion
        let fused = self.reciprocal_rank_fusion(
            bm25_results,
            vector_results,
            self.config.bm25_weight,
            self.config.vector_weight,
            query,
        );

        // Take top results
        // Note: RRF scores are typically small (max ~0.016 with K=60), so we don't apply min_score filter
        let hits: Vec<SearchHit> = fused.into_iter().take(limit).collect();

        // Count text vs semantic hits
        let text_hits = hits
            .iter()
            .filter(|h| matches!(h.match_type, MatchType::Text | MatchType::Hybrid))
            .count();
        let semantic_hits = hits
            .iter()
            .filter(|h| matches!(h.match_type, MatchType::Semantic | MatchType::Hybrid))
            .count();

        let query_time_ms = start.elapsed().as_millis() as u64;

        Ok(SearchResult {
            total: hits.len(),
            hits,
            query_time_ms,
            text_hits,
            semantic_hits,
        })
    }

    /// BM25 full-text search
    fn bm25_search(&self, query: &str, limit: usize) -> Result<Vec<RankedResult>> {
        let reader = self.index.reader()?;
        let searcher = reader.searcher();

        let query_parser = QueryParser::for_index(&self.index, vec![self.fields.content]);

        // Wrap query in quotes for literal phrase matching (like grep)
        let quoted_query = format!("\"{}\"", query.replace('"', "\\\""));
        let (tantivy_query, _errors) = query_parser.parse_query_lenient(&quoted_query);

        let top_docs = searcher.search(&tantivy_query, &TopDocs::with_limit(limit))?;

        let mut results = Vec::with_capacity(top_docs.len());

        for (rank, (score, doc_address)) in top_docs.iter().enumerate() {
            let doc = searcher.doc(*doc_address)?;

            let path = extract_text(&doc, self.fields.path).unwrap_or_default();
            let doc_id = extract_text(&doc, self.fields.doc_id).unwrap_or_default();
            let content = extract_text(&doc, self.fields.content).unwrap_or_default();
            let line_start = extract_u64(&doc, self.fields.line_start).unwrap_or(1);
            let chunk_id = extract_text(&doc, self.fields.chunk_id).unwrap_or_default();

            results.push(RankedResult {
                doc_id: doc_id.clone(),
                path,
                content,
                line_start,
                is_chunk: !chunk_id.is_empty(),
                rank: rank + 1,
                score: *score,
            });
        }

        Ok(results)
    }

    /// Vector similarity search
    fn vector_search(&self, query: &str, limit: usize) -> Result<Vec<RankedResult>> {
        // Check if vector index has data
        if self.vector_index.is_empty() {
            return Ok(vec![]);
        }

        // Get or compute query embedding
        let query_embedding = self.embedding_cache.get_or_insert(query, || {
            self.embedding_model
                .embed(query)
                .unwrap_or_else(|_| vec![0.0; 384])
        });

        // Search vector index
        let neighbors = self.vector_index.search(&query_embedding, limit)?;

        // Look up full document info from tantivy
        let reader = self.index.reader()?;
        let searcher = reader.searcher();

        let mut results = Vec::with_capacity(neighbors.len());

        for (rank, (_, distance, doc_id)) in neighbors.iter().enumerate() {
            // Find document by doc_id in tantivy
            if let Some(hit) = self.lookup_by_doc_id(&searcher, doc_id)? {
                results.push(RankedResult {
                    doc_id: doc_id.clone(),
                    path: hit.path,
                    content: hit.content,
                    line_start: hit.line_start,
                    is_chunk: hit.is_chunk,
                    rank: rank + 1,
                    score: 1.0 / (1.0 + distance), // Convert distance to similarity
                });
            }
        }

        Ok(results)
    }

    /// Look up document by doc_id
    fn lookup_by_doc_id(
        &self,
        searcher: &tantivy::Searcher,
        doc_id: &str,
    ) -> Result<Option<DocInfo>> {
        use tantivy::query::TermQuery;
        use tantivy::schema::IndexRecordOption;
        use tantivy::Term;

        let term = Term::from_field_text(self.fields.doc_id, doc_id);
        let query = TermQuery::new(term, IndexRecordOption::Basic);

        let top_docs = searcher.search(&query, &TopDocs::with_limit(1))?;

        if let Some((_, doc_address)) = top_docs.first() {
            let doc = searcher.doc(*doc_address)?;

            Ok(Some(DocInfo {
                path: extract_text(&doc, self.fields.path).unwrap_or_default(),
                content: extract_text(&doc, self.fields.content).unwrap_or_default(),
                line_start: extract_u64(&doc, self.fields.line_start).unwrap_or(1),
                is_chunk: !extract_text(&doc, self.fields.chunk_id)
                    .unwrap_or_default()
                    .is_empty(),
            }))
        } else {
            Ok(None)
        }
    }

    /// Reciprocal Rank Fusion to combine results from multiple retrieval methods
    fn reciprocal_rank_fusion(
        &self,
        bm25_results: Vec<RankedResult>,
        vector_results: Vec<RankedResult>,
        bm25_weight: f32,
        vector_weight: f32,
        query: &str,
    ) -> Vec<SearchHit> {
        const K: f32 = 60.0; // RRF constant

        let mut combined_scores: HashMap<String, FusedScore> = HashMap::new();

        // Add BM25 results (key by path to avoid collapsing files with identical content)
        for result in &bm25_results {
            let rrf_score = bm25_weight / (K + result.rank as f32);
            let entry = combined_scores
                .entry(result.path.clone())
                .or_insert_with(|| FusedScore {
                    result: result.clone(),
                    bm25_rrf: 0.0,
                    vector_rrf: 0.0,
                });
            entry.bm25_rrf = rrf_score;
        }

        // Add vector results (key by path to avoid collapsing files with identical content)
        for result in &vector_results {
            let rrf_score = vector_weight / (K + result.rank as f32);
            let entry = combined_scores
                .entry(result.path.clone())
                .or_insert_with(|| FusedScore {
                    result: result.clone(),
                    bm25_rrf: 0.0,
                    vector_rrf: 0.0,
                });
            entry.vector_rrf = rrf_score;
        }

        // Calculate final scores and convert to SearchHit
        let mut hits: Vec<SearchHit> = combined_scores
            .into_values()
            .map(|fused| {
                let total_score = fused.bm25_rrf + fused.vector_rrf;
                let (snippet, snippet_offset, line_count, match_line_offset) =
                    create_relevant_snippet(&fused.result.content, query, 10);

                // Adjust line numbers to reflect the snippet position
                let actual_line_start = fused.result.line_start + snippet_offset as u64;
                let actual_line_end = actual_line_start + line_count.saturating_sub(1) as u64;
                let match_line_in_snippet = match_line_offset - snippet_offset;

                // Determine match type based on which sources contributed
                let match_type = match (fused.bm25_rrf > 0.0, fused.vector_rrf > 0.0) {
                    (true, true) => MatchType::Hybrid,
                    (true, false) => MatchType::Text,
                    (false, true) => MatchType::Semantic,
                    (false, false) => MatchType::Text, // shouldn't happen
                };

                SearchHit {
                    path: fused.result.path,
                    line_start: actual_line_start,
                    line_end: actual_line_end,
                    snippet,
                    score: total_score,
                    is_chunk: fused.result.is_chunk,
                    doc_id: fused.result.doc_id,
                    match_type,
                    match_line_in_snippet,
                }
            })
            .collect();

        // Sort by score descending
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        hits
    }
}

/// Intermediate result with ranking info
#[derive(Debug, Clone)]
struct RankedResult {
    doc_id: String,
    path: String,
    content: String,
    line_start: u64,
    is_chunk: bool,
    rank: usize,
    #[allow(dead_code)]
    score: f32,
}

/// Document info from lookup
struct DocInfo {
    path: String,
    content: String,
    line_start: u64,
    is_chunk: bool,
}

/// Fused score from multiple retrieval methods
struct FusedScore {
    result: RankedResult,
    bm25_rrf: f32,
    vector_rrf: f32,
}

/// Extract text value from a document
fn extract_text(doc: &tantivy::TantivyDocument, field: tantivy::schema::Field) -> Option<String> {
    doc.get_first(field).and_then(|v| {
        if let tantivy::schema::OwnedValue::Str(s) = v {
            Some(s.to_string())
        } else {
            None
        }
    })
}

/// Extract u64 value from a document
fn extract_u64(doc: &tantivy::TantivyDocument, field: tantivy::schema::Field) -> Option<u64> {
    doc.get_first(field).and_then(|v| {
        if let tantivy::schema::OwnedValue::U64(n) = v {
            Some(*n)
        } else {
            None
        }
    })
}

/// Standalone RRF fusion for testing (same algorithm as HybridSearcher::reciprocal_rank_fusion)
#[cfg(test)]
fn reciprocal_rank_fusion_standalone(
    bm25_results: Vec<RankedResult>,
    vector_results: Vec<RankedResult>,
    bm25_weight: f32,
    vector_weight: f32,
    query: &str,
) -> Vec<SearchHit> {
    const K: f32 = 60.0;
    let mut combined_scores: HashMap<String, FusedScore> = HashMap::new();

    for result in &bm25_results {
        let rrf_score = bm25_weight / (K + result.rank as f32);
        let entry = combined_scores
            .entry(result.path.clone())
            .or_insert_with(|| FusedScore {
                result: result.clone(),
                bm25_rrf: 0.0,
                vector_rrf: 0.0,
            });
        entry.bm25_rrf = rrf_score;
    }

    for result in &vector_results {
        let rrf_score = vector_weight / (K + result.rank as f32);
        let entry = combined_scores
            .entry(result.path.clone())
            .or_insert_with(|| FusedScore {
                result: result.clone(),
                bm25_rrf: 0.0,
                vector_rrf: 0.0,
            });
        entry.vector_rrf = rrf_score;
    }

    let mut hits: Vec<SearchHit> = combined_scores
        .into_values()
        .map(|fused| {
            let total_score = fused.bm25_rrf + fused.vector_rrf;
            let (snippet, snippet_offset, line_count, match_line_offset) =
                create_relevant_snippet(&fused.result.content, query, 10);
            let actual_line_start = fused.result.line_start + snippet_offset as u64;
            let actual_line_end = actual_line_start + line_count.saturating_sub(1) as u64;
            let match_line_in_snippet = match_line_offset - snippet_offset;
            let match_type = match (fused.bm25_rrf > 0.0, fused.vector_rrf > 0.0) {
                (true, true) => MatchType::Hybrid,
                (true, false) => MatchType::Text,
                (false, true) => MatchType::Semantic,
                (false, false) => MatchType::Text,
            };
            SearchHit {
                path: fused.result.path,
                line_start: actual_line_start,
                line_end: actual_line_end,
                snippet,
                score: total_score,
                is_chunk: fused.result.is_chunk,
                doc_id: fused.result.doc_id,
                match_type,
                match_line_in_snippet,
            }
        })
        .collect();

    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    hits
}

/// Create a snippet showing lines relevant to the query
/// Returns (snippet, snippet_offset, line_count, match_line_offset)
fn create_relevant_snippet(
    content: &str,
    query: &str,
    max_lines: usize,
) -> (String, usize, usize, usize) {
    let lines: Vec<&str> = content.lines().collect();
    let query_lower = query.to_lowercase();
    let query_terms: Vec<&str> = query_lower.split_whitespace().collect();

    // Find lines that contain any query term
    let mut matching_indices: Vec<usize> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let line_lower = line.to_lowercase();
        if query_terms.iter().any(|term| line_lower.contains(term)) {
            matching_indices.push(i);
        }
    }

    if matching_indices.is_empty() {
        // No direct matches, return first lines
        let snippet = lines
            .iter()
            .take(max_lines)
            .copied()
            .collect::<Vec<_>>()
            .join("\n");
        let line_count = snippet.lines().count();
        return (snippet, 0, line_count, 0);
    }

    // For multi-word queries, prefer lines with more matching terms
    let best_match = if query_terms.len() > 1 {
        let mut best_line = matching_indices[0];
        let mut best_count = 0;
        for &idx in &matching_indices {
            let line_lower = lines[idx].to_lowercase();
            let count = query_terms
                .iter()
                .filter(|t| line_lower.contains(*t))
                .count();
            if count > best_count {
                best_count = count;
                best_line = idx;
            }
        }
        best_line
    } else {
        matching_indices[0]
    };

    // Get context around the best match
    let context_before = 2;
    let context_after = max_lines.saturating_sub(context_before + 1);

    let start = best_match.saturating_sub(context_before);
    let end = (best_match + context_after + 1).min(lines.len());

    let snippet = lines[start..end].join("\n");
    let line_count = end - start;
    (snippet, start, line_count, best_match)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ranked(doc_id: &str, path: &str, rank: usize, content: &str) -> RankedResult {
        RankedResult {
            doc_id: doc_id.to_string(),
            path: path.to_string(),
            content: content.to_string(),
            line_start: 1,
            is_chunk: false,
            rank,
            score: 1.0 / rank as f32,
        }
    }

    #[test]
    fn test_rrf_both_sources_hybrid() {
        let bm25 = vec![make_ranked("d1", "src/a.rs", 1, "hello world")];
        let vector = vec![make_ranked("d1", "src/a.rs", 1, "hello world")];

        let hits = reciprocal_rank_fusion_standalone(bm25, vector, 0.5, 0.5, "hello");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].match_type, MatchType::Hybrid);
    }

    #[test]
    fn test_rrf_bm25_only_text_type() {
        let bm25 = vec![make_ranked("d1", "src/a.rs", 1, "hello world")];
        let vector: Vec<RankedResult> = vec![];

        let hits = reciprocal_rank_fusion_standalone(bm25, vector, 0.5, 0.5, "hello");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].match_type, MatchType::Text);
        // Score should be bm25_weight / (K + rank) = 0.5 / (60 + 1)
        let expected = 0.5 / 61.0;
        assert!((hits[0].score - expected).abs() < 1e-6);
    }

    #[test]
    fn test_rrf_vector_only_semantic_type() {
        let bm25: Vec<RankedResult> = vec![];
        let vector = vec![make_ranked("d1", "src/a.rs", 1, "hello world")];

        let hits = reciprocal_rank_fusion_standalone(bm25, vector, 0.5, 0.5, "hello");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].match_type, MatchType::Semantic);
    }

    #[test]
    fn test_rrf_sorted_by_score_descending() {
        let bm25 = vec![
            make_ranked("d1", "src/a.rs", 1, "hello"),
            make_ranked("d2", "src/b.rs", 2, "hello"),
        ];
        let vector = vec![make_ranked("d1", "src/a.rs", 1, "hello")];

        let hits = reciprocal_rank_fusion_standalone(bm25, vector, 0.5, 0.5, "hello");
        assert!(hits.len() >= 2);
        // First hit should have higher score (appears in both)
        assert!(hits[0].score >= hits[1].score);
    }

    #[test]
    fn test_rrf_same_path_merged_not_duplicated() {
        // Same path from both sources should produce one hit, not two
        let bm25 = vec![make_ranked("d1", "src/a.rs", 1, "hello world")];
        let vector = vec![make_ranked("d1", "src/a.rs", 2, "hello world")];

        let hits = reciprocal_rank_fusion_standalone(bm25, vector, 0.5, 0.5, "hello");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "src/a.rs");
        // Combined score = bm25_weight/(60+1) + vector_weight/(60+2)
        let expected = 0.5 / 61.0 + 0.5 / 62.0;
        assert!((hits[0].score - expected).abs() < 1e-6);
    }
}
