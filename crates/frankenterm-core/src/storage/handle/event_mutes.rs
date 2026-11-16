//! [ft-aw52a / ft-dn2tu Phase 6] StorageHandle event-mute methods.
//!
//! Beachhead extraction for the StorageHandle impl-split: 8 cohesive
//! async methods that govern persistent event mute records. Reference
//! types and the writer command surface stay in `storage.rs`; this
//! file only holds the `impl StorageHandle { … }` block.

use std::sync::Arc;

use super::super::{
    EventMuteRecord, StorageHandle, WriteCommand, list_active_mutes_backend, pooled_backend,
    query_event_mute_backend,
};
use crate::error::Result;
use crate::runtime_async::oneshot;

impl StorageHandle {
    /// Add or update a persistent event mute by identity key.
    pub async fn add_event_mute(&self, record: EventMuteRecord) -> Result<()> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.add_event_mute_with_cx(&cx, record).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`add_event_mute`].
    /// Tick 169: inlined to route the mpsc send through `send_with_cx`.
    pub async fn add_event_mute_with_cx(
        &self,
        cx: &crate::cx::Cx,
        record: EventMuteRecord,
    ) -> Result<()> {
        Self::checkpoint_storage_operation(cx, "add_event_mute")?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::UpsertEventMute {
                    record,
                    respond: tx,
                },
            )
            .await
            .map_err(|error| Self::writer_send_error("add_event_mute", error))?;
        Self::recv_writer_response(rx).await
    }

    /// Remove a persistent event mute by identity key.
    pub async fn remove_event_mute(&self, identity_key: &str) -> Result<bool> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.remove_event_mute_with_cx(&cx, identity_key).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`remove_event_mute`].
    pub async fn remove_event_mute_with_cx(
        &self,
        cx: &crate::cx::Cx,
        identity_key: &str,
    ) -> Result<bool> {
        Self::checkpoint_storage_operation(cx, "remove_event_mute")?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::DeleteEventMute {
                    identity_key: identity_key.to_string(),
                    respond: tx,
                },
            )
            .await
            .map_err(|error| Self::writer_send_error("remove_event_mute", error))?;

        Self::recv_writer_response(rx).await
    }

    /// Check whether an identity key is muted (and not expired).
    pub async fn is_event_muted(&self, identity_key: &str, now_ms: i64) -> Result<bool> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.is_event_muted_with_cx(&cx, identity_key, now_ms).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`is_event_muted`].
    pub async fn is_event_muted_with_cx(
        &self,
        cx: &crate::cx::Cx,
        identity_key: &str,
        now_ms: i64,
    ) -> Result<bool> {
        Self::checkpoint_storage_operation(cx, "is_event_muted")?;
        let db_path = Arc::clone(&self.db_path);
        let identity_key = identity_key.to_string();

        Self::spawn_blocking_storage_with_cx(cx, move || {
            pooled_backend(db_path.as_str(), |backend| {
                query_event_mute_backend(backend, &identity_key, now_ms)
            })
        })
        .await
    }

    /// List all active (non-expired) mutes.
    pub async fn list_active_mutes(&self, now_ms: i64) -> Result<Vec<EventMuteRecord>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.list_active_mutes_with_cx(&cx, now_ms).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`list_active_mutes`].
    pub async fn list_active_mutes_with_cx(
        &self,
        cx: &crate::cx::Cx,
        now_ms: i64,
    ) -> Result<Vec<EventMuteRecord>> {
        Self::checkpoint_storage_operation(cx, "list_active_mutes")?;
        let db_path = Arc::clone(&self.db_path);

        Self::spawn_blocking_storage_with_cx(cx, move || {
            pooled_backend(db_path.as_str(), |backend| {
                list_active_mutes_backend(backend, now_ms)
            })
        })
        .await
    }
}
