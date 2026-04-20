//! Integration test: circuit breaker → retry policy → GC compaction.
//!
//! Exercises the reliability + cleanup pipeline that governs long-running
//! watcher operations:
//!
//!   CircuitBreaker.allow()             (gate operations)
//!     → RetryPolicy.delay_for_attempt()  (backoff when failures occur)
//!       → compact_u64_map()              (reclaim dead pane state)
//!         → should_vacuum()              (config-driven DB maintenance)
//!
//! This mirrors the real watcher loop: circuit breaker protects against
//! cascading failures, retry handles transient errors, and GC cleans up
//! stale per-pane caches after panes are destroyed.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use frankenterm_core::circuit_breaker::{
    CircuitBreaker, CircuitBreakerConfig, CircuitStateKind,
};
use frankenterm_core::gc::{
    compact_u64_map, free_page_ratio, normalized_vacuum_threshold, should_vacuum,
    CacheGcSettings,
};
use frankenterm_core::retry::RetryPolicy;

// ── Helpers ─────────────────────────────────────────────────────────────

/// Build a circuit breaker with fast transitions for testing.
fn test_circuit(failure_threshold: u32) -> CircuitBreaker {
    CircuitBreaker::with_name(
        "test_circuit",
        CircuitBreakerConfig::new(
            failure_threshold,
            1, // single success resets
            Duration::from_millis(10), // short cooldown for tests
        ),
    )
}

/// Choose retry policy based on circuit state: more conservative when
/// circuit is stressed (half-open), aggressive when healthy (closed).
fn retry_for_circuit_state(state: CircuitStateKind) -> RetryPolicy {
    match state {
        CircuitStateKind::Closed => RetryPolicy {
            initial_delay: Duration::from_millis(50),
            max_delay: Duration::from_secs(2),
            backoff_factor: 2.0,
            jitter_percent: 0.0,
            max_attempts: Some(5),
        },
        CircuitStateKind::HalfOpen => RetryPolicy {
            initial_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(10),
            backoff_factor: 3.0,
            jitter_percent: 0.0,
            max_attempts: Some(2),
        },
        CircuitStateKind::Open => RetryPolicy {
            // No retries when open — circuit blocks operations.
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(1),
            backoff_factor: 1.0,
            jitter_percent: 0.0,
            max_attempts: Some(0),
        },
    }
}

/// Simulate a per-pane cache that accumulates state over time.
fn build_pane_cache(pane_ids: &[u64]) -> HashMap<u64, Vec<u8>> {
    pane_ids
        .iter()
        .map(|&id| (id, vec![0u8; 64])) // 64 bytes of state per pane
        .collect()
}

// ── Tests ───────────────────────────────────────────────────────────────

/// Circuit breaker state drives retry policy selection: healthy circuit
/// gets aggressive retries, stressed circuit gets conservative retries.
#[test]
fn circuit_state_drives_retry_policy() {
    let mut cb = test_circuit(3);

    // Initially Closed → aggressive retry.
    assert_eq!(cb.status().state, CircuitStateKind::Closed);
    let closed_policy = retry_for_circuit_state(cb.status().state);
    assert_eq!(closed_policy.max_attempts, Some(5));
    assert_eq!(closed_policy.initial_delay, Duration::from_millis(50));

    // Trip circuit with failures.
    for _ in 0..3 {
        cb.record_failure();
    }
    assert_eq!(cb.status().state, CircuitStateKind::Open);
    let open_policy = retry_for_circuit_state(cb.status().state);
    assert_eq!(open_policy.max_attempts, Some(0), "open circuit: no retries");

    // Wait for cooldown, then allow() transitions to HalfOpen.
    std::thread::sleep(Duration::from_millis(15));
    assert!(cb.allow());
    assert_eq!(cb.status().state, CircuitStateKind::HalfOpen);
    let half_open_policy = retry_for_circuit_state(cb.status().state);
    assert_eq!(half_open_policy.max_attempts, Some(2));
    assert_eq!(half_open_policy.initial_delay, Duration::from_millis(500));

    // Half-open backoff is steeper than closed.
    let ho_d1 = half_open_policy.delay_for_attempt(1);
    let cl_d1 = closed_policy.delay_for_attempt(1);
    assert!(
        ho_d1 > cl_d1,
        "half-open delay ({ho_d1:?}) should exceed closed delay ({cl_d1:?})"
    );
}

