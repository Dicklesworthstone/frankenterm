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

use std::collections::{BTreeMap, HashMap};
#[cfg(feature = "agent-detection")]
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tracing::{debug, trace};

use crate::agent_provider::AgentProvider;
use crate::patterns::{AgentType, Detection};
use crate::session_pane_state::AgentMetadata;
use crate::wezterm::PaneInfo;

/// Maximum age of a detection to consider for state inference.
const STATE_DETECTION_MAX_AGE: Duration = Duration::from_secs(300); // 5 minutes

/// Agent installation inventory entry for robot/API consumers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstalledAgentInventoryEntry {
    pub slug: String,
    pub detected: bool,
    pub evidence: Vec<String>,
    pub root_paths: Vec<String>,
    pub config_path: Option<String>,
    pub binary_path: Option<String>,
    pub version: Option<String>,
}

/// Running agent inventory entry keyed by pane id.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunningAgentInventoryEntry {
    pub slug: String,
    pub state: String,
    pub session_id: Option<String>,
    pub source: DetectionSource,
}

/// Unified installed + running inventory snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentInventory {
    pub installed: Vec<InstalledAgentInventoryEntry>,
    pub running: BTreeMap<u64, RunningAgentInventoryEntry>,
}

#[cfg(feature = "agent-detection")]
static INSTALLED_INVENTORY_CACHE: LazyLock<Mutex<Option<Vec<InstalledAgentInventoryEntry>>>> =
    LazyLock::new(|| Mutex::new(None));

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
    /// Canonical provider identity for cross-module integration.
    provider: AgentProvider,
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
            let provider_diagnostics =
                AgentProvider::diagnostics_from_agent_type(&detection.agent_type);
            let provider = provider_diagnostics
                .selected
                .clone()
                .unwrap_or_else(|| AgentProvider::Unknown("unknown".to_string()));

            let entry = self
                .pane_agents
                .entry(pane_id)
                .or_insert_with(|| PaneAgentState {
                    agent_type: detection.agent_type,
                    provider: provider.clone(),
                    source: DetectionSource::PatternEngine,
                    session_id: None,
                    last_state: "active".to_string(),
                    last_state_at: Instant::now(),
                    last_rule_id: None,
                });

            // Update agent type if detection is more specific
            entry.agent_type = detection.agent_type;
            entry.provider = provider;
            entry.source = DetectionSource::PatternEngine;
            entry.last_state = state.to_string();
            entry.last_state_at = Instant::now();
            entry.last_rule_id = Some(detection.rule_id.clone());

            if let Some(sid) = session_id {
                entry.session_id = Some(sid);
            }

            trace!(
                pane_id,
                source = ?provider_diagnostics.source,
                provider_candidates = ?provider_diagnostics.candidates,
                provider_selected = ?provider_diagnostics.selected,
                "Provider resolved from pattern agent type"
            );

            trace!(
                pane_id,
                agent = %entry.provider,
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
                    provider: AgentProvider::from_agent_type(&agent_type),
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
                        provider: AgentProvider::from_agent_type(&agent_type),
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
            agent_type: if state.agent_type != AgentType::Unknown {
                state.agent_type.to_string()
            } else {
                metadata_agent_type_for_provider(&state.provider)
            },
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

    /// Build a unified running/installed inventory snapshot.
    ///
    /// Running inventory is always available from correlator state. Installed
    /// inventory is best-effort and may be empty when filesystem detection is
    /// disabled or unavailable in the current build.
    #[must_use]
    pub fn inventory(&self) -> AgentInventory {
        let running = self
            .pane_agents
            .iter()
            .map(|(pane_id, state)| {
                let effective_state = if state.last_state_at.elapsed() > STATE_DETECTION_MAX_AGE {
                    "unknown".to_string()
                } else {
                    state.last_state.clone()
                };
                (
                    *pane_id,
                    RunningAgentInventoryEntry {
                        slug: state.provider.canonical_name().to_string(),
                        state: effective_state,
                        session_id: state.session_id.clone(),
                        source: state.source,
                    },
                )
            })
            .collect();

        AgentInventory {
            installed: installed_inventory_cached().unwrap_or_default(),
            running,
        }
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
    let normalized = title.to_ascii_lowercase();
    let mut candidates = Vec::new();
    if normalized.contains("claude")
        || normalized.contains("claude-code")
        || normalized.contains("claude code")
    {
        candidates.push(AgentType::ClaudeCode);
    }
    if normalized.contains("codex") || normalized.contains("openai") {
        candidates.push(AgentType::Codex);
    }
    if normalized.contains("gemini") {
        candidates.push(AgentType::Gemini);
    }

    if candidates.is_empty() {
        trace!(
            source = "pane_title",
            input = %title,
            normalized = %normalized,
            "No agent keywords matched pane title"
        );
        return None;
    }

    if candidates.len() > 1 {
        debug!(
            source = "pane_title",
            input = %title,
            normalized = %normalized,
            candidates = ?candidates,
            "Ambiguous pane-title agent match; selecting first candidate"
        );
    }

    candidates.into_iter().next()
}

/// Detect agent type from foreground process name.
fn detect_agent_from_process(process: &str) -> Option<AgentType> {
    let diagnostics = AgentProvider::diagnostics_from_process_name(process);
    if diagnostics.ambiguous {
        debug!(
            source = "process_name",
            input = %diagnostics.input,
            normalized = %diagnostics.normalized,
            candidates = ?diagnostics.candidates,
            selected = ?diagnostics.selected,
            "Ambiguous process-name provider match; selecting first candidate"
        );
    }
    if diagnostics.is_unknown() {
        trace!(
            source = "process_name",
            input = %diagnostics.input,
            normalized = %diagnostics.normalized,
            "No known provider matched process name"
        );
        return None;
    }
    let provider = diagnostics.selected?;
    let agent_type = provider.to_agent_type();
    if agent_type == AgentType::Unknown {
        trace!(
            source = "process_name",
            input = %process,
            provider = %provider,
            "Resolved provider has no legacy AgentType mapping for correlator"
        );
        return None;
    }
    Some(agent_type)
}

fn metadata_agent_type_for_provider(provider: &AgentProvider) -> String {
    let legacy = provider.to_agent_type();
    if legacy != AgentType::Unknown {
        legacy.to_string()
    } else {
        provider.canonical_name().to_string()
    }
}

/// Infer agent state from a detection rule_id.
fn infer_state_from_rule(rule_id: &str) -> &'static str {
    // Rule IDs historically used `provider:event` and now primarily use
    // `provider.event` names. Match on terminal suffixes so both shapes map
    // to the same state semantics.
    let normalized = rule_id.trim();
    if normalized.is_empty() {
        return "active";
    }

    if normalized.ends_with("banner") || normalized.ends_with("session.start") {
        return "starting";
    }

    if normalized.ends_with("compaction.complete")
        || normalized.ends_with("compaction_complete")
        || normalized.ends_with("cost_summary")
        || normalized.ends_with("session.summary")
        || normalized.ends_with("session.end")
        || normalized.ends_with("token_usage")
    {
        return "idle";
    }

    if normalized.ends_with("tool_use") || normalized.ends_with("compaction") {
        return "working";
    }

    if normalized.ends_with("rate_limited")
        || normalized.ends_with("usage_reached")
        || normalized.ends_with("usage.reached")
        || normalized.ends_with("quota_exceeded")
    {
        return "rate_limited";
    }

    if normalized.ends_with("approval_needed") || normalized.ends_with("approval_required") {
        return "waiting_approval";
    }

    if normalized.ends_with("auth.api_key_error")
        || normalized.ends_with("auth.login_required")
        || normalized.ends_with("auth.device_code_prompt")
    {
        return "auth_error";
    }

    if normalized.contains("usage.warning") {
        return "active";
    }

    "active"
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

/// Whether filesystem-based installed-agent detection is compiled in.
#[must_use]
pub fn filesystem_detection_available() -> bool {
    cfg!(feature = "agent-detection")
}

#[cfg(feature = "agent-detection")]
fn parse_version_from_evidence(evidence: &[String]) -> Option<String> {
    evidence.iter().find_map(|line| {
        let (prefix, value) = line.split_once(':')?;
        if prefix.trim().eq_ignore_ascii_case("version") {
            let parsed = value.trim();
            if !parsed.is_empty() {
                return Some(parsed.to_string());
            }
        }
        None
    })
}

#[cfg(feature = "agent-detection")]
fn convert_detection_entry(
    entry: crate::agent_detection::InstalledAgentDetectionEntry,
) -> InstalledAgentInventoryEntry {
    let config_path = entry.root_paths.first().cloned();
    let version = parse_version_from_evidence(&entry.evidence);
    InstalledAgentInventoryEntry {
        slug: entry.slug,
        detected: entry.detected,
        evidence: entry.evidence,
        root_paths: entry.root_paths,
        config_path,
        binary_path: None,
        version,
    }
}

#[cfg(feature = "agent-detection")]
fn detect_installed_inventory() -> Result<Vec<InstalledAgentInventoryEntry>, String> {
    let report = crate::agent_detection::detect_installed_agents(
        &crate::agent_detection::AgentDetectOptions {
            only_connectors: None,
            include_undetected: true,
            root_overrides: Vec::new(),
        },
    )
    .map_err(|err| err.to_string())?;

    Ok(report
        .installed_agents
        .into_iter()
        .map(convert_detection_entry)
        .collect())
}

/// Return installed-agent inventory from cache when available.
///
/// When the cache is empty, a fresh filesystem probe is performed and stored.
#[cfg(feature = "agent-detection")]
pub fn installed_inventory_cached() -> Result<Vec<InstalledAgentInventoryEntry>, String> {
    let mut cache = INSTALLED_INVENTORY_CACHE
        .lock()
        .map_err(|_| "installed agent inventory cache is unavailable".to_string())?;
    if let Some(entries) = cache.as_ref() {
        return Ok(entries.clone());
    }
    let entries = detect_installed_inventory()?;
    *cache = Some(entries.clone());
    Ok(entries)
}

/// Force-refresh installed-agent inventory from filesystem probes.
#[cfg(feature = "agent-detection")]
pub fn installed_inventory_refresh() -> Result<Vec<InstalledAgentInventoryEntry>, String> {
    let entries = detect_installed_inventory()?;
    let mut cache = INSTALLED_INVENTORY_CACHE
        .lock()
        .map_err(|_| "installed agent inventory cache is unavailable".to_string())?;
    *cache = Some(entries.clone());
    Ok(entries)
}

/// Return installed-agent inventory when agent detection is disabled.
#[cfg(not(feature = "agent-detection"))]
pub fn installed_inventory_cached() -> Result<Vec<InstalledAgentInventoryEntry>, String> {
    Err("filesystem agent detection is not enabled in this build".to_string())
}

/// Refresh installed-agent inventory when agent detection is disabled.
#[cfg(not(feature = "agent-detection"))]
pub fn installed_inventory_refresh() -> Result<Vec<InstalledAgentInventoryEntry>, String> {
    Err("filesystem agent detection is not enabled in this build".to_string())
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

    #[test]
    fn detect_agent_from_title_ambiguous_prefers_first_match() {
        assert_eq!(
            detect_agent_from_title("claude and codex session"),
            Some(AgentType::ClaudeCode)
        );
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
        assert_eq!(detect_agent_from_process("cursor"), None);
    }

    #[test]
    fn detect_agent_from_process_ambiguous_prefers_first_provider_match() {
        assert_eq!(
            detect_agent_from_process("claude-codex-wrapper"),
            Some(AgentType::ClaudeCode)
        );
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

    // ====================================================================
    // detect_agent_from_title edge cases
    // ====================================================================

    #[test]
    fn detect_title_empty() {
        assert_eq!(detect_agent_from_title(""), None);
    }

    #[test]
    fn detect_title_case_insensitive_claude() {
        assert_eq!(
            detect_agent_from_title("CLAUDE-CODE"),
            Some(AgentType::ClaudeCode)
        );
        assert_eq!(
            detect_agent_from_title("Claude Code"),
            Some(AgentType::ClaudeCode)
        );
        assert_eq!(
            detect_agent_from_title("cLaUdE"),
            Some(AgentType::ClaudeCode)
        );
    }

    #[test]
    fn detect_title_case_insensitive_codex() {
        assert_eq!(
            detect_agent_from_title("CODEX --model o4-mini"),
            Some(AgentType::Codex)
        );
        assert_eq!(
            detect_agent_from_title("OpenAI CLI"),
            Some(AgentType::Codex)
        );
    }

    #[test]
    fn detect_title_case_insensitive_gemini() {
        assert_eq!(detect_agent_from_title("GEMINI"), Some(AgentType::Gemini));
        assert_eq!(
            detect_agent_from_title("Gemini Pro"),
            Some(AgentType::Gemini)
        );
    }

    #[test]
    fn detect_title_claude_substring() {
        // "claude" keyword is contained in longer string
        assert_eq!(
            detect_agent_from_title("running claude-code in tmux"),
            Some(AgentType::ClaudeCode)
        );
    }

    #[test]
    fn detect_title_no_match_similar_words() {
        assert_eq!(detect_agent_from_title("cloudy weather app"), None);
        assert_eq!(detect_agent_from_title("codec"), None);
        assert_eq!(detect_agent_from_title("gem"), None);
    }

    #[test]
    fn detect_title_whitespace_only() {
        assert_eq!(detect_agent_from_title("   "), None);
    }

    #[test]
    fn detect_title_with_path_prefix() {
        assert_eq!(
            detect_agent_from_title("/usr/local/bin/claude-code"),
            Some(AgentType::ClaudeCode)
        );
    }

    // ====================================================================
    // detect_agent_from_process edge cases
    // ====================================================================

    #[test]
    fn detect_process_empty() {
        assert_eq!(detect_agent_from_process(""), None);
    }

    #[test]
    fn detect_process_case_insensitive() {
        assert_eq!(
            detect_agent_from_process("Claude-Code"),
            Some(AgentType::ClaudeCode)
        );
        assert_eq!(detect_agent_from_process("CODEX"), Some(AgentType::Codex));
        assert_eq!(
            detect_agent_from_process("GEMINI-CLI"),
            Some(AgentType::Gemini)
        );
    }

    #[test]
    fn detect_process_no_match() {
        assert_eq!(detect_agent_from_process("vim"), None);
        assert_eq!(detect_agent_from_process("zsh"), None);
        assert_eq!(detect_agent_from_process("python3"), None);
    }

    #[test]
    fn detect_process_with_path() {
        assert_eq!(
            detect_agent_from_process("/home/user/.local/bin/claude"),
            Some(AgentType::ClaudeCode)
        );
    }

    // ====================================================================
    // infer_state_from_rule comprehensive branch coverage
    // ====================================================================

    #[test]
    fn infer_state_session_start() {
        assert_eq!(
            infer_state_from_rule("core.claude_code:session.start"),
            "starting"
        );
    }

    #[test]
    fn infer_state_compaction() {
        assert_eq!(infer_state_from_rule("core.codex:compaction"), "working");
    }

    #[test]
    fn infer_state_rate_limited_variants() {
        assert_eq!(
            infer_state_from_rule("core.codex:rate_limited"),
            "rate_limited"
        );
        assert_eq!(
            infer_state_from_rule("core.codex:usage_reached"),
            "rate_limited"
        );
        assert_eq!(
            infer_state_from_rule("core.gemini:quota_exceeded"),
            "rate_limited"
        );
    }

    #[test]
    fn infer_state_waiting_approval_variants() {
        assert_eq!(
            infer_state_from_rule("core.claude_code:approval_needed"),
            "waiting_approval"
        );
        assert_eq!(
            infer_state_from_rule("core.claude_code:approval_required"),
            "waiting_approval"
        );
    }

    #[test]
    fn infer_state_idle_variants() {
        assert_eq!(infer_state_from_rule("core.codex:session.end"), "idle");
        assert_eq!(
            infer_state_from_rule("core.claude_code:token_usage"),
            "idle"
        );
    }

    #[test]
    fn infer_state_auth_error_variants() {
        assert_eq!(
            infer_state_from_rule("core.codex:auth.api_key_error"),
            "auth_error"
        );
        assert_eq!(
            infer_state_from_rule("core.gemini:auth.login_required"),
            "auth_error"
        );
    }

    #[test]
    fn infer_state_usage_warning_prefix() {
        assert_eq!(
            infer_state_from_rule("core.codex:usage.warning.90pct"),
            "active"
        );
    }

    #[test]
    fn infer_state_no_colon_defaults_active() {
        assert_eq!(infer_state_from_rule("no_colon_rule_id"), "active");
    }

    #[test]
    fn infer_state_empty_string() {
        assert_eq!(infer_state_from_rule(""), "active");
    }

    #[test]
    fn infer_state_multiple_colons_uses_last() {
        // Legacy colon-delimited IDs must still be recognized.
        assert_eq!(infer_state_from_rule("a:b:c:banner"), "starting");
    }

    #[test]
    fn infer_state_dot_delimited_rule_ids() {
        assert_eq!(infer_state_from_rule("codex.usage_reached"), "rate_limited");
        assert_eq!(
            infer_state_from_rule("claude_code.approval_needed"),
            "waiting_approval"
        );
        assert_eq!(
            infer_state_from_rule("claude_code.compaction.complete"),
            "idle"
        );
        assert_eq!(infer_state_from_rule("claude_code.session.end"), "idle");
        assert_eq!(infer_state_from_rule("gemini.session.summary"), "idle");
        assert_eq!(infer_state_from_rule("codex.session.token_usage"), "idle");
    }

    // ====================================================================
    // extract_session_id edge cases
    // ====================================================================

    #[test]
    fn extract_session_id_from_session_key() {
        let data = serde_json::json!({"session": "s-789"});
        assert_eq!(extract_session_id(&data), Some("s-789".to_string()));
    }

    #[test]
    fn extract_session_id_priority_order() {
        // "session_id" has priority over "resume_session_id" and "session"
        let data = serde_json::json!({
            "session_id": "first",
            "resume_session_id": "second",
            "session": "third"
        });
        assert_eq!(extract_session_id(&data), Some("first".to_string()));
    }

    #[test]
    fn extract_session_id_falls_to_resume() {
        let data = serde_json::json!({"resume_session_id": "resume-x"});
        assert_eq!(extract_session_id(&data), Some("resume-x".to_string()));
    }

    #[test]
    fn extract_session_id_null_value() {
        let data = serde_json::json!({"session_id": null});
        assert_eq!(extract_session_id(&data), None);
    }

    #[test]
    fn extract_session_id_numeric_value() {
        // as_str() returns None for numbers
        let data = serde_json::json!({"session_id": 42});
        assert_eq!(extract_session_id(&data), None);
    }

    #[test]
    fn extract_session_id_empty_object() {
        let data = serde_json::json!({});
        assert_eq!(extract_session_id(&data), None);
    }

    #[test]
    fn extract_session_id_array_value() {
        let data = serde_json::json!({"session_id": ["a", "b"]});
        assert_eq!(extract_session_id(&data), None);
    }

    // ====================================================================
    // DetectionSource serde tests
    // ====================================================================

    #[test]
    fn detection_source_serde_roundtrip() {
        for src in [
            DetectionSource::PatternEngine,
            DetectionSource::PaneTitle,
            DetectionSource::ProcessName,
        ] {
            let json = serde_json::to_string(&src).unwrap();
            let back: DetectionSource = serde_json::from_str(&json).unwrap();
            assert_eq!(src, back);
        }
    }

    #[test]
    fn detection_source_serde_snake_case() {
        assert_eq!(
            serde_json::to_string(&DetectionSource::PatternEngine).unwrap(),
            "\"pattern_engine\""
        );
        assert_eq!(
            serde_json::to_string(&DetectionSource::PaneTitle).unwrap(),
            "\"pane_title\""
        );
        assert_eq!(
            serde_json::to_string(&DetectionSource::ProcessName).unwrap(),
            "\"process_name\""
        );
    }

    #[test]
    fn detection_source_debug() {
        let dbg = format!("{:?}", DetectionSource::PatternEngine);
        assert!(dbg.contains("PatternEngine"));
    }

    #[test]
    fn detection_source_copy() {
        let s = DetectionSource::PaneTitle;
        let s2 = s;
        assert_eq!(s, s2);
    }

    // ====================================================================
    // AgentCorrelator additional behavior tests
    // ====================================================================

    #[test]
    fn correlator_default_is_new() {
        let c = AgentCorrelator::default();
        assert_eq!(c.tracked_pane_count(), 0);
    }

    #[test]
    fn correlator_debug() {
        let c = AgentCorrelator::new();
        let dbg = format!("{c:?}");
        assert!(dbg.contains("AgentCorrelator"));
    }

    #[test]
    fn remove_nonexistent_pane_is_noop() {
        let mut c = AgentCorrelator::new();
        c.remove_pane(999); // should not panic
        assert_eq!(c.tracked_pane_count(), 0);
    }

    #[test]
    fn multiple_panes_tracked_independently() {
        let mut c = AgentCorrelator::new();
        c.ingest_detections(
            1,
            &[make_detection(
                "core.claude_code:banner",
                AgentType::ClaudeCode,
            )],
        );
        c.ingest_detections(
            2,
            &[make_detection("core.codex:tool_use", AgentType::Codex)],
        );
        c.ingest_detections(
            3,
            &[make_detection(
                "core.gemini:rate_limited",
                AgentType::Gemini,
            )],
        );

        assert_eq!(c.tracked_pane_count(), 3);
        assert_eq!(c.get_metadata(1).unwrap().agent_type, "claude_code");
        assert_eq!(
            c.get_metadata(1).unwrap().state.as_deref(),
            Some("starting")
        );
        assert_eq!(c.get_metadata(2).unwrap().agent_type, "codex");
        assert_eq!(c.get_metadata(2).unwrap().state.as_deref(), Some("working"));
        assert_eq!(c.get_metadata(3).unwrap().agent_type, "gemini");
        assert_eq!(
            c.get_metadata(3).unwrap().state.as_deref(),
            Some("rate_limited")
        );
    }

    #[test]
    fn ingest_empty_detections_is_noop() {
        let mut c = AgentCorrelator::new();
        c.ingest_detections(1, &[]);
        assert_eq!(c.tracked_pane_count(), 0);
    }

    #[test]
    fn ingest_multiple_detections_uses_last() {
        let mut c = AgentCorrelator::new();
        c.ingest_detections(
            1,
            &[
                make_detection("core.claude_code:banner", AgentType::ClaudeCode),
                make_detection("core.claude_code:tool_use", AgentType::ClaudeCode),
                make_detection("core.claude_code:cost_summary", AgentType::ClaudeCode),
            ],
        );
        // Last detection wins for state
        assert_eq!(c.get_metadata(1).unwrap().state.as_deref(), Some("idle"));
    }

    #[test]
    fn session_id_preserved_across_updates() {
        let mut c = AgentCorrelator::new();
        c.ingest_detections(
            1,
            &[make_detection_with_session(
                "core.codex:banner",
                AgentType::Codex,
                "sess-original",
            )],
        );
        // Second detection without session_id should not clear it
        c.ingest_detections(
            1,
            &[make_detection("core.codex:tool_use", AgentType::Codex)],
        );
        // session_id comes from extract_session_id which returns None for empty extracted
        // But the code only updates session_id when Some is returned
        // So the original should be preserved
        let meta = c.get_metadata(1).unwrap();
        assert_eq!(meta.session_id.as_deref(), Some("sess-original"));
    }

    #[test]
    fn session_id_updated_when_new_provided() {
        let mut c = AgentCorrelator::new();
        c.ingest_detections(
            1,
            &[make_detection_with_session(
                "core.codex:banner",
                AgentType::Codex,
                "sess-1",
            )],
        );
        c.ingest_detections(
            1,
            &[make_detection_with_session(
                "core.codex:tool_use",
                AgentType::Codex,
                "sess-2",
            )],
        );
        assert_eq!(
            c.get_metadata(1).unwrap().session_id.as_deref(),
            Some("sess-2")
        );
    }

    #[test]
    fn agent_type_updated_on_new_detection() {
        let mut c = AgentCorrelator::new();
        c.ingest_detections(1, &[make_detection("core.codex:banner", AgentType::Codex)]);
        assert_eq!(c.get_metadata(1).unwrap().agent_type, "codex");

        // Different agent type on same pane
        c.ingest_detections(
            1,
            &[make_detection(
                "core.claude_code:banner",
                AgentType::ClaudeCode,
            )],
        );
        assert_eq!(c.get_metadata(1).unwrap().agent_type, "claude_code");
    }

    #[test]
    fn remove_pane_then_redetect() {
        let mut c = AgentCorrelator::new();
        c.ingest_detections(1, &[make_detection("core.codex:banner", AgentType::Codex)]);
        c.remove_pane(1);
        assert!(c.get_metadata(1).is_none());

        // Re-add
        c.ingest_detections(
            1,
            &[make_detection("core.gemini:banner", AgentType::Gemini)],
        );
        assert_eq!(c.get_metadata(1).unwrap().agent_type, "gemini");
    }

    #[test]
    fn metadata_agent_type_prefers_legacy_strings_for_supported_providers() {
        assert_eq!(
            metadata_agent_type_for_provider(&AgentProvider::Claude),
            "claude_code"
        );
        assert_eq!(
            metadata_agent_type_for_provider(&AgentProvider::Codex),
            "codex"
        );
    }

    #[test]
    fn metadata_agent_type_uses_canonical_name_for_non_pattern_provider() {
        assert_eq!(
            metadata_agent_type_for_provider(&AgentProvider::Cursor),
            "cursor"
        );
        assert_eq!(
            metadata_agent_type_for_provider(&AgentProvider::Unknown("x-new".to_string())),
            "x-new"
        );
    }
}
