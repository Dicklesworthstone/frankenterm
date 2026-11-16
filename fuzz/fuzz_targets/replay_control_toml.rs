#![no_main]

use frankenterm_core_replay::replay_artifact_registry::ArtifactManifest;
use frankenterm_core_replay::replay_fault_injection::FaultSpec;
use frankenterm_core_replay::replay_guardrails::ResourceLimits;
use frankenterm_core_replay::replay_guardrails_gate::RegressionBudget;
use frankenterm_core_replay::replay_risk_scoring::SeverityConfig;
use frankenterm_core_replay::replay_scenario_matrix::MatrixConfig;
use libfuzzer_sys::fuzz_target;

const MAX_DATA_BYTES: usize = 16 * 1024;
const MAX_TEXT_CHARS: usize = 8 * 1024;
const MAX_SERIALIZE_ARTIFACTS: usize = 256;
const MAX_SCENARIO_PAIRS: usize = 2_048;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_DATA_BYTES {
        return;
    }

    let text = limited_lossy(data);
    exercise_resource_limits(&text);
    exercise_regression_budget(&text);
    exercise_severity_config(&text);
    exercise_artifact_manifest(&text);
    exercise_fault_spec(&text);
    exercise_matrix_config(&text);
});

fn exercise_resource_limits(text: &str) {
    match ResourceLimits::from_toml(text) {
        Ok(limits) => {
            assert!(
                limits.validate().is_ok(),
                "ResourceLimits::from_toml returned invalid limits: {limits:?}"
            );
            assert!(limits.max_events > 0);
            assert!(limits.max_wall_clock_ms > 0);
            assert!(limits.memory_warning_events > 0);
            assert!(limits.max_concurrent > 0);
            assert!(limits.watchdog_timeout_ms > 0);
        }
        Err(error) => assert_parse_error("ResourceLimits", &error),
    }
}

fn exercise_regression_budget(text: &str) {
    match RegressionBudget::from_toml(text) {
        Ok(budget) => {
            assert!(
                budget.validate().is_ok(),
                "RegressionBudget::from_toml returned invalid budget"
            );
            assert!(budget.skip_budget_percent.is_finite());
            assert!((0.0..=100.0).contains(&budget.skip_budget_percent));
            assert!(budget.time_budget_ms > 0);
        }
        Err(error) => assert_parse_error("RegressionBudget", &error),
    }
}

fn exercise_severity_config(text: &str) {
    match SeverityConfig::from_toml(text) {
        Ok(config) => {
            let _ = config.lookup(None, "fuzz.rule");
            let _ = config.lookup(None, "rate_limit_reached");
            assert!(config.rules.len() <= MAX_DATA_BYTES);
        }
        Err(error) => assert_parse_error("SeverityConfig", &error),
    }
}

fn exercise_artifact_manifest(text: &str) {
    match ArtifactManifest::from_toml(text) {
        Ok(manifest) => {
            let _validation_errors = manifest.validate();
            let _active = manifest.active_artifacts();
            let _retired = manifest.retired_artifacts();
            for artifact in manifest.artifacts.iter().take(16) {
                let _ = manifest.find(&artifact.path);
            }
            if manifest.artifacts.len() <= MAX_SERIALIZE_ARTIFACTS {
                let serialized = manifest
                    .to_toml()
                    .expect("ArtifactManifest parsed from TOML should serialize");
                assert!(!serialized.is_empty());
            }
        }
        Err(error) => assert_parse_error("ArtifactManifest", &error),
    }
}

fn exercise_fault_spec(text: &str) {
    match FaultSpec::from_toml(text) {
        Ok(spec) => {
            assert!(
                spec.validate().is_ok(),
                "FaultSpec::from_toml returned invalid spec"
            );
            assert!(!spec.name.trim().is_empty());
            assert_eq!(spec.fault_count(), spec.faults.len());
            for fault in &spec.faults {
                assert!(!fault.type_name().is_empty());
            }
        }
        Err(error) => assert_parse_error("FaultSpec", &error),
    }
}

fn exercise_matrix_config(text: &str) {
    match MatrixConfig::from_toml(text) {
        Ok(config) => {
            let expected = if config.overrides.is_empty() {
                config.artifacts.len()
            } else {
                config.artifacts.len() * config.overrides.len()
            };
            let scenario_count = config.scenario_count();
            assert_eq!(scenario_count, expected);
            if scenario_count <= MAX_SCENARIO_PAIRS {
                assert_eq!(config.scenario_pairs().len(), scenario_count);
            }
        }
        Err(error) => assert_parse_error("MatrixConfig", &error),
    }
}

fn assert_parse_error(parser: &str, error: &str) {
    assert!(
        !error.trim().is_empty(),
        "{parser} returned an empty parse error"
    );
    assert!(
        error.len() <= MAX_DATA_BYTES * 8,
        "{parser} returned an unexpectedly large parse error: {} bytes",
        error.len()
    );
}

fn limited_lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .take(MAX_TEXT_CHARS)
        .collect()
}
