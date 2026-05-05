//! Latency stage decomposition and budget algebra for the AARSP program.
//!
//! This module defines the formal stage decomposition of the input-to-visible-response
//! path, budget algebra for composing per-stage latency targets, and invariants
//! that the system must maintain under all conditions.
//!
//! # Stage Decomposition
//!
//! The critical path from PTY output to visible response traverses these stages:
//!
//! ```text
//! PTY → Capture → Delta → StorageWrite → PatternDetect → EventEmit
//!     → WorkflowDispatch → ActionExecute → ApiResponse
//! ```
//!
//! Each stage has independent p50/p95/p99/p999 budgets. The aggregate budget
//! is computed via composition rules that account for:
//! - Sequential composition (additive)
//! - Parallel fan-out (max of branches)
//! - Conditional paths (weighted by branch probability)
//!
//! # Budget Algebra
//!
//! Budget composition follows these rules:
//! - **Sequential**: B(A → B) = B(A) + B(B)
//! - **Parallel**: B(A ∥ B) = max(B(A), B(B))
//! - **Conditional**: B(A | p) = p·B(A) + (1-p)·B(skip)
//! - **Slack**: S = B(aggregate) - Σ B(stage_i) — must be ≥ 0
//!
//! # Invariants
//!
//! 1. **Monotonic sequencing**: Segment seq numbers are strictly increasing per pane.
//! 2. **Budget non-negative**: No stage budget can be negative.
//! 3. **Aggregate ceiling**: Sum of stage budgets ≤ aggregate budget at each percentile.
//! 4. **Slack conservation**: Redistributing slack preserves total budget.
//! 5. **Overflow isolation**: A stage exceeding its budget triggers overflow, not cascade.
//! 6. **Deterministic replay**: Same input + seed + config → same stage timings.
//!
//! # Reason Codes
//!
//! Every budget violation produces a structured reason code:
//! - `BUDGET_EXCEEDED_<STAGE>_<PERCENTILE>`: Stage exceeded its target at given percentile.
//! - `SLACK_EXHAUSTED`: Aggregate slack consumed, no redistribution possible.
//! - `OVERFLOW_ISOLATED`: Stage overflow contained, downstream unaffected.
//! - `CASCADE_PREVENTED`: Overflow mitigation activated (skip, degrade, shed).
//!
//! # Module organization (br-ft-l8s7v roadmap)
//!
//! This file is 29,611 LOC / 256 public types — the largest non-vendored
//! single file in the workspace at 976 KB on disk. The eventual goal
//! (per ft-l8s7v) is to extract the larger subsystems into sibling
//! modules under `latency_stages/`. Slice 1 (`percentile.rs`) has
//! already shipped; this roadmap maps the remaining 9 extraction
//! candidates so the follow-up work doesn't have to redo the
//! subsystem-mapping audit.
//!
//! Each cluster is internally cohesive: types within a cluster share
//! configs, snapshots, degradation enums, and log entries; cross-
//! cluster references are mostly via the core types in the first
//! cluster.
//!
//! Cluster | Type definitions | Approx. lines | Suggested extraction module
//! :-- | :-- | --: | :--
//! Core types + budget algebra | LatencyStage, StageBudget, Lindley telemetry, BudgetNode/CompositionMode, ReasonCode/Mitigation, StageObservation, PipelineRun, InvariantViolation, BudgetError, LatencyLogEntry, WorkloadClass, BenchmarkCriterion/Contract, TestCategory, VerificationEntry | ~60–1426 | `latency_stages/core.rs` (or stays here as the umbrella)
//! Enforcement | BudgetEnforcer (+ Config/MitigationPolicy/ObservationResult/StageSnapshot/EnforcerSnapshot), CorrelationContext, StageProbe/StageTiming/InstrumentationOverhead, InstrumentedEnforcer (+ Diagnostic), FastProbe, MitigationLevel, PolicyConstraint, RecoveryProtocol, StageEnforcementState, EnforcementDecision, RuntimeEnforcer (+ Config/Snapshot) | ~1427–3122 | `latency_stages/enforcer.rs`
//! Scheduling | AdaptiveAllocator (+ Config/StagePressure/LaneAllocation/AllocationDecision/StageAdjustment/AllocationReason/AllocatorSnapshot/Degradation/LogEntry), LaneScheduler (+ SchedulerLane/WorkItem/AdmissionDecision/Config/LaneState/SchedulingEvent/SchedulerSnapshot/Degradation/LogEntry), InputRing (+ Item/RingBackpressure/Config/Snapshot) | ~3123–4870 | `latency_stages/scheduler.rs`
//! Priority + fairness | Priority, Resource, InheritanceEvent, LockResult, PriorityInheritanceConfig, HeldLock, PriorityInheritanceTracker (+ Snapshot/Degradation/LogEntry), StarvationConfig, LaneFairnessState, StarvationEvent, StarvationTracker (+ Snapshot/Degradation/LogEntry) | ~4872–5857 | `latency_stages/prio_fairness.rs`
//! Resource pools | MemoryDomain, MemoryPool (+ Config/AllocResult/Snapshot/Degradation/LogEntry), IngestParser (+ Chunk/ParseResult/Config/Snapshot/Degradation/LogEntry), TieredScrollback (+ ScrollbackTier/TierConfig/MigrationPolicy/ScrollbackSegment/MigrationEvent/Snapshot/Manager/Degradation/LogEntry) | ~5859–7289 | `latency_stages/resource_pools.rs`
//! Transport + tail | TransportPolicy (+ Mode/CostModel/Config/Decision/Snapshot/Degradation/LogEntry), TailLatencyController (+ SyscallStrategy/WakeupSource/AffinityHint/Config/WakeupEvent/Snapshot/Degradation/LogEntry), HitchRiskModel (+ EvidenceSignal/EvidenceEntry/Level/Config/Snapshot/Degradation/LogEntry), EProcessDetector (+ Kind/DriftObservable/AlertLevel/Config/Observation/Snapshot/Degradation/LogEntry), PolicyController (+ Action/SystemState/LossEntry/Config/Decision/Snapshot/Degradation/LogEntry) | ~7291–9590 | `latency_stages/transport_tail.rs`
//! Calibration + invariants | CalibrationHarness (+ Scenario/Result/PromotionGateConfig/Verdict/Snapshot/Degradation/LogEntry), InvariantChecker (+ Domain/Severity/FormalInvariant/Scheduler/Budget/RecoveryInvariant/Outcome/Result/Config/Snapshot/Degradation/LogEntry), ModelChecker (+ TraceStep/TraceAction/Counterexample/Strategy/Config/Snapshot/Verdict/Degradation/LogEntry), ReplayCanonicalizer (+ TraceFormatVersion/CanonicalOrdering/TraceEntry/DeterministicTrace/Comparison/TraceMismatch/Config/Snapshot/Degradation/LogEntry), ProofGate (+ GoldenArtifact/Verdict/Config/Summary/Snapshot/Degradation/LogEntry) | ~9592–12881 | `latency_stages/calibration_invariants.rs`
//! Fault + breaker + ack | FaultIsolation (+ Domain/DomainHealth/CrashOnlyContract/Event/State/Config/Manager/Snapshot/Degradation/LogEntry/BlastRadius{Report,Analyzer}/TransitionLog/ReasonCode/InstrumentedManager), BreakerManager (+ ReplayAction/Event/State/Config/RecoveryStep/Choreography/Snapshot/Degradation/LogEntry), AckProtocol (+ Phase/Reason/Token/Deferred/Config/Progress/Snapshot/Degradation/LogEntry/Manager) | ~12883–14792 | `latency_stages/fault_breaker.rs`
//! SLO + validation | ValidationMatrix (+ ScenarioCategory/Verdict/Scenario/Result/PromotionGate/Snapshot/Degradation/LogEntry), QoEGuardrail (+ Metric/SLO/Measurement/Config/SLOVerdict/Snapshot/Degradation/LogEntry) | ~14794–15580 | `latency_stages/slo_validation.rs`
//!
//! # Extraction guidance
//!
//! - **Tests**: umbrella-level tests live in `latency_stages/tests.rs`.
//!   Each extracted module brings its own subsystem-specific tests
//!   inline; cross-subsystem proptest tests should land in
//!   `latency_stages/proptest_*.rs` or `tests/proptest_*.rs` siblings.
//! - **Public re-exports**: every type currently reachable as
//!   `crate::latency_stages::*` MUST keep that path after extraction —
//!   re-export from the umbrella `latency_stages.rs` (this file). The
//!   slice-1 `pub use percentile::Percentile` shape is the template.
//! - **`pub(crate)` → `pub(super)` shift**: items that are
//!   currently `pub(crate)` and used only within latency_stages will
//!   tighten to `pub(super)` once moved into the sibling. Avoid
//!   accidental visibility widening; the network_calculus_bound
//!   import + StageModel/ServiceCurve types are cross-cutting and
//!   stay shared.
//! - **Build-time benchmark**: a single-crate compile of
//!   frankenterm-core takes 4–6 min on this file's class — extracting
//!   the 9 clusters above should measurably improve incremental
//!   rebuild time when touching a single subsystem.
//! - **Slice ordering**: the natural extraction order is from the
//!   leaves toward the umbrella — `slo_validation.rs` and
//!   `fault_breaker.rs` have the fewest cross-cluster references
//!   (mostly only depend on Core types) and should land first; the
//!   `enforcer.rs` and `scheduler.rs` clusters depend on more of
//!   Core and should land last.
//!
//! # AARSP Bead: ft-2p9cb.1.1.1

