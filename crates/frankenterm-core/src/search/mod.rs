//! 2-Tier Semantic Search for FrankenTerm
//!
//! Progressive search system combining lexical (BM25) and semantic (embedding)
//! retrieval with Reciprocal Rank Fusion and two-tier blending.

use serde::{Deserialize, Serialize};

mod chunk_vector_store;
mod chunking;
pub mod chunking_adapter;
#[cfg(feature = "frankensearch")]
pub mod daemon_bridge;
mod embedder;
pub mod facade;
mod hash_embedder;
mod hybrid_search;
mod indexing;
pub mod indexing_pipeline;
pub mod lexical_backend_bridge;
pub mod migration_controller;
pub mod orchestrator;
pub mod regression_diff;
mod reranker;
#[cfg(feature = "frankensearch")]
pub mod reranker_bridge;
pub mod schema_gate;
mod vector_index;
#[cfg(feature = "frankensearch")]
pub mod vector_index_bridge;

#[cfg(feature = "semantic-search")]
mod fastembed_embedder;
#[cfg(feature = "semantic-search")]
mod model_registry;

#[cfg(feature = "semantic-search")]
pub mod daemon;

pub use chunk_vector_store::{
    ChunkEmbeddingUpsert, ChunkEmbeddingUpsertOutcome, ChunkVectorDriftReport, ChunkVectorHit,
    ChunkVectorStore, ChunkVectorStoreError, SemanticEmbedderIdentity, SemanticGeneration,
    SemanticGenerationStatus,
};
pub use chunking::{
    ChunkDirection, ChunkInputEvent, ChunkOverlap, ChunkPolicyConfig, ChunkSourceOffset,
    RECORDER_CHUNKING_POLICY_V1, SemanticChunk, build_semantic_chunks,
};
pub use chunking_adapter::{
    ChunkAdapterStats, ChunkDocument, batch_stats, chunk_to_document, chunks_to_documents,
    document_to_partial_chunk, extract_direction, extract_end_offset, extract_event_ids,
    extract_overlap, extract_pane_id, extract_policy_version, extract_session_id,
    extract_start_offset, terminal_metadata_count,
};
pub use embedder::{EmbedError, Embedder, EmbedderInfo, EmbedderTier};
pub use hash_embedder::HashEmbedder;
pub use hybrid_search::{
    FusedResult, FusionBackend, HybridSearchService, SearchMode, TwoTierMetrics, blend_two_tier,
    kendall_tau, rrf_fuse,
};
pub use indexing::{
    CassContentHashProvider, CommandBlockExtractionConfig, IndexFlushReason, IndexableDocument,
    IndexedDocument, IndexingConfig, IndexingIngestReport, IndexingTickResult, ScrollbackLine,
    SearchDocumentSource, SearchIndex, SearchIndexError, SearchIndexStats, chunk_scrollback_lines,
    extract_agent_artifacts, extract_command_output_blocks,
};
pub use indexing_pipeline::{
    ContentIndexingPipeline, PaneWatermark, PipelineConfig, PipelineSkipReason, PipelineState,
    PipelineStatus, PipelineTickReport,
};
#[cfg(feature = "frankensearch")]
pub use reranker::{FrankenSearchRerankAdapter, apply_frankensearch_rerank_scores};
pub use reranker::{
    PassthroughReranker, RerankBackend, RerankConfig, RerankError, RerankOutcome, Reranker,
    ScoredDoc, rerank_fused_results,
};
pub use vector_index::{FtviIndex, FtviRecord, FtviWriter, write_ftvi_vec};

pub use lexical_backend_bridge::{
    BridgeDocument, DocumentSource, IndexingMeta, IngestLifecyclePolicy, LexicalBackendConfig,
    LexicalBackendExplanation, LexicalBackendMetrics, LexicalSchemaVersion,
    bridge_doc_to_indexing_meta, compute_churn_rate, compute_query_error_rate,
    compute_rejection_rate, explain_lexical_backend,
};

#[cfg(feature = "frankensearch")]
pub use daemon_bridge::{
    BatchEmbedRequest, BatchEmbedResult, DaemonBridgeConfig, DaemonBridgeExplanation,
    DaemonBridgeMetrics, EmbedPriority, SingleEmbedEntry, SingleEmbedResult,
    compute_batch_utilization, compute_cache_hit_rate, compute_priority_skew, entries_to_texts,
    explain_bridge, from_coalescer_config, from_coalescer_metrics, from_fs_priority,
    to_coalescer_config, to_fs_priority, vectors_to_results,
};

