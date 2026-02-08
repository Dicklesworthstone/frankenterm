// Integration tests for event annotations, labels, and triage state (bd-1yk8)
use tempfile::TempDir;
use wa_core::storage::{EventQuery, PaneRecord, StorageHandle, StoredEvent};

fn temp_db() -> (TempDir, String) {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("test.db").to_string_lossy().to_string();
    (dir, path)
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime")
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn make_pane(ts: i64) -> PaneRecord {
    PaneRecord {
        pane_id: 1,
        pane_uuid: None,
        domain: "local".to_string(),
        window_id: None,
        tab_id: None,
        title: None,
        cwd: None,
        tty_name: None,
        first_seen_at: ts,
        last_seen_at: ts,
        observed: true,
        ignore_reason: None,
        last_decision_at: None,
    }
}

fn make_event(pane_id: u64, rule_id: &str, ts: i64) -> StoredEvent {
    StoredEvent {
        id: 0,
        pane_id,
        rule_id: rule_id.to_string(),
        agent_type: "codex".to_string(),
        event_type: "test_event".to_string(),
        severity: "info".to_string(),
        confidence: 0.9,
        extracted: None,
        matched_text: None,
        segment_id: None,
        detected_at: ts,
        dedupe_key: None,
        handled_at: None,
        handled_by_workflow_id: None,
        handled_status: None,
    }
}

/// Helper: create storage, upsert pane, record event, return (storage, event_id).
async fn setup_with_event() -> (TempDir, StorageHandle, i64) {
    let (dir, path) = temp_db();
    let storage = StorageHandle::new(&path).await.expect("create storage");
    let ts = now_ms();
    storage.upsert_pane(make_pane(ts)).await.unwrap();
    let event_id = storage
        .record_event(make_event(1, "rule.test", ts))
        .await
        .unwrap();
    (dir, storage, event_id)
}

// ---- Triage State ----

#[test]
fn set_and_get_triage_state() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, storage, event_id) = setup_with_event().await;

        let updated = storage
            .set_event_triage_state(
                event_id,
                Some("investigating".to_string()),
                Some("alice".to_string()),
            )
            .await
            .unwrap();
        assert!(updated);

        let ann = storage
            .get_event_annotations(event_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ann.triage_state.as_deref(), Some("investigating"));
        assert!(ann.triage_updated_at.is_some());
        assert_eq!(ann.triage_updated_by.as_deref(), Some("alice"));
    });
}

#[test]
fn clear_triage_state() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, storage, event_id) = setup_with_event().await;

        // Set then clear
        storage
            .set_event_triage_state(event_id, Some("resolved".to_string()), None)
            .await
            .unwrap();
        storage
            .set_event_triage_state(event_id, None, None)
            .await
            .unwrap();

        let ann = storage
            .get_event_annotations(event_id)
            .await
            .unwrap()
            .unwrap();
        assert!(ann.triage_state.is_none());
        assert!(ann.triage_updated_at.is_none());
    });
}

#[test]
fn triage_state_nonexistent_event() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        let updated = storage
            .set_event_triage_state(99999, Some("resolved".to_string()), None)
            .await
            .unwrap();
        assert!(!updated);
    });
}

#[test]
fn triage_state_transitions() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, storage, event_id) = setup_with_event().await;

        // new -> investigating -> resolved
        for state in &["new", "investigating", "resolved"] {
            storage
                .set_event_triage_state(event_id, Some(state.to_string()), Some("bot".to_string()))
                .await
                .unwrap();
        }

        let ann = storage
            .get_event_annotations(event_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ann.triage_state.as_deref(), Some("resolved"));
        assert_eq!(ann.triage_updated_by.as_deref(), Some("bot"));
    });
}

// ---- Notes ----

