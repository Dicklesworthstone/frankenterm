//! Embedding worker — deterministic embedding generation for daemon requests.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use super::protocol::{EmbedRequest, EmbedResponse};
use crate::search::{Embedder, HashEmbedder};

/// Worker that processes embedding requests.
#[derive(Debug)]
pub struct EmbedWorker {
    id: u32,
    processed: AtomicU64,
}

impl EmbedWorker {
    /// Create a new worker with the given ID.
    pub fn new(id: u32) -> Self {
        Self {
            id,
            processed: AtomicU64::new(0),
        }
    }

    /// Get the worker ID.
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Get the number of processed requests.
    pub fn processed(&self) -> u64 {
        self.processed.load(Ordering::Relaxed)
    }

    /// Process an embedding request.
    ///
    /// Supported model selectors:
    /// - `None`, `""`, `"hash"`, `"fnv1a-hash"` => default hash embedder
    /// - `"fnv1a-hash-<dimension>"` => hash embedder with explicit dimension
    /// - `"fastembed"`, `"fastembed-<model>"` => FastEmbed ONNX embedder (requires `semantic-search` feature)
    pub fn process(&self, request: &EmbedRequest) -> Result<EmbedResponse, String> {
        let started = Instant::now();
        let embedder = build_embedder(request.model.as_deref())?;
        let vector = embedder
            .embed(&request.text)
            .map_err(|err| err.to_string())?;
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.processed.fetch_add(1, Ordering::Relaxed);

        Ok(EmbedResponse {
            id: request.id,
            vector,
            model: embedder.info().name,
            elapsed_ms,
        })
    }
}

fn build_embedder(model: Option<&str>) -> Result<Box<dyn Embedder>, String> {
    match model.map(str::trim) {
        None | Some("" | "hash" | "fnv1a-hash") => Ok(Box::new(HashEmbedder::default())),
        Some(raw) => {
            if let Some(dim_raw) = raw.strip_prefix("fnv1a-hash-") {
                let dim = dim_raw
                    .parse::<usize>()
                    .map_err(|_| format!("invalid hash embedder dimension: {dim_raw}"))?;
                if dim == 0 {
                    return Err("hash embedder dimension must be > 0".to_string());
                }
                return Ok(Box::new(HashEmbedder::new(dim)));
            }
            // FastEmbed ONNX models (requires semantic-search feature).
            #[cfg(feature = "semantic-search")]
            {
                if raw == "fastembed" || raw.starts_with("fastembed-") {
                    return build_fastembed_embedder(raw);
                }
            }
            Err(format!("unsupported embed model: {raw}"))
        }
    }
}

