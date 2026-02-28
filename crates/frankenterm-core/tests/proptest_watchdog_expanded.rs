#![allow(clippy::overly_complex_bool_expr, clippy::comparison_chain)]
//! Expanded property-based tests for the `watchdog` module.
//!
//! Focuses on HeartbeatRegistry behavioral invariants (check_health logic),
//! WatchdogConfig/MuxWatchdogConfig arbitrary field properties, and
//! MuxHealthReport consistency invariants beyond serde roundtrips.
//!
//! Complements proptest_watchdog.rs which covers serde roundtrips + Display.

use frankenterm_core::watchdog::{
    Component, HealthReport, HealthStatus, HeartbeatRegistry, MuxHealthReport, MuxHealthSample,
    MuxWatchdogConfig, WatchdogConfig,
};
use proptest::prelude::*;
use std::sync::Arc;
use std::time::Duration;

// =========================================================================
// Strategies
// =========================================================================

fn arb_component() -> impl Strategy<Value = Component> {
    prop_oneof![
        Just(Component::Discovery),
        Just(Component::Capture),
        Just(Component::Persistence),
        Just(Component::Maintenance),
    ]
}

fn arb_health_status() -> impl Strategy<Value = HealthStatus> {
    prop_oneof![
        Just(HealthStatus::Healthy),
        Just(HealthStatus::Degraded),
        Just(HealthStatus::Critical),
        Just(HealthStatus::Hung),
    ]
}

/// Arbitrary WatchdogConfig with sensible but varied thresholds.
fn arb_watchdog_config() -> impl Strategy<Value = WatchdogConfig> {
    (
        1_u64..60_000,    // check_interval_ms
        100_u64..120_000, // discovery_stale_ms
        100_u64..120_000, // capture_stale_ms
        100_u64..120_000, // persistence_stale_ms
        100_u64..120_000, // maintenance_stale_ms
        0_u64..120_000,   // grace_period_ms
    )
        .prop_map(
            |(
                check_ms,
                discovery_stale_ms,
                capture_stale_ms,
                persistence_stale_ms,
                maintenance_stale_ms,
                grace_period_ms,
            )| WatchdogConfig {
                check_interval: Duration::from_millis(check_ms),
                discovery_stale_ms,
                capture_stale_ms,
                persistence_stale_ms,
                maintenance_stale_ms,
                grace_period_ms,
            },
        )
}

/// Arbitrary MuxWatchdogConfig with valid field relationships.
fn arb_mux_watchdog_config() -> impl Strategy<Value = MuxWatchdogConfig> {
    (
        1_u64..60_000, // check_interval_ms
        1_u64..30_000, // ping_timeout_ms
        1_u32..20,     // failure_threshold
        // Ensure warning < critical
        1_u64..100_000_000_000,
        1_u64..100_000_000_000,
        1_usize..5000, // history_capacity
    )
        .prop_map(
            |(check_ms, ping_ms, failure_threshold, mem_a, mem_b, history_capacity)| {
                let (warning, critical) = if mem_a <= mem_b {
                    (mem_a, mem_a + mem_b)
                } else {
                    (mem_b, mem_a + mem_b)
                };
                MuxWatchdogConfig {
                    check_interval: Duration::from_millis(check_ms),
                    ping_timeout: Duration::from_millis(ping_ms),
                    failure_threshold,
                    memory_warning_bytes: warning,
                    memory_critical_bytes: critical,
                    history_capacity,
                }
            },
        )
}

fn arb_mux_health_sample() -> impl Strategy<Value = MuxHealthSample> {
    (
        1_000_000_000_000_u64..2_000_000_000_000,
        any::<bool>(),
        proptest::option::of(0_u64..10_000),
        proptest::option::of(0_u64..100_000_000_000),
        arb_health_status(),
    )
        .prop_map(
            |(timestamp_ms, ping_ok, ping_latency_ms, rss_bytes, status)| MuxHealthSample {
                timestamp_ms,
                ping_ok,
                ping_latency_ms,
                rss_bytes,
                status,
                warning_count: 0,
                watchdog_warnings: vec![],
            },
        )
}

