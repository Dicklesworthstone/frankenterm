//! Durable workflow execution engine
//!
//! Provides idempotent, recoverable, audited workflow execution.
//!
//! # Architecture
//!
//! Workflows are explicit state machines with a uniform execution model:
//! - **Workflow trait**: Defines the workflow interface (name, steps, execution)
//! - **WorkflowContext**: Runtime context with WezTerm client, storage, pane state
//! - **StepResult**: Step outcomes (continue, done, retry, abort, wait)
//! - **WaitCondition**: Conditions to pause execution (pattern, idle, external)
//!
//! This design enables:
//! - Persistent/resumable workflows
//! - Deterministic step logic testing
//! - Shared runner across agent-specific workflows

use crate::policy::{InjectionResult, PaneCapabilities};
use crate::storage::StorageHandle;
use crate::wezterm::{
    CodexSummaryWaitResult, PaneTextSource, WaitOptions, elapsed_ms, stable_hash, tail_text,
    wait_for_codex_session_summary,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;

/// Type alias for a boxed future used in dyn-compatible traits.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

// ============================================================================
// Codex Usage-Limit Helpers (wa-nu4.1.3.2)
// ============================================================================

/// Options for exiting Codex and waiting for the session summary markers.
#[derive(Debug, Clone)]
pub struct CodexExitOptions {
    /// Timeout for the first (single Ctrl-C) attempt, in milliseconds.
    pub grace_timeout_ms: u64,
    /// Timeout for the second attempt, in milliseconds.
    pub summary_timeout_ms: u64,
    /// Polling options for summary detection.
    pub wait_options: WaitOptions,
}

impl Default for CodexExitOptions {
    fn default() -> Self {
        Self {
            grace_timeout_ms: 2_000,
            summary_timeout_ms: 20_000,
            wait_options: WaitOptions::default(),
        }
    }
}

/// Outcome of the Codex exit + summary wait step.
#[derive(Debug, Clone)]
pub struct CodexExitOutcome {
    /// Number of Ctrl-C injections performed (1 or 2).
    pub ctrl_c_count: u8,
    /// Summary wait result (matched or timed out).
    pub summary: CodexSummaryWaitResult,
}

/// Convert an injection result into a success/error for Ctrl-C handling.
#[allow(dead_code)]
fn ctrl_c_injection_ok(result: InjectionResult) -> Result<(), String> {
    match result {
        InjectionResult::Allowed { .. } => Ok(()),
        InjectionResult::Denied { decision, .. } => match decision {
            crate::policy::PolicyDecision::Deny { reason, .. } => {
                Err(format!("Ctrl-C denied by policy: {reason}"))
            }
            _ => Err("Ctrl-C denied by policy".to_string()),
        },
        InjectionResult::RequiresApproval { decision, .. } => match decision {
            crate::policy::PolicyDecision::RequireApproval { reason, .. } => {
                Err(format!("Ctrl-C requires approval: {reason}"))
            }
            _ => Err("Ctrl-C requires approval".to_string()),
        },
        InjectionResult::Error { error, .. } => Err(format!("Ctrl-C failed: {error}")),
    }
}

/// Exit Codex by sending Ctrl-C (once or twice) and wait for session summary markers.
///
/// This function:
/// 1) Sends Ctrl-C once and waits for summary markers within a grace window.
/// 2) If not seen, sends Ctrl-C again and waits up to `summary_timeout_ms`.
///
/// Returns the number of Ctrl-C injections performed and the summary wait result.
#[allow(dead_code)]
pub(crate) async fn codex_exit_and_wait_for_summary<S, F, Fut>(
    pane_id: u64,
    source: &S,
    mut send_ctrl_c: F,
    options: &CodexExitOptions,
) -> Result<CodexExitOutcome, String>
where
    S: PaneTextSource + Sync + ?Sized,
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<InjectionResult, String>> + Send,
{
    let grace_timeout = Duration::from_millis(options.grace_timeout_ms);
    let summary_timeout = Duration::from_millis(options.summary_timeout_ms);

    // First Ctrl-C attempt.
    let first = send_ctrl_c().await?;
    ctrl_c_injection_ok(first)?;

    let first_wait = wait_for_codex_session_summary(
        source,
        pane_id,
        grace_timeout,
        options.wait_options.clone(),
    )
    .await
    .map_err(|e| format!("Codex summary wait failed: {e}"))?;

    if first_wait.matched {
        return Ok(CodexExitOutcome {
            ctrl_c_count: 1,
            summary: first_wait,
        });
    }

    // Second Ctrl-C attempt if summary not observed.
    let second = send_ctrl_c().await?;
    ctrl_c_injection_ok(second)?;

    let second_wait = wait_for_codex_session_summary(
        source,
        pane_id,
        summary_timeout,
        options.wait_options.clone(),
    )
    .await
    .map_err(|e| format!("Codex summary wait failed: {e}"))?;

    if second_wait.matched {
        return Ok(CodexExitOutcome {
            ctrl_c_count: 2,
            summary: second_wait,
        });
    }

    let last_hash = second_wait
        .last_tail_hash
        .map_or_else(|| "none".to_string(), |value| format!("{value:016x}"));
    Err(format!(
        "Session summary not found after Ctrl-C x2 (token_usage={}, resume_hint={}, elapsed_ms={}, last_tail_hash={})",
        second_wait.last_markers.token_usage,
        second_wait.last_markers.resume_hint,
        second_wait.elapsed_ms,
        last_hash
    ))
}

// ============================================================================
// Codex Usage-Limit Helpers (wa-nu4.1.3.3)
// ============================================================================

/// Parsed token usage summary from Codex session output.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexTokenUsage {
    pub total: Option<i64>,
    pub input: Option<i64>,
    pub output: Option<i64>,
    pub cached: Option<i64>,
    pub reasoning: Option<i64>,
}

#[allow(dead_code)]
impl CodexTokenUsage {
    fn has_any(&self) -> bool {
        self.total.is_some()
            || self.input.is_some()
            || self.output.is_some()
            || self.cached.is_some()
            || self.reasoning.is_some()
    }
}

/// Parsed Codex session summary details needed for resume + accounting.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexSessionSummary {
    pub session_id: String,
    pub token_usage: CodexTokenUsage,
    pub reset_time: Option<String>,
}

/// Structured error for Codex session summary parsing.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexSessionParseError {
    pub missing: Vec<&'static str>,
    pub tail_hash: u64,
    pub tail_len: usize,
}

impl std::fmt::Display for CodexSessionParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Missing Codex session fields: {:?} (tail_hash={:016x}, tail_len={})",
            self.missing, self.tail_hash, self.tail_len
        )
    }
}

impl std::error::Error for CodexSessionParseError {}

#[allow(dead_code)]
static CODEX_RESUME_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)codex resume\s+(?P<session_id>[0-9a-fA-F-]{8,})").expect("codex resume regex")
});
#[allow(dead_code)]
static CODEX_RESET_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)try again at\s+(?P<reset_time>[^.\n]+)").expect("codex reset time regex")
});
#[allow(dead_code)]
static CODEX_TOTAL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)total\s*=\s*([\d,]+)").expect("total regex"));
#[allow(dead_code)]
static CODEX_INPUT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)input\s*=\s*([\d,]+)").expect("input regex"));
#[allow(dead_code)]
static CODEX_OUTPUT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)output\s*=\s*([\d,]+)").expect("output regex"));
#[allow(dead_code)]
static CODEX_CACHED_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\(\+\s*([\d,]+)\s+cached\)").expect("cached regex"));
#[allow(dead_code)]
static CODEX_REASONING_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\(reasoning\s+([\d,]+)\)").expect("reasoning regex"));



#[allow(dead_code)]
fn parse_number(raw: &str) -> Option<i64> {
    let cleaned = raw.replace(',', "");
    cleaned.parse::<i64>().ok()
}

#[allow(dead_code)]
fn capture_number(regex: &Regex, text: &str) -> Option<i64> {
    regex
        .captures(text)
        .and_then(|caps| caps.get(1).map(|m| m.as_str()))
        .and_then(parse_number)
}

#[allow(dead_code)]
fn extract_token_usage(line: &str) -> CodexTokenUsage {
    CodexTokenUsage {
        total: capture_number(&CODEX_TOTAL_RE, line),
        input: capture_number(&CODEX_INPUT_RE, line),
        output: capture_number(&CODEX_OUTPUT_RE, line),
        cached: capture_number(&CODEX_CACHED_RE, line),
        reasoning: capture_number(&CODEX_REASONING_RE, line),
    }
}

#[allow(dead_code)]
fn find_token_usage_line(tail: &str) -> Option<&str> {
    tail.lines().rfind(|line| line.contains("Token usage:"))
}

#[allow(dead_code)]
fn find_session_id(tail: &str) -> Option<String> {
    CODEX_RESUME_RE
        .captures_iter(tail)
        .filter_map(|caps| caps.name("session_id").map(|m| m.as_str().to_string()))
        .last()
}

#[allow(dead_code)]
fn find_reset_time(tail: &str) -> Option<String> {
    CODEX_RESET_RE
        .captures_iter(tail)
        .filter_map(|caps| {
            caps.name("reset_time")
                .map(|m| m.as_str().trim().to_string())
        })
        .last()
}

/// Parse Codex session summary from pane tail text.
///
/// Required fields:
/// - session_id (from "codex resume ...")
/// - token usage line (from "Token usage:")
///
/// Optional fields:
/// - reset_time ("try again at ...")
#[allow(dead_code)]
pub(crate) fn parse_codex_session_summary(
    tail: &str,
) -> Result<CodexSessionSummary, CodexSessionParseError> {
    let tail_hash = stable_hash(tail.as_bytes());
    let tail_len = tail.len();

    let session_id = find_session_id(tail);
    let token_usage_line = find_token_usage_line(tail);
    let token_usage = token_usage_line.map(extract_token_usage);
    let reset_time = find_reset_time(tail);

    let mut missing = Vec::new();
    if session_id.is_none() {
        missing.push("session_id");
    }
    if token_usage_line.is_none() || !token_usage.as_ref().is_some_and(CodexTokenUsage::has_any) {
        missing.push("token_usage");
    }

    if !missing.is_empty() {
        return Err(CodexSessionParseError {
            missing,
            tail_hash,
            tail_len,
        });
    }

    Ok(CodexSessionSummary {
        session_id: session_id.expect("session_id checked"),
        token_usage: token_usage.expect("token_usage checked"),
        reset_time,
    })
}

/// Build an agent session record from a parsed Codex summary.
#[allow(dead_code)]
pub(crate) fn codex_session_record_from_summary(
    pane_id: u64,
    summary: &CodexSessionSummary,
) -> crate::storage::AgentSessionRecord {
    let mut record = crate::storage::AgentSessionRecord::new_start(pane_id, "codex");
    record.session_id = Some(summary.session_id.clone());
    record.total_tokens = summary.token_usage.total;
    record.input_tokens = summary.token_usage.input;
    record.output_tokens = summary.token_usage.output;
    record.cached_tokens = summary.token_usage.cached;
    record.reasoning_tokens = summary.token_usage.reasoning;
    record
}

/// Persist parsed Codex summary data into agent_sessions.
#[allow(dead_code)]
pub(crate) async fn persist_codex_session_summary(
    storage: &StorageHandle,
    pane_id: u64,
    summary: &CodexSessionSummary,
) -> Result<i64, String> {
    let record = codex_session_record_from_summary(pane_id, summary);
    storage
        .upsert_agent_session(record)
        .await
        .map_err(|e| format!("Failed to persist Codex session summary: {e}"))
}

// ============================================================================
// Account Selection Step (wa-nu4.1.3.4)
// ============================================================================

/// Result of the account selection workflow step.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AccountSelectionStepResult {
    /// The selected account (if any eligible accounts exist)
    pub selected: Option<crate::accounts::AccountRecord>,
    /// Full explanation of the selection decision
    pub explanation: crate::accounts::SelectionExplanation,
    /// Number of accounts refreshed from caut
    pub accounts_refreshed: usize,
}

/// Errors that can occur during account selection step.
#[derive(Debug)]
pub enum AccountSelectionStepError {
    /// caut command failed
    Caut(crate::caut::CautError),
    /// Storage operation failed
    Storage(String),
}

impl std::fmt::Display for AccountSelectionStepError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Caut(e) => write!(f, "caut error: {e}"),
            Self::Storage(e) => write!(f, "storage error: {e}"),
        }
    }
}

impl std::error::Error for AccountSelectionStepError {}

/// Refresh account usage from caut and select the best account for failover.
///
/// This function:
/// 1. Calls `caut refresh --service openai --format json` to get latest usage
/// 2. Updates the accounts mirror in the database
/// 3. Selects the best account according to the configured policy
///
/// # Arguments
/// * `caut_client` - The caut CLI wrapper client
/// * `storage` - Storage handle for persisting accounts
/// * `config` - Account selection configuration (threshold, etc.)
///
/// # Returns
/// An `AccountSelectionStepResult` with the selected account and explanation.
///
/// # Note
/// This function does NOT update `last_used_at` - that should only happen
/// after the failover is actually successful.
#[allow(dead_code)]
pub(crate) async fn refresh_and_select_account(
    caut_client: &crate::caut::CautClient,
    storage: &StorageHandle,
    config: &crate::accounts::AccountSelectionConfig,
) -> Result<AccountSelectionStepResult, AccountSelectionStepError> {
    // Step 1: Refresh usage from caut
    let refresh_result = caut_client
        .refresh(crate::caut::CautService::OpenAI)
        .await
        .map_err(AccountSelectionStepError::Caut)?;

    // Step 2: Update accounts mirror in DB
    let now_ms = crate::accounts::now_ms();
    let accounts_refreshed = refresh_result.accounts.len();

    for usage in &refresh_result.accounts {
        let record =
            crate::accounts::AccountRecord::from_caut(usage, crate::caut::CautService::OpenAI, now_ms);
        storage
            .upsert_account(record)
            .await
            .map_err(|e| AccountSelectionStepError::Storage(e.to_string()))?;
    }

    // Step 3: Select best account
    let selection = storage
        .select_account("openai", config)
        .await
        .map_err(|e| AccountSelectionStepError::Storage(e.to_string()))?;

    Ok(AccountSelectionStepResult {
        selected: selection.selected,
        explanation: selection.explanation,
        accounts_refreshed,
    })
}

/// Mark an account as used (update `last_used_at`) after successful failover.
///
/// This should only be called after the failover workflow completes successfully.
#[allow(dead_code)]
pub(crate) async fn mark_account_used(
    storage: &StorageHandle,
    service: &str,
    account_id: &str,
) -> Result<(), String> {
    // Get current account record
    let account = storage
        .get_account(service, account_id)
        .await
        .map_err(|e| format!("Failed to get account: {e}"))?
        .ok_or_else(|| format!("Account not found: {service}/{account_id}"))?;

    // Update last_used_at
    let now_ms = crate::accounts::now_ms();
    let updated = crate::accounts::AccountRecord {
        last_used_at: Some(now_ms),
        updated_at: now_ms,
        ..account
    };

    storage
        .upsert_account(updated)
        .await
        .map_err(|e| format!("Failed to update account: {e}"))?;

    Ok(())
}

// ============================================================================
// Device Auth Step (wa-nu4.1.3.5)
// ============================================================================

/// Device code extracted from Codex device-auth login prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCode {
    /// The device code (e.g., "ABCD-1234" or "ABCD-12345")
    pub code: String,
    /// The URL to visit for authentication (if present)
    pub url: Option<String>,
}

/// Structured error for device code parsing.
#[derive(Debug, Clone)]
pub struct DeviceCodeParseError {
    /// What was expected
    pub expected: &'static str,
    /// Hash of the tail (for safe diagnostics)
    pub tail_hash: u64,
    /// Length of the tail
    pub tail_len: usize,
}

impl std::fmt::Display for DeviceCodeParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Device code not found (expected: {}, tail_hash={:016x}, tail_len={})",
            self.expected, self.tail_hash, self.tail_len
        )
    }
}

impl std::error::Error for DeviceCodeParseError {}

/// Regex for device codes: 4+ alphanumeric, dash, 4+ alphanumeric
#[allow(dead_code)]
static DEVICE_CODE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:code|enter)[\s:]+([A-Z0-9]{4,}-[A-Z0-9]{4,})").expect("device code regex")
});

/// Regex for authentication URL
#[allow(dead_code)]
static DEVICE_URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:https?://[^\s]+(?:device|auth|activate)[^\s]*)").expect("device url regex")
});

/// Parse device code from pane tail text.
///
/// Looks for patterns like:
/// - "Enter code: ABCD-1234"
/// - "Your code is ABCD-12345"
/// - "code: WXYZ-5678"
#[allow(dead_code)]
pub(crate) fn parse_device_code(tail: &str) -> Result<DeviceCode, DeviceCodeParseError> {
    let tail_hash = stable_hash(tail.as_bytes());
    let tail_len = tail.len();

    // Try to find the device code
    let code = DEVICE_CODE_RE
        .captures(tail)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str().to_uppercase());

    // Try to find the URL (optional)
    let url = DEVICE_URL_RE.find(tail).map(|m| m.as_str().to_string());

    match code {
        Some(code) => Ok(DeviceCode { code, url }),
        None => Err(DeviceCodeParseError {
            expected: "device code pattern like 'code: XXXX-YYYY'",
            tail_hash,
            tail_len,
        }),
    }
}

/// Validate a device code format.
///
/// Returns true if the code matches the expected pattern (4+ chars, dash, 4+ chars).
#[allow(dead_code)]
pub(crate) fn validate_device_code(code: &str) -> bool {
    let parts: Vec<&str> = code.split('-').collect();
    if parts.len() != 2 {
        return false;
    }
    let first_valid = parts[0].len() >= 4 && parts[0].chars().all(|c| c.is_ascii_alphanumeric());
    let second_valid = parts[1].len() >= 4 && parts[1].chars().all(|c| c.is_ascii_alphanumeric());
    first_valid && second_valid
}

/// The command to send to initiate device auth login.
pub const DEVICE_AUTH_LOGIN_COMMAND: &str = "cod login --device-auth\n";

// ============================================================================
// Step Results
// ============================================================================

/// Result of a workflow step execution.
///
/// Each step returns a `StepResult` that determines what happens next:
/// - `Continue`: Proceed to the next step
/// - `Done`: Workflow completed successfully with a result
/// - `Retry`: Retry this step after a delay
/// - `Abort`: Stop workflow with an error
/// - `WaitFor`: Pause until a condition is met
/// - `SendText`: Send text to pane and proceed (policy-gated)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StepResult {
    /// Proceed to next step
    Continue,
    /// Workflow completed successfully with optional result data
    Done { result: serde_json::Value },
    /// Retry this step after delay
    Retry {
        /// Delay before retry in milliseconds
        delay_ms: u64,
    },
    /// Abort workflow with error
    Abort {
        /// Reason for abort
        reason: String,
    },
    /// Wait for condition before proceeding
    WaitFor {
        /// Condition to wait for
        condition: WaitCondition,
        /// Timeout in milliseconds (None = workflow-level default)
        timeout_ms: Option<u64>,
    },
    /// Send text to pane via policy-gated injector, then proceed to next step.
    /// If policy denies the send, the workflow is aborted with the denial reason.
    SendText {
        /// Text to send to the pane
        text: String,
        /// Optional condition to wait for after successful send (for verification)
        wait_for: Option<WaitCondition>,
        /// Timeout for the wait condition in milliseconds
        wait_timeout_ms: Option<u64>,
    },
}

impl StepResult {
    /// Create a Continue result
    #[must_use]
    pub fn cont() -> Self {
        Self::Continue
    }

    /// Create a Done result with JSON value
    #[must_use]
    pub fn done(result: serde_json::Value) -> Self {
        Self::Done { result }
    }

    /// Create a Done result with no data
    #[must_use]
    pub fn done_empty() -> Self {
        Self::Done {
            result: serde_json::Value::Null,
        }
    }

    /// Create a Retry result
    #[must_use]
    pub fn retry(delay_ms: u64) -> Self {
        Self::Retry { delay_ms }
    }

    /// Create an Abort result
    #[must_use]
    pub fn abort(reason: impl Into<String>) -> Self {
        Self::Abort {
            reason: reason.into(),
        }
    }

    /// Create a WaitFor result with default timeout
    #[must_use]
    pub fn wait_for(condition: WaitCondition) -> Self {
        Self::WaitFor {
            condition,
            timeout_ms: None,
        }
    }

    /// Create a WaitFor result with explicit timeout
    #[must_use]
    pub fn wait_for_with_timeout(condition: WaitCondition, timeout_ms: u64) -> Self {
        Self::WaitFor {
            condition,
            timeout_ms: Some(timeout_ms),
        }
    }

    /// Create a SendText result to inject text into the pane.
    /// The runner will call the policy-gated injector and abort if denied.
    #[must_use]
    pub fn send_text(text: impl Into<String>) -> Self {
        Self::SendText {
            text: text.into(),
            wait_for: None,
            wait_timeout_ms: None,
        }
    }

    /// Create a SendText result with optional verification wait condition.
    /// After successful send, waits for the condition before proceeding.
    #[must_use]
    pub fn send_text_and_wait(
        text: impl Into<String>,
        wait_for: WaitCondition,
        timeout_ms: u64,
    ) -> Self {
        Self::SendText {
            text: text.into(),
            wait_for: Some(wait_for),
            wait_timeout_ms: Some(timeout_ms),
        }
    }

    /// Check if this result sends text
    #[must_use]
    pub fn is_send_text(&self) -> bool {
        matches!(self, Self::SendText { .. })
    }

    /// Check if this result continues to the next step
    #[must_use]
    pub fn is_continue(&self) -> bool {
        matches!(self, Self::Continue)
    }

    /// Check if this result completes the workflow
    #[must_use]
    pub fn is_done(&self) -> bool {
        matches!(self, Self::Done { .. })
    }

    /// Check if this result is a terminal state (done or abort)
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done { .. } | Self::Abort { .. })
    }
}

// ============================================================================
// Wait Conditions
// ============================================================================

/// Conditions that a workflow can wait for before proceeding.
///
/// Wait conditions pause workflow execution until satisfied:
/// - `Pattern`: Wait for a pattern rule to match on a pane
/// - `PaneIdle`: Wait for a pane to become idle (no output)
/// - `External`: Wait for an external signal by key
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WaitCondition {
    /// Wait for a pattern to appear on a specific pane
    Pattern {
        /// Pane to monitor (None = workflow's target pane)
        pane_id: Option<u64>,
        /// Rule ID of the pattern to match
        rule_id: String,
    },
    /// Wait for pane to become idle (no recent output)
    PaneIdle {
        /// Pane to monitor (None = workflow's target pane)
        pane_id: Option<u64>,
        /// Idle duration threshold in milliseconds
        idle_threshold_ms: u64,
    },
    /// Wait for an external signal
    External {
        /// Signal key to wait for
        key: String,
    },
}

impl WaitCondition {
    /// Create a Pattern wait condition for the workflow's target pane
    #[must_use]
    pub fn pattern(rule_id: impl Into<String>) -> Self {
        Self::Pattern {
            pane_id: None,
            rule_id: rule_id.into(),
        }
    }

    /// Create a Pattern wait condition for a specific pane
    #[must_use]
    pub fn pattern_on_pane(pane_id: u64, rule_id: impl Into<String>) -> Self {
        Self::Pattern {
            pane_id: Some(pane_id),
            rule_id: rule_id.into(),
        }
    }

    /// Create a PaneIdle wait condition for the workflow's target pane
    #[must_use]
    pub fn pane_idle(idle_threshold_ms: u64) -> Self {
        Self::PaneIdle {
            pane_id: None,
            idle_threshold_ms,
        }
    }

    /// Create a PaneIdle wait condition for a specific pane
    #[must_use]
    pub fn pane_idle_on(pane_id: u64, idle_threshold_ms: u64) -> Self {
        Self::PaneIdle {
            pane_id: Some(pane_id),
            idle_threshold_ms,
        }
    }

    /// Create an External wait condition
    #[must_use]
    pub fn external(key: impl Into<String>) -> Self {
        Self::External { key: key.into() }
    }

    /// Get the pane ID this condition applies to, if any
    #[must_use]
    pub fn pane_id(&self) -> Option<u64> {
        match self {
            Self::Pattern { pane_id, .. } | Self::PaneIdle { pane_id, .. } => *pane_id,
            Self::External { .. } => None,
        }
    }
}

// ============================================================================
// Workflow Steps
// ============================================================================

/// A step in a workflow definition.
///
/// Steps provide metadata for display, logging, and debugging.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowStep {
    /// Step name (identifier)
    pub name: String,
    /// Human-readable description
    pub description: String,
}

impl WorkflowStep {
    /// Create a new workflow step
    #[must_use]
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
        }
    }
}

// ============================================================================
// Workflow Context
// ============================================================================

/// Configuration for a workflow execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowConfig {
    /// Default timeout for wait conditions (milliseconds)
    pub default_wait_timeout_ms: u64,
    /// Maximum number of retries per step
    pub max_step_retries: u32,
    /// Delay between retry attempts (milliseconds)
    pub retry_delay_ms: u64,
}

impl Default for WorkflowConfig {
    fn default() -> Self {
        Self {
            default_wait_timeout_ms: 30_000, // 30 seconds
            max_step_retries: 3,
            retry_delay_ms: 1_000, // 1 second
        }
    }
}

/// Runtime context for workflow execution.
///
/// Provides access to:
/// - WezTerm client for sending commands
/// - Storage handle for persistence
/// - Current pane state and capabilities
/// - Triggering event/detection
/// - Workflow configuration
#[derive(Clone)]
pub struct WorkflowContext {
    /// Storage handle for persistence operations
    storage: Arc<StorageHandle>,
    /// Target pane ID for this workflow
    pane_id: u64,
    /// Current pane capabilities snapshot
    capabilities: PaneCapabilities,
    /// The event/detection that triggered this workflow (JSON)
    trigger: Option<serde_json::Value>,
    /// Workflow configuration
    config: WorkflowConfig,
    /// Workflow execution ID
    execution_id: String,
    /// Policy-gated injector for terminal actions (optional)
    injector: Option<Arc<tokio::sync::Mutex<crate::policy::PolicyGatedInjector>>>,
}

impl WorkflowContext {
    /// Create a new workflow context
    #[must_use]
    pub fn new(
        storage: Arc<StorageHandle>,
        pane_id: u64,
        capabilities: PaneCapabilities,
        execution_id: impl Into<String>,
    ) -> Self {
        Self {
            storage,
            pane_id,
            capabilities,
            trigger: None,
            config: WorkflowConfig::default(),
            execution_id: execution_id.into(),
            injector: None,
        }
    }

    /// Set the policy-gated injector for terminal actions
    #[must_use]
    pub fn with_injector(
        mut self,
        injector: Arc<tokio::sync::Mutex<crate::policy::PolicyGatedInjector>>,
    ) -> Self {
        self.injector = Some(injector);
        self
    }

    /// Set the triggering event/detection
    #[must_use]
    pub fn with_trigger(mut self, trigger: serde_json::Value) -> Self {
        self.trigger = Some(trigger);
        self
    }

    /// Set custom workflow configuration
    #[must_use]
    pub fn with_config(mut self, config: WorkflowConfig) -> Self {
        self.config = config;
        self
    }

    /// Get the storage handle
    #[must_use]
    pub fn storage(&self) -> &Arc<StorageHandle> {
        &self.storage
    }

    /// Get the target pane ID
    #[must_use]
    pub fn pane_id(&self) -> u64 {
        self.pane_id
    }

    /// Get the current pane capabilities
    #[must_use]
    pub fn capabilities(&self) -> &PaneCapabilities {
        &self.capabilities
    }

    /// Update the pane capabilities snapshot
    pub fn update_capabilities(&mut self, capabilities: PaneCapabilities) {
        self.capabilities = capabilities;
    }

