//! `frankenterm-core-cass-types` — cass wrapper type definitions leaf crate
//! (ft-8cg6y / ft-y0loj.5.D.1).
//!
//! Tier-1 leaf crate carved out from `frankenterm-core::cass`. Holds the
//! plain-data types that the cass wrapper exchanges with callers. `CassClient`,
//! subprocess execution, remediation wiring, and recorder-export glue stay in
//! `frankenterm-core::cass`.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

const DEFAULT_CASS_EXPORT_LIMIT: usize = 1000;

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

    #[must_use]
    pub fn from_slug(slug: &str) -> Option<Self> {
        match slug.trim().to_ascii_lowercase().as_str() {
            "codex" => Some(Self::Codex),
            "claude" | "claude-code" | "claude_code" => Some(Self::ClaudeCode),
            "gemini" => Some(Self::Gemini),
            "cursor" => Some(Self::Cursor),
            "aider" => Some(Self::Aider),
            value if is_chatgpt_slug(value) => Some(Self::ChatGpt),
            _ => None,
        }
    }
}

fn is_chatgpt_slug(slug: &str) -> bool {
    matches!(slug, "chatgpt" | "chat_gpt" | "chat-gpt")
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

/// Parsed output for `cass index`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CassIndexResult {
    /// Number of sessions indexed in this run.
    #[serde(default)]
    pub sessions_indexed: Option<usize>,
    /// Number of new sessions discovered.
    #[serde(default)]
    pub new_sessions: Option<usize>,
    /// Whether the index operation completed successfully.
    #[serde(default)]
    pub success: Option<bool>,
    /// Duration of the index operation in milliseconds.
    #[serde(default)]
    pub elapsed_ms: Option<u64>,
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

/// Query controls for exporting FrankenTerm recorder sessions to cass connectors.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CassExportQuery {
    /// Only return sessions whose recorder row id is greater than this value.
    pub after_id: Option<i64>,
    /// Filter by pane id.
    pub pane_id: Option<u64>,
    /// Filter by session start timestamp lower bound (epoch ms).
    pub since: Option<i64>,
    /// Filter by session start timestamp upper bound (epoch ms).
    pub until: Option<i64>,
    /// Maximum number of exported sessions.
    pub limit: usize,
}

impl CassExportQuery {
    #[must_use]
    pub fn effective_limit(&self) -> usize {
        if self.limit == 0 {
            DEFAULT_CASS_EXPORT_LIMIT
        } else {
            self.limit
        }
    }
}

impl Default for CassExportQuery {
    fn default() -> Self {
        Self {
            after_id: None,
            pane_id: None,
            since: None,
            until: None,
            limit: DEFAULT_CASS_EXPORT_LIMIT,
        }
    }
}

/// Query controls for exporting session content chunks to cass connectors.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CassContentExportQuery {
    /// Only return segments with id strictly greater than this value.
    pub after_id: Option<i64>,
    /// Maximum number of content chunks to export.
    pub limit: usize,
}

impl CassContentExportQuery {
    #[must_use]
    pub fn effective_limit(&self) -> usize {
        if self.limit == 0 {
            DEFAULT_CASS_EXPORT_LIMIT
        } else {
            self.limit
        }
    }
}

impl Default for CassContentExportQuery {
    fn default() -> Self {
        Self {
            after_id: None,
            limit: DEFAULT_CASS_EXPORT_LIMIT,
        }
    }
}

/// Cass-facing summary of a FrankenTerm agent session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CassExportSessionRecord {
    /// Stable recorder row id for the session.
    pub session_row_id: i64,
    /// Export identifier. Uses the agent session id when present, otherwise
    /// falls back to `ft-session-<row_id>`.
    pub session_id: String,
    /// Agent provider/classification from the recorder session.
    pub agent_type: String,
    /// Workspace/cwd associated with the pane at export time.
    pub workspace: Option<String>,
    /// Pane ids participating in the session. Currently single-pane, but kept
    /// as a vector to match the connector-facing contract.
    pub pane_ids: Vec<u64>,
    /// Session start timestamp (epoch ms).
    pub started_at_ms: i64,
    /// Session end timestamp (epoch ms) if the session has completed.
    pub ended_at_ms: Option<i64>,
    /// Model name recorded for the agent session, if available.
    pub model_name: Option<String>,
    /// Export-time content token estimate. Prefers recorded token totals when present.
    pub content_tokens: u64,
    /// External correlation id (for example a cass correlation result), if present.
    pub external_id: Option<String>,
    /// External correlation metadata persisted on the session, if present.
    pub external_meta: Option<Value>,
}

