//! Property-based tests for the chaos testing harness.
//!
//! Covers: FaultPoint serde roundtrip, FaultTrigger serde roundtrip,
//! ChaosReport serde roundtrip, FaultPoint Display properties,
//! scenario builder accumulation, probability clamping, and
//! assertion evaluation semantics.
//!
//! Note: FaultInjector fault-triggering logic is tested via inline tests
//! (check_point is private). External proptests focus on the public
//! serialization, construction, and assertion-checking APIs.

use proptest::prelude::*;

use frankenterm_core::chaos::{
    ChaosAssertion, ChaosReport, ChaosScenario, FaultInjector, FaultMode, FaultPoint, FaultTrigger,
};

// ─── Strategies ──────────────────────────────────────────────────────

fn arb_fault_point() -> impl Strategy<Value = FaultPoint> {
    prop_oneof![
        Just(FaultPoint::WeztermCliCall),
        Just(FaultPoint::DbWrite),
        Just(FaultPoint::DbRead),
        Just(FaultPoint::PatternDetect),
        Just(FaultPoint::RetentionCleanup),
        Just(FaultPoint::ConfigReload),
        Just(FaultPoint::WebhookDelivery),
    ]
}

fn arb_fault_trigger() -> impl Strategy<Value = FaultTrigger> {
    (
        arb_fault_point(),
        any::<bool>(),
        proptest::option::of("[a-z]{1,20}"),
        0u64..u64::MAX,
    )
        .prop_map(|(point, fired, error, timestamp_ms)| FaultTrigger {
            point,
            fired,
            error,
            timestamp_ms,
        })
}

fn arb_chaos_report() -> impl Strategy<Value = ChaosReport> {
    (
        "[a-z_]{1,20}",
        0usize..1000,
        0usize..1000,
        0usize..100,
        0usize..100,
        any::<bool>(),
    )
        .prop_map(
            |(
                scenario_name,
                total_checks,
                total_faults_fired,
                assertions_passed,
                assertions_failed,
                all_passed,
            )| {
                ChaosReport {
                    scenario_name,
                    total_checks,
                    total_faults_fired,
                    faults_by_point: std::collections::HashMap::new(),
                    assertions_passed,
                    assertions_failed,
                    all_passed,
                }
            },
        )
}

/// All 7 FaultPoint variants for exhaustive testing.
const ALL_FAULT_POINTS: [FaultPoint; 7] = [
    FaultPoint::WeztermCliCall,
    FaultPoint::DbWrite,
    FaultPoint::DbRead,
    FaultPoint::PatternDetect,
    FaultPoint::RetentionCleanup,
    FaultPoint::ConfigReload,
    FaultPoint::WebhookDelivery,
];

// ─── FaultPoint serde roundtrip ─────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn fault_point_json_roundtrip(point in arb_fault_point()) {
        let json = serde_json::to_string(&point).unwrap();
        let decoded: FaultPoint = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(point, decoded);
    }
}

#[test]
fn fault_point_all_variants_roundtrip() {
    for point in &ALL_FAULT_POINTS {
        let json = serde_json::to_string(point).unwrap();
        let decoded: FaultPoint = serde_json::from_str(&json).unwrap();
        assert_eq!(*point, decoded, "roundtrip failed for {point:?}");
    }
}

#[test]
fn fault_point_display_non_empty() {
    for point in &ALL_FAULT_POINTS {
        let display = point.to_string();
        assert!(
            !display.is_empty(),
            "display for {point:?} should be non-empty"
        );
    }
}

#[test]
fn fault_point_display_is_snake_case() {
    for point in &ALL_FAULT_POINTS {
        let display = point.to_string();
        assert!(
            display
                .chars()
                .all(|c| c.is_ascii_lowercase() || c == '_'),
            "display for {point:?} should be snake_case, got: {display}"
        );
    }
}

