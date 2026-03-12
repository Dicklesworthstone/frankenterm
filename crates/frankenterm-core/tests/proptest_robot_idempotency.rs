//! Property tests for robot_idempotency module (ft-3681t.4.6).
//!
//! Covers serde roundtrips, MutationKey determinism, MutationRecord expiry,
//! MutationGuard dedup semantics, capacity eviction, TTL eviction,
//! failure caching policy, batch check classification, telemetry consistency,
//! and key helper factories.

use frankenterm_core::robot_idempotency::*;
use proptest::prelude::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_action() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("send_text".to_string()),
        Just("split_pane".to_string()),
        Just("close_pane".to_string()),
        Just("event_annotate".to_string()),
        Just("workflow_run".to_string()),
        Just("agent_configure".to_string()),
    ]
}

fn arb_fingerprint() -> impl Strategy<Value = String> {
    "[a-z0-9|]{1,32}"
}

// =============================================================================
// Serde roundtrips
// =============================================================================

proptest! {
    #[test]
    fn serde_roundtrip_mutation_key(
        action in arb_action(),
        fingerprint in arb_fingerprint(),
    ) {
        let key = MutationKey::derive(&action, &fingerprint);
        let json = serde_json::to_string(&key).unwrap();
        let back: MutationKey = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(key, back);
    }

    #[test]
    fn serde_roundtrip_mutation_record_success(
        action in arb_action(),
        elapsed in 0..10_000u64,
        now in 0..1_000_000u64,
    ) {
        let key = MutationKey::derive(&action, "test");
        let record = MutationRecord::success(key, Some("payload".into()), elapsed, now);
        let json = serde_json::to_string(&record).unwrap();
        let back: MutationRecord = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.success, true);
        prop_assert_eq!(back.elapsed_ms, elapsed);
        prop_assert_eq!(back.created_at_ms, now);
    }

    #[test]
    fn serde_roundtrip_mutation_record_failure(
        action in arb_action(),
        elapsed in 0..10_000u64,
        now in 0..1_000_000u64,
    ) {
        let key = MutationKey::derive(&action, "test");
        let record = MutationRecord::failure(key, "error".into(), elapsed, now);
        let json = serde_json::to_string(&record).unwrap();
        let back: MutationRecord = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.success, false);
        prop_assert_eq!(back.error_message.as_deref(), Some("error"));
    }

    #[test]
    fn serde_roundtrip_mutation_outcome_executed(_dummy in 0..1u32) {
        let outcome = MutationOutcome::Executed { key: "rk:test".into() };
        let json = serde_json::to_string(&outcome).unwrap();
        let back: MutationOutcome = serde_json::from_str(&json).unwrap();
        let is_first = back.is_first_execution();
        prop_assert!(is_first);
    }

    #[test]
    fn serde_roundtrip_mutation_outcome_dedup(
        count in 2..100u64,
        success in any::<bool>(),
    ) {
        let outcome = MutationOutcome::Deduplicated {
            key: "rk:test".into(),
            submission_count: count,
            original_success: success,
        };
        let json = serde_json::to_string(&outcome).unwrap();
        let back: MutationOutcome = serde_json::from_str(&json).unwrap();
        let is_dedup = back.is_deduplicated();
        prop_assert!(is_dedup);
    }

    #[test]
    fn serde_roundtrip_guard_telemetry(_dummy in 0..1u32) {
        let telem = GuardTelemetry::default();
        let json = serde_json::to_string(&telem).unwrap();
        let back: GuardTelemetry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.mutations_recorded, 0);
        prop_assert_eq!(back.deduplications, 0);
    }

    #[test]
    fn serde_roundtrip_guard_config(_dummy in 0..1u32) {
        let config = MutationGuardConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let back: MutationGuardConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.capacity, config.capacity);
        prop_assert_eq!(back.ttl_ms, config.ttl_ms);
        prop_assert_eq!(back.cache_failures, config.cache_failures);
    }

    #[test]
    fn serde_roundtrip_batch_check_result(_dummy in 0..1u32) {
        let result = BatchCheckResult {
            new_keys: vec!["a".into()],
            dedup_keys: vec!["b".into()],
            expired_keys: vec!["c".into()],
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: BatchCheckResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.new_keys.len(), 1);
        prop_assert_eq!(back.dedup_keys.len(), 1);
        prop_assert_eq!(back.expired_keys.len(), 1);
    }
}

