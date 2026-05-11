//! Regression guard for ft-5eqd4: README policy subsystem claims must match
//! the runtime diagnostics enumeration.

use std::env;
use std::path::PathBuf;
use std::process::Command;

use frankenterm_core::policy::PolicyEngine;
use frankenterm_core::policy_diagnostics::{
    POLICY_DIAGNOSTIC_CHECK_IDS, POLICY_SUBSYSTEM_COUNT, check_policy_engine_health,
    check_policy_engine_health_with_injected_fault,
};
use frankenterm_core::runtime_health::{CheckStatus, RuntimeHealthCheck};

const NOW_MS: u64 = 1_700_000_000_000;

mod policy_subsystem_count {
    use std::path::Path;

    pub const README_POLICY_FRAMEWORK_PATTERN: &str = r"(\d+)-subsystem policy framework";

    pub fn readme_extract(
        readme_path: &Path,
        readme: &str,
        expected_count: usize,
    ) -> Result<usize, String> {
        let needle = format!("{expected_count}-subsystem policy framework");
        if !readme.contains(&needle) {
            return Err(format!(
                "{} does not advertise '{needle}'; POLICY_SUBSYSTEM_COUNT = {expected_count}. Fix the README claim or update the diagnostics enumeration.",
                readme_path.display(),
            ));
        }

        let regex = regex::Regex::new(README_POLICY_FRAMEWORK_PATTERN).unwrap();
        let mut matches = 0_usize;
        for cap in regex.captures_iter(readme) {
            matches += 1;
            let n = cap[1].parse::<usize>().unwrap();
            if n != expected_count {
                return Err(format!(
                    "{} cites {n} policy subsystems but runtime count is {expected_count}",
                    readme_path.display(),
                ));
            }
        }

        if matches == 0 {
            return Err(format!(
                "{} must contain the policy framework subsystem headline",
                readme_path.display(),
            ));
        }

        Ok(matches)
    }
}

fn readme_path() -> PathBuf {
    env::var_os("FT_5EQD4_README_PATH").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../README.md"),
        PathBuf::from,
    )
}

fn expected_policy_subsystem_count() -> usize {
    env::var("FT_5EQD4_MUTATED_POLICY_SUBSYSTEM_COUNT").map_or(POLICY_SUBSYSTEM_COUNT, |value| {
        value
            .parse::<usize>()
            .expect("FT_5EQD4_MUTATED_POLICY_SUBSYSTEM_COUNT must be a usize")
    })
}

fn read_readme() -> (PathBuf, String) {
    let readme_path = readme_path();
    let mut readme = std::fs::read_to_string(&readme_path).unwrap_or_else(|error| {
        panic!(
            "read README policy count source {}: {error}",
            readme_path.display()
        )
    });
    if let Some(mutated_count) = env::var_os("FT_5EQD4_MUTATE_README_COUNT_TO") {
        let mutated_count = mutated_count
            .into_string()
            .expect("FT_5EQD4_MUTATE_README_COUNT_TO must be UTF-8")
            .parse::<usize>()
            .expect("FT_5EQD4_MUTATE_README_COUNT_TO must be a usize");
        let original = format!("{POLICY_SUBSYSTEM_COUNT}-subsystem policy framework");
        let mutated = format!("{mutated_count}-subsystem policy framework");
        let mutated_readme = readme.replacen(&original, &mutated, 1);
        assert_ne!(
            mutated_readme, readme,
            "README mutation proof could not replace the pinned policy count",
        );
        readme = mutated_readme;
    }
    (readme_path, readme)
}

fn validate_policy_subsystem_count(
    readme_path: &PathBuf,
    readme: &str,
    expected_count: usize,
) -> Result<usize, String> {
    policy_subsystem_count::readme_extract(readme_path, readme, expected_count)
}

fn assert_pinned_count_and_order(checks: &[RuntimeHealthCheck]) {
    assert_eq!(
        POLICY_DIAGNOSTIC_CHECK_IDS.len(),
        POLICY_SUBSYSTEM_COUNT,
        "policy diagnostics id table must stay pinned to POLICY_SUBSYSTEM_COUNT",
    );
    assert_eq!(
        checks.len(),
        POLICY_SUBSYSTEM_COUNT,
        "policy_diagnostics enumeration must equal POLICY_SUBSYSTEM_COUNT",
    );
    for (check, expected_id) in checks.iter().zip(POLICY_DIAGNOSTIC_CHECK_IDS) {
        assert_eq!(
            check.check_id, expected_id,
            "policy diagnostics check order drifted at {}",
            expected_id,
        );
    }
}

