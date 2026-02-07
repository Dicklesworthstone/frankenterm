//! Typed client types for wa robot/MCP JSON responses.
//!
//! These types mirror the serialization-side types in the `wa` binary and provide
//! `Deserialize` so consumers can parse robot JSON output without hand-parsing.
//!
//! # Usage
//!
//! ```no_run
//! use wa_core::robot_types::{RobotResponse, GetTextData};
//!
//! let json = r#"{"ok":true,"data":{"pane_id":1,"text":"hello","tail_lines":100,"escapes_included":false},"elapsed_ms":5,"version":"0.1.0","now":1700000000000}"#;
//! let resp: RobotResponse<GetTextData> = serde_json::from_str(json).unwrap();
//! assert!(resp.ok);
//! assert_eq!(resp.data.unwrap().text, "hello");
//! ```

use serde::{Deserialize, Serialize};

use crate::error_codes::ErrorCategory;

// ============================================================================
// Envelope
// ============================================================================

/// The standard JSON envelope wrapping all robot mode responses.
///
/// Every `wa robot <command> --format json` call returns this envelope.
/// Use `parse_response` or `RobotResponse::<T>::from_json` for convenience.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(bound(deserialize = "T: serde::de::DeserializeOwned"))]
pub struct RobotResponse<T> {
    /// `true` when the command succeeded.
    pub ok: bool,
    /// Command-specific payload (present when `ok == true`).
    #[serde(default)]
    pub data: Option<T>,
    /// Human-readable error message (present when `ok == false`).
    #[serde(default)]
    pub error: Option<String>,
    /// Machine-readable error code like `"WA-1001"` (present when `ok == false`).
    #[serde(default)]
    pub error_code: Option<String>,
    /// Actionable hint for recovery (present on some errors).
    #[serde(default)]
    pub hint: Option<String>,
    /// Wall-clock milliseconds the command took.
    pub elapsed_ms: u64,
    /// wa version that produced this response.
    pub version: String,
    /// Unix epoch milliseconds when the response was generated.
    pub now: u64,
}

impl<T: serde::de::DeserializeOwned> RobotResponse<T> {
    /// Parse a JSON string into a typed response.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// Parse a JSON byte slice into a typed response.
    pub fn from_json_bytes(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }

    /// Returns the data if `ok == true`, otherwise returns an error with the
    /// error message and code from the response.
    pub fn into_result(self) -> Result<T, RobotError> {
        if self.ok {
            match self.data {
                Some(data) => Ok(data),
                None => Err(RobotError {
                    code: self.error_code,
                    message: "ok=true but data is null".to_string(),
                    hint: None,
                }),
            }
        } else {
            Err(RobotError {
                code: self.error_code,
                message: self.error.unwrap_or_else(|| "unknown error".to_string()),
                hint: self.hint,
            })
        }
    }

    /// Returns the parsed `ErrorCode` if present.
    pub fn parsed_error_code(&self) -> Option<ErrorCode> {
        self.error_code.as_deref().and_then(ErrorCode::parse)
    }
}

/// Error extracted from a failed `RobotResponse`.
#[derive(Debug, Clone)]
pub struct RobotError {
    /// Machine-readable error code (e.g. `"WA-1001"`).
    pub code: Option<String>,
    /// Human-readable error message.
    pub message: String,
    /// Actionable hint for recovery.
    pub hint: Option<String>,
}

impl std::fmt::Display for RobotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(code) = &self.code {
            write!(f, "[{}] {}", code, self.message)
        } else {
            write!(f, "{}", self.message)
        }
    }
}

impl std::error::Error for RobotError {}

// ============================================================================
// Error codes
// ============================================================================

/// Parsed error code from wa robot responses.
///
/// Maps the `WA-xxxx` string codes from `error_codes.rs` into a structured enum
/// for pattern matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    // WezTerm (1xxx)
    /// WA-1001: WezTerm CLI not found
    WeztermNotFound,
    /// WA-1002: WezTerm CLI execution failed
    WeztermExecFailed,
    /// WA-1003: WezTerm pane not found
    PaneNotFound,
    /// WA-1004: WezTerm output parse error
    WeztermParseFailed,
    /// WA-1005: WezTerm connection refused
    WeztermConnectionRefused,

    // Storage (2xxx)
    /// WA-2001: Database locked
    DatabaseLocked,
    /// WA-2002: Storage corruption detected
    StorageCorruption,
    /// WA-2003: FTS5 index error
    FtsIndexError,
    /// WA-2004: Migration failed
    MigrationFailed,
    /// WA-2005: Disk full
    DiskFull,

    // Pattern (3xxx)
    /// WA-3001: Invalid regex pattern
    InvalidRegex,
    /// WA-3002: Rule pack not found
    RulePackNotFound,
    /// WA-3003: Pattern match timeout
    PatternTimeout,

    // Policy (4xxx)
    /// WA-4001: Action denied by policy
    ActionDenied,
    /// WA-4002: Rate limit exceeded
    RateLimitExceeded,
    /// WA-4003: Approval required
    ApprovalRequired,
    /// WA-4004: Approval expired
    ApprovalExpired,

    // Workflow (5xxx)
    /// WA-5001: Workflow not found
    WorkflowNotFound,
    /// WA-5002: Workflow step failed
    WorkflowStepFailed,
    /// WA-5003: Workflow timeout
    WorkflowTimeout,
    /// WA-5004: Workflow already running
    WorkflowAlreadyRunning,

    // Network (6xxx)
    /// WA-6001: Network timeout
    NetworkTimeout,
    /// WA-6002: Connection refused
    ConnectionRefused,

    // Config (7xxx)
    /// WA-7001: Config file invalid
    ConfigInvalid,
    /// WA-7002: Config file not found
    ConfigNotFound,

    // Internal (9xxx)
    /// WA-9001: Internal error
    InternalError,
    /// WA-9002: Feature not available
    FeatureNotAvailable,
    /// WA-9003: Version mismatch
    VersionMismatch,

    /// Unknown code not in the catalog.
    Unknown(u16),
}

