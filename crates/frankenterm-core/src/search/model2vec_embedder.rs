//! Model2Vec embedder — fast distilled sentence embeddings.
//!
//! Requires the `semantic-search` feature.

use super::embedder::{EmbedError, Embedder, EmbedderInfo, EmbedderTier};

/// Model2Vec-based embedder for fast sentence embeddings.
pub struct Model2VecEmbedder {
    model_path: String,
    dimension: usize,
}

impl Model2VecEmbedder {
    /// Create a new Model2Vec embedder from a model directory.
    pub fn new(model_path: impl Into<String>, dimension: usize) -> Self {
        Self {
            model_path: model_path.into(),
            dimension,
        }
    }

    pub fn model_path(&self) -> &str {
        &self.model_path
    }
}

impl Embedder for Model2VecEmbedder {
    fn info(&self) -> EmbedderInfo {
        EmbedderInfo {
            name: format!("model2vec-{}", self.dimension),
            dimension: self.dimension,
            tier: EmbedderTier::Fast,
        }
    }

    fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbedError> {
        // Stub: real implementation loads ONNX model
        Err(EmbedError::ModelNotFound(self.model_path.clone()))
    }
}