// =============================================================================
// MutationKey determinism
// =============================================================================

proptest! {
    #[test]
    fn key_derive_is_deterministic(
        action in arb_action(),
        fingerprint in arb_fingerprint(),
    ) {
        let k1 = MutationKey::derive(&action, &fingerprint);
        let k2 = MutationKey::derive(&action, &fingerprint);
        prop_assert_eq!(k1, k2);
    }

    #[test]
    fn key_derive_starts_with_rk_prefix(
        action in arb_action(),
        fingerprint in arb_fingerprint(),
    ) {
        let key = MutationKey::derive(&action, &fingerprint);
        prop_assert!(key.as_str().starts_with("rk:"));
    }

    #[test]
    fn key_display_matches_as_str(
        action in arb_action(),
        fingerprint in arb_fingerprint(),
    ) {
        let key = MutationKey::derive(&action, &fingerprint);
        prop_assert_eq!(format!("{key}"), key.as_str());
    }

    #[test]
    fn key_action_preserved(
        action in arb_action(),
        fingerprint in arb_fingerprint(),
    ) {
        let key = MutationKey::derive(&action, &fingerprint);
        prop_assert_eq!(key.action(), action.as_str());
    }

    #[test]
    fn key_from_client_preserves_value(
        action in arb_action(),
        client_key in "[a-z0-9-]{1,32}",
    ) {
        let key = MutationKey::from_client(&action, &client_key);
        prop_assert_eq!(key.as_str(), client_key.as_str());
        prop_assert_eq!(key.action(), action.as_str());
    }

    #[test]
    fn different_actions_produce_different_keys(
        fingerprint in arb_fingerprint(),
    ) {
        let k1 = MutationKey::derive("send_text", &fingerprint);
        let k2 = MutationKey::derive("split_pane", &fingerprint);
        prop_assert_ne!(k1, k2);
    }

    #[test]
    fn different_fingerprints_produce_different_keys(
        action in arb_action(),
        fp1 in "[a-z]{1,8}",
        fp2 in "[A-Z]{1,8}",
    ) {
        // Different case guarantees different fingerprints
        let k1 = MutationKey::derive(&action, &fp1);
        let k2 = MutationKey::derive(&action, &fp2);
        prop_assert_ne!(k1, k2);
    }
}

// =============================================================================
// MutationRecord expiry
// =============================================================================

proptest! {
    #[test]
    fn record_not_expired_within_ttl(
        created in 0..500_000u64,
        ttl in 1..100_000u64,
        offset in 0..100_000u64,
    ) {
        let key = MutationKey::derive("test", "data");
        let record = MutationRecord::success(key, None, 1, created);
        // Check at created + offset where offset <= ttl
        let check_time = created + offset.min(ttl);
        prop_assert!(!record.is_expired(check_time, ttl));
    }

    #[test]
    fn record_expired_past_ttl(
        created in 0..500_000u64,
        ttl in 1..100_000u64,
        extra in 1..100_000u64,
    ) {
        let key = MutationKey::derive("test", "data");
        let record = MutationRecord::success(key, None, 1, created);
        let check_time = created + ttl + extra;
        prop_assert!(record.is_expired(check_time, ttl));
    }

    #[test]
    fn success_record_fields(
        elapsed in 0..10_000u64,
        now in 0..1_000_000u64,
    ) {
        let key = MutationKey::derive("test", "data");
        let record = MutationRecord::success(key, Some("payload".into()), elapsed, now);
        prop_assert!(record.success);
        prop_assert_eq!(record.submission_count, 1);
        prop_assert!(record.error_message.is_none());
    }

    #[test]
    fn failure_record_fields(
        elapsed in 0..10_000u64,
        now in 0..1_000_000u64,
    ) {
        let key = MutationKey::derive("test", "data");
        let record = MutationRecord::failure(key, "oops".into(), elapsed, now);
        prop_assert!(!record.success);
        prop_assert_eq!(record.submission_count, 1);
        prop_assert!(record.response_payload.is_none());
    }
}

// =============================================================================
// MutationGuard dedup semantics
// =============================================================================