impl ErrorCode {
    /// Parse a `"WA-xxxx"` string into an `ErrorCode`.
    ///
    /// Returns `None` if the string doesn't match the `WA-` prefix.
    pub fn parse(s: &str) -> Option<Self> {
        let num_str = s.strip_prefix("WA-")?;
        let num: u16 = num_str.parse().ok()?;
        Some(Self::from_number(num))
    }

    /// Map a numeric code to the variant.
    pub fn from_number(n: u16) -> Self {
        match n {
            1001 => Self::WeztermNotFound,
            1002 => Self::WeztermExecFailed,
            1003 => Self::PaneNotFound,
            1004 => Self::WeztermParseFailed,
            1005 => Self::WeztermConnectionRefused,
            2001 => Self::DatabaseLocked,
            2002 => Self::StorageCorruption,
            2003 => Self::FtsIndexError,
            2004 => Self::MigrationFailed,
            2005 => Self::DiskFull,
            3001 => Self::InvalidRegex,
            3002 => Self::RulePackNotFound,
            3003 => Self::PatternTimeout,
            4001 => Self::ActionDenied,
            4002 => Self::RateLimitExceeded,
            4003 => Self::ApprovalRequired,
            4004 => Self::ApprovalExpired,
            5001 => Self::WorkflowNotFound,
            5002 => Self::WorkflowStepFailed,
            5003 => Self::WorkflowTimeout,
            5004 => Self::WorkflowAlreadyRunning,
            6001 => Self::NetworkTimeout,
            6002 => Self::ConnectionRefused,
            7001 => Self::ConfigInvalid,
            7002 => Self::ConfigNotFound,
            9001 => Self::InternalError,
            9002 => Self::FeatureNotAvailable,
            9003 => Self::VersionMismatch,
            other => Self::Unknown(other),
        }
    }

    /// Returns the `"WA-xxxx"` string form.
    pub fn as_str(&self) -> String {
        format!("WA-{}", self.number())
    }

    /// Returns the numeric part of the code.
    pub fn number(&self) -> u16 {
        match self {
            Self::WeztermNotFound => 1001,
            Self::WeztermExecFailed => 1002,
            Self::PaneNotFound => 1003,
            Self::WeztermParseFailed => 1004,
            Self::WeztermConnectionRefused => 1005,
            Self::DatabaseLocked => 2001,
            Self::StorageCorruption => 2002,
            Self::FtsIndexError => 2003,
            Self::MigrationFailed => 2004,
            Self::DiskFull => 2005,
            Self::InvalidRegex => 3001,
            Self::RulePackNotFound => 3002,
            Self::PatternTimeout => 3003,
            Self::ActionDenied => 4001,
            Self::RateLimitExceeded => 4002,
            Self::ApprovalRequired => 4003,
            Self::ApprovalExpired => 4004,
            Self::WorkflowNotFound => 5001,
            Self::WorkflowStepFailed => 5002,
            Self::WorkflowTimeout => 5003,
            Self::WorkflowAlreadyRunning => 5004,
            Self::NetworkTimeout => 6001,
            Self::ConnectionRefused => 6002,
            Self::ConfigInvalid => 7001,
            Self::ConfigNotFound => 7002,
            Self::InternalError => 9001,
            Self::FeatureNotAvailable => 9002,
            Self::VersionMismatch => 9003,
            Self::Unknown(n) => *n,
        }
    }

    /// Returns the error category.
    pub fn category(&self) -> ErrorCategory {
        match self.number() / 1000 {
            1 => ErrorCategory::Wezterm,
            2 => ErrorCategory::Storage,
            3 => ErrorCategory::Pattern,
            4 => ErrorCategory::Policy,
            5 => ErrorCategory::Workflow,
            6 => ErrorCategory::Network,
            7 => ErrorCategory::Config,
            _ => ErrorCategory::Internal,
        }
    }

    /// Returns `true` if this is a retryable error.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::DatabaseLocked
                | Self::RateLimitExceeded
                | Self::NetworkTimeout
                | Self::ConnectionRefused
                | Self::PatternTimeout
                | Self::WeztermConnectionRefused
        )
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

// ============================================================================
// Pane operations
// ============================================================================

