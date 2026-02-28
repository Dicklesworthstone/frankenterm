//! Integration tests for the counterfactual engine (ft-og6q6.4.5).
//!
//! Cross-module tests covering override loading + fault injection +
//! matrix execution + guardrail enforcement.

use std::collections::BTreeMap;

use frankenterm_core::replay_counterfactual::{
    LookupResult, OverrideApplicator, OverrideManifest, OverridePackageLoader,
};
use frankenterm_core::replay_fault_injection::{FaultInjector, FaultPresets, SimEvent};
use frankenterm_core::replay_guardrails::{
    CheckResult, ConcurrencyGate, GuardrailReport, ResourceLimits, ResourceTracker, SimulationGuard,
};
use frankenterm_core::replay_scenario_matrix::{
    ArtifactEntry, DiffSummary, MatrixConfig, OverrideEntry, RunnerConfig, ScenarioMatrixRunner,
};

type DecisionGenerator =
    Box<dyn Fn(&str, Option<&str>) -> Result<Vec<String>, String> + Send + Sync>;

// ── Helpers ─────────────────────────────────────────────────────────────────

fn sample_override_toml() -> &'static str {
    r#"
[meta]
name = "test-override"
description = "Test override package"

[[pattern_overrides]]
rule_id = "error_pattern"
action = "replace"
new_definition = "modified_error_pattern"

[[pattern_overrides]]
rule_id = "warning_pattern"
action = "disable"
"#
}

fn mock_decision_generator(
    _artifact: &str,
    override_name: Option<&str>,
) -> Result<Vec<String>, String> {
    if let Some(name) = override_name {
        Ok(vec![
            format!("decision:1:override:{}", name),
            "decision:2:modified".into(),
            "decision:3:divergent".into(),
        ])
    } else {
        Ok(vec![
            "decision:1:baseline".into(),
            "decision:2:standard".into(),
            "decision:3:normal".into(),
        ])
    }
}

fn sample_matrix_config() -> MatrixConfig {
    MatrixConfig {
        artifacts: vec![
            ArtifactEntry {
                path: "traces/a.ftreplay".into(),
                label: "trace-a".into(),
            },
            ArtifactEntry {
                path: "traces/b.ftreplay".into(),
                label: "trace-b".into(),
            },
        ],
        overrides: vec![OverrideEntry {
            path: "overrides/1.toml".into(),
            label: "override-1".into(),
        }],
        config: RunnerConfig::default(),
    }
}

fn make_event(pane_id: &str, kind: &str, ts: u64, seq: u64, payload: &str) -> SimEvent {
    SimEvent {
        event_id: format!("evt-{}-{}", pane_id, seq),
        pane_id: pane_id.into(),
        event_kind: kind.into(),
        timestamp_ms: ts,
        sequence: seq,
        payload: payload.into(),
    }
}

// ── Scenario 1: Override-only (replay with rule override) ───────────────────

#[test]
fn scenario_override_only_divergence_detected() {
    let pkg = OverridePackageLoader::load(sample_override_toml()).unwrap();

    assert_eq!(pkg.override_count(), 2);
    assert!(!pkg.is_empty());

    let applicator = OverrideApplicator::new(&pkg);

    // Pattern lookup: replaced
    let result = applicator.lookup_pattern("error_pattern", None);
    let is_replace = matches!(result, LookupResult::Replace { .. });
    assert!(is_replace, "Expected Replace for error_pattern");

    // Pattern lookup: disabled
    let result = applicator.lookup_pattern("warning_pattern", None);
    let is_disabled = matches!(result, LookupResult::Disabled);
    assert!(is_disabled, "Expected Disabled for warning_pattern");

    // Unknown pattern: no override
    let result = applicator.lookup_pattern("unknown_pattern", None);
    let is_no_override = matches!(result, LookupResult::NoOverride);
    assert!(is_no_override, "Expected NoOverride for unknown");

    // Generate baseline and candidate decisions
    let baseline = mock_decision_generator("trace-a", None).unwrap();
    let candidate = mock_decision_generator("trace-a", Some("test-override")).unwrap();

    // Verify divergence
    let diff = DiffSummary::compute(&baseline, &candidate);
    assert!(!diff.is_identical());
    assert!(diff.divergence_count() > 0);
}

