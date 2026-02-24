//! Property-based tests for replay_guardrails (ft-og6q6.4.4).
//!
//! Invariants tested:
//! - GR-1: Tracker halts exactly at max_events + 1
//! - GR-2: Tracker Ok for all events within limit
//! - GR-3: Wall-clock halt triggers when elapsed exceeds limit
//! - GR-4: Memory warning fires exactly once
//! - GR-5: Watchdog Ok when progress is recent
//! - GR-6: Watchdog halt when stalled beyond timeout
//! - GR-7: ConcurrencyGate allows up to max
//! - GR-8: ConcurrencyGate rejects excess
//! - GR-9: ConcurrencyGate releases on drop
//! - GR-10: ResourceLimits serde roundtrip
//! - GR-11: LimitViolation serde roundtrip
//! - GR-12: GuardrailReport serde roundtrip
//! - GR-13: Report is_safe when no violations
//! - GR-14: Report not is_safe when halted
//! - GR-15: Disabled limits (0) never trigger
//! - GR-16: Event count matches number of record_event calls
//! - GR-17: Violations list grows on limit breach

use proptest::prelude::*;

use frankenterm_core::replay_guardrails::{
    CheckResult, ConcurrencyGate, GuardrailReport, LimitViolation, ResourceLimits, ResourceTracker,
};

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // ── GR-1: Halts exactly at max_events + 1 ──────────────────────────

    #[test]
    fn gr1_halt_at_limit(limit in 1u64..100) {
        let limits = ResourceLimits {
            max_events: limit,
            max_wall_clock_ms: 0,
            memory_warning_events: 0,
            watchdog_timeout_ms: 0,
            ..Default::default()
        };
        let tracker = ResourceTracker::new(limits, 0);
        // Events 1..limit should be Ok.
        for i in 0..limit {
            let result = tracker.record_event(i);
            let is_ok_or_warn = matches!(result, CheckResult::Ok | CheckResult::Warning(_));
            prop_assert!(is_ok_or_warn, "event {} should be ok, got {:?}", i, result);
        }
        // Event limit+1 should halt.
        let result = tracker.record_event(limit);
        let is_halt = matches!(result, CheckResult::Halt(LimitViolation::MaxEvents { .. }));
        prop_assert!(is_halt, "event after limit should halt");
    }

    // ── GR-2: All events Ok within limit ────────────────────────────────

    #[test]
    fn gr2_ok_within_limit(n in 1u64..50) {
        let limits = ResourceLimits {
            max_events: n + 10, // Well above n.
            max_wall_clock_ms: 0,
            memory_warning_events: 0,
            watchdog_timeout_ms: 0,
            ..Default::default()
        };
        let tracker = ResourceTracker::new(limits, 0);
        for i in 0..n {
            let result = tracker.record_event(i);
            prop_assert_eq!(result, CheckResult::Ok);
        }
    }

    // ── GR-3: Wall-clock halt ───────────────────────────────────────────

    #[test]
    fn gr3_wall_clock_halt(limit_ms in 100u64..10000, overshoot in 1u64..5000) {
        let limits = ResourceLimits {
            max_events: 0,
            max_wall_clock_ms: limit_ms,
            memory_warning_events: 0,
            watchdog_timeout_ms: 0,
            ..Default::default()
        };
        let tracker = ResourceTracker::new(limits, 0);
        // Within limit.
        prop_assert_eq!(tracker.record_event(limit_ms / 2), CheckResult::Ok);
        // Exceed limit.
        let result = tracker.record_event(limit_ms + overshoot);
        let is_halt = matches!(result, CheckResult::Halt(LimitViolation::MaxWallClock { .. }));
        prop_assert!(is_halt);
    }

    // ── GR-4: Memory warning fires once ─────────────────────────────────

    #[test]
    fn gr4_memory_warning_once(threshold in 3u64..30) {
        let limits = ResourceLimits {
            max_events: 0,
            max_wall_clock_ms: 0,
            memory_warning_events: threshold,
            watchdog_timeout_ms: 0,
            ..Default::default()
        };
        let tracker = ResourceTracker::new(limits, 0);
        let mut warnings = 0;
        for i in 0..(threshold + 10) {
            let result = tracker.record_event(i);
            if matches!(result, CheckResult::Warning(_)) {
                warnings += 1;
            }
        }
        prop_assert_eq!(warnings, 1, "memory warning should fire exactly once");
    }

    // ── GR-5: Watchdog Ok when recent progress ──────────────────────────

    #[test]
    fn gr5_watchdog_ok(timeout_ms in 1000u64..10000) {
        let limits = ResourceLimits {
            watchdog_timeout_ms: timeout_ms,
            ..Default::default()
        };
        let tracker = ResourceTracker::new(limits, 100);
        tracker.record_event(200); // Progress at 200ms.
        let result = tracker.check_watchdog(200 + timeout_ms / 2);
        prop_assert_eq!(result, CheckResult::Ok);
    }

    // ── GR-6: Watchdog halt on stall ────────────────────────────────────

    #[test]
    fn gr6_watchdog_stall(timeout_ms in 100u64..5000, overshoot in 1u64..1000) {
        let limits = ResourceLimits {
            watchdog_timeout_ms: timeout_ms,
            max_events: 0,
            max_wall_clock_ms: 0,
            memory_warning_events: 0,
            ..Default::default()
        };
        let tracker = ResourceTracker::new(limits, 0);
        tracker.record_event(100); // Last progress at 100ms.
        let stall_time = 100 + timeout_ms + overshoot;
        let result = tracker.check_watchdog(stall_time);
        let is_halt = matches!(result, CheckResult::Halt(LimitViolation::WatchdogTimeout { .. }));
        prop_assert!(is_halt);
    }

    // ── GR-7: ConcurrencyGate allows up to max ─────────────────────────

    #[test]
    fn gr7_concurrency_allows(max in 1u32..10) {
        let gate = ConcurrencyGate::new(max);
        let mut tokens = Vec::new();
        for _ in 0..max {
            let token = gate.try_acquire();
            prop_assert!(token.is_ok());
            tokens.push(token.unwrap());
        }
        prop_assert_eq!(gate.current(), max);
    }

    // ── GR-8: ConcurrencyGate rejects excess ────────────────────────────

    #[test]
    fn gr8_concurrency_rejects(max in 1u32..5) {
        let gate = ConcurrencyGate::new(max);
        let mut tokens = Vec::new();
        for _ in 0..max {
            tokens.push(gate.try_acquire().unwrap());
        }
        let result = gate.try_acquire();
        let is_err = result.is_err();
        prop_assert!(is_err, "should reject when at capacity");
    }

    // ── GR-9: ConcurrencyGate releases on drop ─────────────────────────

    #[test]
    fn gr9_concurrency_releases(max in 1u32..5) {
        let gate = ConcurrencyGate::new(max);
        {
            let mut tokens = Vec::new();
            for _ in 0..max {
                tokens.push(gate.try_acquire().unwrap());
            }
            prop_assert_eq!(gate.current(), max);
        }
        prop_assert_eq!(gate.current(), 0);
        // Can acquire again.
        let _t = gate.try_acquire().unwrap();
        prop_assert_eq!(gate.current(), 1);
    }

    // ── GR-10: ResourceLimits serde roundtrip ────────────────────────────

    #[test]
    fn gr10_limits_serde(
        max_events in 0u64..1_000_000,
        max_wall in 0u64..100_000,
        max_concurrent in 1u32..16
    ) {
        let limits = ResourceLimits {
            max_events,
            max_wall_clock_ms: max_wall,
            max_concurrent,
            ..Default::default()
        };
        let json = serde_json::to_string(&limits).unwrap();
        let restored: ResourceLimits = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.max_events, max_events);
        prop_assert_eq!(restored.max_wall_clock_ms, max_wall);
        prop_assert_eq!(restored.max_concurrent, max_concurrent);
    }

    // ── GR-11: LimitViolation serde roundtrip ────────────────────────────

    #[test]
    fn gr11_violation_serde(limit in 1u64..10000, actual in 1u64..10000) {
        let v = LimitViolation::MaxEvents { limit, actual };
        let json = serde_json::to_string(&v).unwrap();
        let restored: LimitViolation = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, v);
    }

    // ── GR-12: GuardrailReport serde roundtrip ───────────────────────────

    #[test]
    fn gr12_report_serde(n in 1u64..50) {
        let limits = ResourceLimits {
            max_events: n + 100,
            ..Default::default()
        };
        let tracker = ResourceTracker::new(limits, 0);
        for i in 0..n {
            tracker.record_event(i);
        }
        let report = GuardrailReport::from_tracker(&tracker, true);
        let json = serde_json::to_string(&report).unwrap();
        let restored: GuardrailReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.events_processed, n);
    }

    // ── GR-13: Report is_safe when no violations ────────────────────────

    #[test]
    fn gr13_report_safe(n in 1u64..30) {
        let limits = ResourceLimits {
            max_events: 0,
            max_wall_clock_ms: 0,
            memory_warning_events: 0,
            watchdog_timeout_ms: 0,
            ..Default::default()
        };
        let tracker = ResourceTracker::new(limits, 0);
        for i in 0..n {
            tracker.record_event(i);
        }
        let report = GuardrailReport::from_tracker(&tracker, true);
        prop_assert!(report.is_safe());
    }

    // ── GR-14: Report not safe when halted ──────────────────────────────

    #[test]
    fn gr14_report_not_safe(limit in 1u64..20) {
        let limits = ResourceLimits {
            max_events: limit,
            max_wall_clock_ms: 0,
            memory_warning_events: 0,
            watchdog_timeout_ms: 0,
            ..Default::default()
        };
        let tracker = ResourceTracker::new(limits, 0);
        for i in 0..(limit + 5) {
            tracker.record_event(i);
        }
        let report = GuardrailReport::from_tracker(&tracker, true);
        prop_assert!(report.halted_by_guardrail);
    }

    // ── GR-15: Disabled limits never trigger ────────────────────────────

    #[test]
    fn gr15_disabled_limits(n in 1u64..200) {
        let limits = ResourceLimits {
            max_events: 0,
            max_wall_clock_ms: 0,
            memory_warning_events: 0,
            watchdog_timeout_ms: 0,
            ..Default::default()
        };
        let tracker = ResourceTracker::new(limits, 0);
        for i in 0..n {
            let result = tracker.record_event(i * 1000);
            prop_assert_eq!(result, CheckResult::Ok);
        }
        prop_assert!(!tracker.is_halted());
    }

    // ── GR-16: Event count matches ──────────────────────────────────────

    #[test]
    fn gr16_event_count(n in 1u64..100) {
        let limits = ResourceLimits {
            max_events: 0,
            max_wall_clock_ms: 0,
            memory_warning_events: 0,
            watchdog_timeout_ms: 0,
            ..Default::default()
        };
        let tracker = ResourceTracker::new(limits, 0);
        for i in 0..n {
            tracker.record_event(i);
        }
        prop_assert_eq!(tracker.event_count(), n);
    }

    // ── GR-17: Violations grow on breach ────────────────────────────────

    #[test]
    fn gr17_violations_grow(limit in 1u64..10) {
        let limits = ResourceLimits {
            max_events: limit,
            max_wall_clock_ms: 0,
            memory_warning_events: 0,
            watchdog_timeout_ms: 0,
            ..Default::default()
        };
        let tracker = ResourceTracker::new(limits, 0);
        for i in 0..(limit + 3) {
            tracker.record_event(i);
        }
        let violations = tracker.violations();
        prop_assert!(!violations.is_empty(), "violations should exist after exceeding limit");
    }
}
