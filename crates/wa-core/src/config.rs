//! Configuration management for wa
//!
//! Handles loading and validation of wa.toml configuration files.

use serde::{Deserialize, Serialize};

/// Main configuration structure
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    /// General settings
    #[serde(default)]
    pub general: GeneralConfig,

    /// Ingest settings
    #[serde(default)]
    pub ingest: IngestConfig,

    /// Storage settings
    #[serde(default)]
    pub storage: StorageConfig,

    /// Pattern settings
    #[serde(default)]
    pub patterns: PatternsConfig,

    /// Workflow settings
    #[serde(default)]
    pub workflows: WorkflowsConfig,

    /// Safety settings
    #[serde(default)]
    pub safety: SafetyConfig,
}

/// General configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneralConfig {
    /// Log level (trace, debug, info, warn, error)
    #[serde(default = "default_log_level")]
    pub log_level: String,

    /// Data directory path
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            log_level: default_log_level(),
            data_dir: default_data_dir(),
        }
    }
}

fn default_log_level() -> String {
    "info".to_string()
}

fn default_data_dir() -> String {
    "~/.local/share/wa".to_string()
}

/// Ingest configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestConfig {
    /// Poll interval in milliseconds
    #[serde(default = "default_poll_interval")]
    pub poll_interval_ms: u64,

    /// Enable gap detection
    #[serde(default = "default_true")]
    pub gap_detection: bool,
}

impl Default for IngestConfig {
    fn default() -> Self {
        Self {
            poll_interval_ms: default_poll_interval(),
            gap_detection: true,
        }
    }
}

fn default_poll_interval() -> u64 {
    200
}

fn default_true() -> bool {
    true
}

/// Storage configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    /// Database file path
    #[serde(default = "default_db_path")]
    pub db_path: String,

    /// Retention period in days
    #[serde(default = "default_retention_days")]
    pub retention_days: u32,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            db_path: default_db_path(),
            retention_days: default_retention_days(),
        }
    }
}

fn default_db_path() -> String {
    "~/.local/share/wa/wa.db".to_string()
}

fn default_retention_days() -> u32 {
    30
}

/// Patterns configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatternsConfig {
    /// Enabled pattern packs
    #[serde(default = "default_packs")]
    pub packs: Vec<String>,
}

impl Default for PatternsConfig {
    fn default() -> Self {
        Self {
            packs: default_packs(),
        }
    }
}

fn default_packs() -> Vec<String> {
    vec![
        "builtin:core".to_string(),
        "builtin:codex".to_string(),
        "builtin:claude_code".to_string(),
        "builtin:gemini".to_string(),
    ]
}

/// Workflows configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowsConfig {
    /// Enabled workflows
    #[serde(default = "default_workflows")]
    pub enabled: Vec<String>,

    /// Maximum concurrent workflows
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: u32,
}

impl Default for WorkflowsConfig {
    fn default() -> Self {
        Self {
            enabled: default_workflows(),
            max_concurrent: default_max_concurrent(),
        }
    }
}

fn default_workflows() -> Vec<String> {
    vec![
        "handle_compaction".to_string(),
        "handle_usage_limits".to_string(),
    ]
}

fn default_max_concurrent() -> u32 {
    3
}

/// Safety configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafetyConfig {
    /// Rate limit per pane (sends per minute)
    #[serde(default = "default_rate_limit")]
    pub rate_limit_per_pane: u32,

    /// Require prompt to be active before sending
    #[serde(default = "default_true")]
    pub require_prompt_active: bool,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            rate_limit_per_pane: default_rate_limit(),
            require_prompt_active: true,
        }
    }
}

fn default_rate_limit() -> u32 {
    30
}

impl Config {
    /// Load configuration from default locations
    pub fn load() -> crate::Result<Self> {
        // TODO: Implement config file loading
        Ok(Self::default())
    }

    /// Load configuration from a specific path
    pub fn load_from(_path: &std::path::Path) -> crate::Result<Self> {
        // TODO: Implement config file loading
        Ok(Self::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        let config = Config::default();
        assert_eq!(config.general.log_level, "info");
        assert_eq!(config.ingest.poll_interval_ms, 200);
        assert!(config.safety.require_prompt_active);
    }
}
