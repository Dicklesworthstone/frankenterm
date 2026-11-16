//! Property-based tests for semantic_shock_response.rs.
//!
//! Covers serde roundtrips for ShockAction, SemanticShockConfig,
//! ShockRecord, PaneShockSummary, ShockResponseMetricsSnapshot;
//! config default invariants; responder handle_detection filtering,
//! eviction, pause/alert modes, clear semantics, pane isolation,
//! metrics consistency, and TraumaDecision generation.

use frankenterm_core::patterns::{AgentType, Detection, Severity};
use frankenterm_core::semantic_anomaly::ConformalShock;
use frankenterm_core::semantic_shock_response::{
    PaneShockSummary, SemanticShockConfig, SemanticShockResponder, ShockAction, ShockRecord,
    ShockResponseMetricsSnapshot,
};
use proptest::prelude::*;

// ── Helpers ──────────────────────────────────────────────────────────────────

fn make_semantic_detection(p_value: f64, distance: f64) -> Detection {
    Detection {
        rule_id: "core.semantic_anomaly:conformal_shock".to_string(),
        agent_type: AgentType::Unknown,
        event_type: "semantic_anomaly".to_string(),
        severity: Severity::Critical,
        confidence: 1.0 - p_value,
        extracted: serde_json::json!({
            "p_value": p_value,
            "distance": distance,
            "alpha": 0.05,
            "calibration_count": 200,
            "calibration_median": 0.12,
            "segment_len": 1024,
        }),
        matched_text: format!("Semantic anomaly: p={p_value:.4}, distance={distance:.3}"),
        span: (0, 0),
    }
}

// ── Strategies ──────────────────────────────────────────────────────────────

fn arb_shock_action() -> impl Strategy<Value = ShockAction> {
    prop_oneof![Just(ShockAction::Alert), Just(ShockAction::Pause),]
}

fn arb_config() -> impl Strategy<Value = SemanticShockConfig> {
    (
        any::<bool>(),
        arb_shock_action(),
        0.001f64..=0.1,
        1..=50usize,
        0..=3600u64,
        0..=120u64,
    )
        .prop_map(
            |(enabled, action, p_thresh, max_per_pane, auto_clear, cooldown)| SemanticShockConfig {
                enabled,
                action,
                p_value_threshold: p_thresh,
                max_shocks_per_pane: max_per_pane,
                auto_clear_seconds: auto_clear,
                notification_cooldown_seconds: cooldown,
            },
        )
}

fn arb_conformal_shock() -> impl Strategy<Value = ConformalShock> {
    (
        0.0f32..=10.0,
        0.0f64..=1.0,
        0.0f64..=0.1,
        1..=1000usize,
        0.0f32..=5.0,
    )
        .prop_map(
            |(distance, p_value, alpha, calibration_count, calibration_median)| ConformalShock {
                distance,
                p_value,
                alpha,
                calibration_count,
                calibration_median,
            },
        )
}

fn arb_shock_record() -> impl Strategy<Value = ShockRecord> {
    (
        1..=1000u64,
        arb_conformal_shock(),
        0..=10000usize,
        "[a-z._:]{3,20}",
        0.0f64..=1.0,
        0..=1_000_000u64,
        0..=1_000_000u64,
    )
        .prop_map(
            |(pane_id, shock, segment_len, rule_id, confidence, sequence, age_ms)| ShockRecord {
                pane_id,
                shock,
                segment_len,
                rule_id,
                confidence,
                sequence,
                age_ms,
            },
        )
}

fn arb_pane_shock_summary() -> impl Strategy<Value = PaneShockSummary> {
    (
        1..=1000u64,
        0..=50usize,
        proptest::option::of(arb_shock_record()),
        any::<bool>(),
    )
        .prop_map(
            |(pane_id, active_count, latest, is_paused)| PaneShockSummary {
                pane_id,
                active_count,
                latest,
                is_paused,
            },
        )
}

fn arb_metrics_snapshot() -> impl Strategy<Value = ShockResponseMetricsSnapshot> {
    (
        0..=10_000u64,
        0..=10_000u64,
        0..=10_000u64,
        0..=1_000u64,
        0..=1_000u64,
        0..=10_000u64,
        0..=10_000u64,
        0..=10_000u64,
    )
        .prop_map(
            |(
                detections_received,
                detections_filtered,
                shocks_recorded,
                panes_paused,
                panes_cleared,
                notifications_sent,
                notifications_suppressed,
                auto_cleared,
            )| {
                ShockResponseMetricsSnapshot {
                    detections_received,
                    detections_filtered,
                    shocks_recorded,
                    panes_paused,
                    panes_cleared,
                    notifications_sent,
                    notifications_suppressed,
                    auto_cleared,
                }
            },
        )
}