proptest! {
    #[test]
    fn first_submission_is_executed(
        action in arb_action(),
        fingerprint in arb_fingerprint(),
    ) {
        let mut guard = MutationGuard::with_defaults();
        let key = MutationKey::derive(&action, &fingerprint);
        let outcome = guard.record(key, true, None, None, 1, 1000);
        let is_first = outcome.is_first_execution();
        prop_assert!(is_first);
        prop_assert_eq!(guard.len(), 1);
    }

    #[test]
    fn second_submission_is_deduplicated(
        action in arb_action(),
        fingerprint in arb_fingerprint(),
    ) {
        let mut guard = MutationGuard::with_defaults();
        let key = MutationKey::derive(&action, &fingerprint);
        let _ = guard.record(key.clone(), true, None, None, 1, 1000);
        let outcome = guard.record(key, true, None, None, 1, 1001);
        let is_dedup = outcome.is_deduplicated();
        prop_assert!(is_dedup);
        prop_assert_eq!(guard.len(), 1);
    }

    #[test]
    fn submission_count_increments(
        n in 2..10usize,
    ) {
        let mut guard = MutationGuard::with_defaults();
        let key = MutationKey::derive("test", "data");
        let _ = guard.record(key.clone(), true, None, None, 1, 1000);
        let mut last_outcome = None;
        for i in 1..n {
            last_outcome = Some(guard.record(key.clone(), true, None, None, 1, 1000 + i as u64));
        }
        if let Some(MutationOutcome::Deduplicated { submission_count, .. }) = last_outcome {
            prop_assert_eq!(submission_count, n as u64);
        } else {
            prop_assert!(false, "expected Deduplicated");
        }
    }

    #[test]
    fn check_returns_none_for_unknown_key(
        action in arb_action(),
        fingerprint in arb_fingerprint(),
    ) {
        let mut guard = MutationGuard::with_defaults();
        let key = MutationKey::derive(&action, &fingerprint);
        prop_assert!(guard.check(&key, 1000).is_none());
    }

    #[test]
    fn check_returns_record_for_known_key(
        action in arb_action(),
    ) {
        let mut guard = MutationGuard::with_defaults();
        let key = MutationKey::derive(&action, "data");
        let _ = guard.record(key.clone(), true, Some("ok".into()), None, 5, 1000);
        let record = guard.check(&key, 1001);
        prop_assert!(record.is_some());
        prop_assert!(record.unwrap().success);
    }

    #[test]
    fn expired_key_returns_none_on_check(_dummy in 0..1u32) {
        let config = MutationGuardConfig {
            ttl_ms: 100,
            ..Default::default()
        };
        let mut guard = MutationGuard::new(config);
        let key = MutationKey::derive("test", "data");
        let _ = guard.record(key.clone(), true, None, None, 1, 1000);
        prop_assert!(guard.check(&key, 1200).is_none());
    }

    #[test]
    fn expired_key_re_executes(_dummy in 0..1u32) {
        let config = MutationGuardConfig {
            ttl_ms: 100,
            ..Default::default()
        };
        let mut guard = MutationGuard::new(config);
        let key = MutationKey::derive("test", "data");
        let _ = guard.record(key.clone(), true, None, None, 1, 1000);
        let outcome = guard.record(key, true, None, None, 1, 1200);
        let is_first = outcome.is_first_execution();
        prop_assert!(is_first);
    }
}

// =============================================================================
// Capacity eviction
// =============================================================================

proptest! {
    #[test]
    fn capacity_limits_record_count(capacity in 2..20usize) {
        let config = MutationGuardConfig {
            capacity,
            ttl_ms: 60_000,
            cache_failures: false,
        };
        let mut guard = MutationGuard::new(config);
        for i in 0..(capacity * 2) {
            let key = MutationKey::derive("test", &format!("{i}"));
            let _ = guard.record(key, true, None, None, 1, 1000 + i as u64);
        }
        prop_assert_eq!(guard.len(), capacity);
    }

    #[test]
    fn eviction_removes_oldest_first(capacity in 3..10usize) {
        let config = MutationGuardConfig {
            capacity,
            ttl_ms: 60_000,
            cache_failures: false,
        };
        let mut guard = MutationGuard::new(config);
        for i in 0..(capacity + 2) {
            let key = MutationKey::derive("test", &format!("{i}"));
            let _ = guard.record(key, true, None, None, 1, 1000 + i as u64);
        }
        // Oldest 2 should be evicted
        let k0 = MutationKey::derive("test", "0");
        let k1 = MutationKey::derive("test", "1");
        prop_assert!(guard.get_record(k0.as_str()).is_none());
        prop_assert!(guard.get_record(k1.as_str()).is_none());
        // Newest should remain
        let last = MutationKey::derive("test", &format!("{}", capacity + 1));
        prop_assert!(guard.get_record(last.as_str()).is_some());
    }
}

