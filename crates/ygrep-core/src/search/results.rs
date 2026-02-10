use serde::{Deserialize, Serialize};

/// Type of match for a search hit
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatchType {
    /// Matched via BM25 text search
    Text,
    /// Matched via semantic vector search
    Semantic,
    /// Matched by both text and semantic search
    Hybrid,
}

impl std::fmt::Display for MatchType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MatchType::Text => write!(f, "text"),
            MatchType::Semantic => write!(f, "semantic"),
            MatchType::Hybrid => write!(f, "hybrid"),
        }
    }
}

/// Result of a search operation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    /// Search hits
    pub hits: Vec<SearchHit>,
    /// Total number of results (may be more than hits if limited)
    pub total: usize,
    /// Query execution time in milliseconds
    pub query_time_ms: u64,
    /// Number of hits from text search
    #[serde(default)]
    pub text_hits: usize,
    /// Number of hits from semantic search
    #[serde(default)]
    pub semantic_hits: usize,
}

/// A single search hit
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    /// File path (relative to workspace)
    pub path: String,
    /// Line range (start-end)
    pub line_start: u64,
    pub line_end: u64,
    /// Content snippet
    pub snippet: String,
    /// Relevance score (0.0-1.0)
    pub score: f32,
    /// Whether this is a chunk or full document
    pub is_chunk: bool,
    /// Document ID
    pub doc_id: String,
    /// Type of match (text, semantic, or hybrid)
    #[serde(default = "default_match_type")]
    pub match_type: MatchType,
}

fn default_match_type() -> MatchType {
    MatchType::Text
}

impl SearchHit {
    /// Format line range as string (e.g., "10-25")
    pub fn lines_str(&self) -> String {
        if self.line_start == self.line_end {
            format!("{}", self.line_start)
        } else {
            format!("{}-{}", self.line_start, self.line_end)
        }
    }
}

impl SearchResult {
    /// Create an empty result
    pub fn empty() -> Self {
        Self {
            hits: vec![],
            total: 0,
            query_time_ms: 0,
            text_hits: 0,
            semantic_hits: 0,
        }
    }

    /// Check if there are any results
    pub fn is_empty(&self) -> bool {
        self.hits.is_empty()
    }

    /// Format search type summary (e.g., "5 text + 3 semantic" or "text")
    fn search_type_summary(&self) -> String {
        if self.text_hits > 0 && self.semantic_hits > 0 {
            format!("{} text + {} semantic", self.text_hits, self.semantic_hits)
        } else if self.semantic_hits > 0 {
            "semantic".to_string()
        } else {
            "text".to_string()
        }
    }

    /// Normalize score for display (RRF scores are tiny ~0.01, we want 0-100 range)
    fn display_score(score: f32) -> f32 {
        // RRF scores max out around 0.016 for K=60, scale to 0-100
        // A document appearing in both BM25 and vector results at rank 1 would be ~0.033
        (score * 3000.0).min(99.9)
    }

    /// Format results for AI-optimized output (minimal tokens, maximum density)
    pub fn format_ai(&self) -> String {
        let mut output = String::new();

        // Header with count and search type breakdown
        output.push_str(&format!(
            "# {} results ({})\n\n",
            self.hits.len(),
            self.search_type_summary()
        ));

        for hit in &self.hits {
            // Single line format: path:line (score%) [match_type]
            let score_pct = Self::display_score(hit.score);
            let match_indicator = match hit.match_type {
                MatchType::Hybrid => " +",   // both text and semantic
                MatchType::Semantic => " ~", // semantic only
                MatchType::Text => "",       // text only (default, no indicator)
            };
            output.push_str(&format!(
                "{}:{} ({:.0}%){}\n",
                hit.path, hit.line_start, score_pct, match_indicator
            ));

            // Show only the first matching line, trimmed
            if let Some(first_line) = hit.snippet.lines().next() {
                let trimmed = first_line.trim();
                let preview = if trimmed.len() > 100 {
                    let boundary = trimmed.floor_char_boundary(100);
                    format!("{}...", &trimmed[..boundary])
                } else {
                    trimmed.to_string()
                };
                output.push_str(&format!("  {}\n", preview));
            }
            output.push('\n');
        }

        output
    }

