//! Configuration management for wa
//!
//! Handles loading and validation of wa.toml configuration files.
//!
//! # Schema Overview
//!
//! The configuration is structured into sections:
//! - `general`: Log level, workspace/data directory
//! - `ingest`: Poll interval, concurrency, gap detection
//! - `storage`: DB path, retention, flush intervals
//! - `backup`: Scheduled backup configuration
//! - `patterns`: Enabled packs, per-pack overrides
//! - `workflows`: Enable/disable, allowlist/denylist, concurrency
//! - `safety`: Capability gates, rate limits, approval, redaction, reservations
//! - `metrics`: Enable, bind address
//!
//! # Forward Compatibility
//!
//! All sections use `#[serde(default)]` to allow missing fields.
//! Unknown fields are ignored to support forward compatibility.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

// =============================================================================
// Main Config
// =============================================================================

/// Main configuration structure for wa
///
/// This struct represents the complete wa.toml configuration file.
/// All sections are optional with sensible defaults.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    /// General settings (log level, data directory)
    pub general: GeneralConfig,

    /// Ingest settings (polling, gap detection)
    pub ingest: IngestConfig,

    /// Storage settings (database, retention)
    pub storage: StorageConfig,

    /// Backup settings (scheduled backups)
    pub backup: BackupConfig,

    /// Pattern detection settings
    pub patterns: PatternsConfig,

    /// Workflow execution settings
    pub workflows: WorkflowsConfig,

    /// Safety and policy settings
    pub safety: SafetyConfig,

    /// Vendored WezTerm settings
    pub vendored: VendoredConfig,

    /// Metrics/telemetry settings
    pub metrics: MetricsConfig,

    /// Notification filtering and throttling settings
    pub notifications: NotificationConfig,
}

// =============================================================================
// General Config
// =============================================================================

/// Log format options
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// Human-readable pretty format (default for interactive use)
    #[default]
    Pretty,
    /// Machine-parseable JSON lines (for CI/E2E/ops)
    Json,
}

impl std::fmt::Display for LogFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pretty => write!(f, "pretty"),
            Self::Json => write!(f, "json"),
        }
    }
}

impl std::str::FromStr for LogFormat {
    type Err = crate::error::ConfigError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "pretty" => Ok(Self::Pretty),
            "json" => Ok(Self::Json),
            other => Err(crate::error::ConfigError::ParseError(format!(
                "invalid log format: {other} (expected 'pretty' or 'json')"
            ))),
        }
    }
}

/// General configuration settings
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralConfig {
    /// Log level: trace, debug, info, warn, error
    pub log_level: String,

    /// Log format: pretty (human-readable) or json (machine-parseable)
    pub log_format: LogFormat,

    /// Optional log file path (supports ~ expansion)
    /// When set, logs are written to this file in addition to stderr
    pub log_file: Option<String>,

    /// Data directory path (supports ~ expansion)
    /// Default: ~/.local/share/wa (Linux) or ~/Library/Application Support/wa (macOS)
    pub data_dir: String,

    /// Workspace identifier (optional, for multi-workspace setups)
    pub workspace: Option<String>,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            log_level: "info".to_string(),
            log_format: LogFormat::default(),
            log_file: None,
            data_dir: default_data_dir(),
            workspace: None,
        }
    }
}

fn default_data_dir() -> String {
    // XDG on Linux, ~/Library/Application Support on macOS
    #[cfg(target_os = "macos")]
    {
        "~/Library/Application Support/wa".to_string()
    }
    #[cfg(not(target_os = "macos"))]
    {
        "~/.local/share/wa".to_string()
    }
}

// =============================================================================
// Ingest Config
// =============================================================================

/// Ingest pipeline configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct IngestConfig {
    /// Base poll interval in milliseconds
    /// Used when panes are idle; adaptive polling may reduce this when active
    pub poll_interval_ms: u64,

    /// Minimum poll interval when active (adaptive polling lower bound)
    pub min_poll_interval_ms: u64,

    /// Maximum concurrent pane captures
    pub max_concurrent_captures: u32,

    /// Backpressure threshold: pause ingest if storage queue exceeds this
    pub backpressure_threshold: u32,

    /// Enable gap detection (explicit discontinuity tracking)
    pub gap_detection: bool,

    /// Gap detection threshold: if captured text changes by more than this
    /// percentage without overlap, record a gap
    pub gap_detection_threshold_percent: u32,

    /// Maximum segment size in bytes before forced split
    pub max_segment_bytes: u32,

    /// Pane filtering rules (include/exclude)
    pub panes: PaneFilterConfig,
}

impl Default for IngestConfig {
    fn default() -> Self {
        Self {
            poll_interval_ms: 200,
            min_poll_interval_ms: 50,
            max_concurrent_captures: 10,
            backpressure_threshold: 1000,
            gap_detection: true,
            gap_detection_threshold_percent: 50,
            max_segment_bytes: 65536, // 64KB
            panes: PaneFilterConfig::default(),
        }
    }
}

// =============================================================================
// Pane Filter Config
// =============================================================================

/// Pane filtering configuration for controlling which panes are observed
///
/// Precedence rules:
/// - Exclude rules are checked first and always win
/// - If include rules are empty, all panes are included by default
/// - If include rules are specified, only matching panes are included
/// - A pane must match at least one include rule (if any) AND not match any exclude rule
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PaneFilterConfig {
    /// Include rules: panes matching ANY of these are included (if list is non-empty)
    /// If empty, all panes are included by default (subject to exclude rules)
    pub include: Vec<PaneFilterRule>,

    /// Exclude rules: panes matching ANY of these are excluded
    /// Exclude rules always win over include rules
    pub exclude: Vec<PaneFilterRule>,
}

impl PaneFilterConfig {
    /// Check if a pane should be observed based on the filter rules
    ///
    /// Returns `Some(rule_id)` if the pane is excluded (with the matching rule ID),
    /// or `None` if the pane should be observed.
    #[must_use]
    pub fn check_pane(&self, domain: &str, title: &str, cwd: &str) -> Option<String> {
        // Check exclude rules first (exclude always wins)
        for rule in &self.exclude {
            if rule.matches(domain, title, cwd) {
                return Some(rule.id.clone());
            }
        }

        // If include rules are specified, pane must match at least one
        if !self.include.is_empty() {
            let matches_include = self.include.iter().any(|r| r.matches(domain, title, cwd));
            if !matches_include {
                return Some("no_include_match".to_string());
            }
        }

        // Pane should be observed
        None
    }

    /// Check if there are any active filter rules
    #[must_use]
    pub fn has_rules(&self) -> bool {
        !self.include.is_empty() || !self.exclude.is_empty()
    }
}

/// A single pane filter rule with optional matchers for domain, title, and cwd
///
/// All specified matchers must match for the rule to apply (AND logic).
/// Use separate rules for OR logic (multiple rules in include/exclude lists).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PaneFilterRule {
    /// Unique identifier for this rule (shown in status output)
    pub id: String,

    /// Match on domain name (exact match or glob pattern)
    /// Examples: "local", "SSH:*", "unix:*"
    pub domain: Option<String>,

    /// Match on pane title (substring or regex pattern)
    /// If starts with "re:" uses regex matching, otherwise substring match
    /// Examples: "vim", "re:^bash.*$"
    pub title: Option<String>,

    /// Match on current working directory (path prefix or glob)
    /// Examples: "/home/user/private", "/tmp/*"
    pub cwd: Option<String>,
}

impl Default for PaneFilterRule {
    fn default() -> Self {
        Self {
            id: "unnamed_rule".to_string(),
            domain: None,
            title: None,
            cwd: None,
        }
    }
}

impl PaneFilterRule {
    /// Create a new rule with the given ID
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            domain: None,
            title: None,
            cwd: None,
        }
    }

    /// Set the domain matcher
    #[must_use]
    pub fn with_domain(mut self, domain: impl Into<String>) -> Self {
        self.domain = Some(domain.into());
        self
    }

    /// Set the title matcher
    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Set the cwd matcher
    #[must_use]
    pub fn with_cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Check if this rule matches the given pane properties
    ///
    /// All specified matchers must match (AND logic).
    /// If no matchers are specified, the rule matches nothing.
    #[must_use]
    pub fn matches(&self, domain: &str, title: &str, cwd: &str) -> bool {
        // Rule must have at least one matcher
        if self.domain.is_none() && self.title.is_none() && self.cwd.is_none() {
            return false;
        }

        // All specified matchers must match (AND logic)
        let domain_matches = self
            .domain
            .as_ref()
            .is_none_or(|p| Self::match_glob(p, domain));
        let title_matches = self
            .title
            .as_ref()
            .is_none_or(|p| Self::match_title(p, title));
        let cwd_matches = self.cwd.as_ref().is_none_or(|p| Self::match_glob(p, cwd));

        domain_matches && title_matches && cwd_matches
    }

    /// Match using glob-style patterns (* for any, ? for single char)
    fn match_glob(pattern: &str, value: &str) -> bool {
        // Simple glob matching: * matches any sequence, ? matches any single char
        if !pattern.contains('*') && !pattern.contains('?') {
            // Exact match or prefix match for paths
            return value == pattern || value.starts_with(&format!("{pattern}/"));
        }

        // Convert glob to regex-ish matching
        let mut regex_pattern = String::from("^");
        for ch in pattern.chars() {
            match ch {
                '*' => regex_pattern.push_str(".*"),
                '?' => regex_pattern.push('.'),
                '.' | '+' | '^' | '$' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '\\' => {
                    regex_pattern.push('\\');
                    regex_pattern.push(ch);
                }
                _ => regex_pattern.push(ch),
            }
        }
        regex_pattern.push('$');

        fancy_regex::Regex::new(&regex_pattern).is_ok_and(|re| re.is_match(value).unwrap_or(false))
    }

    /// Match title using substring or regex
    fn match_title(pattern: &str, title: &str) -> bool {
        pattern.strip_prefix("re:").map_or_else(
            || title.to_lowercase().contains(&pattern.to_lowercase()),
            |regex_pat| {
                fancy_regex::Regex::new(regex_pat)
                    .is_ok_and(|re| re.is_match(title).unwrap_or(false))
            },
        )
    }

    /// Validate that this rule has at least one matcher and all patterns are valid
    pub fn validate(&self) -> Result<(), String> {
        if self.id.is_empty() {
            return Err("Rule ID cannot be empty".to_string());
        }

        if self.domain.is_none() && self.title.is_none() && self.cwd.is_none() {
            return Err(format!("Rule '{}' has no matchers", self.id));
        }

        // Validate regex patterns
        if let Some(ref title) = self.title {
            if let Some(regex_pat) = title.strip_prefix("re:") {
                if fancy_regex::Regex::new(regex_pat).is_err() {
                    return Err(format!(
                        "Rule '{}' has invalid title regex: {}",
                        self.id, regex_pat
                    ));
                }
            }
        }

        Ok(())
    }
}

// =============================================================================
// Storage Config
// =============================================================================

/// Storage configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    /// Database file path (relative to workspace .wa dir if not absolute)
    pub db_path: String,

    /// Retention period in days (0 = no retention, keep forever)
    pub retention_days: u32,

    /// Size-based retention in megabytes (0 = no size limit)
    pub retention_max_mb: u32,

    /// Checkpoint/flush interval in seconds
    pub checkpoint_interval_secs: u32,

    /// Writer queue size (bounded for backpressure)
    pub writer_queue_size: u32,

    /// Read pool size (concurrent read connections)
    pub read_pool_size: u32,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            db_path: "wa.db".to_string(),
            retention_days: 30,
            retention_max_mb: 0, // No size limit by default
            checkpoint_interval_secs: 60,
            writer_queue_size: 10000,
            read_pool_size: 4,
        }
    }
}

