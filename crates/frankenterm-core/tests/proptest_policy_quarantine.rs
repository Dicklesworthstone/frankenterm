//! Property tests for policy_quarantine module.
//!
//! Covers serde roundtrip for all 11 serializable types plus behavioral
//! invariants for QuarantineRegistry, KillSwitch, and severity ordering.

use frankenterm_core::policy_quarantine::*;
use proptest::prelude::*;

// =============================================================================
// Arbitrary strategies
// =============================================================================

fn arb_quarantine_reason() -> impl Strategy<Value = QuarantineReason> {
    prop_oneof![
        ("[a-z0-9_]{1,20}", "[a-z ]{1,40}")
            .prop_map(|(rule_id, detail)| QuarantineReason::PolicyViolation { rule_id, detail }),
        "[a-z0-9_]{1,20}"
            .prop_map(|credential_id| QuarantineReason::CredentialCompromise { credential_id }),
        ("[a-z_]{1,15}", "[0-9.%]{1,10}")
            .prop_map(|(metric, observed)| QuarantineReason::AnomalousBehavior { metric, observed }),
        ("[a-z]{1,15}", "[a-z ]{1,30}")
            .prop_map(|(operator, note)| QuarantineReason::OperatorDirected { operator, note }),
        "[a-z0-9_-]{1,20}"
            .prop_map(|circuit_id| QuarantineReason::CircuitBreakerTrip { circuit_id }),
        "[a-z0-9_-]{1,20}".prop_map(|parent_component_id| {
            QuarantineReason::CascadeFromParent {
                parent_component_id,
            }
        }),
    ]
}

fn arb_quarantine_severity() -> impl Strategy<Value = QuarantineSeverity> {
    prop_oneof![
        Just(QuarantineSeverity::Advisory),
        Just(QuarantineSeverity::Restricted),
        Just(QuarantineSeverity::Isolated),
        Just(QuarantineSeverity::Terminated),
    ]
}

fn arb_quarantine_state() -> impl Strategy<Value = QuarantineState> {
    prop_oneof![
        Just(QuarantineState::Clear),
        Just(QuarantineState::Quarantined),
        Just(QuarantineState::ProbationaryRelease),
    ]
}

fn arb_component_kind() -> impl Strategy<Value = ComponentKind> {
    prop_oneof![
        Just(ComponentKind::Connector),
        Just(ComponentKind::Pane),
        Just(ComponentKind::Workflow),
        Just(ComponentKind::Agent),
        Just(ComponentKind::Session),
    ]
}

fn arb_kill_switch_level() -> impl Strategy<Value = KillSwitchLevel> {
    prop_oneof![
        Just(KillSwitchLevel::Disarmed),
        Just(KillSwitchLevel::SoftStop),
        Just(KillSwitchLevel::HardStop),
        Just(KillSwitchLevel::EmergencyHalt),
    ]
}

fn arb_quarantined_component() -> impl Strategy<Value = QuarantinedComponent> {
    (
        "[a-z0-9_-]{1,20}",
        arb_component_kind(),
        arb_quarantine_state(),
        arb_quarantine_severity(),
        arb_quarantine_reason(),
        0..u64::MAX,
        0..u64::MAX,
        0..u64::MAX,
        "[a-z]{1,10}",
        0..100u32,
    )
        .prop_map(
            |(
                component_id,
                component_kind,
                state,
                severity,
                reason,
                quarantined_at_ms,
                expires_at_ms,
                last_reviewed_at_ms,
                imposed_by,
                quarantine_count,
            )| {
                QuarantinedComponent {
                    component_id,
                    component_kind,
                    state,
                    severity,
                    reason,
                    quarantined_at_ms,
                    expires_at_ms,
                    last_reviewed_at_ms,
                    imposed_by,
                    quarantine_count,
                }
            },
        )
}

fn arb_kill_switch() -> impl Strategy<Value = KillSwitch> {
    (
        arb_kill_switch_level(),
        0..u64::MAX,
        "[a-z]{0,10}",
        "[a-z ]{0,30}",
        0..u64::MAX,
    )
        .prop_map(
            |(level, changed_at_ms, changed_by, reason, auto_disarm_at_ms)| KillSwitch {
                level,
                changed_at_ms,
                changed_by,
                reason,
                auto_disarm_at_ms,
            },
        )
}

