//! Cross-crate integration tests for the full asupersync runtime stack
//! (ft-e34d9.10.6.3).
//!
//! Validates end-to-end behavior across core runtime-migrated modules:
//! - runtime_telemetry → runtime_slo_gates → gate verdicts
//! - runtime_health → doctor reports → overall status
//! - cross_crate_integration → suite reports → coverage tracking
//! - runtime_performance_contract → benchmark comparison
//! - runtime_diagnostics_ux → rendering → certification
//! - asupersync_observability → health/SLO/incident integration

use frankenterm_core::asupersync_observability::{
    AsupersyncObservabilityConfig, AsupersyncTelemetrySnapshot,
};
use frankenterm_core::cross_crate_integration::{
    CrateBoundary, ScenarioCategory, SuiteReport, standard_scenarios,
};
use frankenterm_core::runtime_diagnostics_ux::{
    CertificationReport, DiagnosticCatalog, render_diagnostic,
};
use frankenterm_core::runtime_health::{
    CheckStatus, RemediationEffort, RemediationHint, RuntimeDoctorReport, RuntimeHealthCheck,
    StatusCounts,
};
use frankenterm_core::runtime_performance_contract::{
    PercentileThresholds, RuntimePerformanceContract,
};
use frankenterm_core::runtime_slo_gates::{
    GateReport, GateVerdict, RuntimeSloSample, standard_alert_policy, standard_runtime_slos,
};
use frankenterm_core::runtime_telemetry::RuntimePhase;
use frankenterm_core::runtime_telemetry::{FailureClass, HealthTier};

// =========================================================================
// 1. SLO evaluation pipeline: standard SLOs → healthy samples → Pass
// =========================================================================

#[test]
fn slo_evaluation_all_healthy_yields_pass() {
    let slos = standard_runtime_slos();
    let policy = standard_alert_policy();

    // All SLOs well within targets
    let samples: Vec<RuntimeSloSample> = slos
        .iter()
        .map(|slo| RuntimeSloSample {
            slo_id: slo.id,
            measured: slo.target * 0.5, // 50% of target = very healthy
            good_count: 100,
            total_count: 100,
        })
        .collect();

    let report = GateReport::evaluate(&slos, &samples, &policy);
    assert_eq!(report.verdict, GateVerdict::Pass);
    assert_eq!(report.breached_count, 0);
    assert_eq!(report.satisfied_count, slos.len());
}

// =========================================================================
// 2. SLO evaluation: breached non-critical → ConditionalPass
// =========================================================================

#[test]
fn slo_evaluation_non_critical_breach_conditional_pass() {
    let slos = standard_runtime_slos();
    let policy = standard_alert_policy();

    let samples: Vec<RuntimeSloSample> = slos
        .iter()
        .map(|slo| {
            let measured = if slo.critical {
                slo.target * 0.5 // critical SLOs pass
            } else {
                slo.target * 5.0 // non-critical SLOs fail badly
            };
            RuntimeSloSample {
                slo_id: slo.id,
                measured,
                good_count: 100,
                total_count: 100,
            }
        })
        .collect();

    let report = GateReport::evaluate(&slos, &samples, &policy);
    // Should be ConditionalPass (non-critical breach) or Pass if all non-critical happen to satisfy
    assert_ne!(
        report.verdict,
        GateVerdict::Fail,
        "only non-critical breaches should not Fail"
    );
}

// =========================================================================
// 3. SLO evaluation: critical breach → Fail
// =========================================================================

#[test]
fn slo_evaluation_critical_breach_yields_fail() {
    let slos = standard_runtime_slos();
    let policy = standard_alert_policy();

    // Find a critical SLO and breach it
    let critical_slo = slos
        .iter()
        .find(|s| s.critical)
        .expect("should have critical SLO");

    let samples = vec![RuntimeSloSample {
        slo_id: critical_slo.id,
        measured: critical_slo.target * 100.0, // massively over target
        good_count: 0,
        total_count: 100,
    }];

    let report = GateReport::evaluate(&slos, &samples, &policy);
    assert_eq!(report.verdict, GateVerdict::Fail);
    assert!(report.critical_breached > 0);
}

// =========================================================================
// 4. SLO report serde roundtrip preserves verdict
// =========================================================================

#[test]
fn slo_report_serde_preserves_verdict() {
    let slos = standard_runtime_slos();
    let policy = standard_alert_policy();

    let samples: Vec<RuntimeSloSample> = slos
        .iter()
        .map(|slo| RuntimeSloSample {
            slo_id: slo.id,
            measured: slo.target * 0.5,
            good_count: 100,
            total_count: 100,
        })
        .collect();

    let report = GateReport::evaluate(&slos, &samples, &policy);
    let json = serde_json::to_string(&report).unwrap();
    let restored: GateReport = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.verdict, report.verdict);
    assert_eq!(restored.total_slos, report.total_slos);
    assert_eq!(restored.satisfied_count, report.satisfied_count);
}