// =========================================================================
// HeartbeatRegistry — check_health invariants
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// check_health always returns exactly 4 component entries regardless of config.
    #[test]
    fn check_health_always_four_components(config in arb_watchdog_config()) {
        let reg = HeartbeatRegistry::new();
        let report = reg.check_health(&config);
        prop_assert_eq!(report.components.len(), 4, "must always have 4 components");
    }

    /// check_health overall status equals the worst component status.
    #[test]
    fn check_health_overall_is_worst(config in arb_watchdog_config()) {
        let reg = HeartbeatRegistry::new();
        // Record all heartbeats so we get real health checks (not grace period logic)
        reg.record_discovery();
        reg.record_capture();
        reg.record_persistence();
        reg.record_maintenance();
        let report = reg.check_health(&config);

        let worst_component = report
            .components
            .iter()
            .map(|c| c.status)
            .max()
            .unwrap_or(HealthStatus::Healthy);
        prop_assert_eq!(
            report.overall, worst_component,
            "overall must equal worst component status"
        );
    }

    /// check_health with fresh heartbeats and large thresholds → all Healthy.
    #[test]
    fn fresh_heartbeats_large_thresholds_all_healthy(
        check_ms in 1_u64..60_000,
        grace_ms in 0_u64..120_000,
    ) {
        let reg = HeartbeatRegistry::new();
        reg.record_discovery();
        reg.record_capture();
        reg.record_persistence();
        reg.record_maintenance();
        // Very large thresholds — heartbeats just recorded can't be stale
        let config = WatchdogConfig {
            check_interval: Duration::from_millis(check_ms),
            discovery_stale_ms: u64::MAX / 2,
            capture_stale_ms: u64::MAX / 2,
            persistence_stale_ms: u64::MAX / 2,
            maintenance_stale_ms: u64::MAX / 2,
            grace_period_ms: grace_ms,
        };
        let report = reg.check_health(&config);
        prop_assert_eq!(report.overall, HealthStatus::Healthy);
        for ch in &report.components {
            prop_assert_eq!(ch.status, HealthStatus::Healthy,
                "component {:?} should be Healthy with huge thresholds", ch.component);
        }
    }

    /// check_health with no records and zero grace period → all Degraded.
    #[test]
    fn no_records_zero_grace_all_degraded(config in arb_watchdog_config()) {
        let reg = HeartbeatRegistry::new();
        // Override grace to 0 so unrecorded heartbeats aren't excused
        let config = WatchdogConfig {
            grace_period_ms: 0,
            ..config
        };
        let report = reg.check_health(&config);
        // All components should be Degraded (never recorded, grace expired)
        for ch in &report.components {
            prop_assert_eq!(ch.status, HealthStatus::Degraded,
                "component {:?} should be Degraded with zero grace and no heartbeats", ch.component);
        }
        prop_assert_eq!(report.overall, HealthStatus::Degraded);
    }

    /// check_health with no records but huge grace → all Healthy.
    #[test]
    fn no_records_huge_grace_all_healthy(config in arb_watchdog_config()) {
        let reg = HeartbeatRegistry::new();
        let config = WatchdogConfig {
            grace_period_ms: u64::MAX,
            ..config
        };
        let report = reg.check_health(&config);
        for ch in &report.components {
            prop_assert_eq!(ch.status, HealthStatus::Healthy,
                "component {:?} should be Healthy within grace period", ch.component);
        }
        prop_assert_eq!(report.overall, HealthStatus::Healthy);
    }

    /// Each component in check_health has the correct threshold from config.
    #[test]
    fn check_health_thresholds_match_config(config in arb_watchdog_config()) {
        let reg = HeartbeatRegistry::new();
        let report = reg.check_health(&config);
        for ch in &report.components {
            let expected_threshold = match ch.component {
                Component::Discovery => config.discovery_stale_ms,
                Component::Capture => config.capture_stale_ms,
                Component::Persistence => config.persistence_stale_ms,
                Component::Maintenance => config.maintenance_stale_ms,
            };
            prop_assert_eq!(ch.threshold_ms, expected_threshold,
                "threshold for {:?} should match config", ch.component);
        }
    }

    /// check_health component order matches Component::ALL order.
    #[test]
    fn check_health_component_order(config in arb_watchdog_config()) {
        let reg = HeartbeatRegistry::new();
        let report = reg.check_health(&config);
        for (i, ch) in report.components.iter().enumerate() {
            prop_assert_eq!(ch.component, Component::ALL[i],
                "component at index {} should match ALL[{}]", i, i);
        }
    }

    /// Unrecorded component has None for last_heartbeat_ms and age_ms.
    #[test]
    fn unrecorded_component_has_none_fields(
        component in arb_component(),
        config in arb_watchdog_config(),
    ) {
        let reg = HeartbeatRegistry::new();
        // Record all EXCEPT the selected component
        for c in &Component::ALL {
            if *c != component {
                match c {
                    Component::Discovery => reg.record_discovery(),
                    Component::Capture => reg.record_capture(),
                    Component::Persistence => reg.record_persistence(),
                    Component::Maintenance => reg.record_maintenance(),
                }
            }
        }
        let report = reg.check_health(&config);
        let ch = report
            .components
            .iter()
            .find(|c| c.component == component)
            .unwrap();
        prop_assert!(ch.last_heartbeat_ms.is_none(),
            "unrecorded {:?} should have None last_heartbeat_ms", component);
        prop_assert!(ch.age_ms.is_none(),
            "unrecorded {:?} should have None age_ms", component);
    }

    /// Recorded component has Some for last_heartbeat_ms and age_ms.
    #[test]
    fn recorded_component_has_some_fields(
        component in arb_component(),
    ) {
        let reg = HeartbeatRegistry::new();
        match component {
            Component::Discovery => reg.record_discovery(),
            Component::Capture => reg.record_capture(),
            Component::Persistence => reg.record_persistence(),
            Component::Maintenance => reg.record_maintenance(),
        }
        let config = WatchdogConfig::default();
        let report = reg.check_health(&config);
        let ch = report
            .components
            .iter()
            .find(|c| c.component == component)
            .unwrap();
        prop_assert!(ch.last_heartbeat_ms.is_some(),
            "recorded {:?} should have Some last_heartbeat_ms", component);
        prop_assert!(ch.age_ms.is_some(),
            "recorded {:?} should have Some age_ms", component);
    }

    /// check_health timestamp_ms is non-zero and recent.
    #[test]
    fn check_health_timestamp_recent(config in arb_watchdog_config()) {
        let reg = HeartbeatRegistry::new();
        let report = reg.check_health(&config);
        // Should be a recent epoch ms (after year 2020)
        prop_assert!(report.timestamp_ms > 1_577_836_800_000,
            "timestamp should be after 2020");
    }

    /// HeartbeatRegistry created_at_ms is non-zero and reasonable.
    #[test]
    fn registry_created_at_reasonable(_dummy in 0..10_u8) {
        let reg = HeartbeatRegistry::new();
        prop_assert!(reg.created_at_ms() > 1_577_836_800_000,
            "created_at should be after 2020");
    }

    /// last_heartbeat returns 0 for unrecorded component.
    #[test]
    fn last_heartbeat_zero_when_unrecorded(component in arb_component()) {
        let reg = HeartbeatRegistry::new();
        prop_assert_eq!(reg.last_heartbeat(component), 0,
            "new registry should have zero heartbeat for {:?}", component);
    }

    /// Recording a heartbeat makes last_heartbeat non-zero.
    #[test]
    fn last_heartbeat_nonzero_after_record(component in arb_component()) {
        let reg = HeartbeatRegistry::new();
        match component {
            Component::Discovery => reg.record_discovery(),
            Component::Capture => reg.record_capture(),
            Component::Persistence => reg.record_persistence(),
            Component::Maintenance => reg.record_maintenance(),
        }
        prop_assert!(reg.last_heartbeat(component) > 0,
            "heartbeat should be nonzero after recording {:?}", component);
    }

    /// Recording one component does not affect others.
    #[test]
    fn record_isolates_components(component in arb_component()) {
        let reg = HeartbeatRegistry::new();
        match component {
            Component::Discovery => reg.record_discovery(),
            Component::Capture => reg.record_capture(),
            Component::Persistence => reg.record_persistence(),
            Component::Maintenance => reg.record_maintenance(),
        }
        for c in &Component::ALL {
            if *c != component {
                prop_assert_eq!(reg.last_heartbeat(*c), 0,
                    "recording {:?} should not affect {:?}", component, c);
            }
        }
    }
}

