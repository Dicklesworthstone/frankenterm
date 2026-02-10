//! Wrapper for the `cass` CLI (session search/query JSON parsing with safety).
//!
//! This module treats cass (coding_agent_session_search) as an external
//! "truth source" for session data. It provides a small, typed API with:
//! - hard timeouts
//! - output size limits
//! - JSON parsing with redacted error previews
//! - version-tolerant parsing (ignores unknown fields)

use crate::error::Remediation;
use crate::policy::Redactor;
use crate::suggestions::Platform;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

/// Supported cass agents for filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CassAgent {
    Codex,
    ClaudeCode,
    Gemini,
    Cursor,
    Aider,
    ChatGpt,
}

impl CassAgent {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::ClaudeCode => "claude_code",
            Self::Gemini => "gemini",
            Self::Cursor => "cursor",
            Self::Aider => "aider",
            Self::ChatGpt => "chatgpt",
        }
    }
}

impl std::fmt::Display for CassAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Parsed output for `cass search`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CassSearchResult {
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
    #[serde(default)]
    pub count: Option<usize>,
    #[serde(default)]
    pub total_matches: Option<usize>,
    #[serde(default)]
    pub hits: Vec<CassSearchHit>,
    #[serde(default)]
    pub max_tokens: Option<usize>,
    #[serde(default)]
    pub request_id: Option<String>,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub hits_clamped: Option<bool>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// A single search hit from cass.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CassSearchHit {
    #[serde(default)]
    pub source_path: Option<String>,
    #[serde(default)]
    pub line_number: Option<usize>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub timestamp: Option<String>,
    #[serde(default)]
    pub score: Option<f64>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// Parsed cass session details.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CassSession {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub project_path: Option<String>,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub ended_at: Option<String>,
    #[serde(default)]
    pub messages: Vec<CassMessage>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// A cass message entry for a session.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CassMessage {
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub timestamp: Option<String>,
    #[serde(default)]
    pub token_count: Option<u64>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// Aggregated summary for a cass session.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CassSessionSummary {
    #[serde(default)]
    pub total_tokens: Option<i64>,
    #[serde(default)]
    pub input_tokens: Option<i64>,
    #[serde(default)]
    pub output_tokens: Option<i64>,
    #[serde(default)]
    pub message_count: usize,
    #[serde(default)]
    pub session_started_at_ms: Option<i64>,
    #[serde(default)]
    pub session_ended_at_ms: Option<i64>,
    #[serde(default)]
    pub first_message_at_ms: Option<i64>,
    #[serde(default)]
    pub last_message_at_ms: Option<i64>,
}

impl CassSessionSummary {
    /// Build a summary from a cass session payload.
    #[must_use]
    pub fn from_session(session: &CassSession) -> Self {
        let mut total_tokens: i64 = 0;
        let mut input_tokens: i64 = 0;
        let mut output_tokens: i64 = 0;
        let mut has_tokens = false;

        let mut first_message_at_ms: Option<i64> = None;
        let mut last_message_at_ms: Option<i64> = None;

        for message in &session.messages {
            if let Some(timestamp) = message.timestamp.as_deref() {
                if let Some(parsed) = parse_cass_timestamp_ms(timestamp) {
                    first_message_at_ms =
                        Some(first_message_at_ms.map_or(parsed, |prev| prev.min(parsed)));
                    last_message_at_ms =
                        Some(last_message_at_ms.map_or(parsed, |prev| prev.max(parsed)));
                }
            }

            if let Some(tokens) = message.token_count {
                has_tokens = true;
                let tokens_i64 = i64::try_from(tokens).unwrap_or(i64::MAX);
                total_tokens = total_tokens.saturating_add(tokens_i64);
                if let Some(role) = message.role.as_deref() {
                    match classify_role(role) {
                        RoleClass::Input => {
                            input_tokens = input_tokens.saturating_add(tokens_i64);
                        }
                        RoleClass::Output => {
                            output_tokens = output_tokens.saturating_add(tokens_i64);
                        }
                        RoleClass::Unknown => {}
                    }
                }
            }
        }

        let session_started_at_ms = session
            .started_at
            .as_deref()
            .and_then(parse_cass_timestamp_ms);
        let session_ended_at_ms = session
            .ended_at
            .as_deref()
            .and_then(parse_cass_timestamp_ms);

        Self {
            total_tokens: has_tokens.then_some(total_tokens),
            input_tokens: has_tokens.then_some(input_tokens),
            output_tokens: has_tokens.then_some(output_tokens),
            message_count: session.messages.len(),
            session_started_at_ms,
            session_ended_at_ms,
            first_message_at_ms,
            last_message_at_ms,
        }
    }
}