#[test]
fn scenario_override_manifest_captures_all() {
    let pkg = OverridePackageLoader::load(sample_override_toml()).unwrap();

    let baseline_hashes = BTreeMap::from([
        ("error_pattern".to_string(), "abc123".to_string()),
        ("warning_pattern".to_string(), "def456".to_string()),
    ]);

    let manifest = OverrideManifest::build(&pkg, &baseline_hashes);
    let json = serde_json::to_string(&manifest).unwrap();
    // Manifest should reference the package name
    assert!(json.contains("test-override"));
}

#[test]
fn scenario_override_substitution_tracking() {
    let pkg = OverridePackageLoader::load(sample_override_toml()).unwrap();
    let applicator = OverrideApplicator::new(&pkg);

    // Trigger lookups to generate substitution records
    let _ = applicator.lookup_pattern("error_pattern", Some("original_hash"));
    let _ = applicator.lookup_pattern("warning_pattern", Some("another_hash"));

    let subs = applicator.substitutions();
    assert!(
        subs.len() >= 2,
        "Should track substitutions for looked-up patterns"
    );
}

// ── Scenario 2: Fault-only (pane_death fault, graceful degradation) ─────────

#[test]
fn scenario_fault_only_pane_death_graceful() {
    let spec = FaultPresets::pane_death("pane-42", 5000, 42);
    let mut injector = FaultInjector::new(spec);

    // Process events before pane death timestamp
    let pre_death = make_event("pane-42", "output", 3000, 1, "normal output");
    let result = injector.process(pre_death);
    assert!(
        !result.is_empty(),
        "Events before death should pass through"
    );

    // Process events after pane death timestamp
    let post_death = make_event("pane-42", "output", 6000, 2, "post-death output");
    let _result = injector.process(post_death);
    let log = injector.log();
    assert!(log.count() > 0, "Fault injector should have logged actions");
}

#[test]
fn scenario_fault_only_batch_processing() {
    let spec = FaultPresets::rate_limit_storm("pane-1", 3, 42);
    let mut injector = FaultInjector::new(spec);

    let events: Vec<SimEvent> = (0..5)
        .map(|i| make_event("pane-1", "output", i * 100, i, &format!("event-{}", i)))
        .collect();

    let _results = injector.process_batch(events);
    let log = injector.into_log();
    let jsonl = log.to_jsonl();
    assert!(!jsonl.is_empty(), "Fault log should have entries");
}

#[test]
fn scenario_fault_clock_skew_effects() {
    let spec = FaultPresets::clock_skew("pane-1", 5, 500, 42);
    let mut injector = FaultInjector::new(spec);

    let events: Vec<SimEvent> = (0..10)
        .map(|i| make_event("pane-1", "output", i * 1000, i, &format!("event-{}", i)))
        .collect();

    let processed = injector.process_batch(events);
    // Clock skew may modify timestamps or inject delays
    assert!(
        !processed.is_empty(),
        "Some events should survive clock skew"
    );

    let log = injector.into_log();
    let by_type = log.count_by_type();
    // Should have logged some fault actions
    assert!(!by_type.is_empty() || log.count() > 0);
}

// ── Scenario 3: Override + Fault combined ───────────────────────────────────

#[test]
fn scenario_combined_override_and_fault() {
    // Step 1: Load overrides
    let pkg = OverridePackageLoader::load(sample_override_toml()).unwrap();
    let applicator = OverrideApplicator::new(&pkg);

    // Step 2: Create fault injector
    let fault_spec = FaultPresets::clock_skew("pane-1", 5, 500, 42);
    let mut injector = FaultInjector::new(fault_spec);

    // Step 3: Process events through fault injector
    let events: Vec<SimEvent> = (0..10)
        .map(|i| make_event("pane-1", "output", i * 1000, i, &format!("event-{}", i)))
        .collect();

    let _processed = injector.process_batch(events);

    // Step 4: Verify override applicator works in conjunction
    let overridden = applicator.lookup_pattern("error_pattern", None);
    let is_replace = matches!(overridden, LookupResult::Replace { .. });
    assert!(
        is_replace,
        "Override lookup should still work alongside fault injection"
    );

    // Step 5: Both systems can operate independently
    let _fault_log = injector.into_log();
    let all_ids = pkg.all_ids();
    assert!(
        !all_ids.is_empty(),
        "Both override and fault systems should be operational"
    );
}

// ── Scenario 4: Matrix sweep (artifacts x overrides) ────────────────────────

