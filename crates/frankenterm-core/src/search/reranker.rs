//! Reranker trait, implementations, and frankensearch-rerank adapter (B6).
//!
//! Provides:
//! - `Reranker` trait: synchronous reranking interface for FrankenTerm
//! - `PassthroughReranker`: no-op fallback
//! - `CrossEncoderReranker`: stub for direct ONNX inference (semantic-search feature)
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

#[derive(Debug)]
pub enum RerankError {
    ModelError(String),
    EmptyInput,
}

impl fmt::Display for RerankError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ModelError(e) => write!(f, "rerank model error: {e}"),
            Self::EmptyInput => write!(f, "empty input to reranker"),
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

/// Cross-encoder reranker (requires semantic-search feature).
#[cfg(feature = "semantic-search")]
pub struct CrossEncoderReranker {
    model_path: String,
}

#[cfg(feature = "semantic-search")]
impl CrossEncoderReranker {
    pub fn new(model_path: impl Into<String>) -> Self {
        Self {
            model_path: model_path.into(),
        }
    }

    pub fn model_path(&self) -> &str {
        &self.model_path
    }
}

#[cfg(feature = "semantic-search")]
impl Reranker for CrossEncoderReranker {
    fn rerank(&self, _query: &str, docs: Vec<ScoredDoc>) -> Result<Vec<ScoredDoc>, RerankError> {
        if docs.is_empty() {
            return Err(RerankError::EmptyInput);
        }
        // Stub: would invoke cross-encoder model inference here
        Ok(docs)
    }
}

// ── B6: Rerank configuration ────────────────────────────────────────────

/// Backend selector for the reranking step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RerankBackend {
    /// No-op: return candidates unchanged (default).
    Passthrough,
    /// Delegate to frankensearch-rerank adapter (cross-encoder scoring).
    FrankenSearch,
}

impl Default for RerankBackend {
    fn default() -> Self {
        Self::Passthrough
    }
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
    let reranked_docs = match reranker.rerank("", scored_docs) {
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
        assert!(result[0].text.is_empty());
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
        let json = r#"{}"#;
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
        assert!(outcome.backend_used.is_empty());
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
        assert_eq!(
            back.skip_reason.as_deref(),
            Some("too few candidates")
        );
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
        let (out, outcome) = rerank_fused_results(results.clone(), &text_for_id, &config, &PassthroughReranker);
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
        let (out, outcome) = rerank_fused_results(vec![], &text_for_id, &config, &PassthroughReranker);
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
        let (out, outcome) = rerank_fused_results(results, &text_for_id, &config, &PassthroughReranker);
        assert_eq!(out.len(), 2);
        assert!(!outcome.reranked);
        assert!(outcome
            .skip_reason
            .as_deref()
            .unwrap()
            .contains("too few candidates"));
    }

    #[test]
    fn rerank_fused_passthrough_preserves_scores() {
        let results = make_fused(&[(1, 0.9), (2, 0.8), (3, 0.7), (4, 0.6), (5, 0.5)]);
        let config = RerankConfig {
            enabled: true,
            min_candidates: 3,
            ..Default::default()
        };
        let (out, outcome) =
            rerank_fused_results(results, &text_for_id, &config, &PassthroughReranker);
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
        let (out, outcome) =
            rerank_fused_results(results, &text_for_id, &config, &PassthroughReranker);
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
        let (out, outcome) =
            rerank_fused_results(results, &sparse_text, &config, &PassthroughReranker);
        assert_eq!(out.len(), 3);
        assert!(!outcome.reranked);
        assert!(outcome
            .skip_reason
            .as_deref()
            .unwrap()
            .contains("too few candidates"));
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
                doc.score = 1.0 - (i as f32 * 0.1);
            }
            Ok(docs)
        }
    }

    #[test]
    fn rerank_fused_with_reversing_reranker() {
        let results = make_fused(&[
            (1, 0.9),
            (2, 0.8),
            (3, 0.7),
            (4, 0.6),
            (5, 0.5),
        ]);
        let config = RerankConfig {
            enabled: true,
            min_candidates: 3,
            ..Default::default()
        };
        let (out, outcome) =
            rerank_fused_results(results, &text_for_id, &config, &ReversingReranker);
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
            rerank_fused_results(results, &text_for_id, &config, &FailingReranker);
        assert_eq!(out.len(), 5);
        assert!(!outcome.reranked);
        assert!(outcome
            .skip_reason
            .as_deref()
            .unwrap()
            .contains("reranker error (fallback)"));
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
            rerank_fused_results(results, &text_for_id, &config, &FailingReranker);
        assert_eq!(out.len(), 5);
        assert!(!outcome.reranked);
        assert!(outcome
            .skip_reason
            .as_deref()
            .unwrap()
            .contains("reranker error:"));
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
        assert!(sanitize_score(f32::NAN) == f32::NEG_INFINITY);
    }

    #[test]
    fn sanitize_infinity_to_neg_infinity() {
        assert!(sanitize_score(f32::INFINITY) == f32::NEG_INFINITY);
        assert!(sanitize_score(f32::NEG_INFINITY) == f32::NEG_INFINITY);
    }
}
