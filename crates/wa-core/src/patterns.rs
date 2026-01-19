//! Pattern detection engine
//!
//! Provides fast, reliable detection of agent state transitions.

use std::collections::{HashMap, HashSet};

use aho_corasick::AhoCorasick;
use fancy_regex::Regex;
use memchr::memchr;
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::error::PatternError;

/// Agent types we support
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentType {
    /// Codex CLI (OpenAI)
    Codex,
    /// Claude Code (Anthropic)
    ClaudeCode,
    /// Gemini CLI (Google)
    Gemini,
    /// WezTerm multiplexer events
    Wezterm,
    /// Unknown agent
    Unknown,
}

impl std::fmt::Display for AgentType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Codex => write!(f, "codex"),
            Self::ClaudeCode => write!(f, "claude_code"),
            Self::Gemini => write!(f, "gemini"),
            Self::Wezterm => write!(f, "wezterm"),
            Self::Unknown => write!(f, "unknown"),
        }
    }
}

/// Detection severity levels
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Informational
    Info,
    /// Warning - attention needed
    Warning,
    /// Critical - immediate action needed
    Critical,
}

/// A detected pattern match
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Detection {
    /// Stable rule identifier (e.g., "core.codex:usage_reached")
    pub rule_id: String,
    /// Agent type this detection applies to
    pub agent_type: AgentType,
    /// Type of event detected
    pub event_type: String,
    /// Severity level
    pub severity: Severity,
    /// Confidence score 0.0-1.0
    pub confidence: f64,
    /// Extracted structured data
    pub extracted: serde_json::Value,
    /// Original matched text
    pub matched_text: String,
}

/// Allowed rule ID prefixes for stable naming
const ALLOWED_RULE_PREFIXES: [&str; 4] = ["codex.", "claude_code.", "gemini.", "wezterm."];

/// Rule definition for pattern detection
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleDef {
    /// Stable rule identifier (e.g., "codex.usage_limit")
    pub id: String,
    /// Agent type this rule applies to
    pub agent_type: AgentType,
    /// Event type emitted on match
    pub event_type: String,
    /// Severity level
    pub severity: Severity,
    /// Literal anchors for quick-reject and Aho-Corasick
    pub anchors: Vec<String>,
    /// Optional extraction regex (named captures preferred)
    pub regex: Option<String>,
    /// Human-readable description
    pub description: String,
    /// Suggested remediation text (optional)
    pub remediation: Option<String>,
    /// Suggested workflow name (optional)
    pub workflow: Option<String>,
}

impl RuleDef {
    fn validate(&self) -> Result<()> {
        if self.id.trim().is_empty() {
            return Err(PatternError::InvalidRule("rule id cannot be empty".to_string()).into());
        }

        if !ALLOWED_RULE_PREFIXES
            .iter()
            .any(|prefix| self.id.starts_with(prefix))
        {
            return Err(PatternError::InvalidRule(format!(
                "rule id '{}' must start with one of: {}",
                self.id,
                ALLOWED_RULE_PREFIXES.join(", ")
            ))
            .into());
        }

        if self.anchors.is_empty() || self.anchors.iter().any(|a| a.trim().is_empty()) {
            return Err(PatternError::InvalidRule(format!(
                "rule id '{}' must include at least one non-empty anchor",
                self.id
            ))
            .into());
        }

        if let Some(ref regex) = self.regex {
            Regex::new(regex).map_err(|e| {
                PatternError::InvalidRegex(format!("rule id '{}' has invalid regex: {e}", self.id))
            })?;
        }

        Ok(())
    }
}

/// Pattern pack containing a set of rules
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatternPack {
    /// Pack name (e.g., "builtin:core")
    pub name: String,
    /// Pack version
    pub version: String,
    /// Rules in this pack
    pub rules: Vec<RuleDef>,
}

impl PatternPack {
    /// Create a new pattern pack
    #[must_use]
    pub fn new(name: impl Into<String>, version: impl Into<String>, rules: Vec<RuleDef>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            rules,
        }
    }

    fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            return Err(PatternError::InvalidRule("pack name cannot be empty".to_string()).into());
        }
        if self.version.trim().is_empty() {
            return Err(
                PatternError::InvalidRule("pack version cannot be empty".to_string()).into(),
            );
        }

        let mut seen = HashSet::new();
        for rule in &self.rules {
            rule.validate()?;
            if !seen.insert(rule.id.as_str()) {
                return Err(PatternError::InvalidRule(format!(
                    "pack '{}' contains duplicate rule id '{}'",
                    self.name, rule.id
                ))
                .into());
            }
        }

        Ok(())
    }
}

