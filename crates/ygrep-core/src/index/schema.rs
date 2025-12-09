use tantivy::schema::{Schema, STORED, STRING, FAST, TextFieldIndexing, TextOptions, IndexRecordOption};

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
    let text_options = TextOptions::default()
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("default")
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
