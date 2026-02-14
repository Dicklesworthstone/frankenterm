//! Property-based tests for recorder replay engine invariants.
//!
//! Bead: wa-eb7u
//!
//! Validates:
//! 1. Frame ordering: events always emitted in timestamp order
//! 2. Completeness: collect_remaining yields all non-filtered events
//! 3. Speed control: delays scale inversely with speed
//! 4. Pane filtering: only requested panes appear in output
//! 5. Kind filtering: only requested kinds appear in output
//! 6. Seek correctness: seek(ts) positions cursor at correct event
//! 7. Reset idempotency: reset then replay yields identical frames
//! 8. Progress monotonicity: progress never decreases during replay
//! 9. Serde roundtrips for ReplayConfig, ReplayStats, ReplayState

use proptest::prelude::*;

use frankenterm_core::policy::ActorKind;
use frankenterm_core::recorder_audit::{AccessTier, ActorIdentity};
use frankenterm_core::recorder_query::QueryEventKind;
use frankenterm_core::recorder_replay::{ReplayConfig, ReplaySession, ReplayState, ReplayStats};
use frankenterm_core::recorder_retention::SensitivityTier;
use frankenterm_core::recording::RecorderEventSource;

// =============================================================================
// Strategies
// =============================================================================

fn arb_event_kind() -> impl Strategy<Value = QueryEventKind> {
    prop_oneof![
        Just(QueryEventKind::IngressText),
        Just(QueryEventKind::EgressOutput),
        Just(QueryEventKind::ControlMarker),
        Just(QueryEventKind::LifecycleMarker),
    ]
}

fn arb_result_event(
    pane_id: u64,
    seq: u64,
    ts_ms: u64,
) -> impl Strategy<Value = frankenterm_core::recorder_query::QueryResultEvent> {
    (proptest::option::of("[a-z0-9 ]{1,40}"), arb_event_kind()).prop_map(move |(text, kind)| {
        frankenterm_core::recorder_query::QueryResultEvent {
            event_id: format!("evt-{}-{}", pane_id, seq),
            pane_id,
            source: RecorderEventSource::WeztermMux,
            occurred_at_ms: ts_ms,
            sequence: seq,
            session_id: None,
            text,
            redacted: false,
            sensitivity: SensitivityTier::T1Standard,
            event_kind: kind,
        }
    })
}

fn arb_events(
    count: usize,
) -> impl Strategy<Value = Vec<frankenterm_core::recorder_query::QueryResultEvent>> {
    let strategies: Vec<_> = (0..count)
        .map(|i| {
            let pane_id = (i % 4) as u64 + 1;
            let seq = i as u64;
            let ts_ms = 1000 + (i as u64) * 100; // 100ms apart
            arb_result_event(pane_id, seq, ts_ms)
        })
        .collect();
    strategies
}

fn arb_replay_config() -> impl Strategy<Value = ReplayConfig> {
    (
        0.5_f64..4.0,     // speed
        1000_u64..60_000, // max_delay_ms
        any::<bool>(),    // skip_empty
        any::<bool>(),    // include_markers
    )
        .prop_map(
            |(speed, max_delay_ms, skip_empty, include_markers)| ReplayConfig {
                speed,
                max_delay_ms,
                skip_empty,
                include_markers,
                pane_filter: Vec::new(),
                kind_filter: Vec::new(),
            },
        )
}

fn arb_replay_state() -> impl Strategy<Value = ReplayState> {
    prop_oneof![
        Just(ReplayState::Ready),
        Just(ReplayState::Playing),
        Just(ReplayState::Paused),
        Just(ReplayState::Completed),
    ]
}

fn human() -> ActorIdentity {
    ActorIdentity::new(ActorKind::Human, "replay-tester")
}

fn make_session(
    events: Vec<frankenterm_core::recorder_query::QueryResultEvent>,
    config: ReplayConfig,
) -> Result<ReplaySession, frankenterm_core::recorder_replay::ReplayError> {
    ReplaySession::new(
        events,
        config,
        human(),
        AccessTier::A2FullQuery,
        "prop-test",
    )
}

// =============================================================================
// Property: Frame ordering — timestamps never decrease
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn frames_emitted_in_timestamp_order(
        events in arb_events(20),
    ) {
        if events.is_empty() {
            return Ok(());
        }

        let mut session = make_session(events, ReplayConfig::instant()).unwrap();
        let frames = session.collect_remaining();

        for window in frames.windows(2) {
            prop_assert!(
                window[0].original_ts_ms <= window[1].original_ts_ms,
                "frame {} (ts={}) should be <= frame {} (ts={})",
                window[0].frame_index, window[0].original_ts_ms,
                window[1].frame_index, window[1].original_ts_ms
            );
        }
    }
}

