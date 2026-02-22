//! Subprocess bridge for `vibe_cockpit` session telemetry export.
//!
//! Wraps the `vibe_cockpit` CLI via [`SubprocessBridge`] to export
//! session telemetry and agent metrics. All calls are fail-open:
//! if `vibe_cockpit` is unavailable or returns malformed output,
//! default telemetry/metrics values are returned.
//!
//! Feature-gated behind `vc-export`.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::robot_envelope::RobotEnvelope;
use crate::subprocess_bridge::SubprocessBridge;

/// Session telemetry snapshot from vibe_cockpit.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct SessionTelemetry {
    #[serde(default)]
    pub session_id: Option<String>,
    /// Duration in seconds.
    #[serde(default)]
    pub duration_secs: f64,
    /// Number of commands executed in the session.
    #[serde(default)]
    pub commands: usize,
    /// Number of errors encountered.
    #[serde(default)]
    pub errors: usize,
    /// Number of agent interactions (tool calls, messages).
    #[serde(default)]
    pub agent_interactions: usize,
    /// Forward-compatibility for extra fields.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

impl SessionTelemetry {
    fn degraded(session_id: &str) -> Self {
        Self {
            session_id: Some(session_id.to_string()),
            ..Self::default()
        }
    }
}

/// Agent-level metrics from vibe_cockpit.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct AgentMetrics {
    #[serde(default)]
    pub agent_id: Option<String>,
    /// Total tokens consumed.
    #[serde(default)]
    pub total_tokens: u64,
    /// Number of tool invocations.
    #[serde(default)]
    pub tool_calls: usize,
    /// Session count for this agent.
    #[serde(default)]
    pub sessions: usize,
    /// Average session duration in seconds.
    #[serde(default)]
    pub avg_session_secs: f64,
    /// Forward-compatibility for extra fields.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

impl AgentMetrics {
    fn degraded(agent_id: &str) -> Self {
        Self {
            agent_id: Some(agent_id.to_string()),
            ..Self::default()
        }
    }
}

/// Vibe cockpit telemetry export bridge.
#[derive(Debug, Clone)]
pub struct VcExport {
    session_bridge: SubprocessBridge<SessionTelemetry>,
    metrics_bridge: SubprocessBridge<AgentMetrics>,
}

impl VcExport {
    /// Create a new export bridge looking for `vibe_cockpit` in PATH and `/dp`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            session_bridge: SubprocessBridge::new("vibe_cockpit"),
            metrics_bridge: SubprocessBridge::new("vibe_cockpit"),
        }
    }

    /// Check whether the `vibe_cockpit` binary can be found.
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.session_bridge.is_available()
    }

    /// Export session telemetry for a given session ID.
    ///
    /// Fail-open behavior: returns default/degraded telemetry on any bridge error.
    #[must_use]
    pub fn export_session_telemetry(&self, session_id: &str) -> SessionTelemetry {
        self.try_export_session_telemetry(session_id)
            .unwrap_or_else(|| SessionTelemetry::degraded(session_id))
    }

    /// Export agent-level metrics for a given agent ID.
    ///
    /// Fail-open behavior: returns default/degraded metrics on any bridge error.
    #[must_use]
    pub fn export_agent_metrics(&self, agent_id: &str) -> AgentMetrics {
        self.try_export_agent_metrics(agent_id)
            .unwrap_or_else(|| AgentMetrics::degraded(agent_id))
    }

    /// Export session telemetry wrapped in a standardized robot envelope.
    #[must_use]
    pub fn export_session_telemetry_envelope(
        &self,
        session_id: &str,
    ) -> RobotEnvelope<SessionTelemetry> {
        match self.try_export_session_telemetry(session_id) {
            Some(telemetry) => RobotEnvelope::wrap("vibe_cockpit", telemetry),
            None => {
                RobotEnvelope::wrap_degraded("vibe_cockpit", SessionTelemetry::degraded(session_id))
            }
        }
    }

    /// Export agent metrics wrapped in a standardized robot envelope.
    #[must_use]
    pub fn export_agent_metrics_envelope(&self, agent_id: &str) -> RobotEnvelope<AgentMetrics> {
        match self.try_export_agent_metrics(agent_id) {
            Some(metrics) => RobotEnvelope::wrap("vibe_cockpit", metrics),
            None => RobotEnvelope::wrap_degraded("vibe_cockpit", AgentMetrics::degraded(agent_id)),
        }
    }

    fn try_export_session_telemetry(&self, session_id: &str) -> Option<SessionTelemetry> {
        match self
            .session_bridge
            .invoke(&["export", "--format=json", "--session", session_id])
        {
            Ok(mut telemetry) => {
                if telemetry.session_id.is_none() {
                    telemetry.session_id = Some(session_id.to_string());
                }
                debug!(
                    bridge = "vibe_cockpit",
                    session = session_id,
                    metrics_count = telemetry.agent_interactions,
                    "session telemetry exported"
                );
                Some(telemetry)
            }
            Err(err) => {
                warn!(
                    bridge = "vibe_cockpit",
                    session = session_id,
                    error = %err,
                    "session telemetry export failed; using degraded fallback"
                );
                None
            }
        }
    }

    fn try_export_agent_metrics(&self, agent_id: &str) -> Option<AgentMetrics> {
        match self
            .metrics_bridge
            .invoke(&["metrics", "--format=json", "--agent", agent_id])
        {
            Ok(mut metrics) => {
                if metrics.agent_id.is_none() {
                    metrics.agent_id = Some(agent_id.to_string());
                }
                debug!(
                    bridge = "vibe_cockpit",
                    agent = agent_id,
                    metrics_count = metrics.tool_calls,
                    "agent metrics exported"
                );
                Some(metrics)
            }
            Err(err) => {
                warn!(
                    bridge = "vibe_cockpit",
                    agent = agent_id,
                    error = %err,
                    "agent metrics export failed; using degraded fallback"
                );
                None
            }
        }
    }
}

