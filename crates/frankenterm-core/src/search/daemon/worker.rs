//! Embedding worker — deterministic embedding generation for daemon requests.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use super::protocol::{EmbedRequest, EmbedResponse};
use crate::search::{Embedder, HashEmbedder};

type SharedEmbedder = Arc<dyn Embedder>;

static EMBEDDER_CACHE_LOCK_POISONED: AtomicU64 = AtomicU64::new(0);

/// Number of times an embedder cache lock was poisoned and recovered.
pub fn embedder_cache_lock_poisoned_count() -> u64 {
    EMBEDDER_CACHE_LOCK_POISONED.load(Ordering::Relaxed)
}

/// Worker that processes embedding requests.
pub struct EmbedWorker {
    id: u32,
    processed: AtomicU64,
    embedders: Mutex<HashMap<String, SharedEmbedder>>,
}

impl fmt::Debug for EmbedWorker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let cached_embedders = self.cached_embedders();
        f.debug_struct("EmbedWorker")
            .field("id", &self.id)
            .field("processed", &self.processed())
            .field("cached_embedders", &cached_embedders)
            .finish()
    }
}

impl EmbedWorker {
    /// Create a new worker with the given ID.
    pub fn new(id: u32) -> Self {
        Self {
            id,
            processed: AtomicU64::new(0),
            embedders: Mutex::new(HashMap::new()),
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

    /// Get the number of cached embedders.
    pub fn cached_embedders(&self) -> usize {
        self.embedders_guard().len()
    }

    /// Process an embedding request.
    ///
    /// Supported model selectors:
    /// - `None`, `""`, `"hash"`, `"fnv1a-hash"` => default hash embedder
    /// - `"fnv1a-hash-<dimension>"` => hash embedder with explicit dimension
    /// - `"fastembed"`, `"fastembed-<model>"` => FastEmbed ONNX embedder (requires `semantic-search` feature)
    pub fn process(&self, request: &EmbedRequest) -> Result<EmbedResponse, String> {
        let started = Instant::now();
        let embedder = self.embedder_for(request.model.as_deref())?;
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

    fn embedder_for(&self, model: Option<&str>) -> Result<SharedEmbedder, String> {
        let selector = normalize_model_selector(model)?;
        {
            let cache = self.embedders_guard();
            if let Some(embedder) = cache.get(&selector) {
                return Ok(Arc::clone(embedder));
            }
        }

        let embedder = build_embedder_from_normalized_selector(&selector)?;
        let mut cache = self.embedders_guard();
        Ok(Arc::clone(
            cache
                .entry(selector)
                .or_insert_with(|| Arc::clone(&embedder)),
        ))
    }

    fn embedders_guard(&self) -> MutexGuard<'_, HashMap<String, SharedEmbedder>> {
        match self.embedders.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                EMBEDDER_CACHE_LOCK_POISONED.fetch_add(1, Ordering::Relaxed);
                self.embedders.clear_poison();
                poisoned.into_inner()
            }
        }
    }
}

/// br-ft-023t1: hard cap on the hash-embedder dimension that
/// daemon requests can ask for. The daemon allocates
/// `vec![0.0f32; dim]` per embedding (4 bytes per slot), so an
/// uncapped dimension turns the daemon into a 4-byte-per-unit
/// resource amplifier — a request frame of `model="fnv1a-hash-
/// 1000000000"` is well under the 4 MiB wire cap but asks the
/// worker for ~4 GB of f32 output. The cap matches the order of
/// magnitude of the highest dimensions used by supported FastEmbed
/// ONNX models (~1536 for OpenAI ada-002 sized output) with a
/// generous safety margin so legitimate operator-tuned hash
/// embedders remain accepted.
pub const MAX_HASH_EMBEDDER_DIMENSION: usize = 16_384;

fn normalize_model_selector(model: Option<&str>) -> Result<String, String> {
    match model.map(str::trim) {
        None | Some("" | "hash" | "fnv1a-hash") => Ok("fnv1a-hash-128".to_string()),
        Some(raw) if raw == "fastembed" || raw.starts_with("fastembed-") => Ok(raw.to_string()),
        Some(raw) if raw.starts_with("fnv1a-hash-") => Ok(raw.to_string()),
        Some(raw) => Err(format!("unsupported embed model: {raw}")),
    }
}