// =============================================================================
// Backup Config
// =============================================================================

/// Backup configuration
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BackupConfig {
    /// Scheduled backups configuration
    pub scheduled: ScheduledBackupConfig,
}

impl Default for BackupConfig {
    fn default() -> Self {
        Self {
            scheduled: ScheduledBackupConfig::default(),
        }
    }
}

/// Scheduled backup configuration
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScheduledBackupConfig {
    /// Enable scheduled backups
    pub enabled: bool,
    /// Schedule string (hourly, daily, weekly, or 5-field cron)
    pub schedule: String,
    /// Retention period in days (0 = keep forever)
    pub retention_days: u32,
    /// Maximum backups to retain (0 = unlimited)
    pub max_backups: u32,
    /// Destination root directory for backups (optional)
    pub destination: Option<String>,
    /// Enable compression (if supported)
    pub compress: bool,
    /// Metadata-only mode (skip expensive verification)
    pub metadata_only: bool,
    /// Notify on failures
    pub notify_on_failure: bool,
    /// Notify on successes
    pub notify_on_success: bool,
}

impl Default for ScheduledBackupConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            schedule: "daily".to_string(),
            retention_days: 30,
            max_backups: 10,
            destination: None,
            compress: false,
            metadata_only: false,
            notify_on_failure: true,
            notify_on_success: false,
        }
    }
}

// =============================================================================
// Patterns Config
// =============================================================================

/// Pattern detection configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PatternsConfig {
    /// Enabled pattern packs (order matters for overrides)
    /// Format: "builtin:<name>" or "file:<path>"
    pub packs: Vec<String>,

    /// Per-pack configuration overrides
    /// Key: pack name, Value: pack-specific settings
    pub pack_overrides: HashMap<String, PackOverride>,

    /// Enable quick-reject optimization (memchr-based pre-filtering)
    pub quick_reject_enabled: bool,
}

impl Default for PatternsConfig {
    fn default() -> Self {
        Self {
            packs: vec![
                "builtin:core".to_string(),
                "builtin:codex".to_string(),
                "builtin:claude_code".to_string(),
                "builtin:gemini".to_string(),
            ],
            pack_overrides: HashMap::new(),
            quick_reject_enabled: true,
        }
    }
}

/// Per-pack configuration override
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PackOverride {
    /// Disable specific rules by ID
    pub disabled_rules: Vec<String>,

    /// Override severity for specific rules
    pub severity_overrides: HashMap<String, String>,

    /// Additional pack-specific settings (extensible)
    pub extra: HashMap<String, toml::Value>,
}

// =============================================================================
// Compaction Prompt Config
// =============================================================================

/// Default prompt for Claude Code agents after compaction.
pub const DEFAULT_COMPACTION_PROMPT_CLAUDE_CODE: &str =
    "Reread AGENTS.md so it's still fresh in your mind.\n";

/// Default prompt for Codex CLI agents after compaction.
pub const DEFAULT_COMPACTION_PROMPT_CODEX: &str =
    "Please re-read AGENTS.md and any key project context files.\n";

/// Default prompt for Gemini CLI agents after compaction.
pub const DEFAULT_COMPACTION_PROMPT_GEMINI: &str =
    "Please re-examine AGENTS.md and project context.\n";

/// Default prompt for unknown agents after compaction.
pub const DEFAULT_COMPACTION_PROMPT_UNKNOWN: &str =
    "Please review the project context files (AGENTS.md, README.md).\n";

const COMPACTION_PROMPT_TOKENS: [&str; 5] = [
    "agent_type",
    "pane_id",
    "pane_domain",
    "pane_title",
    "pane_cwd",
];

/// Per-project/pane-matching prompt override.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CompactionPromptOverride {
    /// Pane-matching rule for selecting this override
    #[serde(flatten)]
    pub rule: PaneFilterRule,

    /// Prompt template to use when the rule matches
    pub prompt: String,
}

impl Default for CompactionPromptOverride {
    fn default() -> Self {
        Self {
            rule: PaneFilterRule::default(),
            prompt: String::new(),
        }
    }
}

impl CompactionPromptOverride {
    /// Validate the override rule and prompt.
    pub fn validate(&self) -> Result<(), String> {
        self.rule
            .validate()
            .map_err(|e| format!("compaction_prompts override invalid: {e}"))?;
        if self.prompt.trim().is_empty() {
            return Err(format!(
                "compaction_prompts override '{}' has empty prompt",
                self.rule.id
            ));
        }
        validate_compaction_prompt_template(&self.prompt)?;
        Ok(())
    }
}

/// Prompt templates for the handle_compaction workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CompactionPromptConfig {
    /// Global default prompt template.
    pub default: String,

    /// Maximum total prompt length (characters).
    pub max_prompt_len: u32,

    /// Maximum length of any embedded snippet value.
    pub max_snippet_len: u32,

    /// Per-agent prompt overrides (keys: codex, claude_code, gemini, unknown).
    pub by_agent: HashMap<String, String>,

    /// Per-pane prompt overrides (keyed by pane_id).
    pub by_pane: HashMap<u64, String>,

    /// Per-project/path prompt overrides (first match wins).
    pub by_project: Vec<CompactionPromptOverride>,
}

impl Default for CompactionPromptConfig {
    fn default() -> Self {
        let mut by_agent = HashMap::new();
        by_agent.insert(
            "claude_code".to_string(),
            DEFAULT_COMPACTION_PROMPT_CLAUDE_CODE.to_string(),
        );
        by_agent.insert(
            "codex".to_string(),
            DEFAULT_COMPACTION_PROMPT_CODEX.to_string(),
        );
        by_agent.insert(
            "gemini".to_string(),
            DEFAULT_COMPACTION_PROMPT_GEMINI.to_string(),
        );
        by_agent.insert(
            "unknown".to_string(),
            DEFAULT_COMPACTION_PROMPT_UNKNOWN.to_string(),
        );

        Self {
            default: DEFAULT_COMPACTION_PROMPT_UNKNOWN.to_string(),
            max_prompt_len: 2000,
            max_snippet_len: 400,
            by_agent,
            by_pane: HashMap::new(),
            by_project: Vec::new(),
        }
    }
}

impl CompactionPromptConfig {
    /// Validate compaction prompt configuration.
    pub fn validate(&self) -> Result<(), String> {
        if self.max_prompt_len == 0 {
            return Err("workflows.compaction_prompts.max_prompt_len must be >= 1".to_string());
        }
        if self.max_snippet_len == 0 {
            return Err("workflows.compaction_prompts.max_snippet_len must be >= 1".to_string());
        }
        if self.default.trim().is_empty() {
            return Err("workflows.compaction_prompts.default must not be empty".to_string());
        }
        validate_compaction_prompt_template(&self.default)?;

        for (agent, prompt) in &self.by_agent {
            if !is_valid_agent_key(agent) {
                return Err(format!(
                    "workflows.compaction_prompts.by_agent has invalid key: {agent}"
                ));
            }
            if prompt.trim().is_empty() {
                return Err(format!(
                    "workflows.compaction_prompts.by_agent.{agent} must not be empty"
                ));
            }
            validate_compaction_prompt_template(prompt)?;
        }

        for (pane_id, prompt) in &self.by_pane {
            if prompt.trim().is_empty() {
                return Err(format!(
                    "workflows.compaction_prompts.by_pane.{pane_id} must not be empty"
                ));
            }
            validate_compaction_prompt_template(prompt)?;
        }

        for override_item in &self.by_project {
            override_item.validate()?;
        }

        Ok(())
    }
}

fn is_valid_agent_key(key: &str) -> bool {
    matches!(key, "codex" | "claude_code" | "gemini" | "unknown")
}

fn validate_compaction_prompt_template(template: &str) -> Result<(), String> {
    for token in extract_prompt_placeholders(template)? {
        if !COMPACTION_PROMPT_TOKENS.contains(&token.as_str()) {
            return Err(format!(
                "Unknown placeholder '{{{{{token}}}}}' in compaction prompt template"
            ));
        }
    }
    Ok(())
}

fn extract_prompt_placeholders(template: &str) -> Result<Vec<String>, String> {
    let mut placeholders = Vec::new();
    let mut cursor = template;

    while let Some(start) = cursor.find("{{") {
        let after_start = &cursor[start + 2..];
        let Some(end) = after_start.find("}}") else {
            return Err("Unterminated '{{' in compaction prompt template".to_string());
        };

        let token = after_start[..end].trim();
        if token.is_empty() {
            return Err("Empty placeholder in compaction prompt template".to_string());
        }

        placeholders.push(token.to_string());
        cursor = &after_start[end + 2..];
    }

    Ok(placeholders)
}

// =============================================================================
// Workflows Config
// =============================================================================

/// Workflow execution configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkflowsConfig {
    /// Enabled workflows (by name)
    pub enabled: Vec<String>,

    /// Workflows that can auto-run on event detection
    pub auto_run_allowlist: Vec<String>,

    /// Workflows that are blocked from auto-running
    pub auto_run_denylist: Vec<String>,

    /// Maximum concurrent workflow executions
    pub max_concurrent: u32,

    /// Default step timeout in milliseconds
    pub default_step_timeout_ms: u64,

    /// Enable step-level audit logging
    pub audit_steps: bool,

    /// Prompt templates for handle_compaction
    pub compaction_prompts: CompactionPromptConfig,
}

impl Default for WorkflowsConfig {
    fn default() -> Self {
        Self {
            enabled: vec![
                "handle_compaction".to_string(),
                "handle_usage_limits".to_string(),
            ],
            auto_run_allowlist: vec!["handle_compaction".to_string()],
            auto_run_denylist: Vec::new(),
            max_concurrent: 3,
            default_step_timeout_ms: 30_000, // 30 seconds
            audit_steps: true,
            compaction_prompts: CompactionPromptConfig::default(),
        }
    }
}

// =============================================================================
// Safety Config
// =============================================================================

/// Safety and policy configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SafetyConfig {
    /// Rate limit: maximum actions per pane per minute (per action kind)
    pub rate_limit_per_pane: u32,

    /// Rate limit: maximum total actions per minute (per action kind)
    pub rate_limit_global: u32,

    /// Require prompt to be detected before allowing send
    pub require_prompt_active: bool,

    /// Block sends to alt-screen applications (vim, less, etc.)
    pub block_alt_screen: bool,

    /// Capability gating rules
    pub capabilities: CapabilityConfig,

    /// Approval (allow-once) settings
    pub approval: ApprovalConfig,

    /// Redaction settings for sensitive data
    pub redaction: RedactionConfig,

    /// Pane reservation defaults
    pub reservations: ReservationConfig,

    /// Command safety gate configuration
    pub command_gate: CommandGateConfig,

    /// Custom policy rules (allow/deny/require_approval)
    pub rules: PolicyRulesConfig,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            rate_limit_per_pane: 30,
            rate_limit_global: 100,
            require_prompt_active: true,
            block_alt_screen: true,
            capabilities: CapabilityConfig::default(),
            approval: ApprovalConfig::default(),
            redaction: RedactionConfig::default(),
            reservations: ReservationConfig::default(),
            command_gate: CommandGateConfig::default(),
            rules: PolicyRulesConfig::default(),
        }
    }
}

