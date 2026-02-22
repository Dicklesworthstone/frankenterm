//! Session resume orchestrator — bridges FrankenTerm ↔ `casr` CLI.
//!
//! Wraps `cross_agent_session_resumer` subprocess calls for discovering,
//! resuming, and exporting agent sessions across providers (Claude Code,
//! Codex, Gemini, etc.).
//!
//! Feature-gated behind `session-resume`.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::casr_types::{
    CasrListEntry, CasrProviderStatus, CasrResumeOutput, CanonicalMessage, CanonicalSession,
};

// =============================================================================
// Agent provider enum
// =============================================================================

/// Known AI agent providers supported by casr.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentProvider {
    ClaudeCode,
    Codex,
    Gemini,
    Grok,
    /// Provider not in the known set.
    Other(String),
}

impl AgentProvider {
    /// The casr CLI slug for this provider.
    pub fn slug(&self) -> &str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::Grok => "grok",
            Self::Other(s) => s,
        }
    }

    /// Parse a slug string into an [`AgentProvider`].
    pub fn from_slug(slug: &str) -> Self {
        match slug {
            "claude-code" | "cc" => Self::ClaudeCode,
            "codex" | "cod" => Self::Codex,
            "gemini" | "gmi" => Self::Gemini,
            "grok" => Self::Grok,
            other => Self::Other(other.to_string()),
        }
    }
}

impl std::fmt::Display for AgentProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.slug())
    }
}

// =============================================================================
// Session resume config
// =============================================================================

/// Configuration for the session resume bridge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionResumeConfig {
    /// Path to the `casr` binary. Defaults to `"casr"` (found via PATH).
    #[serde(default = "default_casr_binary")]
    pub casr_binary: String,
    /// Working directory for subprocess calls (defaults to cwd).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<PathBuf>,
    /// Timeout in seconds for subprocess calls.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Whether to use dry-run mode by default.
    #[serde(default)]
    pub dry_run: bool,
}

fn default_casr_binary() -> String {
    "casr".to_string()
}

fn default_timeout_secs() -> u64 {
    30
}

impl Default for SessionResumeConfig {
    fn default() -> Self {
        Self {
            casr_binary: default_casr_binary(),
            working_dir: None,
            timeout_secs: default_timeout_secs(),
            dry_run: false,
        }
    }
}

// =============================================================================
// Recorder CASR export
// =============================================================================

/// Recorder data exported in CASR-compatible format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecorderCasrExport {
    /// Session metadata.
    pub session: CanonicalSession,
    /// Export generation timestamp (epoch ms).
    pub exported_at: i64,
    /// Source recorder pane IDs included.
    pub pane_ids: Vec<u64>,
    /// Total events processed.
    pub events_processed: usize,
    /// Warnings generated during export.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

// =============================================================================
// Session resume orchestrator
// =============================================================================

/// Orchestrates session discovery, resume, and export via the `casr` CLI.
#[derive(Debug, Clone)]
pub struct SessionResumer {
    config: SessionResumeConfig,
}

/// Error type for session resume operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionResumeError {
    /// The casr binary was not found or not executable.
    CasrNotFound(String),
    /// The subprocess exited with a non-zero code.
    SubprocessFailed { code: Option<i32>, stderr: String },
    /// Failed to parse JSON output from casr.
    ParseError(String),
    /// The requested session was not found.
    SessionNotFound(String),
    /// Provider is not installed.
    ProviderNotInstalled(String),
    /// Operation was cancelled or timed out.
    Timeout,
}

impl std::fmt::Display for SessionResumeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CasrNotFound(msg) => write!(f, "casr not found: {}", msg),
            Self::SubprocessFailed { code, stderr } => {
                write!(f, "casr failed (exit {}): {}", code.unwrap_or(-1), stderr)
            }
            Self::ParseError(msg) => write!(f, "casr parse error: {}", msg),
            Self::SessionNotFound(id) => write!(f, "session not found: {}", id),
            Self::ProviderNotInstalled(slug) => {
                write!(f, "provider not installed: {}", slug)
            }
            Self::Timeout => write!(f, "casr operation timed out"),
        }
    }
}

