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
            AhoCorasick::builder()
                .build(anchor_list.iter().map(String::as_str))
                .map_err(|e| {
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
                regex: Some(r"(?P<remaining>\d+)% of your (?P<limit_hours>\d+)h limit remaining".to_string()),
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
                regex: Some(r"(?P<remaining>\d+)% of your (?P<limit_hours>\d+)h limit remaining".to_string()),
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
                regex: Some(r"(?P<remaining>\d+)% of your (?P<limit_hours>\d+)h limit remaining".to_string()),
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
                id: "codex.auth.device_code_prompt".to_string(),
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
            // Context compaction (canonical ID: claude_code.compaction)
            RuleDef {
                id: "claude_code.compaction".to_string(),
                agent_type: AgentType::ClaudeCode,
                event_type: "session.compaction".to_string(),
                severity: Severity::Warning,
                anchors: vec![
                    "Conversation compacted".to_string(),
                    "Auto-compact".to_string(),
                    "context compacted".to_string(),
                ],
                regex: Some(
                    r"(?:compacted|summarized)\s+(?P<tokens_before>[\d,]+)\s+tokens?\s+to\s+(?P<tokens_after>[\d,]+)".to_string()
                ),
                description: "Claude Code context compaction event".to_string(),
                remediation: Some("Context was reduced - some history may be lost".to_string()),
                workflow: Some("handle_compaction".to_string()),
            },
            // Session banner with version and model info
            RuleDef {
                id: "claude_code.banner".to_string(),
                agent_type: AgentType::ClaudeCode,
                event_type: "session.start".to_string(),
                severity: Severity::Info,
                anchors: vec!["Claude Code v".to_string(), "claude-code/".to_string()],
                regex: Some(
                    r"(?:Claude Code v|claude-code/)(?P<version>[\d.]+)(?:.*?model[:\s]+(?P<model>claude-[^\s,]+))?".to_string()
                ),
                description: "Claude Code session start banner".to_string(),
                remediation: None,
                workflow: None,
            },
            // Usage warning (evolving patterns)
            RuleDef {
                id: "claude_code.usage.warning".to_string(),
                agent_type: AgentType::ClaudeCode,
                event_type: "usage.warning".to_string(),
                severity: Severity::Warning,
                anchors: vec![
                    "usage limit".to_string(),
                    "approaching limit".to_string(),
                    "token limit".to_string(),
                ],
                regex: Some(r"(?P<remaining>\d+)%?\s*(?:remaining|left|of limit)".to_string()),
                description: "Claude Code usage warning".to_string(),
                remediation: Some("Consider saving work soon".to_string()),
                workflow: None,
            },
            // Usage limit reached
            RuleDef {
                id: "claude_code.usage.reached".to_string(),
                agent_type: AgentType::ClaudeCode,
                event_type: "usage.reached".to_string(),
                severity: Severity::Critical,
                anchors: vec![
                    "rate limit".to_string(),
                    "limit reached".to_string(),
                    "quota exceeded".to_string(),
                ],
                regex: Some(r"(?:retry|reset|try again).*?(?P<reset_time>\d+\s*(?:seconds?|minutes?|hours?)|[\d:]+\s*(?:AM|PM|UTC))".to_string()),
                description: "Claude Code usage limit reached".to_string(),
                remediation: Some("Wait for limit reset or switch session".to_string()),
                workflow: Some("handle_usage_limits".to_string()),
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
                    r"Session ID:\s*(?P<session_id>[0-9a-fA-F-]+)[\s\S]*?Tool Calls:\s*(?P<tool_calls>\d+)"
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

        let Some(matcher) = self.anchor_matcher.as_ref() else {
            return Vec::new();
        };

        let mut candidate_rules: HashSet<usize> = HashSet::new();
        let mut matched_anchor_by_rule: HashMap<usize, String> = HashMap::new();

        #[cfg(test)]
        {
            let match_count = matcher.find_overlapping_iter(text).count();
            eprintln!("detect: Aho-Corasick found {match_count} matches in text");
        }

        // Use find_overlapping_iter to detect all anchors, including ones that overlap
        // (e.g., "limit reached" and "Usage limit reached for all Pro models")
        for matched in matcher.find_overlapping_iter(text) {
            #[cfg(test)]
            {
                let pattern = matched.pattern().as_usize();
                let span = matched.span();
                eprintln!("detect: matched pattern {pattern} at {span:?}");
            }

            let Some(anchor) = self.anchor_list.get(matched.pattern().as_usize()) else {
                #[cfg(test)]
                {
                    let pattern = matched.pattern().as_usize();
                    eprintln!("detect: pattern {pattern} not found in anchor_list");
                }
                continue;
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
                let Ok(Some(captures)) = regex.captures(text) else {
                    continue;
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
                    .map_or_else(|| fallback_anchor.clone(), |m| m.as_str().to_string());

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
    use serde::Deserialize;
    use std::collections::{HashMap, HashSet};
    use std::fs;
    use std::path::PathBuf;

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
        assert!(
            ids.contains(&"codex.auth.device_code_prompt"),
            "Missing codex.auth.device_code_prompt"
        );
    }

    #[test]
    fn expected_claude_code_rules_exist() {
        let engine = PatternEngine::new();
        let ids: Vec<&str> = engine.rules().iter().map(|r| r.id.as_str()).collect();

        assert!(
            ids.contains(&"claude_code.compaction"),
            "Missing claude_code.compaction"
        );
        assert!(
            ids.contains(&"claude_code.banner"),
            "Missing claude_code.banner"
        );
        assert!(
            ids.contains(&"claude_code.usage.warning"),
            "Missing claude_code.usage.warning"
        );
        assert!(
            ids.contains(&"claude_code.usage.reached"),
            "Missing claude_code.usage.reached"
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

    fn assert_rule_extraction(rule_id: &str, text: &str, expected: &[(&str, &str)]) {
        let engine = PatternEngine::new();
        let detections = engine.detect(text);
        let detection = detections
            .iter()
            .find(|d| d.rule_id == rule_id)
            .unwrap_or_else(|| panic!("Expected detection for rule '{rule_id}'"));

        let map = detection
            .extracted
            .as_object()
            .unwrap_or_else(|| panic!("Expected extracted object for rule '{rule_id}'"));

        assert_eq!(
            map.len(),
            expected.len(),
            "Unexpected extracted keys for rule '{rule_id}'"
        );

        for (key, value) in expected {
            let actual = map
                .get(*key)
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            assert_eq!(
                actual, *value,
                "Extraction mismatch for rule '{rule_id}' key '{key}'"
            );
        }
    }

    #[test]
    fn builtin_regex_rules_extract_expected_fields() {
        let cases = vec![
            (
                "codex.usage.warning_25",
                "Warning: You have less than 25% of your 24h limit remaining.",
                vec![("remaining", "25"), ("limit_hours", "24")],
            ),
            (
                "codex.usage.warning_10",
                "Warning: You have less than 10% of your 8h limit remaining.",
                vec![("remaining", "10"), ("limit_hours", "8")],
            ),
            (
                "codex.usage.warning_5",
                "Warning: You have less than 5% of your 12h limit remaining.",
                vec![("remaining", "5"), ("limit_hours", "12")],
            ),
            (
                "codex.usage.reached",
                "You've hit your usage limit. Please try again at 2026-01-20 12:34 UTC.",
                vec![("reset_time", "2026-01-20 12:34 UTC")],
            ),
            (
                "codex.session.token_usage",
                "Token usage: total=1,234 input=567 (+ 89 cached) output=987 (reasoning 12)",
                vec![
                    ("total", "1,234"),
                    ("input", "567"),
                    ("cached", "89"),
                    ("output", "987"),
                    ("reasoning", "12"),
                ],
            ),
            (
                "codex.session.resume_hint",
                "To resume later, run: codex resume 123e4567-e89b-12d3-a456-426614174000",
                vec![("session_id", "123e4567-e89b-12d3-a456-426614174000")],
            ),
            (
                "codex.auth.device_code_prompt",
                "Enter this one-time code: ABCD-12345",
                vec![("code", "ABCD-12345")],
            ),
            (
                "claude_code.compaction",
                "Auto-compact: context compacted 12,345 tokens to 3,210",
                vec![("tokens_before", "12,345"), ("tokens_after", "3,210")],
            ),
            (
                "claude_code.session.cost_summary",
                "Session cost: $2.50",
                vec![("cost", "2.50")],
            ),
            (
                "claude_code.model.selected",
                "model: claude-3-5-sonnet-20241022",
                vec![("model", "claude-3-5-sonnet-20241022")],
            ),
            (
                "gemini.session.summary",
                "Interaction Summary: Session ID: abcdef12-3456-7890-abcd-ef1234567890 Tool Calls: 7",
                vec![
                    ("session_id", "abcdef12-3456-7890-abcd-ef1234567890"),
                    ("tool_calls", "7"),
                ],
            ),
            (
                "gemini.model.used",
                "Responding with gemini-1.5-pro",
                vec![("model", "gemini-1.5-pro")],
            ),
            (
                "wezterm.pane.exited",
                "pane exited with status 1",
                vec![("exit_code", "1")],
            ),
        ];

        for (rule_id, text, expected) in cases {
            assert_rule_extraction(rule_id, text, &expected);
        }
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

    // ========================================================================
    // Real-world detection tests for builtin rules
    // ========================================================================

    #[test]
    fn detect_codex_usage_reached() {
        let engine = PatternEngine::new();
        let text = "You've hit your usage limit for the 3h window. Please try again at 2:30 PM.";
        let detections = engine.detect(text);
        assert!(!detections.is_empty(), "Should detect usage limit");
        let detection = detections
            .iter()
            .find(|d| d.rule_id == "codex.usage.reached");
        assert!(detection.is_some(), "Should match codex.usage.reached");
        let d = detection.unwrap();
        assert_eq!(d.severity, Severity::Critical);
        assert_eq!(
            d.extracted.get("reset_time").and_then(|v| v.as_str()),
            Some("2:30 PM")
        );
    }

    #[test]
    fn detect_codex_usage_warning_25() {
        let engine = PatternEngine::new();
        let text = "Warning: You have less than 25% of your 3h limit remaining.";
        let detections = engine.detect(text);
        let detection = detections
            .iter()
            .find(|d| d.rule_id == "codex.usage.warning_25");
        assert!(detection.is_some(), "Should match codex.usage.warning_25");
    }

    #[test]
    fn detect_codex_usage_warning_10() {
        let engine = PatternEngine::new();
        let text = "Warning: You have less than 10% of your 3h limit remaining.";
        let detections = engine.detect(text);
        let detection = detections
            .iter()
            .find(|d| d.rule_id == "codex.usage.warning_10");
        assert!(detection.is_some(), "Should match codex.usage.warning_10");
    }

    #[test]
    fn detect_codex_usage_warning_5() {
        let engine = PatternEngine::new();
        let text = "Warning: You have less than 5% of your 3h limit remaining.";
        let detections = engine.detect(text);
        let detection = detections
            .iter()
            .find(|d| d.rule_id == "codex.usage.warning_5");
        assert!(detection.is_some(), "Should match codex.usage.warning_5");
    }

    #[test]
    fn detect_codex_token_usage() {
        let engine = PatternEngine::new();
        let text = "Token usage: total=125,432 input=50,000 (+ 20,000 cached) output=55,432";
        let detections = engine.detect(text);
        let detection = detections
            .iter()
            .find(|d| d.rule_id == "codex.session.token_usage");
        assert!(
            detection.is_some(),
            "Should match codex.session.token_usage"
        );
        let d = detection.unwrap();
        assert_eq!(
            d.extracted.get("total").and_then(|v| v.as_str()),
            Some("125,432")
        );
        assert_eq!(
            d.extracted.get("input").and_then(|v| v.as_str()),
            Some("50,000")
        );
        assert_eq!(
            d.extracted.get("cached").and_then(|v| v.as_str()),
            Some("20,000")
        );
        assert_eq!(
            d.extracted.get("output").and_then(|v| v.as_str()),
            Some("55,432")
        );
    }

    #[test]
    fn detect_codex_token_usage_with_reasoning() {
        let engine = PatternEngine::new();
        let text = "Token usage: total=200,000 input=80,000 (+ 30,000 cached) output=90,000 (reasoning 10,000)";
        let detections = engine.detect(text);
        let detection = detections
            .iter()
            .find(|d| d.rule_id == "codex.session.token_usage");
        assert!(
            detection.is_some(),
            "Should match codex.session.token_usage with reasoning"
        );
        let d = detection.unwrap();
        assert_eq!(
            d.extracted.get("reasoning").and_then(|v| v.as_str()),
            Some("10,000")
        );
    }

    #[test]
    fn detect_codex_resume_hint() {
        let engine = PatternEngine::new();
        let text = "To resume this session, run: codex resume a1b2c3d4-e5f6-7890-abcd-ef1234567890";
        let detections = engine.detect(text);
        let detection = detections
            .iter()
            .find(|d| d.rule_id == "codex.session.resume_hint");
        assert!(
            detection.is_some(),
            "Should match codex.session.resume_hint"
        );
        let d = detection.unwrap();
        assert_eq!(
            d.extracted.get("session_id").and_then(|v| v.as_str()),
            Some("a1b2c3d4-e5f6-7890-abcd-ef1234567890")
        );
    }

    #[test]
    fn detect_codex_device_auth() {
        let engine = PatternEngine::new();
        let text = "Enter this one-time code at https://auth.openai.com: ABCD-12345";
        let detections = engine.detect(text);
        let detection = detections
            .iter()
            .find(|d| d.rule_id == "codex.auth.device_code_prompt");
        assert!(
            detection.is_some(),
            "Should match codex.auth.device_code_prompt"
        );
        let d = detection.unwrap();
        assert_eq!(
            d.extracted.get("code").and_then(|v| v.as_str()),
            Some("ABCD-12345")
        );
    }

    #[test]
    fn detect_claude_code_compaction() {
        let engine = PatternEngine::new();
        let text = "Conversation compacted 150,000 tokens to 25,000 tokens";
        let detections = engine.detect(text);
        let detection = detections
            .iter()
            .find(|d| d.rule_id == "claude_code.compaction");
        assert!(detection.is_some(), "Should match claude_code.compaction");
        let d = detection.unwrap();
        assert_eq!(d.severity, Severity::Warning);
        assert_eq!(
            d.extracted.get("tokens_before").and_then(|v| v.as_str()),
            Some("150,000")
        );
        assert_eq!(
            d.extracted.get("tokens_after").and_then(|v| v.as_str()),
            Some("25,000")
        );
    }

    #[test]
    fn detect_claude_code_compaction_summarized_variant() {
        let engine = PatternEngine::new();
        let text = "context compacted: summarized 100,000 tokens to 15,000 tokens";
        let detections = engine.detect(text);
        let detection = detections
            .iter()
            .find(|d| d.rule_id == "claude_code.compaction");
        assert!(
            detection.is_some(),
            "Should match claude_code.compaction with summarized variant"
        );
    }

    #[test]
    fn detect_claude_code_cost_summary() {
        let engine = PatternEngine::new();
        let text = "Session complete. Total cost: $1.25";
        let detections = engine.detect(text);
        let detection = detections
            .iter()
            .find(|d| d.rule_id == "claude_code.session.cost_summary");
        assert!(
            detection.is_some(),
            "Should match claude_code.session.cost_summary"
        );
        let d = detection.unwrap();
        assert_eq!(
            d.extracted.get("cost").and_then(|v| v.as_str()),
            Some("1.25")
        );
    }

    #[test]
    fn detect_claude_code_api_key_error() {
        let engine = PatternEngine::new();
        let text = "Error: invalid api key - please check your ANTHROPIC_API_KEY";
        let detections = engine.detect(text);
        let detection = detections
            .iter()
            .find(|d| d.rule_id == "claude_code.auth.api_key_error");
        assert!(
            detection.is_some(),
            "Should match claude_code.auth.api_key_error"
        );
        assert_eq!(detection.unwrap().severity, Severity::Critical);
    }

    #[test]
    fn detect_claude_code_banner() {
        let engine = PatternEngine::new();
        let text = "Claude Code v1.2.3 starting session with model: claude-opus-4-5-20251101";
        let detections = engine.detect(text);
        let detection = detections
            .iter()
            .find(|d| d.rule_id == "claude_code.banner");
        assert!(detection.is_some(), "Should match claude_code.banner");
        let d = detection.unwrap();
        assert_eq!(
            d.extracted.get("version").and_then(|v| v.as_str()),
            Some("1.2.3")
        );
    }

    #[test]
    fn detect_gemini_usage_reached() {
        let engine = PatternEngine::new();
        let text = "Usage limit reached for all Pro models. Please wait before continuing.";
        let detections = engine.detect(text);
        let detection = detections
            .iter()
            .find(|d| d.rule_id == "gemini.usage.reached");
        assert!(detection.is_some(), "Should match gemini.usage.reached");
        assert_eq!(detection.unwrap().severity, Severity::Critical);
    }

    #[test]
    fn detect_gemini_session_summary() {
        let engine = PatternEngine::new();
        let text = "Interaction Summary\nSession ID: abc12345-def6-7890-abcd-0123456789ab\nTool Calls: 42\nTokens Used: 10000";
        let detections = engine.detect(text);
        let detection = detections
            .iter()
            .find(|d| d.rule_id == "gemini.session.summary");
        assert!(detection.is_some(), "Should match gemini.session.summary");
        let d = detection.unwrap();
        assert_eq!(
            d.extracted.get("session_id").and_then(|v| v.as_str()),
            Some("abc12345-def6-7890-abcd-0123456789ab")
        );
        assert_eq!(
            d.extracted.get("tool_calls").and_then(|v| v.as_str()),
            Some("42")
        );
    }

    #[test]
    fn detect_gemini_model_used() {
        let engine = PatternEngine::new();
        let text = "Responding with gemini-2.0-flash-exp model";
        let detections = engine.detect(text);
        let detection = detections.iter().find(|d| d.rule_id == "gemini.model.used");
        assert!(detection.is_some(), "Should match gemini.model.used");
        let d = detection.unwrap();
        assert_eq!(
            d.extracted.get("model").and_then(|v| v.as_str()),
            Some("gemini-2.0-flash-exp")
        );
    }

    #[test]
    fn detect_wezterm_mux_connection_lost() {
        let engine = PatternEngine::new();
        let text = "Error: mux server connection lost, attempting reconnect...";
        let detections = engine.detect(text);
        let detection = detections
            .iter()
            .find(|d| d.rule_id == "wezterm.mux.connection_lost");
        assert!(
            detection.is_some(),
            "Should match wezterm.mux.connection_lost"
        );
        assert_eq!(detection.unwrap().severity, Severity::Critical);
    }

    #[test]
    fn detect_wezterm_pane_exited() {
        let engine = PatternEngine::new();
        let text = "shell exited with exit status: 0";
        let detections = engine.detect(text);
        let detection = detections
            .iter()
            .find(|d| d.rule_id == "wezterm.pane.exited");
        assert!(detection.is_some(), "Should match wezterm.pane.exited");
        let d = detection.unwrap();
        assert_eq!(
            d.extracted.get("exit_code").and_then(|v| v.as_str()),
            Some("0")
        );
    }

    #[test]
    fn detect_wezterm_pane_exited_nonzero() {
        let engine = PatternEngine::new();
        let text = "process exited with status 1";
        let detections = engine.detect(text);
        let detection = detections
            .iter()
            .find(|d| d.rule_id == "wezterm.pane.exited");
        assert!(
            detection.is_some(),
            "Should match wezterm.pane.exited with non-zero exit"
        );
        let d = detection.unwrap();
        assert_eq!(
            d.extracted.get("exit_code").and_then(|v| v.as_str()),
            Some("1")
        );
    }

    #[test]
    fn no_false_positives_on_unrelated_text() {
        let engine = PatternEngine::new();
        let text = "This is just some regular text about coding and programming.";
        let detections = engine.detect(text);
        assert!(
            detections.is_empty(),
            "Should not detect patterns in unrelated text"
        );
    }

    #[test]
    fn no_false_positives_on_similar_keywords() {
        let engine = PatternEngine::new();
        // Text that contains substrings of anchors but shouldn't match full rules
        let text = "The less work we do, the better. Try again later with a 10% discount.";
        let detections = engine.detect(text);
        // Should not match usage warnings because the full anchor isn't present
        let usage_warning = detections
            .iter()
            .find(|d| d.rule_id.contains("usage.warning"));
        assert!(
            usage_warning.is_none(),
            "Should not have false positive on partial matches"
        );
    }

    // ========================================================================
    // Fixture corpus regression harness
    // ========================================================================

    #[derive(Debug, Deserialize)]
    struct PatternFixture {
        name: String,
        text: String,
        expected_rule_ids: Vec<String>,
        #[serde(default)]
        expected_extracted: Option<HashMap<String, String>>,
        #[serde(default)]
        negative_for: Option<String>,
    }

    fn fixtures_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("patterns")
            .join("builtin.json")
    }

    fn load_fixtures() -> Vec<PatternFixture> {
        let path = fixtures_path();
        let content = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("Failed to read fixture {}: {}", path.display(), e));
        serde_json::from_str(&content)
            .unwrap_or_else(|e| panic!("Failed to parse fixture {}: {}", path.display(), e))
    }

    #[test]
    fn fixture_corpus_matches_expected() {
        let engine = PatternEngine::new();
        let fixtures = load_fixtures();

        for fixture in fixtures {
            let detections = engine.detect(&fixture.text);
            let mut actual: Vec<String> = detections.iter().map(|d| d.rule_id.clone()).collect();
            let mut expected = fixture.expected_rule_ids.clone();
            actual.sort();
            expected.sort();

            assert_eq!(
                actual, expected,
                "fixture '{}' mismatch (text: {})",
                fixture.name, fixture.text
            );

            if let Some(expected_extracted) = fixture.expected_extracted.as_ref() {
                assert_eq!(
                    fixture.expected_rule_ids.len(),
                    1,
                    "fixture '{}' extraction expects a single rule id",
                    fixture.name
                );
                let rule_id = fixture.expected_rule_ids[0].as_str();
                let detection = detections
                    .iter()
                    .find(|d| d.rule_id == rule_id)
                    .expect("expected detection missing");
                for (key, expected_value) in expected_extracted {
                    let actual_value = detection
                        .extracted
                        .get(key)
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    assert_eq!(
                        actual_value, expected_value,
                        "fixture '{}' extraction mismatch for {}",
                        fixture.name, key
                    );
                }
            }
        }
    }

    #[test]
    fn fixtures_cover_all_builtin_rules() {
        let engine = PatternEngine::new();
        let fixtures = load_fixtures();

        let mut positives = HashSet::new();
        let mut negatives = HashSet::new();

        for fixture in fixtures {
            for rule_id in &fixture.expected_rule_ids {
                positives.insert(rule_id.clone());
            }
            if let Some(rule_id) = fixture.negative_for {
                negatives.insert(rule_id);
            }
        }

        for rule in engine.rules() {
            assert!(
                positives.contains(&rule.id),
                "Missing positive fixture for rule {}",
                rule.id
            );
            assert!(
                negatives.contains(&rule.id),
                "Missing negative fixture for rule {}",
                rule.id
            );
        }
    }
}