// br-ft-l8s7v slice 1: first sibling-module decomposition. Percentile
// is a self-contained primitive (no latency_stages deps) — extracted
// to `latency_stages/percentile.rs`. Re-exported here so existing
// `latency_stages::Percentile` paths in callsites stay unchanged.
mod percentile;
pub use percentile::Percentile;

// br-ft-l8s7v slice 5: latency stage identity extracted from the
// core cluster. Re-exported here so existing
// `latency_stages::LatencyStage` paths stay unchanged.
mod stage;
pub use stage::LatencyStage;

// br-ft-l8s7v slice 29: reason codes and mitigation labels extracted from
// the core latency-stage monolith. Re-exported here so existing
// latency_stages::ReasonCode and Mitigation paths stay unchanged.
mod reason;
pub use reason::*;

// br-ft-l8s7v slice 30: budget construction error types extracted from
// the core latency-stage monolith. Re-exported here so existing
// latency_stages::BudgetError paths stay unchanged.
mod budget_error;
pub use budget_error::*;

// br-ft-l8s7v slice 31: pipeline observations and validation invariants
// extracted from the core latency-stage monolith. Re-exported here so existing
// latency_stages::PipelineRun and InvariantViolation paths stay unchanged.
mod pipeline_run;
pub use pipeline_run::*;