/// Cass-facing content chunk exported from recorder segments.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CassExportContentChunk {
    /// Stable recorder row id for the session.
    pub session_row_id: i64,
    /// Export identifier for the session.
    pub session_id: String,
    /// Recorder segment id.
    pub segment_id: i64,
    /// Pane id that produced the content.
    pub pane_id: u64,
    /// Per-pane sequence number for the segment.
    pub seq: u64,
    /// Timestamp when the content was captured (epoch ms).
    pub timestamp_ms: i64,
    /// Terminal content captured for indexing.
    pub content: String,
    /// Content class. Recorder segments are terminal output today.
    pub content_type: String,
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
#[must_use]
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

#[cfg(test)]
mod tests {
    //! ft-u9wm4: pin the cass-types leaf-crate boundary post ft-8cg6y
    //! extraction. Sister of the caut-types tests at ft-cui2w / 2d0fb9f7.

    use super::*;

    // ─── CassAgent ────────────────────────────────────────────────────────

    #[test]
    fn agent_as_str_round_trip_for_every_variant() {
        for (agent, expected) in [
            (CassAgent::Codex, "codex"),
            (CassAgent::ClaudeCode, "claude_code"),
            (CassAgent::Gemini, "gemini"),
            (CassAgent::Cursor, "cursor"),
            (CassAgent::Aider, "aider"),
            (CassAgent::ChatGpt, "chatgpt"),
        ] {
            assert_eq!(agent.as_str(), expected);
            assert_eq!(agent.to_string(), expected);
        }
    }

    #[test]
    fn agent_from_slug_recognizes_canonical_and_aliases() {
        assert_eq!(CassAgent::from_slug("codex"), Some(CassAgent::Codex));
        assert_eq!(CassAgent::from_slug("claude"), Some(CassAgent::ClaudeCode));
        assert_eq!(
            CassAgent::from_slug("claude-code"),
            Some(CassAgent::ClaudeCode)
        );
        assert_eq!(
            CassAgent::from_slug("claude_code"),
            Some(CassAgent::ClaudeCode)
        );
        assert_eq!(CassAgent::from_slug("gemini"), Some(CassAgent::Gemini));
        assert_eq!(CassAgent::from_slug("cursor"), Some(CassAgent::Cursor));
        assert_eq!(CassAgent::from_slug("aider"), Some(CassAgent::Aider));
        assert_eq!(CassAgent::from_slug("chatgpt"), Some(CassAgent::ChatGpt));
        assert_eq!(CassAgent::from_slug("chat-gpt"), Some(CassAgent::ChatGpt));
        assert_eq!(CassAgent::from_slug("chat_gpt"), Some(CassAgent::ChatGpt));
    }

    #[test]
    fn agent_from_slug_is_case_insensitive_and_trims_whitespace() {
        assert_eq!(CassAgent::from_slug("  CODEX "), Some(CassAgent::Codex));
        assert_eq!(
            CassAgent::from_slug("Claude-Code"),
            Some(CassAgent::ClaudeCode)
        );
        assert_eq!(CassAgent::from_slug("ChatGPT"), Some(CassAgent::ChatGpt));
    }

    #[test]
    fn agent_from_slug_rejects_unknown_values() {
        for bogus in ["", "bard", "openai", "  ", "chatgpt4"] {
            assert_eq!(CassAgent::from_slug(bogus), None, "{bogus:?}");
        }
    }

    // ─── CassError Display ────────────────────────────────────────────────

    #[test]
    fn cass_error_display_matches_thiserror_format_strings() {
        // CassError is not Clone/PartialEq (Io/Json wrapping reasons), so we
        // pin the externally-visible Display surface only — that's what
        // shows up in MCP responses, recorder logs, and dashboards.
        assert_eq!(
            CassError::NotInstalled.to_string(),
            "cass is not installed or not found on PATH"
        );
        assert_eq!(
            CassError::Timeout { timeout_secs: 5 }.to_string(),
            "cass timed out after 5s"
        );
        assert_eq!(
            CassError::NonZeroExit {
                status: 3,
                stderr: "perm denied".to_string()
            }
            .to_string(),
            "cass failed with exit code 3: perm denied"
        );
        assert_eq!(
            CassError::OutputTooLarge {
                bytes: 0,
                max_bytes: 2048
            }
            .to_string(),
            "cass output exceeded 2048 bytes"
        );
        assert_eq!(
            CassError::InvalidJson {
                message: "expected `]`".to_string(),
                preview: "[1, 2,".to_string()
            }
            .to_string(),
            "cass returned invalid JSON: expected `]`"
        );
        assert_eq!(
            CassError::NoResults {
                query: "needle".to_string()
            }
            .to_string(),
            "cass returned no results for query"
        );
        assert_eq!(
            CassError::Io {
                message: "EPIPE".to_string()
            }
            .to_string(),
            "cass I/O error: EPIPE"
        );
    }