#[test]
fn fault_point_serde_format_is_snake_case() {
    for point in &ALL_FAULT_POINTS {
        let json = serde_json::to_string(point).unwrap();
        // JSON wraps in quotes: e.g., "\"db_write\""
        let inner = json.trim_matches('"');
        assert!(
            inner.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "serde format for {point:?} should be snake_case, got: {inner}"
        );
    }
}

#[test]
fn fault_point_display_matches_serde() {
    for point in &ALL_FAULT_POINTS {
        let display = point.to_string();
        let json = serde_json::to_string(point).unwrap();
        let serde_str = json.trim_matches('"');
        assert_eq!(
            display, serde_str,
            "Display and serde should produce same string for {point:?}"
        );
    }
}

// ─── FaultTrigger serde roundtrip ───────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn fault_trigger_json_roundtrip(trigger in arb_fault_trigger()) {
        let json = serde_json::to_string(&trigger).unwrap();
        let decoded: FaultTrigger = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(trigger.point, decoded.point);
        prop_assert_eq!(trigger.fired, decoded.fired);
        prop_assert_eq!(trigger.error, decoded.error);
        prop_assert_eq!(trigger.timestamp_ms, decoded.timestamp_ms);
    }

    #[test]
    fn fault_trigger_error_absent_when_none(
        point in arb_fault_point(),
        timestamp_ms in 0u64..u64::MAX,
    ) {
        let trigger = FaultTrigger {
            point,
            fired: false,
            error: None,
            timestamp_ms,
        };
        let json = serde_json::to_string(&trigger).unwrap();
        prop_assert!(
            !json.contains("\"error\""),
            "error field should be absent when None, got: {}",
            json
        );
    }

    #[test]
    fn fault_trigger_error_present_when_some(
        point in arb_fault_point(),
        error_msg in "[a-z]{1,20}",
        timestamp_ms in 0u64..u64::MAX,
    ) {
        let trigger = FaultTrigger {
            point,
            fired: true,
            error: Some(error_msg.clone()),
            timestamp_ms,
        };
        let json = serde_json::to_string(&trigger).unwrap();
        prop_assert!(
            json.contains("\"error\""),
            "error field should be present when Some, got: {}",
            json
        );
        prop_assert!(
            json.contains(&error_msg),
            "error message should appear in JSON, got: {}",
            json
        );
    }
}

// ─── ChaosReport serde roundtrip ────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn chaos_report_json_roundtrip(report in arb_chaos_report()) {
        let json = serde_json::to_string(&report).unwrap();
        let decoded: ChaosReport = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(&report.scenario_name, &decoded.scenario_name);
        prop_assert_eq!(report.total_checks, decoded.total_checks);
        prop_assert_eq!(report.total_faults_fired, decoded.total_faults_fired);
        prop_assert_eq!(report.assertions_passed, decoded.assertions_passed);
        prop_assert_eq!(report.assertions_failed, decoded.assertions_failed);
        prop_assert_eq!(report.all_passed, decoded.all_passed);
    }

    #[test]
    fn chaos_report_pretty_json_roundtrip(report in arb_chaos_report()) {
        let json = serde_json::to_string_pretty(&report).unwrap();
        let decoded: ChaosReport = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(&report.scenario_name, &decoded.scenario_name);
        prop_assert_eq!(report.total_checks, decoded.total_checks);
        prop_assert_eq!(report.total_faults_fired, decoded.total_faults_fired);
    }
}