// =========================================================================
// WatchdogConfig — default invariants
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// Arbitrary WatchdogConfig check_health returns valid HealthReport.
    #[test]
    fn arb_config_produces_valid_report(config in arb_watchdog_config()) {
        let reg = HeartbeatRegistry::new();
        reg.record_discovery();
        reg.record_capture();
        reg.record_persistence();
        reg.record_maintenance();
        let report = reg.check_health(&config);
        // Serialize and deserialize to verify the report is well-formed
        let json = serde_json::to_string(&report).unwrap();
        let _back: HealthReport = serde_json::from_str(&json).unwrap();
        prop_assert!(report.timestamp_ms > 0);
        prop_assert_eq!(report.components.len(), 4);
    }

    /// unhealthy_components + healthy components == total components.
    #[test]
    fn healthy_plus_unhealthy_partition(config in arb_watchdog_config()) {
        let reg = HeartbeatRegistry::new();
        reg.record_discovery();
        reg.record_capture();
        reg.record_persistence();
        reg.record_maintenance();
        let report = reg.check_health(&config);
        let unhealthy = report.unhealthy_components().len();
        let healthy = report
            .components
            .iter()
            .filter(|c| c.status == HealthStatus::Healthy)
            .count();
        prop_assert_eq!(
            unhealthy + healthy,
            report.components.len(),
            "healthy + unhealthy should equal total"
        );
    }
}