#[cfg(feature = "frankensearch")]
pub use reranker_bridge::{
    FsToLocalRerankerAdapter, LocalToFsRerankerAdapter, RerankBridgeMetrics, RerankExplanation,
    RerankerBridgeConfig, compute_bridge_metrics, explain_rerank, parse_doc_id,
    rerank_scores_to_scored_docs, scored_doc_to_rerank_document, scored_docs_to_rerank_documents,
};

#[cfg(feature = "semantic-search")]
pub use fastembed_embedder::{
    FastEmbedConfig, FastEmbedEmbedder, FastEmbedInitResult, best_available_embedder,
    resolve_fastembed_model_selector, supported_fastembed_model_selectors, try_init_fastembed,
};
#[cfg(feature = "semantic-search")]
pub use model_registry::{ModelInfo, ModelRegistry};
#[cfg(feature = "semantic-search")]
pub use reranker::CrossEncoderReranker;

/// Report the embedder backends that this build can truthfully advertise.
#[must_use]
pub fn advertised_embedder_tiers(search_enabled: bool, reranker_enabled: bool) -> Vec<String> {
    let _ = (search_enabled, reranker_enabled);
    #[allow(unused_mut)] // mut needed when semantic-search feature is enabled
    let mut tiers = vec!["hash".to_string()];

    #[cfg(feature = "semantic-search")]
    if search_enabled {
        tiers.push("fastembed".to_string());
        if reranker_enabled {
            tiers.push("cross-encoder".to_string());
        }
    }

    tiers
}

/// Freshness classification for a semantic-search proof lane observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticSearchFreshnessStatus {
    /// The latest segment was indexed by the semantic lane and is inside the freshness window.
    Fresh,
    /// The latest segment is missing from the semantic index or outside the freshness window.
    Stale,
    /// No latest segment timestamp was available to classify freshness.
    Unknown,
    /// The latest segment timestamp is ahead of the proof observation timestamp.
    FutureDated,
}

/// Classify whether semantic-search evidence can truthfully claim freshness.
#[must_use]
pub fn classify_semantic_search_freshness(
    latest_segment_indexed: Option<bool>,
    latest_segment_captured_at_ms: Option<i64>,
    observed_at_ms: i64,
    freshness_window_ms: i64,
) -> SemanticSearchFreshnessStatus {
    if latest_segment_indexed == Some(false) {
        return SemanticSearchFreshnessStatus::Stale;
    }

    let Some(captured_at_ms) = latest_segment_captured_at_ms else {
        return SemanticSearchFreshnessStatus::Unknown;
    };

    if captured_at_ms > observed_at_ms {
        return SemanticSearchFreshnessStatus::FutureDated;
    }

    if observed_at_ms.saturating_sub(captured_at_ms) <= freshness_window_ms.max(0) {
        SemanticSearchFreshnessStatus::Fresh
    } else {
        SemanticSearchFreshnessStatus::Stale
    }
}

/// Input used to build a semantic-search proof case with computed freshness fields.
#[derive(Debug, Clone)]
pub struct SemanticSearchProofCaseInput {
    /// Stable case identifier within the proof report.
    pub case_id: String,
    /// Search mode requested by the caller.
    pub requested_mode: String,
    /// Search mode that actually produced results.
    pub effective_mode: String,
    /// Embedder identity used for the semantic query lane.
    pub embedder_id: String,
    /// Embedder tier or backend family used for the semantic query lane.
    pub embedder_tier: String,
    /// Query embedding dimension.
    pub embedder_dimension: usize,
    /// Latest segment id known to the proof fixture, if any.
    pub latest_segment_id: Option<i64>,
    /// Capture timestamp for the latest segment, if known.
    pub latest_segment_captured_at_ms: Option<i64>,
    /// Segment ids with embeddings for this embedder at query time.
    pub embedded_segment_ids: Vec<i64>,
    /// Segment ids returned by the search call.
    pub result_segment_ids: Vec<i64>,
    /// Timestamp when the proof observation was recorded.
    pub observed_at_ms: i64,
    /// Maximum allowed age before evidence is stale.
    pub freshness_window_ms: i64,
    /// Semantic fallback reason reported by the search bundle.
    pub fallback_reason: Option<String>,
    /// Semantic budget state reported by the search bundle.
    pub semantic_budget_state: String,
    /// Whether the semantic lane served this query from cache.
    pub semantic_cache_hit: bool,
    /// Number of semantic rows scanned by this query.
    pub semantic_rows_scanned: usize,
}