/// Parsed output for `cass view` (session query).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CassViewResult {
    #[serde(default)]
    pub source_path: Option<String>,
    #[serde(default)]
    pub line_number: Option<usize>,
    #[serde(default)]
    pub context_before: Option<Vec<CassContextLine>>,
    #[serde(default)]
    pub match_line: Option<CassContextLine>,
    #[serde(default)]
    pub context_after: Option<Vec<CassContextLine>>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// A context line from cass view.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CassContextLine {
    #[serde(default)]
    pub line_number: Option<usize>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// Parsed output for `cass status`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CassStatus {
    #[serde(default)]
    pub healthy: Option<bool>,
    #[serde(default)]
    pub index_path: Option<String>,
    #[serde(default)]
    pub total_sessions: Option<usize>,
    #[serde(default)]
    pub total_lines: Option<usize>,
    #[serde(default)]
    pub last_indexed: Option<String>,
    #[serde(default)]
    pub stale: Option<bool>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// Errors produced by the cass wrapper.
#[derive(thiserror::Error, Debug)]
pub enum CassError {
    #[error("cass is not installed or not found on PATH")]
    NotInstalled,
    #[error("cass timed out after {timeout_secs}s")]
    Timeout { timeout_secs: u64 },
    #[error("cass failed with exit code {status}: {stderr}")]
    NonZeroExit { status: i32, stderr: String },
    #[error("cass output exceeded {max_bytes} bytes")]
    OutputTooLarge { bytes: usize, max_bytes: usize },
    #[error("cass returned invalid JSON: {message}")]
    InvalidJson { message: String, preview: String },
    /// Reserved for future use when cass CLI returns explicit "no results" errors.
    /// Currently, empty search results are not treated as errors (the query succeeded
    /// with zero hits). This variant exists for API completeness and future CLI changes.
    #[error("cass returned no results for query")]
    NoResults { query: String },
    #[error("cass I/O error: {message}")]
    Io { message: String },
}

impl CassError {
    /// Optional remediation guidance for this error.
    #[must_use]
    pub fn remediation(&self) -> Remediation {
        match self {
            Self::NotInstalled => {
                let mut remediation =
                    Remediation::new("Install cass and ensure it is available on PATH.")
                        .command("Verify install", "cass --version");
                if let Some(cmd) = Platform::detect().install_command("cass") {
                    remediation = remediation.command("Install cass", cmd);
                }
                remediation.alternative("If cass is installed elsewhere, add it to PATH.")
            }
            Self::Timeout { timeout_secs } => Remediation::new(format!(
                "cass did not respond within {timeout_secs}s. Retry or check system load."
            ))
            .command("Retry search", "cass search \"query\" --robot --limit 5")
            .alternative("Increase the timeout for cass commands."),
            Self::NonZeroExit { .. } => Remediation::new(
                "cass exited with an error. Check cass logs or rerun with verbose output.",
            )
            .command("Check status", "cass status --json")
            .alternative("Run cass diag --json for detailed diagnostics."),
            Self::OutputTooLarge { .. } => {
                Remediation::new("cass output was too large. Reduce limit or use --fields minimal.")
                    .command(
                        "Smaller query",
                        "cass search \"query\" --robot --limit 5 --fields minimal",
                    )
                    .alternative("Use aggregation to reduce output size.")
            }
            Self::InvalidJson { .. } => Remediation::new(
                "cass returned malformed JSON. Upgrade cass or verify output format.",
            )
            .command("Check cass version", "cass --version")
            .alternative("Report the issue with a redacted output sample."),
            Self::NoResults { query } => Remediation::new(format!(
                "No sessions matched query: {query}. Broaden search or check index."
            ))
            .command("Check index", "cass status --json")
            .alternative("Try a broader search term."),
            Self::Io { .. } => {
                Remediation::new("I/O error while running cass. Check permissions and retry.")
                    .alternative("Verify cass binary permissions.")
            }
        }
    }
}

