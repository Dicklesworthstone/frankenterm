//! Property-based tests for resize_crash_forensics module.
//!
//! Validates crash context capture and forensic data:
//! - Builder population and finalization
//! - Policy decision bounded buffer
//! - Global singleton round-trip
//! - Summary line invariants
//! - Serde roundtrip stability
//! - Domain budget accounting
//!
//! Bead: wa-1u90p.7 (Validation Program)

use proptest::prelude::*;

use frankenterm_core::resize_crash_forensics::{
    DomainBudgetEntry, InFlightTransaction, PolicyDecision, PolicyDecisionKind, ResizeCrashContext,
    ResizeCrashContextBuilder, ResizeQueueDepths, StormState,
};
use frankenterm_core::resize_scheduler::{
    ResizeControlPlaneGateState, ResizeDomain, ResizeExecutionPhase, ResizeWorkClass,
};

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_work_class() -> impl Strategy<Value = ResizeWorkClass> {
    prop_oneof![
        Just(ResizeWorkClass::Interactive),
        Just(ResizeWorkClass::Background),
    ]
}

fn arb_execution_phase() -> impl Strategy<Value = Option<ResizeExecutionPhase>> {
    prop_oneof![
        Just(None),
        Just(Some(ResizeExecutionPhase::Preparing)),
        Just(Some(ResizeExecutionPhase::Reflowing)),
        Just(Some(ResizeExecutionPhase::Presenting)),
    ]
}

fn arb_domain() -> impl Strategy<Value = ResizeDomain> {
    prop_oneof![
        Just(ResizeDomain::Local),
        "[a-z]{3,10}".prop_map(|host| ResizeDomain::Ssh { host }),
        "[a-z]{3,10}".prop_map(|endpoint| ResizeDomain::Mux { endpoint }),
    ]
}

fn arb_policy_kind() -> impl Strategy<Value = PolicyDecisionKind> {
    prop_oneof![
        Just(PolicyDecisionKind::StormThrottle),
        Just(PolicyDecisionKind::DomainBudgetThrottle),
        Just(PolicyDecisionKind::StarvationBypass),
        Just(PolicyDecisionKind::OverloadReject),
        Just(PolicyDecisionKind::OverloadEvict),
        Just(PolicyDecisionKind::InputGuardrailActivated),
        Just(PolicyDecisionKind::GateSuppressed),
    ]
}

fn arb_queue_depths() -> impl Strategy<Value = ResizeQueueDepths> {
    (
        0_u32..100,
        0_u32..50,
        0_u32..200,
        0_u32..500,
        0_u32..32,
        0_u32..32,
    )
        .prop_map(
            |(pending, active, backlog, tracked, budget, spent)| ResizeQueueDepths {
                pending_intents: pending,
                active_transactions: active,
                input_backlog: backlog,
                tracked_panes: tracked,
                frame_budget_units: budget,
                last_frame_spent_units: spent,
            },
        )
}

fn arb_storm_state() -> impl Strategy<Value = StormState> {
    (
        0_u32..20,
        0_u64..1000,
        0_u32..100,
        0_u64..10000,
        0_u64..10000,
    )
        .prop_map(|(tabs, window, threshold, events, throttled)| StormState {
            tabs_in_storm: tabs,
            storm_window_ms: window,
            storm_threshold: threshold,
            total_storm_events: events,
            total_storm_throttled: throttled,
        })
}

fn arb_in_flight() -> impl Strategy<Value = InFlightTransaction> {
    (
        1_u64..1000,
        1_u64..10000,
        arb_work_class(),
        arb_execution_phase(),
        proptest::option::of(0_u64..100000),
        arb_domain(),
        proptest::option::of(0_u64..100),
        0_u32..50,
        any::<bool>(),
    )
        .prop_map(
            |(pane_id, intent_seq, wc, phase, phase_ms, domain, tab_id, deferrals, force)| {
                InFlightTransaction {
                    pane_id,
                    intent_seq,
                    work_class: wc,
                    phase,
                    phase_started_at_ms: phase_ms,
                    domain,
                    tab_id,
                    deferrals,
                    force_served: force,
                }
            },
        )
}

fn arb_policy_decision() -> impl Strategy<Value = PolicyDecision> {
    (
        0_u64..100000,
        arb_policy_kind(),
        proptest::option::of(0_u64..1000),
        "[a-z ]{5,30}",
    )
        .prop_map(|(at_ms, kind, pane_id, rationale)| PolicyDecision {
            at_ms,
            kind,
            pane_id,
            rationale,
        })
}

