//! Property-based tests for `SessionStore`.
//!
//! Tests key invariants:
//! - Save/load roundtrip for arbitrary byte payloads across all key types
//! - TTL expiry boundary: readable at deadline-1, gone at deadline+1
//! - Key isolation between pane/window/session namespaces
//! - Overwrite-last-wins semantics
//! - Delete idempotency
//! - Session ID validation (empty/whitespace rejection)
//! - Arbitrary operation sequences maintain internal consistency
//!
//! Requires feature: `redis-session`

#![cfg(feature = "redis-session")]

use frankenterm_core::session_store::{SessionStore, SessionStoreConfig, SessionStoreError};
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────

fn arb_ttl_ms() -> impl Strategy<Value = u64> {
    prop_oneof![
        1..=100u64,            // very short
        100..=10_000u64,       // medium
        10_000..=1_000_000u64, // long
    ]
}

fn arb_config() -> impl Strategy<Value = SessionStoreConfig> {
    (arb_ttl_ms(), arb_ttl_ms(), arb_ttl_ms(), arb_ttl_ms()).prop_map(
        |(pane, window, session, transient)| SessionStoreConfig {
            pane_state_ttl_ms: pane,
            window_layout_ttl_ms: window,
            session_meta_ttl_ms: session,
            transient_state_ttl_ms: transient,
        },
    )
}

fn arb_pane_id() -> impl Strategy<Value = u64> {
    prop_oneof![
        Just(0u64),
        Just(u64::MAX),
        1..=1000u64,
        any::<u64>(),
    ]
}

fn arb_window_id() -> impl Strategy<Value = u64> {
    prop_oneof![
        Just(0u64),
        Just(u64::MAX),
        1..=1000u64,
        any::<u64>(),
    ]
}

fn arb_session_id() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9_\\-./]{1,64}"
}

fn arb_payload() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        Just(vec![]),                        // empty
        prop::collection::vec(any::<u8>(), 1..=8),    // tiny
        prop::collection::vec(any::<u8>(), 8..=256),  // small
        prop::collection::vec(any::<u8>(), 256..=4096), // medium
    ]
}

fn arb_now_ms() -> impl Strategy<Value = u64> {
    1_000u64..=1_000_000_000u64
}

// ── Pane state roundtrip ────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn pane_state_roundtrip(
        pane_id in arb_pane_id(),
        payload in arb_payload(),
        now_ms in arb_now_ms(),
    ) {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        store.save_pane_state(pane_id, &payload, now_ms);
        let loaded = store.load_pane_state(pane_id, now_ms)
            .expect("load should not fail");
        prop_assert_eq!(loaded, Some(payload));
    }

    #[test]
    fn window_layout_roundtrip(
        window_id in arb_window_id(),
        payload in arb_payload(),
        now_ms in arb_now_ms(),
    ) {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        store.save_window_layout(window_id, &payload, now_ms);
        let loaded = store.load_window_layout(window_id, now_ms)
            .expect("load should not fail");
        prop_assert_eq!(loaded, Some(payload));
    }

    #[test]
    fn session_meta_roundtrip(
        session_id in arb_session_id(),
        payload in arb_payload(),
        now_ms in arb_now_ms(),
    ) {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        store.save_session_meta(&session_id, &payload, now_ms)
            .expect("save should succeed");
        let loaded = store.load_session_meta(&session_id, now_ms)
            .expect("load should not fail");
        prop_assert_eq!(loaded, Some(payload));
    }
}