/// Command safety gate configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CommandGateConfig {
    /// Enable command safety gate for SendText
    pub enabled: bool,
    /// dcg integration mode
    pub dcg_mode: DcgMode,
    /// Policy when dcg denies a command
    pub dcg_deny_policy: DcgDenyPolicy,
}

impl Default for CommandGateConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            dcg_mode: DcgMode::Opportunistic,
            dcg_deny_policy: DcgDenyPolicy::RequireApproval,
        }
    }
}

/// dcg integration mode for command safety gate
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DcgMode {
    Disabled,
    Opportunistic,
    Required,
}

/// Policy to apply when dcg denies a command
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DcgDenyPolicy {
    Deny,
    RequireApproval,
}

// =============================================================================
// Policy Rules Config
// =============================================================================

/// Policy rules configuration
///
/// Allows operators to define custom policy rules that match on action context
/// and specify decisions (allow/deny/require_approval).
///
/// # Precedence
///
/// Rules are evaluated in order of priority (lower number = higher priority):
/// 1. Built-in hard denies (capability gates, alt-screen) always win
/// 2. Explicit deny rules (cannot be overridden by approval)
/// 3. Explicit require_approval rules
/// 4. Explicit allow rules
/// 5. Default behavior (if no rule matches)
///
/// Within the same decision type, more specific matches beat general matches.
/// Specificity is determined by number of non-wildcard match criteria.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PolicyRulesConfig {
    /// Whether custom policy rules are enabled
    pub enabled: bool,

    /// Policy rules (evaluated in order after built-in rules)
    pub rules: Vec<PolicyRule>,
}

impl Default for PolicyRulesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            rules: Vec::new(),
        }
    }
}

/// A single policy rule
///
/// Rules match on action context and produce a decision.
/// All match criteria are optional; omitted criteria match any value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyRule {
    /// Unique identifier for this rule (for audit/debugging)
    pub id: String,

    /// Human-readable description of why this rule exists
    #[serde(default)]
    pub description: Option<String>,

    /// Priority (lower = higher priority, default 100)
    #[serde(default = "default_priority")]
    pub priority: u32,

    /// Match criteria
    #[serde(default)]
    pub match_on: PolicyRuleMatch,

    /// Decision when this rule matches
    pub decision: PolicyRuleDecision,

    /// Message to show when this rule triggers (optional)
    #[serde(default)]
    pub message: Option<String>,
}

fn default_priority() -> u32 {
    100
}

/// Match criteria for a policy rule
///
/// All fields are optional. Omitted fields match any value.
/// Multiple values in a list are OR'd (match any).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PolicyRuleMatch {
    /// Match specific action kinds (e.g., "send_text", "ctrl_c")
    #[serde(default)]
    pub actions: Vec<String>,

    /// Match specific actor kinds (e.g., "robot", "mcp", "workflow")
    #[serde(default)]
    pub actors: Vec<String>,

    /// Match pane by ID (exact match)
    #[serde(default)]
    pub pane_ids: Vec<u64>,

    /// Match pane by title pattern (glob)
    #[serde(default)]
    pub pane_titles: Vec<String>,

    /// Match pane by working directory pattern (glob)
    #[serde(default)]
    pub pane_cwds: Vec<String>,

    /// Match pane by domain (exact match)
    #[serde(default)]
    pub pane_domains: Vec<String>,

    /// Match command text by regex pattern
    #[serde(default)]
    pub command_patterns: Vec<String>,

    /// Match inferred agent type (e.g., "claude", "cursor", "shell")
    #[serde(default)]
    pub agent_types: Vec<String>,
}

impl PolicyRuleMatch {
    /// Returns the specificity score (number of non-empty match criteria)
    ///
    /// Higher specificity = more specific rule = wins ties
    #[must_use]
    pub fn specificity(&self) -> u32 {
        let mut score = 0;
        if !self.actions.is_empty() {
            score += 1;
        }
        if !self.actors.is_empty() {
            score += 1;
        }
        if !self.pane_ids.is_empty() {
            score += 2; // ID match is very specific
        }
        if !self.pane_titles.is_empty() {
            score += 1;
        }
        if !self.pane_cwds.is_empty() {
            score += 1;
        }
        if !self.pane_domains.is_empty() {
            score += 1;
        }
        if !self.command_patterns.is_empty() {
            score += 2; // Command pattern is very specific
        }
        if !self.agent_types.is_empty() {
            score += 1;
        }
        score
    }

    /// Returns true if all criteria are empty (matches everything)
    #[must_use]
    pub fn is_catch_all(&self) -> bool {
        self.actions.is_empty()
            && self.actors.is_empty()
            && self.pane_ids.is_empty()
            && self.pane_titles.is_empty()
            && self.pane_cwds.is_empty()
            && self.pane_domains.is_empty()
            && self.command_patterns.is_empty()
            && self.agent_types.is_empty()
    }
}

/// Decision for a policy rule
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyRuleDecision {
    /// Allow the action
    Allow,
    /// Deny the action (cannot be overridden by approval)
    Deny,
    /// Require explicit user approval
    RequireApproval,
}

impl PolicyRuleDecision {
    /// Returns the decision priority for rule ordering
    ///
    /// Lower number = higher priority (evaluated first)
    /// Deny > RequireApproval > Allow
    #[must_use]
    pub const fn priority(&self) -> u32 {
        match self {
            Self::Deny => 0,
            Self::RequireApproval => 1,
            Self::Allow => 2,
        }
    }

    /// Returns the string representation
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::RequireApproval => "require_approval",
        }
    }
}

/// Capability gating configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[allow(clippy::struct_excessive_bools)] // These are independent capability flags
pub struct CapabilityConfig {
    /// Allow sending control characters (Ctrl-C, Ctrl-D, etc.)
    pub allow_control_chars: bool,

    /// Allow sending to panes without detected agent
    pub allow_non_agent_panes: bool,

    /// Allow sending arbitrary text (vs. only workflow-generated)
    pub allow_arbitrary_text: bool,

    /// Require explicit confirmation for dangerous patterns
    pub confirm_dangerous_patterns: bool,
}

impl Default for CapabilityConfig {
    fn default() -> Self {
        Self {
            allow_control_chars: true,
            allow_non_agent_panes: false,
            allow_arbitrary_text: true,
            confirm_dangerous_patterns: true,
        }
    }
}

/// Approval (allow-once) configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ApprovalConfig {
    /// Token expiry time in seconds
    pub token_expiry_secs: u64,

    /// Maximum active approval tokens
    pub max_active_tokens: u32,

    /// Require re-approval after workflow failure
    pub require_reapproval_on_failure: bool,
}

impl Default for ApprovalConfig {
    fn default() -> Self {
        Self {
            token_expiry_secs: 86400, // 24 hours
            max_active_tokens: 100,
            require_reapproval_on_failure: true,
        }
    }
}

/// Redaction configuration for sensitive data
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RedactionConfig {
    /// Enable automatic redaction of detected secrets
    pub enabled: bool,

    /// Patterns to redact (regex)
    pub patterns: Vec<String>,

    /// Placeholder text for redacted content
    pub placeholder: String,

    /// Redact in audit logs
    pub redact_audit: bool,

    /// Redact in stored segments
    pub redact_segments: bool,
}

impl Default for RedactionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            patterns: vec![
                // API keys (common formats)
                r#"(?i)(api[_-]?key|apikey)[=:]\s*['\"]?[\w-]{20,}"#.to_string(),
                // Bearer tokens
                r"(?i)bearer\s+[\w-]{20,}".to_string(),
                // AWS credentials
                r#"(?i)(aws[_-]?secret|aws[_-]?access)[=:]\s*['\"]?[\w/+=]{20,}"#.to_string(),
            ],
            placeholder: "[REDACTED]".to_string(),
            redact_audit: true,
            redact_segments: false, // Only redact in audit by default
        }
    }
}

/// Pane reservation configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ReservationConfig {
    /// Default reservation TTL in seconds
    pub default_ttl_secs: u64,

    /// Maximum reservation TTL in seconds
    pub max_ttl_secs: u64,

    /// Conflict behavior: "deny", "queue", "force" (with warning)
    pub conflict_behavior: String,

    /// Auto-release on workflow completion
    pub auto_release_on_complete: bool,
}

impl Default for ReservationConfig {
    fn default() -> Self {
        Self {
            default_ttl_secs: 300, // 5 minutes
            max_ttl_secs: 3600,    // 1 hour
            conflict_behavior: "deny".to_string(),
            auto_release_on_complete: true,
        }
    }
}

// =============================================================================
// Vendored Config
// =============================================================================

/// Vendored WezTerm configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VendoredConfig {
    /// Optional mux socket path override (WEZTERM_UNIX_SOCKET equivalent)
    pub mux_socket_path: Option<String>,
}

impl Default for VendoredConfig {
    fn default() -> Self {
        Self {
            mux_socket_path: None,
        }
    }
}

// =============================================================================
// Metrics Config
// =============================================================================

/// Metrics/telemetry configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MetricsConfig {
    /// Enable metrics endpoint
    pub enabled: bool,

    /// Bind address for metrics server (e.g., "127.0.0.1:9090")
    pub bind: String,

    /// Metrics prefix for all exported metrics
    pub prefix: String,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: "127.0.0.1:9090".to_string(),
            prefix: "wa".to_string(),
        }
    }
}

// =============================================================================
// Notification Config
// =============================================================================

/// Notification filtering and throttling configuration.
///
/// Controls which detected events are forwarded to the notification pipeline
/// (webhooks, desktop alerts, etc.) and how aggressively repeated events are
/// suppressed.
///
/// # Example (wa.toml)
///
/// ```toml
/// [notifications]
/// enabled = true
/// cooldown_ms = 30000
/// dedup_window_ms = 300000
/// min_severity = "warning"
///
/// # Only notify on usage-limit and auth events (glob patterns)
/// include = ["*.usage_*", "*.auth_*", "*.error"]
///
/// # Never notify on debug/test rules
/// exclude = ["*.debug", "test.*"]
///
/// # Only for codex and claude_code agents
/// agent_types = ["codex", "claude_code"]
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationConfig {
    /// Master switch for the notification pipeline
    pub enabled: bool,

    /// Notification cooldown period in milliseconds.
    /// Within this window, repeated notifications for the same event key
    /// are suppressed and the suppressed count is included in the next
    /// notification that fires.
    pub cooldown_ms: u64,

    /// Event deduplication window in milliseconds.
    /// Identical events (same rule_id + pane) within this window are
    /// collapsed into a single notification.
    pub dedup_window_ms: u64,

    /// Include patterns: events whose `rule_id` matches ANY of these
    /// glob patterns pass through. If empty, all events are included
    /// (subject to exclude rules).
    ///
    /// Supports `*` (any sequence) and `?` (any single char).
    /// Examples: `"*.error"`, `"codex.*"`, `"core.codex:usage_*"`
    pub include: Vec<String>,

    /// Exclude patterns: events whose `rule_id` matches ANY of these
    /// glob patterns are filtered out. Exclude always wins over include.
    pub exclude: Vec<String>,

    /// Minimum severity for notification. Events below this threshold
    /// are silently filtered out.
    /// Accepts: `"info"`, `"warning"`, `"critical"` (case-insensitive).
    pub min_severity: Option<String>,

    /// Agent type allowlist. If non-empty, only events from these agent
    /// types are forwarded.
    /// Accepts: `"codex"`, `"claude_code"`, `"gemini"`, `"wezterm"`, `"unknown"`.
    pub agent_types: Vec<String>,

    /// Webhook endpoints for HTTP POST delivery.
    ///
    /// Each endpoint can subscribe to specific event patterns and use
    /// a different payload template (generic, slack, discord).
    #[serde(default)]
    pub webhooks: Vec<crate::webhook::WebhookEndpointConfig>,

    /// Desktop notification settings (native OS alerts).
    #[serde(default)]
    pub desktop: crate::desktop_notify::DesktopNotifyConfig,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            cooldown_ms: 30_000,      // 30 seconds
            dedup_window_ms: 300_000, // 5 minutes
            include: Vec::new(),
            exclude: Vec::new(),
            min_severity: None,
            agent_types: Vec::new(),
            webhooks: Vec::new(),
            desktop: crate::desktop_notify::DesktopNotifyConfig::default(),
        }
    }
}

