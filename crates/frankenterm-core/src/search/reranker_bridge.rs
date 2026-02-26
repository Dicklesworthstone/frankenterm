//! Reranker bridge adapter for the B6 frankensearch migration.
//!
//! Maps between the local `Reranker` trait (sync, `ScoredDoc`-based) and
//! frankensearch's reranker types (`RerankDocument`/`RerankScore`).
//!
//! # ID mapping
//!
//! Local rerankers use numeric `u64` IDs via `ScoredDoc`; frankensearch uses
//! string `doc_id`s via `RerankDocument`. This bridge maps between them using
//! decimal string representation (e.g., `42_u64` ↔ `"42"`).
//!
//! # Explainability
//!
//! Provides `RerankExplanation` for per-document score attribution, enabling
//! transparent comparison between pre-rerank and post-rerank orderings.
//!
//! # Feature gate
//!
//! This module requires the `frankensearch` feature.

use frankensearch::{RerankDocument, RerankScore};

use super::reranker::{RerankError, Reranker as LocalReranker, ScoredDoc};
use std::collections::HashMap;

// ── Type Conversions ──────────────────────────────────────────────────

/// Convert a local `ScoredDoc` to a frankensearch `RerankDocument`.
#[must_use]
pub fn scored_doc_to_rerank_document(doc: &ScoredDoc) -> RerankDocument {
    RerankDocument {
        doc_id: doc.id.to_string(),
        text: doc.text.clone(),
    }
}

/// Batch convert local docs to frankensearch documents.
#[must_use]
pub fn scored_docs_to_rerank_documents(docs: &[ScoredDoc]) -> Vec<RerankDocument> {
    docs.iter().map(scored_doc_to_rerank_document).collect()
}

/// Convert frankensearch `RerankScore` results back to local `ScoredDoc` format.
///
/// Looks up original doc text from the originals slice. Returns only IDs
/// that exist in the original set.
#[must_use]
pub fn rerank_scores_to_scored_docs(
    scores: &[RerankScore],
    originals: &[ScoredDoc],
) -> Vec<ScoredDoc> {
    let orig_map: HashMap<String, &ScoredDoc> =
        originals.iter().map(|d| (d.id.to_string(), d)).collect();

    scores
        .iter()
        .filter_map(|s| {
            let orig = orig_map.get(&s.doc_id)?;
            Some(ScoredDoc {
                id: orig.id,
                text: orig.text.clone(),
                score: s.score,
            })
        })
        .collect()
}

/// Parse a string doc_id back to u64.
#[must_use]
pub fn parse_doc_id(doc_id: &str) -> Option<u64> {
    doc_id.parse::<u64>().ok()
}

// ── Explainability ────────────────────────────────────────────────────

/// Per-document explanation of the reranking effect.
#[derive(Debug, Clone)]
pub struct RerankExplanation {
    /// Document ID.
    pub doc_id: u64,
    /// Position in the original (pre-rerank) ordering.
    pub original_rank: usize,
    /// Position in the reranked ordering.
    pub reranked_rank: usize,
    /// Score before reranking.
    pub original_score: f32,
    /// Score after reranking.
    pub rerank_score: f32,
    /// Rank change: positive = promoted, negative = demoted.
    pub rank_delta: i64,
    /// Score difference (rerank_score - original_score).
    pub score_delta: f32,
}

/// Generate explanations comparing pre-rerank and post-rerank orderings.
///
/// Each entry shows how a document's rank and score changed due to reranking.
/// Only documents present in both lists are included.
#[must_use]
pub fn explain_rerank(originals: &[ScoredDoc], reranked: &[ScoredDoc]) -> Vec<RerankExplanation> {
    let orig_rank: HashMap<u64, usize> = originals
        .iter()
        .enumerate()
        .map(|(i, d)| (d.id, i))
        .collect();
    let orig_score: HashMap<u64, f32> = originals.iter().map(|d| (d.id, d.score)).collect();

    reranked
        .iter()
        .enumerate()
        .filter_map(|(new_rank, d)| {
            let old_rank = *orig_rank.get(&d.id)?;
            let old_score = *orig_score.get(&d.id)?;
            Some(RerankExplanation {
                doc_id: d.id,
                original_rank: old_rank,
                reranked_rank: new_rank,
                original_score: old_score,
                rerank_score: d.score,
                rank_delta: old_rank as i64 - new_rank as i64,
                score_delta: d.score - old_score,
            })
        })
        .collect()
}