#[cfg(test)]
fn build_embedder(model: Option<&str>) -> Result<SharedEmbedder, String> {
    let selector = normalize_model_selector(model)?;
    build_embedder_from_normalized_selector(&selector)
}

fn build_embedder_from_normalized_selector(selector: &str) -> Result<SharedEmbedder, String> {
    match selector {
        "fnv1a-hash-128" => Ok(Arc::new(HashEmbedder::default())),
        raw => {
            if let Some(dim_raw) = raw.strip_prefix("fnv1a-hash-") {
                let dim = dim_raw
                    .parse::<usize>()
                    .map_err(|_| format!("invalid hash embedder dimension: {dim_raw}"))?;
                if dim == 0 {
                    return Err("hash embedder dimension must be > 0".to_string());
                }
                if dim > MAX_HASH_EMBEDDER_DIMENSION {
                    // br-ft-023t1: reject before HashEmbedder::new
                    // so no allocation happens for the rejected
                    // request.
                    return Err(format!(
                        "br-ft-023t1: hash embedder dimension {dim} exceeds cap {MAX_HASH_EMBEDDER_DIMENSION}"
                    ));
                }
                return Ok(Arc::new(HashEmbedder::new(dim)));
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
#[cfg(feature = "semantic-search")]
fn build_fastembed_embedder(selector: &str) -> Result<SharedEmbedder, String> {
    use crate::search::fastembed_embedder::{
        FastEmbedConfig, FastEmbedEmbedder, resolve_fastembed_model_selector,
    };

    let model = resolve_fastembed_model_selector(selector)?;

    let config = FastEmbedConfig::default().with_model(model);
    let emb = FastEmbedEmbedder::try_new(config).map_err(|e| e.to_string())?;
    Ok(Arc::new(emb))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::BTreeSet;
    use std::panic::{AssertUnwindSafe, catch_unwind};

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
    fn worker_recovers_poisoned_embedder_cache_lock() {
        let poison_count_before = embedder_cache_lock_poisoned_count();
        let worker = EmbedWorker::new(11);
        worker
            .process(&req(1, "seed", Some("fnv1a-hash-64")))
            .unwrap();

        let poison_result = catch_unwind(AssertUnwindSafe(|| {
            let _guard = worker.embedders.lock().unwrap();
            panic!("poison embedder cache lock");
        }));
        assert!(poison_result.is_err());

        assert_eq!(worker.cached_embedders(), 1);
        let poison_count_after_recovery = embedder_cache_lock_poisoned_count();
        assert!(poison_count_after_recovery > poison_count_before);

        let resp = worker
            .process(&req(2, "after poison", Some("fnv1a-hash-32")))
            .unwrap();
        assert_eq!(resp.vector.len(), 32);
        assert_eq!(worker.cached_embedders(), 2);
        assert_eq!(worker.processed(), 2);
        assert!(embedder_cache_lock_poisoned_count() >= poison_count_after_recovery);
    }

    #[test]
    fn worker_debug_recovers_poisoned_embedder_cache_lock() {
        let poison_count_before = embedder_cache_lock_poisoned_count();
        let worker = EmbedWorker::new(12);
        worker.process(&req(1, "seed", None)).unwrap();

        let poison_result = catch_unwind(AssertUnwindSafe(|| {
            let _guard = worker.embedders.lock().unwrap();
            panic!("poison embedder cache lock before debug");
        }));
        assert!(poison_result.is_err());

        let dbg = format!("{worker:?}");
        assert!(dbg.contains("EmbedWorker"));
        assert!(dbg.contains("cached_embedders: 1"));
        assert!(embedder_cache_lock_poisoned_count() > poison_count_before);
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
    fn build_embedder_rejects_retired_model2vec_selector() {
        let result = build_embedder(Some("model2vec"));
        assert!(result.is_err());
        assert!(result.err().unwrap().contains("unsupported embed model"));
    }

    #[test]
    fn build_embedder_whitespace_trimmed() {
        let emb = build_embedder(Some("  hash  ")).unwrap();
        assert_eq!(emb.info().name, "fnv1a-hash-128");
    }

    #[test]
    fn worker_reuses_default_hash_embedder_aliases_ft_1o34x() {
        let worker = EmbedWorker::new(1);
        for (idx, model) in [
            None,
            Some(""),
            Some("hash"),
            Some("fnv1a-hash"),
            Some(" fnv1a-hash "),
        ]
        .into_iter()
        .enumerate()
        {
            worker
                .process(&req(idx as u64, "cache alias", model))
                .unwrap();
        }

        assert_eq!(worker.cached_embedders(), 1);
        assert_eq!(worker.processed(), 5);
    }

    proptest! {
        #[test]
        fn proptest_worker_cache_cardinality_matches_unique_valid_selectors_ft_1o34x(
            dims in proptest::collection::vec(1_usize..=256, 1..40),
        ) {
            let worker = EmbedWorker::new(1);
            let mut expected = BTreeSet::new();

            for (idx, dim) in dims.iter().copied().enumerate() {
                let model = match idx % 4 {
                    0 => None,
                    1 => Some("hash".to_string()),
                    2 => Some(format!("fnv1a-hash-{dim}")),
                    _ => Some(format!(" fnv1a-hash-{dim} ")),
                };
                let model_ref = model.as_deref();
                let selector = normalize_model_selector(model_ref).unwrap();
                expected.insert(selector);
                worker
                    .process(&req(idx as u64, "cache property", model_ref))
                    .unwrap();
            }

            prop_assert_eq!(worker.cached_embedders(), expected.len());
            prop_assert_eq!(worker.processed(), dims.len() as u64);
        }
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

    // ── br-ft-023t1: dimension cap rejects resource-amplification ──

    #[test]
    fn dimension_at_cap_is_accepted_ft_023t1() {
        // The cap itself must not be off-by-one — a request asking
        // for exactly MAX_HASH_EMBEDDER_DIMENSION succeeds.
        let worker = EmbedWorker::new(1);
        let resp = worker
            .process(&req(
                1,
                "test",
                Some(&format!("fnv1a-hash-{MAX_HASH_EMBEDDER_DIMENSION}")),
            ))
            .unwrap();
        assert_eq!(resp.vector.len(), MAX_HASH_EMBEDDER_DIMENSION);
    }

    #[test]
    fn dimension_above_cap_is_rejected_ft_023t1() {
        // br-ft-023t1: a request asking for dim > cap must fail
        // BEFORE allocation (HashEmbedder::new vec![0.0; dim]).
        // The pre-fix "fnv1a-hash-1000000000" example would have
        // tried to allocate ~4 GB of f32; post-fix it errors out.
        let worker = EmbedWorker::new(1);
        let oversized = MAX_HASH_EMBEDDER_DIMENSION + 1;
        let err = worker
            .process(&req(1, "test", Some(&format!("fnv1a-hash-{oversized}"))))
            .unwrap_err();
        assert!(
            err.contains("br-ft-023t1"),
            "error must cite breadcrumb: {err}"
        );
        assert!(err.contains(&oversized.to_string()));
        assert!(err.contains(&MAX_HASH_EMBEDDER_DIMENSION.to_string()));
        // No allocation should be observable: processed counter
        // shouldn't increment on a failed embedder build.
        assert_eq!(worker.processed(), 0);
    }

    #[test]
    fn dimension_at_amplification_target_is_rejected_ft_023t1() {
        // The bead's specific worry: model="fnv1a-hash-1000000000"
        // (~4 GB allocation). Must fail closed.
        let worker = EmbedWorker::new(1);
        let err = worker
            .process(&req(1, "test", Some("fnv1a-hash-1000000000")))
            .unwrap_err();
        assert!(err.contains("br-ft-023t1"));
        assert_eq!(worker.processed(), 0);
    }
}