/// Response data for `wa robot get-text`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetTextData {
    pub pane_id: u64,
    pub text: String,
    pub tail_lines: usize,
    pub escapes_included: bool,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub truncation_info: Option<TruncationInfo>,
}

/// Truncation details when pane output exceeds limits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TruncationInfo {
    pub original_bytes: usize,
    pub returned_bytes: usize,
    pub original_lines: usize,
    pub returned_lines: usize,
}

/// Response data for `wa robot send`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendData {
    pub pane_id: u64,
    pub injection: serde_json::Value,
    #[serde(default)]
    pub wait_for: Option<WaitForData>,
    #[serde(default)]
    pub verification_error: Option<String>,
}

/// Wait-for result data (used by send and wait-for commands).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaitForData {
    pub pane_id: u64,
    pub pattern: String,
    pub matched: bool,
    pub elapsed_ms: u64,
    pub polls: usize,
    #[serde(default)]
    pub is_regex: bool,
}

// ============================================================================
// Search & Events
// ============================================================================

/// Response data for `wa robot search`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchData {
    pub query: String,
    pub results: Vec<SearchHit>,
    pub total_hits: usize,
    pub limit: usize,
    #[serde(default)]
    pub pane_filter: Option<u64>,
    #[serde(default)]
    pub since_filter: Option<i64>,
}

/// Individual search result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub segment_id: i64,
    pub pane_id: u64,
    pub seq: u64,
    pub captured_at: i64,
    pub score: f64,
    #[serde(default)]
    pub snippet: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
}

/// Response data for `wa robot events`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventsData {
    pub events: Vec<EventItem>,
    pub total_count: usize,
    pub limit: usize,
    #[serde(default)]
    pub pane_filter: Option<u64>,
    #[serde(default)]
    pub rule_id_filter: Option<String>,
    #[serde(default)]
    pub event_type_filter: Option<String>,
    #[serde(default)]
    pub triage_state_filter: Option<String>,
    #[serde(default)]
    pub label_filter: Option<String>,
    #[serde(default)]
    pub unhandled_only: bool,
    #[serde(default)]
    pub since_filter: Option<i64>,
    #[serde(default)]
    pub would_handle: bool,
    #[serde(default)]
    pub dry_run: bool,
}

/// Individual event item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventItem {
    pub id: i64,
    pub pane_id: u64,
    pub rule_id: String,
    pub pack_id: String,
    pub event_type: String,
    pub severity: String,
    pub confidence: f64,
    #[serde(default)]
    pub extracted: Option<serde_json::Value>,
    #[serde(default)]
    pub annotations: Option<serde_json::Value>,
    pub captured_at: i64,
    #[serde(default)]
    pub handled_at: Option<i64>,
    #[serde(default)]
    pub workflow_id: Option<String>,
    #[serde(default)]
    pub would_handle_with: Option<EventWouldHandle>,
}

/// Workflow preview for events dry-run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventWouldHandle {
    pub workflow: String,
    #[serde(default)]
    pub preview_command: Option<String>,
    #[serde(default)]
    pub first_step: Option<String>,
    #[serde(default)]
    pub estimated_duration_ms: Option<u64>,
    #[serde(default)]
    pub would_run: Option<bool>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// Response data for event annotation/triage mutations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventMutationData {
    pub event_id: i64,
    #[serde(default)]
    pub changed: Option<bool>,
    pub annotations: serde_json::Value,
}

// ============================================================================
// Workflows
// ============================================================================

/// Response data for `wa robot workflow run`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowRunData {
    pub workflow_name: String,
    pub pane_id: u64,
    #[serde(default)]
    pub execution_id: Option<String>,
    pub status: String,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub started_at: Option<i64>,
    #[serde(default)]
    pub step_index: Option<usize>,
    #[serde(default)]
    pub elapsed_ms: Option<u64>,
}

/// Response data for `wa robot workflow list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowListData {
    pub workflows: Vec<WorkflowInfo>,
    pub total: usize,
    #[serde(default)]
    pub enabled_count: Option<usize>,
}

/// Individual workflow info.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowInfo {
    pub name: String,
    pub enabled: bool,
    #[serde(default)]
    pub trigger_event_types: Option<Vec<String>>,
    #[serde(default)]
    pub requires_pane: Option<bool>,
}

/// Response data for `wa robot workflow status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowStatusData {
    pub execution_id: String,
    pub workflow_name: String,
    #[serde(default)]
    pub pane_id: Option<u64>,
    #[serde(default)]
    pub trigger_event_id: Option<i64>,
    pub status: String,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub started_at: Option<i64>,
    #[serde(default)]
    pub completed_at: Option<i64>,
    #[serde(default)]
    pub current_step: Option<usize>,
    #[serde(default)]
    pub total_steps: Option<usize>,
    #[serde(default)]
    pub plan: Option<serde_json::Value>,
    #[serde(default)]
    pub created_at: Option<i64>,
}

/// Response data for workflow status list (--pane or --active).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowStatusListData {
    pub executions: Vec<WorkflowStatusData>,
    #[serde(default)]
    pub pane_filter: Option<u64>,
    #[serde(default)]
    pub active_only: Option<bool>,
    pub count: usize,
}

