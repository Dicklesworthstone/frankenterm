//! Integration tests for resize_crash_forensics module.
//!
//! Covers: builder ergonomics, global state lifecycle, serialization
//! round-trips, policy decision bounding, summary formatting, domain
//! diversity, phase coverage, concurrent access, and stress scenarios.

use std::fs;
use std::sync::Arc;
use std::thread;

use frankenterm_core::crash::{CrashManifest, CrashReport, write_crash_bundle};
use frankenterm_core::resize_crash_forensics::{
    DomainBudgetEntry, InFlightTransaction, PolicyDecision, PolicyDecisionKind, ResizeCrashContext,
    ResizeCrashContextBuilder, ResizeQueueDepths, StormState,
};
use frankenterm_core::resize_scheduler::{
    ResizeControlPlaneGateState, ResizeDomain, ResizeExecutionPhase, ResizeWorkClass,
};

// ── Helpers ──────────────────────────────────────────────────────────

fn sample_gate_active() -> ResizeControlPlaneGateState {
    ResizeControlPlaneGateState {
        control_plane_enabled: true,
        emergency_disable: false,
        legacy_fallback_enabled: false,
        active: true,
    }
}

fn sample_gate_disabled() -> ResizeControlPlaneGateState {
    ResizeControlPlaneGateState {
        control_plane_enabled: false,
        emergency_disable: true,
        legacy_fallback_enabled: true,
        active: false,
    }
}

fn sample_txn(pane_id: u64, phase: Option<ResizeExecutionPhase>) -> InFlightTransaction {
    InFlightTransaction {
        pane_id,
        intent_seq: pane_id * 10,
        work_class: ResizeWorkClass::Interactive,
        phase,
        phase_started_at_ms: phase.map(|_| 1000 + pane_id),
        domain: ResizeDomain::Local,
        tab_id: Some(pane_id / 3),
        deferrals: 0,
        force_served: false,
    }
}

fn sample_decision(kind: PolicyDecisionKind, at_ms: u64) -> PolicyDecision {
    PolicyDecision {
        at_ms,
        kind,
        pane_id: Some(1),
        rationale: format!("{kind:?} at {at_ms}"),
    }
}

fn rich_context() -> ResizeCrashContext {
    ResizeCrashContextBuilder::new(99999)
        .gate(sample_gate_active())
        .queue_depths(ResizeQueueDepths {
            pending_intents: 5,
            active_transactions: 3,
            input_backlog: 2,
            tracked_panes: 12,
            frame_budget_units: 20,
            last_frame_spent_units: 15,
        })
        .add_in_flight(sample_txn(1, Some(ResizeExecutionPhase::Preparing)))
        .add_in_flight(sample_txn(2, Some(ResizeExecutionPhase::Reflowing)))
        .add_in_flight(sample_txn(3, Some(ResizeExecutionPhase::Presenting)))
        .add_policy_decision(sample_decision(PolicyDecisionKind::StormThrottle, 99990))
        .add_policy_decision(sample_decision(
            PolicyDecisionKind::DomainBudgetThrottle,
            99991,
        ))
        .storm_state(StormState {
            tabs_in_storm: 2,
            storm_window_ms: 100,
            storm_threshold: 5,
            total_storm_events: 10,
            total_storm_throttled: 4,
        })
        .add_domain_budget(DomainBudgetEntry {
            domain_key: "local".into(),
            weight: 4,
            allocated_units: 12,
            consumed_units: 10,
        })
        .add_domain_budget(DomainBudgetEntry {
            domain_key: "ssh:remote".into(),
            weight: 2,
            allocated_units: 8,
            consumed_units: 5,
        })
        .build()
}

// ── Builder edge cases ───────────────────────────────────────────────

#[test]
fn builder_default_gate_when_not_set() {
    let ctx = ResizeCrashContextBuilder::new(100).build();
    // When gate() is not called, the builder provides a disabled default.
    assert!(!ctx.gate.control_plane_enabled);
    assert!(!ctx.gate.emergency_disable);
    assert!(!ctx.gate.active);
}

#[test]
fn builder_with_disabled_gate() {
    let ctx = ResizeCrashContextBuilder::new(200)
        .gate(sample_gate_disabled())
        .build();
    assert!(!ctx.gate.control_plane_enabled);
    assert!(ctx.gate.emergency_disable);
    assert!(ctx.gate.legacy_fallback_enabled);
    assert!(!ctx.gate.active);
}

