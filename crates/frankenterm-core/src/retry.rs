//! Retry with exponential backoff.
//!
//! Provides a standardized retry policy for all fallible I/O operations in wa.
//! This module works in conjunction with the circuit breaker to provide robust
//! error handling and prevent retry storms.
//!
//! # Usage
//!
//! ```no_run
//! use frankenterm_core::retry::{RetryPolicy, with_retry};
//!
//! # async fn example() -> frankenterm_core::Result<()> {
//! let policy = RetryPolicy::default();
//!
//! let result: frankenterm_core::Result<u64> = with_retry(&policy, || async {
//!     Ok(42) // fallible operation
//! }).await;
//! # Ok(())
//! # }
//! ```
//!
//! # Integration with Circuit Breaker
//!
//! When a circuit breaker is provided, retries will be skipped if the circuit
//! is open. Exceeded retries count as circuit failures.

use std::future::Future;
use std::time::Duration;

use rand::Rng;
use tracing::{debug, warn};

use crate::circuit_breaker::CircuitBreaker;
use crate::error::{Error, Result};

/// Configuration for retry behavior with exponential backoff.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Initial delay before first retry (default: 100ms).
    pub initial_delay: Duration,
    /// Maximum delay between retries (default: 30s).
    pub max_delay: Duration,
    /// Multiplier applied to delay after each retry (default: 2.0).
    pub backoff_factor: f64,
    /// Random jitter range as percentage (default: 0.1 = ±10%).
    pub jitter_percent: f64,
    /// Maximum number of retry attempts. None = retry forever (use with caution).
    pub max_attempts: Option<u32>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(30),
            backoff_factor: 2.0,
            jitter_percent: 0.1,
            max_attempts: Some(3),
        }
    }
}

impl RetryPolicy {
    /// Create a new retry policy with the specified parameters.
    #[must_use]
    pub fn new(
        initial_delay: Duration,
        max_delay: Duration,
        backoff_factor: f64,
        jitter_percent: f64,
        max_attempts: Option<u32>,
    ) -> Self {
        Self {
            initial_delay,
            max_delay,
            backoff_factor: backoff_factor.max(1.0),
            jitter_percent: jitter_percent.clamp(0.0, 1.0),
            max_attempts,
        }
    }

    /// Policy for WezTerm CLI calls: 3 attempts, 100ms initial.
    #[must_use]
    pub fn wezterm_cli() -> Self {
        Self {
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(5),
            backoff_factor: 2.0,
            jitter_percent: 0.1,
            max_attempts: Some(3),
        }
    }

    /// Policy for database writes: 5 attempts, 50ms initial.
    #[must_use]
    pub fn db_write() -> Self {
        Self {
            initial_delay: Duration::from_millis(50),
            max_delay: Duration::from_secs(2),
            backoff_factor: 2.0,
            jitter_percent: 0.1,
            max_attempts: Some(5),
        }
    }

    /// Policy for webhook delivery: 5 attempts, 1s initial.
    #[must_use]
    pub fn webhook() -> Self {
        Self {
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(60),
            backoff_factor: 2.0,
            jitter_percent: 0.1,
            max_attempts: Some(5),
        }
    }

    /// Policy for browser automation: 2 attempts, 500ms initial.
    #[must_use]
    pub fn browser() -> Self {
        Self {
            initial_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(10),
            backoff_factor: 2.0,
            jitter_percent: 0.1,
            max_attempts: Some(2),
        }
    }

    /// Calculate the delay for a given attempt number (0-indexed).
    #[must_use]
    #[allow(clippy::cast_precision_loss)] // ms values are well within f64 precision for delays
    #[allow(clippy::cast_possible_wrap)] // attempt is capped at 31, safe for i32
    pub fn delay_for_attempt(&self, attempt: u32) -> Duration {
        // Clamp to sane values - delays beyond u64::MAX ms are not practical
        let initial_ms = u64::try_from(self.initial_delay.as_millis()).unwrap_or(u64::MAX);
        let max_ms = u64::try_from(self.max_delay.as_millis()).unwrap_or(u64::MAX);

        // Cap exponent to prevent overflow in powi; 31 iterations of 2x is already huge
        let exp = attempt.min(31) as i32;
        let base_ms = (initial_ms as f64) * self.backoff_factor.powi(exp);
        let base_ms = base_ms.min(max_ms as f64);

        // Apply jitter: ±jitter_percent
        let jitter = if self.jitter_percent > 0.0 {
            let mut rng = rand::rng();
            let jitter_range = base_ms * self.jitter_percent;
            rng.random_range(-jitter_range..=jitter_range)
        } else {
            0.0
        };

        let delay_ms = (base_ms + jitter).max(0.0).round();
        Duration::from_millis(delay_ms as u64)
    }
}

/// Outcome of a retry operation.
#[derive(Debug)]
pub struct RetryOutcome<T> {
    /// The result (success or final error).
    pub result: Result<T>,
    /// Number of attempts made.
    pub attempts: u32,
    /// Total time spent (including delays).
    pub elapsed: Duration,
}

/// Execute an async operation with retry and exponential backoff.
///
/// The operation will be retried according to the policy until it succeeds
/// or the maximum number of attempts is exhausted.
///
/// # Logging
///
/// Each retry attempt is logged with:
/// - Attempt number
/// - Delay applied
/// - Error that triggered the retry
pub async fn with_retry<T, F, Fut>(policy: &RetryPolicy, operation: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    with_retry_outcome(policy, operation).await.result
}

/// Execute an async operation with retry under an explicit `&Cx`.
///
/// Cx-first wrapper around [`with_retry_outcome_cx`] (ft-xbnl0.2.2).
pub async fn with_retry_cx<T, F, Fut>(
    cx: &crate::cx::Cx,
    policy: &RetryPolicy,
    operation: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    with_retry_outcome_cx(cx, policy, operation).await.result
}

/// Execute an async operation with retry, returning detailed outcome.
pub async fn with_retry_outcome<T, F, Fut>(policy: &RetryPolicy, operation: F) -> RetryOutcome<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        with_retry_outcome_cx(&cx, policy, operation).await
    }
}

