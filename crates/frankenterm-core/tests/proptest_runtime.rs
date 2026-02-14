//! Property-based tests for the runtime module.
//!
//! Tests invariants of RuntimeLockMemoryTelemetrySnapshot serde,
//! ResizeWatchdogSeverity serde/ordering, ResizeWatchdogAssessment
//! serde roundtrips, warning_line consistency, and field relationships.

use frankenterm_core::resize_scheduler::{ResizeExecutionPhase, ResizeStalledTransaction};
use frankenterm_core::runtime::{
    ResizeWatchdogAssessment, ResizeWatchdogSeverity, RuntimeLockMemoryTelemetrySnapshot,
};
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────────────

fn arb_severity() -> impl Strategy<Value = ResizeWatchdogSeverity> {
    prop_oneof![
        Just(ResizeWatchdogSeverity::Healthy),
        Just(ResizeWatchdogSeverity::Warning),
        Just(ResizeWatchdogSeverity::Critical),
        Just(ResizeWatchdogSeverity::SafeModeActive),
    ]
}

fn arb_execution_phase() -> impl Strategy<Value = ResizeExecutionPhase> {
    prop_oneof![
        Just(ResizeExecutionPhase::Preparing),
        Just(ResizeExecutionPhase::Reflowing),
        Just(ResizeExecutionPhase::Presenting),
    ]
}

fn arb_stalled_transaction() -> impl Strategy<Value = ResizeStalledTransaction> {
    (
        any::<u64>(),
        any::<u64>(),
        prop::option::of(arb_execution_phase()),
        any::<u64>(),
        prop::option::of(any::<u64>()),
    )
        .prop_map(|(pane_id, intent_seq, active_phase, age_ms, latest_seq)| {
            ResizeStalledTransaction {
                pane_id,
                intent_seq,
                active_phase,
                age_ms,
                latest_seq,
            }
        })
}

/// Generate a telemetry snapshot with ordered percentile fields.
fn arb_telemetry_snapshot() -> impl Strategy<Value = RuntimeLockMemoryTelemetrySnapshot> {
    (
        any::<u64>(),                                        // timestamp_ms
        (0.0f64..1000.0),                                    // avg_storage_lock_wait_ms
        (0.0f64..1000.0, 0.0f64..1000.0, 0.0f64..1000.0),    // p50, p95, max wait
        any::<u64>(),                                        // storage_lock_contention_events
        (0.0f64..1000.0),                                    // avg_storage_lock_hold_ms
        (0.0f64..1000.0, 0.0f64..1000.0, 0.0f64..1000.0),    // p50, p95, max hold
        any::<u64>(),                                        // cursor_snapshot_bytes_last
        (0u64..1_000_000, 0u64..1_000_000, 0u64..1_000_000), // p50, p95, max bytes
        (0.0f64..1_000_000.0),                               // avg_cursor_snapshot_bytes
    )
        .prop_map(
            |(
                timestamp_ms,
                avg_wait,
                (w_p50, w_p95, w_max),
                contention,
                avg_hold,
                (h_p50, h_p95, h_max),
                cursor_last,
                (c_p50, c_p95, c_max),
                avg_cursor,
            )| {
                // Sort percentiles to maintain p50 <= p95 <= max ordering
                let mut wait: [f64; 3] = (w_p50, w_p95, w_max).into();
                wait.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let mut hold: [f64; 3] = (h_p50, h_p95, h_max).into();
                hold.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let mut cursor: [u64; 3] = (c_p50, c_p95, c_max).into();
                cursor.sort();

                RuntimeLockMemoryTelemetrySnapshot {
                    timestamp_ms,
                    avg_storage_lock_wait_ms: avg_wait,
                    p50_storage_lock_wait_ms: wait[0],
                    p95_storage_lock_wait_ms: wait[1],
                    max_storage_lock_wait_ms: wait[2],
                    storage_lock_contention_events: contention,
                    avg_storage_lock_hold_ms: avg_hold,
                    p50_storage_lock_hold_ms: hold[0],
                    p95_storage_lock_hold_ms: hold[1],
                    max_storage_lock_hold_ms: hold[2],
                    cursor_snapshot_bytes_last: cursor_last,
                    p50_cursor_snapshot_bytes: cursor[0],
                    p95_cursor_snapshot_bytes: cursor[1],
                    cursor_snapshot_bytes_max: cursor[2],
                    avg_cursor_snapshot_bytes: avg_cursor,
                }
            },
        )
}