#[test]
fn builder_chains_many_in_flight_transactions() {
    let mut builder = ResizeCrashContextBuilder::new(300);
    for i in 0..100 {
        builder = builder.add_in_flight(sample_txn(i, Some(ResizeExecutionPhase::Reflowing)));
    }
    let ctx = builder.build();
    assert_eq!(ctx.in_flight.len(), 100);
    // Verify ordering is preserved.
    for (i, txn) in ctx.in_flight.iter().enumerate() {
        assert_eq!(txn.pane_id, i as u64);
    }
}

#[test]
fn builder_chains_many_domain_budgets() {
    let mut builder = ResizeCrashContextBuilder::new(400);
    for i in 0..20 {
        builder = builder.add_domain_budget(DomainBudgetEntry {
            domain_key: format!("ssh:host-{i}"),
            weight: i as u32 + 1,
            allocated_units: (i as u32 + 1) * 2,
            consumed_units: i as u32,
        });
    }
    let ctx = builder.build();
    assert_eq!(ctx.domain_budgets.len(), 20);
    let total_weight: u32 = ctx.domain_budgets.iter().map(|d| d.weight).sum();
    assert_eq!(total_weight, (1..=20u32).sum::<u32>());
}

// ── Policy decision bounding ─────────────────────────────────────────

#[test]
fn policy_decisions_evict_oldest_at_boundary() {
    // MAX_POLICY_DECISIONS is 64; submit exactly 64 to fill without eviction.
    let mut builder = ResizeCrashContextBuilder::new(500);
    for i in 0..64 {
        builder = builder.add_policy_decision(PolicyDecision {
            at_ms: i,
            kind: PolicyDecisionKind::StormThrottle,
            pane_id: None,
            rationale: format!("d{i}"),
        });
    }
    let ctx = builder.build();
    assert_eq!(ctx.policy_decisions.len(), 64);
    assert_eq!(ctx.policy_decisions[0].at_ms, 0);
    assert_eq!(ctx.policy_decisions[63].at_ms, 63);
}

#[test]
fn policy_decisions_evict_one_past_boundary() {
    let mut builder = ResizeCrashContextBuilder::new(600);
    for i in 0..65 {
        builder = builder.add_policy_decision(PolicyDecision {
            at_ms: i,
            kind: PolicyDecisionKind::OverloadReject,
            pane_id: None,
            rationale: format!("d{i}"),
        });
    }
    let ctx = builder.build();
    assert_eq!(ctx.policy_decisions.len(), 64);
    // Oldest (at_ms=0) should be evicted; first entry should be at_ms=1.
    assert_eq!(ctx.policy_decisions[0].at_ms, 1);
    assert_eq!(ctx.policy_decisions[63].at_ms, 64);
}

#[test]
fn policy_decisions_evict_many_past_boundary() {
    let mut builder = ResizeCrashContextBuilder::new(700);
    for i in 0..200 {
        builder = builder.add_policy_decision(PolicyDecision {
            at_ms: i,
            kind: PolicyDecisionKind::GateSuppressed,
            pane_id: None,
            rationale: format!("d{i}"),
        });
    }
    let ctx = builder.build();
    assert_eq!(ctx.policy_decisions.len(), 64);
    // Latest 64 entries: 136..=199.
    assert_eq!(ctx.policy_decisions[0].at_ms, 136);
    assert_eq!(ctx.policy_decisions[63].at_ms, 199);
}

// ── All policy decision kinds ────────────────────────────────────────

#[test]
fn all_policy_decision_kinds_in_single_context() {
    let all_kinds = [
        PolicyDecisionKind::StormThrottle,
        PolicyDecisionKind::DomainBudgetThrottle,
        PolicyDecisionKind::StarvationBypass,
        PolicyDecisionKind::OverloadReject,
        PolicyDecisionKind::OverloadEvict,
        PolicyDecisionKind::InputGuardrailActivated,
        PolicyDecisionKind::GateSuppressed,
    ];

    let mut builder = ResizeCrashContextBuilder::new(800);
    for (i, kind) in all_kinds.iter().enumerate() {
        builder = builder.add_policy_decision(PolicyDecision {
            at_ms: i as u64,
            kind: *kind,
            pane_id: Some(i as u64),
            rationale: format!("{kind:?}"),
        });
    }
    let ctx = builder.build();
    assert_eq!(ctx.policy_decisions.len(), 7);

    // Verify each kind is present.
    for kind in &all_kinds {
        assert!(
            ctx.policy_decisions.iter().any(|d| d.kind == *kind),
            "Missing kind: {kind:?}"
        );
    }
}

