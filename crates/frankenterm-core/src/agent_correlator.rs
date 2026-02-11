//! Agent session correlation and metadata capture for checkpoint snapshots.
//!
//! Detects which AI coding agent (Claude Code, Codex, Gemini, etc.) is running
//! in each pane and captures metadata for crash-resilient session persistence.
//!
//! # Detection sources (priority order)
//!
//! 1. **Pattern detection context**: Uses existing `DetectionContext.agent_type`
//!    populated by the pattern engine from terminal output matching.
//! 2. **Pane title**: Checks for agent name keywords in the WezTerm pane title.
//! 3. **Process name**: Checks the foreground process name if available.
//!
//! # State tracking
//!
//! Agent state is inferred from the most recent pattern detection rule_id:
//! - `*:banner` → "starting"
//! - `*:tool_use` → "working"
//! - `*:compaction` → "working"
//! - `*:rate_limited` or `*:usage.reached` → "rate_limited"
//! - `*:approval_needed` → "waiting_approval"
//! - `*:cost_summary` or `*:session.end` → "idle"
//!
//! If no recent detection exists, the state defaults to "active".

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tracing::{debug, trace};

use crate::patterns::{AgentType, Detection};
use crate::session_pane_state::AgentMetadata;
use crate::wezterm::PaneInfo;

/// Maximum age of a detection to consider for state inference.
const STATE_DETECTION_MAX_AGE: Duration = Duration::from_secs(300); // 5 minutes

// =============================================================================
// AgentCorrelator
// =============================================================================

/// Correlates pane observations with AI agent identities and states.
///
/// Maintains per-pane tracking of detected agents and their last known states.
/// Called by the checkpoint engine to populate `AgentMetadata` in pane snapshots.
#[derive(Debug)]
pub struct AgentCorrelator {
    /// Per-pane agent tracking state.
    pane_agents: HashMap<u64, PaneAgentState>,
}

/// Tracked agent state for a single pane.
#[derive(Debug, Clone)]
struct PaneAgentState {
    /// Detected agent type.
    agent_type: AgentType,
    /// How the agent was detected.
    source: DetectionSource,
    /// Agent session ID if extracted from patterns.
    session_id: Option<String>,
    /// Last inferred state (e.g., "working", "idle").
    last_state: String,
    /// When the state was last updated.
    last_state_at: Instant,
    /// Most recent rule_id that updated the state.
    last_rule_id: Option<String>,
}

/// How an agent was detected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetectionSource {
    /// Detected from pattern engine output matching.
    PatternEngine,
    /// Detected from pane title keywords.
    PaneTitle,
    /// Detected from foreground process name.
    ProcessName,
}