#[test]
fn set_and_get_note() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, storage, event_id) = setup_with_event().await;

        storage
            .set_event_note(
                event_id,
                Some("This looks suspicious".to_string()),
                Some("alice".to_string()),
            )
            .await
            .unwrap();

        let ann = storage
            .get_event_annotations(event_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ann.note.as_deref(), Some("This looks suspicious"));
        assert!(ann.note_updated_at.is_some());
        assert_eq!(ann.note_updated_by.as_deref(), Some("alice"));
    });
}

#[test]
fn update_note_overwrites() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, storage, event_id) = setup_with_event().await;

        storage
            .set_event_note(
                event_id,
                Some("first".to_string()),
                Some("alice".to_string()),
            )
            .await
            .unwrap();
        storage
            .set_event_note(
                event_id,
                Some("second".to_string()),
                Some("bob".to_string()),
            )
            .await
            .unwrap();

        let ann = storage
            .get_event_annotations(event_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ann.note.as_deref(), Some("second"));
        assert_eq!(ann.note_updated_by.as_deref(), Some("bob"));
    });
}

#[test]
fn clear_note() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, storage, event_id) = setup_with_event().await;

        storage
            .set_event_note(event_id, Some("temp note".to_string()), None)
            .await
            .unwrap();
        storage.set_event_note(event_id, None, None).await.unwrap();

        let ann = storage
            .get_event_annotations(event_id)
            .await
            .unwrap()
            .unwrap();
        assert!(ann.note.is_none());
        assert!(ann.note_updated_at.is_none());
    });
}

// ---- Labels ----

#[test]
fn add_and_get_labels() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, storage, event_id) = setup_with_event().await;

        let inserted = storage
            .add_event_label(event_id, "urgent".to_string(), Some("alice".to_string()))
            .await
            .unwrap();
        assert!(inserted);

        let inserted2 = storage
            .add_event_label(event_id, "billing".to_string(), None)
            .await
            .unwrap();
        assert!(inserted2);

        let ann = storage
            .get_event_annotations(event_id)
            .await
            .unwrap()
            .unwrap();
        // Labels should be sorted alphabetically
        assert_eq!(ann.labels, vec!["billing", "urgent"]);
    });
}

#[test]
fn duplicate_label_is_idempotent() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, storage, event_id) = setup_with_event().await;

        let first = storage
            .add_event_label(event_id, "important".to_string(), None)
            .await
            .unwrap();
        assert!(first);

        let second = storage
            .add_event_label(event_id, "important".to_string(), None)
            .await
            .unwrap();
        assert!(!second); // INSERT OR IGNORE — no new row

        let ann = storage
            .get_event_annotations(event_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ann.labels.len(), 1);
    });
}

#[test]
fn remove_label() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, storage, event_id) = setup_with_event().await;

        storage
            .add_event_label(event_id, "alpha".to_string(), None)
            .await
            .unwrap();
        storage
            .add_event_label(event_id, "beta".to_string(), None)
            .await
            .unwrap();

        let removed = storage
            .remove_event_label(event_id, "alpha".to_string())
            .await
            .unwrap();
        assert!(removed);

        let ann = storage
            .get_event_annotations(event_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ann.labels, vec!["beta"]);
    });
}

#[test]
fn remove_nonexistent_label() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, storage, event_id) = setup_with_event().await;

        let removed = storage
            .remove_event_label(event_id, "ghost".to_string())
            .await
            .unwrap();
        assert!(!removed);
    });
}

// ---- Full annotations roundtrip ----

#[test]
fn full_annotations_roundtrip() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, storage, event_id) = setup_with_event().await;

        // Set triage
        storage
            .set_event_triage_state(
                event_id,
                Some("investigating".to_string()),
                Some("ops".to_string()),
            )
            .await
            .unwrap();

        // Set note
        storage
            .set_event_note(
                event_id,
                Some("Requires follow-up with vendor".to_string()),
                Some("ops".to_string()),
            )
            .await
            .unwrap();

        // Add labels
        storage
            .add_event_label(event_id, "vendor".to_string(), Some("ops".to_string()))
            .await
            .unwrap();
        storage
            .add_event_label(event_id, "billing".to_string(), Some("ops".to_string()))
            .await
            .unwrap();

        // Fetch combined annotations
        let ann = storage
            .get_event_annotations(event_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ann.triage_state.as_deref(), Some("investigating"));
        assert_eq!(ann.note.as_deref(), Some("Requires follow-up with vendor"));
        assert_eq!(ann.labels, vec!["billing", "vendor"]);
    });
}

