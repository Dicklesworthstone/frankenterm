//! Cold-tier write-pipeline driver substrate (br-ft-95vfk
//! sub-tasks 6 + 7).
//!
//! Pure-logic scaffolding the integration's actual side-
//! effecting driver loop wraps. The substrate
//! [`cold_tier_pipeline`] ships
//! [`apply_step_outcome`] (state-machine transition) +
//! [`WritePipelineState`]; this module ships:
//!
//! - The **driver-tick** decision: given the current state,
//!   the retry counter, and the retry config, what should
//!   the integration do next? Run a step? Wait before
//!   retrying? Stop with success/failure?
//! - The **retry policy**: when a step has failed with a
//!   [`StepFailureReason::is_retryable`] reason and the
//!   integration has budget, return the backoff interval
//!   for the next attempt.
//! - The **JSON-line summary emitter** for the bead's
//!   "emit `ChunkWriteSummary` as JSON-line" requirement.
//! - **Retry telemetry** counters.
//!
//! [`cold_tier_pipeline`]: crate::cold_tier_pipeline
//! [`apply_step_outcome`]: crate::cold_tier_pipeline::apply_step_outcome
//! [`WritePipelineState`]: crate::cold_tier_pipeline::WritePipelineState
//! [`StepFailureReason::is_retryable`]: crate::cold_tier_pipeline::StepFailureReason::is_retryable
//!
//! Splitting the driver decision from the side-effecting
//! step calls means the retry / backoff doctrine is unit-
//! tested without touching disk, the keychain, the
//! redactor, zstd, or SQLite. The integration's driver
//! loop is then a tight wrapper:
//!
//! ```text
//! loop {
//!     match evaluate_driver_tick(&state, retries, config) {
//!         RunStep(step) => { state = run(step).await; }
//!         WaitBeforeRetry(ms) => { sleep(ms); retries += 1; reset state; }
//!         Done(summary)   => { emit_jsonl(summary); break; }
//!         Failed(summary) => { emit_jsonl(summary); break; }
//!     }
//! }
//! ```

use serde::{Deserialize, Serialize};

use crate::cold_tier_pipeline::{
    ChunkWriteSummary, StepFailureReason, WritePipelineState, WritePipelineStep,
};

// ============================================================================
// Retry config
// ============================================================================

/// Retry budget + backoff envelope. Retries fire only when
/// the failure is [`StepFailureReason::is_retryable`] AND
/// the integration's [`WritePipelineConfig::retry_on_retryable`]
/// flag is set AND the count is under [`Self::max_retries`].
///
/// [`WritePipelineConfig::retry_on_retryable`]: crate::cold_tier_pipeline::WritePipelineConfig::retry_on_retryable
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryPolicyConfig {
    /// Maximum number of retry attempts after the initial
    /// failure. Default 3.
    pub max_retries: u32,
    /// Base backoff in ms before the first retry. Default
    /// 100 ms.
    pub base_backoff_ms: u64,
    /// Maximum backoff in ms (the exponential is clamped at
    /// this ceiling). Default 30_000 ms (30 s).
    pub max_backoff_ms: u64,
    /// Multiplier applied per attempt: backoff =
    /// base × multiplier^retry_count, clamped at
    /// max_backoff_ms. Default 2.
    pub backoff_multiplier: u32,
}

pub const DEFAULT_MAX_RETRIES: u32 = 3;
pub const DEFAULT_BASE_BACKOFF_MS: u64 = 100;
pub const DEFAULT_MAX_BACKOFF_MS: u64 = 30_000;
pub const DEFAULT_BACKOFF_MULTIPLIER: u32 = 2;

impl Default for RetryPolicyConfig {
    fn default() -> Self {
        Self {
            max_retries: DEFAULT_MAX_RETRIES,
            base_backoff_ms: DEFAULT_BASE_BACKOFF_MS,
            max_backoff_ms: DEFAULT_MAX_BACKOFF_MS,
            backoff_multiplier: DEFAULT_BACKOFF_MULTIPLIER,
        }
    }
}