/// Loaded and merged pattern packs with override semantics
pub struct PatternLibrary {
    packs: Vec<PatternPack>,
    merged_rules: Vec<RuleDef>,
}

impl PatternLibrary {
    /// Build a new library from packs (later packs override earlier packs by rule id)
    pub fn new(packs: Vec<PatternPack>) -> Result<Self> {
        for pack in &packs {
            pack.validate()?;
        }

        let merged_rules = merge_rules(&packs);

        Ok(Self {
            packs,
            merged_rules,
        })
    }

    /// Create an empty library
    #[must_use]
    pub fn empty() -> Self {
        Self {
            packs: Vec::new(),
            merged_rules: Vec::new(),
        }
    }

    /// List all packs in load order
    #[must_use]
    pub fn packs(&self) -> &[PatternPack] {
        &self.packs
    }

    /// List merged rules in deterministic order
    #[must_use]
    pub fn rules(&self) -> &[RuleDef] {
        &self.merged_rules
    }
}

#[derive(Debug, Clone)]
struct CompiledRule {
    def: RuleDef,
    regex: Option<Regex>,
}

struct EngineIndex {
    compiled_rules: Vec<CompiledRule>,
    anchor_list: Vec<String>,
    anchor_to_rules: HashMap<String, Vec<usize>>,
    anchor_matcher: Option<AhoCorasick>,
    quick_bytes: Vec<u8>,
}

fn build_engine_index(rules: &[RuleDef]) -> Result<EngineIndex> {
    let mut compiled_rules = Vec::with_capacity(rules.len());
    let mut anchor_to_rules: HashMap<String, Vec<usize>> = HashMap::new();
    let mut anchor_list: Vec<String> = Vec::new();
    let mut anchor_set: HashSet<String> = HashSet::new();
    let mut quick_byte_set: HashSet<u8> = HashSet::new();

    for (idx, rule) in rules.iter().enumerate() {
        let regex = match rule.regex.as_ref() {
            Some(raw) => Some(Regex::new(raw).map_err(|e| {
                PatternError::InvalidRegex(format!("rule id '{}' has invalid regex: {e}", rule.id))
            })?),
            None => None,
        };

        compiled_rules.push(CompiledRule {
            def: rule.clone(),
            regex,
        });

        for anchor in &rule.anchors {
            anchor_to_rules.entry(anchor.clone()).or_default().push(idx);
            if anchor_set.insert(anchor.clone()) {
                anchor_list.push(anchor.clone());
            }
            if let Some(&byte) = anchor.as_bytes().first() {
                quick_byte_set.insert(byte);
            }
        }
    }

    let anchor_matcher = if anchor_list.is_empty() {
        None
    } else {
        Some(
            AhoCorasick::new(anchor_list.iter().map(String::as_str)).map_err(|e| {
                PatternError::InvalidRule(format!("failed to build anchor matcher: {e}"))
            })?,
        )
    };

    let mut quick_bytes: Vec<u8> = quick_byte_set.into_iter().collect();
    quick_bytes.sort_unstable();

    Ok(EngineIndex {
        compiled_rules,
        anchor_list,
        anchor_to_rules,
        anchor_matcher,
        quick_bytes,
    })
}

fn merge_rules(packs: &[PatternPack]) -> Vec<RuleDef> {
    let mut merged: HashMap<String, RuleDef> = HashMap::new();

    for pack in packs {
        for rule in &pack.rules {
            merged.insert(rule.id.clone(), rule.clone());
        }
    }

    let mut rules: Vec<RuleDef> = merged.into_values().collect();
    rules.sort_by(|a, b| a.id.cmp(&b.id));
    rules
}

fn builtin_packs() -> Vec<PatternPack> {
    vec![
        builtin_codex_pack(),
        builtin_claude_code_pack(),
        builtin_gemini_pack(),
        builtin_wezterm_pack(),
    ]
}