impl AgentCorrelator {
    /// Create a new agent correlator.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pane_agents: HashMap::new(),
        }
    }

    /// Update agent tracking from a batch of pattern detections.
    ///
    /// Called after each detection pass to capture agent identity and state
    /// from newly matched rules.
    pub fn ingest_detections(&mut self, pane_id: u64, detections: &[Detection]) {
        for detection in detections {
            if detection.agent_type == AgentType::Wezterm
                || detection.agent_type == AgentType::Unknown
            {
                continue;
            }

            let state = infer_state_from_rule(&detection.rule_id);
            let session_id = extract_session_id(&detection.extracted);

            let entry = self
                .pane_agents
                .entry(pane_id)
                .or_insert_with(|| PaneAgentState {
                    agent_type: detection.agent_type,
                    source: DetectionSource::PatternEngine,
                    session_id: None,
                    last_state: "active".to_string(),
                    last_state_at: Instant::now(),
                    last_rule_id: None,
                });

            // Update agent type if detection is more specific
            entry.agent_type = detection.agent_type;
            entry.source = DetectionSource::PatternEngine;
            entry.last_state = state.to_string();
            entry.last_state_at = Instant::now();
            entry.last_rule_id = Some(detection.rule_id.clone());

            if let Some(sid) = session_id {
                entry.session_id = Some(sid);
            }

            trace!(
                pane_id,
                agent = %entry.agent_type,
                state = %entry.last_state,
                rule = %detection.rule_id,
                "Agent state updated from detection"
            );
        }
    }

    /// Update agent tracking from pane info (title and process name).
    ///
    /// Called during checkpoint to ensure all panes have agent detection
    /// even without pattern matches (e.g., new panes not yet processed).
    pub fn update_from_pane_info(&mut self, pane: &PaneInfo) {
        if self.pane_agents.contains_key(&pane.pane_id) {
            return; // Already detected via patterns — don't downgrade
        }

        // Try title-based detection
        if let Some(agent_type) = detect_agent_from_title(pane.title.as_deref().unwrap_or("")) {
            self.pane_agents.insert(
                pane.pane_id,
                PaneAgentState {
                    agent_type,
                    source: DetectionSource::PaneTitle,
                    session_id: None,
                    last_state: "active".to_string(),
                    last_state_at: Instant::now(),
                    last_rule_id: None,
                },
            );
            debug!(
                pane_id = pane.pane_id,
                agent = %agent_type,
                "Agent detected from pane title"
            );
            return;
        }

        // Try process name detection (from foreground_process if available in extras)
        if let Some(process) = pane
            .extra
            .get("foreground_process_name")
            .and_then(|v| v.as_str())
        {
            if let Some(agent_type) = detect_agent_from_process(process) {
                self.pane_agents.insert(
                    pane.pane_id,
                    PaneAgentState {
                        agent_type,
                        source: DetectionSource::ProcessName,
                        session_id: None,
                        last_state: "active".to_string(),
                        last_state_at: Instant::now(),
                        last_rule_id: None,
                    },
                );
                debug!(
                    pane_id = pane.pane_id,
                    agent = %agent_type,
                    process = %process,
                    "Agent detected from process name"
                );
            }
        }
    }

    /// Get agent metadata for a pane, suitable for embedding in a checkpoint snapshot.
    ///
    /// Returns `None` if no agent has been detected for this pane.
    #[must_use]
    pub fn get_metadata(&self, pane_id: u64) -> Option<AgentMetadata> {
        let state = self.pane_agents.get(&pane_id)?;

        // Don't report stale state
        let effective_state = if state.last_state_at.elapsed() > STATE_DETECTION_MAX_AGE {
            "unknown".to_string()
        } else {
            state.last_state.clone()
        };

        Some(AgentMetadata {
            agent_type: state.agent_type.to_string(),
            session_id: state.session_id.clone(),
            state: Some(effective_state),
        })
    }

    /// Remove tracking for a pane (e.g., when pane is closed).
    pub fn remove_pane(&mut self, pane_id: u64) {
        self.pane_agents.remove(&pane_id);
    }

    /// Get the number of panes with detected agents.
    #[must_use]
    pub fn tracked_pane_count(&self) -> usize {
        self.pane_agents.len()
    }
}

impl Default for AgentCorrelator {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Detection helpers
// =============================================================================

/// Detect agent type from pane title keywords.
fn detect_agent_from_title(title: &str) -> Option<AgentType> {
    let lower = title.to_lowercase();
    if lower.contains("claude") || lower.contains("claude-code") || lower.contains("claude code") {
        return Some(AgentType::ClaudeCode);
    }
    if lower.contains("codex") || lower.contains("openai") {
        return Some(AgentType::Codex);
    }
    if lower.contains("gemini") {
        return Some(AgentType::Gemini);
    }
    None
}

/// Detect agent type from foreground process name.
fn detect_agent_from_process(process: &str) -> Option<AgentType> {
    let lower = process.to_lowercase();
    if lower.contains("claude") {
        return Some(AgentType::ClaudeCode);
    }
    if lower.contains("codex") {
        return Some(AgentType::Codex);
    }
    if lower.contains("gemini") {
        return Some(AgentType::Gemini);
    }
    None
}

/// Infer agent state from a detection rule_id.
fn infer_state_from_rule(rule_id: &str) -> &'static str {
    // Extract the suffix after the last ':'
    let suffix = rule_id.rsplit(':').next().unwrap_or(rule_id);

    match suffix {
        "banner" | "session.start" => "starting",
        "tool_use" | "compaction" => "working",
        "rate_limited" | "usage_reached" | "usage.reached" | "quota_exceeded" => "rate_limited",
        "approval_needed" | "approval_required" => "waiting_approval",
        "cost_summary" | "session.end" | "token_usage" => "idle",
        "auth.api_key_error" | "auth.login_required" | "auth.device_code_prompt" => "auth_error",
        _ if suffix.starts_with("usage.warning") => "active",
        _ => "active",
    }
}

