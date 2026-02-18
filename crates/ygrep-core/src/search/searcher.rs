use regex::RegexBuilder;
use std::collections::HashSet;
use std::time::Instant;
use tantivy::{collector::TopDocs, query::QueryParser, Index};

use super::results::{MatchType, SearchHit, SearchResult};
use crate::config::SearchConfig;
use crate::error::Result;
use crate::index::schema::SchemaFields;

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
    pub fn search(
        &self,
        query: &str,
        limit: Option<usize>,
        case_sensitive: bool,
        context_before: Option<usize>,
        context_after: Option<usize>,
    ) -> Result<SearchResult> {
        let start = Instant::now();
        let limit = limit
            .unwrap_or(self.config.default_limit)
            .min(self.config.max_limit);

        // Get a reader (with retry for META_LOCK contention, issue #7)
        let reader = super::open_reader_with_retry(&self.index)?;
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
                text_hits: 0,
                semantic_hits: 0,
            });
        }

        // Search for the extracted terms
        let tantivy_query_str = search_terms.join(" ");
        let (tantivy_query, _errors) = query_parser.parse_query_lenient(&tantivy_query_str);

        // Fetch more results since we'll filter them down
        let fetch_limit = limit * 50;
        let top_docs = searcher.search(&tantivy_query, &TopDocs::with_limit(fetch_limit))?;

        // Build results
        let mut hits = Vec::with_capacity(top_docs.len());
        let max_score = top_docs.first().map(|(score, _)| *score).unwrap_or(1.0);
        let mut seen: HashSet<(String, u64, u64)> = HashSet::new();

        // Prepare query for matching
        let query_normalized = if case_sensitive {
            query.to_string()
        } else {
            query.to_lowercase()
        };
        let query_terms: Vec<&str> = query_normalized.split_whitespace().collect();
        let is_multi_word = query_terms.len() > 1;

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
            let chunk_id = extract_text(&doc, self.fields.chunk_id).unwrap_or_default();

            let content_normalized = if case_sensitive {
                content.clone()
            } else {
                content.to_lowercase()
            };

            // LITERAL GREP-LIKE FILTER: exact phrase match, or AND match for multi-word queries
            let exact_match = content_normalized.contains(&query_normalized);
            let and_match = is_multi_word
                && query_terms
                    .iter()
                    .all(|term| content_normalized.contains(term));
            if !exact_match && !and_match {
                continue;
            }

            // Normalize score to 0-1 range
            let normalized_score = if max_score > 0.0 {
                score / max_score
            } else {
                0.0
            };

            // Create snippet showing lines that match the query
            let (snippet, snippet_offset, snippet_line_count, match_line_offset) =
                create_relevant_snippet(&content, query, 10, context_before, context_after);

            // Adjust line numbers to reflect where the snippet is in the file
            let actual_line_start = line_start + snippet_offset as u64;
            let actual_line_end = actual_line_start + snippet_line_count.saturating_sub(1) as u64;
            let match_line_in_snippet = match_line_offset - snippet_offset;

            // Deduplicate: skip if we already have a hit for the same file and line range
            let key = (path.clone(), actual_line_start, actual_line_end);
            if !seen.insert(key) {
                continue;
            }

            hits.push(SearchHit {
                path,
                line_start: actual_line_start,
                line_end: actual_line_end,
                snippet,
                score: normalized_score,
                is_chunk: !chunk_id.is_empty(),
                doc_id,
                match_type: MatchType::Text,
                match_line_in_snippet,
            });
        }

        let query_time_ms = start.elapsed().as_millis() as u64;
        let text_hits = hits.len();

        Ok(SearchResult {
            total: hits.len(),
            hits,
            query_time_ms,
            text_hits,
            semantic_hits: 0,
        })
    }

    /// Search with filters
    pub fn search_filtered(
        &self,
        query: &str,
        limit: Option<usize>,
        filters: SearchFilters,
        use_regex: bool,
        case_sensitive: bool,
        context_before: Option<usize>,
        context_after: Option<usize>,
    ) -> Result<SearchResult> {
        // Use regex search if requested
        let mut result = if use_regex {
            self.search_regex(
                query,
                Some(limit.unwrap_or(self.config.max_limit) * 2),
                case_sensitive,
                context_before,
                context_after,
            )?
        } else {
            self.search(
                query,
                Some(limit.unwrap_or(self.config.max_limit) * 2),
                case_sensitive,
                context_before,
                context_after,
            )?
        };

        // Apply filters
        if let Some(ref extensions) = filters.extensions {
            result.hits.retain(|hit| {
                if let Some(ext) = std::path::Path::new(&hit.path).extension() {
                    extensions
                        .iter()
                        .any(|e| e.eq_ignore_ascii_case(&ext.to_string_lossy()))
                } else {
                    false
                }
            });
        }

        if let Some(ref paths) = filters.paths {
            result
                .hits
                .retain(|hit| paths.iter().any(|p| path_matches(p, &hit.path)));
        }

        // Re-limit
        let limit = limit
            .unwrap_or(self.config.default_limit)
            .min(self.config.max_limit);
        result.hits.truncate(limit);
        result.total = result.hits.len();

        Ok(result)
    }

    /// Search the index with a regex pattern
    pub fn search_regex(
        &self,
        pattern: &str,
        limit: Option<usize>,
        case_sensitive: bool,
        context_before: Option<usize>,
        context_after: Option<usize>,
    ) -> Result<SearchResult> {
        let start = Instant::now();
        let limit = limit
            .unwrap_or(self.config.default_limit)
            .min(self.config.max_limit);

        // Compile regex (case-insensitive by default unless --case-sensitive)
        let regex = match RegexBuilder::new(pattern)
            .case_insensitive(!case_sensitive)
            .build()
        {
            Ok(r) => r,
            Err(e) => {
                return Err(crate::error::YgrepError::Search(format!(
                    "Invalid regex pattern: {}",
                    e
                )));
            }
        };

        // Get a reader (with retry for META_LOCK contention, issue #7)
        let reader = super::open_reader_with_retry(&self.index)?;
        let searcher = reader.searcher();

        // Build query parser for content field
        let query_parser = QueryParser::for_index(&self.index, vec![self.fields.content]);

        // Extract alphanumeric words from the regex pattern for Tantivy pre-filter
        // This is a rough heuristic - we extract literal parts from the regex
        let search_terms: Vec<&str> = pattern
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|s| !s.is_empty() && s.len() > 1) // Skip single chars (likely regex syntax)
            .collect();

        // If we have searchable terms, use Tantivy to narrow down candidates
        let candidates: Vec<_> = if !search_terms.is_empty() {
            let tantivy_query_str = search_terms.join(" ");
            let (tantivy_query, _errors) = query_parser.parse_query_lenient(&tantivy_query_str);

            // Fetch many candidates since regex might be selective
            let fetch_limit = limit * 100;
            searcher.search(&tantivy_query, &TopDocs::with_limit(fetch_limit))?
        } else {
            // No good search terms - scan all documents
            // This is slow but necessary for patterns like "^#" or ".*"
            let all_query = tantivy::query::AllQuery;
            let fetch_limit = limit * 100;
            searcher.search(&all_query, &TopDocs::with_limit(fetch_limit))?
        };

        // Build results by applying regex filter
        let mut hits = Vec::with_capacity(candidates.len());
        let max_score = candidates.first().map(|(score, _)| *score).unwrap_or(1.0);
        let mut seen: HashSet<(String, u64, u64)> = HashSet::new();

        for (score, doc_address) in candidates {
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
            let chunk_id = extract_text(&doc, self.fields.chunk_id).unwrap_or_default();

            // REGEX FILTER: Only include if content matches the regex
            if !regex.is_match(&content) {
                continue;
            }

            // Normalize score to 0-1 range
            let normalized_score = if max_score > 0.0 {
                score / max_score
            } else {
                0.0
            };

            // Create snippet showing lines that match the regex
            let (snippet, snippet_offset, snippet_line_count, match_line_offset) =
                create_regex_snippet(&content, &regex, 10, context_before, context_after);

            // Adjust line numbers to reflect where the snippet is in the file
            let actual_line_start = line_start + snippet_offset as u64;
            let actual_line_end = actual_line_start + snippet_line_count.saturating_sub(1) as u64;
            let match_line_in_snippet = match_line_offset - snippet_offset;

            // Deduplicate: skip if we already have a hit for the same file and line range
            let key = (path.clone(), actual_line_start, actual_line_end);
            if !seen.insert(key) {
                continue;
            }

            hits.push(SearchHit {
                path,
                line_start: actual_line_start,
                line_end: actual_line_end,
                snippet,
                score: normalized_score,
                is_chunk: !chunk_id.is_empty(),
                doc_id,
                match_type: MatchType::Text,
                match_line_in_snippet,
            });
        }

        let query_time_ms = start.elapsed().as_millis() as u64;
        let text_hits = hits.len();

        Ok(SearchResult {
            total: hits.len(),
            hits,
            query_time_ms,
            text_hits,
            semantic_hits: 0,
        })
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
/// Returns (snippet, snippet_offset, line_count, match_line_offset)
/// - snippet_offset: 0-based line index where snippet starts in the chunk
/// - match_line_offset: 0-based line index of the actual match in the chunk
fn create_relevant_snippet(
    content: &str,
    query: &str,
    max_lines: usize,
    ctx_before: Option<usize>,
    ctx_after: Option<usize>,
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
    let context_before = ctx_before.unwrap_or(2);
    let context_after = ctx_after.unwrap_or_else(|| max_lines.saturating_sub(context_before + 1));

    let start = best_match.saturating_sub(context_before);
    let end = (best_match + context_after + 1).min(lines.len());

    let snippet = lines[start..end].join("\n");
    let line_count = end - start;
    (snippet, start, line_count, best_match)
}