/// Builtin Codex pack with rules for OpenAI Codex CLI detection
fn builtin_codex_pack() -> PatternPack {
    PatternPack::new(
        "builtin:codex",
        "0.1.0",
        vec![
            // Usage warnings at different thresholds
            RuleDef {
                id: "codex.usage.warning_25".to_string(),
                agent_type: AgentType::Codex,
                event_type: "usage.warning".to_string(),
                severity: Severity::Info,
                anchors: vec!["less than 25%".to_string()],
                regex: Some(r"(?P<remaining>\d+)% of your (?P<limit_hours>\d+)h limit".to_string()),
                description: "Codex usage below 25% remaining".to_string(),
                remediation: None,
                workflow: None,
            },
            RuleDef {
                id: "codex.usage.warning_10".to_string(),
                agent_type: AgentType::Codex,
                event_type: "usage.warning".to_string(),
                severity: Severity::Warning,
                anchors: vec!["less than 10%".to_string()],
                regex: Some(r"(?P<remaining>\d+)% of your (?P<limit_hours>\d+)h limit".to_string()),
                description: "Codex usage below 10% remaining".to_string(),
                remediation: Some("Consider pausing work soon".to_string()),
                workflow: None,
            },
            RuleDef {
                id: "codex.usage.warning_5".to_string(),
                agent_type: AgentType::Codex,
                event_type: "usage.warning".to_string(),
                severity: Severity::Warning,
                anchors: vec!["less than 5%".to_string()],
                regex: Some(r"(?P<remaining>\d+)% of your (?P<limit_hours>\d+)h limit".to_string()),
                description: "Codex usage below 5% remaining - critical threshold".to_string(),
                remediation: Some("Save work and prepare for limit".to_string()),
                workflow: Some("handle_usage_warning".to_string()),
            },
            // Usage limit reached
            RuleDef {
                id: "codex.usage.reached".to_string(),
                agent_type: AgentType::Codex,
                event_type: "usage.reached".to_string(),
                severity: Severity::Critical,
                anchors: vec!["You've hit your usage limit".to_string()],
                regex: Some(r"try again at (?P<reset_time>[^.]+)".to_string()),
                description: "Codex usage limit reached".to_string(),
                remediation: Some("Wait for reset or switch account".to_string()),
                workflow: Some("handle_usage_limits".to_string()),
            },
            // Session token usage summary
            RuleDef {
                id: "codex.session.token_usage".to_string(),
                agent_type: AgentType::Codex,
                event_type: "session.summary".to_string(),
                severity: Severity::Info,
                anchors: vec!["Token usage:".to_string()],
                regex: Some(
                    r"total=(?P<total>[\d,]+)\s+input=(?P<input>[\d,]+)\s+\(\+\s*(?P<cached>[\d,]+)\s+cached\)\s+output=(?P<output>[\d,]+)(?:\s+\(reasoning\s+(?P<reasoning>[\d,]+)\))?".to_string()
                ),
                description: "Codex session token usage summary".to_string(),
                remediation: None,
                workflow: None,
            },
            // Resume session hint
            RuleDef {
                id: "codex.session.resume_hint".to_string(),
                agent_type: AgentType::Codex,
                event_type: "session.resume_hint".to_string(),
                severity: Severity::Info,
                anchors: vec!["codex resume".to_string()],
                regex: Some(
                    r"codex resume (?P<session_id>[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})".to_string()
                ),
                description: "Codex session resume hint with session ID".to_string(),
                remediation: None,
                workflow: None,
            },
            // Device auth code prompt
            RuleDef {
                id: "codex.auth.device_code".to_string(),
                agent_type: AgentType::Codex,
                event_type: "auth.device_code".to_string(),
                severity: Severity::Info,
                anchors: vec!["Enter this one-time code".to_string()],
                regex: Some(r"(?P<code>[A-Z0-9]{4}-[A-Z0-9]{5})".to_string()),
                description: "Codex device authentication code prompt".to_string(),
                remediation: Some("User needs to enter the code in browser".to_string()),
                workflow: None,
            },
        ],
    )
}

