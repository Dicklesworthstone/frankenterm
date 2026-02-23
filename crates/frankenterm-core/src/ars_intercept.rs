//! FST Interception & PTY Hot-Path Integration for ARS.
//!
//! Wires the compiled FST into the terminal event processing pipeline.
//! When an error regime is detected (e.g., via BOCPD), the interceptor
//! queries the FST for matching reflexes and decides whether to enter
//! "subconscious execution mode" (automated recovery) or fall back to
//! the standard LLM dispatch pipeline.
//!
//! # Decision Flow
//!
//! ```text
//! PTY output ──→ Error Detected ──→ MinHash/Trigger ──→ FST Lookup
//!                                                          │
//!                                          ┌───────────────┼──────────────┐
//!                                          ▼               ▼              ▼
//!                                      No Match       Context Fail     Match OK
//!                                          │               │              │
//!                                          ▼               ▼              ▼
//!                                      LLM Fallback   LLM Fallback   Execute Reflex
//! ```
//!
//! # Context Gate
//!
//! Before executing a reflex, the context gate validates that the target
//! pane's environment (CWD, env vars, shell) matches the reflex's
//! required bounds. If context fails, the interceptor gracefully falls
//! back to LLM processing.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::ars_fst::{FstHandle, FstMatch, ReflexId};

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for the ARS interceptor.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct InterceptConfig {
    /// Whether interception is enabled.
    pub enabled: bool,
    /// Maximum time (ms) allowed for FST lookup + context check.
    pub max_intercept_latency_ms: u64,
    /// Whether to require context gate pass for execution.
    pub require_context_gate: bool,
    /// Minimum priority threshold (lower = higher priority; reject matches above this).
    pub max_priority_threshold: u32,
    /// Whether to log all intercept decisions (even misses).
    pub verbose_logging: bool,
    /// Cooldown period (ms) between consecutive reflex executions on the same pane.
    pub cooldown_ms: u64,
    /// Maximum concurrent reflex executions globally.
    pub max_concurrent_executions: u32,
}

impl Default for InterceptConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_intercept_latency_ms: 10,
            require_context_gate: true,
            max_priority_threshold: 100,
            verbose_logging: false,
            cooldown_ms: 5000,
            max_concurrent_executions: 3,
        }
    }
}

// =============================================================================
// Context Gate
// =============================================================================

/// Required context bounds for a reflex to be executable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextBounds {
    /// Required CWD prefix (e.g., `/app/` or `/home/user/project`).
    /// Empty means any CWD is acceptable.
    pub cwd_prefix: String,
    /// Required environment variables (key → expected value).
    /// Empty means no env requirements.
    pub required_env: HashMap<String, String>,
    /// Required shell type (e.g., "bash", "zsh"). Empty means any.
    pub required_shell: String,
    /// Required minimum pane width (0 = no requirement).
    pub min_pane_cols: u16,
    /// Required minimum pane height (0 = no requirement).
    pub min_pane_rows: u16,
}

impl Default for ContextBounds {
    fn default() -> Self {
        Self {
            cwd_prefix: String::new(),
            required_env: HashMap::new(),
            required_shell: String::new(),
            min_pane_cols: 0,
            min_pane_rows: 0,
        }
    }
}

/// Current pane context for gate evaluation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneContext {
    /// Current working directory.
    pub cwd: String,
    /// Environment variables.
    pub env: HashMap<String, String>,
    /// Shell type.
    pub shell: String,
    /// Pane dimensions.
    pub cols: u16,
    pub rows: u16,
    /// Pane ID for cooldown tracking.
    pub pane_id: u64,
}

/// Result of context gate evaluation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContextGateResult {
    /// Context matches requirements.
    Pass,
    /// CWD doesn't match required prefix.
    CwdMismatch {
        required: String,
        actual: String,
    },
    /// Missing or mismatched environment variable.
    EnvMismatch {
        key: String,
        expected: String,
        actual: Option<String>,
    },
    /// Shell type doesn't match.
    ShellMismatch {
        required: String,
        actual: String,
    },
    /// Pane dimensions too small.
    DimensionMismatch {
        min_cols: u16,
        min_rows: u16,
        actual_cols: u16,
        actual_rows: u16,
    },
}

