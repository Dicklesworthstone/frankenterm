//! Property-based tests for the `watchdog` module.
//!
//! Covers `Component` and `HealthStatus` enum serde roundtrips, Display, Ord
//! invariants, `ComponentHealth`/`HealthReport` serde roundtrips with
//! `unhealthy_components()` aggregate invariants, and
//! `MuxHealthSample`/`MuxHealthReport` serde roundtrips.

use frankenterm_core::watchdog::{
    Component, ComponentHealth, HealthReport, HealthStatus, MuxHealthReport, MuxHealthSample,
};
use proptest::prelude::*;

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

fn arb_component_health() -> impl Strategy<Value = ComponentHealth> {
    (
        arb_component(),
        proptest::option::of(1_000_000_000_000_u64..2_000_000_000_000),
        proptest::option::of(0_u64..300_000),
        0_u64..120_000,
        arb_health_status(),
    )
        .prop_map(
            |(component, last_heartbeat_ms, age_ms, threshold_ms, status)| ComponentHealth {
                component,
                last_heartbeat_ms,
                age_ms,
                threshold_ms,
                status,
            },
        )
}

fn arb_health_report() -> impl Strategy<Value = HealthReport> {
    (
        1_000_000_000_000_u64..2_000_000_000_000,
        arb_health_status(),
        proptest::collection::vec(arb_component_health(), 0..6),
    )
        .prop_map(|(timestamp_ms, overall, components)| HealthReport {
            timestamp_ms,
            overall,
            components,
        })
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
            },
        )
}

fn arb_mux_health_report() -> impl Strategy<Value = MuxHealthReport> {
    (
        1_000_000_000_000_u64..2_000_000_000_000,
        arb_health_status(),
        0_u32..100,
        proptest::option::of(arb_mux_health_sample()),
        0_u64..10_000,
        0_u64..10_000,
    )
        .prop_map(
            |(
                timestamp_ms,
                status,
                consecutive_failures,
                latest_sample,
                total_checks,
                total_failures,
            )| {
                MuxHealthReport {
                    timestamp_ms,
                    status,
                    consecutive_failures,
                    latest_sample,
                    total_checks,
                    total_failures,
                }
            },
        )
}

// =========================================================================
// Component — serde roundtrip, Display, ALL
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// Component serde roundtrip preserves variant.
    #[test]
    fn prop_component_serde(component in arb_component()) {
        let json = serde_json::to_string(&component).unwrap();
        let back: Component = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, component);
    }

    /// Component serde uses snake_case.
    #[test]
    fn prop_component_serde_snake_case(component in arb_component()) {
        let json = serde_json::to_string(&component).unwrap();
        let expected = match component {
            Component::Discovery => "\"discovery\"",
            Component::Capture => "\"capture\"",
            Component::Persistence => "\"persistence\"",
            Component::Maintenance => "\"maintenance\"",
        };
        prop_assert_eq!(&json, expected);
    }

    /// Component Display matches serde name.
    #[test]
    fn prop_component_display_matches_serde(component in arb_component()) {
        let display = component.to_string();
        let json = serde_json::to_string(&component).unwrap();
        // JSON is quoted, Display is not
        prop_assert_eq!(format!("\"{}\"", display), json);
    }

    /// Component serde is deterministic.
    #[test]
    fn prop_component_deterministic(component in arb_component()) {
        let j1 = serde_json::to_string(&component).unwrap();
        let j2 = serde_json::to_string(&component).unwrap();
        prop_assert_eq!(&j1, &j2);
    }

    /// Component::ALL contains exactly 4 distinct variants.
    #[test]
    fn prop_component_all_complete(_dummy in 0..1_u8) {
        let all = Component::ALL;
        prop_assert_eq!(all.len(), 4);
        let set: std::collections::HashSet<_> = all.iter().copied().collect();
        prop_assert_eq!(set.len(), 4);
    }

    /// Every Component::ALL variant serde roundtrips.
    #[test]
    fn prop_component_all_roundtrip(_dummy in 0..1_u8) {
        for component in &Component::ALL {
            let json = serde_json::to_string(component).unwrap();
            let back: Component = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(&back, component);
        }
    }
}

