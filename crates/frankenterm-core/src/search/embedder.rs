//! Core embedding trait and types.
use std::fmt;

#[derive(Debug)]
pub enum EmbedError {
    ModelNotFound(String),
    TokenizationFailed(String),
    InferenceFailed(String),
    DimensionMismatch { expected: usize, actual: usize },
    Io(std::io::Error),
}

impl fmt::Display for EmbedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ModelNotFound(p) => write!(f, "model not found: {p}"),
            Self::TokenizationFailed(e) => write!(f, "tokenization failed: {e}"),
            Self::InferenceFailed(e) => write!(f, "inference failed: {e}"),
            Self::DimensionMismatch { expected, actual } => {
                write!(f, "dimension mismatch: expected {expected}, got {actual}")
            }
            Self::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

impl std::error::Error for EmbedError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for EmbedError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EmbedderTier {
    Hash,
    Fast,
    Quality,
}

impl fmt::Display for EmbedderTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hash => write!(f, "hash"),
            Self::Fast => write!(f, "fast"),
            Self::Quality => write!(f, "quality"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct EmbedderInfo {
    pub name: String,
    pub dimension: usize,
    pub tier: EmbedderTier,
}

pub trait Embedder: Send + Sync {
    fn info(&self) -> EmbedderInfo;
    fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError>;
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        texts.iter().map(|t| self.embed(t)).collect()
    }
    fn dimension(&self) -> usize {
        self.info().dimension
    }
    fn tier(&self) -> EmbedderTier {
        self.info().tier
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embed_error_display() {
        let e = EmbedError::ModelNotFound("/tmp/model".into());
        assert!(e.to_string().contains("/tmp/model"));
        let e = EmbedError::DimensionMismatch {
            expected: 256,
            actual: 384,
        };
        assert!(e.to_string().contains("256"));
    }

    #[test]
    fn embedder_tier_display() {
        assert_eq!(EmbedderTier::Hash.to_string(), "hash");
        assert_eq!(EmbedderTier::Fast.to_string(), "fast");
        assert_eq!(EmbedderTier::Quality.to_string(), "quality");
    }

    #[test]
    fn embed_error_from_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "gone");
        let e = EmbedError::from(io_err);
        assert!(matches!(e, EmbedError::Io(_)));
    }
}
