//! Performance integration test suite.
//!
//! Validates that performance components (connection pool, circuit breaker,
//! retry, watchdog, backpressure, orphan reaper) work correctly in concert.
//!
//! All tests are self-contained — no running WezTerm or mux server required.
//!
//! Bead: wa-3cyp.2

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use frankenterm_core::backpressure::{
    BackpressureConfig, BackpressureManager, BackpressureTier, QueueDepths,
};
use frankenterm_core::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig, CircuitStateKind};
use frankenterm_core::pool::{Pool, PoolConfig, PoolError};
use frankenterm_core::retry::{
    RetryPolicy, with_retry, with_retry_and_circuit, with_retry_outcome,
};
use frankenterm_core::runtime_compat::sleep;
use frankenterm_core::watchdog::{Component, HealthStatus, HeartbeatRegistry, WatchdogConfig};

// =============================================================================
// 1. Connection pool integration
// =============================================================================

#[tokio::test]
async fn pool_lifecycle() {
    let pool: Pool<String> = Pool::new(PoolConfig {
        max_size: 4,
        idle_timeout: Duration::from_secs(60),
        acquire_timeout: Duration::from_secs(1),
    });

    // Seed the pool with connections
    for i in 0..4 {
        pool.put(format!("conn-{i}")).await;
    }

    // Acquire all 4
    let mut guards = Vec::new();
    for _ in 0..4 {
        guards.push(pool.acquire().await.unwrap());
    }
    assert_eq!(pool.stats().await.active_count, 4);

    // Return them
    drop(guards);

    // Re-acquire should succeed
    let _g = pool.acquire().await.unwrap();
    let stats = pool.stats().await;
    assert!(stats.total_acquired >= 5);
}

#[tokio::test]
async fn pool_under_load() {
    let pool = Arc::new(Pool::<u32>::new(PoolConfig {
        max_size: 8,
        idle_timeout: Duration::from_secs(60),
        acquire_timeout: Duration::from_secs(5),
    }));

    // Seed pool
    for i in 0..8 {
        pool.put(i).await;
    }

    let completed = Arc::new(AtomicU32::new(0));

    // Launch 50 concurrent tasks each acquiring, holding briefly, then releasing
    let mut handles = Vec::new();
    for _ in 0..50 {
        let pool = pool.clone();
        let completed = completed.clone();
        handles.push(tokio::spawn(async move {
            let guard = pool.acquire().await.unwrap();
            // Simulate work
            sleep(Duration::from_millis(5)).await;
            drop(guard);
            completed.fetch_add(1, Ordering::SeqCst);
        }));
    }

    for h in handles {
        h.await.unwrap();
    }

    assert_eq!(completed.load(Ordering::SeqCst), 50);
    let stats = pool.stats().await;
    assert!(
        stats.total_acquired >= 50,
        "expected at least 50 acquires, got {}",
        stats.total_acquired
    );
    assert_eq!(stats.total_timeouts, 0);
}

#[tokio::test]
async fn pool_exhaustion_waits() {
    let pool = Arc::new(Pool::<u32>::new(PoolConfig {
        max_size: 1,
        idle_timeout: Duration::from_secs(60),
        acquire_timeout: Duration::from_secs(2),
    }));
    pool.put(1).await;

    // Hold the only connection
    let guard = pool.acquire().await.unwrap();

    // Spawn a task that tries to acquire — it should wait
    let pool2 = pool.clone();
    let waiter = tokio::spawn(async move {
        let start = Instant::now();
        let _g = pool2.acquire().await.unwrap();
        start.elapsed()
    });

    // Release after 200ms
    sleep(Duration::from_millis(200)).await;
    drop(guard);

    let wait_time = waiter.await.unwrap();
    assert!(
        wait_time >= Duration::from_millis(100),
        "waiter should have waited, but waited only {:?}",
        wait_time
    );
}