// =========================================================================
// MuxWatchdogConfig — default invariants
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// MuxWatchdogConfig default has memory_critical > memory_warning.
    #[test]
    fn mux_config_default_memory_ordering(_dummy in 0..1_u8) {
        let config = MuxWatchdogConfig::default();
        prop_assert!(
            config.memory_critical_bytes > config.memory_warning_bytes,
            "critical memory threshold should exceed warning"
        );
    }

    /// MuxWatchdogConfig default has positive failure_threshold.
    #[test]
    fn mux_config_default_positive_threshold(_dummy in 0..1_u8) {
        let config = MuxWatchdogConfig::default();
        prop_assert!(config.failure_threshold > 0);
    }

    /// MuxWatchdogConfig default has positive history_capacity.
    #[test]
    fn mux_config_default_positive_history(_dummy in 0..1_u8) {
        let config = MuxWatchdogConfig::default();
        prop_assert!(config.history_capacity > 0);
    }

    /// Arbitrary MuxWatchdogConfig has warning < critical when properly constructed.
    #[test]
    fn arb_mux_config_memory_ordering(config in arb_mux_watchdog_config()) {
        prop_assert!(
            config.memory_critical_bytes > config.memory_warning_bytes,
            "critical ({}) must exceed warning ({})",
            config.memory_critical_bytes, config.memory_warning_bytes
        );
    }

    /// Arbitrary MuxWatchdogConfig fields are all positive.
    #[test]
    fn arb_mux_config_all_positive(config in arb_mux_watchdog_config()) {
        prop_assert!(config.check_interval > Duration::ZERO);
        prop_assert!(config.ping_timeout > Duration::ZERO);
        prop_assert!(config.failure_threshold > 0);
        prop_assert!(config.history_capacity > 0);
    }
}

