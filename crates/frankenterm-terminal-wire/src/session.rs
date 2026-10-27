use crate::{
    AppliedAck, Capabilities, Capability, ClientFrame, ClientMessage, Dimensions, Hello, Id,
    LeaseDisposition, Scope, ServerFrame, ServerMessage, Sha256, Welcome, WireError, WireU64,
    MAX_SUBSCRIPTIONS, decode_client, decode_server, encode_client, encode_server,
    validate_capabilities,
};

#[derive(Debug, Clone)]
struct Binding {
    scope: Scope,
    dimensions: Dimensions,
    delivered: WireU64,
    applied: WireU64,
    lease_fence: WireU64,
    lease_active: bool,
}

#[derive(Debug, Clone)]
struct SessionState {
    host: Id,
    session: Id,
    capabilities: Capabilities,
    bindings: Vec<Binding>,
}

impl SessionState {
    fn require(&self, capability: Option<Capability>) -> Result<(), WireError> {
        if capability.is_some_and(|value| !self.capabilities.as_slice().contains(&value)) {
            return Err(WireError::UnsupportedCapability);
        }
        Ok(())
    }

    fn binding(&self, scope: &Scope) -> Result<&Binding, WireError> {
        if scope.identity.host != self.host {
            return Err(WireError::IdentityMismatch);
        }
        self.bindings.iter().find(|value| value.scope == *scope).ok_or(WireError::StaleScope)
    }

    fn binding_mut(&mut self, scope: &Scope) -> Result<&mut Binding, WireError> {
        if scope.identity.host != self.host {
            return Err(WireError::IdentityMismatch);
        }
        self.bindings.iter_mut().find(|value| value.scope == *scope).ok_or(WireError::StaleScope)
    }

    fn bind(
        &mut self,
        scope: Scope,
        dimensions: Dimensions,
        delivered: WireU64,
    ) -> Result<(), WireError> {
        dimensions.validate()?;
        if scope.identity.host != self.host {
            return Err(WireError::IdentityMismatch);
        }
        // A new incarnation/epoch cannot silently overwrite a live binding.
        // Its old exact scope must first be explicitly removed by the owner.
        if self.bindings.iter().any(|value| value.scope.identity.pane == scope.identity.pane) {
            return Err(WireError::StaleScope);
        }
        if self.bindings.len() >= MAX_SUBSCRIPTIONS {
            return Err(WireError::LimitExceeded);
        }
        self.bindings.push(Binding {
            scope,
            dimensions,
            delivered,
            applied: WireU64(0),
            lease_fence: WireU64(0),
            lease_active: false,
        });
        Ok(())
    }

    fn unbind(&mut self, scope: &Scope) -> Result<(), WireError> {
        self.binding(scope)?;
        self.bindings.retain(|value| value.scope != *scope);
        Ok(())
    }

    fn update_dimensions(&mut self, scope: &Scope, dimensions: Dimensions) -> Result<(), WireError> {
        dimensions.validate()?;
        self.binding_mut(scope)?.dimensions = dimensions;
        Ok(())
    }

    fn record_delivery(
        &mut self,
        scope: &Scope,
        previous: WireU64,
        sequence: WireU64,
    ) -> Result<(), WireError> {
        let binding = self.binding_mut(scope)?;
        if binding.delivered != previous || sequence < previous {
            return Err(WireError::StaleSequence);
        }
        binding.delivered = sequence;
        Ok(())
    }

    fn record_lease(&mut self, scope: &Scope, fence: WireU64, active: bool) -> Result<(), WireError> {
        self.require(Some(Capability::Control))?;
        let binding = self.binding_mut(scope)?;
        if fence.0 == 0 || fence < binding.lease_fence
            || (active && !binding.lease_active && fence == binding.lease_fence)
        {
            return Err(WireError::StaleLease);
        }
        binding.lease_fence = fence;
        binding.lease_active = active;
        Ok(())
    }

    fn check_lease(&self, scope: &Scope, fence: WireU64) -> Result<(), WireError> {
        self.require(Some(Capability::Control))?;
        let binding = self.binding(scope)?;
        if !binding.lease_active || binding.lease_fence != fence {
            return Err(WireError::StaleLease);
        }
        Ok(())
    }

    fn check_ack(&self, ack: &AppliedAck) -> Result<(), WireError> {
        let binding = self.binding(&ack.scope)?;
        if ack.sequence < binding.applied || ack.sequence > binding.delivered {
            return Err(WireError::StaleSequence);
        }
        Ok(())
    }