fn arb_quarantine_audit_type() -> impl Strategy<Value = QuarantineAuditType> {
    prop_oneof![
        Just(QuarantineAuditType::Imposed),
        Just(QuarantineAuditType::SeverityEscalated),
        Just(QuarantineAuditType::SeverityDeescalated),
        Just(QuarantineAuditType::Released),
        Just(QuarantineAuditType::ProbationStarted),
        Just(QuarantineAuditType::ProbationCompleted),
        Just(QuarantineAuditType::ProbationRevoked),
        Just(QuarantineAuditType::Expired),
        Just(QuarantineAuditType::KillSwitchTripped),
        Just(QuarantineAuditType::KillSwitchReset),
        Just(QuarantineAuditType::KillSwitchAutoDisarmed),
    ]
}

fn arb_quarantine_audit_event() -> impl Strategy<Value = QuarantineAuditEvent> {
    (
        0..u64::MAX,
        arb_quarantine_audit_type(),
        "[a-z0-9_-]{0,20}",
        proptest::option::of(arb_component_kind()),
        "[a-z]{1,10}",
        "[a-z ]{0,30}",
    )
        .prop_map(
            |(timestamp_ms, event_type, component_id, component_kind, actor, detail)| {
                QuarantineAuditEvent {
                    timestamp_ms,
                    event_type,
                    component_id,
                    component_kind,
                    actor,
                    detail,
                }
            },
        )
}

fn arb_quarantine_telemetry() -> impl Strategy<Value = QuarantineTelemetry> {
    (
        0..1000u64,
        0..1000u64,
        0..1000u64,
        0..1000u64,
        0..1000u64,
        0..1000u64,
        0..1000u64,
        0..1000u64,
        0..1000u64,
        0..1000u64,
    )
        .prop_map(
            |(
                quarantines_imposed,
                quarantines_released,
                quarantines_expired,
                probations_started,
                probations_completed,
                probations_revoked,
                severity_escalations,
                severity_deescalations,
                kill_switch_trips,
                kill_switch_resets,
            )| {
                QuarantineTelemetry {
                    quarantines_imposed,
                    quarantines_released,
                    quarantines_expired,
                    probations_started,
                    probations_completed,
                    probations_revoked,
                    severity_escalations,
                    severity_deescalations,
                    kill_switch_trips,
                    kill_switch_resets,
                }
            },
        )
}

fn arb_quarantine_telemetry_snapshot() -> impl Strategy<Value = QuarantineTelemetrySnapshot> {
    (arb_quarantine_telemetry(), 0..u64::MAX, 0..100u32, arb_kill_switch_level()).prop_map(
        |(counters, captured_at_ms, active_quarantines, kill_switch_level)| {
            QuarantineTelemetrySnapshot {
                captured_at_ms,
                counters,
                active_quarantines,
                kill_switch_level,
            }
        },
    )
}