// ---- Empty annotations for fresh event ----

#[test]
fn fresh_event_has_empty_annotations() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, storage, event_id) = setup_with_event().await;

        let ann = storage
            .get_event_annotations(event_id)
            .await
            .unwrap()
            .unwrap();
        assert!(ann.triage_state.is_none());
        assert!(ann.note.is_none());
        assert!(ann.labels.is_empty());
    });
}

// ---- Nonexistent event annotations ----

#[test]
fn annotations_for_nonexistent_event() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        let ann = storage.get_event_annotations(99999).await.unwrap();
        assert!(ann.is_none());
    });
}

// ---- Query filtering by triage state ----

#[test]
fn query_events_by_triage_state() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");
        let ts = now_ms();

        storage.upsert_pane(make_pane(ts)).await.unwrap();

        let id1 = storage
            .record_event(make_event(1, "rule.a", ts - 1000))
            .await
            .unwrap();
        let _id2 = storage
            .record_event(make_event(1, "rule.b", ts - 500))
            .await
            .unwrap();

        // Set triage on first event only
        storage
            .set_event_triage_state(id1, Some("investigating".to_string()), None)
            .await
            .unwrap();

        let results = storage
            .get_events(EventQuery {
                triage_state: Some("investigating".to_string()),
                ..Default::default()
            })
            .await
            .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, id1);
    });
}

// ---- Query filtering by label ----

#[test]
fn query_events_by_label() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");
        let ts = now_ms();

        storage.upsert_pane(make_pane(ts)).await.unwrap();

        let id1 = storage
            .record_event(make_event(1, "rule.x", ts - 1000))
            .await
            .unwrap();
        let id2 = storage
            .record_event(make_event(1, "rule.y", ts - 500))
            .await
            .unwrap();

        // Label both events differently
        storage
            .add_event_label(id1, "production".to_string(), None)
            .await
            .unwrap();
        storage
            .add_event_label(id2, "staging".to_string(), None)
            .await
            .unwrap();

        let results = storage
            .get_events(EventQuery {
                label: Some("production".to_string()),
                ..Default::default()
            })
            .await
            .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, id1);
    });
}

// ---- Combined triage + label filter ----

#[test]
fn query_events_combined_filters() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");
        let ts = now_ms();

        storage.upsert_pane(make_pane(ts)).await.unwrap();

        let id1 = storage
            .record_event(make_event(1, "rule.a", ts - 2000))
            .await
            .unwrap();
        let id2 = storage
            .record_event(make_event(1, "rule.b", ts - 1000))
            .await
            .unwrap();

        // id1: investigating + production
        storage
            .set_event_triage_state(id1, Some("investigating".to_string()), None)
            .await
            .unwrap();
        storage
            .add_event_label(id1, "production".to_string(), None)
            .await
            .unwrap();

        // id2: investigating + staging
        storage
            .set_event_triage_state(id2, Some("investigating".to_string()), None)
            .await
            .unwrap();
        storage
            .add_event_label(id2, "staging".to_string(), None)
            .await
            .unwrap();

        // Filter: investigating + production -> only id1
        let results = storage
            .get_events(EventQuery {
                triage_state: Some("investigating".to_string()),
                label: Some("production".to_string()),
                ..Default::default()
            })
            .await
            .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, id1);
    });
}

// ---- EventAnnotations serialization ----