/// Machine-readable proof case for semantic-search freshness and provenance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticSearchProofCase {
    /// Stable case identifier within the proof report.
    pub case_id: String,
    /// Search mode requested by the caller.
    pub requested_mode: String,
    /// Search mode that actually produced results.
    pub effective_mode: String,
    /// Embedder identity used for the semantic query lane.
    pub embedder_id: String,
    /// Embedder tier or backend family used for the semantic query lane.
    pub embedder_tier: String,
    /// Query embedding dimension.
    pub embedder_dimension: usize,
    /// Latest segment id known to the proof fixture, if any.
    pub latest_segment_id: Option<i64>,
    /// Capture timestamp for the latest segment, if known.
    pub latest_segment_captured_at_ms: Option<i64>,
    /// Segment ids with embeddings for this embedder at query time.
    pub embedded_segment_ids: Vec<i64>,
    /// Segment ids returned by the search call.
    pub result_segment_ids: Vec<i64>,
    /// Timestamp when the proof observation was recorded.
    pub observed_at_ms: i64,
    /// Maximum allowed age before evidence is stale.
    pub freshness_window_ms: i64,
    /// Latest-segment age at observation time.
    pub freshness_age_ms: Option<i64>,
    /// Freshness classification for this proof case.
    pub freshness_status: SemanticSearchFreshnessStatus,
    /// Semantic fallback reason reported by the search bundle.
    pub fallback_reason: Option<String>,
    /// Semantic budget state reported by the search bundle.
    pub semantic_budget_state: String,
    /// Whether the semantic lane served this query from cache.
    pub semantic_cache_hit: bool,
    /// Number of semantic rows scanned by this query.
    pub semantic_rows_scanned: usize,
    /// Whether the latest segment lacks an embedding for this embedder.
    pub stale_index: bool,
}

impl SemanticSearchProofCase {
    /// Build a proof case and compute freshness from the latest segment metadata.
    #[must_use]
    pub fn from_input(input: SemanticSearchProofCaseInput) -> Self {
        let latest_segment_indexed = input
            .latest_segment_id
            .map(|id| input.embedded_segment_ids.contains(&id));
        let freshness_status = classify_semantic_search_freshness(
            latest_segment_indexed,
            input.latest_segment_captured_at_ms,
            input.observed_at_ms,
            input.freshness_window_ms,
        );
        let freshness_age_ms = input
            .latest_segment_captured_at_ms
            .and_then(|captured_at_ms| {
                (input.observed_at_ms >= captured_at_ms)
                    .then_some(input.observed_at_ms.saturating_sub(captured_at_ms))
            });

        Self {
            case_id: input.case_id,
            requested_mode: input.requested_mode,
            effective_mode: input.effective_mode,
            embedder_id: input.embedder_id,
            embedder_tier: input.embedder_tier,
            embedder_dimension: input.embedder_dimension,
            latest_segment_id: input.latest_segment_id,
            latest_segment_captured_at_ms: input.latest_segment_captured_at_ms,
            embedded_segment_ids: input.embedded_segment_ids,
            result_segment_ids: input.result_segment_ids,
            observed_at_ms: input.observed_at_ms,
            freshness_window_ms: input.freshness_window_ms,
            freshness_age_ms,
            freshness_status,
            fallback_reason: input.fallback_reason,
            semantic_budget_state: input.semantic_budget_state,
            semantic_cache_hit: input.semantic_cache_hit,
            semantic_rows_scanned: input.semantic_rows_scanned,
            stale_index: latest_segment_indexed == Some(false),
        }
    }
}

/// Machine-readable semantic-search proof lane report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticSearchProofReport {
    /// Stable proof identifier.
    pub proof_id: String,
    /// Report generation timestamp.
    pub generated_at_ms: i64,
    /// Individual proof cases.
    pub cases: Vec<SemanticSearchProofCase>,
}

impl SemanticSearchProofReport {
    /// True when the report covers lexical, semantic, hybrid, disabled, and stale-index states.
    #[must_use]
    pub fn distinguishes_required_states(&self) -> bool {
        let has_lexical = self
            .cases
            .iter()
            .any(|case| case.requested_mode == "lexical" && case.effective_mode == "lexical");
        let has_semantic = self
            .cases
            .iter()
            .any(|case| case.requested_mode == "semantic" && case.effective_mode == "semantic");
        let has_hybrid = self
            .cases
            .iter()
            .any(|case| case.requested_mode == "hybrid" && case.effective_mode == "hybrid");
        let has_disabled = self
            .cases
            .iter()
            .any(|case| case.semantic_budget_state == "disabled");
        let has_stale_index = self.cases.iter().any(|case| {
            case.stale_index && case.freshness_status == SemanticSearchFreshnessStatus::Stale
        });
        let has_provenance = self.cases.iter().all(|case| {
            !case.embedder_id.is_empty()
                && !case.embedder_tier.is_empty()
                && case.embedder_dimension > 0
        });

        has_lexical
            && has_semantic
            && has_hybrid
            && has_disabled
            && has_stale_index
            && has_provenance
            && !self.contains_false_fresh_stale_claim()
    }