impl ContextGateResult {
    /// Check if the gate passed.
    pub fn passed(&self) -> bool {
        matches!(self, Self::Pass)
    }
}

/// Evaluate context bounds against pane context.
pub fn evaluate_context_gate(bounds: &ContextBounds, context: &PaneContext) -> ContextGateResult {
    // Check CWD prefix.
    if !bounds.cwd_prefix.is_empty() && !context.cwd.starts_with(&bounds.cwd_prefix) {
        return ContextGateResult::CwdMismatch {
            required: bounds.cwd_prefix.clone(),
            actual: context.cwd.clone(),
        };
    }

    // Check env vars.
    for (key, expected) in &bounds.required_env {
        match context.env.get(key) {
            Some(actual) if actual != expected => {
                return ContextGateResult::EnvMismatch {
                    key: key.clone(),
                    expected: expected.clone(),
                    actual: Some(actual.clone()),
                };
            }
            None => {
                return ContextGateResult::EnvMismatch {
                    key: key.clone(),
                    expected: expected.clone(),
                    actual: None,
                };
            }
            _ => {}
        }
    }

    // Check shell.
    if !bounds.required_shell.is_empty() && context.shell != bounds.required_shell {
        return ContextGateResult::ShellMismatch {
            required: bounds.required_shell.clone(),
            actual: context.shell.clone(),
        };
    }

    // Check dimensions.
    if (bounds.min_pane_cols > 0 && context.cols < bounds.min_pane_cols)
        || (bounds.min_pane_rows > 0 && context.rows < bounds.min_pane_rows)
    {
        return ContextGateResult::DimensionMismatch {
            min_cols: bounds.min_pane_cols,
            min_rows: bounds.min_pane_rows,
            actual_cols: context.cols,
            actual_rows: context.rows,
        };
    }

    ContextGateResult::Pass
}

// =============================================================================
// Intercept Decision
// =============================================================================

/// The interceptor's decision for a given error event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InterceptDecision {
    /// Execute the matched reflex.
    Execute {
        reflex_id: ReflexId,
        cluster_id: String,
        priority: u32,
    },
    /// Fall back to LLM processing.
    FallbackToLlm {
        reason: FallbackReason,
    },
    /// Interception is disabled.
    Disabled,
}

/// Why the interceptor fell back to LLM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FallbackReason {
    /// No matching reflex in the FST.
    NoMatch,
    /// Match found but context gate failed.
    ContextFailed {
        gate_result: ContextGateResult,
    },
    /// Match priority exceeds threshold.
    PriorityTooLow {
        priority: u32,
        threshold: u32,
    },
    /// Pane is in cooldown period.
    Cooldown {
        remaining_ms: u64,
    },
    /// Maximum concurrent executions reached.
    ConcurrencyLimit {
        current: u32,
        max: u32,
    },
    /// FST lookup timed out.
    Timeout {
        elapsed_ms: u64,
        max_ms: u64,
    },
}

// =============================================================================
// Interceptor
// =============================================================================

/// ARS interceptor: queries FST and makes execute/fallback decisions.
pub struct ArsInterceptor {
    config: InterceptConfig,
    /// Reflex context bounds registry: reflex_id → bounds.
    context_bounds: HashMap<ReflexId, ContextBounds>,
    /// Per-pane cooldown tracker: pane_id → last_execution_timestamp_ms.
    cooldowns: HashMap<u64, u64>,
    /// Current count of active executions.
    active_executions: AtomicU32,
    /// Total intercept attempts.
    total_attempts: AtomicU64,
    /// Total executions.
    total_executions: AtomicU64,
    /// Total fallbacks.
    total_fallbacks: AtomicU64,
    /// Whether the interceptor is paused (emergency brake).
    paused: AtomicBool,
}

use std::sync::atomic::AtomicU32;

impl ArsInterceptor {
    /// Create an interceptor with the given configuration.
    pub fn new(config: InterceptConfig) -> Self {
        Self {
            config,
            context_bounds: HashMap::new(),
            cooldowns: HashMap::new(),
            active_executions: AtomicU32::new(0),
            total_attempts: AtomicU64::new(0),
            total_executions: AtomicU64::new(0),
            total_fallbacks: AtomicU64::new(0),
            paused: AtomicBool::new(false),
        }
    }

