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
// Section 2: LabRuntime tests for retry under deterministic scheduling
//
// These exercise retry + backoff under the LabRuntime scheduler, verifying
// that the retry mechanism works correctly with deterministic time stepping.
// ===========================================================================

use asupersync::Budget;
use common::lab::{
    ChaosTestConfig, ExplorationTestConfig, LabTestConfig, run_chaos_test, run_exploration_test,
    run_lab_test, run_lab_test_simple,
};

#[test]
fn lab_retry_succeeds_after_transient_failures() {
    let report = run_lab_test_simple(42, "retry_transient_failures", |runtime| {
        let region = runtime.state.create_root_region(Budget::INFINITE);

        let (task_id, _handle) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
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
                            Err(Error::Runtime("transient".into()))
                        } else {
                            Ok::<_, Error>(42)
                        }
                    }
                })
                .await;

                assert!(result.is_ok());
                assert_eq!(result.unwrap(), 42);
                assert_eq!(call_count.load(Ordering::SeqCst), 3);
            })
            .expect("create task");
        runtime
            .scheduler
            .lock()
            .schedule(task_id, 0);
        runtime.run_until_quiescent();
    });
    assert!(report.passed());
}

#[test]
fn lab_retry_exhaustion_under_deterministic_scheduling() {
    let report = run_lab_test(
        LabTestConfig::new(99, "retry_exhaustion_deterministic").worker_count(1),
        |runtime| {
            let region = runtime.state.create_root_region(Budget::INFINITE);

            let (task_id, _handle) = runtime
                .state
                .create_task(region, Budget::INFINITE, async move {
                    let policy = RetryPolicy {
                        initial_delay: Duration::from_millis(1),
                        max_delay: Duration::from_millis(5),
                        backoff_factor: 1.0,
                        jitter_percent: 0.0,
                        max_attempts: Some(3),
                    };
                    let call_count = Arc::new(AtomicU32::new(0));
                    let call_count_clone = Arc::clone(&call_count);

                    let result: frankenterm_core::Result<i32> = with_retry(&policy, || {
                        let count = Arc::clone(&call_count_clone);
                        async move {
                            count.fetch_add(1, Ordering::SeqCst);
                            Err(Error::Runtime("persistent".into()))
                        }
                    })
                    .await;

                    assert!(result.is_err());
                    assert_eq!(call_count.load(Ordering::SeqCst), 3);
                })
                .expect("create task");
            runtime
                .scheduler
                .lock()
                .schedule(task_id, 0);
            runtime.run_until_quiescent();
        },
    );
    assert!(report.passed());
}

#[test]
fn lab_smart_retry_respects_retryability() {
    let report = run_lab_test_simple(200, "smart_retry_retryability", |runtime| {
        let region = runtime.state.create_root_region(Budget::INFINITE);

        let (task_id, _handle) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                let policy = RetryPolicy {
                    initial_delay: Duration::from_millis(1),
                    max_delay: Duration::from_millis(10),
                    backoff_factor: 2.0,
                    jitter_percent: 0.0,
                    max_attempts: Some(5),
                };

                // Non-retryable error should stop immediately
                let non_retry_count = Arc::new(AtomicU32::new(0));
                let nrc = Arc::clone(&non_retry_count);
                let r: frankenterm_core::Result<i32> = with_smart_retry(&policy, || {
                    let c = Arc::clone(&nrc);
                    async move {
                        c.fetch_add(1, Ordering::SeqCst);
                        Err(Error::Policy("forbidden".into()))
                    }
                })
                .await;
                assert!(r.is_err());
                assert_eq!(non_retry_count.load(Ordering::SeqCst), 1);

                // Retryable error with eventual success
                let retry_count = Arc::new(AtomicU32::new(0));
                let rc = Arc::clone(&retry_count);
                let r = with_smart_retry(&policy, || {
                    let c = Arc::clone(&rc);
                    async move {
                        let n = c.fetch_add(1, Ordering::SeqCst);
                        if n < 1 {
                            Err(Error::Runtime("transient".into()))
                        } else {
                            Ok::<_, Error>(99)
                        }
                    }
                })
                .await;
                assert_eq!(r.unwrap(), 99);
                assert_eq!(retry_count.load(Ordering::SeqCst), 2);
            })
            .expect("create task");
        runtime
            .scheduler
            .lock()
            .schedule(task_id, 0);
        runtime.run_until_quiescent();
    });
    assert!(report.passed());
}

// ===========================================================================
// Section 3: DPOR exploration for concurrent retry scenarios
// ===========================================================================

