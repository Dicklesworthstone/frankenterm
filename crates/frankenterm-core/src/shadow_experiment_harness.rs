//! br-ft-1650n.12: Shadow-mode swarm experiment harness substrate.
//!
//! Generic shadow-execution envelope that records what a
//! candidate workflow / policy / pattern pack / load-shedding rule
//! WOULD have decided on each event, alongside the baseline
//! decision, without mutating pane state. Ledger entries pair
//! `(event_id, baseline, candidate, divergence)` and the ledger
//! itself is the only side-effect.
//!
//! Complements `shadow_mode_evaluator.rs` (mission-loop-specific):
//! this substrate is generic over the decision type (any
//! `serde::Serialize + PartialEq` value) so it works for
//! workflows, policies, pattern packs, and load-shedding rules.
//!
//! ## What ships in this slice
//!
//! - [`ExperimentManifest`] — name, baseline/candidate labels,
//!   budget caps, redaction policy version.
//! - [`ExperimentBudget`] — bounded operator-set caps:
//!   `max_events`, `max_overhead_us_per_event`,
//!   `max_total_overhead_us`.
//! - [`DivergenceKind`] — `Match` / `Minor` / `Major`.
//! - [`ShadowDecisionPair`] — one baseline-vs-candidate sample.
//! - [`ExperimentAbortReason`] — typed reason the experiment
//!   stopped early.
//! - [`ExperimentLedger`] — append-only accumulator with
//!   sampling-gap counter.
//! - [`ShadowExperimentHarness`] — append-only API. The harness
//!   owns the ledger and exposes `record_pair(...)` plus
//!   `report()`.
//!
//! ## No-side-effect contract
//!
//! The harness type CANNOT touch pane state. The substrate is
//! enforced by construction: the API surface accepts only
//! `&self` (ledger reads) and `&mut self` (ledger writes) — no
//! `&mut PaneState` argument. The bead's "writes only to an
//! experiment ledger" criterion is type-level.
//!
//! ## What is deferred
//!
//! - Wired-pass: a wired-pass slice will plumb real event
//!   streams into the harness alongside baseline/candidate
//!   evaluators (workflow runner / policy engine / etc.).
//! - Cross-experiment correlation: today the substrate ships
//!   one ledger per harness; a future slice can add a registry.

use std::time::Duration;

use ft_perf_gate::{sprt, EvidenceSample, GateDecision, SprtConfig};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Stable schema version for `ExperimentLedger` exports.
pub const SHADOW_EXPERIMENT_LEDGER_SCHEMA: &str = "ft.shadow_experiment.ledger.v1";

/// Stable schema version for agent A/B experiment receipts.
pub const AGENT_EXPERIMENT_RECEIPT_SCHEMA: &str = "ft.agent_experiment.receipt.v1";

const BASIS_POINTS_DENOMINATOR: u128 = 10_000;

/// br-ft-1650n.12: operator-tunable budgets. The harness aborts
/// when ANY budget is exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentBudget {
    /// Maximum events the harness will record before aborting
    /// the experiment. Bounded state — past this many events
    /// the ledger does not grow further.
    pub max_events: u64,
    /// Maximum per-event candidate-evaluation overhead in
    /// microseconds. A single event exceeding this triggers
    /// `ExperimentAbortReason::OverheadExceeded`.
    pub max_overhead_us_per_event: u64,
    /// Maximum cumulative overhead in microseconds across the
    /// entire experiment. Hitting this triggers
    /// `ExperimentAbortReason::OverheadExceeded`.
    pub max_total_overhead_us: u64,
}

impl ExperimentBudget {
    /// Conservative defaults: 100K events, 10ms per-event,
    /// 30s total overhead.
    #[must_use]
    pub const fn conservative() -> Self {
        Self {
            max_events: 100_000,
            max_overhead_us_per_event: 10_000,
            max_total_overhead_us: 30_000_000,
        }
    }
}

/// br-ft-1650n.12: experiment manifest. The bead's "experiment
/// manifests" item maps to this struct. All fields are
/// operator-supplied and immutable for the lifetime of the
/// experiment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentManifest {
    pub experiment_id: String,
    pub baseline_label: String,
    pub candidate_label: String,
    pub budget: ExperimentBudget,
    /// Redaction-policy version applied to any event content
    /// the caller stores in the ledger. The substrate is
    /// content-blind; it trusts the caller to redact.
    pub redaction_policy_version: String,
}

/// br-ft-1650n.12: divergence classification per recorded
/// sample. The caller decides which band a baseline-vs-candidate
/// pair falls into; the substrate just threads the band through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DivergenceKind {
    /// Baseline and candidate produced identical decisions.
    Match,
    /// Decisions differ but in a way the operator considers
    /// safe (e.g., same outcome, different rationale).
    Minor,
    /// Decisions differ in outcome — operator must review
    /// before promoting the candidate.
    Major,
}

/// br-ft-1650n.12: one shadow-recorded sample. Every entry
/// includes an event_id (for correlation with the source
/// stream) and the per-event candidate-evaluation latency so
/// the budget gate can fire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShadowDecisionPair {
    pub event_id: String,
    /// Already-redacted baseline decision summary.
    pub baseline_decision: String,
    /// Already-redacted candidate decision summary.
    pub candidate_decision: String,
    pub divergence: DivergenceKind,
    /// Candidate-evaluation overhead in microseconds.
    pub overhead_us: u64,
    /// Caller-supplied citations for the divergence (links to
    /// causal-graph nodes, pattern-hit IDs, etc.). The bead's
    /// "decision deltas cite causal/evidence sources" criterion.
    pub evidence_citations: Vec<String>,
}

/// br-ft-1650n.12: typed abort reason. The harness records the
/// reason on the ledger when the experiment stops early.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ExperimentAbortReason {
    /// `max_events` reached.
    EventsCapReached,
    /// A per-event overhead exceeded `max_overhead_us_per_event`.
    OverheadExceeded {
        sample_event_id: String,
        observed_us: u64,
    },
    /// Cumulative overhead exceeded `max_total_overhead_us`.
    TotalOverheadExceeded { observed_us: u64 },
    /// Operator stopped the experiment.
    ManualStop { reason: String },
}

/// br-ft-1650n.12: append-only experiment ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentLedger {
    pub schema_version: String,
    pub manifest: ExperimentManifest,
    pub decisions: Vec<ShadowDecisionPair>,
    /// Number of events the harness skipped because the
    /// experiment had already aborted. Operators read this as
    /// "how much of the source stream the experiment did not
    /// observe". The bead's "if shadow path cannot keep up,
    /// record sampling gaps and disable itself" fallback
    /// criterion.
    pub sampling_gaps: u64,
    /// `Some(reason)` if the experiment aborted early.
    pub abort_reason: Option<ExperimentAbortReason>,
    pub total_overhead_us: u64,
}

