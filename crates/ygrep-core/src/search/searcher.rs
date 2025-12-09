use std::time::Instant;
use tantivy::{Index, collector::TopDocs, query::QueryParser};

use crate::config::SearchConfig;
use crate::error::Result;
use crate::index::schema::SchemaFields;
use super::results::{SearchResult, SearchHit};

/// Search engine for querying the index
pub struct Searcher {
    config: SearchConfig,
    index: Index,
    fields: SchemaFields,
}

impl Searcher {
    /// Create a new searcher for an index
    pub fn new(config: SearchConfig, index: Index) -> Self {
        let schema = index.schema();
        let fields = SchemaFields::new(&schema);

        Self {
            config,
            index,
            fields,
        }
    }

    /// Search the index with a query string (literal text matching like grep)
    pub fn search(&self, query: &str, limit: Option<usize>) -> Result<SearchResult> {
        let start = Instant::now();
        let limit = limit.unwrap_or(self.config.default_limit).min(self.config.max_limit);

        // Get a reader
        let reader = self.index.reader()?;
        let searcher = reader.searcher();

        // Build query parser for content field
        let query_parser = QueryParser::for_index(&self.index, vec![self.fields.content]);

        // Extract alphanumeric words for Tantivy query (it can't search special chars)
        // Then we'll post-filter for exact literal match
        let search_terms: Vec<&str> = query
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|s| !s.is_empty())
            .collect();

        // If no searchable terms, return empty
        if search_terms.is_empty() {
            return Ok(SearchResult {
                total: 0,
                hits: vec![],
                query_time_ms: start.elapsed().as_millis() as u64,
            });
        }

        // Search for the extracted terms
        let tantivy_query_str = search_terms.join(" ");
        let (tantivy_query, _errors) = query_parser.parse_query_lenient(&tantivy_query_str);

        // Fetch more results since we'll filter them down
        let fetch_limit = limit * 10;
        let top_docs = searcher.search(&tantivy_query, &TopDocs::with_limit(fetch_limit))?;

        // Build results
        let mut hits = Vec::with_capacity(top_docs.len());
        let max_score = top_docs.first().map(|(score, _)| *score).unwrap_or(1.0);

        // Case-insensitive literal matching (like grep -i)
        let query_lower = query.to_lowercase();

        for (score, doc_address) in top_docs {
            // Stop if we have enough results
            if hits.len() >= limit {
                break;
            }

            let doc = searcher.doc(doc_address)?;

            // Extract fields
            let path = extract_text(&doc, self.fields.path).unwrap_or_default();
            let doc_id = extract_text(&doc, self.fields.doc_id).unwrap_or_default();
            let content = extract_text(&doc, self.fields.content).unwrap_or_default();
            let line_start = extract_u64(&doc, self.fields.line_start).unwrap_or(1);
            let line_end = extract_u64(&doc, self.fields.line_end).unwrap_or(1);
            let chunk_id = extract_text(&doc, self.fields.chunk_id).unwrap_or_default();

            // LITERAL GREP-LIKE FILTER: Only include if content contains exact query string
            if !content.to_lowercase().contains(&query_lower) {
                continue;
            }

            // Normalize score to 0-1 range
            let normalized_score = if max_score > 0.0 { score / max_score } else { 0.0 };

            // Create snippet showing lines that match the query
            let snippet = create_relevant_snippet(&content, query, 10);

            hits.push(SearchHit {
                path,
                line_start,
                line_end,
                snippet,
                score: normalized_score,
                is_chunk: !chunk_id.is_empty(),
                doc_id,
            });
        }

        let query_time_ms = start.elapsed().as_millis() as u64;

        Ok(SearchResult {
            total: hits.len(),
            hits,
            query_time_ms,
        })
    }

    /// Search with filters
    pub fn search_filtered(
        &self,
        query: &str,
        limit: Option<usize>,
        filters: SearchFilters,
    ) -> Result<SearchResult> {
        // For now, do basic search and post-filter
        // TODO: Build proper Tantivy query with filters
        let mut result = self.search(query, Some(limit.unwrap_or(self.config.max_limit) * 2))?;

        // Apply filters
        if let Some(ref extensions) = filters.extensions {
            result.hits.retain(|hit| {
                if let Some(ext) = std::path::Path::new(&hit.path).extension() {
                    extensions.iter().any(|e| e.eq_ignore_ascii_case(&ext.to_string_lossy()))
                } else {
                    false
                }
            });
        }

        if let Some(ref paths) = filters.paths {
            result.hits.retain(|hit| {
                paths.iter().any(|p| hit.path.starts_with(p) || hit.path.contains(p))
            });
        }

        // Re-limit
        let limit = limit.unwrap_or(self.config.default_limit).min(self.config.max_limit);
        result.hits.truncate(limit);
        result.total = result.hits.len();

        Ok(result)
    }
}

/// Filters for search
#[derive(Debug, Clone, Default)]
pub struct SearchFilters {
    /// Filter by file extensions (e.g., ["rs", "ts"])
    pub extensions: Option<Vec<String>>,
    /// Filter by path patterns
    pub paths: Option<Vec<String>>,
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

/// Create a snippet showing lines relevant to the query
fn create_relevant_snippet(content: &str, query: &str, max_lines: usize) -> String {
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
        return lines.iter().take(max_lines).copied().collect::<Vec<_>>().join("\n");
    }

    // Get context around the first match
    let first_match = matching_indices[0];
    let context_before = 2;
    let context_after = max_lines.saturating_sub(context_before + 1);

    let start = first_match.saturating_sub(context_before);
    let end = (first_match + context_after + 1).min(lines.len());

    lines[start..end].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::schema::build_document_schema;
    use tantivy::doc;
    use tempfile::tempdir;

    #[test]
    fn test_basic_search() -> Result<()> {
        let temp_dir = tempdir().unwrap();
        let index_path = temp_dir.path();

        // Create index with schema
        let schema = build_document_schema();
        let index = Index::create_in_dir(index_path, schema.clone())?;

        let fields = SchemaFields::new(&schema);

        // Add a test document
        let mut writer = index.writer(50_000_000)?;
        writer.add_document(doc!(
            fields.doc_id => "test1",
            fields.path => "src/main.rs",
            fields.workspace => "/test",
            fields.content => "fn main() { println!(\"Hello, world!\"); }",
            fields.mtime => 0u64,
            fields.size => 100u64,
            fields.extension => "rs",
            fields.line_start => 1u64,
            fields.line_end => 1u64,
            fields.chunk_id => "",
            fields.parent_doc => ""
        ))?;
        writer.commit()?;

        // Search
        let config = SearchConfig::default();
        let searcher = Searcher::new(config, index);
        let result = searcher.search("hello", None)?;

        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].path, "src/main.rs");

        Ok(())
    }
}