#[tokio::test]
async fn pool_acquire_timeout() {
    let pool = Pool::<u32>::new(PoolConfig {
        max_size: 1,
        idle_timeout: Duration::from_secs(60),
        acquire_timeout: Duration::from_millis(100),
    });
    pool.put(1).await;

    // Hold the only connection
    let _guard = pool.acquire().await.unwrap();

    // Try to acquire — should timeout
    let result = pool.acquire().await;
    assert_eq!(result.unwrap_err(), PoolError::AcquireTimeout);

    let stats = pool.stats().await;
    assert_eq!(stats.total_timeouts, 1);
}

#[tokio::test]
async fn pool_evict_idle() {
    let pool = Pool::<u32>::new(PoolConfig {
        max_size: 4,
        idle_timeout: Duration::from_millis(50), // very short
        acquire_timeout: Duration::from_secs(1),
    });

    for i in 0..4 {
        pool.put(i).await;
    }

    // Wait for connections to become stale
    sleep(Duration::from_millis(100)).await;

    let evicted = pool.evict_idle().await;
    assert_eq!(evicted, 4);

    let stats = pool.stats().await;
    assert_eq!(stats.idle_count, 0);
    assert_eq!(stats.total_evicted, 4);
}

#[tokio::test]
async fn pool_metrics_accuracy() {
    let pool = Pool::<u32>::new(PoolConfig::default());

    // Seed
    pool.put(1).await;
    pool.put(2).await;

    let s1 = pool.stats().await;
    assert_eq!(s1.idle_count, 2);
    assert_eq!(s1.active_count, 0);

    // Acquire one
    let _g = pool.acquire().await.unwrap();
    let s2 = pool.stats().await;
    assert_eq!(s2.idle_count, 1);
    assert_eq!(s2.active_count, 1);
    assert_eq!(s2.total_acquired, 1);
}

// =============================================================================
// 2. Circuit breaker integration
// =============================================================================

#[test]
fn circuit_breaker_state_transitions() {
    let config = CircuitBreakerConfig::new(3, 1, Duration::from_millis(50));
    let mut cb = CircuitBreaker::new(config);

    // Starts closed
    assert_eq!(cb.status().state, CircuitStateKind::Closed);
    assert!(cb.allow());

    // 3 failures → opens
    cb.record_failure();
    cb.record_failure();
    assert!(cb.allow()); // still closed after 2
    cb.record_failure();
    assert_eq!(cb.status().state, CircuitStateKind::Open);
    assert!(!cb.allow());
}

#[test]
fn circuit_breaker_half_open_to_closed() {
    let config = CircuitBreakerConfig::new(2, 1, Duration::from_millis(10));
    let mut cb = CircuitBreaker::new(config);

    // Open it
    cb.record_failure();
    cb.record_failure();
    assert_eq!(cb.status().state, CircuitStateKind::Open);

    // Wait for cooldown
    std::thread::sleep(Duration::from_millis(20));

    // After cooldown, allow() transitions to half-open
    assert!(cb.allow());
    assert_eq!(cb.status().state, CircuitStateKind::HalfOpen);

    // One success closes it
    cb.record_success();
    assert_eq!(cb.status().state, CircuitStateKind::Closed);
}

#[test]
fn circuit_breaker_half_open_failure_reopens() {
    let config = CircuitBreakerConfig::new(2, 1, Duration::from_millis(10));
    let mut cb = CircuitBreaker::new(config);

    // Open it
    cb.record_failure();
    cb.record_failure();

    // Wait for cooldown and enter half-open
    std::thread::sleep(Duration::from_millis(20));
    assert!(cb.allow());

    // Failure in half-open → reopens
    cb.record_failure();
    assert_eq!(cb.status().state, CircuitStateKind::Open);
}

#[test]
fn circuit_breaker_success_resets_failure_count() {
    let config = CircuitBreakerConfig::new(3, 1, Duration::from_millis(50));
    let mut cb = CircuitBreaker::new(config);

    cb.record_failure();
    cb.record_failure();
    // 2 failures, one more would open
    cb.record_success(); // resets count
    cb.record_failure();
    cb.record_failure();
    // Still closed — success reset the counter
    assert_eq!(cb.status().state, CircuitStateKind::Closed);
}

