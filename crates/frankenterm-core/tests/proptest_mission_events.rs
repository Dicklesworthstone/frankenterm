#![cfg(feature = "subprocess-bridge")]
#![allow(clippy::manual_range_contains)]

//! Property-based tests for the mission event taxonomy, event log, and
//! cycle event emitter.

use proptest::prelude::*;

use frankenterm_core::mission_events::{
    CycleEventEmitter, MissionEventBuilder, MissionEventKind, MissionEventLog,
    MissionEventLogConfig, MissionPhase,
};

// ── Strategies ────────────────────────────────────────────────────────────────

fn arb_event_kind() -> impl Strategy<Value = MissionEventKind> {
    prop_oneof![
        Just(MissionEventKind::ReadinessResolved),
        Just(MissionEventKind::FeaturesExtracted),
        Just(MissionEventKind::ScoringCompleted),
        Just(MissionEventKind::AssignmentsSolved),
        Just(MissionEventKind::SafetyEnvelopeApplied),
        Just(MissionEventKind::SafetyGateRejection),
        Just(MissionEventKind::RetryStormThrottled),
        Just(MissionEventKind::AssignmentEmitted),
        Just(MissionEventKind::AssignmentRejected),
        Just(MissionEventKind::ConflictDetected),
        Just(MissionEventKind::ConflictAutoResolved),
        Just(MissionEventKind::ConflictPendingManual),
        Just(MissionEventKind::UnblockTransitionDetected),
        Just(MissionEventKind::PlannerChurnDetected),
        Just(MissionEventKind::CycleStarted),
        Just(MissionEventKind::CycleCompleted),
        Just(MissionEventKind::TriggerEnqueued),
        Just(MissionEventKind::MetricsSampleRecorded),
    ]
}

fn arb_phase() -> impl Strategy<Value = MissionPhase> {
    prop_oneof![
        Just(MissionPhase::Plan),
        Just(MissionPhase::Safety),
        Just(MissionPhase::Dispatch),
        Just(MissionPhase::Reconcile),
        Just(MissionPhase::Lifecycle),
    ]
}

fn arb_log_config() -> impl Strategy<Value = MissionEventLogConfig> {
    (1usize..100, any::<bool>()).prop_map(|(max, enabled)| MissionEventLogConfig {
        max_events: max,
        enabled,
    })
}

// ── Tests: Event kind → phase mapping ────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// Every event kind maps to exactly one of the five phases.
    #[test]
    fn prop_kind_has_valid_phase(kind in arb_event_kind()) {
        let phase = kind.phase();
        let valid = matches!(
            phase,
            MissionPhase::Plan
                | MissionPhase::Safety
                | MissionPhase::Dispatch
                | MissionPhase::Reconcile
                | MissionPhase::Lifecycle
        );
        prop_assert!(valid, "unexpected phase {:?} for kind {:?}", phase, kind);
    }

    /// Plan-phase events map to MissionPhase::Plan.
    #[test]
    fn prop_plan_events_map_to_plan(
        kind in prop_oneof![
            Just(MissionEventKind::ReadinessResolved),
            Just(MissionEventKind::FeaturesExtracted),
            Just(MissionEventKind::ScoringCompleted),
            Just(MissionEventKind::AssignmentsSolved),
        ]
    ) {
        prop_assert_eq!(kind.phase(), MissionPhase::Plan);
    }

    /// Safety-phase events map to MissionPhase::Safety.
    #[test]
    fn prop_safety_events_map_to_safety(
        kind in prop_oneof![
            Just(MissionEventKind::SafetyEnvelopeApplied),
            Just(MissionEventKind::SafetyGateRejection),
            Just(MissionEventKind::RetryStormThrottled),
        ]
    ) {
        prop_assert_eq!(kind.phase(), MissionPhase::Safety);
    }

    /// Dispatch-phase events map to MissionPhase::Dispatch.
    #[test]
    fn prop_dispatch_events_map_to_dispatch(
        kind in prop_oneof![
            Just(MissionEventKind::AssignmentEmitted),
            Just(MissionEventKind::AssignmentRejected),
        ]
    ) {
        prop_assert_eq!(kind.phase(), MissionPhase::Dispatch);
    }
}