// =============================================================================
// Serde roundtrip tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // -- QuarantineReason --

    #[test]
    fn quarantine_reason_json_roundtrip(reason in arb_quarantine_reason()) {
        let json = serde_json::to_string(&reason).unwrap();
        let back: QuarantineReason = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&reason, &back);
    }

    // -- QuarantineSeverity --

    #[test]
    fn quarantine_severity_json_roundtrip(sev in arb_quarantine_severity()) {
        let json = serde_json::to_string(&sev).unwrap();
        let back: QuarantineSeverity = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(sev, back);
    }

    // -- QuarantineState --

    #[test]
    fn quarantine_state_json_roundtrip(state in arb_quarantine_state()) {
        let json = serde_json::to_string(&state).unwrap();
        let back: QuarantineState = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(state, back);
    }

    // -- ComponentKind --

    #[test]
    fn component_kind_json_roundtrip(kind in arb_component_kind()) {
        let json = serde_json::to_string(&kind).unwrap();
        let back: ComponentKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(kind, back);
    }

    // -- QuarantinedComponent --

    #[test]
    fn quarantined_component_json_roundtrip(comp in arb_quarantined_component()) {
        let json = serde_json::to_string(&comp).unwrap();
        let back: QuarantinedComponent = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&comp.component_id, &back.component_id);
        prop_assert_eq!(&comp.component_kind, &back.component_kind);
        prop_assert_eq!(comp.state, back.state);
        prop_assert_eq!(comp.severity, back.severity);
        prop_assert_eq!(&comp.reason, &back.reason);
        prop_assert_eq!(comp.quarantined_at_ms, back.quarantined_at_ms);
        prop_assert_eq!(comp.expires_at_ms, back.expires_at_ms);
        prop_assert_eq!(comp.quarantine_count, back.quarantine_count);
    }

    // -- KillSwitchLevel --

    #[test]
    fn kill_switch_level_json_roundtrip(level in arb_kill_switch_level()) {
        let json = serde_json::to_string(&level).unwrap();
        let back: KillSwitchLevel = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(level, back);
    }

    // -- KillSwitch --

    #[test]
    fn kill_switch_json_roundtrip(ks in arb_kill_switch()) {
        let json = serde_json::to_string(&ks).unwrap();
        let back: KillSwitch = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(ks.level, back.level);
        prop_assert_eq!(ks.changed_at_ms, back.changed_at_ms);
        prop_assert_eq!(&ks.changed_by, &back.changed_by);
        prop_assert_eq!(&ks.reason, &back.reason);
        prop_assert_eq!(ks.auto_disarm_at_ms, back.auto_disarm_at_ms);
    }

    // -- QuarantineAuditType --

    #[test]
    fn quarantine_audit_type_json_roundtrip(at in arb_quarantine_audit_type()) {
        let json = serde_json::to_string(&at).unwrap();
        let back: QuarantineAuditType = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(at, back);
    }

    // -- QuarantineAuditEvent --

    #[test]
    fn quarantine_audit_event_json_roundtrip(evt in arb_quarantine_audit_event()) {
        let json = serde_json::to_string(&evt).unwrap();
        let back: QuarantineAuditEvent = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(evt.timestamp_ms, back.timestamp_ms);
        prop_assert_eq!(evt.event_type, back.event_type);
        prop_assert_eq!(&evt.component_id, &back.component_id);
        prop_assert_eq!(&evt.component_kind, &back.component_kind);
        prop_assert_eq!(&evt.actor, &back.actor);
        prop_assert_eq!(&evt.detail, &back.detail);
    }

    // -- QuarantineTelemetry --

    #[test]
    fn quarantine_telemetry_json_roundtrip(t in arb_quarantine_telemetry()) {
        let json = serde_json::to_string(&t).unwrap();
        let back: QuarantineTelemetry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(t, back);
    }

    // -- QuarantineTelemetrySnapshot --

    #[test]
    fn quarantine_telemetry_snapshot_json_roundtrip(snap in arb_quarantine_telemetry_snapshot()) {
        let json = serde_json::to_string(&snap).unwrap();
        let back: QuarantineTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap, back);
    }
}

