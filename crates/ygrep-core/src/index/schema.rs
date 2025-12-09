use tantivy::schema::{Schema, STORED, STRING, FAST, TextFieldIndexing, TextOptions, IndexRecordOption};
use tantivy::tokenizer::{TokenizerManager, TextAnalyzer, SimpleTokenizer, LowerCaser, RemoveLongFilter};

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
        }
    }
}

struct CodeTokenStream<'a> {
    text: &'a str,
    chars: std::iter::Peekable<std::str::CharIndices<'a>>,
    token: tantivy::tokenizer::Token,
}

impl<'a> tantivy::tokenizer::TokenStream for CodeTokenStream<'a> {
    fn advance(&mut self) -> bool {
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
                // For now, skip punctuation that's not part of identifiers
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
            self.token.text.push_str(&self.text[start..end]);
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

    // Document identification
    schema_builder.add_text_field(fields::DOC_ID, STRING | STORED);
    schema_builder.add_text_field(fields::PATH, STRING | STORED);
    schema_builder.add_text_field(fields::WORKSPACE, STRING | STORED);

    // File metadata
    schema_builder.add_u64_field(fields::MTIME, FAST | STORED);
    schema_builder.add_u64_field(fields::SIZE, FAST | STORED);
    schema_builder.add_text_field(fields::EXTENSION, STRING | STORED);

    // Content for full-text search
    schema_builder.add_text_field(fields::CONTENT, text_options);

    // Line range for the document/chunk
    schema_builder.add_u64_field(fields::LINE_START, FAST | STORED);
    schema_builder.add_u64_field(fields::LINE_END, FAST | STORED);

    // Chunk-specific fields
    schema_builder.add_text_field(fields::CHUNK_ID, STRING | STORED);
    schema_builder.add_text_field(fields::PARENT_DOC, STRING | STORED);

    schema_builder.build()
}

/// Schema field handles for efficient access
#[derive(Clone)]
pub struct SchemaFields {
    pub doc_id: tantivy::schema::Field,
    pub path: tantivy::schema::Field,
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
}