    /// Format results as JSON (includes all metadata)
    pub fn format_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string())
    }

    /// Format results for human-readable output (more context, line numbers)
    pub fn format_pretty(&self) -> String {
        let mut output = String::new();

        // Header with breakdown
        let type_info = if self.text_hits > 0 || self.semantic_hits > 0 {
            format!(" ({})", self.search_type_summary())
        } else {
            String::new()
        };
        output.push_str(&format!("# {} results{}\n\n", self.hits.len(), type_info));

        for hit in &self.hits {
            // Header: path:line_range
            output.push_str(&format!("{}:{}\n", hit.path, hit.lines_str()));

            // Show first few lines of snippet with line numbers
            for (i, line) in hit.snippet.lines().take(3).enumerate() {
                let line_num = hit.line_start + i as u64;
                let trimmed = line.trim();
                let preview = if trimmed.len() > 80 {
                    let boundary = trimmed.floor_char_boundary(80);
                    format!("{}...", &trimmed[..boundary])
                } else {
                    trimmed.to_string()
                };
                output.push_str(&format!("  {}: {}\n", line_num, preview));
            }
            output.push('\n');
        }

        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lines_str() {
        let hit = SearchHit {
            path: "test.rs".to_string(),
            line_start: 10,
            line_end: 25,
            snippet: "content".to_string(),
            score: 0.8,
            is_chunk: false,
            doc_id: "abc123".to_string(),
            match_type: MatchType::Text,
        };
        assert_eq!(hit.lines_str(), "10-25");

        let single_line = SearchHit {
            line_start: 5,
            line_end: 5,
            ..hit.clone()
        };
        assert_eq!(single_line.lines_str(), "5");
    }

    #[test]
    fn test_format_ai() {
        let result = SearchResult {
            hits: vec![SearchHit {
                path: "src/main.rs".to_string(),
                line_start: 1,
                line_end: 10,
                snippet: "fn main() {\n    println!(\"hello\");\n}".to_string(),
                score: 0.01,
                is_chunk: false,
                doc_id: "abc".to_string(),
                match_type: MatchType::Text,
            }],
            total: 1,
            query_time_ms: 15,
            text_hits: 1,
            semantic_hits: 0,
        };

        let output = result.format_ai();
        assert!(output.contains("# 1 results"));
        assert!(output.contains("src/main.rs:1"));
        assert!(output.contains("(30%)"));
    }

    fn make_hit(path: &str, score: f32, match_type: MatchType) -> SearchHit {
        SearchHit {
            path: path.to_string(),
            line_start: 1,
            line_end: 5,
            snippet: "fn example() {\n    // code\n}".to_string(),
            score,
            is_chunk: false,
            doc_id: "test".to_string(),
            match_type,
        }
    }

    fn make_result(hits: Vec<SearchHit>) -> SearchResult {
        let text_hits = hits
            .iter()
            .filter(|h| matches!(h.match_type, MatchType::Text | MatchType::Hybrid))
            .count();
        let semantic_hits = hits
            .iter()
            .filter(|h| matches!(h.match_type, MatchType::Semantic | MatchType::Hybrid))
            .count();
        let total = hits.len();
        SearchResult {
            hits,
            total,
            query_time_ms: 10,
            text_hits,
            semantic_hits,
        }
    }

    #[test]
    fn test_format_json_valid() {
        let result = make_result(vec![make_hit("src/main.rs", 0.01, MatchType::Text)]);
        let json = result.format_json();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.get("hits").unwrap().is_array());
        assert_eq!(parsed["total"], 1);
    }

    #[test]
    fn test_format_pretty_includes_path_and_line_numbers() {
        let result = make_result(vec![make_hit("src/lib.rs", 0.01, MatchType::Text)]);
        let output = result.format_pretty();
        assert!(output.contains("src/lib.rs:1"));
        assert!(output.contains("1: fn example()"));
    }

    #[test]
    fn test_empty_result_formatting() {
        let result = SearchResult::empty();
        assert!(result.is_empty());

        let ai = result.format_ai();
        assert!(ai.contains("# 0 results"));

        let pretty = result.format_pretty();
        assert!(pretty.contains("# 0 results"));

        let json = result.format_json();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["hits"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_format_ai_match_type_indicators() {
        let result = make_result(vec![
            make_hit("src/hybrid.rs", 0.02, MatchType::Hybrid),
            make_hit("src/semantic.rs", 0.01, MatchType::Semantic),
            make_hit("src/text.rs", 0.01, MatchType::Text),
        ]);
        let output = result.format_ai();

        // Hybrid gets " +" indicator
        assert!(output.contains(" +\n"));
        // Semantic gets " ~" indicator
        assert!(output.contains(" ~\n"));
        // Text gets no indicator — line ends with "%)"\n
    }
}