// ─── Scenario builder ───────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn scenario_builder_accumulates_faults(
        points in prop::collection::vec(arb_fault_point(), 1..10),
    ) {
        let mut scenario = ChaosScenario::new("test", "description");

        for point in &points {
            scenario = scenario.with_fault(*point, FaultMode::always_fail("error"));
        }

        prop_assert_eq!(
            scenario.faults.len(),
            points.len(),
            "scenario should have {} faults",
            points.len()
        );
    }

    #[test]
    fn scenario_builder_accumulates_assertions(
        n_faults in 0usize..5,
        n_assertions in 0usize..5,
    ) {
        let mut scenario = ChaosScenario::new("test", "description");

        for _ in 0..n_faults {
            scenario = scenario.with_fault(FaultPoint::DbWrite, FaultMode::always_fail("err"));
        }
        for _ in 0..n_assertions {
            scenario = scenario.with_assertion(ChaosAssertion::FaultFiredAtLeast(
                FaultPoint::DbWrite,
                1,
            ));
        }

        prop_assert_eq!(scenario.faults.len(), n_faults);
        prop_assert_eq!(scenario.assertions.len(), n_assertions);
    }

    #[test]
    fn scenario_preserves_name_and_description(
        name in "[a-z_]{1,30}",
        description in "[a-z ]{1,50}",
    ) {
        let scenario = ChaosScenario::new(name.clone(), description.clone());
        prop_assert_eq!(&scenario.name, &name);
        prop_assert_eq!(&scenario.description, &description);
    }
}

// ─── FaultMode construction ─────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn fail_with_probability_clamps_to_0_1(raw in -100.0f64..100.0) {
        let _mode = FaultMode::fail_with_probability(raw, "test");
    }

    #[test]
    fn fail_n_times_accepts_any_u32(n in 0u32..u32::MAX) {
        let _mode = FaultMode::fail_n_times(n, "test");
    }

    #[test]
    fn delay_accepts_any_u64(ms in 0u64..1000) {
        let _mode = FaultMode::delay(ms);
    }
}

// ─── FaultInjector: construction and state ──────────────────────────

#[test]
fn new_injector_has_empty_state() {
    let injector = FaultInjector::new();
    assert!(injector.get_log().is_empty());
    assert_eq!(injector.total_fired(), 0);
    for point in &ALL_FAULT_POINTS {
        assert_eq!(injector.fired_count(*point), 0);
    }
}

#[test]
fn default_injector_matches_new() {
    let a = FaultInjector::new();
    let b = FaultInjector::default();
    assert_eq!(a.get_log().len(), b.get_log().len());
    assert_eq!(a.total_fired(), b.total_fired());
}

// ─── Scenario assertion evaluation (no fault triggering needed) ─────

#[test]
fn empty_scenario_has_no_assertion_results() {
    let injector = FaultInjector::new();
    let scenario = ChaosScenario::new("empty", "no assertions");
    let results = injector.check_assertions(&scenario);
    assert!(results.is_empty());
}

#[test]
fn fault_never_fired_passes_on_clean_injector() {
    let injector = FaultInjector::new();
    let scenario = ChaosScenario::new("test", "test")
        .with_assertion(ChaosAssertion::FaultNeverFired(FaultPoint::DbWrite));

    let results = injector.check_assertions(&scenario);
    assert!(
        results[0].passed,
        "FaultNeverFired should pass on clean injector"
    );
}

#[test]
fn fired_at_least_zero_always_passes() {
    let injector = FaultInjector::new();
    let scenario = ChaosScenario::new("test", "test")
        .with_assertion(ChaosAssertion::FaultFiredAtLeast(FaultPoint::DbWrite, 0));

    let results = injector.check_assertions(&scenario);
    assert!(
        results[0].passed,
        "FaultFiredAtLeast(_, 0) should always pass"
    );
}

#[test]
fn total_in_range_includes_zero_passes_on_clean() {
    let injector = FaultInjector::new();
    let scenario = ChaosScenario::new("test", "test")
        .with_assertion(ChaosAssertion::TotalFaultsInRange(0, 10));

    let results = injector.check_assertions(&scenario);
    assert!(
        results[0].passed,
        "TotalFaultsInRange(0, 10) should pass on clean injector"
    );
}