// ── All execution phases in in-flight ────────────────────────────────

#[test]
fn in_flight_with_all_phases() {
    let phases = [
        None,
        Some(ResizeExecutionPhase::Preparing),
        Some(ResizeExecutionPhase::Reflowing),
        Some(ResizeExecutionPhase::Presenting),
    ];

    let mut builder = ResizeCrashContextBuilder::new(900);
    for (i, phase) in phases.iter().enumerate() {
        builder = builder.add_in_flight(InFlightTransaction {
            pane_id: i as u64,
            intent_seq: 1,
            work_class: ResizeWorkClass::Interactive,
            phase: *phase,
            phase_started_at_ms: phase.map(|_| 900),
            domain: ResizeDomain::Local,
            tab_id: None,
            deferrals: 0,
            force_served: false,
        });
    }
    let ctx = builder.build();
    assert_eq!(ctx.in_flight.len(), 4);
    assert!(ctx.in_flight[0].phase.is_none());
    assert_eq!(
        ctx.in_flight[1].phase,
        Some(ResizeExecutionPhase::Preparing)
    );
    assert_eq!(
        ctx.in_flight[2].phase,
        Some(ResizeExecutionPhase::Reflowing)
    );
    assert_eq!(
        ctx.in_flight[3].phase,
        Some(ResizeExecutionPhase::Presenting)
    );
}

// ── All domain types ─────────────────────────────────────────────────

#[test]
fn in_flight_with_all_domain_types() {
    let domains = [
        ResizeDomain::Local,
        ResizeDomain::Ssh {
            host: "server.example.com".into(),
        },
        ResizeDomain::Mux {
            endpoint: "ws://mux:8080".into(),
        },
    ];

    let mut builder = ResizeCrashContextBuilder::new(1000);
    for (i, domain) in domains.into_iter().enumerate() {
        builder = builder.add_in_flight(InFlightTransaction {
            pane_id: i as u64,
            intent_seq: 1,
            work_class: ResizeWorkClass::Background,
            phase: Some(ResizeExecutionPhase::Reflowing),
            phase_started_at_ms: Some(1000),
            domain,
            tab_id: None,
            deferrals: i as u32,
            force_served: i == 2,
        });
    }
    let ctx = builder.build();
    assert_eq!(ctx.in_flight.len(), 3);
    assert_eq!(ctx.in_flight[0].domain, ResizeDomain::Local);
    assert!(matches!(ctx.in_flight[1].domain, ResizeDomain::Ssh { .. }));
    assert!(matches!(ctx.in_flight[2].domain, ResizeDomain::Mux { .. }));
    assert!(ctx.in_flight[2].force_served);
}

// ── Work class coverage ──────────────────────────────────────────────

#[test]
fn in_flight_with_both_work_classes() {
    let ctx = ResizeCrashContextBuilder::new(1100)
        .add_in_flight(InFlightTransaction {
            pane_id: 1,
            intent_seq: 1,
            work_class: ResizeWorkClass::Interactive,
            phase: None,
            phase_started_at_ms: None,
            domain: ResizeDomain::Local,
            tab_id: None,
            deferrals: 0,
            force_served: false,
        })
        .add_in_flight(InFlightTransaction {
            pane_id: 2,
            intent_seq: 1,
            work_class: ResizeWorkClass::Background,
            phase: None,
            phase_started_at_ms: None,
            domain: ResizeDomain::Local,
            tab_id: None,
            deferrals: 3,
            force_served: true,
        })
        .build();

    assert_eq!(ctx.in_flight[0].work_class, ResizeWorkClass::Interactive);
    assert_eq!(ctx.in_flight[1].work_class, ResizeWorkClass::Background);
    assert!(ctx.in_flight[1].force_served);
    assert_eq!(ctx.in_flight[1].deferrals, 3);
}

// ── Serialization ────────────────────────────────────────────────────