#[test]
fn scenario_matrix_sweep_collects_all_results() {
    let config = sample_matrix_config();
    let expected_scenarios = config.scenario_count();
    assert!(expected_scenarios > 0);

    let generator: DecisionGenerator =
        Box::new(|artifact: &str, ov: Option<&str>| mock_decision_generator(artifact, ov));

    let runner = ScenarioMatrixRunner::new(config, generator);
    let mut progress_count = 0usize;
    let result = runner.run(|_evt| progress_count += 1);

    // Verify all scenario pairs produced results
    assert_eq!(result.scenarios.len(), expected_scenarios);

    // At least some scenarios should succeed
    let passed = result.scenarios.iter().filter(|s| s.is_ok()).count();
    assert!(passed > 0, "At least some scenarios should pass");

    // Verify JSON output is valid
    let json = result.to_json();
    let json_val: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(json_val.is_object());
}

#[test]
fn scenario_matrix_baseline_identical() {
    let baseline = mock_decision_generator("trace-a", None).unwrap();
    let candidate = mock_decision_generator("trace-a", None).unwrap();
    let diff = DiffSummary::compute(&baseline, &candidate);
    assert!(
        diff.is_identical(),
        "Same generator with same args should be identical"
    );
    assert_eq!(diff.divergence_count(), 0);
}

#[test]
fn scenario_matrix_divergence_detected() {
    let baseline = mock_decision_generator("trace-a", None).unwrap();
    let candidate = mock_decision_generator("trace-a", Some("override-x")).unwrap();
    let diff = DiffSummary::compute(&baseline, &candidate);
    assert!(
        !diff.is_identical(),
        "Override should produce different decisions"
    );
    assert!(diff.divergence_count() > 0);
}

// ── Scenario 5: Guardrail enforcement ───────────────────────────────────────

#[test]
fn scenario_guardrail_resource_limit_halt() {
    let limits = ResourceLimits {
        max_events: 10,
        ..ResourceLimits::default()
    };
    let tracker = ResourceTracker::new(limits, 0);

    // Process events up to limit
    for i in 1..=10 {
        let result = tracker.record_event(i * 100);
        if i < 10 {
            let is_halt = matches!(result, CheckResult::Halt(_));
            assert!(!is_halt, "Should not halt before limit at event {}", i);
        }
    }

    // Next event should trigger halt
    let result = tracker.record_event(1100);
    let is_halt = matches!(result, CheckResult::Halt(_));
    assert!(is_halt, "Should halt after exceeding max_events");

    assert!(tracker.is_halted());
    assert!(tracker.event_count() >= 10);

    let report = GuardrailReport::from_tracker(&tracker, true);
    let report_is_safe = report.is_safe();
    assert!(!report_is_safe, "Halted tracker should not be safe");
}

#[test]
fn scenario_guardrail_concurrency_gate() {
    let gate = ConcurrencyGate::new(2);

    let _token1 = gate.try_acquire().expect("First acquire should succeed");
    let _token2 = gate.try_acquire().expect("Second acquire should succeed");

    // Third should fail
    let result = gate.try_acquire();
    assert!(result.is_err(), "Third acquire should fail at limit 2");

    assert_eq!(gate.current(), 2);
}

#[test]
fn scenario_guardrail_simulation_guard() {
    let _guard = SimulationGuard::enter();
    assert!(SimulationGuard::is_active());
}

#[test]
fn scenario_guardrail_report_json() {
    let limits = ResourceLimits::default();
    let tracker = ResourceTracker::new(limits, 0);
    let report = GuardrailReport::from_tracker(&tracker, true);
    let json = report.to_json();
    // Report should contain the key fields
    assert!(json.contains("events_processed"));
    assert!(json.contains("halted_by_guardrail"));
}

#[test]
fn scenario_guardrail_watchdog_timeout() {
    let limits = ResourceLimits {
        watchdog_timeout_ms: 1000,
        ..ResourceLimits::default()
    };
    let tracker = ResourceTracker::new(limits, 0);

    // Record an event at t=0
    tracker.record_event(0);

    // Check watchdog long after — should detect timeout
    let result = tracker.check_watchdog(2000);
    let is_halt = matches!(result, CheckResult::Halt(_));
    assert!(is_halt, "Watchdog should trigger after timeout");
}

// ── Cross-module: Matrix with guardrails ────────────────────────────────────