// ── Tests: Event log capacity and eviction ───────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Log never exceeds max_events capacity.
    #[test]
    fn prop_log_bounded_capacity(
        max_events in 1usize..50,
        emit_count in 0usize..100,
        kinds in proptest::collection::vec(arb_event_kind(), 0..100),
    ) {
        let config = MissionEventLogConfig {
            max_events,
            enabled: true,
        };
        let mut log = MissionEventLog::new(config);

        for (i, kind) in kinds.iter().take(emit_count).enumerate() {
            let builder = MissionEventBuilder::new(kind.clone(), "test.reason")
                .cycle(0, i as i64);
            log.emit(builder);
        }

        prop_assert!(log.len() <= max_events,
            "log length {} > max_events {}", log.len(), max_events);
    }

    /// Evicted count + retained count = total appended.
    #[test]
    fn prop_eviction_accounting(
        max_events in 1usize..20,
        emit_count in 0usize..50,
    ) {
        let config = MissionEventLogConfig {
            max_events,
            enabled: true,
        };
        let mut log = MissionEventLog::new(config);

        for i in 0..emit_count {
            let builder = MissionEventBuilder::new(
                MissionEventKind::CycleStarted,
                "test",
            ).cycle(i as u64, i as i64);
            log.emit(builder);
        }

        prop_assert_eq!(
            log.total_evicted() + log.len() as u64,
            log.total_appended(),
            "evicted({}) + retained({}) != total_appended({})",
            log.total_evicted(), log.len(), log.total_appended()
        );
    }

    /// Sequences are strictly monotonically increasing.
    #[test]
    fn prop_sequence_monotonic(
        emit_count in 2usize..30,
        kinds in proptest::collection::vec(arb_event_kind(), 2..30),
    ) {
        let config = MissionEventLogConfig {
            max_events: 100,
            enabled: true,
        };
        let mut log = MissionEventLog::new(config);

        let count = emit_count.min(kinds.len());
        for (i, kind) in kinds.iter().take(count).enumerate() {
            let builder = MissionEventBuilder::new(kind.clone(), "test")
                .cycle(0, i as i64);
            log.emit(builder);
        }

        let events = log.events();
        for pair in events.windows(2) {
            prop_assert!(pair[0].sequence < pair[1].sequence,
                "non-monotonic sequence: {} >= {}", pair[0].sequence, pair[1].sequence);
        }
    }

    /// First sequence number is 1 (1-based).
    #[test]
    fn prop_first_sequence_is_one(_dummy in 0u8..1) {
        let config = MissionEventLogConfig {
            max_events: 10,
            enabled: true,
        };
        let mut log = MissionEventLog::new(config);
        let builder = MissionEventBuilder::new(MissionEventKind::CycleStarted, "test");
        let seq = log.emit(builder);
        prop_assert_eq!(seq, Some(1));
    }

    /// FIFO eviction: after overflow, oldest events are removed first.
    #[test]
    fn prop_fifo_eviction_order(
        max_events in 2usize..10,
        overflow in 1usize..20,
    ) {
        let config = MissionEventLogConfig {
            max_events,
            enabled: true,
        };
        let mut log = MissionEventLog::new(config);

        let total = max_events + overflow;
        for i in 0..total {
            let builder = MissionEventBuilder::new(MissionEventKind::CycleStarted, "test")
                .cycle(i as u64, i as i64);
            log.emit(builder);
        }

        // Oldest remaining should have cycle_id = overflow (0-indexed)
        let events = log.events();
        prop_assert_eq!(events[0].cycle_id, overflow as u64,
            "oldest event cycle_id {} != expected {}", events[0].cycle_id, overflow);
    }
}

// ── Tests: Disabled log ──────────────────────────────────────────────────────