#[test]
fn rich_context_serialization_round_trip() {
    let ctx = rich_context();
    let json = serde_json::to_string_pretty(&ctx).expect("serialize");
    let rt: ResizeCrashContext = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(rt, ctx);
}

#[test]
fn empty_context_serialization_round_trip() {
    let ctx = ResizeCrashContextBuilder::new(0).build();
    let json = serde_json::to_string(&ctx).expect("serialize");
    let rt: ResizeCrashContext = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(rt, ctx);
}

#[test]
fn serialization_preserves_ssh_domain_host() {
    let ctx = ResizeCrashContextBuilder::new(1200)
        .add_in_flight(InFlightTransaction {
            pane_id: 1,
            intent_seq: 1,
            work_class: ResizeWorkClass::Interactive,
            phase: None,
            phase_started_at_ms: None,
            domain: ResizeDomain::Ssh {
                host: "dev.example.com".into(),
            },
            tab_id: None,
            deferrals: 0,
            force_served: false,
        })
        .build();
    let json = serde_json::to_string(&ctx).unwrap();
    assert!(json.contains("dev.example.com"));
    let rt: ResizeCrashContext = serde_json::from_str(&json).unwrap();
    assert_eq!(
        rt.in_flight[0].domain,
        ResizeDomain::Ssh {
            host: "dev.example.com".into()
        }
    );
}

#[test]
fn serialization_preserves_mux_domain_endpoint() {
    let ctx = ResizeCrashContextBuilder::new(1300)
        .add_in_flight(InFlightTransaction {
            pane_id: 1,
            intent_seq: 1,
            work_class: ResizeWorkClass::Background,
            phase: Some(ResizeExecutionPhase::Presenting),
            phase_started_at_ms: Some(1300),
            domain: ResizeDomain::Mux {
                endpoint: "ws://mux-prod:9090/v2".into(),
            },
            tab_id: Some(42),
            deferrals: 10,
            force_served: true,
        })
        .build();
    let json = serde_json::to_string(&ctx).unwrap();
    let rt: ResizeCrashContext = serde_json::from_str(&json).unwrap();
    assert_eq!(
        rt.in_flight[0].domain,
        ResizeDomain::Mux {
            endpoint: "ws://mux-prod:9090/v2".into()
        }
    );
    assert!(rt.in_flight[0].force_served);
    assert_eq!(rt.in_flight[0].deferrals, 10);
}

#[test]
fn all_policy_decision_kinds_serde_snake_case() {
    let kinds = [
        (PolicyDecisionKind::StormThrottle, "storm_throttle"),
        (
            PolicyDecisionKind::DomainBudgetThrottle,
            "domain_budget_throttle",
        ),
        (PolicyDecisionKind::StarvationBypass, "starvation_bypass"),
        (PolicyDecisionKind::OverloadReject, "overload_reject"),
        (PolicyDecisionKind::OverloadEvict, "overload_evict"),
        (
            PolicyDecisionKind::InputGuardrailActivated,
            "input_guardrail_activated",
        ),
        (PolicyDecisionKind::GateSuppressed, "gate_suppressed"),
    ];

    for (kind, expected_name) in kinds {
        let json = serde_json::to_string(&kind).unwrap();
        assert_eq!(json, format!("\"{expected_name}\""), "kind: {kind:?}");
        let rt: PolicyDecisionKind = serde_json::from_str(&json).unwrap();
        assert_eq!(rt, kind);
    }
}

// ── Summary line ─────────────────────────────────────────────────────

#[test]
fn summary_line_includes_all_fields() {
    let ctx = rich_context();
    let line = ctx.summary_line();
    assert!(line.contains("resize_crash_ctx"));
    assert!(line.contains("captured_at=99999"));
    assert!(line.contains("pending=5"));
    assert!(line.contains("active=3"));
    assert!(line.contains("in_flight=3"));
    assert!(line.contains("storm_tabs=2"));
    assert!(line.contains("decisions=2"));
}

#[test]
fn summary_line_with_zero_values() {
    let ctx = ResizeCrashContextBuilder::new(0).build();
    let line = ctx.summary_line();
    assert!(line.contains("captured_at=0"));
    assert!(line.contains("pending=0"));
    assert!(line.contains("active=0"));
    assert!(line.contains("in_flight=0"));
    assert!(line.contains("storm_tabs=0"));
    assert!(line.contains("decisions=0"));
}