#[test]
fn scenario_matrix_with_guardrails() {
    let limits = ResourceLimits {
        max_events: 1000,
        ..ResourceLimits::default()
    };
    let tracker = ResourceTracker::new(limits, 0);

    let config = sample_matrix_config();
    let generator: DecisionGenerator =
        Box::new(|artifact: &str, ov: Option<&str>| mock_decision_generator(artifact, ov));

    let runner = ScenarioMatrixRunner::new(config, generator);
    let result = runner.run(|_evt| {
        tracker.record_event(100);
    });

    let is_halted = tracker.is_halted();
    assert!(
        !is_halted,
        "Guardrails should not trigger for normal matrix"
    );

    let report = GuardrailReport::from_tracker(&tracker, true);
    assert!(report.is_safe());
    assert!(!result.scenarios.is_empty());
}

#[test]
fn scenario_fault_log_jsonl_format() {
    let spec = FaultPresets::network_partition(1000, 5000, 200, 42);
    let mut injector = FaultInjector::new(spec);

    let events: Vec<SimEvent> = (0..20)
        .map(|i| make_event("pane-net", "output", i * 500, i, &format!("packet-{}", i)))
        .collect();

    let _processed = injector.process_batch(events);
    let log = injector.into_log();
    let jsonl = log.to_jsonl();

    for line in jsonl.lines() {
        if !line.is_empty() {
            let parsed: serde_json::Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("Invalid JSONL line: {} — error: {}", line, e));
            assert!(parsed.is_object());
        }
    }
}

// ── Override validation ─────────────────────────────────────────────────────

#[test]
fn scenario_override_validation_against_baseline() {
    let pkg = OverridePackageLoader::load(sample_override_toml()).unwrap();

    let known_ids = vec![
        "error_pattern".to_string(),
        "warning_pattern".to_string(),
        "info_pattern".to_string(),
    ];

    let result = OverridePackageLoader::validate_against_baseline(&pkg, &known_ids);
    assert!(result.is_ok(), "Validation should pass for known IDs");
}

#[test]
fn scenario_override_validation_unknown_id_fails() {
    let pkg = OverridePackageLoader::load(sample_override_toml()).unwrap();

    // Only "other_pattern" known — error_pattern and warning_pattern are unknown
    let known_ids: Vec<String> = vec!["other_pattern".to_string()];

    let result = OverridePackageLoader::validate_against_baseline(&pkg, &known_ids);
    assert!(result.is_err(), "Validation should fail for unknown IDs");
}

// ── Serde roundtrips ────────────────────────────────────────────────────────

#[test]
fn integration_diff_summary_serde() {
    let baseline = vec!["a".into(), "b".into(), "c".into()];
    let candidate = vec!["a".into(), "x".into(), "c".into()];
    let diff = DiffSummary::compute(&baseline, &candidate);
    let json = serde_json::to_string(&diff).unwrap();
    let restored: DiffSummary = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.divergence_count(), diff.divergence_count());
}

#[test]
fn integration_matrix_result_json() {
    let config = sample_matrix_config();
    let generator: DecisionGenerator =
        Box::new(|artifact: &str, ov: Option<&str>| mock_decision_generator(artifact, ov));
    let runner = ScenarioMatrixRunner::new(config, generator);
    let result = runner.run(|_| {});
    let json = result.to_json();
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(parsed.is_object());
}

#[test]
fn integration_guardrail_report_json() {
    let limits = ResourceLimits::default();
    let tracker = ResourceTracker::new(limits, 0);
    let report = GuardrailReport::from_tracker(&tracker, true);
    let json = report.to_json();
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(parsed.is_object());
}

// ── Failure injection: invalid override TOML ────────────────────────────────

#[test]
fn scenario_failure_invalid_override_toml() {
    let bad_toml = "this is not valid toml {{{}}}";
    let result = OverridePackageLoader::load(bad_toml);
    assert!(result.is_err(), "Invalid TOML should produce error");
}

// ── Recovery: fault injector continues after errors ─────────────────────────

#[test]
fn scenario_recovery_fault_injector_empty_batch() {
    let spec = FaultPresets::pane_death("pane-1", 1000, 42);
    let mut injector = FaultInjector::new(spec);

    // Empty batch should not panic
    let results = injector.process_batch(vec![]);
    assert!(results.is_empty());

    // Should still work after empty batch
    let event = make_event("pane-1", "output", 500, 1, "data");
    let result = injector.process(event);
    assert!(!result.is_empty());
}