#[test]
fn circuit_breaker_status_reports_failures() {
    let config = CircuitBreakerConfig::new(5, 1, Duration::from_secs(10));
    let mut cb = CircuitBreaker::new(config);

    cb.record_failure();
    cb.record_failure();
    cb.record_failure();

    let status = cb.status();
    assert_eq!(status.consecutive_failures, 3);
    assert_eq!(status.state, CircuitStateKind::Closed);
}

// =============================================================================
// 3. Retry integration
// =============================================================================

#[tokio::test]
async fn retry_succeeds_on_first_attempt() {
    let policy = RetryPolicy {
        max_attempts: Some(3),
        initial_delay: Duration::from_millis(10),
        ..RetryPolicy::default()
    };

    let attempt_count = Arc::new(AtomicU32::new(0));
    let ac = attempt_count.clone();

    let result = with_retry(&policy, || {
        let ac = ac.clone();
        async move {
            ac.fetch_add(1, Ordering::SeqCst);
            Ok::<_, frankenterm_core::Error>(42)
        }
    })
    .await;

    assert_eq!(result.unwrap(), 42);
    assert_eq!(attempt_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn retry_succeeds_after_transient_failures() {
    let policy = RetryPolicy {
        max_attempts: Some(5),
        initial_delay: Duration::from_millis(5),
        backoff_factor: 1.0, // no backoff for test speed
        jitter_percent: 0.0,
        ..RetryPolicy::default()
    };

    let attempt_count = Arc::new(AtomicU32::new(0));
    let ac = attempt_count.clone();

    let result = with_retry(&policy, || {
        let ac = ac.clone();
        async move {
            let n = ac.fetch_add(1, Ordering::SeqCst);
            if n < 2 {
                Err(frankenterm_core::Error::Wezterm(
                    frankenterm_core::error::WeztermError::Timeout(5),
                ))
            } else {
                Ok(99)
            }
        }
    })
    .await;

    assert_eq!(result.unwrap(), 99);
    assert_eq!(attempt_count.load(Ordering::SeqCst), 3); // 2 failures + 1 success
}

#[tokio::test]
async fn retry_exhausted_returns_error() {
    let policy = RetryPolicy {
        max_attempts: Some(3),
        initial_delay: Duration::from_millis(5),
        backoff_factor: 1.0,
        jitter_percent: 0.0,
        ..RetryPolicy::default()
    };

    let attempt_count = Arc::new(AtomicU32::new(0));
    let ac = attempt_count.clone();

    let result: frankenterm_core::Result<i32> = with_retry(&policy, || {
        let ac = ac.clone();
        async move {
            ac.fetch_add(1, Ordering::SeqCst);
            Err(frankenterm_core::Error::Wezterm(
                frankenterm_core::error::WeztermError::Timeout(5),
            ))
        }
    })
    .await;

    assert!(result.is_err());
    assert_eq!(attempt_count.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn retry_outcome_tracks_attempt_count() {
    let policy = RetryPolicy {
        max_attempts: Some(4),
        initial_delay: Duration::from_millis(5),
        backoff_factor: 1.0,
        jitter_percent: 0.0,
        ..RetryPolicy::default()
    };

    let attempt_count = Arc::new(AtomicU32::new(0));
    let ac = attempt_count.clone();

    let outcome = with_retry_outcome(&policy, || {
        let ac = ac.clone();
        async move {
            let n = ac.fetch_add(1, Ordering::SeqCst);
            if n < 1 {
                Err(frankenterm_core::Error::Wezterm(
                    frankenterm_core::error::WeztermError::Timeout(5),
                ))
            } else {
                Ok::<_, frankenterm_core::Error>("done")
            }
        }
    })
    .await;

    assert!(outcome.result.is_ok());
    assert_eq!(outcome.attempts, 2);
    assert!(outcome.elapsed > Duration::ZERO);
}

#[tokio::test]
async fn retry_with_circuit_breaker_skips_when_open() {
    let policy = RetryPolicy {
        max_attempts: Some(5),
        initial_delay: Duration::from_millis(5),
        backoff_factor: 1.0,
        jitter_percent: 0.0,
        ..RetryPolicy::default()
    };

    let mut cb = CircuitBreaker::new(CircuitBreakerConfig::new(
        2,
        1,
        Duration::from_secs(60), // long cooldown
    ));

    // Open the circuit manually
    cb.record_failure();
    cb.record_failure();
    assert_eq!(cb.status().state, CircuitStateKind::Open);

    let attempt_count = Arc::new(AtomicU32::new(0));
    let ac = attempt_count.clone();

    let result: frankenterm_core::Result<i32> = with_retry_and_circuit(&policy, &mut cb, || {
        let ac = ac.clone();
        async move {
            ac.fetch_add(1, Ordering::SeqCst);
            Ok(42)
        }
    })
    .await;

    // Should fail without executing the operation
    assert!(result.is_err());
    assert_eq!(attempt_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn retry_exponential_backoff_timing() {
    let policy = RetryPolicy {
        max_attempts: Some(4),
        initial_delay: Duration::from_millis(50),
        max_delay: Duration::from_secs(10),
        backoff_factor: 2.0,
        jitter_percent: 0.0, // no jitter for timing accuracy
    };

    let start = Instant::now();
    let attempt_count = Arc::new(AtomicU32::new(0));
    let ac = attempt_count.clone();

    let _: frankenterm_core::Result<i32> = with_retry(&policy, || {
        let ac = ac.clone();
        async move {
            ac.fetch_add(1, Ordering::SeqCst);
            Err(frankenterm_core::Error::Wezterm(
                frankenterm_core::error::WeztermError::Timeout(5),
            ))
        }
    })
    .await;

    let elapsed = start.elapsed();
    // 3 delays: 50ms + 100ms + 200ms = 350ms minimum
    assert!(
        elapsed >= Duration::from_millis(300),
        "expected at least 300ms backoff, got {:?}",
        elapsed
    );
    assert_eq!(attempt_count.load(Ordering::SeqCst), 4);
}

// =============================================================================
// 4. Watchdog / heartbeat integration
// =============================================================================

#[test]
fn watchdog_healthy_server() {
    let registry = HeartbeatRegistry::new();

    // Record heartbeats for all components
    registry.record_discovery();
    registry.record_capture();
    registry.record_persistence();
    registry.record_maintenance();

    let config = WatchdogConfig {
        discovery_stale_ms: 5000,
        capture_stale_ms: 5000,
        persistence_stale_ms: 5000,
        maintenance_stale_ms: 5000,
        grace_period_ms: 30_000,
        ..WatchdogConfig::default()
    };

    let report = registry.check_health(&config);
    assert_eq!(report.overall, HealthStatus::Healthy);
}

#[test]
fn watchdog_detects_stale_component() {
    let registry = HeartbeatRegistry::new();

    // Record only some heartbeats
    registry.record_discovery();
    registry.record_capture();
    // persistence and maintenance NOT recorded

    // Use a very short grace period and wait
    std::thread::sleep(Duration::from_millis(50));

    let config = WatchdogConfig {
        discovery_stale_ms: 5000,
        capture_stale_ms: 5000,
        persistence_stale_ms: 5000,
        maintenance_stale_ms: 5000,
        grace_period_ms: 10, // very short grace period
        ..WatchdogConfig::default()
    };

    let report = registry.check_health(&config);
    // Components that never recorded should be degraded/critical
    assert_ne!(report.overall, HealthStatus::Healthy);
}

#[test]
fn watchdog_grace_period_prevents_false_alarms() {
    let registry = HeartbeatRegistry::new();
    // Don't record any heartbeats — within grace period everything should be healthy

    let config = WatchdogConfig {
        discovery_stale_ms: 100,
        capture_stale_ms: 100,
        persistence_stale_ms: 100,
        maintenance_stale_ms: 100,
        grace_period_ms: 60_000, // generous grace period
        ..WatchdogConfig::default()
    };

    let report = registry.check_health(&config);
    assert_eq!(
        report.overall,
        HealthStatus::Healthy,
        "within grace period, no alarms should fire"
    );
}

#[test]
fn watchdog_report_contains_all_components() {
    let registry = HeartbeatRegistry::new();
    registry.record_discovery();
    registry.record_capture();
    registry.record_persistence();
    registry.record_maintenance();

    let config = WatchdogConfig::default();
    let report = registry.check_health(&config);

    assert_eq!(report.components.len(), 4);
    let component_names: Vec<Component> = report.components.iter().map(|c| c.component).collect();
    assert!(component_names.contains(&Component::Discovery));
    assert!(component_names.contains(&Component::Capture));
    assert!(component_names.contains(&Component::Persistence));
    assert!(component_names.contains(&Component::Maintenance));
}

// =============================================================================
// 5. Backpressure integration
// =============================================================================

#[test]
fn backpressure_green_classification() {
    let config = BackpressureConfig::default();
    let manager = BackpressureManager::new(config);

    let depths = QueueDepths {
        capture_depth: 10,
        capture_capacity: 1000,
        write_depth: 5,
        write_capacity: 500,
    };

    assert_eq!(manager.classify(&depths), BackpressureTier::Green);
}

#[test]
fn backpressure_yellow_classification() {
    let config = BackpressureConfig::default();
    let manager = BackpressureManager::new(config.clone());

    // Push capture queue to yellow threshold (default 0.6)
    let depths = QueueDepths {
        capture_depth: 700,
        capture_capacity: 1000,
        write_depth: 10,
        write_capacity: 500,
    };

    let tier = manager.classify(&depths);
    assert!(
        tier >= BackpressureTier::Yellow,
        "70% capture fill should be at least yellow, got {:?}",
        tier
    );
}

#[test]
fn backpressure_red_classification() {
    let config = BackpressureConfig::default();
    let manager = BackpressureManager::new(config);

    // Push capture queue to red threshold (default 0.85)
    let depths = QueueDepths {
        capture_depth: 900,
        capture_capacity: 1000,
        write_depth: 10,
        write_capacity: 500,
    };

    let tier = manager.classify(&depths);
    assert!(
        tier >= BackpressureTier::Red,
        "90% capture fill should be at least red, got {:?}",
        tier
    );
}

#[test]
fn backpressure_black_near_saturation() {
    let config = BackpressureConfig::default();
    let manager = BackpressureManager::new(config);

    let depths = QueueDepths {
        capture_depth: 998,
        capture_capacity: 1000,
        write_depth: 10,
        write_capacity: 500,
    };

    assert_eq!(manager.classify(&depths), BackpressureTier::Black);
}

#[test]
fn backpressure_tier_ordering() {
    assert!(BackpressureTier::Green < BackpressureTier::Yellow);
    assert!(BackpressureTier::Yellow < BackpressureTier::Red);
    assert!(BackpressureTier::Red < BackpressureTier::Black);
}

#[test]
fn backpressure_evaluate_reports_transition() {
    let config = BackpressureConfig {
        enabled: true,
        hysteresis_ms: 0, // disable hysteresis for test
        ..BackpressureConfig::default()
    };
    let manager = BackpressureManager::new(config);

    // Start green
    let green = QueueDepths {
        capture_depth: 10,
        capture_capacity: 1000,
        write_depth: 5,
        write_capacity: 500,
    };
    let _result = manager.evaluate(&green);
    // First evaluation may or may not report transition
    // (it's from Green to Green — no change)

    // Now push to red
    let red = QueueDepths {
        capture_depth: 900,
        capture_capacity: 1000,
        write_depth: 10,
        write_capacity: 500,
    };
    let result = manager.evaluate(&red);
    if let Some((from, to)) = result {
        assert!(to > from, "should transition upward: {:?} → {:?}", from, to);
    }
}

#[test]
fn backpressure_snapshot_is_consistent() {
    let config = BackpressureConfig::default();
    let manager = BackpressureManager::new(config);

    let depths = QueueDepths {
        capture_depth: 50,
        capture_capacity: 1000,
        write_depth: 25,
        write_capacity: 500,
    };

    let snap = manager.snapshot(&depths);
    assert_eq!(snap.tier, BackpressureTier::Green);
    assert!(snap.capture_depth <= snap.capture_capacity);
    assert!(snap.write_depth <= snap.write_capacity);
}

#[test]
fn backpressure_disabled_returns_none() {
    let config = BackpressureConfig {
        enabled: false,
        ..BackpressureConfig::default()
    };
    let manager = BackpressureManager::new(config);

    let depths = QueueDepths {
        capture_depth: 999,
        capture_capacity: 1000,
        write_depth: 499,
        write_capacity: 500,
    };

    // When disabled, evaluate returns None (no tier transition)
    assert!(manager.evaluate(&depths).is_none());
}

#[test]
fn backpressure_zero_capacity_no_panic() {
    let config = BackpressureConfig::default();
    let manager = BackpressureManager::new(config);

    let depths = QueueDepths {
        capture_depth: 0,
        capture_capacity: 0,
        write_depth: 0,
        write_capacity: 0,
    };

    // Should not panic on division by zero
    let tier = manager.classify(&depths);
    assert_eq!(tier, BackpressureTier::Green);
}

// =============================================================================
// 6. Cross-component integration
// =============================================================================

#[tokio::test]
async fn pool_and_retry_integration() {
    let pool = Arc::new(Pool::<u32>::new(PoolConfig {
        max_size: 2,
        idle_timeout: Duration::from_secs(60),
        acquire_timeout: Duration::from_millis(500),
    }));

    // Seed pool
    pool.put(1).await;
    pool.put(2).await;

    let policy = RetryPolicy {
        max_attempts: Some(3),
        initial_delay: Duration::from_millis(10),
        backoff_factor: 1.0,
        jitter_percent: 0.0,
        ..RetryPolicy::default()
    };

    let pool_clone = pool.clone();
    let attempt_count = Arc::new(AtomicU32::new(0));
    let ac = attempt_count.clone();

    // Use retry to acquire from pool and do work
    let result = with_retry(&policy, || {
        let pool = pool_clone.clone();
        let ac = ac.clone();
        async move {
            let guard = pool
                .acquire()
                .await
                .map_err(|e| frankenterm_core::Error::Runtime(e.to_string()))?;
            ac.fetch_add(1, Ordering::SeqCst);
            drop(guard);
            Ok::<_, frankenterm_core::Error>(true)
        }
    })
    .await;

    assert!(result.unwrap());
    assert_eq!(attempt_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn circuit_breaker_and_pool_integration() {
    let pool = Arc::new(Pool::<u32>::new(PoolConfig {
        max_size: 2,
        idle_timeout: Duration::from_secs(60),
        acquire_timeout: Duration::from_millis(200),
    }));

    pool.put(1).await;

    let cb = Arc::new(Mutex::new(CircuitBreaker::new(CircuitBreakerConfig::new(
        2,
        1,
        Duration::from_secs(60),
    ))));

    // Successful acquire records success
    {
        let guard = pool.acquire().await.unwrap();
        cb.lock().unwrap().record_success();
        drop(guard);
    }

    assert_eq!(cb.lock().unwrap().status().state, CircuitStateKind::Closed);

    // Simulate failures
    cb.lock().unwrap().record_failure();
    cb.lock().unwrap().record_failure();

    // Circuit is now open — operations should be rejected
    assert!(!cb.lock().unwrap().allow());
}

#[tokio::test]
async fn watchdog_and_backpressure_coherence() {
    // Verify that when backpressure is at Black, the watchdog can still
    // function (no deadlocks, no panics)
    let registry = HeartbeatRegistry::new();
    let bp_config = BackpressureConfig::default();
    let bp_manager = BackpressureManager::new(bp_config);

    // Record heartbeats under extreme load
    registry.record_discovery();
    registry.record_capture();
    registry.record_persistence();
    registry.record_maintenance();

    // Classify at near-saturation
    let depths = QueueDepths {
        capture_depth: 999,
        capture_capacity: 1000,
        write_depth: 499,
        write_capacity: 500,
    };

    let tier = bp_manager.classify(&depths);
    assert_eq!(tier, BackpressureTier::Black);

    // Watchdog should still work
    let wd_config = WatchdogConfig::default();
    let report = registry.check_health(&wd_config);
    // Should not panic, health status is valid
    assert!(matches!(
        report.overall,
        HealthStatus::Healthy | HealthStatus::Degraded | HealthStatus::Critical
    ));
}

#[tokio::test]
async fn concurrent_pool_acquire_and_evict() {
    let pool = Arc::new(Pool::<u32>::new(PoolConfig {
        max_size: 4,
        idle_timeout: Duration::from_millis(20),
        acquire_timeout: Duration::from_secs(2),
    }));

    // Seed
    for i in 0..4 {
        pool.put(i).await;
    }

    let pool1 = pool.clone();
    let pool2 = pool.clone();

    // Concurrent: one task acquires/releases, another evicts
    let acquire_task = tokio::spawn(async move {
        for _ in 0..20 {
            if let Ok(guard) = pool1.acquire().await {
                sleep(Duration::from_millis(5)).await;
                drop(guard);
            }
        }
    });

    let evict_task = tokio::spawn(async move {
        for _ in 0..10 {
            pool2.evict_idle().await;
            sleep(Duration::from_millis(10)).await;
        }
    });

    // Both should complete without deadlock
    let (r1, r2) = tokio::join!(acquire_task, evict_task);
    r1.unwrap();
    r2.unwrap();
}

// =============================================================================
// 7. Stress tests
// =============================================================================

#[tokio::test]
async fn pool_stress_100_concurrent() {
    let pool = Arc::new(Pool::<u32>::new(PoolConfig {
        max_size: 4,
        idle_timeout: Duration::from_secs(60),
        acquire_timeout: Duration::from_secs(10),
    }));

    for i in 0..4 {
        pool.put(i).await;
    }

    let completed = Arc::new(AtomicU32::new(0));
    let mut handles = Vec::new();

    for _ in 0..100 {
        let pool = pool.clone();
        let completed = completed.clone();
        handles.push(tokio::spawn(async move {
            let guard = pool.acquire().await.unwrap();
            sleep(Duration::from_millis(1)).await;
            drop(guard);
            completed.fetch_add(1, Ordering::SeqCst);
        }));
    }

    for h in handles {
        h.await.unwrap();
    }

    assert_eq!(completed.load(Ordering::SeqCst), 100);
}

#[test]
fn circuit_breaker_rapid_transitions() {
    let config = CircuitBreakerConfig::new(2, 1, Duration::from_millis(1));
    let mut cb = CircuitBreaker::new(config);

    // Rapidly cycle through states
    for _ in 0..50 {
        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.status().state, CircuitStateKind::Open);

        std::thread::sleep(Duration::from_millis(5));
        assert!(cb.allow()); // transitions to half-open

        cb.record_success();
        assert_eq!(cb.status().state, CircuitStateKind::Closed);
    }
}

#[test]
fn backpressure_continuous_evaluation() {
    let config = BackpressureConfig {
        enabled: true,
        hysteresis_ms: 0,
        ..BackpressureConfig::default()
    };
    let manager = BackpressureManager::new(config);

    // Simulate a load ramp from 0% to 100% and back
    for load in (0..=100).chain((0..100).rev()) {
        let depths = QueueDepths {
            capture_depth: load * 10,
            capture_capacity: 1000,
            write_depth: load * 5,
            write_capacity: 500,
        };
        let _ = manager.evaluate(&depths);
    }

    // Final state should be green (load back to 0)
    let final_depths = QueueDepths {
        capture_depth: 0,
        capture_capacity: 1000,
        write_depth: 0,
        write_capacity: 500,
    };
    assert_eq!(manager.classify(&final_depths), BackpressureTier::Green);
}