impl NotificationConfig {
    /// Build an [`EventFilter`](crate::events::EventFilter) from this config.
    #[must_use]
    pub fn to_event_filter(&self) -> crate::events::EventFilter {
        crate::events::EventFilter::from_config(
            &self.include,
            &self.exclude,
            self.min_severity.as_deref(),
            &self.agent_types,
        )
    }

    /// Build a [`NotificationGate`](crate::events::NotificationGate) from this config.
    #[must_use]
    pub fn to_notification_gate(&self) -> crate::events::NotificationGate {
        crate::events::NotificationGate::from_config(
            self.to_event_filter(),
            Duration::from_millis(self.dedup_window_ms),
            Duration::from_millis(self.cooldown_ms),
        )
    }
}

// =============================================================================
// Config Loading
// =============================================================================

/// CLI overrides applied after env overrides
#[derive(Debug, Default, Clone)]
pub struct ConfigOverrides {
    /// Override log level
    pub log_level: Option<String>,
    /// Override log format (pretty or json)
    pub log_format: Option<LogFormat>,
    /// Override log file path
    pub log_file: Option<String>,
    /// Override storage database path
    pub storage_db_path: Option<String>,
    /// Override metrics enabled flag
    pub metrics_enabled: Option<bool>,
    /// Override metrics bind address
    pub metrics_bind: Option<String>,
    /// Override metrics prefix
    pub metrics_prefix: Option<String>,
}

impl ConfigOverrides {
    fn apply(&self, config: &mut Config) {
        if let Some(ref log_level) = self.log_level {
            config.general.log_level.clone_from(log_level);
        }
        if let Some(log_format) = self.log_format {
            config.general.log_format = log_format;
        }
        if let Some(ref log_file) = self.log_file {
            config.general.log_file = Some(log_file.clone());
        }
        if let Some(ref db_path) = self.storage_db_path {
            config.storage.db_path.clone_from(db_path);
        }
        if let Some(enabled) = self.metrics_enabled {
            config.metrics.enabled = enabled;
        }
        if let Some(ref bind) = self.metrics_bind {
            config.metrics.bind.clone_from(bind);
        }
        if let Some(ref prefix) = self.metrics_prefix {
            config.metrics.prefix.clone_from(prefix);
        }
    }
}

#[derive(Debug, Default)]
struct EnvOverrides {
    log_level: Option<String>,
    log_format: Option<LogFormat>,
    log_file: Option<String>,
    storage_db_path: Option<String>,
    metrics_enabled: Option<bool>,
    metrics_bind: Option<String>,
    metrics_prefix: Option<String>,
}

impl EnvOverrides {
    fn from_env() -> crate::Result<Self> {
        let mut overrides = Self::default();

        if let Ok(value) = std::env::var("WA_LOG_LEVEL") {
            overrides.log_level = Some(value);
        }
        if let Ok(value) = std::env::var("WA_LOG_FORMAT") {
            overrides.log_format = Some(value.parse::<LogFormat>().map_err(crate::Error::Config)?);
        }
        if let Ok(value) = std::env::var("WA_LOG_FILE") {
            overrides.log_file = Some(value);
        }
        if let Ok(value) = std::env::var("WA_STORAGE_DB_PATH") {
            overrides.storage_db_path = Some(value);
        }
        if let Ok(value) = std::env::var("WA_METRICS_ENABLED") {
            overrides.metrics_enabled = Some(parse_env_bool(&value)?);
        }
        if let Ok(value) = std::env::var("WA_METRICS_BIND") {
            overrides.metrics_bind = Some(value);
        }
        if let Ok(value) = std::env::var("WA_METRICS_PREFIX") {
            overrides.metrics_prefix = Some(value);
        }

        Ok(overrides)
    }

    fn apply(self, config: &mut Config) {
        if let Some(log_level) = self.log_level {
            config.general.log_level = log_level;
        }
        if let Some(log_format) = self.log_format {
            config.general.log_format = log_format;
        }
        if let Some(log_file) = self.log_file {
            config.general.log_file = Some(log_file);
        }
        if let Some(db_path) = self.storage_db_path {
            config.storage.db_path = db_path;
        }
        if let Some(enabled) = self.metrics_enabled {
            config.metrics.enabled = enabled;
        }
        if let Some(bind) = self.metrics_bind {
            config.metrics.bind = bind;
        }
        if let Some(prefix) = self.metrics_prefix {
            config.metrics.prefix = prefix;
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct EffectiveConfig {
    pub config: Config,
    pub paths: EffectivePaths,
}

#[derive(Debug, Clone, Serialize)]
pub struct EffectivePaths {
    pub workspace_root: String,
    pub wa_dir: String,
    pub db_path: String,
    pub lock_path: String,
    pub ipc_socket_path: String,
    pub logs_dir: String,
    pub log_path: String,
    pub crash_dir: String,
    pub diag_dir: String,
}

impl EffectivePaths {
    fn from_layout(layout: &WorkspaceLayout) -> Self {
        Self {
            workspace_root: path_to_string(&layout.root),
            wa_dir: path_to_string(&layout.wa_dir),
            db_path: path_to_string(&layout.db_path),
            lock_path: path_to_string(&layout.lock_path),
            ipc_socket_path: path_to_string(&layout.ipc_socket_path),
            logs_dir: path_to_string(&layout.logs_dir),
            log_path: path_to_string(&layout.log_path),
            crash_dir: path_to_string(&layout.crash_dir),
            diag_dir: path_to_string(&layout.diag_dir),
        }
    }
}

// =============================================================================
// Hot Reload Support
// =============================================================================

/// Settings that can be safely hot-reloaded without restarting the watcher.
///
/// These settings do not require reinitialization of stateful components
/// like storage handles or pattern engines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotReloadableConfig {
    // General
    /// Log level (trace, debug, info, warn, error)
    pub log_level: String,

    // Ingest
    /// Base poll interval in milliseconds
    pub poll_interval_ms: u64,
    /// Minimum poll interval (adaptive lower bound)
    pub min_poll_interval_ms: u64,
    /// Maximum concurrent captures
    pub max_concurrent_captures: u32,

    // Storage
    /// Retention period in days
    pub retention_days: u32,
    /// Size-based retention in megabytes
    pub retention_max_mb: u32,
    /// Checkpoint interval in seconds
    pub checkpoint_interval_secs: u32,

    // Patterns
    /// Enabled pattern packs
    pub pattern_packs: Vec<String>,

    // Workflows
    /// Enabled workflows
    pub workflows_enabled: Vec<String>,
    /// Auto-run allowlist
    pub auto_run_allowlist: Vec<String>,
}

impl HotReloadableConfig {
    /// Extract hot-reloadable settings from a full Config.
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        Self {
            log_level: config.general.log_level.clone(),
            poll_interval_ms: config.ingest.poll_interval_ms,
            min_poll_interval_ms: config.ingest.min_poll_interval_ms,
            max_concurrent_captures: config.ingest.max_concurrent_captures,
            retention_days: config.storage.retention_days,
            retention_max_mb: config.storage.retention_max_mb,
            checkpoint_interval_secs: config.storage.checkpoint_interval_secs,
            pattern_packs: config.patterns.packs.clone(),
            workflows_enabled: config.workflows.enabled.clone(),
            auto_run_allowlist: config.workflows.auto_run_allowlist.clone(),
        }
    }
}

/// Result of comparing two configs for hot reload.
#[derive(Debug, Clone)]
pub struct HotReloadResult {
    /// Whether the reload is allowed (no forbidden changes)
    pub allowed: bool,
    /// Settings that changed and can be applied
    pub changes: Vec<HotReloadChange>,
    /// Forbidden changes that require a restart
    pub forbidden: Vec<ForbiddenChange>,
}

/// A single hot-reloadable setting that changed.
#[derive(Debug, Clone)]
pub struct HotReloadChange {
    /// Setting name (e.g., "poll_interval_ms")
    pub name: String,
    /// Previous value (as string for display)
    pub old_value: String,
    /// New value (as string for display)
    pub new_value: String,
}

/// A change to a setting that cannot be hot-reloaded.
#[derive(Debug, Clone)]
pub struct ForbiddenChange {
    /// Setting name
    pub name: String,
    /// Reason why this setting cannot be hot-reloaded
    pub reason: String,
}

impl Config {
    /// Compare two configs and determine what can be hot-reloaded.
    ///
    /// Returns `HotReloadResult` indicating whether the reload is allowed
    /// and what changes would be applied.
    #[must_use]
    pub fn diff_for_hot_reload(&self, new_config: &Self) -> HotReloadResult {
        let mut changes = Vec::new();
        let mut forbidden = Vec::new();

        // Check forbidden settings first
        if self.storage.db_path != new_config.storage.db_path {
            forbidden.push(ForbiddenChange {
                name: "storage.db_path".to_string(),
                reason: "Database path cannot be changed at runtime; requires restart".to_string(),
            });
        }

        if self.general.data_dir != new_config.general.data_dir {
            forbidden.push(ForbiddenChange {
                name: "general.data_dir".to_string(),
                reason: "Data directory cannot be changed at runtime; requires restart".to_string(),
            });
        }

        if self.storage.writer_queue_size != new_config.storage.writer_queue_size {
            forbidden.push(ForbiddenChange {
                name: "storage.writer_queue_size".to_string(),
                reason: "Writer queue size cannot be changed at runtime; requires restart"
                    .to_string(),
            });
        }

        if self.storage.read_pool_size != new_config.storage.read_pool_size {
            forbidden.push(ForbiddenChange {
                name: "storage.read_pool_size".to_string(),
                reason: "Read pool size cannot be changed at runtime; requires restart".to_string(),
            });
        }

        if self.backup.scheduled != new_config.backup.scheduled {
            forbidden.push(ForbiddenChange {
                name: "backup.scheduled".to_string(),
                reason: "Scheduled backup settings cannot be changed at runtime; requires restart"
                    .to_string(),
            });
        }

        // Check hot-reloadable settings
        if self.general.log_level != new_config.general.log_level {
            changes.push(HotReloadChange {
                name: "general.log_level".to_string(),
                old_value: self.general.log_level.clone(),
                new_value: new_config.general.log_level.clone(),
            });
        }

        if self.ingest.poll_interval_ms != new_config.ingest.poll_interval_ms {
            changes.push(HotReloadChange {
                name: "ingest.poll_interval_ms".to_string(),
                old_value: self.ingest.poll_interval_ms.to_string(),
                new_value: new_config.ingest.poll_interval_ms.to_string(),
            });
        }

        if self.ingest.min_poll_interval_ms != new_config.ingest.min_poll_interval_ms {
            changes.push(HotReloadChange {
                name: "ingest.min_poll_interval_ms".to_string(),
                old_value: self.ingest.min_poll_interval_ms.to_string(),
                new_value: new_config.ingest.min_poll_interval_ms.to_string(),
            });
        }

        if self.ingest.max_concurrent_captures != new_config.ingest.max_concurrent_captures {
            changes.push(HotReloadChange {
                name: "ingest.max_concurrent_captures".to_string(),
                old_value: self.ingest.max_concurrent_captures.to_string(),
                new_value: new_config.ingest.max_concurrent_captures.to_string(),
            });
        }

        if self.storage.retention_days != new_config.storage.retention_days {
            changes.push(HotReloadChange {
                name: "storage.retention_days".to_string(),
                old_value: self.storage.retention_days.to_string(),
                new_value: new_config.storage.retention_days.to_string(),
            });
        }

        if self.storage.retention_max_mb != new_config.storage.retention_max_mb {
            changes.push(HotReloadChange {
                name: "storage.retention_max_mb".to_string(),
                old_value: self.storage.retention_max_mb.to_string(),
                new_value: new_config.storage.retention_max_mb.to_string(),
            });
        }

        if self.storage.checkpoint_interval_secs != new_config.storage.checkpoint_interval_secs {
            changes.push(HotReloadChange {
                name: "storage.checkpoint_interval_secs".to_string(),
                old_value: self.storage.checkpoint_interval_secs.to_string(),
                new_value: new_config.storage.checkpoint_interval_secs.to_string(),
            });
        }

        if self.patterns.packs != new_config.patterns.packs {
            changes.push(HotReloadChange {
                name: "patterns.packs".to_string(),
                old_value: format!("{:?}", self.patterns.packs),
                new_value: format!("{:?}", new_config.patterns.packs),
            });
        }

        if self.workflows.enabled != new_config.workflows.enabled {
            changes.push(HotReloadChange {
                name: "workflows.enabled".to_string(),
                old_value: format!("{:?}", self.workflows.enabled),
                new_value: format!("{:?}", new_config.workflows.enabled),
            });
        }

        if self.workflows.auto_run_allowlist != new_config.workflows.auto_run_allowlist {
            changes.push(HotReloadChange {
                name: "workflows.auto_run_allowlist".to_string(),
                old_value: format!("{:?}", self.workflows.auto_run_allowlist),
                new_value: format!("{:?}", new_config.workflows.auto_run_allowlist),
            });
        }

        HotReloadResult {
            allowed: forbidden.is_empty(),
            changes,
            forbidden,
        }
    }