/// Build a FastEmbed embedder from a model selector string.
///
/// Supported selectors:
/// - `"fastembed"` → default model (BGESmallENV15)
/// - `"fastembed-bge-small"` → BGESmallENV15
/// - `"fastembed-bge-base"` → BGEBaseENV15
/// - `"fastembed-bge-large"` → BGELargeENV15
/// - `"fastembed-minilm-l6"` → AllMiniLML6V2
/// - `"fastembed-minilm-l12"` → AllMiniLML12V2
#[cfg(feature = "semantic-search")]
fn build_fastembed_embedder(selector: &str) -> Result<Box<dyn Embedder>, String> {
    use crate::search::fastembed_embedder::{EmbeddingModel, FastEmbedConfig, FastEmbedEmbedder};

    let model = match selector {
        "fastembed" | "fastembed-bge-small" => EmbeddingModel::BGESmallENV15,
        "fastembed-bge-base" => EmbeddingModel::BGEBaseENV15,
        "fastembed-bge-large" => EmbeddingModel::BGELargeENV15,
        "fastembed-minilm-l6" => EmbeddingModel::AllMiniLML6V2,
        "fastembed-minilm-l12" => EmbeddingModel::AllMiniLML12V2,
        other => {
            return Err(format!(
                "unknown fastembed model selector: '{}'. \
                 Supported: fastembed, fastembed-bge-small, fastembed-bge-base, \
                 fastembed-bge-large, fastembed-minilm-l6, fastembed-minilm-l12",
                other
            ));
        }
    };

    let config = FastEmbedConfig::default().with_model(model);
    let emb = FastEmbedEmbedder::try_new(config).map_err(|e| e.to_string())?;
    Ok(Box::new(emb))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(id: u64, text: &str, model: Option<&str>) -> EmbedRequest {
        EmbedRequest {
            id,
            text: text.to_string(),
            model: model.map(ToString::to_string),
        }
    }

    #[test]
    fn process_default_hash_request() {
        let worker = EmbedWorker::new(7);
        let resp = worker.process(&req(1, "hello world", None)).unwrap();
        assert_eq!(resp.id, 1);
        assert_eq!(resp.model, "fnv1a-hash-128");
        assert_eq!(resp.vector.len(), 128);
        assert_eq!(worker.processed(), 1);
    }

    #[test]
    fn process_hash_dimension_override() {
        let worker = EmbedWorker::new(9);
        let resp = worker
            .process(&req(2, "semantic search", Some("fnv1a-hash-64")))
            .unwrap();
        assert_eq!(resp.id, 2);
        assert_eq!(resp.model, "fnv1a-hash-64");
        assert_eq!(resp.vector.len(), 64);
    }

    #[test]
    fn process_rejects_unknown_model() {
        let worker = EmbedWorker::new(3);
        let err = worker
            .process(&req(3, "ignored", Some("openai-ada-002")))
            .unwrap_err();
        assert!(err.contains("unsupported embed model"));
        assert_eq!(worker.processed(), 0);
    }

    #[test]
    fn process_rejects_invalid_dimension() {
        let worker = EmbedWorker::new(4);
        let err = worker
            .process(&req(4, "ignored", Some("fnv1a-hash-0")))
            .unwrap_err();
        assert!(err.contains("must be > 0"));
        assert_eq!(worker.processed(), 0);
    }

    // ── DarkMill test expansion ──────────────────────────────────────

    #[test]
    fn worker_id_is_stored() {
        let worker = EmbedWorker::new(42);
        assert_eq!(worker.id(), 42);
    }

    #[test]
    fn worker_id_zero() {
        let worker = EmbedWorker::new(0);
        assert_eq!(worker.id(), 0);
    }

    #[test]
    fn worker_id_max() {
        let worker = EmbedWorker::new(u32::MAX);
        assert_eq!(worker.id(), u32::MAX);
    }

    #[test]
    fn processed_starts_at_zero() {
        let worker = EmbedWorker::new(1);
        assert_eq!(worker.processed(), 0);
    }

    #[test]
    fn processed_increments_on_success() {
        let worker = EmbedWorker::new(1);
        worker.process(&req(1, "a", None)).unwrap();
        worker.process(&req(2, "b", None)).unwrap();
        worker.process(&req(3, "c", None)).unwrap();
        assert_eq!(worker.processed(), 3);
    }

    #[test]
    fn processed_does_not_increment_on_error() {
        let worker = EmbedWorker::new(1);
        worker.process(&req(1, "a", None)).unwrap();
        let _ = worker.process(&req(2, "x", Some("bad-model")));
        assert_eq!(worker.processed(), 1);
    }

    #[test]
    fn build_embedder_none_model() {
        let emb = build_embedder(None).unwrap();
        assert_eq!(emb.info().name, "fnv1a-hash-128");
    }

    #[test]
    fn build_embedder_empty_string() {
        let emb = build_embedder(Some("")).unwrap();
        assert_eq!(emb.info().name, "fnv1a-hash-128");
    }

    #[test]
    fn build_embedder_hash_alias() {
        let emb = build_embedder(Some("hash")).unwrap();
        assert_eq!(emb.info().name, "fnv1a-hash-128");
    }

    #[test]
    fn build_embedder_fnv1a_hash_alias() {
        let emb = build_embedder(Some("fnv1a-hash")).unwrap();
        assert_eq!(emb.info().name, "fnv1a-hash-128");
    }

    #[test]
    fn build_embedder_explicit_dimension_32() {
        let emb = build_embedder(Some("fnv1a-hash-32")).unwrap();
        assert_eq!(emb.info().name, "fnv1a-hash-32");
    }

    #[test]
    fn build_embedder_explicit_dimension_256() {
        let emb = build_embedder(Some("fnv1a-hash-256")).unwrap();
        assert_eq!(emb.info().name, "fnv1a-hash-256");
    }

    #[test]
    fn build_embedder_zero_dimension_err() {
        let result = build_embedder(Some("fnv1a-hash-0"));
        assert!(result.is_err());
        assert!(result.err().unwrap().contains("must be > 0"));
    }

    #[test]
    fn build_embedder_non_numeric_dimension_err() {
        let result = build_embedder(Some("fnv1a-hash-abc"));
        assert!(result.is_err());
        assert!(
            result
                .err()
                .unwrap()
                .contains("invalid hash embedder dimension")
        );
    }

    #[test]
    fn build_embedder_unsupported_model_err() {
        let result = build_embedder(Some("openai-ada-002"));
        assert!(result.is_err());
        assert!(result.err().unwrap().contains("unsupported embed model"));
    }

    #[test]
    fn build_embedder_whitespace_trimmed() {
        let emb = build_embedder(Some("  hash  ")).unwrap();
        assert_eq!(emb.info().name, "fnv1a-hash-128");
    }

    #[test]
    fn response_id_matches_request_id() {
        let worker = EmbedWorker::new(1);
        for id in [0, 1, 100, u64::MAX] {
            let resp = worker.process(&req(id, "test", None)).unwrap();
            assert_eq!(resp.id, id);
        }
    }

    #[test]
    fn response_has_nonzero_vector_values() {
        let worker = EmbedWorker::new(1);
        let resp = worker.process(&req(1, "hello world", None)).unwrap();
        let nonzero = resp
            .vector
            .iter()
            .filter(|v| v.abs() > f32::EPSILON)
            .count();
        assert!(nonzero > 0, "embedding should have nonzero values");
    }

    #[test]
    fn different_texts_produce_different_vectors() {
        let worker = EmbedWorker::new(1);
        let r1 = worker.process(&req(1, "hello world", None)).unwrap();
        let r2 = worker.process(&req(2, "goodbye world", None)).unwrap();
        assert_ne!(r1.vector, r2.vector);
    }

    #[test]
    fn same_text_produces_same_vector() {
        let worker = EmbedWorker::new(1);
        let r1 = worker.process(&req(1, "deterministic", None)).unwrap();
        let r2 = worker.process(&req(2, "deterministic", None)).unwrap();
        assert_eq!(r1.vector, r2.vector);
    }

    #[test]
    fn empty_text_succeeds() {
        let worker = EmbedWorker::new(1);
        let resp = worker.process(&req(1, "", None)).unwrap();
        assert_eq!(resp.vector.len(), 128);
    }

    #[test]
    fn long_text_succeeds() {
        let worker = EmbedWorker::new(1);
        let long_text = "a".repeat(100_000);
        let resp = worker.process(&req(1, &long_text, None)).unwrap();
        assert_eq!(resp.vector.len(), 128);
    }

    #[test]
    fn unicode_text_succeeds() {
        let worker = EmbedWorker::new(1);
        let resp = worker.process(&req(1, "日本語テスト 🦀", None)).unwrap();
        assert_eq!(resp.vector.len(), 128);
    }

    #[test]
    fn worker_debug_impl() {
        let worker = EmbedWorker::new(7);
        let dbg = format!("{worker:?}");
        assert!(dbg.contains("EmbedWorker"));
        assert!(dbg.contains("7"));
    }

    #[test]
    fn response_elapsed_ms_is_set() {
        let worker = EmbedWorker::new(1);
        let resp = worker.process(&req(1, "timing", None)).unwrap();
        // elapsed_ms should be a reasonable value (not u64::MAX)
        assert!(resp.elapsed_ms < 10_000);
    }

    #[test]
    fn multiple_workers_independent_counters() {
        let w1 = EmbedWorker::new(1);
        let w2 = EmbedWorker::new(2);
        w1.process(&req(1, "a", None)).unwrap();
        w1.process(&req(2, "b", None)).unwrap();
        w2.process(&req(3, "c", None)).unwrap();
        assert_eq!(w1.processed(), 2);
        assert_eq!(w2.processed(), 1);
    }

    #[test]
    fn dimension_1_produces_single_element_vector() {
        let worker = EmbedWorker::new(1);
        let resp = worker
            .process(&req(1, "test", Some("fnv1a-hash-1")))
            .unwrap();
        assert_eq!(resp.vector.len(), 1);
    }

    #[test]
    fn large_dimension_produces_large_vector() {
        let worker = EmbedWorker::new(1);
        let resp = worker
            .process(&req(1, "test", Some("fnv1a-hash-1024")))
            .unwrap();
        assert_eq!(resp.vector.len(), 1024);
    }
}