proptest! {
    /// Disabled log returns None and stores nothing.
    #[test]
    fn prop_disabled_log_returns_none(
        kinds in proptest::collection::vec(arb_event_kind(), 1..10),
    ) {
        let config = MissionEventLogConfig {
            max_events: 100,
            enabled: false,
        };
        let mut log = MissionEventLog::new(config);

        for kind in &kinds {
            let builder = MissionEventBuilder::new(kind.clone(), "test");
            let seq = log.emit(builder);
            prop_assert_eq!(seq, None, "disabled log should return None");
        }
        prop_assert!(log.is_empty(), "disabled log should be empty");
        prop_assert_eq!(log.total_appended(), 0);
    }
}

// ── Tests: Filtering ─────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// events_by_phase returns only events matching the requested phase.
    #[test]
    fn prop_filter_by_phase_correct(
        kinds in proptest::collection::vec(arb_event_kind(), 5..20),
        phase in arb_phase(),
    ) {
        let config = MissionEventLogConfig {
            max_events: 100,
            enabled: true,
        };
        let mut log = MissionEventLog::new(config);

        for (i, kind) in kinds.iter().enumerate() {
            let builder = MissionEventBuilder::new(kind.clone(), "test")
                .cycle(0, i as i64);
            log.emit(builder);
        }

        let filtered = log.events_by_phase(phase);
        for event in &filtered {
            prop_assert_eq!(event.phase, phase,
                "filtered event has phase {:?}, expected {:?}", event.phase, phase);
        }
    }

    /// events_by_kind returns only events matching the requested kind.
    #[test]
    fn prop_filter_by_kind_correct(
        kinds in proptest::collection::vec(arb_event_kind(), 5..20),
        target_kind in arb_event_kind(),
    ) {
        let config = MissionEventLogConfig {
            max_events: 100,
            enabled: true,
        };
        let mut log = MissionEventLog::new(config);

        for (i, kind) in kinds.iter().enumerate() {
            let builder = MissionEventBuilder::new(kind.clone(), "test")
                .cycle(0, i as i64);
            log.emit(builder);
        }

        let filtered = log.events_by_kind(&target_kind);
        for event in &filtered {
            prop_assert_eq!(&event.kind, &target_kind);
        }
    }

    /// events_by_cycle returns only events from the specified cycle.
    #[test]
    fn prop_filter_by_cycle_correct(
        kinds in proptest::collection::vec(arb_event_kind(), 5..20),
        target_cycle in 0u64..5,
    ) {
        let config = MissionEventLogConfig {
            max_events: 100,
            enabled: true,
        };
        let mut log = MissionEventLog::new(config);

        for (i, kind) in kinds.iter().enumerate() {
            let cycle_id = i as u64 % 5;
            let builder = MissionEventBuilder::new(kind.clone(), "test")
                .cycle(cycle_id, i as i64);
            log.emit(builder);
        }

        let filtered = log.events_by_cycle(target_cycle);
        for event in &filtered {
            prop_assert_eq!(event.cycle_id, target_cycle);
        }
    }
}