// ── TTL expiry boundary ─────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn pane_state_ttl_boundary(
        pane_id in arb_pane_id(),
        ttl in arb_ttl_ms(),
        now_ms in 1_000u64..=500_000_000u64,
    ) {
        let config = SessionStoreConfig {
            pane_state_ttl_ms: ttl,
            ..SessionStoreConfig::default()
        };
        let mut store = SessionStore::new(config);
        store.save_pane_state(pane_id, b"data", now_ms);

        // Readable just before expiry
        let before_expiry = now_ms + ttl - 1;
        let loaded = store.load_pane_state(pane_id, before_expiry)
            .expect("load should not fail");
        prop_assert!(loaded.is_some(), "should be readable at ttl-1");

        // Gone after expiry (re-save since lazy eviction removed it)
        store.save_pane_state(pane_id, b"data", now_ms);
        let after_expiry = now_ms + ttl + 1;
        let loaded = store.load_pane_state(pane_id, after_expiry)
            .expect("load should not fail");
        prop_assert!(loaded.is_none(), "should be expired at ttl+1");
    }

    #[test]
    fn window_layout_ttl_boundary(
        window_id in arb_window_id(),
        ttl in arb_ttl_ms(),
        now_ms in 1_000u64..=500_000_000u64,
    ) {
        let config = SessionStoreConfig {
            window_layout_ttl_ms: ttl,
            ..SessionStoreConfig::default()
        };
        let mut store = SessionStore::new(config);
        store.save_window_layout(window_id, b"layout", now_ms);

        let before_expiry = now_ms + ttl - 1;
        let loaded = store.load_window_layout(window_id, before_expiry)
            .expect("load should not fail");
        prop_assert!(loaded.is_some(), "should be readable at ttl-1");

        store.save_window_layout(window_id, b"layout", now_ms);
        let after_expiry = now_ms + ttl + 1;
        let loaded = store.load_window_layout(window_id, after_expiry)
            .expect("load should not fail");
        prop_assert!(loaded.is_none(), "should be expired at ttl+1");
    }

    #[test]
    fn session_meta_ttl_boundary(
        session_id in arb_session_id(),
        ttl in arb_ttl_ms(),
        now_ms in 1_000u64..=500_000_000u64,
    ) {
        let config = SessionStoreConfig {
            session_meta_ttl_ms: ttl,
            ..SessionStoreConfig::default()
        };
        let mut store = SessionStore::new(config);
        store.save_session_meta(&session_id, b"meta", now_ms)
            .expect("save should succeed");

        let before_expiry = now_ms + ttl - 1;
        let loaded = store.load_session_meta(&session_id, before_expiry)
            .expect("load should not fail");
        prop_assert!(loaded.is_some(), "should be readable at ttl-1");

        store.save_session_meta(&session_id, b"meta", now_ms)
            .expect("save should succeed");
        let after_expiry = now_ms + ttl + 1;
        let loaded = store.load_session_meta(&session_id, after_expiry)
            .expect("load should not fail");
        prop_assert!(loaded.is_none(), "should be expired at ttl+1");
    }
}

// ── Key isolation ───────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn keyspaces_are_isolated(
        id in 0u64..=1000u64,
        pane_payload in arb_payload(),
        window_payload in arb_payload(),
        session_payload in arb_payload(),
        now_ms in arb_now_ms(),
    ) {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        let session_id = format!("{id}");

        store.save_pane_state(id, &pane_payload, now_ms);
        store.save_window_layout(id, &window_payload, now_ms);
        store.save_session_meta(&session_id, &session_payload, now_ms)
            .expect("save should succeed");

        prop_assert_eq!(store.key_count(), 3);

        let loaded_pane = store.load_pane_state(id, now_ms)
            .expect("load pane should not fail");
        let loaded_window = store.load_window_layout(id, now_ms)
            .expect("load window should not fail");
        let loaded_session = store.load_session_meta(&session_id, now_ms)
            .expect("load session should not fail");

        prop_assert_eq!(loaded_pane.as_ref(), Some(&pane_payload));
        prop_assert_eq!(loaded_window.as_ref(), Some(&window_payload));
        prop_assert_eq!(loaded_session.as_ref(), Some(&session_payload));

        // Deleting one keyspace doesn't affect others
        store.delete_pane_state(id, now_ms);
        prop_assert_eq!(store.key_count(), 2);

        let loaded_window = store.load_window_layout(id, now_ms)
            .expect("load window should not fail");
        let loaded_session = store.load_session_meta(&session_id, now_ms)
            .expect("load session should not fail");
        prop_assert_eq!(loaded_window.as_ref(), Some(&window_payload));
        prop_assert_eq!(loaded_session.as_ref(), Some(&session_payload));
    }
}

