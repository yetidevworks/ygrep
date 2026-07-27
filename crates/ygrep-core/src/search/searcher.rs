use memchr::memchr2;
use regex::RegexBuilder;
use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, BoostQuery, Occur, Query, QueryParser, TermQuery};
use tantivy::schema::{Field, IndexRecordOption};
use tantivy::{Index, TantivyDocument, Term};

use super::results::{MatchType, SearchHit, SearchResult};
use crate::config::SearchConfig;
use crate::error::Result;
use crate::index::schema::{SchemaFields, CODE_TOKENIZER};

/// How many candidates to pull from Tantivy, as a multiple of the result limit.
///
/// The literal filter runs on the stored text, so a query whose best-scoring documents
/// don't contain the literal needs a deeper pool to fill the page. Starting small keeps
/// the common case — where the first handful of candidates already match — cheap, and
/// the second pass only runs when the first came up short. Its ceiling is above what a
/// single fixed pass used to fetch, so nothing that used to be found gets lost.
const LITERAL_FETCH_MULTIPLIERS: [usize; 2] = [5, 100];

/// Same idea for regex searches, which reject candidates more often.
const REGEX_FETCH_MULTIPLIERS: [usize; 2] = [10, 200];

/// Documents below this count are scanned on the calling thread.
const MIN_PARALLEL_SCAN_DOCS: u64 = 4_096;

/// Smallest document range worth handing to its own thread.
const MIN_SCAN_CHUNK: usize = 2_048;

/// Doc-store blocks each scan worker keeps decompressed.
const STORE_CACHE_BLOCKS: usize = 8;

/// How often a scan worker checks whether the ranges ahead of it filled the page.
const QUOTA_CHECK_INTERVAL: usize = 64;

