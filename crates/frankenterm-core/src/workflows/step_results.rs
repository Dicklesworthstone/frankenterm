//! Step result and wait condition types for workflow execution.
//!
//! Pure data types that define the outcome of workflow step execution
//! and conditions for pausing execution.
//!
//! Extracted from `workflows.rs` as part of strangler fig refactoring (ft-c45am).

#[allow(clippy::wildcard_imports)]
use super::*;

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
    /// Jump to a specific step index.
    JumpTo {
        /// The target step index.
        step: usize,
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

    /// Create a JumpTo result.
    #[must_use]
    pub fn jump_to(step: usize) -> Self {
        Self::JumpTo { step }
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

/// Matchers for text-based wait conditions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TextMatch {
    /// Match a substring.
    Substring { value: String },
    /// Match a regex pattern.
    Regex { pattern: String },
}

impl TextMatch {
    /// Create a substring matcher.
    #[must_use]
    pub fn substring(value: impl Into<String>) -> Self {
        Self::Substring {
            value: value.into(),
        }
    }

    /// Create a regex matcher from a pattern string.
    #[must_use]
    pub fn regex(pattern: impl Into<String>) -> Self {
        Self::Regex {
            pattern: pattern.into(),
        }
    }

    pub(super) fn to_wait_matcher(&self) -> crate::Result<WaitMatcher> {
        match self {
            Self::Substring { value } => Ok(WaitMatcher::substring(value.clone())),
            Self::Regex { pattern } => {
                let regex = fancy_regex::Regex::new(pattern)
                    .map_err(|e| crate::error::PatternError::InvalidRegex(e.to_string()))?;
                Ok(WaitMatcher::regex(regex))
            }
        }
    }

    pub(super) fn description(&self) -> String {
        match self {
            Self::Substring { value } => format!(
                "substring(len={}, hash={:016x})",
                value.len(),
                stable_hash(value.as_bytes())
            ),
            Self::Regex { pattern } => format!(
                "regex(len={}, hash={:016x})",
                pattern.len(),
                stable_hash(pattern.as_bytes())
            ),
        }
    }
}

/// Conditions that a workflow can wait for before proceeding.
///
/// Wait conditions pause workflow execution until satisfied:
/// - `Pattern`: Wait for a pattern rule to match on a pane
/// - `PaneIdle`: Wait for a pane to become idle (no output)
/// - `StableTail`: Wait for pane output to stop changing for a duration
/// - `TextMatch`: Wait for substring/regex match in pane text
/// - `Sleep`: Wait for a fixed duration
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
    /// Wait for pane output tail to be stable for a duration
    StableTail {
        /// Pane to monitor (None = workflow's target pane)
        pane_id: Option<u64>,
        /// Required stability duration in milliseconds
        stable_for_ms: u64,
    },
    /// Wait for a text match (substring or regex) to appear in pane output.
    TextMatch {
        /// Pane to monitor (None = workflow's target pane)
        pane_id: Option<u64>,
        /// Matcher to apply to pane text
        matcher: TextMatch,
    },
    /// Wait for a fixed duration (sleep).
    Sleep {
        /// Duration to sleep in milliseconds
        duration_ms: u64,
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

    /// Create a StableTail wait condition for the workflow's target pane
    #[must_use]
    pub fn stable_tail(stable_for_ms: u64) -> Self {
        Self::StableTail {
            pane_id: None,
            stable_for_ms,
        }
    }

    /// Create a StableTail wait condition for a specific pane
    #[must_use]
    pub fn stable_tail_on(pane_id: u64, stable_for_ms: u64) -> Self {
        Self::StableTail {
            pane_id: Some(pane_id),
            stable_for_ms,
        }
    }

    /// Create a TextMatch wait condition for the workflow's target pane.
    #[must_use]
    pub fn text_match(matcher: TextMatch) -> Self {
        Self::TextMatch {
            pane_id: None,
            matcher,
        }
    }

    /// Create a TextMatch wait condition for a specific pane.
    #[must_use]
    pub fn text_match_on_pane(pane_id: u64, matcher: TextMatch) -> Self {
        Self::TextMatch {
            pane_id: Some(pane_id),
            matcher,
        }
    }

    /// Create a Sleep wait condition.
    #[must_use]
    pub fn sleep(duration_ms: u64) -> Self {
        Self::Sleep { duration_ms }
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
            Self::Pattern { pane_id, .. }
            | Self::PaneIdle { pane_id, .. }
            | Self::StableTail { pane_id, .. }
            | Self::TextMatch { pane_id, .. } => *pane_id,
            Self::Sleep { .. } | Self::External { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // StepResult constructors and predicates
    // ========================================================================

    #[test]
    fn step_result_continue() {
        let r = StepResult::cont();
        assert!(r.is_continue());
        assert!(!r.is_done());
        assert!(!r.is_terminal());
        assert!(!r.is_send_text());
    }

    #[test]
    fn step_result_done() {
        let r = StepResult::done(serde_json::json!({"status": "ok"}));
        assert!(r.is_done());
        assert!(r.is_terminal());
        assert!(!r.is_continue());
    }

    #[test]
    fn step_result_done_empty() {
        let r = StepResult::done_empty();
        assert!(r.is_done());
        if let StepResult::Done { result } = &r {
            assert!(result.is_null());
        } else {
            panic!("Expected Done variant");
        }
    }

    #[test]
    fn step_result_retry() {
        let r = StepResult::retry(5000);
        if let StepResult::Retry { delay_ms } = r {
            assert_eq!(delay_ms, 5000);
        } else {
            panic!("Expected Retry variant");
        }
        assert!(!r.is_terminal());
    }

    #[test]
    fn step_result_abort() {
        let r = StepResult::abort("something failed");
        assert!(r.is_terminal());
        assert!(!r.is_done());
        if let StepResult::Abort { reason } = &r {
            assert_eq!(reason, "something failed");
        }
    }

    #[test]
    fn step_result_wait_for() {
        let r = StepResult::wait_for(WaitCondition::pattern("rule1"));
        if let StepResult::WaitFor {
            condition,
            timeout_ms,
        } = &r
        {
            assert!(timeout_ms.is_none());
            assert!(matches!(condition, WaitCondition::Pattern { .. }));
        } else {
            panic!("Expected WaitFor variant");
        }
    }

    #[test]
    fn step_result_wait_for_with_timeout() {
        let r =
            StepResult::wait_for_with_timeout(WaitCondition::pane_idle(1000), 5000);
        if let StepResult::WaitFor {
            timeout_ms,
            ..
        } = &r
        {
            assert_eq!(*timeout_ms, Some(5000));
        }
    }

    #[test]
    fn step_result_send_text() {
        let r = StepResult::send_text("ls -la\n");
        assert!(r.is_send_text());
        if let StepResult::SendText {
            text,
            wait_for,
            wait_timeout_ms,
        } = &r
        {
            assert_eq!(text, "ls -la\n");
            assert!(wait_for.is_none());
            assert!(wait_timeout_ms.is_none());
        }
    }

    #[test]
    fn step_result_send_text_and_wait() {
        let r = StepResult::send_text_and_wait(
            "cargo test\n",
            WaitCondition::text_match(TextMatch::substring("test result")),
            30_000,
        );
        assert!(r.is_send_text());
        if let StepResult::SendText {
            text,
            wait_for,
            wait_timeout_ms,
        } = &r
        {
            assert_eq!(text, "cargo test\n");
            assert!(wait_for.is_some());
            assert_eq!(*wait_timeout_ms, Some(30_000));
        }
    }

    #[test]
    fn step_result_jump_to() {
        let r = StepResult::jump_to(5);
        if let StepResult::JumpTo { step } = r {
            assert_eq!(step, 5);
        } else {
            panic!("Expected JumpTo variant");
        }
    }

    // ========================================================================
    // StepResult serde roundtrip
    // ========================================================================

    #[test]
    fn step_result_serde_all_variants() {
        let variants = vec![
            StepResult::cont(),
            StepResult::done(serde_json::json!(42)),
            StepResult::done_empty(),
            StepResult::retry(1000),
            StepResult::abort("error"),
            StepResult::wait_for(WaitCondition::sleep(500)),
            StepResult::send_text("hello"),
            StepResult::jump_to(3),
        ];
        for v in variants {
            let json = serde_json::to_string(&v).unwrap();
            let parsed: StepResult = serde_json::from_str(&json).unwrap();
            let json2 = serde_json::to_string(&parsed).unwrap();
            assert_eq!(json, json2, "roundtrip failed for variant");
        }
    }

    // ========================================================================
    // TextMatch
    // ========================================================================

    #[test]
    fn text_match_substring() {
        let m = TextMatch::substring("hello");
        if let TextMatch::Substring { value } = &m {
            assert_eq!(value, "hello");
        } else {
            panic!("Expected Substring variant");
        }
    }

    #[test]
    fn text_match_regex() {
        let m = TextMatch::regex(r"\d+");
        if let TextMatch::Regex { pattern } = &m {
            assert_eq!(pattern, r"\d+");
        }
    }

    #[test]
    fn text_match_serde_roundtrip() {
        let variants = vec![
            TextMatch::substring("hello world"),
            TextMatch::regex(r"^\d{3}-\d{4}$"),
        ];
        for v in &variants {
            let json = serde_json::to_string(v).unwrap();
            let parsed: TextMatch = serde_json::from_str(&json).unwrap();
            assert_eq!(v, &parsed);
        }
    }

    #[test]
    fn text_match_description() {
        let sub = TextMatch::substring("test");
        let desc = sub.description();
        assert!(desc.starts_with("substring("));
        assert!(desc.contains("len=4"));

        let re = TextMatch::regex(r"\d+");
        let desc = re.description();
        assert!(desc.starts_with("regex("));
    }

    #[test]
    fn text_match_to_wait_matcher_substring() {
        let m = TextMatch::substring("prompt$");
        let wm = m.to_wait_matcher();
        assert!(wm.is_ok());
    }

    #[test]
    fn text_match_to_wait_matcher_valid_regex() {
        let m = TextMatch::regex(r"^\$ $");
        let wm = m.to_wait_matcher();
        assert!(wm.is_ok());
    }

    #[test]
    fn text_match_to_wait_matcher_invalid_regex() {
        let m = TextMatch::regex(r"[invalid");
        let wm = m.to_wait_matcher();
        assert!(wm.is_err());
    }

    // ========================================================================
    // WaitCondition constructors and pane_id
    // ========================================================================

    #[test]
    fn wait_condition_pattern() {
        let c = WaitCondition::pattern("my_rule");
        assert!(c.pane_id().is_none());
        if let WaitCondition::Pattern { rule_id, pane_id } = &c {
            assert_eq!(rule_id, "my_rule");
            assert!(pane_id.is_none());
        }
    }

    #[test]
    fn wait_condition_pattern_on_pane() {
        let c = WaitCondition::pattern_on_pane(42, "rule_x");
        assert_eq!(c.pane_id(), Some(42));
    }

    #[test]
    fn wait_condition_pane_idle() {
        let c = WaitCondition::pane_idle(2000);
        assert!(c.pane_id().is_none());
        if let WaitCondition::PaneIdle {
            idle_threshold_ms, ..
        } = &c
        {
            assert_eq!(*idle_threshold_ms, 2000);
        }
    }

    #[test]
    fn wait_condition_pane_idle_on() {
        let c = WaitCondition::pane_idle_on(10, 5000);
        assert_eq!(c.pane_id(), Some(10));
    }

    #[test]
    fn wait_condition_stable_tail() {
        let c = WaitCondition::stable_tail(3000);
        assert!(c.pane_id().is_none());
    }

    #[test]
    fn wait_condition_stable_tail_on() {
        let c = WaitCondition::stable_tail_on(7, 1000);
        assert_eq!(c.pane_id(), Some(7));
    }

    #[test]
    fn wait_condition_text_match() {
        let c = WaitCondition::text_match(TextMatch::substring("done"));
        assert!(c.pane_id().is_none());
    }

    #[test]
    fn wait_condition_text_match_on_pane() {
        let c = WaitCondition::text_match_on_pane(99, TextMatch::regex(r"\$"));
        assert_eq!(c.pane_id(), Some(99));
    }

    #[test]
    fn wait_condition_sleep() {
        let c = WaitCondition::sleep(500);
        assert!(c.pane_id().is_none());
        if let WaitCondition::Sleep { duration_ms } = &c {
            assert_eq!(*duration_ms, 500);
        }
    }

    #[test]
    fn wait_condition_external() {
        let c = WaitCondition::external("signal_key");
        assert!(c.pane_id().is_none());
        if let WaitCondition::External { key } = &c {
            assert_eq!(key, "signal_key");
        }
    }

    // ========================================================================
    // WaitCondition serde roundtrip
    // ========================================================================

    #[test]
    fn wait_condition_serde_all_variants() {
        let variants = vec![
            WaitCondition::pattern("rule1"),
            WaitCondition::pattern_on_pane(5, "rule2"),
            WaitCondition::pane_idle(1000),
            WaitCondition::pane_idle_on(3, 2000),
            WaitCondition::stable_tail(500),
            WaitCondition::stable_tail_on(8, 750),
            WaitCondition::text_match(TextMatch::substring("hello")),
            WaitCondition::text_match_on_pane(12, TextMatch::regex(r"\d+")),
            WaitCondition::sleep(100),
            WaitCondition::external("my_key"),
        ];
        for v in &variants {
            let json = serde_json::to_string(v).unwrap();
            let parsed: WaitCondition = serde_json::from_str(&json).unwrap();
            assert_eq!(v, &parsed);
        }
    }
}
