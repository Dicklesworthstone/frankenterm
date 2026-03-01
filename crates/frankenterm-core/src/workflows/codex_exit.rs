//! Codex usage-limit and session summary helpers.
//!
//! Provides functions for exiting Codex (via Ctrl-C), waiting for session
//! summary markers, parsing token usage, and persisting session records.
//!
//! Extracted from `workflows.rs` as part of strangler fig refactoring (ft-c45am).

#[allow(clippy::wildcard_imports)]
use super::*;

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
pub fn ctrl_c_injection_ok(result: InjectionResult) -> Result<(), String> {
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
pub async fn codex_exit_and_wait_for_summary<S, F, Fut>(
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
    pub fn has_any(&self) -> bool {
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
pub(super) static CODEX_RESUME_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)codex resume\s+(?P<session_id>[0-9a-fA-F-]{8,})").expect("codex resume regex")
});
#[allow(dead_code)]
pub(super) static CODEX_RESET_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)try again at\s+(?P<reset_time>[^.\n]+)").expect("codex reset time regex")
});
#[allow(dead_code)]
pub(super) static CODEX_TOTAL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)total\s*=\s*([\d,]+)").expect("total regex"));
#[allow(dead_code)]
pub(super) static CODEX_INPUT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)input\s*=\s*([\d,]+)").expect("input regex"));
#[allow(dead_code)]
pub(super) static CODEX_OUTPUT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)output\s*=\s*([\d,]+)").expect("output regex"));
#[allow(dead_code)]
pub(super) static CODEX_CACHED_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\(\+\s*([\d,]+)\s+cached\)").expect("cached regex"));
#[allow(dead_code)]
pub(super) static CODEX_REASONING_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\(reasoning\s+([\d,]+)\)").expect("reasoning regex"));

#[allow(dead_code)]
pub(super) fn parse_number(raw: &str) -> Option<i64> {
    let cleaned = raw.replace(',', "");
    cleaned.parse::<i64>().ok()
}

#[allow(dead_code)]
pub(super) fn capture_number(regex: &Regex, text: &str) -> Option<i64> {
    regex
        .captures(text)
        .and_then(|caps| caps.get(1).map(|m| m.as_str()))
        .and_then(parse_number)
}

#[allow(dead_code)]
pub(super) fn extract_token_usage(line: &str) -> CodexTokenUsage {
    CodexTokenUsage {
        total: capture_number(&CODEX_TOTAL_RE, line),
        input: capture_number(&CODEX_INPUT_RE, line),
        output: capture_number(&CODEX_OUTPUT_RE, line),
        cached: capture_number(&CODEX_CACHED_RE, line),
        reasoning: capture_number(&CODEX_REASONING_RE, line),
    }
}

#[allow(dead_code)]
pub(super) fn find_token_usage_line(tail: &str) -> Option<&str> {
    tail.lines().rfind(|line| line.contains("Token usage:"))
}

#[allow(dead_code)]
pub(super) fn find_session_id(tail: &str) -> Option<String> {
    CODEX_RESUME_RE
        .captures_iter(tail)
        .filter_map(|caps| caps.name("session_id").map(|m| m.as_str().to_string()))
        .last()
}