// ── Bridge Metrics ────────────────────────────────────────────────────

/// Summary metrics for a reranking bridge operation.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct RerankBridgeMetrics {
    /// Number of documents submitted to the reranker.
    pub input_count: usize,
    /// Number of documents returned after reranking.
    pub output_count: usize,
    /// Identifier of the reranker that was used.
    pub reranker_id: String,
    /// Documents whose rank improved (promoted).
    pub promoted_count: usize,
    /// Documents whose rank worsened (demoted).
    pub demoted_count: usize,
    /// Documents whose rank stayed the same.
    pub unchanged_count: usize,
    /// Largest absolute rank change across all documents.
    pub max_rank_change: usize,
    /// Mean score delta (rerank_score - original_score).
    pub mean_score_delta: f32,
}

/// Compute bridge metrics from a set of rerank explanations.
#[must_use]
pub fn compute_bridge_metrics(
    explanations: &[RerankExplanation],
    reranker_id: &str,
    input_count: usize,
) -> RerankBridgeMetrics {
    let mut promoted = 0usize;
    let mut demoted = 0usize;
    let mut unchanged = 0usize;
    let mut max_change = 0usize;
    let mut score_delta_sum = 0.0f32;

    for exp in explanations {
        match exp.rank_delta.cmp(&0) {
            std::cmp::Ordering::Greater => promoted += 1,
            std::cmp::Ordering::Less => demoted += 1,
            std::cmp::Ordering::Equal => unchanged += 1,
        }
        let abs_change = exp.rank_delta.unsigned_abs() as usize;
        if abs_change > max_change {
            max_change = abs_change;
        }
        score_delta_sum += exp.score_delta;
    }

    let mean_delta = if explanations.is_empty() {
        0.0
    } else {
        score_delta_sum / explanations.len() as f32
    };

    RerankBridgeMetrics {
        input_count,
        output_count: explanations.len(),
        reranker_id: reranker_id.to_string(),
        promoted_count: promoted,
        demoted_count: demoted,
        unchanged_count: unchanged,
        max_rank_change: max_change,
        mean_score_delta: mean_delta,
    }
}

// ── Bridge Adapters ───────────────────────────────────────────────────

/// Adapter that wraps a local `Reranker` and converts its I/O to
/// frankensearch types for pipeline integration.
///
/// This enables local reranker implementations to feed results into the
/// frankensearch pipeline during bridge-mode operation.
pub struct LocalToFsRerankerAdapter {
    inner: Box<dyn LocalReranker>,
    id: String,
    model_name: String,
}

impl LocalToFsRerankerAdapter {
    /// Wrap a local `Reranker`.
    pub fn new(inner: Box<dyn LocalReranker>, id: &str, model_name: &str) -> Self {
        Self {
            inner,
            id: id.to_string(),
            model_name: model_name.to_string(),
        }
    }

    /// Get the adapter's ID.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Get the adapter's model name.
    #[must_use]
    pub fn model_name(&self) -> &str {
        &self.model_name
    }