fn arb_domain_budget_entry() -> impl Strategy<Value = DomainBudgetEntry> {
    (
        prop_oneof![
            Just("local".to_string()),
            "[a-z]{3,10}".prop_map(|h| format!("ssh:{h}")),
            "[a-z]{3,10}".prop_map(|e| format!("mux:{e}")),
        ],
        1_u32..10,
        0_u32..100,
        0_u32..100,
    )
        .prop_map(|(key, weight, allocated, consumed)| DomainBudgetEntry {
            domain_key: key,
            weight,
            allocated_units: allocated,
            consumed_units: consumed,
        })
}

fn arb_gate() -> impl Strategy<Value = ResizeControlPlaneGateState> {
    (any::<bool>(), any::<bool>(), any::<bool>()).prop_map(|(enabled, emergency, legacy)| {
        ResizeControlPlaneGateState {
            control_plane_enabled: enabled,
            emergency_disable: emergency,
            legacy_fallback_enabled: legacy,
            active: enabled && !emergency,
        }
    })
}

// ---------------------------------------------------------------------------
// Builder properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn builder_preserves_timestamp(ts in 0_u64..u64::MAX) {
        let ctx = ResizeCrashContextBuilder::new(ts).build();
        prop_assert_eq!(ctx.captured_at_ms, ts, "captured_at_ms should match builder input");
    }

    #[test]
    fn builder_preserves_gate(gate in arb_gate()) {
        let ctx = ResizeCrashContextBuilder::new(1000).gate(gate).build();
        prop_assert_eq!(ctx.gate, gate, "gate should match builder input");
    }

    #[test]
    fn builder_preserves_queue_depths(depths in arb_queue_depths()) {
        let ctx = ResizeCrashContextBuilder::new(1000)
            .queue_depths(depths)
            .build();
        prop_assert_eq!(ctx.queue_depths, depths, "queue depths should match");
    }

    #[test]
    fn builder_preserves_storm_state(storm in arb_storm_state()) {
        let ctx = ResizeCrashContextBuilder::new(1000)
            .storm_state(storm)
            .build();
        prop_assert_eq!(ctx.storm_state, storm, "storm state should match");
    }

    #[test]
    fn builder_accumulates_in_flight(
        txns in proptest::collection::vec(arb_in_flight(), 0..10)
    ) {
        let mut builder = ResizeCrashContextBuilder::new(1000);
        for txn in &txns {
            builder = builder.add_in_flight(txn.clone());
        }
        let ctx = builder.build();
        prop_assert_eq!(ctx.in_flight.len(), txns.len(),
            "in_flight count should match input count");
        for (i, txn) in txns.iter().enumerate() {
            prop_assert_eq!(&ctx.in_flight[i], txn,
                "in_flight[{}] should match input", i);
        }
    }

    #[test]
    fn builder_accumulates_domain_budgets(
        entries in proptest::collection::vec(arb_domain_budget_entry(), 0..10)
    ) {
        let mut builder = ResizeCrashContextBuilder::new(1000);
        for entry in &entries {
            builder = builder.add_domain_budget(entry.clone());
        }
        let ctx = builder.build();
        prop_assert_eq!(ctx.domain_budgets.len(), entries.len(),
            "domain budget count should match input");
    }
}

// ---------------------------------------------------------------------------
// Policy decision bounded buffer property
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn policy_decisions_bounded_at_64(
        decisions in proptest::collection::vec(arb_policy_decision(), 0..200)
    ) {
        let mut builder = ResizeCrashContextBuilder::new(1000);
        for d in &decisions {
            builder = builder.add_policy_decision(d.clone());
        }
        let ctx = builder.build();
        prop_assert!(ctx.policy_decisions.len() <= 64,
            "policy decisions should be bounded at 64, got {}",
            ctx.policy_decisions.len()
        );
    }

    #[test]
    fn policy_decisions_evict_oldest(
        count in 65_usize..200
    ) {
        let mut builder = ResizeCrashContextBuilder::new(1000);
        for i in 0..count {
            builder = builder.add_policy_decision(PolicyDecision {
                at_ms: i as u64,
                kind: PolicyDecisionKind::DomainBudgetThrottle,
                pane_id: None,
                rationale: format!("entry {}", i),
            });
        }
        let ctx = builder.build();
        prop_assert_eq!(ctx.policy_decisions.len(), 64,
            "should have exactly 64 decisions after overflow");

        // Last entry should be the most recent
        let last = ctx.policy_decisions.last().unwrap();
        prop_assert_eq!(last.at_ms, (count - 1) as u64,
            "last decision should be the most recently added");

        // First entry should be the oldest surviving entry
        let first = ctx.policy_decisions.first().unwrap();
        prop_assert_eq!(first.at_ms, (count - 64) as u64,
            "first decision should be the oldest surviving entry");
    }

    #[test]
    fn policy_decisions_preserve_order(
        decisions in proptest::collection::vec(arb_policy_decision(), 1..64)
    ) {
        let mut builder = ResizeCrashContextBuilder::new(1000);
        for d in &decisions {
            builder = builder.add_policy_decision(d.clone());
        }
        let ctx = builder.build();
        prop_assert_eq!(ctx.policy_decisions.len(), decisions.len(),
            "under-capacity decisions should be preserved exactly");
        for (i, d) in decisions.iter().enumerate() {
            prop_assert_eq!(ctx.policy_decisions[i].kind, d.kind,
                "decision kind at index {} should match", i);
            prop_assert_eq!(ctx.policy_decisions[i].at_ms, d.at_ms,
                "decision at_ms at index {} should match", i);
        }
    }
}

