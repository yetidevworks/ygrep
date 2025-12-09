use serde::{Deserialize, Serialize};

/// Result of a search operation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    /// Search hits
    pub hits: Vec<SearchHit>,
    /// Total number of results (may be more than hits if limited)
    pub total: usize,
    /// Query execution time in milliseconds
    pub query_time_ms: u64,
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
        }
    }

    /// Check if there are any results
    pub fn is_empty(&self) -> bool {
        self.hits.is_empty()
    }

    /// Format results for AI-optimized output (minimal tokens)
    pub fn format_ai(&self) -> String {
        let mut output = String::new();

        output.push_str(&format!("# {} results ({:.1}ms)\n\n", self.hits.len(), self.query_time_ms as f64));

        for (i, hit) in self.hits.iter().enumerate() {
            // Path and line range
            output.push_str(&format!("{}. `{}:{}`\n", i + 1, hit.path, hit.lines_str()));

            // Snippet (truncated)
            let snippet = truncate_snippet(&hit.snippet, 200);
            if !snippet.is_empty() {
                output.push_str("```\n");
                output.push_str(&snippet);
                if !snippet.ends_with('\n') {
                    output.push('\n');
                }
                output.push_str("```\n\n");
            }
        }

        output
    }

    /// Format results as JSON
    pub fn format_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string())
    }

    /// Format results for human-readable output
    pub fn format_pretty(&self) -> String {
        let mut output = String::new();

        output.push_str(&format!("Found {} results in {:.1}ms\n", self.hits.len(), self.query_time_ms as f64));
        output.push_str(&"─".repeat(50));
        output.push('\n');

        for hit in &self.hits {
            output.push_str(&format!("\n📄 {} (lines {})\n", hit.path, hit.lines_str()));
            output.push_str(&format!("   Score: {:.2}\n", hit.score));

            let snippet = truncate_snippet(&hit.snippet, 300);
            for line in snippet.lines().take(5) {
                output.push_str(&format!("   │ {}\n", line));
            }
        }

        output
    }
}

/// Truncate a snippet to a maximum character length
fn truncate_snippet(s: &str, max_chars: usize) -> String {
    if s.len() <= max_chars {
        return s.to_string();
    }

    // Find a good breaking point (newline)
    if let Some(pos) = s[..max_chars].rfind('\n') {
        return s[..pos].to_string();
    }

    // Fall back to character limit
    s.chars().take(max_chars).collect()
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
            hits: vec![
                SearchHit {
                    path: "src/main.rs".to_string(),
                    line_start: 1,
                    line_end: 10,
                    snippet: "fn main() {\n    println!(\"hello\");\n}".to_string(),
                    score: 0.9,
                    is_chunk: false,
                    doc_id: "abc".to_string(),
                },
            ],
            total: 1,
            query_time_ms: 15,
        };

        let output = result.format_ai();
        assert!(output.contains("# 1 results"));
        assert!(output.contains("src/main.rs:1-10"));
    }
}
