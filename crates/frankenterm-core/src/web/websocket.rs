//! Typed application protocol seam for the future authenticated terminal socket.
//!
//! This component registers no route, opens no listener, and performs no mux
//! effects. The transport must authenticate before negotiation, authorize every
//! returned requirement, and fence the actual pane/control effect.

use frankenterm_terminal_wire::{
    Capabilities, ClientMessage, Dimensions, Id, RequiredAuthority, Scope,
    ServerMessage, ServerSession, Sha256, WireError, WireU64,
};

/// A request that passed wire/identity checks, still requiring real grants.
#[derive(Debug, Clone)]
pub struct TerminalRequest {
    pub message: ClientMessage,
    pub required_authority: RequiredAuthority,
}

/// Native consumer of the same bounded codec exported by the WASM frontend.
#[derive(Debug, Clone)]
pub struct TerminalWireSession {
    inner: ServerSession,
}

impl TerminalWireSession {
    pub fn new(
        host: Id,
        session: Id,
        asset: Sha256,
        capabilities: Capabilities,
    ) -> Result<Self, WireError> {
        Ok(Self { inner: ServerSession::new(host, session, asset, capabilities)? })
    }

    pub fn negotiate(&mut self, hello: &[u8]) -> Result<Vec<u8>, WireError> {
        self.inner.negotiate(hello)
    }

    pub fn decode_request(&mut self, bytes: &[u8]) -> Result<TerminalRequest, WireError> {
        let frame = self.inner.decode_request(bytes)?;
        Ok(TerminalRequest {
            required_authority: frame.message.required_authority(),
            message: frame.message,
        })
    }

    pub fn encode_response(&self, message: ServerMessage) -> Result<Vec<u8>, WireError> {
        self.inner.encode_response(message)
    }

    /// Install only an identity verified by the actual mux/grant owner.
    pub fn bind_scope(&mut self, scope: Scope, dimensions: Dimensions, delivered: WireU64) -> Result<(), WireError> {
        self.inner.bind_scope(scope, dimensions, delivered)
    }

    pub fn unbind_scope(&mut self, scope: &Scope) -> Result<(), WireError> {
        self.inner.unbind_scope(scope)
    }

    /// Mirror an actual resize without discarding delivery or lease watermarks.
    pub fn update_dimensions(&mut self, scope: &Scope, dimensions: Dimensions) -> Result<(), WireError> {
        self.inner.update_dimensions(scope, dimensions)
    }

    /// Encoding is not delivery. The transport owner records complete delivery.
    pub fn record_delivery(&mut self, scope: &Scope, previous: WireU64, sequence: WireU64) -> Result<(), WireError> {
        self.inner.record_delivery(scope, previous, sequence)
    }

    /// This mirrors an actual authority decision; it does not grant a lease.
    pub fn record_lease(&mut self, scope: &Scope, fence: WireU64, active: bool) -> Result<(), WireError> {
        self.inner.record_lease(scope, fence, active)
    }
}
