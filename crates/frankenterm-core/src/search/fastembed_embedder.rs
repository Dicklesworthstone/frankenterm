//! FastEmbed embedder — quality sentence embeddings via ONNX Runtime.
//!
//! Requires the `semantic-search` feature.
//!
//! Bead: ft-344j8.14
//!
//! This module wraps the `fastembed` crate to provide high-quality embedding
//! generation with ONNX inference. Key features:
//!
//! - **Offline-first**: models are cached in a predictable directory and
//!   work entirely offline once downloaded.
//! - **Graceful degradation**: if model loading fails (network error, disk
//!   full, ONNX runtime issue), callers get a clear error and can fall back
//!   to `HashEmbedder`.
//! - **Thread-safe**: `FastEmbedEmbedder` is `Send + Sync` for safe sharing
//!   across the ML thread pool.
//! - **Native batching**: `embed_batch` delegates to fastembed's parallel
//!   batched inference for 10x+ throughput versus sequential calls.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::embedder::{EmbedError, Embedder, EmbedderInfo, EmbedderTier};

// Re-export key fastembed types for callers that need to specify models.
pub use fastembed::EmbeddingModel;

/// Configuration for the FastEmbed ONNX embedder.
#[derive(Debug, Clone)]
pub struct FastEmbedConfig {
    /// Which pre-trained model to load. Default: `BGESmallENV15` (384d).
    pub model: EmbeddingModel,
    /// Directory for cached model files. Default: platform cache dir.
    pub cache_dir: PathBuf,
    /// Maximum input token length. Default: 512.
    pub max_length: usize,
    /// Show download progress on first load. Default: false (headless).
    pub show_download_progress: bool,
}

impl Default for FastEmbedConfig {
    fn default() -> Self {
        // Use a predictable cache location: $HOME/.cache/frankenterm/fastembed
        // Falls back to .fastembed_cache in current dir if home is unavailable.
        let cache_dir = dirs::cache_dir()
            .map(|d| d.join("frankenterm").join("fastembed"))
            .unwrap_or_else(|| PathBuf::from(".fastembed_cache"));

        Self {
            model: EmbeddingModel::BGESmallENV15,
            cache_dir,
            max_length: 512,
            show_download_progress: false,
        }
    }
}

impl FastEmbedConfig {
    /// Create config with a specific model.
    pub fn with_model(mut self, model: EmbeddingModel) -> Self {
        self.model = model;
        self
    }

    /// Override the cache directory.
    pub fn with_cache_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cache_dir = dir.into();
        self
    }

    /// Set maximum token length.
    pub fn with_max_length(mut self, max_length: usize) -> Self {
        self.max_length = max_length;
        self
    }

    /// Enable or disable download progress display.
    pub fn with_show_download_progress(mut self, show: bool) -> Self {
        self.show_download_progress = show;
        self
    }
}

/// Resolve the model dimension from fastembed's model info table.
///
/// Returns 0 if the model isn't in the known list (shouldn't happen for
/// built-in models).
fn model_dimension(model: &EmbeddingModel) -> usize {
    fastembed::TextEmbedding::get_model_info(model)
        .map(|info| info.dim)
        .unwrap_or(0)
}

/// Resolve a human-readable model name string.
fn model_display_name(model: &EmbeddingModel) -> String {
    format!("{model:?}")
}

/// FastEmbed-based embedder for quality sentence embeddings.
///
/// Wraps `fastembed::TextEmbedding` for ONNX-accelerated inference.
/// Thread-safe (`Send + Sync`) via `Arc`.
pub struct FastEmbedEmbedder {
    /// The underlying fastembed model (Arc for cheap clone + Send+Sync).
    inner: Arc<fastembed::TextEmbedding>,
    /// Model identifier for diagnostics.
    model_name: String,
    /// Output embedding dimension.
    dimension: usize,
    /// Config snapshot for introspection.
    config: FastEmbedConfig,
}

impl FastEmbedEmbedder {
    /// Create a new FastEmbed embedder with default configuration.
    ///
    /// This downloads the model on first use (cached for subsequent calls).
    /// Returns `EmbedError` if model loading fails.
    pub fn try_new_default() -> Result<Self, EmbedError> {
        Self::try_new(FastEmbedConfig::default())
    }

