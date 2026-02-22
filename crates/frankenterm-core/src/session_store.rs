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
    Store(#[from] StoreError),
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
    pub fn key_count(&self) -> usize {
        self.store.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.store.is_empty()
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
            .map_err(Into::into)
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
            .map_err(Into::into)
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
            .map_err(Into::into)
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
}
