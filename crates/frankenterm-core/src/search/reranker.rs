//! Reranker trait, implementations, and frankensearch-rerank adapter (B6).
//!
//! Provides:
//! - `Reranker` trait: synchronous reranking interface for FrankenTerm
//! - `PassthroughReranker`: no-op fallback (explicitly labeled)
//! - `CrossEncoderReranker`: ONNX cross-encoder scoring (semantic-search feature).
//!   Requires explicit `load()` before inference; returns `ModelNotLoaded` error
//!   if called before the model is ready.
//! - `RerankConfig` + `RerankBackend`: configuration for the reranking pipeline step
//! - `RerankOutcome`: metrics/diagnostics from a rerank invocation
//! - `FrankenSearchRerankAdapter`: type conversion bridge between FT `ScoredDoc` and
//!   frankensearch `RerankDocument`/`RerankScore` (frankensearch feature)
//! - `rerank_fused_results`: post-fusion reranking step for the orchestrator pipeline

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

use super::FusedResult;

// ── Error type ──────────────────────────────────────────────────────────

/// Errors that can occur when reranking search results against a query.
#[derive(Debug)]
pub enum RerankError {
    ModelError(String),
    EmptyInput,
    /// The reranker was constructed but no model is loaded — scoring cannot proceed.
    ModelNotLoaded(String),
}

impl fmt::Display for RerankError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ModelError(e) => write!(f, "rerank model error: {e}"),
            Self::EmptyInput => write!(f, "empty input to reranker"),
            Self::ModelNotLoaded(path) => {
                write!(f, "reranker model not loaded (path: {path})")
            }
        }
    }
}

impl std::error::Error for RerankError {}

// ── Core types ──────────────────────────────────────────────────────────

/// Scored document for reranking.
#[derive(Debug, Clone)]
pub struct ScoredDoc {
    pub id: u64,
    pub text: String,
    pub score: f32,
}

/// Trait for reranking a set of candidate documents against a query.
pub trait Reranker: Send + Sync {
    fn rerank(&self, query: &str, docs: Vec<ScoredDoc>) -> Result<Vec<ScoredDoc>, RerankError>;
}

/// Passthrough reranker that returns documents unchanged (for testing/fallback).
#[allow(dead_code)]
pub struct PassthroughReranker;

impl Reranker for PassthroughReranker {
    fn rerank(&self, _query: &str, docs: Vec<ScoredDoc>) -> Result<Vec<ScoredDoc>, RerankError> {
        if docs.is_empty() {
            return Err(RerankError::EmptyInput);
        }
        Ok(docs)
    }
}

/// Hard bounds for the optional local inference stage.
pub const SEARCH_RERANK_MAX_CANDIDATES: usize = 100;
pub const SEARCH_RERANK_MAX_TEXT_BYTES: usize = 16 * 1024;
#[cfg(any(feature = "semantic-search", test))]
const SEARCH_RERANK_MAX_TOKENS: usize = 512;
#[cfg(feature = "semantic-search")]
const SEARCH_RERANK_BATCH_SIZE: usize = 8;

/// Cross-encoder backed by a locally supplied, self-contained ONNX model.
///
/// `model_path` names the ONNX file. Its directory must also contain
/// tokenizer.json, config.json, special_tokens_map.json, and
/// tokenizer_config.json. Loading never uses a model hub or downloads files.
/// A successful load includes a real inference probe; file existence alone
/// never makes the model ready.
#[cfg(feature = "semantic-search")]
pub struct CrossEncoderReranker {
    model_path: String,
    model: std::sync::Mutex<Option<fastembed::TextRerank>>,
}

#[cfg(feature = "semantic-search")]
impl CrossEncoderReranker {
    pub fn new(model_path: impl Into<String>) -> Self {
        Self {
            model_path: model_path.into(),
            model: std::sync::Mutex::new(None),
        }
    }

    pub fn model_path(&self) -> &str {
        &self.model_path
    }

    pub fn is_loaded(&self) -> bool {
        self.model.lock().is_ok_and(|model| model.is_some())
    }

    pub fn load(&mut self) -> Result<(), RerankError> {
        // Failed reloads must not keep serving a previously loaded model.
        self.model = std::sync::Mutex::new(None);
        let path = std::path::Path::new(&self.model_path);
        let directory = path.parent().ok_or_else(|| {
            RerankError::ModelError("local reranker model has no parent directory".into())
        })?;
        let model_bytes = read_model_file(path, 512 * 1024 * 1024, "model.onnx")?;
        let tokenizer_files = fastembed::TokenizerFiles {
            tokenizer_file: read_model_file(
                &directory.join("tokenizer.json"),
                8 * 1024 * 1024,
                "tokenizer.json",
            )?,
            config_file: read_model_file(
                &directory.join("config.json"),
                8 * 1024 * 1024,
                "config.json",
            )?,
            special_tokens_map_file: read_model_file(
                &directory.join("special_tokens_map.json"),
                8 * 1024 * 1024,
                "special_tokens_map.json",
            )?,
            tokenizer_config_file: read_model_file(
                &directory.join("tokenizer_config.json"),
                8 * 1024 * 1024,
                "tokenizer_config.json",
            )?,
        };
        // Use the offline user-defined constructor exclusively, with the
        // explicitly supplied self-contained ONNX bytes.
        let loaded = frankenterm_sigpipe::catch_recoverable(
            frankenterm_sigpipe::RecoverablePanicSite::CoreSearchBridge,
            std::panic::AssertUnwindSafe(|| {
                let mut model = fastembed::TextRerank::try_new_from_user_defined(
                    fastembed::UserDefinedRerankingModel::new(model_bytes, tokenizer_files),
                    fastembed::RerankInitOptionsUserDefined::new()
                        .with_max_length(SEARCH_RERANK_MAX_TOKENS)
                        .with_intra_threads(2),
                )
                .map_err(|_| {
                    RerankError::ModelError(
                        "local ONNX/tokenizer initialization failed; verify the reranker bundle"
                            .into(),
                    )
                })?;
                // fastembed expects one finite f32 logits value per query/document pair.
                // Exercise that contract before advertising readiness.
                let probe = model
                    .rerank("readiness probe", ["readiness probe"], false, Some(1))
                    .map_err(|_| {
                        RerankError::ModelError("local reranker readiness inference failed".into())
                    })?;
                validate_model_scores(1, probe.iter().map(|r| (r.index, r.score)))?;
                Ok::<_, RerankError>(model)
            }),
        )
        .map_err(|_| {
            RerankError::ModelError(
                "local reranker initialization panicked; verify ONNX runtime and model outputs"
                    .into(),
            )
        })??;
        self.model = std::sync::Mutex::new(Some(loaded));
        Ok(())
    }
}

#[cfg(feature = "semantic-search")]
fn read_model_file(
    path: &std::path::Path,
    max_bytes: u64,
    name: &str,
) -> Result<Vec<u8>, RerankError> {
    use std::io::Read;

    let file = std::fs::File::open(path).map_err(|_| {
        RerankError::ModelError(format!("local reranker {name} is missing or unreadable"))
    })?;
    let metadata = file
        .metadata()
        .map_err(|_| RerankError::ModelError(format!("cannot inspect local reranker {name}")))?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > max_bytes {
        return Err(RerankError::ModelError(format!(
            "local reranker {name} must be a nonempty regular file of at most {max_bytes} bytes"
        )));
    }
    let mut bytes = Vec::new();
    file.take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| RerankError::ModelError(format!("cannot read local reranker {name}")))?;
    if bytes.is_empty() || bytes.len() as u64 > max_bytes {
        return Err(RerankError::ModelError(format!(
            "local reranker {name} changed outside its size bound while reading"
        )));
    }
    Ok(bytes)
}

#[cfg(any(feature = "semantic-search", test))]
fn validate_model_scores(
    count: usize,
    scores: impl IntoIterator<Item = (usize, f32)>,
) -> Result<Vec<f32>, RerankError> {
    let mut by_index = vec![None; count];
    for (index, score) in scores {
        let slot = by_index.get_mut(index).ok_or_else(|| {
            RerankError::ModelError("reranker returned an unknown candidate index".into())
        })?;
        if slot.is_some() || !score.is_finite() {
            return Err(RerankError::ModelError(
                "reranker returned duplicate indices or non-finite scores".into(),
            ));
        }
        *slot = Some(score);
    }
    by_index
        .into_iter()
        .map(|score| {
            score
                .ok_or_else(|| RerankError::ModelError("reranker omitted a candidate score".into()))
        })
        .collect()
}

