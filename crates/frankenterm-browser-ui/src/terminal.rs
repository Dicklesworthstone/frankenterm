//! Browser terminal codec plus transactional terminal model. Network I/O and
//! browser painting remain host-owned; model application is owned here.

use serde::de::DeserializeOwned;

use crate::terminal_model::{ApplyOutcome, TerminalModel, bounded_json};
use frankenterm_terminal_wire::{
    BrowserSession, ClientMessage, Dimensions, Id, LeaseDisposition, MAX_FRAME_BYTES,
    PaneEventKind, Scope, ServerMessage, WireError, WireU64, decode_client, decode_server,
    encode_server,
};

#[derive(Debug)]
pub struct TerminalClient {
    inner: BrowserSession,
    model: TerminalModel,
}

fn decode_host_value<T: DeserializeOwned>(json: &str) -> Result<T, WireError> {
    if json.len() > MAX_FRAME_BYTES {
        return Err(WireError::FrameTooLarge);
    }
    serde_json::from_str(json).map_err(|_| WireError::Malformed)
}

impl TerminalClient {
    pub fn from_hello_frame(bytes: &[u8]) -> Result<Self, WireError> {
        let frame = decode_client(bytes)?;
        let ClientMessage::Hello(hello) = frame.message else {
            return Err(WireError::NotNegotiated);
        };
        Ok(Self {
            inner: BrowserSession::new(hello)?,
            model: TerminalModel::new(),
        })
    }

    pub fn hello_frame(&mut self) -> Result<Vec<u8>, WireError> {
        self.inner.hello_frame()
    }

    pub fn accept_welcome(&mut self, bytes: &[u8]) -> Result<(), WireError> {
        self.inner.accept_welcome(bytes)
    }

    /// Decode-only compatibility API. It does not create state or enable ACKs.
    pub fn decode_server(&self, bytes: &[u8]) -> Result<String, WireError> {
        let frame = self.inner.decode_response(bytes)?;
        String::from_utf8(encode_server(&frame)?).map_err(|_| WireError::Malformed)
    }

    /// Apply one network frame. A candidate codec watermark is validated before
    /// model mutation and committed only if that exact publication was installed.
    /// Candidate cloning copies bounded session metadata, never the screen.
    pub fn ingest_server(&mut self, bytes: &[u8], now_ms: u64) -> Result<ApplyOutcome, WireError> {
        self.model.expire_snapshots(now_ms);
        let frame = match self.inner.decode_response(bytes) {
            Ok(frame) => frame,
            Err(WireError::StaleSequence) => {
                // The session checker reaches StaleSequence only after checking
                // session, capability and exact scope. The model must distinguish
                // an old duplicate from a forward gap and request recovery.
                decode_server(bytes)?
            }
            Err(error) => return Err(error),
        };
        let mut candidate = self.inner.clone();
        let mut changes_codec = false;
        match &frame.message {
            ServerMessage::SnapshotEnd(value) => {
                let previous = self.model.watermark(&value.scope)?;
                if value.sequence >= previous {
                    candidate.record_delivery(&value.scope, previous, value.sequence)?;
                    changes_codec = true;
                }
            }
            ServerMessage::Delta(value) => {
                let previous = self.model.watermark(&value.scope)?;
                if value.sequence > previous {
                    candidate.record_delivery(&value.scope, previous, value.sequence)?;
                    changes_codec = true;
                }
            }
            ServerMessage::Unsubscribed(value) => {
                candidate.unbind_scope(&value.scope)?;
                changes_codec = true;
            }
            ServerMessage::PaneEvent(value) => match value.event {
                PaneEventKind::Closed => {
                    candidate.unbind_scope(&value.scope)?;
                    changes_codec = true;
                }
                PaneEventKind::Resized => {
                    candidate.update_dimensions(
                        &value.scope,
                        value.dimensions.clone().ok_or(WireError::InvalidState)?,
                    )?;
                    changes_codec = true;
                }
                _ => {}
            },
            ServerMessage::LeaseState(value) => {
                if let Some(fence) = value.fence {
                    let active = matches!(
                        value.disposition,
                        LeaseDisposition::Granted | LeaseDisposition::Renewed
                    );
                    candidate.record_lease(&value.scope, fence, active)?;
                    changes_codec = true;
                }
            }
            _ => {}
        }
        let outcome = self.model.ingest(frame.message, bytes.len(), now_ms)?;
        if changes_codec
            && !matches!(
                &outcome,
                ApplyOutcome::Duplicate { .. } | ApplyOutcome::Staged { .. }
            )
        {
            self.inner = candidate;
        }
        Ok(outcome)
    }