#[test]
fn total_in_range_excludes_zero_fails_on_clean() {
    let injector = FaultInjector::new();
    let scenario = ChaosScenario::new("test", "test")
        .with_assertion(ChaosAssertion::TotalFaultsInRange(1, 10));

    let results = injector.check_assertions(&scenario);
    assert!(
        !results[0].passed,
        "TotalFaultsInRange(1, 10) should fail on clean injector (0 faults)"
    );
}

// ─── ChaosReport from_scenario on clean injector ────────────────────

#[test]
fn report_on_clean_injector_shows_zeros() {
    let injector = FaultInjector::new();
    let scenario = ChaosScenario::new("test", "test");
    let report = ChaosReport::from_scenario(&injector, &scenario);

    assert_eq!(report.scenario_name, "test");
    assert_eq!(report.total_checks, 0);
    assert_eq!(report.total_faults_fired, 0);
    assert!(report.faults_by_point.is_empty());
    assert_eq!(report.assertions_passed, 0);
    assert_eq!(report.assertions_failed, 0);
    assert!(report.all_passed);
}

// ─── Pre-built scenarios: structural validation ─────────────────────

#[test]
fn prebuilt_scenarios_have_at_least_one_fault() {
    let scenarios = [
        frankenterm_core::chaos::scenarios::db_write_failure(),
        frankenterm_core::chaos::scenarios::wezterm_unavailable(),
        frankenterm_core::chaos::scenarios::pattern_engine_failure(),
        frankenterm_core::chaos::scenarios::db_corruption(),
        frankenterm_core::chaos::scenarios::maintenance_failure(),
        frankenterm_core::chaos::scenarios::cascading_failures(),
    ];

    for scenario in &scenarios {
        assert!(
            !scenario.faults.is_empty(),
            "scenario '{}' should have at least one fault",
            scenario.name
        );
        assert!(
            !scenario.assertions.is_empty(),
            "scenario '{}' should have at least one assertion",
            scenario.name
        );
    }
}

#[test]
fn prebuilt_scenarios_have_unique_names() {
    let scenarios = [
        frankenterm_core::chaos::scenarios::db_write_failure(),
        frankenterm_core::chaos::scenarios::wezterm_unavailable(),
        frankenterm_core::chaos::scenarios::pattern_engine_failure(),
        frankenterm_core::chaos::scenarios::db_corruption(),
        frankenterm_core::chaos::scenarios::maintenance_failure(),
        frankenterm_core::chaos::scenarios::cascading_failures(),
    ];

    let mut names: Vec<&str> = scenarios.iter().map(|s| s.name.as_str()).collect();
    let len_before = names.len();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), len_before, "scenario names should be unique");
}

// ─── apply_scenario clears previous state ───────────────────────────

#[test]
fn apply_scenario_clears_log() {
    let injector = FaultInjector::new();
    injector.set_fault(FaultPoint::DbRead, FaultMode::always_fail("old"));

    let scenario = ChaosScenario::new("new", "new scenario");
    injector.apply_scenario(&scenario);

    assert!(
        injector.get_log().is_empty(),
        "apply_scenario should clear the log"
    );
    assert_eq!(
        injector.total_fired(),
        0,
        "apply_scenario should reset total_fired"
    );
}

// ─── clear_all resets everything ────────────────────────────────────

#[test]
fn clear_all_resets_log_and_faults() {
    let injector = FaultInjector::new();
    injector.set_fault(FaultPoint::DbWrite, FaultMode::always_fail("err"));
    injector.set_fault(FaultPoint::DbRead, FaultMode::fail_n_times(5, "err"));

    injector.clear_all();

    assert!(injector.get_log().is_empty());
    assert_eq!(injector.total_fired(), 0);
    for point in &ALL_FAULT_POINTS {
        assert_eq!(injector.fired_count(*point), 0);
    }
}

// ─── drain_log ──────────────────────────────────────────────────────

#[test]
fn drain_log_returns_empty_on_fresh_injector() {
    let injector = FaultInjector::new();
    let drained = injector.drain_log();
    assert!(drained.is_empty());
}