#[cfg(any(feature = "semantic-search", test))]
fn bounded_document(text: &str) -> &str {
    let mut end = text.len().min(SEARCH_RERANK_MAX_TEXT_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(feature = "semantic-search")]
impl Reranker for CrossEncoderReranker {
    fn rerank(&self, query: &str, mut docs: Vec<ScoredDoc>) -> Result<Vec<ScoredDoc>, RerankError> {
        if docs.is_empty() {
            return Err(RerankError::EmptyInput);
        }
        if query.len() > SEARCH_RERANK_MAX_TEXT_BYTES || docs.len() > SEARCH_RERANK_MAX_CANDIDATES {
            return Err(RerankError::ModelError(
                "reranker input exceeds its query/candidate bounds".into(),
            ));
        }
        let mut guard = self
            .model
            .try_lock()
            .map_err(|_| RerankError::ModelError("local reranker is busy or unavailable".into()))?;
        let model = guard
            .as_mut()
            .ok_or_else(|| RerankError::ModelNotLoaded(self.model_path.clone()))?;
        let inference = frankenterm_sigpipe::catch_recoverable(
            frankenterm_sigpipe::RecoverablePanicSite::CoreSearchBridge,
            std::panic::AssertUnwindSafe(|| {
                for batch in docs.chunks_mut(SEARCH_RERANK_BATCH_SIZE) {
                    if let Some(cx) = crate::cx::Cx::current() {
                        cx.checkpoint().map_err(|_| {
                            RerankError::ModelError("search reranking cancelled".into())
                        })?;
                    }
                    let texts: Vec<&str> = batch
                        .iter()
                        .map(|doc| bounded_document(&doc.text))
                        .collect();
                    let output = model
                        .rerank(query, &texts, false, Some(SEARCH_RERANK_BATCH_SIZE))
                        .map_err(|_| {
                            RerankError::ModelError("local reranker inference failed".into())
                        })?;
                    let scores = validate_model_scores(
                        batch.len(),
                        output.into_iter().map(|r| (r.index, r.score)),
                    )?;
                    for (doc, score) in batch.iter_mut().zip(scores) {
                        doc.score = score;
                    }
                }
                Ok::<_, RerankError>(())
            }),
        )
        .map_err(|_| {
            RerankError::ModelError(
                "local reranker inference panicked; verify model output compatibility".into(),
            )
        })
        .and_then(|result| result);
        if let Err(error) = inference {
            *guard = None;
            return Err(error);
        }
        // Stable ties retain original retrieval order.
        docs.sort_by(|left, right| right.score.total_cmp(&left.score));
        Ok(docs)
    }
}

/// Inference score, kept separate from the unchanged retrieval/fusion score.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchRerankScore {
    pub segment_id: i64,
    pub original_rank: usize,
    pub score: f32,
}

/// Optional response metadata. Contains no query, document body, or model path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchRerankReport {
    pub backend: String,
    pub reranked: bool,
    pub candidates_reranked: usize,
    pub truncated_documents: usize,
    pub candidate_limit: usize,
    pub document_byte_limit: usize,
    pub max_sequence_tokens: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<String>,
    pub scores: Vec<SearchRerankScore>,
}

#[cfg(any(feature = "semantic-search", test))]
fn empty_search_rerank_report() -> SearchRerankReport {
    SearchRerankReport {
        backend: "local_cross_encoder".into(),
        reranked: false,
        candidates_reranked: 0,
        truncated_documents: 0,
        candidate_limit: SEARCH_RERANK_MAX_CANDIDATES,
        document_byte_limit: SEARCH_RERANK_MAX_TEXT_BYTES,
        max_sequence_tokens: SEARCH_RERANK_MAX_TOKENS,
        skip_reason: Some("no search results".into()),
        scores: Vec::new(),
    }
}

/// Rerank only the bounded candidate head, preserving every original row and
/// retrieval score. Ordinal identities prevent duplicate segment IDs from
/// aliasing each other; model-returned text and indices are checked before
/// any original row is moved.
#[cfg(any(feature = "semantic-search", test))]
fn rerank_search_rows<T>(
    query: &str,
    rows: Vec<T>,
    document: for<'a> fn(&'a T) -> (i64, &'a str),
    reranker: &dyn Reranker,
) -> Result<(Vec<T>, SearchRerankReport), RerankError> {
    if rows.is_empty() {
        return Ok((rows, empty_search_rerank_report()));
    }
    if query.len() > SEARCH_RERANK_MAX_TEXT_BYTES {
        return Err(RerankError::ModelError(
            "reranker query exceeds 16384 bytes".into(),
        ));
    }
    let count = rows.len().min(SEARCH_RERANK_MAX_CANDIDATES);
    let mut truncated_documents = 0;
    let documents: Vec<ScoredDoc> = rows
        .iter()
        .take(count)
        .enumerate()
        .map(|(index, row)| {
            let (_, text) = document(row);
            let bounded = bounded_document(text);
            truncated_documents += usize::from(bounded.len() != text.len());
            ScoredDoc {
                id: index as u64,
                text: bounded.to_string(),
                score: 0.0,
            }
        })
        .collect();
    let model_output = reranker.rerank(query, documents.clone())?;
    let scores = validate_model_scores(
        count,
        model_output
            .iter()
            .map(|doc| (usize::try_from(doc.id).unwrap_or(usize::MAX), doc.score)),
    )?;
    for doc in &model_output {
        let index = usize::try_from(doc.id).map_err(|_| {
            RerankError::ModelError("reranker returned an unknown candidate identity".into())
        })?;
        if doc.text != documents[index].text {
            return Err(RerankError::ModelError(
                "reranker changed a candidate's text".into(),
            ));
        }
    }
    // Rank from validated scores, rather than trusting a backend's return order.
    let mut order: Vec<usize> = (0..count).collect();
    order.sort_by(|&left, &right| scores[right].total_cmp(&scores[left]));
    let mut report = empty_search_rerank_report();
    report.reranked = true;
    report.candidates_reranked = count;
    report.truncated_documents = truncated_documents;
    report.skip_reason = None;
    report.scores = order
        .iter()
        .map(|&index| SearchRerankScore {
            segment_id: document(&rows[index]).0,
            original_rank: index + 1,
            score: scores[index],
        })
        .collect();
    let mut original: Vec<Option<T>> = rows.into_iter().map(Some).collect();
    let mut output = Vec::with_capacity(original.len());
    for index in order {
        output.push(
            original[index]
                .take()
                .expect("validated candidate permutation"),
        );
    }
    output.extend(original.into_iter().skip(count).flatten());
    Ok((output, report))
}

#[cfg(feature = "semantic-search")]
struct RerankAdmission;

#[cfg(feature = "semantic-search")]
static RERANK_IN_FLIGHT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(feature = "semantic-search")]
impl RerankAdmission {
    fn acquire() -> Result<Self, RerankError> {
        RERANK_IN_FLIGHT
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .map_err(|_| {
                RerankError::ModelError("local reranker is busy; retry the search".into())
            })?;
        Ok(Self)
    }
}

#[cfg(feature = "semantic-search")]
impl Drop for RerankAdmission {
    fn drop(&mut self) {
        RERANK_IN_FLIGHT.store(false, std::sync::atomic::Ordering::Release);
    }
}

#[cfg(feature = "semantic-search")]
#[derive(PartialEq, Eq)]
struct ModelFingerprint {
    path: std::path::PathBuf,
    files: Vec<(u64, std::time::SystemTime)>,
}

#[cfg(feature = "semantic-search")]
fn reranker_model_fingerprint(models_dir: &str) -> Result<ModelFingerprint, RerankError> {
    let directory = if models_dir == "~" {
        dirs::home_dir()
    } else if let Some(relative) = models_dir.strip_prefix("~/") {
        dirs::home_dir().map(|home| home.join(relative))
    } else if models_dir.trim().is_empty() {
        None
    } else {
        Some(std::path::PathBuf::from(models_dir))
    }
    .ok_or_else(|| RerankError::ModelError("cannot resolve search.models_dir".into()))?;
    let path = directory
        .join("reranker")
        .join("model.onnx")
        .canonicalize()
        .map_err(|_| {
            RerankError::ModelError("local reranker model.onnx is missing or unreadable".into())
        })?;
    let parent = path
        .parent()
        .ok_or_else(|| RerankError::ModelError("local reranker bundle has no directory".into()))?;
    let mut files = Vec::new();
    for name in [
        "model.onnx",
        "tokenizer.json",
        "config.json",
        "special_tokens_map.json",
        "tokenizer_config.json",
    ] {
        let metadata = std::fs::metadata(parent.join(name)).map_err(|_| {
            RerankError::ModelError(format!("local reranker {name} is missing or unreadable"))
        })?;
        if !metadata.is_file() {
            return Err(RerankError::ModelError(format!(
                "local reranker {name} is not a regular file"
            )));
        }
        let modified = metadata.modified().map_err(|_| {
            RerankError::ModelError(format!(
                "cannot inspect local reranker {name} modification time"
            ))
        })?;
        files.push((metadata.len(), modified));
    }
    Ok(ModelFingerprint { path, files })
}

