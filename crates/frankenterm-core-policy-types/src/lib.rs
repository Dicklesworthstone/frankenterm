//! `frankenterm-core-policy-types` — policy audit / compliance / metrics / quarantine types leaf crate (ft-0pykm / ft-t2d70.3).
//!
//! Tier-1 leaf crate carved out from `frankenterm-core`. Holds four leaf-clean
//! policy submodules (4296 LOC total) that have zero `crate::*` deps:
//!
//! - [`policy_audit_chain`] — SHA-256 hash-linked immutable audit trail
//!   (ft-3681t.6.4/.6.5 precursor).
//! - [`policy_compliance`] — compliance assessment + reporting types.
//! - [`policy_metrics`] — unified metrics aggregation + health dashboard
//!   (ft-3681t.7.1/.7.2 precursor).
//! - [`policy_quarantine`] — quarantine state + emergency kill-switch
//!   semantics (ft-3681t.6.3 precursor).
//!
//! Re-exported from `frankenterm-core` so existing `crate::policy_*::*` and
//! `frankenterm_core::policy_*::*` paths keep resolving unchanged.
//!
//! ## What's NOT here
//!
//! `policy.rs` (the main rule-evaluation engine), `policy_dsl`,
//! `policy_diagnostics`, and `policy_decision_log` all carry cross-cluster
//! `crate::*` deps (connector telemetry composition, runtime hooks, etc.)
//! and so are not leaf-clean. They stay in `frankenterm-core` for this pass.

pub mod policy_audit_chain;
pub mod policy_compliance;
pub mod policy_metrics;
pub mod policy_quarantine;