impl ExperimentLedger {
    /// Aggregate counts per divergence band. Pure read.
    #[must_use]
    pub fn divergence_counts(&self) -> DivergenceCounts {
        let mut out = DivergenceCounts::default();
        for d in &self.decisions {
            match d.divergence {
                DivergenceKind::Match => out.match_count += 1,
                DivergenceKind::Minor => out.minor_count += 1,
                DivergenceKind::Major => out.major_count += 1,
            }
        }
        out
    }
}

/// br-ft-1650n.12: tally per divergence band.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DivergenceCounts {
    pub match_count: u64,
    pub minor_count: u64,
    pub major_count: u64,
}

/// Variant arm assigned to an equivalent work item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentExperimentVariant {
    /// Baseline fleet template.
    Baseline,
    /// Candidate fleet template.
    Candidate,
}

impl AgentExperimentVariant {
    fn label(self, manifest: &ExperimentManifest) -> &str {
        match self {
            Self::Baseline => &manifest.baseline_label,
            Self::Candidate => &manifest.candidate_label,
        }
    }

    const fn token(self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::Candidate => "candidate",
        }
    }
}

/// Deterministic assignment of one equivalent work item to one template arm.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentWorkAssignment {
    /// Bead id, task id, fixture id, or other stable work source key.
    pub work_item_id: String,
    /// Assigned experiment arm.
    pub variant: AgentExperimentVariant,
    /// Template label copied from the manifest at assignment time.
    pub template_label: String,
    /// Deterministic assignment rationale for operator review.
    pub assignment_reason: String,
}

/// Read-only assignment plan for an agent A/B experiment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentExperimentAssignmentPlan {
    /// Stable schema version.
    pub schema_version: String,
    /// Manifest experiment id.
    pub experiment_id: String,
    /// Assignment strategy identifier.
    pub assignment_strategy: String,
    /// Balanced deterministic work assignments.
    pub assignments: Vec<AgentWorkAssignment>,
}

/// Outcome metrics harvested for one assigned work item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentOutcomeSample {
    /// Work item this outcome describes.
    pub work_item_id: String,
    /// Variant that executed the work item.
    pub variant: AgentExperimentVariant,
    /// Whether the work item reached green proof.
    pub green_proof: bool,
    /// Time from assignment to green proof, if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_to_green_proof_ms: Option<u64>,
    /// Number of errors detected during the run.
    pub error_detection_count: u64,
    /// Number of model/API/resource limit events observed.
    pub limit_event_count: u64,
    /// Number of capture gap events observed.
    pub gap_event_count: u64,
    /// Redacted evidence refs backing this outcome.
    pub evidence_citations: Vec<String>,
}

/// Evaluation parameters for the first-pass A/B verdict scaffold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentExperimentEvaluationConfig {
    /// Minimum completed samples required per arm before a terminal verdict.
    pub min_completed_samples_per_variant: u64,
    /// Practical improvement threshold in basis points. For latency, lower is better.
    pub practical_improvement_bps: u32,
    /// Confidence carried by terminal verdicts until a full statistical gate is wired.
    pub confidence_bps: u32,
}

impl Default for AgentExperimentEvaluationConfig {
    fn default() -> Self {
        Self {
            min_completed_samples_per_variant: 2,
            practical_improvement_bps: 500,
            confidence_bps: 9_500,
        }
    }
}

/// Per-variant outcome aggregate used by the verdict receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentVariantOutcomeSummary {
    /// Variant summarized.
    pub variant: AgentExperimentVariant,
    /// Template label copied from the manifest.
    pub template_label: String,
    /// Number of outcome rows for this variant.
    pub observed_count: u64,
    /// Number of rows that reached green proof.
    pub green_proof_count: u64,
    /// Mean time to green proof over completed rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mean_time_to_green_proof_ms: Option<u64>,
    /// Total errors detected.
    pub error_detection_count: u64,
    /// Total limit events observed.
    pub limit_event_count: u64,
    /// Total capture gap events observed.
    pub gap_event_count: u64,
}

/// Compact, receipt-stable projection of a perf-gate decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentExperimentGateDecision {
    /// Stable gate decision kind (`accept`, `reject`, `continue`, etc.).
    pub kind: String,
    /// Operator-facing reason from the proof gate.
    pub reason: String,
    /// Confidence converted to basis points when the gate provides one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence_bps: Option<u32>,
}

/// Statistical gate evidence attached to every agent A/B receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentExperimentStatisticalGate {
    /// Gate implementation used to evaluate time-to-green-proof evidence.
    pub gate_id: String,
    /// Claim under evaluation.
    pub claim_id: String,
    /// Completed timed baseline samples consumed by the gate.
    pub baseline_sample_count: u64,
    /// Completed timed candidate samples consumed by the gate.
    pub candidate_sample_count: u64,
    /// Candidate must be at or below this latency to beat baseline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_better_threshold_ms: Option<u64>,
    /// Baseline must be at or below this latency to beat candidate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_better_threshold_ms: Option<u64>,
    /// Gate verdict for "candidate is faster than baseline".
    pub candidate_gate: AgentExperimentGateDecision,
    /// Gate verdict for "baseline is faster than candidate".
    pub baseline_gate: AgentExperimentGateDecision,
}

impl Default for AgentExperimentStatisticalGate {
    fn default() -> Self {
        Self {
            gate_id: "sprt_mean_threshold_v1".to_string(),
            claim_id: "agent_experiment.time_to_green_proof_ms".to_string(),
            baseline_sample_count: 0,
            candidate_sample_count: 0,
            candidate_better_threshold_ms: None,
            baseline_better_threshold_ms: None,
            candidate_gate: AgentExperimentGateDecision {
                kind: "continue".to_string(),
                reason: "statistical gate not evaluated".to_string(),
                confidence_bps: None,
            },
            baseline_gate: AgentExperimentGateDecision {
                kind: "continue".to_string(),
                reason: "statistical gate not evaluated".to_string(),
                confidence_bps: None,
            },
        }
    }
}

/// Verdict kind for an agent A/B experiment receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentExperimentVerdictKind {
    /// Candidate beats baseline under the configured practical threshold.
    CandidateBetter,
    /// Baseline beats candidate under the configured practical threshold.
    BaselineBetter,
    /// Evidence exists but has not crossed a terminal threshold.
    Inconclusive,
    /// Required evidence is missing or structurally blocked.
    EvidenceBlocked,
}

impl AgentExperimentVerdictKind {
    const fn token(self) -> &'static str {
        match self {
            Self::CandidateBetter => "candidate_better",
            Self::BaselineBetter => "baseline_better",
            Self::Inconclusive => "inconclusive",
            Self::EvidenceBlocked => "evidence_blocked",
        }
    }
}

