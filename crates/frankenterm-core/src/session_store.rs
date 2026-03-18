#![forbid(unsafe_code)]

use fr_store::{Store, StoreError};

const KEYSPACE_VERSION: &str = "v1";

/// Configuration for `SessionStore` key TTL behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionStoreConfig {
    /// TTL for pane state records (`v1:pane:{pane_id}:state`).
    pub pane_state_ttl_ms: u64,
    /// TTL for window layout records (`v1:window:{window_id}:layout`).
    pub window_layout_ttl_ms: u64,
    /// TTL for session metadata records (`v1:session:{session_id}:meta`).
    pub session_meta_ttl_ms: u64,
    /// TTL for transient coordination keys (`v1:transient:{scope}:{id}`).
    pub transient_state_ttl_ms: u64,
}

impl Default for SessionStoreConfig {
    fn default() -> Self {
        Self {
            pane_state_ttl_ms: 86_400_000,
            window_layout_ttl_ms: 604_800_000,
            session_meta_ttl_ms: 86_400_000,
            transient_state_ttl_ms: 3_600_000,
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SessionStoreError {
    #[error("session id cannot be empty")]
    EmptySessionId,
    #[error("store error: {0:?}")]
    Store(StoreError),
}

/// Session coordination key/value surface backed by `fr_store::Store`.
#[derive(Debug, Default)]
pub struct SessionStore {
    config: SessionStoreConfig,
    store: Store,
}

impl SessionStore {
    #[must_use]
    pub fn new(config: SessionStoreConfig) -> Self {
        Self {
            config,
            store: Store::new(),
        }
    }

    #[must_use]
    pub fn config(&self) -> SessionStoreConfig {
        self.config
    }

    #[must_use]
    pub fn key_count(&mut self, now_ms: u64) -> usize {
        self.store.dbsize(now_ms)
    }

    #[must_use]
    pub fn is_empty(&mut self, now_ms: u64) -> bool {
        self.key_count(now_ms) == 0
    }

    pub fn save_pane_state(&mut self, pane_id: u64, state: &[u8], now_ms: u64) {
        self.store.set(
            pane_state_key(pane_id),
            state.to_vec(),
            Some(self.config.pane_state_ttl_ms),
            now_ms,
        );
    }

    pub fn load_pane_state(
        &mut self,
        pane_id: u64,
        now_ms: u64,
    ) -> Result<Option<Vec<u8>>, SessionStoreError> {
        self.store
            .get(pane_state_key(pane_id).as_slice(), now_ms)
            .map_err(SessionStoreError::Store)
    }

    pub fn delete_pane_state(&mut self, pane_id: u64, now_ms: u64) -> u64 {
        let key = pane_state_key(pane_id);
        self.store.del(std::slice::from_ref(&key), now_ms)
    }

    pub fn save_window_layout(&mut self, window_id: u64, layout: &[u8], now_ms: u64) {
        self.store.set(
            window_layout_key(window_id),
            layout.to_vec(),
            Some(self.config.window_layout_ttl_ms),
            now_ms,
        );
    }

    pub fn load_window_layout(
        &mut self,
        window_id: u64,
        now_ms: u64,
    ) -> Result<Option<Vec<u8>>, SessionStoreError> {
        self.store
            .get(window_layout_key(window_id).as_slice(), now_ms)
            .map_err(SessionStoreError::Store)
    }

    pub fn delete_window_layout(&mut self, window_id: u64, now_ms: u64) -> u64 {
        let key = window_layout_key(window_id);
        self.store.del(std::slice::from_ref(&key), now_ms)
    }

    pub fn save_session_meta(
        &mut self,
        session_id: &str,
        meta: &[u8],
        now_ms: u64,
    ) -> Result<(), SessionStoreError> {
        validate_session_id(session_id)?;
        self.store.set(
            session_meta_key(session_id),
            meta.to_vec(),
            Some(self.config.session_meta_ttl_ms),
            now_ms,
        );
        Ok(())
    }

    pub fn load_session_meta(
        &mut self,
        session_id: &str,
        now_ms: u64,
    ) -> Result<Option<Vec<u8>>, SessionStoreError> {
        validate_session_id(session_id)?;
        self.store
            .get(session_meta_key(session_id).as_slice(), now_ms)
            .map_err(SessionStoreError::Store)
    }

    pub fn delete_session_meta(
        &mut self,
        session_id: &str,
        now_ms: u64,
    ) -> Result<u64, SessionStoreError> {
        validate_session_id(session_id)?;
        let key = session_meta_key(session_id);
        Ok(self.store.del(std::slice::from_ref(&key), now_ms))
    }
}

fn pane_state_key(pane_id: u64) -> Vec<u8> {
    format!("{KEYSPACE_VERSION}:pane:{pane_id}:state").into_bytes()
}

fn window_layout_key(window_id: u64) -> Vec<u8> {
    format!("{KEYSPACE_VERSION}:window:{window_id}:layout").into_bytes()
}

fn session_meta_key(session_id: &str) -> Vec<u8> {
    format!("{KEYSPACE_VERSION}:session:{session_id}:meta").into_bytes()
}

fn validate_session_id(session_id: &str) -> Result<(), SessionStoreError> {
    if session_id.trim().is_empty() {
        return Err(SessionStoreError::EmptySessionId);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_state_roundtrip_and_delete() {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        let now_ms = 1_000_u64;
        store.save_pane_state(7, b"pane-state", now_ms);
        assert_eq!(
            store
                .load_pane_state(7, now_ms)
                .expect("load pane state should succeed"),
            Some(b"pane-state".to_vec())
        );
        assert_eq!(store.delete_pane_state(7, now_ms), 1);
        assert_eq!(
            store
                .load_pane_state(7, now_ms)
                .expect("load pane state should succeed"),
            None
        );
    }

    #[test]
    fn session_meta_requires_non_empty_id() {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        let result = store.save_session_meta("", b"value", 100);
        assert_eq!(result, Err(SessionStoreError::EmptySessionId));
    }

    #[test]
    fn window_layout_expires_after_ttl() {
        let config = SessionStoreConfig {
            window_layout_ttl_ms: 10,
            ..SessionStoreConfig::default()
        };
        let mut store = SessionStore::new(config);
        store.save_window_layout(9, b"layout", 100);

        assert_eq!(
            store
                .load_window_layout(9, 109)
                .expect("load should succeed"),
            Some(b"layout".to_vec())
        );
        assert_eq!(
            store
                .load_window_layout(9, 111)
                .expect("load should succeed"),
            None
        );
    }

    #[test]
    fn session_meta_roundtrip_and_delete() {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        let now_ms = 25_000_u64;

        store
            .save_session_meta("sess-1", b"meta", now_ms)
            .expect("save should succeed");
        assert_eq!(
            store
                .load_session_meta("sess-1", now_ms)
                .expect("load should succeed"),
            Some(b"meta".to_vec())
        );
        assert_eq!(
            store
                .delete_session_meta("sess-1", now_ms)
                .expect("delete should succeed"),
            1
        );
        assert_eq!(
            store
                .load_session_meta("sess-1", now_ms)
                .expect("load should succeed"),
            None
        );
    }

    // ── Config tests ────────────────────────────────────────────────

    #[test]
    fn default_config_has_expected_ttls() {
        let config = SessionStoreConfig::default();
        assert_eq!(config.pane_state_ttl_ms, 86_400_000);
        assert_eq!(config.window_layout_ttl_ms, 604_800_000);
        assert_eq!(config.session_meta_ttl_ms, 86_400_000);
        assert_eq!(config.transient_state_ttl_ms, 3_600_000);
    }

    #[test]
    fn custom_config_is_stored() {
        let config = SessionStoreConfig {
            pane_state_ttl_ms: 100,
            window_layout_ttl_ms: 200,
            session_meta_ttl_ms: 300,
            transient_state_ttl_ms: 400,
        };
        let store = SessionStore::new(config);
        assert_eq!(store.config(), config);
    }

    #[test]
    fn config_clone_and_copy() {
        let config = SessionStoreConfig::default();
        let cloned = config;
        assert_eq!(config, cloned);
    }

    // ── Empty store behavior ────────────────────────────────────────

    #[test]
    fn new_store_is_empty() {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        assert!(store.is_empty(0));
        assert_eq!(store.key_count(0), 0);
    }

    #[test]
    fn default_store_is_empty() {
        let mut store = SessionStore::default();
        assert!(store.is_empty(0));
        assert_eq!(store.key_count(0), 0);
    }

    #[test]
    fn load_missing_pane_returns_none() {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        assert_eq!(
            store.load_pane_state(999, 0).expect("load should succeed"),
            None
        );
    }

    #[test]
    fn load_missing_window_returns_none() {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        assert_eq!(
            store
                .load_window_layout(999, 0)
                .expect("load should succeed"),
            None
        );
    }

    #[test]
    fn load_missing_session_returns_none() {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        assert_eq!(
            store
                .load_session_meta("nonexistent", 0)
                .expect("load should succeed"),
            None
        );
    }

    #[test]
    fn delete_missing_pane_returns_zero() {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        assert_eq!(store.delete_pane_state(999, 0), 0);
    }

    #[test]
    fn delete_missing_window_returns_zero() {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        assert_eq!(store.delete_window_layout(999, 0), 0);
    }

    #[test]
    fn delete_missing_session_returns_zero() {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        assert_eq!(
            store
                .delete_session_meta("nonexistent", 0)
                .expect("delete should succeed"),
            0
        );
    }

    // ── Key count tracking ──────────────────────────────────────────

    #[test]
    fn key_count_increments_on_save() {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        store.save_pane_state(1, b"a", 100);
        assert_eq!(store.key_count(100), 1);
        assert!(!store.is_empty(100));

        store.save_window_layout(2, b"b", 100);
        assert_eq!(store.key_count(100), 2);

        store
            .save_session_meta("s1", b"c", 100)
            .expect("save should succeed");
        assert_eq!(store.key_count(100), 3);
    }

    #[test]
    fn key_count_decrements_on_delete() {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        store.save_pane_state(1, b"a", 100);
        store.save_pane_state(2, b"b", 100);
        assert_eq!(store.key_count(100), 2);

        store.delete_pane_state(1, 100);
        assert_eq!(store.key_count(100), 1);

        store.delete_pane_state(2, 100);
        assert_eq!(store.key_count(100), 0);
        assert!(store.is_empty(100));
    }

    // ── TTL expiry ──────────────────────────────────────────────────

    #[test]
    fn pane_state_expires_after_ttl() {
        let config = SessionStoreConfig {
            pane_state_ttl_ms: 50,
            ..SessionStoreConfig::default()
        };
        let mut store = SessionStore::new(config);
        store.save_pane_state(1, b"data", 1000);

        assert_eq!(
            store.load_pane_state(1, 1049).expect("load should succeed"),
            Some(b"data".to_vec())
        );
        assert_eq!(
            store.load_pane_state(1, 1051).expect("load should succeed"),
            None
        );
        assert_eq!(store.key_count(1051), 0);
        assert!(store.is_empty(1051));
    }

    #[test]
    fn session_meta_expires_after_ttl() {
        let config = SessionStoreConfig {
            session_meta_ttl_ms: 20,
            ..SessionStoreConfig::default()
        };
        let mut store = SessionStore::new(config);
        store
            .save_session_meta("s1", b"data", 500)
            .expect("save should succeed");

        assert_eq!(
            store
                .load_session_meta("s1", 519)
                .expect("load should succeed"),
            Some(b"data".to_vec())
        );
        assert_eq!(
            store
                .load_session_meta("s1", 521)
                .expect("load should succeed"),
            None
        );
        assert_eq!(store.key_count(521), 0);
        assert!(store.is_empty(521));
    }

    // ── Overwrite behavior ──────────────────────────────────────────

    #[test]
    fn pane_state_overwrite_replaces_value() {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        store.save_pane_state(1, b"first", 100);
        store.save_pane_state(1, b"second", 200);

        assert_eq!(
            store.load_pane_state(1, 200).expect("load should succeed"),
            Some(b"second".to_vec())
        );
        assert_eq!(store.key_count(200), 1);
    }

    #[test]
    fn window_layout_overwrite_replaces_value() {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        store.save_window_layout(5, b"layout-v1", 100);
        store.save_window_layout(5, b"layout-v2", 200);

        assert_eq!(
            store
                .load_window_layout(5, 200)
                .expect("load should succeed"),
            Some(b"layout-v2".to_vec())
        );
        assert_eq!(store.key_count(200), 1);
    }

    #[test]
    fn session_meta_overwrite_replaces_value() {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        store
            .save_session_meta("s1", b"v1", 100)
            .expect("save should succeed");
        store
            .save_session_meta("s1", b"v2", 200)
            .expect("save should succeed");

        assert_eq!(
            store
                .load_session_meta("s1", 200)
                .expect("load should succeed"),
            Some(b"v2".to_vec())
        );
        assert_eq!(store.key_count(200), 1);
    }

    // ── Validation ──────────────────────────────────────────────────

    #[test]
    fn session_meta_rejects_whitespace_only_id() {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        assert_eq!(
            store.save_session_meta("   ", b"val", 100),
            Err(SessionStoreError::EmptySessionId)
        );
    }

    #[test]
    fn load_session_meta_rejects_empty_id() {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        assert_eq!(
            store.load_session_meta("", 100),
            Err(SessionStoreError::EmptySessionId)
        );
    }

    #[test]
    fn delete_session_meta_rejects_empty_id() {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        assert_eq!(
            store.delete_session_meta("", 100),
            Err(SessionStoreError::EmptySessionId)
        );
    }

    #[test]
    fn load_session_meta_rejects_whitespace_id() {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        assert_eq!(
            store.load_session_meta("\t\n", 100),
            Err(SessionStoreError::EmptySessionId)
        );
    }

    // ── Edge cases ──────────────────────────────────────────────────

    #[test]
    fn pane_id_zero() {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        store.save_pane_state(0, b"zero", 100);
        assert_eq!(
            store.load_pane_state(0, 100).expect("load should succeed"),
            Some(b"zero".to_vec())
        );
    }

    #[test]
    fn pane_id_max() {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        store.save_pane_state(u64::MAX, b"max", 100);
        assert_eq!(
            store
                .load_pane_state(u64::MAX, 100)
                .expect("load should succeed"),
            Some(b"max".to_vec())
        );
    }

    #[test]
    fn window_id_zero() {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        store.save_window_layout(0, b"zero", 100);
        assert_eq!(
            store
                .load_window_layout(0, 100)
                .expect("load should succeed"),
            Some(b"zero".to_vec())
        );
    }

    #[test]
    fn empty_value_roundtrips() {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        store.save_pane_state(1, b"", 100);
        assert_eq!(
            store.load_pane_state(1, 100).expect("load should succeed"),
            Some(b"".to_vec())
        );
    }

    #[test]
    fn large_value_roundtrips() {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        let big = vec![0xAB_u8; 1_000_000];
        store.save_pane_state(1, &big, 100);
        assert_eq!(
            store.load_pane_state(1, 100).expect("load should succeed"),
            Some(big)
        );
    }

    // ── Multiple key types coexist ──────────────────────────────────

    #[test]
    fn different_key_types_independent() {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        store.save_pane_state(1, b"pane", 100);
        store.save_window_layout(1, b"window", 100);
        store
            .save_session_meta("1", b"session", 100)
            .expect("save should succeed");

        assert_eq!(store.key_count(100), 3);

        assert_eq!(
            store.load_pane_state(1, 100).expect("load should succeed"),
            Some(b"pane".to_vec())
        );
        assert_eq!(
            store
                .load_window_layout(1, 100)
                .expect("load should succeed"),
            Some(b"window".to_vec())
        );
        assert_eq!(
            store
                .load_session_meta("1", 100)
                .expect("load should succeed"),
            Some(b"session".to_vec())
        );
    }

    #[test]
    fn multiple_panes_independent() {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        store.save_pane_state(1, b"first", 100);
        store.save_pane_state(2, b"second", 100);
        store.save_pane_state(3, b"third", 100);

        assert_eq!(
            store.load_pane_state(1, 100).expect("load should succeed"),
            Some(b"first".to_vec())
        );
        assert_eq!(
            store.load_pane_state(2, 100).expect("load should succeed"),
            Some(b"second".to_vec())
        );
        assert_eq!(
            store.load_pane_state(3, 100).expect("load should succeed"),
            Some(b"third".to_vec())
        );

        store.delete_pane_state(2, 100);
        assert_eq!(
            store.load_pane_state(1, 100).expect("load should succeed"),
            Some(b"first".to_vec())
        );
        assert_eq!(
            store.load_pane_state(2, 100).expect("load should succeed"),
            None
        );
        assert_eq!(
            store.load_pane_state(3, 100).expect("load should succeed"),
            Some(b"third".to_vec())
        );
    }

    // ── Key format tests ────────────────────────────────────────────

    #[test]
    fn pane_state_key_format() {
        let key = pane_state_key(42);
        assert_eq!(key, b"v1:pane:42:state");
    }

    #[test]
    fn window_layout_key_format() {
        let key = window_layout_key(99);
        assert_eq!(key, b"v1:window:99:layout");
    }

    #[test]
    fn session_meta_key_format() {
        let key = session_meta_key("my-session");
        assert_eq!(key, b"v1:session:my-session:meta");
    }

    // ── Delete idempotency ──────────────────────────────────────────

    #[test]
    fn double_delete_pane_returns_zero() {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        store.save_pane_state(1, b"data", 100);
        assert_eq!(store.delete_pane_state(1, 100), 1);
        assert_eq!(store.delete_pane_state(1, 100), 0);
    }

    #[test]
    fn double_delete_session_returns_zero() {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        store
            .save_session_meta("s1", b"data", 100)
            .expect("save should succeed");
        assert_eq!(
            store
                .delete_session_meta("s1", 100)
                .expect("delete should succeed"),
            1
        );
        assert_eq!(
            store
                .delete_session_meta("s1", 100)
                .expect("delete should succeed"),
            0
        );
    }

    // ── Error display ───────────────────────────────────────────────

    #[test]
    fn error_display_empty_session_id() {
        let err = SessionStoreError::EmptySessionId;
        assert_eq!(format!("{err}"), "session id cannot be empty");
    }

    #[test]
    fn session_id_with_special_chars() {
        let mut store = SessionStore::new(SessionStoreConfig::default());
        store
            .save_session_meta("a/b:c-d.e_f", b"special", 100)
            .expect("save should succeed");
        assert_eq!(
            store
                .load_session_meta("a/b:c-d.e_f", 100)
                .expect("load should succeed"),
            Some(b"special".to_vec())
        );
    }
}
