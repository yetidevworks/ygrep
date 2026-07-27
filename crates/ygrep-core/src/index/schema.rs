use std::collections::VecDeque;
use tantivy::schema::{
    IndexRecordOption, Schema, TextFieldIndexing, TextOptions, FAST, STORED, STRING,
};
use tantivy::tokenizer::{LowerCaser, RemoveLongFilter, TextAnalyzer, TokenizerManager};

/// Schema version - increment when schema changes require reindexing
pub const SCHEMA_VERSION: u32 = 6;

/// Default zstd level for the doc store
pub const DEFAULT_DOCSTORE_COMPRESSION_LEVEL: i32 = 3;

/// Lowest and highest zstd levels tantivy will accept
pub const MIN_DOCSTORE_COMPRESSION_LEVEL: i32 = 1;
pub const MAX_DOCSTORE_COMPRESSION_LEVEL: i32 = 22;

/// Index settings applied when creating a new index.
///
/// Tantivy defaults the doc store to LZ4. Stored file content is about half of a ygrep
/// index, and zstd compresses code roughly 40% smaller than LZ4 at Tantivy's block
/// granularity. Decompression is only ~19% slower and a query touches a handful of
/// blocks, so the trade lands firmly on the side of disk space.
///
/// The larger block size buys a further few percent: blocks compress better with more
/// context, and a block is cheap to decompress relative to the rest of a query.
///
/// `compression_level` selects the zstd level. Levels above the default trade indexing
/// speed for a smaller index; the stored bytes decompress the same either way, so the
/// level only affects how long a build takes, never how fast a search runs.
/// A level of 0 selects LZ4 instead, for the fastest possible indexing.
pub fn index_settings(compression_level: i32) -> tantivy::IndexSettings {
    use tantivy::store::{Compressor, ZstdCompressor};

    let docstore_compression = if compression_level == 0 {
        Compressor::Lz4
    } else {
        Compressor::Zstd(ZstdCompressor {
            compression_level: Some(clamp_compression_level(compression_level)),
        })
    };

    tantivy::IndexSettings {
        docstore_compression,
        docstore_blocksize: 65_536,
        ..Default::default()
    }
}

/// Hold the configured level inside the range tantivy accepts.
///
/// Out-of-range values come from hand-edited config, so warn rather than fail: an
/// unusable index is a worse outcome than a slightly different compression level.
fn clamp_compression_level(level: i32) -> i32 {
    if level < MIN_DOCSTORE_COMPRESSION_LEVEL || level > MAX_DOCSTORE_COMPRESSION_LEVEL {
        tracing::warn!(
            "docstore_compression_level {} is out of range ({}-{}), using {}",
            level,
            MIN_DOCSTORE_COMPRESSION_LEVEL,
            MAX_DOCSTORE_COMPRESSION_LEVEL,
            level.clamp(
                MIN_DOCSTORE_COMPRESSION_LEVEL,
                MAX_DOCSTORE_COMPRESSION_LEVEL
            )
        );
    }

    level.clamp(
        MIN_DOCSTORE_COMPRESSION_LEVEL,
        MAX_DOCSTORE_COMPRESSION_LEVEL,
    )
}

/// Name of our custom code tokenizer
pub const CODE_TOKENIZER: &str = "code";

/// Register the code-aware tokenizer with an index
pub fn register_tokenizers(tokenizer_manager: &TokenizerManager) {
    // Code tokenizer: keeps $, @, # as part of tokens
    // Uses SimpleTokenizer which splits on whitespace, then we just lowercase
    let code_tokenizer = TextAnalyzer::builder(CodeTokenizer)
        .filter(LowerCaser)
        .filter(RemoveLongFilter::limit(100))
        .build();

    tokenizer_manager.register(CODE_TOKENIZER, code_tokenizer);
}

/// Custom tokenizer for code that preserves $, @, #, etc.
#[derive(Clone)]
struct CodeTokenizer;

impl tantivy::tokenizer::Tokenizer for CodeTokenizer {
    type TokenStream<'a> = CodeTokenStream<'a>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        CodeTokenStream {
            text,
            chars: text.char_indices().peekable(),
            token: tantivy::tokenizer::Token::default(),
            subtoken_buffer: VecDeque::new(),
            subtoken_position: 0,
        }
    }
}

struct CodeTokenStream<'a> {
    text: &'a str,
    chars: std::iter::Peekable<std::str::CharIndices<'a>>,
    token: tantivy::tokenizer::Token,
    /// Buffered subtokens to emit at the same position as the parent token
    subtoken_buffer: VecDeque<String>,
    /// The position value to use for buffered subtokens
    subtoken_position: usize,
}

