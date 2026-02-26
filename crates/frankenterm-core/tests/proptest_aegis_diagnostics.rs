//! Property-based tests for Aegis Diagnostics Integration (ft-l5em3.5).
//!
//! Verifies invariants of the unified AegisEngine, overlay rendering,
//! dump export, and structured logging.

use frankenterm_core::aegis_backpressure::QueueObservation;
use frankenterm_core::aegis_diagnostics::{
    AegisConfig, AegisDump, AegisEngine, AegisLogEvent, AegisLogEventType, EvidenceLine,
    InterventionEvent, InterventionKind, OverlayCard, OverlaySeverity,
};
use frankenterm_core::aegis_entropy_anomaly::PaneAnomalySnapshot;
use proptest::prelude::*;

// ── Strategies ─────────────────────────────────────────────────────────

fn arb_config() -> impl Strategy<Value = AegisConfig> {
    (any::<bool>(), any::<bool>(), 40..=120_usize).prop_map(|(structured, overlay, max_width)| {
        AegisConfig {
            structured_logging: structured,
            overlay_enabled: overlay,
            overlay_max_width: max_width,
            ..Default::default()
        }
    })
}

fn arb_severity() -> impl Strategy<Value = OverlaySeverity> {
    prop_oneof![
        Just(OverlaySeverity::Info),
        Just(OverlaySeverity::Warning),
        Just(OverlaySeverity::Critical),
    ]
}

fn arb_intervention_kind() -> impl Strategy<Value = InterventionKind> {
    prop_oneof![
        Just(InterventionKind::BackpressureThrottle),
        Just(InterventionKind::EntropyAnomalyBlock),
        Just(InterventionKind::CombinedIntervention),
    ]
}

fn arb_byte_chunk(max_len: usize) -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 1..=max_len)
}

fn arb_fill_ratio() -> impl Strategy<Value = f64> {
    0.0..=1.0_f64
}

// ── Config Properties ─────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // CFG-1: Config serde roundtrip
    #[test]
    fn config_serde_roundtrip(config in arb_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: AegisConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config.structured_logging, back.structured_logging);
        prop_assert_eq!(config.overlay_enabled, back.overlay_enabled);
        prop_assert_eq!(config.overlay_max_width, back.overlay_max_width);
    }
}

// ── Overlay Properties ────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // OVL-1: Rendered overlay contains box drawing chars
    #[test]
    fn overlay_has_borders(
        severity in arb_severity(),
        width in 40..=120_usize,
    ) {
        let card = OverlayCard {
            title: "Test".into(),
            summary: "Summary".into(),
            evidence: vec![],
            action: "Action".into(),
            severity,
        };
        let rendered = card.render(width);
        prop_assert!(rendered.contains('┌'));
        prop_assert!(rendered.contains('└'));
        prop_assert!(rendered.contains('│'));
    }

    // OVL-2: Overlay severity tag present
    #[test]
    fn overlay_severity_tag(severity in arb_severity()) {
        let card = OverlayCard {
            title: "T".into(),
            summary: "S".into(),
            evidence: vec![],
            action: String::new(),
            severity,
        };
        let rendered = card.render(80);
        let expected_tag = match severity {
            OverlaySeverity::Info => "[INFO]",
            OverlaySeverity::Warning => "[WARN]",
            OverlaySeverity::Critical => "[CRIT]",
        };
        prop_assert!(
            rendered.contains(expected_tag),
            "Rendered should contain {}, got:\n{}",
            expected_tag,
            rendered
        );
    }

    // OVL-3: Evidence lines appear in render
    #[test]
    fn overlay_evidence_rendered(
        label in "[a-zA-Z]{3,10}",
        value in "[0-9.]{1,8}",
    ) {
        let card = OverlayCard {
            title: "T".into(),
            summary: "S".into(),
            evidence: vec![EvidenceLine {
                label: label.clone(),
                value: value.clone(),
                intuition: "meaning".into(),
            }],
            action: String::new(),
            severity: OverlaySeverity::Info,
        };
        let rendered = card.render(80);
        prop_assert!(
            rendered.contains(&label),
            "Rendered should contain label '{}'",
            label
        );
    }
}