// br-ft-l8s7v slice 32: structured latency log contract extracted from
// the core latency-stage monolith. Re-exported here so existing
// latency_stages::LatencyLogEntry paths stay unchanged.
mod latency_log;
pub use latency_log::*;

// br-ft-l8s7v slice 33: benchmark workload contract extracted from the core
// latency-stage monolith. Re-exported here so existing
// latency_stages::BenchmarkContract and WorkloadClass paths stay unchanged.
mod benchmark_contract;
pub use benchmark_contract::*;

// br-ft-l8s7v slice 34: runtime budget enforcer extracted from the core
// latency-stage monolith. Re-exported here so existing
// latency_stages::BudgetEnforcer and EnforcerSnapshot paths stay unchanged.
mod budget_enforcer;
#[cfg(test)]
use budget_enforcer::LatencyWindow;
pub use budget_enforcer::*;

// br-ft-l8s7v slice 35: correlation/instrumentation probes extracted from
// the core latency-stage monolith. Re-exported here so existing
// latency_stages::CorrelationContext and InstrumentedEnforcer paths stay unchanged.
mod instrumentation;
pub use instrumentation::*;

// br-ft-l8s7v slice 36: runtime mitigation policy and recovery state extracted
// from the core latency-stage monolith. Re-exported here so existing
// latency_stages::MitigationLevel and StageEnforcementState paths stay unchanged.
mod runtime_policy;
pub use runtime_policy::*;