/// Execute an async operation with retry under an explicit `&Cx`, returning
/// detailed outcome.
///
/// This is the Cx-first entry point (ft-xbnl0.2.2): callers that already
/// thread a capability context down their call graph should prefer this so
/// that cancellation, budget, and virtual time propagate cleanly into the
/// retry sleep. Inter-attempt sleeps use [`crate::runtime_compat::sleep_with_cx`]
/// which binds the sleep to the provided `Cx`.
pub async fn with_retry_outcome_cx<T, F, Fut>(
    cx: &crate::cx::Cx,
    policy: &RetryPolicy,
    mut operation: F,
) -> RetryOutcome<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let start = std::time::Instant::now();
    let mut attempt = 0u32;

    loop {
        // Tick 208 (ft-xbnl0.2.3): per-attempt cancel check so a
        // cx-cancel fired during the previous operation or sleep
        // surfaces AT THE ATTEMPT BOUNDARY rather than after the
        // next operation() call completes. Returns the last-captured
        // Cancelled error up through RetryOutcome.result so callers
        // can distinguish cancel from other terminal errors.
        if cx.is_cancel_requested() {
            return RetryOutcome {
                result: Err(Error::Cancelled(
                    "retry loop cancelled before next attempt".to_string(),
                )),
                attempts: attempt,
                elapsed: start.elapsed(),
            };
        }
        match operation().await {
            Ok(value) => {
                if attempt > 0 {
                    debug!(
                        total_attempts = attempt + 1,
                        retries = attempt,
                        "Operation succeeded after retries"
                    );
                }
                return RetryOutcome {
                    result: Ok(value),
                    attempts: attempt + 1,
                    elapsed: start.elapsed(),
                };
            }
            Err(e) => {
                attempt += 1;

                if let Some(max) = policy.max_attempts {
                    if attempt >= max {
                        warn!(
                            attempt,
                            max_attempts = max,
                            error = %e,
                            "Operation failed after all retry attempts"
                        );
                        return RetryOutcome {
                            result: Err(e),
                            attempts: attempt,
                            elapsed: start.elapsed(),
                        };
                    }
                }

                let delay = policy.delay_for_attempt(attempt - 1);
                debug!(
                    attempt,
                    delay_ms = delay.as_millis() as u64,
                    error = %e,
                    "Retrying operation after failure"
                );

                // Tick 208 (ft-xbnl0.2.3): honor the sleep_with_cx
                // result. Previously `let _ = ...` discarded the Err
                // so cancel during backoff was swallowed and the next
                // operation fired anyway. Now cancel during backoff
                // returns the original operation Err plus a cancelled
                // marker via the attempts count (the caller can
                // inspect elapsed vs. policy.delay_for_attempt to tell
                // cancel from natural timeout).
                if crate::runtime_compat::sleep_with_cx(cx, delay)
                    .await
                    .is_err()
                {
                    return RetryOutcome {
                        result: Err(Error::Cancelled(
                            "retry backoff cancelled during sleep".to_string(),
                        )),
                        attempts: attempt,
                        elapsed: start.elapsed(),
                    };
                }
            }
        }
    }
}

/// Execute an operation with retry and circuit breaker integration.
///
/// If the circuit is open, returns immediately with a circuit open error.
/// Exceeded retries count as a circuit failure.
///
/// Note: This function returns a `WeztermError::CircuitOpen` when the circuit
/// is open, making it most suitable for WezTerm CLI operations. For other
/// use cases, consider using `with_retry` and managing the circuit breaker
/// state separately.
pub async fn with_retry_and_circuit<T, F, Fut>(
    policy: &RetryPolicy,
    circuit: &mut CircuitBreaker,
    operation: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        return with_retry_and_circuit_cx(&cx, policy, circuit, operation).await;
    }

}

/// Circuit-aware retry under an explicit `&Cx` (ft-xbnl0.2.2).
pub async fn with_retry_and_circuit_cx<T, F, Fut>(
    cx: &crate::cx::Cx,
    policy: &RetryPolicy,
    circuit: &mut CircuitBreaker,
    operation: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    use crate::error::WeztermError;

    if !circuit.allow() {
        let status = circuit.status();
        let retry_after_ms = status.cooldown_remaining_ms.unwrap_or(0);
        return Err(Error::Wezterm(WeztermError::CircuitOpen { retry_after_ms }));
    }

    let outcome = with_retry_outcome_cx(cx, policy, operation).await;
    match &outcome.result {
        Ok(_) => circuit.record_success(),
        Err(_) => circuit.record_failure(),
    }
    outcome.result
}

/// Check if an error is retryable.
///
/// Some errors should not be retried (e.g., invalid arguments, not found).
/// This function provides a heuristic for retryability.
#[must_use]
pub fn is_retryable(error: &Error) -> bool {
    use crate::error::{StorageError, WeztermError};

    match error {
        // I/O errors are generally retryable (network issues, timeouts)
        Error::Io(_) => true,
        // WezTerm CLI errors - some are retryable
        Error::Wezterm(e) => match e {
            WeztermError::NotRunning => true,          // Might start up
            WeztermError::Timeout(_) => true,          // Temporary slowdown
            WeztermError::CommandFailed(_) => true,    // Might be transient
            WeztermError::CircuitOpen { .. } => false, // Already rate-limited
            WeztermError::CliNotFound => false,        // Need installation
            WeztermError::PaneNotFound(_) => false,    // Won't magically appear
            WeztermError::SocketNotFound(_) => true,   // Might be initializing
            WeztermError::ParseError(_) => false,      // Structural issue
        },
        // Storage errors - only generic database errors are retryable (lock conflicts)
        Error::Storage(e) => match e {
            StorageError::Database(_) => true, // Might be transient lock conflict
            StorageError::ReservationConflict { .. } => false, // Another owner must release first
            StorageError::SequenceDiscontinuity { .. } => false, // Logic error
            StorageError::MigrationFailed(_) => false, // Persistent issue
            StorageError::SchemaTooNew { .. } => false, // Version mismatch
            StorageError::WaTooOld { .. } => false, // Version mismatch
            StorageError::FtsQueryError(_) => false, // Query syntax issue
            StorageError::Corruption { .. } => false, // Serious issue
            StorageError::NotFound(_) => false, // Item doesn't exist
        },
        // Pattern errors are not retryable (invalid regex, etc.)
        Error::Pattern(_) => false,
        // Workflow errors are not retryable (logic errors)
        Error::Workflow(_) => false,
        // Configuration errors are not retryable
        Error::Config(_) => false,
        // Policy violations are not retryable
        Error::Policy(_) => false,
        // JSON errors are not retryable (structural issue)
        Error::Json(_) => false,
        // Runtime errors might be transient
        Error::Runtime(_) => true,
        // [ft-h9g0q] Typed runtime/pane/watchdog variants added in
        // 79702a50. Mirror the deprecated Runtime retry semantics
        // (generally transient / retryable) for backwards-compatible
        // behaviour until per-variant retry decisions can be keyed off
        // the inner `source` field. Conservative default: treat as
        // retryable, same as the Runtime fallback. A follow-up can
        // inspect `source` on each variant and flip non-transient
        // failures (e.g. PermissionDenied) to false.
        Error::RuntimeOperation { .. }
        | Error::PaneOperation { .. }
        | Error::WatchdogWarningRead { .. } => true,
        // Setup errors are not retryable
        Error::SetupError(_) => false,
        // Cancelled operations are not retryable
        Error::Cancelled(_) => false,
        // Panicked operations are not retryable
        Error::Panicked(_) => false,
    }
}

