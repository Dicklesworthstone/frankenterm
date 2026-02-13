//! FastEmbed embedder — quality sentence embeddings via ONNX Runtime.
//!
//! Requires the `semantic-search` feature.

use super::embedder::{EmbedError, Embedder, EmbedderInfo, EmbedderTier};

/// FastEmbed-based embedder for quality sentence embeddings.
pub struct FastEmbedEmbedder {
    model_name: String,
    dimension: usize,
}

impl FastEmbedEmbedder {
    /// Create a new FastEmbed embedder.
    pub fn new(model_name: impl Into<String>, dimension: usize) -> Self {
        Self {
            model_name: model_name.into(),
            dimension,
        }
    }

    pub fn model_name(&self) -> &str {
        &self.model_name
    }
}

impl Embedder for FastEmbedEmbedder {
    fn info(&self) -> EmbedderInfo {
        EmbedderInfo {
            name: format!("fastembed-{}", self.model_name),
            dimension: self.dimension,
            tier: EmbedderTier::Quality,
        }
    }

    fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbedError> {
        // Stub: real implementation uses fastembed crate
        Err(EmbedError::ModelNotFound(self.model_name.clone()))
    }
}