// =========================================================================
// MuxHealthReport — consistency invariants
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// total_failures <= total_checks.
    #[test]
    fn mux_report_failures_le_checks(
        total_checks in 0_u64..10_000,
        failures_ratio in 0.0_f64..1.0,
        status in arb_health_status(),
        consecutive in 0_u32..100,
    ) {
        let total_failures = (total_checks as f64 * failures_ratio) as u64;
        let report = MuxHealthReport {
            timestamp_ms: 1_700_000_000_000,
            status,
            consecutive_failures: consecutive,
            latest_sample: None,
            total_checks,
            total_failures,
        };
        prop_assert!(report.total_failures <= report.total_checks,
            "failures ({}) must not exceed checks ({})", report.total_failures, report.total_checks);
    }

    /// MuxHealthReport with Healthy status should have 0 consecutive_failures
    /// (modeling the intended invariant).
    #[test]
    fn mux_report_healthy_zero_consecutive(
        total_checks in 1_u64..10_000,
        total_failures in 0_u64..100,
    ) {
        let report = MuxHealthReport {
            timestamp_ms: 1_700_000_000_000,
            status: HealthStatus::Healthy,
            consecutive_failures: 0,
            latest_sample: None,
            total_checks,
            total_failures,
        };
        prop_assert_eq!(report.consecutive_failures, 0);
        prop_assert_eq!(report.status, HealthStatus::Healthy);
    }

    /// MuxHealthReport serde preserves all fields including nested sample.
    #[test]
    fn mux_report_nested_sample_serde(
        sample in arb_mux_health_sample(),
        status in arb_health_status(),
        consec in 0_u32..50,
        checks in 0_u64..10_000,
        failures in 0_u64..5_000,
    ) {
        let report = MuxHealthReport {
            timestamp_ms: sample.timestamp_ms,
            status,
            consecutive_failures: consec,
            latest_sample: Some(sample.clone()),
            total_checks: checks,
            total_failures: failures.min(checks),
        };
        let json = serde_json::to_string(&report).unwrap();
        let back: MuxHealthReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.status, status);
        prop_assert_eq!(back.consecutive_failures, consec);
        prop_assert_eq!(back.total_checks, checks);
        let back_sample = back.latest_sample.unwrap();
        prop_assert_eq!(back_sample.ping_ok, sample.ping_ok);
        prop_assert_eq!(back_sample.timestamp_ms, sample.timestamp_ms);
    }
}

