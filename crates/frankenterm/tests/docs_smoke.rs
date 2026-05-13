//! Docs smoke tests (wa-nu4.3.9.9)
//!
//! Validates that quickstart commands referenced in docs remain executable.
//! Runs in a temp environment to avoid touching real user configs.
//!
//! Artifact capture: each test emits structured artifacts via eprintln
//! for CI debugging. On failure, artifacts include stdout/stderr and
//! environment info.

use assert_cmd::Command;
use predicates::prelude::*;
use std::{fs, path::PathBuf};

/// Build a wa command configured to run in a temp workspace.
///
/// Sets FT_WORKSPACE to a temp dir so commands don't touch real state.
#[allow(deprecated)]
fn wa_cmd() -> Command {
    let mut cmd = Command::cargo_bin("ft").expect("ft binary should be built");
    // Use temp workspace to avoid touching real state
    let tmp = std::env::temp_dir().join(format!("ft_smoke_{}", std::process::id()));
    std::fs::create_dir_all(&tmp).ok();
    cmd.env("FT_WORKSPACE", tmp.to_string_lossy().to_string());
    // Prevent any real WezTerm interaction
    cmd.env("FT_WEZTERM_CLI", "/nonexistent/wezterm");
    cmd
}

/// Emit an artifact for CI debugging.
fn emit_artifact(label: &str, content: &str) {
    eprintln!("[ARTIFACT][docs-smoke] {label}:\n{content}");
}

/// Emit environment info artifact.
fn emit_env_artifact() {
    let info = format!(
        "os={}\narch={}\nrustc={}\npid={}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        option_env!("RUSTC_VERSION").unwrap_or("unknown"),
        std::process::id(),
    );
    emit_artifact("env", &info);
}

fn artifact_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ft_smoke_artifacts_{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok();
    dir
}

fn repo_file(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative)
}

fn read_repo_doc(relative: &str) -> String {
    fs::read_to_string(repo_file(relative)).unwrap_or_else(|err| panic!("read {relative}: {err}"))
}

fn assert_contains_all(label: &str, text: &str, needles: &[&str]) {
    for needle in needles {
        assert!(text.contains(needle), "{label} should contain `{needle}`");
    }
}

fn assert_excludes_all(label: &str, text: &str, needles: &[&str]) {
    for needle in needles {
        assert!(
            !text.contains(needle),
            "{label} should not contain stale or forbidden text `{needle}`"
        );
    }
}

fn save_artifact(name: &str, content: &str) {
    let dir = artifact_dir();
    let path = dir.join(name);
    std::fs::write(&path, content).ok();
}

// =============================================================================
// Quickstart command smoke tests
// =============================================================================