    // ─── CassExportQuery / CassContentExportQuery ─────────────────────────

    #[test]
    fn cass_export_query_default_uses_canonical_limit() {
        let q = CassExportQuery::default();
        assert!(q.after_id.is_none());
        assert!(q.pane_id.is_none());
        assert!(q.since.is_none());
        assert!(q.until.is_none());
        assert_eq!(q.limit, DEFAULT_CASS_EXPORT_LIMIT);
        assert_eq!(q.effective_limit(), DEFAULT_CASS_EXPORT_LIMIT);
    }

    #[test]
    fn cass_export_query_effective_limit_substitutes_zero() {
        // limit==0 is the "I forgot to set a cap" case — the connector
        // contract says fall back to the canonical default rather than
        // emit unbounded sessions.
        let q = CassExportQuery {
            limit: 0,
            ..Default::default()
        };
        assert_eq!(q.effective_limit(), DEFAULT_CASS_EXPORT_LIMIT);
    }

    #[test]
    fn cass_export_query_effective_limit_passes_explicit_value() {
        let q = CassExportQuery {
            limit: 7,
            ..Default::default()
        };
        assert_eq!(q.effective_limit(), 7);
    }

    #[test]
    fn cass_content_export_query_default_and_effective_limit() {
        let q = CassContentExportQuery::default();
        assert!(q.after_id.is_none());
        assert_eq!(q.limit, DEFAULT_CASS_EXPORT_LIMIT);
        assert_eq!(q.effective_limit(), DEFAULT_CASS_EXPORT_LIMIT);

        let zero = CassContentExportQuery {
            after_id: None,
            limit: 0,
        };
        assert_eq!(zero.effective_limit(), DEFAULT_CASS_EXPORT_LIMIT);

        let explicit = CassContentExportQuery {
            after_id: Some(42),
            limit: 10,
        };
        assert_eq!(explicit.effective_limit(), 10);
        assert_eq!(explicit.after_id, Some(42));
    }

    // ─── parse_cass_timestamp_ms ──────────────────────────────────────────

    #[test]
    fn parse_timestamp_handles_epoch_seconds_and_milliseconds() {
        // Heuristic: > 10_000_000_000 → ms, else → seconds.
        assert_eq!(parse_cass_timestamp_ms("1700000000"), Some(1_700_000_000_000));
        assert_eq!(
            parse_cass_timestamp_ms("1700000000000"),
            Some(1_700_000_000_000)
        );
        assert_eq!(parse_cass_timestamp_ms("0"), Some(0));
    }

    #[test]
    fn parse_timestamp_handles_rfc3339_and_zoned_offsets() {
        // We don't pin an absolute literal — chrono drives the conversion
        // and we only care that (a) the format parses and (b) explicit
        // timezone offsets shift the resulting ms by the documented amount.
        let utc = parse_cass_timestamp_ms("2026-04-26T20:00:00Z").unwrap();
        // -05:00 offset is 5h *behind* UTC, so the same wall-clock string
        // means a later instant by 5h = 18_000_000 ms.
        let est = parse_cass_timestamp_ms("2026-04-26T20:00:00-05:00").unwrap();
        assert_eq!(est - utc, 5 * 3_600_000);
    }

    #[test]
    fn parse_timestamp_handles_plain_datetime_formats() {
        // The naive-format parser interprets the string as UTC, so all
        // three of these should land at the same instant.
        let a = parse_cass_timestamp_ms("2026-04-26 20:00:00").unwrap();
        let b = parse_cass_timestamp_ms("2026-04-26 20:00:00Z").unwrap();
        let c = parse_cass_timestamp_ms("2026-04-26T20:00:00Z").unwrap();
        assert_eq!(a, c);
        assert_eq!(b, c);
    }

    #[test]
    fn parse_timestamp_rejects_garbage_and_empty() {
        assert_eq!(parse_cass_timestamp_ms(""), None);
        assert_eq!(parse_cass_timestamp_ms("  "), None);
        assert_eq!(parse_cass_timestamp_ms("not a date"), None);
    }

    // ─── CassSessionSummary::from_session ─────────────────────────────────

