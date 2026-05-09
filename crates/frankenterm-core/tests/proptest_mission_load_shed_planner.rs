//! Property tests for [`mission_load_shed_planner`] (ft-1650n.6).
//!
//! Pins the documented invariants over arbitrary candidate sets
//! and signal sequences. Complements the unit tests in
//! `mission_load_shed_planner.rs::tests` (14 cases) and the
//! 50-pane synthetic-overload e2e at
//! `tests/mission_load_shed_planner_e2e.rs` (3 fixtures).
//!
//! Properties pinned here:
//!
//! 1. **Critical-priority KeepAlive** — every Critical-priority
//!    pane receives `KeepAlive` on every tick, regardless of
//!    pressure / capability / utility. The bead's "no
//!    high-priority mission starvation" criterion as a universal
//!    quantifier.
//! 2. **Decision count == candidate count** — every plan tick
//!    emits exactly one decision per candidate.
//! 3. **Decision-bag conservation** — `admit + throttle + pause +
//!    keep_alive == candidate_count` in the report summary.
//! 4. **Capability fail-closed** — every unknown-capability pane
//!    receives `Pause/UnknownCapability` under any non-zero
//!    pressure.
//! 5. **No-pressure admits all non-Critical** — under zero
//!    pressure, every pane gets either `Admit` or `KeepAlive`.
//! 6. **Bounded state** — `bounded_state_entries == candidate
//!    count` per tick (no per-tick growth).
//! 7. **Plan determinism** — two fresh planners ingesting the
//!    same (candidates, signals) sequence produce byte-identical
//!    `LoadShedPlanReport`s.
//! 8. **Schema version stability** — every `LoadShedPlanReport`
//!    carries the documented `LOAD_SHED_PLAN_REPORT_SCHEMA_VERSION`.

use std::sync::Once;

use frankenterm_core::mission_load_shed_planner::{
    LOAD_SHED_PLAN_REPORT_SCHEMA_VERSION, LoadShedConfig, LoadShedDecision, LoadShedPlanner,
    MissionPriority, OverloadSignals, PaneCandidate, PauseReason,
};
use proptest::prelude::*;
use tracing::info;

fn init_test_tracing_json() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .json()
            .with_target(true)
            .with_test_writer()
            .try_init();
    });
}

fn arb_priority() -> impl Strategy<Value = MissionPriority> {
    prop_oneof![
        Just(MissionPriority::Critical),
        Just(MissionPriority::High),
        Just(MissionPriority::Medium),
        Just(MissionPriority::Low),
    ]
}

fn arb_candidate(pane_id: u64) -> impl Strategy<Value = PaneCandidate> {
    (arb_priority(), 0u32..=1000, 0u32..=200, any::<bool>()).prop_map(
        move |(priority, utility, uncertainty, cap_known)| PaneCandidate {
            pane_id,
            mission_priority: priority,
            utility_e3: utility,
            uncertainty_e3: uncertainty,
            capability_known: cap_known,
        },
    )
}

fn arb_candidate_set() -> impl Strategy<Value = Vec<PaneCandidate>> {
    prop::collection::vec(0u32..=20, 1..=15).prop_flat_map(|ids| {
        ids.into_iter()
            .enumerate()
            .map(|(idx, _)| arb_candidate(idx as u64 + 1))
            .collect::<Vec<_>>()
    })
}

fn arb_signals() -> impl Strategy<Value = OverloadSignals> {
    (
        0u32..=1000,
        0u32..=1000,
        0u32..=1000,
        0u32..=1000,
        0u32..=1000,
    )
        .prop_map(|(cpu, mem, storage, api, attn)| OverloadSignals {
            cpu_e3: cpu,
            memory_e3: mem,
            storage_e3: storage,
            api_quota_e3: api,
            human_attention_e3: attn,
        })
}