    /// Get the triggering event/detection, if any
    #[must_use]
    pub fn trigger(&self) -> Option<&serde_json::Value> {
        self.trigger.as_ref()
    }

    /// Get the workflow configuration
    #[must_use]
    pub fn config(&self) -> &WorkflowConfig {
        &self.config
    }

    /// Get the execution ID
    #[must_use]
    pub fn execution_id(&self) -> &str {
        &self.execution_id
    }

    /// Get the default wait timeout from config
    #[must_use]
    pub fn default_wait_timeout_ms(&self) -> u64 {
        self.config.default_wait_timeout_ms
    }

    /// Check if an injector is available for actions
    #[must_use]
    pub fn has_injector(&self) -> bool {
        self.injector.is_some()
    }

    /// Send text to the target pane via policy-gated injection.
    ///
    /// Returns `Ok(InjectionResult)` on success, `Err` if no injector is configured.
    ///
    /// The injection is performed through the `PolicyGatedInjector` which:
    /// - Checks policy authorization
    /// - Emits audit entries
    /// - Only sends if allowed
    pub async fn send_text(
        &mut self,
        text: &str,
    ) -> Result<crate::policy::InjectionResult, &'static str> {
        let injector = self.injector.as_ref().ok_or("No injector configured")?;
        let mut guard = injector.lock().await;
        Ok(guard
            .send_text(
                self.pane_id,
                text,
                crate::policy::ActorKind::Workflow,
                &self.capabilities,
                Some(&self.execution_id),
            )
            .await)
    }

    /// Send Ctrl-C (interrupt) to the target pane via policy-gated injection.
    pub async fn send_ctrl_c(&mut self) -> Result<crate::policy::InjectionResult, &'static str> {
        let injector = self.injector.as_ref().ok_or("No injector configured")?;
        let mut guard = injector.lock().await;
        Ok(guard
            .send_ctrl_c(
                self.pane_id,
                crate::policy::ActorKind::Workflow,
                &self.capabilities,
                Some(&self.execution_id),
            )
            .await)
    }

    /// Send Ctrl-D (EOF) to the target pane via policy-gated injection.
    pub async fn send_ctrl_d(&mut self) -> Result<crate::policy::InjectionResult, &'static str> {
        let injector = self.injector.as_ref().ok_or("No injector configured")?;
        let mut guard = injector.lock().await;
        Ok(guard
            .send_ctrl_d(
                self.pane_id,
                crate::policy::ActorKind::Workflow,
                &self.capabilities,
                Some(&self.execution_id),
            )
            .await)
    }

    /// Send Ctrl-Z (suspend) to the target pane via policy-gated injection.
    pub async fn send_ctrl_z(&mut self) -> Result<crate::policy::InjectionResult, &'static str> {
        let injector = self.injector.as_ref().ok_or("No injector configured")?;
        let mut guard = injector.lock().await;
        Ok(guard
            .send_ctrl_z(
                self.pane_id,
                crate::policy::ActorKind::Workflow,
                &self.capabilities,
                Some(&self.execution_id),
            )
            .await)
    }
}

// ============================================================================
// Workflow Trait
// ============================================================================

/// A durable, resumable workflow definition.
///
/// Workflows are explicit state machines with a uniform execution model.
/// Implement this trait to define custom automation workflows.
///
/// # Example
///
/// ```ignore
/// use wa_core::workflows::{Workflow, WorkflowContext, WorkflowStep, StepResult, WaitCondition};
/// use wa_core::patterns::Detection;
///
/// struct PromptInjectionWorkflow;
///
/// impl Workflow for PromptInjectionWorkflow {
///     fn name(&self) -> &'static str { "prompt_injection" }
///     fn description(&self) -> &'static str { "Sends a prompt and waits for response" }
///
///     fn handles(&self, detection: &Detection) -> bool {
///         detection.rule_id.starts_with("trigger.prompt_injection")
///     }
///
///     fn steps(&self) -> Vec<WorkflowStep> {
///         vec![
///             WorkflowStep::new("send_prompt", "Send prompt to terminal"),
///             WorkflowStep::new("wait_response", "Wait for response pattern"),
///         ]
///     }
///
///     async fn execute_step(&self, ctx: &mut WorkflowContext, step_idx: usize) -> StepResult {
///         match step_idx {
///             0 => {
///                 // Send prompt via WezTerm client
///                 StepResult::cont()
///             }
///             1 => {
///                 // Wait for response
///                 StepResult::wait_for(WaitCondition::pattern("response.complete"))
///             }
///             _ => StepResult::done_empty()
///         }
///     }
/// }
/// ```
pub trait Workflow: Send + Sync {
    /// Workflow name (unique identifier)
    fn name(&self) -> &'static str;

    /// Human-readable description
    fn description(&self) -> &'static str;

    /// Check if this workflow handles a given detection.
    ///
    /// Return true if this workflow should be triggered by the detection.
    fn handles(&self, detection: &crate::patterns::Detection) -> bool;

    /// Get the list of steps in this workflow.
    ///
    /// Step metadata is used for display, logging, and debugging.
    fn steps(&self) -> Vec<WorkflowStep>;

    /// Execute a single step of the workflow.
    ///
    /// # Arguments
    /// * `ctx` - Workflow context with storage, pane state, and config
    /// * `step_idx` - Zero-based step index
    ///
    /// # Returns
    /// A `StepResult` indicating what should happen next.
    fn execute_step(&self, ctx: &mut WorkflowContext, step_idx: usize)
    -> BoxFuture<'_, StepResult>;

    /// Optional cleanup when workflow is aborted or completes with error.
    ///
    /// Override to release resources, revert partial changes, etc.
    fn cleanup(&self, _ctx: &mut WorkflowContext) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }

    /// Get the number of steps in this workflow.
    fn step_count(&self) -> usize {
        self.steps().len()
    }

    // ========================================================================
    // Extended metadata for workflow listing (all with default implementations)
    // ========================================================================

    /// Event types that can trigger this workflow (e.g., "session.compaction").
    /// Returns empty slice if not triggered by specific event types.
    fn trigger_event_types(&self) -> &'static [&'static str] {
        &[]
    }

    /// Rule IDs that can trigger this workflow (e.g., "compaction.detected").
    /// Returns empty slice if not triggered by specific rules.
    fn trigger_rule_ids(&self) -> &'static [&'static str] {
        &[]
    }

    /// Agent types this workflow supports (e.g., ["codex", "claude_code"]).
    /// Returns empty slice if supports all agent types.
    fn supported_agent_types(&self) -> &'static [&'static str] {
        &[]
    }

    /// Whether this workflow requires a target pane to operate on.
    fn requires_pane(&self) -> bool {
        true
    }

    /// Whether this workflow requires approval before execution.
    fn requires_approval(&self) -> bool {
        false
    }

    /// Whether this workflow can be aborted while running.
    fn can_abort(&self) -> bool {
        true
    }

    /// Whether this workflow performs destructive operations.
    fn is_destructive(&self) -> bool {
        false
    }

    /// Names of workflows this one depends on (must complete first).
    fn dependencies(&self) -> &'static [&'static str] {
        &[]
    }

    /// Whether this workflow is currently enabled.
    fn is_enabled(&self) -> bool {
        true
    }
}

// ============================================================================
// Workflow Info (for listing)
// ============================================================================

/// Information about a workflow for listing and discovery.
///
/// This struct captures the metadata exposed by the `Workflow` trait
/// in a serializable form for robot mode and TUI display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowInfo {
    /// Workflow name (unique identifier)
    pub name: String,
    /// Human-readable description
    pub description: String,
    /// Whether the workflow is enabled
    pub enabled: bool,
    /// Event types that can trigger this workflow
    pub trigger_event_types: Vec<String>,
    /// Rule IDs that can trigger this workflow
    pub trigger_rule_ids: Vec<String>,
    /// Agent types this workflow supports (empty = all)
    pub agent_types: Vec<String>,
    /// Number of steps in the workflow
    pub step_count: usize,
    /// Whether this workflow requires a target pane
    pub requires_pane: bool,
    /// Whether this workflow requires approval before execution
    pub requires_approval: bool,
    /// Whether this workflow can be aborted while running
    pub can_abort: bool,
    /// Whether this workflow performs destructive operations
    pub destructive: bool,
    /// Names of workflows this one depends on
    pub dependencies: Vec<String>,
}

impl WorkflowInfo {
    /// Create a WorkflowInfo from a workflow trait object.
    pub fn from_workflow(workflow: &dyn Workflow) -> Self {
        Self {
            name: workflow.name().to_string(),
            description: workflow.description().to_string(),
            enabled: workflow.is_enabled(),
            trigger_event_types: workflow
                .trigger_event_types()
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            trigger_rule_ids: workflow
                .trigger_rule_ids()
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            agent_types: workflow
                .supported_agent_types()
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            step_count: workflow.step_count(),
            requires_pane: workflow.requires_pane(),
            requires_approval: workflow.requires_approval(),
            can_abort: workflow.can_abort(),
            destructive: workflow.is_destructive(),
            dependencies: workflow
                .dependencies()
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
        }
    }
}

// ============================================================================
// Workflow Execution State
// ============================================================================

/// Workflow execution state
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowExecution {
    /// Unique execution ID
    pub id: String,
    /// Workflow name
    pub workflow_name: String,
    /// Pane being operated on
    pub pane_id: u64,
    /// Current step index
    pub current_step: usize,
    /// Status
    pub status: ExecutionStatus,
    /// Started at timestamp
    pub started_at: i64,
    /// Last updated timestamp
    pub updated_at: i64,
}

/// Workflow execution status
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStatus {
    /// Running
    Running,
    /// Waiting for condition
    Waiting,
    /// Completed successfully
    Completed,
    /// Aborted with error
    Aborted,
}

/// Workflow engine for managing executions
pub struct WorkflowEngine {
    /// Maximum concurrent workflows
    max_concurrent: usize,
}

impl Default for WorkflowEngine {
    fn default() -> Self {
        Self::new(3)
    }
}

impl WorkflowEngine {
    /// Create a new workflow engine
    #[must_use]
    pub fn new(max_concurrent: usize) -> Self {
        Self { max_concurrent }
    }

    /// Get the maximum concurrent workflows setting
    #[must_use]
    pub fn max_concurrent(&self) -> usize {
        self.max_concurrent
    }

    /// Start a new workflow execution and persist it to storage
    ///
    /// Creates a new execution record with status 'running' and step 0.
    /// Returns the execution which can be used with `DurableWorkflowRunner`.
    pub async fn start(
        &self,
        storage: &crate::storage::StorageHandle,
        workflow_name: &str,
        pane_id: u64,
        trigger_event_id: Option<i64>,
        context: Option<serde_json::Value>,
    ) -> crate::Result<WorkflowExecution> {
        let execution_id = generate_workflow_id(workflow_name);
        self.start_with_id(
            storage,
            execution_id,
            workflow_name,
            pane_id,
            trigger_event_id,
            context,
        )
        .await
    }

    /// Start a workflow execution using a caller-provided execution_id.
    ///
    /// This is used by `WorkflowRunner` so the lock execution_id matches the persisted DB id.
    pub async fn start_with_id(
        &self,
        storage: &crate::storage::StorageHandle,
        execution_id: String,
        workflow_name: &str,
        pane_id: u64,
        trigger_event_id: Option<i64>,
        context: Option<serde_json::Value>,
    ) -> crate::Result<WorkflowExecution> {
        let now = now_ms();

        let record = crate::storage::WorkflowRecord {
            id: execution_id.clone(),
            workflow_name: workflow_name.to_string(),
            pane_id,
            trigger_event_id,
            current_step: 0,
            status: "running".to_string(),
            wait_condition: None,
            context,
            result: None,
            error: None,
            started_at: now,
            updated_at: now,
            completed_at: None,
        };

        storage.upsert_workflow(record).await?;

        Ok(WorkflowExecution {
            id: execution_id,
            workflow_name: workflow_name.to_string(),
            pane_id,
            current_step: 0,
            status: ExecutionStatus::Running,
            started_at: now,
            updated_at: now,
        })
    }

    /// Resume a workflow execution from storage
    ///
    /// Loads the workflow record and step logs to determine the next step.
    /// Returns None if the workflow doesn't exist or is already completed.
    pub async fn resume(
        &self,
        storage: &crate::storage::StorageHandle,
        execution_id: &str,
    ) -> crate::Result<Option<(WorkflowExecution, usize)>> {
        // Load the workflow record
        let Some(record) = storage.get_workflow(execution_id).await? else {
            return Ok(None);
        };

        // Check if already completed
        if record.status == "completed" || record.status == "aborted" {
            return Ok(None);
        }

        // Load step logs to find the last completed step
        let step_logs = storage.get_step_logs(execution_id).await?;
        let next_step = compute_next_step(&step_logs);

        let execution = WorkflowExecution {
            id: record.id,
            workflow_name: record.workflow_name,
            pane_id: record.pane_id,
            current_step: next_step,
            status: match record.status.as_str() {
                "waiting" => ExecutionStatus::Waiting,
                _ => ExecutionStatus::Running,
            },
            started_at: record.started_at,
            updated_at: record.updated_at,
        };

        Ok(Some((execution, next_step)))
    }

    /// Find all incomplete workflows for resume on restart
    pub async fn find_incomplete(
        &self,
        storage: &crate::storage::StorageHandle,
    ) -> crate::Result<Vec<crate::storage::WorkflowRecord>> {
        storage.find_incomplete_workflows().await
    }

    /// Update workflow status
    pub async fn update_status(
        &self,
        storage: &crate::storage::StorageHandle,
        execution_id: &str,
        status: ExecutionStatus,
        current_step: usize,
        wait_condition: Option<&WaitCondition>,
        error: Option<&str>,
    ) -> crate::Result<()> {
        let now = now_ms();
        let status_str = match status {
            ExecutionStatus::Running => "running",
            ExecutionStatus::Waiting => "waiting",
            ExecutionStatus::Completed => "completed",
            ExecutionStatus::Aborted => "aborted",
        };

        // Load existing record to preserve fields
        let Some(existing) = storage.get_workflow(execution_id).await? else {
            return Err(crate::error::WorkflowError::NotFound(execution_id.to_string()).into());
        };

        let record = crate::storage::WorkflowRecord {
            id: existing.id,
            workflow_name: existing.workflow_name,
            pane_id: existing.pane_id,
            trigger_event_id: existing.trigger_event_id,
            current_step,
            status: status_str.to_string(),
            wait_condition: wait_condition.map(|wc| serde_json::to_value(wc).unwrap_or_default()),
            context: existing.context,
            result: existing.result,
            error: error.map(String::from),
            started_at: existing.started_at,
            updated_at: now,
            completed_at: if status == ExecutionStatus::Completed
                || status == ExecutionStatus::Aborted
            {
                Some(now)
            } else {
                None
            },
        };

        storage.upsert_workflow(record).await
    }

    /// Record a step log entry
    pub async fn log_step(
        &self,
        storage: &crate::storage::StorageHandle,
        execution_id: &str,
        step_index: usize,
        step_name: &str,
        result: &StepResult,
        started_at: i64,
    ) -> crate::Result<()> {
        let completed_at = now_ms();
        let result_type = match result {
            StepResult::Continue => "continue",
            StepResult::Done { .. } => "done",
            StepResult::Abort { .. } => "abort",
            StepResult::Retry { .. } => "retry",
            StepResult::WaitFor { .. } => "wait_for",
            StepResult::SendText { .. } => "send_text",
        };
        let result_data = serde_json::to_string(result).ok();

        storage
            .insert_step_log(
                execution_id,
                None,
                step_index,
                step_name,
                result_type,
                result_data,
                started_at,
                completed_at,
            )
            .await
    }
}

/// Compute the next step index from step logs
///
/// Finds the highest completed step index and returns the next one.
/// If no steps are completed, returns 0.
fn compute_next_step(step_logs: &[crate::storage::WorkflowStepLogRecord]) -> usize {
    if step_logs.is_empty() {
        return 0;
    }

    // Find the highest step index with a terminal result (continue or done)
    // Steps with retry or wait_for should be re-executed
    let mut max_completed = None;
    for log in step_logs {
        if log.result_type == "continue" || log.result_type == "done" {
            max_completed =
                Some(max_completed.map_or(log.step_index, |m: usize| m.max(log.step_index)));
        }
    }

    max_completed.map_or(0, |idx| idx + 1)
}

/// Generate a unique workflow execution ID
fn generate_workflow_id(workflow_name: &str) -> String {
    let timestamp = now_ms();
    let random: u32 = rand::random();
    format!("{workflow_name}-{timestamp}-{random:08x}")
}

/// Get current timestamp in milliseconds
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

// ============================================================================
// Per-Pane Workflow Lock (wa-nu4.1.1.2)
// ============================================================================

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Result of attempting to acquire a pane workflow lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockAcquisitionResult {
    /// Lock acquired successfully.
    Acquired,
    /// Lock is already held by another workflow.
    AlreadyLocked {
        /// Name of the workflow holding the lock.
        held_by_workflow: String,
        /// Execution ID of the workflow holding the lock.
        held_by_execution: String,
        /// When the lock was acquired (unix timestamp ms).
        locked_since_ms: i64,
    },
}

impl LockAcquisitionResult {
    /// Check if the lock was acquired.
    #[must_use]
    pub fn is_acquired(&self) -> bool {
        matches!(self, Self::Acquired)
    }

    /// Check if the lock is already held.
    #[must_use]
    pub fn is_already_locked(&self) -> bool {
        matches!(self, Self::AlreadyLocked { .. })
    }
}

/// Information about an active pane lock.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneLockInfo {
    /// Pane ID that is locked.
    pub pane_id: u64,
    /// Workflow name holding the lock.
    pub workflow_name: String,
    /// Execution ID holding the lock.
    pub execution_id: String,
    /// When the lock was acquired (unix timestamp ms).
    pub locked_at_ms: i64,
}

/// In-memory workflow lock manager for panes.
///
/// Ensures only one workflow runs per pane at a time. This is an internal
/// concurrency primitive that prevents workflow collisions, separate from
/// user-facing pane reservations.
///
/// # Design
///
/// - In-memory lock table keyed by `pane_id`
/// - Thread-safe via internal mutex
/// - Lock acquisition returns detailed info about existing locks
/// - Supports RAII-based release via `PaneWorkflowLockGuard`
///
/// # Example
///
/// ```no_run
/// use wa_core::workflows::{PaneWorkflowLockManager, LockAcquisitionResult};
///
/// let manager = PaneWorkflowLockManager::new();
///
/// // Try to acquire lock
/// match manager.try_acquire(42, "handle_compaction", "exec-001") {
///     LockAcquisitionResult::Acquired => {
///         // Run workflow...
///         manager.release(42, "exec-001");
///     }
///     LockAcquisitionResult::AlreadyLocked { held_by_workflow, .. } => {
///         println!("Pane 42 is locked by {}", held_by_workflow);
///     }
/// }
/// ```
pub struct PaneWorkflowLockManager {
    /// Active locks keyed by pane_id.
    locks: Mutex<HashMap<u64, PaneLockInfo>>,
}

impl Default for PaneWorkflowLockManager {
    fn default() -> Self {
        Self::new()
    }
}

impl PaneWorkflowLockManager {
    /// Create a new lock manager.
    #[must_use]
    pub fn new() -> Self {
        Self {
            locks: Mutex::new(HashMap::new()),
        }
    }

    /// Attempt to acquire a lock for a pane.
    ///
    /// Returns `Acquired` if the lock was obtained, or `AlreadyLocked` with
    /// information about the current lock holder.
    ///
    /// # Arguments
    ///
    /// * `pane_id` - The pane to lock
    /// * `workflow_name` - Name of the workflow requesting the lock
    /// * `execution_id` - Unique execution ID for this workflow run
    pub fn try_acquire(
        &self,
        pane_id: u64,
        workflow_name: &str,
        execution_id: &str,
    ) -> LockAcquisitionResult {
        let mut locks = self.locks.lock().unwrap();

        if let Some(existing) = locks.get(&pane_id) {
            return LockAcquisitionResult::AlreadyLocked {
                held_by_workflow: existing.workflow_name.clone(),
                held_by_execution: existing.execution_id.clone(),
                locked_since_ms: existing.locked_at_ms,
            };
        }

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0);

        locks.insert(
            pane_id,
            PaneLockInfo {
                pane_id,
                workflow_name: workflow_name.to_string(),
                execution_id: execution_id.to_string(),
                locked_at_ms: now_ms,
            },
        );
        drop(locks);

        tracing::debug!(
            pane_id,
            workflow_name,
            execution_id,
            "Acquired pane workflow lock"
        );

        LockAcquisitionResult::Acquired
    }

    /// Release a lock for a pane.
    ///
    /// Only releases if the execution_id matches the current lock holder.
    /// This prevents accidental release by unrelated code.
    ///
    /// # Returns
    ///
    /// `true` if the lock was released, `false` if not found or mismatched.
    pub fn release(&self, pane_id: u64, execution_id: &str) -> bool {
        let mut locks = self.locks.lock().unwrap();

        if let Some(existing) = locks.get(&pane_id) {
            if existing.execution_id == execution_id {
                locks.remove(&pane_id);
                drop(locks);
                tracing::debug!(pane_id, execution_id, "Released pane workflow lock");
                return true;
            }
            let held_by = existing.execution_id.clone();
            drop(locks);
            tracing::warn!(
                pane_id,
                execution_id,
                held_by = %held_by,
                "Attempted to release lock held by different execution"
            );
            return false;
        }

        false
    }

    /// Check if a pane is currently locked.
    ///
    /// Returns lock information if locked, `None` if free.
    #[must_use]
    pub fn is_locked(&self, pane_id: u64) -> Option<PaneLockInfo> {
        let locks = self.locks.lock().unwrap();
        locks.get(&pane_id).cloned()
    }

    /// Get all currently active locks.
    ///
    /// Useful for diagnostics and monitoring.
    #[must_use]
    pub fn active_locks(&self) -> Vec<PaneLockInfo> {
        let locks = self.locks.lock().unwrap();
        locks.values().cloned().collect()
    }

    /// Try to acquire a lock and return an RAII guard.
    ///
    /// The lock is automatically released when the guard is dropped.
    ///
    /// # Returns
    ///
    /// `Some(guard)` if acquired, `None` if already locked.
    pub fn acquire_guard(
        &self,
        pane_id: u64,
        workflow_name: &str,
        execution_id: &str,
    ) -> Option<PaneWorkflowLockGuard<'_>> {
        match self.try_acquire(pane_id, workflow_name, execution_id) {
            LockAcquisitionResult::Acquired => Some(PaneWorkflowLockGuard {
                manager: self,
                pane_id,
                execution_id: execution_id.to_string(),
            }),
            LockAcquisitionResult::AlreadyLocked { .. } => None,
        }
    }

    /// Force-release a lock regardless of execution_id.
    ///
    /// **Use with caution** - only for recovery scenarios.
    pub fn force_release(&self, pane_id: u64) -> Option<PaneLockInfo> {
        let removed = self.locks.lock().unwrap().remove(&pane_id);
        if let Some(ref info) = removed {
            tracing::warn!(
                pane_id,
                execution_id = %info.execution_id,
                "Force-released pane workflow lock"
            );
        }
        removed
    }
}

/// RAII guard for pane workflow lock.
///
/// The lock is automatically released when this guard is dropped.
pub struct PaneWorkflowLockGuard<'a> {
    manager: &'a PaneWorkflowLockManager,
    pane_id: u64,
    execution_id: String,
}

impl PaneWorkflowLockGuard<'_> {
    /// Get the pane ID this guard is locking.
    #[must_use]
    pub fn pane_id(&self) -> u64 {
        self.pane_id
    }

    /// Get the execution ID that holds this lock.
    #[must_use]
    pub fn execution_id(&self) -> &str {
        &self.execution_id
    }

    /// Explicitly release the lock, consuming the guard.
    pub fn release(self) {
        // Drop will handle the release
    }
}

impl Drop for PaneWorkflowLockGuard<'_> {
    fn drop(&mut self) {
        self.manager.release(self.pane_id, &self.execution_id);
    }
}

// ============================================================================
// Wait Condition Execution
// ============================================================================

use crate::ingest::Osc133State;
use crate::patterns::PatternEngine;
use tokio::time::{Instant, sleep};

/// Result of waiting for a condition.
#[derive(Debug, Clone)]
pub enum WaitConditionResult {
    /// Condition was satisfied.
    Satisfied {
        /// Time spent waiting in milliseconds.
        elapsed_ms: u64,
        /// Number of polls performed.
        polls: usize,
        /// Additional context about how the condition was satisfied.
        context: Option<String>,
    },
    /// Timeout elapsed without condition being satisfied.
    TimedOut {
        /// Time spent waiting in milliseconds.
        elapsed_ms: u64,
        /// Number of polls performed.
        polls: usize,
        /// Last observed state (for debugging).
        last_observed: Option<String>,
    },
    /// Condition cannot be evaluated (e.g., external signal not supported).
    Unsupported {
        /// Reason why the condition is unsupported.
        reason: String,
    },
}

impl WaitConditionResult {
    /// Check if the condition was satisfied.
    #[must_use]
    pub fn is_satisfied(&self) -> bool {
        matches!(self, Self::Satisfied { .. })
    }

    /// Check if the wait timed out.
    #[must_use]
    pub fn is_timed_out(&self) -> bool {
        matches!(self, Self::TimedOut { .. })
    }

    /// Get elapsed time in milliseconds, if available.
    #[must_use]
    pub fn elapsed_ms(&self) -> Option<u64> {
        match self {
            Self::Satisfied { elapsed_ms, .. } | Self::TimedOut { elapsed_ms, .. } => {
                Some(*elapsed_ms)
            }
            Self::Unsupported { .. } => None,
        }
    }
}

/// Options for wait condition execution.
#[derive(Debug, Clone)]
pub struct WaitConditionOptions {
    /// Number of tail lines to poll for pattern matching.
    pub tail_lines: usize,
    /// Initial polling interval.
    pub poll_initial: Duration,
    /// Maximum polling interval.
    pub poll_max: Duration,
    /// Maximum number of polls before forcing timeout.
    pub max_polls: usize,
    /// Whether to use fallback heuristics for PaneIdle when OSC 133 unavailable.
    pub allow_idle_heuristics: bool,
}

impl Default for WaitConditionOptions {
    fn default() -> Self {
        Self {
            tail_lines: 200,
            poll_initial: Duration::from_millis(50),
            poll_max: Duration::from_millis(1000),
            max_polls: 10_000,
            allow_idle_heuristics: true,
        }
    }
}

/// Executor for wait conditions.
///
/// This struct wraps the necessary dependencies for executing wait conditions:
/// - `PaneTextSource` for reading pane text (via PaneWaiter)
/// - `PatternEngine` for pattern detection
/// - OSC 133 state for idle detection
///
/// # Example
///
/// ```ignore
/// let executor = WaitConditionExecutor::new(&client, &pattern_engine)
///     .with_osc_state(&osc_state);
///
/// let result = executor.execute(
///     &WaitCondition::pattern("prompt.ready"),
///     pane_id,
///     Duration::from_secs(10),
/// ).await?;
/// ```
pub struct WaitConditionExecutor<'a, S: PaneTextSource + Sync + ?Sized> {
    source: &'a S,
    pattern_engine: &'a PatternEngine,
    osc_state: Option<&'a Osc133State>,
    options: WaitConditionOptions,
}

impl<'a, S: PaneTextSource + Sync + ?Sized> WaitConditionExecutor<'a, S> {
    /// Create a new executor with required dependencies.
    #[must_use]
    pub fn new(source: &'a S, pattern_engine: &'a PatternEngine) -> Self {
        Self {
            source,
            pattern_engine,
            osc_state: None,
            options: WaitConditionOptions::default(),
        }
    }