/// Content-addressed receipt for an agent A/B experiment evaluation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentExperimentReceipt {
    /// Stable schema version.
    pub schema_version: String,
    /// Content-addressed receipt id over assignment, outcome, and verdict fields.
    pub receipt_id: String,
    /// Manifest experiment id.
    pub experiment_id: String,
    /// Baseline label copied from the manifest.
    pub baseline_label: String,
    /// Candidate label copied from the manifest.
    pub candidate_label: String,
    /// Evaluation configuration used for this receipt.
    pub evaluation_config: AgentExperimentEvaluationConfig,
    /// Assignment plan used by the run.
    pub assignments: Vec<AgentWorkAssignment>,
    /// Outcome rows consumed by the verdict.
    pub outcomes: Vec<AgentOutcomeSample>,
    /// Baseline aggregate.
    pub baseline_summary: AgentVariantOutcomeSummary,
    /// Candidate aggregate.
    pub candidate_summary: AgentVariantOutcomeSummary,
    /// Perf-gate evidence used by the verdict.
    #[serde(default)]
    pub statistical_gate: AgentExperimentStatisticalGate,
    /// Verdict kind.
    pub verdict: AgentExperimentVerdictKind,
    /// Operator-facing verdict reason.
    pub verdict_reason: String,
    /// Confidence attached to terminal verdicts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence_bps: Option<u32>,
}

/// Build a balanced, deterministic A/B assignment plan for equivalent work.
#[must_use]
pub fn build_agent_experiment_assignment_plan(
    manifest: &ExperimentManifest,
    work_item_ids: &[String],
) -> AgentExperimentAssignmentPlan {
    let mut work_items = work_item_ids.to_vec();
    work_items.sort();
    work_items.dedup();

    let offset = stable_partition_offset(&manifest.experiment_id);
    let assignments = work_items
        .into_iter()
        .enumerate()
        .map(|(idx, work_item_id)| {
            let variant = if (idx + offset) % 2 == 0 {
                AgentExperimentVariant::Baseline
            } else {
                AgentExperimentVariant::Candidate
            };
            AgentWorkAssignment {
                work_item_id,
                variant,
                template_label: variant.label(manifest).to_string(),
                assignment_reason: format!(
                    "balanced_deterministic_partition:index={idx}:offset={offset}"
                ),
            }
        })
        .collect();

    AgentExperimentAssignmentPlan {
        schema_version: AGENT_EXPERIMENT_RECEIPT_SCHEMA.to_string(),
        experiment_id: manifest.experiment_id.clone(),
        assignment_strategy: "balanced_deterministic_partition_v1".to_string(),
        assignments,
    }
}

/// Evaluate A/B outcomes and emit a content-addressed receipt.
#[must_use]
pub fn evaluate_agent_experiment_receipt(
    manifest: &ExperimentManifest,
    assignments: &[AgentWorkAssignment],
    outcomes: &[AgentOutcomeSample],
    evaluation_config: AgentExperimentEvaluationConfig,
) -> AgentExperimentReceipt {
    let mut sorted_assignments = assignments.to_vec();
    sorted_assignments.sort_by(|left, right| {
        left.work_item_id
            .cmp(&right.work_item_id)
            .then(left.variant.cmp(&right.variant))
    });

    let mut sorted_outcomes = outcomes.to_vec();
    sorted_outcomes.sort_by(|left, right| {
        left.work_item_id
            .cmp(&right.work_item_id)
            .then(left.variant.cmp(&right.variant))
    });

    let baseline_summary =
        summarize_variant_outcomes(manifest, &sorted_outcomes, AgentExperimentVariant::Baseline);
    let candidate_summary = summarize_variant_outcomes(
        manifest,
        &sorted_outcomes,
        AgentExperimentVariant::Candidate,
    );
    let (verdict, verdict_reason, confidence_bps, statistical_gate) =
        decide_agent_experiment_verdict(
            &sorted_outcomes,
            &baseline_summary,
            &candidate_summary,
            evaluation_config,
        );

    let mut receipt = AgentExperimentReceipt {
        schema_version: AGENT_EXPERIMENT_RECEIPT_SCHEMA.to_string(),
        receipt_id: String::new(),
        experiment_id: manifest.experiment_id.clone(),
        baseline_label: manifest.baseline_label.clone(),
        candidate_label: manifest.candidate_label.clone(),
        evaluation_config,
        assignments: sorted_assignments,
        outcomes: sorted_outcomes,
        baseline_summary,
        candidate_summary,
        statistical_gate,
        verdict,
        verdict_reason,
        confidence_bps,
    };
    receipt.receipt_id = agent_experiment_receipt_id(&receipt);
    receipt
}

/// br-ft-1650n.12: harness. Owns the ledger and exposes the
/// append-only API. The type signature CANNOT take a
/// `&mut PaneState` argument — the no-side-effect contract is
/// type-level.
#[derive(Debug, Clone)]
pub struct ShadowExperimentHarness {
    ledger: ExperimentLedger,
    aborted: bool,
}

impl ShadowExperimentHarness {
    /// New harness with an empty ledger.
    #[must_use]
    pub fn new(manifest: ExperimentManifest) -> Self {
        Self {
            ledger: ExperimentLedger {
                schema_version: SHADOW_EXPERIMENT_LEDGER_SCHEMA.to_string(),
                manifest,
                decisions: Vec::new(),
                sampling_gaps: 0,
                abort_reason: None,
                total_overhead_us: 0,
            },
            aborted: false,
        }
    }

    /// Record a shadow decision pair. Returns `true` if recorded,
    /// `false` if the experiment had already aborted (in which
    /// case `sampling_gaps` is incremented).
    pub fn record_pair(&mut self, pair: ShadowDecisionPair) -> bool {
        if self.aborted {
            self.ledger.sampling_gaps = self.ledger.sampling_gaps.saturating_add(1);
            return false;
        }

        // Per-event overhead gate.
        if pair.overhead_us > self.ledger.manifest.budget.max_overhead_us_per_event {
            self.aborted = true;
            self.ledger.abort_reason = Some(ExperimentAbortReason::OverheadExceeded {
                sample_event_id: pair.event_id.clone(),
                observed_us: pair.overhead_us,
            });
            return false;
        }

        // Cumulative overhead gate.
        let new_total = self
            .ledger
            .total_overhead_us
            .saturating_add(pair.overhead_us);
        if new_total > self.ledger.manifest.budget.max_total_overhead_us {
            self.aborted = true;
            self.ledger.abort_reason = Some(ExperimentAbortReason::TotalOverheadExceeded {
                observed_us: new_total,
            });
            return false;
        }

        // Events cap gate.
        let decision_count = u64::try_from(self.ledger.decisions.len()).unwrap_or(u64::MAX);
        if decision_count >= self.ledger.manifest.budget.max_events {
            self.aborted = true;
            self.ledger.abort_reason = Some(ExperimentAbortReason::EventsCapReached);
            return false;
        }

        self.ledger.total_overhead_us = new_total;
        self.ledger.decisions.push(pair);
        true
    }

