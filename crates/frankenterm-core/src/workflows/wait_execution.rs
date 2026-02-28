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