// =========================================================================
// 5. Health checks → doctor report pipeline
// =========================================================================

#[test]
fn health_checks_produce_valid_doctor_report() {
    let checks = vec![
        RuntimeHealthCheck {
            check_id: "scope-pressure".into(),
            display_name: "Scope Tree Pressure".into(),
            status: CheckStatus::Pass,
            tier: HealthTier::Green,
            summary: "Active scopes within budget".into(),
            evidence: vec![],
            remediation: vec![],
            failure_class: None,
            duration_us: 50,
        },
        RuntimeHealthCheck {
            check_id: "task-leak".into(),
            display_name: "Task Leak Rate".into(),
            status: CheckStatus::Warn,
            tier: HealthTier::Yellow,
            summary: "Leak rate at 0.04%".into(),
            evidence: vec!["tasks_leaked=2, tasks_spawned=5000".into()],
            remediation: vec![RemediationHint {
                description: "Check for spawned tasks missing scope cleanup".into(),
                command: Some("ft doctor --verbose tasks".into()),
                doc_link: None,
                effort: RemediationEffort::Medium,
            }],
            failure_class: None,
            duration_us: 50,
        },
    ];

    let report = RuntimeDoctorReport {
        timestamp_ms: 0,
        overall_tier: HealthTier::Yellow,
        phase: RuntimePhase::Running,
        checks: checks.clone(),
        status_counts: StatusCounts {
            pass: 1,
            warn: 1,
            fail: 0,
            skip: 0,
        },
        total_duration_us: 100,
        telemetry_snapshot: None,
    };

    assert_eq!(report.checks.len(), 2);
    assert_eq!(report.overall_tier, HealthTier::Yellow);

    // Serde roundtrip
    let json = serde_json::to_string(&report).unwrap();
    let restored: RuntimeDoctorReport = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.checks.len(), 2);
    assert_eq!(restored.overall_tier, HealthTier::Yellow);
}

// =========================================================================
// 6. Cross-crate scenarios → suite report coverage
// =========================================================================

#[test]
fn standard_scenarios_cover_all_boundaries() {
    let scenarios = standard_scenarios();
    assert!(!scenarios.is_empty(), "should have standard scenarios");

    // Collect all boundaries exercised
    let all_boundaries: Vec<&CrateBoundary> = scenarios
        .iter()
        .flat_map(|s| &s.boundaries_exercised)
        .collect();

    // Should cover core→vendored at minimum
    let has_core_to_vendored = all_boundaries
        .iter()
        .any(|b| matches!(b, CrateBoundary::CoreToVendored));
    assert!(has_core_to_vendored, "should cover CoreToVendored boundary");
}

#[test]
fn standard_scenarios_cover_multiple_categories() {
    let scenarios = standard_scenarios();
    let categories: std::collections::HashSet<&ScenarioCategory> =
        scenarios.iter().map(|s| &s.category).collect();
    assert!(
        categories.len() >= 3,
        "should cover at least 3 scenario categories, got {}",
        categories.len()
    );
}

// =========================================================================
// 7. Suite report from scenarios
// =========================================================================

#[test]
fn suite_report_from_standard_scenarios() {
    let scenarios = standard_scenarios();
    let report = SuiteReport::from_scenarios(&scenarios);

    assert_eq!(report.total_scenarios, scenarios.len());
    assert!(!report.boundaries_covered.is_empty());
    assert!(!report.categories_covered.is_empty());

    let json = serde_json::to_string(&report).unwrap();
    let restored: SuiteReport = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.total_scenarios, report.total_scenarios);
}

// =========================================================================
// 8. Performance contract: standard operations exist
// =========================================================================

#[test]
fn standard_performance_contract_has_operations() {
    let contract = RuntimePerformanceContract::standard();
    assert!(
        contract.operations.len() >= 10,
        "standard contract should have at least 10 operations, got {}",
        contract.operations.len()
    );
}

#[test]
fn standard_contract_has_critical_operations() {
    let contract = RuntimePerformanceContract::standard();
    let critical = contract.critical_operations();
    assert!(!critical.is_empty(), "should have critical operations");
}

#[test]
fn standard_contract_covers_main_categories() {
    let contract = RuntimePerformanceContract::standard();
    let by_cat = contract.by_category();
    assert!(by_cat.contains_key("cli"), "should have CLI operations");
    assert!(by_cat.contains_key("robot"), "should have robot operations");
}