/// After circuit recovery, GC cleans up per-pane state for destroyed panes,
/// and the telemetry snapshot reflects the trip+reset cycle.
#[test]
fn circuit_recovery_triggers_gc_cleanup() {
    let mut cb = test_circuit(2);

    // Build a cache for 10 panes.
    let all_pane_ids: Vec<u64> = (1..=10).collect();
    let mut cache = build_pane_cache(&all_pane_ids);
    assert_eq!(cache.len(), 10);

    // Simulate some panes dying during circuit open.
    cb.record_failure();
    cb.record_failure();
    assert_eq!(cb.status().state, CircuitStateKind::Open);

    // Panes 6-10 have been destroyed.
    let active_panes: HashSet<u64> = (1..=5).collect();

    // After circuit opens, we clean up dead pane state.
    let stats = compact_u64_map(&mut cache, &active_panes);
    assert_eq!(stats.removed_entries, 5);
    assert_eq!(stats.after_len, 5);
    assert!(stats.estimated_bytes_freed > 0);

    // Wait for cooldown and recover circuit.
    std::thread::sleep(Duration::from_millis(15));
    assert!(cb.allow()); // → HalfOpen
    cb.record_success(); // → Closed (success_threshold=1)
    assert_eq!(cb.status().state, CircuitStateKind::Closed);

    // Verify telemetry recorded the full cycle.
    let telem = cb.telemetry().snapshot();
    assert_eq!(telem.trips_total, 1);
    assert_eq!(telem.resets_total, 1);
    assert_eq!(telem.failures_recorded, 2);
    assert_eq!(telem.successes_recorded, 1);
    assert_eq!(telem.half_open_probes, 1);
}

/// GC settings from config drive vacuum decisions, and vacuum threshold
/// integrates with free_page_ratio computation.
#[test]
fn gc_config_drives_vacuum_decisions() {
    // Default settings: vacuum at >20% free pages.
    let settings = CacheGcSettings::default();
    assert!(settings.validate().is_ok());
    assert!((settings.vacuum_threshold - 0.20).abs() < f64::EPSILON);

    // Simulate page stats from SQLite.
    let total_pages: i64 = 1000;

    // 15% free → no vacuum.
    assert!(!should_vacuum(total_pages, 150, settings.vacuum_threshold));

    // 25% free → vacuum.
    assert!(should_vacuum(total_pages, 250, settings.vacuum_threshold));

    // Free page ratio computation is correct.
    let ratio = free_page_ratio(total_pages, 200);
    assert!(
        (ratio - 0.2).abs() < f64::EPSILON,
        "200/1000 should be 0.2, got {ratio}"
    );

    // Edge cases: zero pages, negative counts.
    assert!(!should_vacuum(0, 0, settings.vacuum_threshold));
    assert!(!should_vacuum(-1, 50, settings.vacuum_threshold));

    // Threshold normalization clamps out-of-range values.
    assert!((normalized_vacuum_threshold(-0.5) - 0.0).abs() < f64::EPSILON);
    assert!((normalized_vacuum_threshold(1.5) - 1.0).abs() < f64::EPSILON);

    // Custom strict settings: vacuum at >5% free.
    let strict = CacheGcSettings {
        vacuum_threshold: 0.05,
        ..Default::default()
    };
    assert!(strict.validate().is_ok());
    assert!(should_vacuum(total_pages, 60, strict.vacuum_threshold));
    assert!(!should_vacuum(total_pages, 50, strict.vacuum_threshold));
}

/// Full pipeline: circuit breaker gates ops → retry on failure → GC
/// cleans stale state → vacuum decision from config.
#[test]
fn full_pipeline_circuit_retry_gc_vacuum() {
    let mut cb = test_circuit(3);
    let gc_settings = CacheGcSettings::default();

    // Phase 1: healthy circuit, build up pane state.
    let mut pane_cache: HashMap<u64, Vec<u8>> = HashMap::new();
    let mut active_panes: HashSet<u64> = HashSet::new();

    for pane_id in 1..=20u64 {
        assert!(cb.allow(), "circuit should be closed");
        pane_cache.insert(pane_id, vec![0u8; 128]);
        active_panes.insert(pane_id);
        cb.record_success();
    }
    assert_eq!(pane_cache.len(), 20);
    assert_eq!(cb.status().state, CircuitStateKind::Closed);

    // Phase 2: failures accumulate, circuit trips.
    let policy = retry_for_circuit_state(cb.status().state);
    let mut total_retry_delay = Duration::ZERO;

    for attempt in 0..3u32 {
        if !cb.allow() {
            break;
        }
        total_retry_delay += policy.delay_for_attempt(attempt);
        cb.record_failure();
    }
    assert_eq!(cb.status().state, CircuitStateKind::Open);
    // 3 attempts with 50ms, 100ms, 200ms base delays.
    assert_eq!(total_retry_delay, Duration::from_millis(350));

    // Phase 3: while circuit is open, panes 11-20 die.
    for pane_id in 11..=20u64 {
        active_panes.remove(&pane_id);
    }

    // GC reclaims the dead panes.
    let gc_stats = compact_u64_map(&mut pane_cache, &active_panes);
    assert_eq!(gc_stats.removed_entries, 10);
    assert_eq!(gc_stats.after_len, 10);

    // Phase 4: check if vacuum is warranted.
    // Simulate: 500 total pages, 120 free (24% > 20% threshold).
    let needs_vacuum = should_vacuum(500, 120, gc_settings.vacuum_threshold);
    assert!(needs_vacuum, "24% free pages should trigger vacuum");

    // Phase 5: circuit recovers, resume normal operations.
    std::thread::sleep(Duration::from_millis(15));
    assert!(cb.allow()); // → HalfOpen
    let ho_policy = retry_for_circuit_state(cb.status().state);
    assert_eq!(ho_policy.max_attempts, Some(2));

    cb.record_success(); // → Closed
    assert_eq!(cb.status().state, CircuitStateKind::Closed);

    // Final telemetry check.
    let telem = cb.telemetry().snapshot();
    assert_eq!(telem.trips_total, 1);
    assert_eq!(telem.resets_total, 1);
    assert_eq!(telem.successes_recorded, 21); // 20 pane adds + 1 recovery
    assert_eq!(telem.failures_recorded, 3);
}