// br-ft-l8s7v slice 37: runtime enforcer wrapper extracted from the core
// latency-stage monolith. Re-exported here so existing
// latency_stages::RuntimeEnforcer and EnforcementDecision paths stay unchanged.
mod runtime_enforcer;
pub use runtime_enforcer::*;

// br-ft-l8s7v slice 2: QoE/SLO guardrail lane extracted from the
// SLO + validation cluster. Re-exported here so existing
// `latency_stages::QoEGuardrail` paths stay unchanged.
mod slo_validation;
pub use slo_validation::*;

// br-ft-l8s7v slice 3: validation matrix lane extracted from the
// SLO + validation cluster. Re-exported here so existing
// `latency_stages::ValidationMatrix` paths stay unchanged.
mod validation_matrix;
pub use validation_matrix::*;

// br-ft-l8s7v slice 4: immediate-ack / deferred-completion protocol
// extracted from the fault + breaker + ack cluster. Re-exported here so
// existing `latency_stages::AckProtocolManager` paths stay unchanged.
mod ack_protocol;
pub use ack_protocol::*;

// br-ft-l8s7v slice 10: fault-domain isolation and crash-only contracts
// extracted from the fault + breaker + ack cluster. Re-exported here so
// existing latency_stages::FaultIsolationManager paths stay unchanged.
mod fault_isolation;
pub use fault_isolation::*;

// br-ft-l8s7v slice 6: fault-domain blast-radius DAG analysis
// extracted from the fault + breaker + ack cluster. Re-exported here so
// existing `latency_stages::BlastRadiusAnalyzer` paths stay unchanged.
mod fault_blast_radius;
pub use fault_blast_radius::*;

// br-ft-l8s7v slice 7: structured fault transition logging and deterministic
// replay wrapper extracted from the fault + breaker + ack cluster.
mod fault_transition_log;
pub use fault_transition_log::*;

// br-ft-l8s7v slice 9: circuit breaker and recovery choreography extracted
// from the fault + breaker + ack cluster. Re-exported here so existing
// latency_stages::BreakerManager paths stay unchanged.
mod breaker_manager;
pub use breaker_manager::*;

// br-ft-l8s7v slice 8: Lindley-bounds release-attestation telemetry extracted
// from the core cluster. Re-exported here so existing latency_stages::Lindley*
// paths stay unchanged.
mod lindley;
pub use lindley::*;

// br-ft-l8s7v slice 10: static verification catalog extracted from the core
// cluster. Re-exported here so existing latency_stages::verification_matrix
// and latency_stages::TestCategory paths stay unchanged.
mod verification_catalog;
pub use verification_catalog::*;

// br-ft-l8s7v slice 14: formal invariant predicates and runtime checker
// extracted from the calibration + invariants cluster. Re-exported here so
// existing latency_stages::InvariantChecker paths stay unchanged.
mod formal_invariants;
pub use formal_invariants::*;

// br-ft-l8s7v slice 15: bounded model-checking harness and counterexamples
// extracted from the calibration + invariants cluster. Re-exported here so
// existing latency_stages::TraceAction and ModelChecker paths stay unchanged.
mod model_checker;
pub use model_checker::*;

// br-ft-l8s7v slice 13: calibration harness and promotion gate types
// extracted from the calibration + invariants cluster. Re-exported here so
// existing latency_stages::CalibrationHarness paths stay unchanged.
mod calibration_harness;
pub use calibration_harness::*;

// br-ft-l8s7v slice 12: deterministic trace and replay canonicalization
// extracted from the calibration + invariants cluster. Re-exported here so
// existing latency_stages::ReplayCanonicalizer paths stay unchanged.
mod replay_canonicalizer;
pub use replay_canonicalizer::*;