// ---------------------------------------------------------------------------
// Summary line properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn summary_line_contains_key_fields(
        ts in 0_u64..100000,
        depths in arb_queue_depths(),
        n_in_flight in 0_usize..10,
        storm in arb_storm_state(),
        n_decisions in 0_usize..20,
    ) {
        let mut builder = ResizeCrashContextBuilder::new(ts)
            .queue_depths(depths)
            .storm_state(storm);

        for i in 0..n_in_flight {
            builder = builder.add_in_flight(InFlightTransaction {
                pane_id: i as u64,
                intent_seq: 1,
                work_class: ResizeWorkClass::Interactive,
                phase: None,
                phase_started_at_ms: None,
                domain: ResizeDomain::Local,
                tab_id: None,
                deferrals: 0,
                force_served: false,
            });
        }

        for i in 0..n_decisions {
            builder = builder.add_policy_decision(PolicyDecision {
                at_ms: i as u64,
                kind: PolicyDecisionKind::StormThrottle,
                pane_id: None,
                rationale: "test".into(),
            });
        }

        let ctx = builder.build();
        let line = ctx.summary_line();

        prop_assert!(line.contains(&format!("captured_at={}", ts)),
            "summary should contain timestamp: {}", line);
        prop_assert!(line.contains(&format!("pending={}", depths.pending_intents)),
            "summary should contain pending count: {}", line);
        prop_assert!(line.contains(&format!("active={}", depths.active_transactions)),
            "summary should contain active count: {}", line);
        prop_assert!(line.contains(&format!("in_flight={}", n_in_flight)),
            "summary should contain in_flight count: {}", line);
        prop_assert!(line.contains(&format!("storm_tabs={}", storm.tabs_in_storm)),
            "summary should contain storm tabs: {}", line);
        let expected_decisions = n_decisions.min(64);
        prop_assert!(line.contains(&format!("decisions={}", expected_decisions)),
            "summary should contain decisions count: {}", line);
    }
}

// ---------------------------------------------------------------------------
// Global singleton properties
// ---------------------------------------------------------------------------

// Note: global singleton tests are combined into a single test to avoid
// race conditions between parallel test threads sharing the OnceLock.

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn global_update_get_clear_and_builder_cycle(
        ts in 0_u64..100000,
        gate in arb_gate()
    ) {
        // Phase 1: update → get → clear cycle
        let ctx = ResizeCrashContextBuilder::new(ts).gate(gate).build();
        ResizeCrashContext::update_global(ctx.clone());

        let got = ResizeCrashContext::get_global();
        prop_assert!(got.is_some(), "global should be Some after update");

        ResizeCrashContext::clear_global();
        let cleared = ResizeCrashContext::get_global();
        prop_assert!(cleared.is_none(), "global should be None after clear");

        // Phase 2: build_and_update_global
        ResizeCrashContextBuilder::new(ts + 1)
            .gate(gate)
            .build_and_update_global();

        let got2 = ResizeCrashContext::get_global();
        prop_assert!(got2.is_some(), "global should be set after build_and_update_global");

        ResizeCrashContext::clear_global();
    }
}