/// Options for cass search.
#[derive(Debug, Clone, Default)]
pub struct SearchOptions {
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    pub agent: Option<CassAgent>,
    pub workspace: Option<String>,
    pub days: Option<u32>,
    pub fields: Option<String>,
    pub max_tokens: Option<usize>,
}

/// Options for cass view (query).
#[derive(Debug, Clone, Default)]
pub struct ViewOptions {
    pub context_lines: Option<usize>,
}

/// Thin wrapper around the cass CLI.
#[derive(Debug, Clone)]
pub struct CassClient {
    binary: String,
    timeout: Duration,
    max_output_bytes: usize,
    max_error_bytes: usize,
}

impl Default for CassClient {
    fn default() -> Self {
        Self {
            binary: "cass".to_string(),
            timeout: Duration::from_secs(15),
            max_output_bytes: 512 * 1024,
            max_error_bytes: 8 * 1024,
        }
    }
}

impl CassClient {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_binary(mut self, binary: impl Into<String>) -> Self {
        self.binary = binary.into();
        self
    }

    #[must_use]
    pub fn with_timeout_secs(mut self, timeout_secs: u64) -> Self {
        self.timeout = Duration::from_secs(timeout_secs);
        self
    }

    #[must_use]
    pub fn with_max_output_bytes(mut self, max_output_bytes: usize) -> Self {
        self.max_output_bytes = max_output_bytes;
        self
    }

    #[must_use]
    pub fn with_max_error_bytes(mut self, max_error_bytes: usize) -> Self {
        self.max_error_bytes = max_error_bytes;
        self
    }

    /// Search sessions via `cass search`.
    pub async fn search(
        &self,
        query: &str,
        options: &SearchOptions,
    ) -> Result<CassSearchResult, CassError> {
        let mut args = vec![
            "search".to_string(),
            query.to_string(),
            "--robot".to_string(),
        ];

        if let Some(limit) = options.limit {
            args.push("--limit".to_string());
            args.push(limit.to_string());
        }
        if let Some(offset) = options.offset {
            args.push("--offset".to_string());
            args.push(offset.to_string());
        }
        if let Some(agent) = &options.agent {
            args.push("--agent".to_string());
            args.push(agent.as_str().to_string());
        }
        if let Some(workspace) = &options.workspace {
            args.push("--workspace".to_string());
            args.push(workspace.clone());
        }
        if let Some(days) = options.days {
            args.push("--days".to_string());
            args.push(days.to_string());
        }
        if let Some(fields) = &options.fields {
            args.push("--fields".to_string());
            args.push(fields.clone());
        }
        if let Some(max_tokens) = options.max_tokens {
            args.push("--max-tokens".to_string());
            args.push(max_tokens.to_string());
        }

        let output = self.run(&args).await?;
        parse_json(&output, self.max_error_bytes)
    }

    /// Search cass for sessions under a given path (and optional agent filter).
    pub async fn search_sessions(
        &self,
        path: &Path,
        agent: Option<CassAgent>,
    ) -> Result<Vec<CassSession>, CassError> {
        let mut args = vec![
            "search".to_string(),
            "--path".to_string(),
            path.to_string_lossy().to_string(),
            "--format".to_string(),
            "json".to_string(),
        ];

        if let Some(agent) = agent {
            args.push("--agent".to_string());
            args.push(agent.as_str().to_string());
        }

        let output = self.run(&args).await?;
        let sessions = parse_sessions(&output, self.max_error_bytes)?;
        if sessions.is_empty() {
            return Err(CassError::NoResults {
                query: format!("path={}", path.display()),
            });
        }
        Ok(sessions)
    }

