//! Property-based tests for the connector reliability module.
//!
//! Tests cover error classification invariants, DLQ capacity/eviction bounds,
//! entry lifecycle properties, controller integration, and serde roundtrips.

use std::time::Duration;

use proptest::prelude::*;

use frankenterm_core::connector_outbound_bridge::ConnectorAction;
use frankenterm_core::connector_outbound_bridge::ConnectorActionKind;
use frankenterm_core::connector_reliability::{
    ConnectorCircuitConfig, ConnectorErrorKind, ConnectorReliabilityConfig,
    ConnectorReliabilityController, ConnectorReliabilitySnapshot, DeadLetterEntry, DeadLetterQueue,
    DeadLetterQueueConfig, DeadLetterTelemetrySnapshot, ReliabilityRegistry, ReplayPlan,
    ReplayPolicy, ReplayResult, classify_connector_error,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_error_kind() -> impl Strategy<Value = ConnectorErrorKind> {
    prop_oneof![
        Just(ConnectorErrorKind::Transient),
        Just(ConnectorErrorKind::RateLimited),
        Just(ConnectorErrorKind::AuthFailure),
        Just(ConnectorErrorKind::Permanent),
        Just(ConnectorErrorKind::ServiceUnavailable),
        Just(ConnectorErrorKind::Timeout),
        Just(ConnectorErrorKind::Unknown),
    ]
}

fn arb_non_empty_string() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9_]{1,15}".prop_map(String::from)
}

// =============================================================================
// Error classification properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn error_kind_serde_roundtrip(kind in arb_error_kind()) {
        let json = serde_json::to_string(&kind).unwrap();
        let back: ConnectorErrorKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(kind, back);
    }

    #[test]
    fn error_kind_as_str_nonempty(kind in arb_error_kind()) {
        prop_assert!(!kind.as_str().is_empty());
    }

    #[test]
    fn error_kind_display_matches_as_str(kind in arb_error_kind()) {
        prop_assert_eq!(format!("{kind}"), kind.as_str());
    }

    #[test]
    fn retryable_is_superset_of_trips_breaker(kind in arb_error_kind()) {
        // If it trips the breaker, it must be retryable
        if kind.trips_breaker() {
            prop_assert!(kind.is_retryable(),
                "{} trips breaker but is not retryable", kind);
        }
    }

    #[test]
    fn permanent_and_auth_are_not_retryable(_unused in 0..1u8) {
        prop_assert!(!ConnectorErrorKind::Permanent.is_retryable());
        prop_assert!(!ConnectorErrorKind::AuthFailure.is_retryable());
    }

    #[test]
    fn classify_rate_limit_messages(
        prefix in "[A-Z]{0,5}",
    ) {
        let msg = format!("{prefix} rate limit exceeded");
        let kind = classify_connector_error(&msg);
        prop_assert_eq!(kind, ConnectorErrorKind::RateLimited);
    }

    #[test]
    fn classify_timeout_messages(
        secs in 1u32..120,
    ) {
        let msg = format!("request timed out after {secs}s");
        let kind = classify_connector_error(&msg);
        prop_assert_eq!(kind, ConnectorErrorKind::Timeout);
    }
}