    /// Get the hot-reloadable subset of this config.
    #[must_use]
    pub fn hot_reloadable(&self) -> HotReloadableConfig {
        HotReloadableConfig::from_config(self)
    }
}

impl std::fmt::Display for HotReloadResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.forbidden.is_empty() && self.changes.is_empty() {
            return write!(f, "No configuration changes detected");
        }

        if !self.forbidden.is_empty() {
            writeln!(f, "Forbidden changes (require restart):")?;
            for fc in &self.forbidden {
                writeln!(f, "  - {}: {}", fc.name, fc.reason)?;
            }
        }

        if !self.changes.is_empty() {
            writeln!(f, "Hot-reloadable changes:")?;
            for c in &self.changes {
                writeln!(f, "  - {}: {} -> {}", c.name, c.old_value, c.new_value)?;
            }
        }

        Ok(())
    }
}

/// Resolve the config path that was loaded (if any).
#[must_use]
pub fn resolve_config_path(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return Some(path.to_path_buf());
    }

    let cwd_config = std::path::Path::new("wa.toml");
    if cwd_config.exists() {
        return Some(cwd_config.to_path_buf());
    }

    let config_dir = dirs_config_path();
    if let Some(dir) = config_dir {
        let config_path = dir.join("wa.toml");
        if config_path.exists() {
            return Some(config_path);
        }
    }

    None
}

impl Config {
    /// Load configuration from default locations
    ///
    /// Search order:
    /// 1. ./wa.toml (current directory)
    /// 2. $XDG_CONFIG_HOME/wa/wa.toml or ~/.config/wa/wa.toml
    /// 3. Default values
    pub fn load() -> crate::Result<Self> {
        // Check current directory first
        let cwd_config = std::path::Path::new("wa.toml");
        if cwd_config.exists() {
            return Self::load_from(cwd_config);
        }

        // Check XDG config directory
        let config_dir = dirs_config_path();
        if let Some(ref dir) = config_dir {
            let config_path = dir.join("wa.toml");
            if config_path.exists() {
                return Self::load_from(&config_path);
            }
        }

        // Return defaults
        Ok(Self::default())
    }

    /// Load configuration from a specific path
    pub fn load_from(path: &std::path::Path) -> crate::Result<Self> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            crate::error::ConfigError::ReadFailed(path.display().to_string(), e.to_string())
        })?;

        Self::from_toml(&content)
    }

    /// Parse configuration from TOML string
    pub fn from_toml(content: &str) -> crate::Result<Self> {
        toml::from_str(content)
            .map_err(|e| crate::error::ConfigError::ParseFailed(e.to_string()).into())
    }

    /// Serialize configuration to TOML string
    pub fn to_toml(&self) -> crate::Result<String> {
        toml::to_string_pretty(self)
            .map_err(|e| crate::error::ConfigError::SerializeFailed(e.to_string()).into())
    }

    /// Load configuration with overrides and validation
    ///
    /// Resolution order: defaults -> config file -> env -> CLI overrides.
    pub fn load_with_overrides(
        config_path: Option<&Path>,
        strict: bool,
        overrides: &ConfigOverrides,
    ) -> crate::Result<Self> {
        let mut config = match config_path {
            Some(path) => {
                if path.exists() {
                    Self::load_from(path)?
                } else if strict {
                    return Err(crate::error::ConfigError::FileNotFound(
                        path.display().to_string(),
                    )
                    .into());
                } else {
                    Self::default()
                }
            }
            None => Self::load()?,
        };

        let env_overrides = EnvOverrides::from_env()?;
        env_overrides.apply(&mut config);
        overrides.apply(&mut config);
        config.normalize_paths();
        config.validate()?;

        Ok(config)
    }

    /// Build a resolved, effective view of the config including workspace paths
    pub fn effective_config(
        &self,
        workspace_root: Option<&Path>,
    ) -> crate::Result<EffectiveConfig> {
        let layout = self.workspace_layout(workspace_root)?;
        Ok(EffectiveConfig {
            config: self.clone(),
            paths: EffectivePaths::from_layout(&layout),
        })
    }

    /// Normalize path fields by expanding tildes
    pub fn normalize_paths(&mut self) {
        let data_dir = expand_tilde(&self.general.data_dir);
        self.general.data_dir = path_to_string(&data_dir);

        if let Some(log_file) = self.general.log_file.take() {
            let log_path = expand_tilde(&log_file);
            self.general.log_file = Some(path_to_string(&log_path));
        }

        let db_path = expand_tilde(&self.storage.db_path);
        self.storage.db_path = path_to_string(&db_path);

        if let Some(path) = self.vendored.mux_socket_path.take() {
            let mux_path = expand_tilde(&path);
            self.vendored.mux_socket_path = Some(path_to_string(&mux_path));
        }

        if let Some(dest) = self.backup.scheduled.destination.take() {
            let dest_path = expand_tilde(&dest);
            self.backup.scheduled.destination = Some(path_to_string(&dest_path));
        }
    }

    /// Validate semantic constraints
    pub fn validate(&self) -> crate::Result<()> {
        if self.ingest.min_poll_interval_ms == 0 {
            return Err(crate::error::ConfigError::ValidationError(
                "ingest.min_poll_interval_ms must be >= 1".to_string(),
            )
            .into());
        }

        if self.ingest.poll_interval_ms < self.ingest.min_poll_interval_ms {
            return Err(crate::error::ConfigError::ValidationError(format!(
                "ingest.poll_interval_ms ({}) must be >= ingest.min_poll_interval_ms ({})",
                self.ingest.poll_interval_ms, self.ingest.min_poll_interval_ms
            ))
            .into());
        }

        if self.ingest.max_concurrent_captures == 0 {
            return Err(crate::error::ConfigError::ValidationError(
                "ingest.max_concurrent_captures must be >= 1".to_string(),
            )
            .into());
        }

        if self.backup.scheduled.enabled {
            crate::backup::BackupSchedule::parse(&self.backup.scheduled.schedule)?;
        }

        if self.storage.writer_queue_size == 0 {
            return Err(crate::error::ConfigError::ValidationError(
                "storage.writer_queue_size must be >= 1".to_string(),
            )
            .into());
        }

        if self.workflows.max_concurrent == 0 {
            return Err(crate::error::ConfigError::ValidationError(
                "workflows.max_concurrent must be >= 1".to_string(),
            )
            .into());
        }

        if self.metrics.bind.trim().is_empty() {
            return Err(crate::error::ConfigError::ValidationError(
                "metrics.bind must not be empty".to_string(),
            )
            .into());
        }

        self.workflows
            .compaction_prompts
            .validate()
            .map_err(crate::error::ConfigError::ValidationError)?;

        Ok(())
    }

    /// Get the effective data directory (with ~ expansion)
    #[must_use]
    pub fn effective_data_dir(&self) -> std::path::PathBuf {
        expand_tilde(&self.general.data_dir)
    }

    /// Resolve the workspace root (CLI override > WA_WORKSPACE > current dir)
    pub fn resolve_workspace_root(&self, explicit: Option<&Path>) -> crate::Result<PathBuf> {
        let env_path = std::env::var("WA_WORKSPACE").ok();
        resolve_workspace_root_with_env(explicit, env_path.as_deref())
    }

    /// Resolve workspace layout paths for a given workspace root
    pub fn workspace_layout(&self, explicit: Option<&Path>) -> crate::Result<WorkspaceLayout> {
        let root = self.resolve_workspace_root(explicit)?;
        Ok(WorkspaceLayout::new(root, &self.storage))
    }

    /// Get the effective database path for a workspace root
    #[must_use]
    pub fn effective_db_path(&self, workspace_root: &Path) -> PathBuf {
        let db_path = Path::new(&self.storage.db_path);
        if db_path.is_absolute() {
            db_path.to_path_buf()
        } else {
            workspace_root.join(".wa").join(db_path)
        }
    }
}

// =============================================================================
// Workspace Layout
// =============================================================================

/// Resolved filesystem layout for a workspace
#[derive(Debug, Clone)]
pub struct WorkspaceLayout {
    /// Workspace root directory
    pub root: PathBuf,
    /// Workspace state directory (.wa)
    pub wa_dir: PathBuf,
    /// SQLite database path
    pub db_path: PathBuf,
    /// Watcher lock path
    pub lock_path: PathBuf,
    /// IPC socket path
    pub ipc_socket_path: PathBuf,
    /// Logs directory
    pub logs_dir: PathBuf,
    /// Watcher log file path
    pub log_path: PathBuf,
    /// Crash reports directory
    pub crash_dir: PathBuf,
    /// Diagnostics bundle directory
    pub diag_dir: PathBuf,
}