/// Extract a session ID from detection extracted data (JSON).
fn extract_session_id(extracted: &serde_json::Value) -> Option<String> {
    // Try common patterns for session IDs in extracted data
    extracted
        .get("session_id")
        .or_else(|| extracted.get("resume_session_id"))
        .or_else(|| extracted.get("session"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patterns::Severity;

    fn make_detection(rule_id: &str, agent_type: AgentType) -> Detection {
        Detection {
            rule_id: rule_id.to_string(),
            agent_type,
            event_type: "test".to_string(),
            severity: Severity::Info,
            confidence: 0.9,
            extracted: serde_json::json!({}),
            matched_text: String::new(),
            span: (0, 0),
        }
    }

    fn make_detection_with_session(
        rule_id: &str,
        agent_type: AgentType,
        session_id: &str,
    ) -> Detection {
        Detection {
            rule_id: rule_id.to_string(),
            agent_type,
            event_type: "test".to_string(),
            severity: Severity::Info,
            confidence: 0.9,
            extracted: serde_json::json!({"session_id": session_id}),
            matched_text: String::new(),
            span: (0, 0),
        }
    }

    // ---- Agent detection from detections ----

    #[test]
    fn ingest_detection_tracks_agent() {
        let mut correlator = AgentCorrelator::new();
        let detections = vec![make_detection(
            "core.claude_code:banner",
            AgentType::ClaudeCode,
        )];

        correlator.ingest_detections(1, &detections);

        let meta = correlator.get_metadata(1).unwrap();
        assert_eq!(meta.agent_type, "claude_code");
        assert_eq!(meta.state.as_deref(), Some("starting"));
    }

    #[test]
    fn ingest_detection_updates_state() {
        let mut correlator = AgentCorrelator::new();

        correlator.ingest_detections(
            1,
            &[make_detection(
                "core.claude_code:banner",
                AgentType::ClaudeCode,
            )],
        );
        assert_eq!(
            correlator.get_metadata(1).unwrap().state.as_deref(),
            Some("starting")
        );

        correlator.ingest_detections(
            1,
            &[make_detection(
                "core.claude_code:tool_use",
                AgentType::ClaudeCode,
            )],
        );
        assert_eq!(
            correlator.get_metadata(1).unwrap().state.as_deref(),
            Some("working")
        );
    }

    #[test]
    fn ingest_detection_captures_session_id() {
        let mut correlator = AgentCorrelator::new();
        correlator.ingest_detections(
            1,
            &[make_detection_with_session(
                "core.codex:session.resume_hint",
                AgentType::Codex,
                "codex-sess-abc123",
            )],
        );

        let meta = correlator.get_metadata(1).unwrap();
        assert_eq!(meta.session_id.as_deref(), Some("codex-sess-abc123"));
    }

    // ---- Agent detection from pane info ----

    #[test]
    fn update_from_pane_info_detects_claude() {
        let mut correlator = AgentCorrelator::new();
        let pane = PaneInfo {
            pane_id: 5,
            tab_id: 0,
            window_id: 0,
            domain_id: None,
            domain_name: None,
            workspace: None,
            size: None,
            rows: None,
            cols: None,
            title: Some("claude-code ~/project".to_string()),
            cwd: None,
            tty_name: None,
            cursor_x: None,
            cursor_y: None,
            cursor_visibility: None,
            left_col: None,
            top_row: None,
            is_active: true,
            is_zoomed: false,
            extra: std::collections::HashMap::new(),
        };

        correlator.update_from_pane_info(&pane);

        let meta = correlator.get_metadata(5).unwrap();
        assert_eq!(meta.agent_type, "claude_code");
        assert_eq!(meta.state.as_deref(), Some("active"));
    }

    #[test]
    fn update_from_pane_info_does_not_overwrite_pattern_detection() {
        let mut correlator = AgentCorrelator::new();

        // First: detect via patterns (higher priority)
        correlator.ingest_detections(
            5,
            &[make_detection(
                "core.claude_code:tool_use",
                AgentType::ClaudeCode,
            )],
        );

        // Then: pane info update should NOT overwrite
        let pane = PaneInfo {
            pane_id: 5,
            tab_id: 0,
            window_id: 0,
            domain_id: None,
            domain_name: None,
            workspace: None,
            size: None,
            rows: None,
            cols: None,
            title: Some("gemini-cli".to_string()), // Different agent!
            cwd: None,
            tty_name: None,
            cursor_x: None,
            cursor_y: None,
            cursor_visibility: None,
            left_col: None,
            top_row: None,
            is_active: true,
            is_zoomed: false,
            extra: std::collections::HashMap::new(),
        };

        correlator.update_from_pane_info(&pane);

        // Should still be ClaudeCode from patterns, not Gemini from title
        let meta = correlator.get_metadata(5).unwrap();
        assert_eq!(meta.agent_type, "claude_code");
        assert_eq!(meta.state.as_deref(), Some("working"));
    }

    #[test]
    fn no_detection_returns_none() {
        let correlator = AgentCorrelator::new();
        assert!(correlator.get_metadata(99).is_none());
    }

    // ---- State inference from rule_id ----

    #[test]
    fn infer_state_banner() {
        assert_eq!(infer_state_from_rule("core.claude_code:banner"), "starting");
    }

    #[test]
    fn infer_state_tool_use() {
        assert_eq!(
            infer_state_from_rule("core.claude_code:tool_use"),
            "working"
        );
    }

    #[test]
    fn infer_state_rate_limited() {
        assert_eq!(
            infer_state_from_rule("core.codex:usage.reached"),
            "rate_limited"
        );
        assert_eq!(
            infer_state_from_rule("core.gemini:quota_exceeded"),
            "rate_limited"
        );
    }

    #[test]
    fn infer_state_idle() {
        assert_eq!(
            infer_state_from_rule("core.claude_code:cost_summary"),
            "idle"
        );
    }

    #[test]
    fn infer_state_auth_error() {
        assert_eq!(
            infer_state_from_rule("core.codex:auth.device_code_prompt"),
            "auth_error"
        );
    }

    #[test]
    fn infer_state_unknown_defaults_to_active() {
        assert_eq!(
            infer_state_from_rule("core.claude_code:some_new_rule"),
            "active"
        );
    }

    // ---- Title detection ----

    #[test]
    fn detect_agent_from_title_cases() {
        assert_eq!(
            detect_agent_from_title("claude-code ~/project"),
            Some(AgentType::ClaudeCode)
        );
        assert_eq!(
            detect_agent_from_title("Claude Code"),
            Some(AgentType::ClaudeCode)
        );
        assert_eq!(
            detect_agent_from_title("codex --model o4-mini"),
            Some(AgentType::Codex)
        );
        assert_eq!(
            detect_agent_from_title("openai chat"),
            Some(AgentType::Codex)
        );
        assert_eq!(
            detect_agent_from_title("gemini-cli"),
            Some(AgentType::Gemini)
        );
        assert_eq!(detect_agent_from_title("bash"), None);
        assert_eq!(detect_agent_from_title("vim"), None);
    }

    // ---- Process detection ----

    #[test]
    fn detect_agent_from_process_cases() {
        assert_eq!(
            detect_agent_from_process("claude-code"),
            Some(AgentType::ClaudeCode)
        );
        assert_eq!(detect_agent_from_process("codex"), Some(AgentType::Codex));
        assert_eq!(
            detect_agent_from_process("gemini-cli"),
            Some(AgentType::Gemini)
        );
        assert_eq!(detect_agent_from_process("bash"), None);
        assert_eq!(detect_agent_from_process("node"), None);
    }

    // ---- Session ID extraction ----

    #[test]
    fn extract_session_id_from_extracted() {
        let extracted = serde_json::json!({"session_id": "sess-123"});
        assert_eq!(extract_session_id(&extracted), Some("sess-123".to_string()));

        let extracted = serde_json::json!({"resume_session_id": "resume-456"});
        assert_eq!(
            extract_session_id(&extracted),
            Some("resume-456".to_string())
        );

        let extracted = serde_json::json!({"other_field": "value"});
        assert_eq!(extract_session_id(&extracted), None);

        let extracted = serde_json::json!({"session_id": ""});
        assert_eq!(extract_session_id(&extracted), None);
    }

    // ---- Pane removal ----

    #[test]
    fn remove_pane_clears_tracking() {
        let mut correlator = AgentCorrelator::new();
        correlator.ingest_detections(
            1,
            &[make_detection(
                "core.claude_code:banner",
                AgentType::ClaudeCode,
            )],
        );
        assert!(correlator.get_metadata(1).is_some());

        correlator.remove_pane(1);
        assert!(correlator.get_metadata(1).is_none());
    }

    // ---- Tracked count ----

    #[test]
    fn tracked_pane_count() {
        let mut correlator = AgentCorrelator::new();
        assert_eq!(correlator.tracked_pane_count(), 0);

        correlator.ingest_detections(
            1,
            &[make_detection(
                "core.claude_code:banner",
                AgentType::ClaudeCode,
            )],
        );
        correlator.ingest_detections(2, &[make_detection("core.codex:banner", AgentType::Codex)]);
        assert_eq!(correlator.tracked_pane_count(), 2);
    }

    // ---- Ignores Wezterm/Unknown agent types ----

    #[test]
    fn ignores_wezterm_and_unknown_agents() {
        let mut correlator = AgentCorrelator::new();
        correlator.ingest_detections(
            1,
            &[make_detection("core.wezterm:event", AgentType::Wezterm)],
        );
        correlator.ingest_detections(
            2,
            &[make_detection("unknown:something", AgentType::Unknown)],
        );
        assert_eq!(correlator.tracked_pane_count(), 0);
    }
}