/// Response data for `wa robot workflow abort`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowAbortData {
    pub execution_id: String,
    pub aborted: bool,
    pub forced: bool,
    #[serde(default)]
    pub workflow_name: Option<String>,
    #[serde(default)]
    pub previous_status: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

// ============================================================================
// Rules
// ============================================================================

/// Response data for `wa robot rules list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RulesListData {
    pub rules: Vec<RuleItem>,
    #[serde(default)]
    pub pack_filter: Option<String>,
    #[serde(default)]
    pub agent_type_filter: Option<String>,
}

/// Individual rule item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleItem {
    pub id: String,
    pub agent_type: String,
    pub event_type: String,
    pub severity: String,
    pub description: String,
    #[serde(default)]
    pub workflow: Option<String>,
    pub anchor_count: usize,
    pub has_regex: bool,
}

/// Response data for `wa robot rules test`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RulesTestData {
    pub text_length: usize,
    pub match_count: usize,
    pub matches: Vec<RuleMatchItem>,
}

/// Individual rule match item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleMatchItem {
    pub rule_id: String,
    pub start: usize,
    pub end: usize,
    pub matched_text: String,
    #[serde(default)]
    pub trace: Option<RuleTraceInfo>,
}

/// Trace info for rule match debugging.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleTraceInfo {
    pub anchors_checked: bool,
    pub regex_matched: bool,
}

/// Response data for `wa robot rules show`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleDetailData {
    pub id: String,
    pub agent_type: String,
    pub event_type: String,
    pub severity: String,
    pub description: String,
    pub anchors: Vec<String>,
    #[serde(default)]
    pub regex: Option<String>,
    #[serde(default)]
    pub workflow: Option<String>,
    #[serde(default)]
    pub manual_fix: Option<String>,
    #[serde(default)]
    pub learn_more_url: Option<String>,
}

/// Response data for `wa robot rules lint`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RulesLintData {
    pub total_rules: usize,
    pub rules_checked: usize,
    pub errors: Vec<LintIssue>,
    pub warnings: Vec<LintIssue>,
    #[serde(default)]
    pub fixture_coverage: Option<FixtureCoverage>,
    pub passed: bool,
}

/// Individual lint issue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LintIssue {
    pub rule_id: String,
    pub category: String,
    pub message: String,
    #[serde(default)]
    pub suggestion: Option<String>,
}

/// Fixture coverage statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixtureCoverage {
    pub rules_with_fixtures: usize,
    pub rules_without_fixtures: Vec<String>,
    pub total_fixtures: usize,
}

// ============================================================================
// Accounts
// ============================================================================

/// Response data for `wa robot accounts list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountsListData {
    pub accounts: Vec<AccountInfo>,
    pub total: usize,
    pub service: String,
    #[serde(default)]
    pub pick_preview: Option<AccountPickPreview>,
}

/// Individual account info.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountInfo {
    pub account_id: String,
    pub service: String,
    #[serde(default)]
    pub name: Option<String>,
    pub percent_remaining: f64,
    #[serde(default)]
    pub reset_at: Option<String>,
    #[serde(default)]
    pub tokens_used: Option<i64>,
    #[serde(default)]
    pub tokens_remaining: Option<i64>,
    #[serde(default)]
    pub tokens_limit: Option<i64>,
    pub last_refreshed_at: i64,
    #[serde(default)]
    pub last_used_at: Option<i64>,
}

/// Pick preview showing which account would be selected next.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountPickPreview {
    #[serde(default)]
    pub selected_account_id: Option<String>,
    #[serde(default)]
    pub selected_name: Option<String>,
    pub selection_reason: String,
    pub threshold_percent: f64,
    pub candidates_count: usize,
    pub filtered_count: usize,
}

/// Response data for `wa robot accounts refresh`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountsRefreshData {
    pub service: String,
    pub refreshed_count: usize,
    #[serde(default)]
    pub refreshed_at: Option<String>,
    pub accounts: Vec<AccountInfo>,
}

// ============================================================================
// Reservations
// ============================================================================

/// Response data for `wa robot reserve`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReserveData {
    pub reservation: ReservationInfo,
}

/// Response data for `wa robot release`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseData {
    pub reservation_id: i64,
    pub released: bool,
}

/// Response data for `wa robot reservations list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReservationsListData {
    pub reservations: Vec<ReservationInfo>,
    pub total: usize,
}

/// Individual reservation info.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReservationInfo {
    pub id: i64,
    pub pane_id: u64,
    pub owner_kind: String,
    pub owner_id: String,
    #[serde(default)]
    pub reason: Option<String>,
    pub created_at: i64,
    pub expires_at: i64,
    #[serde(default)]
    pub released_at: Option<i64>,
    pub status: String,
}

// ============================================================================
// Meta / Diagnostics
// ============================================================================

/// Response data for `wa robot why`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhyData {
    pub code: String,
    pub category: String,
    pub title: String,
    pub explanation: String,
    #[serde(default)]
    pub suggestions: Option<Vec<String>>,
    #[serde(default)]
    pub see_also: Option<Vec<String>>,
}

