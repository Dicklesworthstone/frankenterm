//! Pattern detection engine
//!
//! Provides fast, reliable detection of agent state transitions.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use aho_corasick::AhoCorasick;
use bloomfilter::Bloom;
use fancy_regex::Regex;
use memchr::memchr;
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::config::{PackOverride, PatternsConfig};
use crate::error::PatternError;
use crate::policy::Redactor;

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
    /// Byte offsets in the source text
    #[serde(skip)]
    pub span: (usize, usize),
}

/// Options for explain-match trace generation.
#[derive(Debug, Clone)]
pub struct TraceOptions {
    /// Maximum number of evidence items per trace.
    pub max_evidence_items: usize,
    /// Maximum bytes per excerpt in evidence items.
    pub max_excerpt_bytes: usize,
    /// Maximum bytes per extracted capture value.
    pub max_capture_bytes: usize,
    /// Include traces for rules that did not fully match (e.g., regex miss).
    pub include_non_matches: bool,
}

impl Default for TraceOptions {
    fn default() -> Self {
        Self {
            max_evidence_items: 8,
            max_excerpt_bytes: 160,
            max_capture_bytes: 120,
            include_non_matches: false,
        }
    }
}

/// Span information for trace evidence (byte offsets in the original text).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceSpan {
    /// Start byte offset (inclusive)
    pub start: usize,
    /// End byte offset (exclusive)
    pub end: usize,
}

/// Trace evidence item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceEvidence {
    /// Evidence kind (anchor, match, capture)
    pub kind: String,
    /// Optional label (anchor text or capture name)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Optional span in the original text
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<TraceSpan>,
    /// Redacted, bounded excerpt
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
    /// Whether the excerpt was truncated to bounds
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// Gate evaluation trace for rule eligibility.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceGate {
    /// Gate identifier (e.g., "agent_type", "dedupe")
    pub gate: String,
    /// Whether the gate passed
    pub passed: bool,
    /// Optional explanation for failures
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Boundedness metadata for a trace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceBounds {
    /// Maximum evidence items allowed
    pub max_evidence_items: usize,
    /// Maximum bytes per excerpt
    pub max_excerpt_bytes: usize,
    /// Maximum bytes per capture value
    pub max_capture_bytes: usize,
    /// Total evidence items before truncation
    pub evidence_total: usize,
    /// Whether evidence list was truncated
    pub evidence_truncated: bool,
    /// Labels of fields that were truncated
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub truncated_fields: Vec<String>,
}

/// Explain-match trace for a single rule match.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MatchTrace {
    /// Pack identifier (e.g., "builtin:codex")
    pub pack_id: String,
    /// Stable rule identifier
    pub rule_id: String,
    /// Optional extractor identifier (e.g., "regex")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extractor_id: Option<String>,
    /// Redacted, bounded matched text (best-effort)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_text: Option<String>,
    /// Optional confidence (mirrors detection confidence when available)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    /// Whether the rule is eligible after state gates
    pub eligible: bool,
    /// Gate decisions
    pub gates: Vec<TraceGate>,
    /// Evidence list (anchors, matches, captures)
    pub evidence: Vec<TraceEvidence>,
    /// Boundedness metadata
    pub bounds: TraceBounds,
}

impl Detection {
    /// Generate a stable dedup key for this detection.
    ///
    /// The key is based on rule_id + significant extracted values, allowing
    /// the same rule to fire multiple times if the extracted values differ.
    #[must_use]
    pub fn dedup_key(&self) -> String {
        let extracted_hash = self.extracted.as_object().map_or_else(String::new, |obj| {
            let mut parts: Vec<String> = obj.iter().map(|(k, v)| format!("{k}:{v}")).collect();
            parts.sort();
            parts.join("|")
        });
        format!("{}:{}", self.rule_id, extracted_hash)
    }
}

/// Context for detection with agent filtering and deduplication.
///
/// Use this to prevent false positives in non-agent panes and avoid
/// re-emitting the same detection across overlapping tail windows.
#[derive(Debug, Clone)]
pub struct DetectionContext {
    /// Pane ID for tracking
    pub pane_id: Option<u64>,
    /// Inferred agent type for this pane (if known)
    pub agent_type: Option<AgentType>,
    /// Previously seen dedup keys with timestamp to avoid re-emitting
    seen_keys: HashMap<String, Instant>,
    /// Order of seen keys for eviction (FIFO)
    seen_order: VecDeque<String>,
    /// Time-to-live for deduplication (default: 5 minutes)
    pub ttl: Duration,
    /// Tail buffer from previous detection (for cross-segment matching)
    pub tail_buffer: String,
}

impl Default for DetectionContext {
    fn default() -> Self {
        Self::new()
    }
}

impl DetectionContext {
    /// Maximum number of seen keys to retain
    const MAX_SEEN_KEYS: usize = 1000;
    /// Default deduplication TTL
    const DEFAULT_TTL: Duration = Duration::from_secs(5 * 60);
    /// Maximum tail buffer size (2KB)
    const MAX_TAIL_SIZE: usize = 2048;

    /// Create a new empty detection context
    #[must_use]
    pub fn new() -> Self {
        Self {
            pane_id: None,
            agent_type: None,
            seen_keys: HashMap::new(),
            seen_order: VecDeque::new(),
            ttl: Self::DEFAULT_TTL,
            tail_buffer: String::new(),
        }
    }

    /// Create a context with a known agent type
    #[must_use]
    pub fn with_agent_type(agent_type: AgentType) -> Self {
        Self {
            pane_id: None,
            agent_type: Some(agent_type),
            seen_keys: HashMap::new(),
            seen_order: VecDeque::new(),
            ttl: Self::DEFAULT_TTL,
            tail_buffer: String::new(),
        }
    }

    /// Create a context for a specific pane
    #[must_use]
    pub fn with_pane(pane_id: u64, agent_type: Option<AgentType>) -> Self {
        Self {
            pane_id: Some(pane_id),
            agent_type,
            seen_keys: HashMap::new(),
            seen_order: VecDeque::new(),
            ttl: Self::DEFAULT_TTL,
            tail_buffer: String::new(),
        }
    }

    /// Set the deduplication TTL.
    pub fn set_ttl(&mut self, ttl: Duration) {
        self.ttl = ttl;
    }

    /// Mark a detection as seen, returning true if it was new (or expired)
    pub fn mark_seen(&mut self, detection: &Detection) -> bool {
        let key = detection.dedup_key();
        let now = Instant::now();

        // Check if seen and valid (not expired)
        if let Some(timestamp) = self.seen_keys.get(&key) {
            if now.duration_since(*timestamp) < self.ttl {
                return false;
            }
        }

        // Keep the order queue unique to avoid unbounded growth when a key
        // reappears after TTL expiry.
        if let Some(pos) = self.seen_order.iter().position(|item| item == &key) {
            self.seen_order.remove(pos);
        }

        // Enforce capacity if adding a new key (or updating expired one that was pruned?)
        // If we update an existing key, we don't increase count.
        // But if we insert new, we might overflow.
        // Simple strategy: Always remove if at capacity, then insert.
        if !self.seen_keys.contains_key(&key) && self.seen_keys.len() >= Self::MAX_SEEN_KEYS {
            if let Some(oldest) = self.seen_order.pop_front() {
                self.seen_keys.remove(&oldest);
            }
        }

        self.seen_keys.insert(key.clone(), now);
        // Push to back of order for LRU-style eviction.
        self.seen_order.push_back(key);
        true
    }

    /// Check if a detection has been seen before and is unexpired
    #[must_use]
    pub fn is_seen(&self, detection: &Detection) -> bool {
        let key = detection.dedup_key();
        if let Some(timestamp) = self.seen_keys.get(&key) {
            Instant::now().duration_since(*timestamp) < self.ttl
        } else {
            false
        }
    }

    /// Clear the set of seen detections and the tail buffer.
    ///
    /// This allows detecting patterns fresh in subsequent calls, as if
    /// starting from a new context. Both the dedup state and the cross-segment
    /// matching buffer are reset.
    pub fn clear_seen(&mut self) {
        self.seen_keys.clear();
        self.seen_order.clear();
        self.tail_buffer.clear();
    }

    /// Get the number of seen detections
    #[must_use]
    pub fn seen_count(&self) -> usize {
        self.seen_keys.len()
    }
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
    /// Manual fix instructions for when workflow is not available or user prefers manual action
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual_fix: Option<String>,
    /// Preview command template supporting {pane}, {event_id}, {agent} interpolation
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_command: Option<String>,
    /// URL for more information about this rule
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub learn_more_url: Option<String>,
}

impl RuleDef {
    fn validate(&self) -> Result<()> {
        self.validate_inner(false)
    }

    fn validate_user_pack(&self) -> Result<()> {
        self.validate_inner(true)
    }

