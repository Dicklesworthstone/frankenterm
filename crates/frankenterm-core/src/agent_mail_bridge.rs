//! Subprocess bridge for `mcp_agent_mail` inter-agent coordination.
//!
//! Wraps the `agent_mail` CLI (or a future MCP client) via
//! [`SubprocessBridge`] to exchange messages, reserve files, and
//! coordinate work across concurrent coding agents.  All calls
//! fail-open for read/reservation paths and explicit `Result` errors
//! for send/release paths.
//!
//! Feature-gated behind `agent-mail`.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, warn};

use crate::subprocess_bridge::SubprocessBridge;

// =============================================================================
// Types
// =============================================================================

/// Unique identifier for a sent message.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MessageId(pub String);

impl MessageId {
    /// Create a new message ID.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Get the inner string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A mail message received from or sent to another agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailMessage {
    /// Unique message identifier.
    #[serde(default)]
    pub id: Option<String>,
    /// Sender agent name.
    #[serde(default)]
    pub from: Option<String>,
    /// Recipient agent name.
    #[serde(default)]
    pub to: Option<String>,
    /// Message subject line.
    #[serde(default)]
    pub subject: String,
    /// Message body text.
    #[serde(default)]
    pub body: String,
    /// ISO-8601 timestamp of when the message was sent.
    #[serde(default)]
    pub timestamp: Option<String>,
    /// Whether this message has been read/acknowledged.
    #[serde(default)]
    pub read: bool,
    /// Message priority (lower = higher priority).
    #[serde(default)]
    pub priority: Option<u32>,
    /// Forward-compatibility.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Result of a file reservation request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReservationStatus {
    /// All requested files were reserved successfully.
    Granted,
    /// Some or all files are already reserved by another agent.
    Conflict,
    /// The reservation service is unavailable.
    Unavailable,
}

/// Outcome of a file reservation attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReservationResult {
    /// Overall reservation status.
    pub status: ReservationStatus,
    /// Files that were successfully reserved.
    #[serde(default)]
    pub granted: Vec<String>,
    /// Files that could not be reserved (held by others).
    #[serde(default)]
    pub conflicts: Vec<FileConflict>,
    /// Forward-compatibility.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// A file that could not be reserved because another agent holds it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileConflict {
    /// The conflicting file path.
    pub path: String,
    /// The agent currently holding the reservation.
    #[serde(default)]
    pub held_by: Option<String>,
    /// When the reservation was acquired.
    #[serde(default)]
    pub since: Option<String>,
    /// Forward-compatibility.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Agent registration info.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRegistration {
    /// Assigned agent ID.
    #[serde(default)]
    pub agent_id: Option<u64>,
    /// Assigned agent name.
    #[serde(default)]
    pub agent_name: Option<String>,
    /// Project path.
    #[serde(default)]
    pub project: Option<String>,
    /// Forward-compatibility.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Response wrapper for send_message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendResponse {
    /// ID of the sent message.
    #[serde(default)]
    pub message_id: Option<String>,
    /// Whether the send succeeded.
    #[serde(default)]
    pub success: bool,
    /// Forward-compatibility.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Response wrapper for release_files.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseResponse {
    /// Number of files released.
    #[serde(default)]
    pub released: usize,
    /// Whether the release succeeded.
    #[serde(default)]
    pub success: bool,
    /// Forward-compatibility.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Bridge-level failures for agent mail write/release operations.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AgentMailBridgeError {
    /// Message send failed.
    #[error("send_message failed: {0}")]
    SendFailed(String),
    /// File-release request failed.
    #[error("release_files failed: {0}")]
    ReleaseFailed(String),
}

// =============================================================================
// Bridge
// =============================================================================

/// Agent mail coordination bridge.
///
/// Wraps the `agent_mail` CLI to send messages, fetch inbox,
/// reserve/release files, and register agents.  All calls fail-open.
#[derive(Debug, Clone)]
pub struct AgentMailBridge {
    msg_bridge: SubprocessBridge<SendResponse>,
    inbox_bridge: SubprocessBridge<Vec<MailMessage>>,
    reserve_bridge: SubprocessBridge<ReservationResult>,
    release_bridge: SubprocessBridge<ReleaseResponse>,
    register_bridge: SubprocessBridge<AgentRegistration>,
}