    /// Operator-initiated abort. Records the reason on the
    /// ledger; subsequent `record_pair` calls bump
    /// `sampling_gaps` instead of growing the ledger.
    pub fn abort_manually(&mut self, reason: impl Into<String>) {
        if self.aborted {
            return;
        }
        self.aborted = true;
        self.ledger.abort_reason = Some(ExperimentAbortReason::ManualStop {
            reason: reason.into(),
        });
    }

    /// Pure read of the ledger.
    #[must_use]
    pub fn ledger(&self) -> &ExperimentLedger {
        &self.ledger
    }

    /// Whether the experiment has aborted.
    #[must_use]
    pub fn is_aborted(&self) -> bool {
        self.aborted
    }
}

/// Convenience: convert a `Duration` to microseconds saturating
/// to `u64::MAX`. Useful for callers measuring overhead with
/// `Instant::elapsed()`.
#[must_use]
pub fn duration_to_us(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

fn stable_partition_offset(experiment_id: &str) -> usize {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in experiment_id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    if hash & 1 == 0 {
        0
    } else {
        1
    }
}

fn summarize_variant_outcomes(
    manifest: &ExperimentManifest,
    outcomes: &[AgentOutcomeSample],
    variant: AgentExperimentVariant,
) -> AgentVariantOutcomeSummary {
    let mut observed_count = 0_u64;
    let mut green_proof_count = 0_u64;
    let mut total_time_ms = 0_u128;
    let mut timed_green_count = 0_u64;
    let mut error_detection_count = 0_u64;
    let mut limit_event_count = 0_u64;
    let mut gap_event_count = 0_u64;

    for outcome in outcomes.iter().filter(|outcome| outcome.variant == variant) {
        observed_count = observed_count.saturating_add(1);
        error_detection_count = error_detection_count.saturating_add(outcome.error_detection_count);
        limit_event_count = limit_event_count.saturating_add(outcome.limit_event_count);
        gap_event_count = gap_event_count.saturating_add(outcome.gap_event_count);
        if outcome.green_proof {
            green_proof_count = green_proof_count.saturating_add(1);
            if let Some(time_ms) = outcome.time_to_green_proof_ms {
                total_time_ms = total_time_ms.saturating_add(u128::from(time_ms));
                timed_green_count = timed_green_count.saturating_add(1);
            }
        }
    }

    let mean_time_to_green_proof_ms = if timed_green_count == 0 {
        None
    } else {
        Some(u64::try_from(total_time_ms / u128::from(timed_green_count)).unwrap_or(u64::MAX))
    };

    AgentVariantOutcomeSummary {
        variant,
        template_label: variant.label(manifest).to_string(),
        observed_count,
        green_proof_count,
        mean_time_to_green_proof_ms,
        error_detection_count,
        limit_event_count,
        gap_event_count,
    }
}

fn decide_agent_experiment_verdict(
    outcomes: &[AgentOutcomeSample],
    baseline: &AgentVariantOutcomeSummary,
    candidate: &AgentVariantOutcomeSummary,
    config: AgentExperimentEvaluationConfig,
) -> (
    AgentExperimentVerdictKind,
    String,
    Option<u32>,
    AgentExperimentStatisticalGate,
) {
    let statistical_gate = build_agent_experiment_statistical_gate(outcomes, config);

    if baseline.green_proof_count < config.min_completed_samples_per_variant
        || candidate.green_proof_count < config.min_completed_samples_per_variant
    {
        return (
            AgentExperimentVerdictKind::Inconclusive,
            format!(
                "need at least {} completed samples per variant; observed baseline={} candidate={}",
                config.min_completed_samples_per_variant,
                baseline.green_proof_count,
                candidate.green_proof_count
            ),
            None,
            statistical_gate,
        );
    }

    let Some(baseline_mean) = baseline.mean_time_to_green_proof_ms else {
        return (
            AgentExperimentVerdictKind::EvidenceBlocked,
            "baseline completed samples lack time-to-green-proof evidence".to_string(),
            None,
            statistical_gate,
        );
    };
    let Some(candidate_mean) = candidate.mean_time_to_green_proof_ms else {
        return (
            AgentExperimentVerdictKind::EvidenceBlocked,
            "candidate completed samples lack time-to-green-proof evidence".to_string(),
            None,
            statistical_gate,
        );
    };

    if statistical_gate.candidate_gate.kind == "accept" {
        return (
            AgentExperimentVerdictKind::CandidateBetter,
            format!(
                "candidate time-to-green-proof gate accepted: candidate mean {candidate_mean}ms beats baseline {baseline_mean}ms by configured threshold"
            ),
            statistical_gate.candidate_gate.confidence_bps,
            statistical_gate,
        );
    }
    if statistical_gate.baseline_gate.kind == "accept" {
        return (
            AgentExperimentVerdictKind::BaselineBetter,
            format!(
                "baseline time-to-green-proof gate accepted: baseline mean {baseline_mean}ms beats candidate {candidate_mean}ms by configured threshold"
            ),
            statistical_gate.baseline_gate.confidence_bps,
            statistical_gate,
        );
    }

    (
        AgentExperimentVerdictKind::Inconclusive,
        format!(
            "time-to-green-proof gates did not accept either variant; candidate_gate={} baseline_gate={}",
            statistical_gate.candidate_gate.kind, statistical_gate.baseline_gate.kind
        ),
        None,
        statistical_gate,
    )
}

fn build_agent_experiment_statistical_gate(
    outcomes: &[AgentOutcomeSample],
    config: AgentExperimentEvaluationConfig,
) -> AgentExperimentStatisticalGate {
    let baseline_times =
        timed_green_samples_for_variant(outcomes, AgentExperimentVariant::Baseline);
    let candidate_times =
        timed_green_samples_for_variant(outcomes, AgentExperimentVariant::Candidate);
    let baseline_mean = mean_u64(&baseline_times);
    let candidate_mean = mean_u64(&candidate_times);
    let candidate_threshold =
        baseline_mean.map(|mean| practical_threshold_ms(mean, config.practical_improvement_bps));
    let baseline_threshold =
        candidate_mean.map(|mean| practical_threshold_ms(mean, config.practical_improvement_bps));

    let min_samples = usize::try_from(config.min_completed_samples_per_variant)
        .unwrap_or(usize::MAX)
        .max(1);
    let confidence = Some(f64::from(config.confidence_bps) / 10_000.0);

    let candidate_gate = if let Some(threshold) = candidate_threshold {
        let samples = evidence_samples(
            "agent_experiment.time_to_green_proof_ms.candidate",
            &candidate_times,
        );
        let cfg = SprtConfig {
            baseline: u64_to_f64(threshold),
            relative_threshold: 0.0,
            min_samples,
            confidence,
        };
        decision_projection(&sprt::evaluate_samples(&samples, &cfg).decision)
    } else {
        AgentExperimentGateDecision {
            kind: "continue".to_string(),
            reason:
                "baseline timed green-proof samples are required before candidate gate evaluation"
                    .to_string(),
            confidence_bps: None,
        }
    };

    let baseline_gate = if let Some(threshold) = baseline_threshold {
        let samples = evidence_samples(
            "agent_experiment.time_to_green_proof_ms.baseline",
            &baseline_times,
        );
        let cfg = SprtConfig {
            baseline: u64_to_f64(threshold),
            relative_threshold: 0.0,
            min_samples,
            confidence,
        };
        decision_projection(&sprt::evaluate_samples(&samples, &cfg).decision)
    } else {
        AgentExperimentGateDecision {
            kind: "continue".to_string(),
            reason:
                "candidate timed green-proof samples are required before baseline gate evaluation"
                    .to_string(),
            confidence_bps: None,
        }
    };

    AgentExperimentStatisticalGate {
        gate_id: "sprt_mean_threshold_v1".to_string(),
        claim_id: "agent_experiment.time_to_green_proof_ms".to_string(),
        baseline_sample_count: u64::try_from(baseline_times.len()).unwrap_or(u64::MAX),
        candidate_sample_count: u64::try_from(candidate_times.len()).unwrap_or(u64::MAX),
        candidate_better_threshold_ms: candidate_threshold,
        baseline_better_threshold_ms: baseline_threshold,
        candidate_gate,
        baseline_gate,
    }
}

fn timed_green_samples_for_variant(
    outcomes: &[AgentOutcomeSample],
    variant: AgentExperimentVariant,
) -> Vec<u64> {
    outcomes
        .iter()
        .filter(|outcome| outcome.variant == variant && outcome.green_proof)
        .filter_map(|outcome| outcome.time_to_green_proof_ms)
        .collect()
}

fn mean_u64(values: &[u64]) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    let total = values
        .iter()
        .fold(0_u128, |acc, value| acc.saturating_add(u128::from(*value)));
    let len = u128::try_from(values.len()).unwrap_or(u128::MAX);
    Some(u64::try_from(total / len).unwrap_or(u64::MAX))
}

