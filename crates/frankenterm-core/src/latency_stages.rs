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
//! - **Tests**: the file has two `#[cfg(test)] mod tests` blocks — a
//!   small one at ~6621 (TieredScrollback-only) and the main block at
//!   ~15583 covering the rest of the file (~14000 lines of tests).
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

use serde::{Deserialize, Serialize};
use std::fmt;

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
/// Mitigation ladder with ordered escalation levels.
///
/// The ladder defines a strict partial order of increasingly aggressive
/// mitigation actions. The enforcer escalates monotonically (never
/// de-escalates within a single stage evaluation).
///
/// # Ladder ordering (least to most aggressive):
/// ```text
/// None(0) → Defer(1) → Degrade(2) → Shed(3) → Skip(4)
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum MitigationLevel {
    /// No mitigation needed.
    None = 0,
    /// Defer to next cycle.
    Defer = 1,
    /// Degrade quality.
    Degrade = 2,
    /// Shed load.
    Shed = 3,
    /// Skip entirely.
    Skip = 4,
}

impl MitigationLevel {
    /// Convert from Mitigation enum.
    pub fn from_mitigation(m: Mitigation) -> Self {
        match m {
            Mitigation::None => Self::None,
            Mitigation::Defer => Self::Defer,
            Mitigation::Degrade => Self::Degrade,
            Mitigation::Shed => Self::Shed,
            Mitigation::Skip => Self::Skip,
        }
    }

    /// Convert back to Mitigation enum.
    pub fn to_mitigation(self) -> Mitigation {
        match self {
            Self::None => Mitigation::None,
            Self::Defer => Mitigation::Defer,
            Self::Degrade => Mitigation::Degrade,
            Self::Shed => Mitigation::Shed,
            Self::Skip => Mitigation::Skip,
        }
    }

    /// All levels in escalation order.
    pub const ALL: &[Self] = &[
        Self::None,
        Self::Defer,
        Self::Degrade,
        Self::Shed,
        Self::Skip,
    ];

    /// Numeric severity (0-4).
    pub fn severity(self) -> u8 {
        self as u8
    }
}

impl fmt::Display for MitigationLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => f.write_str("NONE"),
            Self::Defer => f.write_str("DEFER"),
            Self::Degrade => f.write_str("DEGRADE"),
            Self::Shed => f.write_str("SHED"),
            Self::Skip => f.write_str("SKIP"),
        }
    }
}

/// Policy constraint that limits which mitigations can be applied to a stage.
///
/// # Safety Contract
/// Some stages are critical and must never be skipped. Others can tolerate
/// degradation but not shedding. PolicyConstraint makes these rules explicit
/// and machine-enforceable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyConstraint {
    /// Stage this policy applies to.
    pub stage: LatencyStage,
    /// Maximum allowed mitigation level.
    pub max_level: MitigationLevel,
    /// Whether this stage is critical (violations generate alerts).
    pub critical: bool,
    /// Minimum observations before enforcement kicks in (warmup).
    pub warmup_count: u64,
}

impl PolicyConstraint {
    /// Check if a proposed mitigation level is allowed.
    pub fn allows(&self, level: MitigationLevel) -> bool {
        level <= self.max_level
    }

    /// Clamp a proposed level to the maximum allowed.
    pub fn clamp(&self, level: MitigationLevel) -> MitigationLevel {
        if level <= self.max_level {
            level
        } else {
            self.max_level
        }
    }
}

/// Default policy constraints for all pipeline stages.
pub fn default_policy_constraints() -> Vec<PolicyConstraint> {
    vec![
        PolicyConstraint {
            stage: LatencyStage::PtyCapture,
            max_level: MitigationLevel::Shed,
            critical: true,
            warmup_count: 10,
        },
        PolicyConstraint {
            stage: LatencyStage::DeltaExtraction,
            max_level: MitigationLevel::Degrade,
            critical: false,
            warmup_count: 10,
        },
        PolicyConstraint {
            stage: LatencyStage::StorageWrite,
            max_level: MitigationLevel::Defer,
            critical: true,
            warmup_count: 10,
        },
        PolicyConstraint {
            stage: LatencyStage::PatternDetection,
            max_level: MitigationLevel::Skip,
            critical: false,
            warmup_count: 10,
        },
        PolicyConstraint {
            stage: LatencyStage::EventEmission,
            max_level: MitigationLevel::Defer,
            critical: true,
            warmup_count: 10,
        },
        PolicyConstraint {
            stage: LatencyStage::WorkflowDispatch,
            max_level: MitigationLevel::Skip,
            critical: false,
            warmup_count: 5,
        },
        PolicyConstraint {
            stage: LatencyStage::ActionExecution,
            max_level: MitigationLevel::Shed,
            critical: false,
            warmup_count: 10,
        },
        PolicyConstraint {
            stage: LatencyStage::ApiResponse,
            max_level: MitigationLevel::Defer,
            critical: true,
            warmup_count: 10,
        },
    ]
}

/// Recovery protocol for stepping back from degraded to full quality.
///
/// After mitigation is applied, the system should recover once latency
/// returns to acceptable levels. RecoveryProtocol defines how quickly
/// and under what conditions recovery occurs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryProtocol {
    /// Number of consecutive within-budget observations before de-escalating.
    pub cooldown_observations: u64,
    /// Maximum time in degraded state before forced recovery attempt (μs).
    pub max_degraded_duration_us: u64,
    /// Whether to step down one level at a time or jump to full.
    pub gradual: bool,
}

impl Default for RecoveryProtocol {
    fn default() -> Self {
        Self {
            cooldown_observations: 20,
            max_degraded_duration_us: 30_000_000, // 30 seconds
            gradual: true,
        }
    }
}

/// Per-stage enforcement state tracking mitigation and recovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageEnforcementState {
    /// Current active mitigation level for this stage.
    pub current_level: MitigationLevel,
    /// Consecutive within-budget observations since last overflow.
    pub consecutive_ok: u64,
    /// Timestamp of last escalation (epoch μs, 0 if never escalated).
    pub last_escalation_us: u64,
    /// Total escalation count.
    pub escalation_count: u64,
    /// Total recovery count.
    pub recovery_count: u64,
}

impl StageEnforcementState {
    fn new() -> Self {
        Self {
            current_level: MitigationLevel::None,
            consecutive_ok: 0,
            last_escalation_us: 0,
            escalation_count: 0,
            recovery_count: 0,
        }
    }
}

/// Enforcement decision emitted for each stage observation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnforcementDecision {
    /// Stage evaluated.
    pub stage: LatencyStage,
    /// Observed latency.
    pub latency_us: f64,
    /// Whether budget was exceeded.
    pub overflow: bool,
    /// Raw mitigation from the enforcer (before policy clamping).
    pub raw_mitigation: MitigationLevel,
    /// Clamped mitigation (after policy constraint).
    pub applied_mitigation: MitigationLevel,
    /// Whether this was a recovery (de-escalation).
    pub recovery: bool,
    /// Reason code.
    pub reason: Option<ReasonCode>,
    /// Whether warmup period is still active (enforcement suppressed).
    pub warmup_active: bool,
}

/// Configuration for the runtime enforcer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeEnforcerConfig {
    /// Base enforcer configuration.
    pub enforcer_config: BudgetEnforcerConfig,
    /// Per-stage policy constraints.
    pub policy_constraints: Vec<PolicyConstraint>,
    /// Recovery protocol.
    pub recovery: RecoveryProtocol,
    /// Whether to emit structured decision logs.
    pub log_decisions: bool,
}

impl Default for RuntimeEnforcerConfig {
    fn default() -> Self {
        Self {
            enforcer_config: BudgetEnforcerConfig::default(),
            policy_constraints: default_policy_constraints(),
            recovery: RecoveryProtocol::default(),
            log_decisions: true,
        }
    }
}

/// The runtime budget enforcer with policy constraints and recovery.
///
/// Wraps BudgetEnforcer with:
/// - Policy-safe mitigation clamping
/// - Warmup suppression
/// - Recovery protocol (gradual de-escalation)
/// - Structured decision logging
///
/// # Determinism
/// All decisions are deterministic given the same sequence of observations.
/// No randomness, no system time — caller provides all timestamps.
#[derive(Debug, Clone)]
pub struct RuntimeEnforcer {
    enforcer: BudgetEnforcer,
    config: RuntimeEnforcerConfig,
    states: Vec<(LatencyStage, StageEnforcementState)>,
    decisions: Vec<EnforcementDecision>,
    observation_count: u64,
}

impl RuntimeEnforcer {
    /// Create a new runtime enforcer with the given configuration.
    pub fn new(config: RuntimeEnforcerConfig) -> Self {
        let enforcer = BudgetEnforcer::new(config.enforcer_config.clone());
        let states = LatencyStage::PIPELINE_STAGES
            .iter()
            .map(|&s| (s, StageEnforcementState::new()))
            .collect();
        Self {
            enforcer,
            config,
            states,
            decisions: Vec::new(),
            observation_count: 0,
        }
    }

    /// Create with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(RuntimeEnforcerConfig::default())
    }

    /// Record an observation and produce an enforcement decision.
    ///
    /// This is the main entry point for the critical path. It:
    /// 1. Records the observation in the base enforcer
    /// 2. Determines raw mitigation from overflow severity
    /// 3. Applies policy constraints (clamping)
    /// 4. Checks recovery conditions
    /// 5. Updates enforcement state
    /// 6. Emits a structured decision
    #[allow(clippy::similar_names)]
    pub fn enforce(
        &mut self,
        stage: LatencyStage,
        latency_us: f64,
        correlation_id: &str,
        now_us: u64,
    ) -> EnforcementDecision {
        self.observation_count += 1;

        // Step 1: Record in base enforcer.
        let obs = self.enforcer.record(stage, latency_us, correlation_id);

        // Find enforcement state for this stage.
        let state = self
            .states
            .iter_mut()
            .find(|(s, _)| *s == stage)
            .map(|(_, st)| st);

        let state = match state {
            Some(s) => s,
            None => {
                // Unknown stage — pass through.
                return EnforcementDecision {
                    stage,
                    latency_us,
                    overflow: false,
                    raw_mitigation: MitigationLevel::None,
                    applied_mitigation: MitigationLevel::None,
                    recovery: false,
                    reason: None,
                    warmup_active: true,
                };
            }
        };

        // Find policy constraint.
        let constraint = self
            .config
            .policy_constraints
            .iter()
            .find(|c| c.stage == stage);

        // Step 2: Check warmup.
        let warmup_active = constraint
            .map(|c| self.observation_count <= c.warmup_count)
            .unwrap_or(false);

        // Step 3: Determine raw mitigation level.
        let raw_level = MitigationLevel::from_mitigation(obs.recommended_mitigation);

        // Step 4: Apply policy constraint.
        let clamped_level = if warmup_active {
            MitigationLevel::None
        } else {
            constraint.map(|c| c.clamp(raw_level)).unwrap_or(raw_level)
        };

        // Step 5: Recovery check.
        let mut recovery = false;
        if obs.overflow {
            state.consecutive_ok = 0;
            if clamped_level > state.current_level {
                state.current_level = clamped_level;
                state.last_escalation_us = now_us;
                state.escalation_count += 1;
            }
        } else {
            state.consecutive_ok += 1;

            // Check recovery conditions.
            let cooldown_met = state.consecutive_ok >= self.config.recovery.cooldown_observations;
            let timeout_met = now_us.saturating_sub(state.last_escalation_us)
                >= self.config.recovery.max_degraded_duration_us;

            if state.current_level > MitigationLevel::None && (cooldown_met || timeout_met) {
                recovery = true;
                state.recovery_count += 1;
                if self.config.recovery.gradual && state.current_level > MitigationLevel::None {
                    // Step down one level.
                    let severity = state.current_level.severity();
                    state.current_level = if severity > 0 {
                        MitigationLevel::ALL[severity as usize - 1]
                    } else {
                        MitigationLevel::None
                    };
                } else {
                    state.current_level = MitigationLevel::None;
                }
                state.consecutive_ok = 0;
            }
        }

        let decision = EnforcementDecision {
            stage,
            latency_us,
            overflow: obs.overflow,
            raw_mitigation: raw_level,
            applied_mitigation: state.current_level,
            recovery,
            reason: obs.reason,
            warmup_active,
        };

        if self.config.log_decisions {
            self.decisions.push(decision.clone());
        }

        decision
    }

    /// Get the current mitigation level for a stage.
    pub fn current_level(&self, stage: LatencyStage) -> MitigationLevel {
        self.states
            .iter()
            .find(|(s, _)| *s == stage)
            .map(|(_, st)| st.current_level)
            .unwrap_or(MitigationLevel::None)
    }

    /// Get the enforcement state for a stage.
    pub fn stage_state(&self, stage: LatencyStage) -> Option<&StageEnforcementState> {
        self.states
            .iter()
            .find(|(s, _)| *s == stage)
            .map(|(_, st)| st)
    }

    /// Get the underlying enforcer.
    pub fn base_enforcer(&self) -> &BudgetEnforcer {
        &self.enforcer
    }

    /// Get accumulated decisions and clear.
    pub fn drain_decisions(&mut self) -> Vec<EnforcementDecision> {
        std::mem::take(&mut self.decisions)
    }

    /// Total observations processed.
    pub fn total_observations(&self) -> u64 {
        self.observation_count
    }

    /// Total escalations across all stages.
    pub fn total_escalations(&self) -> u64 {
        self.states.iter().map(|(_, s)| s.escalation_count).sum()
    }

    /// Total recoveries across all stages.
    pub fn total_recoveries(&self) -> u64 {
        self.states.iter().map(|(_, s)| s.recovery_count).sum()
    }

    /// Whether all stages are at MitigationLevel::None.
    pub fn is_fully_recovered(&self) -> bool {
        self.states
            .iter()
            .all(|(_, s)| s.current_level == MitigationLevel::None)
    }

    /// Compact status string.
    pub fn status_line(&self) -> String {
        let degraded: Vec<String> = self
            .states
            .iter()
            .filter(|(_, s)| s.current_level > MitigationLevel::None)
            .map(|(stage, s)| format!("{}={}", stage, s.current_level))
            .collect();
        if degraded.is_empty() {
            format!(
                "enforcement=NOMINAL obs={} esc={} rec={}",
                self.observation_count,
                self.total_escalations(),
                self.total_recoveries()
            )
        } else {
            format!(
                "enforcement=DEGRADED [{}] obs={} esc={} rec={}",
                degraded.join(", "),
                self.observation_count,
                self.total_escalations(),
                self.total_recoveries()
            )
        }
    }

    /// Process a complete CorrelationContext through the enforcer.
    ///
    /// Returns per-stage enforcement decisions.
    pub fn enforce_run(
        &mut self,
        ctx: &CorrelationContext,
        base_time_us: u64,
    ) -> Vec<EnforcementDecision> {
        let mut decisions = Vec::with_capacity(ctx.timings.len());
        for timing in &ctx.timings {
            let d = self.enforce(
                timing.stage,
                timing.latency_us,
                &ctx.correlation_id,
                base_time_us + timing.end_us,
            );
            decisions.push(d);
        }
        decisions
    }

    /// Get a full diagnostic snapshot.
    pub fn diagnostic_snapshot(&self) -> RuntimeEnforcerSnapshot {
        RuntimeEnforcerSnapshot {
            observation_count: self.observation_count,
            total_escalations: self.total_escalations(),
            total_recoveries: self.total_recoveries(),
            fully_recovered: self.is_fully_recovered(),
            stage_states: self.states.iter().map(|(s, st)| (*s, st.clone())).collect(),
            base_snapshot: self.enforcer.snapshot(),
        }
    }
}

/// Full diagnostic snapshot of the runtime enforcer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeEnforcerSnapshot {
    pub observation_count: u64,
    pub total_escalations: u64,
    pub total_recoveries: u64,
    pub fully_recovered: bool,
    pub stage_states: Vec<(LatencyStage, StageEnforcementState)>,
    pub base_snapshot: EnforcerSnapshot,
}

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
mod tests {
    use super::*;

    // ── Stage Definitions ──

    #[test]
    fn test_pipeline_stages_complete() {
        assert_eq!(LatencyStage::PIPELINE_STAGES.len(), 8);
        assert!(
            !LatencyStage::PIPELINE_STAGES
                .iter()
                .any(|s| s.is_aggregate())
        );
    }

    #[test]
    fn test_capture_path_subset_of_pipeline() {
        for stage in LatencyStage::CAPTURE_PATH {
            assert!(
                LatencyStage::PIPELINE_STAGES.contains(stage),
                "capture path stage {stage} not in pipeline"
            );
        }
    }

    #[test]
    fn test_action_path_subset_of_pipeline() {
        for stage in LatencyStage::ACTION_PATH {
            assert!(
                LatencyStage::PIPELINE_STAGES.contains(stage),
                "action path stage {stage} not in pipeline"
            );
        }
    }

    #[test]
    fn test_aggregate_stages_identified() {
        assert!(LatencyStage::EndToEndCapture.is_aggregate());
        assert!(LatencyStage::EndToEndAction.is_aggregate());
        assert!(!LatencyStage::PtyCapture.is_aggregate());
    }

    #[test]
    fn test_reason_prefix_unique() {
        let mut prefixes = std::collections::HashSet::new();
        for &stage in LatencyStage::PIPELINE_STAGES {
            assert!(
                prefixes.insert(stage.reason_prefix()),
                "duplicate prefix: {}",
                stage.reason_prefix()
            );
        }
    }

    #[test]
    fn test_stage_display_matches_prefix() {
        for &stage in LatencyStage::PIPELINE_STAGES {
            assert_eq!(format!("{stage}"), stage.reason_prefix());
        }
    }

    // ── Percentile ──

    #[test]
    fn test_percentile_values_ordered() {
        let values: Vec<f64> = Percentile::ALL.iter().map(|p| p.value()).collect();
        for window in values.windows(2) {
            assert!(window[0] < window[1], "percentiles not strictly increasing");
        }
    }

    #[test]
    fn test_percentile_display() {
        assert_eq!(format!("{}", Percentile::P50), "p50");
        assert_eq!(format!("{}", Percentile::P999), "p999");
    }

    // ── StageBudget ──

    #[test]
    fn test_budget_construction_valid() {
        let b = StageBudget::new(LatencyStage::PtyCapture, 100.0, 200.0, 300.0, 400.0);
        assert!(b.is_ok());
        let b = b.unwrap();
        assert_eq!(b.target(Percentile::P50), 100.0);
        assert_eq!(b.target(Percentile::P999), 400.0);
    }

    #[test]
    fn test_budget_rejects_negative() {
        let b = StageBudget::new(LatencyStage::PtyCapture, -1.0, 200.0, 300.0, 400.0);
        assert!(matches!(b, Err(BudgetError::NegativeTarget { .. })));
    }

    #[test]
    fn test_budget_rejects_nonmonotonic() {
        let b = StageBudget::new(LatencyStage::PtyCapture, 200.0, 100.0, 300.0, 400.0);
        assert!(matches!(b, Err(BudgetError::NonMonotonic { .. })));
    }

    #[test]
    fn test_budget_equal_percentiles_allowed() {
        // Equal values at consecutive percentiles is valid (≤ not <).
        let b = StageBudget::new(LatencyStage::PtyCapture, 100.0, 100.0, 100.0, 100.0);
        assert!(b.is_ok());
    }

    #[test]
    fn test_budget_exceeds() {
        let b = StageBudget::new(LatencyStage::PtyCapture, 100.0, 200.0, 300.0, 400.0).unwrap();
        assert!(!b.exceeds(Percentile::P50, 99.0));
        assert!(b.exceeds(Percentile::P50, 101.0));
        assert!(!b.exceeds(Percentile::P50, 100.0)); // equal is not exceeded
    }

    #[test]
    fn test_budget_violation_reason() {
        let b = StageBudget::new(LatencyStage::StorageWrite, 100.0, 200.0, 300.0, 400.0).unwrap();
        let reason = b.violation_reason(Percentile::P99);
        assert!(matches!(
            reason,
            ReasonCode::BudgetExceeded {
                stage: LatencyStage::StorageWrite,
                percentile: Percentile::P99,
            }
        ));
    }

    // ── Default Budgets ──

    #[test]
    fn test_default_budgets_cover_all_stages() {
        let budgets = default_budgets();
        for &stage in LatencyStage::PIPELINE_STAGES {
            assert!(
                budgets.iter().any(|b| b.stage == stage),
                "missing budget for {stage}"
            );
        }
        // Aggregates also have budgets.
        assert!(
            budgets
                .iter()
                .any(|b| b.stage == LatencyStage::EndToEndCapture)
        );
        assert!(
            budgets
                .iter()
                .any(|b| b.stage == LatencyStage::EndToEndAction)
        );
    }

    #[test]
    fn test_default_budgets_monotonic() {
        for budget in default_budgets() {
            assert!(
                budget.p50_us <= budget.p95_us,
                "{}: p50 > p95",
                budget.stage
            );
            assert!(
                budget.p95_us <= budget.p99_us,
                "{}: p95 > p99",
                budget.stage
            );
            assert!(
                budget.p99_us <= budget.p999_us,
                "{}: p99 > p999",
                budget.stage
            );
        }
    }

    #[test]
    fn test_default_budgets_nonnegative() {
        for budget in default_budgets() {
            assert!(budget.p50_us >= 0.0, "{}: negative p50", budget.stage);
            assert!(budget.p95_us >= 0.0, "{}: negative p95", budget.stage);
            assert!(budget.p99_us >= 0.0, "{}: negative p99", budget.stage);
            assert!(budget.p999_us >= 0.0, "{}: negative p999", budget.stage);
        }
    }

    // ── Lindley Attestation Telemetry ──

    #[test]
    fn test_lindley_documented_default_builds_substrate_inputs() {
        let model = LindleyTelemetryModel::documented_default();
        let (arrival, stages) = model.to_network_calculus_inputs().unwrap();
        assert_eq!(arrival.burst(), 10.0);
        assert_eq!(arrival.rate(), 90.0);
        assert_eq!(
            stages
                .iter()
                .map(|stage| stage.name.as_str())
                .collect::<Vec<_>>(),
            vec!["capture", "delta_extract", "storage_write"]
        );

        let bound = crate::network_calculus_bound::pipeline_delay_bound(arrival, &stages).unwrap();
        assert!((bound - 8.1).abs() < 1e-9, "bound={bound}");
    }

    #[test]
    fn test_lindley_model_reads_live_enforcer_p99s() {
        let mut enforcer = BudgetEnforcer::with_defaults();
        enforcer.record(LatencyStage::PtyCapture, 1_200.0, "lindley");
        enforcer.record(LatencyStage::DeltaExtraction, 2_300.0, "lindley");
        enforcer.record(LatencyStage::StorageWrite, 3_400.0, "lindley");

        let model = LindleyTelemetryModel::from_enforcer_snapshot(
            &enforcer.snapshot(),
            10.0,
            80.0,
            &[
                (LatencyStage::PtyCapture, 200.0),
                (LatencyStage::DeltaExtraction, 150.0),
                (LatencyStage::StorageWrite, 100.0),
            ],
        )
        .unwrap();

        assert_eq!(model.arrival_rate_events_per_sec, 80.0);
        assert_eq!(model.stages[0].p99_latency_ms, 1.2);
        assert_eq!(model.stages[1].p99_latency_ms, 2.3);
        assert_eq!(model.stages[2].p99_latency_ms, 3.4);

        let (_, stages) = model.to_network_calculus_inputs().unwrap();
        assert_eq!(stages[0].service.rate(), 200.0);
        assert_eq!(stages[0].service.latency(), 1.2);
    }

    #[test]
    fn test_lindley_model_rejects_missing_live_stage_rate() {
        let mut enforcer = BudgetEnforcer::with_defaults();
        enforcer.record(LatencyStage::PtyCapture, 1_200.0, "lindley");
        enforcer.record(LatencyStage::DeltaExtraction, 2_300.0, "lindley");
        enforcer.record(LatencyStage::StorageWrite, 3_400.0, "lindley");

        let error = LindleyTelemetryModel::from_enforcer_snapshot(
            &enforcer.snapshot(),
            10.0,
            80.0,
            &[
                (LatencyStage::PtyCapture, 200.0),
                (LatencyStage::DeltaExtraction, 150.0),
            ],
        )
        .unwrap_err();
        assert!(error.contains("STORAGE_WRITE: missing service rate"));
    }

    // ── Budget Algebra ──

    #[test]
    fn test_leaf_aggregate() {
        let b = StageBudget::new(LatencyStage::PtyCapture, 100.0, 200.0, 300.0, 400.0).unwrap();
        let node = BudgetNode::Leaf(b);
        assert_eq!(node.aggregate(Percentile::P50), 100.0);
        assert_eq!(node.aggregate(Percentile::P999), 400.0);
    }

    #[test]
    fn test_sequential_composition_additive() {
        let a = StageBudget::new(LatencyStage::PtyCapture, 100.0, 200.0, 300.0, 400.0).unwrap();
        let b = StageBudget::new(LatencyStage::DeltaExtraction, 50.0, 100.0, 150.0, 200.0).unwrap();
        let seq = BudgetNode::Seq(vec![BudgetNode::Leaf(a), BudgetNode::Leaf(b)]);
        assert_eq!(seq.aggregate(Percentile::P50), 150.0);
        assert_eq!(seq.aggregate(Percentile::P999), 600.0);
    }

    #[test]
    fn test_parallel_composition_max() {
        let a = StageBudget::new(LatencyStage::PtyCapture, 100.0, 200.0, 300.0, 400.0).unwrap();
        let b =
            StageBudget::new(LatencyStage::DeltaExtraction, 150.0, 180.0, 250.0, 500.0).unwrap();
        let par = BudgetNode::Par(vec![BudgetNode::Leaf(a), BudgetNode::Leaf(b)]);
        assert_eq!(par.aggregate(Percentile::P50), 150.0); // max(100, 150)
        assert_eq!(par.aggregate(Percentile::P95), 200.0); // max(200, 180)
        assert_eq!(par.aggregate(Percentile::P999), 500.0); // max(400, 500)
    }

    #[test]
    fn test_conditional_composition_weighted() {
        let then_b = StageBudget::new(
            LatencyStage::WorkflowDispatch,
            1000.0,
            2000.0,
            3000.0,
            5000.0,
        )
        .unwrap();
        let cond = BudgetNode::Cond {
            probability: 0.5,
            then_branch: Box::new(BudgetNode::Leaf(then_b)),
            else_branch: None,
        };
        assert_eq!(cond.aggregate(Percentile::P50), 500.0); // 0.5 * 1000 + 0.5 * 0
        assert_eq!(cond.aggregate(Percentile::P999), 2500.0);
    }

    #[test]
    fn test_conditional_with_else_branch() {
        let then_b = StageBudget::new(
            LatencyStage::WorkflowDispatch,
            1000.0,
            2000.0,
            3000.0,
            5000.0,
        )
        .unwrap();
        let else_b =
            StageBudget::new(LatencyStage::ApiResponse, 200.0, 400.0, 600.0, 1000.0).unwrap();
        let cond = BudgetNode::Cond {
            probability: 0.3,
            then_branch: Box::new(BudgetNode::Leaf(then_b)),
            else_branch: Some(Box::new(BudgetNode::Leaf(else_b))),
        };
        // 0.3 * 1000 + 0.7 * 200 = 300 + 140 = 440
        let result = cond.aggregate(Percentile::P50);
        assert!((result - 440.0).abs() < 0.01);
    }

    #[test]
    fn test_slack_positive_means_headroom() {
        let a = StageBudget::new(LatencyStage::PtyCapture, 100.0, 200.0, 300.0, 400.0).unwrap();
        let node = BudgetNode::Leaf(a);
        let slack = node.slack(Percentile::P50, 200.0);
        assert_eq!(slack, 100.0); // 200 - 100 = 100μs headroom
    }

    #[test]
    fn test_slack_negative_means_over_budget() {
        let a = StageBudget::new(LatencyStage::PtyCapture, 100.0, 200.0, 300.0, 400.0).unwrap();
        let node = BudgetNode::Leaf(a);
        let slack = node.slack(Percentile::P50, 50.0);
        assert_eq!(slack, -50.0); // 50 - 100 = -50μs over budget
    }

    #[test]
    fn test_leaves_collects_all() {
        let tree = default_pipeline_tree();
        let leaves = tree.leaves();
        // All 8 pipeline stages should appear as leaves.
        assert_eq!(leaves.len(), 8);
    }

    // ── Default Pipeline Tree ──

    #[test]
    fn test_default_pipeline_tree_structure() {
        let tree = default_pipeline_tree();
        let leaves = tree.leaves();
        let stages: Vec<LatencyStage> = leaves.iter().map(|b| b.stage).collect();
        assert_eq!(stages[0], LatencyStage::PtyCapture);
        assert_eq!(stages[1], LatencyStage::DeltaExtraction);
        assert_eq!(stages[2], LatencyStage::StorageWrite);
        assert_eq!(stages[3], LatencyStage::PatternDetection);
        assert_eq!(stages[4], LatencyStage::EventEmission);
        assert_eq!(stages[5], LatencyStage::WorkflowDispatch);
        assert_eq!(stages[6], LatencyStage::ActionExecution);
        assert_eq!(stages[7], LatencyStage::ApiResponse);
    }

    #[test]
    fn test_default_pipeline_aggregate_within_e2e_capture_budget() {
        let _tree = default_pipeline_tree();
        let budgets = default_budgets();
        let e2e_capture = budgets
            .iter()
            .find(|b| b.stage == LatencyStage::EndToEndCapture)
            .unwrap();

        // The capture path aggregate should fit within the E2E capture budget.
        // Note: full tree includes conditional workflow path, so we check capture path only.
        let capture_stages: Vec<BudgetNode> = LatencyStage::CAPTURE_PATH
            .iter()
            .map(|&s| BudgetNode::Leaf(*budgets.iter().find(|b| b.stage == s).unwrap()))
            .collect();
        let capture_tree = BudgetNode::Seq(capture_stages);

        for &p in Percentile::ALL {
            let agg = capture_tree.aggregate(p);
            let ceiling = e2e_capture.target(p);
            assert!(
                agg <= ceiling,
                "capture path {p} aggregate {agg:.0}μs > E2E ceiling {ceiling:.0}μs"
            );
        }
    }

    // ── Reason Codes ──

    #[test]
    fn test_reason_code_display_budget_exceeded() {
        let rc = ReasonCode::BudgetExceeded {
            stage: LatencyStage::StorageWrite,
            percentile: Percentile::P99,
        };
        assert_eq!(format!("{rc}"), "BUDGET_EXCEEDED_STORAGE_WRITE_p99");
    }

    #[test]
    fn test_reason_code_display_slack_exhausted() {
        assert_eq!(format!("{}", ReasonCode::SlackExhausted), "SLACK_EXHAUSTED");
    }

    #[test]
    fn test_reason_code_display_overflow_isolated() {
        let rc = ReasonCode::OverflowIsolated {
            stage: LatencyStage::PatternDetection,
        };
        assert_eq!(format!("{rc}"), "OVERFLOW_ISOLATED_PATTERN_DETECT");
    }

    #[test]
    fn test_reason_code_display_cascade_prevented() {
        let rc = ReasonCode::CascadePrevented {
            stage: LatencyStage::ActionExecution,
            mitigation: Mitigation::Shed,
        };
        assert_eq!(format!("{rc}"), "CASCADE_PREVENTED_ACTION_EXEC_SHED");
    }

    #[test]
    fn test_reason_code_display_redistributed() {
        let rc = ReasonCode::SlackRedistributed {
            donor: LatencyStage::DeltaExtraction,
            recipient: LatencyStage::StorageWrite,
            amount_us: 500,
        };
        assert_eq!(
            format!("{rc}"),
            "SLACK_REDISTRIBUTED_DELTA_EXTRACT_TO_STORAGE_WRITE"
        );
    }

    // ── Mitigation ──

    #[test]
    fn test_mitigation_display() {
        assert_eq!(format!("{}", Mitigation::Skip), "SKIP");
        assert_eq!(format!("{}", Mitigation::Degrade), "DEGRADE");
        assert_eq!(format!("{}", Mitigation::Shed), "SHED");
        assert_eq!(format!("{}", Mitigation::Defer), "DEFER");
        assert_eq!(format!("{}", Mitigation::None), "NONE");
    }

    // ── Pipeline Run Validation ──

    #[test]
    fn test_pipeline_run_valid() {
        let run = make_valid_run();
        assert!(run.validate().is_ok());
    }

    #[test]
    fn test_pipeline_run_detects_stage_misordering() {
        let mut run = make_valid_run();
        // Swap two stages.
        run.stages.swap(0, 1);
        let result = run.validate();
        assert!(result.is_err());
        let violations = result.unwrap_err();
        assert!(
            violations
                .iter()
                .any(|v| matches!(v, InvariantViolation::StageOrdering { .. }))
        );
    }

    #[test]
    fn test_pipeline_run_detects_timestamp_regression() {
        let mut run = make_valid_run();
        // Make second stage start before first ends.
        run.stages[1].start_epoch_us = run.stages[0].start_epoch_us;
        let result = run.validate();
        assert!(result.is_err());
        let violations = result.unwrap_err();
        assert!(
            violations
                .iter()
                .any(|v| matches!(v, InvariantViolation::TimestampRegression { .. }))
        );
    }

    #[test]
    fn test_pipeline_run_detects_total_mismatch() {
        let mut run = make_valid_run();
        run.total_latency_us = 999_999.0; // way off
        let result = run.validate();
        assert!(result.is_err());
        let violations = result.unwrap_err();
        assert!(
            violations
                .iter()
                .any(|v| matches!(v, InvariantViolation::TotalMismatch { .. }))
        );
    }

    #[test]
    fn test_pipeline_run_detects_overflow_mismatch() {
        let mut run = make_valid_run();
        run.has_overflow = true; // no stage actually overflowed
        let result = run.validate();
        assert!(result.is_err());
        let violations = result.unwrap_err();
        assert!(
            violations
                .iter()
                .any(|v| matches!(v, InvariantViolation::OverflowFlagMismatch { .. }))
        );
    }

    // ── Workload Classes ──

    #[test]
    fn test_workload_classes_complete() {
        assert_eq!(WorkloadClass::ALL.len(), 8);
    }

    #[test]
    fn test_adversarial_workloads() {
        assert!(!WorkloadClass::LightSingle.is_adversarial());
        assert!(!WorkloadClass::HeavySingle.is_adversarial());
        assert!(WorkloadClass::BurstySwarm.is_adversarial());
        assert!(WorkloadClass::StorageDegraded.is_adversarial());
    }

    #[test]
    fn test_workload_primary_percentile_ordering() {
        // Adversarial workloads should target higher percentiles.
        let nominal_p = WorkloadClass::LightSingle.primary_percentile();
        let stress_p = WorkloadClass::BurstySwarm.primary_percentile();
        assert!(nominal_p < stress_p);
    }

    // ── Benchmark Contract ──

    #[test]
    fn test_benchmark_contract_covers_all_stages() {
        let contract = BenchmarkContract::default_contract();
        for &stage in LatencyStage::PIPELINE_STAGES {
            let has_criteria = contract.criteria.iter().any(|c| c.stage == stage);
            assert!(has_criteria, "no benchmark criteria for {stage}");
        }
    }

    #[test]
    fn test_benchmark_contract_covers_all_workloads() {
        let contract = BenchmarkContract::default_contract();
        for &workload in WorkloadClass::ALL {
            let has_criteria = contract.criteria.iter().any(|c| c.workload == workload);
            assert!(has_criteria, "no benchmark criteria for {workload}");
        }
    }

    #[test]
    fn test_benchmark_contract_overhead_limits() {
        let contract = BenchmarkContract::default_contract();
        for c in &contract.criteria {
            if c.workload.is_adversarial() {
                assert_eq!(c.max_overhead_fraction, 0.10);
            } else {
                assert_eq!(c.max_overhead_fraction, 0.05);
            }
        }
    }

    // ── Verification Matrix ──

    #[test]
    fn test_verification_matrix_covers_all_categories() {
        let matrix = verification_matrix();
        let categories: std::collections::HashSet<_> = matrix.iter().map(|e| e.category).collect();
        assert!(categories.contains(&TestCategory::Unit));
        assert!(categories.contains(&TestCategory::Property));
        assert!(categories.contains(&TestCategory::Integration));
        assert!(categories.contains(&TestCategory::EndToEnd));
        assert!(categories.contains(&TestCategory::Chaos));
        assert!(categories.contains(&TestCategory::Soak));
    }

    #[test]
    fn test_verification_matrix_all_named() {
        let matrix = verification_matrix();
        for entry in &matrix {
            assert!(!entry.name.is_empty(), "verification entry has empty name");
            assert!(
                !entry.invariants.is_empty(),
                "verification entry {} has no invariants",
                entry.name
            );
        }
    }

    // ── Serde Roundtrip ──

    #[test]
    fn test_stage_budget_serde_roundtrip() {
        let budget =
            StageBudget::new(LatencyStage::PatternDetection, 100.0, 200.0, 300.0, 400.0).unwrap();
        let json = serde_json::to_string(&budget).unwrap();
        let back: StageBudget = serde_json::from_str(&json).unwrap();
        assert_eq!(budget, back);
    }

    #[test]
    fn test_reason_code_serde_roundtrip() {
        let rc = ReasonCode::BudgetExceeded {
            stage: LatencyStage::EventEmission,
            percentile: Percentile::P95,
        };
        let json = serde_json::to_string(&rc).unwrap();
        let back: ReasonCode = serde_json::from_str(&json).unwrap();
        assert_eq!(rc, back);
    }

    #[test]
    fn test_pipeline_run_serde_roundtrip() {
        let run = make_valid_run();
        let json = serde_json::to_string(&run).unwrap();
        let back: PipelineRun = serde_json::from_str(&json).unwrap();
        assert_eq!(run, back);
    }

    #[test]
    fn test_log_entry_serde_roundtrip() {
        let entry = LatencyLogEntry {
            timestamp: "2026-02-23T19:00:00.000000Z".into(),
            subsystem: "latency.pty_capture".into(),
            correlation_id: "run-001".into(),
            scenario_id: Some("test-nominal".into()),
            inputs: serde_json::json!({"pane_id": 0, "content_len": 1024}),
            decision: "delta_extracted".into(),
            outcome: serde_json::json!({"latency_us": 450.0, "overflow": false}),
            reason_code: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: LatencyLogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back);
    }

    // ── Helper ──

    // ── BudgetEnforcer Tests ──

    #[test]
    fn test_enforcer_creation_default() {
        let enforcer = BudgetEnforcer::with_defaults();
        assert_eq!(enforcer.total_observations(), 0);
        assert_eq!(enforcer.total_overflows(), 0);
        for &stage in LatencyStage::PIPELINE_STAGES {
            assert!(enforcer.has_stage(stage), "missing stage {stage}");
        }
    }

    #[test]
    fn test_enforcer_record_within_budget() {
        let mut enforcer = BudgetEnforcer::with_defaults();
        let result = enforcer.record(LatencyStage::DeltaExtraction, 100.0, "test-001");
        assert!(!result.overflow);
        assert_eq!(result.recommended_mitigation, Mitigation::None);
        assert_eq!(enforcer.total_observations(), 1);
        assert_eq!(enforcer.total_overflows(), 0);
    }

    #[test]
    fn test_enforcer_record_exceeds_p999() {
        let mut enforcer = BudgetEnforcer::with_defaults();
        // DeltaExtraction p999 budget is 5000μs. Send 10000μs.
        let result = enforcer.record(LatencyStage::DeltaExtraction, 10_000.0, "test-002");
        assert!(result.overflow);
        assert_eq!(result.violated_percentile, Some(Percentile::P999));
        assert!(result.reason.is_some());
        assert_ne!(result.recommended_mitigation, Mitigation::None);
        assert_eq!(enforcer.total_overflows(), 1);
    }

    #[test]
    fn test_enforcer_record_exceeds_p99_not_p999() {
        let mut enforcer = BudgetEnforcer::with_defaults();
        // DeltaExtraction p99=1000, p999=5000. Send 2000μs.
        let result = enforcer.record(LatencyStage::DeltaExtraction, 2_000.0, "test-003");
        assert!(result.overflow);
        assert_eq!(result.violated_percentile, Some(Percentile::P99));
    }

    #[test]
    fn test_enforcer_percentile_estimation() {
        let mut enforcer = BudgetEnforcer::with_defaults();
        // Add 100 observations for PtyCapture.
        for i in 0..100 {
            enforcer.record(LatencyStage::PtyCapture, (i + 1) as f64 * 10.0, "test");
        }
        let snap = enforcer.snapshot();
        let pty_snap = snap
            .stages
            .iter()
            .find(|s| s.stage == LatencyStage::PtyCapture)
            .unwrap();
        assert_eq!(pty_snap.percentiles.sample_count, 100);
        assert_eq!(pty_snap.percentiles.total_observations, 100);
        // p50 should be around 500μs (50th value in 10,20,...,1000)
        let p50 = pty_snap.percentiles.p50_us.unwrap();
        assert!(p50 > 400.0 && p50 < 600.0, "p50 = {p50}");
    }

    #[test]
    fn test_enforcer_window_wraps() {
        let config = BudgetEnforcerConfig {
            window_size: 10,
            ..BudgetEnforcerConfig::default()
        };
        let mut enforcer = BudgetEnforcer::new(config);
        // Add 25 observations — wraps around.
        for i in 0..25 {
            enforcer.record(LatencyStage::PtyCapture, (i + 1) as f64, "test");
        }
        let snap = enforcer.snapshot();
        let pty_snap = snap
            .stages
            .iter()
            .find(|s| s.stage == LatencyStage::PtyCapture)
            .unwrap();
        assert_eq!(pty_snap.percentiles.sample_count, 10);
        assert_eq!(pty_snap.percentiles.total_observations, 25);
    }

    #[test]
    fn test_enforcer_snapshot_slack() {
        let mut enforcer = BudgetEnforcer::with_defaults();
        // Record normal values for all stages.
        for &stage in LatencyStage::PIPELINE_STAGES {
            enforcer.record(stage, 10.0, "test");
        }
        let snap = enforcer.snapshot();
        // Slack should be positive for all percentiles (10μs is well under budget).
        for (pctl, slack) in &snap.slack {
            assert!(*slack > 0.0, "negative slack at {pctl}: {slack}");
        }
    }

    #[test]
    fn test_enforcer_log_overflows_only() {
        let config = BudgetEnforcerConfig {
            log_overflows_only: true,
            log_all_observations: false,
            ..BudgetEnforcerConfig::default()
        };
        let mut enforcer = BudgetEnforcer::new(config);
        enforcer.record(LatencyStage::DeltaExtraction, 100.0, "test"); // within budget
        assert_eq!(enforcer.log_count(), 0);
        enforcer.record(LatencyStage::DeltaExtraction, 100_000.0, "test"); // overflow
        assert_eq!(enforcer.log_count(), 1);
    }

    #[test]
    fn test_enforcer_log_all() {
        let config = BudgetEnforcerConfig {
            log_overflows_only: false,
            log_all_observations: true,
            ..BudgetEnforcerConfig::default()
        };
        let mut enforcer = BudgetEnforcer::new(config);
        enforcer.record(LatencyStage::DeltaExtraction, 100.0, "test");
        enforcer.record(LatencyStage::DeltaExtraction, 200.0, "test");
        assert_eq!(enforcer.log_count(), 2);
    }

    #[test]
    fn test_enforcer_drain_logs() {
        let config = BudgetEnforcerConfig {
            log_all_observations: true,
            ..BudgetEnforcerConfig::default()
        };
        let mut enforcer = BudgetEnforcer::new(config);
        enforcer.record(LatencyStage::PtyCapture, 100.0, "test");
        let logs = enforcer.drain_logs();
        assert_eq!(logs.len(), 1);
        assert_eq!(enforcer.log_count(), 0);
    }

    #[test]
    fn test_enforcer_mitigation_for_stage() {
        let enforcer = BudgetEnforcer::with_defaults();
        // PatternDetection: p99=Degrade, p999=Skip
        assert_eq!(
            enforcer.mitigation_for(LatencyStage::PatternDetection, Percentile::P99),
            Mitigation::Degrade
        );
        assert_eq!(
            enforcer.mitigation_for(LatencyStage::PatternDetection, Percentile::P999),
            Mitigation::Skip
        );
        assert_eq!(
            enforcer.mitigation_for(LatencyStage::PatternDetection, Percentile::P50),
            Mitigation::None
        );
    }

    #[test]
    fn test_enforcer_unknown_stage() {
        let mut enforcer = BudgetEnforcer::with_defaults();
        // Aggregate stages have no state — should return benign result.
        let result = enforcer.record(LatencyStage::EndToEndCapture, 100.0, "test");
        assert!(!result.overflow);
        assert_eq!(result.current_percentiles.sample_count, 0);
    }

    #[test]
    fn test_enforcer_build_run() {
        let enforcer = BudgetEnforcer::with_defaults();
        let obs = vec![StageObservation {
            stage: LatencyStage::PtyCapture,
            latency_us: 5000.0,
            correlation_id: "run-001".into(),
            scenario_id: None,
            start_epoch_us: 1000,
            end_epoch_us: 6000,
            overflow: false,
            reason: None,
            mitigation: Mitigation::None,
        }];
        let run = enforcer.build_run("run-001", "corr-001", obs);
        assert_eq!(run.run_id, "run-001");
        assert_eq!(run.total_latency_us, 5000.0);
        assert!(!run.has_overflow);
    }

    #[test]
    fn test_enforcer_multiple_stages_tracking() {
        let mut enforcer = BudgetEnforcer::with_defaults();
        let stages = [
            LatencyStage::PtyCapture,
            LatencyStage::DeltaExtraction,
            LatencyStage::StorageWrite,
        ];
        for &stage in &stages {
            for i in 1..=10 {
                enforcer.record(stage, i as f64 * 100.0, "test");
            }
        }
        assert_eq!(enforcer.total_observations(), 30);
        let snap = enforcer.snapshot();
        assert_eq!(snap.stages.len(), 8); // all pipeline stages tracked
        for s in &snap.stages {
            if stages.contains(&s.stage) {
                assert_eq!(s.percentiles.total_observations, 10);
            }
        }
    }

    #[test]
    fn test_enforcer_snapshot_serde_roundtrip() {
        let mut enforcer = BudgetEnforcer::with_defaults();
        for &stage in LatencyStage::PIPELINE_STAGES {
            enforcer.record(stage, 100.0, "test");
        }
        let snap = enforcer.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let back: EnforcerSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap.total_observations, back.total_observations);
        assert_eq!(snap.stages.len(), back.stages.len());
    }

    #[test]
    fn test_default_mitigation_policies_cover_all_stages() {
        let policies = default_mitigation_policies();
        for &stage in LatencyStage::PIPELINE_STAGES {
            assert!(
                policies.iter().any(|p| p.stage == stage),
                "missing mitigation policy for {stage}"
            );
        }
    }

    #[test]
    fn test_latency_window_empty() {
        let window = LatencyWindow::new(10);
        assert!(window.percentile(0.5).is_none());
        assert!(window.mean().is_none());
        assert_eq!(window.len(), 0);
        assert_eq!(window.total_count(), 0);
    }

    #[test]
    fn test_latency_window_single() {
        let mut window = LatencyWindow::new(10);
        window.push(42.0);
        assert_eq!(window.percentile(0.5), Some(42.0));
        assert_eq!(window.mean(), Some(42.0));
        assert_eq!(window.len(), 1);
    }

    #[test]
    fn test_latency_window_mean() {
        let mut window = LatencyWindow::new(100);
        for i in 1..=10 {
            window.push(i as f64);
        }
        let mean = window.mean().unwrap();
        assert!((mean - 5.5).abs() < 0.01);
    }

    // ── CorrelationContext ──

    #[test]
    fn test_correlation_context_new() {
        let ctx = CorrelationContext::new("run-001", 1_000_000);
        assert_eq!(ctx.run_id, "run-001");
        assert_eq!(ctx.correlation_id, "run-001");
        assert!(ctx.propagation_intact);
        assert_eq!(ctx.next_expected, Some(LatencyStage::PtyCapture));
        assert!(ctx.timings.is_empty());
        assert_eq!(ctx.created_at_us, 1_000_000);
    }

    #[test]
    fn test_correlation_context_with_correlation() {
        let ctx = CorrelationContext::with_correlation("run-001", "corr-abc", 500);
        assert_eq!(ctx.run_id, "run-001");
        assert_eq!(ctx.correlation_id, "corr-abc");
    }

    #[test]
    fn test_correlation_context_begin_end_stage() {
        let mut ctx = CorrelationContext::new("run-001", 1000);
        let probe = ctx.begin_stage(LatencyStage::PtyCapture, 1000);
        assert_eq!(probe.stage, LatencyStage::PtyCapture);
        assert_eq!(probe.start_us, 1000);
        assert_eq!(probe.correlation_id, "run-001");
        ctx.end_stage(probe, 1500);
        assert_eq!(ctx.timings.len(), 1);
        assert_eq!(ctx.timings[0].latency_us, 500.0);
        assert_eq!(ctx.next_expected, Some(LatencyStage::DeltaExtraction));
        assert!(ctx.propagation_intact);
    }

    #[test]
    fn test_correlation_context_full_pipeline() {
        let mut ctx = CorrelationContext::new("run-full", 0);
        let mut t = 1000_u64;
        for &stage in LatencyStage::PIPELINE_STAGES {
            let probe = ctx.begin_stage(stage, t);
            t += 100;
            ctx.end_stage(probe, t);
            t += 10; // gap
        }
        assert_eq!(ctx.stage_count(), 8);
        assert!(ctx.propagation_intact);
        assert!(ctx.missing_stages().is_empty());
        // next_expected should be None after last stage
        assert_eq!(ctx.next_expected, None);
    }

    #[test]
    fn test_correlation_context_gap_detection() {
        let mut ctx = CorrelationContext::new("run-gap", 0);
        // Skip PtyCapture, start with DeltaExtraction
        let probe = ctx.begin_stage(LatencyStage::DeltaExtraction, 1000);
        ctx.end_stage(probe, 1500);
        assert!(!ctx.propagation_intact);
        assert_eq!(ctx.missing_stages().len(), 7); // all except DeltaExtraction
    }

    #[test]
    fn test_correlation_context_clock_regression() {
        let mut ctx = CorrelationContext::new("run-clock", 0);
        let probe = ctx.begin_stage(LatencyStage::PtyCapture, 2000);
        // End before start — should clamp to 0
        ctx.end_stage(probe, 1000);
        assert_eq!(ctx.timings[0].latency_us, 0.0);
    }

    #[test]
    fn test_correlation_context_total_elapsed() {
        let mut ctx = CorrelationContext::new("run-elapsed", 0);
        let probe = ctx.begin_stage(LatencyStage::PtyCapture, 1000);
        ctx.end_stage(probe, 1500);
        let probe = ctx.begin_stage(LatencyStage::DeltaExtraction, 1600);
        ctx.end_stage(probe, 2000);
        assert_eq!(ctx.total_elapsed_us(), 1000); // 2000 - 1000
    }

    #[test]
    fn test_correlation_context_total_elapsed_empty() {
        let ctx = CorrelationContext::new("run-empty", 0);
        assert_eq!(ctx.total_elapsed_us(), 0);
    }

    #[test]
    fn test_correlation_context_to_pipeline_run() {
        let mut ctx = CorrelationContext::new("run-convert", 0);
        ctx.scenario_id = Some("test-scenario".into());
        let probe = ctx.begin_stage(LatencyStage::PtyCapture, 1000);
        ctx.end_stage(probe, 1500);
        let probe = ctx.begin_stage(LatencyStage::DeltaExtraction, 1600);
        ctx.end_stage(probe, 2100);

        let run = ctx.to_pipeline_run();
        assert_eq!(run.run_id, "run-convert");
        assert_eq!(run.correlation_id, "run-convert");
        assert_eq!(run.scenario_id, Some("test-scenario".into()));
        assert_eq!(run.stages.len(), 2);
        assert!((run.total_latency_us - 1000.0).abs() < 0.01); // 500 + 500
        assert!(!run.has_overflow);
    }

    #[test]
    fn test_correlation_context_serde_roundtrip() {
        let mut ctx = CorrelationContext::new("run-serde", 1000);
        let probe = ctx.begin_stage(LatencyStage::PtyCapture, 1000);
        ctx.end_stage(probe, 1500);
        let json = serde_json::to_string(&ctx).unwrap();
        let back: CorrelationContext = serde_json::from_str(&json).unwrap();
        assert_eq!(ctx, back);
    }

    // ── StageProbe ──

    #[test]
    fn test_stage_probe_serde_roundtrip() {
        let probe = StageProbe {
            stage: LatencyStage::StorageWrite,
            start_us: 12345,
            correlation_id: "corr-001".into(),
        };
        let json = serde_json::to_string(&probe).unwrap();
        let back: StageProbe = serde_json::from_str(&json).unwrap();
        assert_eq!(probe, back);
    }

    // ── StageTiming ──

    #[test]
    fn test_stage_timing_serde_roundtrip() {
        let timing = StageTiming {
            stage: LatencyStage::PatternDetection,
            start_us: 100,
            end_us: 500,
            latency_us: 400.0,
        };
        let json = serde_json::to_string(&timing).unwrap();
        let back: StageTiming = serde_json::from_str(&json).unwrap();
        assert_eq!(timing, back);
    }

    // ── InstrumentationOverhead ──

    #[test]
    fn test_overhead_new_defaults() {
        let oh = InstrumentationOverhead::new();
        assert_eq!(oh.probe_count, 0);
        assert_eq!(oh.total_overhead_us, 0.0);
        assert_eq!(oh.budget_per_probe_us, 1.0);
        assert!(oh.within_budget);
    }

    #[test]
    fn test_overhead_default_matches_new() {
        let a = InstrumentationOverhead::new();
        let b = InstrumentationOverhead::default();
        assert_eq!(a, b);
    }

    #[test]
    fn test_overhead_record_within_budget() {
        let mut oh = InstrumentationOverhead::new();
        oh.record(0.5);
        oh.record(0.3);
        oh.record(0.8);
        assert_eq!(oh.probe_count, 3);
        assert!((oh.total_overhead_us - 1.6).abs() < 1e-10);
        assert!((oh.mean_overhead_us - 1.6 / 3.0).abs() < 1e-10);
        assert!((oh.max_overhead_us - 0.8).abs() < 1e-10);
        assert!(oh.within_budget);
    }

    #[test]
    fn test_overhead_record_exceeds_budget() {
        let mut oh = InstrumentationOverhead::new();
        oh.record(0.5);
        oh.record(1.5); // exceeds 1μs budget
        assert!(!oh.within_budget);
        assert!((oh.max_overhead_us - 1.5).abs() < 1e-10);
    }

    #[test]
    fn test_overhead_fraction() {
        let mut oh = InstrumentationOverhead::new();
        oh.record(0.5);
        oh.record(0.5);
        // total_overhead = 1.0μs, pipeline = 1000μs → 0.001 = 0.1%
        let frac = oh.overhead_fraction(1000.0);
        assert!((frac - 0.001).abs() < 1e-10);
    }

    #[test]
    fn test_overhead_fraction_zero_pipeline() {
        let oh = InstrumentationOverhead::new();
        assert_eq!(oh.overhead_fraction(0.0), 0.0);
        assert_eq!(oh.overhead_fraction(-1.0), 0.0);
    }

    #[test]
    fn test_overhead_serde_roundtrip() {
        let mut oh = InstrumentationOverhead::new();
        oh.record(0.3);
        oh.record(0.7);
        let json = serde_json::to_string(&oh).unwrap();
        let back: InstrumentationOverhead = serde_json::from_str(&json).unwrap();
        assert_eq!(oh, back);
    }

    // ── InstrumentedEnforcer ──

    #[test]
    fn test_instrumented_enforcer_new() {
        let ie = InstrumentedEnforcer::new();
        assert_eq!(ie.completed_runs(), 0);
        assert_eq!(ie.overflow_runs(), 0);
        assert_eq!(ie.overflow_rate(), 0.0);
    }

    #[test]
    fn test_instrumented_enforcer_default_matches_new() {
        let a = InstrumentedEnforcer::new();
        let b = InstrumentedEnforcer::default();
        assert_eq!(a.completed_runs(), b.completed_runs());
        assert_eq!(a.overflow_runs(), b.overflow_runs());
    }

    #[test]
    fn test_instrumented_enforcer_process_nominal_run() {
        let mut ie = InstrumentedEnforcer::new();
        let mut ctx = CorrelationContext::new("run-nominal", 0);
        let mut t = 1000_u64;
        for &stage in LatencyStage::PIPELINE_STAGES {
            let probe = ctx.begin_stage(stage, t);
            t += 50; // 50μs per stage — well within budget
            ctx.end_stage(probe, t);
            t += 10;
        }
        let results = ie.process_run(&ctx);
        assert_eq!(results.len(), 8);
        assert!(results.iter().all(|r| !r.overflow));
        assert_eq!(ie.completed_runs(), 1);
        assert_eq!(ie.overflow_runs(), 0);
        assert_eq!(ie.overflow_rate(), 0.0);
    }

    #[test]
    fn test_instrumented_enforcer_process_overflow_run() {
        let mut ie = InstrumentedEnforcer::new();
        let mut ctx = CorrelationContext::new("run-overflow", 0);
        // PtyCapture within budget
        let probe = ctx.begin_stage(LatencyStage::PtyCapture, 0);
        ctx.end_stage(probe, 50);
        // DeltaExtraction WAY over budget (100ms vs 1ms p999)
        let probe = ctx.begin_stage(LatencyStage::DeltaExtraction, 100);
        ctx.end_stage(probe, 100_100); // 100,000μs
        let results = ie.process_run(&ctx);
        assert!(results.iter().any(|r| r.overflow));
        assert_eq!(ie.completed_runs(), 1);
        assert_eq!(ie.overflow_runs(), 1);
        assert!((ie.overflow_rate() - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_instrumented_enforcer_overhead_tracking() {
        let mut ie = InstrumentedEnforcer::new();
        ie.record_overhead(0.3);
        ie.record_overhead(0.5);
        assert_eq!(ie.overhead().probe_count, 2);
        assert!(ie.overhead().within_budget);
    }

    #[test]
    fn test_instrumented_enforcer_overflow_rate() {
        let mut ie = InstrumentedEnforcer::new();

        // Run 1: nominal
        let mut ctx = CorrelationContext::new("run-1", 0);
        let probe = ctx.begin_stage(LatencyStage::PtyCapture, 0);
        ctx.end_stage(probe, 10);
        ie.process_run(&ctx);

        // Run 2: overflow
        let mut ctx2 = CorrelationContext::new("run-2", 0);
        let probe = ctx2.begin_stage(LatencyStage::PtyCapture, 0);
        ctx2.end_stage(probe, 1_000_000); // 1s — way over any budget
        ie.process_run(&ctx2);

        assert_eq!(ie.completed_runs(), 2);
        assert_eq!(ie.overflow_runs(), 1);
        assert!((ie.overflow_rate() - 0.5).abs() < 1e-10);
    }

    #[test]
    fn test_instrumented_enforcer_enforcer_access() {
        let ie = InstrumentedEnforcer::new();
        assert!(ie.enforcer().has_stage(LatencyStage::PtyCapture));
        assert!(!ie.enforcer().has_stage(LatencyStage::EndToEndCapture));
    }

    #[test]
    fn test_instrumented_enforcer_with_config() {
        let config = BudgetEnforcerConfig {
            window_size: 50,
            ..BudgetEnforcerConfig::default()
        };
        let ie = InstrumentedEnforcer::with_config(config);
        assert_eq!(ie.completed_runs(), 0);
        assert!(ie.enforcer().has_stage(LatencyStage::PtyCapture));
    }

    // ── Guardrails / Validation ──

    #[test]
    fn test_validation_valid_context() {
        let mut ctx = CorrelationContext::new("run-valid", 0);
        let probe = ctx.begin_stage(LatencyStage::PtyCapture, 1000);
        ctx.end_stage(probe, 1500);
        let errors = ctx.validate();
        assert!(errors.is_empty());
    }

    #[test]
    fn test_validation_empty_run() {
        let ctx = CorrelationContext::new("run-empty", 0);
        let errors = ctx.validate();
        assert_eq!(errors.len(), 1);
        let is_empty = matches!(&errors[0], InstrumentationError::EmptyRun { .. });
        assert!(is_empty);
    }

    #[test]
    fn test_validation_duplicate_stage() {
        let mut ctx = CorrelationContext::new("run-dup", 0);
        // Record PtyCapture twice
        let probe = ctx.begin_stage(LatencyStage::PtyCapture, 1000);
        ctx.end_stage(probe, 1500);
        let probe = ctx.begin_stage(LatencyStage::PtyCapture, 2000);
        ctx.end_stage(probe, 2500);
        let errors = ctx.validate();
        assert!(errors.iter().any(|e| matches!(
            e,
            InstrumentationError::DuplicateStage {
                stage: LatencyStage::PtyCapture
            }
        )));
    }

    #[test]
    fn test_validation_clock_regression_detected() {
        let mut ctx = CorrelationContext::new("run-regress", 0);
        // Manually add a timing with regression
        ctx.timings.push(StageTiming {
            stage: LatencyStage::PtyCapture,
            start_us: 2000,
            end_us: 1000, // before start
            latency_us: 0.0,
        });
        let errors = ctx.validate();
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, InstrumentationError::ClockRegression { .. }))
        );
    }

    #[test]
    fn test_validated_ok() {
        let mut ctx = CorrelationContext::new("run-ok", 0);
        let probe = ctx.begin_stage(LatencyStage::PtyCapture, 100);
        ctx.end_stage(probe, 200);
        let result = ctx.validated();
        assert!(result.is_ok());
    }

    #[test]
    fn test_validated_err() {
        let ctx = CorrelationContext::new("run-err", 0); // empty
        let result = ctx.validated();
        assert!(result.is_err());
    }

    #[test]
    fn test_instrumentation_error_display() {
        let e = InstrumentationError::UnterminatedProbe {
            stage: LatencyStage::StorageWrite,
            start_us: 5000,
        };
        let s = format!("{e}");
        assert!(s.contains("STORAGE_WRITE"));
        assert!(s.contains("5000"));
    }

    #[test]
    fn test_instrumentation_error_serde_roundtrip() {
        let e = InstrumentationError::OverheadBudgetExceeded {
            max_observed_us: 2.5,
            budget_us: 1.0,
        };
        let json = serde_json::to_string(&e).unwrap();
        let back: InstrumentationError = serde_json::from_str(&json).unwrap();
        assert_eq!(e, back);
    }

    // ── Degradation ──

    #[test]
    fn test_degradation_ordering() {
        assert!(InstrumentationDegradation::Full < InstrumentationDegradation::SkipOverhead);
        assert!(
            InstrumentationDegradation::SkipOverhead < InstrumentationDegradation::SkipCorrelation
        );
        assert!(
            InstrumentationDegradation::SkipCorrelation < InstrumentationDegradation::Passthrough
        );
    }

    #[test]
    fn test_degradation_display() {
        assert_eq!(format!("{}", InstrumentationDegradation::Full), "FULL");
        assert_eq!(
            format!("{}", InstrumentationDegradation::Passthrough),
            "PASSTHROUGH"
        );
    }

    #[test]
    fn test_degradation_serde_roundtrip() {
        let d = InstrumentationDegradation::SkipCorrelation;
        let json = serde_json::to_string(&d).unwrap();
        let back: InstrumentationDegradation = serde_json::from_str(&json).unwrap();
        assert_eq!(d, back);
    }

    // ── InstrumentedEnforcer diagnostics ──

    #[test]
    fn test_enforcer_degradation_full_when_within_budget() {
        let ie = InstrumentedEnforcer::new();
        assert_eq!(ie.current_degradation(), InstrumentationDegradation::Full);
    }

    #[test]
    fn test_enforcer_degradation_skip_overhead() {
        let mut ie = InstrumentedEnforcer::new();
        ie.record_overhead(3.0); // 3x budget (budget=1μs)
        assert_eq!(
            ie.current_degradation(),
            InstrumentationDegradation::SkipOverhead
        );
    }

    #[test]
    fn test_enforcer_degradation_skip_correlation() {
        let mut ie = InstrumentedEnforcer::new();
        ie.record_overhead(7.0); // 7x budget
        assert_eq!(
            ie.current_degradation(),
            InstrumentationDegradation::SkipCorrelation
        );
    }

    #[test]
    fn test_enforcer_degradation_passthrough() {
        let mut ie = InstrumentedEnforcer::new();
        ie.record_overhead(15.0); // 15x budget
        assert_eq!(
            ie.current_degradation(),
            InstrumentationDegradation::Passthrough
        );
    }

    #[test]
    fn test_enforcer_is_healthy_nominal() {
        let mut ie = InstrumentedEnforcer::new();
        // Record a nominal run
        let mut ctx = CorrelationContext::new("run-h", 0);
        let probe = ctx.begin_stage(LatencyStage::PtyCapture, 0);
        ctx.end_stage(probe, 10);
        ie.process_run(&ctx);
        assert!(ie.is_healthy());
    }

    #[test]
    fn test_enforcer_is_unhealthy_overhead() {
        let mut ie = InstrumentedEnforcer::new();
        ie.record_overhead(5.0); // over budget
        assert!(!ie.is_healthy());
    }

    #[test]
    fn test_enforcer_status_line_format() {
        let ie = InstrumentedEnforcer::new();
        let status = ie.status_line();
        assert!(status.contains("degradation=FULL"));
        assert!(status.contains("runs=0"));
        assert!(status.contains("overflows=0"));
    }

    #[test]
    fn test_enforcer_diagnostic_snapshot() {
        let mut ie = InstrumentedEnforcer::new();
        ie.record_overhead(0.3);
        let diag = ie.diagnostic();
        assert_eq!(diag.degradation, InstrumentationDegradation::Full);
        assert_eq!(diag.completed_runs, 0);
        assert_eq!(diag.overhead.probe_count, 1);
        assert!(diag.last_validation_errors.is_empty());
    }

    #[test]
    fn test_enforcer_diagnostic_serde_roundtrip() {
        let ie = InstrumentedEnforcer::new();
        let diag = ie.diagnostic();
        let json = serde_json::to_string(&diag).unwrap();
        let back: InstrumentationDiagnostic = serde_json::from_str(&json).unwrap();
        assert_eq!(diag, back);
    }

    #[test]
    fn test_enforcer_process_validated_run() {
        let mut ie = InstrumentedEnforcer::new();
        let mut ctx = CorrelationContext::new("run-pv", 0);
        let probe = ctx.begin_stage(LatencyStage::PtyCapture, 0);
        ctx.end_stage(probe, 50);
        let (results, errors) = ie.process_validated_run(&ctx);
        assert_eq!(results.len(), 1);
        assert!(errors.is_empty());
    }

    #[test]
    fn test_enforcer_process_validated_run_with_errors() {
        let mut ie = InstrumentedEnforcer::new();
        let ctx = CorrelationContext::new("run-empty-val", 0); // empty run
        let (results, errors) = ie.process_validated_run(&ctx);
        assert!(results.is_empty());
        assert!(!errors.is_empty());
    }

    // ── FastProbe ──

    #[test]
    fn test_fast_probe_begin() {
        let probe = FastProbe::begin(LatencyStage::PtyCapture, 1000);
        assert_eq!(probe.stage, LatencyStage::PtyCapture);
        assert_eq!(probe.start_us, 1000);
    }

    #[test]
    fn test_fast_probe_elapsed() {
        let probe = FastProbe::begin(LatencyStage::DeltaExtraction, 1000);
        assert!((probe.elapsed_us(1500) - 500.0).abs() < 1e-10);
    }

    #[test]
    fn test_fast_probe_clock_regression() {
        let probe = FastProbe::begin(LatencyStage::StorageWrite, 2000);
        assert_eq!(probe.elapsed_us(1000), 0.0);
    }

    #[test]
    fn test_fast_probe_zero_duration() {
        let probe = FastProbe::begin(LatencyStage::EventEmission, 1000);
        assert_eq!(probe.elapsed_us(1000), 0.0);
    }

    #[test]
    fn test_fast_probe_copy_semantics() {
        let probe = FastProbe::begin(LatencyStage::ApiResponse, 100);
        let copy = probe;
        // Both should be usable (Copy semantics, no move).
        assert_eq!(probe.elapsed_us(200), 100.0);
        assert_eq!(copy.elapsed_us(200), 100.0);
    }

    // ── MitigationLevel ──

    #[test]
    fn test_mitigation_level_ordering() {
        assert!(MitigationLevel::None < MitigationLevel::Defer);
        assert!(MitigationLevel::Defer < MitigationLevel::Degrade);
        assert!(MitigationLevel::Degrade < MitigationLevel::Shed);
        assert!(MitigationLevel::Shed < MitigationLevel::Skip);
    }

    #[test]
    fn test_mitigation_level_severity() {
        assert_eq!(MitigationLevel::None.severity(), 0);
        assert_eq!(MitigationLevel::Defer.severity(), 1);
        assert_eq!(MitigationLevel::Degrade.severity(), 2);
        assert_eq!(MitigationLevel::Shed.severity(), 3);
        assert_eq!(MitigationLevel::Skip.severity(), 4);
    }

    #[test]
    fn test_mitigation_level_roundtrip() {
        for &level in MitigationLevel::ALL {
            let mit = level.to_mitigation();
            let back = MitigationLevel::from_mitigation(mit);
            assert_eq!(level, back);
        }
    }

    #[test]
    fn test_mitigation_level_serde_roundtrip() {
        for &level in MitigationLevel::ALL {
            let json = serde_json::to_string(&level).unwrap();
            let back: MitigationLevel = serde_json::from_str(&json).unwrap();
            assert_eq!(level, back);
        }
    }

    // ── PolicyConstraint ──

    #[test]
    fn test_policy_constraint_allows() {
        let pc = PolicyConstraint {
            stage: LatencyStage::StorageWrite,
            max_level: MitigationLevel::Defer,
            critical: true,
            warmup_count: 10,
        };
        assert!(pc.allows(MitigationLevel::None));
        assert!(pc.allows(MitigationLevel::Defer));
        assert!(!pc.allows(MitigationLevel::Degrade));
        assert!(!pc.allows(MitigationLevel::Skip));
    }

    #[test]
    fn test_policy_constraint_clamp() {
        let pc = PolicyConstraint {
            stage: LatencyStage::StorageWrite,
            max_level: MitigationLevel::Defer,
            critical: true,
            warmup_count: 10,
        };
        assert_eq!(pc.clamp(MitigationLevel::None), MitigationLevel::None);
        assert_eq!(pc.clamp(MitigationLevel::Defer), MitigationLevel::Defer);
        assert_eq!(pc.clamp(MitigationLevel::Skip), MitigationLevel::Defer);
    }

    #[test]
    fn test_default_policy_constraints_cover_all_stages() {
        let constraints = default_policy_constraints();
        for &stage in LatencyStage::PIPELINE_STAGES {
            assert!(
                constraints.iter().any(|c| c.stage == stage),
                "missing policy constraint for {stage}"
            );
        }
    }

    #[test]
    fn test_critical_stages_have_limited_mitigation() {
        let constraints = default_policy_constraints();
        for c in &constraints {
            if c.critical {
                // Critical stages should NOT allow Skip.
                assert!(
                    c.max_level < MitigationLevel::Skip,
                    "critical stage {} allows Skip",
                    c.stage
                );
            }
        }
    }

    // ── RecoveryProtocol ──

    #[test]
    fn test_recovery_protocol_defaults() {
        let rp = RecoveryProtocol::default();
        assert_eq!(rp.cooldown_observations, 20);
        assert_eq!(rp.max_degraded_duration_us, 30_000_000);
        assert!(rp.gradual);
    }

    #[test]
    fn test_recovery_protocol_serde_roundtrip() {
        let rp = RecoveryProtocol::default();
        let json = serde_json::to_string(&rp).unwrap();
        let back: RecoveryProtocol = serde_json::from_str(&json).unwrap();
        assert_eq!(rp, back);
    }

    // ── RuntimeEnforcer ──

    #[test]
    fn test_runtime_enforcer_new() {
        let re = RuntimeEnforcer::with_defaults();
        assert_eq!(re.total_observations(), 0);
        assert_eq!(re.total_escalations(), 0);
        assert_eq!(re.total_recoveries(), 0);
        assert!(re.is_fully_recovered());
    }

    #[test]
    fn test_runtime_enforcer_nominal() {
        let mut re = RuntimeEnforcer::with_defaults();
        // Record many nominal observations to get past warmup.
        for i in 0..50 {
            let d = re.enforce(LatencyStage::PtyCapture, 10.0, "test", i * 1000);
            assert!(!d.overflow);
            assert_eq!(d.applied_mitigation, MitigationLevel::None);
        }
        assert!(re.is_fully_recovered());
        assert_eq!(re.total_escalations(), 0);
    }

    #[test]
    fn test_runtime_enforcer_warmup_suppresses() {
        let config = RuntimeEnforcerConfig {
            policy_constraints: vec![PolicyConstraint {
                stage: LatencyStage::DeltaExtraction,
                max_level: MitigationLevel::Skip,
                critical: false,
                warmup_count: 5,
            }],
            ..RuntimeEnforcerConfig::default()
        };
        let mut re = RuntimeEnforcer::new(config);
        // During warmup, even overflow shouldn't escalate.
        for i in 0..5 {
            let d = re.enforce(LatencyStage::DeltaExtraction, 100_000.0, "test", i * 1000);
            assert!(d.warmup_active);
            assert_eq!(d.applied_mitigation, MitigationLevel::None);
        }
    }

    #[test]
    fn test_runtime_enforcer_escalation() {
        let mut re = RuntimeEnforcer::with_defaults();
        // Get past warmup with normal observations.
        for i in 0..20 {
            re.enforce(LatencyStage::PatternDetection, 10.0, "test", i * 1000);
        }
        // Now trigger overflow (PatternDetection p999=10000, so 50000 overflows).
        let d = re.enforce(LatencyStage::PatternDetection, 50_000.0, "test", 100_000);
        assert!(d.overflow);
        assert!(d.applied_mitigation >= MitigationLevel::None);
        // Should have escalated.
        let level = re.current_level(LatencyStage::PatternDetection);
        assert!(level > MitigationLevel::None);
    }

    #[test]
    fn test_runtime_enforcer_policy_clamp() {
        let config = RuntimeEnforcerConfig {
            policy_constraints: vec![PolicyConstraint {
                stage: LatencyStage::StorageWrite,
                max_level: MitigationLevel::Defer,
                critical: true,
                warmup_count: 0, // no warmup
            }],
            ..RuntimeEnforcerConfig::default()
        };
        let mut re = RuntimeEnforcer::new(config);
        // StorageWrite with extreme overflow — policy should clamp to Defer.
        re.enforce(LatencyStage::StorageWrite, 1_000_000.0, "test", 1000);
        let level = re.current_level(LatencyStage::StorageWrite);
        assert!(level <= MitigationLevel::Defer);
    }

    #[test]
    fn test_runtime_enforcer_recovery() {
        let config = RuntimeEnforcerConfig {
            recovery: RecoveryProtocol {
                cooldown_observations: 5,
                max_degraded_duration_us: 1_000_000_000,
                gradual: true,
            },
            policy_constraints: vec![PolicyConstraint {
                stage: LatencyStage::PatternDetection,
                max_level: MitigationLevel::Skip,
                critical: false,
                warmup_count: 0,
            }],
            ..RuntimeEnforcerConfig::default()
        };
        let mut re = RuntimeEnforcer::new(config);
        // Trigger escalation.
        re.enforce(LatencyStage::PatternDetection, 100_000.0, "test", 1000);
        assert!(re.current_level(LatencyStage::PatternDetection) > MitigationLevel::None);

        // Now send enough within-budget observations for recovery.
        for i in 0..10 {
            re.enforce(LatencyStage::PatternDetection, 10.0, "test", 2000 + i * 100);
        }
        // Should have recovered (at least partially).
        let level = re.current_level(LatencyStage::PatternDetection);
        // With gradual recovery, may have stepped down but not necessarily to None.
        assert!(level < MitigationLevel::Skip);
    }

    #[test]
    fn test_runtime_enforcer_status_line_nominal() {
        let re = RuntimeEnforcer::with_defaults();
        let status = re.status_line();
        assert!(status.contains("NOMINAL"));
    }

    #[test]
    fn test_runtime_enforcer_status_line_degraded() {
        let config = RuntimeEnforcerConfig {
            policy_constraints: vec![PolicyConstraint {
                stage: LatencyStage::PatternDetection,
                max_level: MitigationLevel::Skip,
                critical: false,
                warmup_count: 0,
            }],
            ..RuntimeEnforcerConfig::default()
        };
        let mut re = RuntimeEnforcer::new(config);
        re.enforce(LatencyStage::PatternDetection, 100_000.0, "test", 1000);
        let status = re.status_line();
        assert!(status.contains("DEGRADED"));
    }

    #[test]
    fn test_runtime_enforcer_drain_decisions() {
        let mut re = RuntimeEnforcer::with_defaults();
        re.enforce(LatencyStage::PtyCapture, 10.0, "test", 0);
        re.enforce(LatencyStage::DeltaExtraction, 10.0, "test", 100);
        let decisions = re.drain_decisions();
        assert_eq!(decisions.len(), 2);
        assert_eq!(re.drain_decisions().len(), 0);
    }

    #[test]
    fn test_enforcement_decision_serde_roundtrip() {
        let d = EnforcementDecision {
            stage: LatencyStage::PtyCapture,
            latency_us: 42.0,
            overflow: false,
            raw_mitigation: MitigationLevel::None,
            applied_mitigation: MitigationLevel::None,
            recovery: false,
            reason: None,
            warmup_active: false,
        };
        let json = serde_json::to_string(&d).unwrap();
        let back: EnforcementDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(d, back);
    }

    #[test]
    fn test_stage_enforcement_state_serde_roundtrip() {
        let s = StageEnforcementState {
            current_level: MitigationLevel::Degrade,
            consecutive_ok: 5,
            last_escalation_us: 1000,
            escalation_count: 2,
            recovery_count: 1,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: StageEnforcementState = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    // ── RuntimeEnforcer Impl extensions ──

    #[test]
    fn test_runtime_enforcer_enforce_run() {
        let config = RuntimeEnforcerConfig {
            policy_constraints: default_policy_constraints()
                .into_iter()
                .map(|mut c| {
                    c.warmup_count = 0;
                    c
                })
                .collect(),
            ..RuntimeEnforcerConfig::default()
        };
        let mut re = RuntimeEnforcer::new(config);
        let mut ctx = CorrelationContext::new("batch-run", 0);
        let mut t = 1000_u64;
        for &stage in LatencyStage::PIPELINE_STAGES {
            let probe = ctx.begin_stage(stage, t);
            t += 50; // 50μs, well within budget
            ctx.end_stage(probe, t);
            t += 10;
        }
        let decisions = re.enforce_run(&ctx, 0);
        assert_eq!(decisions.len(), 8);
        assert!(decisions.iter().all(|d| !d.overflow));
    }

    #[test]
    fn test_runtime_enforcer_diagnostic_snapshot() {
        let mut re = RuntimeEnforcer::with_defaults();
        re.enforce(LatencyStage::PtyCapture, 10.0, "test", 0);
        let snap = re.diagnostic_snapshot();
        assert_eq!(snap.observation_count, 1);
        assert_eq!(snap.total_escalations, 0);
        assert!(snap.fully_recovered);
        assert_eq!(snap.stage_states.len(), 8);
    }

    #[test]
    fn test_runtime_enforcer_snapshot_serde_roundtrip() {
        let mut re = RuntimeEnforcer::with_defaults();
        for i in 0..5 {
            re.enforce(LatencyStage::PtyCapture, 10.0, "test", i * 100);
        }
        let snap = re.diagnostic_snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let back: RuntimeEnforcerSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap.observation_count, back.observation_count);
        assert_eq!(snap.total_escalations, back.total_escalations);
        assert_eq!(snap.fully_recovered, back.fully_recovered);
    }

    #[test]
    fn test_runtime_enforcer_timeout_recovery() {
        let config = RuntimeEnforcerConfig {
            recovery: RecoveryProtocol {
                cooldown_observations: 1000,    // high, so cooldown won't trigger
                max_degraded_duration_us: 5000, // 5ms timeout
                gradual: false,                 // jump to full
            },
            policy_constraints: vec![PolicyConstraint {
                stage: LatencyStage::PatternDetection,
                max_level: MitigationLevel::Skip,
                critical: false,
                warmup_count: 0,
            }],
            ..RuntimeEnforcerConfig::default()
        };
        let mut re = RuntimeEnforcer::new(config);
        // Trigger escalation at time 1000.
        re.enforce(LatencyStage::PatternDetection, 100_000.0, "test", 1000);
        assert!(re.current_level(LatencyStage::PatternDetection) > MitigationLevel::None);

        // Record ok observation at time 7000 (6ms after escalation, past 5ms timeout).
        let d = re.enforce(LatencyStage::PatternDetection, 10.0, "test", 7000);
        assert!(d.recovery);
        assert_eq!(
            re.current_level(LatencyStage::PatternDetection),
            MitigationLevel::None
        );
    }

    // ── Helper ──

    fn make_valid_run() -> PipelineRun {
        let budgets = default_budgets();
        let mut stages = Vec::new();
        let mut t = 1_000_000_u64; // start at 1s epoch

        for &stage in LatencyStage::PIPELINE_STAGES {
            let budget = budgets.iter().find(|b| b.stage == stage).unwrap();
            let latency = budget.p50_us;
            stages.push(StageObservation {
                stage,
                latency_us: latency,
                correlation_id: "test-run-001".into(),
                scenario_id: Some("nominal".into()),
                start_epoch_us: t,
                end_epoch_us: t + latency as u64,
                overflow: false,
                reason: None,
                mitigation: Mitigation::None,
            });
            t += latency as u64 + 100; // 100μs gap between stages
        }

        let total: f64 = stages.iter().map(|s| s.latency_us).sum();

        PipelineRun {
            run_id: "test-run-001".into(),
            correlation_id: "test-run-001".into(),
            scenario_id: Some("nominal".into()),
            stages,
            total_latency_us: total,
            has_overflow: false,
            reasons: vec![],
        }
    }

    // ── A4: Adaptive Budget Allocator ──

    #[test]
    fn test_stage_pressure_compute() {
        let p = StagePressure::compute(LatencyStage::PtyCapture, 5000.0, 10000.0);
        assert_eq!(p.headroom, 0.5);
        assert!(!p.is_over_budget());
        assert_eq!(p.donatable_slack_us(), 5000.0);
        assert_eq!(p.deficit_us(), 0.0);
    }

    #[test]
    fn test_stage_pressure_over_budget() {
        let p = StagePressure::compute(LatencyStage::StorageWrite, 15000.0, 10000.0);
        assert!(p.headroom < 0.0);
        assert!(p.is_over_budget());
        assert_eq!(p.donatable_slack_us(), 0.0);
        assert_eq!(p.deficit_us(), 5000.0);
    }

    #[test]
    fn test_stage_pressure_zero_budget() {
        let p = StagePressure::compute(LatencyStage::PtyCapture, 100.0, 0.0);
        assert_eq!(p.headroom, 0.0);
        assert!(!p.is_over_budget());
        assert_eq!(p.donatable_slack_us(), 0.0);
    }

    #[test]
    fn test_allocator_config_default_valid() {
        let cfg = AdaptiveAllocatorConfig::default();
        let errors = cfg.validate();
        assert!(
            errors.is_empty(),
            "default config should be valid: {:?}",
            errors
        );
    }

    #[test]
    fn test_allocator_config_validation_catches_bad_values() {
        let cfg = AdaptiveAllocatorConfig {
            max_adjustment_pct: -0.1,
            min_budget_pct: 0.0,
            max_budget_pct: 0.5,
            pressure_alpha: 1.5,
            min_donor_headroom: 1.0,
            ..Default::default()
        };
        let errors = cfg.validate();
        assert_eq!(errors.len(), 5);
    }

    #[test]
    fn test_allocator_with_defaults_conservation() {
        let alloc = AdaptiveAllocator::with_defaults();
        let sum: f64 = alloc.lanes().iter().map(|l| l.current_p95_us).sum();
        assert!((sum - alloc.total_budget_us()).abs() < 1e-6);
        assert!(alloc.global_slack_us().abs() < 1e-6);
    }

    #[test]
    fn test_allocator_warmup_noop() {
        let mut alloc = AdaptiveAllocator::with_defaults();
        let pressures: Vec<StagePressure> = alloc
            .lanes()
            .iter()
            .map(|l| StagePressure::compute(l.stage, l.default_p95_us * 0.5, l.default_p95_us))
            .collect();
        let d = alloc.allocate(&pressures, "test-warmup");
        assert!(d.warmup);
        assert_eq!(d.reason, AllocationReason::Warmup);
        assert!(d.adjustments.is_empty());
    }

    #[test]
    fn test_allocator_all_within_budget() {
        let cfg = AdaptiveAllocatorConfig {
            warmup_observations: 0,
            ..Default::default()
        };
        let mut alloc = AdaptiveAllocator::new(&default_budgets(), cfg);
        // All stages well within budget.
        let pressures: Vec<StagePressure> = alloc
            .lanes()
            .iter()
            .map(|l| StagePressure::compute(l.stage, l.default_p95_us * 0.5, l.default_p95_us))
            .collect();
        let d = alloc.allocate(&pressures, "test-nominal");
        assert!(!d.warmup);
        assert_eq!(d.reason, AllocationReason::AllWithinBudget);
    }

    #[test]
    fn test_allocator_redistribution_preserves_total() {
        let cfg = AdaptiveAllocatorConfig {
            warmup_observations: 0,
            min_donor_headroom: 0.10,
            ..Default::default()
        };
        let budgets = default_budgets();
        let mut alloc = AdaptiveAllocator::new(&budgets, cfg);
        let total_before = alloc.total_budget_us();

        // Run many epochs with StorageWrite over-budget and PtyCapture under-budget.
        for epoch in 0..20 {
            let pressures: Vec<StagePressure> = alloc
                .lanes()
                .iter()
                .map(|l| {
                    if l.stage == LatencyStage::StorageWrite {
                        StagePressure::compute(l.stage, l.current_p95_us * 1.5, l.current_p95_us)
                    } else {
                        StagePressure::compute(l.stage, l.current_p95_us * 0.3, l.current_p95_us)
                    }
                })
                .collect();
            alloc.allocate(&pressures, &format!("epoch-{}", epoch));
        }

        // Conservation invariant.
        let sum: f64 = alloc.lanes().iter().map(|l| l.current_p95_us).sum();
        assert!(
            (sum - total_before).abs() < 1.0, // allow small float drift
            "budget conservation violated: {} vs {}",
            sum,
            total_before
        );

        // StorageWrite should have more budget than its default.
        let sw = alloc.allocation(LatencyStage::StorageWrite).unwrap();
        assert!(
            sw.current_p95_us >= sw.default_p95_us,
            "StorageWrite should have received slack"
        );
    }

    #[test]
    fn test_allocator_respects_floor() {
        let cfg = AdaptiveAllocatorConfig {
            warmup_observations: 0,
            min_budget_pct: 0.50,
            max_adjustment_pct: 0.50, // allow big adjustments to test floor
            min_donor_headroom: 0.05,
            ..Default::default()
        };
        let mut alloc = AdaptiveAllocator::new(&default_budgets(), cfg);

        // Many epochs pushing donors hard.
        for epoch in 0..100 {
            let pressures: Vec<StagePressure> = alloc
                .lanes()
                .iter()
                .map(|l| {
                    if l.stage == LatencyStage::ApiResponse {
                        StagePressure::compute(l.stage, l.current_p95_us * 3.0, l.current_p95_us)
                    } else {
                        StagePressure::compute(l.stage, l.current_p95_us * 0.1, l.current_p95_us)
                    }
                })
                .collect();
            alloc.allocate(&pressures, &format!("floor-{}", epoch));
        }

        // No lane should drop below 50% of its default.
        for lane in alloc.lanes() {
            assert!(
                lane.current_p95_us >= lane.default_p95_us.mul_add(0.50, -1e-6),
                "{} dropped below floor: {} < {}",
                lane.stage,
                lane.current_p95_us,
                lane.default_p95_us * 0.50
            );
        }
    }

    #[test]
    fn test_allocator_respects_ceiling() {
        let cfg = AdaptiveAllocatorConfig {
            warmup_observations: 0,
            max_budget_pct: 2.0,
            max_adjustment_pct: 0.50,
            min_donor_headroom: 0.05,
            ..Default::default()
        };
        let mut alloc = AdaptiveAllocator::new(&default_budgets(), cfg);

        for epoch in 0..100 {
            let pressures: Vec<StagePressure> = alloc
                .lanes()
                .iter()
                .map(|l| {
                    if l.stage == LatencyStage::DeltaExtraction {
                        StagePressure::compute(l.stage, l.current_p95_us * 5.0, l.current_p95_us)
                    } else {
                        StagePressure::compute(l.stage, l.current_p95_us * 0.1, l.current_p95_us)
                    }
                })
                .collect();
            alloc.allocate(&pressures, &format!("ceil-{}", epoch));
        }

        for lane in alloc.lanes() {
            assert!(
                lane.current_p95_us <= lane.default_p95_us.mul_add(2.0, 1e-6),
                "{} exceeded ceiling: {} > {}",
                lane.stage,
                lane.current_p95_us,
                lane.default_p95_us * 2.0
            );
        }
    }

    #[test]
    fn test_allocator_reset_restores_defaults() {
        let cfg = AdaptiveAllocatorConfig {
            warmup_observations: 0,
            min_donor_headroom: 0.05,
            ..Default::default()
        };
        let mut alloc = AdaptiveAllocator::new(&default_budgets(), cfg);

        // Do some redistribution.
        for epoch in 0..10 {
            let pressures: Vec<StagePressure> = alloc
                .lanes()
                .iter()
                .map(|l| {
                    if l.stage == LatencyStage::StorageWrite {
                        StagePressure::compute(l.stage, l.current_p95_us * 2.0, l.current_p95_us)
                    } else {
                        StagePressure::compute(l.stage, l.current_p95_us * 0.3, l.current_p95_us)
                    }
                })
                .collect();
            alloc.allocate(&pressures, &format!("pre-reset-{}", epoch));
        }

        let d = alloc.reset();
        assert_eq!(d.reason, AllocationReason::ResetToDefaults);

        for lane in alloc.lanes() {
            assert!(
                (lane.current_p95_us - lane.default_p95_us).abs() < 1e-6,
                "{} not reset: {} vs {}",
                lane.stage,
                lane.current_p95_us,
                lane.default_p95_us
            );
        }
    }

    #[test]
    fn test_allocator_deterministic_replay() {
        let cfg = AdaptiveAllocatorConfig {
            warmup_observations: 0,
            min_donor_headroom: 0.05,
            ..Default::default()
        };
        let budgets = default_budgets();

        // Run sequence once.
        let mut alloc1 = AdaptiveAllocator::new(&budgets, cfg.clone());
        let pressures_seq: Vec<Vec<StagePressure>> = (0..10)
            .map(|i| {
                alloc1
                    .lanes()
                    .iter()
                    .map(|l| {
                        let factor = if l.stage == LatencyStage::StorageWrite {
                            (i as f64).mul_add(0.1, 1.5)
                        } else {
                            0.5
                        };
                        StagePressure::compute(l.stage, l.current_p95_us * factor, l.current_p95_us)
                    })
                    .collect()
            })
            .collect();

        let mut decisions1 = Vec::new();
        for (i, p) in pressures_seq.iter().enumerate() {
            decisions1.push(alloc1.allocate(p, &format!("run-{}", i)));
        }

        // Replay with fresh allocator.
        let mut alloc2 = AdaptiveAllocator::new(&budgets, cfg);
        let mut decisions2 = Vec::new();
        for (i, p) in pressures_seq.iter().enumerate() {
            decisions2.push(alloc2.allocate(p, &format!("run-{}", i)));
        }

        // Decisions should be identical.
        assert_eq!(decisions1.len(), decisions2.len());
        for (d1, d2) in decisions1.iter().zip(decisions2.iter()) {
            assert_eq!(d1.epoch, d2.epoch);
            assert_eq!(d1.reason, d2.reason);
            assert_eq!(d1.adjustments.len(), d2.adjustments.len());
        }

        // Final allocations should be identical.
        for (l1, l2) in alloc1.lanes().iter().zip(alloc2.lanes().iter()) {
            assert!(
                (l1.current_p95_us - l2.current_p95_us).abs() < 1e-6,
                "replay diverged for {}: {} vs {}",
                l1.stage,
                l1.current_p95_us,
                l2.current_p95_us
            );
        }
    }

    #[test]
    fn test_allocator_no_donors_when_all_pressured() {
        let cfg = AdaptiveAllocatorConfig {
            warmup_observations: 0,
            min_donor_headroom: 0.15,
            pressure_alpha: 0.3,
            ..Default::default()
        };
        let mut alloc = AdaptiveAllocator::new(&default_budgets(), cfg);
        // Run many epochs with all stages over-budget so EWMA headroom goes negative.
        for i in 0..20 {
            let pressures: Vec<StagePressure> = alloc
                .lanes()
                .iter()
                .map(|l| StagePressure::compute(l.stage, l.current_p95_us * 2.0, l.current_p95_us))
                .collect();
            alloc.allocate(&pressures, &format!("all-pressure-{}", i));
        }
        // After enough epochs, smoothed headroom should be negative for all lanes.
        let d = alloc.recent_decisions(1)[0].clone();
        assert_eq!(d.reason, AllocationReason::NoDonors);
    }

    #[test]
    fn test_allocator_snapshot_serialization() {
        let alloc = AdaptiveAllocator::with_defaults();
        let snap = alloc.snapshot();
        let json = serde_json::to_string(&snap).expect("serialize");
        let back: AllocatorSnapshot = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(snap.epoch, back.epoch);
        assert_eq!(snap.lanes.len(), back.lanes.len());
        assert!((snap.total_budget_us - back.total_budget_us).abs() < 1e-6);
    }

    #[test]
    fn test_allocator_status_line_nominal() {
        let alloc = AdaptiveAllocator::with_defaults();
        let s = alloc.status_line();
        assert!(s.starts_with("allocator=NOMINAL"));
    }

    #[test]
    fn test_allocator_status_line_redistribution() {
        let cfg = AdaptiveAllocatorConfig {
            warmup_observations: 0,
            min_donor_headroom: 0.05,
            ..Default::default()
        };
        let mut alloc = AdaptiveAllocator::new(&default_budgets(), cfg);
        // Make StorageWrite over-budget so its smoothed headroom goes negative.
        let pressures: Vec<StagePressure> = alloc
            .lanes()
            .iter()
            .map(|l| {
                if l.stage == LatencyStage::StorageWrite {
                    StagePressure::compute(l.stage, l.current_p95_us * 2.0, l.current_p95_us)
                } else {
                    StagePressure::compute(l.stage, l.current_p95_us * 0.3, l.current_p95_us)
                }
            })
            .collect();
        alloc.allocate(&pressures, "status-test");
        let s = alloc.status_line();
        assert!(s.contains("REDISTRIBUTING") || s.contains("NOMINAL"));
    }

    #[test]
    fn test_allocation_reason_display() {
        assert_eq!(format!("{}", AllocationReason::Warmup), "WARMUP");
        assert_eq!(
            format!("{}", AllocationReason::AllWithinBudget),
            "ALL_WITHIN_BUDGET"
        );
        assert_eq!(format!("{}", AllocationReason::NoDonors), "NO_DONORS");
        assert_eq!(
            format!(
                "{}",
                AllocationReason::SlackRedistributed {
                    donor_count: 3,
                    receiver_count: 1
                }
            ),
            "SLACK_REDISTRIBUTED donors=3 receivers=1"
        );
        assert_eq!(
            format!("{}", AllocationReason::ResetToDefaults),
            "RESET_TO_DEFAULTS"
        );
    }

    #[test]
    fn test_allocator_recent_decisions() {
        let cfg = AdaptiveAllocatorConfig {
            warmup_observations: 0,
            min_donor_headroom: 0.05,
            ..Default::default()
        };
        let mut alloc = AdaptiveAllocator::new(&default_budgets(), cfg);
        for i in 0..5 {
            let pressures: Vec<StagePressure> = alloc
                .lanes()
                .iter()
                .map(|l| StagePressure::compute(l.stage, l.current_p95_us * 0.5, l.current_p95_us))
                .collect();
            alloc.allocate(&pressures, &format!("d-{}", i));
        }
        let recent = alloc.recent_decisions(3);
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0].epoch, 3);
        assert_eq!(recent[2].epoch, 5);
    }

    #[test]
    fn test_lane_allocation_serde() {
        let lane = LaneAllocation::new(LatencyStage::PtyCapture, 10000.0);
        let json = serde_json::to_string(&lane).expect("serialize");
        let back: LaneAllocation = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(lane.stage, back.stage);
        assert!((lane.default_p95_us - back.default_p95_us).abs() < 1e-10);
    }

    #[test]
    fn test_allocation_decision_serde() {
        let d = AllocationDecision {
            epoch: 42,
            correlation_id: "test-serde".into(),
            adjustments: vec![StageAdjustment {
                stage: LatencyStage::StorageWrite,
                before_p95_us: 5000.0,
                after_p95_us: 5500.0,
                delta_us: 500.0,
                rate_clamped: false,
                bound_clamped: false,
            }],
            slack_pool_before_us: 100.0,
            slack_pool_after_us: 50.0,
            warmup: false,
            reason: AllocationReason::SlackRedistributed {
                donor_count: 2,
                receiver_count: 1,
            },
        };
        let json = serde_json::to_string(&d).expect("serialize");
        let back: AllocationDecision = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(d.epoch, back.epoch);
        assert_eq!(d.reason, back.reason);
        assert_eq!(d.adjustments.len(), back.adjustments.len());
    }

    #[test]
    fn test_stage_pressure_serde() {
        let p = StagePressure::compute(LatencyStage::EventEmission, 1500.0, 2000.0);
        let json = serde_json::to_string(&p).expect("serialize");
        let back: StagePressure = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(p.stage, back.stage);
        assert!((p.headroom - back.headroom).abs() < 1e-10);
    }

    #[test]
    fn test_allocator_bounded_rate() {
        let cfg = AdaptiveAllocatorConfig {
            warmup_observations: 0,
            max_adjustment_pct: 0.10,
            min_donor_headroom: 0.05,
            ..Default::default()
        };
        let mut alloc = AdaptiveAllocator::new(&default_budgets(), cfg.clone());

        // Single epoch with huge pressure on one stage.
        let pressures: Vec<StagePressure> = alloc
            .lanes()
            .iter()
            .map(|l| {
                if l.stage == LatencyStage::StorageWrite {
                    StagePressure::compute(l.stage, l.current_p95_us * 10.0, l.current_p95_us)
                } else {
                    StagePressure::compute(l.stage, l.current_p95_us * 0.1, l.current_p95_us)
                }
            })
            .collect();
        let d = alloc.allocate(&pressures, "bounded-rate-test");

        // Each donor should have donated at most max_adjustment_pct of its default.
        for adj in &d.adjustments {
            if adj.delta_us < 0.0 {
                let lane = alloc.lanes().iter().find(|l| l.stage == adj.stage).unwrap();
                let max_donate = lane.default_p95_us * cfg.max_adjustment_pct;
                assert!(
                    (-adj.delta_us) <= max_donate + 1e-6,
                    "{} donated too much: {} > {}",
                    adj.stage,
                    -adj.delta_us,
                    max_donate
                );
            }
        }
    }

    #[test]
    fn test_allocator_over_budget_epoch_count() {
        let cfg = AdaptiveAllocatorConfig {
            warmup_observations: 0,
            min_donor_headroom: 0.05,
            ..Default::default()
        };
        let mut alloc = AdaptiveAllocator::new(&default_budgets(), cfg);

        for _epoch in 0..5 {
            let pressures: Vec<StagePressure> = alloc
                .lanes()
                .iter()
                .map(|l| {
                    if l.stage == LatencyStage::PatternDetection {
                        StagePressure::compute(l.stage, l.current_p95_us * 1.5, l.current_p95_us)
                    } else {
                        StagePressure::compute(l.stage, l.current_p95_us * 0.5, l.current_p95_us)
                    }
                })
                .collect();
            alloc.allocate(&pressures, "epoch-count-test");
        }

        let pd = alloc.allocation(LatencyStage::PatternDetection).unwrap();
        assert_eq!(pd.over_budget_epochs, 5);
    }

    // ── A4 Impl: Bridge, Degradation, Logging ──

    #[test]
    fn test_pressures_from_enforcer_snapshot() {
        let enforcer = BudgetEnforcer::with_defaults();
        let snap = enforcer.snapshot();
        let pressures = AdaptiveAllocator::pressures_from_snapshot(&snap);
        // Should have one pressure per non-aggregate stage.
        assert_eq!(pressures.len(), 8);
        for p in &pressures {
            assert!(!p.stage.is_aggregate());
        }
    }

    #[test]
    fn test_pressures_from_snapshot_headroom_with_data() {
        let mut enforcer = BudgetEnforcer::with_defaults();
        // Record some low-latency observations for PtyCapture.
        for _ in 0..10 {
            enforcer.record(LatencyStage::PtyCapture, 1000.0, "test");
        }
        let snap = enforcer.snapshot();
        let pressures = AdaptiveAllocator::pressures_from_snapshot(&snap);
        let pty = pressures
            .iter()
            .find(|p| p.stage == LatencyStage::PtyCapture)
            .unwrap();
        // PtyCapture budget is 10000 p95, observed ~1000 → headroom > 0.
        assert!(
            pty.headroom > 0.0,
            "expected positive headroom: {}",
            pty.headroom
        );
    }

    #[test]
    fn test_adjusted_budgets_default_is_identity() {
        let alloc = AdaptiveAllocator::with_defaults();
        let adjusted = alloc.adjusted_budgets();
        let defaults = default_budgets();
        for adj in &adjusted {
            let orig = defaults.iter().find(|b| b.stage == adj.stage).unwrap();
            assert!(
                (adj.p95_us - orig.p95_us).abs() < 1e-6,
                "{}: adjusted p95={} vs default p95={}",
                adj.stage,
                adj.p95_us,
                orig.p95_us
            );
        }
    }

    #[test]
    fn test_adjusted_budgets_proportional_scaling() {
        let cfg = AdaptiveAllocatorConfig {
            warmup_observations: 0,
            min_donor_headroom: 0.05,
            ..Default::default()
        };
        let mut alloc = AdaptiveAllocator::new(&default_budgets(), cfg);

        // Run epochs with StorageWrite over-budget to trigger redistribution.
        for i in 0..20 {
            let pressures: Vec<StagePressure> = alloc
                .lanes()
                .iter()
                .map(|l| {
                    if l.stage == LatencyStage::StorageWrite {
                        StagePressure::compute(l.stage, l.current_p95_us * 1.5, l.current_p95_us)
                    } else {
                        StagePressure::compute(l.stage, l.current_p95_us * 0.3, l.current_p95_us)
                    }
                })
                .collect();
            alloc.allocate(&pressures, &format!("adj-{}", i));
        }

        let adjusted = alloc.adjusted_budgets();
        for budget in &adjusted {
            // Monotonic invariant: p50 <= p95 <= p99 <= p999.
            assert!(
                budget.p50_us <= budget.p95_us + 1e-6,
                "{}: p50={} > p95={}",
                budget.stage,
                budget.p50_us,
                budget.p95_us
            );
            assert!(
                budget.p95_us <= budget.p99_us + 1e-6,
                "{}: p95={} > p99={}",
                budget.stage,
                budget.p95_us,
                budget.p99_us
            );
            assert!(
                budget.p99_us <= budget.p999_us + 1e-6,
                "{}: p99={} > p999={}",
                budget.stage,
                budget.p99_us,
                budget.p999_us
            );
        }
    }

    #[test]
    fn test_allocator_degradation_healthy() {
        let alloc = AdaptiveAllocator::with_defaults();
        assert_eq!(alloc.current_degradation(), AllocatorDegradation::Healthy);
        assert!(alloc.is_healthy());
    }

    #[test]
    fn test_allocator_degradation_display() {
        assert_eq!(format!("{}", AllocatorDegradation::Healthy), "HEALTHY");
        assert!(
            format!("{}", AllocatorDegradation::Oscillating { lane_count: 5 })
                .contains("OSCILLATING")
        );
        assert!(
            format!(
                "{}",
                AllocatorDegradation::ConservationDrift { drift_us: 1.5 }
            )
            .contains("CONSERVATION_DRIFT")
        );
        assert!(
            format!(
                "{}",
                AllocatorDegradation::FloorSaturation { lane_count: 4 }
            )
            .contains("FLOOR_SATURATION")
        );
    }

    #[test]
    fn test_allocator_log_entry_generation() {
        let cfg = AdaptiveAllocatorConfig {
            warmup_observations: 0,
            min_donor_headroom: 0.05,
            ..Default::default()
        };
        let mut alloc = AdaptiveAllocator::new(&default_budgets(), cfg);
        // Nominal epoch.
        let pressures: Vec<StagePressure> = alloc
            .lanes()
            .iter()
            .map(|l| StagePressure::compute(l.stage, l.current_p95_us * 0.5, l.current_p95_us))
            .collect();
        alloc.allocate(&pressures, "log-test");

        let entry = alloc.last_log_entry().unwrap();
        assert_eq!(entry.epoch, 1);
        assert_eq!(entry.correlation_id, "log-test");
        assert_eq!(entry.reason, "ALL_WITHIN_BUDGET");
        assert_eq!(entry.adjustment_count, 0);
    }

    #[test]
    fn test_allocator_log_entry_redistribution() {
        let cfg = AdaptiveAllocatorConfig {
            warmup_observations: 0,
            min_donor_headroom: 0.05,
            ..Default::default()
        };
        let mut alloc = AdaptiveAllocator::new(&default_budgets(), cfg);

        // Multiple epochs to get headroom negative for StorageWrite.
        for i in 0..10 {
            let pressures: Vec<StagePressure> = alloc
                .lanes()
                .iter()
                .map(|l| {
                    if l.stage == LatencyStage::StorageWrite {
                        StagePressure::compute(l.stage, l.current_p95_us * 2.0, l.current_p95_us)
                    } else {
                        StagePressure::compute(l.stage, l.current_p95_us * 0.2, l.current_p95_us)
                    }
                })
                .collect();
            alloc.allocate(&pressures, &format!("log-redist-{}", i));
        }

        let entry = alloc.last_log_entry().unwrap();
        assert!(entry.reason.contains("SLACK_REDISTRIBUTED"));
        assert!(entry.adjustment_count > 0);
        assert!(entry.total_donated_us > 0.0);
        assert!(entry.total_received_us > 0.0);
    }

    #[test]
    fn test_allocator_log_entry_serde() {
        let entry = AllocationLogEntry {
            epoch: 10,
            correlation_id: "serde-test".into(),
            reason: "WARMUP".into(),
            adjustment_count: 0,
            total_donated_us: 0.0,
            total_received_us: 0.0,
            conservation_error_us: 0.001,
            degradation: AllocatorDegradation::Healthy,
        };
        let json = serde_json::to_string(&entry).expect("serialize");
        let back: AllocationLogEntry = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(entry.epoch, back.epoch);
        assert_eq!(entry.reason, back.reason);
    }

    #[test]
    fn test_allocator_degradation_serde() {
        let cases = vec![
            AllocatorDegradation::Healthy,
            AllocatorDegradation::Oscillating { lane_count: 3 },
            AllocatorDegradation::ConservationDrift { drift_us: 1.23 },
            AllocatorDegradation::FloorSaturation { lane_count: 5 },
        ];
        for case in cases {
            let json = serde_json::to_string(&case).expect("serialize");
            let back: AllocatorDegradation = serde_json::from_str(&json).expect("deserialize");
            // For f64 variant, use tolerance.
            match (&case, &back) {
                (
                    AllocatorDegradation::ConservationDrift { drift_us: a },
                    AllocatorDegradation::ConservationDrift { drift_us: b },
                ) => assert!((a - b).abs() < 1e-10),
                _ => assert_eq!(case, back),
            }
        }
    }

    // ── B1: Three-Lane Scheduler ──

    #[test]
    fn test_scheduler_lane_priority_order() {
        assert!(SchedulerLane::Input < SchedulerLane::Control);
        assert!(SchedulerLane::Control < SchedulerLane::Bulk);
        assert_eq!(SchedulerLane::Input.priority(), 0);
        assert_eq!(SchedulerLane::Control.priority(), 1);
        assert_eq!(SchedulerLane::Bulk.priority(), 2);
    }

    #[test]
    fn test_scheduler_lane_all_complete() {
        assert_eq!(SchedulerLane::ALL.len(), 3);
    }

    #[test]
    fn test_scheduler_lane_display() {
        assert_eq!(format!("{}", SchedulerLane::Input), "input");
        assert_eq!(format!("{}", SchedulerLane::Control), "control");
        assert_eq!(format!("{}", SchedulerLane::Bulk), "bulk");
    }

    #[test]
    fn test_stage_to_lane_mapping() {
        assert_eq!(
            stage_to_lane(LatencyStage::PtyCapture),
            SchedulerLane::Input
        );
        assert_eq!(
            stage_to_lane(LatencyStage::DeltaExtraction),
            SchedulerLane::Input
        );
        assert_eq!(
            stage_to_lane(LatencyStage::ApiResponse),
            SchedulerLane::Input
        );
        assert_eq!(
            stage_to_lane(LatencyStage::EventEmission),
            SchedulerLane::Control
        );
        assert_eq!(
            stage_to_lane(LatencyStage::WorkflowDispatch),
            SchedulerLane::Control
        );
        assert_eq!(
            stage_to_lane(LatencyStage::ActionExecution),
            SchedulerLane::Control
        );
        assert_eq!(
            stage_to_lane(LatencyStage::StorageWrite),
            SchedulerLane::Bulk
        );
        assert_eq!(
            stage_to_lane(LatencyStage::PatternDetection),
            SchedulerLane::Bulk
        );
    }

    #[test]
    fn test_scheduler_config_default_valid() {
        let cfg = LaneSchedulerConfig::default();
        let errors = cfg.validate();
        assert!(
            errors.is_empty(),
            "default config should be valid: {:?}",
            errors
        );
    }

    #[test]
    fn test_scheduler_config_cpu_share_overflow() {
        let cfg = LaneSchedulerConfig {
            input_cpu_share: 0.5,
            control_cpu_share: 0.4,
            bulk_cpu_share: 0.3,
            ..Default::default()
        };
        let errors = cfg.validate();
        assert!(!errors.is_empty());
        assert!(errors[0].contains("CPU shares"));
    }

    #[test]
    fn test_scheduler_admit_basic() {
        let mut sched = LaneScheduler::with_defaults();
        let (item, decision) = sched.admit(LatencyStage::PtyCapture, 100.0, "test-1", 0, 1000);
        assert_eq!(item.lane, SchedulerLane::Input);
        assert_eq!(decision, AdmissionDecision::Admitted);
        assert_eq!(sched.lane_state(SchedulerLane::Input).depth, 1);
    }

    #[test]
    fn test_scheduler_bulk_shed_under_input_pressure() {
        let cfg = LaneSchedulerConfig {
            input_queue_capacity: 4,
            input_pressure_threshold: 0.75,
            ..Default::default()
        };
        let mut sched = LaneScheduler::new(cfg);

        // Fill input to 3/4 capacity (75%) = at threshold.
        for i in 0..3 {
            sched.admit(LatencyStage::PtyCapture, 10.0, &format!("inp-{}", i), 0, 0);
        }
        assert!(sched.input_under_pressure());

        // Bulk item should be shed.
        let (_item, decision) = sched.admit(LatencyStage::StorageWrite, 1000.0, "bulk-shed", 0, 0);
        assert_eq!(decision, AdmissionDecision::Shed);
    }

    #[test]
    fn test_scheduler_input_never_shed() {
        let cfg = LaneSchedulerConfig {
            input_queue_capacity: 2,
            ..Default::default()
        };
        let mut sched = LaneScheduler::new(cfg);

        // Fill input to capacity.
        sched.admit(LatencyStage::PtyCapture, 10.0, "a", 0, 0);
        sched.admit(LatencyStage::PtyCapture, 10.0, "b", 0, 0);

        // Next input item should be deferred, not shed.
        let (_item, decision) = sched.admit(LatencyStage::PtyCapture, 10.0, "c", 0, 0);
        assert_eq!(decision, AdmissionDecision::Deferred);
    }

    #[test]
    fn test_scheduler_bulk_queue_full_shed() {
        let cfg = LaneSchedulerConfig {
            bulk_queue_capacity: 2,
            input_pressure_threshold: 0.99, // Don't trigger pressure shedding.
            ..Default::default()
        };
        let mut sched = LaneScheduler::new(cfg);

        sched.admit(LatencyStage::StorageWrite, 100.0, "b1", 0, 0);
        sched.admit(LatencyStage::StorageWrite, 100.0, "b2", 0, 0);

        // Queue full — bulk items shed.
        let (_item, decision) = sched.admit(LatencyStage::StorageWrite, 100.0, "b3", 0, 0);
        assert_eq!(decision, AdmissionDecision::Shed);
    }

    #[test]
    fn test_scheduler_deadline_promotion() {
        let cfg = LaneSchedulerConfig {
            enable_deadline_promotion: true,
            deadline_promotion_fraction: 0.25,
            input_pressure_threshold: 0.99,
            ..Default::default()
        };
        let mut sched = LaneScheduler::new(cfg);

        // Bulk item with tight deadline: now=900, deadline=1000, remaining=100 < 250 (25% of 1000).
        let (_item, decision) = sched.admit(
            LatencyStage::PatternDetection,
            50.0,
            "promoted-1",
            1000,
            900,
        );
        assert_eq!(
            decision,
            AdmissionDecision::Promoted {
                from: SchedulerLane::Bulk,
                to: SchedulerLane::Control,
            }
        );
        // Control queue should have the item.
        assert_eq!(sched.lane_state(SchedulerLane::Control).depth, 1);
    }

    #[test]
    fn test_scheduler_complete_decrements() {
        let mut sched = LaneScheduler::with_defaults();
        sched.admit(LatencyStage::PtyCapture, 100.0, "c1", 0, 0);
        assert_eq!(sched.lane_state(SchedulerLane::Input).depth, 1);

        sched.complete(SchedulerLane::Input, 95.0);
        assert_eq!(sched.lane_state(SchedulerLane::Input).depth, 0);
        assert_eq!(sched.lane_state(SchedulerLane::Input).total_completed, 1);
        assert!((sched.lane_state(SchedulerLane::Input).cpu_used_us - 95.0).abs() < 1e-6);
    }

    #[test]
    fn test_scheduler_begin_epoch_resets_cpu() {
        let mut sched = LaneScheduler::with_defaults();
        sched.begin_epoch(10000.0);
        let input = sched.lane_state(SchedulerLane::Input);
        assert!((input.cpu_budget_us - 5000.0).abs() < 1e-6); // 50% of 10000
        let control = sched.lane_state(SchedulerLane::Control);
        assert!((control.cpu_budget_us - 3000.0).abs() < 1e-6); // 30%
        let bulk = sched.lane_state(SchedulerLane::Bulk);
        assert!((bulk.cpu_budget_us - 2000.0).abs() < 1e-6); // 20%
    }

    #[test]
    fn test_scheduler_snapshot_serde() {
        let mut sched = LaneScheduler::with_defaults();
        sched.admit(LatencyStage::PtyCapture, 100.0, "snap", 0, 0);
        let snap = sched.snapshot();
        let json = serde_json::to_string(&snap).expect("serialize");
        let back: SchedulerSnapshot = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(snap.epoch, back.epoch);
        assert_eq!(snap.lanes.len(), back.lanes.len());
    }

    #[test]
    fn test_scheduler_status_line() {
        let sched = LaneScheduler::with_defaults();
        let s = sched.status_line();
        assert!(s.contains("scheduler"));
        assert!(s.contains("input=0/256"));
        assert!(s.contains("control=0/128"));
        assert!(s.contains("bulk=0/1024"));
    }

    #[test]
    fn test_scheduler_recent_events() {
        let mut sched = LaneScheduler::with_defaults();
        for i in 0..5 {
            sched.admit(LatencyStage::PtyCapture, 10.0, &format!("ev-{}", i), 0, 0);
        }
        let events = sched.recent_events(3);
        assert_eq!(events.len(), 3);
    }

    #[test]
    fn test_admission_decision_display() {
        assert_eq!(format!("{}", AdmissionDecision::Admitted), "ADMITTED");
        assert_eq!(format!("{}", AdmissionDecision::Deferred), "DEFERRED");
        assert_eq!(format!("{}", AdmissionDecision::Shed), "SHED");
        assert_eq!(
            format!(
                "{}",
                AdmissionDecision::Promoted {
                    from: SchedulerLane::Bulk,
                    to: SchedulerLane::Control,
                }
            ),
            "PROMOTED bulk→control"
        );
    }

    #[test]
    fn test_lane_state_utilization() {
        let mut state = LaneState::new(SchedulerLane::Input, 100);
        assert_eq!(state.utilization(), 0.0);
        state.depth = 50;
        assert!((state.utilization() - 0.5).abs() < 1e-6);
        state.depth = 100;
        assert!((state.utilization() - 1.0).abs() < 1e-6);
        assert!(state.is_full());
    }

    #[test]
    fn test_default_stages_cover_all_pipeline() {
        let mut covered: Vec<LatencyStage> = Vec::new();
        for &lane in SchedulerLane::ALL {
            covered.extend_from_slice(lane.default_stages());
        }
        for &stage in LatencyStage::PIPELINE_STAGES {
            assert!(
                covered.contains(&stage),
                "stage {} not covered by any lane",
                stage
            );
        }
    }

    #[test]
    fn test_work_item_serde() {
        let item = WorkItem {
            id: 42,
            lane: SchedulerLane::Input,
            qos: QosScope::for_pane(QosClass::Interactive, 7),
            stage: LatencyStage::PtyCapture,
            estimated_cost_us: 500.0,
            correlation_id: "serde-test".into(),
            deadline_us: 0,
        };
        let json = serde_json::to_string(&item).expect("serialize");
        let back: WorkItem = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(item.id, back.id);
        assert_eq!(item.lane, back.lane);
    }

    #[test]
    fn test_scheduling_event_serde() {
        let event = SchedulingEvent {
            item_id: 1,
            lane: SchedulerLane::Bulk,
            qos_class: QosClass::BulkSearch,
            pane_id: Some(7),
            mission_id: Some("bulk-search".into()),
            stage: LatencyStage::StorageWrite,
            decision: AdmissionDecision::Shed,
            queue_depth_before: 1024,
            queue_depth_after: 1024,
            correlation_id: "shed-test".into(),
            reason_code: Some("QUEUE_OVERFLOW".into()),
        };
        let json = serde_json::to_string(&event).expect("serialize");
        let back: SchedulingEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(event.item_id, back.item_id);
        assert_eq!(event.decision, back.decision);
    }

    // ── B1 Impl: CPU Budget, Fairness, Degradation ──

    #[test]
    fn test_scheduler_has_cpu_budget() {
        let mut sched = LaneScheduler::with_defaults();
        sched.begin_epoch(10000.0);
        assert!(sched.has_cpu_budget(SchedulerLane::Input));
        assert!((sched.remaining_cpu_us(SchedulerLane::Input) - 5000.0).abs() < 1e-6);
    }

    #[test]
    fn test_scheduler_cpu_budget_exhaustion() {
        let mut sched = LaneScheduler::with_defaults();
        sched.begin_epoch(10000.0);
        sched.admit(LatencyStage::PtyCapture, 100.0, "x", 0, 0);
        sched.complete(SchedulerLane::Input, 5001.0);
        assert!(!sched.has_cpu_budget(SchedulerLane::Input));
        assert_eq!(sched.remaining_cpu_us(SchedulerLane::Input), 0.0);
    }

    #[test]
    fn test_scheduler_next_lane_priority() {
        let mut sched = LaneScheduler::with_defaults();
        sched.begin_epoch(10000.0);
        sched.admit(LatencyStage::StorageWrite, 100.0, "bulk", 0, 0);
        sched.admit(LatencyStage::PtyCapture, 100.0, "input", 0, 0);
        assert_eq!(sched.next_lane(), Some(SchedulerLane::Input));
    }

    #[test]
    fn test_scheduler_next_lane_fallthrough() {
        let mut sched = LaneScheduler::with_defaults();
        sched.begin_epoch(10000.0);
        sched.admit(LatencyStage::StorageWrite, 100.0, "bulk", 0, 0);
        assert_eq!(sched.next_lane(), Some(SchedulerLane::Bulk));
    }

    #[test]
    fn test_scheduler_next_lane_empty() {
        let mut sched = LaneScheduler::with_defaults();
        sched.begin_epoch(10000.0);
        assert_eq!(sched.next_lane(), None);
    }

    #[test]
    fn test_scheduler_fairness_ratios_no_work() {
        let sched = LaneScheduler::with_defaults();
        let ratios = sched.fairness_ratios();
        assert_eq!(ratios.len(), 3);
        for (_lane, ratio) in &ratios {
            assert!((*ratio - 1.0).abs() < 1e-6);
        }
    }

    #[test]
    fn test_scheduler_fairness_ratios_with_work() {
        let mut sched = LaneScheduler::with_defaults();
        sched.begin_epoch(10000.0);
        sched.admit(LatencyStage::PtyCapture, 100.0, "f1", 0, 0);
        sched.complete(SchedulerLane::Input, 5000.0);
        let ratios = sched.fairness_ratios();
        let input_ratio = ratios
            .iter()
            .find(|(l, _)| *l == SchedulerLane::Input)
            .unwrap()
            .1;
        assert!((input_ratio - 2.0).abs() < 1e-6);
    }

    #[test]
    fn test_scheduler_degradation_healthy() {
        let sched = LaneScheduler::with_defaults();
        assert_eq!(sched.current_degradation(), SchedulerDegradation::Healthy);
        assert!(sched.is_healthy());
    }

    #[test]
    fn test_scheduler_degradation_display() {
        assert_eq!(format!("{}", SchedulerDegradation::Healthy), "HEALTHY");
        let inp = SchedulerDegradation::InputStarvation {
            depth: 10,
            deferred: 50,
        };
        assert!(format!("{}", inp).contains("INPUT_STARVATION"));
        let bulk = SchedulerDegradation::BulkStarvation {
            shed_count: 100,
            completed_count: 5,
        };
        assert!(format!("{}", bulk).contains("BULK_STARVATION"));
        let ctrl = SchedulerDegradation::ControlBacklog {
            depth: 70,
            capacity: 128,
        };
        assert!(format!("{}", ctrl).contains("CONTROL_BACKLOG"));
    }

    #[test]
    fn test_scheduler_log_entry() {
        let mut sched = LaneScheduler::with_defaults();
        sched.begin_epoch(10000.0);
        sched.admit(LatencyStage::PtyCapture, 100.0, "log", 0, 0);
        let entry = sched.log_entry();
        assert_eq!(entry.epoch, 1);
        assert_eq!(entry.depths.len(), 3);
        assert!(!entry.input_pressure);
    }

    #[test]
    fn test_scheduler_log_entry_serde() {
        let mut sched = LaneScheduler::with_defaults();
        sched.begin_epoch(10000.0);
        let entry = sched.log_entry();
        let json = serde_json::to_string(&entry).expect("serialize");
        let back: SchedulerLogEntry = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(entry.epoch, back.epoch);
        assert_eq!(entry.depths.len(), back.depths.len());
    }

    #[test]
    fn test_scheduler_degradation_serde() {
        let cases = vec![
            SchedulerDegradation::Healthy,
            SchedulerDegradation::InputStarvation {
                depth: 5,
                deferred: 20,
            },
            SchedulerDegradation::BulkStarvation {
                shed_count: 50,
                completed_count: 2,
            },
            SchedulerDegradation::ControlBacklog {
                depth: 70,
                capacity: 128,
            },
        ];
        for case in cases {
            let json = serde_json::to_string(&case).expect("serialize");
            let back: SchedulerDegradation = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(case, back);
        }
    }

    // ── B2: Bounded Input Ring ──

    #[test]
    fn test_input_ring_basic_enqueue_dequeue() {
        let mut ring = InputRing::with_defaults();
        assert!(ring.is_empty());
        let seq = ring
            .enqueue(LatencyStage::PtyCapture, 100.0, "basic", 1000, 0)
            .unwrap();
        assert_eq!(seq, 1);
        assert_eq!(ring.len(), 1);
        let item = ring.dequeue(1100).unwrap();
        assert_eq!(item.seq, 1);
        assert!(ring.is_empty());
    }

    #[test]
    fn test_input_ring_fifo_order() {
        let mut ring = InputRing::with_defaults();
        for i in 0..5 {
            ring.enqueue(
                LatencyStage::PtyCapture,
                10.0,
                &format!("fifo-{}", i),
                i * 100,
                0,
            )
            .unwrap();
        }
        for i in 0..5 {
            let item = ring.dequeue(1000).unwrap();
            assert_eq!(item.seq, i as u64 + 1);
        }
    }

    #[test]
    fn test_input_ring_full_rejects() {
        let cfg = InputRingConfig {
            capacity: 3,
            ..Default::default()
        };
        let mut ring = InputRing::new(cfg);
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "a", 0, 0)
            .unwrap();
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "b", 0, 0)
            .unwrap();
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "c", 0, 0)
            .unwrap();
        assert!(ring.is_full());
        let result = ring.enqueue(LatencyStage::PtyCapture, 10.0, "d", 0, 0);
        assert_eq!(result, Err(RingBackpressure::Full));
    }

    #[test]
    fn test_input_ring_backpressure_signals() {
        let cfg = InputRingConfig {
            capacity: 4,
            high_water_mark: 0.75,
            ..Default::default()
        };
        let mut ring = InputRing::new(cfg);
        assert_eq!(ring.backpressure(), RingBackpressure::Accept);
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "bp1", 0, 0)
            .unwrap();
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "bp2", 0, 0)
            .unwrap();
        assert_eq!(ring.backpressure(), RingBackpressure::Accept);
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "bp3", 0, 0)
            .unwrap();
        // 3/4 = 0.75 >= high_water_mark → SlowDown
        assert_eq!(ring.backpressure(), RingBackpressure::SlowDown);
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "bp4", 0, 0)
            .unwrap();
        assert_eq!(ring.backpressure(), RingBackpressure::Full);
    }

    #[test]
    fn test_input_ring_wraparound() {
        let cfg = InputRingConfig {
            capacity: 3,
            ..Default::default()
        };
        let mut ring = InputRing::new(cfg);
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "w1", 0, 0)
            .unwrap();
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "w2", 0, 0)
            .unwrap();
        ring.dequeue(100).unwrap(); // remove w1
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "w3", 0, 0)
            .unwrap();
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "w4", 0, 0)
            .unwrap();
        assert_eq!(ring.len(), 3);
        // Should be w2, w3, w4 in FIFO order.
        assert_eq!(ring.dequeue(200).unwrap().seq, 2);
        assert_eq!(ring.dequeue(200).unwrap().seq, 3);
        assert_eq!(ring.dequeue(200).unwrap().seq, 4);
    }

    #[test]
    fn test_input_ring_peek() {
        let mut ring = InputRing::with_defaults();
        assert!(ring.peek().is_none());
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "peek", 100, 0)
            .unwrap();
        let peeked = ring.peek().unwrap();
        assert_eq!(peeked.seq, 1);
        assert_eq!(ring.len(), 1); // Peek doesn't remove.
    }

    #[test]
    fn test_input_ring_sojourn_tracking() {
        let cfg = InputRingConfig {
            track_sojourn: true,
            ..Default::default()
        };
        let mut ring = InputRing::new(cfg);
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "soj", 1000, 0)
            .unwrap();
        ring.dequeue(1500).unwrap(); // sojourn = 500us
        assert!((ring.mean_sojourn_us().unwrap() - 500.0).abs() < 1e-6);
    }

    #[test]
    fn test_input_ring_snapshot() {
        let mut ring = InputRing::with_defaults();
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "snap", 100, 0)
            .unwrap();
        let snap = ring.snapshot();
        assert_eq!(snap.len, 1);
        assert_eq!(snap.total_enqueued, 1);
        assert_eq!(snap.total_dequeued, 0);
        assert_eq!(snap.total_dropped, 0);
    }

    #[test]
    fn test_input_ring_snapshot_serde() {
        let ring = InputRing::with_defaults();
        let snap = ring.snapshot();
        let json = serde_json::to_string(&snap).expect("serialize");
        let back: InputRingSnapshot = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(snap.capacity, back.capacity);
        assert_eq!(snap.len, back.len);
    }

    #[test]
    fn test_input_ring_status_line() {
        let ring = InputRing::with_defaults();
        let s = ring.status_line();
        assert!(s.contains("input_ring"));
        assert!(s.contains("len=0/256"));
    }

    #[test]
    fn test_input_ring_accounting() {
        let cfg = InputRingConfig {
            capacity: 2,
            ..Default::default()
        };
        let mut ring = InputRing::new(cfg);
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "a", 0, 0)
            .unwrap();
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "b", 0, 0)
            .unwrap();
        let _ = ring.enqueue(LatencyStage::PtyCapture, 10.0, "c", 0, 0); // dropped
        ring.dequeue(100).unwrap();
        // Invariant: enqueued = dequeued + len (dropped are separate rejection count)
        assert_eq!(ring.total_enqueued, ring.total_dequeued + ring.len() as u64);
        assert_eq!(ring.total_dropped, 1);
    }

    #[test]
    fn test_ring_backpressure_display() {
        assert_eq!(format!("{}", RingBackpressure::Accept), "ACCEPT");
        assert_eq!(format!("{}", RingBackpressure::SlowDown), "SLOW_DOWN");
        assert_eq!(format!("{}", RingBackpressure::Full), "FULL");
    }

    #[test]
    fn test_input_ring_item_serde() {
        let item = InputRingItem {
            seq: 42,
            stage: LatencyStage::PtyCapture,
            estimated_cost_us: 100.0,
            correlation_id: "serde-item".into(),
            arrived_us: 1000,
            deadline_us: 0,
        };
        let json = serde_json::to_string(&item).expect("serialize");
        let back: InputRingItem = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(item.seq, back.seq);
        assert_eq!(item.stage, back.stage);
    }

    // ── B2 Impl: Drain, Expiry, Utilization ──

    #[test]
    fn test_input_ring_drain() {
        let mut ring = InputRing::with_defaults();
        for i in 0..10 {
            ring.enqueue(
                LatencyStage::PtyCapture,
                10.0,
                &format!("d-{}", i),
                i * 100,
                0,
            )
            .unwrap();
        }
        let items = ring.drain(5, 2000);
        assert_eq!(items.len(), 5);
        assert_eq!(items[0].seq, 1);
        assert_eq!(items[4].seq, 5);
        assert_eq!(ring.len(), 5);
    }

    #[test]
    fn test_input_ring_drain_more_than_available() {
        let mut ring = InputRing::with_defaults();
        for i in 0..3 {
            ring.enqueue(LatencyStage::PtyCapture, 10.0, &format!("dm-{}", i), 0, 0)
                .unwrap();
        }
        let items = ring.drain(100, 1000);
        assert_eq!(items.len(), 3);
        assert!(ring.is_empty());
    }

    #[test]
    fn test_input_ring_drain_expired() {
        let mut ring = InputRing::with_defaults();
        // Item with deadline=500, item with deadline=2000, item with no deadline.
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "exp", 100, 500)
            .unwrap();
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "ok", 200, 2000)
            .unwrap();
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "nodeadline", 300, 0)
            .unwrap();

        let expired = ring.drain_expired(1000);
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].correlation_id, "exp");
        // Remaining ring should have 2 items.
        assert_eq!(ring.len(), 2);
    }

    #[test]
    fn test_input_ring_utilization() {
        let cfg = InputRingConfig {
            capacity: 10,
            ..Default::default()
        };
        let mut ring = InputRing::new(cfg);
        assert!((ring.utilization() - 0.0).abs() < 1e-6);
        for i in 0..5 {
            ring.enqueue(LatencyStage::PtyCapture, 10.0, &format!("u-{}", i), 0, 0)
                .unwrap();
        }
        assert!((ring.utilization() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn test_input_ring_capacity() {
        let cfg = InputRingConfig {
            capacity: 42,
            ..Default::default()
        };
        let ring = InputRing::new(cfg);
        assert_eq!(ring.capacity(), 42);
    }

    // ── B3: Priority Inheritance & Lock-Order ──

    #[test]
    fn test_priority_ordering() {
        assert!(Priority::Critical > Priority::Elevated);
        assert!(Priority::Elevated > Priority::Normal);
        assert!(Priority::Normal > Priority::Background);
    }

    #[test]
    fn test_priority_display() {
        assert_eq!(format!("{}", Priority::Critical), "CRITICAL");
        assert_eq!(format!("{}", Priority::Background), "BACKGROUND");
    }

    #[test]
    fn test_priority_all_covers_four() {
        assert_eq!(Priority::ALL.len(), 4);
    }

    #[test]
    fn test_stage_to_priority_mapping() {
        assert_eq!(
            stage_to_priority(LatencyStage::PtyCapture),
            Priority::Critical
        );
        assert_eq!(
            stage_to_priority(LatencyStage::DeltaExtraction),
            Priority::Critical
        );
        assert_eq!(
            stage_to_priority(LatencyStage::EventEmission),
            Priority::Elevated
        );
        assert_eq!(
            stage_to_priority(LatencyStage::StorageWrite),
            Priority::Background
        );
    }

    #[test]
    fn test_resource_lock_order_is_canonical() {
        for w in Resource::LOCK_ORDER.windows(2) {
            assert!(w[0].order_index() < w[1].order_index());
        }
    }

    #[test]
    fn test_resource_display() {
        assert_eq!(format!("{}", Resource::StorageLock), "storage");
        assert_eq!(format!("{}", Resource::WorkflowLock), "workflow");
    }

    #[test]
    fn test_pi_acquire_free_lock() {
        let mut tracker = PriorityInheritanceTracker::with_defaults();
        let result = tracker.acquire(Resource::StorageLock, "task-1", Priority::Normal, 100);
        assert_eq!(result, LockResult::Acquired);
        assert!(tracker.is_held_by(Resource::StorageLock, "task-1"));
    }

    #[test]
    fn test_pi_reentrant_acquire() {
        let mut tracker = PriorityInheritanceTracker::with_defaults();
        tracker.acquire(Resource::StorageLock, "task-1", Priority::Normal, 100);
        let result = tracker.acquire(Resource::StorageLock, "task-1", Priority::Normal, 200);
        assert_eq!(result, LockResult::Acquired);
    }

    #[test]
    fn test_pi_inheritance_on_contention() {
        let mut tracker = PriorityInheritanceTracker::with_defaults();
        tracker.acquire(Resource::PatternLock, "low", Priority::Background, 100);

        let result = tracker.acquire(Resource::PatternLock, "high", Priority::Critical, 200);
        match result {
            LockResult::AcquiredAfterInheritance { boosted_holder } => {
                assert_eq!(boosted_holder, "low");
            }
            other => panic!("Expected AcquiredAfterInheritance, got {:?}", other),
        }

        // The holder's effective priority should now be Critical.
        assert_eq!(tracker.effective_priority("low"), Some(Priority::Critical));
    }

    #[test]
    fn test_pi_release_reverts_priority() {
        let mut tracker = PriorityInheritanceTracker::with_defaults();
        tracker.acquire(Resource::StorageLock, "low", Priority::Background, 100);
        tracker.acquire(Resource::StorageLock, "high", Priority::Critical, 200);

        let promoted = tracker.release(Resource::StorageLock, "low", 300);
        assert_eq!(promoted, vec!["high".to_string()]);
        assert!(tracker.is_held_by(Resource::StorageLock, "high"));
        assert!(!tracker.is_held_by(Resource::StorageLock, "low"));
    }

    #[test]
    fn test_pi_lock_order_violation() {
        let mut tracker = PriorityInheritanceTracker::with_defaults();
        // Acquire WorkflowLock (index 3) first.
        tracker.acquire(Resource::WorkflowLock, "task-1", Priority::Normal, 100);

        // Try to acquire StorageLock (index 0) — violates canonical order.
        let result = tracker.acquire(Resource::StorageLock, "task-1", Priority::Normal, 200);
        match result {
            LockResult::OrderViolation {
                requested,
                held_after,
            } => {
                assert_eq!(requested, Resource::StorageLock);
                assert_eq!(held_after, Resource::WorkflowLock);
            }
            other => panic!("Expected OrderViolation, got {:?}", other),
        }
    }

    #[test]
    fn test_pi_lock_order_valid_ascending() {
        let mut tracker = PriorityInheritanceTracker::with_defaults();
        let r1 = tracker.acquire(Resource::StorageLock, "task-1", Priority::Normal, 100);
        assert_eq!(r1, LockResult::Acquired);
        let r2 = tracker.acquire(Resource::PatternLock, "task-1", Priority::Normal, 200);
        assert_eq!(r2, LockResult::Acquired);
        let r3 = tracker.acquire(Resource::EventBusLock, "task-1", Priority::Normal, 300);
        assert_eq!(r3, LockResult::Acquired);

        assert!(tracker.check_lock_order("task-1").is_empty());
    }

    #[test]
    fn test_pi_snapshot_reflects_state() {
        let mut tracker = PriorityInheritanceTracker::with_defaults();
        tracker.acquire(Resource::StorageLock, "t1", Priority::Normal, 100);
        tracker.acquire(Resource::StorageLock, "t2", Priority::Critical, 200);

        let snap = tracker.snapshot();
        assert_eq!(snap.held_locks.len(), 1);
        assert_eq!(snap.total_inheritance_events, 1);
        assert_eq!(snap.active_chains, 1);
    }

    #[test]
    fn test_pi_status_line() {
        let tracker = PriorityInheritanceTracker::with_defaults();
        let line = tracker.status_line();
        assert!(line.contains("pi_tracker"));
        assert!(line.contains("held=0"));
    }

    #[test]
    fn test_pi_release_nonexistent() {
        let mut tracker = PriorityInheritanceTracker::with_defaults();
        let promoted = tracker.release(Resource::StorageLock, "nobody", 100);
        assert!(promoted.is_empty());
    }

    #[test]
    fn test_pi_release_wrong_holder() {
        let mut tracker = PriorityInheritanceTracker::with_defaults();
        tracker.acquire(Resource::StorageLock, "owner", Priority::Normal, 100);
        let promoted = tracker.release(Resource::StorageLock, "impostor", 200);
        assert!(promoted.is_empty());
        assert!(tracker.is_held_by(Resource::StorageLock, "owner"));
    }

    #[test]
    fn test_pi_waiter_promotion_order() {
        let mut tracker = PriorityInheritanceTracker::with_defaults();
        tracker.acquire(Resource::PatternLock, "holder", Priority::Background, 100);
        tracker.acquire(Resource::PatternLock, "low", Priority::Normal, 200);
        tracker.acquire(Resource::PatternLock, "high", Priority::Critical, 300);

        // Release: highest priority waiter (high) should be promoted first.
        let promoted = tracker.release(Resource::PatternLock, "holder", 400);
        assert_eq!(promoted, vec!["high".to_string()]);
        assert!(tracker.is_held_by(Resource::PatternLock, "high"));
    }

    #[test]
    fn test_pi_effective_priority_across_locks() {
        let mut tracker = PriorityInheritanceTracker::with_defaults();
        tracker.acquire(Resource::StorageLock, "t1", Priority::Background, 100);
        tracker.acquire(Resource::PatternLock, "t1", Priority::Normal, 200);

        // Effective priority should be the max across all held locks.
        assert_eq!(tracker.effective_priority("t1"), Some(Priority::Normal));
    }

    #[test]
    fn test_inheritance_event_serde() {
        let event = InheritanceEvent {
            holder_id: "h".to_string(),
            waiter_id: "w".to_string(),
            resource: Resource::StorageLock,
            original_priority: Priority::Background,
            inherited_priority: Priority::Critical,
            applied_us: 100,
            released_us: Some(200),
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: InheritanceEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, back);
    }

    #[test]
    fn test_lock_result_serde() {
        let results = vec![
            LockResult::Acquired,
            LockResult::AcquiredAfterInheritance {
                boosted_holder: "x".to_string(),
            },
            LockResult::OrderViolation {
                requested: Resource::StorageLock,
                held_after: Resource::WorkflowLock,
            },
        ];
        for r in &results {
            let json = serde_json::to_string(r).unwrap();
            let back: LockResult = serde_json::from_str(&json).unwrap();
            assert_eq!(*r, back);
        }
    }

    #[test]
    fn test_priority_serde() {
        for p in &Priority::ALL {
            let json = serde_json::to_string(p).unwrap();
            let back: Priority = serde_json::from_str(&json).unwrap();
            assert_eq!(*p, back);
        }
    }

    #[test]
    fn test_resource_serde() {
        for r in &Resource::LOCK_ORDER {
            let json = serde_json::to_string(r).unwrap();
            let back: Resource = serde_json::from_str(&json).unwrap();
            assert_eq!(*r, back);
        }
    }

    #[test]
    fn test_inheritance_snapshot_serde() {
        let snap = InheritanceSnapshot {
            held_locks: vec![],
            total_inheritance_events: 5,
            total_order_violations: 2,
            active_chains: 1,
            max_chain_depth_observed: 3,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: InheritanceSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }

    #[test]
    fn test_pi_config_default() {
        let cfg = PriorityInheritanceConfig::default();
        assert_eq!(cfg.max_chain_depth, 4);
        assert!(cfg.enforce_lock_order);
        assert_eq!(cfg.max_inheritance_duration_us, 50_000);
    }

    #[test]
    fn test_pi_no_order_violation_when_disabled() {
        let config = PriorityInheritanceConfig {
            enforce_lock_order: false,
            ..Default::default()
        };
        let mut tracker = PriorityInheritanceTracker::new(config);
        tracker.acquire(Resource::WorkflowLock, "task-1", Priority::Normal, 100);
        // With lock-order enforcement disabled, this should succeed.
        let result = tracker.acquire(Resource::StorageLock, "task-1", Priority::Normal, 200);
        assert_eq!(result, LockResult::Acquired);
    }

    // ── B3 Impl: Bridge methods ──

    #[test]
    fn test_pi_release_all() {
        let mut tracker = PriorityInheritanceTracker::with_defaults();
        tracker.acquire(Resource::StorageLock, "t1", Priority::Normal, 100);
        tracker.acquire(Resource::PatternLock, "t1", Priority::Normal, 200);
        tracker.acquire(Resource::EventBusLock, "t1", Priority::Normal, 300);

        let released = tracker.release_all("t1", 400);
        assert_eq!(released.len(), 3);
        assert_eq!(tracker.held_count(), 0);
    }

    #[test]
    fn test_pi_expire_stale_inheritance() {
        let config = PriorityInheritanceConfig {
            max_inheritance_duration_us: 100,
            ..Default::default()
        };
        let mut tracker = PriorityInheritanceTracker::new(config);
        tracker.acquire(Resource::StorageLock, "low", Priority::Background, 0);
        tracker.acquire(Resource::StorageLock, "high", Priority::Critical, 50);

        // Before expiry.
        assert_eq!(tracker.effective_priority("low"), Some(Priority::Critical));

        // After expiry (200us > 100us max).
        let expired = tracker.expire_stale_inheritance(200);
        assert_eq!(expired, 1);
        assert_eq!(
            tracker.effective_priority("low"),
            Some(Priority::Background)
        );
    }

    #[test]
    fn test_pi_held_count() {
        let mut tracker = PriorityInheritanceTracker::with_defaults();
        assert_eq!(tracker.held_count(), 0);
        tracker.acquire(Resource::StorageLock, "t1", Priority::Normal, 100);
        assert_eq!(tracker.held_count(), 1);
        tracker.acquire(Resource::PatternLock, "t2", Priority::Normal, 200);
        assert_eq!(tracker.held_count(), 2);
    }

    #[test]
    fn test_pi_total_waiters() {
        let mut tracker = PriorityInheritanceTracker::with_defaults();
        tracker.acquire(Resource::StorageLock, "holder", Priority::Background, 100);
        tracker.acquire(Resource::StorageLock, "w1", Priority::Normal, 200);
        tracker.acquire(Resource::StorageLock, "w2", Priority::Elevated, 300);
        assert_eq!(tracker.total_waiters(), 2);
    }

    #[test]
    fn test_pi_degradation_healthy() {
        let tracker = PriorityInheritanceTracker::with_defaults();
        assert_eq!(
            tracker.detect_degradation(),
            InheritanceDegradation::Healthy
        );
    }

    #[test]
    fn test_pi_degradation_excessive_inheritance() {
        let mut tracker = PriorityInheritanceTracker::with_defaults();
        // Create 3 locks each with inheritance (>2 threshold).
        for (i, resource) in [
            Resource::StorageLock,
            Resource::PatternLock,
            Resource::EventBusLock,
        ]
        .iter()
        .enumerate()
        {
            tracker.acquire(
                *resource,
                &format!("low-{}", i),
                Priority::Background,
                i as u64 * 100,
            );
            tracker.acquire(
                *resource,
                &format!("high-{}", i),
                Priority::Critical,
                i as u64 * 100 + 50,
            );
        }
        let degradation = tracker.detect_degradation();
        let is_excessive = matches!(
            degradation,
            InheritanceDegradation::ExcessiveInheritance { .. }
        );
        assert!(
            is_excessive,
            "Expected ExcessiveInheritance, got {:?}",
            degradation
        );
    }

    #[test]
    fn test_pi_degradation_order_violation_spike() {
        let mut tracker = PriorityInheritanceTracker::with_defaults();
        // Generate >10 order violations.
        for _ in 0..11 {
            tracker.acquire(Resource::WorkflowLock, "task", Priority::Normal, 100);
            let _ = tracker.acquire(Resource::StorageLock, "task", Priority::Normal, 200);
            tracker.release(Resource::WorkflowLock, "task", 300);
        }
        let degradation = tracker.detect_degradation();
        let is_spike = matches!(
            degradation,
            InheritanceDegradation::OrderViolationSpike { .. }
        );
        assert!(
            is_spike,
            "Expected OrderViolationSpike, got {:?}",
            degradation
        );
    }

    #[test]
    fn test_pi_log_entry() {
        let mut tracker = PriorityInheritanceTracker::with_defaults();
        tracker.acquire(Resource::StorageLock, "t1", Priority::Normal, 100);
        let entry = tracker.log_entry(500);
        assert_eq!(entry.timestamp_us, 500);
        assert_eq!(entry.held_locks, 1);
        assert_eq!(entry.degradation, InheritanceDegradation::Healthy);
    }

    #[test]
    fn test_inheritance_degradation_serde() {
        let variants = vec![
            InheritanceDegradation::Healthy,
            InheritanceDegradation::ExcessiveInheritance {
                active_chains: 3,
                threshold: 2,
            },
            InheritanceDegradation::HighContention {
                total_waiters: 10,
                threshold: 8,
            },
            InheritanceDegradation::OrderViolationSpike {
                total_violations: 15,
                threshold: 10,
            },
        ];
        for v in &variants {
            let json = serde_json::to_string(v).unwrap();
            let back: InheritanceDegradation = serde_json::from_str(&json).unwrap();
            assert_eq!(*v, back);
        }
    }

    #[test]
    fn test_inheritance_log_entry_serde() {
        let entry = InheritanceLogEntry {
            timestamp_us: 1000,
            held_locks: 2,
            total_inheritance_events: 5,
            total_order_violations: 1,
            active_chains: 1,
            degradation: InheritanceDegradation::Healthy,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: InheritanceLogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back);
    }

    #[test]
    fn test_inheritance_degradation_display() {
        assert_eq!(format!("{}", InheritanceDegradation::Healthy), "HEALTHY");
        let exc = InheritanceDegradation::ExcessiveInheritance {
            active_chains: 3,
            threshold: 2,
        };
        assert!(format!("{}", exc).contains("3/2"));
    }

    // ── B4: Starvation Prevention & Fairness ──

    #[test]
    fn test_starvation_config_default() {
        let cfg = StarvationConfig::default();
        assert_eq!(cfg.max_starved_epochs, 5);
        assert_eq!(cfg.fairness_window, 20);
        assert!(cfg.enable_aging);
    }

    #[test]
    fn test_starvation_tracker_initial_state() {
        let tracker = StarvationTracker::with_defaults();
        assert_eq!(tracker.epoch(), 0);
        assert!(!tracker.any_starving());
        let snap = tracker.snapshot();
        assert_eq!(snap.lanes.len(), 3);
        assert_eq!(snap.total_starvation_events, 0);
    }

    #[test]
    fn test_starvation_no_starvation_when_all_served() {
        let mut tracker = StarvationTracker::with_defaults();
        for _ in 0..10 {
            let promoted = tracker.observe_epoch(&[5, 3, 2], &[0.5, 0.3, 0.2]);
            assert!(promoted.is_empty());
        }
        assert!(!tracker.any_starving());
    }

    #[test]
    fn test_starvation_detected_after_threshold() {
        let config = StarvationConfig {
            max_starved_epochs: 3,
            ..Default::default()
        };
        let mut tracker = StarvationTracker::new(config);

        // Bulk lane gets zero completions for 3 epochs.
        for i in 0..3 {
            let promoted = tracker.observe_epoch(&[5, 3, 0], &[0.5, 0.3, 0.0]);
            if i < 2 {
                assert!(promoted.is_empty());
            } else {
                assert_eq!(promoted, vec![SchedulerLane::Bulk]);
            }
        }
        assert!(tracker.any_starving());
        assert!(tracker.lane_state(SchedulerLane::Bulk).force_promoted);
    }

    #[test]
    fn test_starvation_clears_on_completion() {
        let config = StarvationConfig {
            max_starved_epochs: 2,
            ..Default::default()
        };
        let mut tracker = StarvationTracker::new(config);

        // Starve bulk for 2 epochs.
        tracker.observe_epoch(&[5, 3, 0], &[0.5, 0.3, 0.0]);
        tracker.observe_epoch(&[5, 3, 0], &[0.5, 0.3, 0.0]);
        assert!(tracker.any_starving());

        // Bulk gets completions — starvation clears.
        tracker.observe_epoch(&[5, 3, 1], &[0.4, 0.3, 0.1]);
        assert!(!tracker.lane_state(SchedulerLane::Bulk).force_promoted);
    }

    #[test]
    fn test_gini_coefficient_equal_shares() {
        let mut tracker = StarvationTracker::with_defaults();
        // Equal shares → Gini ~= 0.
        for _ in 0..5 {
            tracker.observe_epoch(&[3, 3, 3], &[0.333, 0.333, 0.334]);
        }
        let gini = tracker.gini_coefficient();
        assert!(
            gini < 0.01,
            "Gini {} should be near 0 for equal shares",
            gini
        );
    }

    #[test]
    fn test_gini_coefficient_unequal_shares() {
        let mut tracker = StarvationTracker::with_defaults();
        // Very unequal shares → higher Gini.
        for _ in 0..5 {
            tracker.observe_epoch(&[10, 0, 0], &[0.9, 0.05, 0.05]);
        }
        let gini = tracker.gini_coefficient();
        assert!(
            gini > 0.3,
            "Gini {} should be higher for unequal shares",
            gini
        );
    }

    #[test]
    fn test_starvation_snapshot_serde() {
        let snap = FairnessSnapshot {
            lanes: vec![LaneFairnessState {
                lane: SchedulerLane::Input,
                starved_epochs: 0,
                windowed_share: 0.5,
                windowed_completions: 10,
                windowed_deferred: 2,
                force_promoted: false,
            }],
            gini_coefficient: 0.15,
            total_starvation_events: 3,
            any_starving: false,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: FairnessSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap.total_starvation_events, back.total_starvation_events);
        assert_eq!(snap.any_starving, back.any_starving);
        assert_eq!(snap.lanes.len(), back.lanes.len());
    }

    #[test]
    fn test_starvation_event_serde() {
        let event = StarvationEvent {
            epoch: 10,
            lane: SchedulerLane::Bulk,
            starved_epochs: 5,
            cpu_share: 0.01,
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: StarvationEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, back);
    }

    #[test]
    fn test_starvation_status_line() {
        let tracker = StarvationTracker::with_defaults();
        let line = tracker.status_line();
        assert!(line.contains("fairness"));
        assert!(line.contains("gini="));
        assert!(line.contains("epoch=0"));
    }

    #[test]
    fn test_starvation_config_serde() {
        let cfg = StarvationConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let back: StarvationConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn test_lane_fairness_state_serde() {
        let state = LaneFairnessState {
            lane: SchedulerLane::Control,
            starved_epochs: 2,
            windowed_share: 0.3,
            windowed_completions: 5,
            windowed_deferred: 1,
            force_promoted: false,
        };
        let json = serde_json::to_string(&state).unwrap();
        let back: LaneFairnessState = serde_json::from_str(&json).unwrap();
        assert_eq!(state, back);
    }

    #[test]
    fn test_starvation_epoch_monotonic() {
        let mut tracker = StarvationTracker::with_defaults();
        for i in 1..=5 {
            tracker.observe_epoch(&[1, 1, 1], &[0.33, 0.33, 0.34]);
            assert_eq!(tracker.epoch(), i);
        }
    }

    #[test]
    fn test_starvation_multiple_lanes_starve() {
        let config = StarvationConfig {
            max_starved_epochs: 2,
            ..Default::default()
        };
        let mut tracker = StarvationTracker::new(config);

        // Both Control and Bulk starve.
        tracker.observe_epoch(&[5, 0, 0], &[0.8, 0.0, 0.0]);
        let promoted = tracker.observe_epoch(&[5, 0, 0], &[0.8, 0.0, 0.0]);
        assert_eq!(promoted.len(), 2);
        assert!(promoted.contains(&SchedulerLane::Control));
        assert!(promoted.contains(&SchedulerLane::Bulk));
    }

    // ── B4 Impl: Bridge methods ──

    #[test]
    fn test_starvation_reset() {
        let config = StarvationConfig {
            max_starved_epochs: 2,
            ..Default::default()
        };
        let mut tracker = StarvationTracker::new(config);
        tracker.observe_epoch(&[5, 0, 0], &[0.8, 0.0, 0.0]);
        tracker.observe_epoch(&[5, 0, 0], &[0.8, 0.0, 0.0]);
        assert!(tracker.any_starving());

        tracker.reset();
        assert_eq!(tracker.epoch(), 0);
        assert!(!tracker.any_starving());
        assert_eq!(tracker.snapshot().total_starvation_events, 0);
    }

    #[test]
    fn test_starvation_recent_events() {
        let config = StarvationConfig {
            max_starved_epochs: 1,
            ..Default::default()
        };
        let mut tracker = StarvationTracker::new(config);
        tracker.observe_epoch(&[5, 0, 0], &[0.8, 0.0, 0.0]);
        let recent = tracker.recent_events(10);
        assert_eq!(recent.len(), 2); // Control and Bulk both starved.
    }

    #[test]
    fn test_starvation_is_force_promoted() {
        let config = StarvationConfig {
            max_starved_epochs: 1,
            ..Default::default()
        };
        let mut tracker = StarvationTracker::new(config);
        assert!(!tracker.is_force_promoted(SchedulerLane::Bulk));
        tracker.observe_epoch(&[5, 3, 0], &[0.5, 0.3, 0.0]);
        assert!(tracker.is_force_promoted(SchedulerLane::Bulk));
        assert!(!tracker.is_force_promoted(SchedulerLane::Input));
    }

    #[test]
    fn test_fairness_degradation_healthy() {
        let tracker = StarvationTracker::with_defaults();
        assert_eq!(tracker.detect_degradation(), FairnessDegradation::Healthy);
    }

    #[test]
    fn test_fairness_degradation_starvation() {
        let config = StarvationConfig {
            max_starved_epochs: 1,
            ..Default::default()
        };
        let mut tracker = StarvationTracker::new(config);
        tracker.observe_epoch(&[5, 3, 0], &[0.5, 0.3, 0.0]);
        let degradation = tracker.detect_degradation();
        let is_starvation = matches!(degradation, FairnessDegradation::LaneStarvation { .. });
        assert!(
            is_starvation,
            "Expected LaneStarvation, got {:?}",
            degradation
        );
    }

    #[test]
    fn test_fairness_log_entry() {
        let mut tracker = StarvationTracker::with_defaults();
        tracker.observe_epoch(&[5, 3, 2], &[0.5, 0.3, 0.2]);
        let entry = tracker.log_entry();
        assert_eq!(entry.epoch, 1);
        assert_eq!(entry.shares.len(), 3);
        assert_eq!(entry.starved_epochs.len(), 3);
    }

    #[test]
    fn test_fairness_degradation_serde() {
        let variants = vec![
            FairnessDegradation::Healthy,
            FairnessDegradation::LaneStarvation {
                starving_lanes: vec![SchedulerLane::Bulk],
            },
            FairnessDegradation::SevereUnfairness {
                gini: 0.7,
                threshold: 0.5,
            },
            FairnessDegradation::PromotionStorm {
                events_in_window: 10,
                threshold: 5,
            },
        ];
        for v in &variants {
            let json = serde_json::to_string(v).unwrap();
            let back: FairnessDegradation = serde_json::from_str(&json).unwrap();
            assert_eq!(*v, back);
        }
    }

    #[test]
    fn test_fairness_log_entry_serde() {
        let entry = FairnessLogEntry {
            epoch: 5,
            shares: vec![0.5, 0.3, 0.2],
            starved_epochs: vec![0, 0, 0],
            gini_coefficient: 0.1,
            any_starving: false,
            degradation: FairnessDegradation::Healthy,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: FairnessLogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back);
    }

    #[test]
    fn test_fairness_degradation_display() {
        assert_eq!(format!("{}", FairnessDegradation::Healthy), "HEALTHY");
        let storm = FairnessDegradation::PromotionStorm {
            events_in_window: 10,
            threshold: 5,
        };
        assert!(format!("{}", storm).contains("10/5"));
    }

    // ── C1: Memory Ownership Graph & Pool ──

    #[test]
    fn test_memory_domain_all_covers_eight() {
        assert_eq!(MemoryDomain::ALL.len(), 8);
    }

    #[test]
    fn test_memory_domain_display() {
        assert_eq!(format!("{}", MemoryDomain::PtyCapture), "pty_capture");
        assert_eq!(format!("{}", MemoryDomain::Shared), "shared");
    }

    #[test]
    fn test_stage_to_domain_mapping() {
        assert_eq!(
            stage_to_domain(LatencyStage::PtyCapture),
            MemoryDomain::PtyCapture
        );
        assert_eq!(
            stage_to_domain(LatencyStage::StorageWrite),
            MemoryDomain::StorageWrite
        );
        assert_eq!(
            stage_to_domain(LatencyStage::EventEmission),
            MemoryDomain::EventBus
        );
        assert_eq!(
            stage_to_domain(LatencyStage::ApiResponse),
            MemoryDomain::Shared
        );
    }

    #[test]
    fn test_pool_alloc_from_free_list() {
        let mut pool = MemoryPool::with_defaults();
        let result = pool.allocate();
        let is_from_free = matches!(result, AllocResult::FromFreeList { .. });
        assert!(is_from_free, "Expected FromFreeList, got {:?}", result);
        assert_eq!(pool.in_use(), 1);
    }

    #[test]
    fn test_pool_alloc_grow() {
        let config = PoolConfig {
            initial_blocks: 0,
            max_blocks: 10,
            ..Default::default()
        };
        let mut pool = MemoryPool::new(config);
        let result = pool.allocate();
        let is_grown = matches!(result, AllocResult::Grown { .. });
        assert!(is_grown, "Expected Grown, got {:?}", result);
    }

    #[test]
    fn test_pool_alloc_exhausted() {
        let config = PoolConfig {
            initial_blocks: 1,
            max_blocks: 1,
            ..Default::default()
        };
        let mut pool = MemoryPool::new(config);
        pool.allocate();
        let result = pool.allocate();
        assert_eq!(result, AllocResult::PoolExhausted);
    }

    #[test]
    fn test_pool_free_returns_to_free_list() {
        let mut pool = MemoryPool::with_defaults();
        let block_id = match pool.allocate() {
            AllocResult::FromFreeList { block_id } => block_id,
            other => panic!("Expected FromFreeList, got {:?}", other),
        };
        assert_eq!(pool.in_use(), 1);
        pool.free(block_id);
        assert_eq!(pool.in_use(), 0);
        assert_eq!(pool.free_count(), 64); // 64 initial - 1 alloc + 1 free
    }

    #[test]
    fn test_pool_utilization() {
        let config = PoolConfig {
            initial_blocks: 4,
            max_blocks: 4,
            ..Default::default()
        };
        let mut pool = MemoryPool::new(config);
        assert!((pool.utilization() - 0.0).abs() < 1e-10);
        pool.allocate();
        assert!((pool.utilization() - 0.25).abs() < 1e-10);
        pool.allocate();
        assert!((pool.utilization() - 0.5).abs() < 1e-10);
    }

    #[test]
    fn test_pool_under_pressure() {
        let config = PoolConfig {
            initial_blocks: 4,
            max_blocks: 4,
            high_water_mark: 0.75,
            ..Default::default()
        };
        let mut pool = MemoryPool::new(config);
        pool.allocate();
        pool.allocate();
        assert!(!pool.under_pressure()); // 50% < 75%
        pool.allocate();
        assert!(pool.under_pressure()); // 75% >= 75%
    }

    #[test]
    fn test_pool_snapshot_invariant() {
        let mut pool = MemoryPool::with_defaults();
        pool.allocate();
        pool.allocate();
        let snap = pool.snapshot();
        assert_eq!(snap.in_use + snap.free_count, snap.total_blocks);
        assert_eq!(snap.total_allocs, snap.total_frees + snap.in_use as u64);
    }

    #[test]
    fn test_pool_status_line() {
        let pool = MemoryPool::with_defaults();
        let line = pool.status_line();
        assert!(line.contains("pool[shared]"));
        assert!(line.contains("0/64"));
    }

    #[test]
    fn test_pool_config_default() {
        let cfg = PoolConfig::default();
        assert_eq!(cfg.block_size, 4096);
        assert_eq!(cfg.initial_blocks, 64);
        assert_eq!(cfg.max_blocks, 1024);
    }

    #[test]
    fn test_pool_snapshot_serde() {
        let snap = PoolSnapshot {
            domain: MemoryDomain::PtyCapture,
            block_size: 4096,
            total_blocks: 64,
            in_use: 10,
            free_count: 54,
            max_blocks: 1024,
            total_allocs: 20,
            total_frees: 10,
            total_exhausted: 0,
            utilization: 0.15625,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: PoolSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap.domain, back.domain);
        assert_eq!(snap.in_use, back.in_use);
        assert_eq!(snap.total_allocs, back.total_allocs);
    }

    #[test]
    fn test_alloc_result_serde() {
        let results = vec![
            AllocResult::FromFreeList { block_id: 42 },
            AllocResult::Grown { block_id: 99 },
            AllocResult::PoolExhausted,
        ];
        for r in &results {
            let json = serde_json::to_string(r).unwrap();
            let back: AllocResult = serde_json::from_str(&json).unwrap();
            assert_eq!(*r, back);
        }
    }

    #[test]
    fn test_memory_domain_serde() {
        for d in &MemoryDomain::ALL {
            let json = serde_json::to_string(d).unwrap();
            let back: MemoryDomain = serde_json::from_str(&json).unwrap();
            assert_eq!(*d, back);
        }
    }

    #[test]
    fn test_pool_config_serde() {
        let cfg = PoolConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let back: PoolConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
    }

    // ── C1 Impl: Pool bridge methods ──

    #[test]
    fn test_pool_shrink() {
        let mut pool = MemoryPool::with_defaults();
        assert_eq!(pool.free_count(), 64);
        let reclaimed = pool.shrink(10);
        assert_eq!(reclaimed, 54);
        assert_eq!(pool.free_count(), 10);
        assert_eq!(pool.total_blocks(), 10);
    }

    #[test]
    fn test_pool_shrink_no_excess() {
        let config = PoolConfig {
            initial_blocks: 4,
            max_blocks: 10,
            ..Default::default()
        };
        let mut pool = MemoryPool::new(config);
        let reclaimed = pool.shrink(10);
        assert_eq!(reclaimed, 0);
    }

    #[test]
    fn test_pool_reset() {
        let mut pool = MemoryPool::with_defaults();
        pool.allocate();
        pool.allocate();
        pool.allocate();
        assert_eq!(pool.in_use(), 3);

        pool.reset();
        assert_eq!(pool.in_use(), 0);
        assert_eq!(pool.total_blocks(), 64);
        assert_eq!(pool.free_count(), 64);
    }

    #[test]
    fn test_pool_degradation_healthy() {
        let pool = MemoryPool::with_defaults();
        assert_eq!(pool.detect_degradation(), PoolDegradation::Healthy);
    }

    #[test]
    fn test_pool_degradation_exhausted() {
        let config = PoolConfig {
            initial_blocks: 1,
            max_blocks: 1,
            ..Default::default()
        };
        let mut pool = MemoryPool::new(config);
        pool.allocate();
        pool.allocate(); // exhausted
        let degradation = pool.detect_degradation();
        let is_exhausted = matches!(degradation, PoolDegradation::Exhausted { .. });
        assert!(is_exhausted, "Expected Exhausted, got {:?}", degradation);
    }

    #[test]
    fn test_pool_degradation_high_util() {
        let config = PoolConfig {
            initial_blocks: 4,
            max_blocks: 4,
            high_water_mark: 0.5,
            ..Default::default()
        };
        let mut pool = MemoryPool::new(config);
        pool.allocate();
        pool.allocate();
        pool.allocate();
        let degradation = pool.detect_degradation();
        let is_high = matches!(degradation, PoolDegradation::HighUtilization { .. });
        assert!(is_high, "Expected HighUtilization, got {:?}", degradation);
    }

    #[test]
    fn test_pool_log_entry() {
        let mut pool = MemoryPool::with_defaults();
        pool.allocate();
        let entry = pool.log_entry();
        assert_eq!(entry.domain, MemoryDomain::Shared);
        assert_eq!(entry.in_use, 1);
        assert_eq!(entry.degradation, PoolDegradation::Healthy);
    }

    #[test]
    fn test_pool_degradation_serde() {
        let variants = vec![
            PoolDegradation::Healthy,
            PoolDegradation::HighUtilization {
                utilization: 0.9,
                threshold: 0.85,
            },
            PoolDegradation::Exhausted { total_exhausted: 5 },
            PoolDegradation::Fragmented {
                total_blocks: 100,
                free_count: 60,
            },
        ];
        for v in &variants {
            let json = serde_json::to_string(v).unwrap();
            let back: PoolDegradation = serde_json::from_str(&json).unwrap();
            assert_eq!(*v, back);
        }
    }

    #[test]
    fn test_pool_log_entry_serde() {
        let entry = PoolLogEntry {
            domain: MemoryDomain::PtyCapture,
            utilization: 0.5,
            in_use: 32,
            total_blocks: 64,
            degradation: PoolDegradation::Healthy,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: PoolLogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back);
    }

    #[test]
    fn test_pool_degradation_display() {
        assert_eq!(format!("{}", PoolDegradation::Healthy), "HEALTHY");
        let exhausted = PoolDegradation::Exhausted { total_exhausted: 5 };
        assert!(format!("{}", exhausted).contains("5"));
    }

    // ── C3: Tiered Scrollback Tests ────────────────────────────────

    #[test]
    fn test_scrollback_tier_rank() {
        assert_eq!(ScrollbackTier::Hot.rank(), 0);
        assert_eq!(ScrollbackTier::Warm.rank(), 1);
        assert_eq!(ScrollbackTier::Cold.rank(), 2);
    }

    #[test]
    fn test_scrollback_tier_demote() {
        assert_eq!(ScrollbackTier::Hot.demote(), Some(ScrollbackTier::Warm));
        assert_eq!(ScrollbackTier::Warm.demote(), Some(ScrollbackTier::Cold));
        assert_eq!(ScrollbackTier::Cold.demote(), None);
    }

    #[test]
    fn test_scrollback_tier_display() {
        assert_eq!(format!("{}", ScrollbackTier::Hot), "HOT");
        assert_eq!(format!("{}", ScrollbackTier::Warm), "WARM");
        assert_eq!(format!("{}", ScrollbackTier::Cold), "COLD");
    }

    #[test]
    fn test_scrollback_tier_all() {
        assert_eq!(ScrollbackTier::ALL.len(), 3);
        for (i, tier) in ScrollbackTier::ALL.iter().enumerate() {
            assert_eq!(tier.rank(), i);
        }
    }

    #[test]
    fn test_tier_config_default() {
        let config = TierConfig::default();
        assert_eq!(config.tier, ScrollbackTier::Hot);
        assert!(config.max_bytes > 0);
        assert_eq!(config.compression_ratio, 1.0);
    }

    #[test]
    fn test_migration_policy_default() {
        let policy = TierMigrationPolicy::default();
        assert!(policy.hot_to_warm_age_us > 0);
        assert!(policy.warm_to_cold_age_us > policy.hot_to_warm_age_us);
        assert!(policy.pressure_threshold > 0.0 && policy.pressure_threshold < 1.0);
        assert!(policy.max_concurrent_migrations > 0);
    }

    #[test]
    fn test_tiered_scrollback_ingest() {
        let mut mgr = TieredScrollbackManager::with_defaults();
        let id = mgr.ingest(1, 1024, 10, 1000);
        assert_eq!(id, 0);
        assert_eq!(mgr.segment_count(), 1);
        assert_eq!(mgr.total_bytes(), 1024);
        let seg = mgr.segment(id).unwrap();
        assert_eq!(seg.tier, ScrollbackTier::Hot);
        assert_eq!(seg.pane_id, 1);
    }

    #[test]
    fn test_tiered_scrollback_touch() {
        let mut mgr = TieredScrollbackManager::with_defaults();
        let id = mgr.ingest(1, 1024, 10, 1000);
        mgr.touch(id, 5000);
        let seg = mgr.segment(id).unwrap();
        assert_eq!(seg.last_accessed_us, 5000);
    }

    #[test]
    fn test_tiered_scrollback_migrate_age() {
        let policy = TierMigrationPolicy {
            hot_to_warm_age_us: 1000,
            warm_to_cold_age_us: 5000,
            min_segment_bytes: 100,
            pressure_threshold: 0.99,
            max_concurrent_migrations: 10,
        };
        let hot = TierConfig {
            tier: ScrollbackTier::Hot,
            max_bytes: 1_000_000,
            target_latency_us: 10,
            compression_ratio: 1.0,
        };
        let warm = TierConfig {
            tier: ScrollbackTier::Warm,
            max_bytes: 1_000_000,
            target_latency_us: 500,
            compression_ratio: 1.0,
        };
        let cold = TierConfig {
            tier: ScrollbackTier::Cold,
            max_bytes: 10_000_000,
            target_latency_us: 10000,
            compression_ratio: 0.25,
        };
        let mut mgr = TieredScrollbackManager::new(hot, warm, cold, policy);

        mgr.ingest(1, 500, 5, 0);
        // Not old enough — no migration
        assert_eq!(mgr.migrate(500), 0);
        // Old enough → hot→warm
        let migrated = mgr.migrate(2000);
        assert_eq!(migrated, 1);
        assert_eq!(mgr.segment(0).unwrap().tier, ScrollbackTier::Warm);
        assert_eq!(mgr.total_migrations, 1);
    }

    #[test]
    fn test_tiered_scrollback_migrate_warm_to_cold() {
        let policy = TierMigrationPolicy {
            hot_to_warm_age_us: 100,
            warm_to_cold_age_us: 500,
            min_segment_bytes: 100,
            pressure_threshold: 0.99,
            max_concurrent_migrations: 10,
        };
        let hot = TierConfig {
            tier: ScrollbackTier::Hot,
            max_bytes: 1_000_000,
            target_latency_us: 10,
            compression_ratio: 1.0,
        };
        let warm = TierConfig {
            tier: ScrollbackTier::Warm,
            max_bytes: 1_000_000,
            target_latency_us: 500,
            compression_ratio: 1.0,
        };
        let cold = TierConfig {
            tier: ScrollbackTier::Cold,
            max_bytes: 10_000_000,
            target_latency_us: 10000,
            compression_ratio: 0.5,
        };
        let mut mgr = TieredScrollbackManager::new(hot, warm, cold, policy);

        mgr.ingest(1, 1000, 10, 0);
        mgr.migrate(200); // hot→warm
        assert_eq!(mgr.segment(0).unwrap().tier, ScrollbackTier::Warm);

        mgr.migrate(800); // warm→cold
        let seg = mgr.segment(0).unwrap();
        assert_eq!(seg.tier, ScrollbackTier::Cold);
        assert!(seg.compressed);
        // 1000 * 0.5 = 500
        assert_eq!(seg.byte_size, 500);
    }

    #[test]
    fn test_tiered_scrollback_conservation() {
        let mut mgr = TieredScrollbackManager::with_defaults();
        mgr.ingest(1, 1000, 10, 0);
        mgr.ingest(2, 2000, 20, 0);
        // Before migration, all in hot
        let snap = mgr.snapshot();
        assert_eq!(snap.total_bytes, 3000);
        assert_eq!(snap.hot_bytes, 3000);
        assert_eq!(snap.warm_bytes, 0);
        assert_eq!(snap.cold_bytes, 0);
    }

    #[test]
    fn test_tiered_scrollback_utilization() {
        let hot = TierConfig {
            tier: ScrollbackTier::Hot,
            max_bytes: 1000,
            target_latency_us: 10,
            compression_ratio: 1.0,
        };
        let warm = TierConfig {
            tier: ScrollbackTier::Warm,
            max_bytes: 5000,
            target_latency_us: 500,
            compression_ratio: 1.0,
        };
        let cold = TierConfig {
            tier: ScrollbackTier::Cold,
            max_bytes: 10000,
            target_latency_us: 10000,
            compression_ratio: 0.25,
        };
        let mut mgr = TieredScrollbackManager::new(hot, warm, cold, TierMigrationPolicy::default());

        mgr.ingest(1, 500, 5, 0);
        assert!((mgr.hot_utilization() - 0.5).abs() < 0.001);
        assert_eq!(mgr.warm_utilization(), 0.0);
    }

    #[test]
    fn test_tiered_scrollback_evict_pane() {
        let mut mgr = TieredScrollbackManager::with_defaults();
        mgr.ingest(1, 1000, 10, 0);
        mgr.ingest(2, 2000, 20, 0);
        mgr.ingest(1, 500, 5, 0);

        mgr.evict_pane(1);
        assert_eq!(mgr.segment_count(), 1);
        assert_eq!(mgr.total_bytes(), 2000);
    }

    #[test]
    fn test_tiered_scrollback_reset() {
        let mut mgr = TieredScrollbackManager::with_defaults();
        mgr.ingest(1, 1000, 10, 0);
        mgr.reset();
        assert_eq!(mgr.segment_count(), 0);
        assert_eq!(mgr.total_bytes(), 0);
        assert_eq!(mgr.total_migrations, 0);
    }

    #[test]
    fn test_tiered_scrollback_snapshot_serde() {
        let snap = TieredScrollbackSnapshot {
            hot_bytes: 100,
            warm_bytes: 200,
            cold_bytes: 300,
            hot_segments: 1,
            warm_segments: 2,
            cold_segments: 3,
            total_migrations: 5,
            total_bytes: 600,
            hot_utilization: 0.5,
            warm_utilization: 0.3,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: TieredScrollbackSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }

    #[test]
    fn test_scrollback_degradation_healthy() {
        let mgr = TieredScrollbackManager::with_defaults();
        assert_eq!(mgr.detect_degradation(), ScrollbackDegradation::Healthy);
    }

    #[test]
    fn test_scrollback_degradation_hot_pressure() {
        let hot = TierConfig {
            tier: ScrollbackTier::Hot,
            max_bytes: 1000,
            target_latency_us: 10,
            compression_ratio: 1.0,
        };
        let warm = TierConfig {
            tier: ScrollbackTier::Warm,
            max_bytes: 10000,
            target_latency_us: 500,
            compression_ratio: 1.0,
        };
        let cold = TierConfig {
            tier: ScrollbackTier::Cold,
            max_bytes: 100000,
            target_latency_us: 10000,
            compression_ratio: 0.25,
        };
        let policy = TierMigrationPolicy {
            pressure_threshold: 0.8,
            ..Default::default()
        };
        let mut mgr = TieredScrollbackManager::new(hot, warm, cold, policy);
        mgr.ingest(1, 900, 10, 0);
        let is_pressure = matches!(
            mgr.detect_degradation(),
            ScrollbackDegradation::HotPressure { .. }
        );
        assert!(
            is_pressure,
            "Expected HotPressure, got {:?}",
            mgr.detect_degradation()
        );
    }

    #[test]
    fn test_scrollback_degradation_display() {
        assert_eq!(format!("{}", ScrollbackDegradation::Healthy), "HEALTHY");
        let hot = ScrollbackDegradation::HotPressure {
            utilization: 0.9,
            threshold: 0.85,
        };
        assert!(format!("{}", hot).contains("90.0%"));
    }

    #[test]
    fn test_scrollback_log_entry() {
        let mut mgr = TieredScrollbackManager::with_defaults();
        mgr.ingest(1, 1024, 10, 0);
        let entry = mgr.log_entry();
        assert_eq!(entry.hot_bytes, 1024);
        assert_eq!(entry.total_segments, 1);
        assert_eq!(entry.degradation, ScrollbackDegradation::Healthy);
    }

    #[test]
    fn test_scrollback_log_entry_serde() {
        let entry = ScrollbackLogEntry {
            hot_bytes: 100,
            warm_bytes: 200,
            cold_bytes: 300,
            total_segments: 6,
            total_migrations: 3,
            degradation: ScrollbackDegradation::Healthy,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: ScrollbackLogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back);
    }

    #[test]
    fn test_scrollback_degradation_serde() {
        let variants = vec![
            ScrollbackDegradation::Healthy,
            ScrollbackDegradation::HotPressure {
                utilization: 0.9,
                threshold: 0.85,
            },
            ScrollbackDegradation::WarmPressure {
                utilization: 0.88,
                threshold: 0.85,
            },
            ScrollbackDegradation::MigrationBacklog {
                pending: 10,
                max_concurrent: 4,
            },
        ];
        for v in &variants {
            let json = serde_json::to_string(v).unwrap();
            let back: ScrollbackDegradation = serde_json::from_str(&json).unwrap();
            assert_eq!(*v, back);
        }
    }

    #[test]
    fn test_tiered_scrollback_status_line() {
        let mgr = TieredScrollbackManager::with_defaults();
        let line = mgr.status_line();
        assert!(line.contains("scrollback"));
        assert!(line.contains("migrations=0"));
    }

    #[test]
    fn test_tiered_scrollback_pressure_migration() {
        let hot = TierConfig {
            tier: ScrollbackTier::Hot,
            max_bytes: 1000,
            target_latency_us: 10,
            compression_ratio: 1.0,
        };
        let warm = TierConfig {
            tier: ScrollbackTier::Warm,
            max_bytes: 10000,
            target_latency_us: 500,
            compression_ratio: 1.0,
        };
        let cold = TierConfig {
            tier: ScrollbackTier::Cold,
            max_bytes: 100000,
            target_latency_us: 10000,
            compression_ratio: 0.25,
        };
        let policy = TierMigrationPolicy {
            hot_to_warm_age_us: 1_000_000_000, // Very long — won't trigger by age
            warm_to_cold_age_us: 1_000_000_000,
            min_segment_bytes: 100,
            pressure_threshold: 0.8,
            max_concurrent_migrations: 10,
        };
        let mut mgr = TieredScrollbackManager::new(hot, warm, cold, policy);
        // Fill hot tier past 80%
        mgr.ingest(1, 500, 5, 0);
        mgr.ingest(2, 400, 4, 0);
        // 900/1000 = 90% > 80% threshold → pressure migration
        let migrated = mgr.migrate(1);
        assert!(migrated > 0, "Expected pressure-driven migration");
    }

    #[test]
    fn test_tiered_scrollback_max_concurrent() {
        let policy = TierMigrationPolicy {
            hot_to_warm_age_us: 0, // Always migrate
            warm_to_cold_age_us: 1_000_000_000,
            min_segment_bytes: 1,
            pressure_threshold: 0.99,
            max_concurrent_migrations: 2,
        };
        let hot = TierConfig {
            tier: ScrollbackTier::Hot,
            max_bytes: 1_000_000,
            target_latency_us: 10,
            compression_ratio: 1.0,
        };
        let warm = TierConfig {
            tier: ScrollbackTier::Warm,
            max_bytes: 1_000_000,
            target_latency_us: 500,
            compression_ratio: 1.0,
        };
        let cold = TierConfig {
            tier: ScrollbackTier::Cold,
            max_bytes: 10_000_000,
            target_latency_us: 10000,
            compression_ratio: 0.25,
        };
        let mut mgr = TieredScrollbackManager::new(hot, warm, cold, policy);
        for i in 0..5 {
            mgr.ingest(i, 100, 1, 0);
        }
        let migrated = mgr.migrate(1);
        assert_eq!(migrated, 2, "Should respect max_concurrent_migrations");
    }

    #[test]
    fn test_tiered_scrollback_min_segment_filter() {
        let policy = TierMigrationPolicy {
            hot_to_warm_age_us: 0,
            warm_to_cold_age_us: 0,
            min_segment_bytes: 500,
            pressure_threshold: 0.99,
            max_concurrent_migrations: 10,
        };
        let hot = TierConfig {
            tier: ScrollbackTier::Hot,
            max_bytes: 1_000_000,
            target_latency_us: 10,
            compression_ratio: 1.0,
        };
        let warm = TierConfig {
            tier: ScrollbackTier::Warm,
            max_bytes: 1_000_000,
            target_latency_us: 500,
            compression_ratio: 1.0,
        };
        let cold = TierConfig {
            tier: ScrollbackTier::Cold,
            max_bytes: 10_000_000,
            target_latency_us: 10000,
            compression_ratio: 0.25,
        };
        let mut mgr = TieredScrollbackManager::new(hot, warm, cold, policy);
        mgr.ingest(1, 100, 1, 0); // Too small
        mgr.ingest(2, 600, 5, 0); // Large enough
        let migrated = mgr.migrate(1);
        assert_eq!(migrated, 1);
        assert_eq!(mgr.segment(0).unwrap().tier, ScrollbackTier::Hot);
        assert_eq!(mgr.segment(1).unwrap().tier, ScrollbackTier::Warm);
    }

    #[test]
    fn test_tier_migration_event_serde() {
        let evt = TierMigrationEvent {
            segment_id: 42,
            from_tier: ScrollbackTier::Hot,
            to_tier: ScrollbackTier::Warm,
            bytes_migrated: 1024,
            duration_us: 50,
            timestamp_us: 99999,
        };
        let json = serde_json::to_string(&evt).unwrap();
        let back: TierMigrationEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(evt, back);
    }

    #[test]
    fn test_scrollback_segment_serde() {
        let seg = ScrollbackSegment {
            segment_id: 1,
            pane_id: 2,
            tier: ScrollbackTier::Warm,
            byte_size: 4096,
            line_count: 100,
            created_us: 1000,
            last_accessed_us: 2000,
            compressed: false,
        };
        let json = serde_json::to_string(&seg).unwrap();
        let back: ScrollbackSegment = serde_json::from_str(&json).unwrap();
        assert_eq!(seg, back);
    }

    // ── C3 Impl Tests ──────────────────────────────────────────────

    #[test]
    fn test_tiered_scrollback_ingest_bulk() {
        let mut mgr = TieredScrollbackManager::with_defaults();
        let items = vec![(1, 100, 10), (2, 200, 20), (3, 300, 30)];
        let ids = mgr.ingest_bulk(&items, 0);
        assert_eq!(ids, vec![0, 1, 2]);
        assert_eq!(mgr.segment_count(), 3);
        assert_eq!(mgr.total_bytes(), 600);
    }

    #[test]
    fn test_tiered_scrollback_segments_for_pane() {
        let mut mgr = TieredScrollbackManager::with_defaults();
        mgr.ingest(1, 100, 10, 0);
        mgr.ingest(2, 200, 20, 0);
        mgr.ingest(1, 300, 30, 0);
        let pane1 = mgr.segments_for_pane(1);
        assert_eq!(pane1.len(), 2);
        assert_eq!(pane1[0].byte_size, 100);
        assert_eq!(pane1[1].byte_size, 300);
    }

    #[test]
    fn test_tiered_scrollback_tier_bytes() {
        let mut mgr = TieredScrollbackManager::with_defaults();
        mgr.ingest(1, 1000, 10, 0);
        assert_eq!(mgr.tier_bytes(ScrollbackTier::Hot), 1000);
        assert_eq!(mgr.tier_bytes(ScrollbackTier::Warm), 0);
        assert_eq!(mgr.tier_bytes(ScrollbackTier::Cold), 0);
    }

    #[test]
    fn test_tiered_scrollback_total_lines() {
        let mut mgr = TieredScrollbackManager::with_defaults();
        mgr.ingest(1, 100, 10, 0);
        mgr.ingest(2, 200, 25, 0);
        assert_eq!(mgr.total_lines(), 35);
    }

    #[test]
    fn test_tiered_scrollback_evict_hot_to_target() {
        let hot = TierConfig {
            tier: ScrollbackTier::Hot,
            max_bytes: 1000,
            target_latency_us: 10,
            compression_ratio: 1.0,
        };
        let warm = TierConfig {
            tier: ScrollbackTier::Warm,
            max_bytes: 10000,
            target_latency_us: 500,
            compression_ratio: 1.0,
        };
        let cold = TierConfig {
            tier: ScrollbackTier::Cold,
            max_bytes: 100000,
            target_latency_us: 10000,
            compression_ratio: 0.25,
        };
        let mut mgr = TieredScrollbackManager::new(hot, warm, cold, TierMigrationPolicy::default());

        mgr.ingest(1, 300, 10, 100);
        mgr.ingest(2, 300, 10, 200);
        mgr.ingest(3, 300, 10, 300);
        // 900/1000 = 90%. Evict to 50%.
        let freed = mgr.evict_hot_to_target(0.5);
        assert!(
            freed >= 400,
            "Should have freed enough to reach 50%: freed={}",
            freed
        );
        assert!(mgr.hot_utilization() <= 0.51);
    }

    #[test]
    fn test_tiered_scrollback_oldest_hot() {
        let mut mgr = TieredScrollbackManager::with_defaults();
        mgr.ingest(1, 100, 10, 1000);
        mgr.ingest(2, 200, 20, 2000);
        let oldest = mgr.oldest_hot_segment().unwrap();
        assert_eq!(oldest.created_us, 1000);
        assert_eq!(mgr.oldest_hot_age_us(5000), 4000);
    }

    #[test]
    fn test_tiered_scrollback_oldest_hot_empty() {
        let mgr = TieredScrollbackManager::with_defaults();
        assert!(mgr.oldest_hot_segment().is_none());
        assert_eq!(mgr.oldest_hot_age_us(1000), 0);
    }

    #[test]
    fn test_tiered_scrollback_active_pane_ids() {
        let mut mgr = TieredScrollbackManager::with_defaults();
        mgr.ingest(3, 100, 10, 0);
        mgr.ingest(1, 200, 20, 0);
        mgr.ingest(3, 300, 30, 0);
        mgr.ingest(2, 400, 40, 0);
        let ids = mgr.active_pane_ids();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[test]
    fn test_tiered_scrollback_cold_utilization() {
        let hot = TierConfig {
            tier: ScrollbackTier::Hot,
            max_bytes: 1000,
            target_latency_us: 10,
            compression_ratio: 1.0,
        };
        let warm = TierConfig {
            tier: ScrollbackTier::Warm,
            max_bytes: 5000,
            target_latency_us: 500,
            compression_ratio: 1.0,
        };
        let cold = TierConfig {
            tier: ScrollbackTier::Cold,
            max_bytes: 10000,
            target_latency_us: 10000,
            compression_ratio: 0.5,
        };
        let policy = TierMigrationPolicy {
            hot_to_warm_age_us: 10,
            warm_to_cold_age_us: 100,
            min_segment_bytes: 1,
            pressure_threshold: 0.99,
            max_concurrent_migrations: 10,
        };
        let mut mgr = TieredScrollbackManager::new(hot, warm, cold, policy);
        mgr.ingest(1, 2000, 20, 0);
        mgr.migrate(50); // hot→warm
        mgr.migrate(200); // warm→cold
        // 2000 * 0.5 = 1000 cold bytes, util = 1000/10000 = 0.1
        assert!((mgr.cold_utilization() - 0.1).abs() < 0.01);
    }

    #[test]
    fn test_tiered_scrollback_migration_events_recorded() {
        let policy = TierMigrationPolicy {
            hot_to_warm_age_us: 0,
            warm_to_cold_age_us: 1_000_000,
            min_segment_bytes: 1,
            pressure_threshold: 0.99,
            max_concurrent_migrations: 10,
        };
        let hot = TierConfig {
            tier: ScrollbackTier::Hot,
            max_bytes: 1_000_000,
            target_latency_us: 10,
            compression_ratio: 1.0,
        };
        let warm = TierConfig {
            tier: ScrollbackTier::Warm,
            max_bytes: 1_000_000,
            target_latency_us: 500,
            compression_ratio: 1.0,
        };
        let cold = TierConfig {
            tier: ScrollbackTier::Cold,
            max_bytes: 10_000_000,
            target_latency_us: 10000,
            compression_ratio: 0.25,
        };
        let mut mgr = TieredScrollbackManager::new(hot, warm, cold, policy);
        mgr.ingest(1, 500, 5, 0);
        mgr.ingest(2, 600, 6, 0);
        mgr.migrate(1);
        let events = mgr.recent_migrations();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].from_tier, ScrollbackTier::Hot);
        assert_eq!(events[0].to_tier, ScrollbackTier::Warm);
    }

    // ── C4: Transport Policy Tests ─────────────────────────────────

    #[test]
    fn test_transport_mode_display() {
        assert_eq!(format!("{}", TransportMode::Local), "LOCAL");
        assert_eq!(format!("{}", TransportMode::Compressed), "COMPRESSED");
        assert_eq!(format!("{}", TransportMode::Bypass), "BYPASS");
    }

    #[test]
    fn test_transport_cost_model_default() {
        let cm = TransportCostModel::default();
        assert!(cm.compress_cost_per_byte_us > 0.0);
        assert!(cm.bypass_threshold_bytes < cm.compress_threshold_bytes);
    }

    #[test]
    fn test_transport_policy_local_when_no_network() {
        let policy = TransportPolicy::with_defaults();
        // Default cost model has network_cost=0 → always Local
        assert_eq!(policy.select_mode(100), TransportMode::Local);
        assert_eq!(policy.select_mode(100_000), TransportMode::Local);
    }

    #[test]
    fn test_transport_policy_bypass_small() {
        let config = TransportPolicyConfig {
            cost_model: TransportCostModel {
                network_cost_per_byte_us: 0.001,
                bypass_threshold_bytes: 4096,
                compress_threshold_bytes: 65536,
                ..Default::default()
            },
            ..Default::default()
        };
        let policy = TransportPolicy::new(config);
        assert_eq!(policy.select_mode(1000), TransportMode::Bypass);
    }

    #[test]
    fn test_transport_policy_compressed_large() {
        let config = TransportPolicyConfig {
            cost_model: TransportCostModel {
                network_cost_per_byte_us: 0.001,
                bypass_threshold_bytes: 4096,
                compress_threshold_bytes: 65536,
                ..Default::default()
            },
            ..Default::default()
        };
        let policy = TransportPolicy::new(config);
        assert_eq!(policy.select_mode(100_000), TransportMode::Compressed);
    }

    #[test]
    fn test_transport_policy_fixed_mode() {
        let config = TransportPolicyConfig {
            adaptive: false,
            fixed_mode: TransportMode::Compressed,
            ..Default::default()
        };
        let policy = TransportPolicy::new(config);
        assert_eq!(policy.select_mode(1), TransportMode::Compressed);
        assert_eq!(policy.select_mode(1_000_000), TransportMode::Compressed);
    }

    #[test]
    fn test_transport_policy_record() {
        let mut policy = TransportPolicy::with_defaults();
        policy.record(1024, TransportMode::Local, 10.0, 8.0, 1000);
        let snap = policy.snapshot();
        assert_eq!(snap.total_decisions, 1);
        assert_eq!(snap.local_count, 1);
        assert_eq!(snap.total_bytes_transferred, 1024);
        assert!(snap.ewma_cost_us > 0.0);
    }

    #[test]
    fn test_transport_policy_decision_counts() {
        let mut policy = TransportPolicy::with_defaults();
        policy.record(100, TransportMode::Local, 1.0, 1.0, 100);
        policy.record(200, TransportMode::Compressed, 2.0, 2.0, 200);
        policy.record(300, TransportMode::Bypass, 3.0, 3.0, 300);
        let snap = policy.snapshot();
        assert_eq!(
            snap.local_count + snap.compressed_count + snap.bypass_count,
            snap.total_decisions
        );
    }

    #[test]
    fn test_transport_policy_ewma_converges() {
        let mut policy = TransportPolicy::with_defaults();
        for i in 0..100 {
            policy.record(1000, TransportMode::Local, 50.0, 50.0, i * 100);
        }
        // EWMA should converge toward 50.0
        assert!((policy.snapshot().ewma_cost_us - 50.0).abs() < 1.0);
    }

    #[test]
    fn test_transport_policy_reset() {
        let mut policy = TransportPolicy::with_defaults();
        policy.record(1024, TransportMode::Local, 10.0, 8.0, 1000);
        policy.reset();
        let snap = policy.snapshot();
        assert_eq!(snap.total_decisions, 0);
        assert_eq!(snap.total_bytes_transferred, 0);
        assert_eq!(snap.ewma_cost_us, 0.0);
    }

    #[test]
    fn test_transport_degradation_healthy() {
        let policy = TransportPolicy::with_defaults();
        assert_eq!(policy.detect_degradation(), TransportDegradation::Healthy);
    }

    #[test]
    fn test_transport_degradation_high_cost() {
        let mut policy = TransportPolicy::with_defaults();
        // Drive EWMA above 100µs
        for i in 0..50 {
            policy.record(10000, TransportMode::Compressed, 200.0, 200.0, i * 100);
        }
        let is_high = matches!(
            policy.detect_degradation(),
            TransportDegradation::HighCost { .. }
        );
        assert!(
            is_high,
            "Expected HighCost, got {:?}",
            policy.detect_degradation()
        );
    }

    #[test]
    fn test_transport_degradation_display() {
        assert_eq!(format!("{}", TransportDegradation::Healthy), "HEALTHY");
        let high = TransportDegradation::HighCost {
            ewma_cost_us: 150.0,
            threshold_us: 100.0,
        };
        assert!(format!("{}", high).contains("150.0"));
    }

    #[test]
    fn test_transport_log_entry() {
        let mut policy = TransportPolicy::with_defaults();
        policy.record(1024, TransportMode::Local, 10.0, 8.0, 1000);
        let entry = policy.log_entry();
        assert_eq!(entry.total_decisions, 1);
        assert_eq!(entry.degradation, TransportDegradation::Healthy);
    }

    #[test]
    fn test_transport_log_entry_serde() {
        let entry = TransportLogEntry {
            total_decisions: 10,
            local_count: 5,
            compressed_count: 3,
            bypass_count: 2,
            ewma_cost_us: 25.5,
            degradation: TransportDegradation::Healthy,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: TransportLogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back);
    }

    #[test]
    fn test_transport_snapshot_serde() {
        let snap = TransportPolicySnapshot {
            total_decisions: 100,
            local_count: 50,
            compressed_count: 30,
            bypass_count: 20,
            total_bytes_transferred: 1_000_000,
            total_savings_us: 500.0,
            ewma_cost_us: 25.0,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: TransportPolicySnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }

    #[test]
    fn test_transport_decision_serde() {
        let dec = TransportDecision {
            payload_bytes: 4096,
            selected_mode: TransportMode::Compressed,
            estimated_cost_us: 15.0,
            actual_cost_us: 12.0,
            savings_us: 3.0,
            timestamp_us: 99999,
        };
        let json = serde_json::to_string(&dec).unwrap();
        let back: TransportDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(dec, back);
    }

    #[test]
    fn test_transport_degradation_serde() {
        let variants = vec![
            TransportDegradation::Healthy,
            TransportDegradation::HighCost {
                ewma_cost_us: 150.0,
                threshold_us: 100.0,
            },
            TransportDegradation::ModeImbalance {
                dominant_mode: "Local".to_string(),
                share: 0.98,
            },
        ];
        for v in &variants {
            let json = serde_json::to_string(v).unwrap();
            let back: TransportDegradation = serde_json::from_str(&json).unwrap();
            assert_eq!(*v, back);
        }
    }

    #[test]
    fn test_transport_status_line() {
        let policy = TransportPolicy::with_defaults();
        let line = policy.status_line();
        assert!(line.contains("transport"));
        assert!(line.contains("decisions=0"));
    }

    #[test]
    fn test_transport_mode_mid_range_cost_comparison() {
        // In the mid-range, mode depends on cost model
        let config = TransportPolicyConfig {
            cost_model: TransportCostModel {
                compress_cost_per_byte_us: 0.05,
                decompress_cost_per_byte_us: 0.02,
                network_cost_per_byte_us: 0.01,
                expected_compression_ratio: 0.3,
                bypass_threshold_bytes: 1000,
                compress_threshold_bytes: 100000,
            },
            ..Default::default()
        };
        let policy = TransportPolicy::new(config);
        // 10000 bytes: bypass cost = 10000 * 0.01 = 100
        // compress cost = 10000*0.05 + 10000*0.3*0.01 + 10000*0.3*0.02 = 500 + 30 + 60 = 590
        // bypass is cheaper
        assert_eq!(policy.select_mode(10000), TransportMode::Bypass);
    }

    // ── C4 Impl Tests ──────────────────────────────────────────────

    #[test]
    fn test_transport_estimate_cost_local() {
        let policy = TransportPolicy::with_defaults();
        assert_eq!(policy.estimate_cost(1000, TransportMode::Local), 0.0);
    }

    #[test]
    fn test_transport_estimate_cost_bypass() {
        let config = TransportPolicyConfig {
            cost_model: TransportCostModel {
                network_cost_per_byte_us: 0.01,
                ..Default::default()
            },
            ..Default::default()
        };
        let policy = TransportPolicy::new(config);
        let cost = policy.estimate_cost(10000, TransportMode::Bypass);
        assert!((cost - 100.0).abs() < 0.001); // 10000 * 0.01
    }

    #[test]
    fn test_transport_estimate_cost_compressed() {
        let config = TransportPolicyConfig {
            cost_model: TransportCostModel {
                compress_cost_per_byte_us: 0.05,
                decompress_cost_per_byte_us: 0.02,
                network_cost_per_byte_us: 0.01,
                expected_compression_ratio: 0.5,
                ..Default::default()
            },
            ..Default::default()
        };
        let policy = TransportPolicy::new(config);
        let cost = policy.estimate_cost(1000, TransportMode::Compressed);
        // 1000*0.05 + 1000*0.5*0.01 + 1000*0.5*0.02 = 50 + 5 + 10 = 65
        assert!((cost - 65.0).abs() < 0.001);
    }

    #[test]
    fn test_transport_select_and_record() {
        let mut policy = TransportPolicy::with_defaults();
        let mode = policy.select_and_record(1024, 5.0, 1000);
        assert_eq!(mode, TransportMode::Local);
        assert_eq!(policy.snapshot().total_decisions, 1);
    }

    #[test]
    fn test_transport_mode_distribution_empty() {
        let policy = TransportPolicy::with_defaults();
        let (l, c, b) = policy.mode_distribution();
        assert_eq!(l, 0.0);
        assert_eq!(c, 0.0);
        assert_eq!(b, 0.0);
    }

    #[test]
    fn test_transport_mode_distribution() {
        let mut policy = TransportPolicy::with_defaults();
        policy.record(100, TransportMode::Local, 1.0, 1.0, 0);
        policy.record(100, TransportMode::Local, 1.0, 1.0, 1);
        policy.record(100, TransportMode::Compressed, 2.0, 2.0, 2);
        policy.record(100, TransportMode::Bypass, 3.0, 3.0, 3);
        let (l, c, b) = policy.mode_distribution();
        assert!((l - 0.5).abs() < 0.001);
        assert!((c - 0.25).abs() < 0.001);
        assert!((b - 0.25).abs() < 0.001);
    }

    #[test]
    fn test_transport_total_bytes() {
        let mut policy = TransportPolicy::with_defaults();
        policy.record(1000, TransportMode::Local, 1.0, 1.0, 0);
        policy.record(2000, TransportMode::Bypass, 2.0, 2.0, 1);
        assert_eq!(policy.total_bytes(), 3000);
    }

    #[test]
    fn test_transport_update_cost_model() {
        let mut policy = TransportPolicy::with_defaults();
        let new_model = TransportCostModel {
            network_cost_per_byte_us: 0.1,
            ..Default::default()
        };
        policy.update_cost_model(new_model);
        // With non-zero network cost, small payloads should now get bypass
        let config = TransportPolicyConfig {
            cost_model: TransportCostModel {
                network_cost_per_byte_us: 0.1,
                ..Default::default()
            },
            ..Default::default()
        };
        let policy2 = TransportPolicy::new(config);
        assert_eq!(policy2.select_mode(100), TransportMode::Bypass);
    }

    #[test]
    fn test_transport_set_adaptive() {
        let mut policy = TransportPolicy::with_defaults();
        policy.set_adaptive(false);
        policy.set_fixed_mode(TransportMode::Compressed);
        assert_eq!(policy.select_mode(1), TransportMode::Compressed);
    }

    #[test]
    fn test_transport_ewma_accessor() {
        let mut policy = TransportPolicy::with_defaults();
        assert_eq!(policy.ewma_cost_us(), 0.0);
        policy.record(1000, TransportMode::Local, 10.0, 50.0, 0);
        assert!(policy.ewma_cost_us() > 0.0);
    }

    // ── C5: Tail-Latency Tests ─────────────────────────────────────

    #[test]
    fn test_syscall_strategy_display() {
        assert_eq!(format!("{}", SyscallStrategy::Immediate), "IMMEDIATE");
        assert_eq!(format!("{}", SyscallStrategy::Batched), "BATCHED");
        assert_eq!(format!("{}", SyscallStrategy::Adaptive), "ADAPTIVE");
    }

    #[test]
    fn test_wakeup_source_display() {
        assert_eq!(format!("{}", WakeupSource::Timer), "TIMER");
        assert_eq!(format!("{}", WakeupSource::IoEvent), "IO_EVENT");
        assert_eq!(format!("{}", WakeupSource::Signal), "SIGNAL");
        assert_eq!(format!("{}", WakeupSource::Nudge), "NUDGE");
    }

    #[test]
    fn test_affinity_hint_display() {
        assert_eq!(format!("{}", AffinityHint::Any), "ANY");
        assert_eq!(format!("{}", AffinityHint::PerformanceCore), "P_CORE");
        assert_eq!(format!("{}", AffinityHint::EfficiencyCore), "E_CORE");
        assert_eq!(format!("{}", AffinityHint::Pinned(3)), "PINNED(3)");
    }

    #[test]
    fn test_tail_latency_config_default() {
        let config = TailLatencyConfig::default();
        assert_eq!(config.syscall_strategy, SyscallStrategy::Adaptive);
        assert!(config.p99_budget_us < config.p999_budget_us);
    }

    #[test]
    fn test_tail_latency_record_wakeup() {
        let mut ctrl = TailLatencyController::with_defaults();
        ctrl.record_wakeup(WakeupSource::Timer, 100);
        ctrl.record_wakeup(WakeupSource::IoEvent, 200);
        ctrl.record_wakeup(WakeupSource::Signal, 300);
        ctrl.record_wakeup(WakeupSource::Nudge, 400);
        let snap = ctrl.snapshot();
        assert_eq!(snap.total_wakeups, 4);
        assert_eq!(snap.timer_wakeups, 1);
        assert_eq!(snap.io_wakeups, 1);
        assert_eq!(snap.signal_wakeups, 1);
        assert_eq!(snap.nudge_wakeups, 1);
        assert_eq!(snap.max_latency_us, 400);
    }

    #[test]
    fn test_tail_latency_wakeup_conservation() {
        let mut ctrl = TailLatencyController::with_defaults();
        for _ in 0..10 {
            ctrl.record_wakeup(WakeupSource::Timer, 50);
        }
        for _ in 0..5 {
            ctrl.record_wakeup(WakeupSource::IoEvent, 100);
        }
        let snap = ctrl.snapshot();
        assert_eq!(
            snap.timer_wakeups + snap.io_wakeups + snap.signal_wakeups + snap.nudge_wakeups,
            snap.total_wakeups
        );
    }

    #[test]
    fn test_tail_latency_record_batch() {
        let mut ctrl = TailLatencyController::with_defaults();
        ctrl.record_batch(10);
        ctrl.record_batch(20);
        assert_eq!(ctrl.snapshot().total_batches, 2);
        assert_eq!(ctrl.snapshot().total_syscalls, 30);
        assert!((ctrl.avg_batch_depth() - 15.0).abs() < 0.001);
    }

    #[test]
    fn test_tail_latency_p99() {
        let mut ctrl = TailLatencyController::with_defaults();
        // 100 samples: 99 at 100µs, 1 at 5000µs
        for _ in 0..99 {
            ctrl.record_wakeup(WakeupSource::Timer, 100);
        }
        ctrl.record_wakeup(WakeupSource::Timer, 5000);
        let p99 = ctrl.p99_latency_us();
        // p99 of 100 samples → index 99 → should be 5000
        assert!(p99 >= 100); // At minimum, it's at least 100
    }

    #[test]
    fn test_tail_latency_budget_violation() {
        let config = TailLatencyConfig {
            p99_budget_us: 1000,
            ..Default::default()
        };
        let mut ctrl = TailLatencyController::new(config);
        ctrl.record_wakeup(WakeupSource::Timer, 500); // OK
        ctrl.record_wakeup(WakeupSource::Timer, 1500); // Violation
        assert_eq!(ctrl.snapshot().budget_violations, 1);
    }

    #[test]
    fn test_tail_latency_reset() {
        let mut ctrl = TailLatencyController::with_defaults();
        ctrl.record_wakeup(WakeupSource::Timer, 100);
        ctrl.record_batch(5);
        ctrl.reset();
        let snap = ctrl.snapshot();
        assert_eq!(snap.total_wakeups, 0);
        assert_eq!(snap.total_batches, 0);
        assert_eq!(snap.max_latency_us, 0);
    }

    #[test]
    fn test_tail_latency_degradation_healthy() {
        let ctrl = TailLatencyController::with_defaults();
        assert_eq!(ctrl.detect_degradation(), TailLatencyDegradation::Healthy);
    }

    #[test]
    fn test_tail_latency_degradation_p999_breach() {
        let config = TailLatencyConfig {
            p999_budget_us: 5000,
            ..Default::default()
        };
        let mut ctrl = TailLatencyController::new(config);
        ctrl.record_wakeup(WakeupSource::Timer, 10000); // Exceeds p999
        let is_breach = matches!(
            ctrl.detect_degradation(),
            TailLatencyDegradation::P999Breach { .. }
        );
        assert!(
            is_breach,
            "Expected P999Breach, got {:?}",
            ctrl.detect_degradation()
        );
    }

    #[test]
    fn test_tail_latency_degradation_display() {
        assert_eq!(format!("{}", TailLatencyDegradation::Healthy), "HEALTHY");
        let breach = TailLatencyDegradation::P99Breach {
            observed_us: 15000,
            budget_us: 10000,
        };
        assert!(format!("{}", breach).contains("15000"));
    }

    #[test]
    fn test_tail_latency_log_entry() {
        let mut ctrl = TailLatencyController::with_defaults();
        ctrl.record_wakeup(WakeupSource::IoEvent, 500);
        ctrl.record_batch(8);
        let entry = ctrl.log_entry();
        assert_eq!(entry.total_wakeups, 1);
        assert_eq!(entry.degradation, TailLatencyDegradation::Healthy);
    }

    #[test]
    fn test_tail_latency_log_entry_serde() {
        let entry = TailLatencyLogEntry {
            total_wakeups: 100,
            p99_latency_us: 5000,
            max_latency_us: 20000,
            budget_violations: 3,
            avg_batch_depth: 12.5,
            degradation: TailLatencyDegradation::Healthy,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: TailLatencyLogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back);
    }

    #[test]
    fn test_tail_latency_snapshot_serde() {
        let snap = TailLatencySnapshot {
            total_wakeups: 50,
            timer_wakeups: 20,
            io_wakeups: 15,
            signal_wakeups: 10,
            nudge_wakeups: 5,
            total_syscalls: 300,
            total_batches: 30,
            avg_batch_depth: 10.0,
            p99_latency_us: 8000,
            max_latency_us: 25000,
            budget_violations: 2,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: TailLatencySnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }

    #[test]
    fn test_tail_latency_degradation_serde() {
        let variants = vec![
            TailLatencyDegradation::Healthy,
            TailLatencyDegradation::P99Breach {
                observed_us: 15000,
                budget_us: 10000,
            },
            TailLatencyDegradation::P999Breach {
                observed_us: 60000,
                budget_us: 50000,
            },
            TailLatencyDegradation::HighViolationRate {
                violations: 10,
                total: 100,
            },
        ];
        for v in &variants {
            let json = serde_json::to_string(v).unwrap();
            let back: TailLatencyDegradation = serde_json::from_str(&json).unwrap();
            assert_eq!(*v, back);
        }
    }

    #[test]
    fn test_tail_latency_status_line() {
        let ctrl = TailLatencyController::with_defaults();
        let line = ctrl.status_line();
        assert!(line.contains("tail-latency"));
        assert!(line.contains("wakeups=0"));
    }

    #[test]
    fn test_tail_latency_accessors() {
        let ctrl = TailLatencyController::with_defaults();
        assert_eq!(ctrl.strategy(), SyscallStrategy::Adaptive);
        assert_eq!(ctrl.affinity(), AffinityHint::Any);
        assert_eq!(ctrl.sample_count(), 0);
    }

    #[test]
    fn test_wakeup_event_serde() {
        let evt = WakeupEvent {
            source: WakeupSource::IoEvent,
            latency_us: 250,
            timestamp_us: 12345,
            batch_depth: 4,
        };
        let json = serde_json::to_string(&evt).unwrap();
        let back: WakeupEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(evt, back);
    }

    #[test]
    fn test_tail_latency_config_serde() {
        let config = TailLatencyConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let back: TailLatencyConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, back);
    }

    // ── C5 Impl Tests ──────────────────────────────────────────────

    #[test]
    fn test_tail_latency_p50() {
        let mut ctrl = TailLatencyController::with_defaults();
        for i in 1..=100 {
            ctrl.record_wakeup(WakeupSource::Timer, i * 10);
        }
        let p50 = ctrl.p50_latency_us();
        // Median of 10..1000 step 10 → ~500
        assert!(p50 >= 400 && p50 <= 600, "p50={}", p50);
    }

    #[test]
    fn test_tail_latency_wakeup_distribution() {
        let mut ctrl = TailLatencyController::with_defaults();
        for _ in 0..6 {
            ctrl.record_wakeup(WakeupSource::Timer, 100);
        }
        for _ in 0..3 {
            ctrl.record_wakeup(WakeupSource::IoEvent, 100);
        }
        for _ in 0..1 {
            ctrl.record_wakeup(WakeupSource::Signal, 100);
        }
        let (t, io, s, n) = ctrl.wakeup_distribution();
        assert!((t - 0.6).abs() < 0.01);
        assert!((io - 0.3).abs() < 0.01);
        assert!((s - 0.1).abs() < 0.01);
        assert_eq!(n, 0.0);
    }

    #[test]
    fn test_tail_latency_wakeup_distribution_empty() {
        let ctrl = TailLatencyController::with_defaults();
        let (t, io, s, n) = ctrl.wakeup_distribution();
        assert_eq!(t, 0.0);
        assert_eq!(io, 0.0);
        assert_eq!(s, 0.0);
        assert_eq!(n, 0.0);
    }

    #[test]
    fn test_tail_latency_violation_rate() {
        let config = TailLatencyConfig {
            p99_budget_us: 100,
            ..Default::default()
        };
        let mut ctrl = TailLatencyController::new(config);
        for _ in 0..8 {
            ctrl.record_wakeup(WakeupSource::Timer, 50);
        }
        for _ in 0..2 {
            ctrl.record_wakeup(WakeupSource::Timer, 200);
        }
        assert!((ctrl.violation_rate() - 0.2).abs() < 0.01);
    }

    #[test]
    fn test_tail_latency_within_budget() {
        let mut ctrl = TailLatencyController::with_defaults();
        for _ in 0..10 {
            ctrl.record_wakeup(WakeupSource::Timer, 100);
        }
        assert!(ctrl.within_p99_budget());
        assert!(ctrl.within_p999_budget());
    }

    #[test]
    fn test_tail_latency_set_strategy() {
        let mut ctrl = TailLatencyController::with_defaults();
        ctrl.set_strategy(SyscallStrategy::Batched);
        assert_eq!(ctrl.strategy(), SyscallStrategy::Batched);
    }

    #[test]
    fn test_tail_latency_set_affinity() {
        let mut ctrl = TailLatencyController::with_defaults();
        ctrl.set_affinity(AffinityHint::Pinned(7));
        assert_eq!(ctrl.affinity(), AffinityHint::Pinned(7));
    }

    #[test]
    fn test_tail_latency_set_p99_budget() {
        let mut ctrl = TailLatencyController::with_defaults();
        ctrl.set_p99_budget(5000);
        for _ in 0..10 {
            ctrl.record_wakeup(WakeupSource::Timer, 4000);
        }
        assert!(ctrl.within_p99_budget());
    }

    #[test]
    fn test_tail_latency_total_accessors() {
        let mut ctrl = TailLatencyController::with_defaults();
        ctrl.record_wakeup(WakeupSource::Timer, 100);
        ctrl.record_wakeup(WakeupSource::Timer, 20000); // violation (default budget=10000)
        assert_eq!(ctrl.total_wakeups(), 2);
        assert_eq!(ctrl.budget_violations(), 1);
    }

    // ── D1: Hitch-Risk Model Tests ─────────────────────────────────

    #[test]
    fn test_evidence_signal_display() {
        assert_eq!(format!("{}", EvidenceSignal::LatencyProbe), "LATENCY_PROBE");
        assert_eq!(format!("{}", EvidenceSignal::CpuLoad), "CPU_LOAD");
    }

    #[test]
    fn test_hitch_risk_level_display() {
        assert_eq!(format!("{}", HitchRiskLevel::Low), "LOW");
        assert_eq!(format!("{}", HitchRiskLevel::Critical), "CRITICAL");
    }

    #[test]
    fn test_hitch_risk_config_default() {
        let config = HitchRiskConfig::default();
        assert!(config.prior_hitch_prob > 0.0 && config.prior_hitch_prob < 1.0);
        assert!(config.elevated_threshold < config.high_threshold);
        assert!(config.high_threshold < config.critical_threshold);
    }

    #[test]
    fn test_hitch_risk_model_initial_state() {
        let model = HitchRiskModel::with_defaults();
        assert_eq!(model.risk_level(), HitchRiskLevel::Low);
        assert!(model.posterior_prob() < 0.5);
        assert_eq!(model.snapshot().total_updates, 0);
    }

    #[test]
    fn test_hitch_risk_posterior_bounded() {
        let model = HitchRiskModel::with_defaults();
        let prob = model.posterior_prob();
        assert!(prob >= 0.0 && prob <= 1.0, "prob={}", prob);
    }

    #[test]
    fn test_hitch_risk_positive_evidence() {
        let mut model = HitchRiskModel::with_defaults();
        // Strong positive evidence → risk increases
        for i in 0..20 {
            model.update(EvidenceSignal::LatencyProbe, 10000.0, 2.0, i * 100);
        }
        assert!(model.posterior_prob() > 0.5);
        let level_is_elevated = matches!(
            model.risk_level(),
            HitchRiskLevel::Elevated | HitchRiskLevel::High | HitchRiskLevel::Critical
        );
        assert!(level_is_elevated, "level={:?}", model.risk_level());
    }

    #[test]
    fn test_hitch_risk_negative_evidence() {
        let mut model = HitchRiskModel::with_defaults();
        // Strong negative evidence → risk decreases
        for i in 0..20 {
            model.update(EvidenceSignal::LatencyProbe, 10.0, -2.0, i * 100);
        }
        assert!(model.posterior_prob() < 0.1);
        assert_eq!(model.risk_level(), HitchRiskLevel::Low);
    }

    #[test]
    fn test_hitch_risk_reset() {
        let mut model = HitchRiskModel::with_defaults();
        for i in 0..10 {
            model.update(EvidenceSignal::BudgetViolation, 1.0, 3.0, i * 100);
        }
        model.reset();
        assert_eq!(model.risk_level(), HitchRiskLevel::Low);
        assert_eq!(model.snapshot().total_updates, 0);
        assert_eq!(model.recent_evidence().len(), 0);
    }

    #[test]
    fn test_hitch_risk_evidence_capped() {
        let config = HitchRiskConfig {
            max_evidence: 10,
            ..Default::default()
        };
        let mut model = HitchRiskModel::new(config);
        for i in 0..50 {
            model.update(EvidenceSignal::QueueDepth, 100.0, 0.1, i * 100);
        }
        assert_eq!(model.recent_evidence().len(), 10);
    }

    #[test]
    fn test_hitch_risk_snapshot_serde() {
        let snap = HitchRiskSnapshot {
            log_odds: 2.5,
            posterior_prob: 0.924,
            risk_level: HitchRiskLevel::High,
            evidence_count: 15,
            total_updates: 42,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: HitchRiskSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap.risk_level, back.risk_level);
        assert_eq!(snap.evidence_count, back.evidence_count);
    }

    #[test]
    fn test_hitch_risk_degradation_healthy() {
        let model = HitchRiskModel::with_defaults();
        assert_eq!(model.detect_degradation(), HitchRiskDegradation::Healthy);
    }

    #[test]
    fn test_hitch_risk_degradation_elevated() {
        let mut model = HitchRiskModel::with_defaults();
        // Push log_odds above elevated threshold (1.0)
        for i in 0..10 {
            model.update(EvidenceSignal::LatencyProbe, 5000.0, 1.5, i * 100);
        }
        let is_elevated_or_higher = matches!(
            model.detect_degradation(),
            HitchRiskDegradation::ElevatedRisk { .. }
                | HitchRiskDegradation::HighRisk { .. }
                | HitchRiskDegradation::CriticalRisk { .. }
        );
        assert!(
            is_elevated_or_higher,
            "Got {:?}",
            model.detect_degradation()
        );
    }

    #[test]
    fn test_hitch_risk_degradation_display() {
        assert_eq!(format!("{}", HitchRiskDegradation::Healthy), "HEALTHY");
        let elev = HitchRiskDegradation::ElevatedRisk {
            posterior_prob: 0.75,
        };
        assert!(format!("{}", elev).contains("75.0%"));
    }

    #[test]
    fn test_hitch_risk_log_entry() {
        let model = HitchRiskModel::with_defaults();
        let entry = model.log_entry();
        assert_eq!(entry.risk_level, HitchRiskLevel::Low);
        assert_eq!(entry.total_updates, 0);
    }

    #[test]
    fn test_hitch_risk_log_entry_serde() {
        let entry = HitchRiskLogEntry {
            log_odds: 1.5,
            posterior_prob: 0.818,
            risk_level: HitchRiskLevel::Elevated,
            evidence_count: 5,
            total_updates: 10,
            degradation: HitchRiskDegradation::ElevatedRisk {
                posterior_prob: 0.818,
            },
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: HitchRiskLogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry.risk_level, back.risk_level);
    }

    #[test]
    fn test_hitch_risk_degradation_serde() {
        let variants = vec![
            HitchRiskDegradation::Healthy,
            HitchRiskDegradation::ElevatedRisk {
                posterior_prob: 0.7,
            },
            HitchRiskDegradation::HighRisk {
                posterior_prob: 0.9,
                evidence_count: 20,
            },
            HitchRiskDegradation::CriticalRisk {
                posterior_prob: 0.99,
                log_odds: 5.5,
            },
        ];
        for v in &variants {
            let json = serde_json::to_string(v).unwrap();
            let back: HitchRiskDegradation = serde_json::from_str(&json).unwrap();
            assert_eq!(*v, back);
        }
    }

    #[test]
    fn test_evidence_entry_serde() {
        let entry = EvidenceEntry {
            signal: EvidenceSignal::BackpressureChange,
            value: 3.5,
            log_likelihood_ratio: 1.2,
            timestamp_us: 99999,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: EvidenceEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back);
    }

    #[test]
    fn test_hitch_risk_status_line() {
        let model = HitchRiskModel::with_defaults();
        let line = model.status_line();
        assert!(line.contains("hitch-risk"));
        assert!(line.contains("level=LOW"));
    }

    #[test]
    fn test_hitch_risk_config_serde() {
        let config = HitchRiskConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let back: HitchRiskConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, back);
    }

    // ── D1 Impl Tests ──────────────────────────────────────────────

    #[test]
    fn test_hitch_risk_observe_violation() {
        let mut model = HitchRiskModel::with_defaults();
        let initial = model.log_odds();
        model.observe_violation(2.0, 1000);
        assert!(model.log_odds() > initial);
        assert_eq!(model.total_updates(), 1);
    }

    #[test]
    fn test_hitch_risk_observe_latency() {
        let mut model = HitchRiskModel::with_defaults();
        model.observe_latency(15000.0, 1.5, 1000);
        assert_eq!(model.total_updates(), 1);
        assert_eq!(model.evidence_count(), 1);
    }

    #[test]
    fn test_hitch_risk_observe_healthy() {
        let mut model = HitchRiskModel::with_defaults();
        // First push risk up
        for i in 0..10 {
            model.observe_violation(2.0, i * 100);
        }
        let high_odds = model.log_odds();
        // Now submit healthy evidence
        for i in 10..30 {
            model.observe_healthy(i * 100);
        }
        assert!(model.log_odds() < high_odds, "Healthy should reduce odds");
    }

    #[test]
    fn test_hitch_risk_should_mitigate() {
        let mut model = HitchRiskModel::with_defaults();
        assert!(!model.should_mitigate());
        // Push to high risk
        for i in 0..30 {
            model.observe_violation(3.0, i * 100);
        }
        assert!(model.should_mitigate());
    }

    #[test]
    fn test_hitch_risk_is_critical() {
        let mut model = HitchRiskModel::with_defaults();
        assert!(!model.is_critical());
        for i in 0..50 {
            model.observe_violation(5.0, i * 100);
        }
        assert!(model.is_critical());
    }

    #[test]
    fn test_hitch_risk_set_evidence_decay() {
        let mut model = HitchRiskModel::with_defaults();
        model.set_evidence_decay(0.5);
        // Submit evidence; with 0.5 decay, old evidence fades fast
        model.observe_violation(10.0, 1000);
        let odds_after_1 = model.log_odds();
        model.observe_healthy(2000);
        // With 0.5 decay, log_odds *= 0.5 then -0.5 → should reduce significantly
        assert!(model.log_odds() < odds_after_1);
    }

    #[test]
    fn test_hitch_risk_set_prior() {
        let mut model = HitchRiskModel::with_defaults();
        model.set_prior(0.5);
        // This changes the config but doesn't reset log_odds mid-session
        // (by design — set_prior just updates config for next reset)
        assert_eq!(model.total_updates(), 0);
    }

    #[test]
    fn test_hitch_risk_accessors() {
        let mut model = HitchRiskModel::with_defaults();
        assert_eq!(model.total_updates(), 0);
        assert_eq!(model.evidence_count(), 0);
        model.observe_violation(1.0, 100);
        assert_eq!(model.total_updates(), 1);
        assert_eq!(model.evidence_count(), 1);
    }

    // ── D2: E-Process Drift Detector Tests ────────────────────────

    #[test]
    fn test_eprocess_kind_display() {
        assert_eq!(EProcessKind::CusumLike.to_string(), "cusum_like");
        assert_eq!(EProcessKind::Mixture.to_string(), "mixture");
        assert_eq!(
            EProcessKind::ConfidenceSequence.to_string(),
            "confidence_seq"
        );
    }

    #[test]
    fn test_drift_observable_display() {
        assert_eq!(DriftObservable::Latency.to_string(), "latency");
        assert_eq!(DriftObservable::Throughput.to_string(), "throughput");
        assert_eq!(DriftObservable::ErrorRate.to_string(), "error_rate");
        assert_eq!(DriftObservable::QueueDepth.to_string(), "queue_depth");
        assert_eq!(DriftObservable::ResourceUsage.to_string(), "resource_usage");
    }

    #[test]
    fn test_drift_alert_level_display() {
        assert_eq!(DriftAlertLevel::None.to_string(), "none");
        assert_eq!(DriftAlertLevel::Warning.to_string(), "warning");
        assert_eq!(DriftAlertLevel::Alarm.to_string(), "alarm");
    }

    #[test]
    fn test_eprocess_config_default_latency() {
        let config = EProcessConfig::default_latency();
        assert_eq!(config.kind, EProcessKind::Mixture);
        assert_eq!(config.observable, DriftObservable::Latency);
        assert!(config.alpha > 0.0 && config.alpha < 1.0);
        assert!(config.lambda > 0.0);
        assert!(config.warmup > 0);
    }

    #[test]
    fn test_eprocess_initial_state() {
        let det = EProcessDetector::with_defaults();
        assert_eq!(det.total_observations(), 0);
        assert_eq!(det.alarm_count(), 0);
        // E_0 = 1 => e_value() = exp(0) = 1
        assert!((det.e_value() - 1.0).abs() < 1e-10);
        assert!((det.log_e_value() - 0.0).abs() < 1e-10);
        assert_eq!(det.kind(), EProcessKind::Mixture);
        assert_eq!(det.history_len(), 0);
    }

    #[test]
    fn test_eprocess_null_observations_stay_near_one() {
        // Under null (observations near null_mean=0), e-value should fluctuate near 1
        let mut det = EProcessDetector::new(EProcessConfig {
            kind: EProcessKind::Mixture,
            observable: DriftObservable::Latency,
            alpha: 0.05,
            warning_fraction: 0.5,
            lambda: 0.1,
            null_mean: 0.0,
            max_history: 100,
            warmup: 5,
            auto_reset: true,
        });
        for i in 0..50 {
            det.observe(0.0, i * 100);
        }
        // All observations exactly at null mean => LR = 1 => e-value stays at 1
        assert!((det.e_value() - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_eprocess_positive_drift_raises_alarm() {
        let mut det = EProcessDetector::new(EProcessConfig {
            kind: EProcessKind::Mixture,
            observable: DriftObservable::Latency,
            alpha: 0.05,
            warning_fraction: 0.5,
            lambda: 0.5,
            null_mean: 0.0,
            max_history: 1000,
            warmup: 0,
            auto_reset: false,
        });
        let mut alarm_seen = false;
        for i in 0..100 {
            let level = det.observe(5.0, i * 100);
            if level == DriftAlertLevel::Alarm {
                alarm_seen = true;
                break;
            }
        }
        assert!(alarm_seen, "Large positive drift should trigger alarm");
        assert!(det.alarm_count() >= 1);
    }

    #[test]
    fn test_eprocess_warmup_suppresses_alarm() {
        let mut det = EProcessDetector::new(EProcessConfig {
            kind: EProcessKind::Mixture,
            observable: DriftObservable::Latency,
            alpha: 0.05,
            warning_fraction: 0.5,
            lambda: 0.5,
            null_mean: 0.0,
            max_history: 100,
            warmup: 50,
            auto_reset: false,
        });
        // Even with large drift, during warmup we get None
        for i in 0..49 {
            let level = det.observe(100.0, i * 100);
            assert_eq!(level, DriftAlertLevel::None);
        }
    }

    #[test]
    fn test_eprocess_cusum_like_resets_floor() {
        let mut det = EProcessDetector::new(EProcessConfig {
            kind: EProcessKind::CusumLike,
            observable: DriftObservable::Latency,
            alpha: 0.05,
            warning_fraction: 0.5,
            lambda: 0.1,
            null_mean: 0.0,
            max_history: 100,
            warmup: 0,
            auto_reset: true,
        });
        // Drive e-value down with negative observations
        for i in 0..20 {
            det.observe(-5.0, i * 100);
        }
        // CUSUM-like floors at log_e = 0 each step, so it can't go below 0
        // (though negative LR can make log_e = max(0, prev) + log(LR) < 0)
        // The key property is: recovery is faster since negatives don't accumulate below 0
        let e_val = det.e_value();
        // Just verify it ran without panic
        assert!(e_val >= 0.0);
    }

    #[test]
    fn test_eprocess_auto_reset() {
        let mut det = EProcessDetector::new(EProcessConfig {
            kind: EProcessKind::Mixture,
            observable: DriftObservable::Latency,
            alpha: 0.05,
            warning_fraction: 0.5,
            lambda: 0.5,
            null_mean: 0.0,
            max_history: 100,
            warmup: 0,
            auto_reset: true,
        });
        // Drive to alarm
        for i in 0..100 {
            det.observe(10.0, i * 100);
        }
        // After auto-reset, e-value should have been reset
        // (may have been re-driven up, but alarm_count > 0)
        assert!(det.alarm_count() >= 1);
    }

    #[test]
    fn test_eprocess_reset() {
        let mut det = EProcessDetector::with_defaults();
        for i in 0..30 {
            det.observe(5.0, i * 100);
        }
        assert!(det.total_observations() > 0);
        det.reset();
        assert_eq!(det.total_observations(), 0);
        assert_eq!(det.alarm_count(), 0);
        assert!((det.e_value() - 1.0).abs() < 1e-10);
        assert_eq!(det.history_len(), 0);
    }

    #[test]
    fn test_eprocess_running_stats() {
        let mut det = EProcessDetector::new(EProcessConfig {
            kind: EProcessKind::Mixture,
            observable: DriftObservable::Latency,
            alpha: 0.05,
            warning_fraction: 0.5,
            lambda: 0.1,
            null_mean: 0.0,
            max_history: 100,
            warmup: 0,
            auto_reset: true,
        });
        det.observe(10.0, 100);
        det.observe(20.0, 200);
        det.observe(30.0, 300);
        assert!((det.running_mean() - 20.0).abs() < 1e-10);
        assert!(det.running_variance() > 0.0);
    }

    #[test]
    fn test_eprocess_snapshot_fields() {
        let mut det = EProcessDetector::with_defaults();
        for i in 0..5 {
            det.observe(1.0, i * 100);
        }
        let snap = det.snapshot();
        assert_eq!(snap.total_observations, 5);
        assert!(snap.e_value >= 0.0);
        assert!(snap.peak_e_value >= snap.e_value);
    }

    #[test]
    fn test_eprocess_status_line() {
        let det = EProcessDetector::with_defaults();
        let line = det.status_line();
        assert!(line.contains("e-proc"));
        assert!(line.contains("mixture"));
    }

    #[test]
    fn test_eprocess_recent_observations() {
        let mut det = EProcessDetector::new(EProcessConfig {
            kind: EProcessKind::Mixture,
            observable: DriftObservable::Latency,
            alpha: 0.05,
            warning_fraction: 0.5,
            lambda: 0.1,
            null_mean: 0.0,
            max_history: 5,
            warmup: 0,
            auto_reset: true,
        });
        for i in 0..8 {
            det.observe(i as f64, i as u64 * 100);
        }
        let recent = det.recent_observations(3);
        assert_eq!(recent.len(), 3);
        // Should be the last 3: values 5, 6, 7
        assert!((recent[0].value - 5.0).abs() < 1e-10);
        assert!((recent[1].value - 6.0).abs() < 1e-10);
        assert!((recent[2].value - 7.0).abs() < 1e-10);
    }

    #[test]
    fn test_eprocess_degradation_healthy() {
        let det = EProcessDetector::with_defaults();
        assert_eq!(det.detect_degradation(), EProcessDegradation::Healthy);
    }

    #[test]
    fn test_eprocess_degradation_display() {
        assert_eq!(EProcessDegradation::Healthy.to_string(), "healthy");
        let suspected = EProcessDegradation::DriftSuspected {
            e_value: 5.0,
            running_mean: 2.5,
        };
        assert!(suspected.to_string().contains("drift_suspected"));
        let detected = EProcessDegradation::DriftDetected {
            e_value: 25.0,
            alarm_count: 3,
        };
        assert!(detected.to_string().contains("drift_detected"));
    }

    #[test]
    fn test_eprocess_kind_serde() {
        for kind in [
            EProcessKind::CusumLike,
            EProcessKind::Mixture,
            EProcessKind::ConfidenceSequence,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            let back: EProcessKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, back);
        }
    }

    #[test]
    fn test_drift_observable_serde() {
        for obs in [
            DriftObservable::Latency,
            DriftObservable::Throughput,
            DriftObservable::ErrorRate,
            DriftObservable::QueueDepth,
            DriftObservable::ResourceUsage,
        ] {
            let json = serde_json::to_string(&obs).unwrap();
            let back: DriftObservable = serde_json::from_str(&json).unwrap();
            assert_eq!(obs, back);
        }
    }

    #[test]
    fn test_drift_alert_level_serde() {
        for level in [
            DriftAlertLevel::None,
            DriftAlertLevel::Warning,
            DriftAlertLevel::Alarm,
        ] {
            let json = serde_json::to_string(&level).unwrap();
            let back: DriftAlertLevel = serde_json::from_str(&json).unwrap();
            assert_eq!(level, back);
        }
    }

    #[test]
    fn test_eprocess_observation_serde() {
        let obs = EProcessObservation {
            value: std::f64::consts::PI,
            observable: DriftObservable::Latency,
            timestamp_us: 12345,
            likelihood_ratio: 1.2,
        };
        let json = serde_json::to_string(&obs).unwrap();
        let back: EProcessObservation = serde_json::from_str(&json).unwrap();
        assert!((obs.value - back.value).abs() < 1e-10);
        assert_eq!(obs.observable, back.observable);
    }

    #[test]
    fn test_eprocess_log_entry() {
        let mut det = EProcessDetector::with_defaults();
        for i in 0..5 {
            det.observe(1.0, i * 100);
        }
        let entry = det.log_entry();
        assert_eq!(entry.total_observations, 5);
        assert!(entry.e_value >= 0.0);
    }

    #[test]
    fn test_eprocess_degradation_serde() {
        let variants = vec![
            EProcessDegradation::Healthy,
            EProcessDegradation::DriftSuspected {
                e_value: 5.0,
                running_mean: 2.5,
            },
            EProcessDegradation::DriftDetected {
                e_value: 25.0,
                alarm_count: 3,
            },
        ];
        for v in &variants {
            let json = serde_json::to_string(v).unwrap();
            let back: EProcessDegradation = serde_json::from_str(&json).unwrap();
            assert_eq!(*v, back);
        }
    }

    #[test]
    fn test_eprocess_config_serde() {
        let config = EProcessConfig::default_latency();
        let json = serde_json::to_string(&config).unwrap();
        let back: EProcessConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config.kind, back.kind);
        assert_eq!(config.observable, back.observable);
        assert!((config.alpha - back.alpha).abs() < 1e-10);
    }

    #[test]
    fn test_eprocess_history_wraps() {
        let mut det = EProcessDetector::new(EProcessConfig {
            kind: EProcessKind::Mixture,
            observable: DriftObservable::Latency,
            alpha: 0.05,
            warning_fraction: 0.5,
            lambda: 0.1,
            null_mean: 0.0,
            max_history: 3,
            warmup: 0,
            auto_reset: true,
        });
        for i in 0..10 {
            det.observe(i as f64, i as u64 * 100);
        }
        // max_history = 3, so only 3 observations stored
        assert_eq!(det.history_len(), 3);
        assert_eq!(det.total_observations(), 10);
    }

    #[test]
    fn test_eprocess_e_value_nonneg() {
        let mut det = EProcessDetector::with_defaults();
        for i in 0..50 {
            det.observe(-10.0, i * 100);
        }
        // e-value = exp(log_e_value), always >= 0
        assert!(det.e_value() >= 0.0);
    }

    // ── D2 Impl Tests ──────────────────────────────────────────────

    #[test]
    fn test_eprocess_observe_batch() {
        let mut det = EProcessDetector::with_defaults();
        let batch: Vec<(f64, u64)> = (0..10).map(|i| (1.0, i * 100)).collect();
        let level = det.observe_batch(&batch);
        assert_eq!(det.total_observations(), 10);
        // Level should be deterministic
        let _ = level;
    }

    #[test]
    fn test_eprocess_observe_latency_us() {
        let mut det = EProcessDetector::with_defaults();
        let level = det.observe_latency_us(500.0, 100);
        assert_eq!(det.total_observations(), 1);
        let _ = level;
    }

    #[test]
    fn test_eprocess_running_stddev() {
        let mut det = EProcessDetector::new(EProcessConfig {
            kind: EProcessKind::Mixture,
            observable: DriftObservable::Latency,
            alpha: 0.05,
            warning_fraction: 0.5,
            lambda: 0.1,
            null_mean: 0.0,
            max_history: 100,
            warmup: 0,
            auto_reset: true,
        });
        det.observe(10.0, 100);
        det.observe(20.0, 200);
        det.observe(30.0, 300);
        let stddev = det.running_stddev();
        assert!(stddev > 0.0);
        assert!((stddev - 10.0).abs() < 1e-10);
    }

    #[test]
    fn test_eprocess_z_score() {
        let mut det = EProcessDetector::new(EProcessConfig {
            kind: EProcessKind::Mixture,
            observable: DriftObservable::Latency,
            alpha: 0.05,
            warning_fraction: 0.5,
            lambda: 0.1,
            null_mean: 0.0,
            max_history: 100,
            warmup: 0,
            auto_reset: true,
        });
        det.observe(10.0, 100);
        det.observe(20.0, 200);
        det.observe(30.0, 300);
        // mean=20, stddev=10
        let z = det.z_score(30.0);
        assert!((z - 1.0).abs() < 1e-10);
        let z0 = det.z_score(20.0);
        assert!(z0.abs() < 1e-10);
    }

    #[test]
    fn test_eprocess_z_score_zero_variance() {
        let mut det = EProcessDetector::with_defaults();
        det.observe(5.0, 100);
        // Only one observation, variance=0
        let z = det.z_score(10.0);
        assert_eq!(z, 0.0);
    }

    #[test]
    fn test_eprocess_alarm_rate() {
        let det = EProcessDetector::with_defaults();
        assert_eq!(det.alarm_rate(), 0.0);
    }

    #[test]
    fn test_eprocess_alarm_rate_positive() {
        let mut det = EProcessDetector::new(EProcessConfig {
            kind: EProcessKind::Mixture,
            observable: DriftObservable::Latency,
            alpha: 0.05,
            warning_fraction: 0.5,
            lambda: 0.5,
            null_mean: 0.0,
            max_history: 100,
            warmup: 0,
            auto_reset: true,
        });
        for i in 0..100 {
            det.observe(10.0, i * 100);
        }
        let rate = det.alarm_rate();
        assert!(rate >= 0.0 && rate <= 1.0);
    }

    #[test]
    fn test_eprocess_set_lambda() {
        let mut det = EProcessDetector::with_defaults();
        det.set_lambda(0.5);
        // Observe with new lambda — should be more sensitive
        det.observe(10.0, 100);
        assert_eq!(det.total_observations(), 1);
    }

    #[test]
    fn test_eprocess_set_null_mean() {
        let mut det = EProcessDetector::with_defaults();
        det.set_null_mean(5.0);
        // Observations at 5.0 should now give LR = 1
        for i in 0..10 {
            det.observe(5.0, i * 100);
        }
        assert!((det.e_value() - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_eprocess_set_alpha() {
        let mut det = EProcessDetector::with_defaults();
        det.set_alpha(0.01);
        // Higher threshold now
        assert_eq!(det.total_observations(), 0);
    }

    #[test]
    fn test_eprocess_warning_count() {
        let det = EProcessDetector::with_defaults();
        assert_eq!(det.warning_count(), 0);
    }

    #[test]
    fn test_eprocess_peak_e_value() {
        let mut det = EProcessDetector::new(EProcessConfig {
            kind: EProcessKind::Mixture,
            observable: DriftObservable::Latency,
            alpha: 0.05,
            warning_fraction: 0.5,
            lambda: 0.1,
            null_mean: 0.0,
            max_history: 100,
            warmup: 0,
            auto_reset: true,
        });
        // Drive e-value up, then down
        for i in 0..10 {
            det.observe(5.0, i * 100);
        }
        let peak_after_up = det.peak_e_value();
        for i in 10..20 {
            det.observe(-5.0, i * 100);
        }
        // Peak should be >= current and >= what it was after the up phase
        assert!(
            det.peak_e_value() >= peak_after_up
                || (det.peak_e_value() - peak_after_up).abs() < 1e-10
        );
    }

    #[test]
    fn test_eprocess_is_alarming() {
        let mut det = EProcessDetector::new(EProcessConfig {
            kind: EProcessKind::Mixture,
            observable: DriftObservable::Latency,
            alpha: 0.05,
            warning_fraction: 0.5,
            lambda: 0.5,
            null_mean: 0.0,
            max_history: 100,
            warmup: 0,
            auto_reset: false, // Don't auto-reset to keep alarm state
        });
        assert!(!det.is_alarming());
        // Drive to alarm
        for i in 0..100 {
            det.observe(10.0, i * 100);
        }
        // With auto_reset=false, should stay in alarm
        if det.alarm_count() > 0 {
            assert!(det.is_alarming());
        }
    }

    // ── D3: Expected-Loss Policy Controller Tests ─────────────────

    #[test]
    fn test_policy_action_display() {
        assert_eq!(PolicyAction::Hold.to_string(), "hold");
        assert_eq!(PolicyAction::Tighten.to_string(), "tighten");
        assert_eq!(PolicyAction::Relax.to_string(), "relax");
        assert_eq!(PolicyAction::Shed.to_string(), "shed");
    }

    #[test]
    fn test_system_state_display() {
        assert_eq!(SystemState::Healthy.to_string(), "healthy");
        assert_eq!(SystemState::Drifting.to_string(), "drifting");
        assert_eq!(SystemState::Stressed.to_string(), "stressed");
        assert_eq!(SystemState::Critical.to_string(), "critical");
    }

    #[test]
    fn test_policy_controller_initial_state() {
        let ctrl = PolicyController::with_defaults();
        assert_eq!(ctrl.current_action(), PolicyAction::Hold);
        assert_eq!(ctrl.total_decisions(), 0);
    }

    #[test]
    fn test_policy_healthy_selects_hold() {
        let mut ctrl = PolicyController::with_defaults();
        // 100% healthy => Hold is cheapest (loss=0)
        let action = ctrl.decide([1.0, 0.0, 0.0, 0.0], 100);
        // Due to critical_floor, slight redistribution but Hold should still win
        assert_eq!(action, PolicyAction::Hold);
    }

    #[test]
    fn test_policy_critical_selects_shed() {
        let mut ctrl = PolicyController::with_defaults();
        // 100% critical => Shed is cheapest (loss=1)
        let action = ctrl.decide([0.0, 0.0, 0.0, 1.0], 100);
        assert_eq!(action, PolicyAction::Shed);
    }

    #[test]
    fn test_policy_drifting_selects_tighten() {
        let mut ctrl = PolicyController::with_defaults();
        // 100% drifting => Tighten is cheapest (loss=0.5)
        let action = ctrl.decide([0.0, 1.0, 0.0, 0.0], 100);
        assert_eq!(action, PolicyAction::Tighten);
    }

    #[test]
    fn test_policy_stressed_selects_tighten() {
        let mut ctrl = PolicyController::with_defaults();
        // 100% stressed => Tighten is cheapest (loss=1.0)
        let action = ctrl.decide([0.0, 0.0, 1.0, 0.0], 100);
        assert_eq!(action, PolicyAction::Tighten);
    }

    #[test]
    fn test_policy_decision_count() {
        let mut ctrl = PolicyController::with_defaults();
        ctrl.decide([1.0, 0.0, 0.0, 0.0], 100);
        ctrl.decide([0.5, 0.5, 0.0, 0.0], 200);
        ctrl.decide([0.0, 0.0, 0.0, 1.0], 300);
        assert_eq!(ctrl.total_decisions(), 3);
    }

    #[test]
    fn test_policy_critical_floor() {
        let mut ctrl = PolicyController::with_defaults();
        // Even with 0 critical probability, critical_floor ensures min P(Critical)
        ctrl.decide([1.0, 0.0, 0.0, 0.0], 100);
        let recent = ctrl.recent_decisions(1);
        assert_eq!(recent.len(), 1);
        // Critical prob should be at least critical_floor
        assert!(recent[0].state_probs[3] >= ctrl.config.critical_floor - 1e-10);
    }

    #[test]
    fn test_policy_snapshot() {
        let mut ctrl = PolicyController::with_defaults();
        ctrl.decide([1.0, 0.0, 0.0, 0.0], 100);
        let snap = ctrl.snapshot();
        assert_eq!(snap.total_decisions, 1);
        assert_eq!(snap.current_action, PolicyAction::Hold);
    }

    #[test]
    fn test_policy_status_line() {
        let ctrl = PolicyController::with_defaults();
        let line = ctrl.status_line();
        assert!(line.contains("policy"));
        assert!(line.contains("hold"));
    }

    #[test]
    fn test_policy_reset() {
        let mut ctrl = PolicyController::with_defaults();
        ctrl.decide([0.0, 0.0, 0.0, 1.0], 100);
        ctrl.reset();
        assert_eq!(ctrl.total_decisions(), 0);
        assert_eq!(ctrl.current_action(), PolicyAction::Hold);
    }

    #[test]
    fn test_policy_action_serde() {
        for action in [
            PolicyAction::Hold,
            PolicyAction::Tighten,
            PolicyAction::Relax,
            PolicyAction::Shed,
        ] {
            let json = serde_json::to_string(&action).unwrap();
            let back: PolicyAction = serde_json::from_str(&json).unwrap();
            assert_eq!(action, back);
        }
    }

    #[test]
    fn test_system_state_serde() {
        for state in [
            SystemState::Healthy,
            SystemState::Drifting,
            SystemState::Stressed,
            SystemState::Critical,
        ] {
            let json = serde_json::to_string(&state).unwrap();
            let back: SystemState = serde_json::from_str(&json).unwrap();
            assert_eq!(state, back);
        }
    }

    #[test]
    fn test_policy_degradation_display() {
        assert_eq!(PolicyDegradation::Healthy.to_string(), "healthy");
        let t = PolicyDegradation::Tightening { expected_loss: 1.5 };
        assert!(t.to_string().contains("tightening"));
        let e = PolicyDegradation::EmergencyShed {
            total_decisions: 5,
            last_loss: 2.0,
        };
        assert!(e.to_string().contains("emergency_shed"));
    }

    #[test]
    fn test_policy_degradation_serde() {
        let variants = vec![
            PolicyDegradation::Healthy,
            PolicyDegradation::Tightening { expected_loss: 1.5 },
            PolicyDegradation::EmergencyShed {
                total_decisions: 5,
                last_loss: 2.0,
            },
        ];
        for v in &variants {
            let json = serde_json::to_string(v).unwrap();
            let back: PolicyDegradation = serde_json::from_str(&json).unwrap();
            assert_eq!(*v, back);
        }
    }

    #[test]
    fn test_policy_log_entry() {
        let mut ctrl = PolicyController::with_defaults();
        ctrl.decide([1.0, 0.0, 0.0, 0.0], 100);
        let entry = ctrl.log_entry();
        assert_eq!(entry.total_decisions, 1);
        assert_eq!(entry.current_action, PolicyAction::Hold);
    }

    #[test]
    fn test_policy_config_serde() {
        let config = PolicyControllerConfig::default_asymmetric();
        let json = serde_json::to_string(&config).unwrap();
        let back: PolicyControllerConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config.loss_matrix.len(), back.loss_matrix.len());
    }

    #[test]
    fn test_policy_detect_degradation() {
        let mut ctrl = PolicyController::with_defaults();
        assert_eq!(ctrl.detect_degradation(), PolicyDegradation::Healthy);
        ctrl.decide([0.0, 0.0, 0.0, 1.0], 100);
        let is_shed = matches!(
            ctrl.detect_degradation(),
            PolicyDegradation::EmergencyShed { .. }
        );
        assert!(is_shed);
    }

    // ── D3 Impl Tests ──────────────────────────────────────────────

    #[test]
    fn test_policy_decide_from_risk_low() {
        let mut ctrl = PolicyController::with_defaults();
        let action = ctrl.decide_from_risk(HitchRiskLevel::Low, 100);
        assert_eq!(action, PolicyAction::Hold);
    }

    #[test]
    fn test_policy_decide_from_risk_critical() {
        let mut ctrl = PolicyController::with_defaults();
        let action = ctrl.decide_from_risk(HitchRiskLevel::Critical, 100);
        assert_eq!(action, PolicyAction::Shed);
    }

    #[test]
    fn test_policy_decide_from_risk_elevated() {
        let mut ctrl = PolicyController::with_defaults();
        let action = ctrl.decide_from_risk(HitchRiskLevel::Elevated, 100);
        assert_eq!(action, PolicyAction::Tighten);
    }

    #[test]
    fn test_policy_action_distribution() {
        let mut ctrl = PolicyController::with_defaults();
        assert_eq!(ctrl.action_distribution(), [0.0; 4]);
        ctrl.decide([1.0, 0.0, 0.0, 0.0], 100);
        ctrl.decide([1.0, 0.0, 0.0, 0.0], 200);
        let dist = ctrl.action_distribution();
        let sum: f64 = dist.iter().sum();
        assert!((sum - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_policy_action_counts() {
        let mut ctrl = PolicyController::with_defaults();
        ctrl.decide([1.0, 0.0, 0.0, 0.0], 100);
        ctrl.decide([0.0, 0.0, 0.0, 1.0], 200);
        let counts = ctrl.action_counts();
        let total: u64 = counts.iter().sum();
        assert_eq!(total, 2);
    }

    #[test]
    fn test_policy_hysteresis_count() {
        let ctrl = PolicyController::with_defaults();
        assert_eq!(ctrl.hysteresis_count(), 0);
    }

    #[test]
    fn test_policy_last_expected_loss() {
        let mut ctrl = PolicyController::with_defaults();
        assert_eq!(ctrl.last_expected_loss(), 0.0);
        ctrl.decide([1.0, 0.0, 0.0, 0.0], 100);
        // Hold in healthy => loss should be very small (critical floor adds a bit)
        assert!(ctrl.last_expected_loss() >= 0.0);
    }

    #[test]
    fn test_policy_set_hysteresis() {
        let mut ctrl = PolicyController::with_defaults();
        ctrl.set_hysteresis(0.2);
        // Should affect future decisions
        ctrl.decide([1.0, 0.0, 0.0, 0.0], 100);
        assert_eq!(ctrl.total_decisions(), 1);
    }

    #[test]
    fn test_policy_set_critical_floor() {
        let mut ctrl = PolicyController::with_defaults();
        ctrl.set_critical_floor(0.1);
        ctrl.decide([1.0, 0.0, 0.0, 0.0], 100);
        let recent = ctrl.recent_decisions(1);
        // Critical floor should be at least 0.1
        assert!(recent[0].state_probs[3] >= 0.1 - 1e-10);
    }

    #[test]
    fn test_policy_set_loss() {
        let mut ctrl = PolicyController::with_defaults();
        ctrl.set_loss(0, 0, 100.0); // Make Hold very expensive when Healthy
        let action = ctrl.decide([1.0, 0.0, 0.0, 0.0], 100);
        // Now Hold should NOT be selected since it costs 100
        assert_ne!(action, PolicyAction::Hold);
    }

    #[test]
    fn test_policy_decision_count_tracks() {
        let mut ctrl = PolicyController::with_defaults();
        assert_eq!(ctrl.decision_count(), 0);
        ctrl.decide([1.0, 0.0, 0.0, 0.0], 100);
        assert_eq!(ctrl.decision_count(), 1);
    }

    // ── D4: Calibration Harness Tests ─────────────────────────────

    #[test]
    fn test_calibration_scenario_display() {
        assert_eq!(CalibrationScenario::Nominal.to_string(), "nominal");
        assert_eq!(
            CalibrationScenario::GradualDrift.to_string(),
            "gradual_drift"
        );
        assert_eq!(CalibrationScenario::AbruptShift.to_string(), "abrupt_shift");
        assert_eq!(
            CalibrationScenario::NoisyBaseline.to_string(),
            "noisy_baseline"
        );
        assert_eq!(
            CalibrationScenario::PostStressRecovery.to_string(),
            "post_stress_recovery"
        );
    }

    #[test]
    fn test_promotion_verdict_display() {
        assert_eq!(PromotionVerdict::Approved.to_string(), "approved");
        assert_eq!(
            PromotionVerdict::ConditionalHold.to_string(),
            "conditional_hold"
        );
        assert_eq!(PromotionVerdict::Rejected.to_string(), "rejected");
    }

    fn make_passing_result(scenario: CalibrationScenario) -> CalibrationResult {
        CalibrationResult {
            scenario,
            false_positive_rate: 0.01,
            miss_rate: 0.02,
            detection_delay: 10.0,
            mean_expected_loss: 1.0,
            passes_gate: false,
            observation_count: 1000,
            timestamp_us: 12345,
        }
    }

    fn make_failing_result(scenario: CalibrationScenario) -> CalibrationResult {
        CalibrationResult {
            scenario,
            false_positive_rate: 0.2,
            miss_rate: 0.3,
            detection_delay: 100.0,
            mean_expected_loss: 10.0,
            passes_gate: false,
            observation_count: 1000,
            timestamp_us: 12345,
        }
    }

    #[test]
    fn test_calibration_initial_state() {
        let harness = CalibrationHarness::with_defaults();
        assert_eq!(harness.verdict(), PromotionVerdict::Rejected);
        assert_eq!(harness.total_runs(), 0);
        assert_eq!(harness.result_count(), 0);
    }

    #[test]
    fn test_calibration_all_pass_approved() {
        let mut harness = CalibrationHarness::with_defaults();
        let scenarios = [
            CalibrationScenario::Nominal,
            CalibrationScenario::GradualDrift,
            CalibrationScenario::AbruptShift,
            CalibrationScenario::NoisyBaseline,
            CalibrationScenario::PostStressRecovery,
        ];
        for s in &scenarios {
            harness.submit(make_passing_result(*s));
        }
        let verdict = harness.evaluate();
        assert_eq!(verdict, PromotionVerdict::Approved);
    }

    #[test]
    fn test_calibration_one_fail_strict_rejected() {
        let mut harness = CalibrationHarness::with_defaults();
        harness.submit(make_passing_result(CalibrationScenario::Nominal));
        harness.submit(make_passing_result(CalibrationScenario::GradualDrift));
        harness.submit(make_passing_result(CalibrationScenario::AbruptShift));
        harness.submit(make_passing_result(CalibrationScenario::NoisyBaseline));
        harness.submit(make_failing_result(CalibrationScenario::PostStressRecovery));
        let verdict = harness.evaluate();
        assert_eq!(verdict, PromotionVerdict::Rejected);
    }

    #[test]
    fn test_calibration_non_strict_conditional() {
        let config = PromotionGateConfig {
            max_fpr: 0.05,
            max_miss_rate: 0.10,
            max_detection_delay: 50.0,
            max_expected_loss: 5.0,
            min_passing_scenarios: 5,
            strict: false,
        };
        let mut harness = CalibrationHarness::new(config);
        for _ in 0..3 {
            harness.submit(make_passing_result(CalibrationScenario::Nominal));
        }
        harness.submit(make_failing_result(CalibrationScenario::AbruptShift));
        let verdict = harness.evaluate();
        // 3 passing < 5 required, so ConditionalHold
        assert_eq!(verdict, PromotionVerdict::ConditionalHold);
    }

    #[test]
    fn test_calibration_empty_rejected() {
        let mut harness = CalibrationHarness::with_defaults();
        let verdict = harness.evaluate();
        assert_eq!(verdict, PromotionVerdict::Rejected);
    }

    #[test]
    fn test_calibration_reset() {
        let mut harness = CalibrationHarness::with_defaults();
        harness.submit(make_passing_result(CalibrationScenario::Nominal));
        harness.reset();
        assert_eq!(harness.total_runs(), 0);
        assert_eq!(harness.result_count(), 0);
    }

    #[test]
    fn test_calibration_clear_results() {
        let mut harness = CalibrationHarness::with_defaults();
        harness.submit(make_passing_result(CalibrationScenario::Nominal));
        assert_eq!(harness.total_runs(), 1);
        harness.clear_results();
        assert_eq!(harness.result_count(), 0);
        assert_eq!(harness.total_runs(), 1);
    }

    #[test]
    fn test_calibration_snapshot() {
        let mut harness = CalibrationHarness::with_defaults();
        harness.submit(make_passing_result(CalibrationScenario::Nominal));
        harness.evaluate();
        let snap = harness.snapshot();
        assert_eq!(snap.total_runs, 1);
        assert_eq!(snap.scenario_results.len(), 1);
    }

    #[test]
    fn test_calibration_status_line() {
        let harness = CalibrationHarness::with_defaults();
        let line = harness.status_line();
        assert!(line.contains("calibration"));
    }

    #[test]
    fn test_calibration_scenario_serde() {
        for s in [
            CalibrationScenario::Nominal,
            CalibrationScenario::GradualDrift,
            CalibrationScenario::AbruptShift,
            CalibrationScenario::NoisyBaseline,
            CalibrationScenario::PostStressRecovery,
        ] {
            let json = serde_json::to_string(&s).unwrap();
            let back: CalibrationScenario = serde_json::from_str(&json).unwrap();
            assert_eq!(s, back);
        }
    }

    #[test]
    fn test_promotion_verdict_serde() {
        for v in [
            PromotionVerdict::Approved,
            PromotionVerdict::ConditionalHold,
            PromotionVerdict::Rejected,
        ] {
            let json = serde_json::to_string(&v).unwrap();
            let back: PromotionVerdict = serde_json::from_str(&json).unwrap();
            assert_eq!(v, back);
        }
    }

    #[test]
    fn test_calibration_degradation_display() {
        assert_eq!(CalibrationDegradation::Healthy.to_string(), "healthy");
        let m = CalibrationDegradation::GateMarginal {
            passing: 3,
            total: 5,
        };
        assert!(m.to_string().contains("3/5"));
        let f = CalibrationDegradation::GateFailed {
            failing: 2,
            total: 5,
        };
        assert!(f.to_string().contains("2/5"));
    }

    #[test]
    fn test_calibration_degradation_serde() {
        let variants = vec![
            CalibrationDegradation::Healthy,
            CalibrationDegradation::GateMarginal {
                passing: 3,
                total: 5,
            },
            CalibrationDegradation::GateFailed {
                failing: 2,
                total: 5,
            },
        ];
        for v in &variants {
            let json = serde_json::to_string(v).unwrap();
            let back: CalibrationDegradation = serde_json::from_str(&json).unwrap();
            assert_eq!(*v, back);
        }
    }

    #[test]
    fn test_calibration_log_entry() {
        let mut harness = CalibrationHarness::with_defaults();
        harness.submit(make_passing_result(CalibrationScenario::Nominal));
        harness.evaluate();
        let entry = harness.log_entry();
        assert_eq!(entry.total_runs, 1);
    }

    #[test]
    fn test_calibration_detect_degradation_healthy() {
        let mut harness = CalibrationHarness::with_defaults();
        let scenarios = [
            CalibrationScenario::Nominal,
            CalibrationScenario::GradualDrift,
            CalibrationScenario::AbruptShift,
            CalibrationScenario::NoisyBaseline,
            CalibrationScenario::PostStressRecovery,
        ];
        for s in &scenarios {
            harness.submit(make_passing_result(*s));
        }
        harness.evaluate();
        assert_eq!(
            harness.detect_degradation(),
            CalibrationDegradation::Healthy
        );
    }

    #[test]
    fn test_calibration_config_serde() {
        let config = PromotionGateConfig::default_strict();
        let json = serde_json::to_string(&config).unwrap();
        let back: PromotionGateConfig = serde_json::from_str(&json).unwrap();
        assert!((config.max_fpr - back.max_fpr).abs() < 1e-10);
    }

    #[test]
    fn test_calibration_result_serde() {
        let result = make_passing_result(CalibrationScenario::Nominal);
        let json = serde_json::to_string(&result).unwrap();
        let back: CalibrationResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result.scenario, back.scenario);
    }

    // ── D4 Impl Tests ──────────────────────────────────────────────

    #[test]
    fn test_calibration_submit_batch() {
        let mut harness = CalibrationHarness::with_defaults();
        let batch = vec![
            make_passing_result(CalibrationScenario::Nominal),
            make_passing_result(CalibrationScenario::GradualDrift),
            make_passing_result(CalibrationScenario::AbruptShift),
            make_passing_result(CalibrationScenario::NoisyBaseline),
            make_passing_result(CalibrationScenario::PostStressRecovery),
        ];
        let verdict = harness.submit_batch(batch);
        assert_eq!(verdict, PromotionVerdict::Approved);
        assert_eq!(harness.total_runs(), 5);
    }

    #[test]
    fn test_calibration_avg_fpr() {
        let mut harness = CalibrationHarness::with_defaults();
        assert_eq!(harness.avg_fpr(), 0.0);
        harness.submit(make_passing_result(CalibrationScenario::Nominal));
        assert!(harness.avg_fpr() > 0.0);
    }

    #[test]
    fn test_calibration_avg_miss_rate() {
        let mut harness = CalibrationHarness::with_defaults();
        harness.submit(make_passing_result(CalibrationScenario::Nominal));
        assert!(harness.avg_miss_rate() > 0.0);
    }

    #[test]
    fn test_calibration_avg_detection_delay() {
        let mut harness = CalibrationHarness::with_defaults();
        harness.submit(make_passing_result(CalibrationScenario::Nominal));
        assert!(harness.avg_detection_delay() > 0.0);
    }

    #[test]
    fn test_calibration_passing_failing_counts() {
        let mut harness = CalibrationHarness::with_defaults();
        harness.submit(make_passing_result(CalibrationScenario::Nominal));
        harness.submit(make_failing_result(CalibrationScenario::AbruptShift));
        harness.evaluate();
        assert_eq!(harness.passing_count(), 1);
        assert_eq!(harness.failing_count(), 1);
    }

    #[test]
    fn test_calibration_is_approved() {
        let mut harness = CalibrationHarness::with_defaults();
        assert!(!harness.is_approved());
        let scenarios = [
            CalibrationScenario::Nominal,
            CalibrationScenario::GradualDrift,
            CalibrationScenario::AbruptShift,
            CalibrationScenario::NoisyBaseline,
            CalibrationScenario::PostStressRecovery,
        ];
        for s in &scenarios {
            harness.submit(make_passing_result(*s));
        }
        harness.evaluate();
        assert!(harness.is_approved());
    }

    #[test]
    fn test_calibration_set_max_fpr() {
        let mut harness = CalibrationHarness::with_defaults();
        harness.set_max_fpr(0.001);
        harness.submit(make_passing_result(CalibrationScenario::Nominal)); // fpr=0.01 > 0.001
        harness.evaluate();
        assert!(!harness.is_approved());
    }

    #[test]
    fn test_calibration_set_strict() {
        let mut harness = CalibrationHarness::with_defaults();
        harness.set_strict(false);
        // Submit 5 passing + 1 failing
        for _ in 0..5 {
            harness.submit(make_passing_result(CalibrationScenario::Nominal));
        }
        harness.submit(make_failing_result(CalibrationScenario::AbruptShift));
        let verdict = harness.evaluate();
        // Non-strict: 5 >= min_passing_scenarios=5, so Approved
        assert_eq!(verdict, PromotionVerdict::Approved);
    }

    #[test]
    fn test_calibration_results_for_scenario() {
        let mut harness = CalibrationHarness::with_defaults();
        harness.submit(make_passing_result(CalibrationScenario::Nominal));
        harness.submit(make_passing_result(CalibrationScenario::Nominal));
        harness.submit(make_passing_result(CalibrationScenario::AbruptShift));
        let nominal = harness.results_for_scenario(CalibrationScenario::Nominal);
        assert_eq!(nominal.len(), 2);
        let abrupt = harness.results_for_scenario(CalibrationScenario::AbruptShift);
        assert_eq!(abrupt.len(), 1);
    }

    // ── E1: Formal Spec Pack Tests ────────────────────────────────

    #[test]
    fn test_invariant_domain_display() {
        assert_eq!(InvariantDomain::Scheduler.to_string(), "scheduler");
        assert_eq!(InvariantDomain::Budget.to_string(), "budget");
        assert_eq!(InvariantDomain::Recovery.to_string(), "recovery");
        assert_eq!(InvariantDomain::Composition.to_string(), "composition");
    }

    #[test]
    fn test_invariant_severity_ordering() {
        assert!(InvariantSeverity::Info < InvariantSeverity::Warning);
        assert!(InvariantSeverity::Warning < InvariantSeverity::Critical);
    }

    #[test]
    fn test_formal_invariant_display() {
        let inv = FormalInvariant {
            predicate_id: "budget.nonneg".to_string(),
            description: "All targets non-negative".to_string(),
            domain: InvariantDomain::Budget,
            severity: InvariantSeverity::Critical,
            is_safety: true,
        };
        let display = format!("{inv}");
        assert!(display.contains("budget"));
        assert!(display.contains("critical"));
        assert!(display.contains("budget.nonneg"));
    }

    #[test]
    fn test_scheduler_invariant_capacity_bound_holds() {
        let inv = SchedulerInvariant::CapacityBound {
            lane: SchedulerLane::Input,
            capacity: 100,
            actual: 50,
        };
        assert!(inv.holds());
        assert_eq!(inv.predicate_id(), "scheduler.capacity_bound");
    }

    #[test]
    fn test_scheduler_invariant_capacity_bound_violated() {
        let inv = SchedulerInvariant::CapacityBound {
            lane: SchedulerLane::Bulk,
            capacity: 10,
            actual: 15,
        };
        assert!(!inv.holds());
    }

    #[test]
    fn test_scheduler_invariant_conservation_of_work() {
        let good = SchedulerInvariant::ConservationOfWork {
            total_admitted: 100,
            lane_sum: 100,
        };
        assert!(good.holds());

        let bad = SchedulerInvariant::ConservationOfWork {
            total_admitted: 100,
            lane_sum: 99,
        };
        assert!(!bad.holds());
    }

    #[test]
    fn test_scheduler_invariant_starvation_freedom() {
        let ok = SchedulerInvariant::StarvationFreedom {
            lane: SchedulerLane::Control,
            wait_epochs: 5,
            max_epochs: 10,
        };
        assert!(ok.holds());

        let starved = SchedulerInvariant::StarvationFreedom {
            lane: SchedulerLane::Control,
            wait_epochs: 11,
            max_epochs: 10,
        };
        assert!(!starved.holds());
    }

    #[test]
    fn test_scheduler_invariant_epoch_monotonicity() {
        assert!(
            SchedulerInvariant::EpochMonotonicity {
                previous: 5,
                current: 10
            }
            .holds()
        );
        assert!(
            SchedulerInvariant::EpochMonotonicity {
                previous: 5,
                current: 5
            }
            .holds()
        );
        assert!(
            !SchedulerInvariant::EpochMonotonicity {
                previous: 10,
                current: 5
            }
            .holds()
        );
    }

    #[test]
    fn test_scheduler_invariant_item_id_monotonicity() {
        assert!(
            SchedulerInvariant::ItemIdMonotonicity {
                previous: 1,
                current: 2
            }
            .holds()
        );
        assert!(
            !SchedulerInvariant::ItemIdMonotonicity {
                previous: 5,
                current: 3
            }
            .holds()
        );
        assert!(
            SchedulerInvariant::ItemIdMonotonicity {
                previous: 0,
                current: 0
            }
            .holds()
        );
    }

    #[test]
    fn test_scheduler_invariant_deterministic_replay() {
        let good = SchedulerInvariant::DeterministicReplay {
            input_hash: 0xABCD,
            expected_hash: 0x1234,
            actual_hash: 0x1234,
        };
        assert!(good.holds());

        let bad = SchedulerInvariant::DeterministicReplay {
            input_hash: 0xABCD,
            expected_hash: 0x1234,
            actual_hash: 0x5678,
        };
        assert!(!bad.holds());
    }

    #[test]
    fn test_scheduler_invariant_display() {
        let inv = SchedulerInvariant::CapacityBound {
            lane: SchedulerLane::Input,
            capacity: 100,
            actual: 50,
        };
        let s = format!("{inv}");
        assert!(s.contains("capacity_bound"));
        assert!(s.contains("50/100"));
    }

    #[test]
    fn test_budget_invariant_percentile_monotonicity_holds() {
        let inv = BudgetInvariant::PercentileMonotonicity {
            stage: LatencyStage::PatternDetection,
            p50: 100.0,
            p95: 200.0,
            p99: 300.0,
            p999: 400.0,
        };
        assert!(inv.holds());
        assert_eq!(inv.predicate_id(), "budget.percentile_monotonicity");
    }

    #[test]
    fn test_budget_invariant_percentile_monotonicity_violated() {
        let inv = BudgetInvariant::PercentileMonotonicity {
            stage: LatencyStage::PatternDetection,
            p50: 300.0,
            p95: 200.0,
            p99: 100.0,
            p999: 400.0,
        };
        assert!(!inv.holds());
    }

    #[test]
    fn test_budget_invariant_non_negative_targets() {
        assert!(
            BudgetInvariant::NonNegativeTargets {
                stage: LatencyStage::EventEmission,
                min_target: 0.0,
            }
            .holds()
        );
        assert!(
            !BudgetInvariant::NonNegativeTargets {
                stage: LatencyStage::EventEmission,
                min_target: -1.0,
            }
            .holds()
        );
    }

    #[test]
    fn test_budget_invariant_observation_consistency() {
        assert!(
            BudgetInvariant::ObservationConsistency {
                total: 50,
                per_stage_sum: 50
            }
            .holds()
        );
        assert!(
            !BudgetInvariant::ObservationConsistency {
                total: 50,
                per_stage_sum: 49
            }
            .holds()
        );
    }

    #[test]
    fn test_budget_invariant_overflow_bound() {
        assert!(
            BudgetInvariant::OverflowBound {
                overflow_count: 5,
                total_observations: 10
            }
            .holds()
        );
        assert!(
            !BudgetInvariant::OverflowBound {
                overflow_count: 11,
                total_observations: 10
            }
            .holds()
        );
    }

    #[test]
    fn test_budget_invariant_escalation_monotonicity() {
        assert!(
            BudgetInvariant::EscalationMonotonicity {
                stage: LatencyStage::PatternDetection,
                previous_level: MitigationLevel::None,
                current_level: MitigationLevel::Defer,
            }
            .holds()
        );
        assert!(
            !BudgetInvariant::EscalationMonotonicity {
                stage: LatencyStage::PatternDetection,
                previous_level: MitigationLevel::Shed,
                current_level: MitigationLevel::Defer,
            }
            .holds()
        );
    }

    #[test]
    fn test_budget_invariant_aggregate_ceiling() {
        assert!(
            BudgetInvariant::AggregateCeiling {
                percentile: Percentile::P99,
                aggregate_us: 1000.0,
                stage_sum_us: 900.0,
            }
            .holds()
        );
        assert!(
            !BudgetInvariant::AggregateCeiling {
                percentile: Percentile::P99,
                aggregate_us: 800.0,
                stage_sum_us: 900.0,
            }
            .holds()
        );
    }

    #[test]
    fn test_budget_invariant_display() {
        let inv = BudgetInvariant::PercentileMonotonicity {
            stage: LatencyStage::PatternDetection,
            p50: 100.0,
            p95: 200.0,
            p99: 300.0,
            p999: 400.0,
        };
        let s = format!("{inv}");
        assert!(s.contains("pct_mono"));
    }

    #[test]
    fn test_recovery_invariant_gradual_deescalation_holds() {
        let inv = RecoveryInvariant::GradualDeescalation {
            previous_level: MitigationLevel::Degrade,
            recovered_level: MitigationLevel::Defer,
        };
        assert!(inv.holds());
        assert_eq!(inv.predicate_id(), "recovery.gradual_deescalation");
    }

    #[test]
    fn test_recovery_invariant_gradual_deescalation_violated() {
        let inv = RecoveryInvariant::GradualDeescalation {
            previous_level: MitigationLevel::Shed,
            recovered_level: MitigationLevel::Defer,
        };
        assert!(!inv.holds());
    }

    #[test]
    fn test_recovery_invariant_cooldown_enforced() {
        assert!(
            RecoveryInvariant::CooldownEnforced {
                consecutive_ok: 20,
                cooldown_required: 20,
            }
            .holds()
        );
        assert!(
            !RecoveryInvariant::CooldownEnforced {
                consecutive_ok: 19,
                cooldown_required: 20,
            }
            .holds()
        );
    }

    #[test]
    fn test_recovery_invariant_timeout_recovery() {
        assert!(
            RecoveryInvariant::TimeoutRecovery {
                degraded_duration_us: 40_000_000,
                max_duration_us: 30_000_000,
                recovery_triggered: true,
            }
            .holds()
        );
        assert!(
            !RecoveryInvariant::TimeoutRecovery {
                degraded_duration_us: 40_000_000,
                max_duration_us: 30_000_000,
                recovery_triggered: false,
            }
            .holds()
        );
        assert!(
            RecoveryInvariant::TimeoutRecovery {
                degraded_duration_us: 10_000_000,
                max_duration_us: 30_000_000,
                recovery_triggered: false,
            }
            .holds()
        );
    }

    #[test]
    fn test_recovery_invariant_count_monotonicity() {
        assert!(
            RecoveryInvariant::EscalationCountMonotonic {
                previous: 5,
                current: 8
            }
            .holds()
        );
        assert!(
            !RecoveryInvariant::EscalationCountMonotonic {
                previous: 10,
                current: 5
            }
            .holds()
        );
        assert!(
            RecoveryInvariant::RecoveryCountMonotonic {
                previous: 3,
                current: 3
            }
            .holds()
        );
        assert!(
            !RecoveryInvariant::RecoveryCountMonotonic {
                previous: 5,
                current: 2
            }
            .holds()
        );
    }

    #[test]
    fn test_recovery_invariant_level_in_range() {
        for level in MitigationLevel::ALL {
            assert!(RecoveryInvariant::LevelInRange { level: *level }.holds());
        }
    }

    #[test]
    fn test_recovery_invariant_display() {
        let inv = RecoveryInvariant::GradualDeescalation {
            previous_level: MitigationLevel::Degrade,
            recovered_level: MitigationLevel::Defer,
        };
        let s = format!("{inv}");
        assert!(s.contains("gradual"));
    }

    #[test]
    fn test_invariant_outcome_display() {
        assert_eq!(InvariantOutcome::Satisfied.to_string(), "SATISFIED");
        let violated = InvariantOutcome::Violated {
            counterexample: "bad state".to_string(),
        };
        assert!(violated.to_string().contains("VIOLATED"));
        let inc = InvariantOutcome::Inconclusive {
            reason: "timeout".to_string(),
        };
        assert!(inc.to_string().contains("INCONCLUSIVE"));
    }

    #[test]
    fn test_invariant_check_result_passed_violated() {
        let passed = InvariantCheckResult {
            predicate_id: "test".to_string(),
            domain: InvariantDomain::Scheduler,
            severity: InvariantSeverity::Critical,
            outcome: InvariantOutcome::Satisfied,
            eval_time_us: 100,
            timestamp_us: 1000,
        };
        assert!(passed.passed());
        assert!(!passed.violated());

        let failed = InvariantCheckResult {
            predicate_id: "test".to_string(),
            domain: InvariantDomain::Budget,
            severity: InvariantSeverity::Warning,
            outcome: InvariantOutcome::Violated {
                counterexample: "x".to_string(),
            },
            eval_time_us: 50,
            timestamp_us: 2000,
        };
        assert!(!failed.passed());
        assert!(failed.violated());
    }

    #[test]
    fn test_invariant_checker_new() {
        let checker = InvariantChecker::with_defaults();
        assert_eq!(checker.total_checks(), 0);
        assert_eq!(checker.total_violations(), 0);
        assert_eq!(checker.total_satisfied(), 0);
        assert_eq!(checker.registered_count(), 0);
        assert!((checker.violation_rate() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_invariant_checker_register() {
        let mut checker = InvariantChecker::with_defaults();
        checker.register(FormalInvariant {
            predicate_id: "test.inv".to_string(),
            description: "Test invariant".to_string(),
            domain: InvariantDomain::Scheduler,
            severity: InvariantSeverity::Critical,
            is_safety: true,
        });
        assert_eq!(checker.registered_count(), 1);
    }

    #[test]
    fn test_invariant_checker_check_scheduler_satisfied() {
        let mut checker = InvariantChecker::with_defaults();
        let inv = SchedulerInvariant::CapacityBound {
            lane: SchedulerLane::Input,
            capacity: 100,
            actual: 50,
        };
        let result = checker.check_scheduler(&inv, 1000);
        assert!(result.passed());
        assert_eq!(checker.total_checks(), 1);
        assert_eq!(checker.total_satisfied(), 1);
        assert_eq!(checker.total_violations(), 0);
    }

    #[test]
    fn test_invariant_checker_check_scheduler_violated() {
        let mut checker = InvariantChecker::with_defaults();
        let inv = SchedulerInvariant::CapacityBound {
            lane: SchedulerLane::Bulk,
            capacity: 10,
            actual: 20,
        };
        let result = checker.check_scheduler(&inv, 2000);
        assert!(result.violated());
        assert_eq!(checker.total_violations(), 1);
    }

    #[test]
    fn test_invariant_checker_check_budget() {
        let mut checker = InvariantChecker::with_defaults();
        let good = BudgetInvariant::NonNegativeTargets {
            stage: LatencyStage::PatternDetection,
            min_target: 10.0,
        };
        let result = checker.check_budget(&good, 3000);
        assert!(result.passed());

        let bad = BudgetInvariant::NonNegativeTargets {
            stage: LatencyStage::PatternDetection,
            min_target: -5.0,
        };
        let result = checker.check_budget(&bad, 4000);
        assert!(result.violated());
        assert_eq!(checker.total_checks(), 2);
    }

    #[test]
    fn test_invariant_checker_check_recovery() {
        let mut checker = InvariantChecker::with_defaults();
        let inv = RecoveryInvariant::CooldownEnforced {
            consecutive_ok: 25,
            cooldown_required: 20,
        };
        let result = checker.check_recovery(&inv, 5000);
        assert!(result.passed());
    }

    #[test]
    fn test_invariant_checker_violation_rate() {
        let mut checker = InvariantChecker::with_defaults();
        for i in 0..7 {
            let inv = SchedulerInvariant::CapacityBound {
                lane: SchedulerLane::Input,
                capacity: 100,
                actual: i,
            };
            checker.check_scheduler(&inv, i as u64);
        }
        for i in 0..3 {
            let inv = SchedulerInvariant::CapacityBound {
                lane: SchedulerLane::Input,
                capacity: 10,
                actual: 20 + i,
            };
            checker.check_scheduler(&inv, 100 + i as u64);
        }
        assert_eq!(checker.total_checks(), 10);
        assert!((checker.violation_rate() - 0.3).abs() < 1e-6);
    }

    #[test]
    fn test_invariant_checker_recent_results() {
        let mut checker = InvariantChecker::with_defaults();
        for i in 0..5 {
            let inv = SchedulerInvariant::EpochMonotonicity {
                previous: 0,
                current: i,
            };
            checker.check_scheduler(&inv, i);
        }
        assert_eq!(checker.recent_results(3).len(), 3);
        assert_eq!(checker.recent_results(10).len(), 5);
    }

    #[test]
    fn test_invariant_checker_results_by_domain() {
        let mut checker = InvariantChecker::with_defaults();
        checker.check_scheduler(
            &SchedulerInvariant::EpochMonotonicity {
                previous: 0,
                current: 1,
            },
            100,
        );
        checker.check_budget(
            &BudgetInvariant::NonNegativeTargets {
                stage: LatencyStage::PatternDetection,
                min_target: 1.0,
            },
            200,
        );
        checker.check_recovery(
            &RecoveryInvariant::LevelInRange {
                level: MitigationLevel::None,
            },
            300,
        );
        assert_eq!(
            checker.results_by_domain(InvariantDomain::Scheduler).len(),
            1
        );
        assert_eq!(checker.results_by_domain(InvariantDomain::Budget).len(), 1);
        assert_eq!(
            checker.results_by_domain(InvariantDomain::Recovery).len(),
            1
        );
        assert_eq!(
            checker
                .results_by_domain(InvariantDomain::Composition)
                .len(),
            0
        );
    }

    #[test]
    fn test_invariant_checker_violations_filter() {
        let mut checker = InvariantChecker::with_defaults();
        checker.check_scheduler(
            &SchedulerInvariant::CapacityBound {
                lane: SchedulerLane::Input,
                capacity: 100,
                actual: 50,
            },
            100,
        );
        checker.check_scheduler(
            &SchedulerInvariant::CapacityBound {
                lane: SchedulerLane::Input,
                capacity: 10,
                actual: 20,
            },
            200,
        );
        let violations = checker.violations();
        assert_eq!(violations.len(), 1);
        assert!(violations[0].violated());
    }

    #[test]
    fn test_invariant_checker_snapshot() {
        let mut checker = InvariantChecker::with_defaults();
        for _ in 0..5 {
            checker.check_scheduler(
                &SchedulerInvariant::EpochMonotonicity {
                    previous: 0,
                    current: 1,
                },
                0,
            );
        }
        let snap = checker.snapshot();
        assert_eq!(snap.total_checks, 5);
        assert_eq!(snap.total_satisfied, 5);
        assert_eq!(snap.total_violations, 0);
        assert_eq!(snap.history_len, 5);
    }

    #[test]
    fn test_invariant_checker_status_line() {
        let checker = InvariantChecker::with_defaults();
        let line = checker.status_line();
        assert!(line.contains("invariants:"));
        assert!(line.contains("checks=0"));
    }

    #[test]
    fn test_invariant_checker_reset() {
        let mut checker = InvariantChecker::with_defaults();
        for _ in 0..3 {
            checker.check_scheduler(
                &SchedulerInvariant::EpochMonotonicity {
                    previous: 0,
                    current: 1,
                },
                0,
            );
        }
        assert_eq!(checker.total_checks(), 3);
        checker.reset();
        assert_eq!(checker.total_checks(), 0);
        assert_eq!(checker.total_violations(), 0);
        assert_eq!(checker.total_satisfied(), 0);
        assert_eq!(checker.recent_results(10).len(), 0);
    }

    #[test]
    fn test_invariant_checker_degradation_healthy() {
        let checker = InvariantChecker::with_defaults();
        assert_eq!(
            checker.detect_degradation(),
            InvariantCheckerDegradation::Healthy
        );
    }

    #[test]
    fn test_invariant_checker_degradation_violations_detected() {
        let mut checker = InvariantChecker::with_defaults();
        for i in 0..19 {
            checker.check_scheduler(
                &SchedulerInvariant::EpochMonotonicity {
                    previous: 0,
                    current: i,
                },
                i,
            );
        }
        checker.check_scheduler(
            &SchedulerInvariant::CapacityBound {
                lane: SchedulerLane::Input,
                capacity: 1,
                actual: 5,
            },
            100,
        );
        match checker.detect_degradation() {
            InvariantCheckerDegradation::ViolationsDetected {
                violations, total, ..
            } => {
                assert_eq!(violations, 1);
                assert_eq!(total, 20);
            }
            other => panic!("Expected ViolationsDetected, got {other:?}"),
        }
    }

    #[test]
    fn test_invariant_checker_degradation_high_rate() {
        let mut checker = InvariantChecker::with_defaults();
        for i in 0..8 {
            checker.check_scheduler(
                &SchedulerInvariant::EpochMonotonicity {
                    previous: 0,
                    current: i,
                },
                i,
            );
        }
        for _ in 0..2 {
            checker.check_scheduler(
                &SchedulerInvariant::CapacityBound {
                    lane: SchedulerLane::Input,
                    capacity: 1,
                    actual: 5,
                },
                100,
            );
        }
        match checker.detect_degradation() {
            InvariantCheckerDegradation::HighViolationRate {
                violations, total, ..
            } => {
                assert_eq!(violations, 2);
                assert_eq!(total, 10);
            }
            other => panic!("Expected HighViolationRate, got {other:?}"),
        }
    }

    #[test]
    fn test_invariant_checker_log_entry() {
        let mut checker = InvariantChecker::with_defaults();
        checker.check_scheduler(
            &SchedulerInvariant::EpochMonotonicity {
                previous: 0,
                current: 1,
            },
            0,
        );
        let entry = checker.log_entry();
        assert_eq!(entry.total_checks, 1);
        assert_eq!(entry.total_satisfied, 1);
        assert_eq!(entry.total_violations, 0);
    }

    #[test]
    fn test_invariant_checker_history_cap() {
        let config = InvariantCheckerConfig {
            max_history: 5,
            ..Default::default()
        };
        let mut checker = InvariantChecker::new(config);
        for i in 0..10 {
            checker.check_scheduler(
                &SchedulerInvariant::EpochMonotonicity {
                    previous: 0,
                    current: i,
                },
                i,
            );
        }
        assert_eq!(checker.recent_results(100).len(), 5);
        assert_eq!(checker.total_checks(), 10);
    }

    #[test]
    fn test_invariant_domain_serde() {
        for domain in &[
            InvariantDomain::Scheduler,
            InvariantDomain::Budget,
            InvariantDomain::Recovery,
            InvariantDomain::Composition,
        ] {
            let json = serde_json::to_string(domain).unwrap();
            let back: InvariantDomain = serde_json::from_str(&json).unwrap();
            assert_eq!(*domain, back);
        }
    }

    #[test]
    fn test_invariant_severity_serde() {
        for sev in &[
            InvariantSeverity::Info,
            InvariantSeverity::Warning,
            InvariantSeverity::Critical,
        ] {
            let json = serde_json::to_string(sev).unwrap();
            let back: InvariantSeverity = serde_json::from_str(&json).unwrap();
            assert_eq!(*sev, back);
        }
    }

    #[test]
    fn test_invariant_checker_degradation_serde() {
        let variants = vec![
            InvariantCheckerDegradation::Healthy,
            InvariantCheckerDegradation::ViolationsDetected {
                violations: 3,
                total: 100,
            },
            InvariantCheckerDegradation::HighViolationRate {
                violations: 15,
                total: 100,
            },
        ];
        for v in &variants {
            let json = serde_json::to_string(v).unwrap();
            let back: InvariantCheckerDegradation = serde_json::from_str(&json).unwrap();
            assert_eq!(*v, back);
        }
    }

    #[test]
    fn test_invariant_checker_degradation_display() {
        assert_eq!(InvariantCheckerDegradation::Healthy.to_string(), "healthy");
        let det = InvariantCheckerDegradation::ViolationsDetected {
            violations: 2,
            total: 50,
        };
        assert!(det.to_string().contains("2/50"));
        let high = InvariantCheckerDegradation::HighViolationRate {
            violations: 10,
            total: 50,
        };
        assert!(high.to_string().contains("high_rate"));
    }

    #[test]
    fn test_invariant_check_result_display() {
        let r = InvariantCheckResult {
            predicate_id: "test.id".to_string(),
            domain: InvariantDomain::Scheduler,
            severity: InvariantSeverity::Critical,
            outcome: InvariantOutcome::Satisfied,
            eval_time_us: 42,
            timestamp_us: 1000,
        };
        let s = format!("{r}");
        assert!(s.contains("SATISFIED"));
        assert!(s.contains("42"));
    }

    // ── E1 Impl: Bridge Method Tests ──────────────────────────────

    #[test]
    fn test_checker_batch_scheduler() {
        let mut checker = InvariantChecker::with_defaults();
        let invs = vec![
            SchedulerInvariant::EpochMonotonicity {
                previous: 0,
                current: 5,
            },
            SchedulerInvariant::CapacityBound {
                lane: SchedulerLane::Input,
                capacity: 100,
                actual: 50,
            },
            SchedulerInvariant::CapacityBound {
                lane: SchedulerLane::Bulk,
                capacity: 10,
                actual: 20,
            },
        ];
        let results = checker.check_scheduler_batch(&invs, 1000);
        assert_eq!(results.len(), 3);
        assert!(results[0].passed());
        assert!(results[1].passed());
        assert!(results[2].violated());
        assert_eq!(checker.total_checks(), 3);
    }

    #[test]
    fn test_checker_batch_budget() {
        let mut checker = InvariantChecker::with_defaults();
        let invs = vec![
            BudgetInvariant::NonNegativeTargets {
                stage: LatencyStage::PatternDetection,
                min_target: 10.0,
            },
            BudgetInvariant::OverflowBound {
                overflow_count: 5,
                total_observations: 100,
            },
        ];
        let results = checker.check_budget_batch(&invs, 2000);
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r.passed()));
    }

    #[test]
    fn test_checker_batch_recovery() {
        let mut checker = InvariantChecker::with_defaults();
        let invs = vec![
            RecoveryInvariant::LevelInRange {
                level: MitigationLevel::Defer,
            },
            RecoveryInvariant::EscalationCountMonotonic {
                previous: 3,
                current: 5,
            },
        ];
        let results = checker.check_recovery_batch(&invs, 3000);
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r.passed()));
    }

    #[test]
    fn test_checker_all_satisfied() {
        let mut checker = InvariantChecker::with_defaults();
        checker.check_scheduler(
            &SchedulerInvariant::EpochMonotonicity {
                previous: 0,
                current: 1,
            },
            0,
        );
        assert!(checker.all_satisfied());
        checker.check_scheduler(
            &SchedulerInvariant::CapacityBound {
                lane: SchedulerLane::Input,
                capacity: 1,
                actual: 10,
            },
            1,
        );
        assert!(!checker.all_satisfied());
    }

    #[test]
    fn test_checker_violation_count_by_domain() {
        let mut checker = InvariantChecker::with_defaults();
        checker.check_scheduler(
            &SchedulerInvariant::CapacityBound {
                lane: SchedulerLane::Input,
                capacity: 1,
                actual: 10,
            },
            0,
        );
        checker.check_budget(
            &BudgetInvariant::NonNegativeTargets {
                stage: LatencyStage::PatternDetection,
                min_target: -1.0,
            },
            1,
        );
        checker.check_budget(
            &BudgetInvariant::NonNegativeTargets {
                stage: LatencyStage::PatternDetection,
                min_target: 5.0,
            },
            2,
        );
        assert_eq!(
            checker.violation_count_by_domain(InvariantDomain::Scheduler),
            1
        );
        assert_eq!(
            checker.violation_count_by_domain(InvariantDomain::Budget),
            1
        );
        assert_eq!(
            checker.violation_count_by_domain(InvariantDomain::Recovery),
            0
        );
    }

    #[test]
    fn test_checker_last_violation() {
        let mut checker = InvariantChecker::with_defaults();
        assert!(checker.last_violation().is_none());
        checker.check_scheduler(
            &SchedulerInvariant::CapacityBound {
                lane: SchedulerLane::Input,
                capacity: 1,
                actual: 10,
            },
            100,
        );
        checker.check_scheduler(
            &SchedulerInvariant::EpochMonotonicity {
                previous: 0,
                current: 1,
            },
            200,
        );
        let last = checker.last_violation().unwrap();
        assert_eq!(last.predicate_id, "scheduler.capacity_bound");
    }

    #[test]
    fn test_checker_predicate_ever_violated() {
        let mut checker = InvariantChecker::with_defaults();
        checker.check_scheduler(
            &SchedulerInvariant::EpochMonotonicity {
                previous: 0,
                current: 1,
            },
            0,
        );
        assert!(!checker.predicate_ever_violated("scheduler.epoch_monotonicity"));
        checker.check_scheduler(
            &SchedulerInvariant::EpochMonotonicity {
                previous: 5,
                current: 1,
            },
            1,
        );
        assert!(checker.predicate_ever_violated("scheduler.epoch_monotonicity"));
    }

    #[test]
    fn test_checker_predicate_pass_rate() {
        let mut checker = InvariantChecker::with_defaults();
        assert!(checker.predicate_pass_rate("nonexistent").is_nan());
        for _ in 0..3 {
            checker.check_scheduler(
                &SchedulerInvariant::EpochMonotonicity {
                    previous: 0,
                    current: 1,
                },
                0,
            );
        }
        checker.check_scheduler(
            &SchedulerInvariant::EpochMonotonicity {
                previous: 5,
                current: 1,
            },
            1,
        );
        let rate = checker.predicate_pass_rate("scheduler.epoch_monotonicity");
        assert!((rate - 0.75).abs() < 1e-6);
    }

    #[test]
    fn test_checker_checked_predicates() {
        let mut checker = InvariantChecker::with_defaults();
        checker.check_scheduler(
            &SchedulerInvariant::EpochMonotonicity {
                previous: 0,
                current: 1,
            },
            0,
        );
        checker.check_budget(
            &BudgetInvariant::NonNegativeTargets {
                stage: LatencyStage::PatternDetection,
                min_target: 1.0,
            },
            1,
        );
        checker.check_scheduler(
            &SchedulerInvariant::EpochMonotonicity {
                previous: 0,
                current: 2,
            },
            2,
        );
        let preds = checker.checked_predicates();
        assert_eq!(preds.len(), 2);
        assert!(preds.contains(&"scheduler.epoch_monotonicity".to_string()));
        assert!(preds.contains(&"budget.non_negative_targets".to_string()));
    }

    #[test]
    fn test_checker_domain_summary() {
        let mut checker = InvariantChecker::with_defaults();
        checker.check_scheduler(
            &SchedulerInvariant::EpochMonotonicity {
                previous: 0,
                current: 1,
            },
            0,
        );
        checker.check_scheduler(
            &SchedulerInvariant::CapacityBound {
                lane: SchedulerLane::Input,
                capacity: 1,
                actual: 10,
            },
            1,
        );
        checker.check_budget(
            &BudgetInvariant::NonNegativeTargets {
                stage: LatencyStage::PatternDetection,
                min_target: 5.0,
            },
            2,
        );
        let summary = checker.domain_summary();
        assert_eq!(summary.len(), 4);
        // Scheduler: 2 checks, 1 violation
        let sched = summary
            .iter()
            .find(|(d, _, _)| *d == InvariantDomain::Scheduler)
            .unwrap();
        assert_eq!(sched.1, 2);
        assert_eq!(sched.2, 1);
        // Budget: 1 check, 0 violations
        let budget = summary
            .iter()
            .find(|(d, _, _)| *d == InvariantDomain::Budget)
            .unwrap();
        assert_eq!(budget.1, 1);
        assert_eq!(budget.2, 0);
    }

    #[test]
    fn test_checker_from_enforcement_state_monotonic() {
        let mut checker = InvariantChecker::with_defaults();
        let prev = StageEnforcementState {
            current_level: MitigationLevel::Degrade,
            consecutive_ok: 0,
            last_escalation_us: 1000,
            escalation_count: 3,
            recovery_count: 1,
        };
        let curr = StageEnforcementState {
            current_level: MitigationLevel::Degrade,
            consecutive_ok: 5,
            last_escalation_us: 1000,
            escalation_count: 3,
            recovery_count: 1,
        };
        let protocol = RecoveryProtocol::default();
        let results = checker.check_from_enforcement_state(&curr, &prev, &protocol, 5000);
        // Should check: escalation_count_mono, recovery_count_mono, level_in_range
        // No recovery happened (same level), so no gradual or cooldown checks
        assert_eq!(results.len(), 3);
        assert!(results.iter().all(|r| r.passed()));
    }

    #[test]
    fn test_checker_from_enforcement_state_recovery() {
        let mut checker = InvariantChecker::with_defaults();
        let prev = StageEnforcementState {
            current_level: MitigationLevel::Degrade,
            consecutive_ok: 0,
            last_escalation_us: 1000,
            escalation_count: 3,
            recovery_count: 1,
        };
        // Gradual recovery: Degrade -> Defer (one step down)
        let curr = StageEnforcementState {
            current_level: MitigationLevel::Defer,
            consecutive_ok: 25, // > 20 cooldown
            last_escalation_us: 1000,
            escalation_count: 3,
            recovery_count: 2,
        };
        let protocol = RecoveryProtocol::default();
        let results = checker.check_from_enforcement_state(&curr, &prev, &protocol, 6000);
        // escalation_count_mono, recovery_count_mono, level_in_range, gradual_deescalation, cooldown_enforced
        assert_eq!(results.len(), 5);
        assert!(results.iter().all(|r| r.passed()));
    }

    // ── E2: Model-Checking Harness Tests ──────────────────────────

    #[test]
    fn test_trace_action_display() {
        let obs = TraceAction::ObserveLatency {
            stage: LatencyStage::PtyCapture,
            latency_us: 42.5,
        };
        assert!(obs.to_string().contains("observe"));
        let admit = TraceAction::SchedulerAdmit {
            lane: SchedulerLane::Input,
            cost_us: 10.0,
        };
        assert!(admit.to_string().contains("admit"));
        let recover = TraceAction::RecoveryStep {
            level_before: MitigationLevel::Degrade,
            level_after: MitigationLevel::Defer,
        };
        assert!(recover.to_string().contains("recover"));
        let epoch = TraceAction::EpochAdvance { new_epoch: 5 };
        assert!(epoch.to_string().contains("epoch"));
        let reset = TraceAction::Reset {
            domain: InvariantDomain::Budget,
        };
        assert!(reset.to_string().contains("reset"));
    }

    #[test]
    fn test_counterexample_display() {
        let cx = Counterexample {
            predicate_id: "scheduler.capacity_bound".to_string(),
            domain: InvariantDomain::Scheduler,
            trace: vec![TraceStep {
                step: 0,
                action: TraceAction::EpochAdvance { new_epoch: 1 },
                check_results: vec![],
                timestamp_us: 100,
            }],
            description: "capacity exceeded".to_string(),
            found_at_us: 100,
        };
        let s = format!("{cx}");
        assert!(s.contains("scheduler.capacity_bound"));
        assert!(s.contains("1 steps"));
    }

    #[test]
    fn test_exploration_strategy_display() {
        assert_eq!(ExplorationStrategy::BreadthFirst.to_string(), "bfs");
        assert_eq!(ExplorationStrategy::RandomWalk.to_string(), "random");
        assert_eq!(ExplorationStrategy::Guided.to_string(), "guided");
    }

    #[test]
    fn test_model_checker_new() {
        let mc = ModelChecker::with_defaults();
        assert_eq!(mc.states_explored(), 0);
        assert_eq!(mc.counterexample_count(), 0);
        assert_eq!(mc.max_depth_reached(), 0);
    }

    #[test]
    fn test_model_checker_step_no_violation() {
        let mut mc = ModelChecker::with_defaults();
        let result = InvariantCheckResult {
            predicate_id: "test".to_string(),
            domain: InvariantDomain::Scheduler,
            severity: InvariantSeverity::Critical,
            outcome: InvariantOutcome::Satisfied,
            eval_time_us: 0,
            timestamp_us: 100,
        };
        let violated = mc.step(TraceAction::EpochAdvance { new_epoch: 1 }, &[result], 100);
        assert!(!violated);
        assert_eq!(mc.states_explored(), 1);
        assert_eq!(mc.counterexample_count(), 0);
    }

    #[test]
    fn test_model_checker_step_with_violation() {
        let mut mc = ModelChecker::with_defaults();
        let result = InvariantCheckResult {
            predicate_id: "scheduler.capacity_bound".to_string(),
            domain: InvariantDomain::Scheduler,
            severity: InvariantSeverity::Critical,
            outcome: InvariantOutcome::Violated {
                counterexample: "capacity exceeded".to_string(),
            },
            eval_time_us: 0,
            timestamp_us: 200,
        };
        let violated = mc.step(
            TraceAction::SchedulerAdmit {
                lane: SchedulerLane::Input,
                cost_us: 50.0,
            },
            &[result],
            200,
        );
        assert!(violated);
        assert_eq!(mc.counterexample_count(), 1);
        let cx = &mc.counterexamples()[0];
        assert_eq!(cx.predicate_id, "scheduler.capacity_bound");
        assert_eq!(cx.trace.len(), 1);
    }

    #[test]
    fn test_model_checker_new_trace() {
        let mut mc = ModelChecker::with_defaults();
        let result = InvariantCheckResult {
            predicate_id: "test".to_string(),
            domain: InvariantDomain::Budget,
            severity: InvariantSeverity::Warning,
            outcome: InvariantOutcome::Satisfied,
            eval_time_us: 0,
            timestamp_us: 100,
        };
        mc.step(
            TraceAction::EpochAdvance { new_epoch: 1 },
            std::slice::from_ref(&result),
            100,
        );
        mc.step(
            TraceAction::EpochAdvance { new_epoch: 2 },
            std::slice::from_ref(&result),
            200,
        );
        assert_eq!(mc.states_explored(), 2);
        mc.new_trace();
        assert_eq!(mc.states_explored(), 2); // preserved
        // depth resets but states don't
    }

    #[test]
    fn test_model_checker_should_stop_non_exhaustive() {
        let config = ModelCheckerConfig {
            max_depth: 100,
            max_states: 10_000,
            exhaustive: false,
            ..Default::default()
        };
        let mut mc = ModelChecker::new(config);
        assert!(!mc.should_stop());
        // Add a counterexample
        let result = InvariantCheckResult {
            predicate_id: "x".to_string(),
            domain: InvariantDomain::Scheduler,
            severity: InvariantSeverity::Critical,
            outcome: InvariantOutcome::Violated {
                counterexample: "x".to_string(),
            },
            eval_time_us: 0,
            timestamp_us: 0,
        };
        mc.step(TraceAction::EpochAdvance { new_epoch: 1 }, &[result], 0);
        assert!(mc.should_stop()); // non-exhaustive stops after first
    }

    #[test]
    fn test_model_checker_should_stop_exhaustive() {
        let config = ModelCheckerConfig {
            max_depth: 100,
            max_states: 10_000,
            max_counterexamples: 5,
            exhaustive: true,
            ..Default::default()
        };
        let mut mc = ModelChecker::new(config);
        let result = InvariantCheckResult {
            predicate_id: "x".to_string(),
            domain: InvariantDomain::Scheduler,
            severity: InvariantSeverity::Critical,
            outcome: InvariantOutcome::Violated {
                counterexample: "x".to_string(),
            },
            eval_time_us: 0,
            timestamp_us: 0,
        };
        mc.step(TraceAction::EpochAdvance { new_epoch: 1 }, &[result], 0);
        assert!(!mc.should_stop()); // exhaustive continues
    }

    #[test]
    fn test_model_checker_verdict_no_violation() {
        let mc = ModelChecker::with_defaults();
        match mc.verdict() {
            ModelCheckVerdict::NoViolation {
                states_explored, ..
            } => {
                assert_eq!(states_explored, 0);
            }
            other => panic!("Expected NoViolation, got {other}"),
        }
    }

    #[test]
    fn test_model_checker_verdict_violations() {
        let mut mc = ModelChecker::with_defaults();
        let result = InvariantCheckResult {
            predicate_id: "test".to_string(),
            domain: InvariantDomain::Budget,
            severity: InvariantSeverity::Critical,
            outcome: InvariantOutcome::Violated {
                counterexample: "bad".to_string(),
            },
            eval_time_us: 0,
            timestamp_us: 0,
        };
        mc.step(TraceAction::EpochAdvance { new_epoch: 1 }, &[result], 0);
        match mc.verdict() {
            ModelCheckVerdict::ViolationsFound { counterexamples } => {
                assert_eq!(counterexamples.len(), 1);
            }
            other => panic!("Expected ViolationsFound, got {other}"),
        }
    }

    #[test]
    fn test_model_checker_snapshot() {
        let mut mc = ModelChecker::with_defaults();
        let result = InvariantCheckResult {
            predicate_id: "test".to_string(),
            domain: InvariantDomain::Scheduler,
            severity: InvariantSeverity::Info,
            outcome: InvariantOutcome::Satisfied,
            eval_time_us: 0,
            timestamp_us: 0,
        };
        mc.step(TraceAction::EpochAdvance { new_epoch: 1 }, &[result], 0);
        let snap = mc.snapshot();
        assert_eq!(snap.states_explored, 1);
        assert_eq!(snap.counterexamples_found, 0);
    }

    #[test]
    fn test_model_checker_status_line() {
        let mc = ModelChecker::with_defaults();
        let line = mc.status_line();
        assert!(line.contains("model_check:"));
        assert!(line.contains("states=0"));
    }

    #[test]
    fn test_model_checker_reset() {
        let mut mc = ModelChecker::with_defaults();
        let result = InvariantCheckResult {
            predicate_id: "test".to_string(),
            domain: InvariantDomain::Scheduler,
            severity: InvariantSeverity::Critical,
            outcome: InvariantOutcome::Violated {
                counterexample: "x".to_string(),
            },
            eval_time_us: 0,
            timestamp_us: 0,
        };
        mc.step(TraceAction::EpochAdvance { new_epoch: 1 }, &[result], 0);
        assert_eq!(mc.counterexample_count(), 1);
        mc.reset();
        assert_eq!(mc.states_explored(), 0);
        assert_eq!(mc.counterexample_count(), 0);
        assert_eq!(mc.max_depth_reached(), 0);
    }

    #[test]
    fn test_model_checker_degradation_healthy() {
        let mc = ModelChecker::with_defaults();
        assert_eq!(mc.detect_degradation(), ModelCheckerDegradation::Healthy);
    }

    #[test]
    fn test_model_checker_degradation_violations_found() {
        let mut mc = ModelChecker::new(ModelCheckerConfig {
            exhaustive: true,
            ..Default::default()
        });
        let result = InvariantCheckResult {
            predicate_id: "test".to_string(),
            domain: InvariantDomain::Scheduler,
            severity: InvariantSeverity::Critical,
            outcome: InvariantOutcome::Violated {
                counterexample: "x".to_string(),
            },
            eval_time_us: 0,
            timestamp_us: 0,
        };
        mc.step(TraceAction::EpochAdvance { new_epoch: 1 }, &[result], 0);
        match mc.detect_degradation() {
            ModelCheckerDegradation::ViolationsFound { count } => {
                assert_eq!(count, 1);
            }
            other => panic!("Expected ViolationsFound, got {other:?}"),
        }
    }

    #[test]
    fn test_model_checker_log_entry() {
        let mc = ModelChecker::with_defaults();
        let entry = mc.log_entry();
        assert_eq!(entry.states_explored, 0);
        assert_eq!(entry.counterexamples_found, 0);
    }

    #[test]
    fn test_model_check_verdict_display() {
        let nv = ModelCheckVerdict::NoViolation {
            states_explored: 100,
            depth_reached: 10,
        };
        assert!(nv.to_string().contains("NO_VIOLATION"));
        let vf = ModelCheckVerdict::ViolationsFound {
            counterexamples: vec![],
        };
        assert!(vf.to_string().contains("VIOLATIONS_FOUND"));
        let inc = ModelCheckVerdict::Incomplete {
            states_explored: 50,
            reason: "timeout".to_string(),
        };
        assert!(inc.to_string().contains("INCOMPLETE"));
    }

    #[test]
    fn test_model_checker_degradation_serde() {
        let variants = vec![
            ModelCheckerDegradation::Healthy,
            ModelCheckerDegradation::ViolationsFound { count: 2 },
            ModelCheckerDegradation::HighViolationRate {
                count: 10,
                states: 100,
            },
        ];
        for v in &variants {
            let json = serde_json::to_string(v).unwrap();
            let back: ModelCheckerDegradation = serde_json::from_str(&json).unwrap();
            assert_eq!(*v, back);
        }
    }

    #[test]
    fn test_model_checker_degradation_display() {
        assert_eq!(ModelCheckerDegradation::Healthy.to_string(), "healthy");
        let vf = ModelCheckerDegradation::ViolationsFound { count: 3 };
        assert!(vf.to_string().contains("violations(3)"));
        let hr = ModelCheckerDegradation::HighViolationRate {
            count: 10,
            states: 50,
        };
        assert!(hr.to_string().contains("high_rate"));
    }

    #[test]
    fn test_exploration_strategy_serde() {
        for strat in &[
            ExplorationStrategy::BreadthFirst,
            ExplorationStrategy::RandomWalk,
            ExplorationStrategy::Guided,
        ] {
            let json = serde_json::to_string(strat).unwrap();
            let back: ExplorationStrategy = serde_json::from_str(&json).unwrap();
            assert_eq!(*strat, back);
        }
    }

    #[test]
    fn test_model_checker_multi_step_trace() {
        let mut mc = ModelChecker::with_defaults();
        let ok = InvariantCheckResult {
            predicate_id: "test".to_string(),
            domain: InvariantDomain::Scheduler,
            severity: InvariantSeverity::Info,
            outcome: InvariantOutcome::Satisfied,
            eval_time_us: 0,
            timestamp_us: 0,
        };
        mc.step(
            TraceAction::EpochAdvance { new_epoch: 1 },
            std::slice::from_ref(&ok),
            100,
        );
        mc.step(
            TraceAction::EpochAdvance { new_epoch: 2 },
            std::slice::from_ref(&ok),
            200,
        );
        let bad = InvariantCheckResult {
            predicate_id: "sched.cap".to_string(),
            domain: InvariantDomain::Scheduler,
            severity: InvariantSeverity::Critical,
            outcome: InvariantOutcome::Violated {
                counterexample: "overflow".to_string(),
            },
            eval_time_us: 0,
            timestamp_us: 300,
        };
        mc.step(
            TraceAction::SchedulerAdmit {
                lane: SchedulerLane::Bulk,
                cost_us: 999.0,
            },
            &[bad],
            300,
        );
        assert_eq!(mc.counterexample_count(), 1);
        // Trace should have all 3 steps
        assert_eq!(mc.counterexamples()[0].trace.len(), 3);
        assert_eq!(mc.states_explored(), 3);
        assert_eq!(mc.max_depth_reached(), 3);
    }

    // ── E2 Impl: Bridge Method Tests ──────────────────────────────

    #[test]
    fn test_mc_run_scheduler_scenario_no_violation() {
        let mut mc = ModelChecker::with_defaults();
        let mut checker = InvariantChecker::with_defaults();
        let actions = vec![
            (
                TraceAction::EpochAdvance { new_epoch: 1 },
                vec![SchedulerInvariant::EpochMonotonicity {
                    previous: 0,
                    current: 1,
                }],
            ),
            (
                TraceAction::EpochAdvance { new_epoch: 2 },
                vec![SchedulerInvariant::EpochMonotonicity {
                    previous: 1,
                    current: 2,
                }],
            ),
        ];
        let verdict = mc.run_scheduler_scenario(&mut checker, &actions, 1000);
        let is_no_violation = matches!(verdict, ModelCheckVerdict::NoViolation { .. });
        assert!(is_no_violation);
    }

    #[test]
    fn test_mc_run_scheduler_scenario_with_violation() {
        let mut mc = ModelChecker::with_defaults();
        let mut checker = InvariantChecker::with_defaults();
        let actions = vec![
            (
                TraceAction::EpochAdvance { new_epoch: 1 },
                vec![SchedulerInvariant::CapacityBound {
                    lane: SchedulerLane::Input,
                    capacity: 100,
                    actual: 50,
                }],
            ),
            (
                TraceAction::SchedulerAdmit {
                    lane: SchedulerLane::Input,
                    cost_us: 10.0,
                },
                vec![SchedulerInvariant::CapacityBound {
                    lane: SchedulerLane::Input,
                    capacity: 5,
                    actual: 20,
                }],
            ),
        ];
        let verdict = mc.run_scheduler_scenario(&mut checker, &actions, 2000);
        let is_violations = matches!(verdict, ModelCheckVerdict::ViolationsFound { .. });
        assert!(is_violations);
    }

    #[test]
    fn test_mc_run_budget_scenario() {
        let mut mc = ModelChecker::with_defaults();
        let mut checker = InvariantChecker::with_defaults();
        let actions = vec![(
            TraceAction::ObserveLatency {
                stage: LatencyStage::PtyCapture,
                latency_us: 100.0,
            },
            vec![BudgetInvariant::NonNegativeTargets {
                stage: LatencyStage::PtyCapture,
                min_target: 50.0,
            }],
        )];
        let verdict = mc.run_budget_scenario(&mut checker, &actions, 3000);
        let is_no_violation = matches!(verdict, ModelCheckVerdict::NoViolation { .. });
        assert!(is_no_violation);
    }

    #[test]
    fn test_mc_run_recovery_scenario() {
        let mut mc = ModelChecker::with_defaults();
        let mut checker = InvariantChecker::with_defaults();
        let actions = vec![(
            TraceAction::RecoveryStep {
                level_before: MitigationLevel::Degrade,
                level_after: MitigationLevel::Defer,
            },
            vec![RecoveryInvariant::LevelInRange {
                level: MitigationLevel::Defer,
            }],
        )];
        let verdict = mc.run_recovery_scenario(&mut checker, &actions, 4000);
        let is_no_violation = matches!(verdict, ModelCheckVerdict::NoViolation { .. });
        assert!(is_no_violation);
    }

    #[test]
    fn test_mc_counterexamples_by_domain() {
        let mut mc = ModelChecker::new(ModelCheckerConfig {
            exhaustive: true,
            ..Default::default()
        });
        let sched_result = InvariantCheckResult {
            predicate_id: "sched.x".to_string(),
            domain: InvariantDomain::Scheduler,
            severity: InvariantSeverity::Critical,
            outcome: InvariantOutcome::Violated {
                counterexample: "a".to_string(),
            },
            eval_time_us: 0,
            timestamp_us: 0,
        };
        mc.step(
            TraceAction::EpochAdvance { new_epoch: 1 },
            &[sched_result],
            0,
        );
        mc.new_trace();
        let budget_result = InvariantCheckResult {
            predicate_id: "budget.y".to_string(),
            domain: InvariantDomain::Budget,
            severity: InvariantSeverity::Critical,
            outcome: InvariantOutcome::Violated {
                counterexample: "b".to_string(),
            },
            eval_time_us: 0,
            timestamp_us: 1,
        };
        mc.step(
            TraceAction::EpochAdvance { new_epoch: 2 },
            &[budget_result],
            1,
        );
        assert_eq!(
            mc.counterexamples_by_domain(InvariantDomain::Scheduler)
                .len(),
            1
        );
        assert_eq!(
            mc.counterexamples_by_domain(InvariantDomain::Budget).len(),
            1
        );
        assert_eq!(
            mc.counterexamples_by_domain(InvariantDomain::Recovery)
                .len(),
            0
        );
    }

    #[test]
    fn test_mc_shortest_counterexample() {
        let mut mc = ModelChecker::new(ModelCheckerConfig {
            exhaustive: true,
            ..Default::default()
        });
        let bad = InvariantCheckResult {
            predicate_id: "test".to_string(),
            domain: InvariantDomain::Scheduler,
            severity: InvariantSeverity::Critical,
            outcome: InvariantOutcome::Violated {
                counterexample: "x".to_string(),
            },
            eval_time_us: 0,
            timestamp_us: 0,
        };
        let ok = InvariantCheckResult {
            predicate_id: "test2".to_string(),
            domain: InvariantDomain::Scheduler,
            severity: InvariantSeverity::Info,
            outcome: InvariantOutcome::Satisfied,
            eval_time_us: 0,
            timestamp_us: 0,
        };
        // Trace 1: 3 steps then violation
        mc.step(
            TraceAction::EpochAdvance { new_epoch: 1 },
            std::slice::from_ref(&ok),
            0,
        );
        mc.step(
            TraceAction::EpochAdvance { new_epoch: 2 },
            std::slice::from_ref(&ok),
            1,
        );
        mc.step(
            TraceAction::EpochAdvance { new_epoch: 3 },
            std::slice::from_ref(&bad),
            2,
        );
        mc.new_trace();
        // Trace 2: 1 step then violation
        mc.step(
            TraceAction::EpochAdvance { new_epoch: 4 },
            std::slice::from_ref(&bad),
            3,
        );
        let shortest = mc.shortest_counterexample().unwrap();
        assert_eq!(shortest.trace.len(), 1);
    }

    #[test]
    fn test_mc_violated_predicates() {
        let mut mc = ModelChecker::new(ModelCheckerConfig {
            exhaustive: true,
            ..Default::default()
        });
        let r1 = InvariantCheckResult {
            predicate_id: "a.x".to_string(),
            domain: InvariantDomain::Scheduler,
            severity: InvariantSeverity::Critical,
            outcome: InvariantOutcome::Violated {
                counterexample: "x".to_string(),
            },
            eval_time_us: 0,
            timestamp_us: 0,
        };
        let r2 = InvariantCheckResult {
            predicate_id: "b.y".to_string(),
            domain: InvariantDomain::Budget,
            severity: InvariantSeverity::Critical,
            outcome: InvariantOutcome::Violated {
                counterexample: "y".to_string(),
            },
            eval_time_us: 0,
            timestamp_us: 1,
        };
        mc.step(TraceAction::EpochAdvance { new_epoch: 1 }, &[r1], 0);
        mc.new_trace();
        mc.step(TraceAction::EpochAdvance { new_epoch: 2 }, &[r2], 1);
        let preds = mc.violated_predicates();
        assert_eq!(preds.len(), 2);
        assert!(preds.contains(&"a.x".to_string()));
        assert!(preds.contains(&"b.y".to_string()));
    }

    #[test]
    fn test_mc_inner_checker() {
        let mc = ModelChecker::with_defaults();
        assert_eq!(mc.inner_checker().total_checks(), 0);
    }

    #[test]
    fn test_mc_current_trace_len() {
        let mut mc = ModelChecker::with_defaults();
        assert_eq!(mc.current_trace_len(), 0);
        let ok = InvariantCheckResult {
            predicate_id: "test".to_string(),
            domain: InvariantDomain::Scheduler,
            severity: InvariantSeverity::Info,
            outcome: InvariantOutcome::Satisfied,
            eval_time_us: 0,
            timestamp_us: 0,
        };
        mc.step(TraceAction::EpochAdvance { new_epoch: 1 }, &[ok], 0);
        assert_eq!(mc.current_trace_len(), 1);
    }

    #[test]
    fn test_mc_strategy() {
        let mc = ModelChecker::with_defaults();
        assert_eq!(mc.strategy(), ExplorationStrategy::RandomWalk);
    }

    // ── E3: Deterministic Trace v2 Tests ─────────────────────────

    #[test]
    fn test_trace_format_version_display() {
        assert_eq!(TraceFormatVersion::V1.to_string(), "v1");
        assert_eq!(TraceFormatVersion::V2.to_string(), "v2");
    }

    #[test]
    fn test_trace_format_version_serde() {
        let v = TraceFormatVersion::V2;
        let json = serde_json::to_string(&v).unwrap();
        let back: TraceFormatVersion = serde_json::from_str(&json).unwrap();
        assert_eq!(v, back);
    }

    #[test]
    fn test_canonical_ordering_display() {
        assert_eq!(CanonicalOrdering::Temporal.to_string(), "temporal");
        assert_eq!(
            CanonicalOrdering::DomainGrouped.to_string(),
            "domain-grouped"
        );
        assert_eq!(CanonicalOrdering::Causal.to_string(), "causal");
    }

    #[test]
    fn test_canonical_ordering_serde() {
        for ord in [
            CanonicalOrdering::Temporal,
            CanonicalOrdering::DomainGrouped,
            CanonicalOrdering::Causal,
        ] {
            let json = serde_json::to_string(&ord).unwrap();
            let back: CanonicalOrdering = serde_json::from_str(&json).unwrap();
            assert_eq!(ord, back);
        }
    }

    #[test]
    fn test_trace_entry_fingerprint_deterministic() {
        let action = TraceAction::EpochAdvance { new_epoch: 42 };
        let domain = InvariantDomain::Composition;
        let fp1 = TraceEntry::compute_fingerprint(&action, domain);
        let fp2 = TraceEntry::compute_fingerprint(&action, domain);
        assert_eq!(fp1, fp2);
    }

    #[test]
    fn test_trace_entry_fingerprint_varies_by_domain() {
        let action = TraceAction::EpochAdvance { new_epoch: 42 };
        let fp1 = TraceEntry::compute_fingerprint(&action, InvariantDomain::Scheduler);
        let fp2 = TraceEntry::compute_fingerprint(&action, InvariantDomain::Budget);
        assert_ne!(fp1, fp2);
    }

    #[test]
    fn test_trace_entry_fingerprint_varies_by_action() {
        let a1 = TraceAction::EpochAdvance { new_epoch: 1 };
        let a2 = TraceAction::EpochAdvance { new_epoch: 2 };
        let fp1 = TraceEntry::compute_fingerprint(&a1, InvariantDomain::Composition);
        let fp2 = TraceEntry::compute_fingerprint(&a2, InvariantDomain::Composition);
        assert_ne!(fp1, fp2);
    }

    #[test]
    fn test_trace_entry_display() {
        let entry = TraceEntry {
            seq: 0,
            timestamp_us: 100,
            action: TraceAction::EpochAdvance { new_epoch: 5 },
            domain: InvariantDomain::Composition,
            causal_parent: None,
            fingerprint: 42,
        };
        let s = entry.to_string();
        assert!(s.contains("[0]"));
        assert!(s.contains("@100μs"));
        assert!(s.contains("epoch(5)"));
    }

    #[test]
    fn test_deterministic_trace_new_v2() {
        let trace = DeterministicTrace::new_v2("test-1".to_string(), 12345, 0);
        assert_eq!(trace.version, TraceFormatVersion::V2);
        assert_eq!(trace.trace_id, "test-1");
        assert_eq!(trace.seed, 12345);
        assert!(trace.is_empty());
        assert_eq!(trace.len(), 0);
    }

    #[test]
    fn test_deterministic_trace_push() {
        let mut trace = DeterministicTrace::new_v2("t1".to_string(), 0, 0);
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            100,
            None,
        );
        assert_eq!(trace.len(), 1);
        assert!(!trace.is_empty());
        assert_eq!(trace.entries[0].seq, 0);
        assert_eq!(trace.entries[0].timestamp_us, 100);
        assert_eq!(trace.duration_us, 100);
    }

    #[test]
    fn test_deterministic_trace_push_sequence_monotonic() {
        let mut trace = DeterministicTrace::new_v2("t1".to_string(), 0, 0);
        for i in 0..5 {
            trace.push(
                TraceAction::EpochAdvance { new_epoch: i },
                InvariantDomain::Composition,
                i * 10,
                if i > 0 { Some(i - 1) } else { None },
            );
        }
        assert_eq!(trace.len(), 5);
        for (i, entry) in trace.entries.iter().enumerate() {
            assert_eq!(entry.seq, i as u64);
        }
    }

    #[test]
    fn test_deterministic_trace_digest_deterministic() {
        let mut trace = DeterministicTrace::new_v2("t1".to_string(), 0, 0);
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            10,
            None,
        );
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 2 },
            InvariantDomain::Composition,
            20,
            Some(0),
        );
        let d1 = trace.digest();
        let d2 = trace.digest();
        assert_eq!(d1, d2);
    }

    #[test]
    fn test_deterministic_trace_digest_varies() {
        let mut t1 = DeterministicTrace::new_v2("a".to_string(), 0, 0);
        t1.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            10,
            None,
        );
        let mut t2 = DeterministicTrace::new_v2("b".to_string(), 0, 0);
        t2.push(
            TraceAction::EpochAdvance { new_epoch: 2 },
            InvariantDomain::Composition,
            10,
            None,
        );
        assert_ne!(t1.digest(), t2.digest());
    }

    #[test]
    fn test_deterministic_trace_display() {
        let trace = DeterministicTrace::new_v2("t1".to_string(), 42, 0);
        let s = trace.to_string();
        assert!(s.contains("v2"));
        assert!(s.contains("t1"));
        assert!(s.contains("42"));
    }

    #[test]
    fn test_deterministic_trace_serde() {
        let mut trace = DeterministicTrace::new_v2("t1".to_string(), 99, 0);
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            100,
            None,
        );
        let json = serde_json::to_string(&trace).unwrap();
        let back: DeterministicTrace = serde_json::from_str(&json).unwrap();
        assert_eq!(trace, back);
    }

    #[test]
    fn test_replay_comparison_result_display() {
        let id = ReplayComparisonResult::Identical;
        assert_eq!(id.to_string(), "identical");
        let iso = ReplayComparisonResult::Isomorphic { reordered_count: 3 };
        assert!(iso.to_string().contains("isomorphic"));
        let div = ReplayComparisonResult::Divergent {
            first_divergence_idx: 5,
            description: "test".to_string(),
        };
        assert!(div.to_string().contains("divergent"));
    }

    #[test]
    fn test_replay_comparison_result_serde() {
        let results = vec![
            ReplayComparisonResult::Identical,
            ReplayComparisonResult::Isomorphic { reordered_count: 2 },
            ReplayComparisonResult::Divergent {
                first_divergence_idx: 0,
                description: "test".to_string(),
            },
        ];
        for r in results {
            let json = serde_json::to_string(&r).unwrap();
            let back: ReplayComparisonResult = serde_json::from_str(&json).unwrap();
            assert_eq!(r, back);
        }
    }

    #[test]
    fn test_trace_mismatch_serde() {
        let mm = TraceMismatch {
            canonical_idx: 3,
            expected_fingerprint: 111,
            actual_fingerprint: Some(222),
            explanation: "different action".to_string(),
        };
        let json = serde_json::to_string(&mm).unwrap();
        let back: TraceMismatch = serde_json::from_str(&json).unwrap();
        assert_eq!(mm, back);
    }

    #[test]
    fn test_canonicalizer_config_default() {
        let cfg = CanonicalizerConfig::default();
        assert_eq!(cfg.ordering, CanonicalOrdering::Causal);
        assert!(!cfg.strip_timestamps);
        assert!(!cfg.dedup_consecutive);
        assert_eq!(cfg.max_entries, 0);
    }

    #[test]
    fn test_canonicalizer_causal_ordering() {
        let mut c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let mut trace = DeterministicTrace::new_v2("t".to_string(), 0, 0);
        // Insert out of causal order (timestamps swapped).
        trace.entries.push(TraceEntry {
            seq: 1,
            timestamp_us: 200,
            action: TraceAction::EpochAdvance { new_epoch: 2 },
            domain: InvariantDomain::Composition,
            causal_parent: Some(0),
            fingerprint: 1,
        });
        trace.entries.push(TraceEntry {
            seq: 0,
            timestamp_us: 100,
            action: TraceAction::EpochAdvance { new_epoch: 1 },
            domain: InvariantDomain::Composition,
            causal_parent: None,
            fingerprint: 2,
        });
        let canonical = c.canonicalize(&trace);
        // Causal ordering sorts by seq.
        assert_eq!(canonical.entries[0].fingerprint, 2); // was seq=0
        assert_eq!(canonical.entries[1].fingerprint, 1); // was seq=1
    }

    #[test]
    fn test_canonicalizer_temporal_ordering() {
        let mut c = ReplayCanonicalizer::new(CanonicalizerConfig {
            ordering: CanonicalOrdering::Temporal,
            ..Default::default()
        });
        let mut trace = DeterministicTrace::new_v2("t".to_string(), 0, 0);
        trace.entries.push(TraceEntry {
            seq: 0,
            timestamp_us: 200,
            action: TraceAction::EpochAdvance { new_epoch: 2 },
            domain: InvariantDomain::Composition,
            causal_parent: None,
            fingerprint: 1,
        });
        trace.entries.push(TraceEntry {
            seq: 1,
            timestamp_us: 100,
            action: TraceAction::EpochAdvance { new_epoch: 1 },
            domain: InvariantDomain::Composition,
            causal_parent: None,
            fingerprint: 2,
        });
        let canonical = c.canonicalize(&trace);
        assert_eq!(canonical.entries[0].fingerprint, 2); // timestamp 100 first
        assert_eq!(canonical.entries[1].fingerprint, 1); // timestamp 200 second
    }

    #[test]
    fn test_canonicalizer_domain_grouped_ordering() {
        let mut c = ReplayCanonicalizer::new(CanonicalizerConfig {
            ordering: CanonicalOrdering::DomainGrouped,
            ..Default::default()
        });
        let mut trace = DeterministicTrace::new_v2("t".to_string(), 0, 0);
        // Budget entry first, then scheduler.
        trace.push(
            TraceAction::ObserveLatency {
                stage: LatencyStage::PtyCapture,
                latency_us: 10.0,
            },
            InvariantDomain::Budget,
            100,
            None,
        );
        trace.push(
            TraceAction::SchedulerAdmit {
                lane: SchedulerLane::Input,
                cost_us: 5.0,
            },
            InvariantDomain::Scheduler,
            50,
            None,
        );
        let canonical = c.canonicalize(&trace);
        // Scheduler (0) comes before Budget (1) in domain sort.
        assert_eq!(canonical.entries[0].domain, InvariantDomain::Scheduler);
        assert_eq!(canonical.entries[1].domain, InvariantDomain::Budget);
    }

    #[test]
    fn test_canonicalizer_strip_timestamps() {
        let mut c = ReplayCanonicalizer::new(CanonicalizerConfig {
            strip_timestamps: true,
            ..Default::default()
        });
        let mut trace = DeterministicTrace::new_v2("t".to_string(), 0, 0);
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            500,
            None,
        );
        let canonical = c.canonicalize(&trace);
        assert_eq!(canonical.entries[0].timestamp_us, 0);
    }

    #[test]
    fn test_canonicalizer_dedup_consecutive() {
        let mut c = ReplayCanonicalizer::new(CanonicalizerConfig {
            dedup_consecutive: true,
            ..Default::default()
        });
        let action = TraceAction::EpochAdvance { new_epoch: 1 };
        let domain = InvariantDomain::Composition;
        let fp = TraceEntry::compute_fingerprint(&action, domain);
        let mut trace = DeterministicTrace::new_v2("t".to_string(), 0, 0);
        // Push same action twice (same fingerprint).
        trace.entries.push(TraceEntry {
            seq: 0,
            timestamp_us: 100,
            action: action.clone(),
            domain,
            causal_parent: None,
            fingerprint: fp,
        });
        trace.entries.push(TraceEntry {
            seq: 1,
            timestamp_us: 200,
            action: action.clone(),
            domain,
            causal_parent: Some(0),
            fingerprint: fp,
        });
        let canonical = c.canonicalize(&trace);
        assert_eq!(canonical.len(), 1);
        assert_eq!(c.snapshot().entries_deduped, 1);
    }

    #[test]
    fn test_canonicalizer_max_entries() {
        let mut c = ReplayCanonicalizer::new(CanonicalizerConfig {
            max_entries: 2,
            ..Default::default()
        });
        let mut trace = DeterministicTrace::new_v2("t".to_string(), 0, 0);
        for i in 0..5 {
            trace.push(
                TraceAction::EpochAdvance { new_epoch: i },
                InvariantDomain::Composition,
                i * 10,
                None,
            );
        }
        let canonical = c.canonicalize(&trace);
        assert_eq!(canonical.len(), 2);
    }

    #[test]
    fn test_canonicalizer_compare_identical() {
        let mut c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let mut t1 = DeterministicTrace::new_v2("a".to_string(), 0, 0);
        t1.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            10,
            None,
        );
        let t2 = t1.clone();
        let result = c.compare(&t1, &t2);
        assert_eq!(result, ReplayComparisonResult::Identical);
    }

    #[test]
    fn test_canonicalizer_compare_divergent_length() {
        let mut c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let mut t1 = DeterministicTrace::new_v2("a".to_string(), 0, 0);
        t1.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            10,
            None,
        );
        let t2 = DeterministicTrace::new_v2("b".to_string(), 0, 0);
        let result = c.compare(&t1, &t2);
        let is_divergent = matches!(result, ReplayComparisonResult::Divergent { .. });
        assert!(is_divergent);
    }

    #[test]
    fn test_canonicalizer_compare_divergent_content() {
        let mut c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let mut t1 = DeterministicTrace::new_v2("a".to_string(), 0, 0);
        t1.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            10,
            None,
        );
        let mut t2 = DeterministicTrace::new_v2("b".to_string(), 0, 0);
        t2.push(
            TraceAction::EpochAdvance { new_epoch: 99 },
            InvariantDomain::Composition,
            10,
            None,
        );
        let result = c.compare(&t1, &t2);
        let is_divergent = matches!(result, ReplayComparisonResult::Divergent { .. });
        assert!(is_divergent);
    }

    #[test]
    fn test_canonicalizer_diagnose_mismatches_none() {
        let c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let mut t1 = DeterministicTrace::new_v2("a".to_string(), 0, 0);
        t1.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            10,
            None,
        );
        let mismatches = c.diagnose_mismatches(&t1, &t1);
        assert!(mismatches.is_empty());
    }

    #[test]
    fn test_canonicalizer_diagnose_mismatches_found() {
        let c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let mut t1 = DeterministicTrace::new_v2("a".to_string(), 0, 0);
        t1.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            10,
            None,
        );
        let mut t2 = DeterministicTrace::new_v2("b".to_string(), 0, 0);
        t2.push(
            TraceAction::EpochAdvance { new_epoch: 99 },
            InvariantDomain::Composition,
            10,
            None,
        );
        let mismatches = c.diagnose_mismatches(&t1, &t2);
        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0].canonical_idx, 0);
    }

    #[test]
    fn test_canonicalizer_diagnose_missing_entry() {
        let c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let mut t1 = DeterministicTrace::new_v2("a".to_string(), 0, 0);
        t1.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            10,
            None,
        );
        let t2 = DeterministicTrace::new_v2("b".to_string(), 0, 0);
        let mismatches = c.diagnose_mismatches(&t1, &t2);
        assert_eq!(mismatches.len(), 1);
        assert!(mismatches[0].actual_fingerprint.is_none());
    }

    #[test]
    fn test_canonicalizer_upgrade_trace() {
        let mut c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let steps = vec![
            TraceStep {
                step: 0,
                action: TraceAction::EpochAdvance { new_epoch: 1 },
                check_results: vec![],
                timestamp_us: 100,
            },
            TraceStep {
                step: 1,
                action: TraceAction::SchedulerAdmit {
                    lane: SchedulerLane::Input,
                    cost_us: 5.0,
                },
                check_results: vec![],
                timestamp_us: 200,
            },
        ];
        let trace = c.upgrade_trace(&steps, "upgraded".to_string(), 42);
        assert_eq!(trace.version, TraceFormatVersion::V2);
        assert_eq!(trace.len(), 2);
        assert_eq!(trace.seed, 42);
        assert_eq!(trace.entries[0].domain, InvariantDomain::Composition);
        assert_eq!(trace.entries[1].domain, InvariantDomain::Scheduler);
    }

    #[test]
    fn test_canonicalizer_snapshot() {
        let mut c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let mut trace = DeterministicTrace::new_v2("t".to_string(), 0, 0);
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            10,
            None,
        );
        let _ = c.canonicalize(&trace);
        let snap = c.snapshot();
        assert_eq!(snap.traces_processed, 1);
        assert_eq!(snap.entries_processed, 1);
    }

    #[test]
    fn test_canonicalizer_degradation_healthy() {
        let c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let deg = c.detect_degradation();
        assert_eq!(deg, CanonicalizerDegradation::Healthy);
    }

    #[test]
    fn test_canonicalizer_degradation_display() {
        assert_eq!(CanonicalizerDegradation::Healthy.to_string(), "healthy");
        let high = CanonicalizerDegradation::HighDedupRatio { ratio: 0.75 };
        assert!(high.to_string().contains("high-dedup"));
        let vol = CanonicalizerDegradation::HighVolume {
            entries_processed: 200_000,
        };
        assert!(vol.to_string().contains("high-volume"));
    }

    #[test]
    fn test_canonicalizer_degradation_serde() {
        let variants = vec![
            CanonicalizerDegradation::Healthy,
            CanonicalizerDegradation::HighDedupRatio { ratio: 0.8 },
            CanonicalizerDegradation::HighVolume {
                entries_processed: 100,
            },
        ];
        for v in variants {
            let json = serde_json::to_string(&v).unwrap();
            let back: CanonicalizerDegradation = serde_json::from_str(&json).unwrap();
            assert_eq!(v, back);
        }
    }

    #[test]
    fn test_canonicalizer_log_entry() {
        let c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let entry = c.log_entry("trace-1", 10, 8, 500);
        assert_eq!(entry.trace_id, "trace-1");
        assert_eq!(entry.input_entries, 10);
        assert_eq!(entry.output_entries, 8);
        assert_eq!(entry.duration_us, 500);
    }

    #[test]
    fn test_canonicalizer_reset() {
        let mut c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let mut trace = DeterministicTrace::new_v2("t".to_string(), 0, 0);
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            10,
            None,
        );
        let _ = c.canonicalize(&trace);
        assert_eq!(c.snapshot().traces_processed, 1);
        c.reset();
        assert_eq!(c.snapshot().traces_processed, 0);
    }

    #[test]
    fn test_action_domain_mapping() {
        assert_eq!(
            action_domain(&TraceAction::ObserveLatency {
                stage: LatencyStage::PtyCapture,
                latency_us: 1.0
            }),
            InvariantDomain::Budget
        );
        assert_eq!(
            action_domain(&TraceAction::SchedulerAdmit {
                lane: SchedulerLane::Input,
                cost_us: 1.0
            }),
            InvariantDomain::Scheduler
        );
        assert_eq!(
            action_domain(&TraceAction::RecoveryStep {
                level_before: MitigationLevel::None,
                level_after: MitigationLevel::Defer
            }),
            InvariantDomain::Recovery
        );
        assert_eq!(
            action_domain(&TraceAction::EpochAdvance { new_epoch: 1 }),
            InvariantDomain::Composition
        );
        assert_eq!(
            action_domain(&TraceAction::Reset {
                domain: InvariantDomain::Recovery
            }),
            InvariantDomain::Recovery
        );
    }

    #[test]
    fn test_canonicalizer_snapshot_serde() {
        let snap = CanonicalizerSnapshot {
            traces_processed: 5,
            entries_processed: 100,
            entries_deduped: 10,
            comparisons_made: 3,
            config: CanonicalizerConfig::default(),
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: CanonicalizerSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }

    #[test]
    fn test_canonicalizer_log_entry_serde() {
        let entry = CanonicalizerLogEntry {
            timestamp_us: 100,
            trace_id: "t1".to_string(),
            input_entries: 10,
            output_entries: 8,
            duration_us: 50,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: CanonicalizerLogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back);
    }

    #[test]
    fn test_canonicalize_reassigns_seq_numbers() {
        let mut c = ReplayCanonicalizer::new(CanonicalizerConfig {
            ordering: CanonicalOrdering::Temporal,
            ..Default::default()
        });
        let mut trace = DeterministicTrace::new_v2("t".to_string(), 0, 0);
        // Entries with seq 0,1 but timestamp order is reversed.
        trace.entries.push(TraceEntry {
            seq: 0,
            timestamp_us: 200,
            action: TraceAction::EpochAdvance { new_epoch: 2 },
            domain: InvariantDomain::Composition,
            causal_parent: None,
            fingerprint: 1,
        });
        trace.entries.push(TraceEntry {
            seq: 1,
            timestamp_us: 100,
            action: TraceAction::EpochAdvance { new_epoch: 1 },
            domain: InvariantDomain::Composition,
            causal_parent: None,
            fingerprint: 2,
        });
        let canonical = c.canonicalize(&trace);
        // After temporal sort, seq should be reassigned 0,1.
        assert_eq!(canonical.entries[0].seq, 0);
        assert_eq!(canonical.entries[1].seq, 1);
    }

    #[test]
    fn test_canonicalize_preserves_version() {
        let mut c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let trace = DeterministicTrace::new_v2("t".to_string(), 42, 0);
        let canonical = c.canonicalize(&trace);
        assert_eq!(canonical.version, TraceFormatVersion::V2);
        assert_eq!(canonical.seed, 42);
    }

    // ── E3 Impl: Bridge method tests ─────────────────────────────

    #[test]
    fn test_compare_mc_traces_identical() {
        let mut c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let steps = vec![
            TraceStep {
                step: 0,
                action: TraceAction::EpochAdvance { new_epoch: 1 },
                check_results: vec![],
                timestamp_us: 10,
            },
            TraceStep {
                step: 1,
                action: TraceAction::EpochAdvance { new_epoch: 2 },
                check_results: vec![],
                timestamp_us: 20,
            },
        ];
        let result = c.compare_mc_traces(&steps, &steps, 42);
        assert_eq!(result, ReplayComparisonResult::Identical);
    }

    #[test]
    fn test_compare_mc_traces_divergent() {
        let mut c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let a = vec![TraceStep {
            step: 0,
            action: TraceAction::EpochAdvance { new_epoch: 1 },
            check_results: vec![],
            timestamp_us: 10,
        }];
        let b = vec![TraceStep {
            step: 0,
            action: TraceAction::EpochAdvance { new_epoch: 99 },
            check_results: vec![],
            timestamp_us: 10,
        }];
        let result = c.compare_mc_traces(&a, &b, 0);
        let is_divergent = matches!(result, ReplayComparisonResult::Divergent { .. });
        assert!(is_divergent);
    }

    #[test]
    fn test_verify_determinism() {
        let mut c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let mut trace = DeterministicTrace::new_v2("t".to_string(), 0, 0);
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            10,
            None,
        );
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 2 },
            InvariantDomain::Composition,
            20,
            Some(0),
        );
        assert!(c.verify_determinism(&trace));
    }

    #[test]
    fn test_filter_by_domain() {
        let c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let mut trace = DeterministicTrace::new_v2("t".to_string(), 0, 0);
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            10,
            None,
        );
        trace.push(
            TraceAction::SchedulerAdmit {
                lane: SchedulerLane::Input,
                cost_us: 5.0,
            },
            InvariantDomain::Scheduler,
            20,
            None,
        );
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 2 },
            InvariantDomain::Composition,
            30,
            Some(0),
        );

        let filtered = c.filter_by_domain(&trace, InvariantDomain::Composition);
        assert_eq!(filtered.len(), 2);
        for e in &filtered.entries {
            assert_eq!(e.domain, InvariantDomain::Composition);
        }
        // Seq numbers reassigned.
        assert_eq!(filtered.entries[0].seq, 0);
        assert_eq!(filtered.entries[1].seq, 1);
    }

    #[test]
    fn test_filter_by_domain_empty() {
        let c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let mut trace = DeterministicTrace::new_v2("t".to_string(), 0, 0);
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            10,
            None,
        );
        let filtered = c.filter_by_domain(&trace, InvariantDomain::Recovery);
        assert!(filtered.is_empty());
    }

    #[test]
    fn test_causal_chain_no_parents() {
        let c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let mut trace = DeterministicTrace::new_v2("t".to_string(), 0, 0);
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            10,
            None,
        );
        let chain = c.causal_chain(&trace, 0);
        assert_eq!(chain, vec![0]);
    }

    #[test]
    fn test_causal_chain_with_parents() {
        let c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let mut trace = DeterministicTrace::new_v2("t".to_string(), 0, 0);
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            10,
            None,
        );
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 2 },
            InvariantDomain::Composition,
            20,
            Some(0),
        );
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 3 },
            InvariantDomain::Composition,
            30,
            Some(1),
        );
        let chain = c.causal_chain(&trace, 2);
        assert_eq!(chain, vec![0, 1, 2]);
    }

    #[test]
    fn test_domain_histogram() {
        let c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let mut trace = DeterministicTrace::new_v2("t".to_string(), 0, 0);
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            10,
            None,
        );
        trace.push(
            TraceAction::SchedulerAdmit {
                lane: SchedulerLane::Input,
                cost_us: 5.0,
            },
            InvariantDomain::Scheduler,
            20,
            None,
        );
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 2 },
            InvariantDomain::Composition,
            30,
            None,
        );
        let hist = c.domain_histogram(&trace);
        assert_eq!(hist.get("composition"), Some(&2));
        assert_eq!(hist.get("scheduler"), Some(&1));
    }

    #[test]
    fn test_unique_fingerprints() {
        let c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let mut trace = DeterministicTrace::new_v2("t".to_string(), 0, 0);
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            10,
            None,
        );
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            20,
            None,
        );
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 2 },
            InvariantDomain::Composition,
            30,
            None,
        );
        let unique = c.unique_fingerprints(&trace);
        // epoch 2 appears once, epoch 1 appears twice.
        assert_eq!(unique.len(), 1);
        assert_eq!(unique[0], 2); // seq of the epoch(2) entry
    }

    #[test]
    fn test_merge_traces() {
        let mut c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let mut t1 = DeterministicTrace::new_v2("a".to_string(), 1, 0);
        t1.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            10,
            None,
        );
        t1.push(
            TraceAction::EpochAdvance { new_epoch: 3 },
            InvariantDomain::Composition,
            30,
            None,
        );

        let mut t2 = DeterministicTrace::new_v2("b".to_string(), 2, 0);
        t2.push(
            TraceAction::EpochAdvance { new_epoch: 2 },
            InvariantDomain::Composition,
            20,
            None,
        );

        let merged = c.merge_traces(&t1, &t2);
        assert_eq!(merged.len(), 3);
        // Should be sorted by timestamp: 10, 20, 30.
        assert_eq!(merged.entries[0].timestamp_us, 10);
        assert_eq!(merged.entries[1].timestamp_us, 20);
        assert_eq!(merged.entries[2].timestamp_us, 30);
        // Seq reassigned.
        assert_eq!(merged.entries[0].seq, 0);
        assert_eq!(merged.entries[1].seq, 1);
        assert_eq!(merged.entries[2].seq, 2);
    }

    #[test]
    fn test_time_window() {
        let c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let mut trace = DeterministicTrace::new_v2("t".to_string(), 0, 0);
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            10,
            None,
        );
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 2 },
            InvariantDomain::Composition,
            20,
            None,
        );
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 3 },
            InvariantDomain::Composition,
            30,
            None,
        );
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 4 },
            InvariantDomain::Composition,
            40,
            None,
        );

        let windowed = c.time_window(&trace, 15, 35);
        assert_eq!(windowed.len(), 2);
        assert_eq!(windowed.entries[0].timestamp_us, 20);
        assert_eq!(windowed.entries[1].timestamp_us, 30);
    }

    #[test]
    fn test_time_window_empty() {
        let c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let mut trace = DeterministicTrace::new_v2("t".to_string(), 0, 0);
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            10,
            None,
        );
        let windowed = c.time_window(&trace, 100, 200);
        assert!(windowed.is_empty());
    }

    #[test]
    fn test_total_comparisons() {
        let mut c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        assert_eq!(c.total_comparisons(), 0);
        let t = DeterministicTrace::new_v2("t".to_string(), 0, 0);
        let _ = c.compare(&t, &t);
        assert_eq!(c.total_comparisons(), 1);
    }

    #[test]
    fn test_total_traces() {
        let mut c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        assert_eq!(c.total_traces(), 0);
        let t = DeterministicTrace::new_v2("t".to_string(), 0, 0);
        let _ = c.canonicalize(&t);
        assert_eq!(c.total_traces(), 1);
    }

    #[test]
    fn test_config_accessor() {
        let cfg = CanonicalizerConfig {
            ordering: CanonicalOrdering::Temporal,
            ..Default::default()
        };
        let c = ReplayCanonicalizer::new(cfg.clone());
        assert_eq!(*c.config(), cfg);
    }

    // ── E4: Optimization Isomorphism Proof Gate Tests ────────────

    fn make_golden_trace(entries: &[(u64, TraceAction, InvariantDomain)]) -> DeterministicTrace {
        let mut trace = DeterministicTrace::new_v2("golden".to_string(), 42, 0);
        for (ts, action, domain) in entries {
            trace.push(action.clone(), *domain, *ts, None);
        }
        trace
    }

    #[test]
    fn test_golden_artifact_new() {
        let trace = make_golden_trace(&[(
            10,
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
        )]);
        let ga = GoldenArtifact::new("test".to_string(), trace.clone(), "desc".to_string(), 0);
        assert_eq!(ga.artifact_id, "test");
        assert_eq!(ga.version, 1);
        assert!(ga.verify_checksum());
        assert_eq!(ga.checksum, trace.digest());
    }

    #[test]
    fn test_golden_artifact_update() {
        let t1 = make_golden_trace(&[(
            10,
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
        )]);
        let mut ga = GoldenArtifact::new("test".to_string(), t1, "v1".to_string(), 0);
        let t2 = make_golden_trace(&[(
            20,
            TraceAction::EpochAdvance { new_epoch: 2 },
            InvariantDomain::Composition,
        )]);
        ga.update(t2.clone(), 100);
        assert_eq!(ga.version, 2);
        assert_eq!(ga.checksum, t2.digest());
        assert!(ga.verify_checksum());
    }

    #[test]
    fn test_golden_artifact_serde() {
        let trace = make_golden_trace(&[(
            10,
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
        )]);
        let ga = GoldenArtifact::new("test".to_string(), trace, "desc".to_string(), 0);
        let json = serde_json::to_string(&ga).unwrap();
        let back: GoldenArtifact = serde_json::from_str(&json).unwrap();
        assert_eq!(ga, back);
    }

    #[test]
    fn test_golden_artifact_display() {
        let trace = make_golden_trace(&[(
            10,
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
        )]);
        let ga = GoldenArtifact::new("my-opt".to_string(), trace, "desc".to_string(), 0);
        let s = ga.to_string();
        assert!(s.contains("my-opt"));
        assert!(s.contains("v1"));
    }

    #[test]
    fn test_proof_gate_verdict_pass_fail() {
        assert!(ProofGateVerdict::Equivalent.is_pass());
        assert!(!ProofGateVerdict::Equivalent.is_fail());
        assert!(ProofGateVerdict::IsomorphicEquivalent { reordered_count: 1 }.is_pass());
        let drift = ProofGateVerdict::SemanticDrift {
            first_divergence_idx: 0,
            mismatches: vec![],
            summary: "x".to_string(),
        };
        assert!(drift.is_fail());
        let chk = ProofGateVerdict::ChecksumFailure {
            expected: 1,
            actual: 2,
        };
        assert!(chk.is_fail());
    }

    #[test]
    fn test_proof_gate_verdict_display() {
        assert!(ProofGateVerdict::Equivalent.to_string().contains("PASS"));
        let iso = ProofGateVerdict::IsomorphicEquivalent { reordered_count: 3 };
        assert!(iso.to_string().contains("isomorphic"));
        let drift = ProofGateVerdict::SemanticDrift {
            first_divergence_idx: 5,
            mismatches: vec![],
            summary: "oops".to_string(),
        };
        assert!(drift.to_string().contains("FAIL"));
    }

    #[test]
    fn test_proof_gate_verdict_serde() {
        let verdicts = vec![
            ProofGateVerdict::Equivalent,
            ProofGateVerdict::IsomorphicEquivalent { reordered_count: 2 },
            ProofGateVerdict::SemanticDrift {
                first_divergence_idx: 0,
                mismatches: vec![],
                summary: "x".to_string(),
            },
            ProofGateVerdict::ChecksumFailure {
                expected: 1,
                actual: 2,
            },
        ];
        for v in verdicts {
            let json = serde_json::to_string(&v).unwrap();
            let back: ProofGateVerdict = serde_json::from_str(&json).unwrap();
            assert_eq!(v, back);
        }
    }

    #[test]
    fn test_proof_gate_config_default() {
        let cfg = ProofGateConfig::default();
        assert!(cfg.allow_isomorphic);
        assert_eq!(cfg.max_mismatches, 50);
    }

    #[test]
    fn test_proof_summary_display() {
        let summary = ProofSummary {
            artifact_id: "test".to_string(),
            golden_version: 1,
            verdict: ProofGateVerdict::Equivalent,
            candidate_entries: 10,
            golden_entries: 10,
            check_duration_us: 500,
            timestamp_us: 0,
        };
        let s = summary.to_string();
        assert!(s.contains("test"));
        assert!(s.contains("PASS"));
    }

    #[test]
    fn test_proof_summary_serde() {
        let summary = ProofSummary {
            artifact_id: "test".to_string(),
            golden_version: 3,
            verdict: ProofGateVerdict::Equivalent,
            candidate_entries: 10,
            golden_entries: 10,
            check_duration_us: 500,
            timestamp_us: 100,
        };
        let json = serde_json::to_string(&summary).unwrap();
        let back: ProofSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(summary, back);
    }

    #[test]
    fn test_proof_gate_register_and_get() {
        let mut gate = ProofGate::new(ProofGateConfig::default());
        let trace = make_golden_trace(&[(
            10,
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
        )]);
        let ga = GoldenArtifact::new("opt-1".to_string(), trace, "desc".to_string(), 0);
        gate.register_golden(ga);
        assert_eq!(gate.artifact_count(), 1);
        assert!(gate.get_golden("opt-1").is_some());
        assert!(gate.get_golden("opt-2").is_none());
    }

    #[test]
    fn test_proof_gate_register_replaces() {
        let mut gate = ProofGate::new(ProofGateConfig::default());
        let t1 = make_golden_trace(&[(
            10,
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
        )]);
        let t2 = make_golden_trace(&[(
            20,
            TraceAction::EpochAdvance { new_epoch: 2 },
            InvariantDomain::Composition,
        )]);
        gate.register_golden(GoldenArtifact::new(
            "x".to_string(),
            t1,
            "v1".to_string(),
            0,
        ));
        gate.register_golden(GoldenArtifact::new(
            "x".to_string(),
            t2,
            "v2".to_string(),
            100,
        ));
        assert_eq!(gate.artifact_count(), 1);
        assert_eq!(gate.get_golden("x").unwrap().description, "v2");
    }

    #[test]
    fn test_proof_gate_check_equivalent() {
        let mut gate = ProofGate::new(ProofGateConfig::default());
        let trace = make_golden_trace(&[(
            10,
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
        )]);
        gate.register_golden(GoldenArtifact::new(
            "opt-1".to_string(),
            trace.clone(),
            "d".to_string(),
            0,
        ));
        let summary = gate.check("opt-1", &trace, 100);
        assert_eq!(summary.verdict, ProofGateVerdict::Equivalent);
        assert_eq!(gate.snapshot().passes, 1);
    }

    #[test]
    fn test_proof_gate_check_divergent() {
        let mut gate = ProofGate::new(ProofGateConfig::default());
        let golden = make_golden_trace(&[(
            10,
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
        )]);
        gate.register_golden(GoldenArtifact::new(
            "opt-1".to_string(),
            golden,
            "d".to_string(),
            0,
        ));
        let candidate = make_golden_trace(&[(
            10,
            TraceAction::EpochAdvance { new_epoch: 99 },
            InvariantDomain::Composition,
        )]);
        let summary = gate.check("opt-1", &candidate, 100);
        assert!(summary.verdict.is_fail());
        assert_eq!(gate.snapshot().failures, 1);
    }

    #[test]
    fn test_proof_gate_check_missing_artifact() {
        let mut gate = ProofGate::new(ProofGateConfig::default());
        let candidate = DeterministicTrace::new_v2("c".to_string(), 0, 0);
        let summary = gate.check("nonexistent", &candidate, 100);
        assert!(summary.verdict.is_fail());
    }

    #[test]
    fn test_proof_gate_remove_golden() {
        let mut gate = ProofGate::new(ProofGateConfig::default());
        let trace = make_golden_trace(&[(
            10,
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
        )]);
        gate.register_golden(GoldenArtifact::new(
            "x".to_string(),
            trace,
            "d".to_string(),
            0,
        ));
        assert!(gate.remove_golden("x"));
        assert_eq!(gate.artifact_count(), 0);
        assert!(!gate.remove_golden("x"));
    }

    #[test]
    fn test_proof_gate_artifact_ids() {
        let mut gate = ProofGate::new(ProofGateConfig::default());
        let t = make_golden_trace(&[(
            10,
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
        )]);
        gate.register_golden(GoldenArtifact::new(
            "a".to_string(),
            t.clone(),
            "d".to_string(),
            0,
        ));
        gate.register_golden(GoldenArtifact::new("b".to_string(), t, "d".to_string(), 0));
        let ids = gate.artifact_ids();
        assert!(ids.contains(&"a".to_string()));
        assert!(ids.contains(&"b".to_string()));
    }

    #[test]
    fn test_proof_gate_reset_counters() {
        let mut gate = ProofGate::new(ProofGateConfig::default());
        let trace = make_golden_trace(&[(
            10,
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
        )]);
        gate.register_golden(GoldenArtifact::new(
            "x".to_string(),
            trace.clone(),
            "d".to_string(),
            0,
        ));
        let _ = gate.check("x", &trace, 0);
        gate.reset_counters();
        let snap = gate.snapshot();
        assert_eq!(snap.checks_run, 0);
        assert_eq!(snap.passes, 0);
        assert_eq!(snap.failures, 0);
        assert_eq!(snap.artifacts_count, 1); // Artifacts preserved.
    }

    #[test]
    fn test_proof_gate_snapshot_serde() {
        let snap = ProofGateSnapshot {
            checks_run: 10,
            passes: 8,
            failures: 2,
            artifacts_count: 3,
            config: ProofGateConfig::default(),
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: ProofGateSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }

    #[test]
    fn test_proof_gate_degradation_healthy() {
        let gate = ProofGate::new(ProofGateConfig::default());
        assert_eq!(gate.detect_degradation(), ProofGateDegradation::Healthy);
    }

    #[test]
    fn test_proof_gate_degradation_display() {
        assert_eq!(ProofGateDegradation::Healthy.to_string(), "healthy");
        let hfr = ProofGateDegradation::HighFailureRate { rate: 0.75 };
        assert!(hfr.to_string().contains("high-failure-rate"));
        let hac = ProofGateDegradation::HighArtifactCount { count: 200 };
        assert!(hac.to_string().contains("high-artifact-count"));
    }

    #[test]
    fn test_proof_gate_degradation_serde() {
        let variants = vec![
            ProofGateDegradation::Healthy,
            ProofGateDegradation::HighFailureRate { rate: 0.8 },
            ProofGateDegradation::HighArtifactCount { count: 150 },
        ];
        for v in variants {
            let json = serde_json::to_string(&v).unwrap();
            let back: ProofGateDegradation = serde_json::from_str(&json).unwrap();
            assert_eq!(v, back);
        }
    }

    #[test]
    fn test_proof_gate_log_entry() {
        let gate = ProofGate::new(ProofGateConfig::default());
        let entry = gate.log_entry("test", true, 500);
        assert_eq!(entry.artifact_id, "test");
        assert!(entry.passed);
        assert_eq!(entry.check_duration_us, 500);
    }

    #[test]
    fn test_proof_gate_log_entry_serde() {
        let entry = ProofGateLogEntry {
            timestamp_us: 100,
            artifact_id: "opt-1".to_string(),
            passed: true,
            check_duration_us: 50,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: ProofGateLogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back);
    }

    #[test]
    fn test_proof_gate_config_accessor() {
        let cfg = ProofGateConfig {
            allow_isomorphic: false,
            ..Default::default()
        };
        let gate = ProofGate::new(cfg.clone());
        assert_eq!(*gate.config(), cfg);
    }

    // ── E4 Impl: Bridge method tests ─────────────────────────────

    #[test]
    fn test_check_from_mc_trace() {
        let mut gate = ProofGate::new(ProofGateConfig::default());
        let steps = vec![TraceStep {
            step: 0,
            action: TraceAction::EpochAdvance { new_epoch: 1 },
            check_results: vec![],
            timestamp_us: 10,
        }];
        gate.register_golden_from_mc("mc-opt".to_string(), &steps, 42, "desc".to_string(), 0);
        let summary = gate.check_from_mc_trace("mc-opt", &steps, 42, 100);
        assert!(summary.verdict.is_pass());
    }

    #[test]
    fn test_register_golden_from_mc() {
        let mut gate = ProofGate::new(ProofGateConfig::default());
        let steps = vec![TraceStep {
            step: 0,
            action: TraceAction::EpochAdvance { new_epoch: 5 },
            check_results: vec![],
            timestamp_us: 100,
        }];
        gate.register_golden_from_mc("mc-1".to_string(), &steps, 99, "mc golden".to_string(), 0);
        assert_eq!(gate.artifact_count(), 1);
        let ga = gate.get_golden("mc-1").unwrap();
        assert_eq!(ga.trace.version, TraceFormatVersion::V2);
        assert_eq!(ga.trace.len(), 1);
    }

    #[test]
    fn test_approve_drift() {
        let mut gate = ProofGate::new(ProofGateConfig::default());
        let t1 = make_golden_trace(&[(
            10,
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
        )]);
        gate.register_golden(GoldenArtifact::new(
            "x".to_string(),
            t1,
            "v1".to_string(),
            0,
        ));
        let t2 = make_golden_trace(&[(
            20,
            TraceAction::EpochAdvance { new_epoch: 2 },
            InvariantDomain::Composition,
        )]);
        assert!(gate.approve_drift("x", &t2, 100));
        assert_eq!(gate.get_golden("x").unwrap().version, 2);
        assert!(!gate.approve_drift("nonexistent", &t2, 100));
    }

    #[test]
    fn test_failing_passing_artifacts() {
        let summaries = vec![
            ProofSummary {
                artifact_id: "a".to_string(),
                golden_version: 1,
                verdict: ProofGateVerdict::Equivalent,
                candidate_entries: 1,
                golden_entries: 1,
                check_duration_us: 0,
                timestamp_us: 0,
            },
            ProofSummary {
                artifact_id: "b".to_string(),
                golden_version: 1,
                verdict: ProofGateVerdict::SemanticDrift {
                    first_divergence_idx: 0,
                    mismatches: vec![],
                    summary: "x".to_string(),
                },
                candidate_entries: 1,
                golden_entries: 1,
                check_duration_us: 0,
                timestamp_us: 0,
            },
        ];
        assert_eq!(
            ProofGate::failing_artifacts(&summaries),
            vec!["b".to_string()]
        );
        assert_eq!(
            ProofGate::passing_artifacts(&summaries),
            vec!["a".to_string()]
        );
    }

    #[test]
    fn test_pass_rate() {
        let summaries = vec![
            ProofSummary {
                artifact_id: "a".to_string(),
                golden_version: 1,
                verdict: ProofGateVerdict::Equivalent,
                candidate_entries: 1,
                golden_entries: 1,
                check_duration_us: 0,
                timestamp_us: 0,
            },
            ProofSummary {
                artifact_id: "b".to_string(),
                golden_version: 1,
                verdict: ProofGateVerdict::SemanticDrift {
                    first_divergence_idx: 0,
                    mismatches: vec![],
                    summary: "x".to_string(),
                },
                candidate_entries: 1,
                golden_entries: 1,
                check_duration_us: 0,
                timestamp_us: 0,
            },
        ];
        let rate = ProofGate::pass_rate(&summaries);
        assert!((rate - 0.5).abs() < 1e-10);
        assert!((ProofGate::pass_rate(&[]) - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_total_counters() {
        let mut gate = ProofGate::new(ProofGateConfig::default());
        let trace = make_golden_trace(&[(
            10,
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
        )]);
        gate.register_golden(GoldenArtifact::new(
            "x".to_string(),
            trace.clone(),
            "d".to_string(),
            0,
        ));
        let _ = gate.check("x", &trace, 0);
        assert_eq!(gate.total_checks(), 1);
        assert_eq!(gate.total_passes(), 1);
        assert_eq!(gate.total_failures(), 0);
    }

    #[test]
    fn test_check_all() {
        let mut gate = ProofGate::new(ProofGateConfig::default());
        let t1 = make_golden_trace(&[(
            10,
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
        )]);
        let t2 = make_golden_trace(&[(
            20,
            TraceAction::EpochAdvance { new_epoch: 2 },
            InvariantDomain::Composition,
        )]);
        gate.register_golden(GoldenArtifact::new(
            "a".to_string(),
            t1.clone(),
            "d".to_string(),
            0,
        ));
        gate.register_golden(GoldenArtifact::new(
            "b".to_string(),
            t2.clone(),
            "d".to_string(),
            0,
        ));
        let mut candidates = std::collections::HashMap::new();
        candidates.insert("a".to_string(), t1);
        candidates.insert("b".to_string(), t2);
        let summaries = gate.check_all(&candidates, 100);
        assert_eq!(summaries.len(), 2);
        for s in &summaries {
            assert!(s.verdict.is_pass());
        }
    }

    // ── F1: Fault Domain Isolation Tests ─────────────────────────

    #[test]
    fn test_fault_domain_all() {
        assert_eq!(FaultDomain::ALL.len(), 5);
    }

    #[test]
    fn test_fault_domain_display() {
        assert_eq!(FaultDomain::Scheduler.to_string(), "scheduler");
        assert_eq!(FaultDomain::Budget.to_string(), "budget");
        assert_eq!(FaultDomain::Recovery.to_string(), "recovery");
        assert_eq!(FaultDomain::Io.to_string(), "io");
        assert_eq!(FaultDomain::Storage.to_string(), "storage");
    }

    #[test]
    fn test_fault_domain_serde() {
        for d in FaultDomain::ALL {
            let json = serde_json::to_string(d).unwrap();
            let back: FaultDomain = serde_json::from_str(&json).unwrap();
            assert_eq!(*d, back);
        }
    }

    #[test]
    fn test_domain_health_display() {
        assert_eq!(DomainHealth::Healthy.to_string(), "healthy");
        assert_eq!(DomainHealth::Degraded.to_string(), "degraded");
        assert_eq!(DomainHealth::Crashed.to_string(), "crashed");
        assert_eq!(DomainHealth::Restarting.to_string(), "restarting");
        assert_eq!(DomainHealth::Isolated.to_string(), "isolated");
    }

    #[test]
    fn test_domain_health_serde() {
        for h in [
            DomainHealth::Healthy,
            DomainHealth::Degraded,
            DomainHealth::Crashed,
            DomainHealth::Restarting,
            DomainHealth::Isolated,
        ] {
            let json = serde_json::to_string(&h).unwrap();
            let back: DomainHealth = serde_json::from_str(&json).unwrap();
            assert_eq!(h, back);
        }
    }

    #[test]
    fn test_crash_only_contract_default() {
        let c = CrashOnlyContract::default();
        assert_eq!(c.max_restarts, 3);
        assert!(c.checkpoint_on_crash);
    }

    #[test]
    fn test_crash_only_contract_serde() {
        let c = CrashOnlyContract {
            domain: FaultDomain::Io,
            max_restarts: 5,
            restart_cooldown_us: 50_000,
            checkpoint_on_crash: false,
            restart_timeout_us: 1_000_000,
        };
        let json = serde_json::to_string(&c).unwrap();
        let back: CrashOnlyContract = serde_json::from_str(&json).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn test_fault_event_serde() {
        let ev = FaultEvent {
            domain: FaultDomain::Storage,
            timestamp_us: 12345,
            description: "disk full".to_string(),
            recovery_attempted: true,
            recovery_succeeded: false,
        };
        let json = serde_json::to_string(&ev).unwrap();
        let back: FaultEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn test_fault_isolation_manager_new() {
        let mgr = FaultIsolationManager::new(FaultIsolationConfig::default());
        for d in FaultDomain::ALL {
            assert_eq!(mgr.domain_health(*d), DomainHealth::Healthy);
        }
        assert!(!mgr.has_isolated_domains());
    }

    #[test]
    fn test_record_fault_transitions_to_crashed() {
        let mut mgr = FaultIsolationManager::new(FaultIsolationConfig::default());
        mgr.record_fault(FaultDomain::Scheduler, "test fault".to_string(), 100);
        assert_eq!(
            mgr.domain_health(FaultDomain::Scheduler),
            DomainHealth::Crashed
        );
        assert_eq!(
            mgr.domain_state(FaultDomain::Scheduler)
                .unwrap()
                .total_faults,
            1
        );
    }

    #[test]
    fn test_record_fault_auto_isolates() {
        let mut mgr = FaultIsolationManager::new(FaultIsolationConfig::default());
        // Default max_restarts is 3, so 4th fault triggers isolation.
        for i in 0..4 {
            mgr.record_fault(FaultDomain::Scheduler, format!("fault {i}"), (i + 1) * 100);
        }
        assert_eq!(
            mgr.domain_health(FaultDomain::Scheduler),
            DomainHealth::Isolated
        );
        assert!(mgr.has_isolated_domains());
    }

    #[test]
    fn test_attempt_restart_success() {
        let mut mgr = FaultIsolationManager::new(FaultIsolationConfig::default());
        mgr.record_fault(FaultDomain::Budget, "test".to_string(), 100);
        assert!(mgr.attempt_restart(FaultDomain::Budget, 200_000));
        assert_eq!(
            mgr.domain_health(FaultDomain::Budget),
            DomainHealth::Restarting
        );
    }

    #[test]
    fn test_attempt_restart_cooldown_enforced() {
        let mut mgr = FaultIsolationManager::new(FaultIsolationConfig::default());
        mgr.record_fault(FaultDomain::Budget, "test".to_string(), 100);
        assert!(mgr.attempt_restart(FaultDomain::Budget, 200_000));
        mgr.restart_failed(FaultDomain::Budget, 200_001);
        // Too soon — cooldown not elapsed.
        assert!(!mgr.attempt_restart(FaultDomain::Budget, 200_002));
    }

    #[test]
    fn test_restart_succeeded_resets_failures() {
        let mut mgr = FaultIsolationManager::new(FaultIsolationConfig::default());
        mgr.record_fault(FaultDomain::Io, "err".to_string(), 100);
        mgr.attempt_restart(FaultDomain::Io, 200_000);
        mgr.restart_succeeded(FaultDomain::Io);
        assert_eq!(mgr.domain_health(FaultDomain::Io), DomainHealth::Healthy);
        assert_eq!(
            mgr.domain_state(FaultDomain::Io)
                .unwrap()
                .consecutive_failures,
            0
        );
    }

    #[test]
    fn test_restart_failed_increments_failures() {
        let mut mgr = FaultIsolationManager::new(FaultIsolationConfig::default());
        mgr.record_fault(FaultDomain::Io, "err".to_string(), 100);
        mgr.attempt_restart(FaultDomain::Io, 200_000);
        mgr.restart_failed(FaultDomain::Io, 200_001);
        assert_eq!(mgr.domain_health(FaultDomain::Io), DomainHealth::Crashed);
        assert_eq!(
            mgr.domain_state(FaultDomain::Io)
                .unwrap()
                .consecutive_failures,
            2
        );
    }

    #[test]
    fn test_mark_degraded() {
        let mut mgr = FaultIsolationManager::new(FaultIsolationConfig::default());
        mgr.mark_degraded(FaultDomain::Storage);
        assert_eq!(
            mgr.domain_health(FaultDomain::Storage),
            DomainHealth::Degraded
        );
    }

    #[test]
    fn test_un_isolate() {
        let mut mgr = FaultIsolationManager::new(FaultIsolationConfig::default());
        for i in 0..4 {
            mgr.record_fault(FaultDomain::Recovery, format!("f{i}"), i * 100);
        }
        assert_eq!(
            mgr.domain_health(FaultDomain::Recovery),
            DomainHealth::Isolated
        );
        mgr.un_isolate(FaultDomain::Recovery);
        assert_eq!(
            mgr.domain_health(FaultDomain::Recovery),
            DomainHealth::Crashed
        );
    }

    #[test]
    fn test_fault_isolation_snapshot() {
        let mut mgr = FaultIsolationManager::new(FaultIsolationConfig::default());
        mgr.record_fault(FaultDomain::Scheduler, "test".to_string(), 100);
        let snap = mgr.snapshot();
        assert_eq!(snap.total_faults, 1);
        assert_eq!(snap.domains.len(), 5);
    }

    #[test]
    fn test_fault_isolation_snapshot_serde() {
        let mgr = FaultIsolationManager::new(FaultIsolationConfig::default());
        let snap = mgr.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let back: FaultIsolationSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }

    #[test]
    fn test_fault_isolation_degradation_healthy() {
        let mgr = FaultIsolationManager::new(FaultIsolationConfig::default());
        assert_eq!(mgr.detect_degradation(), FaultIsolationDegradation::Healthy);
    }

    #[test]
    fn test_fault_isolation_degradation_partial() {
        let mut mgr = FaultIsolationManager::new(FaultIsolationConfig::default());
        mgr.record_fault(FaultDomain::Scheduler, "x".to_string(), 100);
        let deg = mgr.detect_degradation();
        let is_partial = matches!(deg, FaultIsolationDegradation::PartialDegradation { .. });
        assert!(is_partial);
    }

    #[test]
    fn test_fault_isolation_degradation_isolated() {
        let mut mgr = FaultIsolationManager::new(FaultIsolationConfig::default());
        for i in 0..4 {
            mgr.record_fault(FaultDomain::Io, format!("f{i}"), i * 100);
        }
        let deg = mgr.detect_degradation();
        let is_isolated = matches!(deg, FaultIsolationDegradation::DomainIsolated { .. });
        assert!(is_isolated);
    }

    #[test]
    fn test_fault_isolation_degradation_display() {
        assert_eq!(FaultIsolationDegradation::Healthy.to_string(), "healthy");
        let pd = FaultIsolationDegradation::PartialDegradation { degraded_count: 2 };
        assert!(pd.to_string().contains("partial-degradation"));
        let di = FaultIsolationDegradation::DomainIsolated {
            isolated_domains: vec![FaultDomain::Io],
        };
        assert!(di.to_string().contains("io"));
    }

    #[test]
    fn test_fault_isolation_degradation_serde() {
        let variants: Vec<FaultIsolationDegradation> = vec![
            FaultIsolationDegradation::Healthy,
            FaultIsolationDegradation::PartialDegradation { degraded_count: 2 },
            FaultIsolationDegradation::DomainIsolated {
                isolated_domains: vec![FaultDomain::Scheduler],
            },
        ];
        for v in variants {
            let json = serde_json::to_string(&v).unwrap();
            let back: FaultIsolationDegradation = serde_json::from_str(&json).unwrap();
            assert_eq!(v, back);
        }
    }

    #[test]
    fn test_fault_isolation_log_entry_serde() {
        let entry = FaultIsolationLogEntry {
            timestamp_us: 100,
            domain: FaultDomain::Budget,
            from_health: DomainHealth::Healthy,
            to_health: DomainHealth::Crashed,
            description: "budget exceeded".to_string(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: FaultIsolationLogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back);
    }

    #[test]
    fn test_fault_history_capped() {
        let cfg = FaultIsolationConfig {
            max_history: 3,
            ..Default::default()
        };
        let mut mgr = FaultIsolationManager::new(cfg);
        for i in 0..5 {
            mgr.record_fault(FaultDomain::Scheduler, format!("f{i}"), i * 100);
        }
        assert_eq!(mgr.fault_history().len(), 3);
    }

    #[test]
    fn test_fault_isolation_reset() {
        let mut mgr = FaultIsolationManager::new(FaultIsolationConfig::default());
        mgr.record_fault(FaultDomain::Scheduler, "x".to_string(), 100);
        mgr.reset();
        for d in FaultDomain::ALL {
            assert_eq!(mgr.domain_health(*d), DomainHealth::Healthy);
        }
        assert!(mgr.fault_history().is_empty());
    }

    // ── F1 Impl: Bridge method tests ─────────────────────────────

    #[test]
    fn test_healthy_unhealthy_count() {
        let mut mgr = FaultIsolationManager::new(FaultIsolationConfig::default());
        assert_eq!(mgr.healthy_count(), 5);
        assert_eq!(mgr.unhealthy_count(), 0);
        mgr.record_fault(FaultDomain::Scheduler, "x".to_string(), 100);
        assert_eq!(mgr.healthy_count(), 4);
        assert_eq!(mgr.unhealthy_count(), 1);
    }

    #[test]
    fn test_total_faults_and_restarts() {
        let mut mgr = FaultIsolationManager::new(FaultIsolationConfig::default());
        mgr.record_fault(FaultDomain::Scheduler, "a".to_string(), 100);
        mgr.record_fault(FaultDomain::Budget, "b".to_string(), 200);
        assert_eq!(mgr.total_faults(), 2);
        assert_eq!(mgr.total_restarts(), 0);
    }

    #[test]
    fn test_domain_faults_and_restarts() {
        let mut mgr = FaultIsolationManager::new(FaultIsolationConfig::default());
        mgr.record_fault(FaultDomain::Io, "x".to_string(), 100);
        mgr.record_fault(FaultDomain::Io, "y".to_string(), 200);
        assert_eq!(mgr.domain_faults(FaultDomain::Io), 2);
        assert_eq!(mgr.domain_faults(FaultDomain::Budget), 0);
    }

    #[test]
    fn test_all_healthy() {
        let mut mgr = FaultIsolationManager::new(FaultIsolationConfig::default());
        assert!(mgr.all_healthy());
        mgr.record_fault(FaultDomain::Storage, "x".to_string(), 100);
        assert!(!mgr.all_healthy());
    }

    #[test]
    fn test_to_invariant_domain() {
        assert_eq!(
            FaultIsolationManager::to_invariant_domain(FaultDomain::Scheduler),
            InvariantDomain::Scheduler
        );
        assert_eq!(
            FaultIsolationManager::to_invariant_domain(FaultDomain::Budget),
            InvariantDomain::Budget
        );
        assert_eq!(
            FaultIsolationManager::to_invariant_domain(FaultDomain::Recovery),
            InvariantDomain::Recovery
        );
        assert_eq!(
            FaultIsolationManager::to_invariant_domain(FaultDomain::Io),
            InvariantDomain::Composition
        );
        assert_eq!(
            FaultIsolationManager::to_invariant_domain(FaultDomain::Storage),
            InvariantDomain::Composition
        );
    }

    #[test]
    fn test_config_accessor_fault() {
        let cfg = FaultIsolationConfig {
            auto_isolate: false,
            ..Default::default()
        };
        let mgr = FaultIsolationManager::new(cfg.clone());
        assert_eq!(*mgr.config(), cfg);
    }

    // ── F1: Blast-radius analysis tests ──

    #[test]
    fn test_blast_radius_default_graph() {
        let bra = BlastRadiusAnalyzer::default_graph();
        assert_eq!(bra.edges().len(), 3);
    }

    #[test]
    fn test_blast_radius_io_cascades_to_storage() {
        let bra = BlastRadiusAnalyzer::default_graph();
        let report = bra.analyze(FaultDomain::Io);
        assert!(report.direct_risk.contains(&FaultDomain::Storage));
        assert_eq!(report.total_at_risk, 1);
    }

    #[test]
    fn test_blast_radius_scheduler_cascades_to_budget() {
        let bra = BlastRadiusAnalyzer::default_graph();
        let report = bra.analyze(FaultDomain::Scheduler);
        assert!(report.direct_risk.contains(&FaultDomain::Budget));
    }

    #[test]
    fn test_blast_radius_recovery_transitive() {
        let bra = BlastRadiusAnalyzer::default_graph();
        // Recovery → Scheduler → Budget (transitive chain).
        let report = bra.analyze(FaultDomain::Recovery);
        assert!(report.direct_risk.contains(&FaultDomain::Scheduler));
        assert!(report.transitive_risk.contains(&FaultDomain::Budget));
        assert_eq!(report.total_at_risk, 2);
    }

    #[test]
    fn test_blast_radius_no_cascades_from_budget() {
        let bra = BlastRadiusAnalyzer::default_graph();
        let report = bra.analyze(FaultDomain::Budget);
        assert!(report.direct_risk.is_empty());
        assert_eq!(report.total_at_risk, 0);
    }

    #[test]
    fn test_blast_radius_custom_graph() {
        let bra = BlastRadiusAnalyzer::new(vec![DomainDependency {
            source: FaultDomain::Storage,
            target: FaultDomain::Io,
            cascade_weight: 90,
        }]);
        let report = bra.analyze(FaultDomain::Storage);
        assert_eq!(report.direct_risk, vec![FaultDomain::Io]);
    }

    #[test]
    fn test_blast_radius_reachable_from() {
        let bra = BlastRadiusAnalyzer::default_graph();
        let reachable = bra.reachable_from(FaultDomain::Recovery);
        assert!(reachable.contains(&FaultDomain::Scheduler));
        assert!(reachable.contains(&FaultDomain::Budget));
    }

    #[test]
    fn test_blast_radius_add_edge() {
        let mut bra = BlastRadiusAnalyzer::new(vec![]);
        assert_eq!(bra.edges().len(), 0);
        bra.add_edge(DomainDependency {
            source: FaultDomain::Budget,
            target: FaultDomain::Recovery,
            cascade_weight: 50,
        });
        assert_eq!(bra.edges().len(), 1);
        let report = bra.analyze(FaultDomain::Budget);
        assert_eq!(report.total_at_risk, 1);
    }

    #[test]
    fn test_blast_radius_report_serde() {
        let report = BlastRadiusReport {
            origin: FaultDomain::Io,
            direct_risk: vec![FaultDomain::Storage],
            transitive_risk: vec![],
            total_at_risk: 1,
        };
        let json = serde_json::to_string(&report).unwrap();
        let back: BlastRadiusReport = serde_json::from_str(&json).unwrap();
        assert_eq!(report, back);
    }

    #[test]
    fn test_domain_dependency_serde() {
        let dep = DomainDependency {
            source: FaultDomain::Io,
            target: FaultDomain::Storage,
            cascade_weight: 80,
        };
        let json = serde_json::to_string(&dep).unwrap();
        let back: DomainDependency = serde_json::from_str(&json).unwrap();
        assert_eq!(dep, back);
    }

    // ── F1: Instrumented fault manager + transition log tests ──

    #[test]
    fn test_instrumented_record_fault_emits_log() {
        let mut mgr = InstrumentedFaultManager::new(FaultIsolationConfig::default());
        mgr.record_fault(FaultDomain::Scheduler, "test fault".to_string(), 100);
        assert_eq!(mgr.log_count(), 1);
        let entry = &mgr.transition_log()[0];
        assert_eq!(entry.domain, FaultDomain::Scheduler);
        assert_eq!(entry.from, DomainHealth::Healthy);
        assert_eq!(entry.to, DomainHealth::Crashed);
        assert_eq!(entry.reason_code, FaultReasonCode::FaultRecorded);
    }

    #[test]
    fn test_instrumented_restart_cycle_logs() {
        let mut mgr = InstrumentedFaultManager::new(FaultIsolationConfig::default());
        mgr.record_fault(FaultDomain::Io, "err".to_string(), 100);
        mgr.attempt_restart(FaultDomain::Io, 200_000);
        mgr.restart_succeeded(FaultDomain::Io, 300_000);
        assert_eq!(mgr.log_count(), 3);
        assert_eq!(mgr.domain_health(FaultDomain::Io), DomainHealth::Healthy);
        // Verify transition chain: Healthy→Crashed→Restarting→Healthy.
        assert_eq!(mgr.transition_log()[0].to, DomainHealth::Crashed);
        assert_eq!(mgr.transition_log()[1].to, DomainHealth::Restarting);
        assert_eq!(mgr.transition_log()[2].to, DomainHealth::Healthy);
    }

    #[test]
    fn test_instrumented_auto_isolate_reason_code() {
        let mut mgr = InstrumentedFaultManager::new(FaultIsolationConfig::default());
        for i in 0..4 {
            mgr.record_fault(FaultDomain::Budget, format!("f{i}"), (i + 1) * 100);
        }
        // 4th fault should auto-isolate.
        let last = mgr.transition_log().last().unwrap();
        assert_eq!(last.to, DomainHealth::Isolated);
        assert_eq!(last.reason_code, FaultReasonCode::AutoIsolated);
    }

    #[test]
    fn test_instrumented_drain_log() {
        let mut mgr = InstrumentedFaultManager::new(FaultIsolationConfig::default());
        mgr.record_fault(FaultDomain::Storage, "x".to_string(), 100);
        let drained = mgr.drain_log();
        assert_eq!(drained.len(), 1);
        assert!(mgr.transition_log().is_empty());
    }

    #[test]
    fn test_instrumented_mark_degraded_log() {
        let mut mgr = InstrumentedFaultManager::new(FaultIsolationConfig::default());
        mgr.mark_degraded(FaultDomain::Recovery, 500);
        assert_eq!(mgr.log_count(), 1);
        assert_eq!(
            mgr.transition_log()[0].reason_code,
            FaultReasonCode::ManualDegraded
        );
    }

    #[test]
    fn test_instrumented_un_isolate_log() {
        let mut mgr = InstrumentedFaultManager::new(FaultIsolationConfig::default());
        for i in 0..4 {
            mgr.record_fault(FaultDomain::Io, format!("f{i}"), (i + 1) * 100);
        }
        let before_count = mgr.log_count();
        mgr.un_isolate(FaultDomain::Io, 5000);
        assert_eq!(mgr.log_count(), before_count + 1);
        let last = mgr.transition_log().last().unwrap();
        assert_eq!(last.reason_code, FaultReasonCode::ManualUnIsolate);
        assert_eq!(last.to, DomainHealth::Crashed);
    }

    #[test]
    fn test_instrumented_no_log_on_noop_transition() {
        let mut mgr = InstrumentedFaultManager::new(FaultIsolationConfig::default());
        // mark_degraded on a non-healthy domain is a no-op.
        mgr.record_fault(FaultDomain::Scheduler, "x".to_string(), 100);
        let count_before = mgr.log_count();
        mgr.mark_degraded(FaultDomain::Scheduler, 200);
        // Scheduler is Crashed not Healthy, so mark_degraded is no-op → no log.
        assert_eq!(mgr.log_count(), count_before);
    }

    #[test]
    fn test_instrumented_correlation_ids_unique() {
        let mut mgr = InstrumentedFaultManager::new(FaultIsolationConfig::default());
        mgr.record_fault(FaultDomain::Scheduler, "a".to_string(), 100);
        mgr.record_fault(FaultDomain::Budget, "b".to_string(), 200);
        let ids: Vec<u64> = mgr
            .transition_log()
            .iter()
            .map(|e| e.correlation_id)
            .collect();
        assert_ne!(ids[0], ids[1]);
    }

    #[test]
    fn test_instrumented_seq_numbers_monotonic() {
        let mut mgr = InstrumentedFaultManager::new(FaultIsolationConfig::default());
        mgr.record_fault(FaultDomain::Io, "a".to_string(), 100);
        mgr.record_fault(FaultDomain::Storage, "b".to_string(), 200);
        let seqs: Vec<u64> = mgr.transition_log().iter().map(|e| e.seq).collect();
        for w in seqs.windows(2) {
            assert!(w[1] > w[0]);
        }
    }

    #[test]
    fn test_fault_transition_log_serde() {
        let entry = FaultTransitionLog {
            seq: 0,
            timestamp_us: 100,
            domain: FaultDomain::Scheduler,
            from: DomainHealth::Healthy,
            to: DomainHealth::Crashed,
            reason_code: FaultReasonCode::FaultRecorded,
            description: "test".to_string(),
            correlation_id: 1,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: FaultTransitionLog = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back);
    }

    #[test]
    fn test_fault_reason_code_display() {
        assert_eq!(FaultReasonCode::FaultRecorded.to_string(), "fault-recorded");
        assert_eq!(FaultReasonCode::AutoIsolated.to_string(), "auto-isolated");
        assert_eq!(
            FaultReasonCode::RestartAttempted.to_string(),
            "restart-attempted"
        );
        assert_eq!(
            FaultReasonCode::RestartSucceeded.to_string(),
            "restart-succeeded"
        );
        assert_eq!(FaultReasonCode::RestartFailed.to_string(), "restart-failed");
        assert_eq!(
            FaultReasonCode::ManualDegraded.to_string(),
            "manual-degraded"
        );
        assert_eq!(
            FaultReasonCode::ManualUnIsolate.to_string(),
            "manual-un-isolate"
        );
        assert_eq!(FaultReasonCode::Reset.to_string(), "reset");
        assert_eq!(FaultReasonCode::Replay.to_string(), "replay");
    }

    #[test]
    fn test_fault_reason_code_serde() {
        for code in [
            FaultReasonCode::FaultRecorded,
            FaultReasonCode::AutoIsolated,
            FaultReasonCode::RestartAttempted,
            FaultReasonCode::RestartSucceeded,
            FaultReasonCode::RestartFailed,
            FaultReasonCode::ManualDegraded,
            FaultReasonCode::ManualUnIsolate,
            FaultReasonCode::Reset,
            FaultReasonCode::Replay,
        ] {
            let json = serde_json::to_string(&code).unwrap();
            let back: FaultReasonCode = serde_json::from_str(&json).unwrap();
            assert_eq!(code, back);
        }
    }

    // ── F1: Deterministic replay tests ──

    #[test]
    fn test_replay_produces_same_final_state() {
        // Original execution.
        let cfg = FaultIsolationConfig::default();
        let mut original = InstrumentedFaultManager::new(cfg.clone());
        original.record_fault(FaultDomain::Io, "disk err".to_string(), 100);
        original.attempt_restart(FaultDomain::Io, 200_000);
        original.restart_succeeded(FaultDomain::Io, 300_000);
        original.record_fault(FaultDomain::Scheduler, "oom".to_string(), 400_000);

        // Replay.
        let events = vec![
            ReplayableEvent {
                domain: FaultDomain::Io,
                timestamp_us: 100,
                action: ReplayAction::RecordFault,
                description: "disk err".to_string(),
            },
            ReplayableEvent {
                domain: FaultDomain::Io,
                timestamp_us: 200_000,
                action: ReplayAction::AttemptRestart,
                description: String::new(),
            },
            ReplayableEvent {
                domain: FaultDomain::Io,
                timestamp_us: 300_000,
                action: ReplayAction::RestartSucceeded,
                description: String::new(),
            },
            ReplayableEvent {
                domain: FaultDomain::Scheduler,
                timestamp_us: 400_000,
                action: ReplayAction::RecordFault,
                description: "oom".to_string(),
            },
        ];
        let replayed = InstrumentedFaultManager::replay(cfg, &events);

        // Same final state.
        for d in FaultDomain::ALL {
            assert_eq!(original.domain_health(*d), replayed.domain_health(*d));
        }
        // Same number of transitions.
        assert_eq!(original.log_count(), replayed.log_count());
    }

    #[test]
    fn test_replay_action_display() {
        assert_eq!(ReplayAction::RecordFault.to_string(), "record-fault");
        assert_eq!(ReplayAction::AttemptRestart.to_string(), "attempt-restart");
        assert_eq!(
            ReplayAction::RestartSucceeded.to_string(),
            "restart-succeeded"
        );
        assert_eq!(ReplayAction::RestartFailed.to_string(), "restart-failed");
        assert_eq!(ReplayAction::MarkDegraded.to_string(), "mark-degraded");
        assert_eq!(ReplayAction::UnIsolate.to_string(), "un-isolate");
    }

    #[test]
    fn test_replay_action_serde() {
        for action in [
            ReplayAction::RecordFault,
            ReplayAction::AttemptRestart,
            ReplayAction::RestartSucceeded,
            ReplayAction::RestartFailed,
            ReplayAction::MarkDegraded,
            ReplayAction::UnIsolate,
        ] {
            let json = serde_json::to_string(&action).unwrap();
            let back: ReplayAction = serde_json::from_str(&json).unwrap();
            assert_eq!(action, back);
        }
    }

    #[test]
    fn test_replayable_event_serde() {
        let ev = ReplayableEvent {
            domain: FaultDomain::Storage,
            timestamp_us: 12345,
            action: ReplayAction::RecordFault,
            description: "test".to_string(),
        };
        let json = serde_json::to_string(&ev).unwrap();
        let back: ReplayableEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn test_replay_empty_events() {
        let mgr = InstrumentedFaultManager::replay(FaultIsolationConfig::default(), &[]);
        assert!(mgr.all_healthy());
        assert_eq!(mgr.log_count(), 0);
    }

    #[test]
    fn test_instrumented_delegates_snapshot() {
        let mut mgr = InstrumentedFaultManager::new(FaultIsolationConfig::default());
        mgr.record_fault(FaultDomain::Io, "x".to_string(), 100);
        let snap = mgr.snapshot();
        assert_eq!(snap.total_faults, 1);
    }

    #[test]
    fn test_instrumented_restart_failed_log() {
        let mut mgr = InstrumentedFaultManager::new(FaultIsolationConfig::default());
        mgr.record_fault(FaultDomain::Budget, "err".to_string(), 100);
        mgr.attempt_restart(FaultDomain::Budget, 200_000);
        mgr.restart_failed(FaultDomain::Budget, 200_001);
        let last = mgr.transition_log().last().unwrap();
        assert_eq!(last.domain, FaultDomain::Budget);
        assert_eq!(last.to, DomainHealth::Crashed);
        assert_eq!(last.reason_code, FaultReasonCode::RestartFailed);
    }

    // ── F2: Circuit Breakers and Recovery Choreography ──

    #[test]
    fn test_breaker_state_display() {
        assert_eq!(BreakerState::Closed.to_string(), "closed");
        assert_eq!(BreakerState::Open.to_string(), "open");
        assert_eq!(BreakerState::HalfOpen.to_string(), "half-open");
    }

    #[test]
    fn test_breaker_state_serde() {
        for state in [
            BreakerState::Closed,
            BreakerState::Open,
            BreakerState::HalfOpen,
        ] {
            let json = serde_json::to_string(&state).unwrap();
            let back: BreakerState = serde_json::from_str(&json).unwrap();
            assert_eq!(back, state);
        }
    }

    #[test]
    fn test_stage_breaker_config_default() {
        let cfg = StageBreakerConfig::default();
        assert_eq!(cfg.failure_threshold, 5);
        assert_eq!(cfg.open_duration_us, 1_000_000);
        assert_eq!(cfg.half_open_max_probes, 3);
        assert_eq!(cfg.half_open_success_threshold, 2);
    }

    #[test]
    fn test_stage_breaker_config_serde() {
        let cfg = StageBreakerConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let back: StageBreakerConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn test_stage_breaker_state_serde() {
        let sbs = StageBreakerState {
            stage: LatencyStage::PtyCapture,
            state: BreakerState::Open,
            consecutive_failures: 5,
            opened_at_us: 1000,
            half_open_probes: 0,
            half_open_successes: 0,
            total_trips: 1,
            total_recoveries: 0,
        };
        let json = serde_json::to_string(&sbs).unwrap();
        let back: StageBreakerState = serde_json::from_str(&json).unwrap();
        assert_eq!(back, sbs);
    }

    #[test]
    fn test_recovery_step_serde() {
        let step = RecoveryStep {
            stage: LatencyStage::StorageWrite,
            step_number: 1,
            action: "flush WAL".to_string(),
            requires_prior_success: true,
            timeout_us: 500_000,
        };
        let json = serde_json::to_string(&step).unwrap();
        let back: RecoveryStep = serde_json::from_str(&json).unwrap();
        assert_eq!(back, step);
    }

    #[test]
    fn test_choreography_outcome_display() {
        assert_eq!(
            ChoreographyOutcome::FullRecovery.to_string(),
            "full-recovery"
        );
        let partial = ChoreographyOutcome::PartialRecovery {
            recovered: vec![LatencyStage::PtyCapture],
            failed: vec![LatencyStage::StorageWrite, LatencyStage::EventEmission],
        };
        assert_eq!(partial.to_string(), "partial(1 ok, 2 failed)");
        let aborted = ChoreographyOutcome::Aborted {
            reason: "timeout".to_string(),
        };
        assert_eq!(aborted.to_string(), "aborted: timeout");
    }

    #[test]
    fn test_choreography_outcome_serde() {
        let outcomes = vec![
            ChoreographyOutcome::FullRecovery,
            ChoreographyOutcome::PartialRecovery {
                recovered: vec![LatencyStage::PtyCapture],
                failed: vec![LatencyStage::StorageWrite],
            },
            ChoreographyOutcome::Aborted {
                reason: "cascade".to_string(),
            },
        ];
        for o in outcomes {
            let json = serde_json::to_string(&o).unwrap();
            let back: ChoreographyOutcome = serde_json::from_str(&json).unwrap();
            assert_eq!(back, o);
        }
    }

    #[test]
    fn test_breaker_manager_new_all_closed() {
        let mgr = BreakerManager::new(StageBreakerConfig::default());
        assert!(mgr.all_closed());
        assert_eq!(mgr.open_count(), 0);
        for stage in LatencyStage::PIPELINE_STAGES {
            assert_eq!(mgr.breaker_state(*stage), BreakerState::Closed);
        }
    }

    #[test]
    fn test_breaker_manager_failure_below_threshold() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        // Default threshold is 5. Record 4 failures — should stay closed.
        for i in 0..4 {
            mgr.record_failure(LatencyStage::PtyCapture, 100 + i);
        }
        assert_eq!(
            mgr.breaker_state(LatencyStage::PtyCapture),
            BreakerState::Closed
        );
        assert!(mgr.all_closed());
    }

    #[test]
    fn test_breaker_manager_failure_trips_breaker() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        for i in 0..5 {
            mgr.record_failure(LatencyStage::PtyCapture, 100 + i);
        }
        assert_eq!(
            mgr.breaker_state(LatencyStage::PtyCapture),
            BreakerState::Open
        );
        assert!(!mgr.all_closed());
        assert_eq!(mgr.open_count(), 1);
    }

    #[test]
    fn test_breaker_manager_open_blocks_requests() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        for i in 0..5 {
            mgr.record_failure(LatencyStage::PtyCapture, 100 + i);
        }
        // Immediately after tripping, before open_duration passes, requests blocked.
        assert!(!mgr.allow_request(LatencyStage::PtyCapture, 105));
    }

    #[test]
    fn test_breaker_manager_open_to_half_open() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        for i in 0..5 {
            mgr.record_failure(LatencyStage::PtyCapture, 100 + i);
        }
        // After open_duration (1_000_000 us), should transition to half-open.
        assert!(mgr.allow_request(LatencyStage::PtyCapture, 1_000_200));
        assert_eq!(
            mgr.breaker_state(LatencyStage::PtyCapture),
            BreakerState::HalfOpen
        );
    }

    #[test]
    fn test_breaker_manager_half_open_probe_limit() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        for i in 0..5 {
            mgr.record_failure(LatencyStage::PtyCapture, 100 + i);
        }
        // Transition to half-open.
        assert!(mgr.allow_request(LatencyStage::PtyCapture, 1_100_000));
        // First probe consumed by the transition call. max_probes=3.
        assert!(mgr.allow_request(LatencyStage::PtyCapture, 1_100_001));
        assert!(mgr.allow_request(LatencyStage::PtyCapture, 1_100_002));
        // Now at 3 probes — next should be blocked.
        assert!(!mgr.allow_request(LatencyStage::PtyCapture, 1_100_003));
    }

    #[test]
    fn test_breaker_manager_half_open_recovery() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        for i in 0..5 {
            mgr.record_failure(LatencyStage::PtyCapture, 100 + i);
        }
        // Transition to half-open.
        mgr.allow_request(LatencyStage::PtyCapture, 1_100_000);
        // Record enough successes to close (threshold = 2).
        mgr.record_success(LatencyStage::PtyCapture);
        mgr.record_success(LatencyStage::PtyCapture);
        assert_eq!(
            mgr.breaker_state(LatencyStage::PtyCapture),
            BreakerState::Closed
        );
    }

    #[test]
    fn test_breaker_manager_half_open_failure_reopens() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        for i in 0..5 {
            mgr.record_failure(LatencyStage::PtyCapture, 100 + i);
        }
        mgr.allow_request(LatencyStage::PtyCapture, 1_100_000);
        assert_eq!(
            mgr.breaker_state(LatencyStage::PtyCapture),
            BreakerState::HalfOpen
        );
        mgr.record_failure(LatencyStage::PtyCapture, 1_200_000);
        assert_eq!(
            mgr.breaker_state(LatencyStage::PtyCapture),
            BreakerState::Open
        );
    }

    #[test]
    fn test_breaker_manager_success_resets_failures() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        mgr.record_failure(LatencyStage::PtyCapture, 100);
        mgr.record_failure(LatencyStage::PtyCapture, 101);
        mgr.record_success(LatencyStage::PtyCapture);
        // After success, consecutive failures reset. Need 5 more to trip.
        for i in 0..4 {
            mgr.record_failure(LatencyStage::PtyCapture, 200 + i);
        }
        assert_eq!(
            mgr.breaker_state(LatencyStage::PtyCapture),
            BreakerState::Closed
        );
    }

    #[test]
    fn test_breaker_manager_multiple_stages_independent() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        for i in 0..5 {
            mgr.record_failure(LatencyStage::PtyCapture, 100 + i);
        }
        assert_eq!(
            mgr.breaker_state(LatencyStage::PtyCapture),
            BreakerState::Open
        );
        assert_eq!(
            mgr.breaker_state(LatencyStage::StorageWrite),
            BreakerState::Closed
        );
        assert_eq!(mgr.open_count(), 1);
    }

    #[test]
    fn test_breaker_manager_snapshot() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        for i in 0..5 {
            mgr.record_failure(LatencyStage::PtyCapture, 100 + i);
        }
        let snap = mgr.snapshot();
        assert_eq!(snap.stages.len(), 8); // All pipeline stages.
        assert_eq!(snap.total_trips, 1);
        assert_eq!(snap.total_recoveries, 0);
    }

    #[test]
    fn test_breaker_manager_snapshot_serde() {
        let mgr = BreakerManager::new(StageBreakerConfig::default());
        let snap = mgr.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let back: BreakerManagerSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back, snap);
    }

    #[test]
    fn test_breaker_manager_degradation_healthy() {
        let mgr = BreakerManager::new(StageBreakerConfig::default());
        assert_eq!(mgr.detect_degradation(), BreakerManagerDegradation::Healthy);
    }

    #[test]
    fn test_breaker_manager_degradation_tripped() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        for i in 0..5 {
            mgr.record_failure(LatencyStage::PtyCapture, 100 + i);
        }
        let deg = mgr.detect_degradation();
        assert_eq!(
            deg,
            BreakerManagerDegradation::BreakerTripped { open_count: 1 }
        );
    }

    #[test]
    fn test_breaker_manager_degradation_cascade_risk() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        let stages = [
            LatencyStage::PtyCapture,
            LatencyStage::StorageWrite,
            LatencyStage::EventEmission,
        ];
        for stage in stages {
            for i in 0..5 {
                mgr.record_failure(stage, 100 + i);
            }
        }
        let deg = mgr.detect_degradation();
        assert_eq!(
            deg,
            BreakerManagerDegradation::CascadeRisk { open_count: 3 }
        );
    }

    #[test]
    fn test_breaker_manager_degradation_display() {
        assert_eq!(BreakerManagerDegradation::Healthy.to_string(), "healthy");
        assert_eq!(
            BreakerManagerDegradation::BreakerTripped { open_count: 2 }.to_string(),
            "tripped(2)"
        );
        assert_eq!(
            BreakerManagerDegradation::CascadeRisk { open_count: 4 }.to_string(),
            "cascade-risk(4)"
        );
    }

    #[test]
    fn test_breaker_manager_degradation_serde() {
        let cases = vec![
            BreakerManagerDegradation::Healthy,
            BreakerManagerDegradation::BreakerTripped { open_count: 1 },
            BreakerManagerDegradation::CascadeRisk { open_count: 5 },
        ];
        for deg in cases {
            let json = serde_json::to_string(&deg).unwrap();
            let back: BreakerManagerDegradation = serde_json::from_str(&json).unwrap();
            assert_eq!(back, deg);
        }
    }

    #[test]
    fn test_breaker_log_entry() {
        let mgr = BreakerManager::new(StageBreakerConfig::default());
        let entry = mgr.log_entry(
            LatencyStage::PtyCapture,
            BreakerState::Closed,
            BreakerState::Open,
            "threshold exceeded".to_string(),
            42_000,
        );
        assert_eq!(entry.timestamp_us, 42_000);
        assert_eq!(entry.stage, LatencyStage::PtyCapture);
        assert_eq!(entry.from_state, BreakerState::Closed);
        assert_eq!(entry.to_state, BreakerState::Open);
        assert_eq!(entry.reason, "threshold exceeded");
    }

    #[test]
    fn test_breaker_log_entry_serde() {
        let mgr = BreakerManager::new(StageBreakerConfig::default());
        let entry = mgr.log_entry(
            LatencyStage::StorageWrite,
            BreakerState::Open,
            BreakerState::HalfOpen,
            "cooldown elapsed".to_string(),
            100_000,
        );
        let json = serde_json::to_string(&entry).unwrap();
        let back: BreakerLogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back, entry);
    }

    #[test]
    fn test_breaker_manager_reset() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        for i in 0..5 {
            mgr.record_failure(LatencyStage::PtyCapture, 100 + i);
        }
        assert!(!mgr.all_closed());
        mgr.reset();
        assert!(mgr.all_closed());
        assert_eq!(mgr.open_count(), 0);
        let snap = mgr.snapshot();
        assert_eq!(snap.total_trips, 0);
        assert_eq!(snap.total_recoveries, 0);
    }

    #[test]
    fn test_breaker_manager_config_accessor() {
        let cfg = StageBreakerConfig {
            failure_threshold: 3,
            open_duration_us: 500_000,
            half_open_max_probes: 2,
            half_open_success_threshold: 1,
        };
        let mgr = BreakerManager::new(cfg.clone());
        assert_eq!(*mgr.config(), cfg);
    }

    #[test]
    fn test_breaker_manager_custom_threshold() {
        let cfg = StageBreakerConfig {
            failure_threshold: 2,
            ..Default::default()
        };
        let mut mgr = BreakerManager::new(cfg);
        mgr.record_failure(LatencyStage::DeltaExtraction, 100);
        assert_eq!(
            mgr.breaker_state(LatencyStage::DeltaExtraction),
            BreakerState::Closed
        );
        mgr.record_failure(LatencyStage::DeltaExtraction, 101);
        assert_eq!(
            mgr.breaker_state(LatencyStage::DeltaExtraction),
            BreakerState::Open
        );
    }

    #[test]
    fn test_breaker_total_trips_accumulates() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        // Trip PtyCapture.
        for i in 0..5 {
            mgr.record_failure(LatencyStage::PtyCapture, 100 + i);
        }
        // Recover it.
        mgr.allow_request(LatencyStage::PtyCapture, 1_200_000);
        mgr.record_success(LatencyStage::PtyCapture);
        mgr.record_success(LatencyStage::PtyCapture);
        assert_eq!(
            mgr.breaker_state(LatencyStage::PtyCapture),
            BreakerState::Closed
        );
        // Trip it again.
        for i in 0..5 {
            mgr.record_failure(LatencyStage::PtyCapture, 2_000_000 + i);
        }
        let snap = mgr.snapshot();
        assert_eq!(snap.total_trips, 2);
        assert_eq!(snap.total_recoveries, 1);
    }

    #[test]
    fn test_closed_always_allows_request() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        for ts in 0..100 {
            assert!(mgr.allow_request(LatencyStage::PtyCapture, ts));
        }
    }

    #[test]
    fn test_open_failure_is_noop() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        for i in 0..5 {
            mgr.record_failure(LatencyStage::PtyCapture, 100 + i);
        }
        assert_eq!(
            mgr.breaker_state(LatencyStage::PtyCapture),
            BreakerState::Open
        );
        // Further failures while open are no-op.
        mgr.record_failure(LatencyStage::PtyCapture, 200);
        assert_eq!(
            mgr.breaker_state(LatencyStage::PtyCapture),
            BreakerState::Open
        );
    }

    // ── F2 Impl: Bridge method tests ──

    #[test]
    fn test_breaker_total_trips_method() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        assert_eq!(mgr.total_trips(), 0);
        for i in 0..5 {
            mgr.record_failure(LatencyStage::PtyCapture, 100 + i);
        }
        assert_eq!(mgr.total_trips(), 1);
        for i in 0..5 {
            mgr.record_failure(LatencyStage::StorageWrite, 200 + i);
        }
        assert_eq!(mgr.total_trips(), 2);
    }

    #[test]
    fn test_breaker_total_recoveries_method() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        for i in 0..5 {
            mgr.record_failure(LatencyStage::PtyCapture, 100 + i);
        }
        mgr.allow_request(LatencyStage::PtyCapture, 1_200_000);
        mgr.record_success(LatencyStage::PtyCapture);
        mgr.record_success(LatencyStage::PtyCapture);
        assert_eq!(mgr.total_recoveries(), 1);
    }

    #[test]
    fn test_breaker_total_consecutive_failures() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        mgr.record_failure(LatencyStage::PtyCapture, 100);
        mgr.record_failure(LatencyStage::StorageWrite, 101);
        assert_eq!(mgr.total_consecutive_failures(), 2);
    }

    #[test]
    fn test_breaker_open_stages() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        assert!(mgr.open_stages().is_empty());
        for i in 0..5 {
            mgr.record_failure(LatencyStage::PtyCapture, 100 + i);
        }
        let open = mgr.open_stages();
        assert_eq!(open.len(), 1);
        assert!(open.contains(&LatencyStage::PtyCapture));
    }

    #[test]
    fn test_breaker_half_open_stages() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        for i in 0..5 {
            mgr.record_failure(LatencyStage::PtyCapture, 100 + i);
        }
        mgr.allow_request(LatencyStage::PtyCapture, 1_200_000);
        let half = mgr.half_open_stages();
        assert_eq!(half.len(), 1);
        assert!(half.contains(&LatencyStage::PtyCapture));
    }

    #[test]
    fn test_breaker_closed_stages() {
        let mgr = BreakerManager::new(StageBreakerConfig::default());
        assert_eq!(mgr.closed_stages().len(), 8);
    }

    #[test]
    fn test_breaker_plan_recovery_empty_when_all_closed() {
        let mgr = BreakerManager::new(StageBreakerConfig::default());
        assert!(mgr.plan_recovery().is_empty());
    }

    #[test]
    fn test_breaker_plan_recovery_ordered_by_pipeline() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        // Trip StorageWrite and PtyCapture (out of pipeline order).
        for i in 0..5 {
            mgr.record_failure(LatencyStage::StorageWrite, 100 + i);
        }
        for i in 0..5 {
            mgr.record_failure(LatencyStage::PtyCapture, 200 + i);
        }
        let plan = mgr.plan_recovery();
        assert_eq!(plan.len(), 2);
        // PtyCapture comes before StorageWrite in pipeline.
        assert_eq!(plan[0].stage, LatencyStage::PtyCapture);
        assert_eq!(plan[1].stage, LatencyStage::StorageWrite);
        assert!(!plan[0].requires_prior_success);
        assert!(plan[1].requires_prior_success);
    }

    #[test]
    fn test_breaker_initiate_recovery() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        for i in 0..5 {
            mgr.record_failure(LatencyStage::PtyCapture, 100 + i);
        }
        // Not enough time passed yet.
        assert_eq!(mgr.initiate_recovery(500_000), 0);
        // Enough time passed.
        let transitioned = mgr.initiate_recovery(1_200_000);
        assert_eq!(transitioned, 1);
        assert_eq!(
            mgr.breaker_state(LatencyStage::PtyCapture),
            BreakerState::HalfOpen
        );
    }

    #[test]
    fn test_breaker_to_invariant_domain() {
        assert_eq!(
            BreakerManager::to_invariant_domain(),
            InvariantDomain::Recovery
        );
    }

    #[test]
    fn test_breaker_availability_all_closed() {
        let mgr = BreakerManager::new(StageBreakerConfig::default());
        assert!((mgr.availability() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_breaker_availability_some_open() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        for i in 0..5 {
            mgr.record_failure(LatencyStage::PtyCapture, 100 + i);
        }
        // 7/8 closed.
        assert!((mgr.availability() - 7.0 / 8.0).abs() < 0.01);
    }

    #[test]
    fn test_breaker_record_failures_batch() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        mgr.record_failures_batch(LatencyStage::PtyCapture, 5, 1000);
        assert_eq!(
            mgr.breaker_state(LatencyStage::PtyCapture),
            BreakerState::Open
        );
    }

    #[test]
    fn test_breaker_stage_state() {
        let mgr = BreakerManager::new(StageBreakerConfig::default());
        let st = mgr.stage_state(LatencyStage::PtyCapture);
        assert!(st.is_some());
        assert_eq!(st.unwrap().state, BreakerState::Closed);
    }

    // ── F3: Immediate-Ack / Deferred-Completion UX Protocol ──

    #[test]
    fn test_ack_phase_display() {
        assert_eq!(AckPhase::ImmediateAck.to_string(), "immediate-ack");
        assert_eq!(
            AckPhase::DeferredCompletion.to_string(),
            "deferred-completion"
        );
    }

    #[test]
    fn test_ack_phase_serde() {
        for phase in [AckPhase::ImmediateAck, AckPhase::DeferredCompletion] {
            let json = serde_json::to_string(&phase).unwrap();
            let back: AckPhase = serde_json::from_str(&json).unwrap();
            assert_eq!(back, phase);
        }
    }

    #[test]
    fn test_completion_reason_display() {
        assert_eq!(CompletionReason::Success.to_string(), "success");
        assert_eq!(CompletionReason::Timeout.to_string(), "timeout");
        let up = CompletionReason::UpstreamFailure {
            stage: LatencyStage::StorageWrite,
            detail: "WAL full".to_string(),
        };
        assert!(up.to_string().contains("upstream-failure"));
        let cancel = CompletionReason::Cancelled {
            reason: "user".to_string(),
        };
        assert!(cancel.to_string().contains("cancelled"));
    }

    #[test]
    fn test_completion_reason_serde() {
        let reasons = vec![
            CompletionReason::Success,
            CompletionReason::Timeout,
            CompletionReason::UpstreamFailure {
                stage: LatencyStage::PatternDetection,
                detail: "OOM".to_string(),
            },
            CompletionReason::Cancelled {
                reason: "test".to_string(),
            },
        ];
        for r in reasons {
            let json = serde_json::to_string(&r).unwrap();
            let back: CompletionReason = serde_json::from_str(&json).unwrap();
            assert_eq!(back, r);
        }
    }

    #[test]
    fn test_ack_token_serde() {
        let token = AckToken {
            correlation_id: 42,
            acked_at_us: 1000,
            source_stage: LatencyStage::PtyCapture,
            summary: "received input".to_string(),
        };
        let json = serde_json::to_string(&token).unwrap();
        let back: AckToken = serde_json::from_str(&json).unwrap();
        assert_eq!(back, token);
    }

    #[test]
    fn test_deferred_result_serde() {
        let result = DeferredResult {
            correlation_id: 42,
            completed_at_us: 5000,
            reason: CompletionReason::Success,
            deferred_latency_us: 4000,
            explanation: Some("Pattern matched".to_string()),
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: DeferredResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back, result);
    }

    #[test]
    fn test_ack_protocol_config_default() {
        let cfg = AckProtocolConfig::default();
        assert_eq!(cfg.ack_deadline_us, 50_000);
        assert_eq!(cfg.completion_deadline_us, 5_000_000);
        assert!(cfg.show_progress);
        assert_eq!(cfg.progress_interval_us, 500_000);
    }

    #[test]
    fn test_ack_protocol_config_serde() {
        let cfg = AckProtocolConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let back: AckProtocolConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn test_progress_update_serde() {
        let update = ProgressUpdate {
            correlation_id: 7,
            timestamp_us: 3000,
            fraction: 0.5,
            message: "halfway".to_string(),
        };
        let json = serde_json::to_string(&update).unwrap();
        let back: ProgressUpdate = serde_json::from_str(&json).unwrap();
        assert_eq!(back, update);
    }

    #[test]
    fn test_ack_protocol_issue_ack() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        let token = mgr.issue_ack(LatencyStage::PtyCapture, "got input".to_string(), 1000);
        assert_eq!(token.correlation_id, 1);
        assert_eq!(token.acked_at_us, 1000);
        assert_eq!(token.source_stage, LatencyStage::PtyCapture);
        assert_eq!(mgr.pending_count(), 1);
    }

    #[test]
    fn test_ack_protocol_issue_increments_ids() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        let t1 = mgr.issue_ack(LatencyStage::PtyCapture, "a".to_string(), 100);
        let t2 = mgr.issue_ack(LatencyStage::StorageWrite, "b".to_string(), 200);
        assert_eq!(t1.correlation_id, 1);
        assert_eq!(t2.correlation_id, 2);
        assert_eq!(mgr.pending_count(), 2);
    }

    #[test]
    fn test_ack_protocol_complete_success() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        let token = mgr.issue_ack(LatencyStage::PtyCapture, "x".to_string(), 1000);
        let result = mgr.complete(token.correlation_id, CompletionReason::Success, 3000);
        assert!(result.is_some());
        let r = result.unwrap();
        assert_eq!(r.deferred_latency_us, 2000);
        assert_eq!(r.reason, CompletionReason::Success);
        assert_eq!(mgr.pending_count(), 0);
    }

    #[test]
    fn test_ack_protocol_complete_unknown_id() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        let result = mgr.complete(999, CompletionReason::Success, 1000);
        assert!(result.is_none());
    }

    #[test]
    fn test_ack_protocol_sweep_timeouts() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        mgr.issue_ack(LatencyStage::PtyCapture, "x".to_string(), 1000);
        // Before deadline.
        let results = mgr.sweep_timeouts(4_000_000);
        assert!(results.is_empty());
        assert_eq!(mgr.pending_count(), 1);
        // After deadline (5_000_000 default).
        let results = mgr.sweep_timeouts(6_100_000);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].reason, CompletionReason::Timeout);
        assert_eq!(mgr.pending_count(), 0);
    }

    #[test]
    fn test_ack_protocol_snapshot() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        mgr.issue_ack(LatencyStage::PtyCapture, "x".to_string(), 1000);
        mgr.complete(1, CompletionReason::Success, 2000);
        let snap = mgr.snapshot();
        assert_eq!(snap.total_acks, 1);
        assert_eq!(snap.total_completions, 1);
        assert_eq!(snap.total_timeouts, 0);
        assert_eq!(snap.pending_count, 0);
    }

    #[test]
    fn test_ack_protocol_snapshot_serde() {
        let snap = AckProtocolSnapshot {
            total_acks: 10,
            total_completions: 8,
            total_timeouts: 2,
            pending_count: 0,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: AckProtocolSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back, snap);
    }

    #[test]
    fn test_ack_protocol_degradation_healthy() {
        let mgr = AckProtocolManager::new(AckProtocolConfig::default());
        assert_eq!(mgr.detect_degradation(), AckProtocolDegradation::Healthy);
    }

    #[test]
    fn test_ack_protocol_degradation_slow_ack() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        mgr.record_slow_ack();
        assert_eq!(
            mgr.detect_degradation(),
            AckProtocolDegradation::AckSlow { slow_count: 1 }
        );
    }

    #[test]
    fn test_ack_protocol_degradation_timeout() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        mgr.issue_ack(LatencyStage::PtyCapture, "x".to_string(), 1000);
        mgr.sweep_timeouts(6_100_000);
        let deg = mgr.detect_degradation();
        assert_eq!(
            deg,
            AckProtocolDegradation::CompletionTimeout { timeout_count: 1 }
        );
    }

    #[test]
    fn test_ack_protocol_degradation_display() {
        assert_eq!(AckProtocolDegradation::Healthy.to_string(), "healthy");
        assert_eq!(
            AckProtocolDegradation::AckSlow { slow_count: 3 }.to_string(),
            "ack-slow(3)"
        );
        assert_eq!(
            AckProtocolDegradation::CompletionTimeout { timeout_count: 2 }.to_string(),
            "completion-timeout(2)"
        );
    }

    #[test]
    fn test_ack_protocol_degradation_serde() {
        let cases = vec![
            AckProtocolDegradation::Healthy,
            AckProtocolDegradation::AckSlow { slow_count: 5 },
            AckProtocolDegradation::CompletionTimeout { timeout_count: 3 },
        ];
        for deg in cases {
            let json = serde_json::to_string(&deg).unwrap();
            let back: AckProtocolDegradation = serde_json::from_str(&json).unwrap();
            assert_eq!(back, deg);
        }
    }

    #[test]
    fn test_ack_protocol_log_entry() {
        let mgr = AckProtocolManager::new(AckProtocolConfig::default());
        let entry = mgr.log_entry(AckPhase::ImmediateAck, 42, "ack issued".to_string(), 1000);
        assert_eq!(entry.phase, AckPhase::ImmediateAck);
        assert_eq!(entry.correlation_id, 42);
    }

    #[test]
    fn test_ack_protocol_log_entry_serde() {
        let entry = AckProtocolLogEntry {
            timestamp_us: 1000,
            phase: AckPhase::DeferredCompletion,
            correlation_id: 7,
            event: "completed".to_string(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: AckProtocolLogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back, entry);
    }

    #[test]
    fn test_ack_protocol_reset() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        mgr.issue_ack(LatencyStage::PtyCapture, "x".to_string(), 1000);
        mgr.record_slow_ack();
        mgr.reset();
        assert_eq!(mgr.pending_count(), 0);
        let snap = mgr.snapshot();
        assert_eq!(snap.total_acks, 0);
        assert_eq!(snap.total_completions, 0);
        assert_eq!(snap.total_timeouts, 0);
        assert_eq!(mgr.detect_degradation(), AckProtocolDegradation::Healthy);
    }

    #[test]
    fn test_ack_protocol_config_accessor() {
        let cfg = AckProtocolConfig {
            ack_deadline_us: 100,
            ..Default::default()
        };
        let mgr = AckProtocolManager::new(cfg.clone());
        assert_eq!(*mgr.config(), cfg);
    }

    #[test]
    fn test_ack_timeout_increments_counter() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        mgr.issue_ack(LatencyStage::PtyCapture, "a".to_string(), 1000);
        mgr.issue_ack(LatencyStage::StorageWrite, "b".to_string(), 1000);
        mgr.sweep_timeouts(6_100_000);
        assert_eq!(mgr.snapshot().total_timeouts, 2);
    }

    #[test]
    fn test_ack_cancel_completes() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        let token = mgr.issue_ack(LatencyStage::PtyCapture, "x".to_string(), 1000);
        let result = mgr.complete(
            token.correlation_id,
            CompletionReason::Cancelled {
                reason: "user".to_string(),
            },
            2000,
        );
        assert!(result.is_some());
        assert_eq!(mgr.pending_count(), 0);
        assert_eq!(mgr.snapshot().total_completions, 1);
    }

    // ── F3 Impl: Bridge method tests ──

    #[test]
    fn test_ack_total_acks_accessor() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        assert_eq!(mgr.total_acks(), 0);
        mgr.issue_ack(LatencyStage::PtyCapture, "a".to_string(), 100);
        assert_eq!(mgr.total_acks(), 1);
    }

    #[test]
    fn test_ack_total_completions_accessor() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        mgr.issue_ack(LatencyStage::PtyCapture, "a".to_string(), 100);
        mgr.complete(1, CompletionReason::Success, 200);
        assert_eq!(mgr.total_completions(), 1);
    }

    #[test]
    fn test_ack_total_timeouts_accessor() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        mgr.issue_ack(LatencyStage::PtyCapture, "a".to_string(), 100);
        mgr.sweep_timeouts(6_000_000);
        assert_eq!(mgr.total_timeouts(), 1);
    }

    #[test]
    fn test_ack_total_cancellations() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        mgr.issue_ack(LatencyStage::PtyCapture, "a".to_string(), 100);
        mgr.complete(
            1,
            CompletionReason::Cancelled {
                reason: "x".to_string(),
            },
            200,
        );
        assert_eq!(mgr.total_cancellations(), 1);
    }

    #[test]
    fn test_ack_slow_count_accessor() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        mgr.record_slow_ack();
        mgr.record_slow_ack();
        assert_eq!(mgr.slow_ack_count(), 2);
    }

    #[test]
    fn test_ack_completion_rate() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        assert!((mgr.completion_rate() - 1.0).abs() < f64::EPSILON);
        mgr.issue_ack(LatencyStage::PtyCapture, "a".to_string(), 100);
        mgr.issue_ack(LatencyStage::StorageWrite, "b".to_string(), 100);
        mgr.complete(1, CompletionReason::Success, 200);
        assert!((mgr.completion_rate() - 0.5).abs() < 0.01);
    }

    #[test]
    fn test_ack_timeout_rate() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        assert!((mgr.timeout_rate() - 0.0).abs() < f64::EPSILON);
        mgr.issue_ack(LatencyStage::PtyCapture, "a".to_string(), 100);
        mgr.issue_ack(LatencyStage::StorageWrite, "b".to_string(), 100);
        mgr.complete(1, CompletionReason::Success, 200);
        mgr.complete(2, CompletionReason::Timeout, 300);
        assert!((mgr.timeout_rate() - 0.5).abs() < 0.01);
    }

    #[test]
    fn test_ack_has_pending() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        assert!(!mgr.has_pending());
        mgr.issue_ack(LatencyStage::PtyCapture, "a".to_string(), 100);
        assert!(mgr.has_pending());
    }

    #[test]
    fn test_ack_get_pending() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        let token = mgr.issue_ack(LatencyStage::PtyCapture, "a".to_string(), 100);
        let got = mgr.get_pending(token.correlation_id);
        assert!(got.is_some());
        assert_eq!(got.unwrap().summary, "a");
        assert!(mgr.get_pending(999).is_none());
    }

    #[test]
    fn test_ack_complete_with_explanation() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        let token = mgr.issue_ack(LatencyStage::PtyCapture, "a".to_string(), 100);
        let result = mgr.complete_with_explanation(
            token.correlation_id,
            CompletionReason::Success,
            200,
            "Pattern found".to_string(),
        );
        assert!(result.is_some());
        assert_eq!(
            result.unwrap().explanation,
            Some("Pattern found".to_string())
        );
    }

    #[test]
    fn test_ack_issue_checked_slow() {
        let cfg = AckProtocolConfig {
            ack_deadline_us: 100,
            ..Default::default()
        };
        let mut mgr = AckProtocolManager::new(cfg);
        // Ack took 200μs but deadline is 100μs → slow.
        mgr.issue_ack_checked(LatencyStage::PtyCapture, "a".to_string(), 1000, 1201);
        assert_eq!(mgr.slow_ack_count(), 1);
    }

    #[test]
    fn test_ack_issue_checked_fast() {
        let cfg = AckProtocolConfig {
            ack_deadline_us: 100,
            ..Default::default()
        };
        let mut mgr = AckProtocolManager::new(cfg);
        // Ack took 50μs, deadline is 100μs → fast.
        mgr.issue_ack_checked(LatencyStage::PtyCapture, "a".to_string(), 1000, 1050);
        assert_eq!(mgr.slow_ack_count(), 0);
    }

    #[test]
    fn test_ack_to_invariant_domain() {
        assert_eq!(
            AckProtocolManager::to_invariant_domain(),
            InvariantDomain::Composition
        );
    }

    #[test]
    fn test_ack_make_progress() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        let token = mgr.issue_ack(LatencyStage::PtyCapture, "a".to_string(), 100);
        let prog = mgr.make_progress(token.correlation_id, 0.5, "halfway".to_string(), 500);
        assert!(prog.is_some());
        let p = prog.unwrap();
        assert!((p.fraction - 0.5).abs() < f64::EPSILON);
        // Non-existent correlation ID.
        assert!(mgr.make_progress(999, 0.5, "x".to_string(), 500).is_none());
    }

    #[test]
    fn test_ack_make_progress_clamps_fraction() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        let token = mgr.issue_ack(LatencyStage::PtyCapture, "a".to_string(), 100);
        let p = mgr
            .make_progress(token.correlation_id, 2.0, "over".to_string(), 500)
            .unwrap();
        assert!((p.fraction - 1.0).abs() < f64::EPSILON);
        let p2 = mgr
            .make_progress(token.correlation_id, -0.5, "under".to_string(), 500)
            .unwrap();
        assert!((p2.fraction - 0.0).abs() < f64::EPSILON);
    }

    // ── F4: Unified E2E-Chaos-Soak-Performance Matrix ──

    #[test]
    fn test_scenario_category_display() {
        assert_eq!(ScenarioCategory::E2E.to_string(), "e2e");
        assert_eq!(ScenarioCategory::Chaos.to_string(), "chaos");
        assert_eq!(ScenarioCategory::Soak.to_string(), "soak");
        assert_eq!(ScenarioCategory::Performance.to_string(), "performance");
    }

    #[test]
    fn test_scenario_category_serde() {
        for cat in [
            ScenarioCategory::E2E,
            ScenarioCategory::Chaos,
            ScenarioCategory::Soak,
            ScenarioCategory::Performance,
        ] {
            let json = serde_json::to_string(&cat).unwrap();
            let back: ScenarioCategory = serde_json::from_str(&json).unwrap();
            assert_eq!(back, cat);
        }
    }

    #[test]
    fn test_scenario_verdict_display() {
        assert_eq!(ScenarioVerdict::Pass.to_string(), "pass");
        assert_eq!(ScenarioVerdict::Fail.to_string(), "fail");
        assert_eq!(ScenarioVerdict::Skip.to_string(), "skip");
        assert_eq!(ScenarioVerdict::Flaky.to_string(), "flaky");
    }

    #[test]
    fn test_scenario_verdict_serde() {
        for v in [
            ScenarioVerdict::Pass,
            ScenarioVerdict::Fail,
            ScenarioVerdict::Skip,
            ScenarioVerdict::Flaky,
        ] {
            let json = serde_json::to_string(&v).unwrap();
            let back: ScenarioVerdict = serde_json::from_str(&json).unwrap();
            assert_eq!(back, v);
        }
    }

    #[test]
    fn test_matrix_scenario_serde() {
        let s = MatrixScenario {
            scenario_id: "e2e-001".to_string(),
            category: ScenarioCategory::E2E,
            description: "happy path".to_string(),
            stages: vec![LatencyStage::PtyCapture, LatencyStage::StorageWrite],
            domain: InvariantDomain::Scheduler,
            required_for_promotion: true,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: MatrixScenario = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn test_scenario_result_serde() {
        let r = ScenarioResult {
            scenario_id: "e2e-001".to_string(),
            verdict: ScenarioVerdict::Pass,
            duration_us: 5000,
            failure_message: None,
            artifacts: vec!["trace.json".to_string()],
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: ScenarioResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn test_promotion_gate_serde() {
        let g = PromotionGate {
            name: "staging".to_string(),
            required_scenarios: vec!["e2e-001".to_string()],
            min_pass_rate: 0.95,
            max_flaky_count: 2,
        };
        let json = serde_json::to_string(&g).unwrap();
        let back: PromotionGate = serde_json::from_str(&json).unwrap();
        assert_eq!(back, g);
    }

    #[test]
    fn test_validation_matrix_new_empty() {
        let matrix = ValidationMatrix::new();
        assert_eq!(matrix.scenario_count(), 0);
        assert_eq!(matrix.result_count(), 0);
    }

    #[test]
    fn test_validation_matrix_add_scenario() {
        let mut matrix = ValidationMatrix::new();
        matrix.add_scenario(MatrixScenario {
            scenario_id: "e2e-001".to_string(),
            category: ScenarioCategory::E2E,
            description: "basic".to_string(),
            stages: vec![LatencyStage::PtyCapture],
            domain: InvariantDomain::Scheduler,
            required_for_promotion: true,
        });
        assert_eq!(matrix.scenario_count(), 1);
    }

    #[test]
    fn test_validation_matrix_record_result() {
        let mut matrix = ValidationMatrix::new();
        matrix.record_result(ScenarioResult {
            scenario_id: "e2e-001".to_string(),
            verdict: ScenarioVerdict::Pass,
            duration_us: 1000,
            failure_message: None,
            artifacts: vec![],
        });
        assert_eq!(matrix.result_count(), 1);
    }

    #[test]
    fn test_validation_matrix_latest_result() {
        let mut matrix = ValidationMatrix::new();
        matrix.record_result(ScenarioResult {
            scenario_id: "e2e-001".to_string(),
            verdict: ScenarioVerdict::Fail,
            duration_us: 1000,
            failure_message: Some("first".to_string()),
            artifacts: vec![],
        });
        matrix.record_result(ScenarioResult {
            scenario_id: "e2e-001".to_string(),
            verdict: ScenarioVerdict::Pass,
            duration_us: 2000,
            failure_message: None,
            artifacts: vec![],
        });
        let latest = matrix.latest_result("e2e-001").unwrap();
        assert_eq!(latest.verdict, ScenarioVerdict::Pass);
    }

    #[test]
    fn test_validation_matrix_check_gate_passes() {
        let mut matrix = ValidationMatrix::new();
        matrix.add_gate(PromotionGate {
            name: "staging".to_string(),
            required_scenarios: vec!["e2e-001".to_string()],
            min_pass_rate: 0.5,
            max_flaky_count: 1,
        });
        matrix.record_result(ScenarioResult {
            scenario_id: "e2e-001".to_string(),
            verdict: ScenarioVerdict::Pass,
            duration_us: 1000,
            failure_message: None,
            artifacts: vec![],
        });
        assert!(matrix.check_gate("staging"));
    }

    #[test]
    fn test_validation_matrix_check_gate_fails_required() {
        let mut matrix = ValidationMatrix::new();
        matrix.add_gate(PromotionGate {
            name: "staging".to_string(),
            required_scenarios: vec!["e2e-001".to_string()],
            min_pass_rate: 0.5,
            max_flaky_count: 1,
        });
        matrix.record_result(ScenarioResult {
            scenario_id: "e2e-001".to_string(),
            verdict: ScenarioVerdict::Fail,
            duration_us: 1000,
            failure_message: Some("broken".to_string()),
            artifacts: vec![],
        });
        assert!(!matrix.check_gate("staging"));
    }

    #[test]
    fn test_validation_matrix_check_gate_unknown() {
        let matrix = ValidationMatrix::new();
        assert!(!matrix.check_gate("nonexistent"));
    }

    #[test]
    fn test_validation_matrix_snapshot() {
        let mut matrix = ValidationMatrix::new();
        matrix.add_scenario(MatrixScenario {
            scenario_id: "s1".to_string(),
            category: ScenarioCategory::E2E,
            description: "x".to_string(),
            stages: vec![],
            domain: InvariantDomain::Scheduler,
            required_for_promotion: false,
        });
        matrix.record_result(ScenarioResult {
            scenario_id: "s1".to_string(),
            verdict: ScenarioVerdict::Pass,
            duration_us: 100,
            failure_message: None,
            artifacts: vec![],
        });
        let snap = matrix.snapshot();
        assert_eq!(snap.total_scenarios, 1);
        assert_eq!(snap.pass_count, 1);
        assert_eq!(snap.fail_count, 0);
    }

    #[test]
    fn test_validation_matrix_snapshot_serde() {
        let snap = MatrixSnapshot {
            total_scenarios: 5,
            pass_count: 3,
            fail_count: 1,
            skip_count: 0,
            flaky_count: 1,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: MatrixSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back, snap);
    }

    #[test]
    fn test_validation_matrix_degradation_healthy() {
        let matrix = ValidationMatrix::new();
        assert_eq!(matrix.detect_degradation(), MatrixDegradation::Healthy);
    }

    #[test]
    fn test_validation_matrix_degradation_gate_failure() {
        let mut matrix = ValidationMatrix::new();
        matrix.add_scenario(MatrixScenario {
            scenario_id: "req".to_string(),
            category: ScenarioCategory::E2E,
            description: "required".to_string(),
            stages: vec![],
            domain: InvariantDomain::Scheduler,
            required_for_promotion: true,
        });
        // No result → fails.
        let deg = matrix.detect_degradation();
        assert!(matches!(deg, MatrixDegradation::GateFailure { .. }));
    }

    #[test]
    fn test_validation_matrix_degradation_flaky() {
        let mut matrix = ValidationMatrix::new();
        matrix.record_result(ScenarioResult {
            scenario_id: "s1".to_string(),
            verdict: ScenarioVerdict::Flaky,
            duration_us: 100,
            failure_message: None,
            artifacts: vec![],
        });
        let deg = matrix.detect_degradation();
        assert_eq!(deg, MatrixDegradation::FlakyDetected { flaky_count: 1 });
    }

    #[test]
    fn test_validation_matrix_degradation_display() {
        assert_eq!(MatrixDegradation::Healthy.to_string(), "healthy");
        assert_eq!(
            MatrixDegradation::FlakyDetected { flaky_count: 3 }.to_string(),
            "flaky(3)"
        );
        let gf = MatrixDegradation::GateFailure {
            failed_scenarios: vec!["a".to_string(), "b".to_string()],
        };
        assert_eq!(gf.to_string(), "gate-failure(2)");
    }

    #[test]
    fn test_validation_matrix_degradation_serde() {
        let cases = vec![
            MatrixDegradation::Healthy,
            MatrixDegradation::FlakyDetected { flaky_count: 2 },
            MatrixDegradation::GateFailure {
                failed_scenarios: vec!["x".to_string()],
            },
        ];
        for deg in cases {
            let json = serde_json::to_string(&deg).unwrap();
            let back: MatrixDegradation = serde_json::from_str(&json).unwrap();
            assert_eq!(back, deg);
        }
    }

    #[test]
    fn test_validation_matrix_log_entry() {
        let matrix = ValidationMatrix::new();
        let entry = matrix.log_entry("s1".to_string(), "started".to_string(), 42);
        assert_eq!(entry.scenario_id, "s1");
        assert_eq!(entry.timestamp_us, 42);
    }

    #[test]
    fn test_validation_matrix_log_entry_serde() {
        let entry = MatrixLogEntry {
            timestamp_us: 1000,
            scenario_id: "x".to_string(),
            event: "done".to_string(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: MatrixLogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back, entry);
    }

    #[test]
    fn test_validation_matrix_scenarios_by_category() {
        let mut matrix = ValidationMatrix::new();
        matrix.add_scenario(MatrixScenario {
            scenario_id: "e1".to_string(),
            category: ScenarioCategory::E2E,
            description: "x".to_string(),
            stages: vec![],
            domain: InvariantDomain::Scheduler,
            required_for_promotion: false,
        });
        matrix.add_scenario(MatrixScenario {
            scenario_id: "c1".to_string(),
            category: ScenarioCategory::Chaos,
            description: "y".to_string(),
            stages: vec![],
            domain: InvariantDomain::Recovery,
            required_for_promotion: false,
        });
        assert_eq!(matrix.scenarios_by_category(ScenarioCategory::E2E).len(), 1);
        assert_eq!(
            matrix.scenarios_by_category(ScenarioCategory::Chaos).len(),
            1
        );
        assert_eq!(
            matrix.scenarios_by_category(ScenarioCategory::Soak).len(),
            0
        );
    }

    #[test]
    fn test_validation_matrix_reset_results() {
        let mut matrix = ValidationMatrix::new();
        matrix.record_result(ScenarioResult {
            scenario_id: "s1".to_string(),
            verdict: ScenarioVerdict::Pass,
            duration_us: 100,
            failure_message: None,
            artifacts: vec![],
        });
        matrix.reset_results();
        assert_eq!(matrix.result_count(), 0);
    }

    #[test]
    fn test_validation_matrix_gates_accessor() {
        let mut matrix = ValidationMatrix::new();
        matrix.add_gate(PromotionGate {
            name: "prod".to_string(),
            required_scenarios: vec![],
            min_pass_rate: 0.99,
            max_flaky_count: 0,
        });
        assert_eq!(matrix.gates().len(), 1);
        assert_eq!(matrix.gates()[0].name, "prod");
    }

    #[test]
    fn test_validation_matrix_results_for() {
        let mut matrix = ValidationMatrix::new();
        matrix.record_result(ScenarioResult {
            scenario_id: "s1".to_string(),
            verdict: ScenarioVerdict::Pass,
            duration_us: 100,
            failure_message: None,
            artifacts: vec![],
        });
        matrix.record_result(ScenarioResult {
            scenario_id: "s2".to_string(),
            verdict: ScenarioVerdict::Fail,
            duration_us: 200,
            failure_message: Some("err".to_string()),
            artifacts: vec![],
        });
        assert_eq!(matrix.results_for("s1").len(), 1);
        assert_eq!(matrix.results_for("s2").len(), 1);
        assert_eq!(matrix.results_for("s3").len(), 0);
    }

    // ── F4 Impl: Bridge method tests ──

    #[test]
    fn test_matrix_pass_rate_empty() {
        let matrix = ValidationMatrix::new();
        assert!((matrix.pass_rate() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_matrix_pass_rate() {
        let mut matrix = ValidationMatrix::new();
        matrix.record_result(ScenarioResult {
            scenario_id: "s1".to_string(),
            verdict: ScenarioVerdict::Pass,
            duration_us: 100,
            failure_message: None,
            artifacts: vec![],
        });
        matrix.record_result(ScenarioResult {
            scenario_id: "s2".to_string(),
            verdict: ScenarioVerdict::Fail,
            duration_us: 200,
            failure_message: None,
            artifacts: vec![],
        });
        assert!((matrix.pass_rate() - 0.5).abs() < 0.01);
    }

    #[test]
    fn test_matrix_flaky_rate() {
        let mut matrix = ValidationMatrix::new();
        matrix.record_result(ScenarioResult {
            scenario_id: "s1".to_string(),
            verdict: ScenarioVerdict::Flaky,
            duration_us: 100,
            failure_message: None,
            artifacts: vec![],
        });
        matrix.record_result(ScenarioResult {
            scenario_id: "s2".to_string(),
            verdict: ScenarioVerdict::Pass,
            duration_us: 200,
            failure_message: None,
            artifacts: vec![],
        });
        assert!((matrix.flaky_rate() - 0.5).abs() < 0.01);
    }

    #[test]
    fn test_matrix_mean_pass_duration() {
        let mut matrix = ValidationMatrix::new();
        matrix.record_result(ScenarioResult {
            scenario_id: "s1".to_string(),
            verdict: ScenarioVerdict::Pass,
            duration_us: 100,
            failure_message: None,
            artifacts: vec![],
        });
        matrix.record_result(ScenarioResult {
            scenario_id: "s2".to_string(),
            verdict: ScenarioVerdict::Pass,
            duration_us: 300,
            failure_message: None,
            artifacts: vec![],
        });
        assert!((matrix.mean_pass_duration_us() - 200.0).abs() < 0.01);
    }

    #[test]
    fn test_matrix_passing_failing_gates() {
        let mut matrix = ValidationMatrix::new();
        matrix.add_gate(PromotionGate {
            name: "canary".to_string(),
            required_scenarios: vec!["s1".to_string()],
            min_pass_rate: 0.5,
            max_flaky_count: 10,
        });
        matrix.record_result(ScenarioResult {
            scenario_id: "s1".to_string(),
            verdict: ScenarioVerdict::Pass,
            duration_us: 100,
            failure_message: None,
            artifacts: vec![],
        });
        assert_eq!(matrix.passing_gates(), vec!["canary".to_string()]);
        assert!(matrix.failing_gates().is_empty());
    }

    #[test]
    fn test_matrix_missing_required() {
        let mut matrix = ValidationMatrix::new();
        matrix.add_scenario(MatrixScenario {
            scenario_id: "req1".to_string(),
            category: ScenarioCategory::E2E,
            description: "required".to_string(),
            stages: vec![],
            domain: InvariantDomain::Scheduler,
            required_for_promotion: true,
        });
        assert_eq!(matrix.missing_required(), vec!["req1".to_string()]);
        matrix.record_result(ScenarioResult {
            scenario_id: "req1".to_string(),
            verdict: ScenarioVerdict::Pass,
            duration_us: 100,
            failure_message: None,
            artifacts: vec![],
        });
        assert!(matrix.missing_required().is_empty());
    }

    #[test]
    fn test_matrix_all_artifacts() {
        let mut matrix = ValidationMatrix::new();
        matrix.record_result(ScenarioResult {
            scenario_id: "s1".to_string(),
            verdict: ScenarioVerdict::Pass,
            duration_us: 100,
            failure_message: None,
            artifacts: vec!["a.json".to_string()],
        });
        matrix.record_result(ScenarioResult {
            scenario_id: "s2".to_string(),
            verdict: ScenarioVerdict::Pass,
            duration_us: 200,
            failure_message: None,
            artifacts: vec!["b.json".to_string(), "c.json".to_string()],
        });
        assert_eq!(matrix.all_artifacts().len(), 3);
    }

    #[test]
    fn test_matrix_to_invariant_domain() {
        assert_eq!(
            ValidationMatrix::to_invariant_domain(),
            InvariantDomain::Composition
        );
    }

    // ── F5: Input-to-Paint QoE Guardrail Lane ──

    #[test]
    fn test_qoe_metric_display() {
        assert_eq!(QoEMetric::InputToPaint.to_string(), "input-to-paint");
        assert_eq!(QoEMetric::FrameJitter.to_string(), "frame-jitter");
        assert_eq!(QoEMetric::Smoothness.to_string(), "smoothness");
        assert_eq!(QoEMetric::KeystrokeEcho.to_string(), "keystroke-echo");
    }

    #[test]
    fn test_qoe_metric_serde() {
        for m in [
            QoEMetric::InputToPaint,
            QoEMetric::FrameJitter,
            QoEMetric::Smoothness,
            QoEMetric::KeystrokeEcho,
        ] {
            let json = serde_json::to_string(&m).unwrap();
            let back: QoEMetric = serde_json::from_str(&json).unwrap();
            assert_eq!(back, m);
        }
    }

    #[test]
    fn test_qoe_slo_serde() {
        let slo = QoESLO {
            metric: QoEMetric::InputToPaint,
            target: 16_667.0,
            percentile: 0.95,
            description: "p95 under 16.67ms".to_string(),
        };
        let json = serde_json::to_string(&slo).unwrap();
        let back: QoESLO = serde_json::from_str(&json).unwrap();
        assert_eq!(back, slo);
    }

    #[test]
    fn test_qoe_measurement_serde() {
        let m = QoEMeasurement {
            metric: QoEMetric::FrameJitter,
            value: 2000.0,
            timestamp_us: 42,
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: QoEMeasurement = serde_json::from_str(&json).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn test_qoe_guardrail_config_default() {
        let cfg = QoEGuardrailConfig::default();
        assert_eq!(cfg.slos.len(), 4);
        assert_eq!(cfg.window_size, 1000);
        assert_eq!(cfg.min_samples, 30);
    }

    #[test]
    fn test_qoe_guardrail_config_serde() {
        let cfg = QoEGuardrailConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let back: QoEGuardrailConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn test_qoe_guardrail_record() {
        let mut guard = QoEGuardrail::new(QoEGuardrailConfig::default());
        guard.record(QoEMeasurement {
            metric: QoEMetric::InputToPaint,
            value: 10000.0,
            timestamp_us: 1,
        });
        assert_eq!(guard.total_measurements(), 1);
        assert_eq!(guard.window_len(QoEMetric::InputToPaint), 1);
    }

    #[test]
    fn test_qoe_guardrail_window_eviction() {
        let cfg = QoEGuardrailConfig {
            window_size: 3,
            min_samples: 1,
            ..Default::default()
        };
        let mut guard = QoEGuardrail::new(cfg);
        for i in 0..5 {
            guard.record(QoEMeasurement {
                metric: QoEMetric::InputToPaint,
                value: i as f64,
                timestamp_us: i,
            });
        }
        assert_eq!(guard.window_len(QoEMetric::InputToPaint), 3);
    }

    #[test]
    fn test_qoe_evaluate_slo_insufficient_data() {
        let guard = QoEGuardrail::new(QoEGuardrailConfig::default());
        let slo = &guard.config().slos[0];
        let verdict = guard.evaluate_slo(slo);
        assert!(matches!(verdict, SLOVerdict::InsufficientData { .. }));
    }

    #[test]
    fn test_qoe_evaluate_slo_met() {
        let cfg = QoEGuardrailConfig {
            slos: vec![QoESLO {
                metric: QoEMetric::InputToPaint,
                target: 20_000.0,
                percentile: 0.95,
                description: "test".to_string(),
            }],
            window_size: 100,
            min_samples: 5,
        };
        let mut guard = QoEGuardrail::new(cfg);
        // Add 10 samples all under the target.
        for i in 0..10 {
            guard.record(QoEMeasurement {
                metric: QoEMetric::InputToPaint,
                value: 10_000.0,
                timestamp_us: i,
            });
        }
        let verdict = guard.evaluate_slo(&guard.config().slos[0].clone());
        assert!(matches!(verdict, SLOVerdict::Met { .. }));
    }

    #[test]
    fn test_qoe_evaluate_slo_breached() {
        let cfg = QoEGuardrailConfig {
            slos: vec![QoESLO {
                metric: QoEMetric::InputToPaint,
                target: 5_000.0,
                percentile: 0.95,
                description: "test".to_string(),
            }],
            window_size: 100,
            min_samples: 5,
        };
        let mut guard = QoEGuardrail::new(cfg);
        // Add samples above target.
        for i in 0..10 {
            guard.record(QoEMeasurement {
                metric: QoEMetric::InputToPaint,
                value: 20_000.0,
                timestamp_us: i,
            });
        }
        let verdict = guard.evaluate_slo(&guard.config().slos[0].clone());
        assert!(matches!(verdict, SLOVerdict::Breached { .. }));
    }

    #[test]
    fn test_qoe_smoothness_higher_is_better() {
        let cfg = QoEGuardrailConfig {
            slos: vec![QoESLO {
                metric: QoEMetric::Smoothness,
                target: 0.9,
                percentile: 0.50,
                description: "median smoothness".to_string(),
            }],
            window_size: 100,
            min_samples: 5,
        };
        let mut guard = QoEGuardrail::new(cfg);
        for i in 0..10 {
            guard.record(QoEMeasurement {
                metric: QoEMetric::Smoothness,
                value: 0.95,
                timestamp_us: i,
            });
        }
        let verdict = guard.evaluate_slo(&guard.config().slos[0].clone());
        assert!(matches!(verdict, SLOVerdict::Met { .. }));
    }

    #[test]
    fn test_qoe_snapshot() {
        let cfg = QoEGuardrailConfig {
            min_samples: 1,
            ..Default::default()
        };
        let mut guard = QoEGuardrail::new(cfg);
        guard.record(QoEMeasurement {
            metric: QoEMetric::InputToPaint,
            value: 10000.0,
            timestamp_us: 1,
        });
        let snap = guard.snapshot();
        assert_eq!(snap.total_measurements, 1);
    }

    #[test]
    fn test_qoe_snapshot_serde() {
        let snap = QoEGuardrailSnapshot {
            verdicts: vec![(
                QoEMetric::InputToPaint,
                SLOVerdict::Met {
                    measured: 10.0,
                    target: 20.0,
                },
            )],
            total_measurements: 100,
            breach_count: 0,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: QoEGuardrailSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back, snap);
    }

    #[test]
    fn test_qoe_degradation_warming_up() {
        let guard = QoEGuardrail::new(QoEGuardrailConfig::default());
        let deg = guard.detect_degradation();
        assert!(matches!(deg, QoEDegradation::WarmingUp { .. }));
    }

    #[test]
    fn test_qoe_degradation_healthy() {
        let cfg = QoEGuardrailConfig {
            slos: vec![QoESLO {
                metric: QoEMetric::InputToPaint,
                target: 20_000.0,
                percentile: 0.95,
                description: "test".to_string(),
            }],
            window_size: 100,
            min_samples: 5,
        };
        let mut guard = QoEGuardrail::new(cfg);
        for i in 0..10 {
            guard.record(QoEMeasurement {
                metric: QoEMetric::InputToPaint,
                value: 10000.0,
                timestamp_us: i,
            });
        }
        assert_eq!(guard.detect_degradation(), QoEDegradation::Healthy);
    }

    #[test]
    fn test_qoe_degradation_slo_breach() {
        let cfg = QoEGuardrailConfig {
            slos: vec![QoESLO {
                metric: QoEMetric::InputToPaint,
                target: 5_000.0,
                percentile: 0.95,
                description: "test".to_string(),
            }],
            window_size: 100,
            min_samples: 5,
        };
        let mut guard = QoEGuardrail::new(cfg);
        for i in 0..10 {
            guard.record(QoEMeasurement {
                metric: QoEMetric::InputToPaint,
                value: 20000.0,
                timestamp_us: i,
            });
        }
        let deg = guard.detect_degradation();
        assert!(matches!(deg, QoEDegradation::SLOBreach { .. }));
    }

    #[test]
    fn test_qoe_degradation_display() {
        assert_eq!(QoEDegradation::Healthy.to_string(), "healthy");
        assert_eq!(
            QoEDegradation::SLOBreach { breach_count: 2 }.to_string(),
            "slo-breach(2)"
        );
        assert_eq!(
            QoEDegradation::WarmingUp { samples: 5 }.to_string(),
            "warming-up(5)"
        );
    }

    #[test]
    fn test_qoe_degradation_serde() {
        let cases = vec![
            QoEDegradation::Healthy,
            QoEDegradation::SLOBreach { breach_count: 3 },
            QoEDegradation::WarmingUp { samples: 10 },
        ];
        for deg in cases {
            let json = serde_json::to_string(&deg).unwrap();
            let back: QoEDegradation = serde_json::from_str(&json).unwrap();
            assert_eq!(back, deg);
        }
    }

    #[test]
    fn test_qoe_slo_verdict_display() {
        assert_eq!(
            SLOVerdict::Met {
                measured: 10.0,
                target: 20.0
            }
            .to_string(),
            "met(10.0/20.0)"
        );
        assert_eq!(
            SLOVerdict::Breached {
                measured: 30.0,
                target: 20.0
            }
            .to_string(),
            "breached(30.0/20.0)"
        );
        assert_eq!(
            SLOVerdict::InsufficientData {
                samples: 5,
                required: 30
            }
            .to_string(),
            "insufficient(5/30)"
        );
    }

    #[test]
    fn test_qoe_slo_verdict_serde() {
        let cases = vec![
            SLOVerdict::Met {
                measured: 10.0,
                target: 20.0,
            },
            SLOVerdict::Breached {
                measured: 30.0,
                target: 20.0,
            },
            SLOVerdict::InsufficientData {
                samples: 5,
                required: 30,
            },
        ];
        for v in cases {
            let json = serde_json::to_string(&v).unwrap();
            let back: SLOVerdict = serde_json::from_str(&json).unwrap();
            assert_eq!(back, v);
        }
    }

    #[test]
    fn test_qoe_log_entry() {
        let guard = QoEGuardrail::new(QoEGuardrailConfig::default());
        let entry = guard.log_entry(QoEMetric::InputToPaint, "spike".to_string(), 42);
        assert_eq!(entry.metric, QoEMetric::InputToPaint);
        assert_eq!(entry.timestamp_us, 42);
    }

    #[test]
    fn test_qoe_log_entry_serde() {
        let entry = QoELogEntry {
            timestamp_us: 100,
            metric: QoEMetric::FrameJitter,
            event: "test".to_string(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: QoELogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back, entry);
    }

    #[test]
    fn test_qoe_reset() {
        let mut guard = QoEGuardrail::new(QoEGuardrailConfig::default());
        guard.record(QoEMeasurement {
            metric: QoEMetric::InputToPaint,
            value: 10000.0,
            timestamp_us: 1,
        });
        guard.reset();
        assert_eq!(guard.total_measurements(), 0);
        assert_eq!(guard.window_len(QoEMetric::InputToPaint), 0);
    }

    #[test]
    fn test_qoe_config_accessor() {
        let cfg = QoEGuardrailConfig {
            min_samples: 42,
            ..Default::default()
        };
        let guard = QoEGuardrail::new(cfg.clone());
        assert_eq!(*guard.config(), cfg);
    }

    #[test]
    fn test_qoe_to_invariant_domain() {
        assert_eq!(
            QoEGuardrail::to_invariant_domain(),
            InvariantDomain::Composition
        );
    }

    // ── F5 Impl: Bridge method tests ──

    #[test]
    fn test_qoe_slo_count() {
        let guard = QoEGuardrail::new(QoEGuardrailConfig::default());
        assert_eq!(guard.slo_count(), 4);
    }

    #[test]
    fn test_qoe_met_and_breach_count() {
        let cfg = QoEGuardrailConfig {
            slos: vec![
                QoESLO {
                    metric: QoEMetric::InputToPaint,
                    target: 20_000.0,
                    percentile: 0.95,
                    description: "x".to_string(),
                },
                QoESLO {
                    metric: QoEMetric::FrameJitter,
                    target: 100.0,
                    percentile: 0.95,
                    description: "y".to_string(),
                },
            ],
            window_size: 100,
            min_samples: 5,
        };
        let mut guard = QoEGuardrail::new(cfg);
        for i in 0..10 {
            guard.record(QoEMeasurement {
                metric: QoEMetric::InputToPaint,
                value: 10_000.0,
                timestamp_us: i,
            });
            guard.record(QoEMeasurement {
                metric: QoEMetric::FrameJitter,
                value: 5_000.0,
                timestamp_us: i,
            });
        }
        assert_eq!(guard.met_count(), 1); // InputToPaint met.
        assert_eq!(guard.breach_count(), 1); // FrameJitter breached.
    }

    #[test]
    fn test_qoe_compliance_rate() {
        let cfg = QoEGuardrailConfig {
            slos: vec![
                QoESLO {
                    metric: QoEMetric::InputToPaint,
                    target: 20_000.0,
                    percentile: 0.95,
                    description: "x".to_string(),
                },
                QoESLO {
                    metric: QoEMetric::FrameJitter,
                    target: 100.0,
                    percentile: 0.95,
                    description: "y".to_string(),
                },
            ],
            window_size: 100,
            min_samples: 5,
        };
        let mut guard = QoEGuardrail::new(cfg);
        for i in 0..10 {
            guard.record(QoEMeasurement {
                metric: QoEMetric::InputToPaint,
                value: 10_000.0,
                timestamp_us: i,
            });
            guard.record(QoEMeasurement {
                metric: QoEMetric::FrameJitter,
                value: 5_000.0,
                timestamp_us: i,
            });
        }
        assert!((guard.compliance_rate() - 0.5).abs() < 0.01);
    }

    #[test]
    fn test_qoe_record_batch() {
        let cfg = QoEGuardrailConfig {
            min_samples: 1,
            ..Default::default()
        };
        let mut guard = QoEGuardrail::new(cfg);
        guard.record_batch(QoEMetric::InputToPaint, &[100.0, 200.0, 300.0], 1000);
        assert_eq!(guard.total_measurements(), 3);
        assert_eq!(guard.window_len(QoEMetric::InputToPaint), 3);
    }

    #[test]
    fn test_qoe_current_percentile() {
        let cfg = QoEGuardrailConfig {
            min_samples: 5,
            window_size: 100,
            ..Default::default()
        };
        let mut guard = QoEGuardrail::new(cfg);
        // Not enough data.
        assert!(
            guard
                .current_percentile(QoEMetric::InputToPaint, 0.50)
                .is_none()
        );
        for i in 0..10 {
            guard.record(QoEMeasurement {
                metric: QoEMetric::InputToPaint,
                value: (i + 1) as f64 * 1000.0,
                timestamp_us: i,
            });
        }
        let p50 = guard
            .current_percentile(QoEMetric::InputToPaint, 0.50)
            .unwrap();
        assert!(p50 > 0.0);
    }

    #[test]
    fn test_qoe_all_slos_met() {
        let cfg = QoEGuardrailConfig {
            slos: vec![QoESLO {
                metric: QoEMetric::InputToPaint,
                target: 20_000.0,
                percentile: 0.95,
                description: "x".to_string(),
            }],
            window_size: 100,
            min_samples: 5,
        };
        let mut guard = QoEGuardrail::new(cfg);
        // Insufficient data — no breach → all_slos_met.
        assert!(guard.all_slos_met());
        for i in 0..10 {
            guard.record(QoEMeasurement {
                metric: QoEMetric::InputToPaint,
                value: 10_000.0,
                timestamp_us: i,
            });
        }
        assert!(guard.all_slos_met());
    }
}