// ---------------------------------------------------------------------------
// Serde roundtrip properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn in_flight_serde_roundtrip(txn in arb_in_flight()) {
        let json = serde_json::to_string(&txn).expect("serialize");
        let rt: InFlightTransaction = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(txn, rt, "in-flight serde roundtrip should be stable");
    }

    #[test]
    fn policy_decision_serde_roundtrip(decision in arb_policy_decision()) {
        let json = serde_json::to_string(&decision).expect("serialize");
        let rt: PolicyDecision = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(decision, rt, "policy decision serde roundtrip should be stable");
    }

    #[test]
    fn queue_depths_serde_roundtrip(depths in arb_queue_depths()) {
        let json = serde_json::to_string(&depths).expect("serialize");
        let rt: ResizeQueueDepths = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(depths, rt, "queue depths serde roundtrip should be stable");
    }

    #[test]
    fn storm_state_serde_roundtrip(storm in arb_storm_state()) {
        let json = serde_json::to_string(&storm).expect("serialize");
        let rt: StormState = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(storm, rt, "storm state serde roundtrip should be stable");
    }

    #[test]
    fn domain_budget_serde_roundtrip(entry in arb_domain_budget_entry()) {
        let json = serde_json::to_string(&entry).expect("serialize");
        let rt: DomainBudgetEntry = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(entry, rt, "domain budget serde roundtrip should be stable");
    }

    #[test]
    fn full_context_serde_roundtrip(
        ts in 0_u64..100000,
        gate in arb_gate(),
        depths in arb_queue_depths(),
        storm in arb_storm_state(),
        n_txns in 0_usize..5,
        n_budgets in 0_usize..5,
    ) {
        let mut builder = ResizeCrashContextBuilder::new(ts)
            .gate(gate)
            .queue_depths(depths)
            .storm_state(storm);

        for i in 0..n_txns {
            builder = builder.add_in_flight(InFlightTransaction {
                pane_id: i as u64,
                intent_seq: i as u64 * 10,
                work_class: ResizeWorkClass::Interactive,
                phase: Some(ResizeExecutionPhase::Reflowing),
                phase_started_at_ms: Some(1000),
                domain: ResizeDomain::Local,
                tab_id: Some(0),
                deferrals: 0,
                force_served: false,
            });
        }

        for i in 0..n_budgets {
            builder = builder.add_domain_budget(DomainBudgetEntry {
                domain_key: format!("domain-{}", i),
                weight: (i + 1) as u32,
                allocated_units: 10,
                consumed_units: i as u32,
            });
        }

        let ctx = builder.build();
        let json = serde_json::to_string(&ctx).expect("serialize context");
        let rt: ResizeCrashContext = serde_json::from_str(&json).expect("deserialize context");
        prop_assert_eq!(ctx, rt, "full context serde roundtrip should be stable");
    }

    #[test]
    fn policy_decision_kind_serde_roundtrip(kind in arb_policy_kind()) {
        let json = serde_json::to_string(&kind).expect("serialize kind");
        let rt: PolicyDecisionKind = serde_json::from_str(&json).expect("deserialize kind");
        prop_assert_eq!(kind, rt, "policy kind serde roundtrip should be stable");
    }
}

// ---------------------------------------------------------------------------
// Default and empty state properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn empty_builder_produces_valid_context(ts in 0_u64..u64::MAX) {
        let ctx = ResizeCrashContextBuilder::new(ts).build();
        prop_assert_eq!(ctx.captured_at_ms, ts);
        prop_assert!(!ctx.gate.active, "default gate should be inactive");
        prop_assert!(ctx.in_flight.is_empty());
        prop_assert!(ctx.policy_decisions.is_empty());
        prop_assert!(ctx.domain_budgets.is_empty());
        prop_assert_eq!(ctx.queue_depths.pending_intents, 0);
        prop_assert_eq!(ctx.storm_state.tabs_in_storm, 0);
    }

    #[test]
    fn queue_depths_default_all_zero(_dummy in 0..1_u8) {
        let d = ResizeQueueDepths::default();
        prop_assert_eq!(d.pending_intents, 0);
        prop_assert_eq!(d.active_transactions, 0);
        prop_assert_eq!(d.input_backlog, 0);
        prop_assert_eq!(d.tracked_panes, 0);
        prop_assert_eq!(d.frame_budget_units, 0);
        prop_assert_eq!(d.last_frame_spent_units, 0);
    }

    #[test]
    fn storm_state_default_all_zero(_dummy in 0..1_u8) {
        let s = StormState::default();
        prop_assert_eq!(s.tabs_in_storm, 0);
        prop_assert_eq!(s.storm_window_ms, 0);
        prop_assert_eq!(s.storm_threshold, 0);
        prop_assert_eq!(s.total_storm_events, 0);
        prop_assert_eq!(s.total_storm_throttled, 0);
    }
}