fn practical_threshold_ms(rhs_ms: u64, improvement_bps: u32) -> u64 {
    if improvement_bps >= u32::try_from(BASIS_POINTS_DENOMINATOR).unwrap_or(u32::MAX) {
        return 0;
    }
    let required_numerator = BASIS_POINTS_DENOMINATOR.saturating_sub(u128::from(improvement_bps));
    let threshold =
        u128::from(rhs_ms).saturating_mul(required_numerator) / BASIS_POINTS_DENOMINATOR;
    u64::try_from(threshold).unwrap_or(u64::MAX)
}

fn evidence_samples(claim_id: &str, times_ms: &[u64]) -> Vec<EvidenceSample> {
    times_ms
        .iter()
        .enumerate()
        .map(|(idx, time_ms)| {
            EvidenceSample::new(
                claim_id,
                u64_to_f64(*time_ms),
                "ms",
                1,
                u64::try_from(idx.saturating_add(1)).unwrap_or(u64::MAX),
            )
        })
        .collect()
}

fn decision_projection(decision: &GateDecision) -> AgentExperimentGateDecision {
    AgentExperimentGateDecision {
        kind: decision.kind().to_string(),
        reason: decision.reason().to_string(),
        confidence_bps: decision_confidence_bps(decision),
    }
}

fn decision_confidence_bps(decision: &GateDecision) -> Option<u32> {
    let confidence = match decision {
        GateDecision::Accept { confidence, .. }
        | GateDecision::Reject { confidence, .. }
        | GateDecision::LowConfidence { confidence, .. } => *confidence,
        GateDecision::Continue { .. } | GateDecision::RegimeShift { .. } => None,
    }?;
    if !confidence.is_finite() || confidence < 0.0 {
        return None;
    }
    let bps = (confidence.min(1.0) * 10_000.0).round();
    rounded_f64_to_u32(bps)
}

fn u64_to_f64(value: u64) -> f64 {
    value.to_string().parse::<f64>().unwrap_or(f64::INFINITY)
}

fn rounded_f64_to_u32(value: f64) -> Option<u32> {
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    format!("{value:.0}").parse::<u32>().ok()
}

fn agent_experiment_receipt_id(receipt: &AgentExperimentReceipt) -> String {
    let mut hasher = Sha256::new();
    hash_string(&mut hasher, &receipt.schema_version);
    hash_string(&mut hasher, &receipt.experiment_id);
    hash_string(&mut hasher, &receipt.baseline_label);
    hash_string(&mut hasher, &receipt.candidate_label);
    hash_string(
        &mut hasher,
        &receipt
            .evaluation_config
            .min_completed_samples_per_variant
            .to_string(),
    );
    hash_string(
        &mut hasher,
        &receipt
            .evaluation_config
            .practical_improvement_bps
            .to_string(),
    );
    hash_string(
        &mut hasher,
        &receipt.evaluation_config.confidence_bps.to_string(),
    );
    hash_statistical_gate(&mut hasher, &receipt.statistical_gate);
    for assignment in &receipt.assignments {
        hash_string(&mut hasher, &assignment.work_item_id);
        hash_string(&mut hasher, assignment.variant.token());
        hash_string(&mut hasher, &assignment.template_label);
        hash_string(&mut hasher, &assignment.assignment_reason);
    }
    for outcome in &receipt.outcomes {
        hash_string(&mut hasher, &outcome.work_item_id);
        hash_string(&mut hasher, outcome.variant.token());
        hash_string(&mut hasher, if outcome.green_proof { "1" } else { "0" });
        hash_string(
            &mut hasher,
            &outcome
                .time_to_green_proof_ms
                .map_or_else(|| "none".to_string(), |value| value.to_string()),
        );
        hash_string(&mut hasher, &outcome.error_detection_count.to_string());
        hash_string(&mut hasher, &outcome.limit_event_count.to_string());
        hash_string(&mut hasher, &outcome.gap_event_count.to_string());
        for citation in &outcome.evidence_citations {
            hash_string(&mut hasher, citation);
        }
    }
    hash_string(&mut hasher, receipt.verdict.token());
    hash_string(&mut hasher, &receipt.verdict_reason);
    hash_string(
        &mut hasher,
        &receipt
            .confidence_bps
            .map_or_else(|| "none".to_string(), |value| value.to_string()),
    );
    format!("agent-exp:{}", hex::encode(hasher.finalize()))
}

fn hash_statistical_gate(hasher: &mut Sha256, gate: &AgentExperimentStatisticalGate) {
    hash_string(hasher, &gate.gate_id);
    hash_string(hasher, &gate.claim_id);
    hash_string(hasher, &gate.baseline_sample_count.to_string());
    hash_string(hasher, &gate.candidate_sample_count.to_string());
    hash_option_u64(hasher, gate.candidate_better_threshold_ms);
    hash_option_u64(hasher, gate.baseline_better_threshold_ms);
    hash_gate_decision(hasher, &gate.candidate_gate);
    hash_gate_decision(hasher, &gate.baseline_gate);
}