    fn validate_inner(&self, allow_custom_prefix: bool) -> Result<()> {
        if self.id.trim().is_empty() {
            return Err(PatternError::InvalidRule("rule id cannot be empty".to_string()).into());
        }

        if !allow_custom_prefix
            && !ALLOWED_RULE_PREFIXES
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

        // Even user packs need a dotted namespace (e.g., "myorg.some_rule")
        if allow_custom_prefix && !self.id.contains('.') {
            return Err(PatternError::InvalidRule(format!(
                "rule id '{}' must contain a dot-separated namespace (e.g., 'myorg.my_rule')",
                self.id
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

    /// Interpolate template variables in a string.
    ///
    /// Supported variables:
    /// - `{pane}`: Pane ID where event was detected
    /// - `{event_id}`: Event ID for reference
    /// - `{agent}`: Detected agent type
    /// - `{rule_id}`: The rule ID that matched
    #[must_use]
    pub fn interpolate_template(
        template: &str,
        pane_id: u64,
        event_id_value: Option<i64>,
        agent_type: &AgentType,
        rule_id_value: &str,
    ) -> String {
        template
            .replace("{pane}", &pane_id.to_string())
            .replace(
                "{event_id}",
                &event_id_value.map_or_else(|| "unknown".to_string(), |id| id.to_string()),
            )
            .replace("{agent}", &agent_type.to_string())
            .replace("{rule_id}", rule_id_value)
    }

    /// Get the interpolated preview command for this rule.
    #[must_use]
    pub fn get_preview_command(&self, pane_id: u64, event_id: Option<i64>) -> Option<String> {
        self.preview_command.as_ref().map(|cmd| {
            Self::interpolate_template(cmd, pane_id, event_id, &self.agent_type, &self.id)
        })
    }

    /// Get the interpolated manual fix instructions for this rule.
    #[must_use]
    pub fn get_manual_fix(&self, pane_id: u64, event_id: Option<i64>) -> Option<String> {
        self.manual_fix.as_ref().map(|fix| {
            Self::interpolate_template(fix, pane_id, event_id, &self.agent_type, &self.id)
        })
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

    fn validate_as_user_pack(&self) -> Result<()> {
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
            rule.validate_user_pack()?;
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
    rule_to_pack: HashMap<String, String>,
}

impl PatternLibrary {
    /// Build a new library from packs (later packs override earlier packs by rule id)
    pub fn new(packs: Vec<PatternPack>) -> Result<Self> {
        for pack in &packs {
            pack.validate()?;
        }

        // Track the effective source pack for each rule id. Later packs override earlier packs.
        let mut rule_to_pack: HashMap<String, String> = HashMap::new();
        for pack in &packs {
            for rule in &pack.rules {
                rule_to_pack.insert(rule.id.clone(), pack.name.clone());
            }
        }

        let merged_rules = merge_rules(&packs);

        Ok(Self {
            packs,
            merged_rules,
            rule_to_pack,
        })
    }

    /// Build a library where user packs use relaxed validation (custom prefixes allowed).
    pub fn new_with_user_packs(
        packs: Vec<PatternPack>,
        user_pack_names: &HashSet<String>,
    ) -> Result<Self> {
        for pack in &packs {
            if user_pack_names.contains(&pack.name) {
                pack.validate_as_user_pack()?;
            } else {
                pack.validate()?;
            }
        }

        let mut rule_to_pack: HashMap<String, String> = HashMap::new();
        for pack in &packs {
            for rule in &pack.rules {
                rule_to_pack.insert(rule.id.clone(), pack.name.clone());
            }
        }

        let merged_rules = merge_rules(&packs);

        Ok(Self {
            packs,
            merged_rules,
            rule_to_pack,
        })
    }

    /// Create an empty library
    #[must_use]
    pub fn empty() -> Self {
        Self {
            packs: Vec::new(),
            merged_rules: Vec::new(),
            rule_to_pack: HashMap::new(),
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

    fn pack_id_for_rule_id(&self, rule_id: &str) -> Option<&str> {
        self.rule_to_pack.get(rule_id).map(String::as_str)
    }

    /// Return the pack name that provides a given rule ID, if any.
    #[must_use]
    pub fn pack_for_rule(&self, rule_id: &str) -> Option<&str> {
        self.pack_id_for_rule_id(rule_id)
    }
}

#[derive(Debug, Clone)]
struct CompiledRule {
    def: RuleDef,
    regex: Option<Regex>,
}

/// Target false positive rate for the Bloom filter (1%).
/// This keeps the filter small (~10KB for 1000 patterns) while providing
/// effective rejection of non-matching text.
const BLOOM_FALSE_POSITIVE_RATE: f64 = 0.01;

struct EngineIndex {
    compiled_rules: Vec<CompiledRule>,
    anchor_list: Vec<String>,
    anchor_to_rules: HashMap<String, Vec<usize>>,
    anchor_matcher: Option<AhoCorasick>,
    quick_bytes: Vec<u8>,
    /// Bloom filter for quick rejection of non-matching text.
    /// Contains all anchor strings for O(1) "definitely not present" checks.
    bloom: Option<Bloom<str>>,
    /// Unique anchor lengths (sorted, ascending) for efficient substring checking.
    anchor_lengths: Vec<usize>,
}

impl std::fmt::Debug for EngineIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineIndex")
            .field("compiled_rules", &self.compiled_rules.len())
            .field("anchor_list", &self.anchor_list.len())
            .field("anchor_to_rules", &self.anchor_to_rules.len())
            .field("anchor_matcher", &self.anchor_matcher.is_some())
            .field("quick_bytes", &self.quick_bytes.len())
            .field("bloom", &self.bloom.is_some())
            .field("anchor_lengths", &self.anchor_lengths)
            .finish()
    }
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

    // Build Bloom filter from anchor strings for fast pre-filtering.
    // Uses 1% false positive rate which keeps the filter small (~10KB)
    // while providing effective rejection of non-matching text.
    let bloom = if anchor_list.is_empty() {
        None
    } else {
        // Size the filter for the actual number of anchors, minimum 100 to avoid edge cases
        let num_items = anchor_list.len().max(100);
        let mut bloom = Bloom::new_for_fp_rate(num_items, BLOOM_FALSE_POSITIVE_RATE);
        for anchor in &anchor_list {
            bloom.set(anchor.as_str());
        }
        Some(bloom)
    };

    // Collect unique anchor lengths for efficient sliding window checks.
    // Sorted ascending to allow early exit on short texts.
    let mut anchor_lengths: Vec<usize> = anchor_list
        .iter()
        .map(String::len)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    anchor_lengths.sort_unstable();

    Ok(EngineIndex {
        compiled_rules,
        anchor_list,
        anchor_to_rules,
        anchor_matcher,
        quick_bytes,
        bloom,
        anchor_lengths,
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
        builtin_core_pack(),
        builtin_codex_pack(),
        builtin_claude_code_pack(),
        builtin_gemini_pack(),
        builtin_wezterm_pack(),
    ]
}

fn builtin_pack_by_name(name: &str) -> Option<PatternPack> {
    match name {
        "core" => Some(builtin_core_pack()),
        "codex" => Some(builtin_codex_pack()),
        "claude_code" => Some(builtin_claude_code_pack()),
        "gemini" => Some(builtin_gemini_pack()),
        "wezterm" => Some(builtin_wezterm_pack()),
        _ => None,
    }
}

struct LoadedPacks {
    packs: Vec<PatternPack>,
    user_pack_names: HashSet<String>,
}

fn load_packs_from_config(config: &PatternsConfig, root: Option<&Path>) -> Result<LoadedPacks> {
    let mut packs = Vec::with_capacity(config.packs.len());
    for pack_id in &config.packs {
        packs.push(load_pack_from_id(pack_id, root)?);
    }

    let mut user_pack_names = HashSet::new();

    // Discover user packs from config dir (e.g. ~/.config/wa/patterns/)
    if config.user_packs_enabled {
        if let Some(user_dir) = config.resolved_user_packs_dir() {
            let discovered = discover_packs_from_dir(&user_dir)?;
            for pack in discovered {
                user_pack_names.insert(pack.name.clone());
                packs.push(pack);
            }
        }
    }

    // Discover workspace-local packs from .wa/patterns/
    if let Some(root_path) = root {
        let ws_dir = root_path.join(".wa").join("patterns");
        let discovered = discover_packs_from_dir(&ws_dir)?;
        for pack in discovered {
            user_pack_names.insert(pack.name.clone());
            packs.push(pack);
        }
    }

    Ok(LoadedPacks {
        packs,
        user_pack_names,
    })
}

fn load_pack_from_id(pack_id: &str, root: Option<&Path>) -> Result<PatternPack> {
    if let Some(name) = pack_id.strip_prefix("builtin:") {
        return builtin_pack_by_name(name)
            .ok_or_else(|| PatternError::PackNotFound(pack_id.to_string()).into());
    }

    if let Some(path) = pack_id.strip_prefix("file:") {
        let mut pack = load_pack_from_file(path, root)?;
        pack.name = pack_id.to_string();
        return Ok(pack);
    }

    Err(PatternError::PackNotFound(pack_id.to_string()).into())
}

fn load_pack_from_file(path: &str, root: Option<&Path>) -> Result<PatternPack> {
    let raw_path = PathBuf::from(path);
    let resolved = if raw_path.is_absolute() {
        raw_path
    } else {
        root.map(|r| r.join(&raw_path)).unwrap_or(raw_path)
    };

    let content = std::fs::read_to_string(&resolved)
        .map_err(|e| PatternError::PackNotFound(format!("{} ({})", resolved.display(), e)))?;

    let ext = resolved
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();

    let pack = match ext.as_str() {
        "yaml" | "yml" => serde_yaml::from_str(&content)
            .map_err(|e| PatternError::InvalidRule(format!("invalid YAML pack: {e}")))?,
        "json" => serde_json::from_str(&content)
            .map_err(|e| PatternError::InvalidRule(format!("invalid JSON pack: {e}")))?,
        "toml" => toml::from_str(&content)
            .map_err(|e| PatternError::InvalidRule(format!("invalid TOML pack: {e}")))?,
        other => {
            return Err(PatternError::InvalidRule(format!(
                "unsupported pack extension '{other}' (expected .yaml, .yml, .json, .toml)"
            ))
            .into());
        }
    };

    Ok(pack)
}

fn is_pack_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| matches!(e.to_lowercase().as_str(), "toml" | "yaml" | "yml" | "json"))
        .unwrap_or(false)
}

fn discover_packs_from_dir(dir: &Path) -> Result<Vec<PatternPack>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut packs = Vec::new();

    let entries = std::fs::read_dir(dir)
        .map_err(|e| PatternError::InvalidRule(format!("cannot read {}: {e}", dir.display())))?;

    let mut sorted: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    sorted.sort_by_key(|e| e.file_name());

    for entry in sorted {
        let path = entry.path();
        if path.is_file() && is_pack_file(&path) {
            match load_pack_from_file(path.to_str().unwrap_or_default(), None) {
                Ok(pack) => packs.push(pack),
                Err(e) => {
                    tracing::warn!("Skipping invalid user pack {}: {e}", path.display());
                }
            }
        } else if path.is_dir() {
            let rules_file = path.join("rules.toml");
            if rules_file.is_file() {
                match load_pack_from_file(rules_file.to_str().unwrap_or_default(), None) {
                    Ok(pack) => packs.push(pack),
                    Err(e) => {
                        tracing::warn!("Skipping invalid user pack {}: {e}", rules_file.display());
                    }
                }
            }
        }
    }

    Ok(packs)
}

fn apply_pack_overrides(
    packs: Vec<PatternPack>,
    overrides: &HashMap<String, PackOverride>,
) -> Result<Vec<PatternPack>> {
    if overrides.is_empty() {
        return Ok(packs);
    }

    let normalized = normalize_pack_overrides(overrides, &packs)?;
    let mut updated = Vec::with_capacity(packs.len());
    for mut pack in packs {
        if let Some(override_cfg) = normalized.get(&pack.name) {
            apply_pack_override_to_pack(&mut pack, override_cfg)?;
        }
        updated.push(pack);
    }
    Ok(updated)
}

fn normalize_pack_overrides(
    overrides: &HashMap<String, PackOverride>,
    packs: &[PatternPack],
) -> Result<HashMap<String, PackOverride>> {
    let mut normalized: HashMap<String, PackOverride> = HashMap::new();
    for (key, override_cfg) in overrides {
        let canonical = normalize_pack_key(key, packs).ok_or_else(|| {
            PatternError::InvalidRule(format!(
                "pack override '{}' does not match any enabled pack",
                key
            ))
        })?;

        let entry = normalized.entry(canonical).or_default();
        *entry = merge_pack_overrides(entry, override_cfg);
    }
    Ok(normalized)
}

fn normalize_pack_key(key: &str, packs: &[PatternPack]) -> Option<String> {
    if packs.iter().any(|p| p.name == key) {
        return Some(key.to_string());
    }

    if let Some(stripped) = key.strip_prefix("builtin:") {
        let candidate = format!("builtin:{stripped}");
        if packs.iter().any(|p| p.name == candidate) {
            return Some(candidate);
        }
    }

    packs
        .iter()
        .find(|p| p.name.strip_prefix("builtin:") == Some(key))
        .map(|p| p.name.clone())
}

fn merge_pack_overrides(base: &PackOverride, overlay: &PackOverride) -> PackOverride {
    let mut merged = base.clone();

    for rule in &overlay.disabled_rules {
        if !merged.disabled_rules.contains(rule) {
            merged.disabled_rules.push(rule.clone());
        }
    }

    for (rule_id, severity) in &overlay.severity_overrides {
        merged
            .severity_overrides
            .insert(rule_id.clone(), severity.clone());
    }

    for (key, value) in &overlay.extra {
        merged.extra.insert(key.clone(), value.clone());
    }

    merged
}

fn apply_pack_override_to_pack(pack: &mut PatternPack, override_cfg: &PackOverride) -> Result<()> {
    if !override_cfg.severity_overrides.is_empty() {
        for rule in &mut pack.rules {
            if let Some(severity) = override_cfg.severity_overrides.get(&rule.id) {
                rule.severity = parse_severity_override(severity)?;
            }
        }
    }

    if !override_cfg.disabled_rules.is_empty() {
        let disabled: HashSet<&str> = override_cfg
            .disabled_rules
            .iter()
            .map(String::as_str)
            .collect();
        pack.rules
            .retain(|rule| !disabled.contains(rule.id.as_str()));
    }

    Ok(())
}

fn parse_severity_override(value: &str) -> Result<Severity> {
    match value.trim().to_lowercase().as_str() {
        "info" => Ok(Severity::Info),
        "warning" => Ok(Severity::Warning),
        "critical" => Ok(Severity::Critical),
        _ => Err(PatternError::InvalidRule(format!(
            "invalid severity override '{value}' (expected info, warning, critical)"
        ))
        .into()),
    }
}

/// Builtin core pack (shared rules + placeholders)
fn builtin_core_pack() -> PatternPack {
    PatternPack::new("builtin:core", "0.1.0", Vec::new())
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
                manual_fix: None,
                preview_command: None,
                learn_more_url: None,
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
                manual_fix: None,
                preview_command: None,
                learn_more_url: None,
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
                manual_fix: Some("Finish current task quickly and prepare to switch accounts or wait for reset".to_string()),
                preview_command: Some("wa workflow run handle_usage_warning --pane {pane} --dry-run".to_string()),
                learn_more_url: None,
            },
            // Usage limit reached
            RuleDef {
                id: "codex.usage.reached".to_string(),
                agent_type: AgentType::Codex,
                event_type: "usage.reached".to_string(),
                severity: Severity::Critical,
                anchors: vec![
                    "You've hit your usage limit".to_string(),
                    "You've reached your usage limit".to_string(),
                ],
                regex: Some(r"try again at (?P<reset_time>[^.]+)".to_string()),
                description: "Codex usage limit reached".to_string(),
                remediation: Some("Wait for reset or switch account".to_string()),
                workflow: Some("handle_usage_limits".to_string()),
                manual_fix: Some("Exit Codex with Ctrl-C, log out, then log in with a different OpenAI account".to_string()),
                preview_command: Some("wa workflow run handle_usage_limits --pane {pane} --dry-run".to_string()),
                learn_more_url: None,
            },
            // Session token usage summary
            RuleDef {
                id: "codex.session.token_usage".to_string(),
                agent_type: AgentType::Codex,
                event_type: "session.summary".to_string(),
                severity: Severity::Info,
                anchors: vec!["Token usage:".to_string()],
                regex: Some(
                    r"total=(?P<total>[\d,]+)\s+input=(?P<input>[\d,]+)(?:\s+\(\+\s*(?P<cached>[\d,]+)\s+cached\))?\s+output=(?P<output>[\d,]+)(?:\s+\(reasoning\s+(?P<reasoning>[\d,]+)\))?".to_string()
                ),
                description: "Codex session token usage summary".to_string(),
                remediation: None,
                workflow: Some("handle_session_end".to_string()),
                manual_fix: None,
                preview_command: None,
                learn_more_url: None,
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
                manual_fix: None,
                preview_command: None,
                learn_more_url: None,
            },
            // Device auth code prompt
            RuleDef {
                id: "codex.auth.device_code_prompt".to_string(),
                agent_type: AgentType::Codex,
                event_type: "auth.device_code".to_string(),
                severity: Severity::Info,
                anchors: vec![
                    "Enter this one-time code".to_string(),
                    "enter this one-time code".to_string(),
                ],
                regex: Some(r"(?P<code>[A-Z0-9]{4}-[A-Z0-9]{5})".to_string()),
                description: "Codex device authentication code prompt".to_string(),
                remediation: Some("User needs to enter the code in browser".to_string()),
                workflow: Some("handle_auth_required".to_string()),
                manual_fix: None,
                preview_command: None,
                learn_more_url: None,
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
                manual_fix: Some("Ask the agent to re-read AGENTS.md and key project context files".to_string()),
                preview_command: Some("wa workflow run handle_compaction --pane {pane} --dry-run".to_string()),
                learn_more_url: None,
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
                manual_fix: None,
                preview_command: None,
                learn_more_url: None,
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
                manual_fix: None,
                preview_command: None,
                learn_more_url: None,
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
                workflow: Some("handle_claude_code_limits".to_string()),
                manual_fix: Some("Wait for limit reset or start a new Claude Code session".to_string()),
                preview_command: Some("wa workflow run handle_claude_code_limits --pane {pane} --dry-run".to_string()),
                learn_more_url: None,
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
                workflow: Some("handle_session_end".to_string()),
                manual_fix: None,
                preview_command: None,
                learn_more_url: None,
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
                workflow: Some("handle_auth_required".to_string()),
                manual_fix: None,
                preview_command: None,
                learn_more_url: None,
            },
            // Auth login required (browser auth flow)
            RuleDef {
                id: "claude_code.auth.login_required".to_string(),
                agent_type: AgentType::ClaudeCode,
                event_type: "auth.login_required".to_string(),
                severity: Severity::Warning,
                anchors: vec![
                    "To sign in".to_string(),
                    "login required".to_string(),
                    "please authenticate".to_string(),
                    "auth required".to_string(),
                ],
                regex: None,
                description: "Claude Code login/authentication required via browser".to_string(),
                remediation: Some("Complete authentication in the browser window".to_string()),
                workflow: Some("handle_auth_required".to_string()),
                manual_fix: Some("Open the provided URL in a browser and complete the login flow".to_string()),
                preview_command: None,
                learn_more_url: None,
            },
            // Tool use indicator
            RuleDef {
                id: "claude_code.tool_use".to_string(),
                agent_type: AgentType::ClaudeCode,
                event_type: "session.tool_use".to_string(),
                severity: Severity::Info,
                anchors: vec![
                    "Using tool".to_string(),
                    "Tool call:".to_string(),
                    "Executing:".to_string(),
                ],
                regex: Some(
                    r"(?:Using tool|Tool call|Executing)[:\s]+(?P<tool_name>Bash|Read|Write|Edit|Glob|Grep|Task|WebFetch|WebSearch|TodoWrite|NotebookEdit)".to_string()
                ),
                description: "Claude Code tool invocation".to_string(),
                remediation: None,
                workflow: None,
                manual_fix: None,
                preview_command: None,
                learn_more_url: None,
            },
            // Approval/permission needed
            RuleDef {
                id: "claude_code.approval_needed".to_string(),
                agent_type: AgentType::ClaudeCode,
                event_type: "session.approval_needed".to_string(),
                severity: Severity::Warning,
                anchors: vec![
                    "Approve?".to_string(),
                    "Allow?".to_string(),
                    "Permission".to_string(),
                    "Do you want".to_string(),
                ],
                regex: Some(
                    r"(?P<action>run|execute|write|delete|send|allow|proceed).*?\?".to_string()
                ),
                description: "Claude Code approval/permission prompt".to_string(),
                remediation: Some("User input required for approval".to_string()),
                workflow: None,
                manual_fix: None,
                preview_command: None,
                learn_more_url: None,
            },
            // Context window warning
            RuleDef {
                id: "claude_code.context.warning".to_string(),
                agent_type: AgentType::ClaudeCode,
                event_type: "context.warning".to_string(),
                severity: Severity::Warning,
                anchors: vec![
                    "context window".to_string(),
                    "context limit".to_string(),
                    "running low on context".to_string(),
                ],
                regex: Some(
                    r"(?P<percent>\d+)%?\s*(?:of context|context (?:used|remaining))".to_string()
                ),
                description: "Claude Code context window warning".to_string(),
                remediation: Some("Consider compacting or starting new session".to_string()),
                workflow: None,
                manual_fix: None,
                preview_command: None,
                learn_more_url: None,
            },
            // Extended thinking indicator
            RuleDef {
                id: "claude_code.thinking".to_string(),
                agent_type: AgentType::ClaudeCode,
                event_type: "session.thinking".to_string(),
                severity: Severity::Info,
                anchors: vec![
                    "Thinking".to_string(),
                    "Extended thinking".to_string(),
                    "ultrathink".to_string(),
                ],
                regex: Some(
                    r"(?:Thinking|Extended thinking)(?:\.{3}|\s+for\s+(?P<duration>\d+)\s*(?:seconds?|s))".to_string()
                ),
                description: "Claude Code extended thinking mode".to_string(),
                remediation: None,
                workflow: None,
                manual_fix: None,
                preview_command: None,
                learn_more_url: None,
            },
            // Network/connection error
            RuleDef {
                id: "claude_code.error.network".to_string(),
                agent_type: AgentType::ClaudeCode,
                event_type: "error.network".to_string(),
                severity: Severity::Critical,
                anchors: vec![
                    "connection".to_string(),
                    "network error".to_string(),
                    "failed to connect".to_string(),
                    "ECONNREFUSED".to_string(),
                ],
                regex: Some(
                    r"(?:connection|network)\s+(?:error|failed|refused|timeout|closed)".to_string()
                ),
                description: "Claude Code network/connection error".to_string(),
                remediation: Some("Check network connectivity and retry".to_string()),
                workflow: None,
                manual_fix: None,
                preview_command: None,
                learn_more_url: None,
            },
            // Timeout error
            RuleDef {
                id: "claude_code.error.timeout".to_string(),
                agent_type: AgentType::ClaudeCode,
                event_type: "error.timeout".to_string(),
                severity: Severity::Warning,
                anchors: vec![
                    "timed out".to_string(),
                    "time out".to_string(),
                ],
                regex: Some(
                    r"timed? out(?:\s+after\s+(?P<duration>\d+)\s*(?:seconds?|ms|s))?".to_string()
                ),
                description: "Claude Code timeout error".to_string(),
                remediation: Some("Operation timed out - consider retrying".to_string()),
                workflow: None,
                manual_fix: None,
                preview_command: None,
                learn_more_url: None,
            },
            // Session/conversation end
            RuleDef {
                id: "claude_code.session.end".to_string(),
                agent_type: AgentType::ClaudeCode,
                event_type: "session.end".to_string(),
                severity: Severity::Info,
                anchors: vec![
                    "Session ended".to_string(),
                    "Goodbye".to_string(),
                    "session complete".to_string(),
                ],
                regex: Some(
                    r"(?:Session|Conversation)\s+(?:ended|complete|finished)".to_string()
                ),
                description: "Claude Code session ended".to_string(),
                remediation: None,
                workflow: Some("handle_session_end".to_string()),
                manual_fix: None,
                preview_command: None,
                learn_more_url: None,
            },
            // Model selection/change
            RuleDef {
                id: "claude_code.model.selected".to_string(),
                agent_type: AgentType::ClaudeCode,
                event_type: "session.model".to_string(),
                severity: Severity::Info,
                anchors: vec![
                    "model:".to_string(),
                    "Using model".to_string(),
                    "claude-".to_string(),
                ],
                regex: Some(
                    r"(?:model[:\s]+|Using model[:\s]+)(?P<model>claude-(?:opus|sonnet|haiku)-?[^\s,\]]+)".to_string()
                ),
                description: "Claude Code model selection".to_string(),
                remediation: None,
                workflow: None,
                manual_fix: None,
                preview_command: None,
                learn_more_url: None,
            },
            // Auto-compaction completed
            RuleDef {
                id: "claude_code.compaction.complete".to_string(),
                agent_type: AgentType::ClaudeCode,
                event_type: "session.compaction_complete".to_string(),
                severity: Severity::Info,
                anchors: vec![
                    "Compaction complete".to_string(),
                    "Summary created".to_string(),
                    "compacted successfully".to_string(),
                ],
                regex: Some(
                    r"(?:Compaction complete|compacted successfully|Summary created)(?:.*?saved\s+(?P<tokens_saved>[\d,]+))?".to_string()
                ),
                description: "Claude Code compaction completed".to_string(),
                remediation: None,
                workflow: None,
                manual_fix: None,
                preview_command: None,
                learn_more_url: None,
            },
            // Overloaded error (API busy)
            RuleDef {
                id: "claude_code.error.overloaded".to_string(),
                agent_type: AgentType::ClaudeCode,
                event_type: "error.overloaded".to_string(),
                severity: Severity::Warning,
                anchors: vec![
                    "overloaded".to_string(),
                    "too many requests".to_string(),
                    "server busy".to_string(),
                ],
                regex: Some(
                    r"(?:overloaded|too many requests|server busy)(?:.*?retry\s+(?:in\s+)?(?P<retry_after>\d+)\s*(?:seconds?|s))?".to_string()
                ),
                description: "Claude API is overloaded".to_string(),
                remediation: Some("API is busy - wait and retry".to_string()),
                workflow: None,
                manual_fix: None,
                preview_command: None,
                learn_more_url: None,
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
            // Usage warning (approaching limit)
            RuleDef {
                id: "gemini.usage.warning".to_string(),
                agent_type: AgentType::Gemini,
                event_type: "usage.warning".to_string(),
                severity: Severity::Warning,
                anchors: vec![
                    "Usage limit warning".to_string(),
                    "approaching your usage limit".to_string(),
                    "usage limit approaching".to_string(),
                ],
                regex: Some(
                    r"(?:Usage limit (?:warning|approaching)|approaching your usage limit|usage limit approaching)[^\n]*?(?P<remaining>\d+)%\s+of\s+your\s+Pro\s+models?\s+quota\s+remaining"
                        .to_string(),
                ),
                description: "Gemini usage limit approaching".to_string(),
                remediation: Some("Consider switching models or accounts soon".to_string()),
                workflow: None,
                manual_fix: None,
                preview_command: None,
                learn_more_url: None,
            },
            // Usage limit reached
            RuleDef {
                id: "gemini.usage.reached".to_string(),
                agent_type: AgentType::Gemini,
                event_type: "usage.reached".to_string(),
                severity: Severity::Critical,
                anchors: vec![
                    "Usage limit reached for all Pro models".to_string(),
                    "Usage limit reached for Pro models".to_string(),
                    "Usage limit reached for your Pro models".to_string(),
                ],
                regex: None,
                description: "Gemini usage limit reached".to_string(),
                remediation: Some("Wait for limit reset or switch model".to_string()),
                workflow: Some("handle_gemini_quota".to_string()),
                manual_fix: Some("Switch to a non-Pro model or wait for quota reset".to_string()),
                preview_command: Some("wa workflow run handle_gemini_quota --pane {pane} --dry-run".to_string()),
                learn_more_url: None,
            },
            // Session summary
            RuleDef {
                id: "gemini.session.summary".to_string(),
                agent_type: AgentType::Gemini,
                event_type: "session.summary".to_string(),
                severity: Severity::Info,
                anchors: vec![
                    "Interaction Summary".to_string(),
                    "Session Summary".to_string(),
                ],
                regex: Some(
                    r"Session ID:\s*(?P<session_id>[0-9a-fA-F-]+)[\s\S]*?Tool Calls:\s*(?P<tool_calls>\d+)(?:[\s\S]*?Tokens Used:\s*(?P<tokens_used>[\d,]+))?(?:[\s\S]*?Input Tokens:\s*(?P<input_tokens>[\d,]+))?(?:[\s\S]*?Output Tokens:\s*(?P<output_tokens>[\d,]+))?"
                        .to_string(),
                ),
                description: "Gemini session summary with statistics".to_string(),
                remediation: None,
                workflow: Some("handle_session_end".to_string()),
                manual_fix: None,
                preview_command: None,
                learn_more_url: None,
            },
            // Session resume hint
            RuleDef {
                id: "gemini.session.resume_hint".to_string(),
                agent_type: AgentType::Gemini,
                event_type: "session.resume_hint".to_string(),
                severity: Severity::Info,
                anchors: vec!["gemini resume".to_string(), "gemini --resume".to_string()],
                regex: Some(
                    r"gemini\s+(?:resume|--resume)\s+(?P<session_id>[0-9a-fA-F-]{8,})"
                        .to_string(),
                ),
                description: "Gemini session resume hint".to_string(),
                remediation: None,
                workflow: None,
                manual_fix: None,
                preview_command: None,
                learn_more_url: None,
            },
            // Model indicator
            RuleDef {
                id: "gemini.model.used".to_string(),
                agent_type: AgentType::Gemini,
                event_type: "session.model".to_string(),
                severity: Severity::Info,
                anchors: vec![
                    "Responding with gemini-".to_string(),
                    "Using model".to_string(),
                    "Model:".to_string(),
                    "Switched to model".to_string(),
                ],
                regex: Some(
                    r"(?:Responding with\s+|Using model[:\s]+|Model[:\s]+|Switched to model[:\s]+)(?P<model>gemini-[^\s,]+)"
                        .to_string(),
                ),
                description: "Gemini model being used".to_string(),
                remediation: None,
                workflow: None,
                manual_fix: None,
                preview_command: None,
                learn_more_url: None,
            },
            // OAuth required (browser auth flow)
            RuleDef {
                id: "gemini.auth.oauth_required".to_string(),
                agent_type: AgentType::Gemini,
                event_type: "auth.oauth_required".to_string(),
                severity: Severity::Warning,
                anchors: vec![
                    "authorize this app".to_string(),
                    "complete authentication".to_string(),
                    "sign in with Google".to_string(),
                ],
                regex: None,
                description: "Gemini CLI OAuth authentication required via browser".to_string(),
                remediation: Some("Complete Google OAuth in the browser window".to_string()),
                workflow: Some("handle_auth_required".to_string()),
                manual_fix: Some("Open the provided Google OAuth URL in a browser and complete sign-in".to_string()),
                preview_command: None,
                learn_more_url: None,
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
                manual_fix: None,
                preview_command: None,
                learn_more_url: None,
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
                manual_fix: None,
                preview_command: None,
                learn_more_url: None,
            },
        ],
    )
}

/// Pattern engine for detecting agent state transitions
pub struct PatternEngine {
    /// Merged rule library
    library: PatternLibrary,
    /// Lazily-initialized compiled index (first-use compilation)
    index: OnceLock<EngineIndex>,
    /// Enable quick-reject optimization
    quick_reject_enabled: bool,
}

impl Default for PatternEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl PatternEngine {
    /// Create a new pattern engine with default packs (lazy-compiled on first use).
    #[must_use]
    pub fn new() -> Self {
        let library =
            PatternLibrary::new(builtin_packs()).expect("builtin pattern packs must be valid");
        Self {
            library,
            index: OnceLock::new(),
            quick_reject_enabled: true,
        }
    }

    /// Create a new pattern engine from explicit packs
    pub fn with_packs(packs: Vec<PatternPack>) -> Result<Self> {
        Self::with_packs_and_settings(packs, true)
    }

    /// Create a new pattern engine using a PatternsConfig.
    pub fn from_config(config: &PatternsConfig) -> Result<Self> {
        Self::from_config_with_root(config, None)
    }

    /// Create a new pattern engine using a PatternsConfig and optional root for file packs.
    pub fn from_config_with_root(config: &PatternsConfig, root: Option<&Path>) -> Result<Self> {
        let loaded = load_packs_from_config(config, root)?;
        let packs = apply_pack_overrides(loaded.packs, &config.pack_overrides)?;
        let library = PatternLibrary::new_with_user_packs(packs, &loaded.user_pack_names)?;
        Ok(Self {
            library,
            index: OnceLock::new(),
            quick_reject_enabled: config.quick_reject_enabled,
        })
    }

    fn with_packs_and_settings(
        packs: Vec<PatternPack>,
        quick_reject_enabled: bool,
    ) -> Result<Self> {
        let library = PatternLibrary::new(packs)?;
        let index = build_engine_index(library.rules())?;
        let engine = Self {
            library,
            index: OnceLock::new(),
            quick_reject_enabled,
        };
        engine
            .index
            .set(index)
            .expect("pattern engine index should be uninitialized");
        Ok(engine)
    }

    /// Check if the engine has been compiled yet.
    #[must_use]
    pub fn is_initialized(&self) -> bool {
        self.index.get().is_some()
    }

    fn index(&self) -> &EngineIndex {
        self.index.get_or_init(|| {
            tracing::debug!("Compiling pattern engine (first use)");
            build_engine_index(self.library.rules())
                .expect("pattern engine must compile for builtin packs")
        })
    }

    /// Detect patterns in text
    #[must_use]
    pub fn detect(&self, text: &str) -> Vec<Detection> {
        if text.is_empty() {
            return Vec::new();
        }

        let index = self.index();

        if self.quick_reject_enabled && !Self::quick_reject_with_index(index, text) {
            return Vec::new();
        }

        let Some(matcher) = index.anchor_matcher.as_ref() else {
            return Vec::new();
        };

        let mut candidate_rules: HashSet<usize> = HashSet::new();
        let mut matched_anchor_by_rule: HashMap<usize, (String, (usize, usize))> = HashMap::new();

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

            let Some(anchor) = index.anchor_list.get(matched.pattern().as_usize()) else {
                #[cfg(test)]
                {
                    let pattern = matched.pattern().as_usize();
                    eprintln!("detect: pattern {pattern} not found in anchor_list");
                }
                continue;
            };

            if let Some(rule_indices) = index.anchor_to_rules.get(anchor) {
                for &idx in rule_indices {
                    candidate_rules.insert(idx);
                    matched_anchor_by_rule
                        .entry(idx)
                        .or_insert_with(|| (anchor.clone(), (matched.start(), matched.end())));
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
            let compiled = &index.compiled_rules[idx];
            let rule = &compiled.def;
            let (fallback_anchor, fallback_span) = matched_anchor_by_rule
                .get(&idx)
                .cloned()
                .unwrap_or_default();

            if let Some(regex) = compiled.regex.as_ref() {
                for capture_result in regex.captures_iter(text) {
                    let Ok(captures) = capture_result else {
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

                    let (matched_text, span) = captures.get(0).map_or_else(
                        || (fallback_anchor.clone(), fallback_span),
                        |m| (m.as_str().to_string(), (m.start(), m.end())),
                    );

                    detections.push(Detection {
                        rule_id: rule.id.clone(),
                        agent_type: rule.agent_type,
                        event_type: rule.event_type.clone(),
                        severity: rule.severity,
                        confidence: 0.95,
                        extracted: serde_json::Value::Object(extracted),
                        matched_text,
                        span,
                    });
                }
            } else {
                detections.push(Detection {
                    rule_id: rule.id.clone(),
                    agent_type: rule.agent_type,
                    event_type: rule.event_type.clone(),
                    severity: rule.severity,
                    confidence: 0.6,
                    extracted: serde_json::Value::Object(serde_json::Map::new()),
                    matched_text: fallback_anchor,
                    span: fallback_span,
                });
            }
        }

        detections
    }

    /// Detect patterns in text with agent filtering and deduplication.
    ///
    /// This method filters detections based on the context's agent type:
    /// - Agent-specific rules only fire if the pane is inferred to be that agent
    /// - WezTerm rules fire for all agent types (they're infrastructure)
    /// - Unknown agent type allows all rules (conservative fallback)
    ///
    /// Detections are also deduplicated based on rule_id + extracted values.
    ///
    /// # Cross-Segment Matching
    ///
    /// This method automatically handles cross-segment matching by buffering
    /// the tail of the text in `context.tail_buffer`. The next call will
    /// prepend this buffer to the input text to catch patterns split across segments.
    /// Detections that fall entirely within the overlap region are filtered out.
    #[must_use]
    pub fn detect_with_context(
        &self,
        text: &str,
        context: &mut DetectionContext,
    ) -> Vec<Detection> {
        if text.is_empty() {
            return Vec::new();
        }

        // Combine with tail buffer for cross-segment matching
        let (input_text, overlap_len) = if context.tail_buffer.is_empty() {
            (std::borrow::Cow::Borrowed(text), 0)
        } else {
            let mut s = String::with_capacity(context.tail_buffer.len() + text.len());
            s.push_str(&context.tail_buffer);
            s.push_str(text);
            (std::borrow::Cow::Owned(s), context.tail_buffer.len())
        };

        // Update tail buffer for next time
        // We keep the last N chars
        let full_len = input_text.len();
        if full_len > DetectionContext::MAX_TAIL_SIZE {
            // Take slice from end, respecting char boundaries
            let mut start = full_len - DetectionContext::MAX_TAIL_SIZE;
            while !input_text.is_char_boundary(start) && start < full_len {
                start += 1;
            }
            context.tail_buffer = input_text[start..].to_string();
        } else {
            context.tail_buffer = input_text.to_string();
        }

        // Get all potential detections first
        let all_detections = self.detect(&input_text);

        // Filter by agent type, span (overlap), and dedup
        let mut result = Vec::new();
        for detection in all_detections {
            // Overlap filtering: skip if match is ENTIRELY within the overlap region.
            // If a detection ends within the overlap, it was fully visible in the
            // previous segment and should not be re-emitted. Detections that span
            // the boundary (start in overlap, end in new text) are kept.
            if overlap_len > 0 && detection.span.1 <= overlap_len {
                continue;
            }

            // Adjust matched_text to be just the part relevant to the new segment?
            // No, the match is valid. But if we report it, we report the full match.
            // But wait, if we use `input_text` (Cow), `matched_text` is from that.
            // If `matched_text` spans across overlap, it contains part of tail.
            // This is correct.

            // State gating: filter by agent type if specified
            if let Some(expected_agent) = context.agent_type {
                if !Self::rule_applies_to_agent(&detection, expected_agent) {
                    continue;
                }
            }

            // Deduplication: skip if already seen
            if context.is_seen(&detection) {
                continue;
            }

            // Mark as seen and include in results
            context.mark_seen(&detection);
            result.push(detection);
        }

        result
    }

    /// Detect patterns in text with context filtering/deduplication, and optionally
    /// generate bounded + redacted explain-match traces.
    ///
    /// This method is intentionally separate from `detect_with_context` so that the
    /// hot path is not penalized when tracing is not needed.
    #[must_use]
    pub fn detect_with_context_and_trace(
        &self,
        text: &str,
        context: &mut DetectionContext,
        opts: &TraceOptions,
    ) -> (Vec<Detection>, Vec<MatchTrace>) {
        if text.is_empty() {
            return (Vec::new(), Vec::new());
        }

        // Combine with tail buffer for cross-segment matching (same semantics as detect_with_context).
        let (input_text, overlap_len) = if context.tail_buffer.is_empty() {
            (std::borrow::Cow::Borrowed(text), 0)
        } else {
            let mut s = String::with_capacity(context.tail_buffer.len() + text.len());
            s.push_str(&context.tail_buffer);
            s.push_str(text);
            (std::borrow::Cow::Owned(s), context.tail_buffer.len())
        };

        // Update tail buffer for next time; keep last N chars.
        let full_len = input_text.len();
        if full_len > DetectionContext::MAX_TAIL_SIZE {
            let mut start = full_len - DetectionContext::MAX_TAIL_SIZE;
            while !input_text.is_char_boundary(start) && start < full_len {
                start += 1;
            }
            context.tail_buffer = input_text[start..].to_string();
        } else {
            context.tail_buffer = input_text.to_string();
        }

        self.detect_with_context_and_trace_inner(&input_text, overlap_len, context, opts)
    }

    fn detect_with_context_and_trace_inner(
        &self,
        text: &str,
        overlap_len: usize,
        context: &mut DetectionContext,
        opts: &TraceOptions,
    ) -> (Vec<Detection>, Vec<MatchTrace>) {
        let index = self.index();

        if self.quick_reject_enabled && !Self::quick_reject_with_index(index, text) {
            return (Vec::new(), Vec::new());
        }

        let Some(matcher) = index.anchor_matcher.as_ref() else {
            return (Vec::new(), Vec::new());
        };

        let redactor = Redactor::new();

        let mut candidate_rules: HashSet<usize> = HashSet::new();
        let mut matched_anchor_by_rule: HashMap<usize, (String, (usize, usize))> = HashMap::new();

        for matched in matcher.find_overlapping_iter(text) {
            let Some(anchor) = index.anchor_list.get(matched.pattern().as_usize()) else {
                continue;
            };

            if let Some(rule_indices) = index.anchor_to_rules.get(anchor) {
                for &idx in rule_indices {
                    candidate_rules.insert(idx);
                    matched_anchor_by_rule
                        .entry(idx)
                        .or_insert_with(|| (anchor.clone(), (matched.start(), matched.end())));
                }
            }
        }

        if candidate_rules.is_empty() {
            return (Vec::new(), Vec::new());
        }

        let mut indices: Vec<usize> = candidate_rules.into_iter().collect();
        indices.sort_unstable();

        let mut detections: Vec<Detection> = Vec::new();
        let mut traces: Vec<MatchTrace> = Vec::new();

        for idx in indices {
            let compiled = &index.compiled_rules[idx];
            let rule = &compiled.def;

            let (fallback_anchor, fallback_span) = matched_anchor_by_rule
                .get(&idx)
                .cloned()
                .unwrap_or_default();

            let pack_id = self
                .library
                .pack_id_for_rule_id(&rule.id)
                .unwrap_or("unknown")
                .to_string();

            let anchor_evidence =
                Self::trace_anchor_evidence(text, &redactor, &fallback_anchor, fallback_span, opts);

            if let Some(regex) = compiled.regex.as_ref() {
                let mut any_capture = false;

                for capture_result in regex.captures_iter(text) {
                    let Ok(captures) = capture_result else {
                        continue;
                    };
                    any_capture = true;

                    let mut extracted = serde_json::Map::new();
                    for name in regex.capture_names().flatten() {
                        if let Some(value) = captures.name(name) {
                            extracted.insert(
                                name.to_string(),
                                serde_json::Value::String(value.as_str().to_string()),
                            );
                        }
                    }

                    let (raw_matched_text, span) = captures.get(0).map_or_else(
                        || (fallback_anchor.clone(), fallback_span),
                        |m| (m.as_str().to_string(), (m.start(), m.end())),
                    );

                    let detection = Detection {
                        rule_id: rule.id.clone(),
                        agent_type: rule.agent_type,
                        event_type: rule.event_type.clone(),
                        severity: rule.severity,
                        confidence: 0.95,
                        extracted: serde_json::Value::Object(extracted),
                        matched_text: raw_matched_text.clone(),
                        span,
                    };

                    let (eligible, gates) =
                        Self::evaluate_trace_gates(&detection, overlap_len, context);

                    if eligible {
                        context.mark_seen(&detection);
                        detections.push(detection.clone());
                    }

                    if eligible || opts.include_non_matches {
                        let trace = Self::build_match_trace(
                            text,
                            &redactor,
                            pack_id.clone(),
                            Some("regex".to_string()),
                            &detection,
                            eligible,
                            gates,
                            anchor_evidence.as_ref(),
                            Some((&captures, regex)),
                            opts,
                        );
                        traces.push(trace);
                    }
                }

                if !any_capture && opts.include_non_matches {
                    // Anchor hit, but regex didn't produce a match.
                    let mut gates = Self::trace_gates_skeleton();

                    // Agent-type gate can still be evaluated.
                    let agent_passed = context.agent_type.is_none_or(|expected_agent| {
                        expected_agent == AgentType::Unknown
                            || rule.agent_type == AgentType::Wezterm
                            || rule.agent_type == expected_agent
                    });
                    gates[0].passed = agent_passed;
                    if !agent_passed {
                        gates[0].reason =
                            Some("rule agent_type does not match inferred pane agent".to_string());
                    }

                    gates[3].passed = false;
                    gates[3].reason = Some("regex_no_match".to_string());

                    let eligible = false;

                    let trace = Self::build_match_trace_no_detection(
                        text,
                        &redactor,
                        pack_id.clone(),
                        rule.id.clone(),
                        Some("regex".to_string()),
                        eligible,
                        gates,
                        anchor_evidence.as_ref(),
                        opts,
                    );
                    traces.push(trace);
                }
            } else {
                // Anchor-only rule: anchor hit implies a match.
                let detection = Detection {
                    rule_id: rule.id.clone(),
                    agent_type: rule.agent_type,
                    event_type: rule.event_type.clone(),
                    severity: rule.severity,
                    confidence: 0.6,
                    extracted: serde_json::Value::Object(serde_json::Map::new()),
                    matched_text: fallback_anchor.clone(),
                    span: fallback_span,
                };

                let (eligible, gates) =
                    Self::evaluate_trace_gates(&detection, overlap_len, context);

                if eligible {
                    context.mark_seen(&detection);
                    detections.push(detection.clone());
                }

                if eligible || opts.include_non_matches {
                    let trace = Self::build_match_trace(
                        text,
                        &redactor,
                        pack_id.clone(),
                        Some("anchor".to_string()),
                        &detection,
                        eligible,
                        gates,
                        anchor_evidence.as_ref(),
                        None,
                        opts,
                    );
                    traces.push(trace);
                }
            }
        }

        (detections, traces)
    }

    fn trace_gates_skeleton() -> Vec<TraceGate> {
        vec![
            TraceGate {
                gate: "agent_type".to_string(),
                passed: true,
                reason: None,
            },
            TraceGate {
                gate: "overlap".to_string(),
                passed: true,
                reason: None,
            },
            TraceGate {
                gate: "dedupe".to_string(),
                passed: true,
                reason: None,
            },
            TraceGate {
                gate: "match".to_string(),
                passed: true,
                reason: None,
            },
        ]
    }

    fn evaluate_trace_gates(
        detection: &Detection,
        overlap_len: usize,
        context: &DetectionContext,
    ) -> (bool, Vec<TraceGate>) {
        let mut gates = Self::trace_gates_skeleton();

        // agent_type gate
        if let Some(expected_agent) = context.agent_type {
            let applies = Self::rule_applies_to_agent(detection, expected_agent);
            gates[0].passed = applies;
            if !applies {
                gates[0].reason =
                    Some("rule agent_type does not match inferred pane agent".to_string());
            }
        }

        // overlap gate
        if overlap_len > 0 && detection.span.1 <= overlap_len {
            gates[1].passed = false;
            gates[1].reason = Some("match within overlap window".to_string());
        }

        // dedupe gate
        if context.is_seen(detection) {
            gates[2].passed = false;
            gates[2].reason = Some("already seen within TTL".to_string());
        }

        // match gate is always true for a constructed Detection.
        gates[3].passed = true;

        let eligible = gates.iter().all(|g| g.passed);
        (eligible, gates)
    }

    fn trace_anchor_evidence(
        text: &str,
        redactor: &Redactor,
        anchor: &str,
        span: (usize, usize),
        opts: &TraceOptions,
    ) -> Option<TraceEvidence> {
        if anchor.is_empty() && span == (0, 0) {
            return None;
        }

        let excerpt_raw = Self::slice_bytes(text, span.0, span.1).unwrap_or(anchor);
        let excerpt_redacted = redactor.redact(excerpt_raw);
        let (excerpt, truncated) = Self::bound_utf8(&excerpt_redacted, opts.max_excerpt_bytes);

        Some(TraceEvidence {
            kind: "anchor".to_string(),
            label: if anchor.is_empty() {
                None
            } else {
                Some(anchor.to_string())
            },
            span: Some(TraceSpan {
                start: span.0,
                end: span.1,
            }),
            excerpt: Some(excerpt),
            truncated,
        })
    }

    fn build_match_trace(
        text: &str,
        redactor: &Redactor,
        pack_id: String,
        extractor_id: Option<String>,
        detection: &Detection,
        eligible: bool,
        mut gates: Vec<TraceGate>,
        anchor_evidence: Option<&TraceEvidence>,
        regex_context: Option<(&fancy_regex::Captures<'_>, &Regex)>,
        opts: &TraceOptions,
    ) -> MatchTrace {
        // Ensure gate list includes "match" in stable position.
        // (evaluate_trace_gates always sets match=true; callers may override for non-detections.)
        if gates.len() != 4 || gates[3].gate != "match" {
            gates = Self::trace_gates_skeleton();
        }

        let mut truncated_fields: std::collections::BTreeSet<String> =
            std::collections::BTreeSet::new();

        let matched_text_redacted = redactor.redact(&detection.matched_text);
        let (matched_text_bounded, matched_text_truncated) =
            Self::bound_utf8(&matched_text_redacted, opts.max_excerpt_bytes);
        if matched_text_truncated {
            truncated_fields.insert("matched_text".to_string());
        }

        let mut evidence: Vec<TraceEvidence> = Vec::new();
        if let Some(anchor) = anchor_evidence {
            if anchor.truncated {
                truncated_fields.insert("excerpt".to_string());
            }
            evidence.push(anchor.clone());
        }

        // Match evidence (best-effort excerpt from span).
        let match_excerpt_raw =
            Self::slice_bytes(text, detection.span.0, detection.span.1).unwrap_or("");
        let match_excerpt_redacted = redactor.redact(match_excerpt_raw);
        let (match_excerpt, match_excerpt_truncated) =
            Self::bound_utf8(&match_excerpt_redacted, opts.max_excerpt_bytes);
        if match_excerpt_truncated {
            truncated_fields.insert("excerpt".to_string());
        }

        evidence.push(TraceEvidence {
            kind: "match".to_string(),
            label: None,
            span: Some(TraceSpan {
                start: detection.span.0,
                end: detection.span.1,
            }),
            excerpt: if match_excerpt.is_empty() {
                None
            } else {
                Some(match_excerpt)
            },
            truncated: match_excerpt_truncated,
        });

        // Capture evidence (stable ordering by capture name).
        if let Some((captures, regex)) = regex_context {
            let mut names: Vec<&str> = regex.capture_names().flatten().collect();
            names.sort_unstable();

            for name in names {
                let Some(value) = captures.name(name) else {
                    continue;
                };

                let cap_redacted = redactor.redact(value.as_str());
                let (cap_bounded, cap_truncated) =
                    Self::bound_utf8(&cap_redacted, opts.max_capture_bytes);
                if cap_truncated {
                    truncated_fields.insert(format!("capture.{name}"));
                }

                evidence.push(TraceEvidence {
                    kind: "capture".to_string(),
                    label: Some(name.to_string()),
                    span: Some(TraceSpan {
                        start: value.start(),
                        end: value.end(),
                    }),
                    excerpt: Some(cap_bounded),
                    truncated: cap_truncated,
                });
            }
        }

        let evidence_total = evidence.len();
        let mut evidence_truncated = false;
        if evidence.len() > opts.max_evidence_items {
            evidence.truncate(opts.max_evidence_items);
            evidence_truncated = true;
            truncated_fields.insert("evidence".to_string());
        }

        let bounds = TraceBounds {
            max_evidence_items: opts.max_evidence_items,
            max_excerpt_bytes: opts.max_excerpt_bytes,
            max_capture_bytes: opts.max_capture_bytes,
            evidence_total,
            evidence_truncated,
            truncated_fields: truncated_fields.into_iter().collect(),
        };

        MatchTrace {
            pack_id,
            rule_id: detection.rule_id.clone(),
            extractor_id,
            matched_text: Some(matched_text_bounded),
            confidence: Some(detection.confidence),
            eligible,
            gates,
            evidence,
            bounds,
        }
    }

    fn build_match_trace_no_detection(
        _text: &str,
        _redactor: &Redactor,
        pack_id: String,
        rule_id: String,
        extractor_id: Option<String>,
        eligible: bool,
        gates: Vec<TraceGate>,
        anchor_evidence: Option<&TraceEvidence>,
        opts: &TraceOptions,
    ) -> MatchTrace {
        let mut truncated_fields: std::collections::BTreeSet<String> =
            std::collections::BTreeSet::new();

        let mut evidence: Vec<TraceEvidence> = Vec::new();
        if let Some(anchor) = anchor_evidence {
            if anchor.truncated {
                truncated_fields.insert("excerpt".to_string());
            }
            evidence.push(anchor.clone());
        }

        let evidence_total = evidence.len();
        let mut evidence_truncated = false;
        if evidence.len() > opts.max_evidence_items {
            evidence.truncate(opts.max_evidence_items);
            evidence_truncated = true;
            truncated_fields.insert("evidence".to_string());
        }

        let bounds = TraceBounds {
            max_evidence_items: opts.max_evidence_items,
            max_excerpt_bytes: opts.max_excerpt_bytes,
            max_capture_bytes: opts.max_capture_bytes,
            evidence_total,
            evidence_truncated,
            truncated_fields: truncated_fields.into_iter().collect(),
        };

        MatchTrace {
            pack_id,
            rule_id,
            extractor_id,
            matched_text: None,
            confidence: None,
            eligible,
            gates,
            evidence,
            bounds,
        }
    }

    fn slice_bytes(text: &str, start: usize, end: usize) -> Option<&str> {
        if start > end || end > text.len() {
            return None;
        }
        if !text.is_char_boundary(start) || !text.is_char_boundary(end) {
            return None;
        }
        Some(&text[start..end])
    }

    fn bound_utf8(s: &str, max_bytes: usize) -> (String, bool) {
        if s.len() <= max_bytes {
            return (s.to_string(), false);
        }
        let mut end = max_bytes.min(s.len());
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        (s[..end].to_string(), true)
    }

    /// Check if a detection's rule applies to the given agent type.
    ///
    /// Returns true if:
    /// - The detection's agent type matches the expected agent
    /// - The detection is a WezTerm rule (infrastructure rules apply to all)
    /// - The expected agent is Unknown (conservative fallback)
    #[must_use]
    fn rule_applies_to_agent(detection: &Detection, expected_agent: AgentType) -> bool {
        // WezTerm rules are infrastructure and apply to all agent types
        if detection.agent_type == AgentType::Wezterm {
            return true;
        }

        // Unknown agent type allows all rules (conservative fallback)
        if expected_agent == AgentType::Unknown {
            return true;
        }

        // Otherwise, rule must match the expected agent
        detection.agent_type == expected_agent
    }

    /// Quick reject check - returns false if text definitely has no matches
    #[must_use]
    pub fn quick_reject(&self, text: &str) -> bool {
        if !self.quick_reject_enabled {
            return true;
        }
        if text.is_empty() {
            return false;
        }
        let index = self.index();
        Self::quick_reject_with_index(index, text)
    }

    fn quick_reject_with_index(index: &EngineIndex, text: &str) -> bool {
        if text.is_empty() || index.quick_bytes.is_empty() {
            return false;
        }

        let bytes = text.as_bytes();
        let text_len = text.len();

        // Collect positions where anchor first-bytes appear.
        // This is O(n) but very fast with memchr's SIMD implementation.
        let mut byte_match_positions: Vec<usize> = Vec::new();
        for &byte in &index.quick_bytes {
            let mut pos = 0;
            while let Some(offset) = memchr(byte, &bytes[pos..]) {
                byte_match_positions.push(pos + offset);
                pos += offset + 1;
            }
        }

        if byte_match_positions.is_empty() {
            return false;
        }

        // If we have a Bloom filter, check substrings only at positions where
        // a first-byte matched. This is O(m*k) where m = match count, k = anchor lengths.
        if let Some(ref bloom) = index.bloom {
            for &start in &byte_match_positions {
                for &anchor_len in &index.anchor_lengths {
                    let end = start + anchor_len;
                    // Skip if this substring extends past the text
                    if end > text_len {
                        continue;
                    }
                    // Ensure we're at valid UTF-8 boundaries
                    if !text.is_char_boundary(start) || !text.is_char_boundary(end) {
                        continue;
                    }
                    let window = &text[start..end];
                    if bloom.check(window) {
                        // Bloom says "possibly present" - need full matching
                        return true;
                    }
                }
            }
            // Bloom filter rejected all candidate substrings - definitely no match
            return false;
        }

        // No Bloom filter available, but we found matching bytes
        true
    }

    /// Access the merged rule library
    #[must_use]
    pub fn rules(&self) -> &[RuleDef] {
        self.library.rules()
    }

    /// Access the loaded pattern packs.
    #[must_use]
    pub fn packs(&self) -> &[PatternPack] {
        self.library.packs()
    }

    /// Return the pack name that provides a given rule ID, if any.
    #[must_use]
    pub fn pack_for_rule(&self, rule_id: &str) -> Option<&str> {
        self.library.pack_for_rule(rule_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{PackOverride, PatternsConfig};
    use serde::Deserialize;
    use std::collections::{HashMap, HashSet};
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn engine_can_be_created() {
        let engine = PatternEngine::new();
        assert!(!engine.is_initialized());
        let _ = engine.detect("warmup");
        assert!(engine.is_initialized());
    }

    #[test]
    fn pack_for_rule_returns_pack_name() {
        let engine = PatternEngine::new();
        // Force initialization so rules are loaded
        let _ = engine.detect("warmup");
        // Built-in rules should have a pack name
        if let Some(first_rule) = engine.rules().first() {
            let pack = engine.pack_for_rule(&first_rule.id);
            assert!(pack.is_some(), "built-in rule should have a pack");
        }
        // Non-existent rule should return None
        assert!(engine.pack_for_rule("nonexistent.rule.id").is_none());
    }

    #[test]
    fn from_config_applies_pack_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let pack_path = dir.path().join("test_pack.yaml");
        let yaml = r#"
name: "test-pack"
version: "0.1.0"
rules:
  - id: "codex.test"
    agent_type: "codex"
    event_type: "usage.warning"
    severity: "info"
    anchors: ["hello"]
    regex: null
    description: "test rule"
"#;
        fs::write(&pack_path, yaml).unwrap();

        let mut config = PatternsConfig::default();
        let pack_id = format!("file:{}", pack_path.display());
        config.packs = vec![pack_id.clone()];
        config.pack_overrides.clear();

        let mut override_cfg = PackOverride::default();
        override_cfg
            .severity_overrides
            .insert("codex.test".to_string(), "critical".to_string());
        config.pack_overrides.insert(pack_id.clone(), override_cfg);

        let engine = PatternEngine::from_config(&config).unwrap();
        let rule = engine
            .rules()
            .iter()
            .find(|r| r.id == "codex.test")
            .unwrap();
        assert_eq!(rule.severity, Severity::Critical);

        let mut disabled = PackOverride::default();
        disabled.disabled_rules.push("codex.test".to_string());
        config.pack_overrides.insert(pack_id, disabled);

        let engine = PatternEngine::from_config(&config).unwrap();
        assert!(engine.rules().iter().all(|r| r.id != "codex.test"));
    }

    #[test]
    fn lazy_init_from_config_defers_compilation() {
        let config = PatternsConfig::default();
        let engine = PatternEngine::from_config(&config).unwrap();
        assert!(
            !engine.is_initialized(),
            "from_config should NOT compile eagerly"
        );
        let _ = engine.detect("trigger compilation");
        assert!(
            engine.is_initialized(),
            "detect should trigger lazy compilation"
        );
    }

    #[test]
    fn lazy_init_thread_safe() {
        use std::sync::Arc;
        use std::thread;

        let engine = Arc::new(PatternEngine::new());
        assert!(!engine.is_initialized());

        let mut handles = Vec::new();
        for _ in 0..4 {
            let eng = Arc::clone(&engine);
            handles.push(thread::spawn(move || {
                let _ = eng.detect("concurrent access");
            }));
        }
        for h in handles {
            h.join().expect("thread should not panic");
        }
        assert!(engine.is_initialized());
    }

    #[test]
    fn lazy_vs_eager_produce_same_results() {
        // Lazy path: new() then detect
        let lazy = PatternEngine::new();
        let lazy_results = lazy.detect("test content");

        // Eager path: with_packs compiles immediately
        let eager =
            PatternEngine::with_packs(builtin_packs()).expect("builtin packs should compile");
        assert!(eager.is_initialized(), "with_packs should compile eagerly");
        let eager_results = eager.detect("test content");

        assert_eq!(lazy_results.len(), eager_results.len());
    }

    #[test]
    fn with_packs_compiles_eagerly() {
        let engine =
            PatternEngine::with_packs(builtin_packs()).expect("builtin packs should compile");
        assert!(
            engine.is_initialized(),
            "with_packs should compile the index immediately"
        );
    }

    #[test]
    fn is_initialized_stays_true_after_multiple_detects() {
        let engine = PatternEngine::new();
        assert!(!engine.is_initialized());
        let _ = engine.detect("first");
        assert!(engine.is_initialized());
        let _ = engine.detect("second");
        assert!(engine.is_initialized());
        let _ = engine.detect("third");
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
            manual_fix: None,
            preview_command: None,
            learn_more_url: None,
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
            manual_fix: None,
            preview_command: None,
            learn_more_url: None,
        }
    }

    fn engine_with_rules(rules: Vec<RuleDef>) -> PatternEngine {
        let pack = PatternPack::new("pack", "0.1.0", rules);
        PatternEngine::with_packs(vec![pack]).expect("engine should build")
    }

    #[test]
    fn quick_reject_respects_anchor_bytes() {
        let engine = engine_with_rules(vec![rule_with_anchor("codex.quick", "XYZ", None)]);
        // Text without matching first byte is rejected
        assert!(!engine.quick_reject("abc"));
        // Text with matching first byte but no anchor match is now correctly rejected
        // thanks to the Bloom filter (previously this would return true)
        assert!(!engine.quick_reject("look: X-ray"));
        // Text containing the actual anchor passes
        assert!(engine.quick_reject("check XYZ value"));
    }

    #[test]
    fn bloom_filter_rejects_non_matching_text() {
        // Test that the Bloom filter correctly rejects text that has
        // matching first bytes but no matching anchors
        let engine = engine_with_rules(vec![
            rule_with_anchor("codex.bloom_a", "alpha", None),
            rule_with_anchor("codex.bloom_b", "beta", None),
            rule_with_anchor("codex.bloom_g", "gamma", None),
        ]);

        // These have matching first bytes but don't contain the anchors
        assert!(!engine.quick_reject("arbitrary text"));
        assert!(!engine.quick_reject("ax")); // 'a' matches but "ax" is shorter than "alpha"
        assert!(!engine.quick_reject("bx")); // 'b' matches but "bx" is shorter than "beta"
        assert!(!engine.quick_reject("gx")); // 'g' matches but "gx" is shorter than "gamma"
        assert!(!engine.quick_reject("alphx")); // close to "alpha" but different
        assert!(!engine.quick_reject("betx")); // close to "beta" but different
    }

    #[test]
    fn bloom_filter_accepts_matching_text() {
        let engine = engine_with_rules(vec![
            rule_with_anchor("codex.bloom_hello", "hello", None),
            rule_with_anchor("codex.bloom_world", "world", None),
        ]);

        // These contain the actual anchors
        assert!(engine.quick_reject("say hello to everyone"));
        assert!(engine.quick_reject("hello world"));
        assert!(engine.quick_reject("the world is vast"));

        // This has no matching anchors at all
        assert!(!engine.quick_reject("greetings and salutations"));
    }

    #[test]
    fn bloom_filter_handles_multiple_anchor_lengths() {
        let engine = engine_with_rules(vec![
            rule_with_anchor("codex.bloom_short", "ab", None),
            rule_with_anchor("codex.bloom_medium", "foobar", None),
            rule_with_anchor("codex.bloom_long", "this_is_a_long_anchor", None),
        ]);

        // Short anchor match
        assert!(engine.quick_reject("ab"));
        assert!(engine.quick_reject("xab"));
        assert!(engine.quick_reject("abx"));

        // Medium anchor match
        assert!(engine.quick_reject("foobar"));
        assert!(engine.quick_reject("xfoobar"));
        assert!(engine.quick_reject("foobarx"));

        // Long anchor match
        assert!(engine.quick_reject("this_is_a_long_anchor"));

        // No match (has matching first bytes 'a', 'f', 't' but not the full anchors)
        assert!(!engine.quick_reject("aX")); // starts with 'a' but "aX" != "ab"
        assert!(!engine.quick_reject("fooXXX")); // starts with 'f' but "fooXXX" != "foobar"
        assert!(!engine.quick_reject("this_is_not")); // starts with 't' but doesn't match long anchor
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
            ids.contains(&"gemini.usage.warning"),
            "Missing gemini.usage.warning"
        );
        assert!(
            ids.contains(&"gemini.usage.reached"),
            "Missing gemini.usage.reached"
        );
        assert!(
            ids.contains(&"gemini.session.summary"),
            "Missing gemini.session.summary"
        );
        assert!(
            ids.contains(&"gemini.session.resume_hint"),
            "Missing gemini.session.resume_hint"
        );
        assert!(
            ids.contains(&"gemini.model.used"),
            "Missing gemini.model.used"
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
    fn detect_gemini_usage_warning() {
        let engine = PatternEngine::new();
        let text = "Usage limit warning: 10% of your Pro models quota remaining.";
        let detections = engine.detect(text);
        let detection = detections
            .iter()
            .find(|d| d.rule_id == "gemini.usage.warning");
        assert!(detection.is_some(), "Should match gemini.usage.warning");
        let d = detection.unwrap();
        assert_eq!(d.severity, Severity::Warning);
        assert_eq!(
            d.extracted.get("remaining").and_then(|v| v.as_str()),
            Some("10")
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
    fn detect_gemini_session_resume_hint() {
        let engine = PatternEngine::new();
        let text =
            "To resume this session, run: gemini resume abcdef12-3456-7890-abcd-ef1234567890";
        let detections = engine.detect(text);
        let detection = detections
            .iter()
            .find(|d| d.rule_id == "gemini.session.resume_hint");
        assert!(
            detection.is_some(),
            "Should match gemini.session.resume_hint"
        );
        let d = detection.unwrap();
        assert_eq!(
            d.extracted.get("session_id").and_then(|v| v.as_str()),
            Some("abcdef12-3456-7890-abcd-ef1234567890")
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
    fn detect_gemini_model_used_alt() {
        let engine = PatternEngine::new();
        let text = "Using model: gemini-1.5-pro";
        let detections = engine.detect(text);
        let detection = detections.iter().find(|d| d.rule_id == "gemini.model.used");
        assert!(detection.is_some(), "Should match gemini.model.used");
        let d = detection.unwrap();
        assert_eq!(
            d.extracted.get("model").and_then(|v| v.as_str()),
            Some("gemini-1.5-pro")
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

    // ========== State Gating & Deduplication Tests ==========

    #[test]
    fn state_gating_filters_by_agent_type() {
        let engine = PatternEngine::new();

        // Codex usage limit text - use actual anchor from rule
        let codex_text = "You've hit your usage limit, try again at 3pm";

        // With Codex context - should detect
        let mut codex_ctx = DetectionContext::with_agent_type(AgentType::Codex);
        let detections = engine.detect_with_context(codex_text, &mut codex_ctx);
        assert!(
            detections
                .iter()
                .any(|d| d.rule_id == "codex.usage.reached"),
            "Should detect codex.usage.reached with Codex context"
        );

        // With Claude context - should NOT detect codex rules
        let mut claude_ctx = DetectionContext::with_agent_type(AgentType::ClaudeCode);
        let detections = engine.detect_with_context(codex_text, &mut claude_ctx);
        assert!(
            !detections
                .iter()
                .any(|d| d.rule_id == "codex.usage.reached"),
            "Should NOT detect codex.usage.reached with Claude context"
        );
    }

    #[test]
    fn state_gating_allows_wezterm_rules_for_all_agents() {
        let engine = PatternEngine::new();

        // WezTerm mux connection lost - use actual anchor from rule
        let wezterm_text = "mux server connection lost - please reconnect";

        // Should fire regardless of agent type
        for agent in [
            AgentType::Codex,
            AgentType::ClaudeCode,
            AgentType::Gemini,
            AgentType::Unknown,
        ] {
            let mut ctx = DetectionContext::with_agent_type(agent);
            let detections = engine.detect_with_context(wezterm_text, &mut ctx);
            assert!(
                detections
                    .iter()
                    .any(|d| d.rule_id == "wezterm.mux.connection_lost"),
                "WezTerm rules should fire for {agent:?}"
            );
        }
    }

    #[test]
    fn state_gating_unknown_agent_allows_all() {
        let engine = PatternEngine::new();

        // With Unknown agent type, all rules should pass through
        let codex_text = "You've hit your usage limit, try again at 3pm";
        let mut ctx = DetectionContext::with_agent_type(AgentType::Unknown);
        let detections = engine.detect_with_context(codex_text, &mut ctx);
        assert!(
            detections
                .iter()
                .any(|d| d.rule_id == "codex.usage.reached"),
            "Unknown agent should allow all rules"
        );
    }

    #[test]
    fn dedup_prevents_repeated_detections() {
        let engine = PatternEngine::new();
        // Include trailing period to prevent greedy regex from matching across
        // concatenated text boundaries (the period stops `[^.]+` in the regex)
        let text = "You've hit your usage limit, try again at 3pm.";

        let mut ctx = DetectionContext::new();

        // First detection should succeed
        let first = engine.detect_with_context(text, &mut ctx);
        assert!(!first.is_empty(), "First detection should find matches");

        // Second detection with same text should be empty (deduped)
        let second = engine.detect_with_context(text, &mut ctx);
        assert!(
            second.is_empty(),
            "Repeated detection should be deduped, but got {second:?}"
        );
    }

    #[test]
    fn dedup_allows_different_extracted_values() {
        let engine = PatternEngine::new();

        let mut ctx = DetectionContext::new();

        // First detection with one token count (matches codex.session.token_usage regex)
        let text1 = "Token usage: total=94212 input=2115 (+ 1000 cached) output=1000";
        let first = engine.detect_with_context(text1, &mut ctx);
        assert!(!first.is_empty(), "First detection should succeed");

        // Second detection with different token count should NOT be deduped
        let text2 = "Token usage: total=100000 input=3000 (+ 2000 cached) output=5000";
        let second = engine.detect_with_context(text2, &mut ctx);
        assert!(
            !second.is_empty(),
            "Different extracted values should not be deduped"
        );
    }

    #[test]
    fn dedup_clear_seen_resets() {
        let engine = PatternEngine::new();
        let text = "You've hit your usage limit, try again at 3pm.";

        let mut ctx = DetectionContext::new();

        // First detection
        let first = engine.detect_with_context(text, &mut ctx);
        assert!(!first.is_empty());

        // Clear seen state
        ctx.clear_seen();
        assert_eq!(ctx.seen_count(), 0);

        // Should detect again after clear
        let after_clear = engine.detect_with_context(text, &mut ctx);
        assert!(!after_clear.is_empty(), "Should detect after clearing seen");
    }

    #[test]
    fn detection_dedup_key_includes_extracted() {
        let d1 = Detection {
            rule_id: "test.rule".into(),
            agent_type: AgentType::Codex,
            event_type: "usage".into(),
            severity: Severity::Info,
            confidence: 0.9,
            extracted: serde_json::json!({"tokens": "1000"}),
            matched_text: "test".into(),
            span: (0, 0),
        };

        let d2 = Detection {
            rule_id: "test.rule".into(),
            agent_type: AgentType::Codex,
            event_type: "usage".into(),
            severity: Severity::Info,
            confidence: 0.9,
            extracted: serde_json::json!({"tokens": "2000"}),
            matched_text: "test".into(),
            span: (0, 0),
        };

        assert_ne!(
            d1.dedup_key(),
            d2.dedup_key(),
            "Different extracted values should produce different dedup keys"
        );

        // Same rule with same extracted should match
        let d3 = Detection {
            rule_id: "test.rule".into(),
            agent_type: AgentType::Codex,
            event_type: "usage".into(),
            severity: Severity::Info,
            confidence: 0.9,
            extracted: serde_json::json!({"tokens": "1000"}),
            matched_text: "different text".into(),
            span: (0, 0),
        };

        assert_eq!(
            d1.dedup_key(),
            d3.dedup_key(),
            "Same rule+extracted should produce same dedup key"
        );
    }

    #[test]
    fn context_with_pane_works() {
        let ctx = DetectionContext::with_pane(42, Some(AgentType::ClaudeCode));
        assert_eq!(ctx.pane_id, Some(42));
        assert_eq!(ctx.agent_type, Some(AgentType::ClaudeCode));
        assert_eq!(ctx.seen_count(), 0);
    }

    #[test]
    fn rule_applies_to_agent_logic() {
        // Create a mock detection for Codex
        let codex_detection = Detection {
            rule_id: "codex.usage_reached".into(),
            agent_type: AgentType::Codex,
            event_type: "usage".into(),
            severity: Severity::Warning,
            confidence: 0.9,
            extracted: serde_json::Value::Null,
            matched_text: "test".into(),
            span: (0, 0),
        };

        // Codex rule should apply to Codex agent
        assert!(PatternEngine::rule_applies_to_agent(
            &codex_detection,
            AgentType::Codex
        ));

        // Codex rule should NOT apply to Claude agent
        assert!(!PatternEngine::rule_applies_to_agent(
            &codex_detection,
            AgentType::ClaudeCode
        ));

        // Codex rule should apply to Unknown agent (fallback)
        assert!(PatternEngine::rule_applies_to_agent(
            &codex_detection,
            AgentType::Unknown
        ));

        // WezTerm rules should apply to all agents
        let wezterm_detection = Detection {
            rule_id: "wezterm.pane_exited".into(),
            agent_type: AgentType::Wezterm,
            event_type: "lifecycle".into(),
            severity: Severity::Info,
            confidence: 0.9,
            extracted: serde_json::Value::Null,
            matched_text: "test".into(),
            span: (0, 0),
        };

        assert!(PatternEngine::rule_applies_to_agent(
            &wezterm_detection,
            AgentType::Codex
        ));
        assert!(PatternEngine::rule_applies_to_agent(
            &wezterm_detection,
            AgentType::ClaudeCode
        ));
        assert!(PatternEngine::rule_applies_to_agent(
            &wezterm_detection,
            AgentType::Gemini
        ));
    }

    #[test]
    fn state_gating_prevents_false_positives_in_non_agent_panes() {
        let engine = PatternEngine::new();

        // Text that might appear in a non-agent pane (e.g., log file output)
        // but contains Codex-specific keywords
        let log_text = "Usage limit reached for all Pro models";

        // With Gemini context (wrong agent) - should NOT detect Codex rules
        let mut ctx = DetectionContext::with_pane(1, Some(AgentType::Gemini));
        let detections = engine.detect_with_context(log_text, &mut ctx);

        // Should not contain Codex-specific detections
        for d in &detections {
            assert!(
                d.agent_type != AgentType::Codex,
                "Codex rule {} should not fire in Gemini pane",
                d.rule_id
            );
        }
    }

    #[test]
    fn detect_with_context_and_trace_includes_pack_id_for_emitted_detection() {
        let engine = engine_with_rules(vec![rule_with_anchor("codex.anchor", "hello", None)]);
        let mut ctx = DetectionContext::new();
        let opts = TraceOptions::default();

        let (detections, traces) =
            engine.detect_with_context_and_trace("say hello to the world", &mut ctx, &opts);

        assert_eq!(detections.len(), 1);
        assert_eq!(traces.len(), 1);
        assert_eq!(traces[0].pack_id, "pack");
        assert_eq!(traces[0].rule_id, "codex.anchor");
        assert!(traces[0].eligible);
    }

    #[test]
    fn detect_with_context_and_trace_emits_regex_miss_trace_when_enabled() {
        let engine = engine_with_rules(vec![rule_with_anchor(
            "codex.regex",
            "limit",
            Some(r"limit (?P<value>\d+)"),
        )]);

        let mut ctx = DetectionContext::new();
        let mut opts = TraceOptions::default();
        opts.include_non_matches = true;

        let (detections, traces) =
            engine.detect_with_context_and_trace("limit xx", &mut ctx, &opts);

        assert!(detections.is_empty());
        assert_eq!(traces.len(), 1);
        assert_eq!(traces[0].pack_id, "pack");
        assert_eq!(traces[0].rule_id, "codex.regex");
        assert!(!traces[0].eligible);

        let match_gate = traces[0]
            .gates
            .iter()
            .find(|g| g.gate == "match")
            .expect("match gate should exist");
        assert!(!match_gate.passed);
    }

    #[test]
    fn detect_with_context_and_trace_reports_dedupe_gate_failure_on_second_run() {
        let engine = engine_with_rules(vec![rule_with_anchor("codex.anchor", "hello", None)]);

        let mut ctx = DetectionContext::new();
        let mut opts = TraceOptions::default();
        opts.include_non_matches = true;

        let (detections1, traces1) = engine.detect_with_context_and_trace("hello", &mut ctx, &opts);
        assert_eq!(detections1.len(), 1);
        assert_eq!(traces1.len(), 1);
        assert!(traces1[0].eligible);

        let (detections2, traces2) = engine.detect_with_context_and_trace("hello", &mut ctx, &opts);
        assert!(detections2.is_empty());
        assert_eq!(traces2.len(), 1);
        assert!(!traces2[0].eligible);

        let dedupe_gate = traces2[0]
            .gates
            .iter()
            .find(|g| g.gate == "dedupe")
            .expect("dedupe gate should exist");
        assert!(!dedupe_gate.passed);
    }

    #[test]
    fn detect_with_context_and_trace_redacts_matched_text_and_captures() {
        let p1 = "sk";
        let p2 = "-";
        let p3 = "abc123";
        let p4 = "456789012345678901234567890123456789012345678901";
        let secret = format!("{p1}{p2}{p3}{p4}");

        // Ensure the secret is detectable by the redactor before asserting on output.
        let redactor = Redactor::new();
        assert!(redactor.contains_secrets(&secret));

        let engine = engine_with_rules(vec![rule_with_anchor(
            "codex.secret",
            "Key:",
            Some(r"Key: (?P<key>.+)"),
        )]);

        let mut ctx = DetectionContext::new();
        let mut opts = TraceOptions::default();
        opts.include_non_matches = true;
        opts.max_excerpt_bytes = 1024;
        opts.max_capture_bytes = 1024;

        let text = format!("Key: {secret}");
        let (_detections, traces) = engine.detect_with_context_and_trace(&text, &mut ctx, &opts);

        assert_eq!(traces.len(), 1);
        let trace = &traces[0];
        let mt = trace
            .matched_text
            .as_ref()
            .expect("matched_text should exist");
        assert!(!mt.contains(&secret), "matched_text must not leak secrets");
        assert!(mt.contains("[REDACTED]"), "expected redaction marker");

        let capture = trace
            .evidence
            .iter()
            .find(|ev| ev.kind == "capture" && ev.label.as_deref() == Some("key"))
            .expect("capture evidence should exist");
        let cap_excerpt = capture
            .excerpt
            .as_ref()
            .expect("capture excerpt should exist");
        assert!(
            !cap_excerpt.contains(&secret),
            "capture excerpt must not leak secrets"
        );
        assert!(
            cap_excerpt.contains("[REDACTED]"),
            "expected redaction marker in capture"
        );
    }

    // ========== User Pattern Pack Tests ==========

    #[test]
    fn user_pack_toml_loads_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let pack_path = dir.path().join("my-rules.toml");
        fs::write(
            &pack_path,
            r#"
name = "user:my-rules"
version = "1.0.0"

[[rules]]
id = "myorg.custom_alert"
agent_type = "codex"
event_type = "custom.alert"
severity = "warning"
anchors = ["[MY-ORG] Alert:"]
description = "Custom org alert"
"#,
        )
        .unwrap();

        let pack = load_pack_from_file(pack_path.to_str().unwrap(), Some(dir.path())).unwrap();
        assert_eq!(pack.rules.len(), 1);
        assert_eq!(pack.rules[0].id, "myorg.custom_alert");
    }

    #[test]
    fn user_pack_validates_with_custom_prefix() {
        let pack = PatternPack::new(
            "user:test",
            "1.0.0",
            vec![RuleDef {
                id: "myorg.custom_rule".to_string(),
                agent_type: AgentType::Codex,
                event_type: "custom.event".to_string(),
                severity: Severity::Warning,
                anchors: vec!["custom anchor".to_string()],
                regex: None,
                description: "Custom rule".to_string(),
                remediation: None,
                workflow: None,
                manual_fix: None,
                preview_command: None,
                learn_more_url: None,
            }],
        );
        assert!(pack.validate().is_err());
        assert!(pack.validate_as_user_pack().is_ok());
    }

    #[test]
    fn user_pack_rejects_rule_without_dot() {
        let pack = PatternPack::new(
            "user:bad",
            "1.0.0",
            vec![RuleDef {
                id: "noruleid".to_string(),
                agent_type: AgentType::Codex,
                event_type: "custom.event".to_string(),
                severity: Severity::Warning,
                anchors: vec!["anchor".to_string()],
                regex: None,
                description: "Bad rule".to_string(),
                remediation: None,
                workflow: None,
                manual_fix: None,
                preview_command: None,
                learn_more_url: None,
            }],
        );
        assert!(pack.validate_as_user_pack().is_err());
    }

    #[test]
    fn discover_packs_finds_toml_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("alerts.toml"),
            r#"
name = "user:alerts"
version = "1.0.0"
[[rules]]
id = "myorg.alert"
agent_type = "codex"
event_type = "custom.alert"
severity = "warning"
anchors = ["[ALERT]"]
description = "Alert rule"
"#,
        )
        .unwrap();

        let sub = dir.path().join("monitoring");
        fs::create_dir(&sub).unwrap();
        fs::write(
            sub.join("rules.toml"),
            r#"
name = "user:monitoring"
version = "2.0.0"
[[rules]]
id = "monitor.health_check"
agent_type = "codex"
event_type = "health.check"
severity = "info"
anchors = ["Health check"]
description = "Health check rule"
"#,
        )
        .unwrap();

        fs::write(dir.path().join("readme.txt"), "not a pack").unwrap();

        let packs = discover_packs_from_dir(dir.path()).unwrap();
        assert_eq!(packs.len(), 2);
        let names: Vec<&str> = packs.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"user:alerts"));
        assert!(names.contains(&"user:monitoring"));
    }

    #[test]
    fn discover_packs_nonexistent_dir_returns_empty() {
        let packs = discover_packs_from_dir(Path::new("/nonexistent/path")).unwrap();
        assert!(packs.is_empty());
    }

    #[test]
    fn discover_packs_skips_invalid() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("broken.toml"), "this is not valid TOML{{{").unwrap();
        let packs = discover_packs_from_dir(dir.path()).unwrap();
        assert!(packs.is_empty());
    }

    #[test]
    fn user_pack_rules_detected_by_engine() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("custom.toml"),
            r#"
name = "user:custom"
version = "1.0.0"
[[rules]]
id = "myorg.deploy_alert"
agent_type = "codex"
event_type = "deploy.alert"
severity = "critical"
anchors = ["[DEPLOY-ALERT]"]
description = "Deployment alert"
"#,
        )
        .unwrap();

        let config = PatternsConfig {
            user_packs_enabled: true,
            user_packs_dir: Some(dir.path().to_str().unwrap().to_string()),
            ..PatternsConfig::default()
        };
        let engine = PatternEngine::from_config(&config).unwrap();
        let detections = engine.detect("[DEPLOY-ALERT] Production deployment failed");
        assert_eq!(detections.len(), 1);
        assert_eq!(detections[0].rule_id, "myorg.deploy_alert");
    }

    #[test]
    fn user_packs_disabled_skips_discovery() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("custom.toml"),
            r#"
name = "user:custom"
version = "1.0.0"
[[rules]]
id = "myorg.deploy_alert"
agent_type = "codex"
event_type = "deploy.alert"
severity = "critical"
anchors = ["[DEPLOY-ALERT]"]
description = "Deployment alert"
"#,
        )
        .unwrap();

        let config = PatternsConfig {
            user_packs_enabled: false,
            user_packs_dir: Some(dir.path().to_str().unwrap().to_string()),
            ..PatternsConfig::default()
        };
        let engine = PatternEngine::from_config(&config).unwrap();
        let detections = engine.detect("[DEPLOY-ALERT] Production deployment failed");
        assert!(detections.is_empty());
    }

    #[test]
    fn user_pack_overrides_builtin_rule() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("override.toml"),
            r#"
name = "user:override"
version = "1.0.0"
[[rules]]
id = "codex.usage.reached"
agent_type = "codex"
event_type = "usage.reached"
severity = "info"
anchors = ["custom usage reached"]
description = "Overridden usage reached"
"#,
        )
        .unwrap();

        let config = PatternsConfig {
            user_packs_enabled: true,
            user_packs_dir: Some(dir.path().to_str().unwrap().to_string()),
            ..PatternsConfig::default()
        };
        let engine = PatternEngine::from_config(&config).unwrap();
        let detections = engine.detect("custom usage reached alert");
        assert_eq!(detections.len(), 1);
        assert_eq!(detections[0].rule_id, "codex.usage.reached");
    }

    #[test]
    fn workspace_local_packs_loaded() {
        let root = tempfile::tempdir().unwrap();
        let ws_dir = root.path().join(".wa").join("patterns");
        fs::create_dir_all(&ws_dir).unwrap();
        fs::write(
            ws_dir.join("local.toml"),
            r#"
name = "user:local"
version = "1.0.0"
[[rules]]
id = "project.lint_warning"
agent_type = "codex"
event_type = "lint.warning"
severity = "info"
anchors = ["[LINT-WARN]"]
description = "Project lint warning"
"#,
        )
        .unwrap();

        let config = PatternsConfig {
            user_packs_enabled: false,
            user_packs_dir: None,
            ..PatternsConfig::default()
        };
        let engine = PatternEngine::from_config_with_root(&config, Some(root.path())).unwrap();
        let detections = engine.detect("[LINT-WARN] unused variable");
        assert_eq!(detections.len(), 1);
        assert_eq!(detections[0].rule_id, "project.lint_warning");
    }
}
