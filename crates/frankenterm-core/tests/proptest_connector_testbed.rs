//! Property-based tests for the connector_testbed module.
//!
//! Tests serde roundtrips and behavioral invariants for MockProvider,
//! MockRequest, ChaosScenario, ChaosScenarioKind, SandboxEscapeAttempt,
//! SandboxEscapeResult, SandboxProbeReport, IngestionFloodReport,
//! TestbedTelemetry, TestbedSnapshot, MockProviderOutcome, and TestbedConfig.

use frankenterm_core::connector_host_runtime::ConnectorCapability;
use frankenterm_core::connector_testbed::*;
use proptest::prelude::*;
use std::collections::VecDeque;

// =============================================================================
// Strategies
// =============================================================================

fn arb_chaos_scenario_kind() -> impl Strategy<Value = ChaosScenarioKind> {
    prop_oneof![
        Just(ChaosScenarioKind::ProviderOutage),
        Just(ChaosScenarioKind::ErrorStorm),
        Just(ChaosScenarioKind::RateLimitFlood),
        Just(ChaosScenarioKind::LatencySpike),
        Just(ChaosScenarioKind::SandboxProbe),
        Just(ChaosScenarioKind::CredentialRotation),
        Just(ChaosScenarioKind::IngestionFlood),
    ]
}

fn arb_connector_capability() -> impl Strategy<Value = ConnectorCapability> {
    prop_oneof![
        Just(ConnectorCapability::Invoke),
        Just(ConnectorCapability::ReadState),
        Just(ConnectorCapability::StreamEvents),
        Just(ConnectorCapability::FilesystemRead),
        Just(ConnectorCapability::FilesystemWrite),
        Just(ConnectorCapability::NetworkEgress),
        Just(ConnectorCapability::SecretBroker),
        Just(ConnectorCapability::ProcessExec),
    ]
}

fn arb_mock_request() -> impl Strategy<Value = MockRequest> {
    (
        "[a-z]{1,8}",
        "[a-z]{1,8}",
        0u64..100_000,
        any::<bool>(),
        proptest::option::of("[a-z ]{1,20}"),
    )
        .prop_map(
            |(connector_id, action_kind, at_ms, success, failure_reason)| MockRequest {
                connector_id,
                action_kind,
                at_ms,
                success,
                failure_reason,
            },
        )
}

fn arb_mock_provider() -> impl Strategy<Value = MockProvider> {
    (
        "[a-z]{1,10}",
        any::<bool>(),
        0u64..10_000,
        0u8..=100,
        0u32..1000,
        0u64..1_000_000,
        0u64..1_000_000,
        proptest::collection::vec(arb_mock_request(), 0..5),
        1usize..256,
    )
        .prop_map(
            |(
                provider_id,
                online,
                latency_ms,
                failure_rate_pct,
                rate_limit_rps,
                received,
                failed,
                log,
                max_log,
            )| {
                let failed = failed.min(received);
                MockProvider {
                    provider_id,
                    online,
                    latency_ms,
                    failure_rate_pct,
                    rate_limit_rps,
                    requests_received: received,
                    requests_failed: failed,
                    request_log: VecDeque::from(log),
                    max_log_entries: max_log,
                }
            },
        )
}

fn arb_chaos_scenario() -> impl Strategy<Value = ChaosScenario> {
    (
        "[a-z-]{1,16}",
        "[a-z ]{1,30}",
        arb_chaos_scenario_kind(),
        0u64..100_000,
        0u8..=100,
    )
        .prop_map(
            |(scenario_id, description, kind, duration_ms, intensity_pct)| ChaosScenario {
                scenario_id,
                description,
                kind,
                duration_ms,
                intensity_pct,
            },
        )
}

fn arb_sandbox_escape_attempt() -> impl Strategy<Value = SandboxEscapeAttempt> {
    (
        arb_connector_capability(),
        "[a-z/]{1,20}",
        any::<bool>(),
        "[a-z ]{1,30}",
    )
        .prop_map(
            |(capability, target, expected_blocked, description)| SandboxEscapeAttempt {
                capability,
                target,
                expected_blocked,
                description,
            },
        )
}

fn arb_sandbox_escape_result() -> impl Strategy<Value = SandboxEscapeResult> {
    (arb_sandbox_escape_attempt(), any::<bool>(), any::<bool>()).prop_map(
        |(attempt, was_blocked, passed)| SandboxEscapeResult {
            attempt,
            was_blocked,
            passed,
        },
    )
}

fn arb_sandbox_probe_report() -> impl Strategy<Value = SandboxProbeReport> {
    proptest::collection::vec(arb_sandbox_escape_result(), 0..5).prop_map(|results| {
        let total_attempts = results.len();
        let all_passed = results.iter().all(|r| r.passed);
        SandboxProbeReport {
            total_attempts,
            all_passed,
            results,
        }
    })
}