fn arb_assessment() -> impl Strategy<Value = ResizeWatchdogAssessment> {
    (
        arb_severity(),
        0usize..100,    // stalled_total
        0usize..100,    // stalled_critical
        1u64..10_000,   // warning_threshold_ms
        1u64..30_000,   // critical_threshold_ms
        1usize..10,     // critical_stalled_limit
        any::<bool>(),  // safe_mode_recommended
        any::<bool>(),  // safe_mode_active
        any::<bool>(),  // legacy_fallback_enabled
        "[a-z_]{1,30}", // recommended_action
        proptest::collection::vec(arb_stalled_transaction(), 0..4),
    )
        .prop_map(
            |(
                severity,
                stalled_total,
                stalled_critical,
                warning_threshold_ms,
                critical_threshold_ms,
                critical_stalled_limit,
                safe_mode_recommended,
                safe_mode_active,
                legacy_fallback_enabled,
                recommended_action,
                sample_stalled,
            )| {
                // Ensure critical threshold >= warning threshold
                let (wt, ct) = if warning_threshold_ms <= critical_threshold_ms {
                    (warning_threshold_ms, critical_threshold_ms)
                } else {
                    (critical_threshold_ms, warning_threshold_ms)
                };
                ResizeWatchdogAssessment {
                    severity,
                    stalled_total,
                    stalled_critical,
                    warning_threshold_ms: wt,
                    critical_threshold_ms: ct,
                    critical_stalled_limit,
                    safe_mode_recommended,
                    safe_mode_active,
                    legacy_fallback_enabled,
                    recommended_action,
                    sample_stalled,
                }
            },
        )
}