/// Builtin Claude Code pack with rules for Anthropic Claude Code detection
fn builtin_claude_code_pack() -> PatternPack {
    PatternPack::new(
        "builtin:claude_code",
        "0.1.0",
        vec![
            // Context compaction
            RuleDef {
                id: "claude_code.context.compaction".to_string(),
                agent_type: AgentType::ClaudeCode,
                event_type: "context.compaction".to_string(),
                severity: Severity::Warning,
                anchors: vec![
                    "Auto-compact".to_string(),
                    "context compacted".to_string(),
                    "summarizing conversation".to_string(),
                ],
                regex: Some(
                    r"(?:compacted|summarized)\s+(?P<tokens_before>[\d,]+)\s+tokens?\s+to\s+(?P<tokens_after>[\d,]+)".to_string()
                ),
                description: "Claude Code context compaction event".to_string(),
                remediation: Some("Context was reduced - some history may be lost".to_string()),
                workflow: Some("handle_compaction".to_string()),
            },
            // Session cost summary
            RuleDef {
                id: "claude_code.session.cost_summary".to_string(),
                agent_type: AgentType::ClaudeCode,
                event_type: "session.summary".to_string(),
                severity: Severity::Info,
                anchors: vec!["Total cost:".to_string(), "Session cost:".to_string()],
                regex: Some(r"(?:Total|Session)\s+cost:\s*\$(?P<cost>[\d.]+)".to_string()),
                description: "Claude Code session cost summary".to_string(),
                remediation: None,
                workflow: None,
            },
            // API key error
            RuleDef {
                id: "claude_code.auth.api_key_error".to_string(),
                agent_type: AgentType::ClaudeCode,
                event_type: "auth.error".to_string(),
                severity: Severity::Critical,
                anchors: vec![
                    "ANTHROPIC_API_KEY".to_string(),
                    "API key".to_string(),
                    "invalid api key".to_string(),
                ],
                regex: None,
                description: "Claude Code API key authentication error".to_string(),
                remediation: Some("Check ANTHROPIC_API_KEY environment variable".to_string()),
                workflow: None,
            },
            // Model selection
            RuleDef {
                id: "claude_code.model.selected".to_string(),
                agent_type: AgentType::ClaudeCode,
                event_type: "session.model".to_string(),
                severity: Severity::Info,
                anchors: vec!["claude-".to_string(), "model:".to_string()],
                regex: Some(r"(?:model|Model):\s*(?P<model>claude-[^\s,]+)".to_string()),
                description: "Claude Code model selection".to_string(),
                remediation: None,
                workflow: None,
            },
        ],
    )
}

/// Builtin Gemini pack with rules for Google Gemini CLI detection
fn builtin_gemini_pack() -> PatternPack {
    PatternPack::new(
        "builtin:gemini",
        "0.1.0",
        vec![
            // Usage limit reached
            RuleDef {
                id: "gemini.usage.reached".to_string(),
                agent_type: AgentType::Gemini,
                event_type: "usage.reached".to_string(),
                severity: Severity::Critical,
                anchors: vec!["Usage limit reached for all Pro models".to_string()],
                regex: None,
                description: "Gemini usage limit reached".to_string(),
                remediation: Some("Wait for limit reset or switch model".to_string()),
                workflow: Some("handle_usage_limits".to_string()),
            },
            // Session summary
            RuleDef {
                id: "gemini.session.summary".to_string(),
                agent_type: AgentType::Gemini,
                event_type: "session.summary".to_string(),
                severity: Severity::Info,
                anchors: vec!["Interaction Summary".to_string()],
                regex: Some(
                    r"Session ID:\s*(?P<session_id>[0-9a-f-]+).*?Tool Calls:\s*(?P<tool_calls>\d+)"
                        .to_string(),
                ),
                description: "Gemini session summary with statistics".to_string(),
                remediation: None,
                workflow: None,
            },
            // Model indicator
            RuleDef {
                id: "gemini.model.used".to_string(),
                agent_type: AgentType::Gemini,
                event_type: "session.model".to_string(),
                severity: Severity::Info,
                anchors: vec!["Responding with gemini-".to_string()],
                regex: Some(r"Responding with (?P<model>gemini-[^\s]+)".to_string()),
                description: "Gemini model being used".to_string(),
                remediation: None,
                workflow: None,
            },
        ],
    )
}

