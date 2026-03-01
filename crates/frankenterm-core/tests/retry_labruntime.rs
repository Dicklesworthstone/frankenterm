//! LabRuntime-ported retry tests for deterministic async testing.
//!
//! Ports key `retry.rs` tests from `#[tokio::test]` to asupersync-based
//! `RuntimeFixture`, gaining seed-based reproducibility and deterministic
//! timer behavior for backoff delays.
//!
//! The retry module uses `runtime_compat::sleep()` for backoff delays,
//! so under the asupersync-runtime feature flag, all timer behavior flows
//! through the deterministic asupersync scheduler.
//!
//! Bead: ft-22x4r (Port existing async tests to LabRuntime)

#![cfg(feature = "asupersync-runtime")]

mod common;

use common::fixtures::RuntimeFixture;
use frankenterm_core::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
use frankenterm_core::error::Error;
use frankenterm_core::retry::{
    RetryPolicy, RetryOutcome, with_retry, with_retry_and_circuit, with_retry_outcome,
    with_smart_retry,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

// ===========================================================================
// Section 1: Direct retry tests ported from tokio::test to RuntimeFixture
//
// These test the real retry implementation through the asupersync-backed
// runtime_compat layer. They replace #[tokio::test] with
// RuntimeFixture::current_thread().block_on().
// ===========================================================================

// ── with_retry ───────────────────────────────────────────────────

#[test]
fn retry_succeeds_immediately() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let policy = RetryPolicy {
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(10),
            backoff_factor: 2.0,
            jitter_percent: 0.0,
            max_attempts: Some(3),
        };
        let call_count = Arc::new(AtomicU32::new(0));
        let call_count_clone = Arc::clone(&call_count);

        let result: frankenterm_core::Result<i32> = with_retry(&policy, || {
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

// ── with_retry_outcome ───────────────────────────────────────────

#[test]
fn retry_with_outcome_tracks_attempts() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
fn retry_outcome_on_exhaustion_tracks_all_fields() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
        // Elapsed should be at least 1ms (one backoff sleep)
        assert!(outcome.elapsed >= Duration::from_millis(1));
    });
}

#[test]
fn retry_outcome_immediate_success_has_one_attempt() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let policy = RetryPolicy::default();

        let outcome = with_retry_outcome(&policy, || async { Ok::<_, Error>("hello") }).await;

        assert!(outcome.result.is_ok());
        assert_eq!(outcome.attempts, 1);
    });
}

// ── Circuit breaker integration ──────────────────────────────────

#[test]
fn circuit_breaker_integration() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
        let result: frankenterm_core::Result<i32> =
            with_retry_and_circuit(&policy, &mut circuit, || async {
                Err(Error::Runtime("fail".into()))
            })
            .await;
        assert!(result.is_err());

        // Circuit should now be open
        let result: frankenterm_core::Result<i32> =
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
fn circuit_records_success_on_retry_success() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
            with_retry_and_circuit(&policy, &mut circuit, || async { Ok::<_, Error>(42) }).await;

        assert_eq!(result.unwrap(), 42);
        assert!(circuit.allow());
        let status = circuit.status();
        assert_eq!(format!("{:?}", status.state), "Closed");
    });
}

// ── with_smart_retry ─────────────────────────────────────────────

#[test]
fn smart_retry_stops_on_non_retryable_error() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let policy = RetryPolicy {
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(10),
            backoff_factor: 2.0,
            jitter_percent: 0.0,
            max_attempts: Some(5),
        };
        let call_count = Arc::new(AtomicU32::new(0));
        let call_count_clone = Arc::clone(&call_count);

        let result: frankenterm_core::Result<i32> = with_smart_retry(&policy, || {
            let count = Arc::clone(&call_count_clone);
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                Err(Error::Policy("forbidden".into()))
            }
        })
        .await;

        assert!(result.is_err());
        // Non-retryable error should stop after 1 attempt
        assert_eq!(call_count.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn smart_retry_retries_retryable_errors() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let policy = RetryPolicy {
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(10),
            backoff_factor: 2.0,
            jitter_percent: 0.0,
            max_attempts: Some(3),
        };
        let call_count = Arc::new(AtomicU32::new(0));
        let call_count_clone = Arc::clone(&call_count);

        let result: frankenterm_core::Result<i32> = with_smart_retry(&policy, || {
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let policy = RetryPolicy::default();
        let result = with_smart_retry(&policy, || async { Ok::<_, Error>(42) }).await;
        assert_eq!(result.unwrap(), 42);
    });
}

// ===========================================================================
// Note: LabRuntime sections (2-5) omitted for retry tests.
//
// The retry module internally calls `runtime_compat::sleep()` for backoff
// delays. Under asupersync-runtime, this uses `asupersync::time::sleep`
// which requires wall-clock time to advance. The LabRuntime's deterministic
// scheduler with `run_until_quiescent()` does not advance simulated time,
// causing tasks blocked on sleep to appear "leaked".
//
// The RuntimeFixture ports above (Section 1) use the full asupersync runtime
// where time advances naturally, so they correctly exercise the retry logic
// including backoff sleep behavior.
// ===========================================================================