// =============================================================================
// Property: Completeness — all events emitted without filter
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn all_events_emitted_without_filter(
        events in arb_events(15),
    ) {
        if events.is_empty() {
            return Ok(());
        }

        let n = events.len();
        let mut session = make_session(events, ReplayConfig::instant()).unwrap();
        let frames = session.collect_remaining();

        prop_assert_eq!(frames.len(), n,
            "all {} events should be emitted, got {}", n, frames.len());
        prop_assert_eq!(session.state(), ReplayState::Completed);
        prop_assert!(session.stats().completed);
    }
}

// =============================================================================
// Property: Speed control — delays scale inversely
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn speed_scales_delays(
        speed in 0.5_f64..4.0,
    ) {
        let events = vec![
            frankenterm_core::recorder_query::QueryResultEvent {
                event_id: "evt-1-0".into(),
                pane_id: 1,
                source: RecorderEventSource::WeztermMux,
                occurred_at_ms: 1000,
                sequence: 0,
                session_id: None,
                text: Some("a".into()),
                redacted: false,
                sensitivity: SensitivityTier::T1Standard,
                event_kind: QueryEventKind::IngressText,
            },
            frankenterm_core::recorder_query::QueryResultEvent {
                event_id: "evt-1-1".into(),
                pane_id: 1,
                source: RecorderEventSource::WeztermMux,
                occurred_at_ms: 2000,
                sequence: 1,
                session_id: None,
                text: Some("b".into()),
                redacted: false,
                sensitivity: SensitivityTier::T1Standard,
                event_kind: QueryEventKind::IngressText,
            },
        ];

        let config = ReplayConfig {
            speed,
            max_delay_ms: 100_000, // high enough not to clamp
            ..ReplayConfig::default()
        };

        let mut session = make_session(events, config).unwrap();
        let frames = session.collect_remaining();

        // Second frame delay should be 1000ms / speed.
        let expected_ms = (1000.0 / speed) as u64;
        let actual_ms = frames[1].delay.as_millis() as u64;

        // Allow 1ms tolerance for floating point.
        prop_assert!(
            actual_ms.abs_diff(expected_ms) <= 1,
            "at speed {}, delay should be ~{}ms, got {}ms", speed, expected_ms, actual_ms
        );
    }
}

// =============================================================================
// Property: Pane filtering — only selected panes
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn pane_filter_respected(
        events in arb_events(20),
        filter_pane in 1_u64..5,
    ) {
        if events.is_empty() {
            return Ok(());
        }

        let config = ReplayConfig::instant().with_panes(vec![filter_pane]);
        let mut session = make_session(events.clone(), config).unwrap();
        let frames = session.collect_remaining();

        // All frames should be from the filtered pane.
        for frame in &frames {
            prop_assert_eq!(frame.event.pane_id, filter_pane,
                "frame from pane {} should not appear with filter for pane {}",
                frame.event.pane_id, filter_pane);
        }

        // Skipped count should account for filtered events.
        let expected_from_pane = events.iter().filter(|e| e.pane_id == filter_pane).count();
        prop_assert_eq!(frames.len(), expected_from_pane,
            "expected {} frames for pane {}, got {}",
            expected_from_pane, filter_pane, frames.len());
    }
}

// =============================================================================
// Property: Kind filtering — only selected kinds
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn kind_filter_respected(
        events in arb_events(20),
        filter_kind in arb_event_kind(),
    ) {
        if events.is_empty() {
            return Ok(());
        }

        let config = ReplayConfig::instant().with_kinds(vec![filter_kind]);
        let mut session = make_session(events.clone(), config).unwrap();
        let frames = session.collect_remaining();

        for frame in &frames {
            prop_assert_eq!(frame.event.event_kind, filter_kind,
                "frame kind {:?} should match filter {:?}",
                frame.event.event_kind, filter_kind);
        }

        let expected = events.iter().filter(|e| e.event_kind == filter_kind).count();
        prop_assert_eq!(frames.len(), expected,
            "expected {} frames for kind {:?}, got {}",
            expected, filter_kind, frames.len());
    }
}