// =========================================================================
// 9. Performance threshold comparison
// =========================================================================

#[test]
fn percentile_thresholds_satisfied_by_faster() {
    let target = PercentileThresholds::new(50.0, 100.0, 200.0);
    let actual = PercentileThresholds::new(25.0, 50.0, 100.0);
    assert!(
        target.satisfied_by(&actual),
        "faster actuals should satisfy target"
    );
}

#[test]
fn percentile_thresholds_not_satisfied_by_slower() {
    let target = PercentileThresholds::new(50.0, 100.0, 200.0);
    let actual = PercentileThresholds::new(100.0, 200.0, 400.0);
    assert!(
        !target.satisfied_by(&actual),
        "slower actuals should not satisfy target"
    );
}

#[test]
fn percentile_headroom_calculated() {
    let target = PercentileThresholds::new(50.0, 100.0, 200.0);
    let actual = PercentileThresholds::new(25.0, 60.0, 150.0);
    let headroom = target.headroom(&actual);
    assert!(headroom.p50_ms > 0.0);
    assert!(headroom.p95_ms > 0.0);
    assert!(headroom.p99_ms > 0.0);
}

// =========================================================================
// 10. Diagnostics: catalog → render → certification pipeline
// =========================================================================

#[test]
fn diagnostic_catalog_certification_pipeline() {
    let catalog = DiagnosticCatalog::standard();
    let report = CertificationReport::certify(&catalog);

    assert!(
        report.overall_pass,
        "standard catalog should pass certification"
    );
    assert_eq!(
        report.pass_count, 10,
        "should certify all 10 failure classes"
    );
    assert!(report.missing_classes.is_empty());

    // Serde roundtrip the certification report
    let json = serde_json::to_string(&report).unwrap();
    let restored: CertificationReport = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.overall_pass, report.overall_pass);
}

// =========================================================================
// 11. Diagnostics: rendering for each failure class
// =========================================================================

#[test]
fn all_failure_classes_render_valid_diagnostics() {
    let catalog = DiagnosticCatalog::standard();
    let classes = [
        FailureClass::Transient,
        FailureClass::Permanent,
        FailureClass::Degraded,
        FailureClass::Overload,
        FailureClass::Corruption,
        FailureClass::Timeout,
        FailureClass::Panic,
        FailureClass::Deadlock,
        FailureClass::Safety,
        FailureClass::Configuration,
    ];

    for fc in &classes {
        let template = catalog.lookup(fc).unwrap_or_else(|| {
            panic!("missing template for {:?}", fc);
        });
        let rendered = render_diagnostic(template);

        // Human text must contain the error code
        assert!(
            rendered.human_text.contains(&template.error_code),
            "{:?} human text should contain error code {}",
            fc,
            template.error_code
        );

        // Robot JSON must parse
        let parsed: serde_json::Value =
            serde_json::from_str(&rendered.robot_json).unwrap_or_else(|e| {
                panic!("{:?} robot JSON should parse: {}", fc, e);
            });
        assert_eq!(
            parsed["error_code"].as_str().unwrap(),
            template.error_code.as_str()
        );
    }
}

// =========================================================================
// 12. Telemetry snapshot → health tier derivation
// =========================================================================

#[test]
fn healthy_snapshot_yields_green_tier() {
    let config = AsupersyncObservabilityConfig::default();
    let snap = AsupersyncTelemetrySnapshot {
        scopes_created: 100,
        scopes_destroyed: 90,
        scope_max_depth: 5,
        scope_max_active: 10,
        tasks_spawned: 1000,
        tasks_completed: 998,
        tasks_cancelled: 2,
        tasks_leaked: 0,
        tasks_panicked: 0,
        cancel_requests: 10,
        cancel_completions: 10,
        cancel_latency_sum_us: 5000,
        cancel_latency_max_us: 1000,
        cancel_grace_expirations: 0,
        channel_sends: 500,
        channel_recvs: 500,
        channel_send_failures: 0,
        channel_max_depth: 10,
        lock_acquisitions: 1000,
        lock_contentions: 5,
        lock_timeout_failures: 0,
        permit_acquisitions: 100,
        permit_timeouts: 0,
        permit_max_wait_us: 500,
        recovery_attempts: 0,
        recovery_successes: 0,
        recovery_failures: 0,
        recovery_latency_max_ms: 0,
        health_samples: 100,
        health_green_samples: 100,
        health_yellow_samples: 0,
        health_red_samples: 0,
        health_black_samples: 0,
        gate_evaluations: 10,
        gate_passes: 10,
        gate_conditional_passes: 0,
        gate_failures: 0,
    };

    let tier = snap.overall_health_tier(&config);
    assert_eq!(tier, HealthTier::Green);
}