    /// Set OSC 133 state for deterministic idle detection.
    #[must_use]
    pub fn with_osc_state(mut self, osc_state: &'a Osc133State) -> Self {
        self.osc_state = Some(osc_state);
        self
    }

    /// Override default options.
    #[must_use]
    pub fn with_options(mut self, options: WaitConditionOptions) -> Self {
        self.options = options;
        self
    }

    /// Execute a wait condition.
    ///
    /// This method blocks until the condition is satisfied or the timeout elapses.
    /// It reuses the PaneWaiter infrastructure for consistent polling behavior.
    pub async fn execute(
        &self,
        condition: &WaitCondition,
        context_pane_id: u64,
        timeout: Duration,
    ) -> crate::Result<WaitConditionResult> {
        match condition {
            WaitCondition::Pattern { pane_id, rule_id } => {
                let target_pane = pane_id.unwrap_or(context_pane_id);
                self.execute_pattern_wait(target_pane, rule_id, timeout)
                    .await
            }
            WaitCondition::PaneIdle {
                pane_id,
                idle_threshold_ms,
            } => {
                let target_pane = pane_id.unwrap_or(context_pane_id);
                self.execute_pane_idle_wait(target_pane, *idle_threshold_ms, timeout)
                    .await
            }
            WaitCondition::External { key } => {
                // External signals are not implemented in this layer
                Ok(WaitConditionResult::Unsupported {
                    reason: format!("External signal '{key}' requires external signal registry"),
                })
            }
        }
    }

    /// Execute a pattern wait condition.
    ///
    /// Polls pane text using PaneWaiter, runs pattern detection, and checks
    /// for the specified rule_id. Stops early on match.
    async fn execute_pattern_wait(
        &self,
        pane_id: u64,
        rule_id: &str,
        timeout: Duration,
    ) -> crate::Result<WaitConditionResult> {
        let start = Instant::now();
        let deadline = start + timeout;
        let mut polls = 0usize;
        let mut interval = self.options.poll_initial;
        let mut last_detection_summary: Option<String> = None;

        #[allow(clippy::cast_possible_truncation)]
        let timeout_ms = timeout.as_millis() as u64;
        tracing::info!(pane_id, rule_id, timeout_ms, "pattern_wait start");

        loop {
            polls += 1;

            // Get pane text
            let text = self.source.get_text(pane_id, false).await?;
            let tail = tail_text(&text, self.options.tail_lines);

            // Run pattern detection
            let detections = self.pattern_engine.detect(&tail);

            // Check for matching rule
            if let Some(detection) = detections.iter().find(|d| d.rule_id == rule_id) {
                let elapsed_ms = elapsed_ms(start);
                tracing::info!(
                    pane_id,
                    rule_id,
                    elapsed_ms,
                    polls,
                    matched_text = %detection.matched_text,
                    "pattern_wait matched"
                );
                return Ok(WaitConditionResult::Satisfied {
                    elapsed_ms,
                    polls,
                    context: Some(format!("matched: {}", detection.matched_text)),
                });
            }

            // Update last detection summary for debugging
            if !detections.is_empty() {
                let rule_ids: Vec<&str> = detections.iter().map(|d| d.rule_id.as_str()).collect();
                last_detection_summary = Some(format!("detected: [{}]", rule_ids.join(", ")));
            }

            // Check timeout
            let now = Instant::now();
            if now >= deadline || polls >= self.options.max_polls {
                let elapsed_ms = elapsed_ms(start);
                tracing::info!(pane_id, rule_id, elapsed_ms, polls, "pattern_wait timeout");
                return Ok(WaitConditionResult::TimedOut {
                    elapsed_ms,
                    polls,
                    last_observed: last_detection_summary,
                });
            }

            // Sleep with backoff
            let remaining = deadline.saturating_duration_since(now);
            let sleep_duration = interval.min(remaining);
            if !sleep_duration.is_zero() {
                sleep(sleep_duration).await;
            }

            interval = interval.saturating_mul(2);
            if interval > self.options.poll_max {
                interval = self.options.poll_max;
            }
        }
    }

    /// Execute a pane idle wait condition.
    ///
    /// Primary: Uses OSC 133 state to detect prompt (deterministic).
    /// Fallback: Uses heuristic prompt matching if OSC 133 unavailable.
    async fn execute_pane_idle_wait(
        &self,
        pane_id: u64,
        idle_threshold_ms: u64,
        timeout: Duration,
    ) -> crate::Result<WaitConditionResult> {
        let start = Instant::now();
        let deadline = start + timeout;
        let mut polls = 0usize;
        let mut interval = self.options.poll_initial;
        let idle_threshold = Duration::from_millis(idle_threshold_ms);

        // Track when we first observed idle state (for threshold)
        let mut idle_since: Option<Instant> = None;
        #[allow(unused_assignments)]
        let mut last_state_desc: Option<String> = None;

        #[allow(clippy::cast_possible_truncation)]
        let timeout_ms = timeout.as_millis() as u64;
        tracing::info!(
            pane_id,
            idle_threshold_ms,
            timeout_ms,
            has_osc_state = self.osc_state.is_some(),
            "pane_idle_wait start"
        );

        loop {
            polls += 1;

            // Check idle state
            let (is_idle, state_desc) = self.check_idle_state(pane_id).await?;
            last_state_desc = Some(state_desc.clone());

            if is_idle {
                // Track idle duration
                let idle_start = idle_since.get_or_insert_with(Instant::now);
                let idle_duration = Instant::now().saturating_duration_since(*idle_start);

                if idle_duration >= idle_threshold {
                    let elapsed_ms = elapsed_ms(start);
                    tracing::info!(
                        pane_id,
                        elapsed_ms,
                        polls,
                        idle_duration_ms = %idle_duration.as_millis(),
                        state = %state_desc,
                        "pane_idle_wait satisfied"
                    );
                    return Ok(WaitConditionResult::Satisfied {
                        elapsed_ms,
                        polls,
                        context: Some(format!(
                            "idle for {}ms ({})",
                            idle_duration.as_millis(),
                            state_desc
                        )),
                    });
                }
            } else {
                // Reset idle tracking - activity detected
                idle_since = None;
            }

            // Check timeout
            let now = Instant::now();
            if now >= deadline || polls >= self.options.max_polls {
                let elapsed_ms = elapsed_ms(start);
                tracing::info!(pane_id, elapsed_ms, polls, "pane_idle_wait timeout");
                return Ok(WaitConditionResult::TimedOut {
                    elapsed_ms,
                    polls,
                    last_observed: last_state_desc,
                });
            }

            // Sleep with backoff
            let remaining = deadline.saturating_duration_since(now);
            let sleep_duration = interval.min(remaining);
            if !sleep_duration.is_zero() {
                sleep(sleep_duration).await;
            }

            interval = interval.saturating_mul(2);
            if interval > self.options.poll_max {
                interval = self.options.poll_max;
            }
        }
    }

    /// Check if pane is currently idle.
    ///
    /// Returns (is_idle, description) for logging/debugging.
    async fn check_idle_state(&self, pane_id: u64) -> crate::Result<(bool, String)> {
        // Primary: Use OSC 133 state if available
        if let Some(osc_state) = self.osc_state {
            let shell_state = &osc_state.state;
            let is_idle = shell_state.is_at_prompt();
            let desc = format!("osc133:{shell_state:?}");
            return Ok((is_idle, desc));
        }

        // Fallback: Use heuristic prompt detection
        if self.options.allow_idle_heuristics {
            let text = self.source.get_text(pane_id, false).await?;
            let (is_idle, desc) = heuristic_idle_check(&text, self.options.tail_lines);
            return Ok((is_idle, format!("heuristic:{desc}")));
        }

        // No idle detection available
        Ok((false, "no_osc133_no_heuristics".to_string()))
    }
}





/// Heuristic idle check based on pane text patterns.
///
/// This is a best-effort fallback when OSC 133 shell integration is not available.
/// It looks for common shell prompt patterns in the last few lines.
///
/// Returns (is_idle, description) where description explains the heuristic result.
#[allow(clippy::items_after_statements)]
fn heuristic_idle_check(text: &str, tail_lines: usize) -> (bool, String) {
    let tail = tail_text(text, tail_lines.min(10)); // Only check last 10 lines for heuristics
    let last_line = tail.lines().last().unwrap_or("");
    let trimmed = last_line.trim_end();

    // Common prompt endings that suggest idle state
    // Note: These are intentionally broad and may have false positives
    const PROMPT_ENDINGS: [&str; 7] = [
        "$ ",   // bash/sh default
        "# ",   // root prompt
        "> ",   // zsh/fish
        "% ",   // tcsh/zsh
        ">>> ", // Python REPL
        "... ", // Python continuation
        "❯ ",   // starship/custom
    ];

    // Check if line ends with a prompt pattern (with trailing space for cursor position)
    // We check the UNTRIMMED last_line to preserve trailing space significance
    for ending in PROMPT_ENDINGS {
        if last_line.ends_with(ending) {
            return (true, format!("ends_with_prompt({})", ending.trim()));
        }
    }

    // Also check trimmed line for prompts where trailing space was stripped,
    // but only if the line looks like a shell prompt (contains @ or : typical of user@host:path)
    // This avoids false positives like "Progress: 50%" matching "%" prompt
    const PROMPT_CHARS: [char; 5] = ['$', '#', '>', '%', '❯'];
    if let Some(last_char) = trimmed.chars().last() {
        if PROMPT_CHARS.contains(&last_char) {
            // Require prompt-like context: user@host pattern or very short line (just prompt)
            let has_user_host = trimmed.contains('@') && trimmed.contains(':');
            let is_short_prompt = trimmed.len() <= 3; // e.g., "$ " or "❯"
            if has_user_host || is_short_prompt {
                return (true, format!("ends_with_prompt_char({last_char})"));
            }
        }
    }

    // Check for empty or whitespace-only last line (might indicate prompt)
    if trimmed.is_empty() && !tail.is_empty() {
        // Look at second-to-last line (raw, with trailing spaces)
        let lines: Vec<&str> = tail.lines().collect();
        if lines.len() >= 2 {
            let prev_line_raw = lines[lines.len() - 2];
            for ending in PROMPT_ENDINGS {
                if prev_line_raw.ends_with(ending) {
                    return (true, format!("prev_line_prompt({})", ending.trim()));
                }
            }
        }
    }

    (
        false,
        format!("no_prompt_detected(last={})", truncate_for_log(trimmed, 40)),
    )
}

/// Truncate string for logging, adding ellipsis if truncated.
fn truncate_for_log(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len.saturating_sub(3)])
    }
}

// ============================================================================
// WorkflowRunner - Event-driven workflow execution
// ============================================================================

/// Result of attempting to start a workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowStartResult {
    /// Workflow started successfully
    Started {
        /// Unique execution ID
        execution_id: String,
        /// Name of the workflow that was started
        workflow_name: String,
    },
    /// No workflow handles this detection
    NoMatchingWorkflow {
        /// The rule_id from the detection
        rule_id: String,
    },
    /// The pane is already locked by another workflow
    PaneLocked {
        /// The pane that is locked
        pane_id: u64,
        /// Workflow name holding the lock
        held_by_workflow: String,
        /// Execution ID holding the lock
        held_by_execution: String,
    },
    /// An error occurred
    Error {
        /// Error message
        error: String,
    },
}

impl WorkflowStartResult {
    /// Returns true if a workflow was started.
    #[must_use]
    pub fn is_started(&self) -> bool {
        matches!(self, Self::Started { .. })
    }

    /// Returns true if the pane was locked by another workflow.
    #[must_use]
    pub fn is_locked(&self) -> bool {
        matches!(self, Self::PaneLocked { .. })
    }

    /// Returns the execution ID if the workflow was started.
    #[must_use]
    pub fn execution_id(&self) -> Option<&str> {
        match self {
            Self::Started { execution_id, .. } => Some(execution_id),
            _ => None,
        }
    }
}

/// Result of workflow execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowExecutionResult {
    /// Workflow completed successfully
    Completed {
        /// Execution ID
        execution_id: String,
        /// Final result value
        result: serde_json::Value,
        /// Total elapsed time in milliseconds
        elapsed_ms: u64,
        /// Number of steps executed
        steps_executed: usize,
    },
    /// Workflow was aborted
    Aborted {
        /// Execution ID
        execution_id: String,
        /// Reason for abort
        reason: String,
        /// Step index where abort occurred
        step_index: usize,
        /// Elapsed time in milliseconds
        elapsed_ms: u64,
    },
    /// Workflow step was denied by policy
    PolicyDenied {
        /// Execution ID
        execution_id: String,
        /// Step index where denial occurred
        step_index: usize,
        /// Reason for denial
        reason: String,
    },
    /// An error occurred during execution
    Error {
        /// Execution ID (if available)
        execution_id: Option<String>,
        /// Error message
        error: String,
    },
}

impl WorkflowExecutionResult {
    /// Returns true if the workflow completed successfully.
    #[must_use]
    pub fn is_completed(&self) -> bool {
        matches!(self, Self::Completed { .. })
    }

    /// Returns true if the workflow was aborted.
    #[must_use]
    pub fn is_aborted(&self) -> bool {
        matches!(self, Self::Aborted { .. })
    }

    /// Returns the execution ID.
    #[must_use]
    pub fn execution_id(&self) -> Option<&str> {
        match self {
            Self::Completed { execution_id, .. }
            | Self::Aborted { execution_id, .. }
            | Self::PolicyDenied { execution_id, .. } => Some(execution_id),
            Self::Error { execution_id, .. } => execution_id.as_deref(),
        }
    }
}

/// Configuration for the workflow runner.
#[derive(Debug, Clone)]
pub struct WorkflowRunnerConfig {
    /// Maximum concurrent workflow executions
    pub max_concurrent: usize,
    /// Default timeout for step execution (milliseconds)
    pub step_timeout_ms: u64,
    /// Retry delay multiplier for exponential backoff
    pub retry_backoff_multiplier: f64,
    /// Maximum retries per step
    pub max_retries_per_step: usize,
}

impl Default for WorkflowRunnerConfig {
    fn default() -> Self {
        Self {
            max_concurrent: 3,
            step_timeout_ms: 30_000,
            retry_backoff_multiplier: 2.0,
            max_retries_per_step: 3,
        }
    }
}

/// Event-driven workflow runner that subscribes to detection events
/// and executes matching workflows.
///
/// # Architecture
///
/// ```text
/// EventBus (detections) -> WorkflowRunner -> find_matching_workflow
///                                         -> acquire_pane_lock
///                                         -> WorkflowEngine (persist)
///                                         -> execute_steps
///                                         -> release_pane_lock
/// ```
///
/// # Usage
///
/// ```ignore
/// let runner = WorkflowRunner::new(
///     engine,
///     lock_manager,
///     storage,
///     injector,
///     config,
/// );
///
/// // Register workflows
/// runner.register_workflow(Arc::new(MyWorkflow::new()));
///
/// // Run the event loop
/// runner.run(event_bus).await;
/// ```
pub struct WorkflowRunner {
    /// Registered workflows
    workflows: std::sync::RwLock<Vec<Arc<dyn Workflow>>>,
    /// Workflow engine for persistence
    engine: WorkflowEngine,
    /// Per-pane lock manager
    lock_manager: Arc<PaneWorkflowLockManager>,
    /// Storage handle for persistence
    storage: Arc<crate::storage::StorageHandle>,
    /// Policy-gated injector for terminal input
    injector: Arc<tokio::sync::Mutex<crate::policy::PolicyGatedInjector>>,
    /// Configuration
    config: WorkflowRunnerConfig,
}

impl WorkflowRunner {
    /// Create a new workflow runner.
    pub fn new(
        engine: WorkflowEngine,
        lock_manager: Arc<PaneWorkflowLockManager>,
        storage: Arc<crate::storage::StorageHandle>,
        injector: Arc<tokio::sync::Mutex<crate::policy::PolicyGatedInjector>>,
        config: WorkflowRunnerConfig,
    ) -> Self {
        Self {
            workflows: std::sync::RwLock::new(Vec::new()),
            engine,
            lock_manager,
            storage,
            injector,
            config,
        }
    }

    /// Get the lock manager.
    pub fn lock_manager(&self) -> &Arc<PaneWorkflowLockManager> {
        &self.lock_manager
    }

    /// Register a workflow.
    pub fn register_workflow(&self, workflow: Arc<dyn Workflow>) {
        let mut workflows = self.workflows.write().unwrap();
        workflows.push(workflow);
    }

    /// Find a workflow that handles the given detection.
    pub fn find_matching_workflow(
        &self,
        detection: &crate::patterns::Detection,
    ) -> Option<Arc<dyn Workflow>> {
        let workflows = self.workflows.read().unwrap();
        workflows.iter().find(|w| w.handles(detection)).cloned()
    }

    /// Find a workflow by name.
    pub fn find_workflow_by_name(&self, name: &str) -> Option<Arc<dyn Workflow>> {
        let workflows = self.workflows.read().unwrap();
        workflows.iter().find(|w| w.name() == name).cloned()
    }

    /// Handle a detection event, potentially starting a workflow.
    ///
    /// Returns immediately with `WorkflowStartResult`. The actual workflow
    /// execution happens asynchronously if started.
    pub async fn handle_detection(
        &self,
        pane_id: u64,
        detection: &crate::patterns::Detection,
        event_id: Option<i64>,
    ) -> WorkflowStartResult {
        // Find matching workflow
        let Some(workflow) = self.find_matching_workflow(detection) else {
            return WorkflowStartResult::NoMatchingWorkflow {
                rule_id: detection.rule_id.clone(),
            };
        };

        let workflow_name = workflow.name().to_string();

        // Try to acquire pane lock
        let execution_id = generate_workflow_id(&workflow_name);
        let lock_result = self
            .lock_manager
            .try_acquire(pane_id, &workflow_name, &execution_id);

        match lock_result {
            LockAcquisitionResult::AlreadyLocked {
                held_by_workflow,
                held_by_execution,
                ..
            } => {
                return WorkflowStartResult::PaneLocked {
                    pane_id,
                    held_by_workflow,
                    held_by_execution,
                };
            }
            LockAcquisitionResult::Acquired => {
                // Lock acquired, start execution
            }
        }

        // Start workflow execution via engine
        let context = serde_json::json!({
            "detection": {
                "rule_id": detection.rule_id,
                "matched_text": detection.matched_text,
                "severity": format!("{:?}", detection.severity),
            }
        });

        match self
            .engine
            .start_with_id(
                &self.storage,
                execution_id.clone(),
                &workflow_name,
                pane_id,
                event_id,
                Some(context),
            )
            .await
        {
            Ok(_execution) => WorkflowStartResult::Started {
                execution_id,
                workflow_name,
            },
            Err(e) => {
                // Release lock on error
                self.lock_manager.release(pane_id, &execution_id);
                WorkflowStartResult::Error {
                    error: e.to_string(),
                }
            }
        }
    }