// =============================================================================
// Property: Seek correctness
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn seek_positions_correctly(
        events in arb_events(20),
    ) {
        if events.is_empty() {
            return Ok(());
        }

        let mut session = make_session(events.clone(), ReplayConfig::instant()).unwrap();
        let (min_ts, max_ts) = session.time_range();

        if min_ts == max_ts {
            // All events at same timestamp; seek to that ts should work.
            let idx = session.seek(min_ts).unwrap();
            prop_assert_eq!(idx, 0);
            return Ok(());
        }

        // Seek to middle of the time range.
        #[allow(clippy::manual_midpoint)]
        let mid = min_ts + (max_ts - min_ts) / 2;
        let idx = session.seek(mid).unwrap();

        // All events before idx should have ts < mid.
        let sorted_events = {
            let mut e = events.clone();
            e.sort_by_key(|ev| (ev.occurred_at_ms, ev.sequence));
            e
        };

        for (i, event) in sorted_events.iter().enumerate().take(idx) {
            prop_assert!(event.occurred_at_ms < mid,
                "event at idx {} (ts={}) should be < seek target {}",
                i, event.occurred_at_ms, mid);
        }

        // Event at idx should have ts >= mid.
        if idx < sorted_events.len() {
            prop_assert!(sorted_events[idx].occurred_at_ms >= mid,
                "event at seek idx {} (ts={}) should be >= target {}",
                idx, sorted_events[idx].occurred_at_ms, mid);
        }
    }
}

// =============================================================================
// Property: Reset idempotency — same frames after reset
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn reset_produces_identical_replay(
        events in arb_events(15),
    ) {
        if events.is_empty() {
            return Ok(());
        }

        let mut session = make_session(events, ReplayConfig::instant()).unwrap();

        // First pass.
        let frames1 = session.collect_remaining();
        prop_assert_eq!(session.state(), ReplayState::Completed);

        // Reset and replay.
        session.reset();
        prop_assert_eq!(session.state(), ReplayState::Ready);
        prop_assert_eq!(session.cursor(), 0);

        let frames2 = session.collect_remaining();

        // Should be identical.
        prop_assert_eq!(frames1.len(), frames2.len(),
            "reset replay should have same frame count");

        for (a, b) in frames1.iter().zip(frames2.iter()) {
            prop_assert_eq!(&a.event.event_id, &b.event.event_id,
                "frame {} should have same event ID after reset", a.frame_index);
            prop_assert_eq!(a.frame_index, b.frame_index);
            prop_assert_eq!(a.original_ts_ms, b.original_ts_ms);
        }
    }
}

// =============================================================================
// Property: Progress monotonicity
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn progress_monotonically_increases(
        events in arb_events(15),
    ) {
        if events.is_empty() {
            return Ok(());
        }

        let mut session = make_session(events, ReplayConfig::instant()).unwrap();

        let mut prev_progress = 0.0_f64;
        while let Some(frame) = session.next_frame() {
            prop_assert!(frame.progress >= prev_progress,
                "progress should not decrease: {} -> {}", prev_progress, frame.progress);
            prev_progress = frame.progress;
        }

        // Final progress should be 1.0.
        prop_assert!((prev_progress - 1.0).abs() < 0.01,
            "final progress should be ~1.0, got {}", prev_progress);
    }
}

// =============================================================================
// Property: Frame index uniqueness
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn frame_indices_unique_and_sequential(
        events in arb_events(15),
    ) {
        if events.is_empty() {
            return Ok(());
        }

        let mut session = make_session(events, ReplayConfig::instant()).unwrap();
        let frames = session.collect_remaining();

        let indices: Vec<usize> = frames.iter().map(|f| f.frame_index).collect();

        // Indices should be strictly increasing.
        for window in indices.windows(2) {
            prop_assert!(window[0] < window[1],
                "frame indices should be strictly increasing: {} >= {}",
                window[0], window[1]);
        }
    }
}

// =============================================================================
// Property: Stats consistency
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn stats_consistent_after_replay(
        events in arb_events(15),
        filter_pane in 1_u64..5,
    ) {
        if events.is_empty() {
            return Ok(());
        }

        let config = ReplayConfig::instant().with_panes(vec![filter_pane]);
        let mut session = make_session(events.clone(), config).unwrap();
        let frames = session.collect_remaining();

        let stats = session.stats();

        prop_assert_eq!(stats.frames_emitted, frames.len(),
            "stats.frames_emitted should match actual frame count");
        prop_assert_eq!(
            stats.frames_emitted + stats.frames_skipped,
            events.len(),
            "emitted + skipped should equal total events: {} + {} != {}",
            stats.frames_emitted, stats.frames_skipped, events.len()
        );
    }
}

// =============================================================================
// Property: Instant replay has zero total delay
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn instant_replay_zero_total_delay(
        events in arb_events(15),
    ) {
        if events.is_empty() {
            return Ok(());
        }

        let mut session = make_session(events, ReplayConfig::instant()).unwrap();
        let frames = session.collect_remaining();

        for frame in &frames {
            prop_assert_eq!(frame.delay, std::time::Duration::ZERO,
                "instant replay frame {} should have zero delay", frame.frame_index);
        }

        prop_assert_eq!(session.stats().replay_duration_ms, 0,
            "instant replay should have zero total duration");
    }
}