/// Builtin WezTerm pack with rules for WezTerm multiplexer events
fn builtin_wezterm_pack() -> PatternPack {
    PatternPack::new(
        "builtin:wezterm",
        "0.1.0",
        vec![
            // Mux server connection lost
            RuleDef {
                id: "wezterm.mux.connection_lost".to_string(),
                agent_type: AgentType::Wezterm,
                event_type: "mux.error".to_string(),
                severity: Severity::Critical,
                anchors: vec![
                    "mux server".to_string(),
                    "connection lost".to_string(),
                    "disconnected".to_string(),
                ],
                regex: None,
                description: "WezTerm mux server connection lost".to_string(),
                remediation: Some("Check WezTerm mux server status".to_string()),
                workflow: None,
            },
            // Pane exited
            RuleDef {
                id: "wezterm.pane.exited".to_string(),
                agent_type: AgentType::Wezterm,
                event_type: "pane.exited".to_string(),
                severity: Severity::Info,
                anchors: vec![
                    "pane exited".to_string(),
                    "shell exited".to_string(),
                    "process exited".to_string(),
                ],
                regex: Some(r"(?:exit(?:ed)?|status)[:\s]+(?P<exit_code>\d+)".to_string()),
                description: "WezTerm pane process exited".to_string(),
                remediation: None,
                workflow: None,
            },
        ],
    )
}

/// Pattern engine for detecting agent state transitions
pub struct PatternEngine {
    /// Merged rule library
    library: PatternLibrary,
    /// Compiled rule cache
    compiled_rules: Vec<CompiledRule>,
    /// Anchor list aligned with Aho-Corasick pattern IDs
    anchor_list: Vec<String>,
    /// Anchor-to-rule index (anchor -> rule indices)
    anchor_to_rules: HashMap<String, Vec<usize>>,
    /// Aho-Corasick matcher for anchors
    anchor_matcher: Option<AhoCorasick>,
    /// Quick-reject byte set (first bytes of anchors)
    quick_bytes: Vec<u8>,
    /// Whether the engine is initialized
    initialized: bool,
}

impl Default for PatternEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl PatternEngine {
    /// Create a new pattern engine with default packs
    #[must_use]
    pub fn new() -> Self {
        let library =
            PatternLibrary::new(builtin_packs()).expect("builtin pattern packs must be valid");
        let index =
            build_engine_index(library.rules()).expect("builtin pattern packs must compile");
        Self {
            library,
            compiled_rules: index.compiled_rules,
            anchor_list: index.anchor_list,
            anchor_to_rules: index.anchor_to_rules,
            anchor_matcher: index.anchor_matcher,
            quick_bytes: index.quick_bytes,
            initialized: true,
        }
    }

    /// Create a new pattern engine from explicit packs
    pub fn with_packs(packs: Vec<PatternPack>) -> Result<Self> {
        let library = PatternLibrary::new(packs)?;
        let index = build_engine_index(library.rules())?;
        Ok(Self {
            library,
            compiled_rules: index.compiled_rules,
            anchor_list: index.anchor_list,
            anchor_to_rules: index.anchor_to_rules,
            anchor_matcher: index.anchor_matcher,
            quick_bytes: index.quick_bytes,
            initialized: true,
        })
    }