// =============================================================================
// Dead-letter queue properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn dlq_never_exceeds_max_entries(
        max_entries in 1usize..20,
        n_inserts in 1usize..50,
    ) {
        let config = DeadLetterQueueConfig {
            max_entries,
            max_age_ms: u64::MAX,
            max_retries: 100,
        };
        let mut dlq = DeadLetterQueue::new(config);

        for i in 0..n_inserts {
            let action = ConnectorAction {
                target_connector: "test".to_string(),
                action_kind: ConnectorActionKind::Notify,
                correlation_id: format!("corr-{i}"),
                params: serde_json::json!({}),
                created_at_ms: i as u64 * 100,
            };
            dlq.enqueue(action, format!("error-{i}"), ConnectorErrorKind::Transient, i as u64 * 100);
        }

        let snap = dlq.telemetry_snapshot();
        prop_assert!(snap.current_depth as usize <= max_entries,
            "depth {} exceeded max {}", snap.current_depth, max_entries);
        prop_assert_eq!(snap.total_enqueued, n_inserts as u64);
    }

    #[test]
    fn dlq_discard_reduces_depth(
        n_entries in 1usize..10,
    ) {
        let mut dlq = DeadLetterQueue::new(DeadLetterQueueConfig::default());
        let mut ids = Vec::new();

        for i in 0..n_entries {
            let action = ConnectorAction {
                target_connector: "test".to_string(),
                action_kind: ConnectorActionKind::Notify,
                correlation_id: format!("corr-{i}"),
                params: serde_json::json!({}),
                created_at_ms: 1000,
            };
            let id = dlq.enqueue(action, "err", ConnectorErrorKind::Transient, 1000);
            ids.push(id);
        }

        let depth_before = dlq.depth();
        prop_assert_eq!(depth_before, n_entries);

        // Discard the first entry
        let discarded = dlq.discard(ids[0]);
        prop_assert!(discarded);
        prop_assert_eq!(dlq.depth(), n_entries - 1);
    }

    #[test]
    fn dlq_remove_returns_entry_and_decrements(
        n_entries in 2usize..8,
    ) {
        let mut dlq = DeadLetterQueue::new(DeadLetterQueueConfig::default());
        let mut ids = Vec::new();

        for i in 0..n_entries {
            let action = ConnectorAction {
                target_connector: "test".to_string(),
                action_kind: ConnectorActionKind::Notify,
                correlation_id: format!("corr-{i}"),
                params: serde_json::json!({}),
                created_at_ms: 1000,
            };
            let id = dlq.enqueue(action, "err", ConnectorErrorKind::Transient, 1000);
            ids.push(id);
        }

        let removed = dlq.remove(ids[0]);
        prop_assert!(removed.is_some());
        prop_assert_eq!(dlq.depth(), n_entries - 1);

        // Double remove returns None
        let double = dlq.remove(ids[0]);
        prop_assert!(double.is_none());
    }

    #[test]
    fn dlq_purge_respects_age_limit(
        max_age_ms in 1000u64..10000,
        n_entries in 1usize..10,
    ) {
        let config = DeadLetterQueueConfig {
            max_age_ms,
            max_retries: 100,
            max_entries: 100,
        };
        let mut dlq = DeadLetterQueue::new(config);

        // Add entries at time=0
        for i in 0..n_entries {
            let action = ConnectorAction {
                target_connector: "test".to_string(),
                action_kind: ConnectorActionKind::Notify,
                correlation_id: format!("corr-{i}"),
                params: serde_json::json!({}),
                created_at_ms: 0,
            };
            dlq.enqueue(action, "err", ConnectorErrorKind::Transient, 0);
        }

        // Purge at time > max_age should remove all
        let purged = dlq.purge_expired(max_age_ms + 1);
        prop_assert_eq!(purged, n_entries);
    }

    #[test]
    fn dlq_replayable_excludes_non_retryable(
        n_retryable in 1usize..5,
        n_permanent in 1usize..5,
    ) {
        let mut dlq = DeadLetterQueue::new(DeadLetterQueueConfig::default());

        for i in 0..n_retryable {
            let action = ConnectorAction {
                target_connector: "test".to_string(),
                action_kind: ConnectorActionKind::Notify,
                correlation_id: format!("retry-{i}"),
                params: serde_json::json!({}),
                created_at_ms: 1000,
            };
            dlq.enqueue(action, "timeout", ConnectorErrorKind::Timeout, 1000);
        }

        for i in 0..n_permanent {
            let action = ConnectorAction {
                target_connector: "test".to_string(),
                action_kind: ConnectorActionKind::Ticket,
                correlation_id: format!("perm-{i}"),
                params: serde_json::json!({}),
                created_at_ms: 1000,
            };
            dlq.enqueue(action, "not found", ConnectorErrorKind::Permanent, 1000);
        }

        let replayable = dlq.replayable_entries(2000);
        prop_assert_eq!(replayable.len(), n_retryable);
    }
}