    /// Run the local reranker via bridge types.
    ///
    /// Converts `RerankDocument`s to local `ScoredDoc`s, runs the local reranker,
    /// and converts back to `RerankScore`s.
    pub fn rerank_via_bridge(
        &self,
        query: &str,
        documents: &[RerankDocument],
    ) -> Result<Vec<RerankScore>, RerankError> {
        let local_docs: Vec<ScoredDoc> = documents
            .iter()
            .enumerate()
            .map(|(i, d)| ScoredDoc {
                id: parse_doc_id(&d.doc_id).unwrap_or(i as u64),
                text: d.text.clone(),
                score: 0.0,
            })
            .collect();

        let reranked = self.inner.rerank(query, local_docs)?;

        Ok(reranked
            .iter()
            .enumerate()
            .map(|(rank, doc)| RerankScore {
                doc_id: doc.id.to_string(),
                score: doc.score,
                original_rank: rank,
            })
            .collect())
    }
}

/// Adapter that applies frankensearch `RerankScore` results back into
/// our local `ScoredDoc` pipeline.
///
/// Wraps a set of `RerankScore` results and converts them back to
/// sorted `ScoredDoc` vectors, preserving text from the originals.
pub struct FsToLocalRerankerAdapter;

impl FsToLocalRerankerAdapter {
    /// Apply frankensearch rerank scores to local docs.
    ///
    /// Returns a new `Vec<ScoredDoc>` sorted by rerank score descending,
    /// with text preserved from the original docs.
    #[must_use]
    pub fn apply_scores(scores: &[RerankScore], originals: &[ScoredDoc]) -> Vec<ScoredDoc> {
        let mut result = rerank_scores_to_scored_docs(scores, originals);
        result.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        result
    }
}

// ── Reranker Bridge Config ────────────────────────────────────────────

/// Configuration for the reranker bridge (frankensearch-specific settings).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RerankerBridgeConfig {
    /// Maximum number of candidates to rerank (mirrors frankensearch DEFAULT_TOP_K_RERANK).
    #[serde(default = "default_top_k_rerank")]
    pub top_k_rerank: usize,
    /// Minimum number of candidates required before reranking kicks in.
    #[serde(default = "default_min_candidates")]
    pub min_candidates: usize,
    /// Maximum token length for the reranker model.
    #[serde(default = "default_max_length")]
    pub max_length: usize,
}

fn default_top_k_rerank() -> usize {
    100
}
fn default_min_candidates() -> usize {
    5
}
fn default_max_length() -> usize {
    512
}

impl Default for RerankerBridgeConfig {
    fn default() -> Self {
        Self {
            top_k_rerank: default_top_k_rerank(),
            min_candidates: default_min_candidates(),
            max_length: default_max_length(),
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::reranker::PassthroughReranker;

    fn make_docs(ids_scores: &[(u64, f32)]) -> Vec<ScoredDoc> {
        ids_scores
            .iter()
            .map(|(id, score)| ScoredDoc {
                id: *id,
                text: format!("doc-{id}"),
                score: *score,
            })
            .collect()
    }

    // ── Type Conversion ─────────────────────────────────────────────

    #[test]
    fn scored_doc_to_rerank_document_basic() {
        let doc = ScoredDoc {
            id: 42,
            text: "hello world".into(),
            score: 0.9,
        };
        let rd = scored_doc_to_rerank_document(&doc);
        assert_eq!(rd.doc_id, "42");
        assert_eq!(rd.text, "hello world");
    }

    #[test]
    fn scored_doc_to_rerank_document_large_id() {
        let doc = ScoredDoc {
            id: u64::MAX,
            text: "max".into(),
            score: 0.5,
        };
        let rd = scored_doc_to_rerank_document(&doc);
        assert_eq!(rd.doc_id, u64::MAX.to_string());
    }

    #[test]
    fn scored_doc_to_rerank_document_empty_text() {
        let doc = ScoredDoc {
            id: 1,
            text: String::new(),
            score: 0.0,
        };
        let rd = scored_doc_to_rerank_document(&doc);
        assert!(rd.text.is_empty());
    }

    #[test]
    fn scored_doc_to_rerank_document_unicode() {
        let doc = ScoredDoc {
            id: 1,
            text: "日本語テスト".into(),
            score: 0.5,
        };
        let rd = scored_doc_to_rerank_document(&doc);
        assert_eq!(rd.text, "日本語テスト");
    }

    #[test]
    fn batch_conversion_preserves_order() {
        let docs = make_docs(&[(1, 0.9), (2, 0.8), (3, 0.7)]);
        let rds = scored_docs_to_rerank_documents(&docs);
        assert_eq!(rds.len(), 3);
        assert_eq!(rds[0].doc_id, "1");
        assert_eq!(rds[1].doc_id, "2");
        assert_eq!(rds[2].doc_id, "3");
    }

    #[test]
    fn batch_conversion_empty() {
        let rds = scored_docs_to_rerank_documents(&[]);
        assert!(rds.is_empty());
    }

    #[test]
    fn rerank_scores_to_scored_docs_basic() {
        let originals = make_docs(&[(1, 0.9), (2, 0.8), (3, 0.7)]);
        let scores = vec![
            RerankScore {
                doc_id: "3".into(),
                score: 0.95,
                original_rank: 2,
            },
            RerankScore {
                doc_id: "1".into(),
                score: 0.85,
                original_rank: 0,
            },
        ];
        let result = rerank_scores_to_scored_docs(&scores, &originals);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].id, 3);
        assert!((result[0].score - 0.95).abs() < f32::EPSILON);
        assert_eq!(result[0].text, "doc-3");
        assert_eq!(result[1].id, 1);
    }