// ── Tests: Summary consistency ───────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Summary retained_count matches len().
    #[test]
    fn prop_summary_retained_matches_len(
        kinds in proptest::collection::vec(arb_event_kind(), 0..30),
    ) {
        let config = MissionEventLogConfig {
            max_events: 50,
            enabled: true,
        };
        let mut log = MissionEventLog::new(config);

        for (i, kind) in kinds.iter().enumerate() {
            let builder = MissionEventBuilder::new(kind.clone(), "test")
                .cycle(0, i as i64);
            log.emit(builder);
        }

        let summary = log.summary();
        prop_assert_eq!(summary.retained_count, log.len());
        prop_assert_eq!(summary.total_appended, log.total_appended());
        prop_assert_eq!(summary.total_evicted, log.total_evicted());
    }

    /// Summary by_phase counts sum to retained_count.
    #[test]
    fn prop_summary_phase_counts_sum(
        kinds in proptest::collection::vec(arb_event_kind(), 1..30),
    ) {
        let config = MissionEventLogConfig {
            max_events: 50,
            enabled: true,
        };
        let mut log = MissionEventLog::new(config);

        for (i, kind) in kinds.iter().enumerate() {
            let builder = MissionEventBuilder::new(kind.clone(), "test")
                .cycle(0, i as i64);
            log.emit(builder);
        }

        let summary = log.summary();
        let phase_total: usize = summary.by_phase.values().sum();
        prop_assert_eq!(phase_total, summary.retained_count,
            "phase counts sum {} != retained {}", phase_total, summary.retained_count);
    }

    /// Summary by_kind counts sum to retained_count.
    #[test]
    fn prop_summary_kind_counts_sum(
        kinds in proptest::collection::vec(arb_event_kind(), 1..30),
    ) {
        let config = MissionEventLogConfig {
            max_events: 50,
            enabled: true,
        };
        let mut log = MissionEventLog::new(config);

        for (i, kind) in kinds.iter().enumerate() {
            let builder = MissionEventBuilder::new(kind.clone(), "test")
                .cycle(0, i as i64);
            log.emit(builder);
        }

        let summary = log.summary();
        let kind_total: usize = summary.by_kind.values().sum();
        prop_assert_eq!(kind_total, summary.retained_count,
            "kind counts sum {} != retained {}", kind_total, summary.retained_count);
    }
}

// ── Tests: drain_matching ────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// drain_matching removes exactly the matching events.
    #[test]
    fn prop_drain_matching_removes_correct_events(
        kinds in proptest::collection::vec(arb_event_kind(), 5..20),
        target_phase in arb_phase(),
    ) {
        let config = MissionEventLogConfig {
            max_events: 100,
            enabled: true,
        };
        let mut log = MissionEventLog::new(config);

        for (i, kind) in kinds.iter().enumerate() {
            let builder = MissionEventBuilder::new(kind.clone(), "test")
                .cycle(0, i as i64);
            log.emit(builder);
        }

        let original_len = log.len();
        let expected_drained = log.events_by_phase(target_phase).len();

        let drained = log.drain_matching(|e| e.phase == target_phase);

        prop_assert_eq!(drained.len(), expected_drained,
            "drained {} != expected {}", drained.len(), expected_drained);
        prop_assert_eq!(log.len(), original_len - expected_drained,
            "remaining {} != expected {}", log.len(), original_len - expected_drained);

        // Remaining should have no events of target phase
        let remaining_of_phase = log.events_by_phase(target_phase);
        prop_assert!(remaining_of_phase.is_empty(),
            "drained phase still has {} events", remaining_of_phase.len());
    }
}

// ── Tests: Builder details ───────────────────────────────────────────────────

proptest! {
    /// Builder produces event with correct phase derived from kind.
    #[test]
    fn prop_builder_phase_from_kind(kind in arb_event_kind()) {
        let mut log = MissionEventLog::new(MissionEventLogConfig {
            max_events: 10,
            enabled: true,
        });
        let expected_phase = kind.phase();
        let builder = MissionEventBuilder::new(kind, "test.reason")
            .cycle(1, 100)
            .correlation("corr-1")
            .labels("ws", "track");
        log.emit(builder);

        let event = log.latest().unwrap();
        prop_assert_eq!(event.phase, expected_phase);
        prop_assert_eq!(event.cycle_id, 1);
        prop_assert_eq!(event.timestamp_ms, 100);
        prop_assert_eq!(&event.correlation_id, "corr-1");
        prop_assert_eq!(&event.workspace, "ws");
        prop_assert_eq!(&event.track, "track");
    }

    /// Builder detail methods add correct entries.
    #[test]
    fn prop_builder_details(
        str_val in "[a-z]{3,8}",
        u64_val in 0u64..10000,
        bool_val in any::<bool>(),
    ) {
        let mut log = MissionEventLog::new(MissionEventLogConfig {
            max_events: 10,
            enabled: true,
        });
        let builder = MissionEventBuilder::new(MissionEventKind::CycleStarted, "test")
            .detail_str("name", &str_val)
            .detail_u64("count", u64_val)
            .detail_bool("flag", bool_val);
        log.emit(builder);

        let event = log.latest().unwrap();
        prop_assert_eq!(
            event.details.get("name"),
            Some(&serde_json::Value::String(str_val))
        );
        prop_assert_eq!(
            event.details.get("count"),
            Some(&serde_json::Value::Number(u64_val.into()))
        );
        prop_assert_eq!(
            event.details.get("flag"),
            Some(&serde_json::Value::Bool(bool_val))
        );
    }
}

