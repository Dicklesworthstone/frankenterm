use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

struct FixtureCase {
    name: &'static str,
    should_pass: bool,
    expected_kinds: &'static [&'static str],
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate lives under repo/crates/frankenterm")
        .to_path_buf()
}

fn run_validator(repo: &Path, fixture: &str) -> (bool, Value, String, String) {
    let fixture_path = repo
        .join("tests/fixtures/bead-validator-canary")
        .join(fixture);
    let output = Command::new("bash")
        .arg(repo.join("scripts/check-reality-check-bead-structure.sh"))
        .arg("--beads")
        .arg(&fixture_path)
        .arg("--epic-id")
        .arg("ft-canary")
        .arg("--strict-all")
        .arg("--json")
        .current_dir(repo)
        .output()
        .expect("run structure validator");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let payload: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|err| panic!("parse validator JSON for {fixture}: {err}\n{stdout}"));
    (output.status.success(), payload, stdout, stderr)
}

#[test]
fn reality_check_validator_canary_fixture_matrix() {
    let repo = repo_root();
    let fixtures = [
        FixtureCase {
            name: "missing_background.jsonl",
            should_pass: false,
            expected_kinds: &["missing_section"],
        },
        FixtureCase {
            name: "missing_acceptance.jsonl",
            should_pass: false,
            expected_kinds: &["missing_section"],
        },
        FixtureCase {
            name: "missing_test_companion.jsonl",
            should_pass: false,
            expected_kinds: &["missing_section"],
        },
        FixtureCase {
            name: "missing_operator_surface.jsonl",
            should_pass: false,
            expected_kinds: &["missing_section"],
        },
        FixtureCase {
            name: "missing_degradation.jsonl",
            should_pass: false,
            expected_kinds: &["missing_section"],
        },
        FixtureCase {
            name: "missing_proof_category.jsonl",
            should_pass: false,
            expected_kinds: &["missing_proof_category"],
        },
        FixtureCase {
            name: "invalid_proof_category.jsonl",
            should_pass: false,
            expected_kinds: &["unknown_proof_category"],
        },
        FixtureCase {
            name: "closed_without_audit_comment.jsonl",
            should_pass: false,
            expected_kinds: &["missing_closeout_evidence_comment"],
        },
        FixtureCase {
            name: "foreign_language_description.jsonl",
            should_pass: true,
            expected_kinds: &["parse_warning"],
        },
        FixtureCase {
            name: "degenerate_short_description.jsonl",
            should_pass: false,
            expected_kinds: &["degenerate_description"],
        },
        FixtureCase {
            name: "description_with_unicode_zero_width.jsonl",
            should_pass: true,
            expected_kinds: &[],
        },
        FixtureCase {
            name: "duplicate_section_headers.jsonl",
            should_pass: true,
            expected_kinds: &["duplicate_section_header"],
        },
        FixtureCase {
            name: "notes_null.jsonl",
            should_pass: false,
            expected_kinds: &["missing_notes", "missing_proof_category"],
        },
        FixtureCase {
            name: "notes_empty_string.jsonl",
            should_pass: false,
            expected_kinds: &["missing_notes", "missing_proof_category"],
        },
        FixtureCase {
            name: "all_sections_present_valid.jsonl",
            should_pass: true,
            expected_kinds: &[],
        },
    ];

    for fixture in fixtures {
        let (status_success, payload, stdout, stderr) = run_validator(&repo, fixture.name);
        assert_eq!(
            status_success, fixture.should_pass,
            "unexpected exit status for {}\nstdout:\n{}\nstderr:\n{}",
            fixture.name, stdout, stderr
        );
        assert_eq!(
            payload["ok"].as_bool(),
            Some(fixture.should_pass),
            "unexpected ok value for {}\n{}",
            fixture.name,
            stdout
        );
        let violations = payload["violations"]
            .as_array()
            .expect("violations array in validator payload");
        for expected_kind in fixture.expected_kinds {
            assert!(
                violations
                    .iter()
                    .any(|item| item["kind"].as_str() == Some(expected_kind)),
                "fixture {} did not emit expected violation kind {}\n{}",
                fixture.name,
                expected_kind,
                stdout
            );
        }
    }
}