// =============================================================================
// Dead-letter entry properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn entry_age_is_monotonic(
        first_ts in 0u64..1_000_000,
        now_delta in 0u64..1_000_000,
    ) {
        let action = ConnectorAction {
            target_connector: "test".to_string(),
            action_kind: ConnectorActionKind::Notify,
            correlation_id: "test".to_string(),
            params: serde_json::json!({}),
            created_at_ms: first_ts,
        };
        let entry = DeadLetterEntry::new(1, action, "err", ConnectorErrorKind::Transient, first_ts);
        let now = first_ts + now_delta;
        prop_assert_eq!(entry.age_ms(now), now_delta);
    }

    #[test]
    fn entry_retry_increments_count(
        n_retries in 1u32..20,
    ) {
        let action = ConnectorAction {
            target_connector: "test".to_string(),
            action_kind: ConnectorActionKind::Notify,
            correlation_id: "test".to_string(),
            params: serde_json::json!({}),
            created_at_ms: 1000,
        };
        let mut entry = DeadLetterEntry::new(1, action, "err", ConnectorErrorKind::Transient, 1000);

        for i in 0..n_retries {
            entry.record_retry_failure(
                format!("err-{i}"),
                ConnectorErrorKind::Transient,
                2000 + i as u64 * 100,
            );
        }

        // attempt_count = 1 (initial) + n_retries
        prop_assert_eq!(entry.attempt_count, 1 + n_retries);
    }

    #[test]
    fn entry_exceeded_max_retries(
        attempts in 1u32..20,
        threshold in 1u32..20,
    ) {
        let action = ConnectorAction {
            target_connector: "test".to_string(),
            action_kind: ConnectorActionKind::Notify,
            correlation_id: "test".to_string(),
            params: serde_json::json!({}),
            created_at_ms: 1000,
        };
        let mut entry = DeadLetterEntry::new(1, action, "err", ConnectorErrorKind::Transient, 1000);

        // Simulate attempts-1 retries (total = attempts)
        for _ in 1..attempts {
            entry.record_retry_failure("err", ConnectorErrorKind::Transient, 2000);
        }

        let exceeded = entry.exceeded_max_retries(threshold);
        prop_assert_eq!(exceeded, attempts >= threshold);
    }

    #[test]
    fn entry_serde_roundtrip(
        kind in arb_error_kind(),
        ts in 1u64..u64::MAX,
    ) {
        let action = ConnectorAction {
            target_connector: "test".to_string(),
            action_kind: ConnectorActionKind::Notify,
            correlation_id: "test".to_string(),
            params: serde_json::json!({}),
            created_at_ms: ts,
        };
        let entry = DeadLetterEntry::new(42, action, "test error", kind, ts);
        let json = serde_json::to_string(&entry).unwrap();
        let back: DeadLetterEntry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(entry.id, back.id);
        prop_assert_eq!(entry.error_kind, back.error_kind);
        prop_assert_eq!(entry.attempt_count, back.attempt_count);
        prop_assert_eq!(entry.first_failed_at_ms, back.first_failed_at_ms);
    }
}

// =============================================================================
// Controller properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn controller_telemetry_tracks_operations(
        n_success in 0usize..10,
        n_fail in 0usize..10,
    ) {
        let mut ctrl = ConnectorReliabilityController::new(
            "test",
            ConnectorReliabilityConfig::default(),
        );

        for _ in 0..n_success {
            ctrl.allow_operation();
            ctrl.record_success();
        }

        let action = ConnectorAction {
            target_connector: "test".to_string(),
            action_kind: ConnectorActionKind::Notify,
            correlation_id: "test".to_string(),
            params: serde_json::json!({}),
            created_at_ms: 1000,
        };
        for _ in 0..n_fail {
            ctrl.allow_operation();
            ctrl.record_failure(&action, "err", ConnectorErrorKind::Transient, 1000);
        }

        let snap = ctrl.telemetry_snapshot();
        prop_assert_eq!(snap.operations_attempted as usize, n_success + n_fail);
        prop_assert_eq!(snap.operations_succeeded as usize, n_success);
        prop_assert_eq!(snap.operations_failed as usize, n_fail);
    }

    #[test]
    fn controller_only_enqueues_retryable(kind in arb_error_kind()) {
        let mut ctrl = ConnectorReliabilityController::new(
            "test",
            ConnectorReliabilityConfig::default(),
        );

        let action = ConnectorAction {
            target_connector: "test".to_string(),
            action_kind: ConnectorActionKind::Notify,
            correlation_id: "test".to_string(),
            params: serde_json::json!({}),
            created_at_ms: 1000,
        };

        let dlq_id = ctrl.record_failure(&action, "err", kind, 1000);

        if kind.is_retryable() {
            prop_assert!(dlq_id.is_some(), "{} is retryable but was not enqueued", kind);
        } else {
            prop_assert!(dlq_id.is_none(), "{} is not retryable but was enqueued", kind);
        }
    }

    #[test]
    fn controller_replay_plan_bounded(
        n_entries in 0usize..20,
        batch_size in 1usize..10,
    ) {
        let mut ctrl = ConnectorReliabilityController::new(
            "test",
            ConnectorReliabilityConfig::default(),
        );

        let action = ConnectorAction {
            target_connector: "test".to_string(),
            action_kind: ConnectorActionKind::Notify,
            correlation_id: "test".to_string(),
            params: serde_json::json!({}),
            created_at_ms: 1000,
        };

        for _ in 0..n_entries {
            ctrl.record_failure(&action, "timeout", ConnectorErrorKind::Timeout, 1000);
        }

        let plan = ctrl.build_replay_plan(2000, batch_size, false);
        prop_assert!(plan.entry_ids.len() <= batch_size);
        prop_assert!(plan.entry_ids.len() <= n_entries);
    }
}

