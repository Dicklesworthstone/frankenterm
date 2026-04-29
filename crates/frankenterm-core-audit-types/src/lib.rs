//! `frankenterm-core-audit-types` — audit / forensic / evidence DTOs leaf
//! crate (ft-rqu5e / ft-8nqx0 Phase 1).
//!
//! Phase 1 of the staged extraction proposed in
//! `docs/proposals/ft-8nqx0-audit-operational-boundary.md`. This crate
//! holds the cleanest, zero-`crate::*`-dep DTO clusters identified in
//! that audit so portable evidence types stop living next to operational
//! runtime, recorder, policy, storage, and crash modules.
//!
//! Modules in this crate (current beachhead set):
//!
//! - [`forensic_export`] — canonical forensic records and the
//!   query/export model used for compliance reconstruction.
//! - [`traceability_verification`] — static traceability matrix
//!   verification types and gap/coverage analytics.
//!
//! Both modules are leaf-clean (only `std` + `serde` imports) per the
//! ft-8nqx0 boundary scan, which is what makes them safe to lift in
//! Phase 1 without a parallel API redesign in their callers.
//!
//! ## What stays in `frankenterm-core` (for now)
//!
//! Phase 2+ of the proposal handles the audit/forensics modules that
//! still carry cross-cluster `crate::*` deps:
//!
//! - `recorder_audit` (depends on policy, recorder_storage, tuning_config)
//! - `policy_decision_log` (depends on policy + policy_dsl)
//! - `recorder_retention`, `session_retention` (mixed config + SQL)
//! - `resize_crash_forensics`, `migration_rehearsal`, `cutover_evidence`,
//!   `canary_rehearsal`, `bayesian_ledger`, `reports`
//!
//! Each gets its own follow-up bead (ft-kldww / ft-mq7fl / ft-xcsm0 /
//! ft-4ses2 / ft-nsoxc) per the proposal.
//!
//! ## Re-export contract
//!
//! `frankenterm-core` re-exports both modules via `pub use` so the
//! public surface (`frankenterm_core::forensic_export::*`,
//! `frankenterm_core::traceability_verification::*`) is preserved
//! byte-for-byte — same call sites, same proptests, no API churn.

pub mod forensic_export;
pub mod policy_decision_log_engine;
pub mod recorder_audit_engine;
pub mod traceability_verification;