// ── Overwrite semantics ─────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(150))]

    #[test]
    fn pane_overwrite_last_write_wins(
        pane_id in arb_pane_id(),
        first in arb_payload(),
        second in arb_payload(),
        now_ms in arb_now_ms(),
    ) {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        store.save_pane_state(pane_id, &first, now_ms);
        store.save_pane_state(pane_id, &second, now_ms + 1);

        let loaded = store.load_pane_state(pane_id, now_ms + 1)
            .expect("load should not fail");
        prop_assert_eq!(loaded, Some(second));
        // Overwrite doesn't increase key count
        prop_assert_eq!(store.key_count(), 1);
    }

    #[test]
    fn session_overwrite_last_write_wins(
        session_id in arb_session_id(),
        first in arb_payload(),
        second in arb_payload(),
        now_ms in arb_now_ms(),
    ) {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        store.save_session_meta(&session_id, &first, now_ms)
            .expect("save should succeed");
        store.save_session_meta(&session_id, &second, now_ms + 1)
            .expect("save should succeed");

        let loaded = store.load_session_meta(&session_id, now_ms + 1)
            .expect("load should not fail");
        prop_assert_eq!(loaded, Some(second));
        prop_assert_eq!(store.key_count(), 1);
    }
}

// ── Delete idempotency ──────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(150))]

    #[test]
    fn pane_delete_idempotent(
        pane_id in arb_pane_id(),
        payload in arb_payload(),
        now_ms in arb_now_ms(),
    ) {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        store.save_pane_state(pane_id, &payload, now_ms);
        prop_assert_eq!(store.delete_pane_state(pane_id, now_ms), 1);
        prop_assert_eq!(store.delete_pane_state(pane_id, now_ms), 0);
        prop_assert_eq!(store.delete_pane_state(pane_id, now_ms), 0);

        let loaded = store.load_pane_state(pane_id, now_ms)
            .expect("load should not fail");
        prop_assert_eq!(loaded, None);
    }

    #[test]
    fn window_delete_idempotent(
        window_id in arb_window_id(),
        payload in arb_payload(),
        now_ms in arb_now_ms(),
    ) {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        store.save_window_layout(window_id, &payload, now_ms);
        prop_assert_eq!(store.delete_window_layout(window_id, now_ms), 1);
        prop_assert_eq!(store.delete_window_layout(window_id, now_ms), 0);
    }

    #[test]
    fn session_delete_idempotent(
        session_id in arb_session_id(),
        payload in arb_payload(),
        now_ms in arb_now_ms(),
    ) {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        store.save_session_meta(&session_id, &payload, now_ms)
            .expect("save should succeed");
        let first = store.delete_session_meta(&session_id, now_ms)
            .expect("delete should succeed");
        let second = store.delete_session_meta(&session_id, now_ms)
            .expect("delete should succeed");
        prop_assert_eq!(first, 1);
        prop_assert_eq!(second, 0);
    }
}