/// Split a token into subtokens at camelCase and snake_case boundaries.
/// Returns subtokens only if there are 2+ parts; returns empty vec for simple tokens.
fn split_subtokens(text: &str) -> Vec<String> {
    let mut parts = Vec::new();

    // First handle snake_case: split on underscores
    let segments: Vec<&str> = text.split('_').filter(|s| !s.is_empty()).collect();

    // If there were underscores and multiple segments, process each for camelCase too
    for segment in &segments {
        // Split on camelCase boundaries within each segment
        let chars: Vec<char> = segment.chars().collect();
        let mut part_start = 0;

        for i in 1..chars.len() {
            // camelCase boundary: lowercase followed by uppercase
            if chars[i - 1].is_lowercase() && chars[i].is_uppercase() {
                let part: String = chars[part_start..i].iter().collect();
                if !part.is_empty() {
                    parts.push(part);
                }
                part_start = i;
            }
        }
        // Push the remaining part
        let part: String = chars[part_start..].iter().collect();
        if !part.is_empty() {
            parts.push(part);
        }
    }

    // Only return subtokens if we actually split the token
    if parts.len() <= 1 {
        return Vec::new();
    }

    parts
}

impl<'a> tantivy::tokenizer::TokenStream for CodeTokenStream<'a> {
    fn advance(&mut self) -> bool {
        // First, check if we have buffered subtokens to emit
        if let Some(subtoken) = self.subtoken_buffer.pop_front() {
            self.token.text.clear();
            self.token.text.push_str(&subtoken);
            // Keep the same position as the parent token
            self.token.position = self.subtoken_position;
            return true;
        }

        self.token.text.clear();
        self.token.position = self.token.position.wrapping_add(1);

        // Skip whitespace
        while let Some(&(_, c)) = self.chars.peek() {
            if !c.is_whitespace() {
                break;
            }
            self.chars.next();
        }

        let start = match self.chars.peek() {
            Some(&(pos, _)) => pos,
            None => return false,
        };

        // Collect token: alphanumeric + code chars ($, @, #, _, -)
        let mut end = start;
        while let Some(&(pos, c)) = self.chars.peek() {
            if c.is_alphanumeric() || c == '_' || c == '$' || c == '@' || c == '#' || c == '-' {
                end = pos + c.len_utf8();
                self.chars.next();
            } else if c.is_whitespace() {
                break;
            } else {
                // Other punctuation - emit as separate token or skip
                self.chars.next();
                if start == pos {
                    // Started with punctuation, skip and try again
                    return self.advance();
                }
                break;
            }
        }

        if end > start {
            self.token.offset_from = start;
            self.token.offset_to = end;
            let token_text = &self.text[start..end];
            self.token.text.push_str(token_text);

            // Check for camelCase/snake_case subtokens
            let subtokens = split_subtokens(token_text);
            if !subtokens.is_empty() {
                self.subtoken_position = self.token.position;
                for sub in subtokens {
                    self.subtoken_buffer.push_back(sub);
                }
            }

            true
        } else {
            false
        }
    }

    fn token(&self) -> &tantivy::tokenizer::Token {
        &self.token
    }

    fn token_mut(&mut self) -> &mut tantivy::tokenizer::Token {
        &mut self.token
    }
}

/// Field names for the document index
pub mod fields {
    pub const DOC_ID: &str = "doc_id";
    pub const PATH: &str = "path";
    pub const WORKSPACE: &str = "workspace";
    pub const CONTENT: &str = "content";
    pub const MTIME: &str = "mtime";
    pub const SIZE: &str = "size";
    pub const EXTENSION: &str = "extension";
    pub const LINE_START: &str = "line_start";
    pub const LINE_END: &str = "line_end";
    pub const CHUNK_ID: &str = "chunk_id";
    pub const PARENT_DOC: &str = "parent_doc";
    pub const FILEPATH: &str = "filepath";
}