/// The live CLI, robot, and MCP search stage. Disabled searches do not inspect
/// model files or schedule blocking work. Enabled failures are explicit; no
/// passthrough result is mislabeled as cross-encoder inference.
///
/// Only one bounded inference stage is admitted per process. Cancellation
/// stops publishing its result and is checked between batches; an in-progress
/// native ONNX call finishes before releasing the admission slot.
pub async fn rerank_search_results_with_cx<T>(
    cx: &crate::cx::Cx,
    config: &crate::config::SearchConfig,
    query: &str,
    rows: Vec<T>,
    document: for<'a> fn(&'a T) -> (i64, &'a str),
) -> Result<(Vec<T>, Option<SearchRerankReport>), RerankError>
where
    T: Send + 'static,
{
    if !config.reranker_enabled {
        return Ok((rows, None));
    }
    #[cfg(not(feature = "semantic-search"))]
    {
        let _ = (cx, query, document);
        Err(RerankError::ModelError(
            "search.reranker_enabled requires the semantic-search build feature".into(),
        ))
    }
    #[cfg(feature = "semantic-search")]
    {
        cx.checkpoint()
            .map_err(|_| RerankError::ModelError("search reranking cancelled".into()))?;
        if rows.is_empty() {
            return Ok((rows, Some(empty_search_rerank_report())));
        }
        if query.len() > SEARCH_RERANK_MAX_TEXT_BYTES {
            return Err(RerankError::ModelError(
                "reranker query exceeds 16384 bytes".into(),
            ));
        }
        let admission = RerankAdmission::acquire()?;
        let models_dir = config.models_dir.clone();
        let query = query.to_string();
        let worker_cx = cx.clone();
        let result = crate::runtime_async::spawn_blocking_with_cx(cx, move || {
            let _admission = admission;
            worker_cx
                .checkpoint()
                .map_err(|_| RerankError::ModelError("search reranking cancelled".into()))?;
            static MODEL: std::sync::OnceLock<
                std::sync::Mutex<Option<(ModelFingerprint, CrossEncoderReranker)>>,
            > = std::sync::OnceLock::new();
            let mut cache = MODEL
                .get_or_init(|| std::sync::Mutex::new(None))
                .lock()
                .map_err(|_| {
                    RerankError::ModelError("local reranker cache is unavailable".into())
                })?;
            let fingerprint = reranker_model_fingerprint(&models_dir)?;
            if cache
                .as_ref()
                .is_none_or(|(key, model)| key != &fingerprint || !model.is_loaded())
            {
                // Drop the previous model before loading a different workspace's
                // model, keeping resident native model ownership bounded to one.
                *cache = None;
                let path = fingerprint.path.to_str().ok_or_else(|| {
                    RerankError::ModelError("local reranker model path is not UTF-8".into())
                })?;
                let mut model = CrossEncoderReranker::new(path);
                model.load()?;
                if reranker_model_fingerprint(&models_dir)? != fingerprint {
                    return Err(RerankError::ModelError(
                        "local reranker bundle changed during loading; retry".into(),
                    ));
                }
                *cache = Some((fingerprint, model));
            }
            worker_cx
                .checkpoint()
                .map_err(|_| RerankError::ModelError("search reranking cancelled".into()))?;
            let (_, model) = cache.as_ref().expect("model loaded successfully");
            rerank_search_rows(&query, rows, document, model)
        })
        .await
        .map_err(|error| {
            use crate::runtime_async::SpawnBlockingWithCxError;
            let message = match error {
                SpawnBlockingWithCxError::CancelledBeforeSpawn { .. }
                | SpawnBlockingWithCxError::CancelledMidFlight { .. } => {
                    "search reranking cancelled"
                }
                _ => "search reranking blocking runtime failed",
            };
            RerankError::ModelError(message.into())
        })??;
        cx.checkpoint()
            .map_err(|_| RerankError::ModelError("search reranking cancelled".into()))?;
        Ok((result.0, Some(result.1)))
    }
}

// ── B6: Rerank configuration ────────────────────────────────────────────

/// Backend selector for the reranking step.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RerankBackend {
    /// No-op: return candidates unchanged (default).
    #[default]
    Passthrough,
    /// Delegate to frankensearch-rerank adapter (cross-encoder scoring).
    FrankenSearch,
}

impl RerankBackend {
    /// Parse from string (case-insensitive).
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_lowercase().as_str() {
            "frankensearch" | "flashrank" | "cross_encoder" | "crossencoder" => Self::FrankenSearch,
            _ => Self::Passthrough,
        }
    }

    /// Canonical string representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passthrough => "passthrough",
            Self::FrankenSearch => "frankensearch",
        }
    }
}

impl fmt::Display for RerankBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Default maximum number of candidates to rerank per query.
pub const DEFAULT_TOP_K_RERANK: usize = 100;

/// Default minimum number of candidates required to trigger reranking.
pub const DEFAULT_MIN_CANDIDATES: usize = 5;

/// Configuration for the reranking pipeline step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RerankConfig {
    /// Whether reranking is enabled.
    #[serde(default)]
    pub enabled: bool,
    /// Which reranking backend to use.
    #[serde(default)]
    pub backend: RerankBackend,
    /// Maximum number of top candidates to rerank.
    #[serde(default = "default_top_k_rerank")]
    pub top_k_rerank: usize,
    /// Minimum number of candidates required to trigger reranking.
    #[serde(default = "default_min_candidates")]
    pub min_candidates: usize,
    /// Model name for the cross-encoder (e.g. "flashrank").
    #[serde(default = "default_model_name")]
    pub model_name: String,
    /// Fall back to passthrough on reranker error.
    #[serde(default = "default_true")]
    pub fallback_to_passthrough: bool,
}

fn default_top_k_rerank() -> usize {
    DEFAULT_TOP_K_RERANK
}
fn default_min_candidates() -> usize {
    DEFAULT_MIN_CANDIDATES
}
fn default_model_name() -> String {
    "flashrank".to_string()
}
fn default_true() -> bool {
    true
}

impl Default for RerankConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            backend: RerankBackend::Passthrough,
            top_k_rerank: DEFAULT_TOP_K_RERANK,
            min_candidates: DEFAULT_MIN_CANDIDATES,
            model_name: "flashrank".to_string(),
            fallback_to_passthrough: true,
        }
    }
}

/// Outcome/metrics from a reranking invocation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RerankOutcome {
    /// Whether reranking was actually applied.
    pub reranked: bool,
    /// Which backend was used.
    pub backend_used: String,
    /// Number of candidates that were reranked.
    pub candidates_reranked: usize,
    /// Reason reranking was skipped (if not applied).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<String>,
}

// ── B6: FrankenSearch type adapter ──────────────────────────────────────

/// Adapter between FrankenTerm `ScoredDoc` and frankensearch `RerankDocument`/`RerankScore`.
///
/// Provides bidirectional conversion so the frankensearch reranker pipeline
/// can operate on FrankenTerm search results and return updated scores.
#[cfg(feature = "frankensearch")]
pub struct FrankenSearchRerankAdapter;

#[cfg(feature = "frankensearch")]
impl FrankenSearchRerankAdapter {
    /// Convert a FrankenTerm `ScoredDoc` to a frankensearch `RerankDocument`.
    #[must_use]
    pub fn to_rerank_doc(doc: &ScoredDoc) -> frankensearch::RerankDocument {
        frankensearch::RerankDocument {
            doc_id: doc.id.to_string(),
            text: doc.text.clone(),
        }
    }

    /// Convert a batch of `ScoredDoc` to frankensearch `RerankDocument` list.
    #[must_use]
    pub fn to_rerank_docs(docs: &[ScoredDoc]) -> Vec<frankensearch::RerankDocument> {
        docs.iter().map(Self::to_rerank_doc).collect()
    }