// =============================================================================
// Failure caching policy
// =============================================================================

proptest! {
    #[test]
    fn failures_not_cached_by_default(
        action in arb_action(),
    ) {
        let mut guard = MutationGuard::with_defaults();
        let key = MutationKey::derive(&action, "fail");
        let _ = guard.record(key, false, None, Some("error".into()), 1, 1000);
        prop_assert_eq!(guard.len(), 0);
    }

    #[test]
    fn failures_cached_when_configured(
        action in arb_action(),
    ) {
        let config = MutationGuardConfig {
            cache_failures: true,
            ..Default::default()
        };
        let mut guard = MutationGuard::new(config);
        let key = MutationKey::derive(&action, "fail");
        let _ = guard.record(key.clone(), false, None, Some("error".into()), 1, 1000);
        prop_assert_eq!(guard.len(), 1);
        // Retry deduplicates
        let outcome = guard.record(key, true, None, None, 1, 1001);
        let is_dedup = outcome.is_deduplicated();
        prop_assert!(is_dedup);
    }

    #[test]
    fn uncached_failure_allows_retry(
        action in arb_action(),
    ) {
        let mut guard = MutationGuard::with_defaults();
        let key = MutationKey::derive(&action, "retry");
        let _ = guard.record(key.clone(), false, None, Some("err".into()), 1, 1000);
        let outcome = guard.record(key, true, Some("ok".into()), None, 1, 1001);
        let is_first = outcome.is_first_execution();
        prop_assert!(is_first);
        prop_assert_eq!(guard.len(), 1);
    }
}

// =============================================================================
// Telemetry consistency
// =============================================================================

proptest! {
    #[test]
    fn telemetry_mutations_recorded_matches_unique_keys(n in 1..15usize) {
        let mut guard = MutationGuard::with_defaults();
        for i in 0..n {
            let key = MutationKey::derive("test", &format!("{i}"));
            let _ = guard.record(key, true, None, None, 1, 1000 + i as u64);
        }
        prop_assert_eq!(guard.telemetry().mutations_recorded, n as u64);
        prop_assert_eq!(guard.telemetry().active_records, n as u64);
    }

    #[test]
    fn telemetry_deduplications_match_replays(replays in 1..10usize) {
        let mut guard = MutationGuard::with_defaults();
        let key = MutationKey::derive("test", "data");
        let _ = guard.record(key.clone(), true, None, None, 1, 1000);
        for i in 0..replays {
            let _ = guard.record(key.clone(), true, None, None, 1, 1001 + i as u64);
        }
        prop_assert_eq!(guard.telemetry().mutations_recorded, 1);
        prop_assert_eq!(guard.telemetry().deduplications, replays as u64);
    }

    #[test]
    fn telemetry_failures_tracked(_dummy in 0..1u32) {
        let mut guard = MutationGuard::with_defaults();
        let k1 = MutationKey::derive("test", "fail1");
        let k2 = MutationKey::derive("test", "fail2");
        let _ = guard.record(k1, false, None, Some("err".into()), 1, 1000);
        let _ = guard.record(k2, false, None, Some("err".into()), 1, 1001);
        prop_assert_eq!(guard.telemetry().failures_seen, 2);
    }

    #[test]
    fn new_guard_has_zero_telemetry(_dummy in 0..1u32) {
        let guard = MutationGuard::with_defaults();
        let t = guard.telemetry();
        prop_assert_eq!(t.mutations_recorded, 0);
        prop_assert_eq!(t.deduplications, 0);
        prop_assert_eq!(t.evictions, 0);
        prop_assert_eq!(t.failures_seen, 0);
        prop_assert_eq!(t.active_records, 0);
    }
}

// =============================================================================
// Batch check
// =============================================================================