    /// Create with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(InterceptConfig::default())
    }

    /// Register context bounds for a reflex.
    pub fn register_bounds(&mut self, reflex_id: ReflexId, bounds: ContextBounds) {
        self.context_bounds.insert(reflex_id, bounds);
    }

    /// Make an intercept decision given an FST match and pane context.
    pub fn decide(
        &mut self,
        fst_match: Option<&FstMatch>,
        context: &PaneContext,
        current_time_ms: u64,
    ) -> InterceptDecision {
        self.total_attempts.fetch_add(1, Ordering::Relaxed);

        // Check if disabled.
        if !self.config.enabled || self.paused.load(Ordering::Acquire) {
            return InterceptDecision::Disabled;
        }

        // No match → fallback.
        let m = match fst_match {
            Some(m) => m,
            None => {
                self.total_fallbacks.fetch_add(1, Ordering::Relaxed);
                if self.config.verbose_logging {
                    debug!("ARS intercept: no FST match, falling back to LLM");
                }
                return InterceptDecision::FallbackToLlm {
                    reason: FallbackReason::NoMatch,
                };
            }
        };

        // Priority check.
        if m.priority > self.config.max_priority_threshold {
            self.total_fallbacks.fetch_add(1, Ordering::Relaxed);
            return InterceptDecision::FallbackToLlm {
                reason: FallbackReason::PriorityTooLow {
                    priority: m.priority,
                    threshold: self.config.max_priority_threshold,
                },
            };
        }

        // Cooldown check.
        if let Some(&last_exec) = self.cooldowns.get(&context.pane_id) {
            if current_time_ms < last_exec + self.config.cooldown_ms {
                let remaining = (last_exec + self.config.cooldown_ms) - current_time_ms;
                self.total_fallbacks.fetch_add(1, Ordering::Relaxed);
                return InterceptDecision::FallbackToLlm {
                    reason: FallbackReason::Cooldown {
                        remaining_ms: remaining,
                    },
                };
            }
        }

        // Concurrency check.
        let current_active = self.active_executions.load(Ordering::Acquire);
        if current_active >= self.config.max_concurrent_executions {
            self.total_fallbacks.fetch_add(1, Ordering::Relaxed);
            return InterceptDecision::FallbackToLlm {
                reason: FallbackReason::ConcurrencyLimit {
                    current: current_active,
                    max: self.config.max_concurrent_executions,
                },
            };
        }

        // Context gate.
        if self.config.require_context_gate {
            let bounds = self
                .context_bounds
                .get(&m.reflex_id)
                .cloned()
                .unwrap_or_default();
            let gate = evaluate_context_gate(&bounds, context);
            if !gate.passed() {
                self.total_fallbacks.fetch_add(1, Ordering::Relaxed);
                warn!(
                    reflex_id = m.reflex_id,
                    pane_id = context.pane_id,
                    "context gate failed"
                );
                return InterceptDecision::FallbackToLlm {
                    reason: FallbackReason::ContextFailed {
                        gate_result: gate,
                    },
                };
            }
        }

        // All gates passed → execute.
        self.active_executions.fetch_add(1, Ordering::Release);
        self.cooldowns.insert(context.pane_id, current_time_ms);
        self.total_executions.fetch_add(1, Ordering::Relaxed);

        info!(
            reflex_id = m.reflex_id,
            cluster_id = %m.cluster_id,
            priority = m.priority,
            pane_id = context.pane_id,
            "ARS reflex executing"
        );

        InterceptDecision::Execute {
            reflex_id: m.reflex_id,
            cluster_id: m.cluster_id.clone(),
            priority: m.priority,
        }
    }

    /// Notify that a reflex execution completed (decrements active count).
    pub fn execution_completed(&self) {
        self.active_executions.fetch_sub(1, Ordering::Release);
    }

    /// Pause the interceptor (emergency brake).
    pub fn pause(&self) {
        self.paused.store(true, Ordering::Release);
        warn!("ARS interceptor PAUSED");
    }

    /// Resume the interceptor.
    pub fn resume(&self) {
        self.paused.store(false, Ordering::Release);
        info!("ARS interceptor resumed");
    }

    /// Check if paused.
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    /// Get the current active execution count.
    pub fn active_count(&self) -> u32 {
        self.active_executions.load(Ordering::Acquire)
    }

    /// Get intercept statistics.
    pub fn stats(&self) -> InterceptStats {
        InterceptStats {
            total_attempts: self.total_attempts.load(Ordering::Relaxed),
            total_executions: self.total_executions.load(Ordering::Relaxed),
            total_fallbacks: self.total_fallbacks.load(Ordering::Relaxed),
            active_executions: self.active_executions.load(Ordering::Relaxed),
            is_paused: self.paused.load(Ordering::Relaxed),
            registered_bounds: self.context_bounds.len(),
            cooldown_entries: self.cooldowns.len(),
        }
    }

    /// Clear cooldown state (for testing or reset).
    pub fn clear_cooldowns(&mut self) {
        self.cooldowns.clear();
    }

    /// Get the config.
    pub fn config(&self) -> &InterceptConfig {
        &self.config
    }
}

