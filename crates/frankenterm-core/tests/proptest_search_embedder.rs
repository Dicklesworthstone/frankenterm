//! Property-based tests for core search embedder carrier types.

use frankenterm_core::search::{EmbedError, Embedder, EmbedderInfo, EmbedderTier};
use proptest::prelude::*;

#[derive(Debug, Clone)]
struct DummyEmbedder {
    info: EmbedderInfo,
}

impl Embedder for DummyEmbedder {
    fn info(&self) -> EmbedderInfo {
        self.info.clone()
    }

    fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        Ok(vec![text.len() as f32; self.info.dimension])
    }
}

fn arb_tier() -> impl Strategy<Value = EmbedderTier> {
    prop_oneof![
        Just(EmbedderTier::Hash),
        Just(EmbedderTier::Fast),
        Just(EmbedderTier::Quality),
    ]
}

fn arb_embed_error() -> impl Strategy<Value = EmbedError> {
    prop_oneof![
        "[A-Za-z0-9_./-]{3,40}".prop_map(EmbedError::ModelNotFound),
        "[A-Za-z0-9 _.-]{3,40}".prop_map(EmbedError::TokenizationFailed),
        "[A-Za-z0-9 _.-]{3,40}".prop_map(EmbedError::InferenceFailed),
        (1usize..4096, 0usize..4096)
            .prop_map(|(expected, actual)| EmbedError::DimensionMismatch { expected, actual }),
        "[A-Za-z0-9 _.-]{3,40}"
            .prop_map(|message| EmbedError::from(std::io::Error::other(message))),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn embedder_tier_display_stays_lowercase_and_stable(tier in arb_tier()) {
        let rendered = tier.to_string();
        let rerendered = tier.to_string();

        prop_assert_eq!(&rendered, &rerendered);
        prop_assert!(rendered.chars().all(|ch| ch.is_ascii_lowercase()));
        match tier {
            EmbedderTier::Hash => prop_assert_eq!(rendered, "hash"),
            EmbedderTier::Fast => prop_assert_eq!(rendered, "fast"),
            EmbedderTier::Quality => prop_assert_eq!(rendered, "quality"),
        }
    }

    #[test]
    fn embed_error_display_and_source_match_variant(err in arb_embed_error()) {
        let display = err.to_string();
        prop_assert!(!display.is_empty());

        match &err {
            EmbedError::ModelNotFound(path) => {
                prop_assert!(display.contains("model not found"));
                prop_assert!(display.contains(path));
                prop_assert!(std::error::Error::source(&err).is_none());
            }
            EmbedError::TokenizationFailed(message) => {
                prop_assert!(display.contains("tokenization failed"));
                prop_assert!(display.contains(message));
                prop_assert!(std::error::Error::source(&err).is_none());
            }
            EmbedError::InferenceFailed(message) => {
                prop_assert!(display.contains("inference failed"));
                prop_assert!(display.contains(message));
                prop_assert!(std::error::Error::source(&err).is_none());
            }
            EmbedError::DimensionMismatch { expected, actual } => {
                prop_assert!(display.contains("dimension mismatch"));
                prop_assert!(display.contains(&expected.to_string()));
                prop_assert!(display.contains(&actual.to_string()));
                prop_assert!(std::error::Error::source(&err).is_none());
            }
            EmbedError::Io(source) => {
                prop_assert!(display.contains("I/O error"));
                prop_assert!(display.contains(&source.to_string()));
                let embedded_source = std::error::Error::source(&err).unwrap();
                prop_assert_eq!(embedded_source.to_string(), source.to_string());
            }
        }
    }

    #[test]
    fn embedder_trait_helpers_follow_info_contract(
        name in "[a-z0-9_-]{3,24}",
        dimension in 1usize..256,
        tier in arb_tier(),
        texts in prop::collection::vec("[A-Za-z0-9 _.-]{0,40}", 0..12),
    ) {
        let embedder = DummyEmbedder {
            info: EmbedderInfo { name, dimension, tier },
        };
        let borrowed: Vec<&str> = texts.iter().map(String::as_str).collect();
        let batch = embedder.embed_batch(&borrowed).unwrap();

        prop_assert_eq!(embedder.dimension(), dimension);
        prop_assert_eq!(embedder.tier(), tier);
        prop_assert_eq!(embedder.info().dimension, dimension);
        prop_assert_eq!(embedder.info().tier, tier);
        prop_assert_eq!(batch.len(), texts.len());
        for (embedding, text) in batch.iter().zip(texts.iter()) {
            prop_assert_eq!(embedding.len(), dimension);
            prop_assert!(embedding.iter().all(|value| (*value - text.len() as f32).abs() < f32::EPSILON));
        }
    }
}