#[test]
fn summary_line_with_large_counts() {
    let ctx = ResizeCrashContextBuilder::new(u64::MAX)
        .queue_depths(ResizeQueueDepths {
            pending_intents: u32::MAX,
            active_transactions: u32::MAX,
            input_backlog: u32::MAX,
            tracked_panes: u32::MAX,
            frame_budget_units: u32::MAX,
            last_frame_spent_units: u32::MAX,
        })
        .storm_state(StormState {
            tabs_in_storm: u32::MAX,
            storm_window_ms: u64::MAX,
            storm_threshold: u32::MAX,
            total_storm_events: u64::MAX,
            total_storm_throttled: u64::MAX,
        })
        .build();
    let line = ctx.summary_line();
    assert!(line.contains(&format!("captured_at={}", u64::MAX)));
    assert!(line.contains(&format!("pending={}", u32::MAX)));
    assert!(line.contains(&format!("storm_tabs={}", u32::MAX)));
}

// ── Global state lifecycle ───────────────────────────────────────────

#[test]
fn global_update_then_get_returns_some() {
    let ctx = ResizeCrashContextBuilder::new(10000)
        .gate(sample_gate_active())
        .build();
    ResizeCrashContext::update_global(ctx);
    let got = ResizeCrashContext::get_global();
    assert!(got.is_some());
    // Clean up for other tests.
    ResizeCrashContext::clear_global();
}

#[test]
fn global_clear_then_get_returns_none() {
    ResizeCrashContext::clear_global();
    // Note: another test may race to set this, but clear should work.
    // We just verify the clear path doesn't panic.
    let _ = ResizeCrashContext::get_global();
}

#[test]
fn build_and_update_global_sets_context() {
    ResizeCrashContextBuilder::new(20000)
        .gate(sample_gate_active())
        .build_and_update_global();
    let got = ResizeCrashContext::get_global();
    assert!(got.is_some());
    ResizeCrashContext::clear_global();
}

// ── Concurrent access ────────────────────────────────────────────────

#[test]
fn concurrent_global_updates_do_not_panic() {
    let barrier = Arc::new(std::sync::Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let b = Arc::clone(&barrier);
            thread::spawn(move || {
                b.wait();
                let ctx = ResizeCrashContextBuilder::new(30000 + i)
                    .gate(sample_gate_active())
                    .queue_depths(ResizeQueueDepths {
                        pending_intents: i as u32,
                        active_transactions: 1,
                        ..Default::default()
                    })
                    .build();
                ResizeCrashContext::update_global(ctx);
                let _ = ResizeCrashContext::get_global();
            })
        })
        .collect();
    for h in handles {
        h.join().expect("thread should not panic");
    }
    // At least one write should have succeeded.
    assert!(ResizeCrashContext::get_global().is_some());
    ResizeCrashContext::clear_global();
}

