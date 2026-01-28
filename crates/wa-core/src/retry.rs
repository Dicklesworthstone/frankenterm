//! Retry with exponential backoff.
//!
//! Provides a standardized retry policy for all fallible I/O operations in wa.
//! This module works in conjunction with the circuit breaker to provide robust
//! error handling and prevent retry storms.
//!
//! # Usage
//!
//! ```rust,ignore
//! use wa_core::retry::{RetryPolicy, with_retry};
//!
//! let policy = RetryPolicy::default();
//!
//! let result = with_retry(&policy, || async {
//!     // Fallible operation
//!     some_io_operation().await
//! }).await;
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

        let delay_ms = (base_ms + jitter).max(0.0);
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

/// Execute an async operation with retry, returning detailed outcome.
pub async fn with_retry_outcome<T, F, Fut>(
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
        match operation().await {
            Ok(value) => {
                if attempt > 0 {
                    debug!(
                        attempt = attempt + 1,
                        "Operation succeeded after {} retries", attempt
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

                // Check if we've exhausted retries
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

                // Calculate delay and wait
                let delay = policy.delay_for_attempt(attempt - 1);
                debug!(
                    attempt,
                    delay_ms = delay.as_millis() as u64,
                    error = %e,
                    "Retrying operation after failure"
                );

                tokio::time::sleep(delay).await;
            }
        }
    }
}

/// Execute an operation with retry and circuit breaker integration.
///
/// If the circuit is open, returns immediately with a circuit open error.
/// Exceeded retries count as a circuit failure.
pub async fn with_retry_and_circuit<T, F, Fut>(
    policy: &RetryPolicy,
    circuit: &mut CircuitBreaker,
    operation: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    use crate::error::WeztermError;

    // Check circuit state first
    if !circuit.allow() {
        let status = circuit.status();
        let retry_after_ms = status.cooldown_remaining_ms.unwrap_or(0);
        return Err(Error::Wezterm(WeztermError::CircuitOpen { retry_after_ms }));
    }

    let outcome = with_retry_outcome(policy, operation).await;

    // Update circuit state based on outcome
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
    use crate::error::WeztermError;

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
        // Storage errors might be transient (lock conflicts)
        Error::Storage(_) => true,
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
        // Setup errors are not retryable
        Error::SetupError(_) => false,
    }
}

/// Execute an operation with smart retry (only retries if error is retryable).
pub async fn with_smart_retry<T, F, Fut>(policy: &RetryPolicy, mut operation: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let start = std::time::Instant::now();
    let mut attempt = 0u32;

    loop {
        match operation().await {
            Ok(value) => {
                if attempt > 0 {
                    debug!(
                        attempt = attempt + 1,
                        "Operation succeeded after {} retries", attempt
                    );
                }
                return Ok(value);
            }
            Err(e) => {
                attempt += 1;

                // Check if error is retryable
                if !is_retryable(&e) {
                    debug!(
                        attempt,
                        error = %e,
                        "Non-retryable error, giving up"
                    );
                    return Err(e);
                }

                // Check if we've exhausted retries
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

                // Calculate delay and wait
                let delay = policy.delay_for_attempt(attempt - 1);
                debug!(
                    attempt,
                    delay_ms = delay.as_millis() as u64,
                    error = %e,
                    "Retrying operation after retryable failure"
                );

                tokio::time::sleep(delay).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

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

    #[tokio::test]
    async fn retry_succeeds_immediately() {
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
    }

    #[tokio::test]
    async fn retry_succeeds_after_failures() {
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
    }

    #[tokio::test]
    async fn retry_exhausts_attempts() {
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
    }

    #[tokio::test]
    async fn retry_with_outcome_tracks_attempts() {
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
    }

    #[tokio::test]
    async fn circuit_breaker_integration() {
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
}