impl WorkspaceLayout {
    /// Create a new workspace layout for the given root
    #[must_use]
    pub fn new(root: PathBuf, storage: &StorageConfig) -> Self {
        let wa_dir = root.join(".wa");
        let expanded_db_path = expand_tilde(&storage.db_path);
        let db_path = if expanded_db_path.is_absolute() {
            expanded_db_path
        } else {
            wa_dir.join(expanded_db_path)
        };
        let lock_path = wa_dir.join("watch.lock");
        let ipc_socket_path = wa_dir.join("ipc.sock");
        let logs_dir = wa_dir.join("logs");
        let log_path = logs_dir.join("wa-watch.log");
        let crash_dir = wa_dir.join("crash");
        let diag_dir = wa_dir.join("diag");

        Self {
            root,
            wa_dir,
            db_path,
            lock_path,
            ipc_socket_path,
            logs_dir,
            log_path,
            crash_dir,
            diag_dir,
        }
    }

    /// Ensure workspace directories exist and are writable
    pub fn ensure_directories(&self) -> crate::Result<()> {
        ensure_dir(&self.wa_dir)?;
        ensure_dir(&self.logs_dir)?;
        ensure_dir(&self.crash_dir)?;
        ensure_dir(&self.diag_dir)?;
        Ok(())
    }
}

/// Warning for paths that are more permissive than expected.
#[derive(Debug, Clone)]
pub struct PermissionWarning {
    pub label: &'static str,
    pub path: PathBuf,
    pub expected_mode: u32,
    pub actual_mode: u32,
}

/// Collect permission warnings for known sensitive paths.
#[must_use]
pub fn collect_permission_warnings(
    layout: &WorkspaceLayout,
    config_path: Option<&Path>,
    log_file_override: Option<&Path>,
) -> Vec<PermissionWarning> {
    let mut warnings = Vec::new();

    if let Some(warning) = check_permission(&layout.wa_dir, 0o700, "workspace dir") {
        warnings.push(warning);
    }
    if let Some(warning) = check_permission(&layout.logs_dir, 0o700, "logs dir") {
        warnings.push(warning);
    }
    if let Some(warning) = check_permission(&layout.crash_dir, 0o700, "crash dir") {
        warnings.push(warning);
    }
    if let Some(warning) = check_permission(&layout.diag_dir, 0o700, "diagnostics dir") {
        warnings.push(warning);
    }
    if let Some(warning) = check_permission(&layout.db_path, 0o600, "database") {
        warnings.push(warning);
    }
    if let Some(warning) = check_permission(&layout.log_path, 0o600, "watcher log") {
        warnings.push(warning);
    }
    if let Some(warning) = check_permission(&layout.lock_path, 0o644, "lock file") {
        warnings.push(warning);
    }
    if let Some(warning) = check_permission(&layout.ipc_socket_path, 0o600, "ipc socket") {
        warnings.push(warning);
    }
    if let Some(path) = config_path {
        if let Some(warning) = check_permission(path, 0o600, "config file") {
            warnings.push(warning);
        }
    }
    if let Some(path) = log_file_override {
        if let Some(warning) = check_permission(path, 0o600, "log file") {
            warnings.push(warning);
        }
    }

    warnings
}

#[cfg(unix)]
fn check_permission(
    path: &Path,
    expected_mode: u32,
    label: &'static str,
) -> Option<PermissionWarning> {
    let metadata = std::fs::metadata(path).ok()?;
    let actual_mode = metadata.permissions().mode() & 0o777;
    if actual_mode & !expected_mode != 0 {
        Some(PermissionWarning {
            label,
            path: path.to_path_buf(),
            expected_mode,
            actual_mode,
        })
    } else {
        None
    }
}

#[cfg(not(unix))]
fn check_permission(
    _path: &Path,
    _expected_mode: u32,
    _label: &'static str,
) -> Option<PermissionWarning> {
    None
}

/// Get the config directory path (XDG on Linux, Library on macOS)
fn dirs_config_path() -> Option<std::path::PathBuf> {
    #[cfg(target_os = "macos")]
    {
        dirs::home_dir().map(|h| h.join("Library").join("Application Support").join("wa"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::env::var("XDG_CONFIG_HOME")
            .ok()
            .map(std::path::PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
            .map(|p| p.join("wa"))
    }
}

/// Expand ~ to home directory
fn expand_tilde(path: &str) -> std::path::PathBuf {
    if path == "~" {
        return dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from(path));
    }
    if let Some(suffix) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(suffix);
        }
    }
    std::path::PathBuf::from(path)
}

fn resolve_path(path: &Path) -> crate::Result<PathBuf> {
    let expanded = path
        .to_str()
        .map_or_else(|| path.to_path_buf(), expand_tilde);

    if expanded.is_absolute() {
        Ok(expanded)
    } else {
        let cwd = std::env::current_dir().map_err(|e| {
            crate::error::ConfigError::ValidationError(format!(
                "Failed to resolve current directory: {e}"
            ))
        })?;
        Ok(cwd.join(expanded))
    }
}

fn parse_env_bool(value: &str) -> crate::Result<bool> {
    let normalized = value.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(crate::error::ConfigError::ValidationError(format!(
            "Invalid boolean value '{value}' for environment override"
        ))
        .into()),
    }
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

fn ensure_dir(path: &Path) -> crate::Result<()> {
    let existed = path.exists();
    std::fs::create_dir_all(path).map_err(|e| {
        crate::Error::Config(crate::error::ConfigError::ValidationError(format!(
            "Workspace path not writable: {} ({e}). Hint: choose a writable workspace via --workspace or WA_WORKSPACE.",
            path.display()
        )))
    })?;

    #[cfg(unix)]
    if !existed {
        let permissions = std::fs::Permissions::from_mode(0o700);
        std::fs::set_permissions(path, permissions).map_err(|e| {
            crate::Error::Config(crate::error::ConfigError::ValidationError(format!(
                "Failed to set permissions on {} ({e})",
                path.display()
            )))
        })?;
    }

    Ok(())
}

fn resolve_workspace_root_with_env(
    explicit: Option<&Path>,
    env_path: Option<&str>,
) -> crate::Result<PathBuf> {
    if let Some(path) = explicit {
        return resolve_path(path);
    }

    if let Some(env_path) = env_path {
        return resolve_path(Path::new(env_path));
    }

    std::env::current_dir().map_err(|e| {
        crate::error::ConfigError::ValidationError(format!(
            "Failed to resolve current directory: {e}"
        ))
        .into()
    })
}

// Provide a fallback for dirs crate
mod dirs {
    pub fn home_dir() -> Option<std::path::PathBuf> {
        std::env::var("HOME").ok().map(std::path::PathBuf::from)
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn default_config_is_valid() {
        let config = Config::default();
        assert_eq!(config.general.log_level, "info");
        assert_eq!(config.ingest.poll_interval_ms, 200);
        assert!(config.safety.require_prompt_active);
        assert_eq!(config.workflows.max_concurrent, 3);
        assert!(!config.metrics.enabled);
    }

    #[test]
    fn default_config_serializes_to_toml() {
        let config = Config::default();
        let toml = config.to_toml().expect("Failed to serialize");
        assert!(toml.contains("[general]"));
        assert!(toml.contains("[ingest]"));
        assert!(toml.contains("[storage]"));
        assert!(toml.contains("[patterns]"));
        assert!(toml.contains("[workflows]"));
        assert!(toml.contains("[safety]"));
        assert!(toml.contains("[metrics]"));
    }

    #[test]
    fn default_config_roundtrips() {
        let config = Config::default();
        let toml = config.to_toml().expect("Failed to serialize");
        let parsed = Config::from_toml(&toml).expect("Failed to parse");

        assert_eq!(config.general.log_level, parsed.general.log_level);
        assert_eq!(
            config.ingest.poll_interval_ms,
            parsed.ingest.poll_interval_ms
        );
        assert_eq!(config.storage.retention_days, parsed.storage.retention_days);
        assert_eq!(
            config.workflows.max_concurrent,
            parsed.workflows.max_concurrent
        );
        assert_eq!(
            config.safety.rate_limit_per_pane,
            parsed.safety.rate_limit_per_pane
        );
        assert_eq!(config.metrics.enabled, parsed.metrics.enabled);
    }

    #[test]
    fn empty_toml_uses_defaults() {
        let config = Config::from_toml("").expect("Failed to parse empty TOML");
        assert_eq!(config.general.log_level, "info");
        assert_eq!(config.ingest.poll_interval_ms, 200);
    }

    #[test]
    fn partial_toml_uses_defaults_for_missing() {
        let toml = r#"
[general]
log_level = "debug"

[storage]
retention_days = 7
"#;
        let config = Config::from_toml(toml).expect("Failed to parse");

        // Specified values
        assert_eq!(config.general.log_level, "debug");
        assert_eq!(config.storage.retention_days, 7);

        // Defaults for unspecified
        assert_eq!(config.ingest.poll_interval_ms, 200);
        assert_eq!(config.workflows.max_concurrent, 3);
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let toml = r#"
[general]
log_level = "info"
some_future_field = "value"

[unknown_section]
key = "value"
"#;
        // This should not error - unknown fields are silently ignored
        let result = Config::from_toml(toml);
        assert!(result.is_ok());
    }

    #[test]
    fn pack_overrides_work() {
        let toml = r#"
[patterns]
packs = ["builtin:core", "builtin:codex"]

[patterns.pack_overrides.codex]
disabled_rules = ["codex.usage_warning"]
"#;
        let config = Config::from_toml(toml).expect("Failed to parse");

        assert_eq!(config.patterns.packs.len(), 2);
        let codex_override = config.patterns.pack_overrides.get("codex");
        assert!(codex_override.is_some());
        assert_eq!(
            codex_override.unwrap().disabled_rules,
            vec!["codex.usage_warning"]
        );
    }

    #[test]
    fn effective_paths_expand_tilde() {
        let config = Config::default();
        let data_dir = config.effective_data_dir();

        // Should not contain ~
        assert!(!data_dir.to_string_lossy().contains('~'));

        // Should be absolute if HOME is set
        if std::env::var("HOME").is_ok() {
            assert!(data_dir.is_absolute());
        }
    }

    #[test]
    fn effective_db_path_joins_correctly() {
        let mut config = Config::default();
        config.storage.db_path = "test.db".to_string();

        let workspace_root = Path::new("workspace-root");
        let db_path = config.effective_db_path(workspace_root);
        assert_eq!(db_path, workspace_root.join(".wa").join("test.db"));
    }

    #[test]
    fn absolute_db_path_not_joined() {
        let mut config = Config::default();
        config.storage.db_path = "/custom/path/wa.db".to_string();

        let db_path = config.effective_db_path(Path::new("workspace-root"));
        assert_eq!(db_path.to_string_lossy(), "/custom/path/wa.db");
    }

    #[test]
    fn workspace_resolution_prefers_cli_over_env() {
        let cwd = std::env::current_dir().expect("cwd");
        let root = resolve_workspace_root_with_env(
            Some(Path::new("cli-workspace")),
            Some("env-workspace"),
        )
        .expect("resolve");
        assert_eq!(root, cwd.join("cli-workspace"));
    }

    #[test]
    fn workspace_resolution_prefers_env_over_cwd() {
        let cwd = std::env::current_dir().expect("cwd");
        let root = resolve_workspace_root_with_env(None, Some("env-workspace")).expect("resolve");
        assert_eq!(root, cwd.join("env-workspace"));
    }

    #[test]
    fn workspace_resolution_defaults_to_cwd() {
        let cwd = std::env::current_dir().expect("cwd");
        let root = resolve_workspace_root_with_env(None, None).expect("resolve");
        assert_eq!(root, cwd);
    }

    #[test]
    fn workspace_layout_paths_are_scoped() {
        let mut config = Config::default();
        config.storage.db_path = "wa.db".to_string();
        let root = PathBuf::from("workspace-root");
        let layout = WorkspaceLayout::new(root.clone(), &config.storage);

        assert_eq!(layout.root, root);
        assert_eq!(layout.wa_dir, PathBuf::from("workspace-root").join(".wa"));
        assert_eq!(
            layout.db_path,
            PathBuf::from("workspace-root").join(".wa").join("wa.db")
        );
        assert!(layout.lock_path.ends_with("watch.lock"));
        assert!(layout.ipc_socket_path.ends_with("ipc.sock"));
        assert!(layout.log_path.ends_with("wa-watch.log"));
    }

    #[test]
    fn normalize_paths_expands_tilde() {
        if dirs::home_dir().is_none() {
            return;
        }
        let mut config = Config::default();
        config.general.data_dir = "~/wa-data".to_string();
        config.storage.db_path = "~/wa.db".to_string();
        config.normalize_paths();

        assert!(!config.general.data_dir.contains('~'));
        assert!(!config.storage.db_path.contains('~'));
    }

    #[test]
    fn env_overrides_apply_before_cli_overrides() {
        let mut config = Config::default();
        let env_overrides = EnvOverrides {
            log_level: Some("debug".to_string()),
            log_format: None,
            log_file: None,
            storage_db_path: Some("env.db".to_string()),
            metrics_enabled: Some(false),
            metrics_bind: None,
            metrics_prefix: None,
        };
        env_overrides.apply(&mut config);

        let cli_overrides = ConfigOverrides {
            log_level: Some("info".to_string()),
            log_format: None,
            log_file: None,
            storage_db_path: Some("cli.db".to_string()),
            metrics_enabled: Some(true),
            metrics_bind: None,
            metrics_prefix: None,
        };
        cli_overrides.apply(&mut config);

        assert_eq!(config.general.log_level, "info");
        assert_eq!(config.storage.db_path, "cli.db");
        assert!(config.metrics.enabled);
    }

    #[test]
    fn parse_env_bool_accepts_values() {
        assert!(parse_env_bool("true").unwrap());
        assert!(parse_env_bool("1").unwrap());
        assert!(!parse_env_bool("false").unwrap());
        assert!(!parse_env_bool("0").unwrap());
        assert!(parse_env_bool("Yes").unwrap());
        assert!(!parse_env_bool("off").unwrap());
    }

    #[test]
    fn validate_rejects_bad_poll_intervals() {
        let mut config = Config::default();
        config.ingest.min_poll_interval_ms = 100;
        config.ingest.poll_interval_ms = 50;
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("poll_interval_ms"));
    }