fn hash_gate_decision(hasher: &mut Sha256, decision: &AgentExperimentGateDecision) {
    hash_string(hasher, &decision.kind);
    hash_string(hasher, &decision.reason);
    hash_string(
        hasher,
        &decision
            .confidence_bps
            .map_or_else(|| "none".to_string(), |value| value.to_string()),
    );
}

fn hash_option_u64(hasher: &mut Sha256, value: Option<u64>) {
    hash_string(
        hasher,
        &value.map_or_else(|| "none".to_string(), |value| value.to_string()),
    );
}

fn hash_string(hasher: &mut Sha256, value: &str) {
    hasher.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_le_bytes());
    hasher.update(value.as_bytes());
}

#[cfg(test)]
mod tests {
    use crate::test_fixtures::synthetic_swarm::{synthetic_swarm_scenario, SyntheticSwarmScale};

    use super::*;

    fn manifest(budget: ExperimentBudget) -> ExperimentManifest {
        ExperimentManifest {
            experiment_id: "exp-1".to_string(),
            baseline_label: "baseline".to_string(),
            candidate_label: "candidate".to_string(),
            budget,
            redaction_policy_version: "redaction-v1".to_string(),
        }
    }

    fn pair(event_id: &str, divergence: DivergenceKind, overhead_us: u64) -> ShadowDecisionPair {
        ShadowDecisionPair {
            event_id: event_id.to_string(),
            baseline_decision: "b".to_string(),
            candidate_decision: "c".to_string(),
            divergence,
            overhead_us,
            evidence_citations: vec!["pattern_hit#42".to_string()],
        }
    }

    fn outcome(
        work_item_id: &str,
        variant: AgentExperimentVariant,
        time_to_green_proof_ms: Option<u64>,
    ) -> AgentOutcomeSample {
        AgentOutcomeSample {
            work_item_id: work_item_id.to_string(),
            variant,
            green_proof: time_to_green_proof_ms.is_some(),
            time_to_green_proof_ms,
            error_detection_count: 1,
            limit_event_count: 0,
            gap_event_count: 0,
            evidence_citations: vec![format!("work:{work_item_id}")],
        }
    }

    /// New harness has an empty ledger with the documented
    /// schema version and no abort.
    #[test]
    fn new_harness_starts_empty() {
        let h = ShadowExperimentHarness::new(manifest(ExperimentBudget::conservative()));
        assert_eq!(h.ledger().schema_version, SHADOW_EXPERIMENT_LEDGER_SCHEMA);
        assert!(h.ledger().decisions.is_empty());
        assert_eq!(h.ledger().sampling_gaps, 0);
        assert!(h.ledger().abort_reason.is_none());
        assert!(!h.is_aborted());
    }

    /// Recording a single matching pair appends to the ledger
    /// and returns `true`.
    #[test]
    fn record_match_pair_appends() {
        let mut h = ShadowExperimentHarness::new(manifest(ExperimentBudget::conservative()));
        assert!(h.record_pair(pair("e1", DivergenceKind::Match, 100)));
        assert_eq!(h.ledger().decisions.len(), 1);
        assert_eq!(h.ledger().total_overhead_us, 100);
    }

    /// Divergence counts aggregate over the ledger.
    #[test]
    fn divergence_counts_aggregate() {
        let mut h = ShadowExperimentHarness::new(manifest(ExperimentBudget::conservative()));
        h.record_pair(pair("e1", DivergenceKind::Match, 100));
        h.record_pair(pair("e2", DivergenceKind::Match, 100));
        h.record_pair(pair("e3", DivergenceKind::Minor, 100));
        h.record_pair(pair("e4", DivergenceKind::Major, 100));
        let counts = h.ledger().divergence_counts();
        assert_eq!(counts.match_count, 2);
        assert_eq!(counts.minor_count, 1);
        assert_eq!(counts.major_count, 1);
    }

    #[test]
    fn agent_assignment_plan_is_balanced_and_order_independent() {
        let manifest = manifest(ExperimentBudget::conservative());
        let work_items = vec![
            "ft-c".to_string(),
            "ft-a".to_string(),
            "ft-b".to_string(),
            "ft-a".to_string(),
        ];
        let reversed = vec![
            "ft-a".to_string(),
            "ft-b".to_string(),
            "ft-a".to_string(),
            "ft-c".to_string(),
        ];

        let plan = build_agent_experiment_assignment_plan(&manifest, &work_items);
        let plan_again = build_agent_experiment_assignment_plan(&manifest, &reversed);

        assert_eq!(plan, plan_again);
        assert_eq!(plan.schema_version, AGENT_EXPERIMENT_RECEIPT_SCHEMA);
        assert_eq!(plan.assignments.len(), 3);
        let baseline_count = plan
            .assignments
            .iter()
            .filter(|assignment| assignment.variant == AgentExperimentVariant::Baseline)
            .count();
        let candidate_count = plan
            .assignments
            .iter()
            .filter(|assignment| assignment.variant == AgentExperimentVariant::Candidate)
            .count();
        assert!(baseline_count.abs_diff(candidate_count) <= 1);
        assert!(plan
            .assignments
            .iter()
            .all(|assignment| !assignment.assignment_reason.is_empty()));
    }

    #[test]
    fn agent_receipt_reports_candidate_better_and_stable_id() {
        let manifest = manifest(ExperimentBudget::conservative());
        let work_items = vec![
            "ft-a".to_string(),
            "ft-b".to_string(),
            "ft-c".to_string(),
            "ft-d".to_string(),
        ];
        let plan = build_agent_experiment_assignment_plan(&manifest, &work_items);
        let outcomes = vec![
            outcome("baseline-1", AgentExperimentVariant::Baseline, Some(1_000)),
            outcome("baseline-2", AgentExperimentVariant::Baseline, Some(1_100)),
            outcome("candidate-1", AgentExperimentVariant::Candidate, Some(800)),
            outcome("candidate-2", AgentExperimentVariant::Candidate, Some(790)),
        ];
        let mut reversed_outcomes = outcomes.clone();
        reversed_outcomes.reverse();
        let mut reversed_assignments = plan.assignments.clone();
        reversed_assignments.reverse();

        let receipt = evaluate_agent_experiment_receipt(
            &manifest,
            &plan.assignments,
            &outcomes,
            AgentExperimentEvaluationConfig::default(),
        );
        let receipt_again = evaluate_agent_experiment_receipt(
            &manifest,
            &reversed_assignments,
            &reversed_outcomes,
            AgentExperimentEvaluationConfig::default(),
        );

        assert_eq!(receipt.receipt_id, receipt_again.receipt_id);
        assert!(receipt.receipt_id.starts_with("agent-exp:"));
        assert_eq!(receipt.verdict, AgentExperimentVerdictKind::CandidateBetter);
        assert_eq!(receipt.confidence_bps, Some(9_500));
        assert_eq!(receipt.baseline_summary.green_proof_count, 2);
        assert_eq!(receipt.candidate_summary.green_proof_count, 2);
        assert_eq!(receipt.baseline_summary.error_detection_count, 2);
        assert_eq!(receipt.statistical_gate.gate_id, "sprt_mean_threshold_v1");
        assert_eq!(receipt.statistical_gate.baseline_sample_count, 2);
        assert_eq!(receipt.statistical_gate.candidate_sample_count, 2);
        assert_eq!(
            receipt.statistical_gate.candidate_better_threshold_ms,
            Some(945)
        );
        assert_eq!(receipt.statistical_gate.candidate_gate.kind, "accept");
        assert_eq!(receipt.statistical_gate.baseline_gate.kind, "reject");
    }