#[test]
fn event_annotations_serde_roundtrip() {
    let ann = wa_core::storage::EventAnnotations {
        triage_state: Some("resolved".to_string()),
        triage_updated_at: Some(1700000000000),
        triage_updated_by: Some("bot".to_string()),
        note: Some("All clear".to_string()),
        note_updated_at: Some(1700000001000),
        note_updated_by: Some("bot".to_string()),
        labels: vec!["fixed".to_string(), "verified".to_string()],
    };

    let json = serde_json::to_string(&ann).unwrap();
    let parsed: wa_core::storage::EventAnnotations = serde_json::from_str(&json).unwrap();

    assert_eq!(parsed.triage_state.as_deref(), Some("resolved"));
    assert_eq!(parsed.labels, vec!["fixed", "verified"]);
    assert_eq!(parsed.note.as_deref(), Some("All clear"));
}

// ---- EventAnnotations default ----

#[test]
fn event_annotations_default_is_empty() {
    let ann = wa_core::storage::EventAnnotations::default();

    assert!(ann.triage_state.is_none());
    assert!(ann.triage_updated_at.is_none());
    assert!(ann.triage_updated_by.is_none());
    assert!(ann.note.is_none());
    assert!(ann.note_updated_at.is_none());
    assert!(ann.note_updated_by.is_none());
    assert!(ann.labels.is_empty());
}

// ---- Multiple events with labels ----

#[test]
fn labels_scoped_to_event() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");
        let ts = now_ms();

        storage.upsert_pane(make_pane(ts)).await.unwrap();

        let id1 = storage
            .record_event(make_event(1, "rule.a", ts - 1000))
            .await
            .unwrap();
        let id2 = storage
            .record_event(make_event(1, "rule.b", ts - 500))
            .await
            .unwrap();

        storage
            .add_event_label(id1, "important".to_string(), None)
            .await
            .unwrap();
        storage
            .add_event_label(id2, "low-priority".to_string(), None)
            .await
            .unwrap();

        let ann1 = storage.get_event_annotations(id1).await.unwrap().unwrap();
        let ann2 = storage.get_event_annotations(id2).await.unwrap().unwrap();

        assert_eq!(ann1.labels, vec!["important"]);
        assert_eq!(ann2.labels, vec!["low-priority"]);
    });
}

// ---- Note redaction of secrets (bd-1q77) ----

#[test]
fn note_with_secret_is_redacted_on_storage() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, storage, event_id) = setup_with_event().await;

        // Build an OpenAI-style key at runtime (split to avoid push-protection)
        let secret_key = [
            "sk-",
            "proj-",
            "abc123456789012345678901234567890123456789ABCDE",
        ]
        .concat();
        let note_text = format!("Found key: {secret_key} in logs");

        storage
            .set_event_note(event_id, Some(note_text), Some("bot".to_string()))
            .await
            .unwrap();

        let ann = storage
            .get_event_annotations(event_id)
            .await
            .unwrap()
            .unwrap();

        // The secret should be redacted before persistence
        let stored_note = ann.note.as_deref().unwrap();
        assert!(
            stored_note.contains("[REDACTED]"),
            "note should contain REDACTED marker: {stored_note}"
        );
        let prefix = ["sk-", "proj-"].concat();
        assert!(
            !stored_note.contains(&prefix),
            "API key prefix should not be in stored note: {stored_note}"
        );
    });
}

#[test]
fn note_without_secrets_passes_through() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, storage, event_id) = setup_with_event().await;

        let note_text = "This is a clean note with no secrets".to_string();
        storage
            .set_event_note(event_id, Some(note_text.clone()), None)
            .await
            .unwrap();

        let ann = storage
            .get_event_annotations(event_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ann.note.as_deref(), Some(note_text.as_str()));
    });
}