/// Create a snippet showing lines relevant to a regex match
/// Returns (snippet, snippet_offset, line_count, match_line_offset)
fn create_regex_snippet(
    content: &str,
    regex: &regex::Regex,
    max_lines: usize,
    ctx_before: Option<usize>,
    ctx_after: Option<usize>,
) -> (String, usize, usize, usize) {
    let lines: Vec<&str> = content.lines().collect();

    // Find lines that match the regex
    let mut matching_indices: Vec<usize> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if regex.is_match(line) {
            matching_indices.push(i);
        }
    }

    if matching_indices.is_empty() {
        // No direct line matches, but document matched - return first lines
        let snippet = lines
            .iter()
            .take(max_lines)
            .copied()
            .collect::<Vec<_>>()
            .join("\n");
        let line_count = snippet.lines().count();
        return (snippet, 0, line_count, 0);
    }

    // Get context around the first match
    let first_match = matching_indices[0];
    let context_before = ctx_before.unwrap_or(2);
    let context_after = ctx_after.unwrap_or_else(|| max_lines.saturating_sub(context_before + 1));

    let start = first_match.saturating_sub(context_before);
    let end = (first_match + context_after + 1).min(lines.len());

    let snippet = lines[start..end].join("\n");
    let line_count = end - start;
    (snippet, start, line_count, first_match)
}