#[test]
fn smoke_wa_help() {
    emit_env_artifact();

    let output = wa_cmd()
        .arg("--help")
        .output()
        .expect("ft --help should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    save_artifact("help_stdout.txt", &stdout);
    save_artifact("help_stderr.txt", &stderr);
    emit_artifact("ft_help_stdout", &stdout);

    assert!(
        output.status.success(),
        "ft --help should exit 0, got: {}",
        output.status
    );
    assert!(
        stdout.contains("Usage") || stdout.contains("usage"),
        "ft --help should contain usage info"
    );
    assert!(
        stdout.contains("ft") || stdout.contains("FrankenTerm"),
        "ft --help should mention ft"
    );
}

#[test]
fn smoke_ft_version() {
    let output = wa_cmd()
        .arg("--version")
        .output()
        .expect("ft --version should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    save_artifact("version_stdout.txt", &stdout);
    emit_artifact("ft_version_stdout", &stdout);

    assert!(output.status.success(), "ft --version should exit 0");
    assert!(
        stdout.contains("ft") || stdout.contains("0."),
        "ft --version should contain version info"
    );
}

#[test]
fn smoke_ft_version_full() {
    // `ft version --full` shows detailed build metadata
    let output = wa_cmd()
        .args(["version", "--full"])
        .output()
        .expect("ft version --full should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    save_artifact("version_full_stdout.txt", &stdout);
    emit_artifact("ft_version_full", &stdout);

    // Should succeed or at least not panic
    assert!(
        output.status.success() || !stderr.contains("panicked"),
        "ft version --full should not panic"
    );
}

#[test]
fn smoke_ft_doctor_json() {
    let output = wa_cmd()
        .args(["doctor", "--json"])
        .output()
        .expect("ft doctor --json should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    save_artifact("doctor_json_stdout.txt", &stdout);
    save_artifact("doctor_json_stderr.txt", &stderr);
    emit_artifact("ft_doctor_json", &stdout);

    // Doctor may report warnings (no WezTerm running) but should not panic.
    // In JSON mode, it should produce parseable JSON regardless of pass/fail.
    assert!(
        !stderr.contains("panicked"),
        "ft doctor --json should not panic"
    );

    // If it succeeded, stdout should be valid JSON
    if output.status.success() {
        assert!(
            serde_json::from_str::<serde_json::Value>(&stdout).is_ok(),
            "ft doctor --json should produce valid JSON when successful"
        );
    }
}

#[test]
fn smoke_ft_doctor_plain() {
    let output = wa_cmd()
        .arg("doctor")
        .output()
        .expect("ft doctor should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    save_artifact("doctor_plain_stdout.txt", &stdout);
    save_artifact("doctor_plain_stderr.txt", &stderr);
    emit_artifact("ft_doctor_plain", &stdout);

    // Doctor should not panic; it may exit non-zero if WezTerm is missing
    assert!(!stderr.contains("panicked"), "ft doctor should not panic");
}

#[test]
fn smoke_ft_setup_dry_run() {
    let output = wa_cmd()
        .args(["setup", "--dry-run"])
        .output()
        .expect("ft setup --dry-run should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    save_artifact("setup_dry_run_stdout.txt", &stdout);
    save_artifact("setup_dry_run_stderr.txt", &stderr);
    emit_artifact("ft_setup_dry_run", &stdout);

    // Dry run should not panic and should not modify any files
    assert!(
        !stderr.contains("panicked"),
        "ft setup --dry-run should not panic"
    );
}

#[test]
fn smoke_ft_robot_quick_start() {
    let output = wa_cmd()
        .args(["robot", "quick-start"])
        .output()
        .expect("ft robot quick-start should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    save_artifact("robot_quickstart_stdout.txt", &stdout);
    save_artifact("robot_quickstart_stderr.txt", &stderr);
    emit_artifact("ft_robot_quickstart", &stdout);

    assert!(
        output.status.success(),
        "ft robot quick-start should exit 0, stderr: {stderr}"
    );

    // Quick-start should output structured data (JSON for robot mode)
    let parsed = serde_json::from_str::<serde_json::Value>(&stdout);
    assert!(
        parsed.is_ok(),
        "ft robot quick-start should output valid JSON"
    );
}

#[test]
fn install_docs_use_package_name_and_bin() {
    let readme = read_repo_doc("README.md");
    let remote_setup = read_repo_doc("docs/remote-setup-spec.md");

    let wrong = "cargo install --git https://github.com/Dicklesworthstone/frankenterm.git ft";
    let correct = "cargo install --git https://github.com/Dicklesworthstone/frankenterm.git --bin ft frankenterm";

    assert!(
        !readme.contains(wrong),
        "README must not advertise the binary name as the cargo package"
    );
    assert!(
        readme.contains(correct),
        "README should advertise the explicit package+bin cargo install command"
    );
    assert!(
        remote_setup.contains(correct),
        "remote setup spec should use the explicit package+bin cargo install command"
    );
}

#[test]
fn resource_pressure_cockpit_docs_truth_gate() {
    const CONFORMANCE_SUMMARY: &str = "tests/e2e/artifacts/goal-line/ft-rz0eb.4/resource_cockpit_conformance/20260513T172634Z/summary.json";
    const STALE_CONFORMANCE_SUMMARY: &str = "tests/e2e/artifacts/goal-line/ft-rz0eb.4/resource_cockpit_conformance/20260510T125418Z/summary.json";

    let readme = read_repo_doc("README.md");
    let contract = read_repo_doc("docs/resource-pressure-cockpit-contract.md");
    let high_core = read_repo_doc("docs/high-core-swarm-runbook.md");
    let operator_playbook = read_repo_doc("docs/operator-playbook.md");
    let operator_runbook = read_repo_doc("docs/operator-runbook.md");
    let release_checklist = read_repo_doc("docs/release/checklist.md");
    let provenance = read_repo_doc("docs/json-schema/PROVENANCE.md");
    let schema = read_repo_doc("docs/json-schema/ft-resource-pressure-cockpit.json");

    assert_contains_all(
        "resource cockpit contract",
        &contract,
        &[
            CONFORMANCE_SUMMARY,
            "`run_identity`",
            "`domains`",
            "`residency_buckets`",
            "`queue_backpressure`",
            "`admission_decisions`",
            "`action_receipts`",
            "`artifact_paths`",
            "`remote_reduced = \"passed\"`",
            "`target_hardware = \"skipped_not_proven\"`",
            "`sqlite_page_cache`",
            "`scrollback_cache`",
        ],
    );
    assert_excludes_all(
        "resource cockpit contract",
        &contract,
        &[
            "`sqlite_cache`",
            "`scrollback_tiers`",
            "| `allocator_pools` |",
        ],
    );

    assert_contains_all(
        "high-core runbook cockpit section",
        &high_core,
        &[
            CONFORMANCE_SUMMARY,
            "`run_identity`",
            "`domains`",
            "`residency_buckets`",
            "`queue_backpressure`",
            "`admission_decisions`",
            "`action_receipts`",
            "`artifact_paths`",
            "resource_pressure_cockpit_docs_truth_gate",
        ],
    );
    assert_excludes_all(
        "high-core runbook cockpit section",
        &high_core,
        &[
            "slowest_latency_cohorts",
            "resource_admission_decisions",
            ".resource_cockpit.memory_pressure",
            ".resource_cockpit.memory_tiers",
        ],
    );

    assert_contains_all(
        "operator memory guidance",
        &operator_playbook,
        &[
            CONFORMANCE_SUMMARY,
            "`rust_heap`",
            "`mmap_file_backed`",
            "`sqlite_page_cache`",
            "`graphics_media`",
            "`scrollback_cache`",
            "`child_processes`",
            "`unknown`",
            "`domains.rss_residency`",
            "`domains.storage_io`",
            "`domains.action_receipts`",
            "`action_receipts`",
            "`artifact_paths`",
        ],
    );

    assert_contains_all(
        "README cockpit truth",
        &readme,
        &[
            "retained remote-reduced conformance artifact",
            CONFORMANCE_SUMMARY,
            "`rust_heap`",
            "`sqlite_page_cache`",
            "`scrollback_cache`",
            "`action_receipts`",
            "`skipped_not_proven`",
        ],
    );

    assert_contains_all(
        "release and provenance docs",
        &(release_checklist.clone() + "\n" + &provenance),
        &[CONFORMANCE_SUMMARY, "target hardware", "RCH"],
    );

    let parsed_schema: serde_json::Value =
        serde_json::from_str(&schema).expect("resource cockpit schema should parse as JSON");
    let bucket_enum = parsed_schema
        .pointer("/$defs/residency_bucket/properties/bucket/enum")
        .and_then(serde_json::Value::as_array)
        .expect("schema should expose residency bucket enum");
    for bucket in [
        "rust_heap",
        "mmap_file_backed",
        "sqlite_page_cache",
        "graphics_media",
        "scrollback_cache",
        "child_processes",
        "unknown",
    ] {
        assert!(
            bucket_enum
                .iter()
                .any(|value| value.as_str() == Some(bucket)),
            "schema residency bucket enum should include {bucket}"
        );
    }

    let live_docs = [
        ("README.md", readme.as_str()),
        (
            "docs/resource-pressure-cockpit-contract.md",
            contract.as_str(),
        ),
        ("docs/high-core-swarm-runbook.md", high_core.as_str()),
        ("docs/operator-playbook.md", operator_playbook.as_str()),
        ("docs/operator-runbook.md", operator_runbook.as_str()),
        ("docs/release/checklist.md", release_checklist.as_str()),
        ("docs/json-schema/PROVENANCE.md", provenance.as_str()),
    ];
    let legacy_branch = ["mas", "ter"].concat();
    let legacy_branch_patterns = [
        format!("origin/{legacy_branch}"),
        format!("main:{legacy_branch}"),
        format!("`{legacy_branch}` branch"),
        format!("branch `{legacy_branch}`"),
        format!("branch is `{legacy_branch}`"),
        format!("on {legacy_branch}"),
        format!("to {legacy_branch}"),
        format!("from {legacy_branch}"),
        format!("{legacy_branch} branch"),
    ];
    for (path, doc) in live_docs {
        assert!(
            !doc.contains(STALE_CONFORMANCE_SUMMARY),
            "{path} should not contain stale cockpit conformance artifact `{STALE_CONFORMANCE_SUMMARY}`"
        );
        for pattern in &legacy_branch_patterns {
            assert!(
                !doc.contains(pattern),
                "{path} should not contain legacy branch text `{pattern}`"
            );
        }
    }
}

#[test]
fn context_horizon_contract_docs_truth_gate() {
    let contract = read_repo_doc("docs/context-horizon-contract.md");
    let provenance = read_repo_doc("docs/json-schema/PROVENANCE.md");
    let schema = read_repo_doc("docs/json-schema/ft-context-horizon.json");

    assert_contains_all(
        "context horizon contract",
        &contract,
        &[
            "`ft robot context status`",
            "`ft robot context rotate`",
            "`ft robot context history`",
            "`ft.context_horizon.v1`",
            "`schema_version`",
            "`contract_id`",
            "`evidence_state`",
            "`horizon_window_ms`",
            "`fleet_summary`",
            "`pane_risks`",
            "`recommendations`",
            "`citations`",
            "`unavailable_domains`",
            "`redaction_policy`",
            "`artifact_paths`",
            "`raw_context_content_stored` must be `false`",
            "`measured`",
            "`inferred`",
            "`simulated`",
            "`stale`",
            "`unavailable`",
            "`mixed`",
            "`rotate_context`",
            "`prepare_handoff`",
            "`reduce_fanout`",
            "`pause_assignment`",
            "`inspect_prompt`",
            "`collect_incident_bundle`",
            "`none`",
            "`mutation_allowed`",
            "`policy_state`",
            "`source_regression`",
            "`privacy_violation`",
            "`environment_blocked`",
            "`unavailable_evidence`",
            "`target_hardware_skipped`",
        ],
    );
    assert_contains_all(
        "context horizon privacy invariants",
        &contract,
        &[
            "no raw pane transcript",
            "no prompt body",
            "no session cookies, API keys, or bearer tokens",
            "no unbounded text excerpts",
            "no hidden mutation through a recommended command",
        ],
    );
    assert_excludes_all(
        "context horizon contract",
        &contract,
        &["raw transcript is allowed", "raw prompt is allowed"],
    );

    assert_contains_all(
        "schema provenance",
        &provenance,
        &[
            "`ft-context-horizon.json`",
            "`docs/context-horizon-contract.md`",
            "context_horizon_contract_docs_truth_gate",
        ],
    );

    let parsed_schema: serde_json::Value =
        serde_json::from_str(&schema).expect("context horizon schema should parse as JSON");
    assert_eq!(
        parsed_schema
            .pointer("/properties/contract_id/const")
            .and_then(serde_json::Value::as_str),
        Some("ft.context_horizon.v1")
    );
    assert_eq!(
        parsed_schema
            .pointer("/properties/raw_context_content_stored/const")
            .and_then(serde_json::Value::as_bool),
        Some(false)
    );

    let required = parsed_schema
        .pointer("/required")
        .and_then(serde_json::Value::as_array)
        .expect("schema should expose root required fields");
    for field in [
        "schema_version",
        "contract_id",
        "generated_at_ms",
        "source",
        "evidence_state",
        "horizon_window_ms",
        "fleet_summary",
        "pane_risks",
        "recommendations",
        "citations",
        "unavailable_domains",
        "redaction_policy",
        "raw_context_content_stored",
        "artifact_paths",
    ] {
        assert!(
            required.iter().any(|value| value.as_str() == Some(field)),
            "context horizon schema should require {field}"
        );
    }

    let evidence_states = parsed_schema
        .pointer("/$defs/evidence_state/enum")
        .and_then(serde_json::Value::as_array)
        .expect("schema should expose evidence state enum");
    for state in [
        "measured",
        "inferred",
        "simulated",
        "stale",
        "unavailable",
        "mixed",
    ] {
        assert!(
            evidence_states
                .iter()
                .any(|value| value.as_str() == Some(state)),
            "context horizon schema should include evidence state {state}"
        );
    }

    let actions = parsed_schema
        .pointer("/$defs/action_kind/enum")
        .and_then(serde_json::Value::as_array)
        .expect("schema should expose action kind enum");
    for action in [
        "rotate_context",
        "prepare_handoff",
        "reduce_fanout",
        "pause_assignment",
        "inspect_prompt",
        "collect_incident_bundle",
        "none",
    ] {
        assert!(
            actions.iter().any(|value| value.as_str() == Some(action)),
            "context horizon schema should include action {action}"
        );
    }

    for forbidden_pointer in [
        "/$defs/redaction_policy/properties/raw_transcript_allowed/const",
        "/$defs/redaction_policy/properties/raw_prompt_allowed/const",
        "/$defs/recommendation/properties/mutation_allowed/const",
    ] {
        assert_eq!(
            parsed_schema
                .pointer(forbidden_pointer)
                .and_then(serde_json::Value::as_bool),
            Some(false),
            "{forbidden_pointer} should be fail-closed false"
        );
    }
}

#[test]
fn blocker_radar_contract_docs_truth_gate() {
    let contract = read_repo_doc("docs/blocker-radar-contract.md");
    let provenance = read_repo_doc("docs/json-schema/PROVENANCE.md");
    let schema = read_repo_doc("docs/json-schema/ft-blocker-radar.json");

    assert_contains_all(
        "blocker radar contract",
        &contract,
        &[
            "`scripts/swarm-tick.sh --agent-mail-fallback frankenterm`",
            "`br ready`",
            "`br show`",
            "`br dep cycles --json`",
            "`bv --robot-triage`",
            "`git status --short --branch`",
            "`rch status`",
            "`rch diagnose`",
            "`gh run view`",
            "`ft.blocker_radar.v1`",
            "`schema_version`",
            "`contract_id`",
            "`overall_state`",
            "`sources`",
            "`blockers`",
            "`active_agents`",
            "`dirty_overlap`",
            "`external_queues`",
            "`next_actions`",
            "`forbidden_actions`",
            "`citations`",
            "`unavailable_sources`",
            "`redaction_policy`",
            "`artifact_paths`",
            "`raw_pane_content_stored` must be `false`",
            "`actionable`",
            "`waiting_external`",
            "`waiting_owner`",
            "`stale_possible`",
            "`dirty_overlap`",
            "`rch_substrate_blocked`",
            "`ci_queued`",
            "`ci_zero_jobs`",
            "`artifact_missing`",
            "`mail_unavailable`",
            "`degraded`",
            "`unknown`",
            "`recheck_status`",
            "`inspect_artifact`",
            "`add_beads_comment`",
            "`wait_for_owner`",
            "`choose_ready_bead`",
            "`run_bv_robot_triage`",
            "`run_swarm_tick`",
            "`file_followup_bead`",
            "`none`",
            "`mutation_allowed`",
            "`source_regression`",
            "`privacy_violation`",
            "`environment_blocked`",
            "`unavailable_evidence`",
            "`external_queue_blocked`",
            "`dirty_tree_blocked`",
            "`owner_handoff_required`",
            "`target_hardware_skipped`",
        ],
    );
    assert_contains_all(
        "blocker radar privacy and safety invariants",
        &contract,
        &[
            "no raw pane transcript",
            "no prompt body",
            "no session cookies, API keys, or bearer tokens",
            "no unbounded command output",
            "no hidden mutation through a recommended command",
            "`am service restart`",
            "`am doctor fix`",
            "`kill am`",
            "`rch daemon restart`",
            "`git reset --hard`",
            "`git clean -fd`",
        ],
    );
    assert_contains_all(
        "blocker radar claimability reconciliation",
        &contract,
        &[
            "## Claimability Reconciliation",
            "`ft-htcwc.1`",
            "`bv --robot-next`",
            "`br ready --json`",
            "`br show <id> --json`",
            "`ft-e87u6.2`",
            "`BluePike`",
            "`tracker_inconsistent`",
            "`claimable`",
            "`no_ready`",
            "`dependency_blocked`",
            "`owner_blocked`",
            "`external_wait`",
            "`dirty_overlap`",
            "`mail_degraded`",
            "`candidate_id`",
            "`ready_queue_verdict`",
            "`dependency_verdict`",
            "`owner_verdict`",
            "`dirty_path_verdict`",
            "`external_wait_verdict`",
            "`mail_verdict`",
            "`tracker_consistency_verdict`",
            "`final_verdict`",
            "`next_action`",
            "Precedence is fail-closed",
            "advisory prioritization only",
        ],
    );
    assert_excludes_all(
        "blocker radar contract",
        &contract,
        &[
            "raw pane transcript is allowed",
            "raw prompt is allowed",
            "recommended restart",
        ],
    );

    assert_contains_all(
        "schema provenance",
        &provenance,
        &[
            "`ft-blocker-radar.json`",
            "`docs/blocker-radar-contract.md`",
            "blocker_radar_contract_docs_truth_gate",
        ],
    );

    let parsed_schema: serde_json::Value =
        serde_json::from_str(&schema).expect("blocker radar schema should parse as JSON");
    assert_eq!(
        parsed_schema
            .pointer("/properties/contract_id/const")
            .and_then(serde_json::Value::as_str),
        Some("ft.blocker_radar.v1")
    );
    assert_eq!(
        parsed_schema
            .pointer("/properties/raw_pane_content_stored/const")
            .and_then(serde_json::Value::as_bool),
        Some(false)
    );

    let required = parsed_schema
        .pointer("/required")
        .and_then(serde_json::Value::as_array)
        .expect("schema should expose root required fields");
    for field in [
        "schema_version",
        "contract_id",
        "generated_at_ms",
        "source",
        "overall_state",
        "sources",
        "blockers",
        "active_agents",
        "dirty_overlap",
        "external_queues",
        "next_actions",
        "forbidden_actions",
        "citations",
        "unavailable_sources",
        "redaction_policy",
        "raw_pane_content_stored",
        "artifact_paths",
    ] {
        assert!(
            required.iter().any(|value| value.as_str() == Some(field)),
            "blocker radar schema should require {field}"
        );
    }

    let evidence_states = parsed_schema
        .pointer("/$defs/evidence_state/enum")
        .and_then(serde_json::Value::as_array)
        .expect("schema should expose evidence state enum");
    for state in [
        "actionable",
        "waiting_external",
        "waiting_owner",
        "stale_possible",
        "dirty_overlap",
        "rch_substrate_blocked",
        "ci_queued",
        "ci_zero_jobs",
        "artifact_missing",
        "mail_unavailable",
        "degraded",
        "unknown",
    ] {
        assert!(
            evidence_states
                .iter()
                .any(|value| value.as_str() == Some(state)),
            "blocker radar schema should include evidence state {state}"
        );
    }

    let source_kinds = parsed_schema
        .pointer("/$defs/source_kind/enum")
        .and_then(serde_json::Value::as_array)
        .expect("schema should expose source kind enum");
    for source_kind in [
        "rch",
        "github_actions",
        "agent_mail",
        "beads",
        "git",
        "manual",
        "fixture",
    ] {
        assert!(
            source_kinds
                .iter()
                .any(|value| value.as_str() == Some(source_kind)),
            "blocker radar schema should include source kind {source_kind}"
        );
    }

    let actions = parsed_schema
        .pointer("/$defs/action_kind/enum")
        .and_then(serde_json::Value::as_array)
        .expect("schema should expose action kind enum");
    for action in [
        "recheck_status",
        "inspect_artifact",
        "add_beads_comment",
        "wait_for_owner",
        "choose_ready_bead",
        "run_bv_robot_triage",
        "run_swarm_tick",
        "file_followup_bead",
        "none",
    ] {
        assert!(
            actions.iter().any(|value| value.as_str() == Some(action)),
            "blocker radar schema should include action {action}"
        );
    }

    for forbidden_pointer in [
        "/properties/raw_pane_content_stored/const",
        "/$defs/redaction_policy/properties/raw_pane_content_allowed/const",
        "/$defs/redaction_policy/properties/raw_prompt_allowed/const",
        "/$defs/next_action/properties/mutation_allowed/const",
    ] {
        assert_eq!(
            parsed_schema
                .pointer(forbidden_pointer)
                .and_then(serde_json::Value::as_bool),
            Some(false),
            "{forbidden_pointer} should be fail-closed false"
        );
    }

    let bounded_citations = parsed_schema
        .pointer("/$defs/redaction_policy/properties/bounded_citations_only/const")
        .and_then(serde_json::Value::as_bool);
    assert_eq!(bounded_citations, Some(true));
}

#[test]
fn smoke_ft_robot_default() {
    // `ft robot` with no subcommand defaults to quick-start
    let output = wa_cmd()
        .arg("robot")
        .output()
        .expect("ft robot should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    save_artifact("robot_default_stdout.txt", &stdout);

    assert!(output.status.success(), "ft robot (default) should exit 0");
}

#[test]
fn smoke_ft_export_help() {
    // Export help should always work without a DB
    let output = wa_cmd()
        .args(["export", "--help"])
        .output()
        .expect("ft export --help should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    save_artifact("export_help_stdout.txt", &stdout);

    assert!(output.status.success(), "ft export --help should exit 0");
    assert!(
        stdout.contains("segments") || stdout.contains("Export"),
        "ft export --help should list export kinds"
    );
}

#[test]
fn smoke_ft_robot_health() {
    let output = wa_cmd()
        .args(["robot", "health"])
        .output()
        .expect("ft robot health should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    save_artifact("robot_health_stdout.txt", &stdout);
    save_artifact("robot_health_stderr.txt", &stderr);
    emit_artifact("ft_robot_health", &stdout);

    assert!(
        output.status.success(),
        "ft robot health should exit 0, stderr: {stderr}"
    );

    // Should produce valid JSON with version field
    let parsed = serde_json::from_str::<serde_json::Value>(&stdout);
    assert!(parsed.is_ok(), "ft robot health should output valid JSON");
    let val = parsed.unwrap();
    // Robot response wraps data
    assert!(
        val["data"]["version"].is_string() || val["version"].is_string(),
        "ft robot health should include version"
    );
    assert!(
        val["data"]["active_agents"]["schema_version"].is_number(),
        "ft robot health should include bounded active-agent health data"
    );
    assert!(
        val["data"]["active_agent_sources"]["running_inventory"]["ok"].is_boolean(),
        "ft robot health should report active-agent source availability"
    );
}

#[test]
fn smoke_robot_playbook_commands_emit_json_envelopes() {
    let commands: [(&str, &[&str]); 4] = [
        ("robot_state", &["robot", "--format", "json", "state"]),
        (
            "robot_search",
            &[
                "robot",
                "--format",
                "json",
                "search",
                "playbook-smoke",
                "--limit",
                "1",
            ],
        ),
        (
            "robot_events",
            &["robot", "--format", "json", "events", "--limit", "1"],
        ),
        (
            "robot_workflow_list",
            &["robot", "--format", "json", "workflow", "list"],
        ),
    ];

    for (label, args) in commands {
        let output = wa_cmd()
            .args(args)
            .output()
            .expect("playbook command should execute");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        save_artifact(&format!("{label}_stdout.txt"), &stdout);
        save_artifact(&format!("{label}_stderr.txt"), &stderr);

        assert!(
            !stderr.contains("panicked"),
            "{label} should not panic, stderr: {stderr}"
        );

        let parsed = serde_json::from_str::<serde_json::Value>(&stdout).unwrap_or_else(|e| {
            panic!("{label} should emit valid JSON envelope, parse error: {e}, stdout: {stdout}")
        });
        assert!(
            parsed
                .get("ok")
                .and_then(serde_json::Value::as_bool)
                .is_some(),
            "{label} JSON should include boolean 'ok' field: {parsed}"
        );
    }
}

// =============================================================================
// Predicate-based tests (using assert_cmd sugar)
// =============================================================================

#[test]
fn smoke_help_contains_subcommands() {
    wa_cmd()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("doctor"))
        .stdout(predicate::str::contains("setup"))
        .stdout(predicate::str::contains("export"))
        .stdout(predicate::str::contains("accounts"));
}

#[test]
fn smoke_wa_accounts_help() {
    let output = wa_cmd()
        .args(["accounts", "--help"])
        .output()
        .expect("ft accounts --help should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    save_artifact("accounts_help_stdout.txt", &stdout);

    assert!(output.status.success(), "ft accounts --help should exit 0");
    assert!(
        stdout.contains("accounts") || stdout.contains("Accounts"),
        "ft accounts --help should mention accounts"
    );
    assert!(
        stdout.contains("refresh") || stdout.contains("Refresh"),
        "ft accounts --help should mention refresh subcommand"
    );
}

#[test]
fn smoke_unknown_subcommand_fails() {
    wa_cmd().arg("nonexistent-command-xyz").assert().failure();
}

// =============================================================================
// Summary artifact generation
// =============================================================================

#[test]
fn smoke_generate_summary() {
    // This test runs last (alphabetically) and generates a summary artifact
    let commands = vec![
        ("ft --help", vec!["--help"]),
        ("ft --version", vec!["--version"]),
        ("ft doctor --json", vec!["doctor", "--json"]),
        ("ft robot quick-start", vec!["robot", "quick-start"]),
        ("ft export --help", vec!["export", "--help"]),
    ];

    let mut results = Vec::new();
    for (name, args) in &commands {
        let start = std::time::Instant::now();
        let output = wa_cmd()
            .args(args)
            .output()
            .expect("command should execute");
        let duration_ms = start.elapsed().as_millis();
        let passed = output.status.success();
        results.push(serde_json::json!({
            "command": name,
            "passed": passed,
            "exit_code": output.status.code(),
            "duration_ms": duration_ms,
            "stdout_len": output.stdout.len(),
            "stderr_len": output.stderr.len(),
        }));
    }

    let summary = serde_json::json!({
        "test": "docs_smoke",
        "total": results.len(),
        "passed": results.iter().filter(|r| r["passed"] == true).count(),
        "results": results,
    });

    let summary_str = serde_json::to_string_pretty(&summary).unwrap();
    save_artifact("summary.json", &summary_str);
    emit_artifact("summary", &summary_str);
}