impl std::error::Error for SessionResumeError {}

impl SessionResumer {
    /// Create a new resumer with the given config.
    pub fn new(config: SessionResumeConfig) -> Self {
        Self { config }
    }

    /// Create a resumer with default config.
    pub fn with_defaults() -> Self {
        Self::new(SessionResumeConfig::default())
    }

    /// Access the config.
    pub fn config(&self) -> &SessionResumeConfig {
        &self.config
    }

    /// Discover sessions across all installed providers.
    ///
    /// Calls `casr list --json` and parses the output.
    pub fn discover_sessions(&self) -> Result<Vec<CasrListEntry>, SessionResumeError> {
        info!(session_resume = true, "discovering sessions via casr list");

        let output = self.run_casr(&["list", "--json"])?;
        let entries: Vec<CasrListEntry> =
            serde_json::from_str(&output).map_err(|e| SessionResumeError::ParseError(e.to_string()))?;

        info!(
            session_resume = true,
            sessions_found = entries.len(),
            "discovered sessions"
        );
        Ok(entries)
    }

    /// Discover sessions filtered by provider.
    pub fn discover_sessions_for_provider(
        &self,
        provider: &AgentProvider,
    ) -> Result<Vec<CasrListEntry>, SessionResumeError> {
        let all = self.discover_sessions()?;
        let slug = provider.slug();
        Ok(all
            .into_iter()
            .filter(|e| e.provider.as_deref() == Some(slug))
            .collect())
    }

    /// Resume a session into a target provider.
    ///
    /// Calls `casr resume <session_id> --target <provider> --json`.
    pub fn resume_session(
        &self,
        session_id: &str,
        target_provider: &AgentProvider,
    ) -> Result<CasrResumeOutput, SessionResumeError> {
        info!(
            session_resume = true,
            source_session_id = %session_id,
            target_provider = %target_provider,
            dry_run = self.config.dry_run,
            "resuming session"
        );

        let mut args = vec![
            "resume",
            session_id,
            "--target",
            target_provider.slug(),
            "--json",
        ];
        if self.config.dry_run {
            args.push("--dry-run");
        }

        let output = self.run_casr(&args)?;
        let result: CasrResumeOutput =
            serde_json::from_str(&output).map_err(|e| SessionResumeError::ParseError(e.to_string()))?;

        if !result.ok {
            warn!(
                session_resume = true,
                session_id = %session_id,
                "resume reported failure"
            );
        }

        Ok(result)
    }

    /// List installed providers.
    ///
    /// Calls `casr providers --json`.
    pub fn list_providers(&self) -> Result<Vec<CasrProviderStatus>, SessionResumeError> {
        let output = self.run_casr(&["providers", "--json"])?;
        let providers: Vec<CasrProviderStatus> =
            serde_json::from_str(&output).map_err(|e| SessionResumeError::ParseError(e.to_string()))?;
        Ok(providers)
    }

    /// Check if a specific provider is installed.
    pub fn is_provider_installed(
        &self,
        provider: &AgentProvider,
    ) -> Result<bool, SessionResumeError> {
        let providers = self.list_providers()?;
        let slug = provider.slug();
        Ok(providers.iter().any(|p| p.slug == slug && p.installed))
    }

    /// Check if `casr` is available on PATH.
    pub fn is_casr_available(&self) -> bool {
        Command::new(&self.config.casr_binary)
            .arg("--version")
            .output()
            .is_ok()
    }