    /// Check if the engine is initialized
    #[must_use]
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// Detect patterns in text
    #[must_use]
    pub fn detect(&self, text: &str) -> Vec<Detection> {
        if text.is_empty() {
            return Vec::new();
        }

        if !self.quick_reject(text) {
            return Vec::new();
        }

        let matcher = match self.anchor_matcher.as_ref() {
            Some(matcher) => matcher,
            None => return Vec::new(),
        };

        let mut candidate_rules: HashSet<usize> = HashSet::new();
        let mut matched_anchor_by_rule: HashMap<usize, String> = HashMap::new();

        for matched in matcher.find_iter(text) {
            let anchor = match self.anchor_list.get(matched.pattern().as_usize()) {
                Some(anchor) => anchor,
                None => continue,
            };

            if let Some(rule_indices) = self.anchor_to_rules.get(anchor) {
                for &idx in rule_indices {
                    candidate_rules.insert(idx);
                    matched_anchor_by_rule
                        .entry(idx)
                        .or_insert_with(|| anchor.clone());
                }
            }
        }

        if candidate_rules.is_empty() {
            return Vec::new();
        }

        let mut indices: Vec<usize> = candidate_rules.into_iter().collect();
        indices.sort_unstable();

        let mut detections = Vec::new();
        for idx in indices {
            let compiled = &self.compiled_rules[idx];
            let rule = &compiled.def;
            let fallback_anchor = matched_anchor_by_rule
                .get(&idx)
                .cloned()
                .unwrap_or_default();

            if let Some(regex) = compiled.regex.as_ref() {
                let captures = match regex.captures(text) {
                    Ok(Some(captures)) => captures,
                    _ => continue,
                };

                let mut extracted = serde_json::Map::new();
                for name in regex.capture_names().flatten() {
                    if let Some(value) = captures.name(name) {
                        extracted.insert(
                            name.to_string(),
                            serde_json::Value::String(value.as_str().to_string()),
                        );
                    }
                }

                let matched_text = captures
                    .get(0)
                    .map(|m| m.as_str().to_string())
                    .unwrap_or_else(|| fallback_anchor.clone());

                detections.push(Detection {
                    rule_id: rule.id.clone(),
                    agent_type: rule.agent_type,
                    event_type: rule.event_type.clone(),
                    severity: rule.severity,
                    confidence: 0.95,
                    extracted: serde_json::Value::Object(extracted),
                    matched_text,
                });
            } else {
                detections.push(Detection {
                    rule_id: rule.id.clone(),
                    agent_type: rule.agent_type,
                    event_type: rule.event_type.clone(),
                    severity: rule.severity,
                    confidence: 0.6,
                    extracted: serde_json::Value::Object(serde_json::Map::new()),
                    matched_text: fallback_anchor,
                });
            }
        }

        detections
    }

    /// Quick reject check - returns false if text definitely has no matches
    #[must_use]
    pub fn quick_reject(&self, text: &str) -> bool {
        if text.is_empty() || self.quick_bytes.is_empty() {
            return false;
        }

        let bytes = text.as_bytes();
        self.quick_bytes
            .iter()
            .any(|byte| memchr(*byte, bytes).is_some())
    }

    /// Access the merged rule library
    #[must_use]
    pub fn rules(&self) -> &[RuleDef] {
        self.library.rules()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_can_be_created() {
        let engine = PatternEngine::new();
        assert!(engine.is_initialized());
    }

    #[test]
    fn detect_returns_empty_for_now() {
        let engine = PatternEngine::new();
        let detections = engine.detect("some text");
        assert!(detections.is_empty());
    }

    fn sample_rule(id: &str) -> RuleDef {
        RuleDef {
            id: id.to_string(),
            agent_type: AgentType::Codex,
            event_type: "usage".to_string(),
            severity: Severity::Info,
            anchors: vec!["anchor".to_string()],
            regex: None,
            description: "test rule".to_string(),
            remediation: None,
            workflow: None,
        }
    }

    fn rule_with_anchor(id: &str, anchor: &str, regex: Option<&str>) -> RuleDef {
        RuleDef {
            id: id.to_string(),
            agent_type: AgentType::Codex,
            event_type: "test.event".to_string(),
            severity: Severity::Info,
            anchors: vec![anchor.to_string()],
            regex: regex.map(str::to_string),
            description: "test rule".to_string(),
            remediation: None,
            workflow: None,
        }
    }

    fn engine_with_rules(rules: Vec<RuleDef>) -> PatternEngine {
        let pack = PatternPack::new("pack", "0.1.0", rules);
        PatternEngine::with_packs(vec![pack]).expect("engine should build")
    }

    #[test]
    fn quick_reject_respects_anchor_bytes() {
        let engine = engine_with_rules(vec![rule_with_anchor("codex.quick", "XYZ", None)]);
        assert!(!engine.quick_reject("abc"));
        assert!(engine.quick_reject("look: X-ray"));
    }

    #[test]
    fn detect_matches_anchor_only_rule() {
        let engine = engine_with_rules(vec![rule_with_anchor("codex.anchor", "hello", None)]);
        let detections = engine.detect("say hello to the world");
        assert_eq!(detections.len(), 1);
        assert_eq!(detections[0].rule_id, "codex.anchor");
        assert_eq!(detections[0].matched_text, "hello");
    }

    #[test]
    fn detect_requires_regex_match_for_regex_rules() {
        let engine = engine_with_rules(vec![rule_with_anchor(
            "codex.regex",
            "limit",
            Some(r"limit (?P<value>\d+)"),
        )]);
        let detections = engine.detect("limit xx");
        assert!(detections.is_empty());
    }

    #[test]
    fn detect_extracts_named_captures() {
        let engine = engine_with_rules(vec![rule_with_anchor(
            "codex.regex",
            "limit",
            Some(r"limit (?P<value>\d+)"),
        )]);
        let detections = engine.detect("limit 42");
        assert_eq!(detections.len(), 1);
        assert_eq!(detections[0].rule_id, "codex.regex");
        assert_eq!(
            detections[0]
                .extracted
                .get("value")
                .and_then(|v| v.as_str()),
            Some("42")
        );
    }

    #[test]
    fn rules_are_sorted_deterministically() {
        let pack = PatternPack::new(
            "builtin:core",
            "0.1.0",
            vec![sample_rule("codex.b"), sample_rule("codex.a")],
        );
        let library = PatternLibrary::new(vec![pack]).unwrap();
        let ids: Vec<&str> = library.rules().iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["codex.a", "codex.b"]);
    }