#[allow(dead_code)]
pub(super) fn find_reset_time(tail: &str) -> Option<String> {
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
pub fn parse_codex_session_summary(
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
pub fn codex_session_record_from_summary(
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
pub async fn persist_codex_session_summary(
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

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // parse_number
    // ========================================================================

    #[test]
    fn parse_number_simple_integer() {
        assert_eq!(parse_number("42"), Some(42));
    }

    #[test]
    fn parse_number_with_commas() {
        assert_eq!(parse_number("1,234,567"), Some(1_234_567));
    }

    #[test]
    fn parse_number_large() {
        assert_eq!(parse_number("10,000,000"), Some(10_000_000));
    }

    #[test]
    fn parse_number_zero() {
        assert_eq!(parse_number("0"), Some(0));
    }

    #[test]
    fn parse_number_invalid() {
        assert_eq!(parse_number("abc"), None);
        assert_eq!(parse_number(""), None);
        assert_eq!(parse_number("12.34"), None);
    }

    // ========================================================================
    // capture_number
    // ========================================================================

    #[test]
    fn capture_number_from_total_line() {
        let line = "Token usage: total = 150,000";
        assert_eq!(capture_number(&CODEX_TOTAL_RE, line), Some(150_000));
    }

    #[test]
    fn capture_number_from_input_line() {
        let line = "input = 80,000";
        assert_eq!(capture_number(&CODEX_INPUT_RE, line), Some(80_000));
    }

    #[test]
    fn capture_number_from_output_line() {
        let line = "output = 20,000";
        assert_eq!(capture_number(&CODEX_OUTPUT_RE, line), Some(20_000));
    }

    #[test]
    fn capture_number_from_cached_line() {
        let line = "(+ 50,000 cached)";
        assert_eq!(capture_number(&CODEX_CACHED_RE, line), Some(50_000));
    }

    #[test]
    fn capture_number_from_reasoning_line() {
        let line = "(reasoning 10,000)";
        assert_eq!(capture_number(&CODEX_REASONING_RE, line), Some(10_000));
    }

    #[test]
    fn capture_number_no_match() {
        assert_eq!(capture_number(&CODEX_TOTAL_RE, "nothing here"), None);
    }

    // ========================================================================
    // extract_token_usage
    // ========================================================================

    #[test]
    fn extract_token_usage_full_line() {
        let line = "Token usage: total = 150,000 input = 80,000 output = 20,000 (+ 50,000 cached) (reasoning 10,000)";
        let usage = extract_token_usage(line);
        assert_eq!(usage.total, Some(150_000));
        assert_eq!(usage.input, Some(80_000));
        assert_eq!(usage.output, Some(20_000));
        assert_eq!(usage.cached, Some(50_000));
        assert_eq!(usage.reasoning, Some(10_000));
        assert!(usage.has_any());
    }

    #[test]
    fn extract_token_usage_partial() {
        let line = "Token usage: total = 5000 input = 3000";
        let usage = extract_token_usage(line);
        assert_eq!(usage.total, Some(5000));
        assert_eq!(usage.input, Some(3000));
        assert_eq!(usage.output, None);
        assert_eq!(usage.cached, None);
        assert_eq!(usage.reasoning, None);
        assert!(usage.has_any());
    }

    #[test]
    fn extract_token_usage_empty() {
        let usage = extract_token_usage("no tokens here");
        assert_eq!(usage.total, None);
        assert_eq!(usage.input, None);
        assert_eq!(usage.output, None);
        assert_eq!(usage.cached, None);
        assert_eq!(usage.reasoning, None);
        assert!(!usage.has_any());
    }

    // ========================================================================
    // CodexTokenUsage::has_any
    // ========================================================================

    #[test]
    fn codex_token_usage_has_any_all_none() {
        let usage = CodexTokenUsage {
            total: None,
            input: None,
            output: None,
            cached: None,
            reasoning: None,
        };
        assert!(!usage.has_any());
    }

    #[test]
    fn codex_token_usage_has_any_one_field() {
        let usage = CodexTokenUsage {
            total: None,
            input: None,
            output: Some(100),
            cached: None,
            reasoning: None,
        };
        assert!(usage.has_any());
    }

    // ========================================================================
    // find_token_usage_line
    // ========================================================================

    #[test]
    fn find_token_usage_line_present() {
        let tail = "Some output\nToken usage: total = 5000\nMore output\n";
        let line = find_token_usage_line(tail);
        assert!(line.is_some());
        assert!(line.unwrap().contains("total = 5000"));
    }

    #[test]
    fn find_token_usage_line_missing() {
        let tail = "No token usage here\nJust regular text\n";
        assert!(find_token_usage_line(tail).is_none());
    }

    #[test]
    fn find_token_usage_line_picks_last() {
        let tail = "Token usage: total = 100\nOther line\nToken usage: total = 200\n";
        let line = find_token_usage_line(tail);
        assert!(line.is_some());
        assert!(line.unwrap().contains("total = 200"));
    }

    // ========================================================================
    // find_session_id
    // ========================================================================

    #[test]
    fn find_session_id_present() {
        let tail = "codex resume a1b2c3d4-e5f6-7890-abcd-ef1234567890\n";
        let id = find_session_id(tail);
        assert_eq!(id, Some("a1b2c3d4-e5f6-7890-abcd-ef1234567890".to_string()));
    }

    #[test]
    fn find_session_id_case_insensitive() {
        let tail = "CODEX RESUME abcdef12-3456-7890-abcd-ef1234567890\n";
        let id = find_session_id(tail);
        assert!(id.is_some());
    }

    #[test]
    fn find_session_id_missing() {
        let tail = "no resume command here\n";
        assert!(find_session_id(tail).is_none());
    }

    #[test]
    fn find_session_id_picks_last() {
        let tail = "codex resume aaaa1111-2222-3333-4444-555566667777\nsome other text\ncodex resume bbbb1111-2222-3333-4444-555566667777\n";
        let id = find_session_id(tail);
        assert_eq!(id, Some("bbbb1111-2222-3333-4444-555566667777".to_string()));
    }

    // ========================================================================
    // find_reset_time
    // ========================================================================

    #[test]
    fn find_reset_time_present() {
        let tail = "try again at 3:00 PM EST\n";
        let reset = find_reset_time(tail);
        assert_eq!(reset, Some("3:00 PM EST".to_string()));
    }

    #[test]
    fn find_reset_time_missing() {
        let tail = "no reset info\n";
        assert!(find_reset_time(tail).is_none());
    }

    // ========================================================================
    // parse_codex_session_summary
    // ========================================================================

    #[test]
    fn parse_codex_session_summary_complete() {
        let tail = "\
Session ended.
Token usage: total = 150,000 input = 80,000 output = 20,000 (+ 50,000 cached)
To continue, run: codex resume a1b2c3d4-e5f6-7890-abcd-ef1234567890
Rate limit hit, try again at 3:00 PM EST
";
        let result = parse_codex_session_summary(tail);
        assert!(result.is_ok());
        let summary = result.unwrap();
        assert_eq!(summary.session_id, "a1b2c3d4-e5f6-7890-abcd-ef1234567890");
        assert_eq!(summary.token_usage.total, Some(150_000));
        assert_eq!(summary.token_usage.input, Some(80_000));
        assert_eq!(summary.token_usage.output, Some(20_000));
        assert_eq!(summary.token_usage.cached, Some(50_000));
        assert_eq!(summary.reset_time, Some("3:00 PM EST".to_string()));
    }

    #[test]
    fn parse_codex_session_summary_no_reset_time() {
        let tail = "\
Token usage: total = 5000 input = 3000
codex resume aaaabbbb-cccc-dddd-eeee-ffffffffffff
";
        let result = parse_codex_session_summary(tail);
        assert!(result.is_ok());
        let summary = result.unwrap();
        assert_eq!(summary.session_id, "aaaabbbb-cccc-dddd-eeee-ffffffffffff");
        assert!(summary.reset_time.is_none());
    }

    #[test]
    fn parse_codex_session_summary_missing_session_id() {
        let tail = "Token usage: total = 5000\nNo resume command\n";
        let result = parse_codex_session_summary(tail);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.missing.contains(&"session_id"));
    }

    #[test]
    fn parse_codex_session_summary_missing_token_usage() {
        let tail = "codex resume aaaa1111-2222-3333-4444-555566667777\nNo tokens\n";
        let result = parse_codex_session_summary(tail);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.missing.contains(&"token_usage"));
    }

    #[test]
    fn parse_codex_session_summary_missing_both() {
        let tail = "nothing useful here\n";
        let result = parse_codex_session_summary(tail);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.missing.contains(&"session_id"));
        assert!(err.missing.contains(&"token_usage"));
        assert!(err.tail_len > 0);
    }

    #[test]
    fn parse_codex_session_summary_error_display() {
        let err = CodexSessionParseError {
            missing: vec!["session_id"],
            tail_hash: 0x1234_5678_9abc_def0,
            tail_len: 100,
        };
        let display = format!("{err}");
        assert!(display.contains("session_id"));
        assert!(display.contains("tail_hash="));
        assert!(display.contains("tail_len=100"));
    }

    // ========================================================================
    // codex_session_record_from_summary
    // ========================================================================

    #[test]
    fn codex_session_record_from_summary_populates_fields() {
        let summary = CodexSessionSummary {
            session_id: "test-session-id".to_string(),
            token_usage: CodexTokenUsage {
                total: Some(10_000),
                input: Some(7_000),
                output: Some(3_000),
                cached: Some(2_000),
                reasoning: Some(500),
            },
            reset_time: None,
        };
        let record = codex_session_record_from_summary(42, &summary);
        assert_eq!(record.session_id, Some("test-session-id".to_string()));
        assert_eq!(record.total_tokens, Some(10_000));
        assert_eq!(record.input_tokens, Some(7_000));
        assert_eq!(record.output_tokens, Some(3_000));
        assert_eq!(record.cached_tokens, Some(2_000));
        assert_eq!(record.reasoning_tokens, Some(500));
    }

    // ========================================================================
    // CodexExitOptions default
    // ========================================================================

    #[test]
    fn codex_exit_options_default() {
        let opts = CodexExitOptions::default();
        assert_eq!(opts.grace_timeout_ms, 2_000);
        assert_eq!(opts.summary_timeout_ms, 20_000);
    }

    // ========================================================================
    // DeviceCode parsing
    // ========================================================================

    #[test]
    fn parse_device_code_standard() {
        let tail = "Enter code: ABCD-1234\n";
        let result = parse_device_code(tail);
        assert!(result.is_ok());
        let dc = result.unwrap();
        assert_eq!(dc.code, "ABCD-1234");
    }

    #[test]
    fn parse_device_code_with_url() {
        let tail = "Visit https://auth.example.com/device and enter code: WXYZ-5678\n";
        let result = parse_device_code(tail);
        assert!(result.is_ok());
        let dc = result.unwrap();
        assert_eq!(dc.code, "WXYZ-5678");
        assert!(dc.url.is_some());
        assert!(dc.url.unwrap().contains("auth.example.com/device"));
    }

    #[test]
    fn parse_device_code_not_found() {
        let tail = "No device code here\n";
        let result = parse_device_code(tail);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.expected.contains("device code"));
        assert!(err.tail_len > 0);
    }

    #[test]
    fn parse_device_code_lowercase_converted_to_upper() {
        let tail = "code: abcd-efgh\n";
        let result = parse_device_code(tail);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().code, "ABCD-EFGH");
    }

    // ========================================================================
    // validate_device_code
    // ========================================================================

    #[test]
    fn validate_device_code_valid() {
        assert!(validate_device_code("ABCD-1234"));
        assert!(validate_device_code("WXYZ-56789"));
        assert!(validate_device_code("A1B2C3D4-E5F6G7H8"));
    }

    #[test]
    fn validate_device_code_too_short() {
        assert!(!validate_device_code("AB-12"));
        assert!(!validate_device_code("ABC-123"));
    }

    #[test]
    fn validate_device_code_no_dash() {
        assert!(!validate_device_code("ABCD1234"));
    }

    #[test]
    fn validate_device_code_multiple_dashes() {
        assert!(!validate_device_code("AB-CD-1234"));
    }

    #[test]
    fn validate_device_code_special_chars() {
        assert!(!validate_device_code("AB@D-1234"));
        assert!(!validate_device_code("ABCD-12!4"));
    }

    #[test]
    fn validate_device_code_empty() {
        assert!(!validate_device_code(""));
    }

    // ========================================================================
    // DEVICE_AUTH_LOGIN_COMMAND
    // ========================================================================

    #[test]
    fn device_auth_login_command_format() {
        assert!(DEVICE_AUTH_LOGIN_COMMAND.ends_with('\n'));
        assert!(DEVICE_AUTH_LOGIN_COMMAND.contains("login"));
        assert!(DEVICE_AUTH_LOGIN_COMMAND.contains("device-auth"));
    }

    // ========================================================================
    // DeviceCodeParseError display
    // ========================================================================

    #[test]
    fn device_code_parse_error_display() {
        let err = DeviceCodeParseError {
            expected: "test pattern",
            tail_hash: 0xdeadbeef,
            tail_len: 50,
        };
        let display = format!("{err}");
        assert!(display.contains("test pattern"));
        assert!(display.contains("tail_len=50"));
    }
}