#[test]
fn panicked_tasks_yield_red_tier() {
    let config = AsupersyncObservabilityConfig::default();
    let snap = AsupersyncTelemetrySnapshot {
        scopes_created: 10,
        scopes_destroyed: 10,
        scope_max_depth: 2,
        scope_max_active: 5,
        tasks_spawned: 100,
        tasks_completed: 99,
        tasks_cancelled: 0,
        tasks_leaked: 0,
        tasks_panicked: 1, // 1 panic → Red
        cancel_requests: 0,
        cancel_completions: 0,
        cancel_latency_sum_us: 0,
        cancel_latency_max_us: 0,
        cancel_grace_expirations: 0,
        channel_sends: 0,
        channel_recvs: 0,
        channel_send_failures: 0,
        channel_max_depth: 0,
        lock_acquisitions: 0,
        lock_contentions: 0,
        lock_timeout_failures: 0,
        permit_acquisitions: 0,
        permit_timeouts: 0,
        permit_max_wait_us: 0,
        recovery_attempts: 0,
        recovery_successes: 0,
        recovery_failures: 0,
        recovery_latency_max_ms: 0,
        health_samples: 0,
        health_green_samples: 0,
        health_yellow_samples: 0,
        health_red_samples: 0,
        health_black_samples: 0,
        gate_evaluations: 0,
        gate_passes: 0,
        gate_conditional_passes: 0,
        gate_failures: 0,
    };

    let tier = snap.overall_health_tier(&config);
    assert!(
        tier >= HealthTier::Red,
        "panicked tasks should yield at least Red, got {:?}",
        tier
    );
}

// =========================================================================
// 13. Recovery failures → Black tier
// =========================================================================

#[test]
fn recovery_failures_yield_black_tier() {
    let config = AsupersyncObservabilityConfig::default();
    let snap = AsupersyncTelemetrySnapshot {
        scopes_created: 10,
        scopes_destroyed: 10,
        scope_max_depth: 2,
        scope_max_active: 5,
        tasks_spawned: 100,
        tasks_completed: 100,
        tasks_cancelled: 0,
        tasks_leaked: 0,
        tasks_panicked: 0,
        cancel_requests: 0,
        cancel_completions: 0,
        cancel_latency_sum_us: 0,
        cancel_latency_max_us: 0,
        cancel_grace_expirations: 0,
        channel_sends: 0,
        channel_recvs: 0,
        channel_send_failures: 0,
        channel_max_depth: 0,
        lock_acquisitions: 0,
        lock_contentions: 0,
        lock_timeout_failures: 0,
        permit_acquisitions: 0,
        permit_timeouts: 0,
        permit_max_wait_us: 0,
        recovery_attempts: 10,
        recovery_successes: 3, // < 50% success
        recovery_failures: 7,
        recovery_latency_max_ms: 5000,
        health_samples: 0,
        health_green_samples: 0,
        health_yellow_samples: 0,
        health_red_samples: 0,
        health_black_samples: 0,
        gate_evaluations: 0,
        gate_passes: 0,
        gate_conditional_passes: 0,
        gate_failures: 0,
    };

    let tier = snap.overall_health_tier(&config);
    assert_eq!(
        tier,
        HealthTier::Black,
        "majority recovery failures should yield Black"
    );
}

// =========================================================================
// 14. Telemetry → SLO sample → gate verdict pipeline
// =========================================================================