    #[test]
    fn later_packs_override_earlier_rules() {
        let pack_a = PatternPack::new("pack-a", "0.1.0", vec![sample_rule("codex.test")]);
        let mut rule_b = sample_rule("codex.test");
        rule_b.event_type = "compaction".to_string();
        rule_b.severity = Severity::Critical;
        let pack_b = PatternPack::new("pack-b", "0.1.0", vec![rule_b]);

        let library = PatternLibrary::new(vec![pack_a, pack_b]).unwrap();
        let rule = library
            .rules()
            .iter()
            .find(|r| r.id == "codex.test")
            .unwrap();
        assert_eq!(rule.event_type, "compaction");
        assert_eq!(rule.severity, Severity::Critical);
    }

    #[test]
    fn invalid_rule_id_is_rejected() {
        let pack = PatternPack::new("pack-a", "0.1.0", vec![sample_rule("custom.bad")]);
        let result = PatternLibrary::new(vec![pack]);
        assert!(result.is_err());
    }

    #[test]
    fn invalid_regex_is_rejected() {
        let mut rule = sample_rule("codex.bad_regex");
        rule.regex = Some("(".to_string());
        let pack = PatternPack::new("pack-a", "0.1.0", vec![rule]);
        let result = PatternLibrary::new(vec![pack]);
        assert!(result.is_err());
    }

    // ========================================================================
    // Builtin pack tests
    // ========================================================================

    #[test]
    fn builtin_codex_pack_is_valid() {
        let pack = builtin_codex_pack();
        pack.validate().expect("Codex pack should be valid");
        assert!(!pack.rules.is_empty(), "Codex pack should have rules");
    }

    #[test]
    fn builtin_claude_code_pack_is_valid() {
        let pack = builtin_claude_code_pack();
        pack.validate().expect("Claude Code pack should be valid");
        assert!(!pack.rules.is_empty(), "Claude Code pack should have rules");
    }

    #[test]
    fn builtin_gemini_pack_is_valid() {
        let pack = builtin_gemini_pack();
        pack.validate().expect("Gemini pack should be valid");
        assert!(!pack.rules.is_empty(), "Gemini pack should have rules");
    }

    #[test]
    fn builtin_wezterm_pack_is_valid() {
        let pack = builtin_wezterm_pack();
        pack.validate().expect("WezTerm pack should be valid");
        assert!(!pack.rules.is_empty(), "WezTerm pack should have rules");
    }

    #[test]
    fn all_builtin_rules_have_valid_ids() {
        let engine = PatternEngine::new();
        for rule in engine.rules() {
            let valid = ALLOWED_RULE_PREFIXES.iter().any(|p| rule.id.starts_with(p));
            assert!(valid, "Rule '{}' has invalid prefix", rule.id);
        }
    }

    #[test]
    fn all_builtin_rules_have_anchors() {
        let engine = PatternEngine::new();
        for rule in engine.rules() {
            assert!(
                !rule.anchors.is_empty(),
                "Rule '{}' must have at least one anchor",
                rule.id
            );
        }
    }

