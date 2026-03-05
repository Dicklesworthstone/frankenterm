//! Wait condition execution and polling logic.
//!
//! Implements the runtime evaluation of `WaitCondition` variants including
//! pattern matching, pane idle detection, stable tail monitoring, text matching,
//! sleep, and external signals.
//!
//! Extracted from `workflows.rs` as part of strangler fig refactoring (ft-c45am).

#[allow(clippy::wildcard_imports)]
use super::*;

// ============================================================================
// Wait Condition Execution
// ============================================================================

use crate::ingest::Osc133State;
use crate::patterns::PatternEngine;
use crate::runtime_compat::sleep;

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
            poll_max: Duration::from_secs(1),
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
            WaitCondition::StableTail {
                pane_id,
                stable_for_ms,
            } => {
                let target_pane = pane_id.unwrap_or(context_pane_id);
                self.execute_stable_tail_wait(target_pane, *stable_for_ms, timeout)
                    .await
            }
            WaitCondition::TextMatch { pane_id, matcher } => {
                let target_pane = pane_id.unwrap_or(context_pane_id);
                self.execute_text_match_wait(target_pane, matcher, timeout)
                    .await
            }
            WaitCondition::Sleep { duration_ms } => {
                let dur = Duration::from_millis(*duration_ms);
                let capped = dur.min(timeout);
                sleep(capped).await;
                Ok(WaitConditionResult::Satisfied {
                    elapsed_ms: capped.as_millis() as u64,
                    polls: 0,
                    context: Some("sleep completed".to_string()),
                })
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

    /// Execute a stable tail wait condition.
    ///
    /// Waits until the tail content remains unchanged for `stable_for_ms`.
    async fn execute_stable_tail_wait(
        &self,
        pane_id: u64,
        stable_for_ms: u64,
        timeout: Duration,
    ) -> crate::Result<WaitConditionResult> {
        let start = Instant::now();
        let deadline = start + timeout;
        let mut polls = 0usize;
        let mut interval = self.options.poll_initial;
        let stable_for = Duration::from_millis(stable_for_ms);

        let mut last_hash: Option<u64> = None;
        let mut last_change_at = Instant::now();
        let mut last_tail_len: usize = 0;

        #[allow(clippy::cast_possible_truncation)]
        let timeout_ms = timeout.as_millis() as u64;
        tracing::info!(pane_id, stable_for_ms, timeout_ms, "stable_tail_wait start");

        loop {
            polls += 1;

            let text = self.source.get_text(pane_id, false).await?;
            let tail = tail_text(&text, self.options.tail_lines);
            let tail_hash = stable_hash(tail.as_bytes());
            let tail_len = tail.len();

            if let Some(prev_hash) = last_hash {
                if prev_hash == tail_hash {
                    let stable_duration = Instant::now().saturating_duration_since(last_change_at);
                    if stable_duration >= stable_for {
                        let elapsed_ms = elapsed_ms(start);
                        tracing::info!(
                            pane_id,
                            elapsed_ms,
                            polls,
                            stable_duration_ms = %stable_duration.as_millis(),
                            tail_len,
                            "stable_tail_wait satisfied"
                        );
                        return Ok(WaitConditionResult::Satisfied {
                            elapsed_ms,
                            polls,
                            context: Some(format!(
                                "stable for {}ms (tail_len={}, hash={:016x})",
                                stable_duration.as_millis(),
                                tail_len,
                                tail_hash
                            )),
                        });
                    }
                } else {
                    last_change_at = Instant::now();
                }
            } else {
                last_change_at = Instant::now();
            }

            last_hash = Some(tail_hash);
            if tail_len != last_tail_len {
                last_tail_len = tail_len;
            }

            let now = Instant::now();
            if now >= deadline || polls >= self.options.max_polls {
                let elapsed_ms = elapsed_ms(start);
                tracing::info!(pane_id, elapsed_ms, polls, "stable_tail_wait timeout");
                let last_observed = last_hash.map(|hash| {
                    let tail_len = last_tail_len;
                    format!(
                        "last_hash={:016x} tail_len={} stable_for_ms={}",
                        hash, tail_len, stable_for_ms
                    )
                });
                return Ok(WaitConditionResult::TimedOut {
                    elapsed_ms,
                    polls,
                    last_observed,
                });
            }

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

    async fn execute_text_match_wait(
        &self,
        pane_id: u64,
        matcher: &TextMatch,
        timeout: Duration,
    ) -> crate::Result<WaitConditionResult> {
        let wait_matcher = matcher.to_wait_matcher()?;
        let waiter = PaneWaiter::new(self.source).with_options(WaitOptions {
            tail_lines: self.options.tail_lines,
            escapes: false,
            poll_initial: self.options.poll_initial,
            poll_max: self.options.poll_max,
            max_polls: self.options.max_polls,
        });

        let result = waiter.wait_for(pane_id, &wait_matcher, timeout).await?;
        match result {
            WaitResult::Matched { elapsed_ms, polls } => Ok(WaitConditionResult::Satisfied {
                elapsed_ms,
                polls,
                context: Some(format!("matched {}", matcher.description())),
            }),
            WaitResult::TimedOut {
                elapsed_ms, polls, ..
            } => Ok(WaitConditionResult::TimedOut {
                elapsed_ms,
                polls,
                last_observed: Some(format!("timeout {}", matcher.description())),
            }),
        }
    }
}

/// Heuristic idle check based on pane text patterns.
///
/// This is a best-effort fallback when OSC 133 shell integration is not available.
/// It looks for common shell prompt patterns in the last few lines.
///
/// Returns (is_idle, description) where description explains the heuristic result.
#[allow(clippy::items_after_statements)]
pub(super) fn heuristic_idle_check(text: &str, tail_lines: usize) -> (bool, String) {
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
pub(super) fn truncate_for_log(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len.saturating_sub(3)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::{Osc133State, ShellState};
    use crate::patterns::{AgentType, PatternEngine, PatternPack, RuleDef, Severity};
    use crate::runtime_compat::CompatRuntime;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ========================================================================
    // MockPaneSource (local copy for wait_execution tests)
    // ========================================================================

    struct MockPaneSource {
        texts: StdMutex<Vec<String>>,
        call_count: AtomicUsize,
    }

    impl MockPaneSource {
        fn new(texts: Vec<String>) -> Self {
            Self {
                texts: StdMutex::new(texts),
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

    fn test_runtime() -> crate::runtime_compat::Runtime {
        crate::runtime_compat::RuntimeBuilder::multi_thread()
            .build()
            .unwrap()
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
            last_observed: Some("some state".to_string()),
        };
        assert!(!result.is_satisfied());
        assert!(result.is_timed_out());
        assert_eq!(result.elapsed_ms(), Some(5000));
    }

    #[test]
    fn wait_condition_result_unsupported_has_no_elapsed() {
        let result = WaitConditionResult::Unsupported {
            reason: "not implemented".to_string(),
        };
        assert!(!result.is_satisfied());
        assert!(!result.is_timed_out());
        assert_eq!(result.elapsed_ms(), None);
    }

    #[test]
    fn wait_condition_result_satisfied_with_no_context() {
        let result = WaitConditionResult::Satisfied {
            elapsed_ms: 0,
            polls: 1,
            context: None,
        };
        assert!(result.is_satisfied());
        assert_eq!(result.elapsed_ms(), Some(0));
    }

    #[test]
    fn wait_condition_result_timed_out_with_no_observed() {
        let result = WaitConditionResult::TimedOut {
            elapsed_ms: 10000,
            polls: 50,
            last_observed: None,
        };
        assert!(result.is_timed_out());
        assert_eq!(result.elapsed_ms(), Some(10000));
    }

    // ========================================================================
    // WaitConditionOptions Tests
    // ========================================================================

    #[test]
    fn wait_condition_options_defaults() {
        let opts = WaitConditionOptions::default();
        assert_eq!(opts.tail_lines, 200);
        assert_eq!(opts.poll_initial, Duration::from_millis(50));
        assert_eq!(opts.poll_max, Duration::from_secs(1));
        assert_eq!(opts.max_polls, 10_000);
        assert!(opts.allow_idle_heuristics);
    }

    #[test]
    fn wait_condition_options_custom() {
        let opts = WaitConditionOptions {
            tail_lines: 50,
            poll_initial: Duration::from_millis(10),
            poll_max: Duration::from_millis(500),
            max_polls: 100,
            allow_idle_heuristics: false,
        };
        assert_eq!(opts.tail_lines, 50);
        assert_eq!(opts.poll_initial, Duration::from_millis(10));
        assert!(!opts.allow_idle_heuristics);
    }

    // ========================================================================
    // truncate_for_log Tests
    // ========================================================================

    #[test]
    fn truncate_for_log_short_string() {
        assert_eq!(truncate_for_log("hello", 10), "hello");
    }

    #[test]
    fn truncate_for_log_exact_length() {
        assert_eq!(truncate_for_log("hello", 5), "hello");
    }

    #[test]
    fn truncate_for_log_truncates_long_string() {
        let result = truncate_for_log("hello world this is a long string", 10);
        assert!(result.ends_with("..."));
        assert!(result.len() <= 10);
    }

    #[test]
    fn truncate_for_log_empty_string() {
        assert_eq!(truncate_for_log("", 10), "");
    }

    #[test]
    fn truncate_for_log_zero_max() {
        let result = truncate_for_log("hello", 0);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn truncate_for_log_max_three() {
        // max_len=3 means saturating_sub(3)=0, so we get "..."
        let result = truncate_for_log("hello", 3);
        assert_eq!(result, "...");
    }

    // ========================================================================
    // heuristic_idle_check Tests
    // ========================================================================

    #[test]
    fn heuristic_idle_detects_bash_prompt() {
        let text = "some output\nuser@host:~/dir$ ";
        let (is_idle, desc) = heuristic_idle_check(text, 10);
        assert!(is_idle, "should detect bash prompt: {desc}");
        assert!(desc.contains("prompt"), "desc: {desc}");
    }

    #[test]
    fn heuristic_idle_detects_root_prompt() {
        let text = "output\nroot@server:/# ";
        let (is_idle, desc) = heuristic_idle_check(text, 10);
        assert!(is_idle, "should detect root prompt: {desc}");
    }

    #[test]
    fn heuristic_idle_detects_zsh_prompt() {
        let text = "output\nuser@host:~/dir% ";
        let (is_idle, desc) = heuristic_idle_check(text, 10);
        assert!(is_idle, "should detect zsh prompt: {desc}");
    }

    #[test]
    fn heuristic_idle_detects_fish_prompt() {
        let text = "output\nuser@host ~/dir> ";
        let (is_idle, desc) = heuristic_idle_check(text, 10);
        assert!(is_idle, "should detect fish prompt: {desc}");
    }

    #[test]
    fn heuristic_idle_detects_python_repl() {
        let text = "Python 3.11.0\n>>> ";
        let (is_idle, desc) = heuristic_idle_check(text, 10);
        assert!(is_idle, "should detect python prompt: {desc}");
    }

    #[test]
    fn heuristic_idle_detects_python_continuation() {
        let text = ">>> def foo():\n... ";
        let (is_idle, desc) = heuristic_idle_check(text, 10);
        assert!(is_idle, "should detect python continuation: {desc}");
    }

    #[test]
    fn heuristic_idle_detects_starship_prompt() {
        let text = "some output\n❯ ";
        let (is_idle, desc) = heuristic_idle_check(text, 10);
        assert!(is_idle, "should detect starship prompt: {desc}");
    }

    #[test]
    fn heuristic_idle_rejects_running_command() {
        let text = "Compiling frankenterm v0.1.0\nBuilding [=====>] 50/100";
        let (is_idle, desc) = heuristic_idle_check(text, 10);
        assert!(!is_idle, "should not detect prompt in build output: {desc}");
    }

    #[test]
    fn heuristic_idle_rejects_progress_percent() {
        // "%" should NOT match as a prompt when in context like "50%"
        let text = "Downloading: 50%";
        let (is_idle, desc) = heuristic_idle_check(text, 10);
        assert!(!is_idle, "should not false-positive on percentage: {desc}");
    }

    #[test]
    fn heuristic_idle_empty_text() {
        let (is_idle, desc) = heuristic_idle_check("", 10);
        assert!(!is_idle, "empty text is not idle: {desc}");
    }

    #[test]
    fn heuristic_idle_empty_last_line_with_prompt_on_prev() {
        // When last line is empty, checks previous line for prompt
        let text = "user@host:~/dir$ \n";
        let (is_idle, desc) = heuristic_idle_check(text, 10);
        assert!(is_idle, "should detect prompt on previous line: {desc}");
    }

    #[test]
    fn heuristic_idle_short_prompt_char() {
        // Very short prompt (just "$") should be detected
        let text = "output\n$";
        let (is_idle, desc) = heuristic_idle_check(text, 10);
        assert!(is_idle, "should detect short $ prompt: {desc}");
    }

    #[test]
    fn heuristic_idle_user_host_pattern_with_dollar() {
        // user@host:path$ (trimmed, no trailing space)
        let text = "output\nuser@host:~/projects$";
        let (is_idle, desc) = heuristic_idle_check(text, 10);
        assert!(is_idle, "should detect user@host prompt: {desc}");
    }

    #[test]
    fn heuristic_idle_limits_to_10_lines() {
        // Even with tail_lines=200, heuristic only checks last 10
        let mut text = String::new();
        for i in 0..20 {
            text.push_str(&format!("line {i}\n"));
        }
        text.push_str("user@host:~/dir$ ");
        let (is_idle, _) = heuristic_idle_check(&text, 200);
        assert!(is_idle, "should still detect prompt in last 10 lines");
    }

    // ========================================================================
    // WaitConditionExecutor Construction Tests
    // ========================================================================

    #[test]
    fn executor_new_has_default_options() {
        let source = MockPaneSource::new(vec!["text".into()]);
        let engine = PatternEngine::new();
        let executor = WaitConditionExecutor::new(&source, &engine);
        assert!(executor.osc_state.is_none());
        assert_eq!(executor.options.tail_lines, 200);
        assert!(executor.options.allow_idle_heuristics);
    }

    #[test]
    fn executor_with_osc_state() {
        let source = MockPaneSource::new(vec!["text".into()]);
        let engine = PatternEngine::new();
        let osc = Osc133State::new();
        let executor = WaitConditionExecutor::new(&source, &engine).with_osc_state(&osc);
        assert!(executor.osc_state.is_some());
    }

    #[test]
    fn executor_with_custom_options() {
        let source = MockPaneSource::new(vec!["text".into()]);
        let engine = PatternEngine::new();
        let opts = WaitConditionOptions {
            tail_lines: 50,
            poll_initial: Duration::from_millis(10),
            poll_max: Duration::from_millis(100),
            max_polls: 5,
            allow_idle_heuristics: false,
        };
        let executor = WaitConditionExecutor::new(&source, &engine).with_options(opts);
        assert_eq!(executor.options.tail_lines, 50);
        assert_eq!(executor.options.max_polls, 5);
        assert!(!executor.options.allow_idle_heuristics);
    }

    // ========================================================================
    // execute() dispatch: Sleep
    // ========================================================================

    #[test]
    fn execute_sleep_condition() {
        let rt = test_runtime();
        let source = MockPaneSource::new(vec![]);
        let engine = PatternEngine::new();
        let executor = WaitConditionExecutor::new(&source, &engine);

        let condition = WaitCondition::Sleep { duration_ms: 10 };
        let result = rt.block_on(executor.execute(&condition, 1, Duration::from_secs(5)));
        let result = result.unwrap();

        assert!(result.is_satisfied());
        if let WaitConditionResult::Satisfied { polls, context, .. } = result {
            assert_eq!(polls, 0);
            assert_eq!(context.as_deref(), Some("sleep completed"));
        }
        // MockPaneSource should not have been called for Sleep
        assert_eq!(source.calls(), 0);
    }

    #[test]
    fn execute_sleep_capped_by_timeout() {
        let rt = test_runtime();
        let source = MockPaneSource::new(vec![]);
        let engine = PatternEngine::new();
        let executor = WaitConditionExecutor::new(&source, &engine);

        // Sleep 10 seconds but timeout is 50ms — should cap to 50ms
        let condition = WaitCondition::Sleep { duration_ms: 10000 };
        let start = Instant::now();
        let result = rt
            .block_on(executor.execute(&condition, 1, Duration::from_millis(50)))
            .unwrap();

        assert!(result.is_satisfied());
        // Should complete near 50ms, not 10 seconds
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    // ========================================================================
    // execute() dispatch: External (Unsupported)
    // ========================================================================

    #[test]
    fn execute_external_returns_unsupported() {
        let rt = test_runtime();
        let source = MockPaneSource::new(vec![]);
        let engine = PatternEngine::new();
        let executor = WaitConditionExecutor::new(&source, &engine);

        let condition = WaitCondition::External {
            key: "test_signal".to_string(),
        };
        let result = rt
            .block_on(executor.execute(&condition, 1, Duration::from_secs(1)))
            .unwrap();

        if let WaitConditionResult::Unsupported { reason } = &result {
            assert!(
                reason.contains("test_signal"),
                "reason should mention key: {reason}"
            );
            assert!(reason.contains("external signal registry"));
        } else {
            panic!("Expected Unsupported, got {result:?}");
        }
    }

    // ========================================================================
    // execute() dispatch: TextMatch
    // ========================================================================

    #[test]
    fn execute_text_match_substring_immediate() {
        let rt = test_runtime();
        let source = MockPaneSource::new(vec!["hello world\n$ ".to_string()]);
        let engine = PatternEngine::new();
        let executor = WaitConditionExecutor::new(&source, &engine);

        let condition = WaitCondition::TextMatch {
            pane_id: None,
            matcher: TextMatch::substring("hello"),
        };
        let result = rt
            .block_on(executor.execute(&condition, 42, Duration::from_secs(1)))
            .unwrap();

        assert!(result.is_satisfied());
        if let WaitConditionResult::Satisfied { polls, .. } = result {
            assert_eq!(polls, 1, "should match on first poll");
        }
    }

    #[test]
    fn execute_text_match_substring_after_delay() {
        let rt = test_runtime();
        let source = MockPaneSource::new(vec![
            "compiling...".to_string(),
            "compiling...".to_string(),
            "done\n$ ".to_string(),
        ]);
        let engine = PatternEngine::new();
        let opts = WaitConditionOptions {
            poll_initial: Duration::from_millis(1),
            poll_max: Duration::from_millis(10),
            max_polls: 100,
            ..WaitConditionOptions::default()
        };
        let executor = WaitConditionExecutor::new(&source, &engine).with_options(opts);

        let condition = WaitCondition::TextMatch {
            pane_id: None,
            matcher: TextMatch::substring("done"),
        };
        let result = rt
            .block_on(executor.execute(&condition, 1, Duration::from_secs(5)))
            .unwrap();

        assert!(result.is_satisfied());
        assert!(source.calls() >= 3, "should have polled at least 3 times");
    }

    #[test]
    fn execute_text_match_regex() {
        let rt = test_runtime();
        let source = MockPaneSource::new(vec!["error code: 42\n$ ".to_string()]);
        let engine = PatternEngine::new();
        let executor = WaitConditionExecutor::new(&source, &engine);

        let condition = WaitCondition::TextMatch {
            pane_id: None,
            matcher: TextMatch::regex(r"error code: \d+"),
        };
        let result = rt
            .block_on(executor.execute(&condition, 1, Duration::from_secs(1)))
            .unwrap();

        assert!(result.is_satisfied());
    }

    #[test]
    fn execute_text_match_timeout() {
        let rt = test_runtime();
        let source = MockPaneSource::new(vec!["nothing relevant".to_string()]);
        let engine = PatternEngine::new();
        let opts = WaitConditionOptions {
            poll_initial: Duration::from_millis(1),
            poll_max: Duration::from_millis(5),
            max_polls: 10,
            ..WaitConditionOptions::default()
        };
        let executor = WaitConditionExecutor::new(&source, &engine).with_options(opts);

        let condition = WaitCondition::TextMatch {
            pane_id: None,
            matcher: TextMatch::substring("never_found"),
        };
        let result = rt
            .block_on(executor.execute(&condition, 1, Duration::from_millis(50)))
            .unwrap();

        assert!(result.is_timed_out());
    }

    #[test]
    fn execute_text_match_uses_explicit_pane_id() {
        // When pane_id is Some, it should override context_pane_id
        let rt = test_runtime();
        let source = MockPaneSource::new(vec!["target text".to_string()]);
        let engine = PatternEngine::new();
        let executor = WaitConditionExecutor::new(&source, &engine);

        let condition = WaitCondition::TextMatch {
            pane_id: Some(99),
            matcher: TextMatch::substring("target"),
        };
        let result = rt
            .block_on(executor.execute(&condition, 1, Duration::from_secs(1)))
            .unwrap();

        assert!(result.is_satisfied());
    }

    // ========================================================================
    // execute() dispatch: StableTail
    // ========================================================================

    #[test]
    fn execute_stable_tail_immediate_stability() {
        let rt = test_runtime();
        // All polls return the same text — should stabilize quickly
        let source = MockPaneSource::new(vec!["stable output\n$ ".to_string()]);
        let engine = PatternEngine::new();
        let opts = WaitConditionOptions {
            poll_initial: Duration::from_millis(1),
            poll_max: Duration::from_millis(5),
            max_polls: 100,
            ..WaitConditionOptions::default()
        };
        let executor = WaitConditionExecutor::new(&source, &engine).with_options(opts);

        let condition = WaitCondition::StableTail {
            pane_id: None,
            stable_for_ms: 5, // Very short stability requirement
        };
        let result = rt
            .block_on(executor.execute(&condition, 1, Duration::from_secs(5)))
            .unwrap();

        assert!(
            result.is_satisfied(),
            "stable text should satisfy: {result:?}"
        );
        if let WaitConditionResult::Satisfied { context, .. } = &result {
            let ctx = context.as_deref().unwrap_or("");
            assert!(ctx.contains("stable for"), "context: {ctx}");
        }
    }

    #[test]
    fn execute_stable_tail_changing_text_times_out() {
        let rt = test_runtime();
        // Each poll returns different text — never stabilizes
        let texts: Vec<String> = (0..50).map(|i| format!("changing output {i}")).collect();
        let source = MockPaneSource::new(texts);
        let engine = PatternEngine::new();
        let opts = WaitConditionOptions {
            poll_initial: Duration::from_millis(1),
            poll_max: Duration::from_millis(2),
            max_polls: 20,
            ..WaitConditionOptions::default()
        };
        let executor = WaitConditionExecutor::new(&source, &engine).with_options(opts);

        let condition = WaitCondition::StableTail {
            pane_id: None,
            stable_for_ms: 1000, // Long stability requirement
        };
        let result = rt
            .block_on(executor.execute(&condition, 1, Duration::from_millis(50)))
            .unwrap();

        assert!(result.is_timed_out());
    }

    // ========================================================================
    // execute() dispatch: PaneIdle with OSC 133
    // ========================================================================

    #[test]
    fn execute_pane_idle_with_osc133_prompt_active() {
        let rt = test_runtime();
        let source = MockPaneSource::new(vec!["$ ".to_string()]);
        let engine = PatternEngine::new();
        let mut osc = Osc133State::new();
        osc.state = ShellState::PromptActive;

        let opts = WaitConditionOptions {
            poll_initial: Duration::from_millis(1),
            poll_max: Duration::from_millis(5),
            max_polls: 100,
            ..WaitConditionOptions::default()
        };
        let executor = WaitConditionExecutor::new(&source, &engine)
            .with_osc_state(&osc)
            .with_options(opts);

        let condition = WaitCondition::PaneIdle {
            pane_id: None,
            idle_threshold_ms: 0, // Zero threshold — immediate satisfaction
        };
        let result = rt
            .block_on(executor.execute(&condition, 1, Duration::from_secs(1)))
            .unwrap();

        assert!(
            result.is_satisfied(),
            "PromptActive with 0 threshold should satisfy: {result:?}"
        );
    }

    #[test]
    fn execute_pane_idle_with_osc133_command_running() {
        let rt = test_runtime();
        let source = MockPaneSource::new(vec!["running...".to_string()]);
        let engine = PatternEngine::new();
        let mut osc = Osc133State::new();
        osc.state = ShellState::CommandRunning;

        let opts = WaitConditionOptions {
            poll_initial: Duration::from_millis(1),
            poll_max: Duration::from_millis(2),
            max_polls: 5,
            ..WaitConditionOptions::default()
        };
        let executor = WaitConditionExecutor::new(&source, &engine)
            .with_osc_state(&osc)
            .with_options(opts);

        let condition = WaitCondition::PaneIdle {
            pane_id: None,
            idle_threshold_ms: 0,
        };
        let result = rt
            .block_on(executor.execute(&condition, 1, Duration::from_millis(50)))
            .unwrap();

        assert!(
            result.is_timed_out(),
            "CommandRunning should not be idle: {result:?}"
        );
    }

    #[test]
    fn execute_pane_idle_with_osc133_command_finished() {
        let rt = test_runtime();
        let source = MockPaneSource::new(vec!["done\n$ ".to_string()]);
        let engine = PatternEngine::new();
        let mut osc = Osc133State::new();
        osc.state = ShellState::CommandFinished { exit_code: Some(0) };

        let opts = WaitConditionOptions {
            poll_initial: Duration::from_millis(1),
            poll_max: Duration::from_millis(5),
            max_polls: 100,
            ..WaitConditionOptions::default()
        };
        let executor = WaitConditionExecutor::new(&source, &engine)
            .with_osc_state(&osc)
            .with_options(opts);

        let condition = WaitCondition::PaneIdle {
            pane_id: None,
            idle_threshold_ms: 0,
        };
        let result = rt
            .block_on(executor.execute(&condition, 1, Duration::from_secs(1)))
            .unwrap();

        assert!(result.is_satisfied());
    }

    // ========================================================================
    // execute() dispatch: PaneIdle with heuristics
    // ========================================================================

    #[test]
    fn execute_pane_idle_heuristic_bash_prompt() {
        let rt = test_runtime();
        let source = MockPaneSource::new(vec!["user@host:~/dir$ ".to_string()]);
        let engine = PatternEngine::new();

        let opts = WaitConditionOptions {
            poll_initial: Duration::from_millis(1),
            poll_max: Duration::from_millis(5),
            max_polls: 100,
            allow_idle_heuristics: true,
            ..WaitConditionOptions::default()
        };
        // No OSC state — forces heuristic fallback
        let executor = WaitConditionExecutor::new(&source, &engine).with_options(opts);

        let condition = WaitCondition::PaneIdle {
            pane_id: None,
            idle_threshold_ms: 0,
        };
        let result = rt
            .block_on(executor.execute(&condition, 1, Duration::from_secs(1)))
            .unwrap();

        assert!(
            result.is_satisfied(),
            "heuristic should detect bash prompt: {result:?}"
        );
    }

    #[test]
    fn execute_pane_idle_no_osc_no_heuristics() {
        let rt = test_runtime();
        let source = MockPaneSource::new(vec!["user@host:~/dir$ ".to_string()]);
        let engine = PatternEngine::new();

        let opts = WaitConditionOptions {
            poll_initial: Duration::from_millis(1),
            poll_max: Duration::from_millis(2),
            max_polls: 5,
            allow_idle_heuristics: false,
            ..WaitConditionOptions::default()
        };
        let executor = WaitConditionExecutor::new(&source, &engine).with_options(opts);

        let condition = WaitCondition::PaneIdle {
            pane_id: None,
            idle_threshold_ms: 0,
        };
        let result = rt
            .block_on(executor.execute(&condition, 1, Duration::from_millis(50)))
            .unwrap();

        // With no OSC state and heuristics disabled, should always return not-idle → timeout
        assert!(
            result.is_timed_out(),
            "no osc + no heuristics → never idle: {result:?}"
        );
    }

    // ========================================================================
    // execute() dispatch: Pattern
    // ========================================================================

    #[test]
    fn execute_pattern_wait_matches_rule() {
        let rt = test_runtime();
        // Create a pattern engine with a test rule that matches "ERROR"
        let rule = RuleDef {
            id: "wezterm.error_detect".to_string(),
            agent_type: AgentType::Unknown,
            event_type: "error".to_string(),
            severity: Severity::Warning,
            anchors: vec!["ERROR".to_string()],
            regex: None,
            description: "Test error detection".to_string(),
            remediation: None,
            workflow: None,
            manual_fix: None,
            preview_command: None,
            learn_more_url: None,
        };
        let pack = PatternPack::new("test:pack", "1.0.0", vec![rule]);
        let engine = PatternEngine::with_packs(vec![pack]).unwrap();

        let source = MockPaneSource::new(vec!["ERROR: something went wrong".to_string()]);
        let opts = WaitConditionOptions {
            poll_initial: Duration::from_millis(1),
            poll_max: Duration::from_millis(10),
            max_polls: 100,
            ..WaitConditionOptions::default()
        };
        let executor = WaitConditionExecutor::new(&source, &engine).with_options(opts);

        let condition = WaitCondition::Pattern {
            pane_id: None,
            rule_id: "wezterm.error_detect".to_string(),
        };
        let result = rt
            .block_on(executor.execute(&condition, 1, Duration::from_secs(5)))
            .unwrap();

        assert!(
            result.is_satisfied(),
            "should detect ERROR pattern: {result:?}"
        );
        if let WaitConditionResult::Satisfied { context, .. } = &result {
            let ctx = context.as_deref().unwrap_or("");
            assert!(ctx.contains("matched"), "context: {ctx}");
        }
    }

    #[test]
    fn execute_pattern_wait_timeout_no_match() {
        let rt = test_runtime();
        let engine = PatternEngine::new(); // Default engine won't match our text

        let source = MockPaneSource::new(vec!["normal output".to_string()]);
        let opts = WaitConditionOptions {
            poll_initial: Duration::from_millis(1),
            poll_max: Duration::from_millis(2),
            max_polls: 5,
            ..WaitConditionOptions::default()
        };
        let executor = WaitConditionExecutor::new(&source, &engine).with_options(opts);

        let condition = WaitCondition::Pattern {
            pane_id: None,
            rule_id: "nonexistent.rule".to_string(),
        };
        let result = rt
            .block_on(executor.execute(&condition, 1, Duration::from_millis(50)))
            .unwrap();

        assert!(result.is_timed_out());
    }

    #[test]
    fn execute_pattern_wait_uses_explicit_pane_id() {
        let rt = test_runtime();
        let engine = PatternEngine::new();
        let source = MockPaneSource::new(vec!["text".to_string()]);
        let opts = WaitConditionOptions {
            poll_initial: Duration::from_millis(1),
            poll_max: Duration::from_millis(2),
            max_polls: 3,
            ..WaitConditionOptions::default()
        };
        let executor = WaitConditionExecutor::new(&source, &engine).with_options(opts);

        // Even with pane_id=Some(99), the MockPaneSource ignores pane_id
        // This test verifies the dispatch doesn't panic
        let condition = WaitCondition::Pattern {
            pane_id: Some(99),
            rule_id: "any.rule".to_string(),
        };
        let result = rt
            .block_on(executor.execute(&condition, 1, Duration::from_millis(20)))
            .unwrap();

        // Will timeout since no matching rule, but shouldn't panic
        assert!(result.is_timed_out());
    }

    // ========================================================================
    // check_idle_state Tests (via execute_pane_idle_wait)
    // ========================================================================

    #[test]
    fn check_idle_osc133_input_active_is_idle() {
        let rt = test_runtime();
        let source = MockPaneSource::new(vec!["$ ".to_string()]);
        let engine = PatternEngine::new();
        let mut osc = Osc133State::new();
        osc.state = ShellState::InputActive;

        let opts = WaitConditionOptions {
            poll_initial: Duration::from_millis(1),
            poll_max: Duration::from_millis(5),
            max_polls: 100,
            ..WaitConditionOptions::default()
        };
        let executor = WaitConditionExecutor::new(&source, &engine)
            .with_osc_state(&osc)
            .with_options(opts);

        let condition = WaitCondition::PaneIdle {
            pane_id: None,
            idle_threshold_ms: 0,
        };
        let result = rt
            .block_on(executor.execute(&condition, 1, Duration::from_secs(1)))
            .unwrap();

        assert!(
            result.is_satisfied(),
            "InputActive is at_prompt: {result:?}"
        );
    }

    #[test]
    fn check_idle_osc133_unknown_is_not_idle() {
        let rt = test_runtime();
        let source = MockPaneSource::new(vec!["...".to_string()]);
        let engine = PatternEngine::new();
        let osc = Osc133State::new(); // Default = ShellState::Unknown

        let opts = WaitConditionOptions {
            poll_initial: Duration::from_millis(1),
            poll_max: Duration::from_millis(2),
            max_polls: 5,
            ..WaitConditionOptions::default()
        };
        let executor = WaitConditionExecutor::new(&source, &engine)
            .with_osc_state(&osc)
            .with_options(opts);

        let condition = WaitCondition::PaneIdle {
            pane_id: None,
            idle_threshold_ms: 0,
        };
        let result = rt
            .block_on(executor.execute(&condition, 1, Duration::from_millis(20)))
            .unwrap();

        assert!(
            result.is_timed_out(),
            "Unknown shell state is not idle: {result:?}"
        );
    }

    // ========================================================================
    // Max polls enforcement
    // ========================================================================

    #[test]
    fn execute_respects_max_polls_limit() {
        let rt = test_runtime();
        let source = MockPaneSource::new(vec!["no match".to_string()]);
        let engine = PatternEngine::new();
        let opts = WaitConditionOptions {
            poll_initial: Duration::from_millis(1),
            poll_max: Duration::from_millis(1),
            max_polls: 3, // Very low limit
            ..WaitConditionOptions::default()
        };
        let executor = WaitConditionExecutor::new(&source, &engine).with_options(opts);

        let condition = WaitCondition::TextMatch {
            pane_id: None,
            matcher: TextMatch::substring("never_found"),
        };
        let result = rt
            .block_on(executor.execute(&condition, 1, Duration::from_secs(60)))
            .unwrap();

        assert!(result.is_timed_out());
        if let WaitConditionResult::TimedOut { polls, .. } = result {
            // PaneWaiter enforces max_polls, so total polls should be small
            assert!(polls <= 5, "should respect max_polls, got {polls}");
        }
    }
}