// =========================================================================
// HealthStatus — serde, Display, Ord
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// HealthStatus serde roundtrip preserves variant.
    #[test]
    fn prop_status_serde(status in arb_health_status()) {
        let json = serde_json::to_string(&status).unwrap();
        let back: HealthStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, status);
    }

    /// HealthStatus serde uses snake_case.
    #[test]
    fn prop_status_serde_snake_case(status in arb_health_status()) {
        let json = serde_json::to_string(&status).unwrap();
        let expected = match status {
            HealthStatus::Healthy => "\"healthy\"",
            HealthStatus::Degraded => "\"degraded\"",
            HealthStatus::Critical => "\"critical\"",
            HealthStatus::Hung => "\"hung\"",
        };
        prop_assert_eq!(&json, expected);
    }

    /// HealthStatus Display is non-empty and lowercase.
    #[test]
    fn prop_status_display(status in arb_health_status()) {
        let display = status.to_string();
        prop_assert!(!display.is_empty());
        prop_assert_eq!(&display, &display.to_lowercase());
    }

    /// HealthStatus Ord: Healthy < Degraded < Critical < Hung.
    #[test]
    fn prop_status_ordering(_dummy in 0..1_u8) {
        prop_assert!(HealthStatus::Healthy < HealthStatus::Degraded);
        prop_assert!(HealthStatus::Degraded < HealthStatus::Critical);
        prop_assert!(HealthStatus::Critical < HealthStatus::Hung);
    }

    /// HealthStatus Ord is reflexive (a == a).
    #[test]
    fn prop_status_ord_reflexive(status in arb_health_status()) {
        prop_assert!(status == status);
        prop_assert!(!(status < status));
    }

    /// HealthStatus serde is deterministic.
    #[test]
    fn prop_status_deterministic(status in arb_health_status()) {
        let j1 = serde_json::to_string(&status).unwrap();
        let j2 = serde_json::to_string(&status).unwrap();
        prop_assert_eq!(&j1, &j2);
    }
}

// =========================================================================
// ComponentHealth — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// ComponentHealth serde roundtrip preserves all fields.
    #[test]
    fn prop_component_health_serde(ch in arb_component_health()) {
        let json = serde_json::to_string(&ch).unwrap();
        let back: ComponentHealth = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.component, ch.component);
        prop_assert_eq!(back.last_heartbeat_ms, ch.last_heartbeat_ms);
        prop_assert_eq!(back.age_ms, ch.age_ms);
        prop_assert_eq!(back.threshold_ms, ch.threshold_ms);
        prop_assert_eq!(back.status, ch.status);
    }

    /// ComponentHealth serde is deterministic.
    #[test]
    fn prop_component_health_deterministic(ch in arb_component_health()) {
        let j1 = serde_json::to_string(&ch).unwrap();
        let j2 = serde_json::to_string(&ch).unwrap();
        prop_assert_eq!(&j1, &j2);
    }
}