impl RetryPolicyConfig {
    /// Repair pathological config values so retry decisions
    /// remain well-defined. Zero base / max bumps to 1 ms;
    /// runaway max caps at 1 hour; multiplier 0 → 1 (linear
    /// backoff floor).
    #[must_use]
    pub fn with_repaired_invariants(mut self) -> Self {
        if self.base_backoff_ms == 0 {
            self.base_backoff_ms = 1;
        }
        if self.max_backoff_ms == 0 {
            self.max_backoff_ms = 1;
        }
        if self.max_backoff_ms > 3_600_000 {
            self.max_backoff_ms = 3_600_000;
        }
        if self.base_backoff_ms > self.max_backoff_ms {
            self.base_backoff_ms = self.max_backoff_ms;
        }
        if self.backoff_multiplier == 0 {
            self.backoff_multiplier = 1;
        }
        self
    }
}

// ============================================================================
// Retry decision
// ============================================================================

/// Decision the driver acts on. The integration's loop is a
/// straightforward dispatch: `RunStep` runs the step then
/// re-feeds the outcome; `WaitBeforeRetry` sleeps then
/// resets state to the failed step's start; `Done`/`Failed`
/// emits the summary JSONL and exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDecision {
    /// Don't retry. The state machine reached a terminal
    /// state (Done or Failed-non-retryable / out-of-budget).
    NoRetry,
    /// Retry after waiting `ms`. The integration sleeps,
    /// resets the pipeline state to the failed step's
    /// starting point, increments the retry counter, then
    /// re-runs the step.
    RetryAfterMs(u64),
}

/// Decide whether the integration should retry the
/// current pipeline state.
///
/// Returns `RetryAfterMs` only when:
/// - state is `Failed { reason }` AND `reason.is_retryable()`,
/// - `retry_on_retryable` is enabled,
/// - `retries_so_far < max_retries`.
///
/// Otherwise returns `NoRetry`.
#[must_use]
pub fn evaluate_retry(
    state: WritePipelineState,
    retries_so_far: u32,
    retry_on_retryable: bool,
    config: RetryPolicyConfig,
) -> RetryDecision {
    let config = config.with_repaired_invariants();

    let reason = match state {
        WritePipelineState::Failed { reason, .. } => reason,
        WritePipelineState::Pending(_) | WritePipelineState::Done => {
            return RetryDecision::NoRetry;
        }
    };

    if !retry_on_retryable {
        return RetryDecision::NoRetry;
    }
    if !reason.is_retryable() {
        return RetryDecision::NoRetry;
    }
    if retries_so_far >= config.max_retries {
        return RetryDecision::NoRetry;
    }

    let ms = compute_backoff_ms(retries_so_far, config);
    RetryDecision::RetryAfterMs(ms)
}

/// Pure backoff arithmetic. `retry_count = 0` returns
/// `base_backoff_ms`. Subsequent retries multiply by
/// `backoff_multiplier`, clamped at `max_backoff_ms`. All
/// math saturates so a runaway multiplier × retry_count
/// can't overflow.
#[must_use]
pub fn compute_backoff_ms(retry_count: u32, config: RetryPolicyConfig) -> u64 {
    let config = config.with_repaired_invariants();
    let mult = u64::from(config.backoff_multiplier);
    let mut value = config.base_backoff_ms;
    for _ in 0..retry_count {
        value = value.saturating_mul(mult);
        if value >= config.max_backoff_ms {
            return config.max_backoff_ms;
        }
    }
    value.min(config.max_backoff_ms)
}

// ============================================================================
// Driver tick
// ============================================================================

/// Action the integration's driver loop takes next based on
/// the current pipeline state + retry context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverTickAction {
    /// Run the named step. The integration calls the step
    /// (Compress/Redact/Encrypt/Persist/Index), receives a
    /// [`StepOutcome`], applies it via `apply_step_outcome`,
    /// then ticks again.
    ///
    /// [`StepOutcome`]: crate::cold_tier_pipeline::StepOutcome
    RunStep(WritePipelineStep),
    /// Sleep for `ms` then retry. The integration resets the
    /// state to the failed step's starting point and
    /// increments its `retries_so_far` counter.
    WaitBeforeRetry(u64),
    /// Pipeline done. The integration emits the summary
    /// JSONL and exits.
    Done,
    /// Pipeline failed and won't retry. The integration
    /// emits the summary JSONL and exits.
    Failed,
}

