//! Watcher clients and per-client view-state model for agent swarms.
//!
//! Defines explicit client roles (interactive vs read-only watcher) and tracks
//! per-client view state (active tab, active pane, mirrored or independent view).
//! Watchers **cannot** perform mutating actions — the [`ClientRegistry`] enforces
//! this contract before forwarding actions to the policy engine.
//!
//! # Client roles
//!
//! | Role          | Mutations | View state   | Typical actor           |
//! |---------------|-----------|--------------|-------------------------|
//! | `Interactive` | Allowed   | Independent  | Human operator, CI bot  |
//! | `Watcher`     | Denied    | Mirror / Own | AI agent observer, log  |
//!
//! # View modes
//!
//! Each client independently selects a view mode:
//!
//! * **Mirrored** — follows the leader client's active tab/pane focus.
//! * **Independent** — client tracks its own active tab/pane.
//!
//! # Integration
//!
//! Wire this into the policy layer by checking
//! [`ClientRegistry::authorize`] before executing any [`ActionKind`].
//! The registry returns [`ClientPolicyDecision`] which either allows the action
//! or denies it with a reason.
//!
//! ```ignore
//! let reg = ClientRegistry::new(ClientRegistryConfig::default());
//! let cid = reg.connect("agent-1", ClientRole::Watcher);
//! assert!(reg.authorize(&cid, ActionKind::ReadOutput).is_allowed());
//! assert!(reg.authorize(&cid, ActionKind::SendText).is_denied());
//! ```

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::policy::ActionKind;

// =============================================================================
// Client identity
// =============================================================================

/// Unique identifier for a connected client.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientId(pub String);

impl ClientId {
    fn generate(counter: u64) -> Self {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        Self(format!("cl-{ts:x}-{counter:04x}"))
    }
}

impl std::fmt::Display for ClientId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// =============================================================================
// Client role
// =============================================================================

/// Role determining what a client is allowed to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientRole {
    /// Full read-write access. Can perform mutating actions.
    Interactive,
    /// Read-only observer. Cannot send text, spawn, split, close, or
    /// perform any other mutating action.
    Watcher,
}

impl ClientRole {
    /// Whether this role allows mutating actions.
    #[must_use]
    pub const fn can_mutate(&self) -> bool {
        matches!(self, Self::Interactive)
    }

    /// Human-readable label.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Watcher => "watcher",
        }
    }
}

// =============================================================================
// View mode
// =============================================================================

/// How a client's active-pane focus is determined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewMode {
    /// Client tracks its own active tab and pane independently.
    Independent,
    /// Client mirrors the leader client's focus (follows their active tab/pane).
    Mirrored,
}

impl Default for ViewMode {
    fn default() -> Self {
        Self::Independent
    }
}

// =============================================================================
// Per-client view state
// =============================================================================

/// Per-client view state tracking what each client is looking at.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientViewState {
    /// Active tab (0-based index or tab ID).
    pub active_tab: u64,
    /// Active pane within the active tab (pane ID).
    pub active_pane: u64,
    /// How this client's focus is determined.
    pub view_mode: ViewMode,
    /// Last time the view state was updated (epoch ms).
    pub updated_at_ms: u64,
}

impl Default for ClientViewState {
    fn default() -> Self {
        Self {
            active_tab: 0,
            active_pane: 0,
            view_mode: ViewMode::default(),
            updated_at_ms: now_ms(),
        }
    }
}

impl ClientViewState {
    /// Create a new view state with explicit initial focus.
    #[must_use]
    pub fn new(active_tab: u64, active_pane: u64, view_mode: ViewMode) -> Self {
        Self {
            active_tab,
            active_pane,
            view_mode,
            updated_at_ms: now_ms(),
        }
    }

    /// Update the active tab and pane.
    pub fn set_focus(&mut self, tab: u64, pane: u64) {
        self.active_tab = tab;
        self.active_pane = pane;
        self.updated_at_ms = now_ms();
    }

