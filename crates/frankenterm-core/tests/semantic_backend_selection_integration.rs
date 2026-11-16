#![cfg(feature = "semantic-search")]

use frankenterm_core::search::{
    EmbedError, FastEmbedConfig, FastEmbedEmbedder, advertised_embedder_tiers,
    daemon::{EmbedRequest, EmbedWorker},
    resolve_fastembed_model_selector, supported_fastembed_model_selectors,
};

fn request(model: Option<&str>) -> EmbedRequest {
    EmbedRequest {
        id: 1,
        text: "semantic backend probe".to_string(),
        model: model.map(ToString::to_string),
    }
}

#[test]
fn public_backend_surface_advertises_only_supported_semantic_backend() {
    let tiers = advertised_embedder_tiers(true, true);
    assert_eq!(
        tiers,
        vec![
            "hash".to_string(),
            "fastembed".to_string(),
            "cross-encoder".to_string(),
        ]
    );
    assert!(!tiers.iter().any(|tier| tier == "model2vec"));
}

#[test]
fn supported_fastembed_selectors_are_resolvable() {
    for selector in supported_fastembed_model_selectors() {
        resolve_fastembed_model_selector(selector)
            .unwrap_or_else(|err| panic!("selector {selector} should resolve: {err}"));
    }
}

#[test]
fn embed_worker_rejects_retired_model2vec_selector() {
    let worker = EmbedWorker::new(11);
    let err = worker.process(&request(Some("model2vec"))).unwrap_err();
    assert!(err.contains("unsupported embed model"));
}

#[test]
fn fastembed_initialization_fails_eagerly_for_invalid_cache_dir_file() {
    let temp_file = tempfile::NamedTempFile::new().expect("create temp file");
    let config = FastEmbedConfig::default().with_cache_dir(temp_file.path());
    let err = match FastEmbedEmbedder::try_new(config) {
        Ok(_) => panic!("cache dir pointing at a file should fail eagerly"),
        Err(err) => err,
    };
    assert!(matches!(
        err,
        EmbedError::ModelNotFound(_) | EmbedError::InferenceFailed(_)
    ));
}