// ── Session ID validation ───────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn empty_session_id_always_rejected_save(
        whitespace in "[ \t\n\r]*",
        payload in arb_payload(),
        now_ms in arb_now_ms(),
    ) {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        let result = store.save_session_meta(&whitespace, &payload, now_ms);
        prop_assert_eq!(result, Err(SessionStoreError::EmptySessionId));
    }

    #[test]
    fn empty_session_id_always_rejected_load(
        whitespace in "[ \t\n\r]*",
        now_ms in arb_now_ms(),
    ) {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        let result = store.load_session_meta(&whitespace, now_ms);
        prop_assert_eq!(result, Err(SessionStoreError::EmptySessionId));
    }

    #[test]
    fn empty_session_id_always_rejected_delete(
        whitespace in "[ \t\n\r]*",
        now_ms in arb_now_ms(),
    ) {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        let result = store.delete_session_meta(&whitespace, now_ms);
        prop_assert_eq!(result, Err(SessionStoreError::EmptySessionId));
    }

    #[test]
    fn nonempty_session_id_always_accepted(
        session_id in arb_session_id(),
        payload in arb_payload(),
        now_ms in arb_now_ms(),
    ) {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        let result = store.save_session_meta(&session_id, &payload, now_ms);
        prop_assert!(result.is_ok(), "non-empty session ID should be accepted");
    }
}

// ── Multiple panes independence ─────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn multiple_panes_independent(
        ids in prop::collection::hash_set(1u64..=10_000u64, 2..=10),
        now_ms in arb_now_ms(),
    ) {
        let ids: Vec<u64> = ids.into_iter().collect();
        let mut store = SessionStore::new(SessionStoreConfig::default());

        // Save each pane with its ID as payload
        for &id in &ids {
            let payload = id.to_le_bytes().to_vec();
            store.save_pane_state(id, &payload, now_ms);
        }

        prop_assert_eq!(store.key_count(), ids.len());

        // All panes can be loaded with correct payload
        for &id in &ids {
            let expected = id.to_le_bytes().to_vec();
            let loaded = store.load_pane_state(id, now_ms)
                .expect("load should not fail");
            prop_assert_eq!(loaded, Some(expected));
        }

        // Delete first pane doesn't affect rest
        store.delete_pane_state(ids[0], now_ms);
        for &id in &ids[1..] {
            let expected = id.to_le_bytes().to_vec();
            let loaded = store.load_pane_state(id, now_ms)
                .expect("load should not fail");
            prop_assert_eq!(loaded, Some(expected));
        }
    }
}

// ── Overwrite resets TTL ────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn overwrite_resets_ttl(
        pane_id in arb_pane_id(),
        ttl in 50u64..=1000u64,
        now_ms in 1_000u64..=500_000_000u64,
    ) {
        let config = SessionStoreConfig {
            pane_state_ttl_ms: ttl,
            ..SessionStoreConfig::default()
        };
        let mut store = SessionStore::new(config);

        // First save at now_ms
        store.save_pane_state(pane_id, b"v1", now_ms);

        // Overwrite later, within first TTL window
        let rewrite_time = now_ms + ttl / 2;
        store.save_pane_state(pane_id, b"v2", rewrite_time);

        // Should still be readable at first TTL deadline
        // because TTL was reset by the overwrite
        let first_deadline = now_ms + ttl + 1;
        let loaded = store.load_pane_state(pane_id, first_deadline)
            .expect("load should not fail");
        // If rewrite_time + ttl > first_deadline, data is still alive
        if rewrite_time + ttl > first_deadline {
            prop_assert_eq!(loaded, Some(b"v2".to_vec()));
        }
    }
}

// ── Config roundtrip ────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn config_stored_faithfully(config in arb_config()) {
        let store = SessionStore::new(config);
        prop_assert_eq!(store.config(), config);
    }
}

// ── Operation sequence model ────────────────────────────────────────

#[derive(Debug, Clone)]
enum Op {
    SavePane(u64, Vec<u8>),
    LoadPane(u64),
    DeletePane(u64),
    SaveWindow(u64, Vec<u8>),
    LoadWindow(u64),
    DeleteWindow(u64),
    SaveSession(String, Vec<u8>),
    LoadSession(String),
    DeleteSession(String),
}