proptest! {
    #[test]
    fn batch_check_classifies_all_keys(_dummy in 0..1u32) {
        let config = MutationGuardConfig {
            ttl_ms: 100,
            ..Default::default()
        };
        let mut guard = MutationGuard::new(config);

        let k_active = MutationKey::derive("t", "active");
        let k_expired = MutationKey::derive("t", "expired");
        let k_new = MutationKey::derive("t", "new");

        let _ = guard.record(k_active.clone(), true, None, None, 1, 1000);
        let _ = guard.record(k_expired.clone(), true, None, None, 1, 800);

        let result = guard.check_batch(&[k_active, k_expired, k_new], 1050);
        let total = result.new_keys.len() + result.dedup_keys.len() + result.expired_keys.len();
        prop_assert_eq!(total, 3);
    }
}

// =============================================================================
// Key helpers
// =============================================================================

proptest! {
    #[test]
    fn send_text_key_has_correct_action(
        pane_id in 0..1000u64,
        text in "[a-z]{1,20}",
    ) {
        let key = send_text_key(pane_id, &text);
        prop_assert_eq!(key.action(), "send_text");
    }

    #[test]
    fn split_pane_key_has_correct_action(
        pane_id in 0..1000u64,
    ) {
        let key = split_pane_key(pane_id, "horizontal");
        prop_assert_eq!(key.action(), "split_pane");
    }

    #[test]
    fn close_pane_key_has_correct_action(
        pane_id in 0..1000u64,
    ) {
        let key = close_pane_key(pane_id);
        prop_assert_eq!(key.action(), "close_pane");
    }

    #[test]
    fn send_text_key_deterministic(
        pane_id in 0..1000u64,
        text in "[a-z]{1,20}",
    ) {
        let k1 = send_text_key(pane_id, &text);
        let k2 = send_text_key(pane_id, &text);
        prop_assert_eq!(k1, k2);
    }

    #[test]
    fn send_text_key_varies_by_pane(
        text in "[a-z]{1,20}",
    ) {
        let k1 = send_text_key(1, &text);
        let k2 = send_text_key(2, &text);
        prop_assert_ne!(k1, k2);
    }
}

// =============================================================================
// MutationOutcome predicates
// =============================================================================

proptest! {
    #[test]
    fn outcome_executed_predicates(_dummy in 0..1u32) {
        let outcome = MutationOutcome::Executed { key: "rk:test".into() };
        let is_first = outcome.is_first_execution();
        let is_dedup = outcome.is_deduplicated();
        prop_assert!(is_first);
        prop_assert!(!is_dedup);
        prop_assert_eq!(outcome.key(), "rk:test");
    }

    #[test]
    fn outcome_dedup_predicates(count in 2..100u64) {
        let outcome = MutationOutcome::Deduplicated {
            key: "rk:abc".into(),
            submission_count: count,
            original_success: true,
        };
        let is_first = outcome.is_first_execution();
        let is_dedup = outcome.is_deduplicated();
        prop_assert!(!is_first);
        prop_assert!(is_dedup);
        prop_assert_eq!(outcome.key(), "rk:abc");
    }
}

// =============================================================================
// Standard factory invariants
// =============================================================================

#[test]
fn default_config_has_reasonable_values() {
    let config = MutationGuardConfig::default();
    assert!(config.capacity > 0);
    assert!(config.ttl_ms > 0);
    assert!(!config.cache_failures);
}

#[test]
fn new_guard_is_empty() {
    let guard = MutationGuard::with_defaults();
    assert!(guard.is_empty());
    assert_eq!(guard.len(), 0);
}

#[test]
fn evict_expired_cleans_old_records() {
    let config = MutationGuardConfig {
        ttl_ms: 100,
        ..Default::default()
    };
    let mut guard = MutationGuard::new(config);
    let _ = guard.record(MutationKey::derive("t", "1"), true, None, None, 1, 1000);
    let _ = guard.record(MutationKey::derive("t", "2"), true, None, None, 1, 1050);
    let _ = guard.record(MutationKey::derive("t", "3"), true, None, None, 1, 1200);
    assert_eq!(guard.len(), 3);

    guard.evict_expired(1150);
    assert_eq!(guard.len(), 2);

    guard.evict_expired(1301);
    assert_eq!(guard.len(), 1);
}