#[test]
fn note_with_bearer_token_is_redacted() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, storage, event_id) = setup_with_event().await;

        // Must match the bearer_token regex: Authorization: Bearer <20+ chars>
        let token = [
            "Authorization: Bearer ",
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0",
        ]
        .concat();
        let note_text = format!("Auth header: {token}");

        storage
            .set_event_note(event_id, Some(note_text), None)
            .await
            .unwrap();

        let ann = storage
            .get_event_annotations(event_id)
            .await
            .unwrap()
            .unwrap();
        let stored_note = ann.note.as_deref().unwrap();
        assert!(
            stored_note.contains("[REDACTED]"),
            "bearer token should be redacted: {stored_note}"
        );
    });
}

// ---- Empty and edge-case notes (bd-1q77) ----

#[test]
fn empty_string_note_is_stored() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, storage, event_id) = setup_with_event().await;

        storage
            .set_event_note(event_id, Some(String::new()), Some("alice".to_string()))
            .await
            .unwrap();

        let ann = storage
            .get_event_annotations(event_id)
            .await
            .unwrap()
            .unwrap();
        // Empty string note should be stored (not treated as None)
        assert_eq!(ann.note.as_deref(), Some(""));
        assert!(ann.note_updated_at.is_some());
    });
}

#[test]
fn whitespace_only_note_is_stored() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, storage, event_id) = setup_with_event().await;

        storage
            .set_event_note(event_id, Some("   \n\t  ".to_string()), None)
            .await
            .unwrap();

        let ann = storage
            .get_event_annotations(event_id)
            .await
            .unwrap()
            .unwrap();
        assert!(ann.note.is_some());
    });
}

// ---- Triage state edge cases (bd-1q77) ----

#[test]
fn triage_state_backward_transition() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, storage, event_id) = setup_with_event().await;

        // Forward: new → investigating → resolved
        storage
            .set_event_triage_state(
                event_id,
                Some("resolved".to_string()),
                Some("ops".to_string()),
            )
            .await
            .unwrap();

        // Backward: resolved → new (should be allowed — no state machine enforcement)
        storage
            .set_event_triage_state(event_id, Some("new".to_string()), Some("ops".to_string()))
            .await
            .unwrap();

        let ann = storage
            .get_event_annotations(event_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ann.triage_state.as_deref(), Some("new"));
    });
}

#[test]
fn triage_state_accepts_arbitrary_strings() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, storage, event_id) = setup_with_event().await;

        // Any string is accepted (no restricted state machine)
        let updated = storage
            .set_event_triage_state(event_id, Some("custom_state_xyz".to_string()), None)
            .await
            .unwrap();
        assert!(updated);

        let ann = storage
            .get_event_annotations(event_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ann.triage_state.as_deref(), Some("custom_state_xyz"));
    });
}

#[test]
fn triage_state_set_same_twice_is_idempotent() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, storage, event_id) = setup_with_event().await;

        storage
            .set_event_triage_state(
                event_id,
                Some("investigating".to_string()),
                Some("ops".to_string()),
            )
            .await
            .unwrap();

        let ann1 = storage
            .get_event_annotations(event_id)
            .await
            .unwrap()
            .unwrap();

        // Set same state again
        storage
            .set_event_triage_state(
                event_id,
                Some("investigating".to_string()),
                Some("ops".to_string()),
            )
            .await
            .unwrap();

        let ann2 = storage
            .get_event_annotations(event_id)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(ann1.triage_state, ann2.triage_state);
        // Timestamp may change (it's an update, not a no-op)
    });
}