fn arb_op() -> impl Strategy<Value = Op> {
    let small_id = 0u64..=5u64;
    let small_payload = prop::collection::vec(any::<u8>(), 0..=16);

    prop_oneof![
        (small_id.clone(), small_payload.clone()).prop_map(|(id, p)| Op::SavePane(id, p)),
        small_id.clone().prop_map(Op::LoadPane),
        small_id.clone().prop_map(Op::DeletePane),
        (small_id.clone(), small_payload.clone()).prop_map(|(id, p)| Op::SaveWindow(id, p)),
        small_id.clone().prop_map(Op::LoadWindow),
        small_id.clone().prop_map(Op::DeleteWindow),
        ("[a-z]{1,4}", small_payload).prop_map(|(id, p)| Op::SaveSession(id, p)),
        "[a-z]{1,4}".prop_map(Op::LoadSession),
        "[a-z]{1,4}".prop_map(Op::DeleteSession),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn operation_sequence_never_panics(
        ops in prop::collection::vec(arb_op(), 1..=50),
    ) {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        let now_ms = 100_000u64;

        for op in ops {
            match op {
                Op::SavePane(id, payload) => {
                    store.save_pane_state(id, &payload, now_ms);
                }
                Op::LoadPane(id) => {
                    let _ = store.load_pane_state(id, now_ms);
                }
                Op::DeletePane(id) => {
                    let _ = store.delete_pane_state(id, now_ms);
                }
                Op::SaveWindow(id, payload) => {
                    store.save_window_layout(id, &payload, now_ms);
                }
                Op::LoadWindow(id) => {
                    let _ = store.load_window_layout(id, now_ms);
                }
                Op::DeleteWindow(id) => {
                    let _ = store.delete_window_layout(id, now_ms);
                }
                Op::SaveSession(id, payload) => {
                    let _ = store.save_session_meta(&id, &payload, now_ms);
                }
                Op::LoadSession(id) => {
                    let _ = store.load_session_meta(&id, now_ms);
                }
                Op::DeleteSession(id) => {
                    let _ = store.delete_session_meta(&id, now_ms);
                }
            }
        }

        // After any sequence, key_count should be non-negative (always true for usize)
        // and is_empty should be consistent with key_count
        let is_empty_flag = store.is_empty();
        let count = store.key_count();
        prop_assert_eq!(is_empty_flag, count == 0);
    }
}

// ── Model-checked operation sequence ────────────────────────────────
// Tracks expected state in a simple HashMap and verifies store matches.

use std::collections::HashMap;

#[derive(Debug, Clone)]
enum PaneOp {
    Save(u64, Vec<u8>),
    Load(u64),
    Delete(u64),
}

fn arb_pane_op() -> impl Strategy<Value = PaneOp> {
    let id = 0u64..=3u64;
    let payload = prop::collection::vec(any::<u8>(), 0..=8);

    prop_oneof![
        (id.clone(), payload).prop_map(|(id, p)| PaneOp::Save(id, p)),
        id.clone().prop_map(PaneOp::Load),
        id.prop_map(PaneOp::Delete),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn pane_ops_match_model(
        ops in prop::collection::vec(arb_pane_op(), 1..=30),
    ) {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        let mut model: HashMap<u64, Vec<u8>> = HashMap::new();
        let now_ms = 100_000u64;

        for op in ops {
            match op {
                PaneOp::Save(id, payload) => {
                    store.save_pane_state(id, &payload, now_ms);
                    model.insert(id, payload);
                }
                PaneOp::Load(id) => {
                    let actual = store.load_pane_state(id, now_ms)
                        .expect("load should not fail");
                    let expected = model.get(&id).cloned();
                    prop_assert_eq!(actual, expected, "mismatch for pane {}", id);
                }
                PaneOp::Delete(id) => {
                    store.delete_pane_state(id, now_ms);
                    model.remove(&id);
                }
            }
        }

        // Final consistency check: all model entries match store
        for (&id, expected) in &model {
            let actual = store.load_pane_state(id, now_ms)
                .expect("load should not fail");
            prop_assert_eq!(actual.as_ref(), Some(expected), "final mismatch for pane {}", id);
        }
    }
}