/// Match a path against a pattern, supporting glob wildcards.
///
/// - If the pattern contains `*` or `?`, it is treated as a glob:
///   - `*` matches any characters except `/`
///   - `**` matches any characters including `/`
///   - `?` matches any single character except `/`
/// - Otherwise, falls back to prefix/contains matching.
fn path_matches(pattern: &str, path: &str) -> bool {
    if pattern.contains('*') || pattern.contains('?') {
        glob_to_regex(pattern)
            .map(|re| re.is_match(path))
            .unwrap_or(false)
    } else {
        path.starts_with(pattern) || path.contains(pattern)
    }
}

/// Convert a glob pattern to a compiled regex.
fn glob_to_regex(pattern: &str) -> std::result::Result<regex::Regex, regex::Error> {
    let mut re = String::with_capacity(pattern.len() * 2);
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
            // ** matches anything including /
            re.push_str(".*");
            i += 2;
            // Skip trailing / after **
            if i < chars.len() && chars[i] == '/' {
                re.push_str("/?");
                i += 1;
            }
        } else if chars[i] == '*' {
            // * matches anything except /
            re.push_str("[^/]*");
            i += 1;
        } else if chars[i] == '?' {
            re.push_str("[^/]");
            i += 1;
        } else {
            // Escape regex metacharacters
            let ch = chars[i];
            if ".+(){}[]^$|\\".contains(ch) {
                re.push('\\');
            }
            re.push(ch);
            i += 1;
        }
    }

    RegexBuilder::new(&re).case_insensitive(true).build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::schema::build_document_schema;
    use tantivy::doc;
    use tempfile::tempdir;

    /// Helper: create an index with the code tokenizer registered
    fn create_test_index(path: &std::path::Path) -> (Index, SchemaFields) {
        let schema = build_document_schema();
        let index = Index::create_in_dir(path, schema.clone()).unwrap();
        crate::index::register_tokenizers(index.tokenizers());
        let fields = SchemaFields::new(&schema);
        (index, fields)
    }

    /// Helper: add a document to an index
    fn add_doc(
        index: &Index,
        fields: &SchemaFields,
        doc_id: &str,
        path: &str,
        content: &str,
        ext: &str,
    ) {
        let mut writer = index.writer(50_000_000).unwrap();
        writer
            .add_document(doc!(
                fields.doc_id => doc_id,
                fields.path => path,
                fields.workspace => "/test",
                fields.content => content,
                fields.mtime => 0u64,
                fields.size => content.len() as u64,
                fields.extension => ext,
                fields.line_start => 1u64,
                fields.line_end => content.lines().count() as u64,
                fields.chunk_id => "",
                fields.parent_doc => ""
            ))
            .unwrap();
        writer.commit().unwrap();
    }

    #[test]
    fn test_basic_search() -> Result<()> {
        let temp_dir = tempdir().unwrap();
        let (index, fields) = create_test_index(temp_dir.path());
        add_doc(
            &index,
            &fields,
            "test1",
            "src/main.rs",
            "fn main() { println!(\"Hello, world!\"); }",
            "rs",
        );

        let config = SearchConfig::default();
        let searcher = Searcher::new(config, index);
        let result = searcher.search("hello", None, false, None, None)?;

        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].path, "src/main.rs");

        Ok(())
    }

    #[test]
    fn test_case_insensitive_search() -> Result<()> {
        let temp_dir = tempdir().unwrap();
        let (index, fields) = create_test_index(temp_dir.path());
        add_doc(
            &index,
            &fields,
            "test1",
            "src/lib.rs",
            "fn greet() { println!(\"Hello World\"); }",
            "rs",
        );

        let config = SearchConfig::default();
        let searcher = Searcher::new(config, index);

        // Uppercase query should find mixed-case content
        let result = searcher.search("HELLO", None, false, None, None)?;
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].path, "src/lib.rs");

        Ok(())
    }

    #[test]
    fn test_empty_query_returns_empty() -> Result<()> {
        let temp_dir = tempdir().unwrap();
        let (index, fields) = create_test_index(temp_dir.path());
        add_doc(
            &index,
            &fields,
            "test1",
            "src/main.rs",
            "fn main() {}",
            "rs",
        );

        let config = SearchConfig::default();
        let searcher = Searcher::new(config, index);

        // Queries with no searchable terms should return empty
        let result = searcher.search("...", None, false, None, None)?;
        assert!(result.is_empty());

        Ok(())
    }

    #[test]
    fn test_regex_search_basic() -> Result<()> {
        let temp_dir = tempdir().unwrap();
        let (index, fields) = create_test_index(temp_dir.path());
        add_doc(
            &index,
            &fields,
            "test1",
            "src/main.rs",
            "fn hello_world() {\n    println!(\"Hello!\");\n}",
            "rs",
        );

        let config = SearchConfig::default();
        let searcher = Searcher::new(config, index);

        let result = searcher.search_regex("hello.*world", None, false, None, None)?;
        assert_eq!(result.hits.len(), 1);

        Ok(())
    }

    #[test]
    fn test_regex_search_invalid_returns_error() {
        let temp_dir = tempdir().unwrap();
        let (index, _fields) = create_test_index(temp_dir.path());

        let config = SearchConfig::default();
        let searcher = Searcher::new(config, index);

        let result = searcher.search_regex("[invalid", None, false, None, None);
        assert!(result.is_err());
    }

    #[test]
    fn test_search_extension_filter() -> Result<()> {
        let temp_dir = tempdir().unwrap();
        let (index, fields) = create_test_index(temp_dir.path());
        add_doc(
            &index,
            &fields,
            "test1",
            "src/main.rs",
            "fn hello() {}",
            "rs",
        );
        add_doc(
            &index,
            &fields,
            "test2",
            "src/main.py",
            "def hello(): pass",
            "py",
        );

        let config = SearchConfig::default();
        let searcher = Searcher::new(config, index);

        let filters = SearchFilters {
            extensions: Some(vec!["rs".to_string()]),
            paths: None,
        };
        let result = searcher.search_filtered("hello", None, filters, false, false, None, None)?;

        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].path, "src/main.rs");

        Ok(())
    }

    #[test]
    fn test_search_path_filter() -> Result<()> {
        let temp_dir = tempdir().unwrap();
        let (index, fields) = create_test_index(temp_dir.path());
        add_doc(
            &index,
            &fields,
            "test1",
            "src/main.rs",
            "fn hello() {}",
            "rs",
        );
        add_doc(
            &index,
            &fields,
            "test2",
            "lib/utils.rs",
            "fn hello() {}",
            "rs",
        );

        let config = SearchConfig::default();
        let searcher = Searcher::new(config, index);

        let filters = SearchFilters {
            extensions: None,
            paths: Some(vec!["lib/".to_string()]),
        };
        let result = searcher.search_filtered("hello", None, filters, false, false, None, None)?;

        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].path, "lib/utils.rs");

        Ok(())
    }

    #[test]
    fn test_path_matches_glob() {
        // Plain prefix/contains (no wildcards)
        assert!(path_matches("src/", "src/main.rs"));
        assert!(path_matches("src/", "project/src/main.rs"));
        assert!(!path_matches("lib/", "src/main.rs"));

        // Single * matches within one path segment
        assert!(path_matches("src/*/tests/", "src/api/tests/foo.rs"));
        assert!(path_matches("src/*/tests/", "src/core/tests/bar.rs"));
        assert!(!path_matches("src/*/tests/", "src/a/b/tests/foo.rs"));

        // ** matches across segments
        assert!(path_matches("**/tests/", "src/api/tests/foo.rs"));
        assert!(path_matches("**/tests/", "deep/nested/tests/bar.rs"));
        assert!(path_matches("src/**/test.rs", "src/a/b/c/test.rs"));

        // ? matches single character
        assert!(path_matches("src/?.rs", "src/a.rs"));
        assert!(!path_matches("src/?.rs", "src/ab.rs"));

        // Glob patterns are case-insensitive
        assert!(path_matches("SRC/*/tests/", "src/api/tests/foo.rs"));
        // Plain prefix matching is case-sensitive (existing behavior)
        assert!(!path_matches("SRC/", "src/main.rs"));
    }

    #[test]
    fn test_search_path_filter_glob() -> Result<()> {
        let temp_dir = tempdir().unwrap();
        let (index, fields) = create_test_index(temp_dir.path());
        add_doc(
            &index,
            &fields,
            "test1",
            "user/plugins/impersonate/tests/test.php",
            "class FooTest extends Plugin {}",
            "php",
        );
        add_doc(
            &index,
            &fields,
            "test2",
            "user/plugins/impersonate/src/plugin.php",
            "class Plugin extends Base {}",
            "php",
        );
        add_doc(
            &index,
            &fields,
            "test3",
            "user/plugins/auth/tests/test.php",
            "class BarTest extends Plugin {}",
            "php",
        );

        let config = SearchConfig::default();
        let searcher = Searcher::new(config, index);

        // Glob pattern should match only files in tests/ directories
        let filters = SearchFilters {
            extensions: None,
            paths: Some(vec!["user/plugins/*/tests/".to_string()]),
        };
        let result =
            searcher.search_filtered("extends Plugin", None, filters, false, false, None, None)?;

        assert_eq!(result.hits.len(), 2);
        assert!(result.hits.iter().all(|h| h.path.contains("/tests/")));

        Ok(())
    }

    #[test]
    fn test_multiple_results_ordered_by_score() -> Result<()> {
        let temp_dir = tempdir().unwrap();
        let (index, fields) = create_test_index(temp_dir.path());

        // Document with more occurrences of "hello" should score higher
        add_doc(
            &index,
            &fields,
            "test1",
            "src/many.rs",
            "hello hello hello hello hello",
            "rs",
        );
        add_doc(
            &index,
            &fields,
            "test2",
            "src/one.rs",
            "hello world goodbye",
            "rs",
        );

        let config = SearchConfig::default();
        let searcher = Searcher::new(config, index);
        let result = searcher.search("hello", None, false, None, None)?;

        assert!(result.hits.len() >= 2);
        // Results should be ordered by score descending
        for pair in result.hits.windows(2) {
            assert!(pair[0].score >= pair[1].score);
        }

        Ok(())
    }

    #[test]
    fn test_dedup_full_doc_and_chunk() -> Result<()> {
        let temp_dir = tempdir().unwrap();
        let (index, fields) = create_test_index(temp_dir.path());

        let content = "fn hello() {\n    println!(\"Hello, world!\");\n}";

        // Add full document (empty chunk_id)
        let mut writer = index.writer(50_000_000).unwrap();
        writer
            .add_document(doc!(
                fields.doc_id => "full-doc",
                fields.path => "src/main.rs",
                fields.workspace => "/test",
                fields.content => content,
                fields.mtime => 0u64,
                fields.size => content.len() as u64,
                fields.extension => "rs",
                fields.line_start => 1u64,
                fields.line_end => 3u64,
                fields.chunk_id => "",
                fields.parent_doc => ""
            ))
            .unwrap();
        // Add chunk with same content for the same file
        writer
            .add_document(doc!(
                fields.doc_id => "chunk-1",
                fields.path => "src/main.rs",
                fields.workspace => "/test",
                fields.content => content,
                fields.mtime => 0u64,
                fields.size => content.len() as u64,
                fields.extension => "rs",
                fields.line_start => 1u64,
                fields.line_end => 3u64,
                fields.chunk_id => "chunk-1",
                fields.parent_doc => "full-doc"
            ))
            .unwrap();
        writer.commit().unwrap();

        let config = SearchConfig::default();
        let searcher = Searcher::new(config, index);

        // Text search should return only 1 hit (deduplicated)
        let result = searcher.search("hello", None, false, None, None)?;
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].path, "src/main.rs");

        // Regex search should also return only 1 hit
        let result = searcher.search_regex("hello", None, false, None, None)?;
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].path, "src/main.rs");

        Ok(())
    }
}