    /// Export recorder data as a CASR-compatible session.
    ///
    /// Converts recorder events into the canonical IR format for portability.
    pub fn export_for_recorder(
        &self,
        session_id: &str,
        provider_slug: &str,
        source_path: &Path,
        messages: Vec<CanonicalMessage>,
        pane_ids: Vec<u64>,
    ) -> RecorderCasrExport {
        let now_ms = chrono::Utc::now().timestamp_millis();

        let session = CanonicalSession {
            session_id: session_id.to_string(),
            provider_slug: provider_slug.to_string(),
            workspace: self.config.working_dir.clone(),
            title: None,
            started_at: messages.first().and_then(|m| m.timestamp),
            ended_at: messages.last().and_then(|m| m.timestamp),
            messages,
            metadata: serde_json::Value::Object(serde_json::Map::new()),
            source_path: source_path.to_path_buf(),
            model_name: None,
        };

        let events_processed = session.messages.len();

        RecorderCasrExport {
            session,
            exported_at: now_ms,
            pane_ids,
            events_processed,
            warnings: vec![],
        }
    }

    /// Run a casr subprocess and return stdout on success.
    fn run_casr(&self, args: &[&str]) -> Result<String, SessionResumeError> {
        let mut cmd = Command::new(&self.config.casr_binary);
        cmd.args(args);

        if let Some(ref dir) = self.config.working_dir {
            cmd.current_dir(dir);
        }

        let output = cmd.output().map_err(|e| {
            SessionResumeError::CasrNotFound(format!(
                "{}: {}",
                self.config.casr_binary, e
            ))
        })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            return Err(SessionResumeError::SubprocessFailed {
                code: output.status.code(),
                stderr,
            });
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }
}

/// Fail-open: if casr is unavailable, return empty results instead of errors.
pub fn discover_sessions_failopen(config: &SessionResumeConfig) -> Vec<CasrListEntry> {
    let resumer = SessionResumer::new(config.clone());
    match resumer.discover_sessions() {
        Ok(entries) => entries,
        Err(e) => {
            warn!(
                session_resume = true,
                error = %e,
                "casr unavailable, failing open with empty session list"
            );
            vec![]
        }
    }
}

/// Map a casr provider slug to an [`AgentProvider`].
pub fn provider_from_list_entry(entry: &CasrListEntry) -> AgentProvider {
    match &entry.provider {
        Some(slug) => AgentProvider::from_slug(slug),
        None => AgentProvider::Other("unknown".to_string()),
    }
}