// br-ft-l8s7v slice 11: optimization isomorphism proof gate extracted from
// the calibration + invariants cluster. Re-exported here so existing
// latency_stages::ProofGate paths stay unchanged.
mod proof_gate;
pub use proof_gate::*;

// br-ft-l8s7v slice 13: stage budget table and composition algebra extracted
// from the core cluster. Re-exported here so existing latency_stages::StageBudget
// and latency_stages::default_pipeline_tree paths stay unchanged.
mod budget_algebra;
pub use budget_algebra::*;

// br-ft-l8s7v slice 21: adaptive budget allocator extracted from the
// core latency-stage monolith. Re-exported here so existing
// latency_stages::AdaptiveAllocator paths stay unchanged.
mod adaptive_allocator;
pub use adaptive_allocator::*;

// br-ft-l8s7v slice 22: three-lane scheduler extracted from the core
// latency-stage monolith. Re-exported here so existing
// latency_stages::LaneScheduler and SchedulerLane paths stay unchanged.
mod lane_scheduler;
pub use lane_scheduler::*;

// br-ft-l8s7v slice 23: bounded input ring extracted from the core
// latency-stage monolith. Re-exported here so existing latency_stages::InputRing
// and latency_stages::RingBackpressure paths stay unchanged.
mod input_ring;
pub use input_ring::*;

// br-ft-l8s7v slice 24: priority inheritance and lock-order tracking extracted
// from the core latency-stage monolith. Re-exported here so existing
// latency_stages::PriorityInheritanceTracker paths stay unchanged.
mod priority_inheritance;
pub use priority_inheritance::*;

// br-ft-l8s7v slice 25: starvation prevention and fairness tracking extracted
// from the core latency-stage monolith. Re-exported here so existing
// latency_stages::StarvationTracker paths stay unchanged.
mod starvation;
pub use starvation::*;

// br-ft-l8s7v slice 26: memory ownership domains and fixed-block pool extracted
// from the core latency-stage monolith. Re-exported here so existing
// latency_stages::MemoryPool paths stay unchanged.
mod memory_pool;
pub use memory_pool::*;

// br-ft-l8s7v slice 27: zero-copy ingestion parser extracted from the core
// latency-stage monolith. Re-exported here so existing
// latency_stages::IngestParser paths stay unchanged.
mod ingest_parser;
pub use ingest_parser::*;

// br-ft-l8s7v slice 28: tiered scrollback memory hierarchy extracted from
// the core latency-stage monolith. Re-exported here so existing
// latency_stages::TieredScrollbackManager paths stay unchanged.
mod tiered_scrollback;
pub use tiered_scrollback::*;

// br-ft-l8s7v slice 16: adaptive transport cost-model policy extracted
// from the transport + tail cluster. Re-exported here so existing
// latency_stages::TransportPolicy paths stay unchanged.
mod transport_policy;
pub use transport_policy::*;

// br-ft-l8s7v slice 17: kernel/hardware tail-latency controller extracted
// from the transport + tail cluster. Re-exported here so existing
// latency_stages::TailLatencyController paths stay unchanged.
mod tail_latency;
pub use tail_latency::*;

// br-ft-l8s7v slice 18: Bayesian hitch-risk posterior model extracted
// from the transport + tail cluster. Re-exported here so existing
// latency_stages::HitchRiskModel and HitchRiskLevel paths stay unchanged.
mod hitch_risk;
pub use hitch_risk::*;

// br-ft-l8s7v slice 19: anytime-valid e-process drift detector extracted
// from the transport + tail cluster. Re-exported here so existing
// latency_stages::EProcessDetector paths stay unchanged.
mod e_process_drift;
pub use e_process_drift::*;

// br-ft-l8s7v slice 20: expected-loss policy controller extracted
// from the transport + tail cluster. Re-exported here so existing
// latency_stages::PolicyController paths stay unchanged.
mod policy_controller;
pub use policy_controller::*;