    fn accept_ack(&mut self, ack: &AppliedAck) -> Result<(), WireError> {
        self.check_ack(ack)?;
        self.binding_mut(&ack.scope)?.applied = ack.sequence;
        Ok(())
    }

    fn client(&self, frame: &ClientFrame) -> Result<(), WireError> {
        if matches!(&frame.message, ClientMessage::Hello(_)) {
            return Err(WireError::AlreadyNegotiated);
        }
        if frame.session.as_ref() != Some(&self.session) {
            return Err(WireError::IdentityMismatch);
        }
        self.require(frame.message.capability())?;
        if let Some(scope) = frame.message.scope() {
            self.binding(scope)?;
        }
        if let Some(effect) = frame.message.mutation() {
            self.check_lease(&effect.scope, effect.lease_fence)?;
        }
        match &frame.message {
            ClientMessage::Subscribe(value) => {
                if value.panes.as_slice().iter().any(|pane| pane.host != self.host) {
                    return Err(WireError::IdentityMismatch);
                }
            }
            ClientMessage::Unsubscribe(value) => {
                for scope in value.scopes.as_slice() { self.binding(scope)?; }
            }
            ClientMessage::AppliedAck(value) => self.check_ack(value)?,
            ClientMessage::Resync(value) => {
                if value.last_applied > self.binding(&value.scope)?.delivered {
                    return Err(WireError::StaleSequence);
                }
            }
            ClientMessage::Mouse(value) => {
                let dimensions = &self.binding(&value.effect.scope)?.dimensions;
                if value.column >= dimensions.columns || value.row >= dimensions.rows {
                    return Err(WireError::InvalidBounds);
                }
            }
            ClientMessage::RenewControl(value) | ClientMessage::ReleaseControl(value) => {
                self.check_lease(&value.scope, value.lease_fence)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn server(&self, frame: &ServerFrame) -> Result<(), WireError> {
        if matches!(&frame.message, ServerMessage::Welcome(_)) {
            return Err(WireError::AlreadyNegotiated);
        }
        if frame.session.as_ref() != Some(&self.session) {
            return Err(WireError::IdentityMismatch);
        }
        self.require(frame.message.capability())?;
        if let Some(scope) = frame.message.scope() {
            if matches!(&frame.message, ServerMessage::Subscribed(_)) {
                // Subscription offers an identity; it grants no authority by
                // decoding. The actual owner must explicitly install it.
                if scope.identity.host != self.host {
                    return Err(WireError::IdentityMismatch);
                }
                if self.bindings.iter().any(|value|
                    value.scope.identity.pane == scope.identity.pane && value.scope != *scope)
                {
                    return Err(WireError::StaleScope);
                }
            } else {
                self.binding(scope)?;
            }
        }
        if let Some(patch) = frame.message.patch() {
            if let Some(scope) = frame.message.scope() {
                if patch.dimensions != self.binding(scope)?.dimensions {
                    return Err(WireError::StaleScope);
                }
            }
            if !patch.hyperlinks.is_empty() || !patch.images.is_empty() {
                self.require(Some(Capability::ResourceReferences))?;
            }
        }
        match &frame.message {
            ServerMessage::Delta(value) => {
                if value.base_sequence != self.binding(&value.scope)?.delivered {
                    return Err(WireError::StaleSequence);
                }
            }
            ServerMessage::SnapshotStart(value) => {
                let binding = self.binding(&value.scope)?;
                if value.dimensions != binding.dimensions || value.sequence < binding.delivered {
                    return Err(WireError::StaleSequence);
                }
            }
            ServerMessage::SnapshotChunk(value) => {
                if value.sequence < self.binding(&value.scope)?.delivered {
                    return Err(WireError::StaleSequence);
                }
            }
            ServerMessage::SnapshotEnd(value) => {
                if value.sequence < self.binding(&value.scope)?.delivered {
                    return Err(WireError::StaleSequence);
                }
            }
            ServerMessage::LeaseState(value) => {
                if let Some(fence) = value.fence {
                    let binding = self.binding(&value.scope)?;
                    let active = matches!(value.disposition, LeaseDisposition::Granted | LeaseDisposition::Renewed);
                    if fence < binding.lease_fence
                        || (active && !binding.lease_active && fence == binding.lease_fence)
                    {
                        return Err(WireError::StaleLease);
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// Native codec state. Scope/delivery/lease updates are explicit trusted-owner
/// inputs, from the real authorization and transport owners, never copied
/// from arbitrary requests. No method performs a pane effect.
#[derive(Debug, Clone)]
pub struct ServerSession {
    host: Id,
    session: Id,
    asset: Sha256,
    supported: Capabilities,
    state: Option<SessionState>,
}

impl ServerSession {
    pub fn new(
        host: Id,
        session: Id,
        asset: Sha256,
        supported: Capabilities,
    ) -> Result<Self, WireError> {
        validate_capabilities(&supported)?;
        Ok(Self { host, session, asset, supported, state: None })
    }

    /// One-shot Hello/Welcome exchange. Failure leaves the session unopened.
    /// Run only after the actual HTTP/socket authentication boundary.
    pub fn negotiate(&mut self, bytes: &[u8]) -> Result<Vec<u8>, WireError> {
        if self.state.is_some() { return Err(WireError::AlreadyNegotiated); }
        let frame = decode_client(bytes)?;
        let ClientMessage::Hello(hello) = frame.message else {
            return Err(WireError::NotNegotiated);
        };
        if hello.host != self.host { return Err(WireError::IdentityMismatch); }
        if hello.asset != self.asset { return Err(WireError::AssetMismatch); }
        let mut capabilities = Capabilities::new();
        for capability in hello.required.as_slice() {
            if !self.supported.as_slice().contains(capability) {
                return Err(WireError::UnsupportedCapability);
            }
            capabilities.try_push(*capability)?;
        }
        for capability in hello.optional.as_slice() {
            if self.supported.as_slice().contains(capability) {
                capabilities.try_push(*capability)?;
            }
        }
        let response = ServerFrame::new(Some(self.session.clone()), ServerMessage::Welcome(Welcome {
            host: self.host.clone(),
            hello_id: hello.hello_id,
            asset: self.asset.clone(),
            capabilities: capabilities.clone(),
        }))?;
        let bytes = encode_server(&response)?;
        self.state = Some(SessionState {
            host: self.host.clone(),
            session: self.session.clone(),
            capabilities,
            bindings: Vec::new(),
        });
        Ok(bytes)
    }

    pub fn decode_request(&mut self, bytes: &[u8]) -> Result<ClientFrame, WireError> {
        let frame = decode_client(bytes)?;
        let state = self.state.as_mut().ok_or(WireError::NotNegotiated)?;
        state.client(&frame)?;
        if let ClientMessage::AppliedAck(ack) = &frame.message { state.accept_ack(ack)?; }
        Ok(frame)
    }

    pub fn encode_response(&self, message: ServerMessage) -> Result<Vec<u8>, WireError> {
        let state = self.state.as_ref().ok_or(WireError::NotNegotiated)?;
        let frame = ServerFrame::new(Some(self.session.clone()), message)?;
        frame.validate()?;
        state.server(&frame)?;
        encode_server(&frame)
    }

    pub fn bind_scope(&mut self, scope: Scope, dimensions: Dimensions, delivered: WireU64) -> Result<(), WireError> {
        self.state.as_mut().ok_or(WireError::NotNegotiated)?.bind(scope, dimensions, delivered)
    }

    pub fn unbind_scope(&mut self, scope: &Scope) -> Result<(), WireError> {
        self.state.as_mut().ok_or(WireError::NotNegotiated)?.unbind(scope)
    }

    /// Mirror an authoritative resize without resetting delivery, ACK, or lease
    /// watermarks. It does not resize the real pane or apply a browser grid.
    pub fn update_dimensions(&mut self, scope: &Scope, dimensions: Dimensions) -> Result<(), WireError> {
        self.state.as_mut().ok_or(WireError::NotNegotiated)?.update_dimensions(scope, dimensions)
    }

    /// Called after the transport owner delivered a complete publication;
    /// encoding alone never advances the allowed ACK boundary.
    pub fn record_delivery(&mut self, scope: &Scope, previous: WireU64, sequence: WireU64) -> Result<(), WireError> {
        self.state.as_mut().ok_or(WireError::NotNegotiated)?.record_delivery(scope, previous, sequence)
    }

    /// Mirror the actual authoritative lease decision, including retired fences.
    pub fn record_lease(&mut self, scope: &Scope, fence: WireU64, active: bool) -> Result<(), WireError> {
        self.state.as_mut().ok_or(WireError::NotNegotiated)?.record_lease(scope, fence, active)
    }
}

/// Browser codec state. It never parses through JavaScript Number or JSON.parse.
/// The model owner binds scopes and records a completed snapshot/delta only
/// after full validation/application; decoding alone emits no ACK.
#[derive(Debug, Clone)]
pub struct BrowserSession {
    hello: Hello,
    hello_sent: bool,
    state: Option<SessionState>,
}

impl BrowserSession {
    pub fn new(hello: Hello) -> Result<Self, WireError> {
        hello.validate()?;
        Ok(Self { hello, hello_sent: false, state: None })
    }

    pub fn hello_frame(&mut self) -> Result<Vec<u8>, WireError> {
        if self.hello_sent { return Err(WireError::AlreadyNegotiated); }
        let frame = ClientFrame::new(None, ClientMessage::Hello(self.hello.clone()))?;
        let bytes = encode_client(&frame)?;
        self.hello_sent = true;
        Ok(bytes)
    }

    pub fn accept_welcome(&mut self, bytes: &[u8]) -> Result<(), WireError> {
        if self.state.is_some() { return Err(WireError::AlreadyNegotiated); }
        if !self.hello_sent { return Err(WireError::NotNegotiated); }
        let frame = decode_server(bytes)?;
        let ServerMessage::Welcome(welcome) = frame.message else {
            return Err(WireError::NotNegotiated);
        };
        if welcome.host != self.hello.host || welcome.hello_id != self.hello.hello_id {
            return Err(WireError::IdentityMismatch);
        }
        if welcome.asset != self.hello.asset { return Err(WireError::AssetMismatch); }
        if self.hello.required.as_slice().iter()
            .any(|capability| !welcome.capabilities.as_slice().contains(capability))
            || welcome.capabilities.as_slice().iter().any(|capability|
                !self.hello.required.as_slice().contains(capability)
                && !self.hello.optional.as_slice().contains(capability))
        {
            return Err(WireError::UnsupportedCapability);
        }
        self.state = Some(SessionState {
            host: welcome.host,
            session: frame.session.ok_or(WireError::InvalidState)?,
            capabilities: welcome.capabilities,
            bindings: Vec::new(),
        });
        Ok(())
    }

    pub fn decode_response(&self, bytes: &[u8]) -> Result<ServerFrame, WireError> {
        let frame = decode_server(bytes)?;
        self.state.as_ref().ok_or(WireError::NotNegotiated)?.server(&frame)?;
        Ok(frame)
    }

    pub fn encode_request(&mut self, message: ClientMessage) -> Result<Vec<u8>, WireError> {
        let state = self.state.as_mut().ok_or(WireError::NotNegotiated)?;
        let frame = ClientFrame::new(Some(state.session.clone()), message)?;
        frame.validate()?;
        state.client(&frame)?;
        let bytes = encode_client(&frame)?;
        if let ClientMessage::AppliedAck(ack) = &frame.message { state.accept_ack(ack)?; }
        Ok(bytes)
    }

    pub fn bind_scope(&mut self, scope: Scope, dimensions: Dimensions, delivered: WireU64) -> Result<(), WireError> {
        self.state.as_mut().ok_or(WireError::NotNegotiated)?.bind(scope, dimensions, delivered)
    }

    pub fn unbind_scope(&mut self, scope: &Scope) -> Result<(), WireError> {
        self.state.as_mut().ok_or(WireError::NotNegotiated)?.unbind(scope)
    }

    /// Preserve sequence and lease fences while the host updates its geometry.
    pub fn update_dimensions(&mut self, scope: &Scope, dimensions: Dimensions) -> Result<(), WireError> {
        self.state.as_mut().ok_or(WireError::NotNegotiated)?.update_dimensions(scope, dimensions)
    }

    /// The host supplies this only after complete model application, including
    /// snapshot assembly and coverage checks. It is not a paint acknowledgment.
    pub fn record_delivery(&mut self, scope: &Scope, previous: WireU64, sequence: WireU64) -> Result<(), WireError> {
        self.state.as_mut().ok_or(WireError::NotNegotiated)?.record_delivery(scope, previous, sequence)
    }

    pub fn record_lease(&mut self, scope: &Scope, fence: WireU64, active: bool) -> Result<(), WireError> {
        self.state.as_mut().ok_or(WireError::NotNegotiated)?.record_lease(scope, fence, active)
    }
}
