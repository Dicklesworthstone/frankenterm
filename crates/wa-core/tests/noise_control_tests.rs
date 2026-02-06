//! Tests for dedupe/cooldown/mute correctness and determinism (wa-upg.8.5).
//!
//! Categories:
//! 1. Mute storage CRUD (add, remove, query, upsert, expiry)
//! 2. Mute determinism (idempotent upsert, scope handling)
//! 3. Mute-notification pipeline integration
//! 4. Dedup edge cases (window boundary, concurrent keys)
//! 5. Cooldown edge cases (boundary precision, suppressed count accuracy)
//! 6. NotificationGate composite behavior

use std::time::Duration;
use wa_core::events::{
    CooldownVerdict, DedupeVerdict, EventDeduplicator, EventFilter, NotificationCooldown,
    NotificationGate, NotifyDecision,
};
use wa_core::patterns::{AgentType, Detection, Severity};
use wa_core::storage::{EventMuteRecord, StorageHandle};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_detection(rule_id: &str) -> Detection {
    Detection {
        rule_id: rule_id.to_string(),
        agent_type: AgentType::Codex,
        event_type: "test".to_string(),
        severity: Severity::Warning,
        confidence: 0.95,
        extracted: serde_json::json!({}),
        matched_text: "test output".to_string(),
        span: (0, 11),
    }
}

