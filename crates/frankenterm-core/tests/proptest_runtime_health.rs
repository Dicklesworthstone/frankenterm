//! Property tests for runtime health checks and doctor surfaces (ft-e34d9.10.7.2).

use proptest::prelude::*;
use std::collections::HashMap;

use frankenterm_core::runtime_health::{
    CheckStatus, HealthCheckData, HealthCheckRegistry, IncidentEnrichment,
    IncidentEnrichmentData, RemediationEffort, RemediationHint,
    RuntimeHealthCheck, check_failure_patterns, check_scope_health, check_telemetry_log,
    check_tier_distribution,
};
use frankenterm_core::runtime_telemetry::{
    FailureClass, HealthTier, RuntimePhase, RuntimeTelemetryEventBuilder, RuntimeTelemetryKind,
    RuntimeTelemetryLog, RuntimeTelemetryLogConfig,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_check_status() -> impl Strategy<Value = CheckStatus> {
    prop_oneof![
        Just(CheckStatus::Pass),
        Just(CheckStatus::Warn),
        Just(CheckStatus::Fail),
        Just(CheckStatus::Skip),
    ]
}

fn arb_health_tier() -> impl Strategy<Value = HealthTier> {
    prop_oneof![
        Just(HealthTier::Green),
        Just(HealthTier::Yellow),
        Just(HealthTier::Red),
        Just(HealthTier::Black),
    ]
}

fn arb_remediation_effort() -> impl Strategy<Value = RemediationEffort> {
    prop_oneof![
        Just(RemediationEffort::Low),
        Just(RemediationEffort::Medium),
        Just(RemediationEffort::High),
    ]
}

fn arb_runtime_phase() -> impl Strategy<Value = RuntimePhase> {
    prop_oneof![
        Just(RuntimePhase::Init),
        Just(RuntimePhase::Startup),
        Just(RuntimePhase::Running),
        Just(RuntimePhase::Draining),
        Just(RuntimePhase::Finalizing),
        Just(RuntimePhase::Shutdown),
        Just(RuntimePhase::Cancelling),
        Just(RuntimePhase::Recovering),
        Just(RuntimePhase::Maintenance),
    ]
}

fn arb_health_check() -> impl Strategy<Value = RuntimeHealthCheck> {
    (
        arb_check_status(),
        arb_health_tier(),
        "[a-z_]{3,10}",
    )
        .prop_map(|(status, tier, id)| {
            let mut check = match status {
                CheckStatus::Pass => RuntimeHealthCheck::pass(&id, &id, "test"),
                CheckStatus::Warn => RuntimeHealthCheck::warn(&id, &id, "test"),
                CheckStatus::Fail => RuntimeHealthCheck::fail(&id, &id, "test"),
                CheckStatus::Skip => RuntimeHealthCheck::skip(&id, &id, "test"),
            };
            check.tier = tier;
            check
        })
}

// =============================================================================
// Property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // ── CheckStatus properties ──

    #[test]
    fn check_status_serde_roundtrip(status in arb_check_status()) {
        let json = serde_json::to_string(&status).unwrap();
        let rt: CheckStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(rt, status);
    }

    #[test]
    fn check_status_healthy_consistency(status in arb_check_status()) {
        // Healthy implies Green tier
        if status.is_healthy() {
            prop_assert_eq!(status.to_tier(), HealthTier::Green);
        } else {
            let tier = status.to_tier();
            prop_assert!(tier.is_degraded(),
                "Unhealthy status {:?} should map to degraded tier, got {:?}", status, tier);
        }
    }

    // ── Report aggregation properties ──

    #[test]
    fn report_worst_tier_is_max(
        checks in proptest::collection::vec(arb_health_check(), 1..10)
    ) {
        let expected_worst = checks.iter().map(|c| c.tier).max().unwrap();

        let mut reg = HealthCheckRegistry::new();
        for check in checks {
            reg.register(check);
        }
        let report = reg.build_report();

        prop_assert_eq!(report.overall_tier, expected_worst);
    }

    #[test]
    fn report_status_counts_sum_to_total(
        checks in proptest::collection::vec(arb_health_check(), 0..20)
    ) {
        let n = checks.len();
        let mut reg = HealthCheckRegistry::new();
        for check in checks {
            reg.register(check);
        }
        let report = reg.build_report();

        prop_assert_eq!(report.status_counts.total() as usize, n);
        prop_assert_eq!(report.checks.len(), n);
    }

    #[test]
    fn report_healthy_iff_no_failures(
        checks in proptest::collection::vec(arb_health_check(), 1..10)
    ) {
        let has_fail = checks.iter().any(|c| c.status == CheckStatus::Fail);

        let mut reg = HealthCheckRegistry::new();
        for check in checks {
            reg.register(check);
        }
        let report = reg.build_report();

        if has_fail {
            prop_assert!(!report.overall_healthy());
        } else {
            prop_assert!(report.overall_healthy());
        }
    }

    #[test]
    fn report_has_warnings_correct(
        checks in proptest::collection::vec(arb_health_check(), 1..10)
    ) {
        let has_warn = checks.iter().any(|c| c.status == CheckStatus::Warn);

        let mut reg = HealthCheckRegistry::new();
        for check in checks {
            reg.register(check);
        }
        let report = reg.build_report();

        prop_assert_eq!(report.has_warnings(), has_warn);
    }

    // ── HealthCheckData conversion ──

    #[test]
    fn health_check_data_preserves_counts(
        checks in proptest::collection::vec(arb_health_check(), 0..10)
    ) {
        let mut reg = HealthCheckRegistry::new();
        for check in checks.clone() {
            reg.register(check);
        }
        let report = reg.build_report();
        let data = HealthCheckData::from(&report);

        prop_assert_eq!(data.summary.total as usize, checks.len());
        prop_assert_eq!(data.healthy, report.overall_healthy());
        prop_assert_eq!(data.has_warnings, report.has_warnings());
    }

    #[test]
    fn health_check_data_serde_roundtrip(
        checks in proptest::collection::vec(arb_health_check(), 1..5)
    ) {
        let mut reg = HealthCheckRegistry::new();
        for check in checks {
            reg.register(check);
        }
        let report = reg.build_report();
        let data = HealthCheckData::from(&report);

        let json = serde_json::to_string(&data).unwrap();
        let rt: HealthCheckData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(rt.overall_tier, data.overall_tier);
        prop_assert_eq!(rt.healthy, data.healthy);
        prop_assert_eq!(rt.summary.total, data.summary.total);
    }

    // ── IncidentEnrichment properties ──

    #[test]
    fn incident_enrichment_serde_roundtrip(
        tier in arb_health_tier(),
        phase in arb_runtime_phase(),
    ) {
        let enrichment = IncidentEnrichment::new(tier, phase);
        let json = serde_json::to_string(&enrichment).unwrap();
        let rt: IncidentEnrichment = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(rt.health_tier, tier);
        prop_assert_eq!(rt.phase, phase);
        prop_assert_eq!(rt.schema_version, IncidentEnrichment::SCHEMA_VERSION);
    }

    #[test]
    fn incident_enrichment_data_conversion(
        tier in arb_health_tier(),
        phase in arb_runtime_phase(),
        n_events in 0usize..20,
    ) {
        let mut log = RuntimeTelemetryLog::with_defaults();
        for i in 0..n_events {
            log.emit(
                RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat)
                    .reason(&format!("ev_{}", i)),
            );
        }

        let max_events = 10;
        let enrichment = IncidentEnrichment::new(tier, phase)
            .with_telemetry_log(&log, max_events);

        let data = IncidentEnrichmentData::from(&enrichment);
        prop_assert_eq!(data.health_tier, tier.to_string());
        prop_assert_eq!(data.phase, phase.to_string());
        prop_assert_eq!(data.recent_event_count, n_events.min(max_events));
    }

    // ── Built-in check: telemetry_log ──

    #[test]
    fn check_telemetry_log_never_panics(
        n_events in 0usize..50,
        max_buffer in 1usize..20,
    ) {
        let mut log = RuntimeTelemetryLog::new(RuntimeTelemetryLogConfig {
            max_events: max_buffer,
            enabled: true,
        });
        for i in 0..n_events {
            log.emit(
                RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat)
                    .reason(&format!("ev_{}", i)),
            );
        }
        let check = check_telemetry_log(&log);
        // Should always produce a valid check
        let is_valid = matches!(check.status, CheckStatus::Pass | CheckStatus::Warn | CheckStatus::Fail);
        prop_assert!(is_valid);
    }

    // ── Built-in check: tier_distribution ──

    #[test]
    fn check_tier_distribution_never_panics(
        n_green in 0usize..10,
        n_yellow in 0usize..10,
        n_red in 0usize..10,
        n_black in 0usize..10,
    ) {
        let mut log = RuntimeTelemetryLog::with_defaults();
        for _ in 0..n_green {
            log.emit(
                RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat)
                    .tier(HealthTier::Green).reason("g"),
            );
        }
        for _ in 0..n_yellow {
            log.emit(
                RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat)
                    .tier(HealthTier::Yellow).reason("y"),
            );
        }
        for _ in 0..n_red {
            log.emit(
                RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat)
                    .tier(HealthTier::Red).reason("r"),
            );
        }
        for _ in 0..n_black {
            log.emit(
                RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat)
                    .tier(HealthTier::Black).reason("b"),
            );
        }
        let check = check_tier_distribution(&log);
        let total = n_green + n_yellow + n_red + n_black;
        if total == 0 {
            prop_assert_eq!(check.status, CheckStatus::Skip);
        } else {
            let is_valid = matches!(check.status, CheckStatus::Pass | CheckStatus::Warn | CheckStatus::Fail);
            prop_assert!(is_valid);
        }
    }

    // ── Built-in check: scope_health ──

    #[test]
    fn check_scope_health_never_panics(
        n_running in 0usize..5,
        n_draining in 0usize..3,
        n_finalizing in 0usize..3,
        n_closed in 0usize..5,
    ) {
        let mut states = HashMap::new();
        for i in 0..n_running {
            states.insert(format!("scope_{}", i), "running".to_string());
        }
        for i in 0..n_draining {
            states.insert(format!("drain_{}", i), "draining".to_string());
        }
        for i in 0..n_finalizing {
            states.insert(format!("final_{}", i), "finalizing".to_string());
        }
        for i in 0..n_closed {
            states.insert(format!("closed_{}", i), "closed".to_string());
        }

        let check = check_scope_health(&states);
        if states.is_empty() {
            prop_assert_eq!(check.status, CheckStatus::Skip);
        } else if n_finalizing > 0 {
            prop_assert_eq!(check.status, CheckStatus::Fail);
        } else if n_draining > 0 {
            prop_assert_eq!(check.status, CheckStatus::Warn);
        } else {
            prop_assert_eq!(check.status, CheckStatus::Pass);
        }
    }

    // ── Built-in check: failure_patterns ──

    #[test]
    fn check_failure_patterns_panic_always_fail(n_panics in 1usize..5) {
        let mut log = RuntimeTelemetryLog::with_defaults();
        for _ in 0..n_panics {
            log.emit(
                RuntimeTelemetryEventBuilder::new("rt.error", RuntimeTelemetryKind::PanicCaptured)
                    .failure(FailureClass::Panic)
                    .reason("panic"),
            );
        }
        let check = check_failure_patterns(&log);
        prop_assert_eq!(check.status, CheckStatus::Fail);
        prop_assert_eq!(check.tier, HealthTier::Black);
    }

    // ── RemediationHint serde ──

    #[test]
    fn remediation_hint_serde_roundtrip(effort in arb_remediation_effort()) {
        let hint = RemediationHint::text("test hint").effort(effort);
        let json = serde_json::to_string(&hint).unwrap();
        let rt: RemediationHint = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(rt.effort, effort);
        prop_assert_eq!(&rt.description, "test hint");
    }

    // ── Report format_summary never panics ──

    #[test]
    fn report_format_summary_never_panics(
        checks in proptest::collection::vec(arb_health_check(), 0..10)
    ) {
        let mut reg = HealthCheckRegistry::new();
        for check in checks {
            reg.register(check);
        }
        let report = reg.build_report();
        let summary = report.format_summary();
        prop_assert!(!summary.is_empty());
    }
}