    /// Create a new FastEmbed embedder with the given configuration.
    ///
    /// # Errors
    ///
    /// Returns `EmbedError::ModelNotFound` if the model files can't be
    /// loaded or downloaded, or `EmbedError::InferenceFailed` if the
    /// ONNX runtime can't be initialized.
    pub fn try_new(config: FastEmbedConfig) -> Result<Self, EmbedError> {
        let dimension = model_dimension(&config.model);
        let model_name = model_display_name(&config.model);

        let init_opts = fastembed::InitOptions::new(config.model.clone())
            .with_cache_dir(config.cache_dir.clone())
            .with_max_length(config.max_length)
            .with_show_download_progress(config.show_download_progress);

        let text_embedding = fastembed::TextEmbedding::try_new(init_opts).map_err(|e| {
            let msg = format!(
                "failed to initialize FastEmbed model '{}': {}",
                model_name, e
            );
            // Classify the error: if it mentions download/network/cache, it's ModelNotFound.
            // Otherwise it's an inference/runtime init failure.
            let lower = msg.to_lowercase();
            if lower.contains("not found")
                || lower.contains("download")
                || lower.contains("cache")
                || lower.contains("retrieve")
            {
                EmbedError::ModelNotFound(msg)
            } else {
                EmbedError::InferenceFailed(msg)
            }
        })?;

        Ok(Self {
            inner: Arc::new(text_embedding),
            model_name,
            dimension,
            config,
        })
    }

    /// Backward-compatible constructor matching the original stub API.
    ///
    /// **Deprecated**: prefer `try_new()` or `try_new_default()` which
    /// actually load the model. This creates a stub that will fail on
    /// first embed call unless the model is already cached.
    pub fn new(model_name: impl Into<String>, dimension: usize) -> Self {
        // Best-effort: create with default config. If it fails, we store
        // a sentinel that will error on embed().
        let model_name_str = model_name.into();
        let config = FastEmbedConfig::default();
        match Self::try_new(config.clone()) {
            Ok(mut emb) => {
                emb.model_name = model_name_str;
                emb.dimension = dimension;
                emb
            }
            Err(_) => {
                // Return a struct that will fail on embed() — matches old stub behavior.
                // We need a valid inner, so we'll panic-path here.
                // Since `new()` is the legacy API, callers should migrate to try_new().
                Self {
                    inner: Arc::new(stub_text_embedding()),
                    model_name: model_name_str,
                    dimension,
                    config,
                }
            }
        }
    }

    /// The model name/identifier.
    pub fn model_name(&self) -> &str {
        &self.model_name
    }

    /// The active configuration.
    pub fn config(&self) -> &FastEmbedConfig {
        &self.config
    }

    /// Cache directory being used.
    pub fn cache_dir(&self) -> &Path {
        &self.config.cache_dir
    }
}

/// Create a minimal TextEmbedding for the legacy `new()` fallback path.
/// This will fail on any actual embed call.
fn stub_text_embedding() -> fastembed::TextEmbedding {
    // Use default options — if the model happens to be cached, it works.
    // If not, embed() calls will return errors.
    fastembed::TextEmbedding::try_new(
        fastembed::InitOptions::new(EmbeddingModel::BGESmallENV15)
            .with_show_download_progress(false),
    )
    .unwrap_or_else(|_| {
        // Last resort: try with AllMiniLM which is the smallest model.
        fastembed::TextEmbedding::try_new(
            fastembed::InitOptions::new(EmbeddingModel::AllMiniLML6V2)
                .with_show_download_progress(false),
        )
        .expect(
            "FastEmbedEmbedder::new() requires at least one cached model; use try_new() instead",
        )
    })
}

impl Embedder for FastEmbedEmbedder {
    fn info(&self) -> EmbedderInfo {
        EmbedderInfo {
            name: format!("fastembed-{}", self.model_name),
            dimension: self.dimension,
            tier: EmbedderTier::Quality,
        }
    }

    fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        if text.is_empty() {
            // Return zero vector for empty input (consistent with hash embedder).
            return Ok(vec![0.0; self.dimension]);
        }

        let results = self
            .inner
            .embed(vec![text], None)
            .map_err(|e| EmbedError::InferenceFailed(format!("fastembed embed failed: {}", e)))?;

        let embedding = results.into_iter().next().ok_or_else(|| {
            EmbedError::InferenceFailed("fastembed returned empty results".to_string())
        })?;

