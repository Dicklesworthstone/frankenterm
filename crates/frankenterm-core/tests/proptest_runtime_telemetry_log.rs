//! Property tests for `RuntimeTelemetryLog`, the bounded in-memory runtime
//! telemetry event buffer in `frankenterm_core::runtime_telemetry`. It had
//! no direct coverage. Pins the ring-buffer invariants: capacity bound,
//! emitted = retained + evicted conservation, strictly-increasing sequence,
//! and drain semantics.

use proptest::prelude::*;

use frankenterm_core::runtime_telemetry::{
    RuntimeTelemetryEventBuilder, RuntimeTelemetryKind, RuntimeTelemetryLog,
    RuntimeTelemetryLogConfig,
};

fn emit_n(log: &mut RuntimeTelemetryLog, n: usize) -> Vec<u64> {
    (0..n)
        .map(|_| {
            log.emit(RuntimeTelemetryEventBuilder::new(
                "test",
                RuntimeTelemetryKind::ScopeCreated,
            ))
        })
        .collect()
}

fn log_with_capacity(cap: usize) -> RuntimeTelemetryLog {
    RuntimeTelemetryLog::new(RuntimeTelemetryLogConfig {
        max_events: cap,
        ..RuntimeTelemetryLogConfig::default()
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Retained len never exceeds capacity; the total emitted count equals
    /// retained + evicted (conservation); events() agrees with len().
    #[test]
    fn telemetry_log_capacity_and_conservation(cap in 1usize..20, n in 0usize..60) {
        let mut log = log_with_capacity(cap);
        let max = log.normalized_max_events();
        emit_n(&mut log, n);
        prop_assert!(log.len() <= max, "len {} must be <= capacity {}", log.len(), max);
        prop_assert_eq!(log.total_emitted(), n as u64, "every emit must be counted");
        prop_assert_eq!(
            log.total_emitted(),
            log.len() as u64 + log.total_evicted(),
            "emitted must equal retained + evicted"
        );
        prop_assert_eq!(log.events().len(), log.len(), "events() must agree with len()");
    }

    /// Sequence numbers returned by emit are strictly increasing.
    #[test]
    fn telemetry_log_sequence_strictly_increasing(n in 1usize..40) {
        let mut log = RuntimeTelemetryLog::with_defaults();
        let seqs = emit_n(&mut log, n);
        for w in seqs.windows(2) {
            prop_assert!(w[1] > w[0], "sequence must strictly increase: {} !> {}", w[1], w[0]);
        }
    }

    /// filter_by_kind returns exactly the retained events of that kind:
    /// every result matches the requested kind, the per-kind counts equal
    /// the emissions (capacity 100 >> total, so no eviction), and the two
    /// kind-partitions sum to the full retained set.
    #[test]
    fn telemetry_log_filter_by_kind_partitions_events(a in 0usize..20, b in 0usize..20) {
        let mut log = log_with_capacity(100); // > a + b, so nothing is evicted
        for _ in 0..a {
            log.emit(RuntimeTelemetryEventBuilder::new("c", RuntimeTelemetryKind::ScopeCreated));
        }
        for _ in 0..b {
            log.emit(RuntimeTelemetryEventBuilder::new("c", RuntimeTelemetryKind::ScopeStarted));
        }
        let created = log.filter_by_kind(RuntimeTelemetryKind::ScopeCreated);
        let started = log.filter_by_kind(RuntimeTelemetryKind::ScopeStarted);
        prop_assert!(
            created.iter().all(|e| e.event_kind == RuntimeTelemetryKind::ScopeCreated),
            "filter_by_kind(ScopeCreated) must only return ScopeCreated events"
        );
        prop_assert!(
            started.iter().all(|e| e.event_kind == RuntimeTelemetryKind::ScopeStarted),
            "filter_by_kind(ScopeStarted) must only return ScopeStarted events"
        );
        prop_assert_eq!(created.len(), a);
        prop_assert_eq!(started.len(), b);
        prop_assert_eq!(created.len() + started.len(), log.len(),
            "the two kind-partitions must cover every retained event");
    }

    /// drain() returns the retained events and leaves the log empty.
    #[test]
    fn telemetry_log_drain_empties(cap in 1usize..20, n in 0usize..40) {
        let mut log = log_with_capacity(cap);
        emit_n(&mut log, n);
        let len_before = log.len();
        let drained = log.drain();
        prop_assert_eq!(drained.len(), len_before, "drain must return all retained events");
        prop_assert!(log.is_empty(), "log must be empty after drain");
        prop_assert_eq!(log.len(), 0);
    }
}