/// Build the Tantivy schema for document indexing
pub fn build_document_schema() -> Schema {
    let mut schema_builder = Schema::builder();

    // Content field with positions for phrase queries
    // Uses our custom "code" tokenizer that preserves $, @, #, etc.
    let text_options = TextOptions::default()
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(CODE_TOKENIZER)
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        )
        .set_stored();

    // STRING + STORED + FAST for fields used in incremental indexing lookups
    let string_stored_fast = TextOptions::default()
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("raw")
                .set_index_option(IndexRecordOption::Basic),
        )
        .set_stored()
        .set_fast(None);

    // Document identification (fast for incremental index map building)
    schema_builder.add_text_field(fields::DOC_ID, string_stored_fast.clone());
    schema_builder.add_text_field(fields::PATH, string_stored_fast.clone());
    schema_builder.add_text_field(fields::WORKSPACE, STRING | STORED);

    // File metadata
    schema_builder.add_u64_field(fields::MTIME, FAST | STORED);
    schema_builder.add_u64_field(fields::SIZE, FAST | STORED);
    schema_builder.add_text_field(fields::EXTENSION, STRING | STORED);

    // Searchable file path (uses code tokenizer so path segments are searchable)
    let filepath_options = TextOptions::default()
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(CODE_TOKENIZER)
                .set_index_option(IndexRecordOption::Basic),
        )
        .set_stored();
    schema_builder.add_text_field(fields::FILEPATH, filepath_options);

    // Content for full-text search
    schema_builder.add_text_field(fields::CONTENT, text_options);

    // Line range for the document/chunk
    schema_builder.add_u64_field(fields::LINE_START, FAST | STORED);
    schema_builder.add_u64_field(fields::LINE_END, FAST | STORED);

    // Chunk-specific fields (CHUNK_ID is fast for incremental index filtering)
    schema_builder.add_text_field(fields::CHUNK_ID, string_stored_fast);
    schema_builder.add_text_field(fields::PARENT_DOC, STRING | STORED);

    schema_builder.build()
}

/// Schema field handles for efficient access
#[derive(Clone)]
pub struct SchemaFields {
    pub doc_id: tantivy::schema::Field,
    pub path: tantivy::schema::Field,
    pub filepath: Option<tantivy::schema::Field>,
    pub workspace: tantivy::schema::Field,
    pub content: tantivy::schema::Field,
    pub mtime: tantivy::schema::Field,
    pub size: tantivy::schema::Field,
    pub extension: tantivy::schema::Field,
    pub line_start: tantivy::schema::Field,
    pub line_end: tantivy::schema::Field,
    pub chunk_id: tantivy::schema::Field,
    pub parent_doc: tantivy::schema::Field,
}