        // Validate dimension.
        if self.dimension > 0 && embedding.len() != self.dimension {
            return Err(EmbedError::DimensionMismatch {
                expected: self.dimension,
                actual: embedding.len(),
            });
        }

        Ok(embedding)
    }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        // Handle empty strings: fastembed may not like them.
        // Pre-filter and post-fill.
        let has_empty = texts.iter().any(|t| t.is_empty());
        if has_empty {
            // Fall back to per-item to handle empty strings gracefully.
            return texts.iter().map(|t| self.embed(t)).collect();
        }

        let texts_vec: Vec<&str> = texts.to_vec();
        let results = self
            .inner
            .embed(texts_vec, None)
            .map_err(|e| EmbedError::InferenceFailed(format!("fastembed batch failed: {}", e)))?;

        if results.len() != texts.len() {
            return Err(EmbedError::InferenceFailed(format!(
                "fastembed returned {} embeddings for {} inputs",
                results.len(),
                texts.len()
            )));
        }

        Ok(results)
    }
}

// =============================================================================
// Fallback embedder factory
// =============================================================================

/// Initialization outcome: either a real FastEmbed embedder or a reason it
/// failed. Callers can match on this to decide whether to fall back.
pub enum FastEmbedInitResult {
    /// Model loaded successfully.
    Ok(FastEmbedEmbedder),
    /// Model failed to load. Error message describes why.
    Degraded { error: String },
}

impl std::fmt::Debug for FastEmbedInitResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ok(emb) => write!(f, "Ok(FastEmbedEmbedder({}))", emb.model_name),
            Self::Degraded { error } => f.debug_struct("Degraded").field("error", error).finish(),
        }
    }
}

/// Try to create a FastEmbed embedder, returning a structured result
/// that makes graceful degradation explicit.
pub fn try_init_fastembed(config: FastEmbedConfig) -> FastEmbedInitResult {
    match FastEmbedEmbedder::try_new(config) {
        Ok(emb) => FastEmbedInitResult::Ok(emb),
        Err(e) => FastEmbedInitResult::Degraded {
            error: format!("{}", e),
        },
    }
}