/// Decide what the driver loop should do next.
///
/// `Pending(step)` → `RunStep(step)`.
/// `Done`           → `Done`.
/// `Failed`         → `WaitBeforeRetry(ms)` if eligible,
///                     else `Failed`.
#[must_use]
pub fn evaluate_driver_tick(
    state: WritePipelineState,
    retries_so_far: u32,
    retry_on_retryable: bool,
    config: RetryPolicyConfig,
) -> DriverTickAction {
    match state {
        WritePipelineState::Pending(step) => DriverTickAction::RunStep(step),
        WritePipelineState::Done => DriverTickAction::Done,
        WritePipelineState::Failed { .. } => {
            match evaluate_retry(state, retries_so_far, retry_on_retryable, config) {
                RetryDecision::RetryAfterMs(ms) => DriverTickAction::WaitBeforeRetry(ms),
                RetryDecision::NoRetry => DriverTickAction::Failed,
            }
        }
    }
}

// ============================================================================
// JSON-line summary emitter
// ============================================================================

/// Per-chunk JSONL row the driver emits at the end of each
/// pipeline run. Mirrors [`ChunkWriteSummary`] plus a
/// `retry_count` field so operators can correlate retried
/// chunks with their final disposition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkWriteSummaryJsonl {
    pub steps_completed: u32,
    pub final_state_label: String,
    pub failed_step_label: Option<String>,
    pub failure_reason_label: Option<String>,
    pub elapsed_ns: u64,
    pub bytes_in: u64,
    pub bytes_out_post_compress: u64,
    pub bytes_out_post_encrypt: u64,
    pub retry_count: u32,
}

impl ChunkWriteSummaryJsonl {
    /// Convert a [`ChunkWriteSummary`] + retry-count into the
    /// JSONL row.
    #[must_use]
    pub fn from_summary(summary: &ChunkWriteSummary, retry_count: u32) -> Self {
        let (final_state_label, failed_step_label, failure_reason_label) = match summary.final_state
        {
            WritePipelineState::Done => ("done".to_string(), None, None),
            WritePipelineState::Pending(step) => {
                ("pending".to_string(), Some(step.label().to_string()), None)
            }
            WritePipelineState::Failed { step, reason } => (
                "failed".to_string(),
                Some(step.label().to_string()),
                Some(reason.label().to_string()),
            ),
        };
        Self {
            steps_completed: summary.steps_completed,
            final_state_label,
            failed_step_label,
            failure_reason_label,
            elapsed_ns: summary.elapsed_ns,
            bytes_in: summary.bytes_in,
            bytes_out_post_compress: summary.bytes_out_post_compress,
            bytes_out_post_encrypt: summary.bytes_out_post_encrypt,
            retry_count,
        }
    }
}

/// Render one JSONL row (one line of valid JSON, no
/// trailing newline). The integration appends `\n` and
/// flushes.
#[must_use]
pub fn render_summary_jsonl(summary: &ChunkWriteSummary, retry_count: u32) -> String {
    let row = ChunkWriteSummaryJsonl::from_summary(summary, retry_count);
    serde_json::to_string(&row).expect("ChunkWriteSummaryJsonl serializes")
}

// ============================================================================
// Retry telemetry
// ============================================================================

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryTelemetry {
    /// Total retries fired across all chunks.
    pub retries_total: u64,
    /// Retries that exhausted the budget (max_retries hit).
    pub retries_budget_exhausted_total: u64,
    /// Retries that succeeded (chunk eventually reached
    /// `Done` after at least one retry).
    pub retries_succeeded_total: u64,
    /// Highest retry-count observed for any one chunk.
    pub max_retry_count_observed: u32,
}

impl RetryTelemetry {
    /// Record one retry firing.
    pub fn record_retry(&mut self, retry_count_after: u32) {
        self.retries_total = self.retries_total.saturating_add(1);
        if retry_count_after > self.max_retry_count_observed {
            self.max_retry_count_observed = retry_count_after;
        }
    }

