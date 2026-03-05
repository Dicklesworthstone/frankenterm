//! LabRuntime port of all `#[tokio::test]` async tests from `protocol_recovery.rs`.
//!
//! Each test that previously used `#[tokio::test]` is wrapped in
//! `RuntimeFixture::current_thread()` + `rt.block_on(async { … })`.
//! Feature-gated behind `asupersync-runtime`.

#![cfg(feature = "asupersync-runtime")]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use frankenterm_core::circuit_breaker::CircuitStateKind;
use frankenterm_core::protocol_recovery::{
    ProtocolErrorKind, RecoveryConfig, RecoveryEngine, RecoveryError, classify_error_message,
};

use common::fixtures::RuntimeFixture;

// ===========================================================================
// 1. Engine succeeds on first try
// ===========================================================================

#[test]
fn engine_succeeds_first_try() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mut e = RecoveryEngine::new(RecoveryConfig::default());
        let o = e
            .execute(
                |_| async { Ok::<_, String>(42) },
                |_: &String| ProtocolErrorKind::Transient,
            )
            .await;
        assert_eq!(o.result.unwrap(), 42);
        assert_eq!(o.attempts, 1);
        assert_eq!(e.stats().first_try_successes, 1);
    });
}

// ===========================================================================
// 2. Engine retries transient errors
// ===========================================================================

#[test]
fn engine_retries_transient() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let cc = Arc::new(AtomicU32::new(0));
        let cc2 = cc.clone();
        let config = RecoveryConfig {
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
            ..RecoveryConfig::default()
        };
        let mut e = RecoveryEngine::new(config);
        let o = e
            .execute(
                move |_| {
                    let cc = cc2.clone();
                    async move {
                        let n = cc.fetch_add(1, Ordering::Relaxed);
                        if n < 2 {
                            Err("read from mux socket timed out".into())
                        } else {
                            Ok(99)
                        }
                    }
                },
                |err: &String| classify_error_message(err),
            )
            .await;
        assert_eq!(o.result.unwrap(), 99);
        assert_eq!(o.attempts, 3);
        assert_eq!(e.stats().transient_failures, 2);
    });
}

// ===========================================================================
// 3. Engine stops on permanent error
// ===========================================================================

#[test]
fn engine_stops_on_permanent() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let config = RecoveryConfig {
            initial_delay: Duration::from_millis(1),
            ..RecoveryConfig::default()
        };
        let mut e = RecoveryEngine::new(config);
        let o = e
            .execute(
                |_| async {
                    Err::<i32, _>("codec version mismatch: local 4 != remote 3".to_string())
                },
                |err: &String| classify_error_message(err),
            )
            .await;
        assert!(matches!(o.result.unwrap_err(), RecoveryError::Permanent(_)));
        assert_eq!(o.attempts, 1);
    });
}

// ===========================================================================
// 4. Engine exhausts retries
// ===========================================================================

#[test]
fn engine_exhausts_retries() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let config = RecoveryConfig {
            max_retries: 2,
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
            report_degradation: false,
            ..RecoveryConfig::default()
        };
        let mut e = RecoveryEngine::new(config);
        let o = e
            .execute(
                |_| async { Err::<i32, _>("mux socket disconnected".to_string()) },
                |err: &String| classify_error_message(err),
            )
            .await;
        assert!(matches!(
            o.result.unwrap_err(),
            RecoveryError::RetriesExhausted { .. }
        ));
        assert_eq!(o.attempts, 3);
    });
}

// ===========================================================================
// 5. Circuit breaker opens after repeated failures
// ===========================================================================

#[test]
fn engine_circuit_breaker_opens() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let config = RecoveryConfig {
            max_retries: 0,
            circuit_failure_threshold: 2,
            circuit_cooldown: Duration::from_secs(60),
            initial_delay: Duration::from_millis(1),
            report_degradation: false,
            ..RecoveryConfig::default()
        };
        let mut e = RecoveryEngine::new(config);
        for _ in 0..2 {
            let _ = e
                .execute(
                    |_| async { Err::<i32, _>("mux socket disconnected".to_string()) },
                    |err: &String| classify_error_message(err),
                )
                .await;
        }
        let o = e
            .execute(
                |_| async { Ok::<_, String>(42) },
                |_: &String| ProtocolErrorKind::Transient,
            )
            .await;
        assert!(matches!(o.result.unwrap_err(), RecoveryError::CircuitOpen));
    });
}

// ===========================================================================
// 6. Permanent failure limit
// ===========================================================================

#[test]
fn engine_permanent_limit() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let config = RecoveryConfig {
            max_retries: 0,
            permanent_failure_limit: 2,
            initial_delay: Duration::from_millis(1),
            report_degradation: false,
            ..RecoveryConfig::default()
        };
        let mut e = RecoveryEngine::new(config);
        for _ in 0..2 {
            let _ = e
                .execute(
                    |_| async {
                        Err::<i32, _>("codec version mismatch: local 4 != remote 3".to_string())
                    },
                    |err: &String| classify_error_message(err),
                )
                .await;
        }
        let o = e
            .execute(
                |_| async { Ok::<_, String>(42) },
                |_: &String| ProtocolErrorKind::Transient,
            )
            .await;
        assert!(matches!(
            o.result.unwrap_err(),
            RecoveryError::PermanentLimitReached { .. }
        ));
    });
}