// ── ShockAction ─────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // 1. Serde roundtrip
    #[test]
    fn shock_action_serde_roundtrip(action in arb_shock_action()) {
        let json = serde_json::to_string(&action).unwrap();
        let restored: ShockAction = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, action);
    }

    // 2. Default is Alert
    #[test]
    fn shock_action_default_is_alert(_seed in 0..10u32) {
        prop_assert_eq!(ShockAction::default(), ShockAction::Alert);
    }

    // 3. Alert and Pause are distinct
    #[test]
    fn shock_action_distinct(_seed in 0..10u32) {
        prop_assert_ne!(ShockAction::Alert, ShockAction::Pause);
    }
}

// ── SemanticShockConfig ─────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 4. Serde roundtrip
    #[test]
    fn config_serde_roundtrip(config in arb_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let restored: SemanticShockConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.enabled, config.enabled);
        prop_assert_eq!(restored.action, config.action);
        prop_assert!((restored.p_value_threshold - config.p_value_threshold).abs() < 1e-10);
        prop_assert_eq!(restored.max_shocks_per_pane, config.max_shocks_per_pane);
        prop_assert_eq!(restored.auto_clear_seconds, config.auto_clear_seconds);
        prop_assert_eq!(
            restored.notification_cooldown_seconds,
            config.notification_cooldown_seconds
        );
    }

    // 5. Default config invariants
    #[test]
    fn config_default_invariants(_seed in 0..10u32) {
        let config = SemanticShockConfig::default();
        prop_assert!(config.enabled);
        prop_assert_eq!(config.action, ShockAction::Alert);
        prop_assert!(config.p_value_threshold > 0.0);
        prop_assert!(config.p_value_threshold <= 1.0);
        prop_assert!(config.max_shocks_per_pane > 0);
        prop_assert!(config.auto_clear_seconds > 0);
        prop_assert!(config.notification_cooldown_seconds > 0);
    }
}

// ── ShockRecord ─────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 6. Serde roundtrip
    #[test]
    fn shock_record_serde_roundtrip(record in arb_shock_record()) {
        let json = serde_json::to_string(&record).unwrap();
        let restored: ShockRecord = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.pane_id, record.pane_id);
        prop_assert_eq!(restored.segment_len, record.segment_len);
        prop_assert_eq!(&restored.rule_id, &record.rule_id);
        prop_assert_eq!(restored.sequence, record.sequence);
        prop_assert_eq!(restored.age_ms, record.age_ms);
        prop_assert!((restored.confidence - record.confidence).abs() < 1e-10);
        // ConformalShock fields
        prop_assert!((restored.shock.p_value - record.shock.p_value).abs() < 1e-10);
        prop_assert!((restored.shock.distance - record.shock.distance).abs() < 1e-4);
        prop_assert_eq!(restored.shock.calibration_count, record.shock.calibration_count);
    }
}

// ── PaneShockSummary ────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 7. Serde roundtrip
    #[test]
    fn pane_shock_summary_serde_roundtrip(summary in arb_pane_shock_summary()) {
        let json = serde_json::to_string(&summary).unwrap();
        let restored: PaneShockSummary = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.pane_id, summary.pane_id);
        prop_assert_eq!(restored.active_count, summary.active_count);
        prop_assert_eq!(restored.is_paused, summary.is_paused);
        prop_assert_eq!(restored.latest.is_some(), summary.latest.is_some());
    }
}

// ── ShockResponseMetricsSnapshot ────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 8. Serde roundtrip
    #[test]
    fn metrics_snapshot_serde_roundtrip(snap in arb_metrics_snapshot()) {
        let json = serde_json::to_string(&snap).unwrap();
        let restored: ShockResponseMetricsSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, snap);
    }

    // 9. Debug impl exists
    #[test]
    fn metrics_snapshot_debug(snap in arb_metrics_snapshot()) {
        let dbg = format!("{:?}", snap);
        prop_assert!(dbg.contains("detections_received"));
    }
}