    /// Change the view mode.
    pub fn set_view_mode(&mut self, mode: ViewMode) {
        self.view_mode = mode;
        self.updated_at_ms = now_ms();
    }
}

// =============================================================================
// Client session
// =============================================================================

/// A connected client session with role and view state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientSession {
    /// Unique client ID.
    pub id: ClientId,
    /// Human-readable name (e.g. agent name, user name).
    pub name: String,
    /// Client role (interactive or watcher).
    pub role: ClientRole,
    /// Per-client view state.
    pub view_state: ClientViewState,
    /// When the client connected (epoch ms).
    pub connected_at_ms: u64,
    /// Optional metadata for this client (e.g. agent type, version).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, String>,
}

// =============================================================================
// Policy decision
// =============================================================================

/// Result of a client-level authorization check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientPolicyDecision {
    /// Action is allowed for this client.
    Allow,
    /// Action is denied because the client is a watcher.
    DeniedWatcher {
        /// The action that was denied.
        action: ActionKind,
        /// The client ID that attempted the action.
        client_id: ClientId,
    },
    /// Action is denied because the client ID is unknown.
    DeniedUnknown {
        /// The unrecognized client ID.
        client_id: ClientId,
    },
}

impl ClientPolicyDecision {
    /// Whether the action is allowed.
    #[must_use]
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow)
    }

    /// Whether the action is denied.
    #[must_use]
    pub fn is_denied(&self) -> bool {
        !self.is_allowed()
    }

    /// Human-readable denial reason, or `None` if allowed.
    #[must_use]
    pub fn denial_reason(&self) -> Option<String> {
        match self {
            Self::Allow => None,
            Self::DeniedWatcher { action, client_id } => Some(format!(
                "watcher client {client_id} cannot perform mutating action {action:?}"
            )),
            Self::DeniedUnknown { client_id } => {
                Some(format!("unknown client {client_id}"))
            }
        }
    }
}

// =============================================================================
// Client summary (diagnostic)
// =============================================================================

