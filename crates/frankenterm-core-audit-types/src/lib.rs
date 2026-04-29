//! `frankenterm-core-audit-types` — audit / forensic / evidence DTOs leaf
//! crate (ft-rqu5e + ft-mq7fl / ft-8nqx0 Phases 1 + 3).
//!
//! Staged extraction proposed in
//! `docs/proposals/ft-8nqx0-audit-operational-boundary.md`. This crate
//! holds the cleanest, zero-`crate::*`-dep DTO clusters identified in
//! that audit so portable evidence types stop living next to operational
//! runtime, recorder, policy, storage, and crash modules.
//!
//! ## Modules
//!
//! Phase 1 (ft-rqu5e):
//! - [`forensic_export`] — canonical forensic records + query/export
//!   model used for compliance reconstruction.
//! - [`traceability_verification`] — static traceability matrix
//!   verification types and gap/coverage analytics.
//! - [`policy_decision_log_engine`] — generic bounded decision-log
//!   mechanics (pre-seed for ft-kldww Phase 2).
//! - [`recorder_audit_engine`] — generic recorder-audit hash-chain
//!   engine (pre-seed for ft-kldww Phase 2).
//!
//! Phase 3 (ft-mq7fl):
//! - [`migration_rehearsal`] — migration rehearsal scenarios,
//!   execution, drill metrics, reports.
//! - [`cutover_evidence`] — go/no-go evidence package, prerequisites,
//!   regression gates, risk registry, soak outcomes.
//! - [`canary_rehearsal`] — canary rollout rehearsal + fail-safe drill
//!   modeling.
//!
//! All modules above are leaf-clean (only `std` + `serde` (+ `serde_json`
//! in `forensic_export`) imports) per the ft-8nqx0 boundary scan, which
//! is what makes them safe to lift without a parallel API redesign in
//! their callers.
//!
//! ## What stays in `frankenterm-core` (for now)
//!
//! Phase 2+ of the proposal handles the audit/forensics modules that
//! still carry cross-cluster `crate::*` deps:
//!
//! - `recorder_audit` (depends on policy, recorder_storage, tuning_config)
//! - `policy_decision_log` (depends on policy + policy_dsl —
//!   the engine half lives here in [`policy_decision_log_engine`])
//! - `recorder_retention`, `session_retention` (mixed config + SQL)
//! - `resize_crash_forensics` (depends on `resize_scheduler`; DTOs only
//!   should move once a scheduler-snapshot adapter exists)
//! - `bayesian_ledger`, `reports` (cross-cluster runtime/storage deps)
//!
//! Each gets its own follow-up bead (ft-kldww / ft-xcsm0 / ft-4ses2 /
//! ft-nsoxc) per the proposal.
//!
//! ## Re-export contract
//!
//! `frankenterm-core` re-exports each module via `pub use` so the
//! public surface (`frankenterm_core::forensic_export::*`,
//! `frankenterm_core::migration_rehearsal::*`, etc.) is preserved
//! byte-for-byte — same call sites, same proptests, no API churn.

pub mod canary_rehearsal;
pub mod cutover_evidence;
pub mod forensic_export;
pub mod migration_rehearsal;
pub mod policy_decision_log_engine;
pub mod recorder_audit_engine;
pub mod traceability_verification;
