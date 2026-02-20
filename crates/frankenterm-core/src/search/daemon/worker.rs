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

fn build_embedder(model: Option<&str>) -> Result<HashEmbedder, String> {
    match model.map(str::trim) {
        None | Some("") | Some("hash") | Some("fnv1a-hash") => Ok(HashEmbedder::default()),
        Some(raw) => {
            if let Some(dim_raw) = raw.strip_prefix("fnv1a-hash-") {
                let dim = dim_raw
                    .parse::<usize>()
                    .map_err(|_| format!("invalid hash embedder dimension: {dim_raw}"))?;
                if dim == 0 {
                    return Err("hash embedder dimension must be > 0".to_string());
                }
                return Ok(HashEmbedder::new(dim));
            }
            Err(format!("unsupported embed model: {raw}"))
        }
    }
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
            .process(&req(3, "ignored", Some("fastembed-e5-large")))
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
}
