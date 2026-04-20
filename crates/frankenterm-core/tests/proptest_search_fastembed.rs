//! Property-based tests for fastembed configuration and selector helpers.

use std::path::PathBuf;

use fastembed::EmbeddingModel;
use frankenterm_core::search::{
    FastEmbedConfig, resolve_fastembed_model_selector, supported_fastembed_model_selectors,
};
use proptest::prelude::*;

fn arb_supported_selector() -> impl Strategy<Value = &'static str> {
    prop_oneof![
        Just("fastembed"),
        Just("fastembed-bge-small"),
        Just("fastembed-bge-base"),
        Just("fastembed-bge-large"),
        Just("fastembed-minilm-l6"),
        Just("fastembed-minilm-l12"),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn fastembed_config_builder_methods_preserve_overrides(
        max_length in 1usize..4096,
        show_download_progress in any::<bool>(),
        cache_tail in "[a-z0-9_./-]{3,40}",
    ) {
        let cache_dir = PathBuf::from(format!("/tmp/{cache_tail}"));
        let config = FastEmbedConfig::default()
            .with_model(EmbeddingModel::AllMiniLML6V2)
            .with_cache_dir(cache_dir.clone())
            .with_max_length(max_length)
            .with_show_download_progress(show_download_progress);

        prop_assert_eq!(config.model, EmbeddingModel::AllMiniLML6V2);
        prop_assert_eq!(config.cache_dir, cache_dir);
        prop_assert_eq!(config.max_length, max_length);
        prop_assert_eq!(config.show_download_progress, show_download_progress);
    }

    #[test]
    fn supported_fastembed_selectors_resolve(
        selector in arb_supported_selector()
    ) {
        let supported = supported_fastembed_model_selectors();
        prop_assert!(supported.contains(&selector));

        let model = resolve_fastembed_model_selector(selector).unwrap();
        match selector {
            "fastembed" | "fastembed-bge-small" => prop_assert_eq!(model, EmbeddingModel::BGESmallENV15),
            "fastembed-bge-base" => prop_assert_eq!(model, EmbeddingModel::BGEBaseENV15),
            "fastembed-bge-large" => prop_assert_eq!(model, EmbeddingModel::BGELargeENV15),
            "fastembed-minilm-l6" => prop_assert_eq!(model, EmbeddingModel::AllMiniLML6V2),
            "fastembed-minilm-l12" => prop_assert_eq!(model, EmbeddingModel::AllMiniLML12V2),
            _ => prop_assert!(false, "unexpected selector"),
        }
    }

    #[test]
    fn unknown_fastembed_selector_is_rejected(
        selector in "[a-z0-9_-]{3,24}"
    ) {
        prop_assume!(!supported_fastembed_model_selectors().contains(&selector.as_str()));
        let err = resolve_fastembed_model_selector(&selector).unwrap_err();
        prop_assert!(err.contains("unknown fastembed model selector"));
        prop_assert!(err.contains(&selector));
    }
}