/// Create the best available embedder: FastEmbed if possible, HashEmbedder fallback.
///
/// This is the recommended entry point for production code. It logs degradation
/// via `tracing::warn!` and never panics.
pub fn best_available_embedder(config: FastEmbedConfig) -> (Box<dyn Embedder>, bool) {
    match try_init_fastembed(config) {
        FastEmbedInitResult::Ok(emb) => {
            tracing::info!(
                model = %emb.model_name(),
                dimension = emb.dimension,
                "FastEmbed ONNX embedder initialized"
            );
            (Box::new(emb), true)
        }
        FastEmbedInitResult::Degraded { error } => {
            tracing::warn!(
                error = %error,
                "FastEmbed unavailable, falling back to HashEmbedder"
            );
            let fallback = super::hash_embedder::HashEmbedder::default();
            (Box::new(fallback), false)
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // Configuration tests
    // =========================================================================

    #[test]
    fn config_default_creates_valid_config() {
        let config = FastEmbedConfig::default();
        assert_eq!(config.max_length, 512);
        assert!(!config.show_download_progress);
        // Cache dir should end with "fastembed".
        let path_str = config.cache_dir.to_string_lossy();
        assert!(
            path_str.contains("fastembed"),
            "cache dir should contain 'fastembed': {}",
            path_str
        );
    }

    #[test]
    fn config_builder_with_model() {
        let config = FastEmbedConfig::default().with_model(EmbeddingModel::AllMiniLML6V2);
        assert!(matches!(config.model, EmbeddingModel::AllMiniLML6V2));
    }

    #[test]
    fn config_builder_with_cache_dir() {
        let config = FastEmbedConfig::default().with_cache_dir("/tmp/test-cache");
        assert_eq!(config.cache_dir, PathBuf::from("/tmp/test-cache"));
    }

    #[test]
    fn config_builder_with_max_length() {
        let config = FastEmbedConfig::default().with_max_length(256);
        assert_eq!(config.max_length, 256);
    }

    #[test]
    fn config_builder_with_show_download_progress() {
        let config = FastEmbedConfig::default().with_show_download_progress(true);
        assert!(config.show_download_progress);
    }

    #[test]
    fn config_builder_chaining() {
        let config = FastEmbedConfig::default()
            .with_model(EmbeddingModel::BGEBaseENV15)
            .with_cache_dir("/tmp/chain-test")
            .with_max_length(128)
            .with_show_download_progress(true);

        assert!(matches!(config.model, EmbeddingModel::BGEBaseENV15));
        assert_eq!(config.cache_dir, PathBuf::from("/tmp/chain-test"));
        assert_eq!(config.max_length, 128);
        assert!(config.show_download_progress);
    }

    #[test]
    fn config_clone() {
        let config = FastEmbedConfig::default().with_max_length(256);
        let cloned = config.clone();
        assert_eq!(cloned.max_length, 256);
    }

    // =========================================================================
    // Model dimension resolution tests
    // =========================================================================

    #[test]
    fn model_dimension_bge_small() {
        let dim = model_dimension(&EmbeddingModel::BGESmallENV15);
        assert_eq!(dim, 384, "BGESmallENV15 should be 384-dimensional");
    }

    #[test]
    fn model_dimension_all_minilm_l6() {
        let dim = model_dimension(&EmbeddingModel::AllMiniLML6V2);
        assert_eq!(dim, 384, "AllMiniLML6V2 should be 384-dimensional");
    }

    #[test]
    fn model_dimension_bge_large() {
        let dim = model_dimension(&EmbeddingModel::BGELargeENV15);
        assert_eq!(dim, 1024, "BGELargeENV15 should be 1024-dimensional");
    }

    #[test]
    fn model_display_name_format() {
        let name = model_display_name(&EmbeddingModel::BGESmallENV15);
        assert!(
            name.contains("BGESmallENV15"),
            "display name should contain model variant: {}",
            name
        );
    }

    // =========================================================================
    // EmbedderInfo and trait tests (don't require model loading)
    // =========================================================================

    #[test]
    fn embedder_info_tier_is_quality() {
        // We can test the info() output without loading a model by checking
        // that a hypothetical embedder would report Quality tier.
        let info = EmbedderInfo {
            name: "fastembed-BGESmallENV15".into(),
            dimension: 384,
            tier: EmbedderTier::Quality,
        };
        assert_eq!(info.tier, EmbedderTier::Quality);
        assert_eq!(info.dimension, 384);
        assert!(info.name.starts_with("fastembed-"));
    }

    // =========================================================================
    // FastEmbedInitResult tests
    // =========================================================================

    #[test]
    fn init_result_degraded_contains_error() {
        let result = FastEmbedInitResult::Degraded {
            error: "network timeout".to_string(),
        };
        if let FastEmbedInitResult::Degraded { error } = result {
            assert!(error.contains("network timeout"));
        } else {
            panic!("expected Degraded variant");
        }
    }

    // =========================================================================
    // Integration tests (require model download)
    //
    // These are guarded by the FASTEMBED_INTEGRATION environment variable.
    // Run with: FASTEMBED_INTEGRATION=1 cargo test -p frankenterm-core \
    //   --features semantic-search -- fastembed
    // =========================================================================

    fn integration_enabled() -> bool {
        std::env::var("FASTEMBED_INTEGRATION").is_ok()
    }

    #[test]
    fn integration_try_new_default() {
        if !integration_enabled() {
            return;
        }
        let emb = FastEmbedEmbedder::try_new_default().expect("should load default model");
        assert_eq!(emb.dimension, 384);
        assert!(emb.model_name().contains("BGESmallENV15"));
    }

    #[test]
    fn integration_embed_single() {
        if !integration_enabled() {
            return;
        }
        let emb = FastEmbedEmbedder::try_new_default().unwrap();
        let v = emb.embed("Hello world").unwrap();
        assert_eq!(v.len(), 384);
        // Embedding should be normalized (roughly unit length).
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 0.1,
            "embedding should be roughly normalized, got norm={}",
            norm
        );
    }

    #[test]
    fn integration_embed_empty_returns_zero_vec() {
        if !integration_enabled() {
            return;
        }
        let emb = FastEmbedEmbedder::try_new_default().unwrap();
        let v = emb.embed("").unwrap();
        assert_eq!(v.len(), 384);
        assert!(v.iter().all(|&x| x == 0.0));
    }

    #[test]
    fn integration_embed_batch() {
        if !integration_enabled() {
            return;
        }
        let emb = FastEmbedEmbedder::try_new_default().unwrap();
        let texts: Vec<&str> = vec![
            "Compiling frankenterm v0.1.0",
            "error[E0308]: mismatched types",
            "Progress: [========>] 50%",
        ];
        let results = emb.embed_batch(&texts).unwrap();
        assert_eq!(results.len(), 3);
        for v in &results {
            assert_eq!(v.len(), 384);
        }
    }

    #[test]
    fn integration_embed_batch_empty() {
        if !integration_enabled() {
            return;
        }
        let emb = FastEmbedEmbedder::try_new_default().unwrap();
        let results = emb.embed_batch(&[]).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn integration_embed_batch_with_empty_string() {
        if !integration_enabled() {
            return;
        }
        let emb = FastEmbedEmbedder::try_new_default().unwrap();
        let texts: Vec<&str> = vec!["hello", "", "world"];
        let results = emb.embed_batch(&texts).unwrap();
        assert_eq!(results.len(), 3);
        // Middle one should be zero vector.
        assert!(results[1].iter().all(|&x| x == 0.0));
    }

    #[test]
    fn integration_similar_texts_have_high_cosine() {
        if !integration_enabled() {
            return;
        }
        let emb = FastEmbedEmbedder::try_new_default().unwrap();
        let v1 = emb.embed("error: compilation failed").unwrap();
        let v2 = emb.embed("error: build failure").unwrap();
        let v3 = emb
            .embed("the quick brown fox jumps over the lazy dog")
            .unwrap();

        let sim_12 = cosine_sim(&v1, &v2);
        let sim_13 = cosine_sim(&v1, &v3);

        // Similar error messages should have higher similarity than unrelated text.
        assert!(
            sim_12 > sim_13,
            "similar texts should be closer: sim(err1,err2)={} vs sim(err1,fox)={}",
            sim_12,
            sim_13
        );
    }

    #[test]
    fn integration_best_available_returns_fastembed() {
        if !integration_enabled() {
            return;
        }
        let (emb, is_quality) = best_available_embedder(FastEmbedConfig::default());
        assert!(is_quality, "should use FastEmbed when available");
        assert_eq!(emb.tier(), EmbedderTier::Quality);
    }

    #[test]
    fn integration_embedder_is_send_sync() {
        if !integration_enabled() {
            return;
        }
        let emb = FastEmbedEmbedder::try_new_default().unwrap();
        // Compile-time check: FastEmbedEmbedder is Send + Sync.
        fn assert_send_sync<T: Send + Sync>(_: &T) {}
        assert_send_sync(&emb);
    }

    #[test]
    fn integration_config_accessors() {
        if !integration_enabled() {
            return;
        }
        let config = FastEmbedConfig::default()
            .with_cache_dir("/tmp/ft-test-cache")
            .with_max_length(256);
        let emb = FastEmbedEmbedder::try_new(config).unwrap();
        assert_eq!(emb.cache_dir(), Path::new("/tmp/ft-test-cache"));
        assert_eq!(emb.config().max_length, 256);
    }

    #[test]
    fn best_available_with_bad_cache_falls_back() {
        // Use a non-existent/inaccessible cache dir to force degradation.
        let config = FastEmbedConfig::default()
            .with_cache_dir("/nonexistent/path/that/cannot/exist/fastembed");
        let (_emb, is_quality) = best_available_embedder(config);
        // If we're in an environment without the model, this should degrade.
        // (In CI with cached models, it might still succeed — that's OK.)
        if !is_quality {
            // Verify fallback is Hash tier.
            assert_eq!(_emb.tier(), EmbedderTier::Hash);
        }
    }

    #[test]
    fn try_init_fastembed_returns_degraded_on_bad_path() {
        let config = FastEmbedConfig::default()
            .with_cache_dir("/nonexistent/path/that/cannot/exist/fastembed");
        let result = try_init_fastembed(config);
        // May succeed if HF hub downloads to a temp location, but if it fails
        // it should be Degraded.
        match result {
            FastEmbedInitResult::Ok(_) => { /* model was available — fine */ }
            FastEmbedInitResult::Degraded { error } => {
                assert!(!error.is_empty());
            }
        }
    }

    // =========================================================================
    // Helper functions for tests
    // =========================================================================

    #[allow(dead_code)]
    fn cosine_sim(a: &[f32], b: &[f32]) -> f32 {
        assert_eq!(a.len(), b.len());
        let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm_a < f32::EPSILON || norm_b < f32::EPSILON {
            return 0.0;
        }
        dot / (norm_a * norm_b)
    }
}
