//! 2-Tier Semantic Search for FrankenTerm
//!
//! Progressive search system combining lexical (BM25) and semantic (embedding)
//! retrieval with Reciprocal Rank Fusion and two-tier blending.

mod chunk_vector_store;
mod chunking;
mod embedder;
mod hash_embedder;
mod hybrid_search;
mod reranker;
mod vector_index;

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
pub use embedder::{EmbedError, Embedder, EmbedderInfo, EmbedderTier};
pub use hash_embedder::HashEmbedder;
pub use hybrid_search::{
    FusedResult, HybridSearchService, SearchMode, TwoTierMetrics, blend_two_tier, kendall_tau,
    rrf_fuse,
};
pub use reranker::{RerankError, Reranker};
pub use vector_index::{FtviIndex, FtviRecord, FtviWriter, write_ftvi_vec};

#[cfg(feature = "semantic-search")]
pub use fastembed_embedder::FastEmbedEmbedder;
#[cfg(feature = "semantic-search")]
pub use model_registry::{ModelInfo, ModelRegistry};
#[cfg(feature = "semantic-search")]
pub use model2vec_embedder::Model2VecEmbedder;
#[cfg(feature = "semantic-search")]
pub use reranker::CrossEncoderReranker;