/// Interceptor statistics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterceptStats {
    pub total_attempts: u64,
    pub total_executions: u64,
    pub total_fallbacks: u64,
    pub active_executions: u32,
    pub is_paused: bool,
    pub registered_bounds: usize,
    pub cooldown_entries: usize,
}

// =============================================================================
// Convenience: full pipeline intercept
// =============================================================================

/// Run the full interception pipeline: FST lookup → context gate → decision.
pub fn intercept(
    handle: &FstHandle,
    interceptor: &mut ArsInterceptor,
    trigger: &[u8],
    context: &PaneContext,
    current_time_ms: u64,
) -> InterceptDecision {
    let matches = handle.prefix_search(trigger);
    let fst_match = matches.iter().min_by_key(|m| m.priority);
    interceptor.decide(fst_match, context, current_time_ms)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ars_fst::{FstCompiler, TriggerEntry};

    fn test_context() -> PaneContext {
        PaneContext {
            cwd: "/app/src".to_string(),
            env: HashMap::from([("RUST_LOG".to_string(), "debug".to_string())]),
            shell: "zsh".to_string(),
            cols: 120,
            rows: 40,
            pane_id: 1,
        }
    }

    fn test_match(reflex_id: ReflexId, priority: u32) -> FstMatch {
        FstMatch {
            reflex_id,
            priority,
            match_len: 10,
            cluster_id: format!("c-{reflex_id}"),
        }
    }

    // ---- Context Gate ----

    #[test]
    fn context_gate_pass_no_requirements() {
        let bounds = ContextBounds::default();
        let ctx = test_context();
        let result = evaluate_context_gate(&bounds, &ctx);
        assert!(result.passed());
    }

    #[test]
    fn context_gate_pass_with_cwd_prefix() {
        let bounds = ContextBounds {
            cwd_prefix: "/app".to_string(),
            ..Default::default()
        };
        let ctx = test_context();
        assert!(evaluate_context_gate(&bounds, &ctx).passed());
    }

    #[test]
    fn context_gate_fail_cwd_mismatch() {
        let bounds = ContextBounds {
            cwd_prefix: "/home/other".to_string(),
            ..Default::default()
        };
        let ctx = test_context();
        let result = evaluate_context_gate(&bounds, &ctx);
        let is_cwd_fail = matches!(result, ContextGateResult::CwdMismatch { .. });
        assert!(is_cwd_fail);
    }

    #[test]
    fn context_gate_pass_with_env_match() {
        let bounds = ContextBounds {
            required_env: HashMap::from([("RUST_LOG".to_string(), "debug".to_string())]),
            ..Default::default()
        };
        let ctx = test_context();
        assert!(evaluate_context_gate(&bounds, &ctx).passed());
    }

    #[test]
    fn context_gate_fail_env_mismatch() {
        let bounds = ContextBounds {
            required_env: HashMap::from([("RUST_LOG".to_string(), "info".to_string())]),
            ..Default::default()
        };
        let ctx = test_context();
        let result = evaluate_context_gate(&bounds, &ctx);
        let is_env_fail = matches!(result, ContextGateResult::EnvMismatch { .. });
        assert!(is_env_fail);
    }

    #[test]
    fn context_gate_fail_env_missing() {
        let bounds = ContextBounds {
            required_env: HashMap::from([("MISSING_VAR".to_string(), "x".to_string())]),
            ..Default::default()
        };
        let ctx = test_context();
        let result = evaluate_context_gate(&bounds, &ctx);
        if let ContextGateResult::EnvMismatch { actual, .. } = result {
            assert!(actual.is_none());
        } else {
            panic!("expected EnvMismatch");
        }
    }

    #[test]
    fn context_gate_fail_shell_mismatch() {
        let bounds = ContextBounds {
            required_shell: "bash".to_string(),
            ..Default::default()
        };
        let ctx = test_context();
        let result = evaluate_context_gate(&bounds, &ctx);
        let is_shell_fail = matches!(result, ContextGateResult::ShellMismatch { .. });
        assert!(is_shell_fail);
    }

    #[test]
    fn context_gate_fail_dimension_too_small() {
        let bounds = ContextBounds {
            min_pane_cols: 200,
            ..Default::default()
        };
        let ctx = test_context();
        let result = evaluate_context_gate(&bounds, &ctx);
        let is_dim_fail = matches!(result, ContextGateResult::DimensionMismatch { .. });
        assert!(is_dim_fail);
    }

    #[test]
    fn context_gate_pass_with_sufficient_dimensions() {
        let bounds = ContextBounds {
            min_pane_cols: 80,
            min_pane_rows: 24,
            ..Default::default()
        };
        let ctx = test_context();
        assert!(evaluate_context_gate(&bounds, &ctx).passed());
    }

    // ---- Interceptor decisions ----

    #[test]
    fn decide_execute_on_match() {
        let mut interceptor = ArsInterceptor::with_defaults();
        let m = test_match(1, 0);
        let ctx = test_context();
        let decision = interceptor.decide(Some(&m), &ctx, 1000);
        let is_execute = matches!(decision, InterceptDecision::Execute { reflex_id: 1, .. });
        assert!(is_execute);
    }

    #[test]
    fn decide_fallback_on_no_match() {
        let mut interceptor = ArsInterceptor::with_defaults();
        let ctx = test_context();
        let decision = interceptor.decide(None, &ctx, 1000);
        let is_fallback = matches!(
            decision,
            InterceptDecision::FallbackToLlm {
                reason: FallbackReason::NoMatch
            }
        );
        assert!(is_fallback);
    }

    #[test]
    fn decide_disabled_when_not_enabled() {
        let config = InterceptConfig {
            enabled: false,
            ..Default::default()
        };
        let mut interceptor = ArsInterceptor::new(config);
        let m = test_match(1, 0);
        let ctx = test_context();
        let decision = interceptor.decide(Some(&m), &ctx, 1000);
        assert_eq!(decision, InterceptDecision::Disabled);
    }

    #[test]
    fn decide_disabled_when_paused() {
        let mut interceptor = ArsInterceptor::with_defaults();
        interceptor.pause();
        let m = test_match(1, 0);
        let ctx = test_context();
        let decision = interceptor.decide(Some(&m), &ctx, 1000);
        assert_eq!(decision, InterceptDecision::Disabled);
    }

    #[test]
    fn decide_fallback_on_priority_too_low() {
        let config = InterceptConfig {
            max_priority_threshold: 5,
            ..Default::default()
        };
        let mut interceptor = ArsInterceptor::new(config);
        let m = test_match(1, 10); // priority 10 > threshold 5
        let ctx = test_context();
        let decision = interceptor.decide(Some(&m), &ctx, 1000);
        let is_priority_fail = matches!(
            decision,
            InterceptDecision::FallbackToLlm {
                reason: FallbackReason::PriorityTooLow { .. }
            }
        );
        assert!(is_priority_fail);
    }

    #[test]
    fn decide_fallback_on_cooldown() {
        let config = InterceptConfig {
            cooldown_ms: 5000,
            ..Default::default()
        };
        let mut interceptor = ArsInterceptor::new(config);
        let m = test_match(1, 0);
        let ctx = test_context();

        // First execution succeeds.
        let d1 = interceptor.decide(Some(&m), &ctx, 1000);
        assert!(matches!(d1, InterceptDecision::Execute { .. }));
        interceptor.execution_completed();

        // Second within cooldown fails.
        let d2 = interceptor.decide(Some(&m), &ctx, 3000);
        let is_cooldown = matches!(
            d2,
            InterceptDecision::FallbackToLlm {
                reason: FallbackReason::Cooldown { .. }
            }
        );
        assert!(is_cooldown);
    }

    #[test]
    fn decide_succeeds_after_cooldown() {
        let config = InterceptConfig {
            cooldown_ms: 1000,
            ..Default::default()
        };
        let mut interceptor = ArsInterceptor::new(config);
        let m = test_match(1, 0);
        let ctx = test_context();

        interceptor.decide(Some(&m), &ctx, 1000);
        interceptor.execution_completed();

        // After cooldown.
        let d = interceptor.decide(Some(&m), &ctx, 3000);
        assert!(matches!(d, InterceptDecision::Execute { .. }));
    }

    #[test]
    fn decide_fallback_on_concurrency_limit() {
        let config = InterceptConfig {
            max_concurrent_executions: 1,
            ..Default::default()
        };
        let mut interceptor = ArsInterceptor::new(config);
        let m = test_match(1, 0);
        let mut ctx = test_context();

        // First execution.
        let d1 = interceptor.decide(Some(&m), &ctx, 1000);
        assert!(matches!(d1, InterceptDecision::Execute { .. }));
        // Don't complete — still active.

        // Second attempt on different pane.
        ctx.pane_id = 2;
        let d2 = interceptor.decide(Some(&m), &ctx, 2000);
        let is_limit = matches!(
            d2,
            InterceptDecision::FallbackToLlm {
                reason: FallbackReason::ConcurrencyLimit { .. }
            }
        );
        assert!(is_limit);
    }

    #[test]
    fn decide_fallback_on_context_gate_fail() {
        let mut interceptor = ArsInterceptor::with_defaults();
        interceptor.register_bounds(
            1,
            ContextBounds {
                cwd_prefix: "/other/path".to_string(),
                ..Default::default()
            },
        );

        let m = test_match(1, 0);
        let ctx = test_context(); // cwd = /app/src
        let decision = interceptor.decide(Some(&m), &ctx, 1000);
        let is_context_fail = matches!(
            decision,
            InterceptDecision::FallbackToLlm {
                reason: FallbackReason::ContextFailed { .. }
            }
        );
        assert!(is_context_fail);
    }

    #[test]
    fn context_gate_skipped_when_disabled() {
        let config = InterceptConfig {
            require_context_gate: false,
            ..Default::default()
        };
        let mut interceptor = ArsInterceptor::new(config);
        interceptor.register_bounds(
            1,
            ContextBounds {
                cwd_prefix: "/other/path".to_string(),
                ..Default::default()
            },
        );

        let m = test_match(1, 0);
        let ctx = test_context();
        let decision = interceptor.decide(Some(&m), &ctx, 1000);
        // Should execute even though context doesn't match (gate disabled).
        assert!(matches!(decision, InterceptDecision::Execute { .. }));
    }

    // ---- Stats ----

    #[test]
    fn stats_track_attempts() {
        let mut interceptor = ArsInterceptor::with_defaults();
        let m = test_match(1, 0);
        let ctx = test_context();

        interceptor.decide(Some(&m), &ctx, 1000);
        interceptor.decide(None, &ctx, 2000);

        let stats = interceptor.stats();
        assert_eq!(stats.total_attempts, 2);
        assert_eq!(stats.total_executions, 1);
        assert_eq!(stats.total_fallbacks, 1);
    }

    #[test]
    fn stats_active_count() {
        let mut interceptor = ArsInterceptor::with_defaults();
        let m = test_match(1, 0);
        let ctx = test_context();

        interceptor.decide(Some(&m), &ctx, 1000);
        assert_eq!(interceptor.active_count(), 1);

        interceptor.execution_completed();
        assert_eq!(interceptor.active_count(), 0);
    }

    // ---- Pause/resume ----

    #[test]
    fn pause_and_resume() {
        let interceptor = ArsInterceptor::with_defaults();
        assert!(!interceptor.is_paused());

        interceptor.pause();
        assert!(interceptor.is_paused());

        interceptor.resume();
        assert!(!interceptor.is_paused());
    }

    // ---- Clear cooldowns ----

    #[test]
    fn clear_cooldowns_enables_reexecution() {
        let config = InterceptConfig {
            cooldown_ms: 999_999,
            ..Default::default()
        };
        let mut interceptor = ArsInterceptor::new(config);
        let m = test_match(1, 0);
        let ctx = test_context();

        interceptor.decide(Some(&m), &ctx, 1000);
        interceptor.execution_completed();

        // Within cooldown → fallback.
        let d = interceptor.decide(Some(&m), &ctx, 2000);
        assert!(matches!(d, InterceptDecision::FallbackToLlm { .. }));

        // Clear and retry.
        interceptor.clear_cooldowns();
        let d2 = interceptor.decide(Some(&m), &ctx, 3000);
        assert!(matches!(d2, InterceptDecision::Execute { .. }));
    }

    // ---- Full pipeline ----

    #[test]
    fn full_intercept_pipeline() {
        let compiler = FstCompiler::with_defaults();
        let entries = vec![TriggerEntry {
            key: b"error: not found".to_vec(),
            reflex_id: 42,
            priority: 0,
            cluster_id: "c42".to_string(),
        }];
        let index = compiler.compile(&entries).unwrap();
        let handle = FstHandle::new(index);

        let mut interceptor = ArsInterceptor::with_defaults();
        let ctx = test_context();

        let decision = intercept(
            &handle,
            &mut interceptor,
            b"error: not found in src/main.rs",
            &ctx,
            1000,
        );

        let is_execute = matches!(decision, InterceptDecision::Execute { reflex_id: 42, .. });
        assert!(is_execute);
    }

    #[test]
    fn full_intercept_no_match() {
        let compiler = FstCompiler::with_defaults();
        let entries = vec![TriggerEntry {
            key: b"warning: unused".to_vec(),
            reflex_id: 1,
            priority: 0,
            cluster_id: "c1".to_string(),
        }];
        let index = compiler.compile(&entries).unwrap();
        let handle = FstHandle::new(index);

        let mut interceptor = ArsInterceptor::with_defaults();
        let ctx = test_context();

        let decision = intercept(&handle, &mut interceptor, b"error: timeout", &ctx, 1000);
        let is_fallback = matches!(
            decision,
            InterceptDecision::FallbackToLlm {
                reason: FallbackReason::NoMatch
            }
        );
        assert!(is_fallback);
    }

    // ---- Serde roundtrips ----

    #[test]
    fn intercept_config_serde_roundtrip() {
        let config = InterceptConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let decoded: InterceptConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.enabled, config.enabled);
        assert_eq!(decoded.cooldown_ms, config.cooldown_ms);
    }

    #[test]
    fn context_bounds_serde_roundtrip() {
        let bounds = ContextBounds {
            cwd_prefix: "/app".to_string(),
            required_shell: "zsh".to_string(),
            ..Default::default()
        };
        let json = serde_json::to_string(&bounds).unwrap();
        let decoded: ContextBounds = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, bounds);
    }

    #[test]
    fn intercept_decision_serde_roundtrip() {
        let d = InterceptDecision::Execute {
            reflex_id: 1,
            cluster_id: "c1".to_string(),
            priority: 0,
        };
        let json = serde_json::to_string(&d).unwrap();
        let decoded: InterceptDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, d);
    }

    #[test]
    fn intercept_stats_serde_roundtrip() {
        let stats = InterceptStats {
            total_attempts: 10,
            total_executions: 5,
            total_fallbacks: 5,
            active_executions: 1,
            is_paused: false,
            registered_bounds: 3,
            cooldown_entries: 2,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let decoded: InterceptStats = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, stats);
    }

    #[test]
    fn fallback_reason_serde_roundtrip() {
        let reason = FallbackReason::Cooldown { remaining_ms: 3000 };
        let json = serde_json::to_string(&reason).unwrap();
        let decoded: FallbackReason = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, reason);
    }

    #[test]
    fn pane_context_serde_roundtrip() {
        let ctx = test_context();
        let json = serde_json::to_string(&ctx).unwrap();
        let decoded: PaneContext = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, ctx);
    }
}