    #[test]
    fn builtin_rule_enumeration_is_deterministic() {
        let engine1 = PatternEngine::new();
        let engine2 = PatternEngine::new();

        let ids1: Vec<&str> = engine1.rules().iter().map(|r| r.id.as_str()).collect();
        let ids2: Vec<&str> = engine2.rules().iter().map(|r| r.id.as_str()).collect();

        assert_eq!(ids1, ids2, "Rule enumeration should be deterministic");
    }

    #[test]
    fn expected_codex_rules_exist() {
        let engine = PatternEngine::new();
        let ids: Vec<&str> = engine.rules().iter().map(|r| r.id.as_str()).collect();

        assert!(
            ids.contains(&"codex.usage.reached"),
            "Missing codex.usage.reached"
        );
        assert!(
            ids.contains(&"codex.session.token_usage"),
            "Missing codex.session.token_usage"
        );
        assert!(
            ids.contains(&"codex.session.resume_hint"),
            "Missing codex.session.resume_hint"
        );
    }

    #[test]
    fn expected_claude_code_rules_exist() {
        let engine = PatternEngine::new();
        assert!(
            engine
                .rules()
                .iter()
                .map(|r| r.id.as_str())
                .any(|id| id == "claude_code.context.compaction"),
            "Missing claude_code.context.compaction"
        );
    }

    #[test]
    fn expected_gemini_rules_exist() {
        let engine = PatternEngine::new();
        let ids: Vec<&str> = engine.rules().iter().map(|r| r.id.as_str()).collect();

        assert!(
            ids.contains(&"gemini.usage.reached"),
            "Missing gemini.usage.reached"
        );
        assert!(
            ids.contains(&"gemini.session.summary"),
            "Missing gemini.session.summary"
        );
    }

    #[test]
    fn expected_wezterm_rules_exist() {
        let engine = PatternEngine::new();
        assert!(
            engine
                .rules()
                .iter()
                .map(|r| r.id.as_str())
                .any(|id| id == "wezterm.mux.connection_lost"),
            "Missing wezterm.mux.connection_lost"
        );
    }

    // ========================================================================
    // AgentType tests
    // ========================================================================

    #[test]
    fn agent_type_display() {
        assert_eq!(AgentType::Codex.to_string(), "codex");
        assert_eq!(AgentType::ClaudeCode.to_string(), "claude_code");
        assert_eq!(AgentType::Gemini.to_string(), "gemini");
        assert_eq!(AgentType::Wezterm.to_string(), "wezterm");
        assert_eq!(AgentType::Unknown.to_string(), "unknown");
    }

    // ========================================================================
    // Rule validation edge cases
    // ========================================================================

    #[test]
    fn empty_rule_id_is_rejected() {
        let mut rule = sample_rule("codex.test");
        rule.id = String::new();
        let pack = PatternPack::new("pack", "0.1.0", vec![rule]);
        assert!(PatternLibrary::new(vec![pack]).is_err());
    }

    #[test]
    fn empty_anchor_is_rejected() {
        let mut rule = sample_rule("codex.test");
        rule.anchors = vec![String::new()];
        let pack = PatternPack::new("pack", "0.1.0", vec![rule]);
        assert!(PatternLibrary::new(vec![pack]).is_err());
    }

    #[test]
    fn empty_anchors_list_is_rejected() {
        let mut rule = sample_rule("codex.test");
        rule.anchors = Vec::new();
        let pack = PatternPack::new("pack", "0.1.0", vec![rule]);
        assert!(PatternLibrary::new(vec![pack]).is_err());
    }

    #[test]
    fn duplicate_rule_ids_in_same_pack_rejected() {
        let pack = PatternPack::new(
            "pack",
            "0.1.0",
            vec![sample_rule("codex.dup"), sample_rule("codex.dup")],
        );
        assert!(PatternLibrary::new(vec![pack]).is_err());
    }

    #[test]
    fn empty_pack_name_is_rejected() {
        let pack = PatternPack::new("", "0.1.0", vec![sample_rule("codex.test")]);
        assert!(PatternLibrary::new(vec![pack]).is_err());
    }

    #[test]
    fn empty_pack_version_is_rejected() {
        let pack = PatternPack::new("pack", "", vec![sample_rule("codex.test")]);
        assert!(PatternLibrary::new(vec![pack]).is_err());
    }
}