// ── Tests: Serde roundtrip ───────────────────────────────────────────────────

proptest! {
    /// MissionEventKind serde roundtrip.
    #[test]
    fn prop_event_kind_serde(kind in arb_event_kind()) {
        let json = serde_json::to_string(&kind).unwrap();
        let back: MissionEventKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(kind, back);
    }

    /// MissionPhase serde roundtrip.
    #[test]
    fn prop_phase_serde(phase in arb_phase()) {
        let json = serde_json::to_string(&phase).unwrap();
        let back: MissionPhase = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(phase, back);
    }

    /// MissionEventLogConfig serde roundtrip.
    #[test]
    fn prop_log_config_serde(config in arb_log_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: MissionEventLogConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config.max_events, back.max_events);
        prop_assert_eq!(config.enabled, back.enabled);
    }

    /// Full event log serde roundtrip preserves structure.
    #[test]
    fn prop_event_log_serde_roundtrip(
        kinds in proptest::collection::vec(arb_event_kind(), 1..15),
    ) {
        let config = MissionEventLogConfig {
            max_events: 50,
            enabled: true,
        };
        let mut log = MissionEventLog::new(config);
        for (i, kind) in kinds.iter().enumerate() {
            let builder = MissionEventBuilder::new(kind.clone(), "test")
                .cycle(i as u64, i as i64);
            log.emit(builder);
        }

        let json = serde_json::to_string(&log).unwrap();
        let back: MissionEventLog = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(log.len(), back.len());
        prop_assert_eq!(log.total_appended(), back.total_appended());
        prop_assert_eq!(log.total_evicted(), back.total_evicted());
    }
}

// ── Tests: CycleEventEmitter ─────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// CycleEventEmitter binds cycle context to all emitted events.
    #[test]
    fn prop_cycle_emitter_binds_context(
        cycle_id in 0u64..100,
        timestamp_ms in 0i64..100000,
        kinds in proptest::collection::vec(arb_event_kind(), 1..10),
    ) {
        let mut log = MissionEventLog::new(MissionEventLogConfig {
            max_events: 100,
            enabled: true,
        });

        {
            let mut emitter = CycleEventEmitter::new(
                &mut log, cycle_id, timestamp_ms, "corr-xyz", "ws-test", "track-test",
            );
            for kind in &kinds {
                emitter.emit(kind.clone(), "test.reason");
            }
        }

        for event in log.events() {
            prop_assert_eq!(event.cycle_id, cycle_id,
                "cycle_id mismatch: {} != {}", event.cycle_id, cycle_id);
            prop_assert_eq!(event.timestamp_ms, timestamp_ms);
            prop_assert_eq!(&event.correlation_id, "corr-xyz");
            prop_assert_eq!(&event.workspace, "ws-test");
            prop_assert_eq!(&event.track, "track-test");
        }
    }
}

// ── Tests: clear ─────────────────────────────────────────────────────────────

proptest! {
    /// clear() empties the log but preserves counters.
    #[test]
    fn prop_clear_empties_preserves_counters(
        kinds in proptest::collection::vec(arb_event_kind(), 1..20),
    ) {
        let mut log = MissionEventLog::new(MissionEventLogConfig {
            max_events: 50,
            enabled: true,
        });
        for (i, kind) in kinds.iter().enumerate() {
            let builder = MissionEventBuilder::new(kind.clone(), "test")
                .cycle(0, i as i64);
            log.emit(builder);
        }

        let appended_before = log.total_appended();
        log.clear();

        prop_assert!(log.is_empty());
        prop_assert_eq!(log.len(), 0);
        prop_assert_eq!(log.total_appended(), appended_before,
            "clear should preserve total_appended");
    }
}