// ── ResizeWatchdogSeverity: serde ───────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// JSON serde roundtrip preserves the severity variant.
    #[test]
    fn severity_serde_roundtrip(s in arb_severity()) {
        let json = serde_json::to_string(&s).unwrap();
        let parsed: ResizeWatchdogSeverity = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed, s);
    }

    /// Serde produces snake_case strings.
    #[test]
    fn severity_serde_snake_case(s in arb_severity()) {
        let json = serde_json::to_string(&s).unwrap();
        let unquoted = json.trim_matches('"');
        // Should be lowercase with underscores only
        for ch in unquoted.chars() {
            prop_assert!(ch.is_ascii_lowercase() || ch == '_',
                "expected snake_case, got: {}", unquoted);
        }
    }

    /// Each severity serializes to its expected string.
    #[test]
    fn severity_known_serializations(s in arb_severity()) {
        let json = serde_json::to_string(&s).unwrap();
        let expected = match s {
            ResizeWatchdogSeverity::Healthy => "\"healthy\"",
            ResizeWatchdogSeverity::Warning => "\"warning\"",
            ResizeWatchdogSeverity::Critical => "\"critical\"",
            ResizeWatchdogSeverity::SafeModeActive => "\"safe_mode_active\"",
        };
        prop_assert_eq!(json.as_str(), expected);
    }

    /// Copy semantics work correctly.
    #[test]
    fn severity_copy(s in arb_severity()) {
        let copied = s;
        prop_assert_eq!(s, copied);
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Check that two f64 values are approximately equal (JSON roundtrip precision).
fn f64_approx_eq(a: f64, b: f64) -> bool {
    if a == b {
        return true;
    }
    let diff = (a - b).abs();
    let max_val = a.abs().max(b.abs());
    if max_val == 0.0 {
        diff < 1e-15
    } else {
        diff / max_val < 1e-12
    }
}

/// Assert that two telemetry snapshots are approximately equal.
fn assert_snapshot_approx_eq(
    a: &RuntimeLockMemoryTelemetrySnapshot,
    b: &RuntimeLockMemoryTelemetrySnapshot,
) -> Result<(), proptest::test_runner::TestCaseError> {
    prop_assert_eq!(a.timestamp_ms, b.timestamp_ms);
    prop_assert!(
        f64_approx_eq(a.avg_storage_lock_wait_ms, b.avg_storage_lock_wait_ms),
        "avg_wait: {} vs {}",
        a.avg_storage_lock_wait_ms,
        b.avg_storage_lock_wait_ms
    );
    prop_assert!(
        f64_approx_eq(a.p50_storage_lock_wait_ms, b.p50_storage_lock_wait_ms),
        "p50_wait: {} vs {}",
        a.p50_storage_lock_wait_ms,
        b.p50_storage_lock_wait_ms
    );
    prop_assert!(
        f64_approx_eq(a.p95_storage_lock_wait_ms, b.p95_storage_lock_wait_ms),
        "p95_wait: {} vs {}",
        a.p95_storage_lock_wait_ms,
        b.p95_storage_lock_wait_ms
    );
    prop_assert!(
        f64_approx_eq(a.max_storage_lock_wait_ms, b.max_storage_lock_wait_ms),
        "max_wait: {} vs {}",
        a.max_storage_lock_wait_ms,
        b.max_storage_lock_wait_ms
    );
    prop_assert_eq!(
        a.storage_lock_contention_events,
        b.storage_lock_contention_events
    );
    prop_assert!(
        f64_approx_eq(a.avg_storage_lock_hold_ms, b.avg_storage_lock_hold_ms),
        "avg_hold: {} vs {}",
        a.avg_storage_lock_hold_ms,
        b.avg_storage_lock_hold_ms
    );
    prop_assert!(
        f64_approx_eq(a.p50_storage_lock_hold_ms, b.p50_storage_lock_hold_ms),
        "p50_hold: {} vs {}",
        a.p50_storage_lock_hold_ms,
        b.p50_storage_lock_hold_ms
    );
    prop_assert!(
        f64_approx_eq(a.p95_storage_lock_hold_ms, b.p95_storage_lock_hold_ms),
        "p95_hold: {} vs {}",
        a.p95_storage_lock_hold_ms,
        b.p95_storage_lock_hold_ms
    );
    prop_assert!(
        f64_approx_eq(a.max_storage_lock_hold_ms, b.max_storage_lock_hold_ms),
        "max_hold: {} vs {}",
        a.max_storage_lock_hold_ms,
        b.max_storage_lock_hold_ms
    );
    prop_assert_eq!(a.cursor_snapshot_bytes_last, b.cursor_snapshot_bytes_last);
    prop_assert_eq!(a.p50_cursor_snapshot_bytes, b.p50_cursor_snapshot_bytes);
    prop_assert_eq!(a.p95_cursor_snapshot_bytes, b.p95_cursor_snapshot_bytes);
    prop_assert_eq!(a.cursor_snapshot_bytes_max, b.cursor_snapshot_bytes_max);
    prop_assert!(
        f64_approx_eq(a.avg_cursor_snapshot_bytes, b.avg_cursor_snapshot_bytes),
        "avg_cursor: {} vs {}",
        a.avg_cursor_snapshot_bytes,
        b.avg_cursor_snapshot_bytes
    );
    Ok(())
}

// ── RuntimeLockMemoryTelemetrySnapshot: serde ───────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// JSON serde roundtrip preserves all fields (with f64 tolerance).
    #[test]
    fn telemetry_snapshot_serde_roundtrip(snap in arb_telemetry_snapshot()) {
        let json = serde_json::to_string(&snap).unwrap();
        let parsed: RuntimeLockMemoryTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        assert_snapshot_approx_eq(&parsed, &snap)?;
    }

    /// Serialized snapshot is valid JSON object.
    #[test]
    fn telemetry_snapshot_valid_json(snap in arb_telemetry_snapshot()) {
        let json = serde_json::to_string(&snap).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.is_object());
    }

    /// Pretty-printed JSON roundtrips (with f64 tolerance).
    #[test]
    fn telemetry_snapshot_pretty_roundtrip(snap in arb_telemetry_snapshot()) {
        let json = serde_json::to_string_pretty(&snap).unwrap();
        let parsed: RuntimeLockMemoryTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        assert_snapshot_approx_eq(&parsed, &snap)?;
    }

    /// Clone produces an equal snapshot.
    #[test]
    fn telemetry_snapshot_clone(snap in arb_telemetry_snapshot()) {
        let cloned = snap.clone();
        prop_assert_eq!(snap, cloned);
    }
}