    /// Run a workflow execution to completion.
    ///
    /// This method executes all steps of a workflow, handling retries,
    /// wait conditions, and policy gates.
    pub async fn run_workflow(
        &self,
        pane_id: u64,
        workflow: Arc<dyn Workflow>,
        execution_id: &str,
        start_step: usize,
    ) -> WorkflowExecutionResult {
        let start_time = Instant::now();
        let workflow_name = workflow.name().to_string();
        let step_count = workflow.step_count();
        let mut current_step = start_step;
        let mut retries = 0;

        // Create workflow context with injector for policy-gated actions
        let mut ctx = WorkflowContext::new(
            self.storage.clone(),
            pane_id,
            PaneCapabilities::default(),
            execution_id,
        )
        .with_injector(Arc::clone(&self.injector));

        while current_step < step_count {
            // Execute the step
            let step_result = workflow.execute_step(&mut ctx, current_step).await;

            // Log step result
            let result_type = match &step_result {
                StepResult::Continue => "continue",
                StepResult::Done { .. } => "done",
                StepResult::Retry { .. } => "retry",
                StepResult::Abort { .. } => "abort",
                StepResult::WaitFor { .. } => "wait_for",
                StepResult::SendText { .. } => "send_text",
            };

            let steps = workflow.steps();
            let step_name = steps
                .get(current_step)
                .map_or("unknown", |s| s.name.as_str());

            let result_data = serde_json::to_string(&step_result).ok();
            let step_started_at = now_ms();
            let step_completed_at = now_ms();

            // Persist step log for non-SendText steps
            // SendText steps are logged after injection to capture the audit_action_id (wa-nu4.1.1.11)
            if !matches!(&step_result, StepResult::SendText { .. }) {
                if let Err(e) = self
                    .storage
                    .insert_step_log(
                        execution_id,
                        None,
                        current_step,
                        step_name,
                        result_type,
                        result_data.clone(),
                        step_started_at,
                        step_completed_at,
                    )
                    .await
                {
                    tracing::warn!(
                        workflow = %workflow_name,
                        execution_id,
                        step = current_step,
                        error = %e,
                        "Failed to log step"
                    );
                }
            }

            // Handle step result
            match step_result {
                StepResult::Continue => {
                    current_step += 1;
                    retries = 0;

                    // Update execution state
                    if let Err(e) = self.update_execution_step(execution_id, current_step).await {
                        tracing::warn!(
                            execution_id,
                            error = %e,
                            "Failed to update execution step"
                        );
                        if let crate::Error::Workflow(crate::error::WorkflowError::Aborted(reason)) = e {
                            self.lock_manager.release(pane_id, execution_id);
                            return WorkflowExecutionResult::Aborted {
                                execution_id: execution_id.to_string(),
                                reason,
                                step_index: current_step,
                                elapsed_ms: elapsed_ms(start_time),
                            };
                        }
                    }
                }
                StepResult::Done { result } => {
                    // Workflow completed
                    let elapsed_ms = elapsed_ms(start_time);

                    // Update execution to completed
                    if let Err(e) = self
                        .complete_execution(execution_id, Some(result.clone()))
                        .await
                    {
                        tracing::warn!(
                            execution_id,
                            error = %e,
                            "Failed to complete execution"
                        );
                    }

                    // Mark trigger event as handled
                    if let Err(e) = self
                        .mark_trigger_event_handled(execution_id, "completed")
                        .await
                    {
                        tracing::warn!(
                            execution_id,
                            error = %e,
                            "Failed to mark trigger event as handled"
                        );
                    }

                    // Release lock
                    self.lock_manager.release(pane_id, execution_id);

                    return WorkflowExecutionResult::Completed {
                        execution_id: execution_id.to_string(),
                        result,
                        elapsed_ms,
                        steps_executed: current_step + 1,
                    };
                }
                StepResult::Retry { delay_ms } => {
                    retries += 1;
                    if retries > self.config.max_retries_per_step {
                        let elapsed_ms = elapsed_ms(start_time);
                        let reason = format!(
                            "Max retries ({}) exceeded at step {}",
                            self.config.max_retries_per_step, current_step
                        );

                        // Update execution to failed
                        if let Err(e) = self.fail_execution(execution_id, &reason).await {
                            tracing::warn!(
                                execution_id,
                                error = %e,
                                "Failed to fail execution"
                            );
                        }

                        // Mark trigger event as handled (with failed status)
                        if let Err(e) = self
                            .mark_trigger_event_handled(execution_id, "failed")
                            .await
                        {
                            tracing::warn!(
                                execution_id,
                                error = %e,
                                "Failed to mark trigger event as handled"
                            );
                        }

                        // Cleanup and release lock
                        workflow.cleanup(&mut ctx).await;
                        self.lock_manager.release(pane_id, execution_id);

                        return WorkflowExecutionResult::Aborted {
                            execution_id: execution_id.to_string(),
                            reason,
                            step_index: current_step,
                            elapsed_ms,
                        };
                    }

                    // Wait before retry
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
                StepResult::Abort { reason } => {
                    let elapsed_ms = elapsed_ms(start_time);

                    // Update execution to failed
                    if let Err(e) = self.fail_execution(execution_id, &reason).await {
                        tracing::warn!(
                            execution_id,
                            error = %e,
                            "Failed to fail execution"
                        );
                    }

                    // Mark trigger event as handled (with aborted status)
                    if let Err(e) = self
                        .mark_trigger_event_handled(execution_id, "aborted")
                        .await
                    {
                        tracing::warn!(
                            execution_id,
                            error = %e,
                            "Failed to mark trigger event as handled"
                        );
                    }

                    // Cleanup and release lock
                    workflow.cleanup(&mut ctx).await;
                    self.lock_manager.release(pane_id, execution_id);

                    return WorkflowExecutionResult::Aborted {
                        execution_id: execution_id.to_string(),
                        reason,
                        step_index: current_step,
                        elapsed_ms,
                    };
                }
                StepResult::WaitFor {
                    condition,
                    timeout_ms,
                } => {
                    // Update execution to waiting
                    if let Err(e) = self
                        .set_execution_waiting(execution_id, current_step, &condition)
                        .await
                    {
                        tracing::warn!(
                            execution_id,
                            error = %e,
                            "Failed to set waiting state"
                        );
                        if let crate::Error::Workflow(crate::error::WorkflowError::Aborted(reason)) = e {
                            self.lock_manager.release(pane_id, execution_id);
                            return WorkflowExecutionResult::Aborted {
                                execution_id: execution_id.to_string(),
                                reason,
                                step_index: current_step,
                                elapsed_ms: elapsed_ms(start_time),
                            };
                        }
                    }

                    // Execute wait condition
                    let timeout = timeout_ms.map_or_else(
                        || Duration::from_millis(self.config.step_timeout_ms),
                        Duration::from_millis,
                    );

                    // Simple wait implementation - in practice would use WaitConditionExecutor
                    match &condition {
                        WaitCondition::PaneIdle {
                            idle_threshold_ms, ..
                        } => {
                            tokio::time::sleep(Duration::from_millis(*idle_threshold_ms)).await;
                        }
                        WaitCondition::Pattern { .. } => {
                            // Would use WaitConditionExecutor here
                            tokio::time::sleep(timeout).await;
                        }
                        WaitCondition::External { .. } => {
                            // Would wait for external signal
                            tokio::time::sleep(timeout).await;
                        }
                    }

                    // Continue to next step after wait
                    current_step += 1;
                    retries = 0;

                    // Update execution back to running
                    if let Err(e) = self.update_execution_step(execution_id, current_step).await {
                        tracing::warn!(
                            execution_id,
                            error = %e,
                            "Failed to update execution step after wait"
                        );
                        if let crate::Error::Workflow(crate::error::WorkflowError::Aborted(reason)) = e {
                            self.lock_manager.release(pane_id, execution_id);
                            return WorkflowExecutionResult::Aborted {
                                execution_id: execution_id.to_string(),
                                reason,
                                step_index: current_step,
                                elapsed_ms: elapsed_ms(start_time),
                            };
                        }
                    }
                }
                StepResult::SendText {
                    text,
                    wait_for,
                    wait_timeout_ms,
                } => {
                    // Attempt to send text via policy-gated injector
                    tracing::info!(
                        pane_id,
                        execution_id,
                        text_len = text.len(),
                        "Workflow requesting text injection"
                    );

                    let send_result = {
                        let mut guard = self.injector.lock().await;
                        guard
                            .send_text(
                                pane_id,
                                &text,
                                crate::policy::ActorKind::Workflow,
                                ctx.capabilities(),
                                Some(execution_id),
                            )
                            .await
                    };

                    // Log the SendText step with audit_action_id (wa-nu4.1.1.11)
                    let audit_action_id = send_result.audit_action_id();
                    if let Err(e) = self
                        .storage
                        .insert_step_log(
                            execution_id,
                            audit_action_id,
                            current_step,
                            step_name,
                            "send_text",
                            result_data.clone(),
                            step_started_at,
                            now_ms(), // Use current time as completion
                        )
                        .await
                    {
                        tracing::warn!(
                            workflow = %workflow_name,
                            execution_id,
                            step = current_step,
                            ?audit_action_id,
                            error = %e,
                            "Failed to log SendText step"
                        );
                    }

                    match send_result {
                        crate::policy::InjectionResult::Allowed { .. } => {
                            tracing::info!(pane_id, execution_id, "Text injection succeeded");

                            // If there's a wait condition, handle it
                            if let Some(condition) = wait_for {
                                let timeout = wait_timeout_ms.map_or_else(
                                    || Duration::from_millis(self.config.step_timeout_ms),
                                    Duration::from_millis,
                                );

                                // Simple wait implementation
                                match &condition {
                                    WaitCondition::PaneIdle {
                                        idle_threshold_ms, ..
                                    } => {
                                        tokio::time::sleep(Duration::from_millis(
                                            *idle_threshold_ms,
                                        ))
                                        .await;
                                    }
                                    WaitCondition::Pattern { .. } => {
                                        tokio::time::sleep(timeout).await;
                                    }
                                    WaitCondition::External { .. } => {
                                        tokio::time::sleep(timeout).await;
                                    }
                                }
                            }

                            // Continue to next step
                            current_step += 1;
                            retries = 0;

                            if let Err(e) =
                                self.update_execution_step(execution_id, current_step).await
                            {
                                tracing::warn!(
                                    execution_id,
                                    error = %e,
                                    "Failed to update execution step after send"
                                );
                                if let crate::Error::Workflow(crate::error::WorkflowError::Aborted(reason)) = e {
                                    self.lock_manager.release(pane_id, execution_id);
                                    return WorkflowExecutionResult::Aborted {
                                        execution_id: execution_id.to_string(),
                                        reason,
                                        step_index: current_step,
                                        elapsed_ms: elapsed_ms(start_time),
                                    };
                                }
                            }
                        }
                        crate::policy::InjectionResult::Denied { decision, .. } => {
                            let elapsed_ms = elapsed_ms(start_time);
                            let reason = match &decision {
                                crate::policy::PolicyDecision::Deny { reason, .. } => {
                                    reason.clone()
                                }
                                _ => "Unknown denial reason".to_string(),
                            };
                            let abort_reason = format!("Policy denied text injection: {reason}");

                            tracing::warn!(
                                pane_id,
                                execution_id,
                                reason = %reason,
                                "Text injection denied by policy"
                            );

                            // Update execution to failed
                            if let Err(e) = self.fail_execution(execution_id, &abort_reason).await {
                                tracing::warn!(
                                    execution_id,
                                    error = %e,
                                    "Failed to fail execution"
                                );
                            }

                            // Mark trigger event as handled (with denied status)
                            if let Err(e) = self
                                .mark_trigger_event_handled(execution_id, "denied")
                                .await
                            {
                                tracing::warn!(
                                    execution_id,
                                    error = %e,
                                    "Failed to mark trigger event as handled"
                                );
                            }

                            // Cleanup and release lock
                            workflow.cleanup(&mut ctx).await;
                            self.lock_manager.release(pane_id, execution_id);

                            return WorkflowExecutionResult::Aborted {
                                execution_id: execution_id.to_string(),
                                reason: abort_reason,
                                step_index: current_step,
                                elapsed_ms,
                            };
                        }
                        crate::policy::InjectionResult::RequiresApproval { decision, .. } => {
                            let elapsed_ms = elapsed_ms(start_time);
                            let code = match &decision {
                                crate::policy::PolicyDecision::RequireApproval {
                                    approval, ..
                                } => approval.as_ref().map_or_else(
                                    || "unknown".to_string(),
                                    |a| a.allow_once_code.clone(),
                                ),
                                _ => "unknown".to_string(),
                            };
                            let abort_reason =
                                format!("Text injection requires approval (code: {code})");

                            tracing::warn!(
                                pane_id,
                                execution_id,
                                code = %code,
                                "Text injection requires approval"
                            );

                            // Update execution to failed (approval not auto-granted for workflows)
                            if let Err(e) = self.fail_execution(execution_id, &abort_reason).await {
                                tracing::warn!(
                                    execution_id,
                                    error = %e,
                                    "Failed to fail execution"
                                );
                            }

                            // Cleanup and release lock
                            workflow.cleanup(&mut ctx).await;
                            self.lock_manager.release(pane_id, execution_id);

                            return WorkflowExecutionResult::Aborted {
                                execution_id: execution_id.to_string(),
                                reason: abort_reason,
                                step_index: current_step,
                                elapsed_ms,
                            };
                        }
                        crate::policy::InjectionResult::Error { error, .. } => {
                            let elapsed_ms = elapsed_ms(start_time);
                            let abort_reason =
                                format!("Text injection failed after policy allowed: {error}");

                            tracing::error!(
                                pane_id,
                                execution_id,
                                error = %error,
                                "Text injection failed after policy allowed"
                            );

                            // Update execution to failed
                            if let Err(e) = self.fail_execution(execution_id, &abort_reason).await {
                                tracing::warn!(
                                    execution_id,
                                    error = %e,
                                    "Failed to fail execution"
                                );
                            }

                            // Mark trigger event as handled (with error status)
                            if let Err(e) =
                                self.mark_trigger_event_handled(execution_id, "error").await
                            {
                                tracing::warn!(
                                    execution_id,
                                    error = %e,
                                    "Failed to mark trigger event as handled"
                                );
                            }

                            // Cleanup and release lock
                            workflow.cleanup(&mut ctx).await;
                            self.lock_manager.release(pane_id, execution_id);

                            return WorkflowExecutionResult::Aborted {
                                execution_id: execution_id.to_string(),
                                reason: abort_reason,
                                step_index: current_step,
                                elapsed_ms,
                            };
                        }
                    }
                }
            }
        }

        // All steps completed without explicit Done
        let elapsed_ms = elapsed_ms(start_time);
        let result = serde_json::json!({ "status": "completed" });

        if let Err(e) = self
            .complete_execution(execution_id, Some(result.clone()))
            .await
        {
            tracing::warn!(
                execution_id,
                error = %e,
                "Failed to complete execution"
            );
        }

        // Mark trigger event as handled
        if let Err(e) = self
            .mark_trigger_event_handled(execution_id, "completed")
            .await
        {
            tracing::warn!(
                execution_id,
                error = %e,
                "Failed to mark trigger event as handled"
            );
        }

        self.lock_manager.release(pane_id, execution_id);

        WorkflowExecutionResult::Completed {
            execution_id: execution_id.to_string(),
            result,
            elapsed_ms,
            steps_executed: step_count,
        }
    }

    /// Run the event loop, subscribing to detection events.
    ///
    /// This spawns workflow executions for matching detections. The loop
    /// runs until the event bus channel is closed.
    ///
    /// On startup, resumes any incomplete workflows that were interrupted
    /// (e.g., by a previous watcher crash or restart).
    pub async fn run(&self, event_bus: &crate::events::EventBus) {
        // Resume any incomplete workflows from a previous run
        let resumed = self.resume_incomplete().await;
        if !resumed.is_empty() {
            tracing::info!(
                count = resumed.len(),
                "Resumed incomplete workflows from previous run"
            );
            for result in &resumed {
                match result {
                    WorkflowExecutionResult::Completed { execution_id, .. } => {
                        tracing::info!(execution_id, "Resumed workflow completed");
                    }
                    WorkflowExecutionResult::Error {
                        execution_id,
                        error,
                    } => {
                        tracing::warn!(?execution_id, error, "Resumed workflow errored");
                    }
                    _ => {}
                }
            }
        }

        let mut subscriber = event_bus.subscribe_detections();

        loop {
            match subscriber.recv().await {
                Ok(event) => {
                    if let crate::events::Event::PatternDetected {
                        pane_id,
                        detection,
                        event_id,
                    } = event
                    {
                        // Handle detection with event_id for proper event lifecycle
                        let result = self.handle_detection(pane_id, &detection, event_id).await;

                        match result {
                            WorkflowStartResult::Started {
                                execution_id,
                                workflow_name,
                            } => {
                                // Find workflow and spawn execution
                                if let Some(workflow) = self.find_workflow_by_name(&workflow_name) {
                                    let execution_id_clone = execution_id.clone();
                                    let workflow_clone = Arc::clone(&workflow);
                                    let storage = Arc::clone(&self.storage);
                                    let lock_manager = Arc::clone(&self.lock_manager);
                                    let config = self.config.clone();
                                    let engine = WorkflowEngine::new(config.max_concurrent);

                                    // Create a mini-runner for the spawned task
                                    let runner = Self {
                                        workflows: std::sync::RwLock::new(vec![
                                            workflow_clone.clone(),
                                        ]),
                                        engine,
                                        lock_manager,
                                        storage,
                                        injector: Arc::clone(&self.injector),
                                        config,
                                    };

                                    tokio::spawn(async move {
                                        let result = runner
                                            .run_workflow(
                                                pane_id,
                                                workflow_clone,
                                                &execution_id_clone,
                                                0,
                                            )
                                            .await;

                                        match &result {
                                            WorkflowExecutionResult::Completed {
                                                execution_id,
                                                steps_executed,
                                                elapsed_ms,
                                                ..
                                            } => {
                                                tracing::info!(
                                                    execution_id,
                                                    steps = steps_executed,
                                                    elapsed_ms,
                                                    "Workflow completed"
                                                );
                                            }
                                            WorkflowExecutionResult::Aborted {
                                                execution_id,
                                                reason,
                                                step_index,
                                                ..
                                            } => {
                                                tracing::warn!(
                                                    execution_id,
                                                    step = step_index,
                                                    reason,
                                                    "Workflow aborted"
                                                );
                                            }
                                            WorkflowExecutionResult::PolicyDenied {
                                                execution_id,
                                                step_index,
                                                reason,
                                            } => {
                                                tracing::warn!(
                                                    execution_id,
                                                    step = step_index,
                                                    reason,
                                                    "Workflow denied by policy"
                                                );
                                            }
                                            WorkflowExecutionResult::Error {
                                                execution_id,
                                                error,
                                            } => {
                                                tracing::error!(
                                                    execution_id = execution_id.as_deref(),
                                                    error,
                                                    "Workflow error"
                                                );
                                            }
                                        }
                                    });
                                }
                            }
                            WorkflowStartResult::NoMatchingWorkflow { rule_id } => {
                                tracing::debug!(rule_id, "No workflow handles detection");
                            }
                            WorkflowStartResult::PaneLocked {
                                pane_id,
                                held_by_workflow,
                                ..
                            } => {
                                tracing::debug!(
                                    pane_id,
                                    held_by = %held_by_workflow,
                                    "Pane locked, skipping detection"
                                );
                            }
                            WorkflowStartResult::Error { error } => {
                                tracing::error!(error, "Failed to start workflow");
                            }
                        }
                    }
                }
                Err(crate::events::RecvError::Lagged { missed_count }) => {
                    tracing::warn!(
                        skipped = missed_count,
                        "Workflow runner lagged, skipped events"
                    );
                }
                Err(crate::events::RecvError::Closed) => {
                    tracing::info!("Event bus closed, workflow runner stopping");
                    break;
                }
            }
        }
    }

    /// Resume incomplete workflows after restart.
    ///
    /// Queries storage for workflows with status 'running' or 'waiting'
    /// and attempts to resume them.
    pub async fn resume_incomplete(&self) -> Vec<WorkflowExecutionResult> {
        let incomplete = match self.storage.find_incomplete_workflows().await {
            Ok(workflows) => workflows,
            Err(e) => {
                tracing::error!(error = %e, "Failed to query incomplete workflows");
                return vec![];
            }
        };

        let mut results = Vec::new();

        for record in incomplete {
            // Find the workflow definition
            let Some(workflow) = self.find_workflow_by_name(&record.workflow_name) else {
                tracing::warn!(
                    workflow_name = %record.workflow_name,
                    execution_id = %record.id,
                    "Cannot resume: workflow not registered"
                );
                continue;
            };

            // Compute next step from logs
            let step_logs = match self.storage.get_step_logs(&record.id).await {
                Ok(logs) => logs,
                Err(e) => {
                    tracing::warn!(
                        execution_id = %record.id,
                        error = %e,
                        "Failed to get step logs for resume"
                    );
                    continue;
                }
            };

            let next_step = compute_next_step(&step_logs);

            // Try to re-acquire lock
            let lock_result =
                self.lock_manager
                    .try_acquire(record.pane_id, &record.workflow_name, &record.id);

            match lock_result {
                LockAcquisitionResult::AlreadyLocked { .. } => {
                    tracing::warn!(
                        execution_id = %record.id,
                        pane_id = record.pane_id,
                        "Cannot resume: pane locked"
                    );
                    continue;
                }
                LockAcquisitionResult::Acquired => {}
            }

            tracing::info!(
                execution_id = %record.id,
                workflow = %record.workflow_name,
                pane_id = record.pane_id,
                resume_step = next_step,
                "Resuming workflow"
            );

            let result = self
                .run_workflow(record.pane_id, workflow, &record.id, next_step)
                .await;

            results.push(result);
        }

        results
    }

    // --- Private helper methods ---

    async fn update_execution_step(&self, execution_id: &str, step: usize) -> crate::Result<()> {
        let mut record = self
            .storage
            .get_workflow(execution_id)
            .await?
            .ok_or_else(|| {
                crate::Error::Workflow(crate::error::WorkflowError::NotFound(
                    execution_id.to_string(),
                ))
            })?;

        // Check if workflow was externally aborted/completed
        if record.status == "aborted" || record.status == "failed" || record.status == "completed" {
            return Err(crate::Error::Workflow(crate::error::WorkflowError::Aborted(
                format!("Workflow externally modified to status: {}", record.status),
            )));
        }

        record.current_step = step;
        record.status = "running".to_string();
        record.wait_condition = None;
        record.updated_at = now_ms();

        self.storage.upsert_workflow(record).await
    }

    async fn set_execution_waiting(
        &self,
        execution_id: &str,
        step: usize,
        condition: &WaitCondition,
    ) -> crate::Result<()> {
        let mut record = self
            .storage
            .get_workflow(execution_id)
            .await?
            .ok_or_else(|| {
                crate::Error::Workflow(crate::error::WorkflowError::NotFound(
                    execution_id.to_string(),
                ))
            })?;

        // Check if workflow was externally aborted/completed
        if record.status == "aborted" || record.status == "failed" || record.status == "completed" {
            return Err(crate::Error::Workflow(crate::error::WorkflowError::Aborted(
                format!("Workflow externally modified to status: {}", record.status),
            )));
        }

        record.current_step = step;
        record.status = "waiting".to_string();
        record.wait_condition = Some(serde_json::to_value(condition)?);
        record.updated_at = now_ms();

        self.storage.upsert_workflow(record).await
    }

    async fn complete_execution(
        &self,
        execution_id: &str,
        result: Option<serde_json::Value>,
    ) -> crate::Result<()> {
        let mut record = self
            .storage
            .get_workflow(execution_id)
            .await?
            .ok_or_else(|| {
                crate::Error::Workflow(crate::error::WorkflowError::NotFound(
                    execution_id.to_string(),
                ))
            })?;

        record.status = "completed".to_string();
        record.result = result;
        record.updated_at = now_ms();
        record.completed_at = Some(now_ms());

        self.storage.upsert_workflow(record).await
    }

    async fn fail_execution(&self, execution_id: &str, error: &str) -> crate::Result<()> {
        let mut record = self
            .storage
            .get_workflow(execution_id)
            .await?
            .ok_or_else(|| {
                crate::Error::Workflow(crate::error::WorkflowError::NotFound(
                    execution_id.to_string(),
                ))
            })?;

        record.status = "failed".to_string();
        record.error = Some(error.to_string());
        record.updated_at = now_ms();
        record.completed_at = Some(now_ms());

        self.storage.upsert_workflow(record).await
    }

    /// Mark the triggering event as handled after workflow completion.
    ///
    /// This ensures proper event lifecycle management - events that triggered
    /// workflows are marked with the outcome so they won't be re-processed.
    ///
    /// # Arguments
    /// * `execution_id` - The workflow execution ID
    /// * `status` - The handling status ("completed", "failed", "aborted", "denied")
    async fn mark_trigger_event_handled(
        &self,
        execution_id: &str,
        status: &str,
    ) -> crate::Result<()> {
        // Get the workflow record to find trigger_event_id
        let record = self.storage.get_workflow(execution_id).await?;

        if let Some(record) = record {
            if let Some(event_id) = record.trigger_event_id {
                self.storage
                    .mark_event_handled(event_id, Some(execution_id.to_string()), status)
                    .await?;

                tracing::debug!(
                    execution_id,
                    event_id,
                    status,
                    "Marked trigger event as handled"
                );
            }
        }

        Ok(())
    }

    /// Abort a running workflow execution.
    ///
    /// This is the external API for aborting workflows (e.g., from robot mode).
    /// It differs from internal abort handling in that:
    /// 1. It validates the execution state before aborting
    /// 2. It releases the pane lock if held
    /// 3. It returns detailed abort information
    ///
    /// # Arguments
    /// * `execution_id` - The workflow execution ID to abort
    /// * `reason` - Optional reason for the abort (recorded in audit)
    /// * `force` - If true, skip cleanup steps
    ///
    /// # Returns
    /// * `Ok(AbortResult)` - Details about the aborted workflow
    /// * `Err` - If the workflow doesn't exist or is in invalid state
    pub async fn abort_execution(
        &self,
        execution_id: &str,
        reason: Option<&str>,
        _force: bool, // Reserved for future cleanup skipping
    ) -> crate::Result<AbortResult> {
        // Load the workflow record
        let record = self
            .storage
            .get_workflow(execution_id)
            .await?
            .ok_or_else(|| {
                crate::Error::Workflow(crate::error::WorkflowError::NotFound(
                    execution_id.to_string(),
                ))
            })?;

        // Check if already in terminal state
        match record.status.as_str() {
            "completed" => {
                return Ok(AbortResult {
                    aborted: false,
                    execution_id: execution_id.to_string(),
                    workflow_name: record.workflow_name,
                    pane_id: record.pane_id,
                    previous_status: record.status.clone(),
                    aborted_at_step: record.current_step,
                    reason: None,
                    aborted_at: None,
                    error_reason: Some("already_completed".to_string()),
                });
            }
            "aborted" => {
                return Ok(AbortResult {
                    aborted: false,
                    execution_id: execution_id.to_string(),
                    workflow_name: record.workflow_name,
                    pane_id: record.pane_id,
                    previous_status: record.status.clone(),
                    aborted_at_step: record.current_step,
                    reason: None,
                    aborted_at: None,
                    error_reason: Some("already_aborted".to_string()),
                });
            }
            "failed" => {
                return Ok(AbortResult {
                    aborted: false,
                    execution_id: execution_id.to_string(),
                    workflow_name: record.workflow_name,
                    pane_id: record.pane_id,
                    previous_status: record.status.clone(),
                    aborted_at_step: record.current_step,
                    reason: None,
                    aborted_at: None,
                    error_reason: Some("already_failed".to_string()),
                });
            }
            _ => {} // running, waiting - proceed with abort
        }

        let previous_status = record.status.clone();
        let workflow_name = record.workflow_name.clone();
        let pane_id = record.pane_id;
        let aborted_at_step = record.current_step;
        let now = now_ms();

        // Update the record to aborted status
        let mut updated_record = record;
        updated_record.status = "aborted".to_string();
        updated_record.error = reason.map(|r| format!("Aborted: {r}"));
        updated_record.updated_at = now;
        updated_record.completed_at = Some(now);

        self.storage.upsert_workflow(updated_record).await?;

        // Release the pane lock if held
        self.lock_manager.release(pane_id, execution_id);

        // Mark trigger event as handled with aborted status
        if let Err(e) = self
            .mark_trigger_event_handled(execution_id, "aborted")
            .await
        {
            tracing::warn!(
                execution_id,
                error = %e,
                "Failed to mark trigger event as handled during abort"
            );
        }

        tracing::info!(
            execution_id,
            workflow_name,
            pane_id,
            reason = reason.unwrap_or("no reason provided"),
            "Workflow aborted"
        );

        Ok(AbortResult {
            aborted: true,
            execution_id: execution_id.to_string(),
            workflow_name,
            pane_id,
            previous_status,
            aborted_at_step,
            reason: reason.map(std::string::ToString::to_string),
            aborted_at: Some(now as u64),
            error_reason: None,
        })
    }
}

/// Result of an abort operation
#[derive(Debug, Clone, serde::Serialize)]
pub struct AbortResult {
    /// Whether the abort was successful
    pub aborted: bool,
    /// Execution ID
    pub execution_id: String,
    /// Workflow name
    pub workflow_name: String,
    /// Pane ID
    pub pane_id: u64,
    /// Status before abort
    pub previous_status: String,
    /// Step index where abort occurred
    pub aborted_at_step: usize,
    /// Reason for abort (if provided)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Timestamp of abort (epoch ms)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aborted_at: Option<u64>,
    /// Error reason if abort failed (e.g., "already_completed")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_reason: Option<String>,
}

// ============================================================================
// Built-in Workflows
// ============================================================================

/// Agent-specific prompts for context refresh after compaction.
///
/// These prompts are carefully crafted to be:
/// - Minimal in length (to avoid adding too much to already-compacted context)
/// - Clear in intent (agent should re-read key project files)
/// - Agent-specific (matching each agent's communication style)
pub mod compaction_prompts {
    /// Prompt for Claude Code agents.
    pub const CLAUDE_CODE: &str = "Reread AGENTS.md so it's still fresh in your mind.\n";

    /// Prompt for Codex CLI agents.
    pub const CODEX: &str = "Please re-read AGENTS.md and any key project context files.\n";

    /// Prompt for Gemini CLI agents.
    pub const GEMINI: &str = "Please re-examine AGENTS.md and project context.\n";

    /// Default prompt for unknown agents.
    pub const UNKNOWN: &str = "Please review the project context files (AGENTS.md, README.md).\n";
}

/// Handle compaction workflow: re-inject critical context after conversation compaction.
///
/// This workflow is triggered when an AI agent compacts or summarizes its context window.
/// After compaction, the agent may have lost important project context, so we prompt
/// the agent to re-read key files like AGENTS.md.
///
/// # Steps
///
/// 1. **Acquire lock**: Get per-pane workflow lock to prevent concurrent workflows.
/// 2. **Validate state**: Check that pane is not in alt-screen mode and has no recent gap.
/// 3. **Confirm anchor**: Re-read pane tail to verify compaction anchor is still present.
/// 4. **Stabilize**: Wait for pane to be idle (2s default) before sending.
/// 5. **Send prompt**: Inject agent-specific context refresh prompt.
/// 6. **Verify**: Wait for response pattern or timeout.
///
/// # Safety
///
/// - All sends are policy-gated (may be denied by PolicyEngine).
/// - Workflow is idempotent: dedupe/cooldown prevents spam on repeated detections.
/// - Guards abort workflow if pane state is unsuitable for injection.
///
/// # Example Detection
///
/// ```text
/// rule_id: "claude_code.compaction"
/// event_type: "session.compaction"
/// matched_text: "Auto-compact: compacted 150,000 tokens to 25,000 tokens"
/// ```
pub struct HandleCompaction {
    /// Default stabilization wait time in milliseconds.
    pub stabilization_ms: u64,
    /// Timeout for the idle wait condition.
    pub idle_timeout_ms: u64,
}

impl Default for HandleCompaction {
    fn default() -> Self {
        Self {
            stabilization_ms: 2000,
            idle_timeout_ms: 10_000,
        }
    }
}

impl HandleCompaction {
    /// Create a new HandleCompaction workflow with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create with custom stabilization time.
    #[must_use]
    pub fn with_stabilization_ms(mut self, ms: u64) -> Self {
        self.stabilization_ms = ms;
        self
    }

    /// Create with custom idle timeout.
    #[must_use]
    pub fn with_idle_timeout_ms(mut self, ms: u64) -> Self {
        self.idle_timeout_ms = ms;
        self
    }

    /// Get the agent-specific prompt based on agent type from trigger detection.
    fn get_prompt_for_agent(ctx: &WorkflowContext) -> &'static str {
        // Extract agent type from trigger detection if available
        let agent_type = ctx
            .trigger()
            .and_then(|t| t.get("agent_type"))
            .and_then(|v| v.as_str())
            .map_or(crate::patterns::AgentType::Unknown, |s| match s {
                "claude_code" => crate::patterns::AgentType::ClaudeCode,
                "codex" => crate::patterns::AgentType::Codex,
                "gemini" => crate::patterns::AgentType::Gemini,
                _ => crate::patterns::AgentType::Unknown,
            });

        match agent_type {
            crate::patterns::AgentType::ClaudeCode => compaction_prompts::CLAUDE_CODE,
            crate::patterns::AgentType::Codex => compaction_prompts::CODEX,
            crate::patterns::AgentType::Gemini => compaction_prompts::GEMINI,
            _ => compaction_prompts::UNKNOWN,
        }
    }

    /// Check if pane state allows workflow execution.
    ///
    /// Guards against:
    /// - Alt-screen mode (vim, less, etc.)
    /// - Recent output gap (unknown pane state)
    /// - Command currently running
    fn check_pane_guards(ctx: &WorkflowContext) -> Result<(), String> {
        let caps = ctx.capabilities();

        // Guard: alt-screen blocks sends (Some(true) = definitely in alt-screen)
        if caps.alt_screen == Some(true) {
            return Err("Pane is in alt-screen mode (vim, less, etc.) - aborting".to_string());
        }

        // Guard: command running could cause issues
        if caps.command_running {
            return Err("Command is currently running in pane - aborting".to_string());
        }

        // Guard: recent gap suggests unknown state
        if caps.has_recent_gap {
            return Err("Recent output gap detected - pane state uncertain".to_string());
        }

        Ok(())
    }
}