#[test]
fn telemetry_to_slo_to_gate_pipeline() {
    let slos = standard_runtime_slos();
    let policy = standard_alert_policy();

    // Build SLO samples from a healthy telemetry snapshot
    let config = AsupersyncObservabilityConfig::default();
    let snap = AsupersyncTelemetrySnapshot {
        scopes_created: 50,
        scopes_destroyed: 40,
        scope_max_depth: 4,
        scope_max_active: 10,
        tasks_spawned: 5000,
        tasks_completed: 4990,
        tasks_cancelled: 10,
        tasks_leaked: 0,
        tasks_panicked: 0,
        cancel_requests: 50,
        cancel_completions: 50,
        cancel_latency_sum_us: 100_000, // avg = 2000us = 2ms
        cancel_latency_max_us: 10_000,  // max = 10ms (well under 50ms)
        cancel_grace_expirations: 0,
        channel_sends: 1000,
        channel_recvs: 1000,
        channel_send_failures: 0,
        channel_max_depth: 50,
        lock_acquisitions: 10000,
        lock_contentions: 50, // 0.5% contention
        lock_timeout_failures: 0,
        permit_acquisitions: 500,
        permit_timeouts: 0,
        permit_max_wait_us: 1000,
        recovery_attempts: 0,
        recovery_successes: 0,
        recovery_failures: 0,
        recovery_latency_max_ms: 0,
        health_samples: 100,
        health_green_samples: 100,
        health_yellow_samples: 0,
        health_red_samples: 0,
        health_black_samples: 0,
        gate_evaluations: 5,
        gate_passes: 5,
        gate_conditional_passes: 0,
        gate_failures: 0,
    };

    // Verify health tier is Green
    assert_eq!(snap.overall_health_tier(&config), HealthTier::Green);

    // Create samples matching SLO IDs from the snapshot metrics
    let samples: Vec<RuntimeSloSample> = slos
        .iter()
        .map(|slo| {
            // Map each SLO to a measurement from the snapshot
            let measured = match slo.id {
                frankenterm_core::runtime_slo_gates::RuntimeSloId::CancellationLatency => {
                    snap.cancel_latency_max_us as f64 / 1000.0 // us → ms
                }
                frankenterm_core::runtime_slo_gates::RuntimeSloId::QueueBacklogDepth => {
                    snap.channel_max_depth as f64
                }
                frankenterm_core::runtime_slo_gates::RuntimeSloId::TaskLeakRate => {
                    snap.task_leak_ratio()
                }
                _ => slo.target * 0.5, // other SLOs at 50% of target
            };
            RuntimeSloSample {
                slo_id: slo.id,
                measured,
                good_count: 100,
                total_count: 100,
            }
        })
        .collect();

    let report = GateReport::evaluate(&slos, &samples, &policy);

    // With healthy snapshot, gate should pass
    assert_eq!(
        report.verdict,
        GateVerdict::Pass,
        "healthy snapshot should yield Pass verdict"
    );

    // Verify report is serializable
    let json = serde_json::to_string(&report).unwrap();
    assert!(!json.is_empty());
}

// =========================================================================
// 15. Suite report coverage gap detection
// =========================================================================

#[test]
fn suite_report_detects_coverage_gaps() {
    let scenarios = standard_scenarios();
    let report = SuiteReport::from_scenarios(&scenarios);
    let gaps = report.coverage_gaps();

    // Standard scenarios should be well-covered, but verify the gap detector works
    // by checking the total coverage
    let total_categories = [
        ScenarioCategory::UserCli,
        ScenarioCategory::RobotMode,
        ScenarioCategory::WatchPipeline,
        ScenarioCategory::SearchStack,
        ScenarioCategory::SessionLifecycle,
        ScenarioCategory::DegradedPath,
        ScenarioCategory::CancellationPath,
        ScenarioCategory::RestartRecovery,
    ];

    let covered = report.categories_covered.len();
    let uncovered = gaps.len();
    assert_eq!(
        covered + uncovered,
        total_categories.len(),
        "covered + gaps should equal total categories"
    );
}

// =========================================================================
// 16. Performance contract serde roundtrip
// =========================================================================

#[test]
fn performance_contract_serde_roundtrip() {
    let contract = RuntimePerformanceContract::standard();
    let json = serde_json::to_string(&contract).unwrap();
    let restored: RuntimePerformanceContract = serde_json::from_str(&json).unwrap();

    assert_eq!(restored.contract_id, contract.contract_id);
    assert_eq!(restored.operations.len(), contract.operations.len());
}

// =========================================================================
// 17. Cross-module: diagnostics + SLO failure class mapping
// =========================================================================

#[test]
fn failure_class_links_diagnostics_to_slos() {
    let catalog = DiagnosticCatalog::standard();
    let slos = standard_runtime_slos();

    // Each SLO has a failure_class; verify diagnostics exist for all SLO failure classes
    let slo_classes: std::collections::HashSet<FailureClass> =
        slos.iter().map(|s| s.failure_class).collect();

    for fc in &slo_classes {
        assert!(
            catalog.lookup(fc).is_some(),
            "diagnostic catalog should have template for SLO failure class {:?}",
            fc
        );
    }
}

// =========================================================================
// 18. Cross-module: alert policy covers SLO failure classes
// =========================================================================

#[test]
fn alert_policy_covers_slo_failure_classes() {
    let slos = standard_runtime_slos();
    let policy = standard_alert_policy();

    for slo in &slos {
        let escalation = policy.escalation_for(&slo.failure_class);
        assert!(
            escalation.is_some(),
            "alert policy should cover SLO failure class {:?} (SLO: {})",
            slo.failure_class,
            slo.id.as_str()
        );
    }
}