/// Response data for `wa robot approve`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApproveData {
    pub code: String,
    pub valid: bool,
    #[serde(default)]
    pub created_at: Option<u64>,
    #[serde(default)]
    pub action_kind: Option<String>,
    #[serde(default)]
    pub pane_id: Option<u64>,
    #[serde(default)]
    pub expires_at: Option<u64>,
    #[serde(default)]
    pub action_fingerprint: Option<String>,
    #[serde(default)]
    pub dry_run: Option<bool>,
}

/// Response data for `wa robot quick-start`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuickStartData {
    pub description: String,
    pub global_flags: Vec<QuickStartGlobalFlag>,
    pub core_loop: Vec<QuickStartStep>,
    pub commands: Vec<QuickStartCommand>,
    pub tips: Vec<String>,
    pub error_handling: QuickStartErrorHandling,
}

/// Global flag for quick-start.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuickStartGlobalFlag {
    pub flag: String,
    #[serde(default)]
    pub env_var: Option<String>,
    pub description: String,
}

/// Step in the core loop for quick-start.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuickStartStep {
    pub step: u8,
    pub action: String,
    pub command: String,
}

/// Command entry for quick-start.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuickStartCommand {
    pub name: String,
    pub args: String,
    pub summary: String,
    pub examples: Vec<String>,
}

/// Error handling section for quick-start.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuickStartErrorHandling {
    pub common_codes: Vec<QuickStartErrorCode>,
    pub safety_notes: Vec<String>,
}

/// Common error code entry for quick-start.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuickStartErrorCode {
    pub code: String,
    pub meaning: String,
    pub recovery: String,
}

// ============================================================================
// Convenience parsing
// ============================================================================

/// Parse a raw JSON string into a typed `RobotResponse<T>`.
///
/// This is a convenience wrapper around `serde_json::from_str`.
pub fn parse_response<T: serde::de::DeserializeOwned>(
    json: &str,
) -> Result<RobotResponse<T>, serde_json::Error> {
    serde_json::from_str(json)
}