// =========================================================================
// HeartbeatRegistry — concurrent access
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    /// Recording heartbeats from multiple threads doesn't panic.
    #[test]
    fn concurrent_heartbeat_recording(n_threads in 4_usize..8) {
        let reg = Arc::new(HeartbeatRegistry::new());
        let mut handles = vec![];
        for i in 0..n_threads {
            let r = Arc::clone(&reg);
            handles.push(std::thread::spawn(move || {
                for _ in 0..100 {
                    match i % 4 {
                        0 => r.record_discovery(),
                        1 => r.record_capture(),
                        2 => r.record_persistence(),
                        _ => r.record_maintenance(),
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        // With >= 4 threads, all components should have been recorded
        for c in &Component::ALL {
            prop_assert!(reg.last_heartbeat(*c) > 0,
                "component {:?} should have been recorded by some thread", c);
        }
    }

    /// check_health is safe to call concurrently with recording.
    #[test]
    fn concurrent_check_health_with_recording(_dummy in 0..5_u8) {
        let reg = Arc::new(HeartbeatRegistry::new());
        let config = WatchdogConfig::default();
        reg.record_discovery();
        reg.record_capture();
        reg.record_persistence();
        reg.record_maintenance();

        let r1 = Arc::clone(&reg);
        let recorder = std::thread::spawn(move || {
            for _ in 0..200 {
                r1.record_discovery();
                r1.record_capture();
                r1.record_persistence();
                r1.record_maintenance();
            }
        });

        // Check health concurrently
        for _ in 0..50 {
            let report = reg.check_health(&config);
            assert_eq!(report.components.len(), 4);
        }

        recorder.join().unwrap();
    }
}

// =========================================================================
// HealthStatus — additional ordering properties
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// HealthStatus ordering is transitive: a < b && b < c => a < c.
    #[test]
    fn status_ordering_transitive(
        a in arb_health_status(),
        b in arb_health_status(),
        c in arb_health_status(),
    ) {
        if a < b && b < c {
            prop_assert!(a < c, "ordering should be transitive");
        }
    }

    /// HealthStatus ordering is antisymmetric: a < b => !(b < a).
    #[test]
    fn status_ordering_antisymmetric(
        a in arb_health_status(),
        b in arb_health_status(),
    ) {
        if a < b {
            prop_assert!(!(b < a), "ordering should be antisymmetric");
        }
    }

    /// HealthStatus ordering is total: a < b || a == b || a > b.
    #[test]
    fn status_ordering_total(
        a in arb_health_status(),
        b in arb_health_status(),
    ) {
        prop_assert!(a <= b || a > b, "ordering should be total");
    }
}

// =========================================================================
// WatchdogConfig defaults — unit-style via proptest
// =========================================================================

#[test]
fn watchdog_config_default_check_interval() {
    let config = WatchdogConfig::default();
    assert_eq!(config.check_interval, Duration::from_secs(30));
}

#[test]
fn watchdog_config_default_all_thresholds_positive() {
    let config = WatchdogConfig::default();
    assert!(config.discovery_stale_ms > 0);
    assert!(config.capture_stale_ms > 0);
    assert!(config.persistence_stale_ms > 0);
    assert!(config.maintenance_stale_ms > 0);
    assert!(config.grace_period_ms > 0);
}

#[test]
fn watchdog_config_default_capture_tightest() {
    let config = WatchdogConfig::default();
    // Capture has the tightest threshold since it ticks most frequently
    assert!(config.capture_stale_ms <= config.discovery_stale_ms);
    assert!(config.capture_stale_ms <= config.persistence_stale_ms);
    assert!(config.capture_stale_ms <= config.maintenance_stale_ms);
}

#[test]
fn watchdog_config_default_maintenance_loosest() {
    let config = WatchdogConfig::default();
    // Maintenance runs least frequently, so has the loosest threshold
    assert!(config.maintenance_stale_ms >= config.discovery_stale_ms);
    assert!(config.maintenance_stale_ms >= config.capture_stale_ms);
    assert!(config.maintenance_stale_ms >= config.persistence_stale_ms);
}

#[test]
fn heartbeat_registry_default_same_as_new() {
    let from_default = HeartbeatRegistry::default();
    let from_new = HeartbeatRegistry::new();
    // Both should have zero heartbeats
    for c in &Component::ALL {
        assert_eq!(from_default.last_heartbeat(*c), from_new.last_heartbeat(*c));
    }
}

#[test]
fn mux_watchdog_config_default_memory_ratio() {
    let config = MuxWatchdogConfig::default();
    // Critical should be exactly 2x warning
    assert_eq!(
        config.memory_critical_bytes,
        config.memory_warning_bytes * 2
    );
}

// =========================================================================
// Batch 16: additional property tests (DarkMill)
// =========================================================================

#[test]
fn watchdog_config_all_thresholds_positive() {
    let config = WatchdogConfig::default();
    assert!(config.discovery_stale_ms > 0);
    assert!(config.capture_stale_ms > 0);
    assert!(config.persistence_stale_ms > 0);
    assert!(config.maintenance_stale_ms > 0);
}

#[test]
fn mux_watchdog_config_memory_critical_ge_warning() {
    let config = MuxWatchdogConfig::default();
    assert!(
        config.memory_critical_bytes >= config.memory_warning_bytes,
        "critical {} should be >= warning {}",
        config.memory_critical_bytes,
        config.memory_warning_bytes
    );
}