    /// Query a specific session by session id.
    pub async fn query_session(&self, session_id: &str) -> Result<CassSession, CassError> {
        let args = vec![
            "query".to_string(),
            "--session-id".to_string(),
            session_id.to_string(),
            "--format".to_string(),
            "json".to_string(),
        ];

        let output = self.run(&args).await?;
        let mut sessions = parse_sessions(&output, self.max_error_bytes)?;
        if let Some(session) = sessions.pop() {
            return Ok(session);
        }
        Err(CassError::NoResults {
            query: format!("session_id={session_id}"),
        })
    }

    /// Query a specific session via `cass view`.
    pub async fn query(
        &self,
        session_path: &Path,
        line_number: usize,
        options: &ViewOptions,
    ) -> Result<CassViewResult, CassError> {
        let mut args = vec![
            "view".to_string(),
            session_path.to_string_lossy().to_string(),
            "-n".to_string(),
            line_number.to_string(),
            "--json".to_string(),
        ];

        if let Some(context) = options.context_lines {
            args.push("-C".to_string());
            args.push(context.to_string());
        }

        let output = self.run(&args).await?;
        parse_json(&output, self.max_error_bytes)
    }

    /// Check cass health via `cass status`.
    pub async fn status(&self) -> Result<CassStatus, CassError> {
        let args = vec!["status".to_string(), "--json".to_string()];
        let output = self.run(&args).await?;
        parse_json(&output, self.max_error_bytes)
    }

    async fn run(&self, args: &[String]) -> Result<String, CassError> {
        let mut cmd = Command::new(&self.binary);
        cmd.args(args);

        let output = match timeout(self.timeout, cmd.output()).await {
            Ok(result) => result.map_err(|err| categorize_io_error(&err))?,
            Err(_) => {
                return Err(CassError::Timeout {
                    timeout_secs: self.timeout.as_secs(),
                });
            }
        };

        if !output.status.success() {
            let status = output.status.code().unwrap_or(-1);
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let stderr_preview = redact_and_truncate(&stderr, self.max_error_bytes);
            return Err(CassError::NonZeroExit {
                status,
                stderr: stderr_preview,
            });
        }

        // Check raw byte length BEFORE allocating string to prevent OOM on huge outputs
        let raw_bytes = output.stdout.len();
        if raw_bytes > self.max_output_bytes {
            return Err(CassError::OutputTooLarge {
                bytes: raw_bytes,
                max_bytes: self.max_output_bytes,
            });
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RoleClass {
    Input,
    Output,
    Unknown,
}

fn classify_role(role: &str) -> RoleClass {
    match role.trim().to_lowercase().as_str() {
        "user" | "human" | "system" => RoleClass::Input,
        "assistant" | "model" | "ai" => RoleClass::Output,
        _ => RoleClass::Unknown,
    }
}

/// Parse a cass timestamp into epoch milliseconds.
///
/// Accepts epoch seconds, epoch milliseconds, RFC3339, and common
/// `%Y-%m-%d %H:%M:%S` formats.
pub fn parse_cass_timestamp_ms(raw: &str) -> Option<i64> {
    use chrono::{DateTime, NaiveDateTime, Utc};

    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    if let Ok(value) = raw.parse::<i64>() {
        // Heuristic: treat large values as ms, smaller as seconds.
        return Some(if value > 10_000_000_000 {
            value
        } else {
            value.saturating_mul(1_000)
        });
    }

    if let Ok(parsed) = DateTime::parse_from_rfc3339(raw) {
        return Some(parsed.timestamp_millis());
    }

    if let Ok(naive) = NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S") {
        return Some(DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc).timestamp_millis());
    }

    if let Ok(naive) = NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%SZ") {
        return Some(DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc).timestamp_millis());
    }

    None
}

fn categorize_io_error(err: &std::io::Error) -> CassError {
    match err.kind() {
        std::io::ErrorKind::NotFound => CassError::NotInstalled,
        _ => CassError::Io {
            message: err.to_string(),
        },
    }
}

fn parse_json<T: DeserializeOwned>(input: &str, max_preview: usize) -> Result<T, CassError> {
    serde_json::from_str(input).map_err(|err| CassError::InvalidJson {
        message: err.to_string(),
        preview: redact_and_truncate(input, max_preview),
    })
}

