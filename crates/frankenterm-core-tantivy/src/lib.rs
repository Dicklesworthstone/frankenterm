//! `frankenterm-core-tantivy` — Tantivy-backed lexical search sub-crate
//! extracted from `frankenterm-core` under ft-y0loj.1.
//!
//! Modules in this crate (all unconditionally compiled — the `recorder-lexical`
//! feature flag that gated them in the parent crate is encoded by the
//! crate-membership decision instead: consumers that don't need lexical
//! search simply don't depend on this crate):
//!
//! - [`tantivy_ingest`]   — Tantivy index writer adapter + ingestion pipeline
//! - [`tantivy_policy`]   — query policy / filter rules
//! - [`tantivy_quality`]  — ranking-quality harness
//! - [`tantivy_query`]    — search service + snippet extraction
//! - [`tantivy_reindex`]  — index rebuild orchestration
//! - [`recorder_lexical_ingest`] — `LexicalIndexer` wiring the recorder
//!   ingestion pipeline into the Tantivy stack
//! - [`recorder_lexical_schema`] — schema fingerprint + tokenizer registration
//!
//! ## Dependency direction
//!
//! This crate depends on `frankenterm-core` for the `recorder_storage` and
//! `recording` types it consumes. The reverse direction (core → tantivy)
//! is intentionally severed; the parent crate's `lib.rs` no longer
//! declares `pub mod tantivy_*`. Cargo cycle-free by construction.

pub mod recorder_lexical_ingest;
pub mod recorder_lexical_schema;
pub mod tantivy_ingest;
pub mod tantivy_policy;
pub mod tantivy_quality;
pub mod tantivy_query;
pub mod tantivy_reindex;