fn make_detection_with_severity(rule_id: &str, severity: Severity) -> Detection {
    Detection {
        rule_id: rule_id.to_string(),
        agent_type: AgentType::Codex,
        event_type: "test".to_string(),
        severity,
        confidence: 0.95,
        extracted: serde_json::json!({}),
        matched_text: "test output".to_string(),
        span: (0, 11),
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

async fn temp_storage(label: &str) -> (StorageHandle, String) {
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!(
        "wa_noise_test_{}_{}_{}.db",
        label,
        std::process::id(),
        now_ms()
    ));
    let db_path_str = db_path.to_string_lossy().to_string();
    let storage = StorageHandle::new(&db_path_str).await.unwrap();
    (storage, db_path_str)
}

fn cleanup(db_path: &str) {
    let _ = std::fs::remove_file(db_path);
    let _ = std::fs::remove_file(format!("{db_path}-wal"));
    let _ = std::fs::remove_file(format!("{db_path}-shm"));
}

// ===========================================================================
// 1. Mute storage CRUD
// ===========================================================================

#[tokio::test]
async fn mute_add_and_query_active() {
    let (storage, db_path) = temp_storage("add_query").await;

    let record = EventMuteRecord {
        identity_key: "evt:abc123".to_string(),
        scope: "workspace".to_string(),
        created_at: now_ms(),
        expires_at: None, // permanent
        created_by: Some("test".to_string()),
        reason: Some("noisy".to_string()),
    };
    storage.add_event_mute(record).await.unwrap();

    let muted = storage
        .is_event_muted("evt:abc123", now_ms())
        .await
        .unwrap();
    assert!(muted, "permanent mute should be active");

    cleanup(&db_path);
}

#[tokio::test]
async fn mute_query_nonexistent_returns_false() {
    let (storage, db_path) = temp_storage("nonexistent").await;

    let muted = storage
        .is_event_muted("evt:nonexistent", now_ms())
        .await
        .unwrap();
    assert!(!muted, "nonexistent key should not be muted");

    cleanup(&db_path);
}

#[tokio::test]
async fn mute_remove_existing() {
    let (storage, db_path) = temp_storage("remove").await;

    let record = EventMuteRecord {
        identity_key: "evt:removable".to_string(),
        scope: "workspace".to_string(),
        created_at: now_ms(),
        expires_at: None,
        created_by: None,
        reason: None,
    };
    storage.add_event_mute(record).await.unwrap();

    // Confirm muted
    assert!(
        storage
            .is_event_muted("evt:removable", now_ms())
            .await
            .unwrap()
    );

    // Remove
    let removed = storage.remove_event_mute("evt:removable").await.unwrap();
    assert!(removed, "should return true when a row was deleted");

    // Confirm no longer muted
    assert!(
        !storage
            .is_event_muted("evt:removable", now_ms())
            .await
            .unwrap()
    );

    cleanup(&db_path);
}

#[tokio::test]
async fn mute_remove_nonexistent_returns_false() {
    let (storage, db_path) = temp_storage("remove_none").await;

    let removed = storage.remove_event_mute("evt:ghost").await.unwrap();
    assert!(!removed, "removing nonexistent key should return false");

    cleanup(&db_path);
}

#[tokio::test]
async fn mute_expiry_past_timestamp() {
    let (storage, db_path) = temp_storage("expiry_past").await;

    let now = now_ms();
    let record = EventMuteRecord {
        identity_key: "evt:expired".to_string(),
        scope: "workspace".to_string(),
        created_at: now - 60_000,
        expires_at: Some(now - 1_000), // expired 1 second ago
        created_by: None,
        reason: None,
    };
    storage.add_event_mute(record).await.unwrap();

    let muted = storage.is_event_muted("evt:expired", now).await.unwrap();
    assert!(!muted, "expired mute should not be active");

    cleanup(&db_path);
}

#[tokio::test]
async fn mute_expiry_future_timestamp() {
    let (storage, db_path) = temp_storage("expiry_future").await;

    let now = now_ms();
    let record = EventMuteRecord {
        identity_key: "evt:future".to_string(),
        scope: "workspace".to_string(),
        created_at: now,
        expires_at: Some(now + 60_000), // expires in 1 minute
        created_by: None,
        reason: None,
    };
    storage.add_event_mute(record).await.unwrap();

    let muted = storage.is_event_muted("evt:future", now).await.unwrap();
    assert!(muted, "future-expiry mute should still be active");

    cleanup(&db_path);
}

#[tokio::test]
async fn mute_expiry_exact_boundary() {
    let (storage, db_path) = temp_storage("expiry_boundary").await;

    let now = now_ms();
    let record = EventMuteRecord {
        identity_key: "evt:boundary".to_string(),
        scope: "workspace".to_string(),
        created_at: now - 1000,
        expires_at: Some(now), // expires exactly now
        created_by: None,
        reason: None,
    };
    storage.add_event_mute(record).await.unwrap();

    // Query says "expires_at > now_ms" — at exact boundary, it should NOT be muted
    let muted = storage.is_event_muted("evt:boundary", now).await.unwrap();
    assert!(!muted, "mute at exact expiry boundary should not be active");

    cleanup(&db_path);
}

// ===========================================================================
// 2. Mute determinism — upsert semantics, scope handling
// ===========================================================================

#[tokio::test]
async fn mute_upsert_overwrites_fields() {
    let (storage, db_path) = temp_storage("upsert").await;

    let now = now_ms();
    let record1 = EventMuteRecord {
        identity_key: "evt:upsert_key".to_string(),
        scope: "workspace".to_string(),
        created_at: now,
        expires_at: Some(now + 60_000),
        created_by: Some("agent_a".to_string()),
        reason: Some("first mute".to_string()),
    };
    storage.add_event_mute(record1).await.unwrap();

    // Upsert with different scope and reason
    let record2 = EventMuteRecord {
        identity_key: "evt:upsert_key".to_string(),
        scope: "global".to_string(),
        created_at: now + 1000,
        expires_at: None, // now permanent
        created_by: Some("agent_b".to_string()),
        reason: Some("upgraded to permanent".to_string()),
    };
    storage.add_event_mute(record2).await.unwrap();

    // Should still be muted (permanent now)
    let muted = storage
        .is_event_muted("evt:upsert_key", now + 120_000)
        .await
        .unwrap();
    assert!(
        muted,
        "upserted permanent mute should be active beyond original expiry"
    );

    cleanup(&db_path);
}

#[tokio::test]
async fn mute_upsert_idempotent() {
    let (storage, db_path) = temp_storage("upsert_idem").await;

    let now = now_ms();
    let record = EventMuteRecord {
        identity_key: "evt:idem".to_string(),
        scope: "workspace".to_string(),
        created_at: now,
        expires_at: None,
        created_by: None,
        reason: None,
    };
    // Insert twice — should not error
    storage.add_event_mute(record.clone()).await.unwrap();
    storage.add_event_mute(record).await.unwrap();

    let muted = storage.is_event_muted("evt:idem", now).await.unwrap();
    assert!(muted);

    // Remove once — should succeed
    let removed = storage.remove_event_mute("evt:idem").await.unwrap();
    assert!(removed);

    // Second remove — should be no-op
    let removed2 = storage.remove_event_mute("evt:idem").await.unwrap();
    assert!(!removed2);

    cleanup(&db_path);
}

#[tokio::test]
async fn mute_multiple_keys_independent() {
    let (storage, db_path) = temp_storage("multi_key").await;

    let now = now_ms();
    for i in 0..5 {
        let record = EventMuteRecord {
            identity_key: format!("evt:key_{i}"),
            scope: "workspace".to_string(),
            created_at: now,
            expires_at: if i % 2 == 0 { None } else { Some(now - 1000) },
            created_by: None,
            reason: None,
        };
        storage.add_event_mute(record).await.unwrap();
    }

    // Even keys (0,2,4) are permanent, odd keys (1,3) are expired
    for i in 0..5 {
        let muted = storage
            .is_event_muted(&format!("evt:key_{i}"), now)
            .await
            .unwrap();
        if i % 2 == 0 {
            assert!(muted, "key_{i} should be muted (permanent)");
        } else {
            assert!(!muted, "key_{i} should not be muted (expired)");
        }
    }

    cleanup(&db_path);
}

// ===========================================================================
// 3. Event identity key + mute integration
// ===========================================================================

#[tokio::test]
async fn mute_via_identity_key_round_trip() {
    let (storage, db_path) = temp_storage("identity_key_rt").await;

    // Compute identity key the same way the pipeline does
    let detection = make_detection("core.codex:usage_reached");
    let identity_key = wa_core::events::event_identity_key(&detection, 42, None);

    // Mute that identity key
    let now = now_ms();
    let record = EventMuteRecord {
        identity_key: identity_key.clone(),
        scope: "workspace".to_string(),
        created_at: now,
        expires_at: None,
        created_by: Some("test".to_string()),
        reason: Some("mute usage reached for pane 42".to_string()),
    };
    storage.add_event_mute(record).await.unwrap();

    // Same detection + pane should produce the same key and be muted
    let check_key = wa_core::events::event_identity_key(&detection, 42, None);
    assert_eq!(
        identity_key, check_key,
        "identity key should be deterministic"
    );

    let muted = storage.is_event_muted(&check_key, now).await.unwrap();
    assert!(muted, "detection with muted identity key should be muted");

    // Different pane_id produces a different key
    let other_key = wa_core::events::event_identity_key(&detection, 99, None);
    assert_ne!(
        identity_key, other_key,
        "different pane should produce different key"
    );

    let other_muted = storage.is_event_muted(&other_key, now).await.unwrap();
    assert!(!other_muted, "different pane's key should not be muted");

    cleanup(&db_path);
}

#[tokio::test]
async fn identity_key_deterministic_with_uuid() {
    let detection = make_detection("core.claude_code:compaction");
    let key1 = wa_core::events::event_identity_key(&detection, 7, Some("uuid-abc-123"));
    let key2 = wa_core::events::event_identity_key(&detection, 7, Some("uuid-abc-123"));
    assert_eq!(key1, key2, "same inputs should produce same identity key");

    // Different UUID = different key
    let key3 = wa_core::events::event_identity_key(&detection, 7, Some("uuid-xyz-999"));
    assert_ne!(key1, key3, "different UUID should produce different key");
}

// ===========================================================================
// 4. Dedup edge cases
// ===========================================================================

#[test]
fn dedup_zero_window_always_new() {
    let mut dedup = EventDeduplicator::with_config(Duration::ZERO, 100);
    // With a zero window, every check should be New (window expires immediately)
    assert!(matches!(dedup.check("key"), DedupeVerdict::New));
    // Second check: depending on timing, may still be duplicate if instant
    // is same. Use a sleep to ensure expiry.
    std::thread::sleep(Duration::from_millis(1));
    assert!(matches!(dedup.check("key"), DedupeVerdict::New));
}

#[test]
fn dedup_capacity_one_evicts_immediately() {
    let mut dedup = EventDeduplicator::with_config(Duration::from_secs(60), 1);

    assert!(matches!(dedup.check("a"), DedupeVerdict::New));
    assert_eq!(dedup.len(), 1);

    // Adding "b" should evict "a"
    assert!(matches!(dedup.check("b"), DedupeVerdict::New));
    assert_eq!(dedup.len(), 1);

    // "a" was evicted so it's new again
    assert!(matches!(dedup.check("a"), DedupeVerdict::New));
}

#[test]
fn dedup_suppressed_count_accuracy() {
    let mut dedup = EventDeduplicator::with_config(Duration::from_secs(60), 100);

    assert!(matches!(dedup.check("k"), DedupeVerdict::New));
    for i in 1..=10 {
        match dedup.check("k") {
            DedupeVerdict::Duplicate { suppressed_count } => {
                assert_eq!(suppressed_count, i, "suppressed count should be {i}");
            }
            DedupeVerdict::New => panic!("should be duplicate after first occurrence"),
        }
    }
    assert_eq!(dedup.suppressed_count("k"), 10);
}

#[test]
fn dedup_get_expired_returns_none() {
    let mut dedup = EventDeduplicator::with_config(Duration::from_millis(10), 100);
    assert!(matches!(dedup.check("k"), DedupeVerdict::New));
    std::thread::sleep(Duration::from_millis(15));
    assert!(
        dedup.get("k").is_none(),
        "expired entry should not be returned by get()"
    );
}

#[test]
fn dedup_default_constants() {
    let dedup = EventDeduplicator::new();
    assert_eq!(dedup.len(), 0);
    assert!(dedup.is_empty());
}

// ===========================================================================
// 5. Cooldown edge cases
// ===========================================================================

#[test]
fn cooldown_zero_period_always_sends() {
    let mut cd = NotificationCooldown::with_config(Duration::ZERO, 100);
    match cd.check("k") {
        CooldownVerdict::Send {
            suppressed_since_last,
        } => assert_eq!(suppressed_since_last, 0),
        CooldownVerdict::Suppress { .. } => panic!("first check should send"),
    }
    std::thread::sleep(Duration::from_millis(1));
    // With zero cooldown, the next check after any time should also send
    match cd.check("k") {
        CooldownVerdict::Send { .. } => {}
        CooldownVerdict::Suppress { .. } => panic!("zero cooldown should never suppress"),
    }
}

#[test]
fn cooldown_capacity_one_evicts() {
    let mut cd = NotificationCooldown::with_config(Duration::from_secs(60), 1);

    assert!(matches!(
        cd.check("a"),
        CooldownVerdict::Send {
            suppressed_since_last: 0
        }
    ));
    assert_eq!(cd.len(), 1);

    // "b" should evict "a"
    assert!(matches!(cd.check("b"), CooldownVerdict::Send { .. }));
    assert_eq!(cd.len(), 1);

    // "a" was evicted so it's a fresh Send
    assert!(matches!(
        cd.check("a"),
        CooldownVerdict::Send {
            suppressed_since_last: 0
        }
    ));
}

#[test]
fn cooldown_suppressed_count_accumulates() {
    let mut cd = NotificationCooldown::with_config(Duration::from_secs(60), 100);

    assert!(matches!(cd.check("k"), CooldownVerdict::Send { .. }));
    for i in 1..=5 {
        match cd.check("k") {
            CooldownVerdict::Suppress { total_suppressed } => {
                assert_eq!(total_suppressed, i, "suppressed count should be {i}");
            }
            CooldownVerdict::Send { .. } => panic!("should be suppressed within cooldown"),
        }
    }
}

#[test]
fn cooldown_expired_includes_suppressed_count() {
    let mut cd = NotificationCooldown::with_config(Duration::from_millis(10), 100);

    assert!(matches!(cd.check("k"), CooldownVerdict::Send { .. }));
    // Suppress 3 times
    cd.check("k");
    cd.check("k");
    cd.check("k");

    std::thread::sleep(Duration::from_millis(15));

    // After cooldown expires, send should include suppressed count
    match cd.check("k") {
        CooldownVerdict::Send {
            suppressed_since_last,
        } => {
            assert_eq!(
                suppressed_since_last, 3,
                "should report 3 suppressed events"
            );
        }
        CooldownVerdict::Suppress { .. } => panic!("should send after cooldown expires"),
    }
}

#[test]
fn cooldown_get_returns_entry() {
    let mut cd = NotificationCooldown::with_config(Duration::from_secs(60), 100);
    assert!(cd.get("k").is_none());
    cd.check("k");
    let entry = cd.get("k").unwrap();
    assert_eq!(entry.suppressed_since_notify, 0);
}

// ===========================================================================
// 6. NotificationGate composite behavior
// ===========================================================================

#[test]
fn gate_muted_by_filter_excludes() {
    let filter = EventFilter::from_config(&[], &["core.codex:*".to_string()], None, &[]);
    let mut gate = NotificationGate::new(
        filter,
        EventDeduplicator::new(),
        NotificationCooldown::new(),
    );
    let detection = make_detection("core.codex:usage_reached");
    let result = gate.should_notify(&detection, 1, None);
    assert_eq!(result, NotifyDecision::Filtered);
}

#[test]
fn gate_severity_filter_blocks_low() {
    let filter = EventFilter::from_config(&[], &[], Some("critical"), &[]);
    let mut gate = NotificationGate::new(
        filter,
        EventDeduplicator::new(),
        NotificationCooldown::new(),
    );
    let detection = make_detection_with_severity("test.rule", Severity::Info);
    let result = gate.should_notify(&detection, 1, None);
    assert_eq!(result, NotifyDecision::Filtered);
}

#[test]
fn gate_severity_filter_allows_critical() {
    let filter = EventFilter::from_config(&[], &[], Some("critical"), &[]);
    let mut gate = NotificationGate::new(
        filter,
        EventDeduplicator::new(),
        NotificationCooldown::new(),
    );
    let detection = make_detection_with_severity("test.rule", Severity::Critical);
    let result = gate.should_notify(&detection, 1, None);
    assert!(matches!(result, NotifyDecision::Send { .. }));
}

#[test]
fn gate_agent_type_filter() {
    let filter = EventFilter::from_config(&[], &[], None, &["gemini".to_string()]);
    let mut gate = NotificationGate::new(
        filter,
        EventDeduplicator::new(),
        NotificationCooldown::new(),
    );
    // Detection is for Codex, filter only allows Gemini
    let detection = make_detection("test.rule");
    let result = gate.should_notify(&detection, 1, None);
    assert_eq!(result, NotifyDecision::Filtered);
}

#[test]
fn gate_dedup_then_cooldown_sequence() {
    // Short dedup window, longer cooldown
    let mut gate = NotificationGate::from_config(
        EventFilter::allow_all(),
        Duration::from_millis(10),
        Duration::from_secs(60),
    );
    let detection = make_detection("test.rule");

    // First: Send
    assert!(matches!(
        gate.should_notify(&detection, 1, None),
        NotifyDecision::Send {
            suppressed_since_last: 0
        }
    ));

    // Second (within dedup window): Deduplicated
    assert!(matches!(
        gate.should_notify(&detection, 1, None),
        NotifyDecision::Deduplicated { .. }
    ));

    // Wait for dedup to expire
    std::thread::sleep(Duration::from_millis(15));

    // Third (dedup expired, but cooldown still active): Throttled
    assert!(matches!(
        gate.should_notify(&detection, 1, None),
        NotifyDecision::Throttled { .. }
    ));
}

#[test]
fn gate_include_filter_allows_matching() {
    let filter = EventFilter::from_config(&["core.codex:*".to_string()], &[], None, &[]);
    let mut gate = NotificationGate::new(
        filter,
        EventDeduplicator::new(),
        NotificationCooldown::new(),
    );
    let detection = make_detection("core.codex:usage_reached");
    let result = gate.should_notify(&detection, 1, None);
    assert!(
        matches!(result, NotifyDecision::Send { .. }),
        "matching include pattern should allow through"
    );
}

#[test]
fn gate_include_filter_blocks_non_matching() {
    let filter = EventFilter::from_config(&["core.claude_code:*".to_string()], &[], None, &[]);
    let mut gate = NotificationGate::new(
        filter,
        EventDeduplicator::new(),
        NotificationCooldown::new(),
    );
    let detection = make_detection("core.codex:usage_reached");
    let result = gate.should_notify(&detection, 1, None);
    assert_eq!(
        result,
        NotifyDecision::Filtered,
        "non-matching include pattern should filter"
    );
}

#[test]
fn gate_exclude_wins_over_include() {
    let filter = EventFilter::from_config(
        &["core.*".to_string()],
        &["core.codex:*".to_string()],
        None,
        &[],
    );
    let mut gate = NotificationGate::new(
        filter,
        EventDeduplicator::new(),
        NotificationCooldown::new(),
    );
    let detection = make_detection("core.codex:usage_reached");
    let result = gate.should_notify(&detection, 1, None);
    assert_eq!(
        result,
        NotifyDecision::Filtered,
        "exclude should take priority over include"
    );
}

// ===========================================================================
// 7. EventFilter standalone
// ===========================================================================

#[test]
fn filter_allow_all_is_permissive() {
    let filter = EventFilter::allow_all();
    assert!(filter.is_permissive());
    let detection = make_detection("any.rule");
    assert!(filter.matches(&detection));
}

#[test]
fn filter_with_restrictions_is_not_permissive() {
    let filter = EventFilter::from_config(&[], &["noisy.*".to_string()], None, &[]);
    assert!(!filter.is_permissive());
}

#[test]
fn filter_glob_wildcard_matching() {
    assert!(wa_core::events::match_rule_glob(
        "core.*",
        "core.codex:usage"
    ));
    assert!(wa_core::events::match_rule_glob(
        "core.codex:*",
        "core.codex:usage_reached"
    ));
    assert!(!wa_core::events::match_rule_glob(
        "core.claude:*",
        "core.codex:usage"
    ));
    assert!(wa_core::events::match_rule_glob("?ore.*", "core.anything"));
    assert!(!wa_core::events::match_rule_glob(
        "?ore.*",
        "xcore.anything"
    ));
}

#[test]
fn filter_exact_match_without_wildcards() {
    assert!(wa_core::events::match_rule_glob(
        "exact.rule.id",
        "exact.rule.id"
    ));
    assert!(!wa_core::events::match_rule_glob(
        "exact.rule.id",
        "exact.rule.id2"
    ));
}