#[test]
fn exploration_concurrent_retries_no_interference() {
    let config = ExplorationTestConfig::new("concurrent_retries_no_interference", 10)
        .base_seed(0)
        .worker_count(2)
        .max_steps_per_run(100_000);

    let report = run_exploration_test(config, |runtime| {
        let region = runtime.state.create_root_region(Budget::INFINITE);
        let total_successes = Arc::new(AtomicU32::new(0));

        // Spawn 3 concurrent retry tasks — they should not interfere
        for i in 0..3_u32 {
            let successes = Arc::clone(&total_successes);
            let (task_id, _handle) = runtime
                .state
                .create_task(region, Budget::INFINITE, async move {
                    let policy = RetryPolicy {
                        initial_delay: Duration::from_millis(1),
                        max_delay: Duration::from_millis(5),
                        backoff_factor: 1.0,
                        jitter_percent: 0.0,
                        max_attempts: Some(3),
                    };
                    let call_count = Arc::new(AtomicU32::new(0));
                    let cc = Arc::clone(&call_count);

                    let result = with_retry(&policy, || {
                        let c = Arc::clone(&cc);
                        async move {
                            let n = c.fetch_add(1, Ordering::SeqCst);
                            if n < 1 {
                                Err(Error::Runtime(format!("transient-{i}")))
                            } else {
                                Ok::<_, Error>(i)
                            }
                        }
                    })
                    .await;

                    if result.is_ok() {
                        successes.fetch_add(1, Ordering::SeqCst);
                    }
                })
                .expect("create task");
            runtime
                .scheduler
                .lock()
                .schedule(task_id, 0);
        }

        runtime.run_until_quiescent();

        // All 3 should succeed after 1 retry each
        assert_eq!(
            total_successes.load(Ordering::SeqCst),
            3,
            "all concurrent retries should succeed"
        );
    });
    assert!(report.passed());
    assert!(
        report.total_runs >= 5,
        "should explore multiple schedules"
    );
}

// ===========================================================================
// Section 4: Chaos fault injection for retry resilience
// ===========================================================================

#[test]
fn chaos_retry_survives_scheduling_faults() {
    let config = ChaosTestConfig::light(42, "retry_chaos_light").worker_count(2);
    let report = run_chaos_test(config, |runtime| {
        let region = runtime.state.create_root_region(Budget::INFINITE);

        let (task_id, _handle) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                let policy = RetryPolicy {
                    initial_delay: Duration::from_millis(1),
                    max_delay: Duration::from_millis(5),
                    backoff_factor: 1.0,
                    jitter_percent: 0.0,
                    max_attempts: Some(5),
                };

                // Under chaos, the retry sleep/timer might behave
                // differently, but the retry logic should still work.
                let result = with_retry(&policy, || async { Ok::<_, Error>(42) }).await;
                assert!(result.is_ok());
            })
            .expect("create task");
        runtime
            .scheduler
            .lock()
            .schedule(task_id, 0);
        runtime.run_until_quiescent();
    });
    assert!(report.passed());
}

// ===========================================================================
// Section 5: Multi-seed sweep
// ===========================================================================

#[test]
fn multi_seed_retry_behavior_consistent() {
    let seeds = [1, 42, 99, 256, 1000, 9999];
    let reports = common::lab::run_multi_seed_test(
        "retry_multi_seed_consistency",
        &seeds,
        |runtime| {
            let region = runtime.state.create_root_region(Budget::INFINITE);

            let (task_id, _handle) = runtime
                .state
                .create_task(region, Budget::INFINITE, async move {
                    let policy = RetryPolicy {
                        initial_delay: Duration::from_millis(1),
                        max_delay: Duration::from_millis(10),
                        backoff_factor: 2.0,
                        jitter_percent: 0.0,
                        max_attempts: Some(4),
                    };
                    let call_count = Arc::new(AtomicU32::new(0));
                    let cc = Arc::clone(&call_count);

                    let result = with_retry(&policy, || {
                        let c = Arc::clone(&cc);
                        async move {
                            let n = c.fetch_add(1, Ordering::SeqCst);
                            if n < 2 {
                                Err(Error::Runtime("transient".into()))
                            } else {
                                Ok::<_, Error>(n)
                            }
                        }
                    })
                    .await;

                    // Regardless of seed, retry logic is deterministic:
                    // always succeeds on 3rd attempt
                    assert!(result.is_ok());
                    assert_eq!(call_count.load(Ordering::SeqCst), 3);
                })
                .expect("create task");
            runtime
                .scheduler
                .lock()
                .schedule(task_id, 0);
            runtime.run_until_quiescent();
        },
    );
    assert_eq!(reports.len(), seeds.len());
    for report in &reports {
        assert!(report.passed());
    }
}