    /// True when a stale semantic index is mislabeled as fresh.
    #[must_use]
    pub fn contains_false_fresh_stale_claim(&self) -> bool {
        self.cases.iter().any(|case| {
            case.stale_index && case.freshness_status == SemanticSearchFreshnessStatus::Fresh
        })
    }
}

pub use facade::{FacadeConfig, FacadeResult, FacadeRouting, SearchFacade, ShadowComparison};
pub use migration_controller::{
    HealthCheckResult, MigrationController, MigrationControllerConfig, MigrationPhase,
    PhaseTransitionError, RetirementGateResult, run_default_retirement_gate,
};
pub use regression_diff::{
    DiffArtifact, RegressionDiffReport, RegressionScenario, ReplayGateConfig, ReplayGateVerdict,
    ScenarioOutcome, default_scenarios, run_regression_suite, run_replay_gate,
    run_replay_gate_default,
};
pub use schema_gate::{
    SchemaField, SchemaGateResult, SchemaSnapshot, SchemaTypeMismatch, check_schema_preservation,
    gate_fusion_schema, gate_orchestration_schema, snapshot_bridge_document_schema,
    snapshot_facade_result_schema, snapshot_fused_result_schema,
    snapshot_orchestration_metrics_schema, snapshot_orchestration_result_schema,
};

#[cfg(test)]
mod tests {
    use super::{
        SemanticSearchFreshnessStatus, SemanticSearchProofCase, SemanticSearchProofCaseInput,
        SemanticSearchProofReport, advertised_embedder_tiers, classify_semantic_search_freshness,
    };

    #[test]
    fn advertised_embedder_tiers_without_semantic_search_only_reports_hash() {
        let tiers = advertised_embedder_tiers(false, true);
        assert_eq!(tiers, vec!["hash".to_string()]);
    }

    #[test]
    fn advertised_embedder_tiers_never_reports_retired_model2vec_backend() {
        let tiers = advertised_embedder_tiers(true, true);
        assert!(!tiers.iter().any(|tier| tier == "model2vec"));
    }

    #[test]
    fn semantic_search_freshness_refuses_unindexed_latest_segment() {
        assert_eq!(
            classify_semantic_search_freshness(Some(false), Some(100), 100, 1_000),
            SemanticSearchFreshnessStatus::Stale
        );
    }

    #[test]
    fn semantic_search_proof_report_detects_required_states() {
        fn case(
            case_id: &str,
            requested_mode: &str,
            effective_mode: &str,
        ) -> SemanticSearchProofCase {
            SemanticSearchProofCase::from_input(SemanticSearchProofCaseInput {
                case_id: case_id.to_string(),
                requested_mode: requested_mode.to_string(),
                effective_mode: effective_mode.to_string(),
                embedder_id: "hash:test".to_string(),
                embedder_tier: "hash".to_string(),
                embedder_dimension: 2,
                latest_segment_id: Some(7),
                latest_segment_captured_at_ms: Some(1_000),
                embedded_segment_ids: vec![7],
                result_segment_ids: vec![7],
                observed_at_ms: 1_001,
                freshness_window_ms: 100,
                fallback_reason: None,
                semantic_budget_state: if requested_mode == "lexical" {
                    "disabled".to_string()
                } else {
                    "active".to_string()
                },
                semantic_cache_hit: false,
                semantic_rows_scanned: 1,
            })
        }

        let stale = SemanticSearchProofCase::from_input(SemanticSearchProofCaseInput {
            case_id: "stale".to_string(),
            requested_mode: "hybrid".to_string(),
            effective_mode: "lexical".to_string(),
            embedder_id: "hash:test".to_string(),
            embedder_tier: "hash".to_string(),
            embedder_dimension: 2,
            latest_segment_id: Some(8),
            latest_segment_captured_at_ms: Some(1_002),
            embedded_segment_ids: vec![7],
            result_segment_ids: vec![8],
            observed_at_ms: 1_003,
            freshness_window_ms: 100,
            fallback_reason: Some("semantic_no_hits".to_string()),
            semantic_budget_state: "active".to_string(),
            semantic_cache_hit: false,
            semantic_rows_scanned: 1,
        });

        let report = SemanticSearchProofReport {
            proof_id: "semantic-search-proof.v1".to_string(),
            generated_at_ms: 1_004,
            cases: vec![
                case("lexical", "lexical", "lexical"),
                case("semantic", "semantic", "semantic"),
                case("hybrid", "hybrid", "hybrid"),
                stale,
            ],
        };

        assert!(report.distinguishes_required_states());
    }
}