// ── Responder: disabled ─────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    // 10. Disabled config processes nothing
    #[test]
    fn responder_disabled_ignores_all(
        pane_id in 1..=100u64,
        p_value in 0.0001f64..=0.009,
    ) {
        let config = SemanticShockConfig {
            enabled: false,
            ..Default::default()
        };
        let r = SemanticShockResponder::new(config);
        let det = make_semantic_detection(p_value, 0.95);
        let result = r.handle_detection(pane_id, &det);
        prop_assert!(result.is_none());
        prop_assert_eq!(r.metrics_snapshot().detections_received, 0);
    }
}

// ── Responder: p-value filtering ────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 11. p-value above threshold is filtered
    #[test]
    fn responder_filters_high_p_value(
        threshold in 0.001f64..=0.05,
    ) {
        let config = SemanticShockConfig {
            p_value_threshold: threshold,
            ..Default::default()
        };
        let r = SemanticShockResponder::new(config);
        // Detection with p_value clearly above threshold
        let det = make_semantic_detection(threshold + 0.1, 0.5);
        let result = r.handle_detection(1, &det);
        prop_assert!(result.is_none());
        prop_assert_eq!(r.metrics_snapshot().detections_filtered, 1);
        prop_assert_eq!(r.metrics_snapshot().shocks_recorded, 0);
    }

    // 12. p-value below threshold is accepted
    #[test]
    fn responder_accepts_low_p_value(
        threshold in 0.01f64..=0.1,
    ) {
        let config = SemanticShockConfig {
            p_value_threshold: threshold,
            notification_cooldown_seconds: 0,
            ..Default::default()
        };
        let r = SemanticShockResponder::new(config);
        // Detection with p_value clearly below threshold
        let p_val = threshold / 10.0;
        let det = make_semantic_detection(p_val, 0.95);
        let result = r.handle_detection(1, &det);
        prop_assert!(result.is_some());
        prop_assert_eq!(r.metrics_snapshot().shocks_recorded, 1);
    }
}

// ── Responder: pause vs alert mode ──────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    // 13. Alert mode: shock recorded but pane NOT paused
    #[test]
    fn responder_alert_mode_no_pause(pane_id in 1..=100u64) {
        let config = SemanticShockConfig {
            action: ShockAction::Alert,
            notification_cooldown_seconds: 0,
            ..Default::default()
        };
        let r = SemanticShockResponder::new(config);
        let det = make_semantic_detection(0.001, 0.95);
        r.handle_detection(pane_id, &det);
        prop_assert!(!r.is_pane_paused(pane_id));
        prop_assert_eq!(r.paused_pane_count(), 0);
        prop_assert_eq!(r.metrics_snapshot().shocks_recorded, 1);
    }

    // 14. Pause mode: shock recorded AND pane paused
    #[test]
    fn responder_pause_mode_pauses(pane_id in 1..=100u64) {
        let config = SemanticShockConfig {
            action: ShockAction::Pause,
            notification_cooldown_seconds: 0,
            ..Default::default()
        };
        let r = SemanticShockResponder::new(config);
        let det = make_semantic_detection(0.001, 0.95);
        r.handle_detection(pane_id, &det);
        prop_assert!(r.is_pane_paused(pane_id));
        prop_assert_eq!(r.paused_pane_count(), 1);
        prop_assert_eq!(r.metrics_snapshot().panes_paused, 1);
    }
}

// ── Responder: eviction ─────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    // 15. Evicts oldest when over max_shocks_per_pane
    #[test]
    fn responder_evicts_oldest(max_per_pane in 1..=5usize) {
        let config = SemanticShockConfig {
            max_shocks_per_pane: max_per_pane,
            notification_cooldown_seconds: 0,
            ..Default::default()
        };
        let r = SemanticShockResponder::new(config);

        let total = max_per_pane + 3;
        for i in 0..total {
            let det = make_semantic_detection(0.001 * (i as f64 + 1.0), 0.9);
            r.handle_detection(1, &det);
        }

        let summary = r.pane_summary(1).unwrap();
        prop_assert_eq!(summary.active_count, max_per_pane);
        // Latest should be the last inserted
        let latest = summary.latest.unwrap();
        prop_assert_eq!(latest.sequence as usize, total - 1);
    }
}