impl Default for VcExport {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn write_executable(path: &std::path::Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::write(path, body).unwrap();
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    #[cfg(unix)]
    fn fixture_vc_export(script_body: &str) -> VcExport {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("vibe_cockpit");
        write_executable(&bin, script_body);

        let bridge = SubprocessBridge::new(bin.to_string_lossy().as_ref());

        VcExport {
            session_bridge: bridge.clone(),
            metrics_bridge: bridge,
        }
    }

    #[test]
    fn test_session_telemetry_default_values() {
        let t = SessionTelemetry::default();
        assert!(t.session_id.is_none());
        assert_eq!(t.duration_secs, 0.0);
        assert_eq!(t.commands, 0);
        assert_eq!(t.errors, 0);
        assert_eq!(t.agent_interactions, 0);
    }

    #[test]
    fn test_session_telemetry_full_parse() {
        let json = r#"{
            "session_id": "sess-123",
            "duration_secs": 3600.5,
            "commands": 42,
            "errors": 3,
            "agent_interactions": 15
        }"#;
        let t: SessionTelemetry = serde_json::from_str(json).unwrap();
        assert_eq!(t.session_id, Some("sess-123".to_string()));
        assert!((t.duration_secs - 3600.5).abs() < 0.01);
        assert_eq!(t.commands, 42);
        assert_eq!(t.errors, 3);
        assert_eq!(t.agent_interactions, 15);
    }

    #[test]
    fn test_session_telemetry_forward_compat() {
        let json = r#"{"new_field": "surprise", "commands": 5}"#;
        let t: SessionTelemetry = serde_json::from_str(json).unwrap();
        assert_eq!(t.commands, 5);
        assert!(t.extra.contains_key("new_field"));
    }

    #[test]
    fn test_session_telemetry_serde_roundtrip() {
        let t = SessionTelemetry {
            session_id: Some("abc".to_string()),
            duration_secs: 120.0,
            commands: 10,
            errors: 1,
            agent_interactions: 5,
            extra: HashMap::new(),
        };
        let json = serde_json::to_string(&t).unwrap();
        let back: SessionTelemetry = serde_json::from_str(&json).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn test_session_telemetry_degraded_helper() {
        let t = SessionTelemetry::degraded("sess-1");
        assert_eq!(t.session_id, Some("sess-1".to_string()));
        assert_eq!(t.commands, 0);
    }

    #[test]
    fn test_agent_metrics_default_values() {
        let m = AgentMetrics::default();
        assert!(m.agent_id.is_none());
        assert_eq!(m.total_tokens, 0);
        assert_eq!(m.tool_calls, 0);
        assert_eq!(m.sessions, 0);
    }

    #[test]
    fn test_agent_metrics_full_parse() {
        let json = r#"{
            "agent_id": "agent-456",
            "total_tokens": 150000,
            "tool_calls": 200,
            "sessions": 5,
            "avg_session_secs": 1800.0
        }"#;
        let m: AgentMetrics = serde_json::from_str(json).unwrap();
        assert_eq!(m.agent_id, Some("agent-456".to_string()));
        assert_eq!(m.total_tokens, 150000);
        assert_eq!(m.tool_calls, 200);
        assert_eq!(m.sessions, 5);
    }