impl SchemaFields {
    pub fn new(schema: &Schema) -> Self {
        Self {
            doc_id: schema.get_field(fields::DOC_ID).unwrap(),
            path: schema.get_field(fields::PATH).unwrap(),
            filepath: schema.get_field(fields::FILEPATH).ok(),
            workspace: schema.get_field(fields::WORKSPACE).unwrap(),
            content: schema.get_field(fields::CONTENT).unwrap(),
            mtime: schema.get_field(fields::MTIME).unwrap(),
            size: schema.get_field(fields::SIZE).unwrap(),
            extension: schema.get_field(fields::EXTENSION).unwrap(),
            line_start: schema.get_field(fields::LINE_START).unwrap(),
            line_end: schema.get_field(fields::LINE_END).unwrap(),
            chunk_id: schema.get_field(fields::CHUNK_ID).unwrap(),
            parent_doc: schema.get_field(fields::PARENT_DOC).unwrap(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tantivy::tokenizer::TokenStream;

    /// Helper: tokenize text with the code tokenizer and return token strings
    fn tokenize(text: &str) -> Vec<String> {
        let mut tokenizer = TextAnalyzer::builder(CodeTokenizer)
            .filter(LowerCaser)
            .filter(RemoveLongFilter::limit(100))
            .build();
        let mut stream = tokenizer.token_stream(text);
        let mut tokens = Vec::new();
        while stream.advance() {
            tokens.push(stream.token().text.clone());
        }
        tokens
    }

    #[test]
    fn index_settings_use_zstd_for_the_doc_store() {
        use tantivy::store::Compressor;

        let settings = index_settings(DEFAULT_DOCSTORE_COMPRESSION_LEVEL);

        assert!(
            matches!(settings.docstore_compression, Compressor::Zstd(_)),
            "doc store must be zstd, got {:?}",
            settings.docstore_compression
        );
        assert_eq!(settings.docstore_blocksize, 65_536);
    }

    #[test]
    fn compression_level_is_configurable() {
        use tantivy::store::{Compressor, ZstdCompressor};

        let settings = index_settings(9);

        assert!(matches!(
            settings.docstore_compression,
            Compressor::Zstd(ZstdCompressor {
                compression_level: Some(9)
            })
        ));
    }

    #[test]
    fn compression_level_zero_selects_lz4() {
        use tantivy::store::Compressor;

        let settings = index_settings(0);

        assert!(
            matches!(settings.docstore_compression, Compressor::Lz4),
            "level 0 must fall back to lz4, got {:?}",
            settings.docstore_compression
        );
    }

    #[test]
    fn out_of_range_compression_levels_are_clamped() {
        use tantivy::store::{Compressor, ZstdCompressor};

        // Negative levels are valid zstd but tantivy rejects them, so clamp up.
        assert!(matches!(
            index_settings(-5).docstore_compression,
            Compressor::Zstd(ZstdCompressor {
                compression_level: Some(MIN_DOCSTORE_COMPRESSION_LEVEL)
            })
        ));

        assert!(matches!(
            index_settings(99).docstore_compression,
            Compressor::Zstd(ZstdCompressor {
                compression_level: Some(MAX_DOCSTORE_COMPRESSION_LEVEL)
            })
        ));
    }

    #[test]
    fn a_created_index_carries_the_doc_store_settings() {
        use tantivy::store::Compressor;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let index = tantivy::Index::builder()
            .schema(build_document_schema())
            .settings(index_settings(DEFAULT_DOCSTORE_COMPRESSION_LEVEL))
            .create_in_dir(dir.path())
            .unwrap();

        assert!(matches!(
            index.settings().docstore_compression,
            Compressor::Zstd(_)
        ));
    }

    #[test]
    fn test_schema_creation() {
        let schema = build_document_schema();
        let fields = SchemaFields::new(&schema);

        // Verify all fields are accessible
        assert!(schema.get_field(fields::DOC_ID).is_ok());
        assert!(schema.get_field(fields::PATH).is_ok());
        assert!(schema.get_field(fields::CONTENT).is_ok());

        // Verify field handles work
        let _ = fields.doc_id;
        let _ = fields.content;
    }

    #[test]
    fn test_tokenizer_preserves_code_chars() {
        // $variable, @decorator, #include should be preserved as tokens
        let tokens = tokenize("$variable @decorator #include");
        assert!(tokens.contains(&"$variable".to_string()));
        assert!(tokens.contains(&"@decorator".to_string()));
        assert!(tokens.contains(&"#include".to_string()));

        // Hyphen is kept (e.g., CSS class names like "my-class")
        let tokens = tokenize("my-class foo-bar");
        assert!(tokens.contains(&"my-class".to_string()));
        assert!(tokens.contains(&"foo-bar".to_string()));

        // Underscore is kept (identifiers like "hello_world")
        let tokens = tokenize("hello_world some_func");
        assert!(tokens.contains(&"hello_world".to_string()));
        assert!(tokens.contains(&"some_func".to_string()));
    }

    #[test]
    fn test_tokenizer_lowercases() {
        let tokens = tokenize("FnMain HelloWorld UPPER");
        // Full tokens are lowercased
        assert!(tokens.contains(&"fnmain".to_string()));
        assert!(tokens.contains(&"helloworld".to_string()));
        assert!(tokens.contains(&"upper".to_string()));
    }

    #[test]
    fn test_tokenizer_camelcase_subtokens() {
        let tokens = tokenize("sendCampaign");
        // Full token
        assert!(tokens.contains(&"sendcampaign".to_string()));
        // Subtokens from camelCase split
        assert!(tokens.contains(&"send".to_string()));
        assert!(tokens.contains(&"campaign".to_string()));
    }

    #[test]
    fn test_tokenizer_snake_case_subtokens() {
        let tokens = tokenize("send_campaign");
        // Full token
        assert!(tokens.contains(&"send_campaign".to_string()));
        // Subtokens from snake_case split
        assert!(tokens.contains(&"send".to_string()));
        assert!(tokens.contains(&"campaign".to_string()));
    }

    #[test]
    fn test_tokenizer_mixed_case_subtokens() {
        // camelCase within snake_case segments
        let tokens = tokenize("myQueue_sendCampaign");
        assert!(tokens.contains(&"myqueue_sendcampaign".to_string()));
        // snake_case split
        assert!(tokens.contains(&"my".to_string()));
        assert!(tokens.contains(&"queue".to_string()));
        assert!(tokens.contains(&"send".to_string()));
        assert!(tokens.contains(&"campaign".to_string()));
    }

    #[test]
    fn test_tokenizer_no_subtokens_for_simple() {
        // Single word should not produce subtokens
        let tokens = tokenize("hello");
        assert_eq!(tokens, vec!["hello".to_string()]);
    }

    #[test]
    fn test_tokenizer_removes_long_tokens() {
        let long_token = "a".repeat(101);
        let text = format!("short {} end", long_token);
        let tokens = tokenize(&text);
        assert!(tokens.contains(&"short".to_string()));
        assert!(tokens.contains(&"end".to_string()));
        // The 101-char token should be removed by RemoveLongFilter
        assert!(!tokens.iter().any(|t| t.len() > 100));
    }
}