// ── Responder: clear semantics ──────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    // 16. clear_pane unpauses and removes shocks
    #[test]
    fn responder_clear_pane(pane_id in 1..=100u64) {
        let config = SemanticShockConfig {
            action: ShockAction::Pause,
            notification_cooldown_seconds: 0,
            ..Default::default()
        };
        let r = SemanticShockResponder::new(config);
        let det = make_semantic_detection(0.001, 0.95);
        r.handle_detection(pane_id, &det);
        prop_assert!(r.is_pane_paused(pane_id));

        let cleared = r.clear_pane(pane_id);
        prop_assert!(cleared);
        prop_assert!(!r.is_pane_paused(pane_id));
        prop_assert_eq!(r.metrics_snapshot().panes_cleared, 1);
    }

    // 17. clear_all unpauses all panes
    #[test]
    fn responder_clear_all(num_panes in 1..=5u64) {
        let config = SemanticShockConfig {
            action: ShockAction::Pause,
            notification_cooldown_seconds: 0,
            ..Default::default()
        };
        let r = SemanticShockResponder::new(config);
        let det = make_semantic_detection(0.001, 0.95);

        for pid in 1..=num_panes {
            r.handle_detection(pid, &det);
        }
        prop_assert_eq!(r.paused_pane_count(), num_panes as usize);

        let cleared = r.clear_all();
        prop_assert_eq!(cleared as u64, num_panes);
        prop_assert_eq!(r.paused_pane_count(), 0);
    }

    // 18. clear nonexistent pane returns false
    #[test]
    fn responder_clear_nonexistent(pane_id in 500..=1000u64) {
        let r = SemanticShockResponder::new(SemanticShockConfig::default());
        prop_assert!(!r.clear_pane(pane_id));
    }
}

// ── Responder: pane isolation ───────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    // 19. Pausing one pane doesn't affect others
    #[test]
    fn responder_pane_isolation(
        pane_a in 1..=50u64,
        pane_b in 51..=100u64,
    ) {
        let config = SemanticShockConfig {
            action: ShockAction::Pause,
            notification_cooldown_seconds: 0,
            ..Default::default()
        };
        let r = SemanticShockResponder::new(config);
        let det = make_semantic_detection(0.001, 0.95);

        r.handle_detection(pane_a, &det);
        prop_assert!(r.is_pane_paused(pane_a));
        prop_assert!(!r.is_pane_paused(pane_b));

        r.clear_pane(pane_a);
        prop_assert!(!r.is_pane_paused(pane_a));
    }
}

// ── Responder: TraumaDecision ───────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    // 20. trauma_decision reflects pause state
    #[test]
    fn responder_trauma_decision_pause(pane_id in 1..=100u64) {
        let config = SemanticShockConfig {
            action: ShockAction::Pause,
            notification_cooldown_seconds: 0,
            ..Default::default()
        };
        let r = SemanticShockResponder::new(config);
        let det = make_semantic_detection(0.001, 0.95);

        // Before detection: no intervention
        let decision = r.trauma_decision_for_pane(pane_id);
        prop_assert!(!decision.should_intervene);
        prop_assert!(decision.reason_code.is_none());

        // After detection: should intervene
        r.handle_detection(pane_id, &det);
        let decision = r.trauma_decision_for_pane(pane_id);
        prop_assert!(decision.should_intervene);
        prop_assert_eq!(
            decision.reason_code.as_deref(),
            Some("semantic_anomaly_pause")
        );

        // After clear: no intervention
        r.clear_pane(pane_id);
        let decision = r.trauma_decision_for_pane(pane_id);
        prop_assert!(!decision.should_intervene);
    }

    // 21. trauma_decision for alert-mode pane: no intervention
    #[test]
    fn responder_trauma_decision_alert(pane_id in 1..=100u64) {
        let config = SemanticShockConfig {
            action: ShockAction::Alert,
            notification_cooldown_seconds: 0,
            ..Default::default()
        };
        let r = SemanticShockResponder::new(config);
        let det = make_semantic_detection(0.001, 0.95);
        r.handle_detection(pane_id, &det);

        let decision = r.trauma_decision_for_pane(pane_id);
        prop_assert!(!decision.should_intervene);
    }
}