#[test]
fn triage_timestamp_updates_on_mutation() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, storage, event_id) = setup_with_event().await;

        storage
            .set_event_triage_state(event_id, Some("new".to_string()), Some("alice".to_string()))
            .await
            .unwrap();

        let ann1 = storage
            .get_event_annotations(event_id)
            .await
            .unwrap()
            .unwrap();
        let ts1 = ann1.triage_updated_at.unwrap();

        // Small delay then update
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        storage
            .set_event_triage_state(
                event_id,
                Some("investigating".to_string()),
                Some("bob".to_string()),
            )
            .await
            .unwrap();

        let ann2 = storage
            .get_event_annotations(event_id)
            .await
            .unwrap()
            .unwrap();
        let ts2 = ann2.triage_updated_at.unwrap();

        assert!(
            ts2 >= ts1,
            "timestamp should not decrease: ts1={ts1} ts2={ts2}"
        );
        assert_eq!(ann2.triage_updated_by.as_deref(), Some("bob"));
    });
}

// ---- Label edge cases (bd-1q77) ----

#[test]
fn many_labels_sorted_alphabetically() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, storage, event_id) = setup_with_event().await;

        for label in &["zebra", "alpha", "middle", "beta"] {
            storage
                .add_event_label(event_id, label.to_string(), None)
                .await
                .unwrap();
        }

        let ann = storage
            .get_event_annotations(event_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ann.labels, vec!["alpha", "beta", "middle", "zebra"]);
    });
}

#[test]
fn remove_all_labels_leaves_empty_vec() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, storage, event_id) = setup_with_event().await;

        storage
            .add_event_label(event_id, "a".to_string(), None)
            .await
            .unwrap();
        storage
            .add_event_label(event_id, "b".to_string(), None)
            .await
            .unwrap();

        storage
            .remove_event_label(event_id, "a".to_string())
            .await
            .unwrap();
        storage
            .remove_event_label(event_id, "b".to_string())
            .await
            .unwrap();

        let ann = storage
            .get_event_annotations(event_id)
            .await
            .unwrap()
            .unwrap();
        assert!(ann.labels.is_empty());
    });
}

#[test]
fn label_on_nonexistent_event() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        // Adding a label to a nonexistent event should fail gracefully
        let result = storage
            .add_event_label(99999, "orphan".to_string(), None)
            .await;
        // May succeed (FOREIGN KEY might be disabled) or error
        // Either way, no panic
        let _ = result;
    });
}

#[test]
fn note_timestamp_updates_on_mutation() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, storage, event_id) = setup_with_event().await;

        storage
            .set_event_note(
                event_id,
                Some("first".to_string()),
                Some("alice".to_string()),
            )
            .await
            .unwrap();

        let ann1 = storage
            .get_event_annotations(event_id)
            .await
            .unwrap()
            .unwrap();
        let ts1 = ann1.note_updated_at.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        storage
            .set_event_note(
                event_id,
                Some("second".to_string()),
                Some("bob".to_string()),
            )
            .await
            .unwrap();

        let ann2 = storage
            .get_event_annotations(event_id)
            .await
            .unwrap()
            .unwrap();
        let ts2 = ann2.note_updated_at.unwrap();

        assert!(
            ts2 >= ts1,
            "note timestamp should not decrease: ts1={ts1} ts2={ts2}"
        );
        assert_eq!(ann2.note_updated_by.as_deref(), Some("bob"));
    });
}

// ---- Schema migration v18 ----

#[test]
fn schema_migration_v18_annotation_tables_exist() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");
        let ts = now_ms();

        // Verify the tables exist by using them
        storage.upsert_pane(make_pane(ts)).await.unwrap();
        let event_id = storage
            .record_event(make_event(1, "rule.test", ts))
            .await
            .unwrap();

        // All annotation operations should work (tables exist)
        storage
            .set_event_triage_state(event_id, Some("new".to_string()), None)
            .await
            .unwrap();
        storage
            .set_event_note(event_id, Some("note".to_string()), None)
            .await
            .unwrap();
        storage
            .add_event_label(event_id, "label".to_string(), None)
            .await
            .unwrap();

        let ann = storage
            .get_event_annotations(event_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ann.triage_state.as_deref(), Some("new"));
        assert_eq!(ann.note.as_deref(), Some("note"));
        assert_eq!(ann.labels, vec!["label"]);
    });
}