    #[test]
    fn agent_receipt_stays_inconclusive_until_minimum_completed_samples() {
        let manifest = manifest(ExperimentBudget::conservative());
        let outcomes = vec![
            outcome("baseline-1", AgentExperimentVariant::Baseline, Some(1_000)),
            outcome("candidate-1", AgentExperimentVariant::Candidate, Some(500)),
        ];

        let receipt = evaluate_agent_experiment_receipt(
            &manifest,
            &[],
            &outcomes,
            AgentExperimentEvaluationConfig::default(),
        );

        assert_eq!(receipt.verdict, AgentExperimentVerdictKind::Inconclusive);
        assert!(receipt.confidence_bps.is_none());
        assert!(receipt.verdict_reason.contains("need at least 2"));
    }

    #[test]
    fn agent_receipt_blocks_when_time_evidence_missing() {
        let manifest = manifest(ExperimentBudget::conservative());
        let outcomes = vec![
            AgentOutcomeSample {
                work_item_id: "baseline-1".to_string(),
                variant: AgentExperimentVariant::Baseline,
                green_proof: true,
                time_to_green_proof_ms: None,
                error_detection_count: 0,
                limit_event_count: 1,
                gap_event_count: 1,
                evidence_citations: vec!["missing-clock".to_string()],
            },
            AgentOutcomeSample {
                work_item_id: "baseline-2".to_string(),
                variant: AgentExperimentVariant::Baseline,
                green_proof: true,
                time_to_green_proof_ms: None,
                error_detection_count: 0,
                limit_event_count: 0,
                gap_event_count: 1,
                evidence_citations: vec!["missing-clock".to_string()],
            },
            outcome("candidate-1", AgentExperimentVariant::Candidate, Some(500)),
            outcome("candidate-2", AgentExperimentVariant::Candidate, Some(520)),
        ];

        let receipt = evaluate_agent_experiment_receipt(
            &manifest,
            &[],
            &outcomes,
            AgentExperimentEvaluationConfig::default(),
        );

        assert_eq!(receipt.verdict, AgentExperimentVerdictKind::EvidenceBlocked);
        assert_eq!(receipt.baseline_summary.limit_event_count, 1);
        assert_eq!(receipt.baseline_summary.gap_event_count, 2);
        assert_eq!(receipt.statistical_gate.baseline_sample_count, 0);
        assert_eq!(receipt.statistical_gate.candidate_sample_count, 2);
        assert_eq!(receipt.statistical_gate.candidate_gate.kind, "continue");
    }

    /// Per-event overhead exceeding the budget aborts with
    /// `OverheadExceeded` and the offending event_id is captured.
    #[test]
    fn per_event_overhead_aborts() {
        let mut h = ShadowExperimentHarness::new(manifest(ExperimentBudget {
            max_overhead_us_per_event: 1_000,
            ..ExperimentBudget::conservative()
        }));
        h.record_pair(pair("e1", DivergenceKind::Match, 500));
        assert!(!h.is_aborted());
        // Spike that exceeds per-event cap.
        let recorded = h.record_pair(pair("e2-spike", DivergenceKind::Match, 5_000));
        assert!(!recorded);
        assert!(h.is_aborted());
        let abort_reason = h.ledger().abort_reason.as_ref();
        assert!(matches!(
            abort_reason,
            Some(ExperimentAbortReason::OverheadExceeded { .. })
        ));
        if let Some(ExperimentAbortReason::OverheadExceeded {
            sample_event_id,
            observed_us,
        }) = abort_reason
        {
            assert_eq!(sample_event_id, "e2-spike");
            assert_eq!(*observed_us, 5_000);
        }
        // The spike sample is NOT in the ledger (over budget).
        assert_eq!(h.ledger().decisions.len(), 1);
    }

    /// Cumulative overhead exceeding the total budget aborts.
    #[test]
    fn cumulative_overhead_aborts() {
        let mut h = ShadowExperimentHarness::new(manifest(ExperimentBudget {
            max_overhead_us_per_event: 10_000,
            max_total_overhead_us: 5_000,
            ..ExperimentBudget::conservative()
        }));
        h.record_pair(pair("e1", DivergenceKind::Match, 2_000));
        h.record_pair(pair("e2", DivergenceKind::Match, 2_000));
        // Third would push cumulative past 5_000.
        let recorded = h.record_pair(pair("e3", DivergenceKind::Match, 2_000));
        assert!(!recorded);
        assert!(h.is_aborted());
        assert!(matches!(
            h.ledger().abort_reason.as_ref(),
            Some(ExperimentAbortReason::TotalOverheadExceeded { .. })
        ));
    }

    /// Events-cap reached: ledger holds exactly `max_events`
    /// entries; the (max+1)th sample bumps `sampling_gaps`.
    #[test]
    fn events_cap_reached_aborts() {
        let mut h = ShadowExperimentHarness::new(manifest(ExperimentBudget {
            max_events: 3,
            ..ExperimentBudget::conservative()
        }));
        for i in 0..3 {
            h.record_pair(pair(&format!("e{i}"), DivergenceKind::Match, 100));
        }
        assert_eq!(h.ledger().decisions.len(), 3);
        // 4th event triggers cap.
        let recorded = h.record_pair(pair("e4", DivergenceKind::Match, 100));
        assert!(!recorded);
        assert!(h.is_aborted());
        assert!(matches!(
            h.ledger().abort_reason.as_ref(),
            Some(ExperimentAbortReason::EventsCapReached)
        ));
    }