/// File and line range a hit covers, used to drop duplicates
type HitKey = (String, u64, u64);

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
        self.search_literal(
            query,
            limit,
            case_sensitive,
            context_before,
            context_after,
            &CompiledFilters::default(),
        )
    }

    fn search_literal(
        &self,
        query: &str,
        limit: Option<usize>,
        case_sensitive: bool,
        context_before: Option<usize>,
        context_after: Option<usize>,
        filters: &CompiledFilters,
    ) -> Result<SearchResult> {
        let start = Instant::now();
        let limit = limit
            .unwrap_or(self.config.default_limit)
            .min(self.config.max_limit);
        if limit == 0 || query.trim().is_empty() {
            return Ok(empty_result(start));
        }

        // Get a reader (with retry for META_LOCK contention, issue #7)
        let reader = super::open_reader_with_retry(&self.index)?;
        let searcher = reader.searcher();

        // Extract alphanumeric words for Tantivy query (it can't search special chars)
        // Then we'll post-filter for exact literal match
        let search_terms: Vec<&str> = query
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|s| !s.is_empty())
            .collect();

        // Prepare query for matching. Snippet selection stays case-insensitive even
        // when the document filter isn't, so both forms are kept.
        let query_lower = query.to_lowercase();
        let query_normalized: &str = if case_sensitive { query } else { &query_lower };
        let query_terms: Vec<&str> = query_normalized.split_whitespace().collect();
        let lowered_terms: Vec<&str> = query_lower.split_whitespace().collect();
        let matcher = LiteralMatcher {
            normalized: query_normalized,
            terms: &query_terms,
            is_multi_word: query_terms.len() > 1,
            case_sensitive,
            lowered: &query_lower,
            lowered_terms: &lowered_terms,
        };

        let hits = if search_terms.is_empty() {
            // Punctuation-only literals such as "->", "{%", or "::" have no
            // useful index terms. Scan stored docs so literal search still
            // behaves like grep.
            self.scan_documents(&searcher, limit, |doc, seen| {
                self.literal_hit_from_doc(
                    doc,
                    1.0,
                    1.0,
                    &matcher,
                    context_before,
                    context_after,
                    seen,
                    filters,
                )
            })?
        } else {
            let tantivy_query_str = search_terms.join(" ");
            let (parsed, _errors) = self.query_parser().parse_query_lenient(&tantivy_query_str);
            let tantivy_query = self.with_filters(parsed, filters);

            let mut hits = Vec::new();
            let mut fetched = 0usize;
            for multiplier in LITERAL_FETCH_MULTIPLIERS {
                let fetch_limit = limit.saturating_mul(multiplier);
                if fetch_limit <= fetched {
                    break;
                }
                let top_docs =
                    searcher.search(&tantivy_query, &TopDocs::with_limit(fetch_limit))?;
                let candidates = top_docs.len();
                let max_score = top_docs.first().map(|(score, _)| *score).unwrap_or(1.0);

                hits = Vec::with_capacity(limit);
                let mut seen: HashSet<HitKey> = HashSet::new();
                for (score, doc_address) in top_docs {
                    if hits.len() >= limit {
                        break;
                    }
                    let doc = searcher.doc(doc_address)?;
                    if let Some(hit) = self.literal_hit_from_doc(
                        &doc,
                        score,
                        max_score,
                        &matcher,
                        context_before,
                        context_after,
                        &mut seen,
                        filters,
                    ) {
                        hits.push(hit);
                    }
                }

                if hits.len() >= limit || candidates < fetch_limit {
                    break;
                }
                fetched = fetch_limit;
            }
            hits
        };

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
    #[allow(clippy::too_many_arguments)]
    pub fn search_filtered(
        &self,
        query: &str,
        limit: Option<usize>,
        filters: SearchFilters,
        use_regex: bool,
        case_sensitive: bool,
        context_before: Option<usize>,
        context_after: Option<usize>,
        verbose: bool,
    ) -> Result<SearchResult> {
        let requested_limit = limit
            .unwrap_or(self.config.default_limit)
            .min(self.config.max_limit);
        if requested_limit == 0 {
            return Ok(SearchResult {
                total: 0,
                hits: vec![],
                query_time_ms: 0,
                text_hits: 0,
                semantic_hits: 0,
            });
        }

        // Filters run while candidates are being collected rather than on the finished
        // page. Trimming afterwards used to throw away everything the filter rejected
        // and return short — often empty — result sets for queries with plenty of
        // matching files.
        let compiled = CompiledFilters::compile(&filters);

        if verbose {
            eprintln!(
                "[verbose] search mode: {}",
                if use_regex { "regex" } else { "text" }
            );
            if let Some(ref extensions) = filters.extensions {
                eprintln!("[verbose] extension filter: {}", extensions.join(", "));
            }
            if let Some(ref paths) = filters.paths {
                eprintln!("[verbose] path filter: {}", paths.join(", "));
            }
        }

        let result = if use_regex {
            self.search_regex_filtered(
                query,
                Some(requested_limit),
                case_sensitive,
                context_before,
                context_after,
                &compiled,
            )?
        } else {
            self.search_literal(
                query,
                Some(requested_limit),
                case_sensitive,
                context_before,
                context_after,
                &compiled,
            )?
        };

        if verbose {
            eprintln!("[verbose] final results: {}", result.total);
        }

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
        self.search_regex_filtered(
            pattern,
            limit,
            case_sensitive,
            context_before,
            context_after,
            &CompiledFilters::default(),
        )
    }

    fn search_regex_filtered(
        &self,
        pattern: &str,
        limit: Option<usize>,
        case_sensitive: bool,
        context_before: Option<usize>,
        context_after: Option<usize>,
        filters: &CompiledFilters,
    ) -> Result<SearchResult> {
        let start = Instant::now();
        let limit = limit
            .unwrap_or(self.config.default_limit)
            .min(self.config.max_limit);
        if limit == 0 || pattern.trim().is_empty() {
            return Ok(empty_result(start));
        }

        // Compile regex (case-insensitive by default unless --case-sensitive)
        let regex = match RegexBuilder::new(pattern)
            .case_insensitive(!case_sensitive)
            .multi_line(true)
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

        // Extract alphanumeric words from the regex pattern for Tantivy pre-filter
        // This is a rough heuristic - we extract literal parts from the regex
        let search_terms: Vec<&str> = pattern
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|s| !s.is_empty() && s.len() > 1) // Skip single chars (likely regex syntax)
            .collect();

        // If we have searchable terms, use Tantivy to narrow down candidates.
        // Otherwise scan stored docs so regexes like "^#" or punctuation-only
        // expressions are exhaustive instead of capped by an arbitrary TopDocs size.
        let hits = if search_terms.is_empty() {
            self.scan_documents(&searcher, limit, |doc, seen| {
                self.regex_hit_from_doc(
                    doc,
                    &regex,
                    1.0,
                    1.0,
                    context_before,
                    context_after,
                    seen,
                    filters,
                )
            })?
        } else {
            let tantivy_query_str = search_terms.join(" ");
            let (parsed, _errors) = self.query_parser().parse_query_lenient(&tantivy_query_str);
            let tantivy_query = self.with_filters(parsed, filters);

            let mut hits = Vec::new();
            let mut fetched = 0usize;
            for multiplier in REGEX_FETCH_MULTIPLIERS {
                let fetch_limit = limit.saturating_mul(multiplier);
                if fetch_limit <= fetched {
                    break;
                }
                let candidates =
                    searcher.search(&tantivy_query, &TopDocs::with_limit(fetch_limit))?;
                let candidate_count = candidates.len();
                let max_score = candidates.first().map(|(score, _)| *score).unwrap_or(1.0);

                hits = Vec::with_capacity(limit);
                let mut seen: HashSet<HitKey> = HashSet::new();
                for (score, doc_address) in candidates {
                    if hits.len() >= limit {
                        break;
                    }
                    let doc = searcher.doc(doc_address)?;
                    if let Some(hit) = self.regex_hit_from_doc(
                        &doc,
                        &regex,
                        score,
                        max_score,
                        context_before,
                        context_after,
                        &mut seen,
                        filters,
                    ) {
                        hits.push(hit);
                    }
                }

                if hits.len() >= limit || candidate_count < fetch_limit {
                    break;
                }
                fetched = fetch_limit;
            }
            hits
        };

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

    /// Query parser covering the content and file path fields
    fn query_parser(&self) -> QueryParser {
        let mut query_fields = vec![self.fields.content];
        if let Some(fp) = self.fields.filepath {
            query_fields.push(fp);
        }
        QueryParser::for_index(&self.index, query_fields)
    }

    /// Combine the parsed query with the index-side part of the filters.
    ///
    /// The filter clauses contribute nothing to the score, so ranking within the
    /// filtered set matches what an unfiltered search would have produced.
    fn with_filters(&self, main: Box<dyn Query>, filters: &CompiledFilters) -> Box<dyn Query> {
        match self.filter_query(filters) {
            Some(filter) => Box::new(BooleanQuery::new(vec![
                (Occur::Must, main),
                (
                    Occur::Must,
                    Box::new(BoostQuery::new(filter, 0.0)) as Box<dyn Query>,
                ),
            ])),
            None => main,
        }
    }

    /// Build the part of a filter that the index can answer.
    ///
    /// The file path is indexed with the code tokenizer, which splits on punctuation
    /// and lowercases, so `src/main.rs` carries the terms `src`, `main` and `rs`. That
    /// makes an extension a term lookup, and complete directory names inside a path
    /// pattern likewise. Both narrow the candidate set to a superset of what the
    /// pattern matching would keep, so the exact check still runs on every hit.
    fn filter_query(&self, filters: &CompiledFilters) -> Option<Box<dyn Query>> {
        let filepath = self.fields.filepath?;
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();

        if !filters.extensions.is_empty() {
            let mut alternatives = Vec::new();
            for extension in &filters.extensions {
                let terms = self.filepath_terms(filepath, extension);
                if terms.is_empty() {
                    alternatives.clear();
                    break;
                }
                alternatives.push(all_of(terms));
            }
            if let Some(query) = any_of(alternatives) {
                clauses.push((Occur::Must, query));
            }
        }

        if !filters.paths.is_empty() {
            let mut alternatives = Vec::new();
            for pattern in &filters.paths {
                let mut terms = Vec::new();
                for anchor in pattern.anchors() {
                    terms.extend(self.filepath_terms(filepath, anchor));
                }
                if terms.is_empty() {
                    alternatives.clear();
                    break;
                }
                alternatives.push(all_of(terms));
            }
            if let Some(query) = any_of(alternatives) {
                clauses.push((Occur::Must, query));
            }
        }

        if clauses.is_empty() {
            None
        } else {
            Some(Box::new(BooleanQuery::new(clauses)))
        }
    }

    /// Terms the code tokenizer produces for a fragment of a path
    fn filepath_terms(&self, field: Field, text: &str) -> Vec<Term> {
        let Some(mut analyzer) = self.index.tokenizers().get(CODE_TOKENIZER) else {
            return Vec::new();
        };
        let mut terms = Vec::new();
        let mut stream = analyzer.token_stream(text);
        while stream.advance() {
            terms.push(Term::from_field_text(field, &stream.token().text));
        }
        terms
    }

    /// Walk every stored document, handing the live ones to `make_hit`.
    ///
    /// Punctuation-only queries have no index terms to narrow candidates with, so the
    /// whole doc store has to be decompressed. Splitting it into contiguous document
    /// ranges spreads that across cores while keeping the order a serial walk produced:
    /// ranges are merged back in document order and only then trimmed to the limit.
    ///
    /// A range only contributes once the ranges ahead of it come up short, so each one
    /// watches their running totals and stops as soon as they have the page covered.
    /// That keeps a query like `->`, which matches almost every file, as cheap as the
    /// serial walk that stopped at the first handful of documents.
    fn scan_documents<F>(
        &self,
        searcher: &tantivy::Searcher,
        limit: usize,
        make_hit: F,
    ) -> Result<Vec<SearchHit>>
    where
        F: Fn(&TantivyDocument, &mut HashSet<HitKey>) -> Option<SearchHit> + Sync,
    {
        let units = scan_units(searcher);
        let found: Vec<AtomicUsize> = units.iter().map(|_| AtomicUsize::new(0)).collect();

        let run = |position: usize, unit: &ScanUnit| -> Result<Vec<SearchHit>> {
            let segment = &searcher.segment_readers()[unit.segment];
            let store = segment.get_store_reader(STORE_CACHE_BLOCKS)?;
            let alive = segment.alive_bitset();
            let mut seen: HashSet<HitKey> = HashSet::new();
            let mut hits = Vec::new();

            for (examined, doc_id) in (unit.start..unit.end).enumerate() {
                if hits.len() >= limit {
                    break;
                }
                if examined % QUOTA_CHECK_INTERVAL == 0
                    && found[..position]
                        .iter()
                        .map(|count| count.load(Ordering::Relaxed))
                        .sum::<usize>()
                        >= limit
                {
                    break;
                }
                if alive
                    .map(|bitset| !bitset.is_alive(doc_id))
                    .unwrap_or(false)
                {
                    continue;
                }
                let doc: TantivyDocument = store.get(doc_id)?;
                if let Some(hit) = make_hit(&doc, &mut seen) {
                    hits.push(hit);
                    found[position].store(hits.len(), Ordering::Relaxed);
                }
            }

            Ok(hits)
        };

        let collected: Vec<Result<Vec<SearchHit>>> = if units.len() > 1 {
            let run = &run;
            std::thread::scope(|scope| {
                let handles: Vec<_> = units
                    .iter()
                    .enumerate()
                    .map(|(position, unit)| scope.spawn(move || run(position, unit)))
                    .collect();
                handles
                    .into_iter()
                    .map(|handle| match handle.join() {
                        Ok(result) => result,
                        Err(panic) => std::panic::resume_unwind(panic),
                    })
                    .collect()
            })
        } else {
            units
                .iter()
                .enumerate()
                .map(|(position, unit)| run(position, unit))
                .collect()
        };

        let mut seen: HashSet<HitKey> = HashSet::new();
        let mut hits = Vec::with_capacity(limit);
        for unit_hits in collected {
            for hit in unit_hits? {
                if hits.len() >= limit {
                    return Ok(hits);
                }
                if seen.insert((hit.path.clone(), hit.line_start, hit.line_end)) {
                    hits.push(hit);
                }
            }
        }

        Ok(hits)
    }

    #[allow(clippy::too_many_arguments)]
    fn literal_hit_from_doc(
        &self,
        doc: &TantivyDocument,
        score: f32,
        max_score: f32,
        matcher: &LiteralMatcher<'_>,
        context_before: Option<usize>,
        context_after: Option<usize>,
        seen: &mut HashSet<HitKey>,
        filters: &CompiledFilters,
    ) -> Option<SearchHit> {
        let path = extract_str(doc, self.fields.path).unwrap_or_default();
        if !filters.matches(path) {
            return None;
        }

        let content = extract_str(doc, self.fields.content).unwrap_or_default();
        let line_start = extract_u64(doc, self.fields.line_start).unwrap_or(1);

        // Check if path matches the query (filename search)
        let path_match = !matcher.terms.is_empty()
            && matcher
                .terms
                .iter()
                .all(|term| matcher.contains(path, term));

        // LITERAL GREP-LIKE FILTER: exact phrase match, or AND match for multi-word queries
        let exact_match = matcher.contains(content, matcher.normalized);
        let and_match = matcher.is_multi_word
            && matcher
                .terms
                .iter()
                .all(|term| matcher.contains(content, term));
        if !exact_match && !and_match && !path_match {
            return None;
        }

        // Normalize score to 0-1 range
        let normalized_score = if max_score > 0.0 {
            score / max_score
        } else {
            0.0
        };

        // For path-only matches (no content match), show beginning of file
        let is_content_match = exact_match || and_match;

        let (snippet, snippet_offset, snippet_line_count, match_line_offset) = if is_content_match {
            create_relevant_snippet(content, matcher, 10, context_before, context_after)
        } else {
            // Path-only match: show first few lines
            let lines: Vec<&str> = content.lines().take(10).collect();
            let snippet = lines.join("\n");
            let line_count = lines.len();
            (snippet, 0, line_count, 0)
        };

        // Adjust line numbers to reflect where the snippet is in the file
        let actual_line_start = line_start + snippet_offset as u64;
        let actual_line_end = actual_line_start + snippet_line_count.saturating_sub(1) as u64;
        let match_line_in_snippet = match_line_offset.saturating_sub(snippet_offset);

        // Deduplicate: skip if we already have a hit for the same file and line range
        let key = (path.to_string(), actual_line_start, actual_line_end);
        if !seen.insert(key) {
            return None;
        }

        let chunk_id = extract_str(doc, self.fields.chunk_id).unwrap_or_default();
        Some(SearchHit {
            path: path.to_string(),
            line_start: actual_line_start,
            line_end: actual_line_end,
            snippet,
            score: normalized_score,
            is_chunk: !chunk_id.is_empty(),
            doc_id: extract_str(doc, self.fields.doc_id)
                .unwrap_or_default()
                .to_string(),
            match_type: MatchType::Text,
            match_line_in_snippet,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn regex_hit_from_doc(
        &self,
        doc: &TantivyDocument,
        regex: &regex::Regex,
        score: f32,
        max_score: f32,
        context_before: Option<usize>,
        context_after: Option<usize>,
        seen: &mut HashSet<HitKey>,
        filters: &CompiledFilters,
    ) -> Option<SearchHit> {
        let path = extract_str(doc, self.fields.path).unwrap_or_default();
        if !filters.matches(path) {
            return None;
        }

        let content = extract_str(doc, self.fields.content).unwrap_or_default();
        let line_start = extract_u64(doc, self.fields.line_start).unwrap_or(1);

        // REGEX FILTER: Only include if content matches the regex
        if !regex.is_match(content) {
            return None;
        }

        // Normalize score to 0-1 range
        let normalized_score = if max_score > 0.0 {
            score / max_score
        } else {
            0.0
        };

        // Create snippet showing lines that match the regex
        let (snippet, snippet_offset, snippet_line_count, match_line_offset) =
            create_regex_snippet(content, regex, 10, context_before, context_after);

        // Adjust line numbers to reflect where the snippet is in the file
        let actual_line_start = line_start + snippet_offset as u64;
        let actual_line_end = actual_line_start + snippet_line_count.saturating_sub(1) as u64;
        let match_line_in_snippet = match_line_offset.saturating_sub(snippet_offset);

        // Deduplicate: skip if we already have a hit for the same file and line range
        let key = (path.to_string(), actual_line_start, actual_line_end);
        if !seen.insert(key) {
            return None;
        }

        let chunk_id = extract_str(doc, self.fields.chunk_id).unwrap_or_default();
        Some(SearchHit {
            path: path.to_string(),
            line_start: actual_line_start,
            line_end: actual_line_end,
            snippet,
            score: normalized_score,
            is_chunk: !chunk_id.is_empty(),
            doc_id: extract_str(doc, self.fields.doc_id)
                .unwrap_or_default()
                .to_string(),
            match_type: MatchType::Text,
            match_line_in_snippet,
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

/// The parts of a literal query every candidate document is tested against
struct LiteralMatcher<'a> {
    /// Query as it is compared against document text
    normalized: &'a str,
    /// Words of `normalized`
    terms: &'a [&'a str],
    is_multi_word: bool,
    case_sensitive: bool,
    /// Lowercased query, used for picking the snippet line, which stays
    /// case-insensitive even when the document filter isn't
    lowered: &'a str,
    /// Words of `lowered`
    lowered_terms: &'a [&'a str],
}

impl LiteralMatcher<'_> {
    fn contains(&self, haystack: &str, needle: &str) -> bool {
        if self.case_sensitive {
            haystack.contains(needle)
        } else {
            contains_lowered(haystack, needle)
        }
    }
}

/// Path and extension filters, compiled once per search.
///
/// Globs used to be turned into a regex inside the retain closure, so every pattern was
/// recompiled for every hit it was tested against.
#[derive(Default)]
struct CompiledFilters {
    extensions: Vec<String>,
    paths: Vec<PathPattern>,
}

impl CompiledFilters {
    fn compile(filters: &SearchFilters) -> Self {
        Self {
            extensions: filters.extensions.clone().unwrap_or_default(),
            paths: filters
                .paths
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|pattern| PathPattern::new(pattern))
                .collect(),
        }
    }

    fn matches(&self, path: &str) -> bool {
        if !self.extensions.is_empty() {
            let matched = match std::path::Path::new(path).extension() {
                Some(ext) => self
                    .extensions
                    .iter()
                    .any(|e| e.eq_ignore_ascii_case(&ext.to_string_lossy())),
                None => false,
            };
            if !matched {
                return false;
            }
        }

        if !self.paths.is_empty() && !self.paths.iter().any(|p| p.matches(path)) {
            return false;
        }

        true
    }
}

/// A `-p` pattern with its glob regex already compiled
struct PathPattern {
    pattern: String,
    glob: Option<regex::Regex>,
}

impl PathPattern {
    fn new(pattern: &str) -> Self {
        let glob = if pattern.contains('*') || pattern.contains('?') {
            glob_to_regex(pattern).ok()
        } else {
            None
        };
        Self {
            pattern: pattern.to_string(),
            glob,
        }
    }

    fn matches(&self, path: &str) -> bool {
        if self.pattern.contains('*') || self.pattern.contains('?') {
            self.glob
                .as_ref()
                .map(|re| re.is_match(path))
                .unwrap_or(false)
        } else {
            path.starts_with(&self.pattern) || path.contains(&self.pattern)
        }
    }

    /// Directory names a matching path is guaranteed to contain in full.
    ///
    /// Only segments with a separator on both sides qualify: the pattern matches
    /// anywhere in the path, so a leading `lib/` also matches `mylib/`, and a segment
    /// next to a wildcard is only part of a name.
    fn anchors(&self) -> Vec<&str> {
        let segments: Vec<&str> = self.pattern.split('/').collect();
        if segments.len() < 3 {
            return Vec::new();
        }
        segments[1..segments.len() - 1]
            .iter()
            .copied()
            .filter(|segment| {
                !segment.is_empty() && !segment.contains('*') && !segment.contains('?')
            })
            .collect()
    }
}

/// A contiguous run of document ids inside one segment
struct ScanUnit {
    segment: usize,
    start: u32,
    end: u32,
}

/// Split the stored documents into runs, one per worker
fn scan_units(searcher: &tantivy::Searcher) -> Vec<ScanUnit> {
    let segments = searcher.segment_readers();
    let total: u64 = segments.iter().map(|s| s.max_doc() as u64).sum();
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    let chunk = if total <= MIN_PARALLEL_SCAN_DOCS || workers <= 1 {
        usize::MAX
    } else {
        ((total as usize).div_ceil(workers)).max(MIN_SCAN_CHUNK)
    };

    let mut units = Vec::new();
    for (segment, reader) in segments.iter().enumerate() {
        let max_doc = reader.max_doc();
        let mut start = 0u32;
        while start < max_doc {
            let end = (start as usize).saturating_add(chunk).min(max_doc as usize) as u32;
            units.push(ScanUnit {
                segment,
                start,
                end,
            });
            start = end;
        }
    }
    units
}

/// Require every term
fn all_of(terms: Vec<Term>) -> Box<dyn Query> {
    let clauses: Vec<(Occur, Box<dyn Query>)> = terms
        .into_iter()
        .map(|term| {
            (
                Occur::Must,
                Box::new(TermQuery::new(term, IndexRecordOption::Basic)) as Box<dyn Query>,
            )
        })
        .collect();
    Box::new(BooleanQuery::new(clauses))
}

/// Require at least one alternative
fn any_of(mut alternatives: Vec<Box<dyn Query>>) -> Option<Box<dyn Query>> {
    match alternatives.len() {
        0 => None,
        1 => alternatives.pop(),
        _ => Some(Box::new(BooleanQuery::new(
            alternatives
                .into_iter()
                .map(|query| (Occur::Should, query))
                .collect(),
        ))),
    }
}

fn empty_result(start: Instant) -> SearchResult {
    SearchResult {
        total: 0,
        hits: vec![],
        query_time_ms: start.elapsed().as_millis() as u64,
        text_hits: 0,
        semantic_hits: 0,
    }
}

/// Borrow a text value from a document.
///
/// The document already owns its stored text, so matching against a borrow avoids
/// copying every candidate's file content just to look at it.
fn extract_str(doc: &TantivyDocument, field: tantivy::schema::Field) -> Option<&str> {
    doc.get_first(field).and_then(|v| {
        if let tantivy::schema::OwnedValue::Str(s) = v {
            Some(s.as_str())
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

/// Case-insensitive substring test against an already-lowercased needle.
///
/// Lowercasing a whole file to run one `contains` was the single biggest allocation in
/// a search. Text that is entirely ASCII — nearly all source code — folds case a byte
/// at a time instead, and only genuinely non-ASCII text falls back to a lowercased copy.
fn contains_lowered(haystack: &str, needle_lower: &str) -> bool {
    if haystack.is_ascii() {
        contains_ascii_ignore_case(haystack.as_bytes(), needle_lower.as_bytes())
    } else {
        haystack.to_lowercase().contains(needle_lower)
    }
}

/// Substring search over ASCII bytes, ignoring case. `needle` must already be lowercase.
fn contains_ascii_ignore_case(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.len() > haystack.len() {
        return false;
    }

    let first = needle[0];
    let first_upper = first.to_ascii_uppercase();
    let last_start = haystack.len() - needle.len() + 1;
    let mut offset = 0;

    while offset < last_start {
        let Some(found) = memchr2(first, first_upper, &haystack[offset..last_start]) else {
            return false;
        };
        let at = offset + found;
        if haystack[at..at + needle.len()].eq_ignore_ascii_case(needle) {
            return true;
        }
        offset = at + 1;
    }

    false
}

/// Create a snippet showing lines relevant to the query
/// Returns (snippet, snippet_offset, line_count, match_line_offset)
/// - snippet_offset: 0-based line index where snippet starts in the chunk
/// - match_line_offset: 0-based line index of the actual match in the chunk
fn create_relevant_snippet(
    content: &str,
    matcher: &LiteralMatcher<'_>,
    max_lines: usize,
    ctx_before: Option<usize>,
    ctx_after: Option<usize>,
) -> (String, usize, usize, usize) {
    let lines: Vec<&str> = content.lines().collect();
    let query_terms = matcher.lowered_terms;

    // Find lines that contain any query term
    let mut matching_indices: Vec<usize> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let matches = if query_terms.is_empty() {
            !matcher.lowered.is_empty() && contains_lowered(line, matcher.lowered)
        } else {
            query_terms.iter().any(|term| contains_lowered(line, term))
        };
        if matches {
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
            let count = query_terms
                .iter()
                .filter(|t| contains_lowered(lines[idx], t))
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
#[cfg(test)]
fn path_matches(pattern: &str, path: &str) -> bool {
    PathPattern::new(pattern).matches(path)
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
                fields.filepath.unwrap() => path,
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

    /// Helper: add many documents through a single writer
    fn add_docs(index: &Index, fields: &SchemaFields, docs: &[(String, String, String, String)]) {
        let mut writer = index.writer(50_000_000).unwrap();
        for (doc_id, path, content, ext) in docs {
            writer
                .add_document(doc!(
                    fields.doc_id => doc_id.as_str(),
                    fields.path => path.as_str(),
                    fields.filepath.unwrap() => path.as_str(),
                    fields.workspace => "/test",
                    fields.content => content.as_str(),
                    fields.mtime => 0u64,
                    fields.size => content.len() as u64,
                    fields.extension => ext.as_str(),
                    fields.line_start => 1u64,
                    fields.line_end => content.lines().count() as u64,
                    fields.chunk_id => "",
                    fields.parent_doc => ""
                ))
                .unwrap();
        }
        writer.commit().unwrap();
    }

    /// Helper: build a batch of documents from a naming pattern
    fn batch(
        count: usize,
        prefix: &str,
        path: impl Fn(usize) -> String,
        content: &str,
        ext: &str,
    ) -> Vec<(String, String, String, String)> {
        (0..count)
            .map(|i| {
                (
                    format!("{prefix}-{i}"),
                    path(i),
                    content.to_string(),
                    ext.to_string(),
                )
            })
            .collect()
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
    fn test_case_sensitive_search_respects_case() -> Result<()> {
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

        assert_eq!(
            searcher.search("Hello", None, true, None, None)?.hits.len(),
            1
        );
        assert!(searcher.search("hello", None, true, None, None)?.is_empty());

        Ok(())
    }

    #[test]
    fn test_non_ascii_content_matches_case_insensitively() -> Result<()> {
        let temp_dir = tempdir().unwrap();
        let (index, fields) = create_test_index(temp_dir.path());
        add_doc(
            &index,
            &fields,
            "test1",
            "src/greet.rs",
            "let greeting = \"CAFÉ Ünicode\";",
            "rs",
        );

        let config = SearchConfig::default();
        let searcher = Searcher::new(config, index);

        let result = searcher.search("café", None, false, None, None)?;
        assert_eq!(result.hits.len(), 1);

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
    fn test_punctuation_literal_search_scans_when_no_index_terms() -> Result<()> {
        let temp_dir = tempdir().unwrap();
        let (index, fields) = create_test_index(temp_dir.path());
        add_doc(
            &index,
            &fields,
            "test1",
            "src/client.php",
            "$client->get('/api/users');",
            "php",
        );

        let config = SearchConfig::default();
        let searcher = Searcher::new(config, index);

        let result = searcher.search("->", None, false, None, None)?;
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].path, "src/client.php");
        assert!(result.hits[0].snippet.contains("->get"));

        Ok(())
    }

    #[test]
    fn test_punctuation_scan_is_exhaustive_across_many_documents() -> Result<()> {
        let temp_dir = tempdir().unwrap();
        let (index, fields) = create_test_index(temp_dir.path());

        // Enough documents that the scan splits into parallel ranges
        add_docs(
            &index,
            &fields,
            &batch(
                6_000,
                "nonmatch",
                |i| format!("src/file_{i}.rs"),
                "fn main() {}",
                "rs",
            ),
        );
        add_doc(
            &index,
            &fields,
            "match",
            "src/client.php",
            "$client->get('/api/users');",
            "php",
        );

        let config = SearchConfig::default();
        let searcher = Searcher::new(config, index);

        let result = searcher.search("->", Some(5), false, None, None)?;
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].path, "src/client.php");

        Ok(())
    }

    #[test]
    fn test_regex_without_index_terms_scans_all_documents() -> Result<()> {
        let temp_dir = tempdir().unwrap();
        let (index, fields) = create_test_index(temp_dir.path());

        add_docs(
            &index,
            &fields,
            &batch(
                120,
                "nonmatch",
                |i| format!("src/file_{i}.rs"),
                "fn main() {}",
                "rs",
            ),
        );
        add_doc(
            &index,
            &fields,
            "match",
            "README.md",
            "# Project\n\nDetails",
            "md",
        );

        let config = SearchConfig::default();
        let searcher = Searcher::new(config, index);

        let result = searcher.search_regex("^#", Some(1), false, None, None)?;
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].path, "README.md");

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
    fn test_regex_search_line_anchors_are_multiline() -> Result<()> {
        let temp_dir = tempdir().unwrap();
        let (index, fields) = create_test_index(temp_dir.path());
        add_doc(
            &index,
            &fields,
            "test1",
            "src/imports.php",
            "<?php\nuse Grav\\Common\\Grav;\nclass Imports {}",
            "php",
        );

        let config = SearchConfig::default();
        let searcher = Searcher::new(config, index);

        let result = searcher.search_regex("^use ", None, false, None, None)?;
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].path, "src/imports.php");
        assert!(result.hits[0].snippet.contains("use Grav"));

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
        let result =
            searcher.search_filtered("hello", None, filters, false, false, None, None, false)?;

        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].path, "src/main.rs");

        Ok(())
    }

    #[test]
    fn test_extension_filter_survives_a_crowd_of_other_extensions() -> Result<()> {
        // The filter used to run after the result page was cut to size, so a query whose
        // top matches were all the wrong extension returned nothing at all.
        let temp_dir = tempdir().unwrap();
        let (index, fields) = create_test_index(temp_dir.path());

        add_docs(
            &index,
            &fields,
            &batch(
                500,
                "php",
                |i| format!("src/handler_{i}.php"),
                "function handler() { handler(); handler(); }",
                "php",
            ),
        );
        add_docs(
            &index,
            &fields,
            &batch(
                5,
                "rs",
                |i| format!("src/handler_{i}.rs"),
                "fn handler() {}",
                "rs",
            ),
        );

        let config = SearchConfig::default();
        let searcher = Searcher::new(config, index);

        let filters = SearchFilters {
            extensions: Some(vec!["rs".to_string()]),
            paths: None,
        };
        let result = searcher.search_filtered(
            "handler",
            Some(10),
            filters,
            false,
            false,
            None,
            None,
            false,
        )?;

        assert_eq!(result.hits.len(), 5);
        assert!(result.hits.iter().all(|h| h.path.ends_with(".rs")));

        Ok(())
    }

    #[test]
    fn test_extension_filter_matches_uppercase_extensions() -> Result<()> {
        let temp_dir = tempdir().unwrap();
        let (index, fields) = create_test_index(temp_dir.path());
        add_doc(
            &index,
            &fields,
            "test1",
            "docs/README.MD",
            "handler notes",
            "MD",
        );

        let config = SearchConfig::default();
        let searcher = Searcher::new(config, index);

        let filters = SearchFilters {
            extensions: Some(vec!["md".to_string()]),
            paths: None,
        };
        let result =
            searcher.search_filtered("handler", None, filters, false, false, None, None, false)?;

        assert_eq!(result.hits.len(), 1);

        Ok(())
    }

    #[test]
    fn test_path_filter_survives_a_crowd_of_other_paths() -> Result<()> {
        let temp_dir = tempdir().unwrap();
        let (index, fields) = create_test_index(temp_dir.path());

        add_docs(
            &index,
            &fields,
            &batch(
                500,
                "vendor",
                |i| format!("vendor/pkg/handler_{i}.php"),
                "function handler() { handler(); handler(); }",
                "php",
            ),
        );
        add_docs(
            &index,
            &fields,
            &batch(
                3,
                "app",
                |i| format!("app/http/handler_{i}.php"),
                "function handler() {}",
                "php",
            ),
        );

        let config = SearchConfig::default();
        let searcher = Searcher::new(config, index);

        let filters = SearchFilters {
            extensions: None,
            paths: Some(vec!["app/http/".to_string()]),
        };
        let result = searcher.search_filtered(
            "handler",
            Some(10),
            filters,
            false,
            false,
            None,
            None,
            false,
        )?;

        assert_eq!(result.hits.len(), 3);
        assert!(result.hits.iter().all(|h| h.path.starts_with("app/http/")));

        Ok(())
    }

    #[test]
    fn test_extension_filter_on_punctuation_query() -> Result<()> {
        let temp_dir = tempdir().unwrap();
        let (index, fields) = create_test_index(temp_dir.path());
        add_doc(
            &index,
            &fields,
            "php",
            "src/client.php",
            "$client->get('/api');",
            "php",
        );
        add_doc(
            &index,
            &fields,
            "rs",
            "src/client.rs",
            "let x = a -> b;",
            "rs",
        );

        let config = SearchConfig::default();
        let searcher = Searcher::new(config, index);

        let filters = SearchFilters {
            extensions: Some(vec!["rs".to_string()]),
            paths: None,
        };
        let result =
            searcher.search_filtered("->", None, filters, false, false, None, None, false)?;

        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].path, "src/client.rs");

        Ok(())
    }

    #[test]
    fn test_regex_search_with_extension_filter() -> Result<()> {
        let temp_dir = tempdir().unwrap();
        let (index, fields) = create_test_index(temp_dir.path());

        add_docs(
            &index,
            &fields,
            &batch(
                200,
                "php",
                |i| format!("src/handler_{i}.php"),
                "function handler() { handler(); }",
                "php",
            ),
        );
        add_doc(
            &index,
            &fields,
            "rs",
            "src/handler.rs",
            "fn handler() {}",
            "rs",
        );

        let config = SearchConfig::default();
        let searcher = Searcher::new(config, index);

        let filters = SearchFilters {
            extensions: Some(vec!["rs".to_string()]),
            paths: None,
        };
        let result = searcher.search_filtered(
            "handler",
            Some(5),
            filters,
            true,
            false,
            None,
            None,
            false,
        )?;

        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].path, "src/handler.rs");

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
        let result =
            searcher.search_filtered("hello", None, filters, false, false, None, None, false)?;

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
    fn test_path_anchors_skip_partial_segments() {
        // A leading segment can be the tail of a longer directory name
        assert!(PathPattern::new("lib/").anchors().is_empty());
        // Wildcards leave only part of a name behind
        assert_eq!(PathPattern::new("src/ma*n/tests/").anchors(), vec!["tests"]);
        assert_eq!(
            PathPattern::new("user/plugins/*/tests/").anchors(),
            vec!["plugins", "tests"]
        );
        assert!(PathPattern::new("utils").anchors().is_empty());
    }

    #[test]
    fn test_contains_ascii_ignore_case() {
        assert!(contains_ascii_ignore_case(b"Hello World", b"hello"));
        assert!(contains_ascii_ignore_case(b"Hello World", b"world"));
        assert!(contains_ascii_ignore_case(b"aAaB", b"aab"));
        assert!(!contains_ascii_ignore_case(b"Hello", b"goodbye"));
        assert!(!contains_ascii_ignore_case(b"hi", b"longer"));
        assert!(contains_ascii_ignore_case(b"anything", b""));
        assert!(contains_ascii_ignore_case(b"->get(", b"->get("));
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
        let result = searcher.search_filtered(
            "extends Plugin",
            None,
            filters,
            false,
            false,
            None,
            None,
            false,
        )?;

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
                fields.filepath.unwrap() => "src/main.rs",
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
                fields.filepath.unwrap() => "src/main.rs",
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

    #[test]
    fn test_filename_search() -> Result<()> {
        let temp_dir = tempdir().unwrap();
        let (index, fields) = create_test_index(temp_dir.path());

        // Add a file where content does NOT contain the search term,
        // but the filename does
        add_doc(
            &index,
            &fields,
            "test1",
            "src/commands/dashboard.rs",
            "fn run() {\n    println!(\"starting...\");\n}",
            "rs",
        );
        add_doc(
            &index,
            &fields,
            "test2",
            "src/main.rs",
            "fn main() { hello(); }",
            "rs",
        );

        let config = SearchConfig::default();
        let searcher = Searcher::new(config, index);

        // Search for "dashboard" - should find via filename even though content doesn't contain it
        let result = searcher.search("dashboard", None, false, None, None)?;
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].path, "src/commands/dashboard.rs");

        Ok(())
    }

    #[test]
    fn test_text_hits_consistent_after_filter() -> Result<()> {
        // Issue #10: text_hits should reflect post-filter count, not pre-filter
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
        add_doc(
            &index,
            &fields,
            "test3",
            "lib/utils.js",
            "function hello() {}",
            "js",
        );

        let config = SearchConfig::default();
        let searcher = Searcher::new(config, index);

        // Filter to only .rs files - should get 1 hit, and text_hits must equal total
        let filters = SearchFilters {
            extensions: Some(vec!["rs".to_string()]),
            paths: None,
        };
        let result =
            searcher.search_filtered("hello", None, filters, false, false, None, None, false)?;

        assert_eq!(result.total, 1);
        assert_eq!(result.text_hits, 1);
        assert_eq!(result.text_hits, result.total);

        // Filter to a path that matches nothing - should get 0 hits with text_hits = 0
        let filters = SearchFilters {
            extensions: None,
            paths: Some(vec!["nonexistent/".to_string()]),
        };
        let result =
            searcher.search_filtered("hello", None, filters, false, false, None, None, false)?;

        assert_eq!(result.total, 0);
        assert_eq!(result.text_hits, 0);

        Ok(())
    }
}