// ── Stage Definitions ──────────────────────────────────────────────
// `LatencyStage` extracted to `latency_stages/stage.rs` under
// br-ft-l8s7v slice 5. Re-exported above via `pub use`.

// ── Percentile Targets ─────────────────────────────────────────────
// `Percentile` extracted to `latency_stages/percentile.rs` under
// br-ft-l8s7v slice 1. Re-exported above via `pub use`.

// ── Reason Codes ───────────────────────────────────────────────────
// Extracted to `latency_stages/reason.rs` under br-ft-l8s7v slice 29.
// Re-exported above via `pub use`.

// ── Stage Measurement and Invariant Violations ─────────────────────
// Extracted to `latency_stages/pipeline_run.rs` under br-ft-l8s7v slice 31.
// Re-exported above via `pub use`.

// ── Error Types ────────────────────────────────────────────────────
// Extracted to `latency_stages/budget_error.rs` under br-ft-l8s7v slice 30.
// Re-exported above via `pub use`.

// ── Structured Logging Contract ────────────────────────────────────
// Extracted to `latency_stages/latency_log.rs` under br-ft-l8s7v slice 32.
// Re-exported above via `pub use`.

// ── Benchmark Contract ─────────────────────────────────────────────
// Extracted to `latency_stages/benchmark_contract.rs` under
// br-ft-l8s7v slice 33. Re-exported above via `pub use`.

// `TestCategory`, `VerificationEntry`, and `verification_matrix` extracted to
// `latency_stages/verification_catalog.rs` under br-ft-l8s7v slice 10.
// Re-exported above via `pub use`.

// ── Runtime Budget Enforcer ─────────────────────────────────────────
// Extracted to `latency_stages/budget_enforcer.rs` under br-ft-l8s7v slice 34.
// Re-exported above via `pub use`.

// ── Instrumentation Probes ─────────────────────────────────────────
// Extracted to `latency_stages/instrumentation.rs` under br-ft-l8s7v slice 35.
// Re-exported above via `pub use`.

// ── Runtime Enforcement ─────────────────────────────────────────────

/// AARSP Bead: ft-2p9cb.1.3 — Runtime Budget Enforcement
///
/// This section implements the enforcement guards that sit on the critical path,
/// applying deterministic mitigation when budgets are exceeded.
/// Mitigation policy, recovery protocol, and per-stage enforcement state are
/// extracted to `latency_stages/runtime_policy.rs` under br-ft-l8s7v slice 36.
/// Re-exported above via `pub use`.
/// Runtime enforcer decision/config/snapshot types are extracted to
/// `latency_stages/runtime_enforcer.rs` under br-ft-l8s7v slice 37.
/// Re-exported above via `pub use`.

// ── A4: Adaptive Budget Allocator ─────────────────────────────────
// Extracted to `latency_stages/adaptive_allocator.rs` under br-ft-l8s7v slice 21.
// Re-exported above via `pub use`.

// ── B1: Three-Lane Scheduler Architecture ─────────────────────────
// Extracted to `latency_stages/lane_scheduler.rs` under br-ft-l8s7v slice 22.
// Re-exported above via `pub use`.

// ── B2: Bounded Input Ring ────────────────────────────────────────
// Extracted to `latency_stages/input_ring.rs` under br-ft-l8s7v slice 23.
// Re-exported above via `pub use`.

// ── AARSP Bead: ft-2p9cb.2.3 — Priority Inheritance & Lock-Order ──
// Extracted to `latency_stages/priority_inheritance.rs` under br-ft-l8s7v slice 24.
// Re-exported above via `pub use`.

// ── AARSP Bead: ft-2p9cb.2.4 — Starvation Prevention & Fairness ──
// Extracted to `latency_stages/starvation.rs` under br-ft-l8s7v slice 25.
// Re-exported above via `pub use`.

// ── AARSP Bead: ft-2p9cb.3.1 — Memory Ownership Graph & Pool ──────
// Extracted to `latency_stages/memory_pool.rs` under br-ft-l8s7v slice 26.
// Re-exported above via `pub use`.