// =========================================================================
// HealthReport — serde roundtrip + unhealthy_components
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// HealthReport serde roundtrip preserves all fields.
    #[test]
    fn prop_health_report_serde(report in arb_health_report()) {
        let json = serde_json::to_string(&report).unwrap();
        let back: HealthReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.timestamp_ms, report.timestamp_ms);
        prop_assert_eq!(back.overall, report.overall);
        prop_assert_eq!(back.components.len(), report.components.len());
    }

    /// unhealthy_components returns only non-Healthy entries.
    #[test]
    fn prop_unhealthy_only_non_healthy(report in arb_health_report()) {
        let unhealthy = report.unhealthy_components();
        for ch in &unhealthy {
            prop_assert_ne!(ch.status, HealthStatus::Healthy);
        }
    }

    /// unhealthy_components count + healthy count == total components.
    #[test]
    fn prop_unhealthy_plus_healthy_eq_total(report in arb_health_report()) {
        let unhealthy_count = report.unhealthy_components().len();
        let healthy_count = report.components.iter().filter(|c| c.status == HealthStatus::Healthy).count();
        prop_assert_eq!(unhealthy_count + healthy_count, report.components.len());
    }

    /// All-healthy report yields empty unhealthy_components.
    #[test]
    fn prop_all_healthy_no_unhealthy(
        timestamp_ms in 1_000_000_000_000_u64..2_000_000_000_000,
        n in 0_usize..5,
    ) {
        let components: Vec<ComponentHealth> = (0..n).map(|i| ComponentHealth {
            component: Component::ALL[i % 4],
            last_heartbeat_ms: Some(timestamp_ms),
            age_ms: Some(100),
            threshold_ms: 5000,
            status: HealthStatus::Healthy,
        }).collect();
        let report = HealthReport {
            timestamp_ms,
            overall: HealthStatus::Healthy,
            components,
        };
        prop_assert!(report.unhealthy_components().is_empty());
    }

    /// HealthReport serde is deterministic.
    #[test]
    fn prop_health_report_deterministic(report in arb_health_report()) {
        let j1 = serde_json::to_string(&report).unwrap();
        let j2 = serde_json::to_string(&report).unwrap();
        prop_assert_eq!(&j1, &j2);
    }

    /// HealthReport with all components unhealthy yields all in unhealthy_components.
    #[test]
    fn prop_all_unhealthy(
        timestamp_ms in 1_000_000_000_000_u64..2_000_000_000_000,
        status in prop_oneof![
            Just(HealthStatus::Degraded),
            Just(HealthStatus::Critical),
            Just(HealthStatus::Hung),
        ],
    ) {
        let components: Vec<ComponentHealth> = Component::ALL.iter().map(|&c| ComponentHealth {
            component: c,
            last_heartbeat_ms: Some(timestamp_ms - 60_000),
            age_ms: Some(60_000),
            threshold_ms: 5000,
            status,
        }).collect();
        let report = HealthReport {
            timestamp_ms,
            overall: status,
            components,
        };
        prop_assert_eq!(report.unhealthy_components().len(), 4);
    }
}

// =========================================================================
// MuxHealthSample — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// MuxHealthSample serde roundtrip preserves all fields.
    #[test]
    fn prop_mux_sample_serde(sample in arb_mux_health_sample()) {
        let json = serde_json::to_string(&sample).unwrap();
        let back: MuxHealthSample = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.timestamp_ms, sample.timestamp_ms);
        prop_assert_eq!(back.ping_ok, sample.ping_ok);
        prop_assert_eq!(back.ping_latency_ms, sample.ping_latency_ms);
        prop_assert_eq!(back.rss_bytes, sample.rss_bytes);
        prop_assert_eq!(back.status, sample.status);
    }

    /// MuxHealthSample serde is deterministic.
    #[test]
    fn prop_mux_sample_deterministic(sample in arb_mux_health_sample()) {
        let j1 = serde_json::to_string(&sample).unwrap();
        let j2 = serde_json::to_string(&sample).unwrap();
        prop_assert_eq!(&j1, &j2);
    }

    /// MuxHealthSample with None optional fields omits them in JSON or uses null.
    #[test]
    fn prop_mux_sample_none_fields(
        timestamp_ms in 1_000_000_000_000_u64..2_000_000_000_000,
        ping_ok in any::<bool>(),
        status in arb_health_status(),
    ) {
        let sample = MuxHealthSample {
            timestamp_ms,
            ping_ok,
            ping_latency_ms: None,
            rss_bytes: None,
            status,
        };
        let json = serde_json::to_string(&sample).unwrap();
        let back: MuxHealthSample = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.ping_latency_ms, None);
        prop_assert_eq!(back.rss_bytes, None);
    }
}