// =============================================================================
// Registry properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn registry_idempotent_get_or_create(
        connector in arb_non_empty_string(),
    ) {
        let mut registry = ReliabilityRegistry::new(ConnectorReliabilityConfig::default());

        registry.get_or_create(&connector);
        registry.get_or_create(&connector);

        prop_assert_eq!(registry.connector_ids().len(), 1);
    }

    #[test]
    fn registry_total_depth_is_sum(
        n_connectors in 1usize..5,
        n_failures in 1usize..5,
    ) {
        let mut registry = ReliabilityRegistry::new(ConnectorReliabilityConfig::default());

        for c in 0..n_connectors {
            let connector_id = format!("conn-{c}");
            let action = ConnectorAction {
                target_connector: connector_id.clone(),
                action_kind: ConnectorActionKind::Notify,
                correlation_id: "test".to_string(),
                params: serde_json::json!({}),
                created_at_ms: 1000,
            };
            for _ in 0..n_failures {
                registry.get_or_create(&connector_id).record_failure(
                    &action, "err", ConnectorErrorKind::Transient, 1000,
                );
            }
        }

        prop_assert_eq!(registry.total_dlq_depth(), n_connectors * n_failures);
    }

    #[test]
    fn registry_all_snapshots_matches_count(
        n_connectors in 1usize..8,
    ) {
        let mut registry = ReliabilityRegistry::new(ConnectorReliabilityConfig::default());

        for c in 0..n_connectors {
            registry.get_or_create(&format!("conn-{c}"));
        }

        let snapshots = registry.all_snapshots();
        prop_assert_eq!(snapshots.len(), n_connectors);
    }
}

// =============================================================================
// Serde roundtrip properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn reliability_snapshot_serde_roundtrip(
        attempted in 0u64..1000,
        succeeded in 0u64..1000,
        failed in 0u64..1000,
        rejections in 0u64..100,
    ) {
        let snap = ConnectorReliabilitySnapshot {
            connector_id: "test".to_string(),
            operations_attempted: attempted,
            operations_succeeded: succeeded,
            operations_failed: failed,
            circuit_rejections: rejections,
            dlq: DeadLetterTelemetrySnapshot {
                total_enqueued: failed,
                current_depth: 0,
                replayed_ok: 0,
                retry_attempts: 0,
                evictions: 0,
                discarded: 0,
                purged: 0,
            },
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: ConnectorReliabilitySnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap, back);
    }

    #[test]
    fn replay_plan_serde_roundtrip(
        n_ids in 0usize..10,
        batch_size in 1usize..20,
        stop in proptest::bool::ANY,
    ) {
        let plan = ReplayPlan {
            entry_ids: (0..n_ids as u64).collect(),
            policy: ReplayPolicy::default(),
            batch_size,
            stop_on_failure: stop,
        };
        let json = serde_json::to_string(&plan).unwrap();
        let back: ReplayPlan = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(plan.entry_ids.len(), back.entry_ids.len());
        prop_assert_eq!(plan.batch_size, back.batch_size);
        prop_assert_eq!(plan.stop_on_failure, back.stop_on_failure);
    }

    #[test]
    fn replay_result_serde_roundtrip(
        succeeded in 0usize..100,
        failed in 0usize..100,
        skipped in 0usize..100,
    ) {
        let result = ReplayResult {
            succeeded,
            failed,
            skipped,
            failed_ids: (0..failed as u64).collect(),
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: ReplayResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(result.succeeded, back.succeeded);
        prop_assert_eq!(result.failed, back.failed);
        prop_assert_eq!(result.skipped, back.skipped);
    }
}

// =============================================================================
// Circuit config presets
// =============================================================================

#[test]
fn circuit_config_presets_are_valid() {
    let default = ConnectorCircuitConfig::default();
    assert!(default.failure_threshold > 0);
    assert!(default.success_threshold > 0);
    assert!(default.cooldown > Duration::ZERO);

    let critical = ConnectorCircuitConfig::critical();
    assert!(critical.failure_threshold < default.failure_threshold);
    assert!(critical.cooldown < default.cooldown);

    let lenient = ConnectorCircuitConfig::lenient();
    assert!(lenient.failure_threshold > default.failure_threshold);
    assert!(lenient.cooldown > default.cooldown);
}

#[test]
fn replay_result_empty() {
    let r = ReplayResult::empty();
    assert_eq!(r.succeeded, 0);
    assert_eq!(r.failed, 0);
    assert_eq!(r.skipped, 0);
    assert!(r.failed_ids.is_empty());
}