    #[test]
    fn compaction_prompt_config_rejects_unknown_placeholder() {
        let mut config = Config::default();
        config.workflows.compaction_prompts.default = "Please review {{unknown_token}}".to_string();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("Unknown placeholder"));
    }

    #[test]
    fn compaction_prompt_config_rejects_empty_prompt() {
        let mut config = Config::default();
        config.workflows.compaction_prompts.default = "   ".to_string();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("compaction_prompts.default"));
    }

    #[test]
    fn redaction_patterns_are_valid_regex() {
        let config = Config::default();
        for pattern in &config.safety.redaction.patterns {
            assert!(
                fancy_regex::Regex::new(pattern).is_ok(),
                "Invalid regex pattern: {pattern}"
            );
        }
    }

    #[test]
    fn safety_defaults_are_conservative() {
        let config = Config::default();

        // Should require prompt by default
        assert!(config.safety.require_prompt_active);

        // Should block alt-screen by default
        assert!(config.safety.block_alt_screen);

        // Should not allow non-agent panes by default
        assert!(!config.safety.capabilities.allow_non_agent_panes);

        // Should require confirmation for dangerous patterns
        assert!(config.safety.capabilities.confirm_dangerous_patterns);

        // Redaction should be enabled
        assert!(config.safety.redaction.enabled);
    }

    // =========================================================================
    // Pane Filter Tests
    // =========================================================================

    #[test]
    fn pane_filter_default_allows_all() {
        let filter = PaneFilterConfig::default();
        assert!(filter.include.is_empty());
        assert!(filter.exclude.is_empty());
        assert!(!filter.has_rules());

        // With no rules, all panes should be observed
        assert!(filter.check_pane("local", "bash", "/home/user").is_none());
        assert!(filter.check_pane("SSH:remote", "vim", "/tmp").is_none());
    }

    #[test]
    fn pane_filter_exclude_wins_over_include() {
        let mut filter = PaneFilterConfig::default();

        // Include all SSH panes
        filter
            .include
            .push(PaneFilterRule::new("include_ssh").with_domain("SSH:*"));

        // But exclude those with private in cwd
        filter
            .exclude
            .push(PaneFilterRule::new("exclude_private").with_cwd("/home/user/private*"));

        // SSH pane in normal cwd - allowed
        assert!(
            filter
                .check_pane("SSH:remote", "bash", "/home/user/work")
                .is_none()
        );

        // SSH pane in private cwd - excluded (exclude wins)
        let result = filter.check_pane("SSH:remote", "bash", "/home/user/private/secrets");
        assert_eq!(result, Some("exclude_private".to_string()));

        // Local pane - excluded (not in include list)
        let result = filter.check_pane("local", "bash", "/home/user");
        assert_eq!(result, Some("no_include_match".to_string()));
    }

    #[test]
    fn pane_filter_rule_domain_exact_match() {
        let rule = PaneFilterRule::new("test_domain").with_domain("local");

        assert!(rule.matches("local", "any", "any"));
        assert!(!rule.matches("LOCAL", "any", "any")); // case-sensitive
        assert!(!rule.matches("local2", "any", "any"));
        assert!(!rule.matches("SSH:local", "any", "any"));
    }

    #[test]
    fn pane_filter_rule_domain_glob() {
        let rule = PaneFilterRule::new("ssh_glob").with_domain("SSH:*");

        assert!(rule.matches("SSH:remote", "any", "any"));
        assert!(rule.matches("SSH:server.example.com", "any", "any"));
        assert!(!rule.matches("local", "any", "any"));
        assert!(!rule.matches("ssh:remote", "any", "any")); // case-sensitive
    }

    #[test]
    fn pane_filter_rule_title_substring() {
        let rule = PaneFilterRule::new("vim_title").with_title("vim");

        assert!(rule.matches("any", "vim", "any"));
        assert!(rule.matches("any", "nvim - file.rs", "any"));
        assert!(rule.matches("any", "VIM", "any")); // case-insensitive
        assert!(rule.matches("any", "using NEOVIM for editing", "any"));
        assert!(!rule.matches("any", "bash", "any"));
    }

    #[test]
    fn pane_filter_rule_title_regex() {
        let rule = PaneFilterRule::new("bash_regex").with_title("re:^bash.*$");

        assert!(rule.matches("any", "bash", "any"));
        assert!(rule.matches("any", "bash --login", "any"));
        assert!(!rule.matches("any", "using bash here", "any")); // regex anchored to start
        assert!(!rule.matches("any", "zsh", "any"));
    }

    #[test]
    fn pane_filter_rule_cwd_prefix() {
        let rule = PaneFilterRule::new("tmp_cwd").with_cwd("/tmp");

        assert!(rule.matches("any", "any", "/tmp"));
        assert!(rule.matches("any", "any", "/tmp/subdir"));
        assert!(rule.matches("any", "any", "/tmp/deep/nested/path"));
        assert!(!rule.matches("any", "any", "/home/tmp"));
        assert!(!rule.matches("any", "any", "/tmpfile"));
    }

    #[test]
    fn pane_filter_rule_cwd_glob() {
        let rule = PaneFilterRule::new("home_glob").with_cwd("/home/*/private");

        assert!(rule.matches("any", "any", "/home/user/private"));
        assert!(rule.matches("any", "any", "/home/admin/private"));
        assert!(!rule.matches("any", "any", "/home/user/public"));
        assert!(!rule.matches("any", "any", "/home/user"));
    }

    #[test]
    fn pane_filter_rule_and_logic() {
        // Rule with multiple matchers uses AND logic
        let rule = PaneFilterRule::new("ssh_vim")
            .with_domain("SSH:*")
            .with_title("vim");

        // Both match - true
        assert!(rule.matches("SSH:remote", "vim editor", "/home"));

        // Only domain matches - false
        assert!(!rule.matches("SSH:remote", "bash", "/home"));

        // Only title matches - false
        assert!(!rule.matches("local", "vim editor", "/home"));
    }

    #[test]
    fn pane_filter_rule_empty_matches_nothing() {
        let rule = PaneFilterRule::default();

        // Rule with no matchers should match nothing
        assert!(!rule.matches("local", "bash", "/home"));
        assert!(!rule.matches("SSH:remote", "vim", "/tmp"));
    }

    #[test]
    fn pane_filter_rule_validation() {
        // Valid rule
        let valid = PaneFilterRule::new("test").with_domain("local");
        assert!(valid.validate().is_ok());

        // Empty ID
        let mut empty_id = PaneFilterRule::new("test").with_domain("local");
        empty_id.id = String::new();
        assert!(empty_id.validate().is_err());

        // No matchers
        let no_matchers = PaneFilterRule::new("test");
        assert!(no_matchers.validate().is_err());

        // Invalid regex
        let invalid_regex = PaneFilterRule::new("test").with_title("re:[invalid(regex");
        assert!(invalid_regex.validate().is_err());
    }

    #[test]
    fn pane_filter_config_toml_roundtrip() {
        let toml = r#"
[ingest]
poll_interval_ms = 100

[ingest.panes]
[[ingest.panes.include]]
id = "observe_ssh"
domain = "SSH:*"

[[ingest.panes.exclude]]
id = "skip_private"
cwd = "/home/*/private"

[[ingest.panes.exclude]]
id = "skip_vim"
title = "vim"
"#;
        let config = Config::from_toml(toml).expect("Failed to parse");

        assert_eq!(config.ingest.poll_interval_ms, 100);
        assert_eq!(config.ingest.panes.include.len(), 1);
        assert_eq!(config.ingest.panes.exclude.len(), 2);

        let include = &config.ingest.panes.include[0];
        assert_eq!(include.id, "observe_ssh");
        assert_eq!(include.domain, Some("SSH:*".to_string()));

        let exclude1 = &config.ingest.panes.exclude[0];
        assert_eq!(exclude1.id, "skip_private");
        assert_eq!(exclude1.cwd, Some("/home/*/private".to_string()));

        let exclude2 = &config.ingest.panes.exclude[1];
        assert_eq!(exclude2.id, "skip_vim");
        assert_eq!(exclude2.title, Some("vim".to_string()));
    }

    #[test]
    fn pane_filter_config_serialization() {
        let mut config = Config::default();
        config
            .ingest
            .panes
            .include
            .push(PaneFilterRule::new("test_include").with_domain("local"));
        config
            .ingest
            .panes
            .exclude
            .push(PaneFilterRule::new("test_exclude").with_cwd("/tmp"));

        let toml = config.to_toml().expect("Failed to serialize");
        let parsed = Config::from_toml(&toml).expect("Failed to parse");

        assert_eq!(parsed.ingest.panes.include.len(), 1);
        assert_eq!(parsed.ingest.panes.exclude.len(), 1);
        assert_eq!(parsed.ingest.panes.include[0].id, "test_include");
        assert_eq!(parsed.ingest.panes.exclude[0].id, "test_exclude");
    }

    #[test]
    fn pane_filter_glob_special_chars() {
        // Test that special regex characters in domain/cwd are properly escaped
        let rule = PaneFilterRule::new("special").with_domain("domain.with.dots");

        assert!(rule.matches("domain.with.dots", "any", "any"));
        assert!(!rule.matches("domainXwithXdots", "any", "any"));
    }

    #[test]
    fn pane_filter_question_mark_glob() {
        let rule = PaneFilterRule::new("single_char").with_domain("SSH:?");

        assert!(rule.matches("SSH:a", "any", "any"));
        assert!(rule.matches("SSH:1", "any", "any"));
        assert!(!rule.matches("SSH:ab", "any", "any"));
        assert!(!rule.matches("SSH:", "any", "any"));
    }

    #[cfg(unix)]
    #[test]
    fn ensure_dir_sets_secure_permissions() {
        let dir = std::env::temp_dir().join(format!(
            "wa_perm_dir_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        ensure_dir(&dir).expect("ensure_dir");

        let mode = std::fs::metadata(&dir)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn collect_permission_warnings_flags_open_modes() {
        let root = std::env::temp_dir().join(format!(
            "wa_perm_root_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);

        let config = Config::default();
        let layout = WorkspaceLayout::new(root.clone(), &config.storage);

        std::fs::create_dir_all(&layout.wa_dir).expect("create wa_dir");
        std::fs::set_permissions(&layout.wa_dir, std::fs::Permissions::from_mode(0o755))
            .expect("set wa_dir perms");

        std::fs::create_dir_all(layout.db_path.parent().expect("db parent"))
            .expect("create db parent");
        std::fs::File::create(&layout.db_path).expect("create db file");
        std::fs::set_permissions(&layout.db_path, std::fs::Permissions::from_mode(0o644))
            .expect("set db perms");

        let warnings = collect_permission_warnings(&layout, None, None);
        assert!(warnings.iter().any(|w| w.label == "workspace dir"));
        assert!(warnings.iter().any(|w| w.label == "database"));

        let _ = std::fs::remove_file(&layout.db_path);
        let _ = std::fs::remove_dir_all(&root);
    }

    // ==========================================================================
    // Hot Reload Tests
    // ==========================================================================

    #[test]
    fn hot_reload_allows_poll_interval_change() {
        let config1 = Config::default();
        let mut config2 = Config::default();
        config2.ingest.poll_interval_ms = 500;

        let result = config1.diff_for_hot_reload(&config2);

        assert!(result.allowed);
        assert_eq!(result.changes.len(), 1);
        assert_eq!(result.changes[0].name, "ingest.poll_interval_ms");
        assert_eq!(result.changes[0].old_value, "200");
        assert_eq!(result.changes[0].new_value, "500");
        assert!(result.forbidden.is_empty());
    }

    #[test]
    fn hot_reload_allows_log_level_change() {
        let config1 = Config::default();
        let mut config2 = Config::default();
        config2.general.log_level = "debug".to_string();

        let result = config1.diff_for_hot_reload(&config2);

        assert!(result.allowed);
        assert_eq!(result.changes.len(), 1);
        assert_eq!(result.changes[0].name, "general.log_level");
        assert_eq!(result.changes[0].old_value, "info");
        assert_eq!(result.changes[0].new_value, "debug");
    }

    #[test]
    fn hot_reload_allows_retention_change() {
        let config1 = Config::default();
        let mut config2 = Config::default();
        config2.storage.retention_days = 60;

        let result = config1.diff_for_hot_reload(&config2);

        assert!(result.allowed);
        assert_eq!(result.changes.len(), 1);
        assert_eq!(result.changes[0].name, "storage.retention_days");
    }

    #[test]
    fn hot_reload_allows_pattern_packs_change() {
        let config1 = Config::default();
        let mut config2 = Config::default();
        config2.patterns.packs = vec!["builtin:core".to_string()];

        let result = config1.diff_for_hot_reload(&config2);

        assert!(result.allowed);
        assert!(result.changes.iter().any(|c| c.name == "patterns.packs"));
    }

    #[test]
    fn hot_reload_forbids_db_path_change() {
        let config1 = Config::default();
        let mut config2 = Config::default();
        config2.storage.db_path = "/new/path/wa.db".to_string();

        let result = config1.diff_for_hot_reload(&config2);

        assert!(!result.allowed);
        assert_eq!(result.forbidden.len(), 1);
        assert_eq!(result.forbidden[0].name, "storage.db_path");
        assert!(
            result.forbidden[0]
                .reason
                .contains("cannot be changed at runtime")
        );
    }

    #[test]
    fn hot_reload_forbids_data_dir_change() {
        let config1 = Config::default();
        let mut config2 = Config::default();
        config2.general.data_dir = "/new/data/dir".to_string();

        let result = config1.diff_for_hot_reload(&config2);

        assert!(!result.allowed);
        assert!(
            result
                .forbidden
                .iter()
                .any(|f| f.name == "general.data_dir")
        );
    }

    #[test]
    fn hot_reload_forbids_writer_queue_size_change() {
        let config1 = Config::default();
        let mut config2 = Config::default();
        config2.storage.writer_queue_size = 50000;

        let result = config1.diff_for_hot_reload(&config2);

        assert!(!result.allowed);
        assert!(
            result
                .forbidden
                .iter()
                .any(|f| f.name == "storage.writer_queue_size")
        );
    }

    #[test]
    fn hot_reload_no_changes_detected() {
        let config1 = Config::default();
        let config2 = Config::default();

        let result = config1.diff_for_hot_reload(&config2);

        assert!(result.allowed);
        assert!(result.changes.is_empty());
        assert!(result.forbidden.is_empty());
    }

    #[test]
    fn hot_reload_multiple_allowed_changes() {
        let config1 = Config::default();
        let mut config2 = Config::default();
        config2.general.log_level = "debug".to_string();
        config2.ingest.poll_interval_ms = 500;
        config2.storage.retention_days = 60;

        let result = config1.diff_for_hot_reload(&config2);

        assert!(result.allowed);
        assert_eq!(result.changes.len(), 3);
        assert!(result.changes.iter().any(|c| c.name == "general.log_level"));
        assert!(
            result
                .changes
                .iter()
                .any(|c| c.name == "ingest.poll_interval_ms")
        );
        assert!(
            result
                .changes
                .iter()
                .any(|c| c.name == "storage.retention_days")
        );
    }

    #[test]
    fn hot_reload_mixed_allowed_and_forbidden() {
        let config1 = Config::default();
        let mut config2 = Config::default();
        config2.general.log_level = "debug".to_string(); // Allowed
        config2.storage.db_path = "/new/path/wa.db".to_string(); // Forbidden

        let result = config1.diff_for_hot_reload(&config2);

        // Should be forbidden overall
        assert!(!result.allowed);
        // But should still report what would have been allowed
        assert!(result.changes.iter().any(|c| c.name == "general.log_level"));
        assert!(result.forbidden.iter().any(|f| f.name == "storage.db_path"));
    }

    #[test]
    fn hot_reloadable_config_extracts_correctly() {
        let mut config = Config::default();
        config.general.log_level = "debug".to_string();
        config.ingest.poll_interval_ms = 500;
        config.storage.retention_days = 45;
        config.patterns.packs = vec!["builtin:core".to_string()];

        let hot = config.hot_reloadable();

        assert_eq!(hot.log_level, "debug");
        assert_eq!(hot.poll_interval_ms, 500);
        assert_eq!(hot.retention_days, 45);
        assert_eq!(hot.pattern_packs, vec!["builtin:core".to_string()]);
    }

    #[test]
    fn hot_reload_result_display_format() {
        let config1 = Config::default();
        let mut config2 = Config::default();
        config2.ingest.poll_interval_ms = 500;
        config2.storage.db_path = "/forbidden/path".to_string();

        let result = config1.diff_for_hot_reload(&config2);
        let output = format!("{result}");

        assert!(output.contains("Forbidden changes"));
        assert!(output.contains("storage.db_path"));
        assert!(output.contains("Hot-reloadable changes"));
        assert!(output.contains("ingest.poll_interval_ms"));
    }

    // ========================================================================
    // NotificationConfig tests (wa-psm.3)
    // ========================================================================

    #[test]
    fn notification_config_defaults() {
        let nc = NotificationConfig::default();
        assert!(nc.enabled);
        assert_eq!(nc.cooldown_ms, 30_000);
        assert_eq!(nc.dedup_window_ms, 300_000);
        assert!(nc.include.is_empty());
        assert!(nc.exclude.is_empty());
        assert!(nc.min_severity.is_none());
        assert!(nc.agent_types.is_empty());
    }

    #[test]
    fn notification_config_in_default_config() {
        let config = Config::default();
        assert!(config.notifications.enabled);
    }

    #[test]
    fn notification_config_toml_roundtrip() {
        let toml_str = r#"
[notifications]
enabled = true
cooldown_ms = 5000
dedup_window_ms = 60000
include = ["*.error", "codex.*"]
exclude = ["test.*"]
min_severity = "warning"
agent_types = ["codex", "claude_code"]
"#;
        let config: Config = toml::from_str(toml_str).expect("parse");
        assert!(config.notifications.enabled);
        assert_eq!(config.notifications.cooldown_ms, 5000);
        assert_eq!(config.notifications.dedup_window_ms, 60000);
        assert_eq!(config.notifications.include, vec!["*.error", "codex.*"]);
        assert_eq!(config.notifications.exclude, vec!["test.*"]);
        assert_eq!(
            config.notifications.min_severity,
            Some("warning".to_string())
        );
        assert_eq!(
            config.notifications.agent_types,
            vec!["codex", "claude_code"]
        );
    }

    #[test]
    fn notification_config_builds_event_filter() {
        let nc = NotificationConfig {
            enabled: true,
            cooldown_ms: 1000,
            dedup_window_ms: 5000,
            include: vec!["codex.*".to_string()],
            exclude: vec!["*.debug".to_string()],
            min_severity: Some("warning".to_string()),
            agent_types: vec!["codex".to_string()],
            webhooks: Vec::new(),
            desktop: crate::desktop_notify::DesktopNotifyConfig::default(),
        };
        let filter = nc.to_event_filter();
        assert!(!filter.is_permissive());
    }

    #[test]
    fn notification_config_builds_gate() {
        let nc = NotificationConfig::default();
        let _gate = nc.to_notification_gate();
        // Smoke test: gate creation doesn't panic
    }

    #[test]
    fn notification_config_missing_section_uses_defaults() {
        // Config with no [notifications] section
        let toml_str = r#"
[general]
log_level = "debug"
"#;
        let config: Config = toml::from_str(toml_str).expect("parse");
        assert!(config.notifications.enabled);
        assert_eq!(config.notifications.cooldown_ms, 30_000);
    }

    #[test]
    fn default_config_serializes_notifications_section() {
        let config = Config::default();
        let toml = config.to_toml().expect("Failed to serialize");
        assert!(toml.contains("[notifications]"));
    }
}