/// Execute an operation with smart retry (only retries if error is retryable).
pub async fn with_smart_retry<T, F, Fut>(policy: &RetryPolicy, operation: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        with_smart_retry_cx(&cx, policy, operation).await
    }
}

/// Smart retry under an explicit `&Cx` (ft-xbnl0.2.2).
pub async fn with_smart_retry_cx<T, F, Fut>(
    cx: &crate::cx::Cx,
    policy: &RetryPolicy,
    mut operation: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let start = std::time::Instant::now();
    let mut attempt = 0u32;

    loop {
        // Tick 208 (ft-xbnl0.2.3): per-attempt cancel check — same
        // rationale as with_retry_outcome_cx.
        if cx.is_cancel_requested() {
            return Err(Error::Cancelled(
                "smart retry loop cancelled before next attempt".to_string(),
            ));
        }
        match operation().await {
            Ok(value) => {
                if attempt > 0 {
                    debug!(
                        total_attempts = attempt + 1,
                        retries = attempt,
                        "Operation succeeded after retries"
                    );
                }
                return Ok(value);
            }
            Err(e) => {
                attempt += 1;
                if !is_retryable(&e) {
                    debug!(attempt, error = %e, "Non-retryable error, giving up");
                    return Err(e);
                }
                if let Some(max) = policy.max_attempts {
                    if attempt >= max {
                        warn!(
                            attempt,
                            max_attempts = max,
                            error = %e,
                            elapsed_ms = start.elapsed().as_millis() as u64,
                            "Operation failed after all retry attempts"
                        );
                        return Err(e);
                    }
                }
                let delay = policy.delay_for_attempt(attempt - 1);
                debug!(
                    attempt,
                    delay_ms = delay.as_millis() as u64,
                    error = %e,
                    "Retrying operation after retryable failure"
                );
                // Tick 208 (ft-xbnl0.2.3): honor the sleep_with_cx
                // result (see with_retry_outcome_cx for rationale).
                if crate::runtime_compat::sleep_with_cx(cx, delay)
                    .await
                    .is_err()
                {
                    return Err(Error::Cancelled(
                        "smart retry backoff cancelled during sleep".to_string(),
                    ));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// LabRuntime-based determinism test (ft-xbnl0.2.2): prove that Cx-first
    /// retry runs under seed-locked virtual-time scheduling with no wall-clock
    /// dependence. If inter-attempt sleeps ever re-acquire a tokio-shaped
    /// (real-time) assumption, this test will either block the wall clock or
    /// step-explode and fail.
    #[test]
    fn retry_runs_deterministically_under_labruntime_with_cx() {
        const SEED: u64 = 0x2A2B_C5E1_0102_2000;
        let wall_start = std::time::Instant::now();
        let attempts_observed = Arc::new(AtomicU32::new(0));
        let attempts_task = Arc::clone(&attempts_observed);
        let final_attempts = Arc::new(AtomicU32::new(0));
        let final_attempts_task = Arc::clone(&final_attempts);

        let mut runtime = asupersync::LabRuntime::new(
            asupersync::LabConfig::new(SEED)
                .with_auto_advance()
                .worker_count(1)
                .max_steps(50_000),
        );
        let region = runtime
            .state
            .create_root_region(asupersync::Budget::INFINITE);
        let (task_id, _handle) = runtime
            .state
            .create_task(region, asupersync::Budget::INFINITE, async move {
                // Reuse the root region's Cx so sleeps bind to lab virtual time.
                let cx = crate::cx::for_request();
                let policy = RetryPolicy {
                    initial_delay: Duration::from_millis(10),
                    max_delay: Duration::from_millis(50),
                    backoff_factor: 2.0,
                    jitter_percent: 0.0,
                    max_attempts: Some(4),
                };
                let outcome = with_retry_outcome_cx(&cx, &policy, || {
                    let n = attempts_task.fetch_add(1, Ordering::SeqCst);
                    async move {
                        if n < 3 {
                            Err(Error::Runtime(format!("attempt {n} transient")))
                        } else {
                            Ok::<_, Error>(n)
                        }
                    }
                })
                .await;
                final_attempts_task.store(outcome.attempts, Ordering::SeqCst);
                assert!(outcome.result.is_ok(), "expected success on attempt 4");
            })
            .expect("spawn retry task");
        runtime.scheduler.lock().schedule(task_id, 0);
        runtime.step_for_test();
        let _ = runtime.run_with_auto_advance();
        let report = runtime.run_until_quiescent_with_report();

        assert_eq!(
            final_attempts.load(Ordering::SeqCst),
            4,
            "retry must have issued exactly 4 attempts (3 failures + 1 success)"
        );
        assert_eq!(
            attempts_observed.load(Ordering::SeqCst),
            4,
            "operation closure must be invoked once per attempt"
        );
        assert!(
            report.oracle_report.all_passed(),
            "LabRuntime oracles must all pass: {report:?}"
        );
        assert!(
            wall_start.elapsed() < Duration::from_secs(1),
            "virtual-time retry sleeps must not consume real time; elapsed {:?}",
            wall_start.elapsed()
        );
    }

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        use crate::runtime_compat::CompatRuntime;
        let runtime = crate::runtime_compat::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("failed to build retry test runtime");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(future);
        }));
        // Absorb TLS destructor panics from asupersync during runtime drop.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(runtime);
        }));
        // Clear handle from TLS so it doesn't panic during thread exit.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::runtime_compat::clear_runtime_handle();
        }));
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn delay_calculation_with_backoff() {
        let policy = RetryPolicy {
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(10),
            backoff_factor: 2.0,
            jitter_percent: 0.0, // No jitter for deterministic test
            max_attempts: Some(5),
        };

        // Attempt 0: 100ms
        assert_eq!(policy.delay_for_attempt(0), Duration::from_millis(100));
        // Attempt 1: 200ms
        assert_eq!(policy.delay_for_attempt(1), Duration::from_millis(200));
        // Attempt 2: 400ms
        assert_eq!(policy.delay_for_attempt(2), Duration::from_millis(400));
        // Attempt 3: 800ms
        assert_eq!(policy.delay_for_attempt(3), Duration::from_millis(800));
    }

    #[test]
    fn delay_capped_at_max() {
        let policy = RetryPolicy {
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_millis(500),
            backoff_factor: 2.0,
            jitter_percent: 0.0,
            max_attempts: Some(10),
        };

        // Attempt 5: would be 3200ms but capped at 500ms
        assert_eq!(policy.delay_for_attempt(5), Duration::from_millis(500));
    }

    #[test]
    #[allow(clippy::cast_precision_loss)] // Test values are small enough for f64
    fn jitter_within_range() {
        let policy = RetryPolicy {
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(10),
            backoff_factor: 1.0, // No backoff for this test
            jitter_percent: 0.1, // ±10%
            max_attempts: Some(5),
        };

        // Run multiple times to check jitter is within range
        for _ in 0..100 {
            let delay = policy.delay_for_attempt(0);
            let delay_ms = delay.as_millis() as f64;
            // Should be within 900-1100ms (1000 ± 10%)
            assert!(delay_ms >= 900.0, "delay too small: {delay_ms}");
            assert!(delay_ms <= 1100.0, "delay too large: {delay_ms}");
        }
    }

    #[test]
    fn retry_succeeds_immediately() {
        run_async_test(async {
            let policy = RetryPolicy::default();
            let call_count = Arc::new(AtomicU32::new(0));
            let call_count_clone = Arc::clone(&call_count);

            let result = with_retry(&policy, || {
                let count = Arc::clone(&call_count_clone);
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, Error>(42)
                }
            })
            .await;

            assert!(result.is_ok());
            assert_eq!(result.unwrap(), 42);
            assert_eq!(call_count.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn retry_succeeds_after_failures() {
        run_async_test(async {
            let policy = RetryPolicy {
                initial_delay: Duration::from_millis(1),
                max_delay: Duration::from_millis(10),
                backoff_factor: 2.0,
                jitter_percent: 0.0,
                max_attempts: Some(5),
            };
            let call_count = Arc::new(AtomicU32::new(0));
            let call_count_clone = Arc::clone(&call_count);

            let result = with_retry(&policy, || {
                let count = Arc::clone(&call_count_clone);
                async move {
                    let n = count.fetch_add(1, Ordering::SeqCst);
                    if n < 2 {
                        Err(Error::Runtime("transient failure".into()))
                    } else {
                        Ok::<_, Error>(42)
                    }
                }
            })
            .await;

            assert!(result.is_ok());
            assert_eq!(result.unwrap(), 42);
            assert_eq!(call_count.load(Ordering::SeqCst), 3);
        });
    }

    #[test]
    fn retry_exhausts_attempts() {
        run_async_test(async {
            let policy = RetryPolicy {
                initial_delay: Duration::from_millis(1),
                max_delay: Duration::from_millis(10),
                backoff_factor: 2.0,
                jitter_percent: 0.0,
                max_attempts: Some(3),
            };
            let call_count = Arc::new(AtomicU32::new(0));
            let call_count_clone = Arc::clone(&call_count);

            let result: Result<i32> = with_retry(&policy, || {
                let count = Arc::clone(&call_count_clone);
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    Err(Error::Runtime("persistent failure".into()))
                }
            })
            .await;

            assert!(result.is_err());
            assert_eq!(call_count.load(Ordering::SeqCst), 3);
        });
    }

    #[test]
    fn retry_with_outcome_tracks_attempts() {
        run_async_test(async {
            let policy = RetryPolicy {
                initial_delay: Duration::from_millis(1),
                max_delay: Duration::from_millis(10),
                backoff_factor: 2.0,
                jitter_percent: 0.0,
                max_attempts: Some(5),
            };
            let call_count = Arc::new(AtomicU32::new(0));
            let call_count_clone = Arc::clone(&call_count);

            let outcome = with_retry_outcome(&policy, || {
                let count = Arc::clone(&call_count_clone);
                async move {
                    let n = count.fetch_add(1, Ordering::SeqCst);
                    if n < 2 {
                        Err(Error::Runtime("transient".into()))
                    } else {
                        Ok::<_, Error>(42)
                    }
                }
            })
            .await;

            assert!(outcome.result.is_ok());
            assert_eq!(outcome.attempts, 3);
        });
    }

    #[test]
    fn circuit_breaker_integration() {
        run_async_test(async {
            use crate::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};

            let policy = RetryPolicy {
                initial_delay: Duration::from_millis(1),
                max_delay: Duration::from_millis(10),
                backoff_factor: 2.0,
                jitter_percent: 0.0,
                max_attempts: Some(2),
            };

            let mut circuit = CircuitBreaker::new(CircuitBreakerConfig::new(
                1, // Open after 1 failure
                1,
                Duration::from_secs(60),
            ));

            // First call fails and trips circuit
            let result: Result<i32> = with_retry_and_circuit(&policy, &mut circuit, || async {
                Err(Error::Runtime("fail".into()))
            })
            .await;
            assert!(result.is_err());

            // Circuit should now be open
            let result: Result<i32> =
                with_retry_and_circuit(&policy, &mut circuit, || async { Ok(42) }).await;
            assert!(result.is_err());
            let err_msg = result.unwrap_err().to_string();
            assert!(
                err_msg.contains("circuit breaker is open"),
                "Expected circuit breaker error, got: {err_msg}"
            );
        });
    }

    #[test]
    fn preset_policies_have_sensible_defaults() {
        let wezterm = RetryPolicy::wezterm_cli();
        assert_eq!(wezterm.max_attempts, Some(3));
        assert_eq!(wezterm.initial_delay, Duration::from_millis(100));

        let db = RetryPolicy::db_write();
        assert_eq!(db.max_attempts, Some(5));
        assert_eq!(db.initial_delay, Duration::from_millis(50));

        let webhook = RetryPolicy::webhook();
        assert_eq!(webhook.max_attempts, Some(5));
        assert_eq!(webhook.initial_delay, Duration::from_secs(1));

        let browser = RetryPolicy::browser();
        assert_eq!(browser.max_attempts, Some(2));
        assert_eq!(browser.initial_delay, Duration::from_millis(500));
    }

    // ── Default trait ──────────────────────────────────────────────

    #[test]
    fn default_policy_fields() {
        let p = RetryPolicy::default();
        assert_eq!(p.initial_delay, Duration::from_millis(100));
        assert_eq!(p.max_delay, Duration::from_secs(30));
        assert!(
            (p.backoff_factor - 2.0).abs() < f64::EPSILON,
            "backoff_factor: {}",
            p.backoff_factor
        );
        assert!(
            (p.jitter_percent - 0.1).abs() < f64::EPSILON,
            "jitter_percent: {}",
            p.jitter_percent
        );
        assert_eq!(p.max_attempts, Some(3));
    }

    // ── RetryPolicy::new() clamping ────────────────────────────────

    #[test]
    fn new_clamps_backoff_factor_to_minimum_one() {
        let p = RetryPolicy::new(
            Duration::from_millis(100),
            Duration::from_secs(10),
            0.5,
            0.1,
            Some(3),
        );
        assert!(
            (p.backoff_factor - 1.0).abs() < f64::EPSILON,
            "backoff_factor: {}",
            p.backoff_factor
        );
    }

    #[test]
    fn new_clamps_negative_backoff_factor() {
        let p = RetryPolicy::new(
            Duration::from_millis(100),
            Duration::from_secs(10),
            -2.0,
            0.1,
            Some(3),
        );
        assert!(
            (p.backoff_factor - 1.0).abs() < f64::EPSILON,
            "backoff_factor: {}",
            p.backoff_factor
        );
    }

    #[test]
    fn new_preserves_valid_backoff_factor() {
        let p = RetryPolicy::new(
            Duration::from_millis(100),
            Duration::from_secs(10),
            3.5,
            0.1,
            Some(3),
        );
        assert!(
            (p.backoff_factor - 3.5).abs() < f64::EPSILON,
            "backoff_factor: {}",
            p.backoff_factor
        );
    }

    #[test]
    fn new_clamps_jitter_percent_above_one() {
        let p = RetryPolicy::new(
            Duration::from_millis(100),
            Duration::from_secs(10),
            2.0,
            1.5,
            Some(3),
        );
        assert!(
            (p.jitter_percent - 1.0).abs() < f64::EPSILON,
            "jitter_percent: {}",
            p.jitter_percent
        );
    }

    #[test]
    fn new_clamps_negative_jitter_percent() {
        let p = RetryPolicy::new(
            Duration::from_millis(100),
            Duration::from_secs(10),
            2.0,
            -0.3,
            Some(3),
        );
        assert!(
            p.jitter_percent.abs() < f64::EPSILON,
            "jitter_percent: {}",
            p.jitter_percent
        );
    }

    #[test]
    fn new_preserves_valid_jitter_percent() {
        let p = RetryPolicy::new(
            Duration::from_millis(100),
            Duration::from_secs(10),
            2.0,
            0.5,
            Some(3),
        );
        assert!(
            (p.jitter_percent - 0.5).abs() < f64::EPSILON,
            "jitter_percent: {}",
            p.jitter_percent
        );
    }

    #[test]
    fn new_accepts_none_max_attempts() {
        let p = RetryPolicy::new(
            Duration::from_millis(100),
            Duration::from_secs(10),
            2.0,
            0.1,
            None,
        );
        assert_eq!(p.max_attempts, None);
    }

    // ── delay_for_attempt edge cases ───────────────────────────────

    #[test]
    fn delay_for_attempt_zero_initial() {
        let policy = RetryPolicy {
            initial_delay: Duration::ZERO,
            max_delay: Duration::from_secs(10),
            backoff_factor: 2.0,
            jitter_percent: 0.0,
            max_attempts: Some(5),
        };
        assert_eq!(policy.delay_for_attempt(0), Duration::ZERO);
        assert_eq!(policy.delay_for_attempt(5), Duration::ZERO);
    }

    #[test]
    fn delay_for_attempt_high_attempt_capped_at_31() {
        let policy = RetryPolicy {
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_secs(u64::MAX),
            backoff_factor: 2.0,
            jitter_percent: 0.0,
            max_attempts: None,
        };
        let at_31 = policy.delay_for_attempt(31);
        let at_100 = policy.delay_for_attempt(100);
        assert_eq!(at_31, at_100);
    }

    #[test]
    fn delay_for_attempt_backoff_factor_one_stays_constant() {
        let policy = RetryPolicy {
            initial_delay: Duration::from_millis(200),
            max_delay: Duration::from_secs(10),
            backoff_factor: 1.0,
            jitter_percent: 0.0,
            max_attempts: Some(5),
        };
        for attempt in 0..5 {
            assert_eq!(
                policy.delay_for_attempt(attempt),
                Duration::from_millis(200)
            );
        }
    }

    #[test]
    fn delay_with_zero_jitter_is_deterministic() {
        let policy = RetryPolicy {
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(10),
            backoff_factor: 2.0,
            jitter_percent: 0.0,
            max_attempts: Some(5),
        };
        for _ in 0..50 {
            assert_eq!(policy.delay_for_attempt(2), Duration::from_millis(400));
        }
    }

    // ── Preset policy completeness ─────────────────────────────────

    #[test]
    fn preset_wezterm_cli_all_fields() {
        let p = RetryPolicy::wezterm_cli();
        assert_eq!(p.initial_delay, Duration::from_millis(100));
        assert_eq!(p.max_delay, Duration::from_secs(5));
        assert!(
            (p.backoff_factor - 2.0).abs() < f64::EPSILON,
            "backoff_factor: {}",
            p.backoff_factor
        );
        assert!(
            (p.jitter_percent - 0.1).abs() < f64::EPSILON,
            "jitter_percent: {}",
            p.jitter_percent
        );
        assert_eq!(p.max_attempts, Some(3));
    }

    #[test]
    fn preset_db_write_all_fields() {
        let p = RetryPolicy::db_write();
        assert_eq!(p.initial_delay, Duration::from_millis(50));
        assert_eq!(p.max_delay, Duration::from_secs(2));
        assert!(
            (p.backoff_factor - 2.0).abs() < f64::EPSILON,
            "backoff_factor: {}",
            p.backoff_factor
        );
        assert!(
            (p.jitter_percent - 0.1).abs() < f64::EPSILON,
            "jitter_percent: {}",
            p.jitter_percent
        );
        assert_eq!(p.max_attempts, Some(5));
    }

    #[test]
    fn preset_webhook_all_fields() {
        let p = RetryPolicy::webhook();
        assert_eq!(p.initial_delay, Duration::from_secs(1));
        assert_eq!(p.max_delay, Duration::from_secs(60));
        assert!(
            (p.backoff_factor - 2.0).abs() < f64::EPSILON,
            "backoff_factor: {}",
            p.backoff_factor
        );
        assert!(
            (p.jitter_percent - 0.1).abs() < f64::EPSILON,
            "jitter_percent: {}",
            p.jitter_percent
        );
        assert_eq!(p.max_attempts, Some(5));
    }

    #[test]
    fn preset_browser_all_fields() {
        let p = RetryPolicy::browser();
        assert_eq!(p.initial_delay, Duration::from_millis(500));
        assert_eq!(p.max_delay, Duration::from_secs(10));
        assert!(
            (p.backoff_factor - 2.0).abs() < f64::EPSILON,
            "backoff_factor: {}",
            p.backoff_factor
        );
        assert!(
            (p.jitter_percent - 0.1).abs() < f64::EPSILON,
            "jitter_percent: {}",
            p.jitter_percent
        );
        assert_eq!(p.max_attempts, Some(2));
    }

    // ── is_retryable ───────────────────────────────────────────────

    #[test]
    fn is_retryable_io_error() {
        let err = Error::Io(std::io::Error::new(std::io::ErrorKind::TimedOut, "timeout"));
        assert!(is_retryable(&err));
    }

    #[test]
    fn is_retryable_wezterm_not_running() {
        use crate::error::WeztermError;
        assert!(is_retryable(&Error::Wezterm(WeztermError::NotRunning)));
    }

    #[test]
    fn is_retryable_wezterm_timeout() {
        use crate::error::WeztermError;
        assert!(is_retryable(&Error::Wezterm(WeztermError::Timeout(30))));
    }

    #[test]
    fn is_retryable_wezterm_command_failed() {
        use crate::error::WeztermError;
        assert!(is_retryable(&Error::Wezterm(WeztermError::CommandFailed(
            "stderr".into()
        ))));
    }

    #[test]
    fn is_retryable_wezterm_socket_not_found() {
        use crate::error::WeztermError;
        assert!(is_retryable(&Error::Wezterm(WeztermError::SocketNotFound(
            "/tmp/wez.sock".into()
        ))));
    }

    #[test]
    fn not_retryable_wezterm_circuit_open() {
        use crate::error::WeztermError;
        assert!(!is_retryable(&Error::Wezterm(WeztermError::CircuitOpen {
            retry_after_ms: 5000,
        })));
    }

    #[test]
    fn not_retryable_wezterm_cli_not_found() {
        use crate::error::WeztermError;
        assert!(!is_retryable(&Error::Wezterm(WeztermError::CliNotFound)));
    }

    #[test]
    fn not_retryable_wezterm_pane_not_found() {
        use crate::error::WeztermError;
        assert!(!is_retryable(&Error::Wezterm(WeztermError::PaneNotFound(
            42
        ))));
    }

    #[test]
    fn not_retryable_wezterm_parse_error() {
        use crate::error::WeztermError;
        assert!(!is_retryable(&Error::Wezterm(WeztermError::ParseError(
            "bad json".into()
        ))));
    }

    #[test]
    fn is_retryable_storage_database() {
        use crate::error::StorageError;
        assert!(is_retryable(&Error::Storage(StorageError::Database(
            "SQLITE_BUSY".into()
        ))));
    }

    #[test]
    fn not_retryable_storage_sequence_discontinuity() {
        use crate::error::StorageError;
        assert!(!is_retryable(&Error::Storage(
            StorageError::SequenceDiscontinuity {
                expected: 5,
                actual: 7,
            }
        )));
    }

    #[test]
    fn not_retryable_storage_reservation_conflict() {
        use crate::error::StorageError;
        assert!(!is_retryable(&Error::Storage(
            StorageError::ReservationConflict {
                pane_id: 5,
                existing_id: 12,
            }
        )));
    }

    #[test]
    fn not_retryable_storage_migration_failed() {
        use crate::error::StorageError;
        assert!(!is_retryable(&Error::Storage(
            StorageError::MigrationFailed("v3 to v4".into())
        )));
    }

    #[test]
    fn not_retryable_storage_schema_too_new() {
        use crate::error::StorageError;
        assert!(!is_retryable(&Error::Storage(StorageError::SchemaTooNew {
            current: 5,
            supported: 3,
        })));
    }

    #[test]
    fn not_retryable_storage_wa_too_old() {
        use crate::error::StorageError;
        assert!(!is_retryable(&Error::Storage(StorageError::WaTooOld {
            current: "1.0".into(),
            min_compatible: "2.0".into(),
        })));
    }

    #[test]
    fn not_retryable_storage_fts_query_error() {
        use crate::error::StorageError;
        assert!(!is_retryable(&Error::Storage(StorageError::FtsQueryError(
            "bad syntax".into()
        ))));
    }

    #[test]
    fn not_retryable_storage_corruption() {
        use crate::error::StorageError;
        assert!(!is_retryable(&Error::Storage(StorageError::Corruption {
            details: "checksum mismatch".into(),
        })));
    }

    #[test]
    fn not_retryable_storage_not_found() {
        use crate::error::StorageError;
        assert!(!is_retryable(&Error::Storage(StorageError::NotFound(
            "session-123".into()
        ))));
    }

    #[test]
    fn not_retryable_pattern_error() {
        use crate::error::PatternError;
        assert!(!is_retryable(&Error::Pattern(PatternError::InvalidRule(
            "bad rule".into()
        ))));
    }

    #[test]
    fn not_retryable_workflow_error() {
        use crate::error::WorkflowError;
        assert!(!is_retryable(&Error::Workflow(WorkflowError::Aborted(
            "user cancel".into()
        ))));
    }

    #[test]
    fn not_retryable_config_error() {
        use crate::error::ConfigError;
        assert!(!is_retryable(&Error::Config(ConfigError::FileNotFound(
            "ft.toml".into()
        ))));
    }

    #[test]
    fn not_retryable_policy_error() {
        assert!(!is_retryable(&Error::Policy("rate limit exceeded".into())));
    }

    #[test]
    fn not_retryable_json_error() {
        let bad_json = serde_json::from_str::<serde_json::Value>("not json").unwrap_err();
        assert!(!is_retryable(&Error::Json(bad_json)));
    }

    #[test]
    fn is_retryable_runtime_error() {
        assert!(is_retryable(&Error::Runtime("channel closed".into())));
    }

    #[test]
    fn not_retryable_setup_error() {
        assert!(!is_retryable(&Error::SetupError("missing config".into())));
    }

    #[test]
    fn not_retryable_cancelled_error() {
        assert!(!is_retryable(&Error::Cancelled("timeout".into())));
    }

    #[test]
    fn not_retryable_panicked_error() {
        assert!(!is_retryable(&Error::Panicked("thread panic".into())));
    }

    // ── with_smart_retry ───────────────────────────────────────────

    #[test]
    fn smart_retry_stops_on_non_retryable_error() {
        run_async_test(async {
            let policy = RetryPolicy {
                initial_delay: Duration::from_millis(1),
                max_delay: Duration::from_millis(10),
                backoff_factor: 2.0,
                jitter_percent: 0.0,
                max_attempts: Some(5),
            };
            let call_count = Arc::new(AtomicU32::new(0));
            let call_count_clone = Arc::clone(&call_count);

            let result: Result<i32> = with_smart_retry(&policy, || {
                let count = Arc::clone(&call_count_clone);
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    Err(Error::Policy("forbidden".into()))
                }
            })
            .await;

            assert!(result.is_err());
            assert_eq!(call_count.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn smart_retry_retries_retryable_errors() {
        run_async_test(async {
            let policy = RetryPolicy {
                initial_delay: Duration::from_millis(1),
                max_delay: Duration::from_millis(10),
                backoff_factor: 2.0,
                jitter_percent: 0.0,
                max_attempts: Some(5),
            };
            let call_count = Arc::new(AtomicU32::new(0));
            let call_count_clone = Arc::clone(&call_count);

            let result = with_smart_retry(&policy, || {
                let count = Arc::clone(&call_count_clone);
                async move {
                    let n = count.fetch_add(1, Ordering::SeqCst);
                    if n < 2 {
                        Err(Error::Runtime("transient".into()))
                    } else {
                        Ok::<_, Error>(99)
                    }
                }
            })
            .await;

            assert!(result.is_ok());
            assert_eq!(result.unwrap(), 99);
            assert_eq!(call_count.load(Ordering::SeqCst), 3);
        });
    }

    #[test]
    fn smart_retry_exhausts_attempts_on_retryable_errors() {
        run_async_test(async {
            let policy = RetryPolicy {
                initial_delay: Duration::from_millis(1),
                max_delay: Duration::from_millis(10),
                backoff_factor: 2.0,
                jitter_percent: 0.0,
                max_attempts: Some(3),
            };
            let call_count = Arc::new(AtomicU32::new(0));
            let call_count_clone = Arc::clone(&call_count);

            let result: Result<i32> = with_smart_retry(&policy, || {
                let count = Arc::clone(&call_count_clone);
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    Err(Error::Runtime("always fails".into()))
                }
            })
            .await;

            assert!(result.is_err());
            assert_eq!(call_count.load(Ordering::SeqCst), 3);
        });
    }

    #[test]
    fn smart_retry_succeeds_immediately() {
        run_async_test(async {
            let policy = RetryPolicy::default();
            let result = with_smart_retry(&policy, || async { Ok::<_, Error>(42) }).await;
            assert_eq!(result.unwrap(), 42);
        });
    }

    // ── with_retry_outcome edge cases ──────────────────────────────

    #[test]
    fn retry_outcome_on_exhaustion_tracks_all_fields() {
        run_async_test(async {
            let policy = RetryPolicy {
                initial_delay: Duration::from_millis(1),
                max_delay: Duration::from_millis(5),
                backoff_factor: 1.0,
                jitter_percent: 0.0,
                max_attempts: Some(2),
            };

            let outcome: RetryOutcome<i32> = with_retry_outcome(&policy, || async {
                Err::<i32, Error>(Error::Runtime("fail".into()))
            })
            .await;

            assert!(outcome.result.is_err());
            assert_eq!(outcome.attempts, 2);
            assert!(outcome.elapsed >= Duration::from_millis(1));
        });
    }

    #[test]
    fn retry_outcome_immediate_success_has_one_attempt() {
        run_async_test(async {
            let policy = RetryPolicy::default();

            let outcome = with_retry_outcome(&policy, || async { Ok::<_, Error>("hello") }).await;

            assert!(outcome.result.is_ok());
            assert_eq!(outcome.attempts, 1);
        });
    }

    // ── with_retry_and_circuit success path ────────────────────────

    #[test]
    fn circuit_records_success_on_retry_success() {
        run_async_test(async {
            use crate::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};

            let policy = RetryPolicy {
                initial_delay: Duration::from_millis(1),
                max_delay: Duration::from_millis(10),
                backoff_factor: 2.0,
                jitter_percent: 0.0,
                max_attempts: Some(3),
            };

            let mut circuit =
                CircuitBreaker::new(CircuitBreakerConfig::new(3, 1, Duration::from_secs(60)));

            let result =
                with_retry_and_circuit(&policy, &mut circuit, || async { Ok::<_, Error>(42) })
                    .await;

            assert_eq!(result.unwrap(), 42);
            assert!(circuit.allow());
            let status = circuit.status();
            assert_eq!(format!("{:?}", status.state), "Closed");
        });
    }

    // ========================================================================
    // Cx-first LabRuntime retry tests (ft-xbnl0.2.5 coverage)
    // ========================================================================

    /// Helper: run a closure inside a LabRuntime task.
    fn run_lab<F>(seed: u64, f: impl FnOnce() -> F + Send + 'static)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let mut runtime = asupersync::LabRuntime::new(
            asupersync::LabConfig::new(seed)
                .with_auto_advance()
                .worker_count(1)
                .max_steps(100_000),
        );
        let region = runtime
            .state
            .create_root_region(asupersync::Budget::INFINITE);
        let (task_id, _handle) = runtime
            .state
            .create_task(region, asupersync::Budget::INFINITE, async move {
                f().await;
            })
            .expect("spawn lab task");
        runtime.scheduler.lock().schedule(task_id, 0);
        runtime.step_for_test();
        let _ = runtime.run_with_auto_advance();
        let report = runtime.run_until_quiescent_with_report();
        assert!(
            report.oracle_report.all_passed(),
            "LabRuntime oracles must all pass: {report:?}"
        );
    }

    /// Pre-cancelled cx returns Cancelled immediately from with_retry_cx
    /// without invoking the operation closure at all.
    #[test]
    fn with_retry_cx_pre_cancelled_returns_cancelled() {
        let invoked = Arc::new(AtomicU32::new(0));
        let invoked_task = Arc::clone(&invoked);

        run_lab(0xCA0C_E100, move || async move {
            let cx = crate::cx::for_testing();
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("pre-cancel retry test"),
            );
            let policy = RetryPolicy {
                initial_delay: Duration::from_millis(10),
                max_delay: Duration::from_millis(50),
                backoff_factor: 2.0,
                jitter_percent: 0.0,
                max_attempts: Some(5),
            };
            let result = with_retry_cx(&cx, &policy, || {
                invoked_task.fetch_add(1, Ordering::SeqCst);
                async { Ok::<_, Error>(42) }
            })
            .await;
            assert!(result.is_err(), "pre-cancelled cx must return error");
            let err_msg = result.unwrap_err().to_string();
            assert!(
                err_msg.contains("cancelled"),
                "error must mention cancellation: {err_msg}"
            );
        });
        assert_eq!(
            invoked.load(Ordering::SeqCst),
            0,
            "operation must not be invoked when cx is pre-cancelled"
        );
    }

    /// with_retry_outcome_cx cancel after first attempt: the operation fails
    /// once, then cx is cancelled before the next attempt boundary check,
    /// so the retry loop returns Cancelled instead of continuing.
    #[test]
    fn with_retry_outcome_cx_cancel_after_first_attempt_returns_cancelled() {
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_task = Arc::clone(&attempts);
        let got_cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let got_cancelled_task = Arc::clone(&got_cancelled);

        run_lab(0xCA0C_E200, move || async move {
            let cx = crate::cx::for_request();
            let policy = RetryPolicy {
                initial_delay: Duration::from_millis(10),
                max_delay: Duration::from_millis(50),
                backoff_factor: 2.0,
                jitter_percent: 0.0,
                max_attempts: Some(10),
            };

            let cancel_cx = cx.clone();
            let outcome = with_retry_outcome_cx(&cx, &policy, || {
                let n = attempts_task.fetch_add(1, Ordering::SeqCst);
                let ccx = cancel_cx.clone();
                async move {
                    if n == 0 {
                        // First attempt: fail, then cancel cx so next
                        // attempt-boundary check sees cancellation.
                        ccx.cancel_with(
                            crate::outcome::CancelKind::User,
                            Some("cancel after first attempt"),
                        );
                    }
                    Err::<u32, _>(Error::Runtime(format!("attempt {n} fail")))
                }
            })
            .await;

            if let Err(Error::Cancelled(_)) = &outcome.result {
                got_cancelled_task.store(true, Ordering::SeqCst);
            }
        });
        assert!(
            got_cancelled.load(Ordering::SeqCst),
            "retry must surface Cancelled when cx cancelled after first attempt"
        );
        // Exactly 1 attempt: the operation fires once, cancels cx, then
        // either the backoff sleep or the next attempt-boundary check
        // returns Cancelled.
        let a = attempts.load(Ordering::SeqCst);
        assert!(
            a == 1,
            "expected exactly 1 attempt before cancel surfaces, got {a}"
        );
    }

    /// with_retry_and_circuit_cx returns CircuitOpen when circuit is open,
    /// without invoking the operation.
    #[test]
    fn with_retry_and_circuit_cx_open_circuit_returns_circuit_open() {
        let invoked = Arc::new(AtomicU32::new(0));
        let invoked_task = Arc::clone(&invoked);

        run_lab(0xC1AC_0001, move || async move {
            use crate::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};

            let cx = crate::cx::for_request();
            let policy = RetryPolicy {
                initial_delay: Duration::from_millis(10),
                max_delay: Duration::from_millis(50),
                backoff_factor: 2.0,
                jitter_percent: 0.0,
                max_attempts: Some(3),
            };
            // Trip the circuit by recording failures
            let mut circuit =
                CircuitBreaker::new(CircuitBreakerConfig::new(2, 1, Duration::from_secs(300)));
            circuit.record_failure();
            circuit.record_failure();
            assert!(!circuit.allow(), "circuit must be open after 2 failures");

            let result = with_retry_and_circuit_cx(&cx, &policy, &mut circuit, || {
                invoked_task.fetch_add(1, Ordering::SeqCst);
                async { Ok::<_, Error>(99) }
            })
            .await;

            assert!(result.is_err(), "open circuit must return error");
            let err_msg = format!("{:?}", result.unwrap_err());
            assert!(
                err_msg.contains("CircuitOpen"),
                "error must be CircuitOpen: {err_msg}"
            );
        });
        assert_eq!(
            invoked.load(Ordering::SeqCst),
            0,
            "operation must not be invoked when circuit is open"
        );
    }

    /// with_retry_and_circuit_cx pre-cancelled cx returns Cancelled and
    /// records a failure on the circuit breaker.
    #[test]
    fn with_retry_and_circuit_cx_pre_cancelled_returns_cancelled() {
        run_lab(0xC1AC_0002, || async move {
            use crate::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};

            let cx = crate::cx::for_testing();
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("pre-cancel circuit retry"),
            );
            let policy = RetryPolicy {
                initial_delay: Duration::from_millis(10),
                max_delay: Duration::from_millis(50),
                backoff_factor: 2.0,
                jitter_percent: 0.0,
                max_attempts: Some(5),
            };
            let mut circuit =
                CircuitBreaker::new(CircuitBreakerConfig::new(5, 1, Duration::from_secs(60)));

            let result = with_retry_and_circuit_cx(&cx, &policy, &mut circuit, || async {
                Ok::<_, Error>(42)
            })
            .await;

            assert!(result.is_err(), "pre-cancelled cx must return error");
            let err_msg = result.unwrap_err().to_string();
            assert!(
                err_msg.contains("cancelled"),
                "error must mention cancellation: {err_msg}"
            );
        });
    }

    /// with_smart_retry_cx pre-cancelled cx returns Cancelled immediately.
    #[test]
    fn with_smart_retry_cx_pre_cancelled_returns_cancelled() {
        let invoked = Arc::new(AtomicU32::new(0));
        let invoked_task = Arc::clone(&invoked);

        run_lab(0x50A0_7001, move || async move {
            let cx = crate::cx::for_testing();
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("pre-cancel smart retry"),
            );
            let policy = RetryPolicy {
                initial_delay: Duration::from_millis(10),
                max_delay: Duration::from_millis(50),
                backoff_factor: 2.0,
                jitter_percent: 0.0,
                max_attempts: Some(5),
            };
            let result = with_smart_retry_cx(&cx, &policy, || {
                invoked_task.fetch_add(1, Ordering::SeqCst);
                async { Ok::<_, Error>(42) }
            })
            .await;

            assert!(result.is_err(), "pre-cancelled cx must return error");
            let err_msg = result.unwrap_err().to_string();
            assert!(
                err_msg.contains("cancelled"),
                "error must mention cancellation: {err_msg}"
            );
        });
        assert_eq!(
            invoked.load(Ordering::SeqCst),
            0,
            "smart retry must not invoke operation when cx is pre-cancelled"
        );
    }

    /// with_retry_outcome_cx succeeds on first attempt under LabRuntime
    /// and reports attempts=1 with Ok result.
    #[test]
    fn with_retry_outcome_cx_first_try_success() {
        run_lab(0xF105_7000, || async move {
            let cx = crate::cx::for_request();
            let policy = RetryPolicy {
                initial_delay: Duration::from_millis(10),
                max_delay: Duration::from_millis(50),
                backoff_factor: 2.0,
                jitter_percent: 0.0,
                max_attempts: Some(3),
            };
            let outcome =
                with_retry_outcome_cx(&cx, &policy, || async { Ok::<_, Error>(777) }).await;
            assert!(outcome.result.is_ok());
            assert_eq!(outcome.result.unwrap(), 777);
            assert_eq!(
                outcome.attempts, 1,
                "first-try success should report 1 attempt"
            );
        });
    }

    /// with_retry_outcome_cx exhausts max_attempts and returns last error.
    #[test]
    fn with_retry_outcome_cx_exhausts_retries() {
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_task = Arc::clone(&attempts);

        run_lab(0xE00A_0001, move || async move {
            let cx = crate::cx::for_request();
            let policy = RetryPolicy {
                initial_delay: Duration::from_millis(5),
                max_delay: Duration::from_millis(20),
                backoff_factor: 2.0,
                jitter_percent: 0.0,
                max_attempts: Some(3),
            };
            let outcome = with_retry_outcome_cx(&cx, &policy, || {
                attempts_task.fetch_add(1, Ordering::SeqCst);
                async { Err::<u32, _>(Error::Runtime("always fail".into())) }
            })
            .await;
            assert!(outcome.result.is_err());
            assert_eq!(outcome.attempts, 3, "must exhaust all 3 attempts");
        });
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            3,
            "operation must be invoked exactly 3 times"
        );
    }
}