fn arb_ingestion_flood_report() -> impl Strategy<Value = IngestionFloodReport> {
    (0u64..10_000, any::<bool>()).prop_map(|(total, valid)| {
        let recorded = total / 2;
        let filtered = total / 4;
        let rejected = total - recorded - filtered;
        IngestionFloodReport {
            total_events: total,
            recorded,
            filtered,
            rejected,
            chain_integrity_valid: valid,
        }
    })
}

fn arb_testbed_telemetry() -> impl Strategy<Value = TestbedTelemetry> {
    (
        0u64..10_000,
        0u64..10_000,
        0u64..10_000,
        0u64..10_000,
        0u64..10_000,
        0u64..10_000,
        0u64..10_000,
        0u64..10_000,
        0u64..10_000,
        0u64..10_000,
        0u64..10_000,
    )
        .prop_map(
            |(
                scenarios_run,
                scenarios_passed,
                scenarios_failed,
                escape_attempts,
                escapes_blocked,
                escapes_allowed,
                provider_requests,
                provider_failures,
                events_ingested,
                governor_evaluations,
                governor_rejects,
            )| {
                TestbedTelemetry {
                    scenarios_run,
                    scenarios_passed,
                    scenarios_failed,
                    escape_attempts,
                    escapes_blocked,
                    escapes_allowed,
                    provider_requests,
                    provider_failures,
                    events_ingested,
                    governor_evaluations,
                    governor_rejects,
                }
            },
        )
}

fn arb_testbed_snapshot() -> impl Strategy<Value = TestbedSnapshot> {
    (
        0u64..100_000,
        arb_testbed_telemetry(),
        0usize..100,
        0usize..100,
        0usize..100,
    )
        .prop_map(
            |(captured_at_ms, counters, provider_count, escape_result_count, audit_chain_length)| {
                TestbedSnapshot {
                    captured_at_ms,
                    counters,
                    provider_count,
                    escape_result_count,
                    audit_chain_length,
                }
            },
        )
}

// =============================================================================
// Serde roundtrip tests
// =============================================================================

proptest! {
    #[test]
    fn mock_request_serde_roundtrip(req in arb_mock_request()) {
        let json = serde_json::to_string(&req).unwrap();
        let back: MockRequest = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(req, back);
    }

    #[test]
    fn mock_provider_serde_roundtrip(provider in arb_mock_provider()) {
        let json = serde_json::to_string(&provider).unwrap();
        let back: MockProvider = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(provider, back);
    }

    #[test]
    fn chaos_scenario_kind_serde_roundtrip(kind in arb_chaos_scenario_kind()) {
        let json = serde_json::to_string(&kind).unwrap();
        let back: ChaosScenarioKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(kind, back);
    }

    #[test]
    fn chaos_scenario_serde_roundtrip(scenario in arb_chaos_scenario()) {
        let json = serde_json::to_string(&scenario).unwrap();
        let back: ChaosScenario = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(scenario, back);
    }

    #[test]
    fn sandbox_escape_attempt_serde_roundtrip(attempt in arb_sandbox_escape_attempt()) {
        let json = serde_json::to_string(&attempt).unwrap();
        let back: SandboxEscapeAttempt = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(attempt, back);
    }

    #[test]
    fn sandbox_escape_result_serde_roundtrip(result in arb_sandbox_escape_result()) {
        let json = serde_json::to_string(&result).unwrap();
        let back: SandboxEscapeResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(result, back);
    }

    #[test]
    fn sandbox_probe_report_serde_roundtrip(report in arb_sandbox_probe_report()) {
        let json = serde_json::to_string(&report).unwrap();
        let back: SandboxProbeReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(report, back);
    }

    #[test]
    fn ingestion_flood_report_serde_roundtrip(report in arb_ingestion_flood_report()) {
        let json = serde_json::to_string(&report).unwrap();
        let back: IngestionFloodReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(report, back);
    }

    #[test]
    fn testbed_telemetry_serde_roundtrip(t in arb_testbed_telemetry()) {
        let json = serde_json::to_string(&t).unwrap();
        let back: TestbedTelemetry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(t, back);
    }

    #[test]
    fn testbed_snapshot_serde_roundtrip(snap in arb_testbed_snapshot()) {
        let json = serde_json::to_string(&snap).unwrap();
        let back: TestbedSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap, back);
    }
}

// =============================================================================
// Display tests
// =============================================================================

proptest! {
    #[test]
    fn chaos_scenario_kind_display_nonempty(kind in arb_chaos_scenario_kind()) {
        let s = kind.to_string();
        prop_assert!(!s.is_empty());
    }

    #[test]
    fn mock_provider_outcome_display_all_nonempty(idx in 0u8..4) {
        let outcome = match idx {
            0 => MockProviderOutcome::Success,
            1 => MockProviderOutcome::Offline,
            2 => MockProviderOutcome::SimulatedFailure,
            _ => MockProviderOutcome::RateLimited,
        };
        prop_assert!(!outcome.to_string().is_empty());
    }
}