// ── AARSP Bead: ft-2p9cb.3.2 — Zero-Copy Ingestion Parser ──────
// Extracted to `latency_stages/ingest_parser.rs` under br-ft-l8s7v slice 27.
// Re-exported above via `pub use`.

// ── C3: Tiered Scrollback Memory Hierarchy ─────────────────────────
// Extracted to `latency_stages/tiered_scrollback.rs` under br-ft-l8s7v slice 28.
// Re-exported above via `pub use`.

// ── C4: Adaptive Transport Policy ──────────────────────────────────
// Extracted to `latency_stages/transport_policy.rs` under
// br-ft-l8s7v slice 16. Re-exported above via `pub use`.

// ── C5: Kernel/Hardware Tail-Latency ───────────────────────────────
// Extracted to `latency_stages/tail_latency.rs` under
// br-ft-l8s7v slice 17. Re-exported above via `pub use`.

// ── D1: Bayesian Hitch-Risk Posterior Model ────────────────────────
// Extracted to `latency_stages/hitch_risk.rs` under br-ft-l8s7v slice 18.
// Re-exported above via `pub use`.

// ── D2: Anytime-Valid E-Process Drift Detector ─────────────────────
// Extracted to `latency_stages/e_process_drift.rs` under br-ft-l8s7v slice 19.
// Re-exported above via `pub use`.

// ── D3: Expected-Loss Policy Controller ────────────────────────────
// Extracted to `latency_stages/policy_controller.rs` under br-ft-l8s7v slice 20.
// Re-exported above via `pub use`.

// ── D4: Calibration Harness and Promotion Gates ────────────────────
// Extracted to `latency_stages/calibration_harness.rs` under
// br-ft-l8s7v slice 13. Re-exported above via `pub use`.

// ── E1: Formal Specification Pack ──────────────────────────────────
// `InvariantDomain`, `FormalInvariant`, `SchedulerInvariant`,
// `BudgetInvariant`, `RecoveryInvariant`, and `InvariantChecker` extracted
// to `latency_stages/formal_invariants.rs` under br-ft-l8s7v slice 14.
// Re-exported above via `pub use`.

// ── E2: Model-Checking Harness and Counterexample Pipeline ────────
// `TraceStep`, `TraceAction`, `Counterexample`, and `ModelChecker` extracted
// to `latency_stages/model_checker.rs` under br-ft-l8s7v slice 15.
// Re-exported above via `pub use`.

// ── E3: Deterministic Trace v2 and Replay Canonicalization ─────────
// Extracted to `latency_stages/replay_canonicalizer.rs` under
// br-ft-l8s7v slice 12. Re-exported above via `pub use`.

#[cfg(test)]
fn action_domain(action: &TraceAction) -> InvariantDomain {
    replay_canonicalizer::action_domain(action)
}

// ── E4: Optimization Isomorphism Proof Gate ───────────────────────
// Extracted to `latency_stages/proof_gate.rs` under
// br-ft-l8s7v slice 11. Re-exported above via `pub use`.

// ── F1: Fault-Domain Isolation and Crash-Only Service Contracts ────
// Extracted to `latency_stages/fault_isolation.rs` under
// br-ft-l8s7v slice 10. Re-exported above via `pub use`.

// ── F1: Cross-domain blast-radius analysis ────────────────────────
// Extracted to `latency_stages/fault_blast_radius.rs` under
// br-ft-l8s7v slice 6. Re-exported above via `pub use`.

// ── F1: Structured transition log buffer ──────────────────────────
// Extracted to `latency_stages/fault_transition_log.rs` under
// br-ft-l8s7v slice 7. Re-exported above via `pub use`.

// ── F2: Circuit Breakers and Recovery Choreography ────────────────
// Extracted to `latency_stages/breaker_manager.rs` under
// br-ft-l8s7v slice 9. Re-exported above via `pub use`.

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::float_cmp, clippy::manual_range_contains)]
mod tests;