/// The CLI binary name to look for.
const BINARY_NAME: &str = "agent_mail";

impl AgentMailBridge {
    /// Create a new agent mail bridge.
    ///
    /// Searches for the `agent_mail` binary in PATH and `/dp`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            msg_bridge: SubprocessBridge::new(BINARY_NAME),
            inbox_bridge: SubprocessBridge::new(BINARY_NAME),
            reserve_bridge: SubprocessBridge::new(BINARY_NAME),
            release_bridge: SubprocessBridge::new(BINARY_NAME),
            register_bridge: SubprocessBridge::new(BINARY_NAME),
        }
    }

    /// Check whether the `agent_mail` binary can be found.
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.msg_bridge.is_available()
    }

    /// Send a message to another agent.
    ///
    /// Returns [`AgentMailBridgeError::SendFailed`] on failure.
    pub fn send_message(
        &self,
        to: &str,
        subject: &str,
        body: &str,
    ) -> Result<MessageId, AgentMailBridgeError> {
        match self.msg_bridge.invoke(&[
            "send",
            "--format=json",
            "--to",
            to,
            "--subject",
            subject,
            "--body",
            body,
        ]) {
            Ok(resp) => {
                if resp.success {
                    let id = resp.message_id.unwrap_or_else(|| "unknown".to_string());
                    debug!(
                        agent_mail = true,
                        action = "send_message",
                        agent = to,
                        message_id = %id,
                        "message sent"
                    );
                    Ok(MessageId::new(id))
                } else {
                    warn!(
                        agent_mail = true,
                        action = "send_message",
                        agent = to,
                        "send_message returned success=false"
                    );
                    Err(AgentMailBridgeError::SendFailed(
                        "success=false".to_string(),
                    ))
                }
            }
            Err(err) => {
                warn!(
                    agent_mail = true,
                    action = "send_message",
                    agent = to,
                    error = %err,
                    "send_message failed"
                );
                Err(AgentMailBridgeError::SendFailed(err.to_string()))
            }
        }
    }

    /// Fetch the inbox for a given agent.
    ///
    /// Returns an empty vec on any failure (fail-open).
    pub fn fetch_inbox(&self, agent_name: &str) -> Vec<MailMessage> {
        match self
            .inbox_bridge
            .invoke(&["inbox", "--format=json", "--agent", agent_name])
        {
            Ok(messages) => {
                debug!(
                    bridge = "agent_mail",
                    agent = agent_name,
                    count = messages.len(),
                    "inbox fetched"
                );
                messages
            }
            Err(err) => {
                warn!(
                    bridge = "agent_mail",
                    agent = agent_name,
                    error = %err,
                    "fetch_inbox failed"
                );
                Vec::new()
            }
        }
    }

    /// Reserve files for exclusive editing by an agent.
    ///
    /// Returns [`ReservationResult`] with status [`Unavailable`](ReservationStatus::Unavailable)
    /// on bridge failure.
    pub fn reserve_files(&self, agent_name: &str, paths: &[&str]) -> ReservationResult {
        let paths_arg = paths.join(",");
        match self.reserve_bridge.invoke(&[
            "reserve",
            "--format=json",
            "--agent",
            agent_name,
            "--paths",
            &paths_arg,
        ]) {
            Ok(result) => {
                debug!(
                    bridge = "agent_mail",
                    agent = agent_name,
                    status = ?result.status,
                    granted = result.granted.len(),
                    conflicts = result.conflicts.len(),
                    "file reservation attempted"
                );
                result
            }
            Err(err) => {
                warn!(
                    bridge = "agent_mail",
                    agent = agent_name,
                    error = %err,
                    "reserve_files failed"
                );
                ReservationResult {
                    status: ReservationStatus::Unavailable,
                    granted: Vec::new(),
                    conflicts: Vec::new(),
                    extra: HashMap::new(),
                }
            }
        }
    }

    /// Release all file reservations held by an agent.
    ///
    /// Returns [`AgentMailBridgeError::ReleaseFailed`] on failure.
    pub fn release_files(&self, agent_name: &str) -> Result<(), AgentMailBridgeError> {
        match self
            .release_bridge
            .invoke(&["release", "--format=json", "--agent", agent_name])
        {
            Ok(resp) => {
                if resp.success {
                    debug!(
                        agent_mail = true,
                        action = "release_files",
                        agent = agent_name,
                        released = resp.released,
                        "files released"
                    );
                    Ok(())
                } else {
                    warn!(
                        agent_mail = true,
                        action = "release_files",
                        agent = agent_name,
                        released = resp.released,
                        "release_files returned success=false"
                    );
                    Err(AgentMailBridgeError::ReleaseFailed(
                        "success=false".to_string(),
                    ))
                }
            }
            Err(err) => {
                warn!(
                    agent_mail = true,
                    action = "release_files",
                    agent = agent_name,
                    error = %err,
                    "release_files failed"
                );
                Err(AgentMailBridgeError::ReleaseFailed(err.to_string()))
            }
        }
    }

    /// Register an agent with the coordination server.
    ///
    /// Returns `None` on failure (fail-open).
    pub fn register_agent(&self, agent_name: &str, project: &str) -> Option<AgentRegistration> {
        match self.register_bridge.invoke(&[
            "register",
            "--format=json",
            "--name",
            agent_name,
            "--project",
            project,
        ]) {
            Ok(reg) => {
                debug!(
                    bridge = "agent_mail",
                    agent = agent_name,
                    agent_id = ?reg.agent_id,
                    "agent registered"
                );
                Some(reg)
            }
            Err(err) => {
                warn!(
                    bridge = "agent_mail",
                    agent = agent_name,
                    error = %err,
                    "register_agent failed"
                );
                None
            }
        }
    }

    /// Fetch unread message count for quick inbox polling.
    ///
    /// Returns 0 on failure (fail-open).
    pub fn unread_count(&self, agent_name: &str) -> usize {
        self.fetch_inbox(agent_name)
            .iter()
            .filter(|m| !m.read)
            .count()
    }
}