    #[test]
    fn session_summary_aggregates_tokens_by_role() {
        let session = CassSession {
            session_id: Some("s1".to_string()),
            started_at: Some("2026-04-26T20:00:00Z".to_string()),
            ended_at: Some("2026-04-26T20:05:00Z".to_string()),
            messages: vec![
                CassMessage {
                    role: Some("user".to_string()),
                    token_count: Some(10),
                    timestamp: Some("2026-04-26T20:00:30Z".to_string()),
                    ..Default::default()
                },
                CassMessage {
                    role: Some("assistant".to_string()),
                    token_count: Some(40),
                    timestamp: Some("2026-04-26T20:01:00Z".to_string()),
                    ..Default::default()
                },
                CassMessage {
                    role: Some("system".to_string()),
                    token_count: Some(5),
                    timestamp: Some("2026-04-26T20:02:00Z".to_string()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let s = CassSessionSummary::from_session(&session);
        assert_eq!(s.message_count, 3);
        assert_eq!(s.total_tokens, Some(55));
        assert_eq!(s.input_tokens, Some(15)); // user + system
        assert_eq!(s.output_tokens, Some(40)); // assistant
        // Bracketing message timestamps are tracked separately from the
        // session-level started_at / ended_at.
        assert!(s.first_message_at_ms.is_some());
        assert!(s.last_message_at_ms.is_some());
        assert!(s.first_message_at_ms.unwrap() < s.last_message_at_ms.unwrap());
        assert!(s.session_started_at_ms.is_some());
        assert!(s.session_ended_at_ms.is_some());
    }

    #[test]
    fn session_summary_returns_no_tokens_when_messages_lack_counts() {
        let session = CassSession {
            messages: vec![CassMessage {
                role: Some("user".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let s = CassSessionSummary::from_session(&session);
        assert_eq!(s.message_count, 1);
        assert_eq!(s.total_tokens, None);
        assert_eq!(s.input_tokens, None);
        assert_eq!(s.output_tokens, None);
    }

    #[test]
    fn session_summary_unknown_role_does_not_inflate_input_or_output() {
        let session = CassSession {
            messages: vec![CassMessage {
                role: Some("tool".to_string()), // not in input/output classifier
                token_count: Some(99),
                ..Default::default()
            }],
            ..Default::default()
        };
        let s = CassSessionSummary::from_session(&session);
        // Total counts the tokens, but neither input nor output gets credit.
        assert_eq!(s.total_tokens, Some(99));
        assert_eq!(s.input_tokens, Some(0));
        assert_eq!(s.output_tokens, Some(0));
    }

    // ─── Serde roundtrip ──────────────────────────────────────────────────

    #[test]
    fn search_result_unknown_fields_land_in_extra() {
        let json = r#"{
            "query": "needle",
            "limit": 50,
            "vendorSpecific": {"shard": 7},
            "experimental": true
        }"#;
        let r: CassSearchResult = serde_json::from_str(json).unwrap();
        assert_eq!(r.query.as_deref(), Some("needle"));
        assert_eq!(r.limit, Some(50));
        assert!(r.extra.contains_key("vendorSpecific"));
        assert_eq!(
            r.extra.get("experimental"),
            Some(&serde_json::Value::Bool(true))
        );
    }

    #[test]
    fn export_session_record_serde_round_trip() {
        let rec = CassExportSessionRecord {
            session_row_id: 42,
            session_id: "ft-session-42".to_string(),
            agent_type: "claude".to_string(),
            workspace: Some("/tmp/ws".to_string()),
            pane_ids: vec![7, 11],
            started_at_ms: 1_777_996_800_000,
            ended_at_ms: Some(1_777_996_900_000),
            model_name: Some("claude-opus-4".to_string()),
            content_tokens: 1234,
            external_id: Some("ext-1".to_string()),
            external_meta: Some(serde_json::json!({"shard": 1})),
        };
        let json = serde_json::to_string(&rec).unwrap();
        let rt: CassExportSessionRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(rt, rec);
    }

    #[test]
    fn export_content_chunk_serde_round_trip() {
        let chunk = CassExportContentChunk {
            session_row_id: 9,
            session_id: "s-9".to_string(),
            segment_id: 100,
            pane_id: 3,
            seq: 7,
            timestamp_ms: 1_777_996_800_000,
            content: "ERROR: boom".to_string(),
            content_type: "terminal_output".to_string(),
        };
        let json = serde_json::to_string(&chunk).unwrap();
        let rt: CassExportContentChunk = serde_json::from_str(&json).unwrap();
        assert_eq!(rt, chunk);
    }

    #[test]
    fn cass_export_query_serde_round_trip() {
        let q = CassExportQuery {
            after_id: Some(123),
            pane_id: Some(7),
            since: Some(1_700_000_000_000),
            until: Some(1_777_000_000_000),
            limit: 25,
        };
        let json = serde_json::to_string(&q).unwrap();
        let rt: CassExportQuery = serde_json::from_str(&json).unwrap();
        assert_eq!(rt, q);
    }
}