// =============================================================================
// Serde roundtrip: ReplayConfig
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    /// ReplayConfig serde roundtrip preserves all fields.
    #[test]
    fn prop_replay_config_serde(config in arb_replay_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: ReplayConfig = serde_json::from_str(&json).unwrap();
        prop_assert!((back.speed - config.speed).abs() < 1e-10,
            "speed: {} vs {}", back.speed, config.speed);
        prop_assert_eq!(back.max_delay_ms, config.max_delay_ms);
        prop_assert_eq!(back.skip_empty, config.skip_empty);
        prop_assert_eq!(back.include_markers, config.include_markers);
        prop_assert_eq!(back.pane_filter.len(), config.pane_filter.len());
        prop_assert_eq!(back.kind_filter.len(), config.kind_filter.len());
    }

    /// ReplayConfig default serde roundtrip.
    #[test]
    fn prop_replay_config_default_roundtrip(_dummy in 0..1_u8) {
        let config = ReplayConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let back: ReplayConfig = serde_json::from_str(&json).unwrap();
        prop_assert!((back.speed - 1.0).abs() < 1e-15);
        prop_assert_eq!(back.max_delay_ms, 5000);
        prop_assert!(!back.skip_empty);
        prop_assert!(back.include_markers);
    }

    /// ReplayConfig from empty JSON uses defaults.
    #[test]
    fn prop_replay_config_from_empty_json(_dummy in 0..1_u8) {
        let back: ReplayConfig = serde_json::from_str("{}").unwrap();
        prop_assert!((back.speed - 1.0).abs() < 1e-15);
        prop_assert_eq!(back.max_delay_ms, 5000);
        prop_assert!(back.include_markers);
    }
}

// =============================================================================
// Serde roundtrip: ReplayStats
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    /// ReplayStats serde roundtrip preserves all fields.
    #[test]
    fn prop_replay_stats_serde(
        emitted in 0_usize..1000,
        skipped in 0_usize..1000,
        original_ms in 0_u64..1_000_000,
        replay_ms in 0_u64..1_000_000,
        unique_panes in 0_usize..20,
        completed in any::<bool>(),
    ) {
        let stats = ReplayStats {
            frames_emitted: emitted,
            frames_skipped: skipped,
            original_duration_ms: original_ms,
            replay_duration_ms: replay_ms,
            unique_panes,
            by_kind: std::collections::HashMap::new(),
            completed,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let back: ReplayStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.frames_emitted, emitted);
        prop_assert_eq!(back.frames_skipped, skipped);
        prop_assert_eq!(back.original_duration_ms, original_ms);
        prop_assert_eq!(back.replay_duration_ms, replay_ms);
        prop_assert_eq!(back.unique_panes, unique_panes);
        prop_assert_eq!(back.completed, completed);
    }
}

// =============================================================================
// Serde roundtrip: ReplayState
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// ReplayState serde roundtrip for all variants.
    #[test]
    fn prop_replay_state_serde(state in arb_replay_state()) {
        let json = serde_json::to_string(&state).unwrap();
        let back: ReplayState = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, state);
    }

    /// ReplayState serializes to snake_case strings.
    #[test]
    fn prop_replay_state_snake_case(state in arb_replay_state()) {
        let json = serde_json::to_string(&state).unwrap();
        let expected = match state {
            ReplayState::Ready => "\"ready\"",
            ReplayState::Playing => "\"playing\"",
            ReplayState::Paused => "\"paused\"",
            ReplayState::Completed => "\"completed\"",
        };
        prop_assert_eq!(&json, expected);
    }
}

// =============================================================================
// Property: Combined pane + kind filter
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn combined_filter_respects_both(
        events in arb_events(20),
        filter_pane in 1_u64..5,
        filter_kind in arb_event_kind(),
    ) {
        if events.is_empty() {
            return Ok(());
        }

        let config = ReplayConfig::instant()
            .with_panes(vec![filter_pane])
            .with_kinds(vec![filter_kind]);
        let mut session = make_session(events.clone(), config).unwrap();
        let frames = session.collect_remaining();

        for frame in &frames {
            prop_assert_eq!(frame.event.pane_id, filter_pane,
                "frame pane {} should match filter {}", frame.event.pane_id, filter_pane);
            prop_assert_eq!(frame.event.event_kind, filter_kind,
                "frame kind {:?} should match filter {:?}", frame.event.event_kind, filter_kind);
        }

        let expected = events.iter()
            .filter(|e| e.pane_id == filter_pane && e.event_kind == filter_kind)
            .count();
        prop_assert_eq!(frames.len(), expected,
            "expected {} frames for combined filter, got {}", expected, frames.len());
    }
}