/// Parse a raw JSON string into a `RobotResponse<serde_json::Value>` for
/// untyped access when the data type is not known at compile time.
pub fn parse_response_untyped(
    json: &str,
) -> Result<RobotResponse<serde_json::Value>, serde_json::Error> {
    serde_json::from_str(json)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- Envelope parsing ---------------------------------------------------

    #[test]
    fn parse_success_envelope() {
        let json = json!({
            "ok": true,
            "data": {"pane_id": 1, "text": "hello", "tail_lines": 100, "escapes_included": false},
            "elapsed_ms": 5,
            "version": "0.1.0",
            "now": 1700000000000u64
        });
        let resp: RobotResponse<GetTextData> = serde_json::from_value(json).unwrap();
        assert!(resp.ok);
        let data = resp.data.unwrap();
        assert_eq!(data.pane_id, 1);
        assert_eq!(data.text, "hello");
        assert_eq!(data.tail_lines, 100);
        assert!(!data.escapes_included);
        assert!(!data.truncated);
        assert!(data.truncation_info.is_none());
    }

    #[test]
    fn parse_error_envelope() {
        let json = json!({
            "ok": false,
            "error": "pane 42 not found",
            "error_code": "WA-1003",
            "hint": "check wa list-panes",
            "elapsed_ms": 2,
            "version": "0.1.0",
            "now": 1700000000000u64
        });
        let resp: RobotResponse<GetTextData> = serde_json::from_value(json).unwrap();
        assert!(!resp.ok);
        assert!(resp.data.is_none());
        assert_eq!(resp.error.as_deref(), Some("pane 42 not found"));
        assert_eq!(resp.error_code.as_deref(), Some("WA-1003"));
        assert_eq!(resp.hint.as_deref(), Some("check wa list-panes"));
    }

    #[test]
    fn into_result_ok() {
        let json = json!({
            "ok": true,
            "data": {"pane_id": 1, "text": "x", "tail_lines": 1, "escapes_included": false},
            "elapsed_ms": 1,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<GetTextData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert_eq!(data.text, "x");
    }

    #[test]
    fn into_result_err() {
        let json = json!({
            "ok": false,
            "error": "denied",
            "error_code": "WA-4001",
            "elapsed_ms": 1,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<GetTextData> = serde_json::from_value(json).unwrap();
        let err = resp.into_result().unwrap_err();
        assert_eq!(err.message, "denied");
        assert_eq!(err.code.as_deref(), Some("WA-4001"));
        assert!(err.to_string().contains("WA-4001"));
    }

    #[test]
    fn into_result_ok_null_data() {
        let json = json!({
            "ok": true,
            "elapsed_ms": 1,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<GetTextData> = serde_json::from_value(json).unwrap();
        let err = resp.into_result().unwrap_err();
        assert!(err.message.contains("null"));
    }

    // -- Error code parsing -------------------------------------------------

    #[test]
    fn error_code_roundtrip() {
        for code in ["WA-1001", "WA-2003", "WA-4001", "WA-5004", "WA-9003"] {
            let parsed = ErrorCode::parse(code).unwrap();
            assert_eq!(parsed.as_str(), code);
        }
    }

    #[test]
    fn error_code_unknown() {
        let parsed = ErrorCode::parse("WA-8888").unwrap();
        assert_eq!(parsed, ErrorCode::Unknown(8888));
        assert_eq!(parsed.number(), 8888);
    }

    #[test]
    fn error_code_invalid_prefix() {
        assert!(ErrorCode::parse("XX-1001").is_none());
        assert!(ErrorCode::parse("garbage").is_none());
    }

    #[test]
    fn error_code_categories() {
        assert_eq!(
            ErrorCode::WeztermNotFound.category(),
            ErrorCategory::Wezterm
        );
        assert_eq!(ErrorCode::DatabaseLocked.category(), ErrorCategory::Storage);
        assert_eq!(ErrorCode::InvalidRegex.category(), ErrorCategory::Pattern);
        assert_eq!(ErrorCode::ActionDenied.category(), ErrorCategory::Policy);
        assert_eq!(
            ErrorCode::WorkflowNotFound.category(),
            ErrorCategory::Workflow
        );
        assert_eq!(ErrorCode::NetworkTimeout.category(), ErrorCategory::Network);
        assert_eq!(ErrorCode::ConfigInvalid.category(), ErrorCategory::Config);
        assert_eq!(ErrorCode::InternalError.category(), ErrorCategory::Internal);
    }

    #[test]
    fn error_code_retryable() {
        assert!(ErrorCode::DatabaseLocked.is_retryable());
        assert!(ErrorCode::RateLimitExceeded.is_retryable());
        assert!(ErrorCode::NetworkTimeout.is_retryable());
        assert!(!ErrorCode::ActionDenied.is_retryable());
        assert!(!ErrorCode::ConfigInvalid.is_retryable());
        assert!(!ErrorCode::InternalError.is_retryable());
    }

    #[test]
    fn parsed_error_code_from_response() {
        let json = json!({
            "ok": false,
            "error": "locked",
            "error_code": "WA-2001",
            "elapsed_ms": 1,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<GetTextData> = serde_json::from_value(json).unwrap();
        assert_eq!(resp.parsed_error_code(), Some(ErrorCode::DatabaseLocked));
    }

    // -- Data type parsing --------------------------------------------------

    #[test]
    fn parse_get_text_with_truncation() {
        let json = json!({
            "ok": true,
            "data": {
                "pane_id": 3,
                "text": "output...",
                "tail_lines": 50,
                "escapes_included": true,
                "truncated": true,
                "truncation_info": {
                    "original_bytes": 10000,
                    "returned_bytes": 5000,
                    "original_lines": 200,
                    "returned_lines": 50
                }
            },
            "elapsed_ms": 12,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<GetTextData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert!(data.truncated);
        let info = data.truncation_info.unwrap();
        assert_eq!(info.original_bytes, 10000);
        assert_eq!(info.returned_lines, 50);
    }

    #[test]
    fn parse_wait_for_data() {
        let json = json!({
            "ok": true,
            "data": {
                "pane_id": 1,
                "pattern": "\\$",
                "matched": true,
                "elapsed_ms": 500,
                "polls": 10,
                "is_regex": true
            },
            "elapsed_ms": 510,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<WaitForData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert!(data.matched);
        assert!(data.is_regex);
        assert_eq!(data.polls, 10);
    }

    #[test]
    fn parse_search_data() {
        let json = json!({
            "ok": true,
            "data": {
                "query": "error",
                "results": [{
                    "segment_id": 1,
                    "pane_id": 2,
                    "seq": 5,
                    "captured_at": 1700000000000i64,
                    "score": 1.5,
                    "snippet": "...error occurred..."
                }],
                "total_hits": 1,
                "limit": 20
            },
            "elapsed_ms": 3,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<SearchData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert_eq!(data.total_hits, 1);
        assert!((data.results[0].score - 1.5).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_events_data() {
        let json = json!({
            "ok": true,
            "data": {
                "events": [{
                    "id": 42,
                    "pane_id": 1,
                    "rule_id": "codex.build_error",
                    "pack_id": "codex",
                    "event_type": "error",
                    "severity": "high",
                    "confidence": 0.95,
                    "captured_at": 1700000000000i64
                }],
                "total_count": 1,
                "limit": 50,
                "unhandled_only": false
            },
            "elapsed_ms": 8,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<EventsData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert_eq!(data.events.len(), 1);
        assert_eq!(data.events[0].rule_id, "codex.build_error");
        assert!((data.events[0].confidence - 0.95).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_workflow_run_data() {
        let json = json!({
            "ok": true,
            "data": {
                "workflow_name": "fix_build",
                "pane_id": 1,
                "execution_id": "exec-abc",
                "status": "running",
                "started_at": 1700000000000i64
            },
            "elapsed_ms": 15,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<WorkflowRunData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert_eq!(data.workflow_name, "fix_build");
        assert_eq!(data.status, "running");
    }

    #[test]
    fn parse_workflow_list_data() {
        let json = json!({
            "ok": true,
            "data": {
                "workflows": [
                    {"name": "fix_build", "enabled": true},
                    {"name": "notify", "enabled": false}
                ],
                "total": 2,
                "enabled_count": 1
            },
            "elapsed_ms": 2,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<WorkflowListData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert_eq!(data.total, 2);
        assert!(data.workflows[0].enabled);
        assert!(!data.workflows[1].enabled);
    }

    #[test]
    fn parse_rules_list_data() {
        let json = json!({
            "ok": true,
            "data": {
                "rules": [{
                    "id": "codex.build_error",
                    "agent_type": "codex",
                    "event_type": "error",
                    "severity": "high",
                    "description": "Build error detected",
                    "anchor_count": 3,
                    "has_regex": true
                }],
                "pack_filter": "codex"
            },
            "elapsed_ms": 1,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<RulesListData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert_eq!(data.rules[0].id, "codex.build_error");
        assert!(data.rules[0].has_regex);
    }

    #[test]
    fn parse_rules_test_data() {
        let json = json!({
            "ok": true,
            "data": {
                "text_length": 500,
                "match_count": 2,
                "matches": [
                    {
                        "rule_id": "codex.build_error",
                        "start": 10,
                        "end": 30,
                        "matched_text": "error: cannot find",
                        "trace": {
                            "anchors_checked": true,
                            "regex_matched": true
                        }
                    }
                ]
            },
            "elapsed_ms": 5,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<RulesTestData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert_eq!(data.match_count, 2);
        assert!(data.matches[0].trace.as_ref().unwrap().regex_matched);
    }

    #[test]
    fn parse_why_data() {
        let json = json!({
            "ok": true,
            "data": {
                "code": "WA-2001",
                "category": "storage",
                "title": "Database locked",
                "explanation": "SQLite database is locked by another process.",
                "suggestions": ["retry after 1s", "check for hung wa processes"]
            },
            "elapsed_ms": 1,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<WhyData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert_eq!(data.code, "WA-2001");
        assert_eq!(data.suggestions.unwrap().len(), 2);
    }

    #[test]
    fn parse_approve_data() {
        let json = json!({
            "ok": true,
            "data": {
                "code": "AP-abc123",
                "valid": true,
                "created_at": 1700000000000u64,
                "action_kind": "send_text",
                "pane_id": 1,
                "expires_at": 1700000060000u64
            },
            "elapsed_ms": 1,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<ApproveData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert!(data.valid);
        assert_eq!(data.action_kind.as_deref(), Some("send_text"));
    }

    #[test]
    fn parse_accounts_list_data() {
        let json = json!({
            "ok": true,
            "data": {
                "accounts": [{
                    "account_id": "acc-1",
                    "service": "anthropic",
                    "percent_remaining": 85.5,
                    "last_refreshed_at": 1700000000000i64
                }],
                "total": 1,
                "service": "anthropic"
            },
            "elapsed_ms": 3,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<AccountsListData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert!((data.accounts[0].percent_remaining - 85.5).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_reservations_list_data() {
        let json = json!({
            "ok": true,
            "data": {
                "reservations": [{
                    "id": 1,
                    "pane_id": 5,
                    "owner_kind": "agent",
                    "owner_id": "codex-1",
                    "reason": "build monitoring",
                    "created_at": 1700000000000i64,
                    "expires_at": 1700000060000i64,
                    "status": "active"
                }],
                "total": 1
            },
            "elapsed_ms": 2,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<ReservationsListData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert_eq!(data.reservations[0].status, "active");
        assert_eq!(
            data.reservations[0].reason.as_deref(),
            Some("build monitoring")
        );
    }

    #[test]
    fn parse_untyped_response() {
        let raw = r#"{"ok":true,"data":{"foo":"bar"},"elapsed_ms":1,"version":"0.1.0","now":0}"#;
        let resp = parse_response_untyped(raw).unwrap();
        assert!(resp.ok);
        assert_eq!(resp.data.unwrap()["foo"], "bar");
    }

    #[test]
    fn from_json_convenience() {
        let raw = r#"{"ok":true,"data":{"pane_id":1,"text":"hi","tail_lines":10,"escapes_included":false},"elapsed_ms":1,"version":"0.1.0","now":0}"#;
        let resp = RobotResponse::<GetTextData>::from_json(raw).unwrap();
        assert_eq!(resp.data.unwrap().text, "hi");
    }

    #[test]
    fn tolerant_of_missing_optional_fields() {
        // Minimal envelope with only required fields in data
        let json = json!({
            "ok": true,
            "data": {
                "events": [],
                "total_count": 0,
                "limit": 20
            },
            "elapsed_ms": 1,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<EventsData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert!(data.pane_filter.is_none());
        assert!(!data.unhandled_only);
        assert!(!data.would_handle);
    }

    #[test]
    fn robot_error_display() {
        let err = RobotError {
            code: Some("WA-1003".to_string()),
            message: "pane not found".to_string(),
            hint: Some("check pane id".to_string()),
        };
        assert_eq!(err.to_string(), "[WA-1003] pane not found");

        let err_no_code = RobotError {
            code: None,
            message: "something failed".to_string(),
            hint: None,
        };
        assert_eq!(err_no_code.to_string(), "something failed");
    }

    #[test]
    fn quick_start_data_parses() {
        let json = json!({
            "ok": true,
            "data": {
                "description": "Quick start guide",
                "global_flags": [{"flag": "--pane", "env_var": "WA_PANE", "description": "target pane"}],
                "core_loop": [{"step": 1, "action": "get text", "command": "wa robot get-text"}],
                "commands": [{
                    "name": "get-text",
                    "args": "--pane <ID>",
                    "summary": "Get pane text",
                    "examples": ["wa robot get-text --pane 1"]
                }],
                "tips": ["use --format json"],
                "error_handling": {
                    "common_codes": [{"code": "WA-1003", "meaning": "pane not found", "recovery": "check id"}],
                    "safety_notes": ["always check ok field"]
                }
            },
            "elapsed_ms": 1,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<QuickStartData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert_eq!(data.global_flags.len(), 1);
        assert_eq!(data.core_loop[0].step, 1);
        assert_eq!(data.commands[0].name, "get-text");
        assert_eq!(data.error_handling.common_codes[0].code, "WA-1003");
    }

    #[test]
    fn workflow_abort_parses() {
        let json = json!({
            "ok": true,
            "data": {
                "execution_id": "exec-xyz",
                "aborted": true,
                "forced": false,
                "workflow_name": "fix_build",
                "previous_status": "running"
            },
            "elapsed_ms": 5,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<WorkflowAbortData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert!(data.aborted);
        assert!(!data.forced);
    }

    #[test]
    fn rules_lint_parses() {
        let json = json!({
            "ok": true,
            "data": {
                "total_rules": 50,
                "rules_checked": 48,
                "errors": [],
                "warnings": [{"rule_id": "x.y", "category": "style", "message": "no desc"}],
                "passed": true
            },
            "elapsed_ms": 30,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<RulesLintData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert!(data.passed);
        assert_eq!(data.warnings.len(), 1);
    }

    #[test]
    fn event_mutation_parses() {
        let json = json!({
            "ok": true,
            "data": {
                "event_id": 99,
                "changed": true,
                "annotations": {"triage_state": "resolved"}
            },
            "elapsed_ms": 2,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<EventMutationData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert_eq!(data.event_id, 99);
        assert_eq!(data.changed, Some(true));
    }

    #[test]
    fn rule_detail_parses() {
        let json = json!({
            "ok": true,
            "data": {
                "id": "codex.build_error",
                "agent_type": "codex",
                "event_type": "error",
                "severity": "high",
                "description": "Build error detected",
                "anchors": ["error:", "failed"],
                "regex": "error\\[E\\d+\\]",
                "workflow": "fix_build"
            },
            "elapsed_ms": 1,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<RuleDetailData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert_eq!(data.anchors.len(), 2);
        assert!(data.regex.is_some());
    }

    #[test]
    fn workflow_status_list_parses() {
        let json = json!({
            "ok": true,
            "data": {
                "executions": [{
                    "execution_id": "exec-1",
                    "workflow_name": "fix_build",
                    "status": "completed"
                }],
                "count": 1,
                "active_only": true
            },
            "elapsed_ms": 3,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<WorkflowStatusListData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert_eq!(data.count, 1);
        assert_eq!(data.executions[0].status, "completed");
    }

    #[test]
    fn send_data_parses() {
        let json = json!({
            "ok": true,
            "data": {
                "pane_id": 1,
                "injection": {"status": "allowed", "summary": "echo hello", "pane_id": 1, "action": "send_text", "decision": {"decision": "allow"}}
            },
            "elapsed_ms": 50,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<SendData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert_eq!(data.pane_id, 1);
        assert!(data.injection.is_object());
    }

    #[test]
    fn reserve_data_parses() {
        let json = json!({
            "ok": true,
            "data": {
                "reservation": {
                    "id": 7,
                    "pane_id": 3,
                    "owner_kind": "agent",
                    "owner_id": "codex-1",
                    "created_at": 1700000000000i64,
                    "expires_at": 1700000060000i64,
                    "status": "active"
                }
            },
            "elapsed_ms": 4,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<ReserveData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert_eq!(data.reservation.id, 7);
        assert_eq!(data.reservation.status, "active");
    }

    #[test]
    fn release_data_parses() {
        let json = json!({
            "ok": true,
            "data": {
                "reservation_id": 7,
                "released": true
            },
            "elapsed_ms": 2,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<ReleaseData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert!(data.released);
    }

    #[test]
    fn accounts_refresh_parses() {
        let json = json!({
            "ok": true,
            "data": {
                "service": "anthropic",
                "refreshed_count": 2,
                "refreshed_at": "2025-01-01T00:00:00Z",
                "accounts": [
                    {"account_id": "a1", "service": "anthropic", "percent_remaining": 90.0, "last_refreshed_at": 0},
                    {"account_id": "a2", "service": "anthropic", "percent_remaining": 50.0, "last_refreshed_at": 0}
                ]
            },
            "elapsed_ms": 100,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<AccountsRefreshData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert_eq!(data.refreshed_count, 2);
        assert_eq!(data.accounts.len(), 2);
    }
}