    /// Apply frankensearch `RerankScore` results back to the original `ScoredDoc` list.
    ///
    /// Scores are matched by `doc_id` (parsed back to u64). Documents without a
    /// matching score retain their original score. The result is sorted by
    /// descending rerank score (reranked documents first, then originals).
    #[must_use]
    pub fn apply_rerank_scores(
        original_docs: &[ScoredDoc],
        scores: &[frankensearch::RerankScore],
    ) -> Vec<ScoredDoc> {
        // Build a map of doc_id → rerank score for O(1) lookup
        let score_map: HashMap<u64, f32> = scores
            .iter()
            .filter_map(|s| {
                let id = s.doc_id.parse::<u64>().ok()?;
                if s.score.is_finite() {
                    Some((id, s.score))
                } else {
                    None
                }
            })
            .collect();

        let mut result: Vec<ScoredDoc> = original_docs
            .iter()
            .map(|doc| {
                if let Some(&rerank_score) = score_map.get(&doc.id) {
                    ScoredDoc {
                        id: doc.id,
                        text: doc.text.clone(),
                        score: rerank_score,
                    }
                } else {
                    doc.clone()
                }
            })
            .collect();

        // Sort by descending score (NaN-safe: push NaN to end)
        result.sort_by(|a, b| sanitize_score(b.score).total_cmp(&sanitize_score(a.score)));
        result
    }

    /// Convert a frankensearch `RerankScore` to an (id, score) pair.
    #[must_use]
    pub fn score_to_pair(score: &frankensearch::RerankScore) -> Option<(u64, f32)> {
        let id = score.doc_id.parse::<u64>().ok()?;
        Some((id, score.score))
    }
}

// ── B6: Post-fusion reranking ───────────────────────────────────────────

/// Apply reranking to fused search results using a text lookup function.
///
/// This is the main entry point for the orchestrator's reranking step.
/// It converts `FusedResult` candidates to `ScoredDoc` (using `text_fn` to
/// retrieve document text), applies the configured reranker, and returns
/// updated `FusedResult` with reranked scores.
///
/// # Behavior
///
/// - If `config.enabled` is false, returns results unchanged.
/// - If fewer than `config.min_candidates` have text available, skips reranking.
/// - Only the top `config.top_k_rerank` candidates are reranked; the rest keep
///   their original scores and positions.
/// - On reranker error with `fallback_to_passthrough`, returns original results.
pub fn rerank_fused_results(
    query: &str,
    results: Vec<FusedResult>,
    text_fn: &dyn Fn(u64) -> Option<String>,
    config: &RerankConfig,
    reranker: &dyn Reranker,
) -> (Vec<FusedResult>, RerankOutcome) {
    if !config.enabled {
        return (
            results,
            RerankOutcome {
                reranked: false,
                backend_used: "none".to_string(),
                candidates_reranked: 0,
                skip_reason: Some("reranking disabled".to_string()),
            },
        );
    }

    if results.is_empty() {
        return (
            results,
            RerankOutcome {
                reranked: false,
                backend_used: config.backend.as_str().to_string(),
                candidates_reranked: 0,
                skip_reason: Some("empty results".to_string()),
            },
        );
    }

    // Split into rerank candidates (top_k) and remainder
    let rerank_count = results.len().min(config.top_k_rerank);

    // Build ScoredDoc candidates with text from the lookup function
    let mut scored_docs: Vec<ScoredDoc> = Vec::with_capacity(rerank_count);
    let mut included_indices: Vec<usize> = Vec::with_capacity(rerank_count);

    for (i, result) in results.iter().take(rerank_count).enumerate() {
        if let Some(text) = text_fn(result.id) {
            scored_docs.push(ScoredDoc {
                id: result.id,
                text,
                score: result.score,
            });
            included_indices.push(i);
        }
    }

    if included_indices.len() < config.min_candidates {
        return (
            results,
            RerankOutcome {
                reranked: false,
                backend_used: config.backend.as_str().to_string(),
                candidates_reranked: 0,
                skip_reason: Some(format!(
                    "too few candidates with text: {} < {}",
                    included_indices.len(),
                    config.min_candidates
                )),
            },
        );
    }

    // Apply the reranker
    let reranked_docs = match reranker.rerank(query, scored_docs) {
        Ok(docs) => docs,
        Err(err) => {
            if config.fallback_to_passthrough {
                return (
                    results,
                    RerankOutcome {
                        reranked: false,
                        backend_used: config.backend.as_str().to_string(),
                        candidates_reranked: 0,
                        skip_reason: Some(format!("reranker error (fallback): {err}")),
                    },
                );
            }
            return (
                results,
                RerankOutcome {
                    reranked: false,
                    backend_used: config.backend.as_str().to_string(),
                    candidates_reranked: 0,
                    skip_reason: Some(format!("reranker error: {err}")),
                },
            );
        }
    };

    // Build a score map from reranked results
    let score_map: HashMap<u64, f32> = reranked_docs
        .iter()
        .filter(|d| d.score.is_finite())
        .map(|d| (d.id, d.score))
        .collect();

    // Apply rerank scores to the original FusedResults
    let mut updated: Vec<FusedResult> = results
        .iter()
        .take(rerank_count)
        .map(|r| {
            if let Some(&new_score) = score_map.get(&r.id) {
                FusedResult {
                    id: r.id,
                    score: new_score,
                    lexical_rank: r.lexical_rank,
                    semantic_rank: r.semantic_rank,
                }
            } else {
                r.clone()
            }
        })
        .collect();

    // Sort reranked portion by descending score
    updated.sort_by(|a, b| sanitize_score(b.score).total_cmp(&sanitize_score(a.score)));

    // Append non-reranked remainder
    updated.extend(results.into_iter().skip(rerank_count));

    let reranked_count = score_map.len();

    (
        updated,
        RerankOutcome {
            reranked: true,
            backend_used: config.backend.as_str().to_string(),
            candidates_reranked: reranked_count,
            skip_reason: None,
        },
    )
}

/// Sanitize a score for comparison: NaN/Inf → -infinity.
#[inline]
fn sanitize_score(score: f32) -> f32 {
    if score.is_finite() {
        score
    } else {
        f32::NEG_INFINITY
    }
}

// ── B6: FrankenSearch-aware reranking ───────────────────────────────────