/// Build a summary line for a list entry (for TUI/CLI display).
pub fn summarize_entry(entry: &CasrListEntry) -> String {
    let provider = entry.provider.as_deref().unwrap_or("?");
    let title = entry
        .title
        .as_deref()
        .unwrap_or("(untitled)")
        .chars()
        .take(60)
        .collect::<String>();
    let msgs = entry.messages;
    format!("[{}] {} ({} msgs) — {}", provider, entry.session_id, msgs, title)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use crate::casr_types::MessageRole;
    use serde_json::json;

    // -- AgentProvider --

    #[test]
    fn agent_provider_slug_roundtrip() {
        let providers = vec![
            AgentProvider::ClaudeCode,
            AgentProvider::Codex,
            AgentProvider::Gemini,
            AgentProvider::Grok,
            AgentProvider::Other("custom".into()),
        ];
        for p in providers {
            let slug = p.slug();
            let rt = AgentProvider::from_slug(slug);
            assert_eq!(p, rt);
        }
    }

    #[test]
    fn agent_provider_aliases() {
        assert_eq!(AgentProvider::from_slug("cc"), AgentProvider::ClaudeCode);
        assert_eq!(AgentProvider::from_slug("cod"), AgentProvider::Codex);
        assert_eq!(AgentProvider::from_slug("gmi"), AgentProvider::Gemini);
    }

    #[test]
    fn agent_provider_unknown_slug() {
        let p = AgentProvider::from_slug("future-agent");
        assert_eq!(p, AgentProvider::Other("future-agent".into()));
        assert_eq!(p.slug(), "future-agent");
    }

    #[test]
    fn agent_provider_display() {
        assert_eq!(AgentProvider::ClaudeCode.to_string(), "claude-code");
        assert_eq!(AgentProvider::Codex.to_string(), "codex");
    }

    #[test]
    fn agent_provider_serde_roundtrip() {
        let p = AgentProvider::ClaudeCode;
        let json_str = serde_json::to_string(&p).unwrap();
        let rt: AgentProvider = serde_json::from_str(&json_str).unwrap();
        assert_eq!(p, rt);
    }

    #[test]
    fn agent_provider_other_serde_roundtrip() {
        let p = AgentProvider::Other("custom-x".into());
        let json_str = serde_json::to_string(&p).unwrap();
        let rt: AgentProvider = serde_json::from_str(&json_str).unwrap();
        assert_eq!(p, rt);
    }

    // -- SessionResumeConfig --

    #[test]
    fn config_default() {
        let c = SessionResumeConfig::default();
        assert_eq!(c.casr_binary, "casr");
        assert_eq!(c.timeout_secs, 30);
        assert!(!c.dry_run);
        assert!(c.working_dir.is_none());
    }

    #[test]
    fn config_serde_roundtrip() {
        let c = SessionResumeConfig {
            casr_binary: "/usr/bin/casr".into(),
            working_dir: Some(PathBuf::from("/project")),
            timeout_secs: 60,
            dry_run: true,
        };
        let json_str = serde_json::to_string(&c).unwrap();
        let rt: SessionResumeConfig = serde_json::from_str(&json_str).unwrap();
        assert_eq!(rt.casr_binary, "/usr/bin/casr");
        assert_eq!(rt.timeout_secs, 60);
        assert!(rt.dry_run);
    }

    #[test]
    fn config_serde_defaults() {
        let json_str = "{}";
        let c: SessionResumeConfig = serde_json::from_str(json_str).unwrap();
        assert_eq!(c.casr_binary, "casr");
        assert_eq!(c.timeout_secs, 30);
        assert!(!c.dry_run);
    }

    // -- SessionResumer --

    #[test]
    fn resumer_with_defaults() {
        let r = SessionResumer::with_defaults();
        assert_eq!(r.config().casr_binary, "casr");
    }

    #[test]
    fn resumer_casr_not_available() {
        let r = SessionResumer::new(SessionResumeConfig {
            casr_binary: "/nonexistent/casr-binary-that-does-not-exist".into(),
            ..Default::default()
        });
        assert!(!r.is_casr_available());
    }

    #[test]
    fn resumer_discover_fails_gracefully_when_binary_missing() {
        let r = SessionResumer::new(SessionResumeConfig {
            casr_binary: "/nonexistent/casr-binary-that-does-not-exist".into(),
            ..Default::default()
        });
        let result = r.discover_sessions();
        assert!(result.is_err());
        match result.unwrap_err() {
            SessionResumeError::CasrNotFound(_) => {}
            other => panic!("expected CasrNotFound, got: {}", other),
        }
    }

    #[test]
    fn resumer_list_providers_fails_gracefully() {
        let r = SessionResumer::new(SessionResumeConfig {
            casr_binary: "/nonexistent/casr-binary-that-does-not-exist".into(),
            ..Default::default()
        });
        let result = r.list_providers();
        assert!(result.is_err());
    }

    #[test]
    fn resumer_resume_fails_gracefully() {
        let r = SessionResumer::new(SessionResumeConfig {
            casr_binary: "/nonexistent/casr-binary-that-does-not-exist".into(),
            ..Default::default()
        });
        let result = r.resume_session("sess-1", &AgentProvider::Codex);
        assert!(result.is_err());
    }

    // -- RecorderCasrExport --

    #[test]
    fn export_for_recorder_basic() {
        let r = SessionResumer::with_defaults();
        let messages = vec![CanonicalMessage {
            idx: 0,
            role: MessageRole::User,
            content: "hello".into(),
            timestamp: Some(1_700_000_000_000),
            author: None,
            tool_calls: vec![],
            tool_results: vec![],
            extra: json!({}),
        }];
        let export = r.export_for_recorder(
            "sess-1",
            "claude-code",
            Path::new("/tmp/src.jsonl"),
            messages,
            vec![1, 2],
        );
        assert_eq!(export.session.session_id, "sess-1");
        assert_eq!(export.pane_ids, vec![1, 2]);
        assert_eq!(export.events_processed, 1);
        assert!(export.warnings.is_empty());
    }

    #[test]
    fn export_for_recorder_empty_messages() {
        let r = SessionResumer::with_defaults();
        let export = r.export_for_recorder(
            "sess-2",
            "codex",
            Path::new("/tmp/x"),
            vec![],
            vec![],
        );
        assert_eq!(export.events_processed, 0);
        assert!(export.session.started_at.is_none());
        assert!(export.session.ended_at.is_none());
    }

    #[test]
    fn export_for_recorder_timestamps() {
        let r = SessionResumer::with_defaults();
        let messages = vec![
            CanonicalMessage {
                idx: 0,
                role: MessageRole::User,
                content: "a".into(),
                timestamp: Some(100),
                author: None,
                tool_calls: vec![],
                tool_results: vec![],
                extra: json!({}),
            },
            CanonicalMessage {
                idx: 1,
                role: MessageRole::Assistant,
                content: "b".into(),
                timestamp: Some(200),
                author: None,
                tool_calls: vec![],
                tool_results: vec![],
                extra: json!({}),
            },
        ];
        let export = r.export_for_recorder(
            "s",
            "cc",
            Path::new("/x"),
            messages,
            vec![],
        );
        assert_eq!(export.session.started_at, Some(100));
        assert_eq!(export.session.ended_at, Some(200));
    }

    #[test]
    fn export_serde_roundtrip() {
        let r = SessionResumer::with_defaults();
        let export = r.export_for_recorder(
            "s1",
            "codex",
            Path::new("/tmp/x"),
            vec![],
            vec![42],
        );
        let json_str = serde_json::to_string(&export).unwrap();
        let rt: RecorderCasrExport = serde_json::from_str(&json_str).unwrap();
        assert_eq!(rt.session.session_id, "s1");
        assert_eq!(rt.pane_ids, vec![42]);
    }

    // -- SessionResumeError --

    #[test]
    fn error_display() {
        let e = SessionResumeError::CasrNotFound("no binary".into());
        assert!(e.to_string().contains("casr not found"));

        let e = SessionResumeError::SubprocessFailed {
            code: Some(1),
            stderr: "fail".into(),
        };
        assert!(e.to_string().contains("exit 1"));

        let e = SessionResumeError::ParseError("bad json".into());
        assert!(e.to_string().contains("parse error"));

        let e = SessionResumeError::SessionNotFound("abc".into());
        assert!(e.to_string().contains("abc"));

        let e = SessionResumeError::ProviderNotInstalled("codex".into());
        assert!(e.to_string().contains("codex"));

        let e = SessionResumeError::Timeout;
        assert!(e.to_string().contains("timed out"));
    }

    #[test]
    fn error_is_std_error() {
        let e: Box<dyn std::error::Error> =
            Box::new(SessionResumeError::Timeout);
        assert!(!e.to_string().is_empty());
    }

    // -- Helper functions --

    #[test]
    fn discover_sessions_failopen_returns_empty() {
        let config = SessionResumeConfig {
            casr_binary: "/nonexistent/casr-binary-that-does-not-exist".into(),
            ..Default::default()
        };
        let result = discover_sessions_failopen(&config);
        assert!(result.is_empty());
    }

    #[test]
    fn provider_from_list_entry_known() {
        let entry = CasrListEntry {
            session_id: "s1".into(),
            provider: Some("claude-code".into()),
            title: None,
            messages: 0,
            workspace: None,
            started_at: None,
            path: None,
            extra: HashMap::new(),
        };
        assert_eq!(provider_from_list_entry(&entry), AgentProvider::ClaudeCode);
    }

    #[test]
    fn provider_from_list_entry_none() {
        let entry = CasrListEntry {
            session_id: "s1".into(),
            provider: None,
            title: None,
            messages: 0,
            workspace: None,
            started_at: None,
            path: None,
            extra: HashMap::new(),
        };
        assert_eq!(
            provider_from_list_entry(&entry),
            AgentProvider::Other("unknown".into())
        );
    }

    #[test]
    fn summarize_entry_full() {
        let entry = CasrListEntry {
            session_id: "abc-123".into(),
            provider: Some("codex".into()),
            title: Some("Fix the bug".into()),
            messages: 42,
            workspace: None,
            started_at: None,
            path: None,
            extra: HashMap::new(),
        };
        let summary = summarize_entry(&entry);
        assert!(summary.contains("codex"));
        assert!(summary.contains("abc-123"));
        assert!(summary.contains("42 msgs"));
        assert!(summary.contains("Fix the bug"));
    }

    #[test]
    fn summarize_entry_missing_fields() {
        let entry = CasrListEntry {
            session_id: "s1".into(),
            provider: None,
            title: None,
            messages: 0,
            workspace: None,
            started_at: None,
            path: None,
            extra: HashMap::new(),
        };
        let summary = summarize_entry(&entry);
        assert!(summary.contains("?"));
        assert!(summary.contains("(untitled)"));
    }

    #[test]
    fn summarize_entry_long_title_truncated() {
        let long_title = "a".repeat(200);
        let entry = CasrListEntry {
            session_id: "s1".into(),
            provider: Some("cc".into()),
            title: Some(long_title),
            messages: 1,
            workspace: None,
            started_at: None,
            path: None,
            extra: HashMap::new(),
        };
        let summary = summarize_entry(&entry);
        // Title should be truncated to 60 chars
        assert!(summary.len() < 200);
    }

    // -- AgentProvider edge cases --

    #[test]
    fn agent_provider_all_variants_serializable() {
        let variants = vec![
            AgentProvider::ClaudeCode,
            AgentProvider::Codex,
            AgentProvider::Gemini,
            AgentProvider::Grok,
            AgentProvider::Other("x".into()),
        ];
        for v in &variants {
            let json_str = serde_json::to_string(v).unwrap();
            assert!(!json_str.is_empty());
        }
    }

    #[test]
    fn agent_provider_kebab_case_serialization() {
        let json_str = serde_json::to_string(&AgentProvider::ClaudeCode).unwrap();
        assert!(json_str.contains("claude-code"));
    }

    #[test]
    fn agent_provider_hash_and_eq() {
        let mut set = std::collections::HashSet::new();
        set.insert(AgentProvider::ClaudeCode);
        set.insert(AgentProvider::ClaudeCode);
        assert_eq!(set.len(), 1);
        set.insert(AgentProvider::Codex);
        assert_eq!(set.len(), 2);
    }

    // -- Config edge cases --

    #[test]
    fn config_custom_binary_and_dir() {
        let c = SessionResumeConfig {
            casr_binary: "/opt/bin/casr".into(),
            working_dir: Some(PathBuf::from("/my/project")),
            timeout_secs: 120,
            dry_run: true,
        };
        let r = SessionResumer::new(c);
        assert_eq!(r.config().casr_binary, "/opt/bin/casr");
        assert_eq!(
            r.config().working_dir.as_deref(),
            Some(Path::new("/my/project"))
        );
    }

    #[test]
    fn resumer_is_provider_installed_fails_gracefully() {
        let r = SessionResumer::new(SessionResumeConfig {
            casr_binary: "/nonexistent/casr-binary-that-does-not-exist".into(),
            ..Default::default()
        });
        let result = r.is_provider_installed(&AgentProvider::ClaudeCode);
        assert!(result.is_err());
    }

    #[test]
    fn resumer_discover_for_provider_fails_gracefully() {
        let r = SessionResumer::new(SessionResumeConfig {
            casr_binary: "/nonexistent/casr-binary-that-does-not-exist".into(),
            ..Default::default()
        });
        let result = r.discover_sessions_for_provider(&AgentProvider::Codex);
        assert!(result.is_err());
    }
}