// =============================================================================
// Behavioral property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    // -- Severity ordering is total --

    #[test]
    fn severity_ordering_total(a in arb_quarantine_severity(), b in arb_quarantine_severity()) {
        // Total order: exactly one of <, ==, > holds
        let lt = a < b;
        let eq = a == b;
        let gt = a > b;
        let count = [lt, eq, gt].iter().filter(|&&x| x).count();
        prop_assert_eq!(count, 1);
    }

    // -- Kill switch level ordering is total --

    #[test]
    fn kill_switch_level_ordering_total(a in arb_kill_switch_level(), b in arb_kill_switch_level()) {
        let lt = a < b;
        let eq = a == b;
        let gt = a > b;
        let count = [lt, eq, gt].iter().filter(|&&x| x).count();
        prop_assert_eq!(count, 1);
    }

    // -- QuarantinedComponent expiry semantics --

    #[test]
    fn indefinite_quarantine_never_expires(now_ms in 0..u64::MAX) {
        let comp = QuarantinedComponent {
            component_id: "c1".into(),
            component_kind: ComponentKind::Pane,
            state: QuarantineState::Quarantined,
            severity: QuarantineSeverity::Isolated,
            reason: QuarantineReason::PolicyViolation {
                rule_id: "r1".into(),
                detail: "test".into(),
            },
            quarantined_at_ms: 0,
            expires_at_ms: 0, // indefinite
            last_reviewed_at_ms: 0,
            imposed_by: "op".into(),
            quarantine_count: 1,
        };
        prop_assert!(!comp.is_expired_at(now_ms));
    }

    #[test]
    fn expiry_monotonic(
        expires_at in 1..u64::MAX / 2,
        delta in 0..1000u64
    ) {
        let comp = QuarantinedComponent {
            component_id: "c1".into(),
            component_kind: ComponentKind::Connector,
            state: QuarantineState::Quarantined,
            severity: QuarantineSeverity::Isolated,
            reason: QuarantineReason::CircuitBreakerTrip {
                circuit_id: "cb1".into(),
            },
            quarantined_at_ms: 0,
            expires_at_ms: expires_at,
            last_reviewed_at_ms: 0,
            imposed_by: "sys".into(),
            quarantine_count: 1,
        };
        // Before expiry -> not expired
        if expires_at > delta {
            prop_assert!(!comp.is_expired_at(expires_at - delta - 1));
        }
        // At or after expiry -> expired
        prop_assert!(comp.is_expired_at(expires_at + delta));
    }

    // -- Blocking semantics based on severity --

    #[test]
    fn advisory_quarantine_blocks_nothing(kind in arb_component_kind()) {
        let comp = QuarantinedComponent {
            component_id: "test".into(),
            component_kind: kind,
            state: QuarantineState::Quarantined,
            severity: QuarantineSeverity::Advisory,
            reason: QuarantineReason::OperatorDirected {
                operator: "op".into(),
                note: "test".into(),
            },
            quarantined_at_ms: 0,
            expires_at_ms: 0,
            last_reviewed_at_ms: 0,
            imposed_by: "op".into(),
            quarantine_count: 1,
        };
        prop_assert!(!comp.blocks_writes());
        prop_assert!(!comp.blocks_all());
    }

    #[test]
    fn isolated_blocks_all(kind in arb_component_kind()) {
        let comp = QuarantinedComponent {
            component_id: "test".into(),
            component_kind: kind,
            state: QuarantineState::Quarantined,
            severity: QuarantineSeverity::Isolated,
            reason: QuarantineReason::CredentialCompromise {
                credential_id: "cred1".into(),
            },
            quarantined_at_ms: 0,
            expires_at_ms: 0,
            last_reviewed_at_ms: 0,
            imposed_by: "sys".into(),
            quarantine_count: 1,
        };
        prop_assert!(comp.blocks_writes());
        prop_assert!(comp.blocks_all());
    }

    #[test]
    fn clear_state_blocks_nothing(sev in arb_quarantine_severity()) {
        let comp = QuarantinedComponent {
            component_id: "test".into(),
            component_kind: ComponentKind::Agent,
            state: QuarantineState::Clear,
            severity: sev,
            reason: QuarantineReason::PolicyViolation {
                rule_id: "r1".into(),
                detail: "test".into(),
            },
            quarantined_at_ms: 0,
            expires_at_ms: 0,
            last_reviewed_at_ms: 0,
            imposed_by: "op".into(),
            quarantine_count: 1,
        };
        // Clear state never blocks regardless of severity
        prop_assert!(!comp.blocks_writes());
        prop_assert!(!comp.blocks_all());
    }

    // -- KillSwitch state machine properties --

    #[test]
    fn disarmed_allows_everything(
        changed_at in 0..u64::MAX,
        by in "[a-z]{1,5}",
        reason in "[a-z ]{1,10}"
    ) {
        let ks = KillSwitch {
            level: KillSwitchLevel::Disarmed,
            changed_at_ms: changed_at,
            changed_by: by,
            reason,
            auto_disarm_at_ms: 0,
        };
        prop_assert!(ks.allows_new_workflows());
        prop_assert!(ks.allows_inflight());
        prop_assert!(!ks.is_emergency());
    }

    #[test]
    fn emergency_blocks_everything(
        changed_at in 0..u64::MAX,
        by in "[a-z]{1,5}",
        reason in "[a-z ]{1,10}"
    ) {
        let ks = KillSwitch {
            level: KillSwitchLevel::EmergencyHalt,
            changed_at_ms: changed_at,
            changed_by: by,
            reason,
            auto_disarm_at_ms: 0,
        };
        prop_assert!(!ks.allows_new_workflows());
        prop_assert!(!ks.allows_inflight());
        prop_assert!(ks.is_emergency());
    }

    #[test]
    fn kill_switch_trip_updates_state(
        level in arb_kill_switch_level(),
        now_ms in 0..u64::MAX / 2
    ) {
        let mut ks = KillSwitch::disarmed();
        ks.trip(level, "op", "reason", now_ms);
        prop_assert_eq!(ks.level, level);
        prop_assert_eq!(ks.changed_at_ms, now_ms);
        prop_assert_eq!(&ks.changed_by, "op");
    }

    #[test]
    fn kill_switch_reset_always_disarms(
        initial_level in arb_kill_switch_level(),
        now_ms in 0..u64::MAX / 2
    ) {
        let mut ks = KillSwitch::disarmed();
        ks.trip(initial_level, "op", "incident", now_ms);
        ks.reset("admin", now_ms + 1000);
        prop_assert_eq!(ks.level, KillSwitchLevel::Disarmed);
        prop_assert!(ks.allows_new_workflows());
        prop_assert!(ks.allows_inflight());
    }

    #[test]
    fn auto_disarm_before_timeout_is_noop(
        timeout_ms in 1..100_000u64,
        now_ms in 0..u64::MAX / 4
    ) {
        let mut ks = KillSwitch::disarmed();
        ks.trip_with_timeout(KillSwitchLevel::SoftStop, "op", "temp", now_ms, timeout_ms);
        // Just before timeout: should not auto-disarm
        let before = now_ms + timeout_ms - 1;
        let did_disarm = ks.should_auto_disarm(before);
        prop_assert!(!did_disarm);
    }

    #[test]
    fn auto_disarm_at_or_after_timeout(
        timeout_ms in 1..100_000u64,
        now_ms in 0..u64::MAX / 4,
        extra in 0..1000u64
    ) {
        let mut ks = KillSwitch::disarmed();
        ks.trip_with_timeout(KillSwitchLevel::SoftStop, "op", "temp", now_ms, timeout_ms);
        // At or after timeout
        let after = now_ms + timeout_ms + extra;
        prop_assert!(ks.should_auto_disarm(after));
        // Tick should actually disarm
        let did_tick = ks.tick(after);
        prop_assert!(did_tick);
        prop_assert_eq!(ks.level, KillSwitchLevel::Disarmed);
    }

    // -- Registry behavioral properties --

    #[test]
    fn quarantine_then_release_clears(
        kind in arb_component_kind(),
        sev in arb_quarantine_severity()
    ) {
        let mut reg = QuarantineRegistry::new();
        reg.quarantine(
            "comp", kind, sev,
            QuarantineReason::PolicyViolation { rule_id: "r".into(), detail: "d".into() },
            "op", 1000, 0,
        ).unwrap();
        prop_assert!(reg.is_quarantined("comp"));
        reg.release("comp", "op", false, 2000).unwrap();
        prop_assert!(!reg.is_quarantined("comp"));
    }

    #[test]
    fn telemetry_imposed_matches_quarantine_count(
        n in 1..10usize
    ) {
        let mut reg = QuarantineRegistry::new();
        for i in 0..n {
            reg.quarantine(
                &format!("c{i}"),
                ComponentKind::Pane,
                QuarantineSeverity::Isolated,
                QuarantineReason::PolicyViolation { rule_id: "r1".into(), detail: "d".into() },
                "op", i as u64 * 1000, 0,
            ).unwrap();
        }
        let snap = reg.telemetry_snapshot(n as u64 * 1000);
        prop_assert_eq!(snap.counters.quarantines_imposed, n as u64);
        prop_assert_eq!(snap.active_quarantines, n as u32);
    }

    #[test]
    fn escalate_increases_severity(
        starting_sev_idx in 0..3u8
    ) {
        let sev = match starting_sev_idx {
            0 => QuarantineSeverity::Advisory,
            1 => QuarantineSeverity::Restricted,
            2 => QuarantineSeverity::Isolated,
            _ => unreachable!(),
        };
        let mut reg = QuarantineRegistry::new();
        reg.quarantine(
            "c1", ComponentKind::Agent, sev,
            QuarantineReason::AnomalousBehavior { metric: "cpu".into(), observed: "99".into() },
            "op", 1000, 0,
        ).unwrap();
        let new_sev = reg.escalate("c1", "op", 2000).unwrap();
        prop_assert!(new_sev > sev);
    }

    #[test]
    fn deescalate_decreases_severity(
        starting_sev_idx in 1..4u8
    ) {
        let sev = match starting_sev_idx {
            1 => QuarantineSeverity::Restricted,
            2 => QuarantineSeverity::Isolated,
            3 => QuarantineSeverity::Terminated,
            _ => unreachable!(),
        };
        let mut reg = QuarantineRegistry::new();
        reg.quarantine(
            "c1", ComponentKind::Workflow, sev,
            QuarantineReason::OperatorDirected { operator: "admin".into(), note: "review".into() },
            "op", 1000, 0,
        ).unwrap();
        let new_sev = reg.deescalate("c1", "op", 2000).unwrap();
        prop_assert!(new_sev < sev);
    }

    // -- Display roundtrip consistency --

    #[test]
    fn quarantine_reason_display_nonempty(reason in arb_quarantine_reason()) {
        let display = reason.to_string();
        prop_assert!(!display.is_empty());
    }

    #[test]
    fn quarantine_severity_display_matches_variant(sev in arb_quarantine_severity()) {
        let display = sev.to_string();
        let expected = match sev {
            QuarantineSeverity::Advisory => "advisory",
            QuarantineSeverity::Restricted => "restricted",
            QuarantineSeverity::Isolated => "isolated",
            QuarantineSeverity::Terminated => "terminated",
        };
        prop_assert_eq!(display, expected);
    }

    #[test]
    fn kill_switch_level_display_matches(level in arb_kill_switch_level()) {
        let display = level.to_string();
        let expected = match level {
            KillSwitchLevel::Disarmed => "disarmed",
            KillSwitchLevel::SoftStop => "soft_stop",
            KillSwitchLevel::HardStop => "hard_stop",
            KillSwitchLevel::EmergencyHalt => "emergency_halt",
        };
        prop_assert_eq!(display, expected);
    }

    // -- Probation lifecycle --

    #[test]
    fn probation_complete_clears_state(kind in arb_component_kind()) {
        let mut reg = QuarantineRegistry::new();
        reg.quarantine(
            "c1", kind, QuarantineSeverity::Restricted,
            QuarantineReason::PolicyViolation { rule_id: "r".into(), detail: "d".into() },
            "op", 1000, 0,
        ).unwrap();
        reg.release("c1", "op", true, 2000).unwrap();
        prop_assert_eq!(reg.probationary_components(), vec!["c1".to_string()]);
        reg.complete_probation("c1", 3000).unwrap();
        prop_assert!(reg.probationary_components().is_empty());
        prop_assert!(!reg.is_quarantined("c1"));
    }

    #[test]
    fn probation_revoke_re_quarantines(kind in arb_component_kind()) {
        let mut reg = QuarantineRegistry::new();
        reg.quarantine(
            "c1", kind, QuarantineSeverity::Isolated,
            QuarantineReason::PolicyViolation { rule_id: "r".into(), detail: "d".into() },
            "op", 1000, 0,
        ).unwrap();
        reg.release("c1", "op", true, 2000).unwrap();
        prop_assert!(!reg.is_quarantined("c1"));
        reg.revoke_probation(
            "c1",
            QuarantineReason::AnomalousBehavior { metric: "m".into(), observed: "v".into() },
            3000,
        ).unwrap();
        prop_assert!(reg.is_quarantined("c1"));
        // Quarantine count should be 2
        prop_assert_eq!(reg.get("c1").unwrap().quarantine_count, 2);
    }

    // -- Expiry within registry --

    #[test]
    fn expire_only_clears_expired_components(
        ttl_a in 1000..5000u64,
        ttl_b in 6000..10000u64
    ) {
        let mut reg = QuarantineRegistry::new();
        // c1 expires at ttl_a, c2 at ttl_b
        reg.quarantine(
            "c1", ComponentKind::Pane, QuarantineSeverity::Isolated,
            QuarantineReason::PolicyViolation { rule_id: "r".into(), detail: "d".into() },
            "op", 0, ttl_a,
        ).unwrap();
        reg.quarantine(
            "c2", ComponentKind::Agent, QuarantineSeverity::Restricted,
            QuarantineReason::CircuitBreakerTrip { circuit_id: "cb".into() },
            "op", 0, ttl_b,
        ).unwrap();
        // At ttl_a: only c1 expires
        let expired = reg.expire_quarantines(ttl_a);
        prop_assert_eq!(expired, vec!["c1".to_string()]);
        prop_assert!(!reg.is_quarantined("c1"));
        prop_assert!(reg.is_quarantined("c2"));
        // At ttl_b: c2 also expires
        let expired2 = reg.expire_quarantines(ttl_b);
        prop_assert_eq!(expired2, vec!["c2".to_string()]);
    }

    // -- Kill switch + component interaction --

    #[test]
    fn kill_switch_soft_stop_blocks_all_writes(
        comp_id in "[a-z]{1,5}"
    ) {
        let mut reg = QuarantineRegistry::new();
        reg.trip_kill_switch(KillSwitchLevel::SoftStop, "op", "test", 1000);
        // Even unregistered components are blocked for writes
        prop_assert!(reg.is_blocked_for_writes(&comp_id));
    }

    #[test]
    fn kill_switch_emergency_blocks_all(
        comp_id in "[a-z]{1,5}"
    ) {
        let mut reg = QuarantineRegistry::new();
        reg.trip_kill_switch(KillSwitchLevel::EmergencyHalt, "op", "test", 1000);
        prop_assert!(reg.is_blocked_for_all(&comp_id));
    }
}
