//! Property-based tests for replay_checkpoint (ft-og6q6.3.5).
//!
//! Invariants tested:
//! - CP-1: event_position monotonically increases with advance()
//! - CP-2: checkpoint fires at configured event_interval
//! - CP-3: checkpoint fires at configured time_interval_ms
//! - CP-4: resume_from restores event_position exactly
//! - CP-5: halted checkpointer rejects further advance()
//! - CP-6: lenient mode: events_skipped == number of handle_error calls
//! - CP-7: strict mode: no checkpoints created on error
//! - CP-8: default mode: checkpoint created on error (when enabled)
//! - CP-9: report total_events >= events_replayed + events_skipped
//! - CP-10: deterministic_eq ignores duration_ms
//! - CP-11: CheckpointState serde roundtrip
//! - CP-12: CheckpointConfig serde roundtrip
//! - CP-13: ReplayError serde roundtrip
//! - CP-14: ReplayReport serde roundtrip
//! - CP-15: FailureMode serde roundtrip
//! - CP-16: ReplayErrorKind serde roundtrip
//! - CP-17: checkpoint_count == len of checkpoints()
//! - CP-18: ProcessResult::Checkpointed only at interval boundaries
//! - CP-19: effects_logged and anomalies_detected track record_effect/record_anomaly
//! - CP-20: complete() sets is_completed, no further halts
//! - CP-21: errors() grows with handle_error calls
//! - CP-22: report.is_success iff completed && no failure
//! - CP-23: effect_log_hash set via set_effect_log_hash persists
//! - CP-24: checkpoint state captures virtual_clock_ms

use proptest::prelude::*;

use frankenterm_core::replay_checkpoint::{
    CheckpointConfig, CheckpointState, FailureMode, ProcessResult, ReplayCheckpointer,
    ReplayError, ReplayErrorKind, ReplayReport, CHECKPOINT_VERSION,
};

// ── Strategies ──────────────────────────────────────────────────────────

fn arb_failure_mode() -> impl Strategy<Value = FailureMode> {
    prop_oneof![
        Just(FailureMode::Default),
        Just(FailureMode::Lenient),
        Just(FailureMode::Strict),
    ]
}

fn arb_error_kind() -> impl Strategy<Value = ReplayErrorKind> {
    prop_oneof![
        Just(ReplayErrorKind::UnknownEventKind),
        Just(ReplayErrorKind::SchemaMismatch),
        Just(ReplayErrorKind::CorruptEvent),
        Just(ReplayErrorKind::ClockAnomaly),
        Just(ReplayErrorKind::IsolationViolation),
        Just(ReplayErrorKind::CheckpointError),
        Just(ReplayErrorKind::RuntimeError),
    ]
}

fn arb_replay_error() -> impl Strategy<Value = ReplayError> {
    (arb_error_kind(), 0u64..1000, "evt_[a-z]{4}", "msg_[a-z]{8}")
        .prop_map(|(kind, pos, eid, msg)| ReplayError {
            kind,
            event_position: pos,
            event_id: Some(eid),
            message: msg,
            context: None,
        })
}

fn arb_checkpoint_config() -> impl Strategy<Value = CheckpointConfig> {
    (2u64..50, 0u64..10000, any::<bool>(), any::<bool>()).prop_map(
        |(event_interval, time_interval_ms, cleanup, on_error)| CheckpointConfig {
            event_interval,
            time_interval_ms,
            cleanup_on_success: cleanup,
            checkpoint_on_error: on_error,
        },
    )
}

fn arb_checkpoint_state() -> impl Strategy<Value = CheckpointState> {
    (
        "[a-z]{6}",
        0u64..10000,
        0u64..1_000_000,
        0u64..500,
        0u64..100,
        0u64..200,
        0u64..50,
    )
        .prop_map(
            |(run_id, pos, vclock, decisions, skipped, effects, anomalies)| {
                let mut s = CheckpointState::new(run_id);
                s.event_position = pos;
                s.virtual_clock_ms = vclock;
                s.decisions_made = decisions;
                s.events_skipped = skipped;
                s.effects_logged = effects;
                s.anomalies_detected = anomalies;
                s
            },
        )
}