    #[test]
    fn rerank_scores_to_scored_docs_missing_id() {
        let originals = make_docs(&[(1, 0.9)]);
        let scores = vec![RerankScore {
            doc_id: "999".into(),
            score: 0.5,
            original_rank: 0,
        }];
        let result = rerank_scores_to_scored_docs(&scores, &originals);
        assert!(result.is_empty());
    }

    #[test]
    fn rerank_scores_to_scored_docs_empty() {
        let originals = make_docs(&[(1, 0.9)]);
        let result = rerank_scores_to_scored_docs(&[], &originals);
        assert!(result.is_empty());
    }

    #[test]
    fn rerank_scores_preserves_text_from_originals() {
        let originals = vec![ScoredDoc {
            id: 42,
            text: "original text content".into(),
            score: 0.5,
        }];
        let scores = vec![RerankScore {
            doc_id: "42".into(),
            score: 0.99,
            original_rank: 0,
        }];
        let result = rerank_scores_to_scored_docs(&scores, &originals);
        assert_eq!(result[0].text, "original text content");
        assert!((result[0].score - 0.99).abs() < f32::EPSILON);
    }

    // ── parse_doc_id ─────────────────────────────────────────────────

    #[test]
    fn parse_doc_id_valid() {
        assert_eq!(parse_doc_id("42"), Some(42));
        assert_eq!(parse_doc_id("0"), Some(0));
        assert_eq!(parse_doc_id(&u64::MAX.to_string()), Some(u64::MAX));
    }

    #[test]
    fn parse_doc_id_invalid() {
        assert_eq!(parse_doc_id("abc"), None);
        assert_eq!(parse_doc_id(""), None);
        assert_eq!(parse_doc_id("-1"), None);
        assert_eq!(parse_doc_id("1.5"), None);
    }

    // ── Explainability ──────────────────────────────────────────────

    #[test]
    fn explain_rerank_promoted() {
        let originals = make_docs(&[(1, 0.5), (2, 0.8), (3, 0.3)]);
        let reranked = make_docs(&[(3, 0.95), (2, 0.85), (1, 0.55)]);
        let exps = explain_rerank(&originals, &reranked);
        assert_eq!(exps.len(), 3);

        let explain_doc3 = exps.iter().find(|e| e.doc_id == 3).unwrap();
        assert_eq!(explain_doc3.original_rank, 2);
        assert_eq!(explain_doc3.reranked_rank, 0);
        assert_eq!(explain_doc3.rank_delta, 2);

        let explain_doc1 = exps.iter().find(|e| e.doc_id == 1).unwrap();
        assert_eq!(explain_doc1.rank_delta, -2);
    }

