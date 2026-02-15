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

    // =====================================================================
    // EmbedError Display exhaustive tests
    // =====================================================================

    #[test]
    fn embed_error_display_model_not_found() {
        let e = EmbedError::ModelNotFound("my_model.bin".into());
        let msg = e.to_string();
        assert!(msg.contains("model not found"));
        assert!(msg.contains("my_model.bin"));
    }

    #[test]
    fn embed_error_display_tokenization_failed() {
        let e = EmbedError::TokenizationFailed("invalid UTF-8 input".into());
        let msg = e.to_string();
        assert!(msg.contains("tokenization failed"));
        assert!(msg.contains("invalid UTF-8 input"));
    }

    #[test]
    fn embed_error_display_inference_failed() {
        let e = EmbedError::InferenceFailed("out of memory".into());
        let msg = e.to_string();
        assert!(msg.contains("inference failed"));
        assert!(msg.contains("out of memory"));
    }

    #[test]
    fn embed_error_display_dimension_mismatch() {
        let e = EmbedError::DimensionMismatch {
            expected: 128,
            actual: 512,
        };
        let msg = e.to_string();
        assert!(msg.contains("dimension mismatch"));
        assert!(msg.contains("128"));
        assert!(msg.contains("512"));
    }

    #[test]
    fn embed_error_display_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "access denied");
        let e = EmbedError::Io(io_err);
        let msg = e.to_string();
        assert!(msg.contains("I/O error"));
        assert!(msg.contains("access denied"));
    }

    // =====================================================================
    // EmbedError source() tests (std::error::Error trait)
    // =====================================================================

    #[test]
    fn embed_error_source_io_has_source() {
        use std::error::Error;
        let io_err = std::io::Error::other("underlying");
        let e = EmbedError::Io(io_err);
        assert!(e.source().is_some());
    }

    #[test]
    fn embed_error_source_non_io_has_no_source() {
        use std::error::Error;
        let cases: Vec<EmbedError> = vec![
            EmbedError::ModelNotFound("x".into()),
            EmbedError::TokenizationFailed("x".into()),
            EmbedError::InferenceFailed("x".into()),
            EmbedError::DimensionMismatch {
                expected: 1,
                actual: 2,
            },
        ];
        for e in cases {
            assert!(e.source().is_none(), "Non-IO error should have no source");
        }
    }

    // =====================================================================
    // EmbedderTier tests
    // =====================================================================

    #[test]
    fn embedder_tier_eq() {
        assert_eq!(EmbedderTier::Hash, EmbedderTier::Hash);
        assert_eq!(EmbedderTier::Fast, EmbedderTier::Fast);
        assert_eq!(EmbedderTier::Quality, EmbedderTier::Quality);
    }

    #[test]
    fn embedder_tier_ne() {
        assert_ne!(EmbedderTier::Hash, EmbedderTier::Fast);
        assert_ne!(EmbedderTier::Fast, EmbedderTier::Quality);
        assert_ne!(EmbedderTier::Hash, EmbedderTier::Quality);
    }

    #[test]
    fn embedder_tier_debug() {
        assert_eq!(format!("{:?}", EmbedderTier::Hash), "Hash");
        assert_eq!(format!("{:?}", EmbedderTier::Fast), "Fast");
        assert_eq!(format!("{:?}", EmbedderTier::Quality), "Quality");
    }

    #[test]
    fn embedder_tier_clone_copy() {
        let t = EmbedderTier::Fast;
        let t2 = t; // Copy
        let t3 = t; // Copy (also Clone)
        assert_eq!(t, t2);
        assert_eq!(t, t3);
    }

    #[test]
    fn embedder_tier_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(EmbedderTier::Hash);
        set.insert(EmbedderTier::Fast);
        set.insert(EmbedderTier::Quality);
        assert_eq!(set.len(), 3);
        // Inserting duplicate shouldn't change count
        set.insert(EmbedderTier::Hash);
        assert_eq!(set.len(), 3);
    }

    // =====================================================================
    // EmbedderInfo tests
    // =====================================================================

    #[test]
    fn embedder_info_fields() {
        let info = EmbedderInfo {
            name: "test-model".into(),
            dimension: 384,
            tier: EmbedderTier::Quality,
        };
        assert_eq!(info.name, "test-model");
        assert_eq!(info.dimension, 384);
        assert_eq!(info.tier, EmbedderTier::Quality);
    }

    #[test]
    fn embedder_info_clone() {
        let info = EmbedderInfo {
            name: "clone-test".into(),
            dimension: 128,
            tier: EmbedderTier::Hash,
        };
        let cloned = info.clone();
        assert_eq!(cloned.name, "clone-test");
        assert_eq!(cloned.dimension, 128);
        assert_eq!(cloned.tier, EmbedderTier::Hash);
    }

    #[test]
    fn embedder_info_debug() {
        let info = EmbedderInfo {
            name: "dbg".into(),
            dimension: 64,
            tier: EmbedderTier::Fast,
        };
        let dbg = format!("{info:?}");
        assert!(dbg.contains("dbg"));
        assert!(dbg.contains("64"));
        assert!(dbg.contains("Fast"));
    }

    // =====================================================================
    // Mock Embedder trait implementation test
    // =====================================================================

    struct MockEmbedder {
        info: EmbedderInfo,
    }

    impl Embedder for MockEmbedder {
        fn info(&self) -> EmbedderInfo {
            self.info.clone()
        }

        fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbedError> {
            Ok(vec![0.0; self.info.dimension])
        }
    }

    #[test]
    fn mock_embedder_dimension() {
        let emb = MockEmbedder {
            info: EmbedderInfo {
                name: "mock".into(),
                dimension: 256,
                tier: EmbedderTier::Hash,
            },
        };
        assert_eq!(emb.dimension(), 256);
    }

    #[test]
    fn mock_embedder_tier() {
        let emb = MockEmbedder {
            info: EmbedderInfo {
                name: "mock".into(),
                dimension: 128,
                tier: EmbedderTier::Quality,
            },
        };
        assert_eq!(emb.tier(), EmbedderTier::Quality);
    }

    #[test]
    fn mock_embedder_embed_returns_correct_dimension() {
        let emb = MockEmbedder {
            info: EmbedderInfo {
                name: "mock".into(),
                dimension: 64,
                tier: EmbedderTier::Fast,
            },
        };
        let result = emb.embed("hello world").unwrap();
        assert_eq!(result.len(), 64);
    }

    #[test]
    fn mock_embedder_embed_batch_default_impl() {
        let emb = MockEmbedder {
            info: EmbedderInfo {
                name: "mock".into(),
                dimension: 32,
                tier: EmbedderTier::Hash,
            },
        };
        let results = emb.embed_batch(&["a", "b", "c"]).unwrap();
        assert_eq!(results.len(), 3);
        for v in &results {
            assert_eq!(v.len(), 32);
        }
    }

    #[test]
    fn mock_embedder_embed_batch_empty() {
        let emb = MockEmbedder {
            info: EmbedderInfo {
                name: "mock".into(),
                dimension: 16,
                tier: EmbedderTier::Fast,
            },
        };
        let results = emb.embed_batch(&[]).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn mock_embedder_info_returns_clone() {
        let emb = MockEmbedder {
            info: EmbedderInfo {
                name: "info-test".into(),
                dimension: 100,
                tier: EmbedderTier::Quality,
            },
        };
        let info = emb.info();
        assert_eq!(info.name, "info-test");
        assert_eq!(info.dimension, 100);
    }
}