// =============================================================================
// Behavioral invariants
// =============================================================================

proptest! {
    #[test]
    fn mock_provider_failure_ratio_bounded(
        received in 1u64..100_000,
        failed in 0u64..100_000,
    ) {
        let failed = failed.min(received);
        let mut p = MockProvider::new("test");
        p.requests_received = received;
        p.requests_failed = failed;
        let ratio = p.failure_ratio();
        prop_assert!(ratio >= 0.0);
        prop_assert!(ratio <= 1.0);
    }

    #[test]
    fn mock_provider_successful_eq_received_minus_failed(
        received in 0u64..100_000,
        failed in 0u64..100_000,
    ) {
        let failed = failed.min(received);
        let mut p = MockProvider::new("test");
        p.requests_received = received;
        p.requests_failed = failed;
        prop_assert_eq!(p.successful_requests(), received - failed);
    }

    #[test]
    fn mock_provider_zero_requests_zero_ratio(id in "[a-z]{1,8}") {
        let p = MockProvider::new(id);
        prop_assert_eq!(p.failure_ratio(), 0.0);
    }

    #[test]
    fn mock_provider_100pct_always_fails(seed in 0u64..100) {
        let mut p = MockProvider::new("test").with_failure_rate(100);
        let outcome = p.receive_request("c", "act", 1000, seed);
        prop_assert_eq!(outcome, MockProviderOutcome::SimulatedFailure);
    }

    #[test]
    fn mock_provider_0pct_never_fails(seed in 0u64..10_000) {
        let mut p = MockProvider::new("test").with_failure_rate(0);
        let outcome = p.receive_request("c", "act", 1000, seed);
        prop_assert_eq!(outcome, MockProviderOutcome::Success);
    }

    #[test]
    fn mock_provider_offline_always_offline(seed in 0u64..10_000) {
        let mut p = MockProvider::new("test");
        p.go_offline();
        let outcome = p.receive_request("c", "act", 1000, seed);
        prop_assert_eq!(outcome, MockProviderOutcome::Offline);
    }

    #[test]
    fn mock_provider_request_log_bounded(
        max_entries in 1usize..10,
        num_requests in 0usize..30,
    ) {
        let mut p = MockProvider::new("test");
        p.max_log_entries = max_entries;
        for i in 0..num_requests {
            p.receive_request("c", "act", i as u64 * 100, 99);
        }
        prop_assert!(p.request_log.len() <= max_entries);
    }

    #[test]
    fn mock_provider_failure_rate_clamped(rate in 0u8..=255) {
        let p = MockProvider::new("test").with_failure_rate(rate);
        prop_assert!(p.failure_rate_pct <= 100);
    }

    #[test]
    fn sandbox_probe_report_all_passed_consistency(results in proptest::collection::vec(arb_sandbox_escape_result(), 0..10)) {
        let all_passed = results.iter().all(|r| r.passed);
        let report = SandboxProbeReport {
            total_attempts: results.len(),
            all_passed,
            results: results.clone(),
        };
        if report.all_passed {
            for r in &report.results {
                prop_assert!(r.passed);
            }
        }
    }

    #[test]
    fn ingestion_flood_report_total_eq_sum(
        recorded in 0u64..5000,
        filtered in 0u64..5000,
        rejected in 0u64..5000,
    ) {
        let report = IngestionFloodReport {
            total_events: recorded + filtered + rejected,
            recorded,
            filtered,
            rejected,
            chain_integrity_valid: true,
        };
        prop_assert_eq!(report.total_events, report.recorded + report.filtered + report.rejected);
    }

    #[test]
    fn standard_escape_attempts_nonempty(_dummy in 0u8..1) {
        let attempts = standard_escape_attempts();
        prop_assert!(!attempts.is_empty());
        prop_assert!(attempts.len() >= 4); // At least fs_read, fs_write, net, exec
    }

    #[test]
    fn testbed_telemetry_default_all_zero(_dummy in 0u8..1) {
        let t = TestbedTelemetry::default();
        prop_assert_eq!(t.scenarios_run, 0);
        prop_assert_eq!(t.scenarios_passed, 0);
        prop_assert_eq!(t.scenarios_failed, 0);
        prop_assert_eq!(t.escape_attempts, 0);
        prop_assert_eq!(t.escapes_blocked, 0);
        prop_assert_eq!(t.escapes_allowed, 0);
        prop_assert_eq!(t.provider_requests, 0);
        prop_assert_eq!(t.provider_failures, 0);
        prop_assert_eq!(t.events_ingested, 0);
        prop_assert_eq!(t.governor_evaluations, 0);
        prop_assert_eq!(t.governor_rejects, 0);
    }

    #[test]
    fn chaos_scenario_intensity_clamped_by_constructors(pct in 0u8..=255) {
        let s = ChaosScenario::error_storm(1000, pct);
        prop_assert!(s.intensity_pct <= 100);
    }
}