fn parse_sessions(input: &str, max_preview: usize) -> Result<Vec<CassSession>, CassError> {
    let value: Value = serde_json::from_str(input).map_err(|err| CassError::InvalidJson {
        message: err.to_string(),
        preview: redact_and_truncate(input, max_preview),
    })?;

    if let Some(array) = value.as_array() {
        return serde_json::from_value(Value::Array(array.clone())).map_err(|err| {
            CassError::InvalidJson {
                message: err.to_string(),
                preview: redact_and_truncate(input, max_preview),
            }
        });
    }

    if let Some(sessions_val) = value.get("sessions") {
        return serde_json::from_value(sessions_val.clone()).map_err(|err| {
            CassError::InvalidJson {
                message: err.to_string(),
                preview: redact_and_truncate(input, max_preview),
            }
        });
    }

    serde_json::from_value(value)
        .map(|session: CassSession| vec![session])
        .map_err(|err| CassError::InvalidJson {
            message: err.to_string(),
            preview: redact_and_truncate(input, max_preview),
        })
}

fn redact_and_truncate(input: &str, max_len: usize) -> String {
    let redactor = Redactor::new();
    let redacted = redactor.redact(input);
    if redacted.len() <= max_len {
        return redacted;
    }
    let mut truncated = redacted.chars().take(max_len).collect::<String>();
    truncated.push_str("...");
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_search_result_basic() {
        let payload = json!({
            "query": "test",
            "limit": 10,
            "offset": 0,
            "count": 2,
            "total_matches": 2,
            "hits": [
                {
                    "source_path": "/path/to/session.jsonl",
                    "line_number": 42,
                    "agent": "codex"
                },
                {
                    "source_path": "/path/to/other.jsonl",
                    "line_number": 100,
                    "agent": "claude_code"
                }
            ]
        });

        let parsed: CassSearchResult =
            parse_json(&payload.to_string(), 4096).expect("should parse");
        assert_eq!(parsed.query.as_deref(), Some("test"));
        assert_eq!(parsed.hits.len(), 2);
        assert_eq!(parsed.hits[0].agent.as_deref(), Some("codex"));
        assert_eq!(parsed.hits[1].line_number, Some(100));
    }

    #[test]
    fn parse_search_result_with_extras() {
        let payload = json!({
            "query": "test",
            "hits": [],
            "unknown_field": "ignored",
            "nested": { "deep": true }
        });

        let parsed: CassSearchResult =
            parse_json(&payload.to_string(), 4096).expect("should parse");
        assert!(parsed.extra.contains_key("unknown_field"));
        assert!(parsed.extra.contains_key("nested"));
    }

    #[test]
    fn parse_search_hit_with_content() {
        let payload = json!({
            "query": "error",
            "hits": [
                {
                    "source_path": "/path/to/session.jsonl",
                    "line_number": 10,
                    "agent": "gemini",
                    "content": "Some error message here",
                    "timestamp": "2026-01-29T10:00:00Z",
                    "score": 0.95
                }
            ]
        });

        let parsed: CassSearchResult =
            parse_json(&payload.to_string(), 4096).expect("should parse");
        let hit = &parsed.hits[0];
        assert_eq!(hit.content.as_deref(), Some("Some error message here"));
        assert_eq!(hit.timestamp.as_deref(), Some("2026-01-29T10:00:00Z"));
        assert!((hit.score.unwrap() - 0.95).abs() < 0.001);
    }

    #[test]
    fn parse_search_empty_hits() {
        let payload = json!({
            "query": "nonexistent",
            "hits": [],
            "count": 0,
            "total_matches": 0
        });

        let parsed: CassSearchResult =
            parse_json(&payload.to_string(), 4096).expect("should parse");
        assert!(parsed.hits.is_empty());
        assert_eq!(parsed.count, Some(0));
    }

    #[test]
    fn parse_view_result_basic() {
        let payload = json!({
            "source_path": "/path/to/session.jsonl",
            "line_number": 42,
            "match_line": {
                "line_number": 42,
                "content": "The matching line content",
                "role": "assistant"
            },
            "agent": "codex"
        });

        let parsed: CassViewResult = parse_json(&payload.to_string(), 4096).expect("should parse");
        assert_eq!(parsed.line_number, Some(42));
        assert!(parsed.match_line.is_some());
        let match_line = parsed.match_line.unwrap();
        assert_eq!(match_line.role.as_deref(), Some("assistant"));
    }

    #[test]
    fn parse_sessions_array() {
        let payload = json!([
            {
                "session_id": "sess-1",
                "agent": "codex",
                "project_path": "/repo",
                "messages": [
                    { "role": "user", "content": "hi", "timestamp": "2026-01-29T12:00:00Z" }
                ]
            },
            {
                "session_id": "sess-2",
                "agent": "claude_code",
                "project_path": "/repo2"
            }
        ]);

        let parsed = parse_sessions(&payload.to_string(), 4096).expect("should parse");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].session_id.as_deref(), Some("sess-1"));
        assert_eq!(parsed[0].messages.len(), 1);
        assert_eq!(parsed[1].agent.as_deref(), Some("claude_code"));
    }

    #[test]
    fn parse_sessions_wrapped() {
        let payload = json!({
            "sessions": [
                {
                    "session_id": "sess-3",
                    "agent": "gemini",
                    "project_path": "/repo3"
                }
            ],
            "extra_field": true
        });

        let parsed = parse_sessions(&payload.to_string(), 4096).expect("should parse");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].session_id.as_deref(), Some("sess-3"));
    }

    #[test]
    fn parse_sessions_single_object() {
        let payload = json!({
            "session_id": "sess-4",
            "agent": "codex",
            "project_path": "/repo4"
        });

        let parsed = parse_sessions(&payload.to_string(), 4096).expect("should parse");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].session_id.as_deref(), Some("sess-4"));
    }

    #[test]
    fn parse_view_result_with_context() {
        let payload = json!({
            "source_path": "/path/to/session.jsonl",
            "line_number": 42,
            "context_before": [
                { "line_number": 40, "content": "before line 1" },
                { "line_number": 41, "content": "before line 2" }
            ],
            "match_line": { "line_number": 42, "content": "match" },
            "context_after": [
                { "line_number": 43, "content": "after line 1" }
            ]
        });

        let parsed: CassViewResult = parse_json(&payload.to_string(), 4096).expect("should parse");
        assert_eq!(parsed.context_before.as_ref().map(|v| v.len()), Some(2));
        assert_eq!(parsed.context_after.as_ref().map(|v| v.len()), Some(1));
    }

    #[test]
    fn parse_status_healthy() {
        let payload = json!({
            "healthy": true,
            "index_path": "/home/user/.cass/index",
            "total_sessions": 150,
            "total_lines": 50000,
            "last_indexed": "2026-01-29T10:00:00Z",
            "stale": false
        });

        let parsed: CassStatus = parse_json(&payload.to_string(), 4096).expect("should parse");
        assert_eq!(parsed.healthy, Some(true));
        assert_eq!(parsed.total_sessions, Some(150));
        assert_eq!(parsed.stale, Some(false));
    }

    #[test]
    fn parse_status_stale() {
        let payload = json!({
            "healthy": true,
            "stale": true,
            "total_sessions": 100
        });

        let parsed: CassStatus = parse_json(&payload.to_string(), 4096).expect("should parse");
        assert_eq!(parsed.stale, Some(true));
    }

    #[test]
    fn parse_minimal_valid_json() {
        let parsed: CassSearchResult = parse_json("{}", 4096).expect("should parse");
        assert!(parsed.query.is_none());
        assert!(parsed.hits.is_empty());
    }

    #[test]
    fn invalid_json_returns_preview() {
        let err = parse_json::<CassSearchResult>("{not_json}", 20).expect_err("should error");
        match err {
            CassError::InvalidJson { preview, .. } => {
                assert!(preview.contains("not_json"));
            }
            other => panic!("Unexpected error: {other:?}"),
        }
    }

    #[test]
    fn redact_and_truncate_masks_secrets() {
        let secret = "sk-abc123456789012345678901234567890123456789012345678901";
        let text = format!("token={secret}");
        let redacted = redact_and_truncate(&text, 200);
        assert!(!redacted.contains("sk-"));
        assert!(redacted.contains("[REDACTED]"));
    }

    #[test]
    fn cass_agent_display() {
        assert_eq!(CassAgent::Codex.as_str(), "codex");
        assert_eq!(CassAgent::ClaudeCode.as_str(), "claude_code");
        assert_eq!(format!("{}", CassAgent::Gemini), "gemini");
    }

    #[test]
    fn cass_error_remediation_not_installed() {
        let err = CassError::NotInstalled;
        let rem = err.remediation();
        assert!(!rem.summary.is_empty());
        assert!(!rem.commands.is_empty());
    }

    #[test]
    fn cass_error_remediation_timeout() {
        let err = CassError::Timeout { timeout_secs: 15 };
        let rem = err.remediation();
        assert!(rem.summary.contains("15s"));
    }

    #[test]
    fn cass_error_remediation_no_results() {
        let err = CassError::NoResults {
            query: "nonexistent".to_string(),
        };
        let rem = err.remediation();
        assert!(rem.summary.contains("nonexistent"));
    }

    #[test]
    fn categorize_not_found_as_not_installed() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
        let cass_err = categorize_io_error(&io_err);
        assert!(matches!(cass_err, CassError::NotInstalled));
    }

    #[test]
    fn categorize_other_io_as_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let cass_err = categorize_io_error(&io_err);
        match cass_err {
            CassError::Io { message } => assert!(message.contains("denied")),
            other => panic!("Expected Io, got: {other:?}"),
        }
    }

    #[test]
    fn search_options_default() {
        let opts = SearchOptions::default();
        assert!(opts.limit.is_none());
        assert!(opts.agent.is_none());
    }

    #[test]
    fn view_options_default() {
        let opts = ViewOptions::default();
        assert!(opts.context_lines.is_none());
    }

    #[test]
    fn summarize_session_with_roles() {
        let session = CassSession {
            started_at: Some("2026-01-29T09:58:00Z".to_string()),
            ended_at: Some("2026-01-29T10:02:00Z".to_string()),
            messages: vec![
                CassMessage {
                    role: Some("user".to_string()),
                    token_count: Some(10),
                    timestamp: Some("2026-01-29T10:00:00Z".to_string()),
                    ..CassMessage::default()
                },
                CassMessage {
                    role: Some("assistant".to_string()),
                    token_count: Some(20),
                    timestamp: Some("2026-01-29T10:01:00Z".to_string()),
                    ..CassMessage::default()
                },
                CassMessage {
                    role: Some("system".to_string()),
                    token_count: Some(5),
                    timestamp: Some("2026-01-29T09:59:00Z".to_string()),
                    ..CassMessage::default()
                },
            ],
            ..CassSession::default()
        };

        let summary = CassSessionSummary::from_session(&session);
        assert_eq!(summary.total_tokens, Some(35));
        assert_eq!(summary.input_tokens, Some(15));
        assert_eq!(summary.output_tokens, Some(20));
        assert_eq!(summary.message_count, 3);
        assert!(summary.session_started_at_ms.is_some());
        assert!(summary.session_ended_at_ms.is_some());
        assert!(summary.first_message_at_ms.is_some());
        assert!(summary.last_message_at_ms.is_some());
    }

    #[test]
    fn summarize_session_without_token_counts() {
        let session = CassSession {
            messages: vec![
                CassMessage {
                    role: Some("user".to_string()),
                    timestamp: Some("2026-01-29T10:00:00Z".to_string()),
                    ..CassMessage::default()
                },
                CassMessage {
                    role: Some("assistant".to_string()),
                    timestamp: Some("2026-01-29T10:01:00Z".to_string()),
                    ..CassMessage::default()
                },
            ],
            ..CassSession::default()
        };

        let summary = CassSessionSummary::from_session(&session);
        assert_eq!(summary.total_tokens, None);
        assert_eq!(summary.input_tokens, None);
        assert_eq!(summary.output_tokens, None);
        assert_eq!(summary.message_count, 2);
    }
}
