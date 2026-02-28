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
