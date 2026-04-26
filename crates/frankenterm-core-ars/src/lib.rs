//! `frankenterm-core-ars` — Automated Reasoning System (ARS) sub-crate
//! extracted from `frankenterm-core` under ft-y0loj.2 / ft-mr35k.
//!
//! 15 modules carved out as a Tier-1 leaf extraction (no feature gate;
//! ARS was always-on in the parent crate). The cluster captures
//! frankenterm's automated-reasoning pipeline:
//!
//! - [`ars_blast_radius`]    — maturity-tier policy + deny-reason taxonomy
//! - [`ars_compile`]         — descriptor → reflex compilation
//! - [`ars_drift`]           — drift detection + e-value reconciliation
//! - [`ars_evidence`]        — append-only evidence ledger + verdicts
//! - [`ars_evolve`]          — version status + maturity evolution
//! - [`ars_explain`]         — human-readable trace generation
//! - [`ars_federation`]      — cross-org reflex federation
//! - [`ars_fst`]             — finite-state-transducer matcher
//! - [`ars_generalize`]      — command-template generalization
//! - [`ars_intercept`]       — workflow-step interception hooks
//! - [`ars_replay`]          — replay-assessment harness
//! - [`ars_secret_scan`]     — credential/secret detection
//! - [`ars_serialize`]       — reflex-record persistence
//! - [`ars_symbolic_exec`]   — symbolic execution of command flows
//! - [`ars_timeout`]         — timeout-decision policy
//!
//! ## Dependency direction
//!
//! This crate depends on `frankenterm-core` for `mdl_extraction`,
//! `token_bucket`, and `workflows` types it consumes. The reverse
//! direction (core → ars) is severed; the parent's `lib.rs` no longer
//! declares `pub mod ars_*`. Cargo cycle-free in the regular dep
//! graph.
//!
//! Tests in `frankenterm-core/tests/proptest_ars_*.rs` import from this
//! crate via a dev-dep cycle, which Cargo allows.

pub mod ars_blast_radius;
pub mod ars_compile;
pub mod ars_drift;
pub mod ars_evidence;
pub mod ars_evolve;
pub mod ars_explain;
pub mod ars_federation;
pub mod ars_fst;
pub mod ars_generalize;
pub mod ars_intercept;
pub mod ars_replay;
pub mod ars_secret_scan;
pub mod ars_serialize;
pub mod ars_symbolic_exec;
pub mod ars_timeout;