    pub fn apply_server_json(&mut self, bytes: &[u8], now_ms: u64) -> Result<String, WireError> {
        bounded_json(&self.ingest_server(bytes, now_ms)?)
    }

    pub fn model(&self) -> &TerminalModel {
        &self.model
    }

    /// Even generic ACK construction is checked against the actual installed
    /// model. Host-supplied counters cannot manufacture a baseline.
    pub fn encode_request(&mut self, message_json: &str) -> Result<Vec<u8>, WireError> {
        let message: ClientMessage = decode_host_value(message_json)?;
        self.encode_message(message)
    }

    fn encode_message(&mut self, message: ClientMessage) -> Result<Vec<u8>, WireError> {
        if let ClientMessage::AppliedAck(ack) = &message {
            self.model.validate_ack(ack)?;
        }
        self.model.validate_recovery_request(&message)?;
        let mut candidate = self.inner.clone();
        let bytes = candidate.encode_request(message.clone())?;
        match &message {
            ClientMessage::AppliedAck(ack) => self.model.acknowledge_encoded(ack)?,
            ClientMessage::SnapshotRequest(value) => self.model.recovery_encoded(&value.scope)?,
            ClientMessage::Resync(value) => self.model.recovery_encoded(&value.scope)?,
            _ => {}
        }
        self.inner = candidate;
        Ok(bytes)
    }

    /// `minimum` is the server's advertised lower stream cursor, not an applied
    /// baseline. No ACK, including zero, is enabled until a snapshot commits.
    pub fn bind_scope(
        &mut self,
        scope_json: &str,
        dimensions_json: &str,
        minimum: &str,
    ) -> Result<(), WireError> {
        let scope: Scope = decode_host_value(scope_json)?;
        let dimensions: Dimensions = decode_host_value(dimensions_json)?;
        let minimum = WireU64::parse(minimum)?;
        let mut candidate = self.inner.clone();
        candidate.bind_scope(scope.clone(), dimensions.clone(), minimum)?;
        self.model.bind_scope(scope, dimensions, minimum)?;
        self.inner = candidate;
        Ok(())
    }

    pub fn unbind_scope(&mut self, scope_json: &str) -> Result<(), WireError> {
        let scope: Scope = decode_host_value(scope_json)?;
        let mut candidate = self.inner.clone();
        candidate.unbind_scope(&scope)?;
        self.model.unbind_scope(&scope)?;
        self.inner = candidate;
        Ok(())
    }

    pub fn update_dimensions(
        &mut self,
        scope_json: &str,
        dimensions_json: &str,
    ) -> Result<(), WireError> {
        let scope: Scope = decode_host_value(scope_json)?;
        let dimensions: Dimensions = decode_host_value(dimensions_json)?;
        let mut candidate = self.inner.clone();
        candidate.update_dimensions(&scope, dimensions.clone())?;
        self.model.update_dimensions(&scope, dimensions)?;
        self.inner = candidate;
        Ok(())
    }

    /// Compatibility check only: the model already has to contain this exact
    /// completed transition. This method cannot advance delivery or ACK authority.
    pub fn record_delivery(
        &mut self,
        scope_json: &str,
        previous: &str,
        sequence: &str,
    ) -> Result<(), WireError> {
        let scope: Scope = decode_host_value(scope_json)?;
        self.model
            .verify_delivery(&scope, WireU64::parse(previous)?, WireU64::parse(sequence)?)
    }