    /// Record final disposition of a chunk that needed at
    /// least one retry: `succeeded = true` if the chunk
    /// reached `Done`, `false` if it bailed (budget hit or
    /// non-retryable).
    pub fn record_chunk_disposition(&mut self, retries: u32, succeeded: bool) {
        if retries == 0 {
            return; // chunk completed first try; not a retry case
        }
        if succeeded {
            self.retries_succeeded_total = self.retries_succeeded_total.saturating_add(1);
        } else {
            self.retries_budget_exhausted_total =
                self.retries_budget_exhausted_total.saturating_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cold_tier_pipeline::{
        StepOutcome, WritePipelineConfig, WritePipelineState, apply_step_outcome,
    };

    // ----------------------------------------------------------------
    // RetryPolicyConfig invariant repair
    // ----------------------------------------------------------------

    #[test]
    fn config_default_matches_documented_constants() {
        let c = RetryPolicyConfig::default();
        assert_eq!(c.max_retries, DEFAULT_MAX_RETRIES);
        assert_eq!(c.base_backoff_ms, DEFAULT_BASE_BACKOFF_MS);
        assert_eq!(c.max_backoff_ms, DEFAULT_MAX_BACKOFF_MS);
        assert_eq!(c.backoff_multiplier, DEFAULT_BACKOFF_MULTIPLIER);
    }

    #[test]
    fn config_repair_clamps_zero_base_backoff() {
        let c = RetryPolicyConfig {
            base_backoff_ms: 0,
            ..Default::default()
        }
        .with_repaired_invariants();
        assert_eq!(c.base_backoff_ms, 1);
    }

    #[test]
    fn config_repair_caps_runaway_max_backoff_at_one_hour() {
        let c = RetryPolicyConfig {
            max_backoff_ms: u64::MAX,
            ..Default::default()
        }
        .with_repaired_invariants();
        assert_eq!(c.max_backoff_ms, 3_600_000);
    }

    #[test]
    fn config_repair_clamps_zero_multiplier_to_one() {
        let c = RetryPolicyConfig {
            backoff_multiplier: 0,
            ..Default::default()
        }
        .with_repaired_invariants();
        assert_eq!(c.backoff_multiplier, 1);
    }

    #[test]
    fn config_repair_clamps_base_above_max() {
        let c = RetryPolicyConfig {
            base_backoff_ms: 50_000,
            max_backoff_ms: 1_000,
            ..Default::default()
        }
        .with_repaired_invariants();
        assert_eq!(c.base_backoff_ms, 1_000);
    }

    // ----------------------------------------------------------------
    // compute_backoff_ms
    // ----------------------------------------------------------------

    #[test]
    fn backoff_first_retry_returns_base() {
        let cfg = RetryPolicyConfig::default();
        assert_eq!(compute_backoff_ms(0, cfg), DEFAULT_BASE_BACKOFF_MS);
    }

    #[test]
    fn backoff_grows_exponentially() {
        let cfg = RetryPolicyConfig::default();
        // base=100, mult=2, max=30_000
        // attempt 0 → 100
        // attempt 1 → 200
        // attempt 2 → 400
        // attempt 3 → 800
        assert_eq!(compute_backoff_ms(0, cfg), 100);
        assert_eq!(compute_backoff_ms(1, cfg), 200);
        assert_eq!(compute_backoff_ms(2, cfg), 400);
        assert_eq!(compute_backoff_ms(3, cfg), 800);
    }

    #[test]
    fn backoff_clamps_at_max() {
        let cfg = RetryPolicyConfig {
            base_backoff_ms: 100,
            max_backoff_ms: 1_000,
            backoff_multiplier: 2,
            max_retries: 100,
        };
        // 100, 200, 400, 800, 1600 → clamped to 1000.
        assert_eq!(compute_backoff_ms(4, cfg), 1_000);
        assert_eq!(compute_backoff_ms(50, cfg), 1_000);
    }

    #[test]
    fn backoff_handles_runaway_multiplier_via_saturation() {
        let cfg = RetryPolicyConfig {
            base_backoff_ms: 10,
            max_backoff_ms: 1_000_000,
            backoff_multiplier: u32::MAX,
            max_retries: 100,
        };
        // saturating_mul caps at u64::MAX; clamp returns
        // max_backoff_ms.
        let out = compute_backoff_ms(5, cfg);
        assert_eq!(out, 1_000_000);
    }

    // ----------------------------------------------------------------
    // evaluate_retry
    // ----------------------------------------------------------------

    #[test]
    fn evaluate_retry_no_retry_for_pending_state() {
        let dec = evaluate_retry(
            WritePipelineState::Pending(WritePipelineStep::Redact),
            0,
            true,
            RetryPolicyConfig::default(),
        );
        assert_eq!(dec, RetryDecision::NoRetry);
    }

    #[test]
    fn evaluate_retry_no_retry_for_done_state() {
        let dec = evaluate_retry(
            WritePipelineState::Done,
            0,
            true,
            RetryPolicyConfig::default(),
        );
        assert_eq!(dec, RetryDecision::NoRetry);
    }

    #[test]
    fn evaluate_retry_no_retry_for_non_retryable_failure() {
        // CompressionRatioBelowFloor is permanent for this
        // chunk (substrate's is_retryable returns false).
        let dec = evaluate_retry(
            WritePipelineState::Failed {
                step: WritePipelineStep::Compress,
                reason: StepFailureReason::CompressionRatioBelowFloor,
            },
            0,
            true,
            RetryPolicyConfig::default(),
        );
        assert_eq!(dec, RetryDecision::NoRetry);
    }

    #[test]
    fn evaluate_retry_no_retry_when_disabled() {
        // retry_on_retryable=false → no retry even on
        // retryable failure.
        let dec = evaluate_retry(
            WritePipelineState::Failed {
                step: WritePipelineStep::Persist,
                reason: StepFailureReason::DiskFull,
            },
            0,
            false,
            RetryPolicyConfig::default(),
        );
        assert_eq!(dec, RetryDecision::NoRetry);
    }

    #[test]
    fn evaluate_retry_no_retry_above_max_retries() {
        let cfg = RetryPolicyConfig::default();
        let dec = evaluate_retry(
            WritePipelineState::Failed {
                step: WritePipelineStep::Persist,
                reason: StepFailureReason::DiskFull,
            },
            cfg.max_retries, // already at the max
            true,
            cfg,
        );
        assert_eq!(dec, RetryDecision::NoRetry);
    }

    #[test]
    fn evaluate_retry_returns_retry_for_retryable_disk_full() {
        let cfg = RetryPolicyConfig::default();
        let dec = evaluate_retry(
            WritePipelineState::Failed {
                step: WritePipelineStep::Persist,
                reason: StepFailureReason::DiskFull,
            },
            0,
            true,
            cfg,
        );
        assert_eq!(dec, RetryDecision::RetryAfterMs(DEFAULT_BASE_BACKOFF_MS));
    }

    #[test]
    fn evaluate_retry_uses_growing_backoff_per_attempt() {
        let cfg = RetryPolicyConfig::default();
        let mk = |n| {
            evaluate_retry(
                WritePipelineState::Failed {
                    step: WritePipelineStep::Persist,
                    reason: StepFailureReason::DiskFull,
                },
                n,
                true,
                cfg,
            )
        };
        assert_eq!(mk(0), RetryDecision::RetryAfterMs(100));
        assert_eq!(mk(1), RetryDecision::RetryAfterMs(200));
        assert_eq!(mk(2), RetryDecision::RetryAfterMs(400));
    }

    #[test]
    fn evaluate_retry_no_retry_on_illegal_skip_even_within_budget() {
        // IllegalSkip is permanent — substrate's privacy
        // guard. Retry would re-fire the same illegal call.
        let dec = evaluate_retry(
            WritePipelineState::Failed {
                step: WritePipelineStep::Redact,
                reason: StepFailureReason::IllegalSkip,
            },
            0,
            true,
            RetryPolicyConfig::default(),
        );
        assert_eq!(dec, RetryDecision::NoRetry);
    }

    // ----------------------------------------------------------------
    // evaluate_driver_tick
    // ----------------------------------------------------------------

    #[test]
    fn driver_tick_runs_step_when_pending() {
        let dec = evaluate_driver_tick(
            WritePipelineState::Pending(WritePipelineStep::Redact),
            0,
            true,
            RetryPolicyConfig::default(),
        );
        assert_eq!(dec, DriverTickAction::RunStep(WritePipelineStep::Redact));
    }

    #[test]
    fn driver_tick_done_when_done() {
        let dec = evaluate_driver_tick(
            WritePipelineState::Done,
            0,
            true,
            RetryPolicyConfig::default(),
        );
        assert_eq!(dec, DriverTickAction::Done);
    }

    #[test]
    fn driver_tick_waits_when_failed_retryable_within_budget() {
        let dec = evaluate_driver_tick(
            WritePipelineState::Failed {
                step: WritePipelineStep::Persist,
                reason: StepFailureReason::DiskFull,
            },
            0,
            true,
            RetryPolicyConfig::default(),
        );
        assert_eq!(dec, DriverTickAction::WaitBeforeRetry(100));
    }

    #[test]
    fn driver_tick_failed_when_non_retryable() {
        let dec = evaluate_driver_tick(
            WritePipelineState::Failed {
                step: WritePipelineStep::Compress,
                reason: StepFailureReason::CompressionRatioBelowFloor,
            },
            0,
            true,
            RetryPolicyConfig::default(),
        );
        assert_eq!(dec, DriverTickAction::Failed);
    }

    #[test]
    fn driver_tick_failed_when_budget_exhausted() {
        let cfg = RetryPolicyConfig::default();
        let dec = evaluate_driver_tick(
            WritePipelineState::Failed {
                step: WritePipelineStep::Persist,
                reason: StepFailureReason::DiskFull,
            },
            cfg.max_retries,
            true,
            cfg,
        );
        assert_eq!(dec, DriverTickAction::Failed);
    }

    // ----------------------------------------------------------------
    // ChunkWriteSummaryJsonl
    // ----------------------------------------------------------------

    #[test]
    fn jsonl_from_done_summary_has_no_failed_step() {
        let summary = ChunkWriteSummary {
            steps_completed: 5,
            final_state: WritePipelineState::Done,
            elapsed_ns: 12_345_000,
            bytes_in: 4096,
            bytes_out_post_compress: 1024,
            bytes_out_post_encrypt: 1100,
        };
        let row = ChunkWriteSummaryJsonl::from_summary(&summary, 0);
        assert_eq!(row.final_state_label, "done");
        assert_eq!(row.failed_step_label, None);
        assert_eq!(row.failure_reason_label, None);
        assert_eq!(row.retry_count, 0);
        assert_eq!(row.steps_completed, 5);
    }

    #[test]
    fn jsonl_from_failed_summary_carries_step_and_reason() {
        let summary = ChunkWriteSummary {
            steps_completed: 2,
            final_state: WritePipelineState::Failed {
                step: WritePipelineStep::Persist,
                reason: StepFailureReason::DiskFull,
            },
            elapsed_ns: 5_000_000,
            bytes_in: 4096,
            bytes_out_post_compress: 1024,
            bytes_out_post_encrypt: 0,
        };
        let row = ChunkWriteSummaryJsonl::from_summary(&summary, 3);
        assert_eq!(row.final_state_label, "failed");
        assert_eq!(row.failed_step_label.as_deref(), Some("persist"));
        assert_eq!(row.failure_reason_label.as_deref(), Some("disk_full"));
        assert_eq!(row.retry_count, 3);
    }

    #[test]
    fn render_summary_jsonl_emits_valid_one_line_json() {
        let summary = ChunkWriteSummary {
            steps_completed: 5,
            final_state: WritePipelineState::Done,
            elapsed_ns: 1_000,
            bytes_in: 100,
            bytes_out_post_compress: 50,
            bytes_out_post_encrypt: 60,
        };
        let line = render_summary_jsonl(&summary, 1);
        assert!(!line.contains('\n'), "JSONL line must not contain newline");
        let back: ChunkWriteSummaryJsonl =
            serde_json::from_str(&line).expect("JSONL line must roundtrip");
        assert_eq!(back.final_state_label, "done");
        assert_eq!(back.retry_count, 1);
    }

    // ----------------------------------------------------------------
    // RetryTelemetry
    // ----------------------------------------------------------------

    #[test]
    fn telemetry_record_retry_increments_total_and_max() {
        let mut t = RetryTelemetry::default();
        t.record_retry(1);
        t.record_retry(2);
        t.record_retry(3);
        assert_eq!(t.retries_total, 3);
        assert_eq!(t.max_retry_count_observed, 3);
    }

    #[test]
    fn telemetry_disposition_first_try_does_not_count() {
        let mut t = RetryTelemetry::default();
        t.record_chunk_disposition(0, true);
        assert_eq!(t.retries_succeeded_total, 0);
        assert_eq!(t.retries_budget_exhausted_total, 0);
    }

    #[test]
    fn telemetry_disposition_routes_succeeded_and_exhausted() {
        let mut t = RetryTelemetry::default();
        t.record_chunk_disposition(2, true);
        t.record_chunk_disposition(3, false);
        assert_eq!(t.retries_succeeded_total, 1);
        assert_eq!(t.retries_budget_exhausted_total, 1);
    }

    // ----------------------------------------------------------------
    // End-to-end: drive a full pipeline through with one retry
    // ----------------------------------------------------------------

    #[test]
    fn scenario_disk_full_retries_once_then_succeeds() {
        // First Persist fails with DiskFull; the retry
        // succeeds. Verify the driver waits 100ms (first
        // backoff), then resumes and reaches Done with
        // retries=1 and the telemetry attributed to the
        // succeeded-after-retry bucket.
        let cfg = RetryPolicyConfig::default();
        let mut state = WritePipelineState::new();
        let mut retries = 0u32;
        let mut persist_attempts = 0u32;
        let mut tlm = RetryTelemetry::default();
        let pipeline_cfg = WritePipelineConfig::default();

        loop {
            match evaluate_driver_tick(state, retries, true, cfg) {
                DriverTickAction::RunStep(step) => {
                    let outcome =
                        if step == WritePipelineStep::Encrypt && !pipeline_cfg.encrypt_at_rest {
                            StepOutcome::Skipped
                        } else if step == WritePipelineStep::Persist {
                            persist_attempts += 1;
                            if persist_attempts == 1 {
                                StepOutcome::Failure(StepFailureReason::DiskFull)
                            } else {
                                StepOutcome::Success
                            }
                        } else {
                            StepOutcome::Success
                        };
                    let _ = apply_step_outcome(&mut state, outcome);
                }
                DriverTickAction::WaitBeforeRetry(ms) => {
                    // First (and only) retry uses the base
                    // backoff because retries=0 at this
                    // point.
                    assert_eq!(ms, DEFAULT_BASE_BACKOFF_MS);
                    retries += 1;
                    tlm.record_retry(retries);
                    state = WritePipelineState::Pending(WritePipelineStep::Persist);
                }
                DriverTickAction::Done => {
                    tlm.record_chunk_disposition(retries, true);
                    break;
                }
                DriverTickAction::Failed => {
                    panic!("scenario should succeed after one retry");
                }
            }
            if retries > 5 {
                panic!("guard: too many retries");
            }
        }

        assert_eq!(retries, 1);
        assert_eq!(persist_attempts, 2);
        assert_eq!(tlm.retries_total, 1);
        assert_eq!(tlm.retries_succeeded_total, 1);
        assert_eq!(tlm.retries_budget_exhausted_total, 0);
    }

    #[test]
    fn scenario_disk_full_retries_until_budget_exhausted() {
        let cfg = RetryPolicyConfig::default();
        let mut state = WritePipelineState::new();
        let mut retries = 0u32;
        let mut tlm = RetryTelemetry::default();

        // Drive each step success until Persist; persist always
        // fails with DiskFull.
        loop {
            match evaluate_driver_tick(state, retries, true, cfg) {
                DriverTickAction::RunStep(step) => {
                    let outcome = if step == WritePipelineStep::Encrypt {
                        StepOutcome::Skipped
                    } else if step == WritePipelineStep::Persist {
                        StepOutcome::Failure(StepFailureReason::DiskFull)
                    } else {
                        StepOutcome::Success
                    };
                    let _ = apply_step_outcome(&mut state, outcome);
                }
                DriverTickAction::WaitBeforeRetry(_) => {
                    retries += 1;
                    tlm.record_retry(retries);
                    state = WritePipelineState::Pending(WritePipelineStep::Persist);
                }
                DriverTickAction::Failed => {
                    tlm.record_chunk_disposition(retries, false);
                    break;
                }
                DriverTickAction::Done => panic!("expected Failed eventually"),
            }
            if retries > 10 {
                panic!("guard: retries unbounded");
            }
        }

        // After max_retries=3, the next failure leaves the
        // tick in the Failed-no-retry branch. Telemetry should
        // record the budget-exhausted disposition.
        assert_eq!(retries, cfg.max_retries);
        assert_eq!(tlm.retries_total, cfg.max_retries as u64);
        assert_eq!(tlm.retries_budget_exhausted_total, 1);
        assert_eq!(tlm.retries_succeeded_total, 0);
    }
}
