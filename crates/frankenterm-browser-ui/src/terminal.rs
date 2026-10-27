//! Browser terminal application codec. Network I/O, snapshot assembly, terminal
//! model application and rendering remain owned by their respective hosts.
//! Incoming bytes pass through the shared Rust decoder before any JS parsing.

use serde::de::DeserializeOwned;

use frankenterm_terminal_wire::{
    BrowserSession, ClientMessage, Dimensions, Scope, WireError, WireU64,
    MAX_FRAME_BYTES, decode_client, encode_server,
};

#[derive(Debug, Clone)]
pub struct TerminalClient {
    inner: BrowserSession,
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
        Ok(Self { inner: BrowserSession::new(hello)? })
    }

    pub fn hello_frame(&mut self) -> Result<Vec<u8>, WireError> {
        self.inner.hello_frame()
    }

    pub fn accept_welcome(&mut self, bytes: &[u8]) -> Result<(), WireError> {
        self.inner.accept_welcome(bytes)
    }

    /// Canonical, validated JSON for the host model; all u64 values stay strings.
    /// No ACK is emitted merely because this method decoded a frame.
    pub fn decode_server(&self, bytes: &[u8]) -> Result<String, WireError> {
        let frame = self.inner.decode_response(bytes)?;
        String::from_utf8(encode_server(&frame)?).map_err(|_| WireError::Malformed)
    }

    /// The payload uses the shared Rust ClientMessage schema. Rust supplies the
    /// negotiated session/envelope rather than a duplicate JavaScript model.
    pub fn encode_request(&mut self, message_json: &str) -> Result<Vec<u8>, WireError> {
        let message: ClientMessage = decode_host_value(message_json)?;
        self.inner.encode_request(message)
    }

    /// Call after the host established the exact authorized scope and baseline.
    /// This configures a validator; it is not a subscription or permission effect.
    pub fn bind_scope(&mut self, scope_json: &str, dimensions_json: &str, delivered: &str) -> Result<(), WireError> {
        let scope: Scope = decode_host_value(scope_json)?;
        let dimensions: Dimensions = decode_host_value(dimensions_json)?;
        self.inner.bind_scope(scope, dimensions, WireU64::parse(delivered)?)
    }

    pub fn unbind_scope(&mut self, scope_json: &str) -> Result<(), WireError> {
        let scope: Scope = decode_host_value(scope_json)?;
        self.inner.unbind_scope(&scope)
    }

    pub fn update_dimensions(&mut self, scope_json: &str, dimensions_json: &str) -> Result<(), WireError> {
        let scope: Scope = decode_host_value(scope_json)?;
        let dimensions: Dimensions = decode_host_value(dimensions_json)?;
        self.inner.update_dimensions(&scope, dimensions)
    }

    /// Called after full model application, including complete snapshot coverage.
    /// An ACK is a separate explicit request, never an automatic retry.
    pub fn record_delivery(&mut self, scope_json: &str, previous: &str, sequence: &str) -> Result<(), WireError> {
        let scope: Scope = decode_host_value(scope_json)?;
        self.inner.record_delivery(&scope, WireU64::parse(previous)?, WireU64::parse(sequence)?)
    }

    pub fn record_lease(&mut self, scope_json: &str, fence: &str, active: bool) -> Result<(), WireError> {
        let scope: Scope = decode_host_value(scope_json)?;
        self.inner.record_lease(&scope, WireU64::parse(fence)?, active)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frankenterm_terminal_wire::{
        Capabilities, ClientFrame, Hello, Id, Ping, ServerMessage, ServerSession, Sha256,
    };

    #[test]
    fn native_and_browser_consumer_share_exact_identifiers_and_negotiation() {
        let host = Id::new("host-1").unwrap();
        let asset = Sha256::new(&"a".repeat(64)).unwrap();
        let hello = ClientFrame::new(None, ClientMessage::Hello(Hello {
            host: host.clone(),
            hello_id: Id::new("hello-9007199254740993").unwrap(),
            asset: asset.clone(),
            required: Capabilities::new(),
            optional: Capabilities::new(),
        })).unwrap();
        let mut browser = TerminalClient::from_hello_frame(
            &frankenterm_terminal_wire::encode_client(&hello).unwrap(),
        ).unwrap();
        let mut server = ServerSession::new(
            host, Id::new("session-1").unwrap(), asset, Capabilities::new(),
        ).unwrap();
        assert_eq!(
            browser.encode_request(r#"{"type":"ping","payload":{"nonce":"9007199254740993"}}"#),
            Err(WireError::NotNegotiated),
        );
        let welcome = server.negotiate(&browser.hello_frame().unwrap()).unwrap();
        browser.accept_welcome(&welcome).unwrap();
        let request = browser.encode_request(
            r#"{"type":"ping","payload":{"nonce":"9007199254740993"}}"#,
        ).unwrap();
        assert_eq!(
            server.decode_request(&request).unwrap().message,
            ClientMessage::Ping(Ping { nonce: WireU64(9_007_199_254_740_993) }),
        );
        let response = server.encode_response(ServerMessage::Pong(Ping {
            nonce: WireU64(u64::MAX),
        })).unwrap();
        let canonical = browser.decode_server(&response).unwrap();
        assert!(canonical.contains("\"18446744073709551615\""));
        assert_eq!(browser.decode_server(b"\xff"), Err(WireError::Malformed));
    }
}