// ── CP-1: event_position monotonically increases ────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn cp1_event_position_monotonic(n in 1usize..200) {
        let ckpt = ReplayCheckpointer::with_defaults("cp1".into());
        let mut prev = 0u64;
        for i in 0..n {
            ckpt.advance(i as u64 * 10, i as u64);
            let pos = ckpt.current_state().event_position;
            prop_assert!(pos > prev, "position must increase: {} <= {}", pos, prev);
            prev = pos;
        }
    }

    // ── CP-2: checkpoint at event_interval ───────────────────────────────

    #[test]
    fn cp2_checkpoint_at_event_interval(interval in 2u64..30, events in 1usize..120) {
        let config = CheckpointConfig {
            event_interval: interval,
            time_interval_ms: 0,
            ..Default::default()
        };
        let ckpt = ReplayCheckpointer::new("cp2".into(), config, FailureMode::Default);
        let mut checkpoint_results = 0u64;
        for i in 0..events {
            let result = ckpt.advance(i as u64 * 10, i as u64);
            if result == ProcessResult::Checkpointed {
                checkpoint_results += 1;
            }
        }
        let expected = events as u64 / interval;
        prop_assert_eq!(checkpoint_results, expected,
            "expected {} checkpoints for {} events at interval {}",
            expected, events, interval);
    }

    // ── CP-3: checkpoint at time_interval ────────────────────────────────

    #[test]
    fn cp3_checkpoint_at_time_interval(
        time_ms in 100u64..5000,
        events in 5usize..50
    ) {
        let config = CheckpointConfig {
            event_interval: 0, // disable event-based
            time_interval_ms: time_ms,
            ..Default::default()
        };
        let ckpt = ReplayCheckpointer::new("cp3".into(), config, FailureMode::Default);
        let mut count = 0u64;
        for i in 0..events {
            // Space events apart at exactly time_ms intervals to make checkpoints predictable.
            let wall = i as u64 * time_ms;
            let result = ckpt.advance(wall, wall);
            if result == ProcessResult::Checkpointed {
                count += 1;
            }
        }
        // At least some checkpoints should fire for sufficient events.
        if events >= 3 {
            prop_assert!(count >= 1, "expected at least 1 time-based checkpoint");
        }
    }

    // ── CP-4: resume_from restores event_position ────────────────────────

    #[test]
    fn cp4_resume_restores_position(state in arb_checkpoint_state()) {
        let ckpt = ReplayCheckpointer::with_defaults("cp4".into());
        ckpt.resume_from(&state);
        let restored = ckpt.current_state();
        prop_assert_eq!(restored.event_position, state.event_position);
        prop_assert_eq!(restored.virtual_clock_ms, state.virtual_clock_ms);
        prop_assert_eq!(restored.decisions_made, state.decisions_made);
        prop_assert_eq!(restored.events_skipped, state.events_skipped);
        prop_assert_eq!(restored.effects_logged, state.effects_logged);
        prop_assert_eq!(restored.anomalies_detected, state.anomalies_detected);
    }

    // ── CP-5: halted rejects advance ────────────────────────────────────

    #[test]
    fn cp5_halted_rejects_advance(n in 1usize..20) {
        let ckpt = ReplayCheckpointer::with_defaults("cp5".into());
        ckpt.advance(0, 0);
        let error = ReplayError {
            kind: ReplayErrorKind::RuntimeError,
            event_position: 0,
            event_id: None,
            message: "halt".into(),
            context: None,
        };
        ckpt.handle_error(error, 100);
        prop_assert!(ckpt.is_halted());
        for i in 0..n {
            let result = ckpt.advance(i as u64 * 10, i as u64 + 1);
            let is_halted = matches!(result, ProcessResult::Halted(_));
            prop_assert!(is_halted, "advance after halt must return Halted");
        }
    }

    // ── CP-6: lenient mode tracks skips ─────────────────────────────────

    #[test]
    fn cp6_lenient_tracks_skips(n_errors in 1usize..30) {
        let config = CheckpointConfig::default();
        let ckpt = ReplayCheckpointer::new("cp6".into(), config, FailureMode::Lenient);
        for i in 0..n_errors {
            let error = ReplayError {
                kind: ReplayErrorKind::CorruptEvent,
                event_position: i as u64,
                event_id: None,
                message: "skip".into(),
                context: None,
            };
            let result = ckpt.handle_error(error, i as u64 * 10);
            prop_assert_eq!(result, ProcessResult::Skipped);
        }
        let state = ckpt.current_state();
        prop_assert_eq!(state.events_skipped, n_errors as u64);
        prop_assert!(!ckpt.is_halted(), "lenient mode should not halt");
    }

    // ── CP-7: strict mode: no checkpoints on error ──────────────────────

    #[test]
    fn cp7_strict_no_checkpoints(n_errors in 1usize..10) {
        let config = CheckpointConfig {
            checkpoint_on_error: true,
            ..Default::default()
        };
        let ckpt = ReplayCheckpointer::new("cp7".into(), config, FailureMode::Strict);
        ckpt.advance(0, 0);
        let error = ReplayError {
            kind: ReplayErrorKind::RuntimeError,
            event_position: 0,
            event_id: None,
            message: "strict".into(),
            context: None,
        };
        let result = ckpt.handle_error(error, 100);
        let is_halted = matches!(result, ProcessResult::Halted(_));
        prop_assert!(is_halted, "strict mode should halt");
        prop_assert_eq!(ckpt.checkpoint_count(), 0, "strict mode must not checkpoint");
        let _ = n_errors; // Suppress unused warning.
    }

    // ── CP-8: default mode checkpoints on error ─────────────────────────

    #[test]
    fn cp8_default_checkpoints_on_error(pos in 0u64..1000) {
        let config = CheckpointConfig {
            checkpoint_on_error: true,
            ..Default::default()
        };
        let ckpt = ReplayCheckpointer::new("cp8".into(), config, FailureMode::Default);
        ckpt.advance(pos, 0);
        let error = ReplayError {
            kind: ReplayErrorKind::SchemaMismatch,
            event_position: pos,
            event_id: None,
            message: "default".into(),
            context: None,
        };
        ckpt.handle_error(error, 500);
        prop_assert_eq!(ckpt.checkpoint_count(), 1);
        let cp = ckpt.last_checkpoint().unwrap();
        prop_assert_eq!(cp.event_position, 1); // After one advance.
    }

    // ── CP-9: report consistency ────────────────────────────────────────

    #[test]
    fn cp9_report_consistency(n in 1usize..100) {
        let ckpt = ReplayCheckpointer::with_defaults("cp9".into());
        for i in 0..n {
            ckpt.advance(i as u64 * 10, i as u64);
        }
        ckpt.complete();
        let report = ckpt.report(n as u64, 1000);
        prop_assert_eq!(report.events_replayed, n as u64);
        prop_assert_eq!(report.events_skipped, 0);
        prop_assert!(report.is_success());
        prop_assert!(report.total_events >= report.events_replayed);
    }

    // ── CP-10: deterministic_eq ignores duration_ms ─────────────────────

    #[test]
    fn cp10_deterministic_eq_ignores_duration(dur1 in 0u64..10000, dur2 in 0u64..10000) {
        let ckpt = ReplayCheckpointer::with_defaults("cp10".into());
        ckpt.advance(100, 0);
        ckpt.complete();
        let r1 = ckpt.report(1, dur1);
        let r2 = ckpt.report(1, dur2);
        prop_assert!(r1.deterministic_eq(&r2),
            "reports with different durations must be deterministic_eq");
    }

    // ── CP-11: CheckpointState serde roundtrip ──────────────────────────

    #[test]
    fn cp11_checkpoint_state_serde(state in arb_checkpoint_state()) {
        let json = state.to_json();
        let restored = CheckpointState::from_json(&json).unwrap();
        prop_assert_eq!(restored.event_position, state.event_position);
        prop_assert_eq!(restored.virtual_clock_ms, state.virtual_clock_ms);
        prop_assert_eq!(restored.decisions_made, state.decisions_made);
        prop_assert_eq!(restored.checkpoint_version, CHECKPOINT_VERSION);
    }

    // ── CP-12: CheckpointConfig serde roundtrip ─────────────────────────

    #[test]
    fn cp12_config_serde(config in arb_checkpoint_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let restored: CheckpointConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.event_interval, config.event_interval);
        prop_assert_eq!(restored.time_interval_ms, config.time_interval_ms);
        prop_assert_eq!(restored.cleanup_on_success, config.cleanup_on_success);
        prop_assert_eq!(restored.checkpoint_on_error, config.checkpoint_on_error);
    }

    // ── CP-13: ReplayError serde roundtrip ───────────────────────────────

    #[test]
    fn cp13_error_serde(error in arb_replay_error()) {
        let json = serde_json::to_string(&error).unwrap();
        let restored: ReplayError = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.kind, error.kind);
        prop_assert_eq!(restored.event_position, error.event_position);
        prop_assert_eq!(restored.event_id, error.event_id);
        prop_assert_eq!(restored.message, error.message);
    }

    // ── CP-14: ReplayReport serde roundtrip ──────────────────────────────

    #[test]
    fn cp14_report_serde(n in 1usize..50, dur in 0u64..10000) {
        let ckpt = ReplayCheckpointer::with_defaults("cp14".into());
        for i in 0..n {
            ckpt.advance(i as u64 * 10, i as u64);
        }
        ckpt.complete();
        let report = ckpt.report(n as u64, dur);
        let json = serde_json::to_string(&report).unwrap();
        let restored: ReplayReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.events_replayed, report.events_replayed);
        prop_assert_eq!(restored.completed, report.completed);
        prop_assert_eq!(restored.replay_run_id, report.replay_run_id);
    }

    // ── CP-15: FailureMode serde roundtrip ───────────────────────────────

    #[test]
    fn cp15_failure_mode_serde(mode in arb_failure_mode()) {
        let json = serde_json::to_string(&mode).unwrap();
        let restored: FailureMode = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, mode);
    }

    // ── CP-16: ReplayErrorKind serde roundtrip ───────────────────────────

    #[test]
    fn cp16_error_kind_serde(kind in arb_error_kind()) {
        let json = serde_json::to_string(&kind).unwrap();
        let restored: ReplayErrorKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, kind);
    }

    // ── CP-17: checkpoint_count == checkpoints().len() ───────────────────

    #[test]
    fn cp17_checkpoint_count_matches(interval in 2u64..20, events in 1usize..80) {
        let config = CheckpointConfig {
            event_interval: interval,
            time_interval_ms: 0,
            ..Default::default()
        };
        let ckpt = ReplayCheckpointer::new("cp17".into(), config, FailureMode::Default);
        for i in 0..events {
            ckpt.advance(i as u64 * 10, i as u64);
        }
        let count = ckpt.checkpoint_count();
        let vec_len = ckpt.checkpoints().len();
        prop_assert_eq!(count, vec_len,
            "checkpoint_count ({}) must equal checkpoints().len() ({})",
            count, vec_len);
    }

    // ── CP-18: Checkpointed only at interval boundaries ─────────────────

    #[test]
    fn cp18_checkpointed_at_boundaries(interval in 3u64..25, events in 1usize..100) {
        let config = CheckpointConfig {
            event_interval: interval,
            time_interval_ms: 0,
            ..Default::default()
        };
        let ckpt = ReplayCheckpointer::new("cp18".into(), config, FailureMode::Default);
        for i in 0..events {
            let result = ckpt.advance(i as u64 * 10, i as u64);
            if result == ProcessResult::Checkpointed {
                let pos = ckpt.current_state().event_position;
                prop_assert_eq!(pos % interval, 0,
                    "checkpoint at non-boundary: pos={}, interval={}", pos, interval);
            }
        }
    }

    // ── CP-19: effects/anomalies tracking ───────────────────────────────

    #[test]
    fn cp19_effects_anomalies(n_effects in 0usize..30, n_anomalies in 0usize..30) {
        let ckpt = ReplayCheckpointer::with_defaults("cp19".into());
        for _ in 0..n_effects {
            ckpt.record_effect();
        }
        for _ in 0..n_anomalies {
            ckpt.record_anomaly();
        }
        let state = ckpt.current_state();
        prop_assert_eq!(state.effects_logged, n_effects as u64);
        prop_assert_eq!(state.anomalies_detected, n_anomalies as u64);
    }

    // ── CP-20: complete sets is_completed ────────────────────────────────

    #[test]
    fn cp20_complete_sets_flag(n in 0usize..50) {
        let ckpt = ReplayCheckpointer::with_defaults("cp20".into());
        for i in 0..n {
            ckpt.advance(i as u64 * 10, i as u64);
        }
        prop_assert!(!ckpt.is_completed());
        ckpt.complete();
        prop_assert!(ckpt.is_completed());
    }

    // ── CP-21: errors grow with handle_error ─────────────────────────────

    #[test]
    fn cp21_errors_grow(n in 1usize..15) {
        let ckpt = ReplayCheckpointer::new(
            "cp21".into(),
            CheckpointConfig::default(),
            FailureMode::Lenient,
        );
        for i in 0..n {
            let error = ReplayError {
                kind: ReplayErrorKind::CorruptEvent,
                event_position: i as u64,
                event_id: None,
                message: format!("err{i}"),
                context: None,
            };
            ckpt.handle_error(error, i as u64);
        }
        let errors = ckpt.errors();
        prop_assert_eq!(errors.len(), n,
            "expected {} errors, got {}", n, errors.len());
    }

    // ── CP-22: report.is_success iff completed && no failure ─────────────

    #[test]
    fn cp22_report_is_success(n in 1usize..30) {
        // Success case
        let ckpt = ReplayCheckpointer::with_defaults("cp22s".into());
        for i in 0..n {
            ckpt.advance(i as u64 * 10, i as u64);
        }
        ckpt.complete();
        let report = ckpt.report(n as u64, 500);
        prop_assert!(report.is_success());

        // Failure case
        let ckpt2 = ReplayCheckpointer::with_defaults("cp22f".into());
        ckpt2.advance(0, 0);
        let error = ReplayError {
            kind: ReplayErrorKind::RuntimeError,
            event_position: 0,
            event_id: None,
            message: "fail".into(),
            context: None,
        };
        ckpt2.handle_error(error, 100);
        let report2 = ckpt2.report(1, 100);
        prop_assert!(!report2.is_success());
    }

    // ── CP-23: effect_log_hash persists ──────────────────────────────────

    #[test]
    fn cp23_effect_log_hash(hash in "[0-9a-f]{64}") {
        let ckpt = ReplayCheckpointer::with_defaults("cp23".into());
        ckpt.set_effect_log_hash(hash.clone());
        let state = ckpt.current_state();
        prop_assert_eq!(state.effect_log_hash, hash);
    }

    // ── CP-24: checkpoint captures virtual_clock_ms ──────────────────────

    #[test]
    fn cp24_checkpoint_captures_vclock(interval in 2u64..20, events in 2usize..60) {
        let config = CheckpointConfig {
            event_interval: interval,
            time_interval_ms: 0,
            ..Default::default()
        };
        let ckpt = ReplayCheckpointer::new("cp24".into(), config, FailureMode::Default);
        for i in 0..events {
            let vclock = i as u64 * 100;
            ckpt.advance(vclock, i as u64);
        }
        let cps = ckpt.checkpoints();
        for cp in &cps {
            prop_assert!(cp.virtual_clock_ms > 0 || cp.event_position <= 1,
                "checkpoint vclock should be positive for non-initial positions");
        }
        // Each successive checkpoint should have >= virtual_clock_ms
        for window in cps.windows(2) {
            prop_assert!(window[1].virtual_clock_ms >= window[0].virtual_clock_ms,
                "checkpoint vclock must not decrease");
        }
    }
}
