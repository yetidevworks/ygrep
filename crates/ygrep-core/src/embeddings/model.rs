//! Embedding model wrapper using fastembed
//!
//! Provides lazy-loaded embedding generation using local models.

use std::sync::Arc;
use parking_lot::RwLock;
use fastembed::{TextEmbedding, InitOptions, EmbeddingModel as FastEmbedModel};

use crate::error::{Result, YgrepError};

/// Supported embedding models
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelType {
    /// BGE Small - Fast, ~50MB, 384 dimensions
    BgeSmall,
    /// All-MiniLM-L6 - Very fast, ~25MB, 384 dimensions
    AllMiniLmL6,
}

impl ModelType {
    pub fn dimension(&self) -> usize {
        match self {
            ModelType::BgeSmall => 384,
            ModelType::AllMiniLmL6 => 384,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            ModelType::BgeSmall => "BAAI/bge-small-en-v1.5",
            ModelType::AllMiniLmL6 => "sentence-transformers/all-MiniLM-L6-v2",
        }
    }

    fn to_fastembed(&self) -> FastEmbedModel {
        match self {
            ModelType::BgeSmall => FastEmbedModel::BGESmallENV15,
            ModelType::AllMiniLmL6 => FastEmbedModel::AllMiniLML6V2,
        }
    }
}

impl Default for ModelType {
    fn default() -> Self {
        ModelType::AllMiniLmL6
    }
}

/// Lazy-loaded embedding model
pub struct EmbeddingModel {
    model_type: ModelType,
    model: RwLock<Option<Arc<TextEmbedding>>>,
}

impl EmbeddingModel {
    /// Create a new embedding model (lazy-loaded)
    pub fn new(model_type: ModelType) -> Self {
        Self {
            model_type,
            model: RwLock::new(None),
        }
    }

    /// Get the embedding dimension
    pub fn dimension(&self) -> usize {
        self.model_type.dimension()
    }

    /// Get the model name
    pub fn name(&self) -> &'static str {
        self.model_type.name()
    }

    /// Load the model if not already loaded
    fn ensure_loaded(&self) -> Result<Arc<TextEmbedding>> {
        // Fast path: model already loaded
        {
            let guard = self.model.read();
            if let Some(ref model) = *guard {
                return Ok(Arc::clone(model));
            }
        }

        // Slow path: load the model
        let mut guard = self.model.write();

        // Double-check after acquiring write lock
        if let Some(ref model) = *guard {
            return Ok(Arc::clone(model));
        }

        eprint!("Loading embedding model...");

        let model = TextEmbedding::try_new(
            InitOptions::new(self.model_type.to_fastembed())
                .with_show_download_progress(true)
        ).map_err(|e| YgrepError::Config(format!("Failed to load embedding model: {}", e)))?;

        let model = Arc::new(model);
        *guard = Some(Arc::clone(&model));

        eprintln!(" done");

        Ok(model)
    }

    /// Generate embedding for a single text
    pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let model = self.ensure_loaded()?;
        let embeddings = model.embed(vec![text], None)
            .map_err(|e| YgrepError::Config(format!("Embedding failed: {}", e)))?;

        embeddings.into_iter().next()
            .ok_or_else(|| YgrepError::Config("No embedding returned".to_string()))
    }

    /// Generate embeddings for multiple texts (batched)
    pub fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }

        let model = self.ensure_loaded()?;
        model.embed(texts.to_vec(), None)
            .map_err(|e| YgrepError::Config(format!("Batch embedding failed: {}", e)))
    }

    /// Check if the model is loaded
    pub fn is_loaded(&self) -> bool {
        self.model.read().is_some()
    }
}

impl Default for EmbeddingModel {
    fn default() -> Self {
        Self::new(ModelType::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_dimensions() {
        assert_eq!(ModelType::BgeSmall.dimension(), 384);
        assert_eq!(ModelType::AllMiniLmL6.dimension(), 384);
    }

    // Note: Full embedding tests require model download
    // They are expensive and should be run separately
    #[test]
    #[ignore]
    fn test_embedding_generation() {
        let model = EmbeddingModel::new(ModelType::AllMiniLmL6);
        let embedding = model.embed("Hello, world!").unwrap();
        assert_eq!(embedding.len(), 384);
    }
}