fn arb_signal_sequence() -> impl Strategy<Value = Vec<OverloadSignals>> {
    prop::collection::vec(arb_signals(), 1..=10)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// **Critical-priority KeepAlive**: every Critical-priority
    /// pane receives KeepAlive on every plan tick, regardless of
    /// pressure / capability / utility.
    #[test]
    fn critical_priority_always_keep_alive(
        candidates in arb_candidate_set(),
        signals in arb_signal_sequence(),
    ) {
        init_test_tracing_json();
        let mut planner = LoadShedPlanner::new(LoadShedConfig::conservative());
        for s in &signals {
            let decisions = planner.plan(&candidates, s);
            for (pane_id, decision, evidence) in &decisions {
                if evidence.mission_priority == MissionPriority::Critical {
                    let is_keep_alive = matches!(decision, LoadShedDecision::KeepAlive);
                    prop_assert!(
                        is_keep_alive,
                        "Critical pane {pane_id} got {decision:?}"
                    );
                }
            }
        }
    }

    /// **Decision count == candidate count**: per-tick.
    #[test]
    fn decision_count_matches_candidate_count(
        candidates in arb_candidate_set(),
        signals in arb_signal_sequence(),
    ) {
        init_test_tracing_json();
        let mut planner = LoadShedPlanner::new(LoadShedConfig::conservative());
        for s in &signals {
            let decisions = planner.plan(&candidates, s);
            prop_assert_eq!(decisions.len(), candidates.len());
        }
    }

    /// **Decision-bag conservation**: admit + throttle + pause +
    /// keep_alive == candidate_count for every plan_report tick.
    #[test]
    fn decision_bag_conservation(
        candidates in arb_candidate_set(),
        signals in arb_signal_sequence(),
    ) {
        init_test_tracing_json();
        let mut planner = LoadShedPlanner::new(LoadShedConfig::conservative());
        for s in &signals {
            let report = planner.plan_report(&candidates, s);
            let total = report.summary.admit_count
                + report.summary.throttle_count
                + report.summary.pause_count
                + report.summary.keep_alive_count;
            prop_assert_eq!(total, candidates.len());
            prop_assert_eq!(report.summary.candidate_count, candidates.len());
        }
        info!(
            test = "decision_bag_conservation",
            candidate_count = candidates.len(),
            signal_count = signals.len(),
            "load-shed proptest case"
        );
    }

    /// **Capability fail-closed**: every unknown-capability pane
    /// receives Pause/UnknownCapability under any non-zero
    /// pressure, regardless of priority (Critical short-circuits
    /// before capability check; this property excludes Critical).
    #[test]
    fn capability_fail_closed_under_pressure(
        candidates in arb_candidate_set(),
        signals in arb_signals(),
    ) {
        init_test_tracing_json();
        let aggregate = signals.aggregate_e3();
        prop_assume!(aggregate > 0);
        let mut planner = LoadShedPlanner::new(LoadShedConfig::conservative());
        let decisions = planner.plan(&candidates, &signals);
        for (pane_id, decision, evidence) in &decisions {
            if !evidence.capability_known
                && evidence.mission_priority != MissionPriority::Critical
            {
                let is_pause_unknown = matches!(
                    decision,
                    LoadShedDecision::Pause {
                        reason: PauseReason::UnknownCapability
                    }
                );
                prop_assert!(
                    is_pause_unknown,
                    "unknown-capability pane {pane_id} got {decision:?}"
                );
            }
        }
    }

    /// **No-pressure admits all non-Critical**: under zero
    /// pressure, every pane gets either Admit (non-Critical) or
    /// KeepAlive (Critical). Pinned because the planner must be
    /// fully idle when there's no overload signal.
    #[test]
    fn no_pressure_admits_all_non_critical(
        candidates in arb_candidate_set(),
    ) {
        init_test_tracing_json();
        let mut planner = LoadShedPlanner::new(LoadShedConfig::conservative());
        let decisions = planner.plan(&candidates, &OverloadSignals::default());
        for (_, decision, evidence) in &decisions {
            let is_admit_or_keepalive = matches!(
                (evidence.mission_priority, decision),
                (MissionPriority::Critical, LoadShedDecision::KeepAlive)
                    | (_, LoadShedDecision::Admit)
            );
            prop_assert!(
                is_admit_or_keepalive,
                "no-pressure tick produced unexpected decision: {decision:?}"
            );
        }
    }

    /// **Bounded state**: report.summary.bounded_state_entries
    /// equals the candidate count per tick. The planner does not
    /// grow state per tick beyond the candidate set.
    #[test]
    fn bounded_state_equals_candidate_count(
        candidates in arb_candidate_set(),
        signals in arb_signal_sequence(),
    ) {
        init_test_tracing_json();
        let mut planner = LoadShedPlanner::new(LoadShedConfig::conservative());
        for s in &signals {
            let report = planner.plan_report(&candidates, s);
            prop_assert_eq!(report.summary.bounded_state_entries, candidates.len());
        }
    }

    /// **Plan determinism**: two fresh planners ingesting the
    /// same (candidates, signals) sequence produce byte-identical
    /// `LoadShedPlanReport`s, including JSON encoding.
    #[test]
    fn plan_report_is_deterministic(
        candidates in arb_candidate_set(),
        signals in arb_signal_sequence(),
    ) {
        init_test_tracing_json();
        let mut p1 = LoadShedPlanner::new(LoadShedConfig::conservative());
        let mut p2 = LoadShedPlanner::new(LoadShedConfig::conservative());
        for s in &signals {
            let r1 = p1.plan_report(&candidates, s);
            let r2 = p2.plan_report(&candidates, s);
            prop_assert_eq!(&r1, &r2);
            let j1 = serde_json::to_string(&r1).expect("serialize r1");
            let j2 = serde_json::to_string(&r2).expect("serialize r2");
            prop_assert_eq!(j1, j2);
        }
    }

    /// **Schema version stability**: every report carries the
    /// documented schema version constant.
    #[test]
    fn schema_version_is_pinned(
        candidates in arb_candidate_set(),
        signals in arb_signals(),
    ) {
        init_test_tracing_json();
        let mut planner = LoadShedPlanner::new(LoadShedConfig::conservative());
        let report = planner.plan_report(&candidates, &signals);
        prop_assert_eq!(report.schema_version, LOAD_SHED_PLAN_REPORT_SCHEMA_VERSION);
        prop_assert!(report.dry_run);
        prop_assert!(!report.enforcement_allowed);
    }

    /// **KeepAlive count matches Critical-priority count**: the
    /// summary's keep_alive_count equals the number of Critical-
    /// priority candidates (every other priority class can never
    /// be KeepAlive). Pinned as a numeric invariant.
    #[test]
    fn keep_alive_count_matches_critical_count(
        candidates in arb_candidate_set(),
        signals in arb_signals(),
    ) {
        init_test_tracing_json();
        let critical_count = candidates
            .iter()
            .filter(|c| c.mission_priority == MissionPriority::Critical)
            .count();
        let mut planner = LoadShedPlanner::new(LoadShedConfig::conservative());
        let report = planner.plan_report(&candidates, &signals);
        prop_assert_eq!(report.summary.keep_alive_count, critical_count);
    }
}