// ── Telemetry percentile ordering ───────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// After serde roundtrip, p50 <= p95 <= max still holds for wait latencies.
    #[test]
    fn telemetry_wait_percentile_order(snap in arb_telemetry_snapshot()) {
        let json = serde_json::to_string(&snap).unwrap();
        let parsed: RuntimeLockMemoryTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        prop_assert!(parsed.p50_storage_lock_wait_ms <= parsed.p95_storage_lock_wait_ms,
            "p50 ({}) > p95 ({})", parsed.p50_storage_lock_wait_ms, parsed.p95_storage_lock_wait_ms);
        prop_assert!(parsed.p95_storage_lock_wait_ms <= parsed.max_storage_lock_wait_ms,
            "p95 ({}) > max ({})", parsed.p95_storage_lock_wait_ms, parsed.max_storage_lock_wait_ms);
    }

    /// After serde roundtrip, p50 <= p95 <= max still holds for hold times.
    #[test]
    fn telemetry_hold_percentile_order(snap in arb_telemetry_snapshot()) {
        let json = serde_json::to_string(&snap).unwrap();
        let parsed: RuntimeLockMemoryTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        prop_assert!(parsed.p50_storage_lock_hold_ms <= parsed.p95_storage_lock_hold_ms,
            "p50 ({}) > p95 ({})", parsed.p50_storage_lock_hold_ms, parsed.p95_storage_lock_hold_ms);
        prop_assert!(parsed.p95_storage_lock_hold_ms <= parsed.max_storage_lock_hold_ms,
            "p95 ({}) > max ({})", parsed.p95_storage_lock_hold_ms, parsed.max_storage_lock_hold_ms);
    }

    /// After serde roundtrip, p50 <= p95 <= max still holds for cursor bytes.
    #[test]
    fn telemetry_cursor_percentile_order(snap in arb_telemetry_snapshot()) {
        let json = serde_json::to_string(&snap).unwrap();
        let parsed: RuntimeLockMemoryTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        prop_assert!(parsed.p50_cursor_snapshot_bytes <= parsed.p95_cursor_snapshot_bytes,
            "p50 ({}) > p95 ({})", parsed.p50_cursor_snapshot_bytes, parsed.p95_cursor_snapshot_bytes);
        prop_assert!(parsed.p95_cursor_snapshot_bytes <= parsed.cursor_snapshot_bytes_max,
            "p95 ({}) > max ({})", parsed.p95_cursor_snapshot_bytes, parsed.cursor_snapshot_bytes_max);
    }
}

// ── ResizeWatchdogAssessment: serde ─────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// JSON serde roundtrip preserves all assessment fields.
    #[test]
    fn assessment_serde_roundtrip(a in arb_assessment()) {
        let json = serde_json::to_string(&a).unwrap();
        let parsed: ResizeWatchdogAssessment = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed, a);
    }

    /// Serialized assessment is valid JSON object.
    #[test]
    fn assessment_valid_json(a in arb_assessment()) {
        let json = serde_json::to_string(&a).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.is_object());
    }

    /// Assessment JSON has required top-level fields.
    #[test]
    fn assessment_has_required_fields(a in arb_assessment()) {
        let json = serde_json::to_string(&a).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.get("severity").is_some(), "missing 'severity'");
        prop_assert!(value.get("stalled_total").is_some(), "missing 'stalled_total'");
        prop_assert!(value.get("stalled_critical").is_some(), "missing 'stalled_critical'");
        prop_assert!(value.get("recommended_action").is_some(), "missing 'recommended_action'");
    }

    /// Clone produces an equal assessment.
    #[test]
    fn assessment_clone(a in arb_assessment()) {
        let cloned = a.clone();
        prop_assert_eq!(a, cloned);
    }
}