    /// Post-abort `record_pair` calls bump `sampling_gaps`. The
    /// bead's "record sampling gaps and disable itself" fallback.
    #[test]
    fn post_abort_records_sampling_gaps() {
        let mut h = ShadowExperimentHarness::new(manifest(ExperimentBudget {
            max_events: 1,
            ..ExperimentBudget::conservative()
        }));
        h.record_pair(pair("e1", DivergenceKind::Match, 100));
        // e2 trips the cap (aborts during the call, not counted
        // as a gap). e3, e4, e5 are post-abort attempts.
        h.record_pair(pair("e2", DivergenceKind::Match, 100));
        h.record_pair(pair("e3", DivergenceKind::Match, 100));
        h.record_pair(pair("e4", DivergenceKind::Match, 100));
        h.record_pair(pair("e5", DivergenceKind::Match, 100));
        assert!(h.is_aborted());
        // 3 post-abort attempts -> 3 gaps.
        assert_eq!(h.ledger().sampling_gaps, 3);
    }

    /// `abort_manually` records a typed ManualStop reason and
    /// is idempotent.
    #[test]
    fn abort_manually_records_reason() {
        let mut h = ShadowExperimentHarness::new(manifest(ExperimentBudget::conservative()));
        h.record_pair(pair("e1", DivergenceKind::Match, 100));
        h.abort_manually("operator decision");
        assert!(h.is_aborted());
        let first_reason = h.ledger().abort_reason.as_ref();
        assert!(matches!(
            first_reason,
            Some(ExperimentAbortReason::ManualStop { reason }) if reason == "operator decision"
        ));
        // Idempotent.
        h.abort_manually("another");
        let second_reason = h.ledger().abort_reason.as_ref();
        assert!(matches!(
            second_reason,
            Some(ExperimentAbortReason::ManualStop { reason }) if reason == "operator decision"
        ));
    }

    /// Evidence citations are preserved on every recorded pair
    /// (the bead's "cite causal/evidence sources" criterion).
    #[test]
    fn evidence_citations_preserved() {
        let mut h = ShadowExperimentHarness::new(manifest(ExperimentBudget::conservative()));
        let mut p = pair("e1", DivergenceKind::Major, 100);
        p.evidence_citations = vec![
            "causal_node#7".to_string(),
            "policy_decision#12".to_string(),
        ];
        h.record_pair(p);
        let citations = h
            .ledger()
            .decisions
            .first()
            .map(|entry| &entry.evidence_citations);
        assert!(matches!(citations, Some(citations) if citations.len() == 2));
        assert!(matches!(
            citations,
            Some(citations) if citations.iter().any(|c| c == "causal_node#7")
        ));
    }

    /// Replay-style synthetic 50-pane proof: candidate decisions
    /// diverge from baseline while the source pane inventory remains
    /// unchanged and the overhead budget stays bounded.
    #[test]
    fn synthetic_fleet50_replay_logs_divergence_without_pane_mutation() {
        let scenario = synthetic_swarm_scenario(SyntheticSwarmScale::Fleet50);
        let original_panes = scenario.pane_scripts.clone();
        let mut h = ShadowExperimentHarness::new(manifest(ExperimentBudget {
            max_events: 100,
            max_overhead_us_per_event: 500,
            max_total_overhead_us: 5_000,
        }));

        for (idx, pane) in scenario.pane_scripts.iter().enumerate() {
            let divergence = if idx % 5 == 0 {
                DivergenceKind::Major
            } else {
                DivergenceKind::Match
            };
            let Some(event_id) = pane.event_ids.first() else {
                continue;
            };
            assert!(h.record_pair(ShadowDecisionPair {
                event_id: event_id.clone(),
                baseline_decision: "baseline:admit".to_string(),
                candidate_decision: if divergence == DivergenceKind::Major {
                    "candidate:throttle".to_string()
                } else {
                    "baseline:admit".to_string()
                },
                divergence,
                overhead_us: 40,
                evidence_citations: vec![
                    format!("event:{event_id}"),
                    format!("pane:{}", pane.pane_id),
                ],
            }));
        }

        let counts = h.ledger().divergence_counts();
        assert_eq!(scenario.manifest.pane_count, 50);
        assert_eq!(h.ledger().decisions.len(), 50);
        assert_eq!(counts.major_count, 10);
        assert_eq!(counts.match_count, 40);
        assert_eq!(h.ledger().sampling_gaps, 0);
        assert_eq!(h.ledger().total_overhead_us, 2_000);
        assert_eq!(scenario.pane_scripts, original_panes);
        assert!(h.ledger().decisions.iter().all(|entry| {
            entry
                .evidence_citations
                .iter()
                .any(|citation| citation.starts_with("event:"))
        }));
    }

    /// Ledger serde roundtrip preserves every field including
    /// abort reason variants.
    #[test]
    fn ledger_serde_roundtrip() {
        let mut h = ShadowExperimentHarness::new(manifest(ExperimentBudget::conservative()));
        h.record_pair(pair("e1", DivergenceKind::Match, 100));
        h.record_pair(pair("e2", DivergenceKind::Major, 200));
        h.abort_manually("end");
        let ledger = h.ledger().clone();
        let json_result = serde_json::to_string(&ledger);
        assert!(json_result.is_ok());
        let Ok(json) = json_result else {
            return;
        };
        let back = serde_json::from_str::<ExperimentLedger>(&json);
        assert!(matches!(back, Ok(back) if back == ledger));
    }

    /// ExperimentAbortReason serde roundtrips for every variant.
    #[test]
    fn abort_reason_serde_roundtrip() {
        let reasons = vec![
            ExperimentAbortReason::EventsCapReached,
            ExperimentAbortReason::OverheadExceeded {
                sample_event_id: "e1".to_string(),
                observed_us: 5_000,
            },
            ExperimentAbortReason::TotalOverheadExceeded {
                observed_us: 30_000_000,
            },
            ExperimentAbortReason::ManualStop {
                reason: "x".to_string(),
            },
        ];
        for r in reasons {
            let json_result = serde_json::to_string(&r);
            assert!(json_result.is_ok());
            let Ok(json) = json_result else {
                continue;
            };
            let back = serde_json::from_str::<ExperimentAbortReason>(&json);
            assert!(matches!(back, Ok(back) if back == r));
        }
    }

    /// `duration_to_us` saturates at `u64::MAX` for absurd
    /// durations.
    #[test]
    fn duration_to_us_saturates() {
        assert_eq!(duration_to_us(Duration::from_micros(123)), 123);
        // u128 microseconds that exceed u64 → saturates.
        let huge = Duration::from_secs(u64::MAX);
        assert_eq!(duration_to_us(huge), u64::MAX);
    }

    /// No-side-effect contract: the harness's public API
    /// signature does not accept any `&mut` to anything other
    /// than itself. This compiles only if that contract holds —
    /// the test is the type-system pin.
    #[test]
    fn no_side_effect_api_shape() {
        // Compile-time check: the closures below must type-check.
        let _: fn(&mut ShadowExperimentHarness, ShadowDecisionPair) -> bool =
            ShadowExperimentHarness::record_pair;
        let _: fn(&ShadowExperimentHarness) -> &ExperimentLedger = ShadowExperimentHarness::ledger;
        let _: fn(&ShadowExperimentHarness) -> bool = ShadowExperimentHarness::is_aborted;
    }
}