// ===========================================================================
// 7. Engine disabled
// ===========================================================================

#[test]
fn engine_disabled() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mut e = RecoveryEngine::new(RecoveryConfig {
            enabled: false,
            ..RecoveryConfig::default()
        });
        let o = e
            .execute(
                |_| async { Ok::<_, String>(42) },
                |_: &String| ProtocolErrorKind::Transient,
            )
            .await;
        assert!(matches!(o.result.unwrap_err(), RecoveryError::Disabled));
    });
}

// ===========================================================================
// 8. Permanent counter resets on success
// ===========================================================================

#[test]
fn engine_permanent_counter_resets() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let config = RecoveryConfig {
            max_retries: 0,
            permanent_failure_limit: 3,
            circuit_failure_threshold: 10,
            initial_delay: Duration::from_millis(1),
            report_degradation: false,
            ..RecoveryConfig::default()
        };
        let mut e = RecoveryEngine::new(config);
        let _ = e
            .execute(
                |_| async {
                    Err::<i32, _>("codec version mismatch: local 4 != remote 3".to_string())
                },
                |err: &String| classify_error_message(err),
            )
            .await;
        assert_eq!(e.stats().consecutive_permanent, 1);
        let _ = e
            .execute(
                |_| async { Ok::<_, String>(42) },
                |_: &String| ProtocolErrorKind::Transient,
            )
            .await;
        assert_eq!(e.stats().consecutive_permanent, 0);
    });
}

// ===========================================================================
// 9. Engine with_name works
// ===========================================================================

#[test]
fn engine_with_name_works() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mut e = RecoveryEngine::with_name("test_engine", RecoveryConfig::default());
        let stats = e.stats();
        assert_eq!(stats.total_operations, 0);
        let o = e
            .execute(
                |_| async { Ok::<_, String>(42) },
                |_: &String| ProtocolErrorKind::Transient,
            )
            .await;
        assert_eq!(o.result.unwrap(), 42);
    });
}

// ===========================================================================
// 10. Stats initial all zeros
// ===========================================================================

#[test]
fn engine_stats_initial_all_zeros() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let e = RecoveryEngine::new(RecoveryConfig::default());
        let s = e.stats();
        assert_eq!(s.total_operations, 0);
        assert_eq!(s.first_try_successes, 0);
        assert_eq!(s.retry_successes, 0);
        assert_eq!(s.total_retries, 0);
        assert_eq!(s.recoverable_failures, 0);
        assert_eq!(s.transient_failures, 0);
        assert_eq!(s.permanent_failures, 0);
        assert_eq!(s.circuit_rejections, 0);
        assert_eq!(s.consecutive_permanent, 0);
    });
}

// ===========================================================================
// 11. Circuit state accessor
// ===========================================================================

#[test]
fn engine_circuit_state_accessor() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let e = RecoveryEngine::new(RecoveryConfig::default());
        assert_eq!(e.circuit_state(), CircuitStateKind::Closed);
    });
}

// ===========================================================================
// 12. Config accessor
// ===========================================================================

#[test]
fn engine_config_accessor() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let config = RecoveryConfig {
            max_retries: 7,
            ..RecoveryConfig::default()
        };
        let e = RecoveryEngine::new(config);
        assert_eq!(e.config().max_retries, 7);
    });
}

// ===========================================================================
// 13. Engine is available when enabled
// ===========================================================================

#[test]
fn engine_is_available_when_enabled() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mut e = RecoveryEngine::new(RecoveryConfig::default());
        assert!(e.is_available());
    });
}

// ===========================================================================
// 14. Engine is not available when disabled
// ===========================================================================

#[test]
fn engine_is_not_available_when_disabled() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mut e = RecoveryEngine::new(RecoveryConfig {
            enabled: false,
            ..RecoveryConfig::default()
        });
        assert!(!e.is_available());
    });
}

// ===========================================================================
// 15. Reset permanent counter works
// ===========================================================================

#[test]
fn engine_reset_permanent_counter_works() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let config = RecoveryConfig {
            max_retries: 0,
            permanent_failure_limit: 3,
            circuit_failure_threshold: 10,
            report_degradation: false,
            ..RecoveryConfig::default()
        };
        let mut e = RecoveryEngine::new(config);
        let _ = e
            .execute(
                |_| async {
                    Err::<i32, _>("codec version mismatch: local 4 != remote 3".to_string())
                },
                |err: &String| classify_error_message(err),
            )
            .await;
        assert_eq!(e.stats().consecutive_permanent, 1);
        e.reset_permanent_counter();
        assert_eq!(e.stats().consecutive_permanent, 0);
    });
}

// ===========================================================================
// 16. Outcome error kinds populated
// ===========================================================================

#[test]
fn engine_outcome_error_kinds_populated() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let config = RecoveryConfig {
            max_retries: 2,
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
            report_degradation: false,
            ..RecoveryConfig::default()
        };
        let mut e = RecoveryEngine::new(config);
        let o = e
            .execute(
                |_| async { Err::<i32, _>("mux socket disconnected".to_string()) },
                |err: &String| classify_error_message(err),
            )
            .await;
        assert_eq!(o.error_kinds.len(), 3);
        assert!(
            o.error_kinds
                .iter()
                .all(|k| *k == ProtocolErrorKind::Recoverable)
        );
    });
}