// ── ResizeWatchdogAssessment: warning_line ───────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Healthy severity produces None warning_line.
    #[test]
    fn warning_line_healthy_is_none(a in arb_assessment()) {
        let healthy = ResizeWatchdogAssessment {
            severity: ResizeWatchdogSeverity::Healthy,
            ..a
        };
        prop_assert!(healthy.warning_line().is_none(),
            "Healthy should produce None warning_line");
    }

    /// Non-Healthy severity always produces Some warning_line.
    #[test]
    fn warning_line_non_healthy_is_some(
        a in arb_assessment(),
        s in prop_oneof![
            Just(ResizeWatchdogSeverity::Warning),
            Just(ResizeWatchdogSeverity::Critical),
            Just(ResizeWatchdogSeverity::SafeModeActive),
        ],
    ) {
        let non_healthy = ResizeWatchdogAssessment {
            severity: s,
            ..a
        };
        prop_assert!(non_healthy.warning_line().is_some(),
            "non-Healthy severity {:?} should produce Some warning_line", s);
    }

    /// Warning line always contains "watchdog" text.
    #[test]
    fn warning_line_contains_watchdog(a in arb_assessment()) {
        if let Some(line) = a.warning_line() {
            // All warning lines contain "watchdog" (lowercase in output)
            let lower = line.to_lowercase();
            prop_assert!(lower.contains("watchdog"),
                "warning_line should contain 'watchdog': {}", line);
        }
    }

    /// Critical warning line mentions "safe-mode" when legacy fallback is enabled.
    #[test]
    fn warning_line_critical_mentions_legacy(a in arb_assessment()) {
        let critical_with_legacy = ResizeWatchdogAssessment {
            severity: ResizeWatchdogSeverity::Critical,
            legacy_fallback_enabled: true,
            ..a
        };
        let line = critical_with_legacy.warning_line().unwrap();
        prop_assert!(line.contains("legacy"),
            "critical+legacy should mention 'legacy': {}", line);
    }
}

// ── ResizeStalledTransaction: serde ─────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// JSON serde roundtrip preserves stalled transaction fields.
    #[test]
    fn stalled_transaction_serde_roundtrip(txn in arb_stalled_transaction()) {
        let json = serde_json::to_string(&txn).unwrap();
        let parsed: ResizeStalledTransaction = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed, txn);
    }

    /// Clone produces an equal stalled transaction.
    #[test]
    fn stalled_transaction_clone(txn in arb_stalled_transaction()) {
        let cloned = txn.clone();
        prop_assert_eq!(txn, cloned);
    }
}

// ── ResizeExecutionPhase: serde ─────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// JSON serde roundtrip preserves execution phase.
    #[test]
    fn execution_phase_serde_roundtrip(phase in arb_execution_phase()) {
        let json = serde_json::to_string(&phase).unwrap();
        let parsed: ResizeExecutionPhase = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed, phase);
    }

    /// Copy semantics for execution phase.
    #[test]
    fn execution_phase_copy(phase in arb_execution_phase()) {
        let copied = phase;
        prop_assert_eq!(phase, copied);
    }
}

// ── Assessment threshold ordering ───────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// After serde roundtrip, warning_threshold <= critical_threshold still holds.
    #[test]
    fn assessment_threshold_order_preserved(a in arb_assessment()) {
        let json = serde_json::to_string(&a).unwrap();
        let parsed: ResizeWatchdogAssessment = serde_json::from_str(&json).unwrap();
        prop_assert!(parsed.warning_threshold_ms <= parsed.critical_threshold_ms,
            "warning ({}) > critical ({})", parsed.warning_threshold_ms, parsed.critical_threshold_ms);
    }

    /// stalled_critical <= stalled_total is a reasonable invariant for generated data.
    #[test]
    fn assessment_stalled_ordering(
        total in 0usize..100,
        crit_frac in 0usize..100,
    ) {
        let critical = crit_frac % (total + 1);
        prop_assert!(critical <= total);
    }
}