fn run_count_guard_child(configure: impl FnOnce(&mut Command)) -> (i32, String) {
    let mut command = Command::new(env::current_exe().expect("current test binary path"));
    command
        .arg("--exact")
        .arg("readme_policy_subsystem_count_matches_runtime")
        .arg("--nocapture")
        .env_remove("FT_5EQD4_MUTATE_README_COUNT_TO")
        .env_remove("FT_5EQD4_MUTATED_POLICY_SUBSYSTEM_COUNT");
    configure(&mut command);

    let output = command
        .output()
        .expect("spawn policy count guard child test");
    let status_code = output.status.code().unwrap_or(1);
    let output_text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        !output.status.success(),
        "policy count guard child test unexpectedly passed:\n{output_text}",
    );
    assert!(
        output_text.contains("does not advertise") || output_text.contains("runtime count is"),
        "policy count guard child failed without count-drift diagnostics:\n{output_text}",
    );
    (status_code, output_text)
}

#[test]
fn readme_policy_subsystem_count_matches_runtime() {
    let (readme_path, readme) = read_readme();
    let expected_count = expected_policy_subsystem_count();
    validate_policy_subsystem_count(&readme_path, &readme, expected_count)
        .unwrap_or_else(|error| panic!("{error}"));
}

#[test]
fn epic_convergence_reuses_readme_extract_helper() {
    let (readme_path, readme) = read_readme();
    let expected_count = expected_policy_subsystem_count();
    let readme_matches =
        policy_subsystem_count::readme_extract(&readme_path, &readme, expected_count)
            .unwrap_or_else(|error| panic!("{error}"));
    let mut engine = PolicyEngine::new(10, 100, true);
    let checks = check_policy_engine_health(&mut engine, NOW_MS);
    assert_pinned_count_and_order(&checks);
    println!(
        "epic_convergence policy_subsystem_count={expected_count} readme_matches={readme_matches} diagnostics_count={} helper=policy_subsystem_count::readme_extract",
        checks.len(),
    );
}

#[test]
fn mutation_readme_wrong_number_is_rejected() {
    let expected_count = POLICY_SUBSYSTEM_COUNT;
    let wrong_count = expected_count + 1;

    let (child_status, _) = run_count_guard_child(|command| {
        command.env("FT_5EQD4_MUTATE_README_COUNT_TO", wrong_count.to_string());
    });
    println!(
        "mutation_proof=readme_wrong_number expected_count={expected_count} wrong_count={wrong_count} child_status={child_status} result=rejected",
    );
}

#[test]
fn mutation_runtime_count_wrong_is_rejected() {
    let expected_count = POLICY_SUBSYSTEM_COUNT;
    let wrong_count = expected_count + 1;

    let (child_status, _) = run_count_guard_child(|command| {
        command.env(
            "FT_5EQD4_MUTATED_POLICY_SUBSYSTEM_COUNT",
            wrong_count.to_string(),
        );
    });
    println!(
        "mutation_proof=constant_wrong expected_count={expected_count} wrong_count={wrong_count} child_status={child_status} result=rejected",
    );
}

#[test]
fn enumerate_returns_pinned_count() {
    let mut engine = PolicyEngine::new(10, 100, true);
    let checks = check_policy_engine_health(&mut engine, NOW_MS);
    assert_pinned_count_and_order(&checks);
}

#[test]
fn policy_subsystem_count_under_fault_injection() {
    let mut cases = 0_usize;
    for faulty_check_id in POLICY_DIAGNOSTIC_CHECK_IDS {
        cases += 1;
        let mut engine = PolicyEngine::new(10, 100, true);
        let checks =
            check_policy_engine_health_with_injected_fault(&mut engine, NOW_MS, faulty_check_id);
        assert_pinned_count_and_order(&checks);

        let mut found_fault = false;
        for check in &checks {
            if check.check_id == faulty_check_id {
                found_fault = true;
                assert!(
                    matches!(check.status, CheckStatus::Warn | CheckStatus::Fail),
                    "faulted subsystem {} must report degraded health, got {:?}",
                    check.check_id,
                    check.status,
                );
                assert!(
                    check
                        .evidence
                        .iter()
                        .any(|line| line == "fault_injection=ft-5eqd4.4"),
                    "faulted subsystem {} must carry fault-injection evidence",
                    check.check_id,
                );
            } else {
                assert!(
                    check.status.is_healthy(),
                    "non-faulted subsystem {} must stay healthy, got {:?}: {}",
                    check.check_id,
                    check.status,
                    check.summary,
                );
            }
        }
        assert!(
            found_fault,
            "fault injection target {faulty_check_id} was not present in diagnostics output",
        );
    }
    println!(
        "fault_injection_count_invariance=passed cases={cases} policy_subsystem_count={POLICY_SUBSYSTEM_COUNT}",
    );
}