impl Default for AgentMailBridge {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------------------
    // MessageId
    // -------------------------------------------------------------------------

    #[test]
    fn test_message_id_new() {
        let id = MessageId::new("msg-123");
        assert_eq!(id.as_str(), "msg-123");
    }

    #[test]
    fn test_message_id_equality() {
        let a = MessageId::new("x");
        let b = MessageId::new("x");
        assert_eq!(a, b);
    }

    #[test]
    fn test_message_id_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(MessageId::new("a"));
        set.insert(MessageId::new("a"));
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn test_message_id_serde_roundtrip() {
        let id = MessageId::new("msg-456");
        let json = serde_json::to_string(&id).unwrap();
        let back: MessageId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn test_message_id_from_string() {
        let id = MessageId::new(String::from("owned"));
        assert_eq!(id.as_str(), "owned");
    }

    // -------------------------------------------------------------------------
    // MailMessage
    // -------------------------------------------------------------------------

    #[test]
    fn test_mail_message_full() {
        let json = r#"{
            "id": "m-1",
            "from": "DarkMill",
            "to": "BlueLake",
            "subject": "Build status",
            "body": "Tests pass",
            "timestamp": "2026-02-22T08:00:00Z",
            "read": false,
            "priority": 1
        }"#;
        let msg: MailMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.id, Some("m-1".to_string()));
        assert_eq!(msg.from, Some("DarkMill".to_string()));
        assert_eq!(msg.to, Some("BlueLake".to_string()));
        assert_eq!(msg.subject, "Build status");
        assert_eq!(msg.body, "Tests pass");
        assert!(!msg.read);
        assert_eq!(msg.priority, Some(1));
    }

    #[test]
    fn test_mail_message_minimal() {
        let json = "{}";
        let msg: MailMessage = serde_json::from_str(json).unwrap();
        assert!(msg.id.is_none());
        assert!(msg.from.is_none());
        assert!(msg.to.is_none());
        assert!(msg.subject.is_empty());
        assert!(msg.body.is_empty());
        assert!(!msg.read);
        assert!(msg.priority.is_none());
    }

    #[test]
    fn test_mail_message_serde_roundtrip() {
        let msg = MailMessage {
            id: Some("m-2".to_string()),
            from: Some("A".to_string()),
            to: Some("B".to_string()),
            subject: "Hello".to_string(),
            body: "World".to_string(),
            timestamp: Some("2026-01-01T00:00:00Z".to_string()),
            read: true,
            priority: Some(3),
            extra: HashMap::new(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: MailMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, msg.id);
        assert_eq!(back.subject, "Hello");
        assert!(back.read);
    }

    #[test]
    fn test_mail_message_forward_compat() {
        let json = r#"{"subject": "test", "new_field": 42}"#;
        let msg: MailMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.subject, "test");
        assert!(msg.extra.contains_key("new_field"));
    }

    #[test]
    fn test_mail_message_empty_body() {
        let msg = MailMessage {
            id: None,
            from: None,
            to: None,
            subject: String::new(),
            body: String::new(),
            timestamp: None,
            read: false,
            priority: None,
            extra: HashMap::new(),
        };
        assert!(msg.body.is_empty());
        assert!(msg.subject.is_empty());
    }

    // -------------------------------------------------------------------------
    // ReservationStatus
    // -------------------------------------------------------------------------

    #[test]
    fn test_reservation_status_granted() {
        let json = r#""granted""#;
        let s: ReservationStatus = serde_json::from_str(json).unwrap();
        assert_eq!(s, ReservationStatus::Granted);
    }

    #[test]
    fn test_reservation_status_conflict() {
        let json = r#""conflict""#;
        let s: ReservationStatus = serde_json::from_str(json).unwrap();
        assert_eq!(s, ReservationStatus::Conflict);
    }

    #[test]
    fn test_reservation_status_unavailable() {
        let json = r#""unavailable""#;
        let s: ReservationStatus = serde_json::from_str(json).unwrap();
        assert_eq!(s, ReservationStatus::Unavailable);
    }

    #[test]
    fn test_reservation_status_serde_roundtrip() {
        for status in [
            ReservationStatus::Granted,
            ReservationStatus::Conflict,
            ReservationStatus::Unavailable,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let back: ReservationStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(back, status);
        }
    }

    // -------------------------------------------------------------------------
    // ReservationResult
    // -------------------------------------------------------------------------

    #[test]
    fn test_reservation_result_granted() {
        let json = r#"{
            "status": "granted",
            "granted": ["src/lib.rs", "src/main.rs"],
            "conflicts": []
        }"#;
        let r: ReservationResult = serde_json::from_str(json).unwrap();
        assert_eq!(r.status, ReservationStatus::Granted);
        assert_eq!(r.granted.len(), 2);
        assert!(r.conflicts.is_empty());
    }

    #[test]
    fn test_reservation_result_conflict() {
        let json = r#"{
            "status": "conflict",
            "granted": [],
            "conflicts": [
                {"path": "src/lib.rs", "held_by": "BlueLake"}
            ]
        }"#;
        let r: ReservationResult = serde_json::from_str(json).unwrap();
        assert_eq!(r.status, ReservationStatus::Conflict);
        assert_eq!(r.conflicts.len(), 1);
        assert_eq!(r.conflicts[0].path, "src/lib.rs");
        assert_eq!(r.conflicts[0].held_by, Some("BlueLake".to_string()));
    }

    #[test]
    fn test_reservation_result_minimal() {
        let json = r#"{"status": "granted"}"#;
        let r: ReservationResult = serde_json::from_str(json).unwrap();
        assert_eq!(r.status, ReservationStatus::Granted);
        assert!(r.granted.is_empty());
        assert!(r.conflicts.is_empty());
    }

    #[test]
    fn test_reservation_result_forward_compat() {
        let json = r#"{"status": "granted", "ttl_secs": 300}"#;
        let r: ReservationResult = serde_json::from_str(json).unwrap();
        assert!(r.extra.contains_key("ttl_secs"));
    }

    // -------------------------------------------------------------------------
    // FileConflict
    // -------------------------------------------------------------------------

    #[test]
    fn test_file_conflict_full() {
        let json = r#"{
            "path": "src/main.rs",
            "held_by": "GreenCliff",
            "since": "2026-02-22T07:00:00Z"
        }"#;
        let c: FileConflict = serde_json::from_str(json).unwrap();
        assert_eq!(c.path, "src/main.rs");
        assert_eq!(c.held_by, Some("GreenCliff".to_string()));
        assert!(c.since.is_some());
    }

    #[test]
    fn test_file_conflict_minimal() {
        let json = r#"{"path": "foo.rs"}"#;
        let c: FileConflict = serde_json::from_str(json).unwrap();
        assert_eq!(c.path, "foo.rs");
        assert!(c.held_by.is_none());
        assert!(c.since.is_none());
    }

    #[test]
    fn test_file_conflict_forward_compat() {
        let json = r#"{"path": "x.rs", "lock_type": "exclusive"}"#;
        let c: FileConflict = serde_json::from_str(json).unwrap();
        assert!(c.extra.contains_key("lock_type"));
    }

    // -------------------------------------------------------------------------
    // AgentRegistration
    // -------------------------------------------------------------------------

    #[test]
    fn test_agent_registration_full() {
        let json = r#"{
            "agent_id": 842,
            "agent_name": "DarkMill",
            "project": "/Users/jemanuel/projects/frankenterm"
        }"#;
        let reg: AgentRegistration = serde_json::from_str(json).unwrap();
        assert_eq!(reg.agent_id, Some(842));
        assert_eq!(reg.agent_name, Some("DarkMill".to_string()));
    }

    #[test]
    fn test_agent_registration_minimal() {
        let json = "{}";
        let reg: AgentRegistration = serde_json::from_str(json).unwrap();
        assert!(reg.agent_id.is_none());
        assert!(reg.agent_name.is_none());
        assert!(reg.project.is_none());
    }

    #[test]
    fn test_agent_registration_serde_roundtrip() {
        let reg = AgentRegistration {
            agent_id: Some(100),
            agent_name: Some("TestBot".to_string()),
            project: Some("/tmp/test".to_string()),
            extra: HashMap::new(),
        };
        let json = serde_json::to_string(&reg).unwrap();
        let back: AgentRegistration = serde_json::from_str(&json).unwrap();
        assert_eq!(back.agent_id, Some(100));
    }

    // -------------------------------------------------------------------------
    // SendResponse
    // -------------------------------------------------------------------------

    #[test]
    fn test_send_response_success() {
        let json = r#"{"message_id": "m-99", "success": true}"#;
        let resp: SendResponse = serde_json::from_str(json).unwrap();
        assert!(resp.success);
        assert_eq!(resp.message_id, Some("m-99".to_string()));
    }

    #[test]
    fn test_send_response_failure() {
        let json = r#"{"success": false}"#;
        let resp: SendResponse = serde_json::from_str(json).unwrap();
        assert!(!resp.success);
        assert!(resp.message_id.is_none());
    }

    // -------------------------------------------------------------------------
    // ReleaseResponse
    // -------------------------------------------------------------------------

    #[test]
    fn test_release_response_success() {
        let json = r#"{"released": 3, "success": true}"#;
        let resp: ReleaseResponse = serde_json::from_str(json).unwrap();
        assert!(resp.success);
        assert_eq!(resp.released, 3);
    }

    #[test]
    fn test_release_response_none_released() {
        let json = r#"{"released": 0, "success": true}"#;
        let resp: ReleaseResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.released, 0);
    }

    // -------------------------------------------------------------------------
    // AgentMailBridge construction
    // -------------------------------------------------------------------------

    #[test]
    fn test_bridge_new() {
        let bridge = AgentMailBridge::new();
        assert_eq!(bridge.msg_bridge.binary_name(), BINARY_NAME);
    }

    #[test]
    fn test_bridge_default() {
        let bridge = AgentMailBridge::default();
        assert_eq!(bridge.msg_bridge.binary_name(), BINARY_NAME);
    }

    #[cfg(unix)]
    fn write_executable(path: &std::path::Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::write(path, body).unwrap();
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    #[cfg(unix)]
    fn fixture_bridge(script_body: &str) -> (tempfile::TempDir, AgentMailBridge) {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join(BINARY_NAME);
        write_executable(&bin, script_body);
        let bin_path = bin.to_string_lossy().into_owned();

        (
            dir,
            AgentMailBridge {
                msg_bridge: SubprocessBridge::new(&bin_path),
                inbox_bridge: SubprocessBridge::new(&bin_path),
                reserve_bridge: SubprocessBridge::new(&bin_path),
                release_bridge: SubprocessBridge::new(&bin_path),
                register_bridge: SubprocessBridge::new(&bin_path),
            },
        )
    }

    // -------------------------------------------------------------------------
    // Fail-open behavior
    // -------------------------------------------------------------------------

    fn unavailable_bridge() -> AgentMailBridge {
        AgentMailBridge {
            msg_bridge: SubprocessBridge::new("definitely-missing-agent-mail-xyz"),
            inbox_bridge: SubprocessBridge::new("definitely-missing-agent-mail-xyz"),
            reserve_bridge: SubprocessBridge::new("definitely-missing-agent-mail-xyz"),
            release_bridge: SubprocessBridge::new("definitely-missing-agent-mail-xyz"),
            register_bridge: SubprocessBridge::new("definitely-missing-agent-mail-xyz"),
        }
    }

    #[test]
    fn test_fail_open_send_message() {
        let bridge = unavailable_bridge();
        assert!(bridge.send_message("B", "hi", "hello").is_err());
    }

    #[test]
    fn test_fail_open_fetch_inbox() {
        let bridge = unavailable_bridge();
        assert!(bridge.fetch_inbox("A").is_empty());
    }

    #[test]
    fn test_fail_open_reserve_files() {
        let bridge = unavailable_bridge();
        let result = bridge.reserve_files("A", &["src/lib.rs"]);
        assert_eq!(result.status, ReservationStatus::Unavailable);
        assert!(result.granted.is_empty());
        assert!(result.conflicts.is_empty());
    }

    #[test]
    fn test_fail_open_release_files() {
        let bridge = unavailable_bridge();
        assert!(bridge.release_files("A").is_err());
    }

    #[test]
    fn test_fail_open_register_agent() {
        let bridge = unavailable_bridge();
        assert!(bridge.register_agent("A", "/tmp/proj").is_none());
    }

    #[test]
    fn test_fail_open_is_available() {
        let bridge = unavailable_bridge();
        assert!(!bridge.is_available());
    }

    #[test]
    fn test_fail_open_unread_count() {
        let bridge = unavailable_bridge();
        assert_eq!(bridge.unread_count("A"), 0);
    }

    #[test]
    fn test_fail_open_when_agent_mail_unavailable() {
        let bridge = unavailable_bridge();
        assert!(bridge.send_message("B", "subject", "body").is_err());
        assert!(bridge.fetch_inbox("A").is_empty());
        let reservation = bridge.reserve_files("A", &["src/lib.rs"]);
        assert_eq!(reservation.status, ReservationStatus::Unavailable);
        assert!(bridge.release_files("A").is_err());
    }

    // -------------------------------------------------------------------------
    // Method behavior with fixture binary
    // -------------------------------------------------------------------------

    #[cfg(unix)]
    #[test]
    fn test_send_message_returns_id() {
        let (_dir, bridge) = fixture_bridge(
            "#!/bin/sh\ncase \"$1\" in\n  send) printf '{\"message_id\":\"m-123\",\"success\":true}' ;;\n  inbox) printf '[]' ;;\n  reserve) printf '{\"status\":\"granted\",\"granted\":[],\"conflicts\":[]}' ;;\n  release) printf '{\"released\":0,\"success\":true}' ;;\n  register) printf '{\"agent_id\":1,\"agent_name\":\"A\"}' ;;\n  *) printf '{}' ;;\nesac\n",
        );
        let id = bridge.send_message("B", "subj", "body").unwrap();
        assert_eq!(id.as_str(), "m-123");
    }

    #[cfg(unix)]
    #[test]
    fn test_fetch_inbox_parses_messages() {
        let (_dir, bridge) = fixture_bridge(
            "#!/bin/sh\ncase \"$1\" in\n  inbox) printf '[{\"id\":\"m-1\",\"from\":\"A\",\"to\":\"B\",\"subject\":\"Hi\",\"body\":\"Hello\",\"read\":false}]' ;;\n  send) printf '{\"message_id\":\"m-123\",\"success\":true}' ;;\n  reserve) printf '{\"status\":\"granted\",\"granted\":[],\"conflicts\":[]}' ;;\n  release) printf '{\"released\":0,\"success\":true}' ;;\n  register) printf '{\"agent_id\":1,\"agent_name\":\"A\"}' ;;\n  *) printf '{}' ;;\nesac\n",
        );
        let inbox = bridge.fetch_inbox("B");
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].subject, "Hi");
    }

    #[cfg(unix)]
    #[test]
    fn test_reserve_files_granted() {
        let (_dir, bridge) = fixture_bridge(
            "#!/bin/sh\ncase \"$1\" in\n  reserve) printf '{\"status\":\"granted\",\"granted\":[\"src/lib.rs\"],\"conflicts\":[]}' ;;\n  send) printf '{\"message_id\":\"m-123\",\"success\":true}' ;;\n  inbox) printf '[]' ;;\n  release) printf '{\"released\":0,\"success\":true}' ;;\n  register) printf '{\"agent_id\":1,\"agent_name\":\"A\"}' ;;\n  *) printf '{}' ;;\nesac\n",
        );
        let result = bridge.reserve_files("A", &["src/lib.rs"]);
        assert_eq!(result.status, ReservationStatus::Granted);
        assert_eq!(result.granted, vec!["src/lib.rs".to_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn test_reserve_files_conflict() {
        let (_dir, bridge) = fixture_bridge(
            "#!/bin/sh\ncase \"$1\" in\n  reserve) printf '{\"status\":\"conflict\",\"granted\":[],\"conflicts\":[{\"path\":\"src/lib.rs\",\"held_by\":\"OtherAgent\"}]}' ;;\n  send) printf '{\"message_id\":\"m-123\",\"success\":true}' ;;\n  inbox) printf '[]' ;;\n  release) printf '{\"released\":0,\"success\":true}' ;;\n  register) printf '{\"agent_id\":1,\"agent_name\":\"A\"}' ;;\n  *) printf '{}' ;;\nesac\n",
        );
        let result = bridge.reserve_files("A", &["src/lib.rs"]);
        assert_eq!(result.status, ReservationStatus::Conflict);
        assert_eq!(result.conflicts.len(), 1);
        assert_eq!(result.conflicts[0].path, "src/lib.rs");
    }

    #[cfg(unix)]
    #[test]
    fn test_release_files_succeeds() {
        let (_dir, bridge) = fixture_bridge(
            "#!/bin/sh\ncase \"$1\" in\n  release) printf '{\"released\":2,\"success\":true}' ;;\n  send) printf '{\"message_id\":\"m-123\",\"success\":true}' ;;\n  inbox) printf '[]' ;;\n  reserve) printf '{\"status\":\"granted\",\"granted\":[],\"conflicts\":[]}' ;;\n  register) printf '{\"agent_id\":1,\"agent_name\":\"A\"}' ;;\n  *) printf '{}' ;;\nesac\n",
        );
        let released = bridge.release_files("A");
        assert!(released.is_ok());
    }

    // -------------------------------------------------------------------------
    // Edge cases
    // -------------------------------------------------------------------------

    #[test]
    fn test_mail_message_unicode_body() {
        let json = r#"{"subject": "🤖", "body": "日本語テスト"}"#;
        let msg: MailMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.subject, "🤖");
        assert_eq!(msg.body, "日本語テスト");
    }

    #[test]
    fn test_reservation_many_files() {
        let json = r#"{
            "status": "granted",
            "granted": ["a.rs", "b.rs", "c.rs", "d.rs", "e.rs"]
        }"#;
        let r: ReservationResult = serde_json::from_str(json).unwrap();
        assert_eq!(r.granted.len(), 5);
    }

    #[test]
    fn test_message_id_debug() {
        let id = MessageId::new("test");
        let dbg = format!("{:?}", id);
        assert!(dbg.contains("test"));
    }

    #[test]
    fn test_mail_message_read_flag() {
        let unread: MailMessage = serde_json::from_str(r#"{"read": false}"#).unwrap();
        let read: MailMessage = serde_json::from_str(r#"{"read": true}"#).unwrap();
        assert!(!unread.read);
        assert!(read.read);
    }
}