    #[test]
    fn explain_rerank_unchanged() {
        let docs = make_docs(&[(1, 0.9), (2, 0.8)]);
        let exps = explain_rerank(&docs, &docs);
        for exp in &exps {
            assert_eq!(exp.rank_delta, 0);
        }
    }

    #[test]
    fn explain_rerank_empty() {
        let exps = explain_rerank(&[], &[]);
        assert!(exps.is_empty());
    }

    #[test]
    fn explain_rerank_partial_overlap() {
        let originals = make_docs(&[(1, 0.9), (2, 0.8)]);
        let reranked = make_docs(&[(2, 0.95), (3, 0.7)]);
        let exps = explain_rerank(&originals, &reranked);
        assert_eq!(exps.len(), 1);
        assert_eq!(exps[0].doc_id, 2);
    }

    #[test]
    fn explain_rerank_score_delta() {
        let originals = make_docs(&[(1, 0.5)]);
        let reranked = make_docs(&[(1, 0.8)]);
        let exps = explain_rerank(&originals, &reranked);
        assert!((exps[0].score_delta - 0.3).abs() < 1e-6);
    }

    // ── Bridge Metrics ──────────────────────────────────────────────

    #[test]
    fn compute_metrics_empty() {
        let m = compute_bridge_metrics(&[], "test", 0);
        assert_eq!(m.input_count, 0);
        assert_eq!(m.output_count, 0);
        assert_eq!(m.promoted_count, 0);
        assert_eq!(m.demoted_count, 0);
        assert_eq!(m.unchanged_count, 0);
        assert_eq!(m.max_rank_change, 0);
        assert!((m.mean_score_delta - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn compute_metrics_mixed() {
        let exps = vec![
            RerankExplanation {
                doc_id: 1,
                original_rank: 0,
                reranked_rank: 2,
                original_score: 0.9,
                rerank_score: 0.7,
                rank_delta: -2,
                score_delta: -0.2,
            },
            RerankExplanation {
                doc_id: 2,
                original_rank: 2,
                reranked_rank: 0,
                original_score: 0.7,
                rerank_score: 0.95,
                rank_delta: 2,
                score_delta: 0.25,
            },
            RerankExplanation {
                doc_id: 3,
                original_rank: 1,
                reranked_rank: 1,
                original_score: 0.8,
                rerank_score: 0.8,
                rank_delta: 0,
                score_delta: 0.0,
            },
        ];
        let m = compute_bridge_metrics(&exps, "flashrank", 3);
        assert_eq!(m.input_count, 3);
        assert_eq!(m.output_count, 3);
        assert_eq!(m.reranker_id, "flashrank");
        assert_eq!(m.promoted_count, 1);
        assert_eq!(m.demoted_count, 1);
        assert_eq!(m.unchanged_count, 1);
        assert_eq!(m.max_rank_change, 2);
    }

    #[test]
    fn bridge_metrics_serde_roundtrip() {
        let m = RerankBridgeMetrics {
            input_count: 10,
            output_count: 8,
            reranker_id: "test-reranker".into(),
            promoted_count: 3,
            demoted_count: 2,
            unchanged_count: 3,
            max_rank_change: 5,
            mean_score_delta: 0.05,
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: RerankBridgeMetrics = serde_json::from_str(&json).unwrap();
        assert_eq!(back.input_count, 10);
        assert_eq!(back.output_count, 8);
        assert_eq!(back.reranker_id, "test-reranker");
        assert_eq!(back.promoted_count, 3);
    }

    // ── Bridge Config ───────────────────────────────────────────────

    #[test]
    fn bridge_config_defaults() {
        let cfg = RerankerBridgeConfig::default();
        assert_eq!(cfg.top_k_rerank, 100);
        assert_eq!(cfg.min_candidates, 5);
        assert_eq!(cfg.max_length, 512);
    }

    #[test]
    fn bridge_config_serde_roundtrip() {
        let cfg = RerankerBridgeConfig {
            top_k_rerank: 50,
            min_candidates: 10,
            max_length: 256,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: RerankerBridgeConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.top_k_rerank, 50);
        assert_eq!(back.min_candidates, 10);
        assert_eq!(back.max_length, 256);
    }

    #[test]
    fn bridge_config_serde_absent_uses_defaults() {
        let json = "{}";
        let cfg: RerankerBridgeConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.top_k_rerank, 100);
        assert_eq!(cfg.min_candidates, 5);
        assert_eq!(cfg.max_length, 512);
    }

    // ── LocalToFsRerankerAdapter ────────────────────────────────────

    #[test]
    fn local_to_fs_adapter_basic() {
        let adapter = LocalToFsRerankerAdapter::new(
            Box::new(PassthroughReranker),
            "passthrough",
            "passthrough-v1",
        );
        assert_eq!(adapter.id(), "passthrough");
        assert_eq!(adapter.model_name(), "passthrough-v1");

        let docs = vec![
            RerankDocument {
                doc_id: "1".into(),
                text: "hello".into(),
            },
            RerankDocument {
                doc_id: "2".into(),
                text: "world".into(),
            },
        ];
        let scores = adapter.rerank_via_bridge("test query", &docs).unwrap();
        assert_eq!(scores.len(), 2);
        assert_eq!(scores[0].doc_id, "1");
        assert_eq!(scores[1].doc_id, "2");
    }

    #[test]
    fn local_to_fs_adapter_empty_input() {
        let adapter = LocalToFsRerankerAdapter::new(Box::new(PassthroughReranker), "pt", "pt");
        let result = adapter.rerank_via_bridge("q", &[]);
        assert!(result.is_err());
    }

    #[test]
    fn local_to_fs_adapter_preserves_id_mapping() {
        let adapter = LocalToFsRerankerAdapter::new(Box::new(PassthroughReranker), "pt", "pt");
        let docs = vec![RerankDocument {
            doc_id: "12345".into(),
            text: "test".into(),
        }];
        let scores = adapter.rerank_via_bridge("q", &docs).unwrap();
        assert_eq!(scores[0].doc_id, "12345");
    }

    #[test]
    fn local_to_fs_adapter_non_numeric_id_fallback() {
        let adapter = LocalToFsRerankerAdapter::new(Box::new(PassthroughReranker), "pt", "pt");
        let docs = vec![RerankDocument {
            doc_id: "not-a-number".into(),
            text: "test".into(),
        }];
        let scores = adapter.rerank_via_bridge("q", &docs).unwrap();
        assert_eq!(scores[0].doc_id, "0");
    }

    // ── FsToLocalRerankerAdapter ────────────────────────────────────

    #[test]
    fn fs_to_local_apply_scores_basic() {
        let originals = make_docs(&[(1, 0.9), (2, 0.8), (3, 0.7)]);
        let scores = vec![
            RerankScore {
                doc_id: "3".into(),
                score: 0.99,
                original_rank: 2,
            },
            RerankScore {
                doc_id: "1".into(),
                score: 0.5,
                original_rank: 0,
            },
            RerankScore {
                doc_id: "2".into(),
                score: 0.7,
                original_rank: 1,
            },
        ];
        let result = FsToLocalRerankerAdapter::apply_scores(&scores, &originals);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].id, 3);
        assert!((result[0].score - 0.99).abs() < f32::EPSILON);
        assert_eq!(result[1].id, 2);
        assert_eq!(result[2].id, 1);
    }

    #[test]
    fn fs_to_local_apply_scores_empty() {
        let originals = make_docs(&[(1, 0.9)]);
        let result = FsToLocalRerankerAdapter::apply_scores(&[], &originals);
        assert!(result.is_empty());
    }

    #[test]
    fn fs_to_local_apply_scores_preserves_text() {
        let originals = vec![ScoredDoc {
            id: 42,
            text: "preserved text".into(),
            score: 0.5,
        }];
        let scores = vec![RerankScore {
            doc_id: "42".into(),
            score: 0.9,
            original_rank: 0,
        }];
        let result = FsToLocalRerankerAdapter::apply_scores(&scores, &originals);
        assert_eq!(result[0].text, "preserved text");
    }

    // ── End-to-end roundtrip ────────────────────────────────────────

    #[test]
    fn roundtrip_local_to_fs_and_back() {
        let original_docs = make_docs(&[(10, 0.9), (20, 0.8), (30, 0.7)]);

        let fs_docs = scored_docs_to_rerank_documents(&original_docs);
        assert_eq!(fs_docs.len(), 3);
        assert_eq!(fs_docs[0].doc_id, "10");

        let scores = vec![
            RerankScore {
                doc_id: "30".into(),
                score: 0.99,
                original_rank: 2,
            },
            RerankScore {
                doc_id: "10".into(),
                score: 0.88,
                original_rank: 0,
            },
            RerankScore {
                doc_id: "20".into(),
                score: 0.77,
                original_rank: 1,
            },
        ];

        let reranked = FsToLocalRerankerAdapter::apply_scores(&scores, &original_docs);
        assert_eq!(reranked.len(), 3);
        assert_eq!(reranked[0].id, 30);
        assert_eq!(reranked[0].text, "doc-30");
        assert!((reranked[0].score - 0.99).abs() < f32::EPSILON);

        let exps = explain_rerank(&original_docs, &reranked);
        assert_eq!(exps.len(), 3);

        let exp30 = exps.iter().find(|e| e.doc_id == 30).unwrap();
        assert_eq!(exp30.rank_delta, 2);

        let metrics = compute_bridge_metrics(&exps, "flashrank-v1", 3);
        assert_eq!(metrics.input_count, 3);
        assert_eq!(metrics.output_count, 3);
        assert_eq!(metrics.promoted_count, 1); // doc 30: rank 2→0
        assert_eq!(metrics.demoted_count, 2); // doc 10: rank 0→1, doc 20: rank 1→2
        assert_eq!(metrics.unchanged_count, 0);
    }

    #[test]
    fn roundtrip_via_adapter() {
        let adapter = LocalToFsRerankerAdapter::new(
            Box::new(PassthroughReranker),
            "pt-bridge",
            "passthrough-bridge",
        );

        let fs_docs = vec![
            RerankDocument {
                doc_id: "1".into(),
                text: "hello".into(),
            },
            RerankDocument {
                doc_id: "2".into(),
                text: "world".into(),
            },
        ];
        let scores = adapter.rerank_via_bridge("query", &fs_docs).unwrap();
        assert_eq!(scores.len(), 2);

        let originals = make_docs(&[(1, 0.9), (2, 0.8)]);
        let reranked = FsToLocalRerankerAdapter::apply_scores(&scores, &originals);
        assert_eq!(reranked.len(), 2);
    }

    // ── Determinism ─────────────────────────────────────────────────

    #[test]
    fn conversion_deterministic() {
        let docs = make_docs(&[(1, 0.9), (2, 0.8), (3, 0.7)]);
        let r1 = scored_docs_to_rerank_documents(&docs);
        let r2 = scored_docs_to_rerank_documents(&docs);
        for (a, b) in r1.iter().zip(r2.iter()) {
            assert_eq!(a.doc_id, b.doc_id);
            assert_eq!(a.text, b.text);
        }
    }

    #[test]
    fn explain_deterministic() {
        let originals = make_docs(&[(1, 0.9), (2, 0.8)]);
        let reranked = make_docs(&[(2, 0.95), (1, 0.85)]);
        let e1 = explain_rerank(&originals, &reranked);
        let e2 = explain_rerank(&originals, &reranked);
        assert_eq!(e1.len(), e2.len());
        for (a, b) in e1.iter().zip(e2.iter()) {
            assert_eq!(a.doc_id, b.doc_id);
            assert_eq!(a.rank_delta, b.rank_delta);
        }
    }
}
