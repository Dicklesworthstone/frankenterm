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

use serde::{Deserialize, Serialize};

/// Stable schema version for `ExperimentLedger` exports.
pub const SHADOW_EXPERIMENT_LEDGER_SCHEMA: &str = "ft.shadow_experiment.ledger.v1";

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
        if self.ledger.decisions.len() as u64 >= self.ledger.manifest.budget.max_events {
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

#[cfg(test)]
mod tests {
    use crate::test_fixtures::synthetic_swarm::{SyntheticSwarmScale, synthetic_swarm_scenario};

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
        match h.ledger().abort_reason.as_ref().expect("reason") {
            ExperimentAbortReason::OverheadExceeded {
                sample_event_id,
                observed_us,
            } => {
                assert_eq!(sample_event_id, "e2-spike");
                assert_eq!(*observed_us, 5_000);
            }
            other => panic!("expected OverheadExceeded, got {other:?}"),
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
            h.ledger().abort_reason.as_ref().expect("reason"),
            ExperimentAbortReason::TotalOverheadExceeded { .. }
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
            h.ledger().abort_reason.as_ref().expect("reason"),
            ExperimentAbortReason::EventsCapReached
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
        // 3 post-abort attempts → 3 gaps.
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
        match h.ledger().abort_reason.as_ref().expect("reason") {
            ExperimentAbortReason::ManualStop { reason } => {
                assert_eq!(reason, "operator decision");
            }
            other => panic!("expected ManualStop, got {other:?}"),
        }
        // Idempotent.
        h.abort_manually("another");
        match h.ledger().abort_reason.as_ref().expect("reason") {
            ExperimentAbortReason::ManualStop { reason } => {
                assert_eq!(reason, "operator decision", "first reason wins");
            }
            other => panic!("expected ManualStop, got {other:?}"),
        }
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
        let citations = &h.ledger().decisions[0].evidence_citations;
        assert_eq!(citations.len(), 2);
        assert!(citations.iter().any(|c| c == "causal_node#7"));
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
            assert!(h.record_pair(ShadowDecisionPair {
                event_id: pane.event_ids[0].clone(),
                baseline_decision: "baseline:admit".to_string(),
                candidate_decision: if divergence == DivergenceKind::Major {
                    "candidate:throttle".to_string()
                } else {
                    "baseline:admit".to_string()
                },
                divergence,
                overhead_us: 40,
                evidence_citations: vec![
                    format!("event:{}", pane.event_ids[0]),
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
        let json = serde_json::to_string(&ledger).expect("serialize");
        let back: ExperimentLedger = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(ledger, back);
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
            let json = serde_json::to_string(&r).expect("serialize");
            let back: ExperimentAbortReason = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(r, back);
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
        let _record: fn(&mut ShadowExperimentHarness, ShadowDecisionPair) -> bool =
            ShadowExperimentHarness::record_pair;
        let _ledger: fn(&ShadowExperimentHarness) -> &ExperimentLedger =
            ShadowExperimentHarness::ledger;
        let _is_aborted: fn(&ShadowExperimentHarness) -> bool = ShadowExperimentHarness::is_aborted;
    }
}
