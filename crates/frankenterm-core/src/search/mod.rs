//! 2-Tier Semantic Search for FrankenTerm
//!
//! Progressive search system combining lexical (BM25) and semantic (embedding)
//! retrieval with Reciprocal Rank Fusion and two-tier blending.

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
mod model2vec_embedder;
#[cfg(feature = "semantic-search")]
mod model_registry;

#[cfg(feature = "semantic-search")]
pub mod daemon;

pub use chunk_vector_store::{
    ChunkEmbeddingUpsert, ChunkEmbeddingUpsertOutcome, ChunkVectorDriftReport, ChunkVectorHit,
    ChunkVectorStore, ChunkVectorStoreError, SemanticGeneration, SemanticGenerationStatus,
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
    try_init_fastembed,
};
#[cfg(feature = "semantic-search")]
pub use model_registry::{ModelInfo, ModelRegistry};
#[cfg(feature = "semantic-search")]
pub use model2vec_embedder::Model2VecEmbedder;
#[cfg(feature = "semantic-search")]
pub use reranker::CrossEncoderReranker;

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