    #[test]
    fn test_agent_metrics_forward_compat() {
        let json = r#"{"total_tokens": 100, "cost_usd": 0.05}"#;
        let m: AgentMetrics = serde_json::from_str(json).unwrap();
        assert!(m.extra.contains_key("cost_usd"));
    }

    #[test]
    fn test_agent_metrics_serde_roundtrip() {
        let m = AgentMetrics {
            agent_id: Some("x".to_string()),
            total_tokens: 5000,
            tool_calls: 10,
            sessions: 2,
            avg_session_secs: 300.0,
            extra: HashMap::new(),
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: AgentMetrics = serde_json::from_str(&json).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn test_agent_metrics_degraded_helper() {
        let m = AgentMetrics::degraded("agent-a");
        assert_eq!(m.agent_id, Some("agent-a".to_string()));
        assert_eq!(m.total_tokens, 0);
    }

    #[test]
    fn test_vc_export_new() {
        let vc = VcExport::new();
        assert_eq!(vc.session_bridge.binary_name(), "vibe_cockpit");
    }

    #[test]
    fn test_vc_export_default() {
        let vc = VcExport::default();
        assert_eq!(vc.session_bridge.binary_name(), "vibe_cockpit");
    }

    #[test]
    fn test_fail_open_when_vc_unavailable() {
        let vc = VcExport {
            session_bridge: SubprocessBridge::new("definitely-missing-vc-xyz"),
            metrics_bridge: SubprocessBridge::new("definitely-missing-vc-xyz"),
        };
        let telemetry = vc.export_session_telemetry("s1");
        assert_eq!(telemetry.session_id, Some("s1".to_string()));
        assert_eq!(telemetry.commands, 0);
    }

    #[test]
    fn test_fail_open_agent_metrics_when_vc_unavailable() {
        let vc = VcExport {
            session_bridge: SubprocessBridge::new("definitely-missing-vc-xyz"),
            metrics_bridge: SubprocessBridge::new("definitely-missing-vc-xyz"),
        };
        let metrics = vc.export_agent_metrics("a1");
        assert_eq!(metrics.agent_id, Some("a1".to_string()));
        assert_eq!(metrics.total_tokens, 0);
    }

    #[test]
    fn test_fail_open_session_envelope_when_vc_unavailable() {
        let vc = VcExport {
            session_bridge: SubprocessBridge::new("definitely-missing-vc-xyz"),
            metrics_bridge: SubprocessBridge::new("definitely-missing-vc-xyz"),
        };
        let envelope = vc.export_session_telemetry_envelope("s1");
        assert!(envelope.degraded);
        assert_eq!(envelope.data.session_id, Some("s1".to_string()));
    }

    #[test]
    fn test_fail_open_agent_envelope_when_vc_unavailable() {
        let vc = VcExport {
            session_bridge: SubprocessBridge::new("definitely-missing-vc-xyz"),
            metrics_bridge: SubprocessBridge::new("definitely-missing-vc-xyz"),
        };
        let envelope = vc.export_agent_metrics_envelope("a1");
        assert!(envelope.degraded);
        assert_eq!(envelope.data.agent_id, Some("a1".to_string()));
    }

    #[test]
    fn test_is_available_false_when_vc_missing() {
        let vc = VcExport {
            session_bridge: SubprocessBridge::new("definitely-missing-vc-xyz"),
            metrics_bridge: SubprocessBridge::new("definitely-missing-vc-xyz"),
        };
        assert!(!vc.is_available());
    }

    #[cfg(unix)]
    #[test]
    fn test_export_session_telemetry_parses() {
        let vc = fixture_vc_export(
            "#!/bin/sh\nif [ \"$1\" = \"export\" ]; then printf '{\"duration_secs\":9.0,\"commands\":2,\"errors\":1,\"agent_interactions\":4}'; else printf '{\"total_tokens\":10,\"tool_calls\":2,\"sessions\":1,\"avg_session_secs\":5.0}'; fi\n",
        );

        let telemetry = vc.export_session_telemetry("sess-42");
        assert_eq!(telemetry.session_id, Some("sess-42".to_string()));
        assert_eq!(telemetry.commands, 2);
        assert_eq!(telemetry.errors, 1);
        assert_eq!(telemetry.agent_interactions, 4);
    }

    #[cfg(unix)]
    #[test]
    fn test_export_agent_metrics_parses() {
        let vc = fixture_vc_export(
            "#!/bin/sh\nif [ \"$1\" = \"export\" ]; then printf '{\"duration_secs\":2.0,\"commands\":1,\"errors\":0,\"agent_interactions\":1}'; else printf '{\"total_tokens\":777,\"tool_calls\":8,\"sessions\":3,\"avg_session_secs\":12.5}'; fi\n",
        );

        let metrics = vc.export_agent_metrics("agent-z");
        assert_eq!(metrics.agent_id, Some("agent-z".to_string()));
        assert_eq!(metrics.total_tokens, 777);
        assert_eq!(metrics.tool_calls, 8);
        assert_eq!(metrics.sessions, 3);
    }

    #[cfg(unix)]
    #[test]
    fn test_export_session_envelope_not_degraded_on_success() {
        let vc = fixture_vc_export(
            "#!/bin/sh\nif [ \"$1\" = \"export\" ]; then printf '{\"duration_secs\":1.0,\"commands\":1,\"errors\":0,\"agent_interactions\":1}'; else printf '{\"total_tokens\":1,\"tool_calls\":1,\"sessions\":1,\"avg_session_secs\":1.0}'; fi\n",
        );
        let envelope = vc.export_session_telemetry_envelope("s-ok");
        assert!(!envelope.degraded);
        assert_eq!(envelope.source, "vibe_cockpit");
    }

    #[cfg(unix)]
    #[test]
    fn test_export_agent_envelope_not_degraded_on_success() {
        let vc = fixture_vc_export(
            "#!/bin/sh\nif [ \"$1\" = \"export\" ]; then printf '{\"duration_secs\":1.0,\"commands\":1,\"errors\":0,\"agent_interactions\":1}'; else printf '{\"total_tokens\":1,\"tool_calls\":9,\"sessions\":2,\"avg_session_secs\":1.0}'; fi\n",
        );
        let envelope = vc.export_agent_metrics_envelope("a-ok");
        assert!(!envelope.degraded);
        assert_eq!(envelope.source, "vibe_cockpit");
        assert_eq!(envelope.data.agent_id, Some("a-ok".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn test_export_fills_session_id_when_missing() {
        let vc = fixture_vc_export(
            "#!/bin/sh\nif [ \"$1\" = \"export\" ]; then printf '{\"duration_secs\":3.0,\"commands\":5,\"errors\":0,\"agent_interactions\":2}'; else printf '{\"total_tokens\":11,\"tool_calls\":2,\"sessions\":1,\"avg_session_secs\":5.0}'; fi\n",
        );
        let telemetry = vc.export_session_telemetry("sess-fill");
        assert_eq!(telemetry.session_id, Some("sess-fill".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn test_export_fills_agent_id_when_missing() {
        let vc = fixture_vc_export(
            "#!/bin/sh\nif [ \"$1\" = \"export\" ]; then printf '{\"duration_secs\":3.0,\"commands\":5,\"errors\":0,\"agent_interactions\":2}'; else printf '{\"total_tokens\":44,\"tool_calls\":6,\"sessions\":7,\"avg_session_secs\":8.5}'; fi\n",
        );
        let metrics = vc.export_agent_metrics("agent-fill");
        assert_eq!(metrics.agent_id, Some("agent-fill".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn test_invalid_json_falls_back_to_degraded_defaults() {
        let vc = fixture_vc_export("#!/bin/sh\nprintf 'not-json'\n");
        let telemetry = vc.export_session_telemetry("sess-bad");
        let metrics = vc.export_agent_metrics("agent-bad");
        assert_eq!(telemetry.session_id, Some("sess-bad".to_string()));
        assert_eq!(telemetry.commands, 0);
        assert_eq!(metrics.agent_id, Some("agent-bad".to_string()));
        assert_eq!(metrics.total_tokens, 0);
    }
}