/// Diagnostic snapshot of a connected client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientSummary {
    pub id: ClientId,
    pub name: String,
    pub role: ClientRole,
    pub active_tab: u64,
    pub active_pane: u64,
    pub view_mode: ViewMode,
    pub connected_at_ms: u64,
}

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for [`ClientRegistry`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientRegistryConfig {
    /// Maximum number of concurrent clients.
    pub max_clients: usize,
    /// Maximum number of watcher clients.
    pub max_watchers: usize,
}

impl Default for ClientRegistryConfig {
    fn default() -> Self {
        Self {
            max_clients: 256,
            max_watchers: 200,
        }
    }
}

// =============================================================================
// Client registry
// =============================================================================

/// Manages connected clients, their roles, and view states.
///
/// Enforces that watcher clients cannot perform mutating actions and
/// tracks per-client view state for independent or mirrored focus.
pub struct ClientRegistry {
    config: ClientRegistryConfig,
    clients: HashMap<ClientId, ClientSession>,
    counter: u64,
    /// ID of the "leader" client whose focus mirrored clients follow.
    leader: Option<ClientId>,
}

impl ClientRegistry {
    /// Create a new client registry.
    #[must_use]
    pub fn new(config: ClientRegistryConfig) -> Self {
        Self {
            config,
            clients: HashMap::new(),
            counter: 0,
            leader: None,
        }
    }

    /// Connect a new client. Returns `None` if at capacity.
    pub fn connect(&mut self, name: &str, role: ClientRole) -> Option<ClientId> {
        self.connect_with_metadata(name, role, HashMap::new())
    }

    /// Connect with optional metadata. Returns `None` if at capacity.
    pub fn connect_with_metadata(
        &mut self,
        name: &str,
        role: ClientRole,
        metadata: HashMap<String, String>,
    ) -> Option<ClientId> {
        // Check total capacity.
        if self.clients.len() >= self.config.max_clients {
            return None;
        }
        // Check watcher capacity.
        if role == ClientRole::Watcher && self.watcher_count() >= self.config.max_watchers {
            return None;
        }

        self.counter += 1;
        let id = ClientId::generate(self.counter);
        let now = now_ms();

        let session = ClientSession {
            id: id.clone(),
            name: name.to_string(),
            role,
            view_state: ClientViewState::default(),
            connected_at_ms: now,
            metadata,
        };
        self.clients.insert(id.clone(), session);

        // First interactive client becomes the leader.
        if role == ClientRole::Interactive && self.leader.is_none() {
            self.leader = Some(id.clone());
        }

        Some(id)
    }

    /// Disconnect a client. Returns the session if it existed.
    pub fn disconnect(&mut self, client_id: &ClientId) -> Option<ClientSession> {
        let session = self.clients.remove(client_id);
        // If the leader disconnects, promote the next interactive client.
        if self.leader.as_ref() == Some(client_id) {
            self.leader = self
                .clients
                .values()
                .find(|c| c.role == ClientRole::Interactive)
                .map(|c| c.id.clone());
        }
        session
    }

    /// Check whether a client is authorized to perform an action.
    ///
    /// Watchers are denied all mutating actions. Interactive clients are
    /// always allowed (further policy checks happen downstream in the
    /// policy engine).
    #[must_use]
    pub fn authorize(&self, client_id: &ClientId, action: ActionKind) -> ClientPolicyDecision {
        match self.clients.get(client_id) {
            None => ClientPolicyDecision::DeniedUnknown {
                client_id: client_id.clone(),
            },
            Some(session) => {
                if action.is_mutating() && session.role == ClientRole::Watcher {
                    ClientPolicyDecision::DeniedWatcher {
                        action,
                        client_id: client_id.clone(),
                    }
                } else {
                    ClientPolicyDecision::Allow
                }
            }
        }
    }

    /// Update the active focus for a client. Returns `false` if the client
    /// doesn't exist or is in mirrored mode (mirrored clients can't set
    /// their own focus).
    pub fn set_focus(&mut self, client_id: &ClientId, tab: u64, pane: u64) -> bool {
        if let Some(session) = self.clients.get_mut(client_id) {
            if session.view_state.view_mode == ViewMode::Mirrored {
                return false;
            }
            session.view_state.set_focus(tab, pane);
            // If this is the leader, propagate to mirrored clients.
            if self.leader.as_ref() == Some(client_id) {
                self.propagate_leader_focus(tab, pane);
            }
            true
        } else {
            false
        }
    }

    /// Change a client's view mode.
    pub fn set_view_mode(&mut self, client_id: &ClientId, mode: ViewMode) -> bool {
        if let Some(session) = self.clients.get_mut(client_id) {
            session.view_state.set_view_mode(mode);
            // If switching to mirrored, sync focus from leader.
            if mode == ViewMode::Mirrored {
                if let Some(leader) = self.leader.as_ref().and_then(|l| self.clients.get(l)) {
                    let tab = leader.view_state.active_tab;
                    let pane = leader.view_state.active_pane;
                    // Re-borrow mutably.
                    if let Some(s) = self.clients.get_mut(client_id) {
                        s.view_state.active_tab = tab;
                        s.view_state.active_pane = pane;
                    }
                }
            }
            true
        } else {
            false
        }
    }

    /// Set the leader client. The leader's focus is propagated to mirrored
    /// clients. Returns `false` if the client doesn't exist or is a watcher.
    pub fn set_leader(&mut self, client_id: &ClientId) -> bool {
        match self.clients.get(client_id) {
            Some(session) if session.role == ClientRole::Interactive => {
                self.leader = Some(client_id.clone());
                // Propagate current leader focus to all mirrored.
                let tab = session.view_state.active_tab;
                let pane = session.view_state.active_pane;
                self.propagate_leader_focus(tab, pane);
                true
            }
            _ => false,
        }
    }

    /// Get the current leader client ID.
    #[must_use]
    pub fn leader(&self) -> Option<&ClientId> {
        self.leader.as_ref()
    }

    /// Get a client session by ID.
    #[must_use]
    pub fn get(&self, client_id: &ClientId) -> Option<&ClientSession> {
        self.clients.get(client_id)
    }

    /// Get the effective focus for a client (resolves mirrored mode).
    #[must_use]
    pub fn effective_focus(&self, client_id: &ClientId) -> Option<(u64, u64)> {
        let session = self.clients.get(client_id)?;
        match session.view_state.view_mode {
            ViewMode::Independent => {
                Some((session.view_state.active_tab, session.view_state.active_pane))
            }
            ViewMode::Mirrored => {
                if let Some(leader) = self.leader.as_ref().and_then(|l| self.clients.get(l)) {
                    Some((
                        leader.view_state.active_tab,
                        leader.view_state.active_pane,
                    ))
                } else {
                    // No leader — fall back to own focus.
                    Some((session.view_state.active_tab, session.view_state.active_pane))
                }
            }
        }
    }

    /// Total connected clients.
    #[must_use]
    pub fn total_count(&self) -> usize {
        self.clients.len()
    }

    /// Number of interactive clients.
    #[must_use]
    pub fn interactive_count(&self) -> usize {
        self.clients
            .values()
            .filter(|c| c.role == ClientRole::Interactive)
            .count()
    }

    /// Number of watcher clients.
    #[must_use]
    pub fn watcher_count(&self) -> usize {
        self.clients
            .values()
            .filter(|c| c.role == ClientRole::Watcher)
            .count()
    }

    /// Diagnostic summary of all connected clients.
    #[must_use]
    pub fn summary(&self) -> Vec<ClientSummary> {
        self.clients
            .values()
            .map(|c| ClientSummary {
                id: c.id.clone(),
                name: c.name.clone(),
                role: c.role,
                active_tab: c.view_state.active_tab,
                active_pane: c.view_state.active_pane,
                view_mode: c.view_state.view_mode,
                connected_at_ms: c.connected_at_ms,
            })
            .collect()
    }

    /// Propagate leader focus to all mirrored clients (internal).
    fn propagate_leader_focus(&mut self, tab: u64, pane: u64) {
        let leader_id = match &self.leader {
            Some(id) => id.clone(),
            None => return,
        };
        for session in self.clients.values_mut() {
            if session.id != leader_id && session.view_state.view_mode == ViewMode::Mirrored {
                session.view_state.active_tab = tab;
                session.view_state.active_pane = pane;
            }
        }
    }
}

// =============================================================================
// Utilities
// =============================================================================

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> ClientRegistryConfig {
        ClientRegistryConfig {
            max_clients: 10,
            max_watchers: 5,
        }
    }

    // -- ClientRole -----------------------------------------------------------

    #[test]
    fn role_interactive_can_mutate() {
        assert!(ClientRole::Interactive.can_mutate());
        assert_eq!(ClientRole::Interactive.as_str(), "interactive");
    }

    #[test]
    fn role_watcher_cannot_mutate() {
        assert!(!ClientRole::Watcher.can_mutate());
        assert_eq!(ClientRole::Watcher.as_str(), "watcher");
    }

    // -- ClientId -------------------------------------------------------------

    #[test]
    fn client_id_generation_unique() {
        let a = ClientId::generate(1);
        let b = ClientId::generate(2);
        assert_ne!(a, b);
        assert!(a.0.starts_with("cl-"));
    }

    #[test]
    fn client_id_display() {
        let id = ClientId("cl-test-0001".to_string());
        assert_eq!(format!("{id}"), "cl-test-0001");
    }

    #[test]
    fn client_id_serde_roundtrip() {
        let id = ClientId("cl-test-0042".to_string());
        let json = serde_json::to_string(&id).unwrap();
        let back: ClientId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }

    // -- ViewMode & ClientViewState ------------------------------------------

    #[test]
    fn view_mode_default_is_independent() {
        assert_eq!(ViewMode::default(), ViewMode::Independent);
    }

    #[test]
    fn view_state_set_focus() {
        let mut vs = ClientViewState::default();
        assert_eq!(vs.active_tab, 0);
        assert_eq!(vs.active_pane, 0);

        vs.set_focus(2, 5);
        assert_eq!(vs.active_tab, 2);
        assert_eq!(vs.active_pane, 5);
    }

    #[test]
    fn view_state_set_view_mode() {
        let mut vs = ClientViewState::default();
        assert_eq!(vs.view_mode, ViewMode::Independent);

        vs.set_view_mode(ViewMode::Mirrored);
        assert_eq!(vs.view_mode, ViewMode::Mirrored);
    }

    // -- ClientRegistry: connect/disconnect -----------------------------------

    #[test]
    fn connect_interactive() {
        let mut reg = ClientRegistry::new(default_config());
        let cid = reg.connect("user-1", ClientRole::Interactive).unwrap();
        assert_eq!(reg.total_count(), 1);
        assert_eq!(reg.interactive_count(), 1);
        assert_eq!(reg.watcher_count(), 0);
        assert_eq!(reg.get(&cid).unwrap().name, "user-1");
    }

    #[test]
    fn connect_watcher() {
        let mut reg = ClientRegistry::new(default_config());
        let cid = reg.connect("agent-1", ClientRole::Watcher).unwrap();
        assert_eq!(reg.total_count(), 1);
        assert_eq!(reg.interactive_count(), 0);
        assert_eq!(reg.watcher_count(), 1);
        assert_eq!(reg.get(&cid).unwrap().role, ClientRole::Watcher);
    }

    #[test]
    fn disconnect_removes_client() {
        let mut reg = ClientRegistry::new(default_config());
        let cid = reg.connect("user-1", ClientRole::Interactive).unwrap();
        assert_eq!(reg.total_count(), 1);

        let session = reg.disconnect(&cid).unwrap();
        assert_eq!(session.name, "user-1");
        assert_eq!(reg.total_count(), 0);
        assert!(reg.get(&cid).is_none());
    }

    #[test]
    fn disconnect_unknown_returns_none() {
        let mut reg = ClientRegistry::new(default_config());
        let fake = ClientId("cl-fake-0000".to_string());
        assert!(reg.disconnect(&fake).is_none());
    }

    #[test]
    fn capacity_limit_total() {
        let config = ClientRegistryConfig {
            max_clients: 2,
            max_watchers: 5,
        };
        let mut reg = ClientRegistry::new(config);
        reg.connect("a", ClientRole::Interactive).unwrap();
        reg.connect("b", ClientRole::Interactive).unwrap();
        assert!(reg.connect("c", ClientRole::Interactive).is_none());
    }

    #[test]
    fn capacity_limit_watchers() {
        let config = ClientRegistryConfig {
            max_clients: 10,
            max_watchers: 2,
        };
        let mut reg = ClientRegistry::new(config);
        reg.connect("w1", ClientRole::Watcher).unwrap();
        reg.connect("w2", ClientRole::Watcher).unwrap();
        // Third watcher rejected.
        assert!(reg.connect("w3", ClientRole::Watcher).is_none());
        // But an interactive client is fine.
        assert!(reg.connect("i1", ClientRole::Interactive).is_some());
    }

    // -- ClientRegistry: authorization ----------------------------------------

    #[test]
    fn interactive_can_mutate() {
        let mut reg = ClientRegistry::new(default_config());
        let cid = reg.connect("user-1", ClientRole::Interactive).unwrap();

        assert!(reg.authorize(&cid, ActionKind::SendText).is_allowed());
        assert!(reg.authorize(&cid, ActionKind::Spawn).is_allowed());
        assert!(reg.authorize(&cid, ActionKind::Close).is_allowed());
    }

    #[test]
    fn interactive_can_read() {
        let mut reg = ClientRegistry::new(default_config());
        let cid = reg.connect("user-1", ClientRole::Interactive).unwrap();

        assert!(reg.authorize(&cid, ActionKind::ReadOutput).is_allowed());
        assert!(reg.authorize(&cid, ActionKind::SearchOutput).is_allowed());
    }

    #[test]
    fn watcher_denied_mutating_actions() {
        let mut reg = ClientRegistry::new(default_config());
        let cid = reg.connect("agent-1", ClientRole::Watcher).unwrap();

        let decision = reg.authorize(&cid, ActionKind::SendText);
        assert!(decision.is_denied());
        assert!(decision.denial_reason().unwrap().contains("watcher"));
        assert!(decision.denial_reason().unwrap().contains("SendText"));

        assert!(reg.authorize(&cid, ActionKind::Spawn).is_denied());
        assert!(reg.authorize(&cid, ActionKind::Split).is_denied());
        assert!(reg.authorize(&cid, ActionKind::Close).is_denied());
        assert!(reg.authorize(&cid, ActionKind::SendCtrlC).is_denied());
    }

    #[test]
    fn watcher_allowed_read_actions() {
        let mut reg = ClientRegistry::new(default_config());
        let cid = reg.connect("agent-1", ClientRole::Watcher).unwrap();

        assert!(reg.authorize(&cid, ActionKind::ReadOutput).is_allowed());
        assert!(reg.authorize(&cid, ActionKind::SearchOutput).is_allowed());
    }

    #[test]
    fn unknown_client_denied() {
        let reg = ClientRegistry::new(default_config());
        let fake = ClientId("cl-fake-0000".to_string());

        let decision = reg.authorize(&fake, ActionKind::ReadOutput);
        assert!(decision.is_denied());
        assert!(decision.denial_reason().unwrap().contains("unknown"));
    }

    // -- ClientRegistry: focus & view modes -----------------------------------

    #[test]
    fn set_focus_independent() {
        let mut reg = ClientRegistry::new(default_config());
        let cid = reg.connect("user-1", ClientRole::Interactive).unwrap();

        assert!(reg.set_focus(&cid, 3, 42));
        let (tab, pane) = reg.effective_focus(&cid).unwrap();
        assert_eq!(tab, 3);
        assert_eq!(pane, 42);
    }

    #[test]
    fn mirrored_client_follows_leader() {
        let mut reg = ClientRegistry::new(default_config());
        let leader = reg.connect("user-1", ClientRole::Interactive).unwrap();
        let watcher = reg.connect("agent-1", ClientRole::Watcher).unwrap();

        // Set watcher to mirrored mode.
        reg.set_view_mode(&watcher, ViewMode::Mirrored);

        // Leader sets focus.
        reg.set_focus(&leader, 5, 99);

        // Watcher effective focus follows leader.
        let (tab, pane) = reg.effective_focus(&watcher).unwrap();
        assert_eq!(tab, 5);
        assert_eq!(pane, 99);
    }

    #[test]
    fn mirrored_client_cannot_set_own_focus() {
        let mut reg = ClientRegistry::new(default_config());
        let _leader = reg.connect("user-1", ClientRole::Interactive).unwrap();
        let watcher = reg.connect("agent-1", ClientRole::Watcher).unwrap();

        reg.set_view_mode(&watcher, ViewMode::Mirrored);
        // Attempting to set focus on a mirrored client fails.
        assert!(!reg.set_focus(&watcher, 10, 20));
    }

    #[test]
    fn independent_client_ignores_leader() {
        let mut reg = ClientRegistry::new(default_config());
        let leader = reg.connect("user-1", ClientRole::Interactive).unwrap();
        let agent = reg.connect("agent-1", ClientRole::Interactive).unwrap();

        // Both are independent by default.
        reg.set_focus(&leader, 5, 99);
        reg.set_focus(&agent, 1, 2);

        // Each has their own focus.
        let (tab, pane) = reg.effective_focus(&leader).unwrap();
        assert_eq!((tab, pane), (5, 99));
        let (tab, pane) = reg.effective_focus(&agent).unwrap();
        assert_eq!((tab, pane), (1, 2));
    }

    #[test]
    fn switching_to_mirrored_syncs_from_leader() {
        let mut reg = ClientRegistry::new(default_config());
        let leader = reg.connect("user-1", ClientRole::Interactive).unwrap();
        let watcher = reg.connect("agent-1", ClientRole::Watcher).unwrap();

        reg.set_focus(&leader, 7, 33);

        // Watcher switches to mirrored — should pick up leader's focus.
        reg.set_view_mode(&watcher, ViewMode::Mirrored);
        let session = reg.get(&watcher).unwrap();
        assert_eq!(session.view_state.active_tab, 7);
        assert_eq!(session.view_state.active_pane, 33);
    }

    // -- ClientRegistry: leader management ------------------------------------

    #[test]
    fn first_interactive_becomes_leader() {
        let mut reg = ClientRegistry::new(default_config());
        // Watcher connects first — not leader.
        let _w = reg.connect("w1", ClientRole::Watcher).unwrap();
        assert!(reg.leader().is_none());

        // Interactive connects — becomes leader.
        let i1 = reg.connect("i1", ClientRole::Interactive).unwrap();
        assert_eq!(reg.leader(), Some(&i1));
    }

    #[test]
    fn disconnect_leader_promotes_next() {
        let mut reg = ClientRegistry::new(default_config());
        let i1 = reg.connect("i1", ClientRole::Interactive).unwrap();
        let _i2 = reg.connect("i2", ClientRole::Interactive).unwrap();
        assert_eq!(reg.leader(), Some(&i1));

        reg.disconnect(&i1);
        // i2 should now be leader (or possibly another interactive).
        assert!(reg.leader().is_some());
        assert_ne!(reg.leader(), Some(&i1));
    }

    #[test]
    fn watcher_cannot_be_leader() {
        let mut reg = ClientRegistry::new(default_config());
        let w = reg.connect("w1", ClientRole::Watcher).unwrap();
        assert!(!reg.set_leader(&w));
    }

    #[test]
    fn explicit_leader_change() {
        let mut reg = ClientRegistry::new(default_config());
        let i1 = reg.connect("i1", ClientRole::Interactive).unwrap();
        let i2 = reg.connect("i2", ClientRole::Interactive).unwrap();
        assert_eq!(reg.leader(), Some(&i1));

        assert!(reg.set_leader(&i2));
        assert_eq!(reg.leader(), Some(&i2));
    }

    // -- ClientRegistry: summary & metadata -----------------------------------

    #[test]
    fn summary_contains_all_clients() {
        let mut reg = ClientRegistry::new(default_config());
        reg.connect("user-1", ClientRole::Interactive).unwrap();
        reg.connect("agent-1", ClientRole::Watcher).unwrap();
        reg.connect("agent-2", ClientRole::Watcher).unwrap();

        let summary = reg.summary();
        assert_eq!(summary.len(), 3);
    }

    #[test]
    fn connect_with_metadata() {
        let mut reg = ClientRegistry::new(default_config());
        let mut meta = HashMap::new();
        meta.insert("agent_type".to_string(), "claude-code".to_string());
        meta.insert("version".to_string(), "1.0".to_string());

        let cid = reg
            .connect_with_metadata("agent-1", ClientRole::Watcher, meta)
            .unwrap();
        let session = reg.get(&cid).unwrap();
        assert_eq!(session.metadata.get("agent_type").unwrap(), "claude-code");
    }

    // -- ClientPolicyDecision -------------------------------------------------

    #[test]
    fn policy_decision_allow() {
        let d = ClientPolicyDecision::Allow;
        assert!(d.is_allowed());
        assert!(!d.is_denied());
        assert!(d.denial_reason().is_none());
    }

    #[test]
    fn policy_decision_denied_watcher() {
        let d = ClientPolicyDecision::DeniedWatcher {
            action: ActionKind::SendText,
            client_id: ClientId("cl-test".to_string()),
        };
        assert!(d.is_denied());
        let reason = d.denial_reason().unwrap();
        assert!(reason.contains("watcher"));
        assert!(reason.contains("SendText"));
    }

    #[test]
    fn policy_decision_denied_unknown() {
        let d = ClientPolicyDecision::DeniedUnknown {
            client_id: ClientId("cl-unknown".to_string()),
        };
        assert!(d.is_denied());
        assert!(d.denial_reason().unwrap().contains("unknown"));
    }

    // -- Effective focus with no leader ---------------------------------------

    #[test]
    fn mirrored_no_leader_falls_back_to_own_focus() {
        let mut reg = ClientRegistry::new(default_config());
        // Only watchers — no leader.
        let w1 = reg.connect("w1", ClientRole::Watcher).unwrap();
        reg.set_view_mode(&w1, ViewMode::Mirrored);

        // Falls back to own (default) focus.
        let (tab, pane) = reg.effective_focus(&w1).unwrap();
        assert_eq!((tab, pane), (0, 0));
    }

    #[test]
    fn effective_focus_unknown_client_returns_none() {
        let reg = ClientRegistry::new(default_config());
        let fake = ClientId("cl-fake".to_string());
        assert!(reg.effective_focus(&fake).is_none());
    }

    // -- Serde roundtrip for view types ---------------------------------------

    #[test]
    fn client_role_serde_roundtrip() {
        for role in [ClientRole::Interactive, ClientRole::Watcher] {
            let json = serde_json::to_string(&role).unwrap();
            let back: ClientRole = serde_json::from_str(&json).unwrap();
            assert_eq!(role, back);
        }
    }

    #[test]
    fn view_mode_serde_roundtrip() {
        for mode in [ViewMode::Independent, ViewMode::Mirrored] {
            let json = serde_json::to_string(&mode).unwrap();
            let back: ViewMode = serde_json::from_str(&json).unwrap();
            assert_eq!(mode, back);
        }
    }

    #[test]
    fn client_summary_serde_roundtrip() {
        let summary = ClientSummary {
            id: ClientId("cl-test-0001".to_string()),
            name: "test-agent".to_string(),
            role: ClientRole::Watcher,
            active_tab: 2,
            active_pane: 7,
            view_mode: ViewMode::Mirrored,
            connected_at_ms: 1_700_000_000_000,
        };
        let json = serde_json::to_string(&summary).unwrap();
        let back: ClientSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "test-agent");
        assert_eq!(back.role, ClientRole::Watcher);
    }

    // -- Leader propagation ---------------------------------------------------

    #[test]
    fn leader_focus_propagates_to_mirrored_clients() {
        let mut reg = ClientRegistry::new(default_config());
        let leader = reg.connect("leader", ClientRole::Interactive).unwrap();
        let m1 = reg.connect("m1", ClientRole::Watcher).unwrap();
        let m2 = reg.connect("m2", ClientRole::Watcher).unwrap();
        let ind = reg.connect("ind", ClientRole::Watcher).unwrap();

        // m1, m2 mirrored; ind stays independent.
        reg.set_view_mode(&m1, ViewMode::Mirrored);
        reg.set_view_mode(&m2, ViewMode::Mirrored);

        // Leader changes focus.
        reg.set_focus(&leader, 4, 88);

        // Mirrored clients synced.
        assert_eq!(reg.get(&m1).unwrap().view_state.active_tab, 4);
        assert_eq!(reg.get(&m1).unwrap().view_state.active_pane, 88);
        assert_eq!(reg.get(&m2).unwrap().view_state.active_tab, 4);
        assert_eq!(reg.get(&m2).unwrap().view_state.active_pane, 88);

        // Independent client unaffected.
        assert_eq!(reg.get(&ind).unwrap().view_state.active_tab, 0);
        assert_eq!(reg.get(&ind).unwrap().view_state.active_pane, 0);
    }

    // -- Multiple interactive clients -----------------------------------------

    #[test]
    fn multiple_interactive_independent_focus() {
        let mut reg = ClientRegistry::new(default_config());
        let i1 = reg.connect("user-1", ClientRole::Interactive).unwrap();
        let i2 = reg.connect("user-2", ClientRole::Interactive).unwrap();

        reg.set_focus(&i1, 0, 1);
        reg.set_focus(&i2, 3, 7);

        assert_eq!(reg.effective_focus(&i1), Some((0, 1)));
        assert_eq!(reg.effective_focus(&i2), Some((3, 7)));
    }
}