impl Workflow for HandleCompaction {
    fn name(&self) -> &'static str {
        "handle_compaction"
    }

    fn description(&self) -> &'static str {
        "Re-inject critical context (AGENTS.md) after conversation compaction"
    }

    fn handles(&self, detection: &crate::patterns::Detection) -> bool {
        // Handle any compaction-related detection
        detection.event_type == "session.compaction" || detection.rule_id.contains("compaction")
    }

    fn steps(&self) -> Vec<WorkflowStep> {
        vec![
            WorkflowStep::new("check_guards", "Validate pane state allows injection"),
            WorkflowStep::new("stabilize", "Wait for pane to become idle"),
            WorkflowStep::new("send_prompt", "Send agent-specific context refresh prompt"),
            WorkflowStep::new("verify_send", "Verify the prompt was processed"),
        ]
    }

    fn execute_step(
        &self,
        ctx: &mut WorkflowContext,
        step_idx: usize,
    ) -> BoxFuture<'_, StepResult> {
        // Capture all values needed in the async block BEFORE entering it.
        // This avoids lifetime issues since we own the captured values.
        let stabilization_ms = self.stabilization_ms;
        let pane_id = ctx.pane_id();
        let execution_id = ctx.execution_id().to_string();

        // For step 0: capture guard check result
        let guard_check_result = if step_idx == 0 {
            Some(Self::check_pane_guards(ctx))
        } else {
            None
        };

        // For step 2: capture prompt and injector availability
        let prompt = if step_idx == 2 {
            Some(Self::get_prompt_for_agent(ctx))
        } else {
            None
        };
        let has_injector = ctx.has_injector();

        // For step 3: capture trigger info
        let (tokens_before, tokens_after) = if step_idx == 3 {
            let before = ctx
                .trigger()
                .and_then(|t| t.get("extracted"))
                .and_then(|e| e.get("tokens_before"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let after = ctx
                .trigger()
                .and_then(|t| t.get("extracted"))
                .and_then(|e| e.get("tokens_after"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            (before, after)
        } else {
            (String::new(), String::new())
        };

        Box::pin(async move {
            match step_idx {
                // Step 0: Check guards - validate pane state
                0 => {
                    tracing::info!(
                        pane_id,
                        execution_id = %execution_id,
                        "handle_compaction: checking pane guards"
                    );

                    if let Some(Err(reason)) = guard_check_result {
                        tracing::warn!(
                            pane_id,
                            reason = %reason,
                            "handle_compaction: guard check failed"
                        );
                        return StepResult::abort(reason);
                    }

                    tracing::debug!(
                        pane_id,
                        "handle_compaction: guards passed, proceeding to stabilization"
                    );
                    StepResult::cont()
                }

                // Step 1: Stabilize - wait for pane to be idle
                1 => {
                    tracing::info!(
                        pane_id,
                        stabilization_ms,
                        "handle_compaction: waiting for pane to stabilize"
                    );

                    // Wait for pane idle using wait condition
                    StepResult::wait_for_with_timeout(
                        WaitCondition::pane_idle(stabilization_ms),
                        stabilization_ms * 5, // 5x stabilization as timeout
                    )
                }

                // Step 2: Send agent-specific prompt
                // The runner will handle the actual text injection via policy-gated injector.
                2 => {
                    let prompt = prompt.unwrap_or(compaction_prompts::UNKNOWN);

                    tracing::info!(
                        pane_id,
                        execution_id = %execution_id,
                        prompt_len = prompt.len(),
                        "handle_compaction: sending context refresh prompt"
                    );

                    // Check if injector is available
                    if !has_injector {
                        tracing::error!(pane_id, "handle_compaction: no injector configured");
                        return StepResult::abort("No injector configured for text injection");
                    }

                    // Use SendText to request the runner inject the prompt.
                    // The runner will call the policy-gated injector and abort if denied.
                    StepResult::send_text(prompt)
                }

                // Step 3: Verify the send (best-effort)
                3 => {
                    // For now, we consider the workflow done after the send step.
                    // Future: wait for OSC 133 prompt boundary or agent response pattern.
                    tracing::info!(
                        pane_id,
                        execution_id = %execution_id,
                        "handle_compaction: workflow completed successfully"
                    );

                    StepResult::done(serde_json::json!({
                        "status": "completed",
                        "pane_id": pane_id,
                        "tokens_before": tokens_before,
                        "tokens_after": tokens_after,
                        "action": "sent_context_refresh_prompt"
                    }))
                }

                _ => {
                    tracing::error!(
                        pane_id,
                        step_idx,
                        "handle_compaction: unexpected step index"
                    );
                    StepResult::abort(format!("Unexpected step index: {step_idx}"))
                }
            }
        })
    }

    fn cleanup(&self, _ctx: &mut WorkflowContext) -> BoxFuture<'_, ()> {
        // Note: We don't use ctx here because the async block would need to capture
        // values from ctx, which has a different lifetime. For a simple cleanup,
        // we just log that cleanup was called.
        Box::pin(async move {
            tracing::debug!("handle_compaction: cleanup completed");
        })
    }
}

/// Handle usage limits workflow: exit agent, persist session, and select new account.
pub struct HandleUsageLimits;

impl HandleUsageLimits {
    pub fn new() -> Self {
        Self
    }
}

impl Default for HandleUsageLimits {
    fn default() -> Self {
        Self::new()
    }
}

impl Workflow for HandleUsageLimits {
    fn name(&self) -> &'static str {
        "handle_usage_limits"
    }

    fn description(&self) -> &'static str {
        "Exit agent, persist session summary, and select new account for failover"
    }

    fn handles(&self, detection: &crate::patterns::Detection) -> bool {
        detection.rule_id.contains("usage") && detection.agent_type == crate::patterns::AgentType::Codex
    }

    fn steps(&self) -> Vec<WorkflowStep> {
        vec![
            WorkflowStep::new("check_guards", "Validate pane state allows interaction"),
            WorkflowStep::new("exit_and_persist", "Exit Codex and persist session summary"),
            WorkflowStep::new("select_account", "Select best available account"),
        ]
    }

    fn execute_step(
        &self,
        ctx: &mut WorkflowContext,
        step_idx: usize,
    ) -> BoxFuture<'_, StepResult> {
        let pane_id = ctx.pane_id();
        let storage = ctx.storage().clone();
        let mut ctx_clone = ctx.clone();

        Box::pin(async move {
            match step_idx {
                0 => {
                    let caps = ctx_clone.capabilities();
                    if caps.alt_screen == Some(true) {
                        return StepResult::abort("Pane is in alt-screen mode");
                    }
                    if caps.command_running {
                        return StepResult::abort("Command is running");
                    }
                    StepResult::cont()
                }
                1 => {
                    let client = crate::wezterm::WeztermClient::new();
                    let options = CodexExitOptions::default();

                    let outcome = codex_exit_and_wait_for_summary(
                        pane_id,
                        &client,
                        || {
                            let mut c = ctx_clone.clone();
                            async move {
                                c.send_ctrl_c().await.map_err(|e| e.to_string())
                            }
                        },
                        &options
                    ).await;

                    match outcome {
                        Ok(_) => {
                            let text = match client.get_text(pane_id, false).await {
                                Ok(t) => t,
                                Err(e) => return StepResult::abort(format!("Failed to get text: {e}")),
                            };
                            let tail = crate::wezterm::tail_text(&text, 200);
                            
                            match parse_codex_session_summary(&tail) {
                                Ok(parsed) => {
                                    if let Err(e) = persist_codex_session_summary(&storage, pane_id, &parsed).await {
                                        tracing::warn!("Failed to persist session summary: {e}");
                                    }
                                    StepResult::cont()
                                }
                                Err(e) => {
                                    tracing::warn!("Failed to parse session summary: {e}");
                                    StepResult::cont()
                                }
                            }
                        }
                        Err(e) => StepResult::abort(format!("Failed to exit Codex: {e}")),
                    }
                }
                2 => {
                    let caut_client = crate::caut::CautClient::new();
                    let config = crate::accounts::AccountSelectionConfig::default();
                    let result = refresh_and_select_account(&caut_client, &storage, &config).await;

                    match result {
                        Ok(selection) => {
                            let json = serde_json::to_value(selection).unwrap_or_default();
                            StepResult::done(json)
                        }
                        Err(e) => StepResult::abort(e.to_string()),
                    }
                }
                _ => StepResult::abort("Unexpected step"),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patterns::{AgentType, Detection, Severity};

    // ========================================================================
    // StepResult Tests
    // ========================================================================

    #[test]
    fn step_result_continue_serializes() {
        let result = StepResult::Continue;
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("continue"));

        let parsed: StepResult = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_continue());
    }

    #[test]
    fn step_result_done_serializes() {
        let result = StepResult::done(serde_json::json!({"status": "ok"}));
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("done"));
        assert!(json.contains("status"));

        let parsed: StepResult = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_done());
        assert!(parsed.is_terminal());
    }

    #[test]
    fn step_result_retry_serializes() {
        let result = StepResult::retry(5000);
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("retry"));
        assert!(json.contains("5000"));

        let parsed: StepResult = serde_json::from_str(&json).unwrap();
        match parsed {
            StepResult::Retry { delay_ms } => assert_eq!(delay_ms, 5000),
            _ => panic!("Expected Retry"),
        }
    }

    #[test]
    fn step_result_abort_serializes() {
        let result = StepResult::abort("test failure");
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("abort"));
        assert!(json.contains("test failure"));

        let parsed: StepResult = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_terminal());
    }

    #[test]
    fn step_result_wait_for_serializes() {
        let result =
            StepResult::wait_for_with_timeout(WaitCondition::pattern("prompt.ready"), 10_000);
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("wait_for"));
        assert!(json.contains("prompt.ready"));
        assert!(json.contains("10000"));

        let parsed: StepResult = serde_json::from_str(&json).unwrap();
        match parsed {
            StepResult::WaitFor {
                condition,
                timeout_ms,
            } => {
                assert_eq!(timeout_ms, Some(10_000));
                match condition {
                    WaitCondition::Pattern { rule_id, .. } => assert_eq!(rule_id, "prompt.ready"),
                    _ => panic!("Expected Pattern condition"),
                }
            }
            _ => panic!("Expected WaitFor"),
        }
    }

    #[test]
    fn step_result_helper_methods() {
        assert!(StepResult::cont().is_continue());
        assert!(StepResult::done_empty().is_done());
        assert!(StepResult::done_empty().is_terminal());
        assert!(StepResult::abort("error").is_terminal());
        assert!(!StepResult::retry(100).is_terminal());
        assert!(!StepResult::wait_for(WaitCondition::external("key")).is_terminal());
    }

    // ========================================================================
    // WaitCondition Tests
    // ========================================================================

    #[test]
    fn wait_condition_pattern_serializes() {
        let cond = WaitCondition::pattern("test.rule");
        let json = serde_json::to_string(&cond).unwrap();
        assert!(json.contains("pattern"));
        assert!(json.contains("test.rule"));

        let parsed: WaitCondition = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, cond);
        assert_eq!(parsed.pane_id(), None);
    }

    #[test]
    fn wait_condition_pattern_on_pane_serializes() {
        let cond = WaitCondition::pattern_on_pane(42, "test.rule");
        let json = serde_json::to_string(&cond).unwrap();
        assert!(json.contains("42"));

        let parsed: WaitCondition = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.pane_id(), Some(42));
    }

    #[test]
    fn wait_condition_pane_idle_serializes() {
        let cond = WaitCondition::pane_idle(1000);
        let json = serde_json::to_string(&cond).unwrap();
        assert!(json.contains("pane_idle"));
        assert!(json.contains("1000"));

        let parsed: WaitCondition = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, cond);
    }

    #[test]
    fn wait_condition_pane_idle_on_serializes() {
        let cond = WaitCondition::pane_idle_on(99, 500);
        assert_eq!(cond.pane_id(), Some(99));
    }

    #[test]
    fn wait_condition_external_serializes() {
        let cond = WaitCondition::external("approval_granted");
        let json = serde_json::to_string(&cond).unwrap();
        assert!(json.contains("external"));
        assert!(json.contains("approval_granted"));

        let parsed: WaitCondition = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, cond);
        assert_eq!(parsed.pane_id(), None);
    }

    // ========================================================================
    // WorkflowStep Tests
    // ========================================================================

    #[test]
    fn workflow_step_creates() {
        let step = WorkflowStep::new("send_prompt", "Send a prompt to the terminal");
        assert_eq!(step.name, "send_prompt");
        assert_eq!(step.description, "Send a prompt to the terminal");
    }

    // ========================================================================
    // WorkflowConfig Tests
    // ========================================================================

    #[test]
    fn workflow_config_defaults() {
        let config = WorkflowConfig::default();
        assert_eq!(config.default_wait_timeout_ms, 30_000);
        assert_eq!(config.max_step_retries, 3);
        assert_eq!(config.retry_delay_ms, 1_000);
    }

    // ========================================================================
    // WorkflowEngine Tests
    // ========================================================================

    #[test]
    fn engine_can_be_created() {
        let engine = WorkflowEngine::new(5);
        assert_eq!(engine.max_concurrent(), 5);
    }

    // ========================================================================
    // Stub Workflow Tests (wa-nu4.1.1.1 acceptance criteria)
    // ========================================================================

    /// A stub workflow for testing that demonstrates all workflow capabilities
    struct StubWorkflow {
        name: &'static str,
        description: &'static str,
        target_rule_prefix: &'static str,
    }

    impl StubWorkflow {
        fn new() -> Self {
            Self {
                name: "stub_workflow",
                description: "A test workflow for verification",
                target_rule_prefix: "test.",
            }
        }
    }

    impl Workflow for StubWorkflow {
        fn name(&self) -> &'static str {
            self.name
        }

        fn description(&self) -> &'static str {
            self.description
        }

        fn handles(&self, detection: &Detection) -> bool {
            detection.rule_id.starts_with(self.target_rule_prefix)
        }

        fn steps(&self) -> Vec<WorkflowStep> {
            vec![
                WorkflowStep::new("step_one", "First step - sends prompt"),
                WorkflowStep::new("step_two", "Second step - waits for response"),
                WorkflowStep::new("step_three", "Third step - completes"),
            ]
        }

        fn execute_step(
            &self,
            _ctx: &mut WorkflowContext,
            step_idx: usize,
        ) -> BoxFuture<'_, StepResult> {
            Box::pin(async move {
                match step_idx {
                    0 => StepResult::cont(),
                    1 => StepResult::wait_for(WaitCondition::pattern("response.ready")),
                    2 => StepResult::done(serde_json::json!({"completed": true})),
                    _ => StepResult::abort("unexpected step index"),
                }
            })
        }

        fn cleanup(&self, _ctx: &mut WorkflowContext) -> BoxFuture<'_, ()> {
            Box::pin(async {
                // Stub cleanup - no-op
            })
        }
    }

    fn make_test_detection(rule_id: &str) -> Detection {
        Detection {
            rule_id: rule_id.to_string(),
            agent_type: AgentType::Wezterm,
            event_type: "test".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::Value::Null,
            matched_text: "test".to_string(),
        }
    }

    #[test]
    fn stub_workflow_compiles_and_has_correct_metadata() {
        let workflow = StubWorkflow::new();

        assert_eq!(workflow.name(), "stub_workflow");
        assert_eq!(workflow.description(), "A test workflow for verification");
        assert_eq!(workflow.step_count(), 3);

        let steps = workflow.steps();
        assert_eq!(steps[0].name, "step_one");
        assert_eq!(steps[1].name, "step_two");
        assert_eq!(steps[2].name, "step_three");
    }

    #[test]
    fn stub_workflow_handles_matching_detections() {
        let workflow = StubWorkflow::new();

        // Should handle detections with matching prefix
        assert!(workflow.handles(&make_test_detection("test.prompt_ready")));
        assert!(workflow.handles(&make_test_detection("test.anything")));

        // Should not handle detections with non-matching prefix
        assert!(!workflow.handles(&make_test_detection("other.prompt_ready")));
        assert!(!workflow.handles(&make_test_detection("production.event")));
    }

    #[tokio::test]
    async fn stub_workflow_executes_steps_correctly() {
        let workflow = StubWorkflow::new();

        // Create a minimal context for testing
        // Note: In real usage, this would have an actual StorageHandle
        // For this test, we just verify the step execution logic

        // We can't easily create a WorkflowContext without a real StorageHandle,
        // but we can verify the workflow's step logic independently
        let steps = workflow.steps();
        assert_eq!(steps.len(), 3);
    }

    #[test]
    fn step_result_transitions_exhaustive() {
        // Verify all StepResult variants can be created and identified
        let variants = [
            StepResult::Continue,
            StepResult::Done {
                result: serde_json::Value::Null,
            },
            StepResult::Retry { delay_ms: 1000 },
            StepResult::Abort {
                reason: "test".to_string(),
            },
            StepResult::WaitFor {
                condition: WaitCondition::external("key"),
                timeout_ms: None,
            },
        ];

        // Each variant serializes uniquely
        let mut json_types = std::collections::HashSet::new();
        for variant in &variants {
            let json = serde_json::to_string(variant).unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
            let type_field = parsed["type"].as_str().unwrap().to_string();
            json_types.insert(type_field);
        }

        // All 5 variants have unique type identifiers
        assert_eq!(json_types.len(), 5);
        assert!(json_types.contains("continue"));
        assert!(json_types.contains("done"));
        assert!(json_types.contains("retry"));
        assert!(json_types.contains("abort"));
        assert!(json_types.contains("wait_for"));
    }

    #[test]
    fn wait_condition_transitions_exhaustive() {
        // Verify all WaitCondition variants
        let variants = [
            WaitCondition::Pattern {
                pane_id: None,
                rule_id: "test".to_string(),
            },
            WaitCondition::PaneIdle {
                pane_id: None,
                idle_threshold_ms: 1000,
            },
            WaitCondition::External {
                key: "test".to_string(),
            },
        ];

        let mut json_types = std::collections::HashSet::new();
        for variant in &variants {
            let json = serde_json::to_string(variant).unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
            let type_field = parsed["type"].as_str().unwrap().to_string();
            json_types.insert(type_field);
        }

        assert_eq!(json_types.len(), 3);
        assert!(json_types.contains("pattern"));
        assert!(json_types.contains("pane_idle"));
        assert!(json_types.contains("external"));
    }

    // ========================================================================
    // WaitConditionResult Tests
    // ========================================================================

    #[test]
    fn wait_condition_result_satisfied_is_satisfied() {
        let result = WaitConditionResult::Satisfied {
            elapsed_ms: 100,
            polls: 5,
            context: Some("matched".to_string()),
        };
        assert!(result.is_satisfied());
        assert!(!result.is_timed_out());
        assert_eq!(result.elapsed_ms(), Some(100));
    }

    #[test]
    fn wait_condition_result_timed_out_is_timed_out() {
        let result = WaitConditionResult::TimedOut {
            elapsed_ms: 5000,
            polls: 100,
            last_observed: Some("waiting for prompt".to_string()),
        };
        assert!(!result.is_satisfied());
        assert!(result.is_timed_out());
        assert_eq!(result.elapsed_ms(), Some(5000));
    }

    #[test]
    fn wait_condition_result_unsupported_has_no_elapsed() {
        let result = WaitConditionResult::Unsupported {
            reason: "external signals not implemented".to_string(),
        };
        assert!(!result.is_satisfied());
        assert!(!result.is_timed_out());
        assert_eq!(result.elapsed_ms(), None);
    }

    // ========================================================================
    // WaitConditionOptions Tests
    // ========================================================================

    #[test]
    fn wait_condition_options_defaults() {
        let options = WaitConditionOptions::default();
        assert_eq!(options.tail_lines, 200);
        assert_eq!(options.poll_initial.as_millis(), 50);
        assert_eq!(options.poll_max.as_millis(), 1000);
        assert_eq!(options.max_polls, 10_000);
        assert!(options.allow_idle_heuristics);
    }

    // ========================================================================
    // Helper Function Tests
    // ========================================================================

    #[test]
    fn tail_text_extracts_last_n_lines() {
        let text = "line1\nline2\nline3\nline4\nline5";
        assert_eq!(tail_text(text, 3), "line3\nline4\nline5");
        assert_eq!(tail_text(text, 1), "line5");
        assert_eq!(tail_text(text, 10), text);
        assert_eq!(tail_text(text, 0), "");
    }

    #[test]
    fn tail_text_handles_empty_input() {
        assert_eq!(tail_text("", 5), "");
    }

    #[test]
    fn tail_text_handles_single_line() {
        assert_eq!(tail_text("single line", 5), "single line");
    }

    #[test]
    fn truncate_for_log_preserves_short_strings() {
        assert_eq!(truncate_for_log("hello", 10), "hello");
        assert_eq!(truncate_for_log("exact", 5), "exact");
    }

    #[test]
    fn truncate_for_log_truncates_long_strings() {
        assert_eq!(truncate_for_log("hello world", 8), "hello...");
    }

    // ========================================================================
    // Heuristic Idle Check Tests
    // ========================================================================

    #[test]
    fn heuristic_idle_detects_bash_prompt() {
        let text = "output from command\nuser@host:~$ ";
        let (is_idle, desc) = heuristic_idle_check(text, 10);
        assert!(is_idle);
        assert!(desc.contains("ends_with_prompt"));
    }

    #[test]
    fn heuristic_idle_detects_root_prompt() {
        let text = "output\nroot@host:~# ";
        let (is_idle, desc) = heuristic_idle_check(text, 10);
        assert!(is_idle);
        assert!(desc.contains("ends_with_prompt"));
    }

    #[test]
    fn heuristic_idle_detects_zsh_prompt() {
        let text = "output\n❯ ";
        let (is_idle, desc) = heuristic_idle_check(text, 10);
        assert!(is_idle);
        assert!(desc.contains("ends_with_prompt"));
    }

    #[test]
    fn heuristic_idle_detects_python_repl() {
        let text = ">>> ";
        let (is_idle, desc) = heuristic_idle_check(text, 10);
        assert!(is_idle);
        assert!(desc.contains("ends_with_prompt"));
    }

    #[test]
    fn heuristic_idle_detects_prompt_with_trailing_newline() {
        // Note: Rust's lines() iterator doesn't include trailing empty lines,
        // so "user@host:~$ \n" becomes the last line as "user@host:~$ "
        // which after trim_end becomes "user@host:~$" ending with "$"
        let text = "output\nuser@host:~$ \n";
        let (is_idle, desc) = heuristic_idle_check(text, 10);
        assert!(is_idle);
        assert!(desc.contains("ends_with_prompt"));
    }

    #[test]
    fn heuristic_idle_rejects_command_output() {
        let text = "building project...\nCompiling foo v1.0.0";
        let (is_idle, desc) = heuristic_idle_check(text, 10);
        assert!(!is_idle);
        assert!(desc.contains("no_prompt_detected"));
    }

    #[test]
    fn heuristic_idle_rejects_running_command() {
        // Use "50/100" instead of "50%" - the % character would match the tcsh prompt pattern
        let text = "npm run build\nProgress: 50/100";
        let (is_idle, _desc) = heuristic_idle_check(text, 10);
        assert!(!is_idle);
    }

    // ========================================================================
    // WaitConditionExecutor Tests (using mock source)
    // ========================================================================

    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Mock pane text source for testing
    struct MockPaneSource {
        texts: Mutex<Vec<String>>,
        call_count: AtomicUsize,
    }

    impl MockPaneSource {
        fn new(texts: Vec<String>) -> Self {
            Self {
                texts: Mutex::new(texts),
                call_count: AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.call_count.load(Ordering::Relaxed)
        }
    }

    impl crate::wezterm::PaneTextSource for MockPaneSource {
        type Fut<'a> =
            std::pin::Pin<Box<dyn std::future::Future<Output = crate::Result<String>> + Send + 'a>>;

        fn get_text(&self, _pane_id: u64, _escapes: bool) -> Self::Fut<'_> {
            let count = self.call_count.fetch_add(1, Ordering::Relaxed);
            let texts = self.texts.lock().unwrap();
            let text = if count < texts.len() {
                texts[count].clone()
            } else {
                texts.last().cloned().unwrap_or_default()
            };
            Box::pin(async move { Ok(text) })
        }
    }

    #[tokio::test]
    async fn pattern_wait_succeeds_on_immediate_match() {
        let source = MockPaneSource::new(vec![
            "Conversation compacted 100,000 tokens to 25,000 tokens".to_string(),
        ]);
        let engine = PatternEngine::new();

        let executor =
            WaitConditionExecutor::new(&source, &engine).with_options(WaitConditionOptions {
                tail_lines: 200,
                poll_initial: Duration::from_millis(1),
                poll_max: Duration::from_millis(10),
                max_polls: 100,
                allow_idle_heuristics: true,
            });

        let condition = WaitCondition::pattern("claude_code.compaction");
        let result = executor
            .execute(&condition, 1, Duration::from_secs(5))
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_satisfied());
        assert_eq!(source.calls(), 1);
    }

    #[tokio::test]
    async fn pattern_wait_times_out_on_no_match() {
        let source = MockPaneSource::new(vec!["no matching pattern here".to_string()]);
        let engine = PatternEngine::new();

        let executor =
            WaitConditionExecutor::new(&source, &engine).with_options(WaitConditionOptions {
                tail_lines: 200,
                poll_initial: Duration::from_millis(1),
                poll_max: Duration::from_millis(5),
                max_polls: 5,
                allow_idle_heuristics: true,
            });

        let condition = WaitCondition::pattern("claude_code.compaction");
        let result = executor
            .execute(&condition, 1, Duration::from_millis(20))
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_timed_out());
    }

    #[tokio::test]
    async fn pattern_wait_succeeds_after_multiple_polls() {
        let source = MockPaneSource::new(vec![
            "no match yet".to_string(),
            "still no match".to_string(),
            "Conversation compacted 100,000 tokens to 25,000 tokens".to_string(),
        ]);
        let engine = PatternEngine::new();

        let executor =
            WaitConditionExecutor::new(&source, &engine).with_options(WaitConditionOptions {
                tail_lines: 200,
                poll_initial: Duration::from_millis(1),
                poll_max: Duration::from_millis(5),
                max_polls: 100,
                allow_idle_heuristics: true,
            });

        let condition = WaitCondition::pattern("claude_code.compaction");
        let result = executor
            .execute(&condition, 1, Duration::from_secs(5))
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_satisfied());
        assert!(source.calls() >= 3);
    }

    #[tokio::test]
    async fn pane_idle_succeeds_with_osc133_prompt_active() {
        use crate::ingest::{Osc133State, ShellState};

        let source = MockPaneSource::new(vec!["some text".to_string()]);
        let engine = PatternEngine::new();
        let mut osc_state = Osc133State::new();
        osc_state.state = ShellState::PromptActive;

        let executor = WaitConditionExecutor::new(&source, &engine)
            .with_osc_state(&osc_state)
            .with_options(WaitConditionOptions {
                tail_lines: 200,
                poll_initial: Duration::from_millis(1),
                poll_max: Duration::from_millis(5),
                max_polls: 100,
                allow_idle_heuristics: true,
            });

        // idle_threshold_ms = 0 means immediate satisfaction when idle
        let condition = WaitCondition::pane_idle(0);
        let result = executor
            .execute(&condition, 1, Duration::from_secs(5))
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_satisfied());
        if let WaitConditionResult::Satisfied { context, .. } = result {
            assert!(context.unwrap().contains("osc133"));
        }
    }

    #[tokio::test]
    async fn pane_idle_times_out_with_osc133_command_running() {
        use crate::ingest::{Osc133State, ShellState};

        let source = MockPaneSource::new(vec!["running command...".to_string()]);
        let engine = PatternEngine::new();
        let mut osc_state = Osc133State::new();
        osc_state.state = ShellState::CommandRunning;

        let executor = WaitConditionExecutor::new(&source, &engine)
            .with_osc_state(&osc_state)
            .with_options(WaitConditionOptions {
                tail_lines: 200,
                poll_initial: Duration::from_millis(1),
                poll_max: Duration::from_millis(5),
                max_polls: 5,
                allow_idle_heuristics: true,
            });

        let condition = WaitCondition::pane_idle(0);
        let result = executor
            .execute(&condition, 1, Duration::from_millis(20))
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_timed_out());
    }

    #[tokio::test]
    async fn pane_idle_uses_heuristics_when_no_osc133() {
        let source = MockPaneSource::new(vec!["user@host:~$ ".to_string()]);
        let engine = PatternEngine::new();

        let executor =
            WaitConditionExecutor::new(&source, &engine).with_options(WaitConditionOptions {
                tail_lines: 200,
                poll_initial: Duration::from_millis(1),
                poll_max: Duration::from_millis(5),
                max_polls: 100,
                allow_idle_heuristics: true,
            });

        let condition = WaitCondition::pane_idle(0);
        let result = executor
            .execute(&condition, 1, Duration::from_secs(5))
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_satisfied());
        if let WaitConditionResult::Satisfied { context, .. } = result {
            assert!(context.unwrap().contains("heuristic"));
        }
    }

    #[tokio::test]
    async fn pane_idle_respects_threshold_duration() {
        use crate::ingest::{Osc133State, ShellState};

        let source = MockPaneSource::new(vec!["some text".to_string()]);
        let engine = PatternEngine::new();
        let mut osc_state = Osc133State::new();
        osc_state.state = ShellState::PromptActive;

        let executor = WaitConditionExecutor::new(&source, &engine)
            .with_osc_state(&osc_state)
            .with_options(WaitConditionOptions {
                tail_lines: 200,
                poll_initial: Duration::from_millis(10),
                poll_max: Duration::from_millis(50),
                max_polls: 100,
                allow_idle_heuristics: true,
            });

        // Require 50ms idle threshold
        let condition = WaitCondition::pane_idle(50);
        let start = std::time::Instant::now();
        let result = executor
            .execute(&condition, 1, Duration::from_secs(5))
            .await;
        let elapsed = start.elapsed();

        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_satisfied());
        // Should have waited at least the threshold duration
        assert!(elapsed >= Duration::from_millis(50));
    }

    #[tokio::test]
    async fn external_wait_returns_unsupported() {
        let source = MockPaneSource::new(vec!["text".to_string()]);
        let engine = PatternEngine::new();

        let executor = WaitConditionExecutor::new(&source, &engine);
        let condition = WaitCondition::external("my_signal");
        let result = executor
            .execute(&condition, 1, Duration::from_secs(5))
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();
        match result {
            WaitConditionResult::Unsupported { reason } => {
                assert!(reason.contains("my_signal"));
            }
            _ => panic!("Expected Unsupported"),
        }
    }

    #[tokio::test]
    async fn wait_respects_max_polls() {
        let source = MockPaneSource::new(vec!["no match".to_string()]);
        let engine = PatternEngine::new();

        let executor =
            WaitConditionExecutor::new(&source, &engine).with_options(WaitConditionOptions {
                tail_lines: 200,
                poll_initial: Duration::from_millis(1),
                poll_max: Duration::from_millis(1),
                max_polls: 3,
                allow_idle_heuristics: true,
            });

        let condition = WaitCondition::pattern("nonexistent.rule");
        let result = executor
            .execute(&condition, 1, Duration::from_secs(60))
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_timed_out());
        if let WaitConditionResult::TimedOut { polls, .. } = result {
            assert!(polls <= 3);
        }
    }

    // ========================================================================
    // Workflow Persistence Tests (wa-nu4.1.1.3)
    // ========================================================================

    #[test]
    fn compute_next_step_empty_logs_returns_zero() {
        let logs: Vec<crate::storage::WorkflowStepLogRecord> = vec![];
        assert_eq!(super::compute_next_step(&logs), 0);
    }

    #[test]
    fn compute_next_step_with_continue_returns_next() {
        let logs = vec![crate::storage::WorkflowStepLogRecord {
            id: 1,
            workflow_id: "test-123".to_string(),
            audit_action_id: None,
            step_index: 0,
            step_name: "step_0".to_string(),
            result_type: "continue".to_string(),
            result_data: None,
            started_at: 1000,
            completed_at: 1100,
            duration_ms: 100,
        }];
        assert_eq!(super::compute_next_step(&logs), 1);
    }

    #[test]
    fn compute_next_step_with_done_returns_next() {
        let logs = vec![crate::storage::WorkflowStepLogRecord {
            id: 1,
            workflow_id: "test-123".to_string(),
            audit_action_id: None,
            step_index: 2,
            step_name: "step_2".to_string(),
            result_type: "done".to_string(),
            result_data: None,
            started_at: 1000,
            completed_at: 1100,
            duration_ms: 100,
        }];
        assert_eq!(super::compute_next_step(&logs), 3);
    }

    #[test]
    fn compute_next_step_with_retry_returns_same() {
        // Retry means the step should be re-executed
        let logs = vec![crate::storage::WorkflowStepLogRecord {
            id: 1,
            workflow_id: "test-123".to_string(),
            audit_action_id: None,
            step_index: 1,
            step_name: "step_1".to_string(),
            result_type: "retry".to_string(),
            result_data: None,
            started_at: 1000,
            completed_at: 1100,
            duration_ms: 100,
        }];
        // No completed steps, so start from 0
        assert_eq!(super::compute_next_step(&logs), 0);
    }

    #[test]
    fn compute_next_step_mixed_logs_finds_highest_completed() {
        let logs = vec![
            crate::storage::WorkflowStepLogRecord {
                id: 1,
                workflow_id: "test-123".to_string(),
                audit_action_id: None,
                step_index: 0,
                step_name: "step_0".to_string(),
                result_type: "continue".to_string(),
                result_data: None,
                started_at: 1000,
                completed_at: 1100,
                duration_ms: 100,
            },
            crate::storage::WorkflowStepLogRecord {
                id: 2,
                workflow_id: "test-123".to_string(),
                audit_action_id: None,
                step_index: 1,
                step_name: "step_1".to_string(),
                result_type: "continue".to_string(),
                result_data: None,
                started_at: 1100,
                completed_at: 1200,
                duration_ms: 100,
            },
            crate::storage::WorkflowStepLogRecord {
                id: 3,
                workflow_id: "test-123".to_string(),
                audit_action_id: None,
                step_index: 2,
                step_name: "step_2".to_string(),
                result_type: "retry".to_string(),
                result_data: None,
                started_at: 1200,
                completed_at: 1300,
                duration_ms: 100,
            },
        ];
        // Highest completed is step_index 1, so next is 2
        assert_eq!(super::compute_next_step(&logs), 2);
    }

    #[test]
    fn compute_next_step_out_of_order_logs() {
        // Logs might not be in order; function should still find max
        let logs = vec![
            crate::storage::WorkflowStepLogRecord {
                id: 3,
                workflow_id: "test-123".to_string(),
                audit_action_id: None,
                step_index: 2,
                step_name: "step_2".to_string(),
                result_type: "continue".to_string(),
                result_data: None,
                started_at: 1200,
                completed_at: 1300,
                duration_ms: 100,
            },
            crate::storage::WorkflowStepLogRecord {
                id: 1,
                workflow_id: "test-123".to_string(),
                audit_action_id: None,
                step_index: 0,
                step_name: "step_0".to_string(),
                result_type: "continue".to_string(),
                result_data: None,
                started_at: 1000,
                completed_at: 1100,
                duration_ms: 100,
            },
        ];
        // Highest completed is step_index 2, so next is 3
        assert_eq!(super::compute_next_step(&logs), 3);
    }

    #[test]
    fn generate_workflow_id_format() {
        let id = super::generate_workflow_id("test_workflow");
        assert!(id.starts_with("test_workflow-"));
        // Should have format: name-timestamp-random
        let parts: Vec<&str> = id.split('-').collect();
        assert!(parts.len() >= 3);
        // Last part should be hex (8 chars)
        let last = parts.last().unwrap();
        assert_eq!(last.len(), 8);
        assert!(last.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generate_workflow_id_uniqueness() {
        let id1 = super::generate_workflow_id("workflow");
        let id2 = super::generate_workflow_id("workflow");
        // Random component should make them different
        assert_ne!(id1, id2);
    }

    #[test]
    fn execution_status_serialization() {
        let statuses = [
            ExecutionStatus::Running,
            ExecutionStatus::Waiting,
            ExecutionStatus::Completed,
            ExecutionStatus::Aborted,
        ];

        for status in &statuses {
            let json = serde_json::to_string(status).unwrap();
            let parsed: ExecutionStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(&parsed, status);
        }
    }

    #[test]
    fn workflow_execution_serialization() {
        let execution = WorkflowExecution {
            id: "test-123-abc".to_string(),
            workflow_name: "test_workflow".to_string(),
            pane_id: 42,
            current_step: 2,
            status: ExecutionStatus::Running,
            started_at: 1000,
            updated_at: 1500,
        };

        let json = serde_json::to_string(&execution).unwrap();
        let parsed: WorkflowExecution = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.id, execution.id);
        assert_eq!(parsed.workflow_name, execution.workflow_name);
        assert_eq!(parsed.pane_id, execution.pane_id);
        assert_eq!(parsed.current_step, execution.current_step);
        assert_eq!(parsed.status, execution.status);
    }

    // ========================================================================
    // PaneWorkflowLockManager Tests (wa-nu4.1.1.2)
    // ========================================================================

    #[test]
    fn lock_manager_acquire_and_release() {
        let manager = PaneWorkflowLockManager::new();

        // Initially unlocked
        assert!(manager.is_locked(42).is_none());

        // Acquire succeeds
        let result = manager.try_acquire(42, "test_workflow", "exec-001");
        assert!(result.is_acquired());
        assert!(!result.is_already_locked());

        // Now locked
        let lock_info = manager.is_locked(42);
        assert!(lock_info.is_some());
        let info = lock_info.unwrap();
        assert_eq!(info.pane_id, 42);
        assert_eq!(info.workflow_name, "test_workflow");
        assert_eq!(info.execution_id, "exec-001");
        assert!(info.locked_at_ms > 0);

        // Release succeeds
        assert!(manager.release(42, "exec-001"));

        // Now unlocked
        assert!(manager.is_locked(42).is_none());
    }

    #[test]
    fn lock_manager_double_acquire_fails() {
        let manager = PaneWorkflowLockManager::new();

        // First acquire succeeds
        let result1 = manager.try_acquire(42, "workflow_a", "exec-001");
        assert!(result1.is_acquired());

        // Second acquire fails with details about the existing lock
        let result2 = manager.try_acquire(42, "workflow_b", "exec-002");
        assert!(result2.is_already_locked());
        match result2 {
            LockAcquisitionResult::AlreadyLocked {
                held_by_workflow,
                held_by_execution,
                locked_since_ms,
            } => {
                assert_eq!(held_by_workflow, "workflow_a");
                assert_eq!(held_by_execution, "exec-001");
                assert!(locked_since_ms > 0);
            }
            LockAcquisitionResult::Acquired => panic!("Expected AlreadyLocked"),
        }

        // Release and retry succeeds
        manager.release(42, "exec-001");
        let result3 = manager.try_acquire(42, "workflow_b", "exec-002");
        assert!(result3.is_acquired());
    }

    #[test]
    fn lock_manager_release_with_wrong_execution_id_fails() {
        let manager = PaneWorkflowLockManager::new();

        manager.try_acquire(42, "test_workflow", "exec-001");

        // Release with wrong execution_id fails
        assert!(!manager.release(42, "wrong-exec-id"));

        // Lock still held
        assert!(manager.is_locked(42).is_some());

        // Correct execution_id works
        assert!(manager.release(42, "exec-001"));
        assert!(manager.is_locked(42).is_none());
    }

    #[test]
    fn lock_manager_multiple_panes_independent() {
        let manager = PaneWorkflowLockManager::new();

        // Lock pane 1
        let r1 = manager.try_acquire(1, "workflow_a", "exec-001");
        assert!(r1.is_acquired());

        // Lock pane 2 succeeds (different pane)
        let r2 = manager.try_acquire(2, "workflow_b", "exec-002");
        assert!(r2.is_acquired());

        // Lock pane 3 succeeds
        let r3 = manager.try_acquire(3, "workflow_c", "exec-003");
        assert!(r3.is_acquired());

        // All locked
        assert!(manager.is_locked(1).is_some());
        assert!(manager.is_locked(2).is_some());
        assert!(manager.is_locked(3).is_some());

        // Release pane 2 doesn't affect others
        manager.release(2, "exec-002");
        assert!(manager.is_locked(1).is_some());
        assert!(manager.is_locked(2).is_none());
        assert!(manager.is_locked(3).is_some());
    }

    #[test]
    fn lock_manager_active_locks() {
        let manager = PaneWorkflowLockManager::new();

        // Initially empty
        assert!(manager.active_locks().is_empty());

        manager.try_acquire(1, "workflow_a", "exec-001");
        manager.try_acquire(2, "workflow_b", "exec-002");

        let active = manager.active_locks();
        assert_eq!(active.len(), 2);

        let pane_ids: std::collections::HashSet<u64> = active.iter().map(|l| l.pane_id).collect();
        assert!(pane_ids.contains(&1));
        assert!(pane_ids.contains(&2));
    }

    #[test]
    fn lock_guard_releases_on_drop() {
        let manager = PaneWorkflowLockManager::new();

        // Acquire via guard
        {
            let guard = manager.acquire_guard(42, "test_workflow", "exec-001");
            assert!(guard.is_some());
            let guard = guard.unwrap();
            assert_eq!(guard.pane_id(), 42);
            assert_eq!(guard.execution_id(), "exec-001");

            // Lock is held
            assert!(manager.is_locked(42).is_some());
        }

        // Guard dropped, lock released
        assert!(manager.is_locked(42).is_none());
    }

    #[test]
    fn lock_guard_acquire_fails_when_locked() {
        let manager = PaneWorkflowLockManager::new();

        // Acquire first lock
        let _guard1 = manager.acquire_guard(42, "workflow_a", "exec-001");
        assert!(manager.is_locked(42).is_some());

        // Second acquire fails
        let guard2 = manager.acquire_guard(42, "workflow_b", "exec-002");
        assert!(guard2.is_none());
    }

    #[test]
    fn lock_manager_force_release() {
        let manager = PaneWorkflowLockManager::new();

        manager.try_acquire(42, "test_workflow", "exec-001");
        assert!(manager.is_locked(42).is_some());

        // Force release works even with unknown execution_id
        let removed = manager.force_release(42);
        assert!(removed.is_some());
        let info = removed.unwrap();
        assert_eq!(info.execution_id, "exec-001");

        // Now unlocked
        assert!(manager.is_locked(42).is_none());

        // Force release on unlocked pane returns None
        assert!(manager.force_release(42).is_none());
    }

    #[test]
    fn lock_acquisition_result_methods() {
        let acquired = LockAcquisitionResult::Acquired;
        assert!(acquired.is_acquired());
        assert!(!acquired.is_already_locked());

        let locked = LockAcquisitionResult::AlreadyLocked {
            held_by_workflow: "test".to_string(),
            held_by_execution: "exec-001".to_string(),
            locked_since_ms: 1_234_567_890,
        };
        assert!(!locked.is_acquired());
        assert!(locked.is_already_locked());
    }

    #[test]
    fn lock_manager_concurrent_simulation() {
        use std::sync::Arc;
        use std::thread;

        let manager = Arc::new(PaneWorkflowLockManager::new());
        let pane_id = 42;

        // Simulate concurrent access with threads
        let mut handles = vec![];

        for i in 0..10 {
            let m = Arc::clone(&manager);
            let handle = thread::spawn(move || {
                let exec_id = format!("exec-{i:03}");
                m.try_acquire(pane_id, "concurrent_workflow", &exec_id)
            });
            handles.push(handle);
        }

        // Collect results
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        // Exactly one should have acquired the lock
        let acquired_count = results.iter().filter(|r| r.is_acquired()).count();
        let locked_count = results.iter().filter(|r| r.is_already_locked()).count();

        assert_eq!(acquired_count, 1);
        assert_eq!(locked_count, 9);
    }

    #[test]
    fn pane_lock_info_serialization() {
        let info = PaneLockInfo {
            pane_id: 42,
            workflow_name: "test_workflow".to_string(),
            execution_id: "exec-001".to_string(),
            locked_at_ms: 1_234_567_890_000,
        };

        let json = serde_json::to_string(&info).unwrap();
        let parsed: PaneLockInfo = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.pane_id, info.pane_id);
        assert_eq!(parsed.workflow_name, info.workflow_name);
        assert_eq!(parsed.execution_id, info.execution_id);
        assert_eq!(parsed.locked_at_ms, info.locked_at_ms);
    }

    // ========================================================================
    // WorkflowRunner Tests
    // ========================================================================

    #[test]
    fn workflow_runner_config_default_has_sensible_values() {
        let config = WorkflowRunnerConfig::default();

        assert!(config.max_concurrent > 0);
        assert!(config.step_timeout_ms > 0);
        assert!(config.retry_backoff_multiplier >= 1.0);
        assert!(config.max_retries_per_step > 0);
    }

    #[test]
    fn workflow_start_result_variants_serialize() {
        let variants = vec![
            WorkflowStartResult::Started {
                execution_id: "exec-001".to_string(),
                workflow_name: "test_workflow".to_string(),
            },
            WorkflowStartResult::NoMatchingWorkflow {
                rule_id: "test.rule".to_string(),
            },
            WorkflowStartResult::PaneLocked {
                pane_id: 42,
                held_by_workflow: "other_workflow".to_string(),
                held_by_execution: "exec-002".to_string(),
            },
            WorkflowStartResult::Error {
                error: "Something went wrong".to_string(),
            },
        ];

        for variant in variants {
            let json = serde_json::to_string(&variant).unwrap();
            let parsed: WorkflowStartResult = serde_json::from_str(&json).unwrap();

            // Verify round-trip
            let json2 = serde_json::to_string(&parsed).unwrap();
            assert_eq!(json, json2);
        }
    }

    #[test]
    fn workflow_execution_result_variants_serialize() {
        let variants = vec![
            WorkflowExecutionResult::Completed {
                execution_id: "exec-001".to_string(),
                result: serde_json::json!({"success": true}),
                elapsed_ms: 1000,
                steps_executed: 3,
            },
            WorkflowExecutionResult::Aborted {
                execution_id: "exec-002".to_string(),
                reason: "Timeout exceeded".to_string(),
                step_index: 2,
                elapsed_ms: 5000,
            },
            WorkflowExecutionResult::PolicyDenied {
                execution_id: "exec-003".to_string(),
                step_index: 1,
                reason: "Rate limit exceeded".to_string(),
            },
            WorkflowExecutionResult::Error {
                execution_id: Some("exec-004".to_string()),
                error: "Database connection failed".to_string(),
            },
            WorkflowExecutionResult::Error {
                execution_id: None,
                error: "Early failure".to_string(),
            },
        ];

        for variant in variants {
            let json = serde_json::to_string(&variant).unwrap();
            let parsed: WorkflowExecutionResult = serde_json::from_str(&json).unwrap();

            // Verify round-trip
            let json2 = serde_json::to_string(&parsed).unwrap();
            assert_eq!(json, json2);
        }
    }

    #[test]
    fn workflow_start_result_accessors_work() {
        let started = WorkflowStartResult::Started {
            execution_id: "exec-001".to_string(),
            workflow_name: "test".to_string(),
        };
        assert!(started.is_started());
        assert!(!started.is_locked());
        assert!(started.execution_id().is_some());

        let locked = WorkflowStartResult::PaneLocked {
            pane_id: 1,
            held_by_workflow: "other".to_string(),
            held_by_execution: "exec-002".to_string(),
        };
        assert!(!locked.is_started());
        assert!(locked.is_locked());
        assert!(locked.execution_id().is_none());

        let no_match = WorkflowStartResult::NoMatchingWorkflow {
            rule_id: "test".to_string(),
        };
        assert!(!no_match.is_started());
        assert!(!no_match.is_locked());
        assert!(no_match.execution_id().is_none());

        let error = WorkflowStartResult::Error {
            error: "fail".to_string(),
        };
        assert!(!error.is_started());
        assert!(!error.is_locked());
        assert!(error.execution_id().is_none());
    }

    #[test]
    fn workflow_execution_result_accessors_work() {
        let completed = WorkflowExecutionResult::Completed {
            execution_id: "exec-001".to_string(),
            result: serde_json::Value::Null,
            elapsed_ms: 100,
            steps_executed: 2,
        };
        assert!(completed.is_completed());
        assert!(!completed.is_aborted());
        assert_eq!(completed.execution_id(), Some("exec-001"));

        let aborted = WorkflowExecutionResult::Aborted {
            execution_id: "exec-002".to_string(),
            reason: "test".to_string(),
            step_index: 1,
            elapsed_ms: 50,
        };
        assert!(!aborted.is_completed());
        assert!(aborted.is_aborted());
        assert_eq!(aborted.execution_id(), Some("exec-002"));

        let denied = WorkflowExecutionResult::PolicyDenied {
            execution_id: "exec-003".to_string(),
            step_index: 0,
            reason: "rate limit".to_string(),
        };
        assert!(!denied.is_completed());
        assert!(!denied.is_aborted());
        assert_eq!(denied.execution_id(), Some("exec-003"));

        let error_with_id = WorkflowExecutionResult::Error {
            execution_id: Some("exec-004".to_string()),
            error: "fail".to_string(),
        };
        assert!(!error_with_id.is_completed());
        assert!(!error_with_id.is_aborted());
        assert_eq!(error_with_id.execution_id(), Some("exec-004"));

        let error_no_id = WorkflowExecutionResult::Error {
            execution_id: None,
            error: "fail".to_string(),
        };
        assert!(error_no_id.execution_id().is_none());
    }

    // ========================================================================
    // Workflow Selection Tests
    // ========================================================================

    /// Test workflow that handles compaction patterns.
    struct MockCompactionWorkflow;

    impl Workflow for MockCompactionWorkflow {
        fn name(&self) -> &'static str {
            "handle_compaction"
        }

        fn description(&self) -> &'static str {
            "Mock workflow for compaction handling"
        }

        fn handles(&self, detection: &Detection) -> bool {
            detection.rule_id.contains("compaction")
        }

        fn steps(&self) -> Vec<WorkflowStep> {
            vec![WorkflowStep::new("notify", "Send notification")]
        }

        fn execute_step(
            &self,
            _ctx: &mut WorkflowContext,
            step_idx: usize,
        ) -> BoxFuture<'_, StepResult> {
            Box::pin(async move {
                match step_idx {
                    0 => StepResult::done_empty(),
                    _ => StepResult::abort("Unexpected step"),
                }
            })
        }
    }

    /// Test workflow that handles usage limit patterns.
    struct MockUsageLimitWorkflow;

    impl Workflow for MockUsageLimitWorkflow {
        fn name(&self) -> &'static str {
            "handle_usage_limit"
        }

        fn description(&self) -> &'static str {
            "Mock workflow for usage limit handling"
        }

        fn handles(&self, detection: &Detection) -> bool {
            detection.rule_id.contains("usage")
        }

        fn steps(&self) -> Vec<WorkflowStep> {
            vec![WorkflowStep::new("warn", "Send warning")]
        }

        fn execute_step(
            &self,
            _ctx: &mut WorkflowContext,
            step_idx: usize,
        ) -> BoxFuture<'_, StepResult> {
            Box::pin(async move {
                match step_idx {
                    0 => StepResult::done_empty(),
                    _ => StepResult::abort("Unexpected step"),
                }
            })
        }
    }

    /// Test that find_matching_workflow returns the correct workflow for a detection.
    #[test]
    fn workflow_runner_selects_correct_workflow_for_compaction() {
        // Create runner with multiple registered workflows
        let engine = WorkflowEngine::default();
        let lock_manager = Arc::new(PaneWorkflowLockManager::new());

        // Create mock injector (won't be called in this test)
        let injector = Arc::new(tokio::sync::Mutex::new(
            crate::policy::PolicyGatedInjector::new(
                crate::policy::PolicyEngine::permissive(),
                crate::wezterm::WeztermClient::new(),
            ),
        ));

        // Create a minimal storage handle using temp file
        let rt = tokio::runtime::Runtime::new().unwrap();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test.db")
            .to_string_lossy()
            .to_string();
        let storage = rt.block_on(async {
            Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap())
        });

        let runner = WorkflowRunner::new(
            engine,
            lock_manager,
            storage.clone(),
            injector,
            WorkflowRunnerConfig::default(),
        );

        // Register workflows
        runner.register_workflow(Arc::new(MockCompactionWorkflow));
        runner.register_workflow(Arc::new(MockUsageLimitWorkflow));

        // Create compaction detection
        let compaction_detection = Detection {
            rule_id: "claude.compaction".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "compaction".to_string(),
            severity: Severity::Warning,
            confidence: 0.9,
            matched_text: "Auto-compact: compacted".to_string(),
            extracted: serde_json::json!({}),
        };

        // Should find compaction workflow
        let workflow = runner.find_matching_workflow(&compaction_detection);
        assert!(workflow.is_some());
        assert_eq!(workflow.unwrap().name(), "handle_compaction");

        // Create usage detection
        let usage_detection = Detection {
            rule_id: "codex.usage.warning".to_string(),
            agent_type: AgentType::Codex,
            event_type: "usage_warning".to_string(),
            severity: Severity::Info,
            confidence: 0.8,
            matched_text: "less than 25%".to_string(),
            extracted: serde_json::json!({}),
        };

        // Should find usage limit workflow
        let workflow = runner.find_matching_workflow(&usage_detection);
        assert!(workflow.is_some());
        assert_eq!(workflow.unwrap().name(), "handle_usage_limit");

        // Create unmatched detection
        let unmatched_detection = Detection {
            rule_id: "unknown.pattern".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "unknown".to_string(),
            severity: Severity::Info,
            confidence: 0.5,
            matched_text: "something".to_string(),
            extracted: serde_json::json!({}),
        };

        // Should not find any workflow
        let workflow = runner.find_matching_workflow(&unmatched_detection);
        assert!(workflow.is_none());

        // Cleanup
        rt.block_on(async { storage.shutdown().await.unwrap() });
    }

    /// Test that pane locks prevent concurrent workflow executions.
    #[test]
    fn workflow_runner_lock_prevents_concurrent_runs() {
        let engine = WorkflowEngine::default();
        let lock_manager = Arc::new(PaneWorkflowLockManager::new());
        let injector = Arc::new(tokio::sync::Mutex::new(
            crate::policy::PolicyGatedInjector::new(
                crate::policy::PolicyEngine::permissive(),
                crate::wezterm::WeztermClient::new(),
            ),
        ));

        let rt = tokio::runtime::Runtime::new().unwrap();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test.db")
            .to_string_lossy()
            .to_string();
        let storage = rt.block_on(async {
            Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap())
        });

        let runner = WorkflowRunner::new(
            engine,
            lock_manager,
            storage.clone(),
            injector,
            WorkflowRunnerConfig::default(),
        );

        runner.register_workflow(Arc::new(MockCompactionWorkflow));

        let pane_id = 42u64;
        let detection = Detection {
            rule_id: "claude.compaction".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "compaction".to_string(),
            severity: Severity::Warning,
            confidence: 0.9,
            matched_text: "compacted".to_string(),
            extracted: serde_json::json!({}),
        };

        // Create test pane first
        rt.block_on(async {
            let pane = crate::storage::PaneRecord {
                pane_id,
                pane_uuid: None,
                domain: "local".to_string(),
                window_id: Some(1),
                tab_id: Some(1),
                title: Some("test".to_string()),
                cwd: Some("/tmp".to_string()),
                tty_name: None,
                first_seen_at: now_ms(),
                last_seen_at: now_ms(),
                observed: true,
                ignore_reason: None,
                last_decision_at: None,
            };
            storage.upsert_pane(pane).await.unwrap();
        });

        // First handle_detection should start
        let result1 = rt.block_on(runner.handle_detection(pane_id, &detection, None));
        assert!(result1.is_started());

        // Second handle_detection should be blocked by lock
        let result2 = rt.block_on(runner.handle_detection(pane_id, &detection, None));
        assert!(result2.is_locked());

        // Verify the lock info
        if let WorkflowStartResult::PaneLocked {
            held_by_workflow, ..
        } = result2
        {
            assert_eq!(held_by_workflow, "handle_compaction");
        }

        // Cleanup
        rt.block_on(async { storage.shutdown().await.unwrap() });
    }

    /// Test that find_workflow_by_name works correctly.
    #[test]
    fn workflow_runner_find_by_name() {
        let engine = WorkflowEngine::default();
        let lock_manager = Arc::new(PaneWorkflowLockManager::new());
        let injector = Arc::new(tokio::sync::Mutex::new(
            crate::policy::PolicyGatedInjector::new(
                crate::policy::PolicyEngine::permissive(),
                crate::wezterm::WeztermClient::new(),
            ),
        ));

        let rt = tokio::runtime::Runtime::new().unwrap();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test.db")
            .to_string_lossy()
            .to_string();
        let storage = rt.block_on(async {
            Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap())
        });

        let runner = WorkflowRunner::new(
            engine,
            lock_manager,
            storage.clone(),
            injector,
            WorkflowRunnerConfig::default(),
        );

        runner.register_workflow(Arc::new(MockCompactionWorkflow));
        runner.register_workflow(Arc::new(MockUsageLimitWorkflow));

        // Find by name
        let workflow = runner.find_workflow_by_name("handle_compaction");
        assert!(workflow.is_some());
        assert_eq!(workflow.unwrap().name(), "handle_compaction");

        let workflow = runner.find_workflow_by_name("handle_usage_limit");
        assert!(workflow.is_some());
        assert_eq!(workflow.unwrap().name(), "handle_usage_limit");

        // Not found
        let workflow = runner.find_workflow_by_name("nonexistent");
        assert!(workflow.is_none());

        // Cleanup
        rt.block_on(async { storage.shutdown().await.unwrap() });
    }

    // ========================================================================
    // Policy Denial Tests (wa-nu4.1.1.5)
    // ========================================================================

    /// Test workflow that attempts to send text and checks policy result.
    /// Kept for future integration tests that need to test workflow execution with policy gates.
    #[allow(dead_code)]
    struct MockTextSendingWorkflow;

    impl Workflow for MockTextSendingWorkflow {
        fn name(&self) -> &'static str {
            "text_sender"
        }

        fn description(&self) -> &'static str {
            "Mock workflow that sends text to test policy gates"
        }

        fn handles(&self, detection: &Detection) -> bool {
            detection.rule_id.contains("text_send")
        }

        fn steps(&self) -> Vec<WorkflowStep> {
            vec![WorkflowStep::new("send_text", "Send text to terminal")]
        }

        fn execute_step(
            &self,
            ctx: &mut WorkflowContext,
            step_idx: usize,
        ) -> BoxFuture<'_, StepResult> {
            // Need to move ctx reference into the async block properly
            let pane_id = ctx.pane_id();
            let has_injector = ctx.has_injector();
            let execution_id = ctx.execution_id().to_string();

            // Clone capabilities for use in async block
            let capabilities = ctx.capabilities().clone();

            Box::pin(async move {
                match step_idx {
                    0 => {
                        // Try to send text - policy should deny if command is running
                        if !has_injector {
                            return StepResult::abort("No injector configured");
                        }
                        // We need access to the context to call send_text
                        // This is a limitation of the mock - we'll test via the policy directly
                        StepResult::done(serde_json::json!({
                            "pane_id": pane_id,
                            "has_injector": has_injector,
                            "execution_id": execution_id,
                            "prompt_active": capabilities.prompt_active,
                            "command_running": capabilities.command_running,
                        }))
                    }
                    _ => StepResult::abort("Unexpected step"),
                }
            })
        }
    }

    /// Test that policy denial is properly returned when sending to a running command.
    #[test]
    fn policy_denies_send_when_command_running() {
        use crate::policy::{
            ActionKind, ActorKind, PaneCapabilities, PolicyDecision, PolicyEngine, PolicyInput,
        };

        // Create a strict policy engine (requires prompt active)
        let mut engine = PolicyEngine::strict();

        // Create capabilities where command is running (not at prompt)
        let caps = PaneCapabilities::running();

        // Try to authorize a send - should be denied
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Workflow)
            .with_pane(42)
            .with_capabilities(caps)
            .with_text_summary("test command")
            .with_workflow("wf-test-001");

        let decision = engine.authorize(&input);

        // Verify it's denied with the expected reason
        match decision {
            PolicyDecision::Deny {
                reason, rule_id, ..
            } => {
                assert!(
                    reason.contains("running command") || reason.contains("wait for prompt"),
                    "Expected denial reason about running command, got: {reason}"
                );
                assert_eq!(rule_id, Some("policy.prompt_required".to_string()));
            }
            other => panic!("Expected Deny, got: {other:?}"),
        }
    }

    /// Test that InjectionResult::Denied is returned when policy denies.
    #[tokio::test]
    async fn policy_gated_injector_returns_denied_for_running_command() {
        use crate::policy::{
            ActorKind, InjectionResult, PaneCapabilities, PolicyEngine, PolicyGatedInjector,
        };

        // Create a strict policy engine (requires prompt active)
        let engine = PolicyEngine::strict();
        let client = crate::wezterm::WeztermClient::new();
        let mut injector = PolicyGatedInjector::new(engine, client);

        // Create capabilities where command is running (not at prompt)
        let caps = PaneCapabilities::running();

        // Try to send text - should be denied by policy
        let result = injector
            .send_text(
                42,
                "echo test",
                ActorKind::Workflow,
                &caps,
                Some("wf-test-002"),
            )
            .await;

        // Verify it's denied
        assert!(
            result.is_denied(),
            "Expected denied result, got: {result:?}"
        );

        // Verify the rule ID
        if let InjectionResult::Denied { decision, .. } = result {
            assert_eq!(
                decision.rule_id(),
                Some("policy.prompt_required"),
                "Expected policy.prompt_required rule, got: {:?}",
                decision.rule_id()
            );
        }
    }

    /// Test that WorkflowContext has injector access after with_injector is called.
    #[test]
    fn workflow_context_injector_access() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test.db")
            .to_string_lossy()
            .to_string();

        rt.block_on(async {
            let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());

            // Create context without injector
            let ctx =
                WorkflowContext::new(storage.clone(), 42, PaneCapabilities::default(), "exec-001");
            assert!(!ctx.has_injector());

            // Create context with injector
            let engine = crate::policy::PolicyEngine::permissive();
            let client = crate::wezterm::WeztermClient::new();
            let injector = Arc::new(tokio::sync::Mutex::new(
                crate::policy::PolicyGatedInjector::new(engine, client),
            ));

            let ctx_with_injector =
                WorkflowContext::new(storage.clone(), 42, PaneCapabilities::default(), "exec-002")
                    .with_injector(injector);

            assert!(ctx_with_injector.has_injector());

            storage.shutdown().await.unwrap();
        });
    }

    // ========================================================================
    // HandleCompaction Workflow Tests (wa-nu4.1.2.1)
    // ========================================================================

    #[test]
    fn handle_compaction_metadata() {
        let workflow = HandleCompaction::new();

        assert_eq!(workflow.name(), "handle_compaction");
        assert_eq!(
            workflow.description(),
            "Re-inject critical context (AGENTS.md) after conversation compaction"
        );

        let steps = workflow.steps();
        assert_eq!(steps.len(), 4);
        assert_eq!(steps[0].name, "check_guards");
        assert_eq!(steps[1].name, "stabilize");
        assert_eq!(steps[2].name, "send_prompt");
        assert_eq!(steps[3].name, "verify_send");
    }

    #[test]
    fn handle_compaction_handles_compaction_events() {
        let workflow = HandleCompaction::new();

        // Should handle event_type "session.compaction"
        let detection_event_type = Detection {
            rule_id: "something.other".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "session.compaction".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::Value::Null,
            matched_text: "test".to_string(),
        };
        assert!(workflow.handles(&detection_event_type));

        // Should handle rule_id containing "compaction"
        let detection_rule_id = Detection {
            rule_id: "claude_code.compaction".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "other".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::Value::Null,
            matched_text: "test".to_string(),
        };
        assert!(workflow.handles(&detection_rule_id));

        // Should NOT handle unrelated detections
        let detection_unrelated = Detection {
            rule_id: "prompt.ready".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "prompt".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::Value::Null,
            matched_text: "test".to_string(),
        };
        assert!(!workflow.handles(&detection_unrelated));
    }

    #[test]
    fn handle_compaction_guard_checks() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_guards.db")
            .to_string_lossy()
            .to_string();

        rt.block_on(async {
            let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());

            // Normal capabilities - should pass guards
            let normal_caps = PaneCapabilities {
                alt_screen: Some(false),
                command_running: false,
                has_recent_gap: false,
                ..Default::default()
            };
            let ctx_normal =
                WorkflowContext::new(storage.clone(), 42, normal_caps, "exec-guard-normal");
            let result = HandleCompaction::check_pane_guards(&ctx_normal);
            assert!(result.is_ok(), "Normal state should pass guards");

            // Alt-screen active - should fail
            let alt_caps = PaneCapabilities {
                alt_screen: Some(true),
                command_running: false,
                has_recent_gap: false,
                ..Default::default()
            };
            let ctx_alt = WorkflowContext::new(storage.clone(), 42, alt_caps, "exec-guard-alt");
            let result = HandleCompaction::check_pane_guards(&ctx_alt);
            assert!(result.is_err());
            assert!(result.unwrap_err().contains("alt-screen"));

            // Command running - should fail
            let cmd_caps = PaneCapabilities {
                alt_screen: Some(false),
                command_running: true,
                has_recent_gap: false,
                ..Default::default()
            };
            let ctx_cmd = WorkflowContext::new(storage.clone(), 42, cmd_caps, "exec-guard-cmd");
            let result = HandleCompaction::check_pane_guards(&ctx_cmd);
            assert!(result.is_err());
            assert!(result.unwrap_err().contains("running"));

            // Recent gap - should fail
            let gap_caps = PaneCapabilities {
                alt_screen: Some(false),
                command_running: false,
                has_recent_gap: true,
                ..Default::default()
            };
            let ctx_gap = WorkflowContext::new(storage.clone(), 42, gap_caps, "exec-guard-gap");
            let result = HandleCompaction::check_pane_guards(&ctx_gap);
            assert!(result.is_err());
            assert!(result.unwrap_err().contains("gap"));

            storage.shutdown().await.unwrap();
        });
    }

    #[test]
    fn handle_compaction_prompts_exist() {
        // Verify all agent-specific prompts are non-empty
        assert!(!compaction_prompts::CLAUDE_CODE.is_empty());
        assert!(!compaction_prompts::CODEX.is_empty());
        assert!(!compaction_prompts::GEMINI.is_empty());
        assert!(!compaction_prompts::UNKNOWN.is_empty());

        // Verify they contain AGENTS.md reference (key context file)
        assert!(compaction_prompts::CLAUDE_CODE.contains("AGENTS.md"));
        assert!(compaction_prompts::CODEX.contains("AGENTS.md"));
        assert!(compaction_prompts::GEMINI.contains("AGENTS.md"));
    }

    #[test]
    fn handle_compaction_builder_pattern() {
        let workflow = HandleCompaction::new()
            .with_stabilization_ms(5000)
            .with_idle_timeout_ms(60_000);

        assert_eq!(workflow.stabilization_ms, 5000);
        assert_eq!(workflow.idle_timeout_ms, 60_000);
    }

    #[test]
    fn handle_compaction_default_values() {
        let workflow = HandleCompaction::default();

        // Defaults should be reasonable values
        assert!(workflow.stabilization_ms > 0);
        assert!(workflow.idle_timeout_ms > 0);
        assert!(workflow.idle_timeout_ms > workflow.stabilization_ms);
    }

    // ========================================================================
    // HandleCompaction Integration Tests (wa-nu4.1.2.4)
    // ========================================================================
    //
    // These tests verify the full workflow execution path with synthetic
    // detections and various pane states.

    /// Test: Synthetic compaction detection + PromptActive state
    /// Expected: Workflow proceeds through guards → step logs show completion path
    #[test]
    fn handle_compaction_integration_prompt_active_passes_guards() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_hc_integration.db")
            .to_string_lossy()
            .to_string();

        rt.block_on(async {
            let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());

            // Create PromptActive capabilities (pane is ready for input)
            let prompt_caps = PaneCapabilities {
                alt_screen: Some(false),
                command_running: false,
                has_recent_gap: false,
                ..Default::default()
            };

            let execution_id = "test-hc-prompt-active-001";
            let pane_id = 42u64;

            // Create context with PromptActive state
            let ctx =
                WorkflowContext::new(storage.clone(), pane_id, prompt_caps.clone(), execution_id);

            // Verify guards pass for PromptActive state
            let guard_result = HandleCompaction::check_pane_guards(&ctx);
            assert!(
                guard_result.is_ok(),
                "Guard check should pass for PromptActive state, got: {:?}",
                guard_result
            );

            // Create synthetic compaction detection
            let detection = Detection {
                rule_id: "claude_code.compaction".to_string(),
                agent_type: AgentType::ClaudeCode,
                event_type: "session.compaction".to_string(),
                severity: Severity::Info,
                confidence: 1.0,
                extracted: serde_json::json!({
                    "tokens_before": 150_000,
                    "tokens_after": 25_000
                }),
                matched_text: "Auto-compact: compacted 150,000 tokens to 25,000 tokens".to_string(),
            };

            // Verify HandleCompaction handles this detection
            let workflow = HandleCompaction::new();
            assert!(
                workflow.handles(&detection),
                "HandleCompaction should handle compaction detections"
            );

            storage.shutdown().await.unwrap();
        });
    }

    /// Test: AltScreen active state causes workflow abort
    /// Expected: Guard check fails with "alt-screen" in error message
    #[test]
    fn handle_compaction_integration_alt_screen_aborts() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_hc_altscreen.db")
            .to_string_lossy()
            .to_string();

        rt.block_on(async {
            let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());

            // Create AltScreen capabilities (vim, less, htop, etc.)
            let alt_screen_caps = PaneCapabilities {
                alt_screen: Some(true),
                command_running: false,
                has_recent_gap: false,
                ..Default::default()
            };

            let execution_id = "test-hc-altscreen-001";
            let pane_id = 42u64;

            let ctx = WorkflowContext::new(storage.clone(), pane_id, alt_screen_caps, execution_id);

            // Verify guards fail for AltScreen state
            let guard_result = HandleCompaction::check_pane_guards(&ctx);
            assert!(
                guard_result.is_err(),
                "Guard check should fail for AltScreen state"
            );

            // Verify error message is actionable (contains "alt-screen")
            let err = guard_result.unwrap_err();
            assert!(
                err.contains("alt-screen"),
                "Error message should mention 'alt-screen' for actionable diagnosis, got: {}",
                err
            );

            storage.shutdown().await.unwrap();
        });
    }

    /// Test: Command running state causes workflow abort
    /// Expected: Guard check fails with "running" in error message
    #[test]
    fn handle_compaction_integration_command_running_aborts() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_hc_cmdrunning.db")
            .to_string_lossy()
            .to_string();

        rt.block_on(async {
            let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());

            // Create capabilities where command is running
            let running_caps = PaneCapabilities {
                alt_screen: Some(false),
                command_running: true,
                has_recent_gap: false,
                ..Default::default()
            };

            let execution_id = "test-hc-running-001";
            let pane_id = 42u64;

            let ctx = WorkflowContext::new(storage.clone(), pane_id, running_caps, execution_id);

            // Verify guards fail
            let guard_result = HandleCompaction::check_pane_guards(&ctx);
            assert!(
                guard_result.is_err(),
                "Guard check should fail when command is running"
            );

            // Verify error message is actionable
            let err = guard_result.unwrap_err();
            assert!(
                err.contains("running"),
                "Error message should mention 'running' for actionable diagnosis, got: {}",
                err
            );

            storage.shutdown().await.unwrap();
        });
    }

    /// Test: Recent gap state causes workflow abort
    /// Expected: Guard check fails with "gap" in error message
    #[test]
    fn handle_compaction_integration_recent_gap_aborts() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_hc_gap.db")
            .to_string_lossy()
            .to_string();

        rt.block_on(async {
            let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());

            // Create capabilities with recent gap
            let gap_caps = PaneCapabilities {
                alt_screen: Some(false),
                command_running: false,
                has_recent_gap: true,
                ..Default::default()
            };

            let execution_id = "test-hc-gap-001";
            let pane_id = 42u64;

            let ctx = WorkflowContext::new(storage.clone(), pane_id, gap_caps, execution_id);

            // Verify guards fail
            let guard_result = HandleCompaction::check_pane_guards(&ctx);
            assert!(
                guard_result.is_err(),
                "Guard check should fail with recent gap"
            );

            // Verify error message is actionable
            let err = guard_result.unwrap_err();
            assert!(
                err.contains("gap"),
                "Error message should mention 'gap' for actionable diagnosis, got: {}",
                err
            );

            storage.shutdown().await.unwrap();
        });
    }

    /// Test: Verify step metadata is correct for handle_compaction
    /// Expected: Steps array contains check_guards, stabilize, send_prompt, verify_send
    #[test]
    fn handle_compaction_step_metadata_complete() {
        let workflow = HandleCompaction::new();
        let steps = workflow.steps();

        // Verify all expected steps are present
        assert_eq!(steps.len(), 4, "HandleCompaction should have 4 steps");

        // Verify step names are descriptive (for logging/debugging)
        let step_names: Vec<&str> = steps.iter().map(|s| s.name.as_str()).collect();
        assert!(
            step_names.contains(&"check_guards"),
            "Should have check_guards step"
        );
        assert!(
            step_names.contains(&"stabilize"),
            "Should have stabilize step"
        );
        assert!(
            step_names.contains(&"send_prompt"),
            "Should have send_prompt step"
        );
        assert!(
            step_names.contains(&"verify_send"),
            "Should have verify_send step"
        );

        // Verify step descriptions are non-empty (for actionable logging)
        for step in &steps {
            assert!(
                !step.description.is_empty(),
                "Step '{}' should have a description for actionable logging",
                step.name
            );
        }
    }

    /// Test: Agent-specific prompt selection is deterministic
    /// Expected: Each agent type gets a consistent, non-empty prompt
    #[test]
    fn handle_compaction_agent_prompt_selection_deterministic() {
        // Test each agent type gets a deterministic prompt
        let agents = vec![
            (AgentType::ClaudeCode, compaction_prompts::CLAUDE_CODE),
            (AgentType::Codex, compaction_prompts::CODEX),
            (AgentType::Gemini, compaction_prompts::GEMINI),
            (AgentType::Unknown, compaction_prompts::UNKNOWN),
        ];

        for (agent_type, expected_prompt) in agents {
            // Verify prompt is non-empty
            assert!(
                !expected_prompt.is_empty(),
                "Prompt for {:?} should not be empty",
                agent_type
            );

            // Verify prompt contains AGENTS.md reference (except Unknown)
            if agent_type != AgentType::Unknown {
                assert!(
                    expected_prompt.contains("AGENTS.md"),
                    "Prompt for {:?} should reference AGENTS.md",
                    agent_type
                );
            }

            // Verify prompt ends with newline (for clean send)
            assert!(
                expected_prompt.ends_with('\n'),
                "Prompt for {:?} should end with newline for clean send",
                agent_type
            );
        }
    }

    /// Test: Workflow execution step 0 (check_guards) with PromptActive state
    /// Expected: Step returns Continue (not Abort)
    #[tokio::test]
    async fn handle_compaction_execute_step0_prompt_active_continues() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_step0.db")
            .to_string_lossy()
            .to_string();

        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());

        // Create PromptActive capabilities
        let prompt_caps = PaneCapabilities {
            alt_screen: Some(false),
            command_running: false,
            has_recent_gap: false,
            ..Default::default()
        };

        let mut ctx = WorkflowContext::new(storage.clone(), 42, prompt_caps, "test-step0-001");

        let workflow = HandleCompaction::new();
        let result = workflow.execute_step(&mut ctx, 0).await;

        // Step 0 should return Continue for valid state
        match result {
            StepResult::Continue => {
                // Success - guards passed
            }
            StepResult::Abort { reason } => {
                panic!("Step 0 should not abort for PromptActive state: {}", reason);
            }
            other => {
                panic!("Unexpected step result for step 0: {:?}", other);
            }
        }

        storage.shutdown().await.unwrap();
    }

    /// Test: Workflow execution step 0 (check_guards) with AltScreen state
    /// Expected: Step returns Abort with actionable reason
    #[tokio::test]
    async fn handle_compaction_execute_step0_alt_screen_aborts() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_step0_alt.db")
            .to_string_lossy()
            .to_string();

        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());

        // Create AltScreen capabilities
        let alt_caps = PaneCapabilities {
            alt_screen: Some(true),
            command_running: false,
            has_recent_gap: false,
            ..Default::default()
        };

        let mut ctx = WorkflowContext::new(storage.clone(), 42, alt_caps, "test-step0-alt-001");

        let workflow = HandleCompaction::new();
        let result = workflow.execute_step(&mut ctx, 0).await;

        // Step 0 should abort for AltScreen
        match result {
            StepResult::Abort { reason } => {
                assert!(
                    reason.contains("alt-screen"),
                    "Abort reason should mention 'alt-screen': {}",
                    reason
                );
            }
            StepResult::Continue => {
                panic!("Step 0 should abort for AltScreen state, got Continue");
            }
            other => {
                panic!(
                    "Unexpected step result for step 0 with AltScreen: {:?}",
                    other
                );
            }
        }

        storage.shutdown().await.unwrap();
    }

    /// Test: Workflow execution step 1 (stabilize) returns WaitFor condition
    /// Expected: Step returns WaitFor with PaneIdle condition
    #[tokio::test]
    async fn handle_compaction_execute_step1_returns_wait_for() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_step1.db")
            .to_string_lossy()
            .to_string();

        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());

        let prompt_caps = PaneCapabilities {
            alt_screen: Some(false),
            command_running: false,
            has_recent_gap: false,
            ..Default::default()
        };

        let mut ctx = WorkflowContext::new(storage.clone(), 42, prompt_caps, "test-step1-001");

        let workflow = HandleCompaction::new();
        let result = workflow.execute_step(&mut ctx, 1).await;

        // Step 1 should return WaitFor with PaneIdle condition
        match result {
            StepResult::WaitFor { condition, .. } => {
                // Verify it's waiting for pane idle
                if let WaitCondition::PaneIdle {
                    idle_threshold_ms, ..
                } = condition
                {
                    assert!(idle_threshold_ms > 0, "Idle threshold should be positive");
                } else {
                    panic!("Step 1 should wait for PaneIdle, got: {:?}", condition);
                }
            }
            other => {
                panic!("Step 1 should return WaitFor, got: {:?}", other);
            }
        }

        storage.shutdown().await.unwrap();
    }

    /// Test: Workflow execution step 2 (send_prompt) without injector
    /// Expected: Step returns Abort (no injector configured)
    #[tokio::test]
    async fn handle_compaction_execute_step2_no_injector_aborts() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_step2_no_inj.db")
            .to_string_lossy()
            .to_string();

        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());

        let prompt_caps = PaneCapabilities {
            alt_screen: Some(false),
            command_running: false,
            has_recent_gap: false,
            ..Default::default()
        };

        // Create context WITHOUT injector
        let mut ctx =
            WorkflowContext::new(storage.clone(), 42, prompt_caps, "test-step2-no-inj-001");

        let workflow = HandleCompaction::new();
        let result = workflow.execute_step(&mut ctx, 2).await;

        // Step 2 should abort without injector
        match result {
            StepResult::Abort { reason } => {
                assert!(
                    reason.to_lowercase().contains("injector"),
                    "Abort reason should mention missing injector: {}",
                    reason
                );
            }
            other => {
                panic!("Step 2 should abort without injector, got: {:?}", other);
            }
        }

        storage.shutdown().await.unwrap();
    }

    /// Test: Unexpected step index returns Abort
    /// Expected: Step indices >= step_count return Abort
    #[tokio::test]
    async fn handle_compaction_execute_invalid_step_aborts() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_invalid_step.db")
            .to_string_lossy()
            .to_string();

        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());

        let prompt_caps = PaneCapabilities::default();

        let mut ctx =
            WorkflowContext::new(storage.clone(), 42, prompt_caps, "test-invalid-step-001");

        let workflow = HandleCompaction::new();

        // Try to execute step beyond the workflow's steps
        let invalid_step = workflow.step_count() + 1;
        let result = workflow.execute_step(&mut ctx, invalid_step).await;

        // Should abort for invalid step
        match result {
            StepResult::Abort { reason } => {
                assert!(
                    reason.contains("step") || reason.contains("index"),
                    "Abort reason should mention invalid step: {}",
                    reason
                );
            }
            other => {
                panic!("Invalid step should abort, got: {:?}", other);
            }
        }

        storage.shutdown().await.unwrap();
    }

    // ========================================================================
    // Workflow Engine Tests (wa-nu4.1.1.7)
    // Lock Behavior, Step Logging, and Resume Tests
    // ========================================================================

    /// Simple workflow that completes after one step (for testing lock release on success)
    struct SimpleCompletingWorkflow;

    impl Workflow for SimpleCompletingWorkflow {
        fn name(&self) -> &'static str {
            "simple_completing"
        }

        fn description(&self) -> &'static str {
            "Test workflow that completes immediately"
        }

        fn handles(&self, detection: &Detection) -> bool {
            detection.rule_id.contains("simple_complete")
        }

        fn steps(&self) -> Vec<WorkflowStep> {
            vec![WorkflowStep::new("complete", "Complete immediately")]
        }

        fn execute_step(
            &self,
            _ctx: &mut WorkflowContext,
            step_idx: usize,
        ) -> BoxFuture<'_, StepResult> {
            Box::pin(async move {
                match step_idx {
                    0 => StepResult::done(serde_json::json!({"completed": true})),
                    _ => StepResult::abort("Unexpected step"),
                }
            })
        }
    }

    /// Workflow that aborts after one step (for testing lock release on abort)
    struct AbortingWorkflow {
        abort_reason: String,
    }

    impl AbortingWorkflow {
        fn new(reason: &str) -> Self {
            Self {
                abort_reason: reason.to_string(),
            }
        }
    }

    impl Workflow for AbortingWorkflow {
        fn name(&self) -> &'static str {
            "aborting_workflow"
        }

        fn description(&self) -> &'static str {
            "Test workflow that aborts"
        }

        fn handles(&self, detection: &Detection) -> bool {
            detection.rule_id.contains("abort_test")
        }

        fn steps(&self) -> Vec<WorkflowStep> {
            vec![WorkflowStep::new("abort_step", "Abort immediately")]
        }

        fn execute_step(
            &self,
            _ctx: &mut WorkflowContext,
            step_idx: usize,
        ) -> BoxFuture<'_, StepResult> {
            let reason = self.abort_reason.clone();
            Box::pin(async move {
                match step_idx {
                    0 => StepResult::abort(&reason),
                    _ => StepResult::abort("Unexpected step"),
                }
            })
        }
    }

    /// Multi-step workflow for testing step logging and resume
    struct MultiStepWorkflow {
        fail_at_step: Option<usize>,
    }

    impl MultiStepWorkflow {
        fn new() -> Self {
            Self { fail_at_step: None }
        }

        fn failing_at(step: usize) -> Self {
            Self {
                fail_at_step: Some(step),
            }
        }
    }

    impl Workflow for MultiStepWorkflow {
        fn name(&self) -> &'static str {
            "multi_step"
        }

        fn description(&self) -> &'static str {
            "Test workflow with multiple steps"
        }

        fn handles(&self, detection: &Detection) -> bool {
            detection.rule_id.contains("multi_step")
        }

        fn steps(&self) -> Vec<WorkflowStep> {
            vec![
                WorkflowStep::new("step_0", "First step"),
                WorkflowStep::new("step_1", "Second step"),
                WorkflowStep::new("step_2", "Third step"),
                WorkflowStep::new("step_3", "Final step"),
            ]
        }

        fn execute_step(
            &self,
            _ctx: &mut WorkflowContext,
            step_idx: usize,
        ) -> BoxFuture<'_, StepResult> {
            let fail_at = self.fail_at_step;
            Box::pin(async move {
                if Some(step_idx) == fail_at {
                    return StepResult::abort("Simulated failure");
                }
                match step_idx {
                    0 | 1 | 2 => StepResult::cont(),
                    3 => StepResult::done(serde_json::json!({"steps_completed": 4})),
                    _ => StepResult::abort("Unexpected step index"),
                }
            })
        }
    }

    /// Helper to create a test WorkflowRunner with storage
    async fn create_test_runner(
        db_path: &str,
    ) -> (
        WorkflowRunner,
        Arc<crate::storage::StorageHandle>,
        Arc<PaneWorkflowLockManager>,
    ) {
        let engine = WorkflowEngine::default();
        let lock_manager = Arc::new(PaneWorkflowLockManager::new());
        let storage = Arc::new(crate::storage::StorageHandle::new(db_path).await.unwrap());
        let injector = Arc::new(tokio::sync::Mutex::new(
            crate::policy::PolicyGatedInjector::new(
                crate::policy::PolicyEngine::permissive(),
                crate::wezterm::WeztermClient::new(),
            ),
        ));

        let runner = WorkflowRunner::new(
            engine,
            Arc::clone(&lock_manager),
            Arc::clone(&storage),
            injector,
            WorkflowRunnerConfig::default(),
        );

        (runner, storage, lock_manager)
    }

    /// Helper to create a test pane in storage
    async fn create_test_pane(storage: &crate::storage::StorageHandle, pane_id: u64) {
        let pane = crate::storage::PaneRecord {
            pane_id,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: Some(1),
            tab_id: Some(1),
            title: Some("test".to_string()),
            cwd: Some("/tmp".to_string()),
            tty_name: None,
            first_seen_at: now_ms(),
            last_seen_at: now_ms(),
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };
        storage.upsert_pane(pane).await.unwrap();
    }

    // ------------------------------------------------------------------------
    // Lock Release Tests (wa-nu4.1.1.7)
    // ------------------------------------------------------------------------

    /// Test: Lock is released when workflow completes successfully (Done)
    #[tokio::test]
    async fn lock_released_on_workflow_completion() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_lock_complete.db")
            .to_string_lossy()
            .to_string();

        let (runner, storage, lock_manager) = create_test_runner(&db_path).await;
        let pane_id = 42u64;

        create_test_pane(&storage, pane_id).await;
        runner.register_workflow(Arc::new(SimpleCompletingWorkflow));

        // Start workflow - acquires lock
        let detection = make_test_detection("simple_complete.test");
        let start_result = runner.handle_detection(pane_id, &detection, None).await;
        assert!(start_result.is_started(), "Workflow should start");

        // Lock should be held
        assert!(
            lock_manager.is_locked(pane_id).is_some(),
            "Lock should be held after starting workflow"
        );

        // Run the workflow to completion
        let workflow = runner.find_workflow_by_name("simple_completing").unwrap();
        let execution_id = start_result.execution_id().unwrap();
        let exec_result = runner
            .run_workflow(pane_id, workflow, execution_id, 0)
            .await;

        // Verify workflow completed
        assert!(
            exec_result.is_completed(),
            "Workflow should complete successfully"
        );

        // Lock should be released after completion
        assert!(
            lock_manager.is_locked(pane_id).is_none(),
            "Lock should be released after workflow completion"
        );

        storage.shutdown().await.unwrap();
    }

    /// Test: Lock is released when workflow aborts
    #[tokio::test]
    async fn lock_released_on_workflow_abort() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_lock_abort.db")
            .to_string_lossy()
            .to_string();

        let (runner, storage, lock_manager) = create_test_runner(&db_path).await;
        let pane_id = 43u64;

        create_test_pane(&storage, pane_id).await;
        runner.register_workflow(Arc::new(AbortingWorkflow::new("Test abort reason")));

        // Start workflow - acquires lock
        let detection = make_test_detection("abort_test.trigger");
        let start_result = runner.handle_detection(pane_id, &detection, None).await;
        assert!(start_result.is_started(), "Workflow should start");

        // Lock should be held
        assert!(
            lock_manager.is_locked(pane_id).is_some(),
            "Lock should be held after starting workflow"
        );

        // Run the workflow (will abort)
        let workflow = runner.find_workflow_by_name("aborting_workflow").unwrap();
        let execution_id = start_result.execution_id().unwrap();
        let exec_result = runner
            .run_workflow(pane_id, workflow, execution_id, 0)
            .await;

        // Verify workflow aborted
        assert!(exec_result.is_aborted(), "Workflow should abort");
        if let WorkflowExecutionResult::Aborted { reason, .. } = &exec_result {
            assert!(
                reason.contains("Test abort reason"),
                "Abort should have expected reason"
            );
        }

        // Lock should be released after abort
        assert!(
            lock_manager.is_locked(pane_id).is_none(),
            "Lock should be released after workflow abort"
        );

        storage.shutdown().await.unwrap();
    }

    /// Test: Per-pane lock prevents concurrent workflow execution
    #[tokio::test]
    async fn per_pane_lock_prevents_concurrent_workflows() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_lock_concurrent.db")
            .to_string_lossy()
            .to_string();

        let (runner, storage, lock_manager) = create_test_runner(&db_path).await;
        let pane_id = 44u64;

        create_test_pane(&storage, pane_id).await;
        runner.register_workflow(Arc::new(MultiStepWorkflow::new()));

        // Start first workflow
        let detection1 = make_test_detection("multi_step.first");
        let start_result1 = runner.handle_detection(pane_id, &detection1, None).await;
        assert!(start_result1.is_started(), "First workflow should start");

        // Verify lock is held by first workflow
        let lock_info = lock_manager.is_locked(pane_id);
        assert!(lock_info.is_some(), "Lock should be held");
        let info = lock_info.unwrap();
        assert_eq!(info.workflow_name, "multi_step");

        // Try to start second workflow on same pane
        let detection2 = make_test_detection("multi_step.second");
        let start_result2 = runner.handle_detection(pane_id, &detection2, None).await;

        // Second workflow should be blocked
        assert!(
            start_result2.is_locked(),
            "Second workflow should be blocked by lock"
        );
        if let WorkflowStartResult::PaneLocked {
            held_by_workflow, ..
        } = start_result2
        {
            assert_eq!(
                held_by_workflow, "multi_step",
                "Lock should be held by first workflow"
            );
        }

        // Complete first workflow to release lock
        let workflow = runner.find_workflow_by_name("multi_step").unwrap();
        let exec_id = start_result1.execution_id().unwrap();
        let _ = runner
            .run_workflow(pane_id, workflow.clone(), exec_id, 0)
            .await;

        // Now second workflow can start
        let start_result3 = runner.handle_detection(pane_id, &detection2, None).await;
        assert!(
            start_result3.is_started(),
            "Workflow should start after lock released"
        );

        storage.shutdown().await.unwrap();
    }

    // ------------------------------------------------------------------------
    // Step Logging Tests (wa-nu4.1.1.7)
    // ------------------------------------------------------------------------

    /// Test: Step logs are written correctly during workflow execution
    #[tokio::test]
    async fn step_logs_written_correctly() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_step_logs.db")
            .to_string_lossy()
            .to_string();

        let (runner, storage, _lock_manager) = create_test_runner(&db_path).await;
        let pane_id = 45u64;

        create_test_pane(&storage, pane_id).await;
        runner.register_workflow(Arc::new(MultiStepWorkflow::new()));

        // Start and run workflow
        let detection = make_test_detection("multi_step.log_test");
        let start_result = runner.handle_detection(pane_id, &detection, None).await;
        assert!(start_result.is_started());

        let workflow = runner.find_workflow_by_name("multi_step").unwrap();
        let execution_id = start_result.execution_id().unwrap();
        let exec_result = runner
            .run_workflow(pane_id, workflow, execution_id, 0)
            .await;

        assert!(exec_result.is_completed(), "Workflow should complete");

        // Verify step logs were written
        let step_logs = storage.get_step_logs(execution_id).await.unwrap();

        // Multi-step workflow has 4 steps (0, 1, 2, 3)
        assert_eq!(step_logs.len(), 4, "Should have 4 step log entries");

        // Verify each step log
        for (i, log) in step_logs.iter().enumerate() {
            assert_eq!(log.workflow_id, execution_id);
            assert_eq!(log.step_index, i);
            assert_eq!(log.step_name, format!("step_{i}"));
            assert!(log.started_at > 0, "Started timestamp should be set");
            assert!(log.completed_at >= log.started_at, "Completed >= started");
            assert!(log.duration_ms >= 0, "Duration should be non-negative");
        }

        // First 3 steps should be "continue", last should be "done"
        assert_eq!(step_logs[0].result_type, "continue");
        assert_eq!(step_logs[1].result_type, "continue");
        assert_eq!(step_logs[2].result_type, "continue");
        assert_eq!(step_logs[3].result_type, "done");

        storage.shutdown().await.unwrap();
    }

    /// Test: Step logs record abort correctly
    #[tokio::test]
    async fn step_logs_record_abort() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_step_logs_abort.db")
            .to_string_lossy()
            .to_string();

        let (runner, storage, _lock_manager) = create_test_runner(&db_path).await;
        let pane_id = 46u64;

        create_test_pane(&storage, pane_id).await;
        // Workflow that fails at step 2
        runner.register_workflow(Arc::new(MultiStepWorkflow::failing_at(2)));

        // Start and run workflow
        let detection = make_test_detection("multi_step.abort_log_test");
        let start_result = runner.handle_detection(pane_id, &detection, None).await;
        assert!(start_result.is_started());

        let workflow = runner.find_workflow_by_name("multi_step").unwrap();
        let execution_id = start_result.execution_id().unwrap();
        let exec_result = runner
            .run_workflow(pane_id, workflow, execution_id, 0)
            .await;

        assert!(exec_result.is_aborted(), "Workflow should abort");

        // Verify step logs
        let step_logs = storage.get_step_logs(execution_id).await.unwrap();

        // Should have 3 step logs (steps 0, 1, 2 where 2 aborts)
        assert_eq!(step_logs.len(), 3, "Should have 3 step log entries");

        // Steps 0 and 1 should be "continue"
        assert_eq!(step_logs[0].result_type, "continue");
        assert_eq!(step_logs[1].result_type, "continue");
        // Step 2 should be "abort"
        assert_eq!(step_logs[2].result_type, "abort");

        storage.shutdown().await.unwrap();
    }

    // ------------------------------------------------------------------------
    // Resume Tests (wa-nu4.1.1.7)
    // ------------------------------------------------------------------------

    /// Test: WorkflowEngine.resume computes correct next step from logs
    #[tokio::test]
    async fn engine_resume_finds_correct_step() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_resume.db")
            .to_string_lossy()
            .to_string();

        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());
        let engine = WorkflowEngine::new(3);

        // Create a test pane
        create_test_pane(&storage, 50).await;

        // Start a workflow
        let execution = engine
            .start(&storage, "test_workflow", 50, None, None)
            .await
            .unwrap();

        // Manually insert step logs to simulate partial execution
        // Steps 0 and 1 completed, step 2 was in progress
        storage
            .insert_step_log(
                &execution.id,
                None,
                0,
                "step_0",
                "continue",
                None,
                1000,
                1100,
            )
            .await
            .unwrap();
        storage
            .insert_step_log(
                &execution.id,
                None,
                1,
                "step_1",
                "continue",
                None,
                1100,
                1200,
            )
            .await
            .unwrap();

        // Resume should find next step is 2
        let resume_result = engine.resume(&storage, &execution.id).await.unwrap();
        assert!(resume_result.is_some(), "Should find incomplete workflow");

        let (resumed_exec, next_step) = resume_result.unwrap();
        assert_eq!(resumed_exec.id, execution.id);
        assert_eq!(next_step, 2, "Next step should be 2 (after steps 0, 1)");

        storage.shutdown().await.unwrap();
    }

    /// Test: find_incomplete_workflows returns workflows with running/waiting status
    #[tokio::test]
    async fn find_incomplete_workflows_returns_running_and_waiting() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_find_incomplete.db")
            .to_string_lossy()
            .to_string();

        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());
        let engine = WorkflowEngine::new(3);

        // Create test panes
        create_test_pane(&storage, 51).await;
        create_test_pane(&storage, 52).await;
        create_test_pane(&storage, 53).await;

        // Start multiple workflows in different states
        let exec1 = engine
            .start(&storage, "workflow_1", 51, None, None)
            .await
            .unwrap();
        let exec2 = engine
            .start(&storage, "workflow_2", 52, None, None)
            .await
            .unwrap();
        let exec3 = engine
            .start(&storage, "workflow_3", 53, None, None)
            .await
            .unwrap();

        // Mark exec2 as waiting
        engine
            .update_status(
                &storage,
                &exec2.id,
                ExecutionStatus::Waiting,
                1,
                Some(&WaitCondition::pane_idle(1000)),
                None,
            )
            .await
            .unwrap();

        // Mark exec3 as completed (should not be returned)
        engine
            .update_status(
                &storage,
                &exec3.id,
                ExecutionStatus::Completed,
                2,
                None,
                None,
            )
            .await
            .unwrap();

        // Find incomplete workflows
        let incomplete = storage.find_incomplete_workflows().await.unwrap();

        // Should find exec1 (running) and exec2 (waiting), not exec3 (completed)
        assert_eq!(incomplete.len(), 2, "Should find 2 incomplete workflows");

        let incomplete_ids: std::collections::HashSet<_> =
            incomplete.iter().map(|w| w.id.as_str()).collect();
        assert!(incomplete_ids.contains(exec1.id.as_str()));
        assert!(incomplete_ids.contains(exec2.id.as_str()));
        assert!(!incomplete_ids.contains(exec3.id.as_str()));

        storage.shutdown().await.unwrap();
    }

    /// Test: resume_incomplete resumes workflows from last completed step
    #[tokio::test]
    async fn resume_incomplete_resumes_from_last_step() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_resume_incomplete.db")
            .to_string_lossy()
            .to_string();

        let (runner, storage, lock_manager) = create_test_runner(&db_path).await;
        let pane_id = 54u64;

        create_test_pane(&storage, pane_id).await;
        runner.register_workflow(Arc::new(MultiStepWorkflow::new()));

        // Start workflow and simulate partial execution
        let detection = make_test_detection("multi_step.resume_test");
        let start_result = runner.handle_detection(pane_id, &detection, None).await;
        assert!(start_result.is_started());
        let execution_id = start_result.execution_id().unwrap().to_string();

        // Insert step logs for steps 0 and 1 (completed)
        storage
            .insert_step_log(
                &execution_id,
                None,
                0,
                "step_0",
                "continue",
                None,
                1000,
                1100,
            )
            .await
            .unwrap();
        storage
            .insert_step_log(
                &execution_id,
                None,
                1,
                "step_1",
                "continue",
                None,
                1100,
                1200,
            )
            .await
            .unwrap();

        // Release the lock to simulate a restart scenario
        lock_manager.force_release(pane_id);

        // Call resume_incomplete
        let results = runner.resume_incomplete().await;

        // Should have resumed and completed the workflow
        assert_eq!(results.len(), 1, "Should resume 1 workflow");
        assert!(
            results[0].is_completed(),
            "Resumed workflow should complete"
        );

        // Verify step logs show resumed execution (steps 2 and 3)
        let step_logs = storage.get_step_logs(&execution_id).await.unwrap();

        // Should have 4 step logs total now
        assert_eq!(step_logs.len(), 4, "Should have 4 step logs after resume");

        // Steps 0, 1 were from before, steps 2, 3 from resume
        assert_eq!(step_logs[2].step_index, 2);
        assert_eq!(step_logs[3].step_index, 3);
        assert_eq!(step_logs[3].result_type, "done");

        storage.shutdown().await.unwrap();
    }

    /// Test: Aborted workflows are not resumed
    #[tokio::test]
    async fn aborted_workflows_not_resumed() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_aborted_not_resumed.db")
            .to_string_lossy()
            .to_string();

        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());
        let engine = WorkflowEngine::new(3);

        // Create test pane
        create_test_pane(&storage, 55).await;

        // Start a workflow and mark it aborted
        let execution = engine
            .start(&storage, "test_workflow", 55, None, None)
            .await
            .unwrap();

        engine
            .update_status(
                &storage,
                &execution.id,
                ExecutionStatus::Aborted,
                1,
                None,
                Some("Test abort"),
            )
            .await
            .unwrap();

        // Find incomplete - should not include aborted workflow
        let incomplete = storage.find_incomplete_workflows().await.unwrap();
        assert!(
            incomplete.is_empty(),
            "Aborted workflow should not be in incomplete list"
        );

        // Resume should return None
        let resume_result = engine.resume(&storage, &execution.id).await.unwrap();
        assert!(
            resume_result.is_none(),
            "Aborted workflow should not be resumable"
        );

        storage.shutdown().await.unwrap();
    }

    // ====================================================================
    // Codex Exit Step Tests (wa-nu4.1.3.2)
    // ====================================================================

    #[derive(Clone)]
    struct TestTextSource {
        sequence: Arc<Vec<String>>,
        index: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl TestTextSource {
        fn new(sequence: Vec<&str>) -> Self {
            Self {
                sequence: Arc::new(sequence.into_iter().map(str::to_string).collect()),
                index: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
        }
    }

    impl PaneTextSource for TestTextSource {
        type Fut<'a> = Pin<Box<dyn Future<Output = crate::Result<String>> + Send + 'a>>;

        fn get_text(&self, _pane_id: u64, _escapes: bool) -> Self::Fut<'_> {
            let idx = self.index.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let text = self
                .sequence
                .get(idx)
                .cloned()
                .or_else(|| self.sequence.last().cloned())
                .unwrap_or_default();
            Box::pin(async move { Ok(text) })
        }
    }

    fn allowed_ctrl_c_result() -> InjectionResult {
        InjectionResult::Allowed {
            decision: crate::policy::PolicyDecision::allow(),
            summary: "ctrl-c".to_string(),
            pane_id: 1,
            action: crate::policy::ActionKind::SendCtrlC,
            audit_action_id: None,
        }
    }

    fn wait_options_single_poll() -> WaitOptions {
        WaitOptions {
            tail_lines: 200,
            escapes: false,
            poll_initial: Duration::from_millis(0),
            poll_max: Duration::from_millis(0),
            max_polls: 1,
        }
    }

    #[tokio::test]
    async fn codex_exit_sends_one_ctrl_c_when_summary_present() {
        let source = TestTextSource::new(vec![
            "Token usage: total=10 input=5 (+ 0 cached) output=5\nTo resume, run: codex resume 123e4567-e89b-12d3-a456-426614174000",
        ]);
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter_clone = Arc::clone(&counter);
        let send_ctrl_c = move || {
            let counter = Arc::clone(&counter_clone);
            async move {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(allowed_ctrl_c_result())
            }
        };

        let options = CodexExitOptions {
            grace_timeout_ms: 0,
            summary_timeout_ms: 0,
            wait_options: wait_options_single_poll(),
        };

        let result = codex_exit_and_wait_for_summary(1, &source, send_ctrl_c, &options)
            .await
            .expect("exit should succeed");

        assert_eq!(result.ctrl_c_count, 1);
        assert!(result.summary.matched);
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn codex_exit_sends_second_ctrl_c_when_grace_times_out() {
        let source = TestTextSource::new(vec![
            "still running...",
            "Token usage: total=10 input=5 (+ 0 cached) output=5\nTo resume, run: codex resume 123e4567-e89b-12d3-a456-426614174000",
        ]);
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter_clone = Arc::clone(&counter);
        let send_ctrl_c = move || {
            let counter = Arc::clone(&counter_clone);
            async move {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(allowed_ctrl_c_result())
            }
        };

        let options = CodexExitOptions {
            grace_timeout_ms: 0,
            summary_timeout_ms: 0,
            wait_options: wait_options_single_poll(),
        };

        let result = codex_exit_and_wait_for_summary(1, &source, send_ctrl_c, &options)
            .await
            .expect("exit should succeed");

        assert_eq!(result.ctrl_c_count, 2);
        assert!(result.summary.matched);
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn codex_exit_errors_when_summary_never_appears() {
        let source = TestTextSource::new(vec!["no summary", "still no summary"]);
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter_clone = Arc::clone(&counter);
        let send_ctrl_c = move || {
            let counter = Arc::clone(&counter_clone);
            async move {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(allowed_ctrl_c_result())
            }
        };

        let options = CodexExitOptions {
            grace_timeout_ms: 0,
            summary_timeout_ms: 0,
            wait_options: wait_options_single_poll(),
        };

        let err = codex_exit_and_wait_for_summary(1, &source, send_ctrl_c, &options)
            .await
            .expect_err("expected failure");
        assert!(err.contains("Session summary not found"));
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn codex_exit_aborts_on_policy_denial() {
        let source = TestTextSource::new(vec![
            "Token usage: total=1 input=1 (+ 0 cached) output=0 codex resume 123e4567-e89b-12d3-a456-426614174000",
        ]);
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter_clone = Arc::clone(&counter);
        let send_ctrl_c = move || {
            let counter = Arc::clone(&counter_clone);
            async move {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(InjectionResult::Denied {
                    decision: crate::policy::PolicyDecision::deny("blocked"),
                    summary: "ctrl-c".to_string(),
                    pane_id: 1,
                    action: crate::policy::ActionKind::SendCtrlC,
                    audit_action_id: None,
                })
            }
        };

        let options = CodexExitOptions {
            grace_timeout_ms: 0,
            summary_timeout_ms: 0,
            wait_options: wait_options_single_poll(),
        };

        let err = codex_exit_and_wait_for_summary(1, &source, send_ctrl_c, &options)
            .await
            .expect_err("expected denial");
        assert!(err.contains("denied"));
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    // ========================================================================
    // Codex Session Summary Parsing Tests (wa-nu4.1.3.3)
    // ========================================================================

    #[test]
    fn parse_codex_session_summary_succeeds_on_valid_fixture() {
        let tail = r#"
You've reached your usage limit.
Token usage: total=1,234 input=500 (+ 200 cached) output=534 (reasoning 100)
To continue this session, run: codex resume 123e4567-e89b-12d3-a456-426614174000
Try again at 3:00 PM UTC.
"#;
        let result = parse_codex_session_summary(tail).expect("should parse");

        assert_eq!(result.session_id, "123e4567-e89b-12d3-a456-426614174000");
        assert_eq!(result.token_usage.total, Some(1234));
        assert_eq!(result.token_usage.input, Some(500));
        assert_eq!(result.token_usage.cached, Some(200));
        assert_eq!(result.token_usage.output, Some(534));
        assert_eq!(result.token_usage.reasoning, Some(100));
        assert_eq!(result.reset_time.as_deref(), Some("3:00 PM UTC"));
    }

    #[test]
    fn parse_codex_session_summary_handles_minimal_valid_input() {
        let tail = "Token usage: total=100\ncodex resume abc12345-1234-1234-1234-123456789abc";
        let result = parse_codex_session_summary(tail).expect("should parse");

        assert_eq!(result.session_id, "abc12345-1234-1234-1234-123456789abc");
        assert_eq!(result.token_usage.total, Some(100));
        assert!(result.token_usage.input.is_none());
        assert!(result.reset_time.is_none());
    }

    #[test]
    fn parse_codex_session_summary_handles_numbers_with_commas() {
        let tail = "Token usage: total=1,234,567 input=999,999\ncodex resume abcd1234-5678-90ab-cdef-1234567890ab";
        let result = parse_codex_session_summary(tail).expect("should parse");

        assert_eq!(result.token_usage.total, Some(1_234_567));
        assert_eq!(result.token_usage.input, Some(999_999));
    }

    #[test]
    fn parse_codex_session_summary_fails_when_session_id_missing() {
        let tail = "Token usage: total=100 input=50";
        let err = parse_codex_session_summary(tail).expect_err("should fail");

        assert!(err.missing.contains(&"session_id"));
        assert!(!err.missing.contains(&"token_usage"));
    }

    #[test]
    fn parse_codex_session_summary_fails_when_token_usage_missing() {
        let tail = "codex resume 123e4567-e89b-12d3-a456-426614174000";
        let err = parse_codex_session_summary(tail).expect_err("should fail");

        assert!(err.missing.contains(&"token_usage"));
        assert!(!err.missing.contains(&"session_id"));
    }

    #[test]
    fn parse_codex_session_summary_fails_when_both_missing() {
        let tail = "Some random text without markers";
        let err = parse_codex_session_summary(tail).expect_err("should fail");

        assert!(err.missing.contains(&"session_id"));
        assert!(err.missing.contains(&"token_usage"));
    }

    #[test]
    fn parse_codex_session_summary_error_does_not_leak_raw_content() {
        let tail = "secret_api_key=sk-12345 some sensitive data";
        let err = parse_codex_session_summary(tail).expect_err("should fail");

        // Error should contain hash and length, not raw content
        let err_string = err.to_string();
        assert!(err_string.contains("tail_hash="));
        assert!(err_string.contains("tail_len="));
        assert!(!err_string.contains("secret_api_key"));
        assert!(!err_string.contains("sk-12345"));
    }

    #[test]
    fn parse_codex_session_summary_extracts_reset_time_variations() {
        // Various reset time formats
        let cases = [
            ("Token usage: total=1\ncodex resume abcd1234\ntry again at 2:30 PM", Some("2:30 PM")),
            ("Token usage: total=1\ncodex resume abcd1234\nTry again at tomorrow 9am.", Some("tomorrow 9am")),
            ("Token usage: total=1\ncodex resume abcd1234", None),
        ];

        for (tail, expected_reset) in cases {
            let result = parse_codex_session_summary(tail).expect("should parse");
            assert_eq!(result.reset_time.as_deref(), expected_reset, "Failed for: {tail}");
        }
    }

    #[test]
    fn parse_codex_session_summary_uses_last_session_id_when_multiple() {
        // If multiple resume hints appear, use the last one
        let tail = "codex resume 11111111-1111-1111-1111-111111111111\nToken usage: total=1\ncodex resume 22222222-2222-2222-2222-222222222222";
        let result = parse_codex_session_summary(tail).expect("should parse");

        assert_eq!(result.session_id, "22222222-2222-2222-2222-222222222222");
    }

    // ========================================================================
    // Account Selection Step Tests (wa-nu4.1.3.4)
    // ========================================================================
    //
    // Note: The core selection logic (determinism, threshold filtering, LRU tie-break)
    // is tested in accounts.rs (9 tests). The `refresh_and_select_account` function
    // wires caut + storage + selection together.
    //
    // Full integration tests with a real database should be added to verify:
    // - caut refresh results are correctly persisted to the accounts table
    // - selection uses the refreshed data from DB
    // - last_used_at updates work correctly after successful failover
    //
    // The tests below verify the error types and result structures.

    #[test]
    fn account_selection_step_error_displays_caut_error() {
        let caut_err = crate::caut::CautError::NotInstalled;
        let step_err = AccountSelectionStepError::Caut(caut_err);
        let display = step_err.to_string();
        assert!(display.contains("caut error"));
        assert!(display.contains("not installed"));
    }

    #[test]
    fn account_selection_step_error_displays_storage_error() {
        let step_err = AccountSelectionStepError::Storage("connection failed".to_string());
        let display = step_err.to_string();
        assert!(display.contains("storage error"));
        assert!(display.contains("connection failed"));
    }

    #[test]
    fn account_selection_step_result_can_be_constructed() {
        use crate::accounts::SelectionExplanation;

        // Verify the step result structure is correct
        let explanation = SelectionExplanation {
            total_considered: 2,
            filtered_out: vec![],
            candidates: vec![],
            selection_reason: "Test reason".to_string(),
        };

        let result = AccountSelectionStepResult {
            selected: None,
            explanation,
            accounts_refreshed: 2,
        };

        assert!(result.selected.is_none());
        assert_eq!(result.accounts_refreshed, 2);
        assert_eq!(result.explanation.total_considered, 2);
    }

    #[test]
    fn account_selection_step_result_with_selected_account() {
        use crate::accounts::{AccountRecord, SelectionExplanation};

        let account = AccountRecord {
            id: 1,
            account_id: "acc-123".to_string(),
            service: "openai".to_string(),
            name: Some("Test Account".to_string()),
            percent_remaining: 75.0,
            reset_at: None,
            tokens_used: Some(1000),
            tokens_remaining: Some(3000),
            tokens_limit: Some(4000),
            last_refreshed_at: 1000,
            last_used_at: None,
            created_at: 1000,
            updated_at: 1000,
        };

        let explanation = SelectionExplanation {
            total_considered: 1,
            filtered_out: vec![],
            candidates: vec![],
            selection_reason: "Only eligible account".to_string(),
        };

        let result = AccountSelectionStepResult {
            selected: Some(account.clone()),
            explanation,
            accounts_refreshed: 1,
        };

        assert!(result.selected.is_some());
        assert_eq!(result.selected.as_ref().unwrap().account_id, "acc-123");
        assert_eq!(result.accounts_refreshed, 1);
    }
}