    /// Mirror an independently established lease; the real server still checks
    /// permissions and its authoritative lease before any pane effect.
    pub fn record_lease(
        &mut self,
        scope_json: &str,
        fence: &str,
        active: bool,
    ) -> Result<(), WireError> {
        let scope: Scope = decode_host_value(scope_json)?;
        self.inner
            .record_lease(&scope, WireU64::parse(fence)?, active)
    }

    pub fn take_applied_ack(&mut self, scope_json: &str) -> Result<Option<Vec<u8>>, WireError> {
        let scope: Scope = decode_host_value(scope_json)?;
        let Some(ack) = self.model.pending_ack(&scope)? else {
            return Ok(None);
        };
        self.encode_message(ClientMessage::AppliedAck(ack))
            .map(Some)
    }

    pub fn take_recovery_request(
        &mut self,
        scope_json: &str,
        request_id: &str,
        now_ms: u64,
    ) -> Result<Option<Vec<u8>>, WireError> {
        self.model.expire_snapshots(now_ms);
        let scope: Scope = decode_host_value(scope_json)?;
        let Some(message) = self.model.recovery_request(&scope, Id::new(request_id)?)? else {
            return Ok(None);
        };
        self.encode_message(message).map(Some)
    }

    pub fn expire_snapshots(&mut self, now_ms: u64) -> usize {
        self.model.expire_snapshots(now_ms)
    }

    pub fn abort_snapshot(&mut self, scope_json: &str) -> Result<(), WireError> {
        self.model.abort_snapshot(&decode_host_value(scope_json)?)
    }

    pub fn screen_metadata_json(&self, scope_json: &str) -> Result<String, WireError> {
        self.model.metadata_json(&decode_host_value(scope_json)?)
    }

    pub fn screen_row_json(&self, scope_json: &str, row: u16) -> Result<String, WireError> {
        self.model.row_json(&decode_host_value(scope_json)?, row)
    }

    pub fn screen_text_page(
        &self,
        scope_json: &str,
        first_row: u16,
        row_count: u16,
    ) -> Result<String, WireError> {
        self.model
            .text_page(&decode_host_value(scope_json)?, first_row, row_count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frankenterm_terminal_wire::{
        Capabilities, ClientFrame, Hello, Ping, ServerSession, Sha256,
    };

    #[test]
    fn native_and_browser_consumer_share_exact_identifiers_and_negotiation() {
        let host = Id::new("host-1").unwrap();
        let asset = Sha256::new(&"a".repeat(64)).unwrap();
        let hello = ClientFrame::new(
            None,
            ClientMessage::Hello(Hello {
                host: host.clone(),
                hello_id: Id::new("hello-9007199254740993").unwrap(),
                asset: asset.clone(),
                required: Capabilities::new(),
                optional: Capabilities::new(),
            }),
        )
        .unwrap();
        let mut browser = TerminalClient::from_hello_frame(
            &frankenterm_terminal_wire::encode_client(&hello).unwrap(),
        )
        .unwrap();
        let mut server = ServerSession::new(
            host,
            Id::new("session-1").unwrap(),
            asset,
            Capabilities::new(),
        )
        .unwrap();
        assert_eq!(
            browser.encode_request(r#"{"type":"ping","payload":{"nonce":"9007199254740993"}}"#),
            Err(WireError::NotNegotiated)
        );
        let welcome = server.negotiate(&browser.hello_frame().unwrap()).unwrap();
        browser.accept_welcome(&welcome).unwrap();
        let request = browser
            .encode_request(r#"{"type":"ping","payload":{"nonce":"9007199254740993"}}"#)
            .unwrap();
        assert_eq!(
            server.decode_request(&request).unwrap().message,
            ClientMessage::Ping(Ping {
                nonce: WireU64(9_007_199_254_740_993)
            })
        );
        let response = server
            .encode_response(ServerMessage::Pong(Ping {
                nonce: WireU64(u64::MAX),
            }))
            .unwrap();
        assert!(
            browser
                .decode_server(&response)
                .unwrap()
                .contains("\"18446744073709551615\"")
        );
        assert_eq!(browser.decode_server(b"\xff"), Err(WireError::Malformed));
    }
}