#[test]
fn concurrent_reads_and_writes_do_not_panic() {
    let barrier = Arc::new(std::sync::Barrier::new(16));
    let handles: Vec<_> = (0..16)
        .map(|i| {
            let b = Arc::clone(&barrier);
            thread::spawn(move || {
                b.wait();
                if i % 2 == 0 {
                    // Writers
                    let ctx = ResizeCrashContextBuilder::new(40000 + i)
                        .gate(sample_gate_active())
                        .build();
                    ResizeCrashContext::update_global(ctx);
                } else {
                    // Readers
                    let _ = ResizeCrashContext::get_global();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().expect("thread should not panic");
    }
    ResizeCrashContext::clear_global();
}

// ── Stress: many in-flight + decisions + domains ─────────────────────

#[test]
fn stress_large_context_serializes_and_deserializes() {
    let mut builder = ResizeCrashContextBuilder::new(50000)
        .gate(sample_gate_active())
        .queue_depths(ResizeQueueDepths {
            pending_intents: 200,
            active_transactions: 50,
            input_backlog: 100,
            tracked_panes: 250,
            frame_budget_units: 40,
            last_frame_spent_units: 35,
        })
        .storm_state(StormState {
            tabs_in_storm: 10,
            storm_window_ms: 200,
            storm_threshold: 20,
            total_storm_events: 500,
            total_storm_throttled: 100,
        });

    // 100 in-flight transactions
    for i in 0..100u64 {
        builder = builder.add_in_flight(InFlightTransaction {
            pane_id: i,
            intent_seq: i * 5,
            work_class: if i % 3 == 0 {
                ResizeWorkClass::Background
            } else {
                ResizeWorkClass::Interactive
            },
            phase: match i % 4 {
                0 => None,
                1 => Some(ResizeExecutionPhase::Preparing),
                2 => Some(ResizeExecutionPhase::Reflowing),
                _ => Some(ResizeExecutionPhase::Presenting),
            },
            phase_started_at_ms: if i % 4 == 0 { None } else { Some(50000 + i) },
            domain: match i % 3 {
                0 => ResizeDomain::Local,
                1 => ResizeDomain::Ssh {
                    host: format!("host-{i}"),
                },
                _ => ResizeDomain::Mux {
                    endpoint: format!("ws://mux-{i}:8080"),
                },
            },
            tab_id: Some(i / 5),
            deferrals: (i % 10) as u32,
            force_served: i % 20 == 0,
        });
    }

    // 100 policy decisions (will be bounded to 64)
    for i in 0..100u64 {
        builder = builder.add_policy_decision(PolicyDecision {
            at_ms: 50000 + i,
            kind: match i % 7 {
                0 => PolicyDecisionKind::StormThrottle,
                1 => PolicyDecisionKind::DomainBudgetThrottle,
                2 => PolicyDecisionKind::StarvationBypass,
                3 => PolicyDecisionKind::OverloadReject,
                4 => PolicyDecisionKind::OverloadEvict,
                5 => PolicyDecisionKind::InputGuardrailActivated,
                _ => PolicyDecisionKind::GateSuppressed,
            },
            pane_id: Some(i),
            rationale: format!("stress decision {i}"),
        });
    }

    // 15 domain budgets
    for i in 0..15u32 {
        builder = builder.add_domain_budget(DomainBudgetEntry {
            domain_key: format!("domain-{i}"),
            weight: i + 1,
            allocated_units: (i + 1) * 3,
            consumed_units: (i + 1) * 2,
        });
    }

    let ctx = builder.build();
    assert_eq!(ctx.in_flight.len(), 100);
    assert_eq!(ctx.policy_decisions.len(), 64); // bounded
    assert_eq!(ctx.domain_budgets.len(), 15);

    // Serialize and deserialize.
    let json = serde_json::to_string(&ctx).unwrap();
    let rt: ResizeCrashContext = serde_json::from_str(&json).unwrap();
    assert_eq!(rt, ctx);

    // Verify the JSON is non-trivially sized.
    assert!(
        json.len() > 10_000,
        "expected large JSON, got {} bytes",
        json.len()
    );
}

// ── Queue depths edge cases ──────────────────────────────────────────

#[test]
fn queue_depths_with_extreme_values() {
    let depths = ResizeQueueDepths {
        pending_intents: u32::MAX,
        active_transactions: u32::MAX,
        input_backlog: u32::MAX,
        tracked_panes: u32::MAX,
        frame_budget_units: u32::MAX,
        last_frame_spent_units: u32::MAX,
    };
    let ctx = ResizeCrashContextBuilder::new(u64::MAX)
        .queue_depths(depths)
        .build();
    let json = serde_json::to_string(&ctx).unwrap();
    let rt: ResizeCrashContext = serde_json::from_str(&json).unwrap();
    assert_eq!(rt.queue_depths.pending_intents, u32::MAX);
    assert_eq!(rt.queue_depths.frame_budget_units, u32::MAX);
    assert_eq!(rt.captured_at_ms, u64::MAX);
}

#[test]
fn queue_depths_default_is_all_zeroes() {
    let depths = ResizeQueueDepths::default();
    assert_eq!(depths.pending_intents, 0);
    assert_eq!(depths.active_transactions, 0);
    assert_eq!(depths.input_backlog, 0);
    assert_eq!(depths.tracked_panes, 0);
    assert_eq!(depths.frame_budget_units, 0);
    assert_eq!(depths.last_frame_spent_units, 0);
}

// ── Storm state edge cases ───────────────────────────────────────────

#[test]
fn storm_state_with_extreme_values() {
    let storm = StormState {
        tabs_in_storm: u32::MAX,
        storm_window_ms: u64::MAX,
        storm_threshold: u32::MAX,
        total_storm_events: u64::MAX,
        total_storm_throttled: u64::MAX,
    };
    let ctx = ResizeCrashContextBuilder::new(60000)
        .storm_state(storm)
        .build();
    let json = serde_json::to_string(&ctx).unwrap();
    let rt: ResizeCrashContext = serde_json::from_str(&json).unwrap();
    assert_eq!(rt.storm_state.tabs_in_storm, u32::MAX);
    assert_eq!(rt.storm_state.total_storm_events, u64::MAX);
}

// ── InFlightTransaction edge cases ───────────────────────────────────

#[test]
fn in_flight_with_high_deferral_count_and_force_served() {
    let txn = InFlightTransaction {
        pane_id: u64::MAX,
        intent_seq: u64::MAX,
        work_class: ResizeWorkClass::Background,
        phase: Some(ResizeExecutionPhase::Presenting),
        phase_started_at_ms: Some(u64::MAX),
        domain: ResizeDomain::Local,
        tab_id: Some(u64::MAX),
        deferrals: u32::MAX,
        force_served: true,
    };
    let json = serde_json::to_string(&txn).unwrap();
    let rt: InFlightTransaction = serde_json::from_str(&json).unwrap();
    assert_eq!(rt.pane_id, u64::MAX);
    assert_eq!(rt.deferrals, u32::MAX);
    assert!(rt.force_served);
}

#[test]
fn in_flight_with_no_optional_fields() {
    let txn = InFlightTransaction {
        pane_id: 0,
        intent_seq: 0,
        work_class: ResizeWorkClass::Interactive,
        phase: None,
        phase_started_at_ms: None,
        domain: ResizeDomain::Local,
        tab_id: None,
        deferrals: 0,
        force_served: false,
    };
    let json = serde_json::to_string(&txn).unwrap();
    let rt: InFlightTransaction = serde_json::from_str(&json).unwrap();
    assert!(rt.phase.is_none());
    assert!(rt.phase_started_at_ms.is_none());
    assert!(rt.tab_id.is_none());
}

// ── DomainBudgetEntry edge cases ─────────────────────────────────────

#[test]
fn domain_budget_consumed_exceeds_allocated() {
    // The struct allows consumed > allocated (over-spend); verify it round-trips.
    let entry = DomainBudgetEntry {
        domain_key: "local".into(),
        weight: 1,
        allocated_units: 5,
        consumed_units: 10,
    };
    let json = serde_json::to_string(&entry).unwrap();
    let rt: DomainBudgetEntry = serde_json::from_str(&json).unwrap();
    assert_eq!(rt.consumed_units, 10);
    assert!(rt.consumed_units > rt.allocated_units);
}

#[test]
fn domain_budget_empty_key() {
    let entry = DomainBudgetEntry {
        domain_key: String::new(),
        weight: 0,
        allocated_units: 0,
        consumed_units: 0,
    };
    let json = serde_json::to_string(&entry).unwrap();
    let rt: DomainBudgetEntry = serde_json::from_str(&json).unwrap();
    assert!(rt.domain_key.is_empty());
}

// ── Clone and equality ───────────────────────────────────────────────

#[test]
fn context_clone_is_independent() {
    let ctx1 = rich_context();
    let mut ctx2 = ctx1.clone();
    ctx2.captured_at_ms = 0;
    // Mutating the clone should not affect the original.
    assert_eq!(ctx1.captured_at_ms, 99999);
    assert_eq!(ctx2.captured_at_ms, 0);
    assert_ne!(ctx1, ctx2);
}

// ── PolicyDecision with None pane_id ─────────────────────────────────

#[test]
fn policy_decision_with_none_pane_serializes() {
    let decision = PolicyDecision {
        at_ms: 1000,
        kind: PolicyDecisionKind::InputGuardrailActivated,
        pane_id: None,
        rationale: "global input pressure".into(),
    };
    let json = serde_json::to_string(&decision).unwrap();
    assert!(json.contains("null") || json.contains("\"pane_id\":null"));
    let rt: PolicyDecision = serde_json::from_str(&json).unwrap();
    assert!(rt.pane_id.is_none());
}

#[test]
fn policy_decision_with_empty_rationale() {
    let decision = PolicyDecision {
        at_ms: 0,
        kind: PolicyDecisionKind::OverloadEvict,
        pane_id: Some(0),
        rationale: String::new(),
    };
    let json = serde_json::to_string(&decision).unwrap();
    let rt: PolicyDecision = serde_json::from_str(&json).unwrap();
    assert!(rt.rationale.is_empty());
}

// ── Crash bundle integration ─────────────────────────────────────────

fn basic_report() -> CrashReport {
    CrashReport {
        message: "test crash".to_string(),
        location: Some("src/main.rs:42:5".to_string()),
        backtrace: Some("   0: std::backtrace\n   1: my_func".to_string()),
        timestamp: 1_700_000_000,
        pid: 12345,
        thread_name: Some("main".to_string()),
    }
}

#[test]
fn crash_bundle_includes_forensics_file_when_provided() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = rich_context();
    let path = write_crash_bundle(tmp.path(), &basic_report(), None, Some(&ctx)).unwrap();

    // resize_forensics.json should exist in the bundle.
    let forensics_path = path.join("resize_forensics.json");
    assert!(forensics_path.exists(), "resize_forensics.json missing");

    let content = fs::read_to_string(&forensics_path).unwrap();
    let rt: ResizeCrashContext = serde_json::from_str(&content).unwrap();
    assert_eq!(rt, ctx);
}

#[test]
fn crash_bundle_manifest_has_resize_forensics_flag_when_provided() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = ResizeCrashContextBuilder::new(70000)
        .gate(sample_gate_active())
        .build();
    let path = write_crash_bundle(tmp.path(), &basic_report(), None, Some(&ctx)).unwrap();

    let manifest_json = fs::read_to_string(path.join("manifest.json")).unwrap();
    let manifest: CrashManifest = serde_json::from_str(&manifest_json).unwrap();
    assert!(
        manifest.has_resize_forensics,
        "manifest should indicate resize forensics present"
    );
    assert!(
        manifest.files.contains(&"resize_forensics.json".to_string()),
        "files list should include resize_forensics.json"
    );
}

#[test]
fn crash_bundle_manifest_no_forensics_flag_when_none() {
    let tmp = tempfile::tempdir().unwrap();
    let path = write_crash_bundle(tmp.path(), &basic_report(), None, None).unwrap();

    let manifest_json = fs::read_to_string(path.join("manifest.json")).unwrap();
    let manifest: CrashManifest = serde_json::from_str(&manifest_json).unwrap();
    assert!(
        !manifest.has_resize_forensics,
        "manifest should not indicate resize forensics when None"
    );
    assert!(
        !path.join("resize_forensics.json").exists(),
        "resize_forensics.json should not exist when None"
    );
}

#[test]
fn crash_bundle_forensics_data_survives_redaction() {
    let tmp = tempfile::tempdir().unwrap();
    // The forensics context itself doesn't contain secrets, so redaction
    // should preserve all data.
    let ctx = ResizeCrashContextBuilder::new(80000)
        .gate(sample_gate_active())
        .add_in_flight(InFlightTransaction {
            pane_id: 42,
            intent_seq: 7,
            work_class: ResizeWorkClass::Interactive,
            phase: Some(ResizeExecutionPhase::Reflowing),
            phase_started_at_ms: Some(80000),
            domain: ResizeDomain::Ssh {
                host: "secure-host.internal".into(),
            },
            tab_id: Some(1),
            deferrals: 0,
            force_served: false,
        })
        .add_policy_decision(PolicyDecision {
            at_ms: 79999,
            kind: PolicyDecisionKind::StormThrottle,
            pane_id: Some(42),
            rationale: "tab 1 in storm mode".into(),
        })
        .build();

    let path = write_crash_bundle(tmp.path(), &basic_report(), None, Some(&ctx)).unwrap();
    let content = fs::read_to_string(path.join("resize_forensics.json")).unwrap();

    // All forensics data should be intact (no secrets to redact).
    assert!(content.contains("secure-host.internal"));
    assert!(content.contains("tab 1 in storm mode"));
    assert!(content.contains("80000"));

    let rt: ResizeCrashContext = serde_json::from_str(&content).unwrap();
    assert_eq!(rt.in_flight.len(), 1);
    assert_eq!(rt.policy_decisions.len(), 1);
}