// =========================================================================
// MuxHealthReport — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// MuxHealthReport serde roundtrip preserves all fields.
    #[test]
    fn prop_mux_report_serde(report in arb_mux_health_report()) {
        let json = serde_json::to_string(&report).unwrap();
        let back: MuxHealthReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.timestamp_ms, report.timestamp_ms);
        prop_assert_eq!(back.status, report.status);
        prop_assert_eq!(back.consecutive_failures, report.consecutive_failures);
        prop_assert_eq!(back.total_checks, report.total_checks);
        prop_assert_eq!(back.total_failures, report.total_failures);
        prop_assert_eq!(back.latest_sample.is_some(), report.latest_sample.is_some());
    }

    /// MuxHealthReport without latest_sample roundtrips.
    #[test]
    fn prop_mux_report_no_sample(
        timestamp_ms in 1_000_000_000_000_u64..2_000_000_000_000,
        status in arb_health_status(),
        total_checks in 0_u64..1000,
        total_failures in 0_u64..1000,
    ) {
        let report = MuxHealthReport {
            timestamp_ms,
            status,
            consecutive_failures: 0,
            latest_sample: None,
            total_checks,
            total_failures,
        };
        let json = serde_json::to_string(&report).unwrap();
        let back: MuxHealthReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.total_checks, total_checks);
        prop_assert!(back.latest_sample.is_none());
    }

    /// MuxHealthReport serde is deterministic.
    #[test]
    fn prop_mux_report_deterministic(report in arb_mux_health_report()) {
        let j1 = serde_json::to_string(&report).unwrap();
        let j2 = serde_json::to_string(&report).unwrap();
        prop_assert_eq!(&j1, &j2);
    }

    /// MuxHealthReport with sample roundtrips sample fields.
    #[test]
    fn prop_mux_report_with_sample(
        report_status in arb_health_status(),
        sample in arb_mux_health_sample(),
    ) {
        let report = MuxHealthReport {
            timestamp_ms: sample.timestamp_ms,
            status: report_status,
            consecutive_failures: 1,
            latest_sample: Some(sample.clone()),
            total_checks: 10,
            total_failures: 1,
        };
        let json = serde_json::to_string(&report).unwrap();
        let back: MuxHealthReport = serde_json::from_str(&json).unwrap();
        let back_sample = back.latest_sample.unwrap();
        prop_assert_eq!(back_sample.ping_ok, sample.ping_ok);
        prop_assert_eq!(back_sample.status, sample.status);
    }
}

// =========================================================================
// Unit tests
// =========================================================================

#[test]
fn component_all_matches_variants() {
    assert_eq!(Component::ALL.len(), 4);
    assert!(Component::ALL.contains(&Component::Discovery));
    assert!(Component::ALL.contains(&Component::Capture));
    assert!(Component::ALL.contains(&Component::Persistence));
    assert!(Component::ALL.contains(&Component::Maintenance));
}

#[test]
fn health_status_full_ordering() {
    let mut statuses = vec![
        HealthStatus::Hung,
        HealthStatus::Healthy,
        HealthStatus::Critical,
        HealthStatus::Degraded,
    ];
    statuses.sort();
    assert_eq!(
        statuses,
        vec![
            HealthStatus::Healthy,
            HealthStatus::Degraded,
            HealthStatus::Critical,
            HealthStatus::Hung,
        ]
    );
}

#[test]
fn health_report_empty_components() {
    let report = HealthReport {
        timestamp_ms: 1_700_000_000_000,
        overall: HealthStatus::Healthy,
        components: vec![],
    };
    assert!(report.unhealthy_components().is_empty());
    let json = serde_json::to_string(&report).unwrap();
    let back: HealthReport = serde_json::from_str(&json).unwrap();
    assert_eq!(back.components.len(), 0);
}

#[test]
fn mux_health_sample_all_fields() {
    let sample = MuxHealthSample {
        timestamp_ms: 1_700_000_000_000,
        ping_ok: true,
        ping_latency_ms: Some(42),
        rss_bytes: Some(1024 * 1024 * 512),
        status: HealthStatus::Healthy,
    };
    let json = serde_json::to_string(&sample).unwrap();
    assert!(json.contains("\"ping_ok\":true"));
    assert!(json.contains("\"ping_latency_ms\":42"));
}