// ── Engine Properties ──────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // ENG-1: Backpressure severity always bounded [0, 1]
    #[test]
    fn backpressure_severity_bounded(
        fill in arb_fill_ratio(),
        frame_dropped in any::<bool>(),
    ) {
        let mut engine = AegisEngine::with_defaults();
        let obs = QueueObservation {
            pane_id: 1,
            fill_ratio: fill,
            frame_dropped,
            external_cause: None,
        };
        let actions = engine.observe_backpressure(&obs);
        prop_assert!(
            actions.severity >= 0.0 && actions.severity <= 1.0,
            "Severity {} out of bounds",
            actions.severity
        );
    }

    // ENG-2: Entropy observation never blocks diverse data
    #[test]
    fn entropy_diverse_never_blocks(data in arb_byte_chunk(256)) {
        let mut engine = AegisEngine::with_defaults();
        // Feed max-entropy data
        let diverse: Vec<u8> = (0..4096).map(|i| (i % 256) as u8).collect();
        for _ in 0..20 {
            let decision = engine.observe_entropy(1, &diverse, &[]);
            prop_assert!(!decision.should_block);
        }
        drop(data); // unused, just to satisfy proptest
    }

    // ENG-3: Dump produces valid JSON
    #[test]
    fn dump_is_valid_json(
        config in arb_config(),
        n_obs in 1..10_usize,
    ) {
        let mut engine = AegisEngine::new(config);
        let data: Vec<u8> = (0..128).map(|i| i as u8).collect();
        for i in 0..n_obs {
            engine.observe_entropy(i as u64, &data, &[]);
        }
        let json = engine.dump_json();
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(&json);
        prop_assert!(parsed.is_ok(), "Dump JSON should be valid");
    }

    // ENG-4: Dump pane count matches observed panes
    #[test]
    fn dump_pane_count_matches(
        pane_ids in prop::collection::vec(1..50_u64, 1..10),
    ) {
        let mut engine = AegisEngine::with_defaults();
        let data: Vec<u8> = (0..128).map(|i| i as u8).collect();
        let mut unique = std::collections::HashSet::new();
        for &pid in &pane_ids {
            engine.observe_entropy(pid, &data, &[]);
            unique.insert(pid);
        }
        let dump = engine.dump();
        prop_assert_eq!(dump.entropy_anomaly_panes.len(), unique.len());
    }

    // ENG-5: Reset clears everything
    #[test]
    fn engine_reset_clears(n_obs in 1..10_usize) {
        let mut engine = AegisEngine::with_defaults();
        let data: Vec<u8> = (0..128).map(|i| i as u8).collect();
        for i in 0..n_obs {
            engine.observe_entropy(i as u64, &data, &[]);
        }
        engine.reset();
        prop_assert!(engine.recent_interventions().is_empty());
        prop_assert!(engine.drain_logs().is_empty());
    }

    // ENG-6: Dump serde roundtrip
    #[test]
    fn dump_serde_roundtrip(n_obs in 1..5_usize) {
        let mut engine = AegisEngine::with_defaults();
        let data: Vec<u8> = (0..128).map(|i| i as u8).collect();
        for i in 0..n_obs {
            engine.observe_entropy(i as u64, &data, &[]);
        }
        let dump = engine.dump();
        let json = serde_json::to_string(&dump).unwrap();
        let back: AegisDump = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(dump.schema_version, back.schema_version);
        prop_assert_eq!(
            dump.entropy_anomaly_panes.len(),
            back.entropy_anomaly_panes.len()
        );
    }
}

// ── Serde Properties ──────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // SER-1: InterventionEvent serde roundtrip
    #[test]
    fn intervention_serde(
        pane_id in 1..100_u64,
        kind in arb_intervention_kind(),
    ) {
        let event = InterventionEvent {
            timestamp: "2026-01-01T00:00:00Z".into(),
            pane_id,
            kind,
            evidence: "test evidence".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: InterventionEvent = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(event.pane_id, back.pane_id);
        prop_assert_eq!(event.kind, back.kind);
    }

    // SER-2: AegisLogEvent serde roundtrip
    #[test]
    fn log_event_serde(pane_id in 1..100_u64) {
        let event = AegisLogEvent {
            timestamp: "2026-01-01T00:00:00Z".into(),
            component: "aegis".into(),
            event_type: AegisLogEventType::InterventionTriggered,
            pane_id: Some(pane_id),
            data: std::collections::HashMap::new(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: AegisLogEvent = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(event.pane_id, back.pane_id);
        prop_assert_eq!(event.event_type, back.event_type);
    }

    // SER-3: PaneAnomalySnapshot serde roundtrip
    #[test]
    fn pane_snapshot_serde(
        pane_id in 1..100_u64,
        e_value in 0.001..=1000.0_f64,
        entropy in 0.0..=8.0_f64,
    ) {
        let snap = PaneAnomalySnapshot {
            pane_id,
            e_value,
            n_observations: 42,
            collapse_streak: 3,
            last_entropy: entropy,
            error_density: 0.5,
            error_hits: 10,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: PaneAnomalySnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap.pane_id, back.pane_id);
        prop_assert!((snap.e_value - back.e_value).abs() < 1e-10);
    }
}