/// Multiple named circuits can have independent GC caches, and each
/// circuit's state drives its own retry policy independently.
#[test]
fn independent_circuits_with_separate_caches() {
    let mut cb_cli = CircuitBreaker::with_name(
        "wezterm_cli",
        CircuitBreakerConfig::new(2, 1, Duration::from_millis(10)),
    );
    let mut cb_db = CircuitBreaker::with_name(
        "db_write",
        CircuitBreakerConfig::new(5, 2, Duration::from_millis(20)),
    );

    // Separate caches per subsystem.
    let mut cli_cache: HashMap<u64, String> = (1..=5).map(|i| (i, format!("pane-{i}"))).collect();
    let mut db_cache: HashMap<u64, u64> = (1..=8).map(|i| (i, i * 100)).collect();

    // CLI circuit trips (2 failures).
    cb_cli.record_failure();
    cb_cli.record_failure();
    assert_eq!(cb_cli.status().state, CircuitStateKind::Open);

    // DB circuit still healthy.
    cb_db.record_failure();
    assert_eq!(cb_db.status().state, CircuitStateKind::Closed);

    // GC CLI cache: panes 3-5 died.
    let cli_active: HashSet<u64> = [1, 2].into_iter().collect();
    let cli_stats = compact_u64_map(&mut cli_cache, &cli_active);
    assert_eq!(cli_stats.removed_entries, 3);

    // GC DB cache: panes 5-8 died.
    let db_active: HashSet<u64> = (1..=4).collect();
    let db_stats = compact_u64_map(&mut db_cache, &db_active);
    assert_eq!(db_stats.removed_entries, 4);

    // Retry policies differ based on each circuit's state.
    let cli_retry = retry_for_circuit_state(cb_cli.status().state);
    let db_retry = retry_for_circuit_state(cb_db.status().state);
    assert_eq!(cli_retry.max_attempts, Some(0)); // open: no retries
    assert_eq!(db_retry.max_attempts, Some(5)); // closed: full retries

    // DB circuit can still operate normally.
    assert!(cb_db.allow());
    cb_db.record_success();
    assert_eq!(cb_db.status().consecutive_failures, 0);
}

/// Retry backoff delay accumulation stays bounded even when circuit
/// cycles between states.
#[test]
fn retry_delay_bounded_across_circuit_cycles() {
    let mut cb = test_circuit(2);

    for _cycle in 0..3 {
        // Closed → failures → Open.
        let policy = retry_for_circuit_state(cb.status().state);
        for attempt in 0..2 {
            let delay = policy.delay_for_attempt(attempt);
            assert!(delay <= policy.max_delay);
            cb.record_failure();
        }
        assert_eq!(cb.status().state, CircuitStateKind::Open);

        // Open: no operations (circuit blocks).
        assert!(!cb.allow());

        // Wait for cooldown → HalfOpen.
        std::thread::sleep(Duration::from_millis(15));
        assert!(cb.allow());
        assert_eq!(cb.status().state, CircuitStateKind::HalfOpen);

        // HalfOpen retry policy is conservative.
        let ho_policy = retry_for_circuit_state(cb.status().state);
        let ho_delay = ho_policy.delay_for_attempt(0);
        assert_eq!(ho_delay, Duration::from_millis(500));

        // Recover.
        cb.record_success();
        assert_eq!(cb.status().state, CircuitStateKind::Closed);
    }

    // After 3 cycles: 3 trips, 3 resets.
    let telem = cb.telemetry().snapshot();
    assert_eq!(telem.trips_total, 3);
    assert_eq!(telem.resets_total, 3);
}