// ── Responder: metrics consistency ──────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    // 22. shocks_recorded counts correctly across panes
    #[test]
    fn responder_shocks_counted(
        num_panes in 1..=5u64,
        shocks_per in 1..=3usize,
    ) {
        let config = SemanticShockConfig {
            notification_cooldown_seconds: 0,
            ..Default::default()
        };
        let r = SemanticShockResponder::new(config);

        for pid in 1..=num_panes {
            for i in 0..shocks_per {
                let det = make_semantic_detection(0.001 * (i as f64 + 1.0), 0.9);
                r.handle_detection(pid, &det);
            }
        }

        let snap = r.metrics_snapshot();
        prop_assert_eq!(
            snap.shocks_recorded,
            (num_panes as usize * shocks_per) as u64
        );
        prop_assert_eq!(r.tracked_pane_count(), num_panes as usize);
    }

    // 23. repeated pause on same pane only increments panes_paused once
    #[test]
    fn responder_repeated_pause_same_pane(count in 2..=5usize) {
        let config = SemanticShockConfig {
            action: ShockAction::Pause,
            notification_cooldown_seconds: 0,
            ..Default::default()
        };
        let r = SemanticShockResponder::new(config);
        let det = make_semantic_detection(0.001, 0.95);

        for _ in 0..count {
            r.handle_detection(1, &det);
        }

        prop_assert_eq!(r.metrics_snapshot().panes_paused, 1);
        prop_assert_eq!(r.metrics_snapshot().shocks_recorded, count as u64);
    }
}

// ── Responder: non-semantic detection ignored ───────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    // 24. Non-semantic_anomaly event_type is ignored
    #[test]
    fn responder_ignores_non_semantic(pane_id in 1..=100u64) {
        let r = SemanticShockResponder::new(SemanticShockConfig::default());
        let det = Detection {
            rule_id: "core.codex:error_loop".to_string(),
            agent_type: AgentType::Codex,
            event_type: "error_loop".to_string(),
            severity: Severity::Warning,
            confidence: 0.85,
            extracted: serde_json::json!({}),
            matched_text: "error loop detected".to_string(),
            span: (0, 0),
        };
        let result = r.handle_detection(pane_id, &det);
        prop_assert!(result.is_none());
        prop_assert_eq!(r.metrics_snapshot().detections_received, 1);
        prop_assert_eq!(r.metrics_snapshot().shocks_recorded, 0);
    }
}

// ── Responder: summary queries ──────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    // 25. pane_summary for non-existent pane is None
    #[test]
    fn responder_summary_nonexistent(pane_id in 500..=1000u64) {
        let r = SemanticShockResponder::new(SemanticShockConfig::default());
        prop_assert!(r.pane_summary(pane_id).is_none());
    }

    // 26. all_summaries matches tracked count
    #[test]
    fn responder_all_summaries_count(num_panes in 1..=5u64) {
        let config = SemanticShockConfig {
            notification_cooldown_seconds: 0,
            ..Default::default()
        };
        let r = SemanticShockResponder::new(config);
        let det = make_semantic_detection(0.001, 0.95);

        for pid in 1..=num_panes {
            r.handle_detection(pid, &det);
        }

        let summaries = r.all_summaries();
        prop_assert_eq!(summaries.len(), num_panes as usize);
    }

    // 27. GC with auto_clear disabled is no-op
    #[test]
    fn responder_gc_disabled(_seed in 0..10u32) {
        let config = SemanticShockConfig {
            auto_clear_seconds: 0,
            ..Default::default()
        };
        let r = SemanticShockResponder::new(config);
        let det = make_semantic_detection(0.001, 0.95);
        r.handle_detection(1, &det);

        let cleared = r.gc_expired_shocks();
        prop_assert_eq!(cleared, 0);
    }

    // 28. Fresh responder has zero metrics
    #[test]
    fn responder_fresh_metrics(_seed in 0..10u32) {
        let r = SemanticShockResponder::new(SemanticShockConfig::default());
        let snap = r.metrics_snapshot();
        prop_assert_eq!(snap.detections_received, 0);
        prop_assert_eq!(snap.detections_filtered, 0);
        prop_assert_eq!(snap.shocks_recorded, 0);
        prop_assert_eq!(snap.panes_paused, 0);
        prop_assert_eq!(snap.panes_cleared, 0);
        prop_assert_eq!(snap.notifications_sent, 0);
        prop_assert_eq!(snap.notifications_suppressed, 0);
        prop_assert_eq!(snap.auto_cleared, 0);
        prop_assert_eq!(r.tracked_pane_count(), 0);
        prop_assert_eq!(r.paused_pane_count(), 0);
    }
}