/// Apply frankensearch rerank scores to FusedResults (behind `frankensearch` feature).
///
/// Takes `RerankScore` output from frankensearch's async reranker and maps
/// scores back onto the FusedResult list. This bridges the async frankensearch
/// world with the synchronous orchestrator pipeline.
#[cfg(feature = "frankensearch")]
pub fn apply_frankensearch_rerank_scores(
    results: Vec<FusedResult>,
    scores: &[frankensearch::RerankScore],
    top_k_rerank: usize,
) -> Vec<FusedResult> {
    let rerank_count = results.len().min(top_k_rerank);

    let score_map: HashMap<u64, f32> = scores
        .iter()
        .filter_map(|s| {
            let id = s.doc_id.parse::<u64>().ok()?;
            if s.score.is_finite() {
                Some((id, s.score))
            } else {
                None
            }
        })
        .collect();

    let mut updated: Vec<FusedResult> = results
        .iter()
        .take(rerank_count)
        .map(|r| {
            if let Some(&new_score) = score_map.get(&r.id) {
                FusedResult {
                    id: r.id,
                    score: new_score,
                    lexical_rank: r.lexical_rank,
                    semantic_rank: r.semantic_rank,
                }
            } else {
                r.clone()
            }
        })
        .collect();

    updated.sort_by(|a, b| sanitize_score(b.score).total_cmp(&sanitize_score(a.score)));
    updated.extend(results.into_iter().skip(rerank_count));
    updated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_returns_unchanged() {
        let reranker = PassthroughReranker;
        let docs = vec![
            ScoredDoc {
                id: 1,
                text: "hello".into(),
                score: 0.9,
            },
            ScoredDoc {
                id: 2,
                text: "world".into(),
                score: 0.8,
            },
        ];
        let result = reranker.rerank("query", docs).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].id, 1);
    }

    #[test]
    fn passthrough_empty_errors() {
        let reranker = PassthroughReranker;
        let result = reranker.rerank("query", vec![]);
        assert!(result.is_err());
    }

    #[test]
    fn rerank_error_display() {
        let e = RerankError::ModelError("oom".into());
        assert!(e.to_string().contains("oom"));
        let e = RerankError::EmptyInput;
        assert!(e.to_string().contains("empty"));
    }

    #[test]
    fn scored_doc_clone() {
        let doc = ScoredDoc {
            id: 42,
            text: "test".into(),
            score: 0.5,
        };
        let doc2 = doc.clone();
        assert_eq!(doc2.id, 42);
        assert_eq!(doc2.text, "test");
    }

    #[test]
    fn passthrough_preserves_scores() {
        let reranker = PassthroughReranker;
        let docs = vec![
            ScoredDoc {
                id: 1,
                text: "a".into(),
                score: 0.1,
            },
            ScoredDoc {
                id: 2,
                text: "b".into(),
                score: 0.9,
            },
        ];
        let result = reranker.rerank("q", docs).unwrap();
        assert!((result[0].score - 0.1).abs() < f32::EPSILON);
        assert!((result[1].score - 0.9).abs() < f32::EPSILON);
    }

    #[test]
    fn rerank_error_is_error_trait() {
        let e = RerankError::EmptyInput;
        // Verify it implements std::error::Error
        let _: &dyn std::error::Error = &e;
    }

    // =====================================================================
    // RerankError tests
    // =====================================================================

    #[test]
    fn rerank_error_model_error_display() {
        let e = RerankError::ModelError("out of memory".into());
        let msg = e.to_string();
        assert!(msg.contains("rerank model error"));
        assert!(msg.contains("out of memory"));
    }

    #[test]
    fn rerank_error_empty_input_display() {
        let e = RerankError::EmptyInput;
        assert_eq!(e.to_string(), "empty input to reranker");
    }

    #[test]
    fn rerank_error_debug() {
        let e = RerankError::ModelError("fail".into());
        let dbg = format!("{e:?}");
        assert!(dbg.contains("ModelError"));
        assert!(dbg.contains("fail"));

        let e = RerankError::EmptyInput;
        let dbg = format!("{e:?}");
        assert!(dbg.contains("EmptyInput"));
    }

    // =====================================================================
    // ScoredDoc tests
    // =====================================================================

    #[test]
    fn scored_doc_debug() {
        let doc = ScoredDoc {
            id: 1,
            text: "hello".into(),
            score: 0.9,
        };
        let dbg = format!("{doc:?}");
        assert!(dbg.contains("ScoredDoc"));
        assert!(dbg.contains("hello"));
    }

    #[test]
    fn scored_doc_clone_preserves_all_fields() {
        let doc = ScoredDoc {
            id: 999,
            text: "long text content here".into(),
            score: 0.12345,
        };
        let doc2 = doc.clone();
        assert_eq!(doc2.id, 999);
        assert_eq!(doc2.text, "long text content here");
        assert!((doc2.score - 0.12345).abs() < f32::EPSILON);
    }

    // =====================================================================
    // PassthroughReranker additional tests
    // =====================================================================

    #[test]
    fn passthrough_single_doc() {
        let reranker = PassthroughReranker;
        let docs = vec![ScoredDoc {
            id: 1,
            text: "only".into(),
            score: 1.0,
        }];
        let result = reranker.rerank("q", docs).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, 1);
    }

    #[test]
    fn passthrough_preserves_order() {
        let reranker = PassthroughReranker;
        let docs = vec![
            ScoredDoc {
                id: 3,
                text: "third".into(),
                score: 0.3,
            },
            ScoredDoc {
                id: 1,
                text: "first".into(),
                score: 0.1,
            },
            ScoredDoc {
                id: 2,
                text: "second".into(),
                score: 0.2,
            },
        ];
        let result = reranker.rerank("query", docs).unwrap();
        assert_eq!(result[0].id, 3);
        assert_eq!(result[1].id, 1);
        assert_eq!(result[2].id, 2);
    }

    #[test]
    fn passthrough_large_batch() {
        let reranker = PassthroughReranker;
        let docs: Vec<ScoredDoc> = (0..100)
            .map(|i| ScoredDoc {
                id: i,
                text: format!("doc-{i}"),
                score: i as f32 / 100.0,
            })
            .collect();
        let result = reranker.rerank("q", docs).unwrap();
        assert_eq!(result.len(), 100);
        assert_eq!(result[0].id, 0);
        assert_eq!(result[99].id, 99);
    }

    #[test]
    fn passthrough_preserves_text_content() {
        let reranker = PassthroughReranker;
        let docs = vec![ScoredDoc {
            id: 1,
            text: "special chars: !@#$%^&*()".into(),
            score: 0.5,
        }];
        let result = reranker.rerank("q", docs).unwrap();
        assert_eq!(result[0].text, "special chars: !@#$%^&*()");
    }

    // =====================================================================
    // Trait object usage
    // =====================================================================

    #[test]
    fn passthrough_as_trait_object() {
        let reranker: Box<dyn Reranker> = Box::new(PassthroughReranker);
        let docs = vec![ScoredDoc {
            id: 1,
            text: "via trait".into(),
            score: 0.7,
        }];
        let result = reranker.rerank("q", docs).unwrap();
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn passthrough_as_arc_trait_object() {
        let reranker: std::sync::Arc<dyn Reranker> = std::sync::Arc::new(PassthroughReranker);
        let docs = vec![ScoredDoc {
            id: 1,
            text: "arc".into(),
            score: 0.5,
        }];
        let result = reranker.rerank("q", docs).unwrap();
        assert_eq!(result[0].text, "arc");
    }

    // =====================================================================
    // Passthrough edge cases
    // =====================================================================

    #[test]
    fn passthrough_different_queries_same_docs() {
        let reranker = PassthroughReranker;
        let make_docs = || {
            vec![ScoredDoc {
                id: 1,
                text: "doc".into(),
                score: 1.0,
            }]
        };
        let r1 = reranker.rerank("query A", make_docs()).unwrap();
        let r2 = reranker.rerank("query B", make_docs()).unwrap();
        // Passthrough ignores query, results should be identical
        assert_eq!(r1[0].id, r2[0].id);
        assert!((r1[0].score - r2[0].score).abs() < f32::EPSILON);
    }

    #[test]
    fn passthrough_empty_query_string() {
        let reranker = PassthroughReranker;
        let docs = vec![ScoredDoc {
            id: 1,
            text: "doc".into(),
            score: 0.5,
        }];
        let result = reranker.rerank("", docs).unwrap();
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn passthrough_zero_score() {
        let reranker = PassthroughReranker;
        let docs = vec![ScoredDoc {
            id: 1,
            text: "zero".into(),
            score: 0.0,
        }];
        let result = reranker.rerank("q", docs).unwrap();
        assert!(result[0].score.abs() < f32::EPSILON);
    }

    #[test]
    fn passthrough_negative_score() {
        let reranker = PassthroughReranker;
        let docs = vec![ScoredDoc {
            id: 1,
            text: "neg".into(),
            score: -1.5,
        }];
        let result = reranker.rerank("q", docs).unwrap();
        assert!((result[0].score - (-1.5)).abs() < f32::EPSILON);
    }

    #[test]
    fn passthrough_unicode_text_and_query() {
        let reranker = PassthroughReranker;
        let docs = vec![ScoredDoc {
            id: 1,
            text: "日本語テスト 🎉".into(),
            score: 0.9,
        }];
        let result = reranker.rerank("検索クエリ", docs).unwrap();
        assert_eq!(result[0].text, "日本語テスト 🎉");
    }

    #[test]
    fn passthrough_empty_text() {
        let reranker = PassthroughReranker;
        let docs = vec![ScoredDoc {
            id: 1,
            text: String::new(),
            score: 0.5,
        }];
        let result = reranker.rerank("q", docs).unwrap();
        assert_eq!(result[0].text, "");
    }

    #[test]
    fn passthrough_duplicate_ids() {
        let reranker = PassthroughReranker;
        let docs = vec![
            ScoredDoc {
                id: 1,
                text: "a".into(),
                score: 0.5,
            },
            ScoredDoc {
                id: 1,
                text: "b".into(),
                score: 0.6,
            },
        ];
        let result = reranker.rerank("q", docs).unwrap();
        assert_eq!(result.len(), 2);
        // Both keep their own text despite same id
        assert_eq!(result[0].text, "a");
        assert_eq!(result[1].text, "b");
    }

    // =====================================================================
    // RerankError additional edge cases
    // =====================================================================

    #[test]
    fn rerank_error_model_error_empty_message() {
        let e = RerankError::ModelError(String::new());
        let msg = e.to_string();
        assert!(msg.contains("rerank model error"));
    }

    #[test]
    fn rerank_error_model_error_long_message() {
        let long_msg = "x".repeat(1000);
        let e = RerankError::ModelError(long_msg.clone());
        let msg = e.to_string();
        assert!(msg.contains(&long_msg));
    }

    // =====================================================================
    // ScoredDoc additional tests
    // =====================================================================

    #[test]
    fn scored_doc_max_id() {
        let doc = ScoredDoc {
            id: u64::MAX,
            text: "max".into(),
            score: 0.0,
        };
        assert_eq!(doc.id, u64::MAX);
    }

    #[test]
    fn scored_doc_large_text() {
        let large = "x".repeat(10_000);
        let doc = ScoredDoc {
            id: 1,
            text: large.clone(),
            score: 0.5,
        };
        let doc2 = doc.clone();
        assert_eq!(doc2.text.len(), 10_000);
    }

    #[test]
    fn scored_doc_nan_score() {
        let doc = ScoredDoc {
            id: 1,
            text: "nan".into(),
            score: f32::NAN,
        };
        assert!(doc.score.is_nan());
    }

    #[test]
    fn scored_doc_infinity_score() {
        let doc = ScoredDoc {
            id: 1,
            text: "inf".into(),
            score: f32::INFINITY,
        };
        assert!(doc.score.is_infinite());
    }

    // =====================================================================
    // B6: RerankBackend tests
    // =====================================================================

    #[test]
    fn rerank_backend_default_is_passthrough() {
        assert_eq!(RerankBackend::default(), RerankBackend::Passthrough);
    }

    #[test]
    fn rerank_backend_parse() {
        assert_eq!(
            RerankBackend::parse("passthrough"),
            RerankBackend::Passthrough
        );
        assert_eq!(
            RerankBackend::parse("frankensearch"),
            RerankBackend::FrankenSearch
        );
        assert_eq!(
            RerankBackend::parse("flashrank"),
            RerankBackend::FrankenSearch
        );
        assert_eq!(
            RerankBackend::parse("cross_encoder"),
            RerankBackend::FrankenSearch
        );
        assert_eq!(
            RerankBackend::parse("crossencoder"),
            RerankBackend::FrankenSearch
        );
        assert_eq!(
            RerankBackend::parse("FRANKENSEARCH"),
            RerankBackend::FrankenSearch
        );
        assert_eq!(RerankBackend::parse("unknown"), RerankBackend::Passthrough);
        assert_eq!(RerankBackend::parse(""), RerankBackend::Passthrough);
    }

    #[test]
    fn rerank_backend_as_str() {
        assert_eq!(RerankBackend::Passthrough.as_str(), "passthrough");
        assert_eq!(RerankBackend::FrankenSearch.as_str(), "frankensearch");
    }

    #[test]
    fn rerank_backend_display() {
        assert_eq!(format!("{}", RerankBackend::Passthrough), "passthrough");
        assert_eq!(format!("{}", RerankBackend::FrankenSearch), "frankensearch");
    }

    #[test]
    fn rerank_backend_serde_roundtrip() {
        for backend in [RerankBackend::Passthrough, RerankBackend::FrankenSearch] {
            let json = serde_json::to_string(&backend).unwrap();
            let back: RerankBackend = serde_json::from_str(&json).unwrap();
            assert_eq!(backend, back);
        }
    }

    // =====================================================================
    // B6: RerankConfig tests
    // =====================================================================

    #[test]
    fn rerank_config_default() {
        let cfg = RerankConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.backend, RerankBackend::Passthrough);
        assert_eq!(cfg.top_k_rerank, 100);
        assert_eq!(cfg.min_candidates, 5);
        assert_eq!(cfg.model_name, "flashrank");
        assert!(cfg.fallback_to_passthrough);
    }

    #[test]
    fn rerank_config_serde_roundtrip() {
        let cfg = RerankConfig {
            enabled: true,
            backend: RerankBackend::FrankenSearch,
            top_k_rerank: 50,
            min_candidates: 3,
            model_name: "custom-model".to_string(),
            fallback_to_passthrough: false,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: RerankConfig = serde_json::from_str(&json).unwrap();
        assert!(back.enabled);
        assert_eq!(back.backend, RerankBackend::FrankenSearch);
        assert_eq!(back.top_k_rerank, 50);
        assert_eq!(back.min_candidates, 3);
        assert_eq!(back.model_name, "custom-model");
        assert!(!back.fallback_to_passthrough);
    }

    #[test]
    fn rerank_config_serde_absent_defaults() {
        let json = r"{}";
        let cfg: RerankConfig = serde_json::from_str(json).unwrap();
        assert!(!cfg.enabled);
        assert_eq!(cfg.backend, RerankBackend::Passthrough);
        assert_eq!(cfg.top_k_rerank, 100);
        assert_eq!(cfg.min_candidates, 5);
        assert_eq!(cfg.model_name, "flashrank");
        assert!(cfg.fallback_to_passthrough);
    }

    // =====================================================================
    // B6: RerankOutcome tests
    // =====================================================================

    #[test]
    fn rerank_outcome_default() {
        let outcome = RerankOutcome::default();
        assert!(!outcome.reranked);
        assert_eq!(outcome.backend_used, "");
        assert_eq!(outcome.candidates_reranked, 0);
        assert!(outcome.skip_reason.is_none());
    }

    #[test]
    fn rerank_outcome_serde_roundtrip() {
        let outcome = RerankOutcome {
            reranked: true,
            backend_used: "frankensearch".to_string(),
            candidates_reranked: 42,
            skip_reason: None,
        };
        let json = serde_json::to_string(&outcome).unwrap();
        let back: RerankOutcome = serde_json::from_str(&json).unwrap();
        assert!(back.reranked);
        assert_eq!(back.backend_used, "frankensearch");
        assert_eq!(back.candidates_reranked, 42);
    }

    #[test]
    fn rerank_outcome_serde_with_skip_reason() {
        let outcome = RerankOutcome {
            reranked: false,
            backend_used: "passthrough".to_string(),
            candidates_reranked: 0,
            skip_reason: Some("too few candidates".to_string()),
        };
        let json = serde_json::to_string(&outcome).unwrap();
        assert!(json.contains("too few candidates"));
        let back: RerankOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(back.skip_reason.as_deref(), Some("too few candidates"));
    }

    #[derive(Clone, Debug, PartialEq)]
    struct SearchRow {
        id: i64,
        text: String,
        retrieval_score: f64,
    }

    fn search_document(row: &SearchRow) -> (i64, &str) {
        (row.id, &row.text)
    }

    struct RecordingReranker {
        seen: std::sync::Mutex<Vec<(String, Vec<ScoredDoc>)>>,
    }

    impl Reranker for RecordingReranker {
        fn rerank(
            &self,
            query: &str,
            mut docs: Vec<ScoredDoc>,
        ) -> Result<Vec<ScoredDoc>, RerankError> {
            self.seen
                .lock()
                .unwrap()
                .push((query.to_string(), docs.clone()));
            for doc in &mut docs {
                doc.score = if doc.id == 1 { 8.0 } else { 2.0 };
            }
            // The integration must rank from scores and retain original tie order,
            // even if a backend returns the scores in an arbitrary order.
            docs.reverse();
            Ok(docs)
        }
    }

    #[test]
    fn live_reranking_forwards_query_bounds_work_and_preserves_candidate_identity() {
        let query = "Rate_LIMIT OR \"Retry after\"";
        let mut rows: Vec<SearchRow> = (0..103)
            .map(|index| SearchRow {
                id: index as i64,
                text: format!("original document {index}"),
                retrieval_score: -(index as f64) / 10.0,
            })
            .collect();
        rows[0].id = 42;
        rows[1].id = 42; // Matching IDs must not alias distinct candidate rows.
        rows[2].text = "文".repeat(6_000);
        let originals = rows.clone();
        let reranker = RecordingReranker {
            seen: std::sync::Mutex::new(Vec::new()),
        };
        let (output, report) = rerank_search_rows(query, rows, search_document, &reranker).unwrap();
        let seen = reranker.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, query);
        assert_eq!(seen[0].1.len(), SEARCH_RERANK_MAX_CANDIDATES);
        assert_eq!(seen[0].1[0].id, 0);
        assert_eq!(seen[0].1[1].id, 1);
        assert_eq!(seen[0].1[0].text, originals[0].text);
        assert_eq!(seen[0].1[1].text, originals[1].text);
        assert!(seen[0].1[2].text.len() <= SEARCH_RERANK_MAX_TEXT_BYTES);
        assert!(originals[2].text.starts_with(&seen[0].1[2].text));
        assert_eq!(output[0], originals[1]);
        assert_eq!(output[1], originals[0]);
        assert_eq!(&output[2..], &originals[2..]);
        assert_eq!(report.candidates_reranked, 100);
        assert_eq!(report.truncated_documents, 1);
        assert_eq!(report.scores[0].segment_id, 42);
        assert_eq!(report.scores[0].original_rank, 2);
        assert_eq!(report.scores[0].score, 8.0);
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains(query));
        assert!(!json.contains("original document"));
    }

    struct ReturnedDocuments(Vec<ScoredDoc>);

    impl Reranker for ReturnedDocuments {
        fn rerank(
            &self,
            _query: &str,
            _docs: Vec<ScoredDoc>,
        ) -> Result<Vec<ScoredDoc>, RerankError> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn live_reranking_rejects_missing_foreign_duplicate_and_nonfinite_scores() {
        let rows = vec![
            SearchRow {
                id: 7,
                text: "first".into(),
                retrieval_score: -1.0,
            },
            SearchRow {
                id: 8,
                text: "second".into(),
                retrieval_score: -2.0,
            },
        ];
        let valid = vec![
            ScoredDoc {
                id: 0,
                text: "first".into(),
                score: 0.2,
            },
            ScoredDoc {
                id: 1,
                text: "second".into(),
                score: 0.8,
            },
        ];
        let mut duplicate = valid.clone();
        duplicate[1].id = 0;
        let mut foreign = valid.clone();
        foreign[1].id = u64::MAX;
        let mut nonfinite = valid.clone();
        nonfinite[0].score = f32::NAN;
        let mut infinity = valid.clone();
        infinity[0].score = f32::INFINITY;
        let mut changed_text = valid.clone();
        changed_text[0].text = "a different candidate".into();
        for malformed in [
            Vec::new(),
            vec![valid[0].clone()],
            duplicate,
            foreign,
            nonfinite,
            infinity,
            changed_text,
        ] {
            assert!(
                rerank_search_rows(
                    "query",
                    rows.clone(),
                    search_document,
                    &ReturnedDocuments(malformed),
                )
                .is_err()
            );
        }
    }

    #[test]
    fn fused_reranking_forwards_the_actual_query() {
        let reranker = RecordingReranker {
            seen: std::sync::Mutex::new(Vec::new()),
        };
        let config = RerankConfig {
            enabled: true,
            min_candidates: 1,
            ..Default::default()
        };
        let (_, outcome) = rerank_fused_results(
            "specific error phrase",
            make_fused(&[(0, 1.0), (1, 0.5)]),
            &text_for_id,
            &config,
            &reranker,
        );
        assert!(outcome.reranked);
        assert_eq!(reranker.seen.lock().unwrap()[0].0, "specific error phrase");
    }

    fn run_rerank_async_test(future: impl std::future::Future<Output = ()>) {
        use crate::runtime_async::CompatRuntime;
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("reranker test runtime");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(future);
        }));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(runtime)));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::runtime_async::clear_runtime_handle();
        }));
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn disabled_live_reranking_never_reads_candidates_or_model_files() {
        run_rerank_async_test(async {
            let config = crate::config::SearchConfig {
                reranker_enabled: false,
                models_dir: "/missing/disabled-reranker".into(),
                ..Default::default()
            };
            let rows = vec![SearchRow {
                id: 8,
                text: "unchanged".into(),
                retrieval_score: -0.4,
            }];
            let (output, report) = rerank_search_results_with_cx(
                &crate::cx::for_request(),
                &config,
                &"x".repeat(SEARCH_RERANK_MAX_TEXT_BYTES + 1),
                rows.clone(),
                |_| panic!("disabled reranking must not inspect candidate text"),
            )
            .await
            .unwrap();
            assert_eq!(output, rows);
            assert!(report.is_none());
        });
    }

    #[cfg(feature = "semantic-search")]
    #[test]
    fn enabled_live_reranking_reports_empty_results_and_missing_model_honestly() {
        run_rerank_async_test(async {
            let directory = tempfile::tempdir().unwrap();
            let config = crate::config::SearchConfig {
                reranker_enabled: true,
                models_dir: directory.path().to_string_lossy().into_owned(),
                ..Default::default()
            };
            let cx = crate::cx::for_request();
            let (_, report) = rerank_search_results_with_cx(
                &cx,
                &config,
                "query",
                Vec::<SearchRow>::new(),
                search_document,
            )
            .await
            .unwrap();
            let report = report.unwrap();
            assert!(!report.reranked);
            assert_eq!(report.candidates_reranked, 0);
            assert_eq!(report.skip_reason.as_deref(), Some("no search results"));
            let rows = vec![SearchRow {
                id: 9,
                text: "candidate".into(),
                retrieval_score: -0.6,
            }];
            let error = rerank_search_results_with_cx(&cx, &config, "query", rows, search_document)
                .await
                .unwrap_err();
            let message = error.to_string();
            assert!(
                message.contains("model.onnx") || message.contains("reranker is busy"),
                "{message}",
            );
        });
    }

    #[cfg(not(feature = "semantic-search"))]
    #[test]
    fn enabled_live_reranking_rejects_builds_without_inference_support() {
        run_rerank_async_test(async {
            let config = crate::config::SearchConfig {
                reranker_enabled: true,
                ..Default::default()
            };
            let error = rerank_search_results_with_cx(
                &crate::cx::for_request(),
                &config,
                "query",
                Vec::<SearchRow>::new(),
                search_document,
            )
            .await
            .unwrap_err();
            assert!(error.to_string().contains("semantic-search"));
        });
    }

    #[cfg(feature = "semantic-search")]
    #[test]
    fn cross_encoder_missing_or_invalid_bundle_never_becomes_loaded() {
        let directory = tempfile::tempdir().unwrap();
        let model_path = directory.path().join("model.onnx");
        let mut reranker = CrossEncoderReranker::new(model_path.to_string_lossy());
        assert!(!reranker.is_loaded());
        assert!(reranker.load().is_err());
        // All named files exist, but this is not an ONNX/tokenizer bundle.
        // The former existence-only loader incorrectly reported success here.
        for name in [
            "model.onnx",
            "tokenizer.json",
            "config.json",
            "special_tokens_map.json",
            "tokenizer_config.json",
        ] {
            std::fs::write(directory.path().join(name), b"invalid model data").unwrap();
        }
        assert!(reranker.load().is_err());
        assert!(!reranker.is_loaded());
        assert!(matches!(
            reranker.rerank(
                "query",
                vec![ScoredDoc {
                    id: 0,
                    text: "document".into(),
                    score: 0.0
                }]
            ),
            Err(RerankError::ModelNotLoaded(_)),
        ));
    }

    /// Positive native proof for maintainers with an explicitly provisioned
    /// local bundle. Never fetches weights or runs in the default test suite.
    #[cfg(feature = "semantic-search")]
    #[test]
    #[ignore = "set FT_TEST_RERANK_MODEL to a compatible local model.onnx and run explicitly"]
    fn cross_encoder_local_model_native_smoke() {
        let path = std::env::var("FT_TEST_RERANK_MODEL")
            .expect("FT_TEST_RERANK_MODEL must explicitly name a local model.onnx");
        let mut model = CrossEncoderReranker::new(path);
        model
            .load()
            .expect("local model must initialize and complete readiness inference");
        assert!(model.is_loaded());
        let documents = vec![
            ScoredDoc { id: 31, text: "A recipe for roasting carrots with olive oil.".into(), score: 0.0 },
            ScoredDoc { id: 72, text: "Rust compilation failed with error E0308: mismatched types; expected u64, found String.".into(), score: 0.0 },
            ScoredDoc { id: 94, text: "The train leaves at six in the morning.".into(), score: 0.0 },
        ];
        let result = model
            .rerank("Rust compiler mismatched types error", documents.clone())
            .expect("real cross-encoder inference must succeed");
        assert_eq!(result.len(), documents.len());
        assert!(result.iter().all(|doc| doc.score.is_finite()));
        assert!(result.windows(2).all(|pair| pair[0].score >= pair[1].score));
        // Sorted non-increasing above, so a strict drop means the scores differ.
        assert!(result.windows(2).any(|pair| pair[0].score > pair[1].score));
        let mut ids: Vec<_> = result.iter().map(|doc| doc.id).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![31, 72, 94]);
        for scored in result {
            let original = documents.iter().find(|doc| doc.id == scored.id).unwrap();
            assert_eq!(scored.text, original.text);
        }
    }

    // =====================================================================
    // B6: rerank_fused_results tests
    // =====================================================================

    fn make_fused(ids_scores: &[(u64, f32)]) -> Vec<FusedResult> {
        ids_scores
            .iter()
            .map(|&(id, score)| FusedResult {
                id,
                score,
                lexical_rank: None,
                semantic_rank: None,
            })
            .collect()
    }

    fn text_for_id(id: u64) -> Option<String> {
        Some(format!("document text for id {id}"))
    }

    #[test]
    fn rerank_fused_disabled_returns_unchanged() {
        let results = make_fused(&[(1, 0.9), (2, 0.8), (3, 0.7)]);
        let config = RerankConfig::default(); // enabled = false
        let (out, outcome) = rerank_fused_results(
            "query",
            results.clone(),
            &text_for_id,
            &config,
            &PassthroughReranker,
        );
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].id, 1);
        assert!(!outcome.reranked);
        assert_eq!(outcome.skip_reason.as_deref(), Some("reranking disabled"));
    }

    #[test]
    fn rerank_fused_empty_results() {
        let config = RerankConfig {
            enabled: true,
            ..Default::default()
        };
        let (out, outcome) =
            rerank_fused_results("query", vec![], &text_for_id, &config, &PassthroughReranker);
        assert!(out.is_empty());
        assert!(!outcome.reranked);
        assert_eq!(outcome.skip_reason.as_deref(), Some("empty results"));
    }

    #[test]
    fn rerank_fused_too_few_candidates() {
        let results = make_fused(&[(1, 0.9), (2, 0.8)]);
        let config = RerankConfig {
            enabled: true,
            min_candidates: 5, // need 5, only have 2
            ..Default::default()
        };
        let (out, outcome) = rerank_fused_results(
            "query",
            results,
            &text_for_id,
            &config,
            &PassthroughReranker,
        );
        assert_eq!(out.len(), 2);
        assert!(!outcome.reranked);
        assert!(
            outcome
                .skip_reason
                .as_deref()
                .unwrap()
                .contains("too few candidates")
        );
    }

    #[test]
    fn rerank_fused_passthrough_preserves_scores() {
        let results = make_fused(&[(1, 0.9), (2, 0.8), (3, 0.7), (4, 0.6), (5, 0.5)]);
        let config = RerankConfig {
            enabled: true,
            min_candidates: 3,
            ..Default::default()
        };
        let (out, outcome) = rerank_fused_results(
            "query",
            results,
            &text_for_id,
            &config,
            &PassthroughReranker,
        );
        assert_eq!(out.len(), 5);
        assert!(outcome.reranked);
        assert_eq!(outcome.candidates_reranked, 5);
        // Passthrough preserves scores so order should be unchanged
        assert_eq!(out[0].id, 1);
        assert_eq!(out[4].id, 5);
    }

    #[test]
    fn rerank_fused_top_k_limits_reranking() {
        let results = make_fused(&[
            (1, 0.9),
            (2, 0.8),
            (3, 0.7),
            (4, 0.6),
            (5, 0.5),
            (6, 0.4),
            (7, 0.3),
        ]);
        let config = RerankConfig {
            enabled: true,
            min_candidates: 2,
            top_k_rerank: 3, // only rerank top 3
            ..Default::default()
        };
        let (out, outcome) = rerank_fused_results(
            "query",
            results,
            &text_for_id,
            &config,
            &PassthroughReranker,
        );
        assert_eq!(out.len(), 7);
        assert!(outcome.reranked);
        // The last 4 should keep their original order
        assert_eq!(out[3].id, 4);
        assert_eq!(out[6].id, 7);
    }

    #[test]
    fn rerank_fused_missing_text_reduces_candidates() {
        let results = make_fused(&[(1, 0.9), (2, 0.8), (3, 0.7)]);
        // Only provide text for id=1, not for 2 or 3
        let sparse_text = |id: u64| -> Option<String> {
            if id == 1 {
                Some("text".to_string())
            } else {
                None
            }
        };
        let config = RerankConfig {
            enabled: true,
            min_candidates: 2, // need 2, only 1 has text
            ..Default::default()
        };
        let (out, outcome) = rerank_fused_results(
            "query",
            results,
            &sparse_text,
            &config,
            &PassthroughReranker,
        );
        assert_eq!(out.len(), 3);
        assert!(!outcome.reranked);
        assert!(
            outcome
                .skip_reason
                .as_deref()
                .unwrap()
                .contains("too few candidates")
        );
    }

    /// A test reranker that reverses document order and assigns decreasing scores.
    struct ReversingReranker;

    impl Reranker for ReversingReranker {
        fn rerank(
            &self,
            _query: &str,
            mut docs: Vec<ScoredDoc>,
        ) -> Result<Vec<ScoredDoc>, RerankError> {
            if docs.is_empty() {
                return Err(RerankError::EmptyInput);
            }
            docs.reverse();
            for (i, doc) in docs.iter_mut().enumerate() {
                doc.score = (i as f32).mul_add(-0.1, 1.0);
            }
            Ok(docs)
        }
    }

    #[test]
    fn rerank_fused_with_reversing_reranker() {
        let results = make_fused(&[(1, 0.9), (2, 0.8), (3, 0.7), (4, 0.6), (5, 0.5)]);
        let config = RerankConfig {
            enabled: true,
            min_candidates: 3,
            ..Default::default()
        };
        let (out, outcome) =
            rerank_fused_results("query", results, &text_for_id, &config, &ReversingReranker);
        assert!(outcome.reranked);
        assert_eq!(outcome.candidates_reranked, 5);
        // Reversing reranker gives highest score to last doc (id=5)
        assert_eq!(out[0].id, 5);
    }

    /// A test reranker that always fails.
    struct FailingReranker;

    impl Reranker for FailingReranker {
        fn rerank(
            &self,
            _query: &str,
            _docs: Vec<ScoredDoc>,
        ) -> Result<Vec<ScoredDoc>, RerankError> {
            Err(RerankError::ModelError("test failure".into()))
        }
    }

    #[test]
    fn rerank_fused_error_with_fallback() {
        let results = make_fused(&[(1, 0.9), (2, 0.8), (3, 0.7), (4, 0.6), (5, 0.5)]);
        let config = RerankConfig {
            enabled: true,
            min_candidates: 3,
            fallback_to_passthrough: true,
            ..Default::default()
        };
        let (out, outcome) =
            rerank_fused_results("query", results, &text_for_id, &config, &FailingReranker);
        assert_eq!(out.len(), 5);
        assert!(!outcome.reranked);
        assert!(
            outcome
                .skip_reason
                .as_deref()
                .unwrap()
                .contains("reranker error (fallback)")
        );
        // Original order preserved on fallback
        assert_eq!(out[0].id, 1);
    }

    #[test]
    fn rerank_fused_error_without_fallback() {
        let results = make_fused(&[(1, 0.9), (2, 0.8), (3, 0.7), (4, 0.6), (5, 0.5)]);
        let config = RerankConfig {
            enabled: true,
            min_candidates: 3,
            fallback_to_passthrough: false,
            ..Default::default()
        };
        let (out, outcome) =
            rerank_fused_results("query", results, &text_for_id, &config, &FailingReranker);
        assert_eq!(out.len(), 5);
        assert!(!outcome.reranked);
        assert!(
            outcome
                .skip_reason
                .as_deref()
                .unwrap()
                .contains("reranker error:")
        );
    }

    // =====================================================================
    // B6: sanitize_score tests
    // =====================================================================

    #[test]
    fn sanitize_finite_scores() {
        assert!((sanitize_score(0.5) - 0.5).abs() < f32::EPSILON);
        assert!((sanitize_score(-1.0) - (-1.0)).abs() < f32::EPSILON);
    }

    #[test]
    fn sanitize_nan_to_neg_infinity() {
        assert_eq!(
            sanitize_score(f32::NAN).to_bits(),
            f32::NEG_INFINITY.to_bits()
        );
    }

    #[test]
    fn sanitize_infinity_to_neg_infinity() {
        assert_eq!(
            sanitize_score(f32::INFINITY).to_bits(),
            f32::NEG_INFINITY.to_bits()
        );
        assert_eq!(
            sanitize_score(f32::NEG_INFINITY).to_bits(),
            f32::NEG_INFINITY.to_bits()
        );
    }

    // =====================================================================
    // RerankError::ModelNotLoaded tests
    // =====================================================================

    #[test]
    fn rerank_error_model_not_loaded_display() {
        let err = RerankError::ModelNotLoaded("/path/to/model".to_string());
        let msg = err.to_string();
        assert!(
            msg.contains("not loaded"),
            "error should mention 'not loaded': {msg}"
        );
        assert!(
            msg.contains("/path/to/model"),
            "error should include model path: {msg}"
        );
    }

    #[test]
    fn passthrough_reranker_returns_docs_unchanged() {
        let reranker = PassthroughReranker;
        let docs = vec![
            ScoredDoc {
                id: 1,
                text: "first".into(),
                score: 0.5,
            },
            ScoredDoc {
                id: 2,
                text: "second".into(),
                score: 0.9,
            },
        ];
        let result = reranker.rerank("query", docs).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].id, 1);
        assert_eq!(result[1].id, 2);
    }

    #[test]
    fn passthrough_reranker_rejects_empty_input() {
        let reranker = PassthroughReranker;
        let result = reranker.rerank("query", vec![]);
        assert!(matches!(result, Err(RerankError::EmptyInput)));
    }
}
