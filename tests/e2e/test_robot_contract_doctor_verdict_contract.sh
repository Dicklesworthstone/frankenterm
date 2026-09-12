#!/usr/bin/env bash
# Static conformance/golden verifier for the Robot/MCP Contract Doctor verdict
# surface. ZERO RCH: this script validates fixtures and optional live artifacts
# with Ruby/JQ only.
set -euo pipefail

ROOT="${FRANKENTERM_REPO_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"
export FRANKENTERM_REPO_ROOT="$ROOT"
cd "$ROOT"

source "tests/scripts/static_attestation_helpers.sh"

static_attestation_require_command jq
static_attestation_require_executable_script "tests/e2e/test_robot_contract_doctor_verdict_contract.sh"

jq empty \
  fixtures/robot-contract-doctor/manifest.json \
  fixtures/robot-contract-doctor/golden/verdict.v1.json \
  fixtures/robot-contract-doctor/golden/verdict-projection.v1.json \
  fixtures/robot-contract-doctor/invalid/fragments.v1.json

static_attestation_run_ruby - <<'RUBY'
require "json"
require "set"

MANIFEST = "fixtures/robot-contract-doctor/manifest.json"
GOLDEN = "fixtures/robot-contract-doctor/golden/verdict.v1.json"
PROJECTION = "fixtures/robot-contract-doctor/golden/verdict-projection.v1.json"
INVALID = "fixtures/robot-contract-doctor/invalid/fragments.v1.json"
LIVE_ARTIFACT = "docs/attestations/proofs/robot-contract-doctor.json"
LIVE_MANIFEST = "docs/attestations/manifest.json"
# Releases are DSR-only (AGENTS.md Rule 0.1): the verdict steps are wired in
# the repo release-gate script, not a GitHub workflow (ft-xxfwy.16).
WORKFLOW = "scripts/release-gates.sh"
MATRIX = "docs/robot-contracts/contract-doctor-matrix.md"
ORACLE = "scripts/check-contract-doctor-coverage.sh"
REGISTRY = "crates/frankenterm-core/src/robot_api_contracts.rs"
registry_source = StaticAttestation.read_text!(REGISTRY, check: "robot_contract_doctor.registry")
registry_all = registry_source.match(/pub const ALL:\s*&'static \[ApiSurface\]\s*=\s*&\[(.*?)\];/m)
raise "cannot locate ApiSurface::ALL" unless registry_all
LIVE_SURFACE_COUNT = registry_all[1].scan(/Self::[A-Za-z0-9_]+/).length
raise "ApiSurface::ALL must not be empty" if LIVE_SURFACE_COUNT.zero?

EXPECTED_DIMENSIONS = %w[ENV PAR POL RED TOON ERR].freeze
EXPECTED_GAPS = {
  "G1" => "ft-6mmyp",
  "G3" => "ft-5puf0",
  "G4" => "ft-7h5da.13.6",
}.freeze
EXPECTED_STEPS = [
  "Robot/MCP Contract Doctor static verdict",
  "Robot/MCP Contract Doctor attestation slot",
  "Robot/MCP Contract Doctor cargo verdict",
].freeze
EXPECTED_BOUNDARIES = %w[
  does_not_claim_all_policy_audit_rows_complete
  does_not_claim_dom_redaction_gap_closed
  does_not_claim_local_cargo_as_remote_proof
  does_not_claim_mission_pause_resume_abort_in_api_surface_all
  does_not_mutate_panes_services_or_beads
].freeze

def assert_ok(condition, message, check:, input_path: nil, expected: true, actual: condition)
  StaticAttestation.assert!(
    condition,
    message,
    check: check,
    input_path: input_path,
    expected: expected,
    actual: actual,
  )
end

def expect_equal(actual, expected, message, check:, input_path: nil)
  assert_ok(
    actual == expected,
    message,
    check: check,
    input_path: input_path,
    expected: expected,
    actual: actual,
  )
end

def repo_file?(path)
  File.file?(StaticAttestation.repo_path(path))
end

def read_json(path)
  StaticAttestation.read_json!(path, check: "robot_contract_doctor.read_json")
end

def projection_for(verdict)
  {
    "contract_id" => verdict.fetch("contract").fetch("contract_id"),
    "category" => verdict.fetch("category"),
    "produced_by_bead" => verdict.fetch("produced_by_bead"),
    "overall_status" => verdict.fetch("overall_status"),
    "api_surface_count" => verdict.fetch("contract").fetch("api_surface_count"),
    "matrix_surface_count" => verdict.fetch("contract").fetch("matrix_surface_count"),
    "dimensions" => verdict.fetch("contract").fetch("dimensions"),
    "tracked_gap_ids" => verdict.fetch("tracked_exceptions").map { |gap| gap.fetch("gap_id") }.sort,
    "tracked_gap_beads" => verdict.fetch("tracked_exceptions").map { |gap| gap.fetch("tracking_bead") }.sort,
    "fail_closed_step_names" => verdict.fetch("ci_verdict").fetch("fail_closed_steps").map { |step| step.fetch("name") },
    "claim_boundaries" => verdict.fetch("claim_boundaries").select { |_key, value| value == true }.keys.sort,
  }
end

def validation_errors(verdict, input_path)
  errors = []

  errors << "schema_version" unless verdict["schema_version"] == "1.0.0"
  errors << "kind" unless verdict["kind"] == "robot-contract-doctor-attestation"
  errors << "category" unless verdict["category"] == "proofs/robot-contracts"
  errors << "produced_by_bead" unless verdict["produced_by_bead"] == "ft-7h5da.13.7"
  expected_status = input_path == LIVE_ARTIFACT ? "pending_final_qualification" : "pass_with_tracked_exceptions"
  errors << "overall_status" unless verdict["overall_status"] == expected_status

  proof_categories = verdict["proof_categories"]
  errors << "proof_categories" unless proof_categories.is_a?(Array) && proof_categories.include?(4)

  contract = verdict["contract"]
  unless contract.is_a?(Hash)
    errors << "contract"
    return errors
  end
  errors << "contract_id" unless contract["contract_id"] == "ft.robot_contract_doctor.v1"
  # Historical golden/invalid documents retain their original registry size;
  # the live producer must track the current typed registry, not that snapshot.
  expected_surface_count = input_path == LIVE_ARTIFACT ? LIVE_SURFACE_COUNT : 38
  errors << "api_surface_count" unless contract["api_surface_count"] == expected_surface_count
  errors << "matrix_surface_count" unless contract["matrix_surface_count"] == expected_surface_count
  errors << "dimensions" unless contract["dimensions"] == EXPECTED_DIMENSIONS
  errors << "unified_ci_verdict" unless contract["unified_ci_verdict"] == true
  errors << "local_cargo_counts_as_proof" unless contract["local_cargo_counts_as_proof"] == false
  errors << "tracked_exceptions_are_not_green_claims" unless contract["tracked_exceptions_are_not_green_claims"] == true
  if input_path == LIVE_ARTIFACT
    errors << "partial_cell_count" unless contract["partial_cell_count"] == 0
    errors << "full_release_qualified" unless contract["full_release_qualified"] == false
  end

  boundaries = verdict["claim_boundaries"]
  if boundaries.is_a?(Hash)
    EXPECTED_BOUNDARIES.each do |boundary|
      errors << "claim_boundaries.#{boundary}" unless boundaries[boundary] == true
    end
  else
    errors << "claim_boundaries"
  end

  ci = verdict["ci_verdict"]
  if ci.is_a?(Hash)
    errors << "ci_verdict.workflow" unless ci["workflow"] == WORKFLOW
    expected_job = input_path == LIVE_ARTIFACT ? "dsr-repository-gates" : "matrix"
    errors << "ci_verdict.job" unless ci["job"] == expected_job
    steps = ci["fail_closed_steps"]
    if steps.is_a?(Array)
      names = steps.map { |step| step["name"] }
      errors << "fail_closed_steps" unless names == EXPECTED_STEPS
      command_text = steps.map { |step| step["command"].to_s }.join("\n")
      errors << "fail_closed_steps.static_oracle" unless command_text.include?(ORACLE)
      expected_slot_command = input_path == LIVE_ARTIFACT ? "bash scripts/release-gates.sh --only 'Robot/MCP Contract Doctor attestation slot'" : "jq -e"
      errors << "fail_closed_steps.slot" unless command_text.include?(expected_slot_command)
      expected_cargo_command = if input_path == LIVE_ARTIFACT
        "bash scripts/release-gates.sh --cargo --only 'Robot/MCP Contract Doctor cargo verdict'"
      else
        "cargo test -p frankenterm-core --lib robot_api_contracts"
      end
      errors << "fail_closed_steps.cargo_filter" unless command_text.include?(expected_cargo_command)
    else
      errors << "fail_closed_steps"
    end
  else
    errors << "ci_verdict"
  end

  tracked = verdict["tracked_exceptions"]
  if tracked.is_a?(Array)
    actual = tracked.to_h { |gap| [gap["gap_id"], gap["tracking_bead"]] }
    errors << "tracked_exceptions" unless actual == EXPECTED_GAPS
    tracked.each do |gap|
      expected_gap_status = input_path == LIVE_ARTIFACT ? "scope_boundary" : "tracked_exception"
      errors << "tracked_exceptions.status" unless gap["status"] == expected_gap_status
    end
  else
    errors << "tracked_exceptions"
  end

  source_artifacts = verdict["source_artifacts"]
  if source_artifacts.is_a?(Array)
    source_artifacts.each do |source|
      path = source["path"]
      begin
        StaticAttestation.repo_relative_path!(path, field: "source_artifacts.path", check: "robot_contract_doctor.source_path")
      rescue StaticAttestation::Failure
        errors << "source_artifacts.path"
      end
      unless repo_file?(path)
        errors << "source_artifacts.exists"
      end
    end
  elsif input_path == GOLDEN || input_path == LIVE_ARTIFACT
    errors << "source_artifacts"
  end

  errors
end

def assert_valid_verdict(verdict, input_path)
  errors = validation_errors(verdict, input_path)
  assert_ok(
    errors.empty?,
    "#{input_path} is not a valid Robot Contract Doctor verdict: #{errors.join(", ")}",
    check: "robot_contract_doctor.valid_verdict",
    input_path: input_path,
    expected: [],
    actual: errors,
  )
end

manifest = read_json(MANIFEST)
expect_equal(manifest.fetch("contract_id"), "ft.robot_contract_doctor.v1", "fixture manifest contract id drifted", check: "robot_contract_doctor.manifest.contract_id", input_path: MANIFEST)
expect_equal(manifest.fetch("verifier"), "tests/e2e/test_robot_contract_doctor_verdict_contract.sh", "fixture manifest verifier drifted", check: "robot_contract_doctor.manifest.verifier", input_path: MANIFEST)
manifest.fetch("source_documents").each { |path| StaticAttestation.require_file!(path, check: "robot_contract_doctor.manifest.source_document") }

StaticAttestation.require_file_terms!(
  MATRIX,
  StaticAttestation.expected_strings(
    "Unified verdict + attestation",
    "ft-7h5da.13.7",
    "G1",
    "G3",
    "G4",
    "ft-6mmyp",
    "ft-5puf0",
  ),
  check: "robot_contract_doctor.matrix_terms",
)
StaticAttestation.require_file_terms!(
  ORACLE,
  StaticAttestation.expected_strings(
    "Contract-Doctor completeness oracle",
    "ApiSurface::ALL",
    "mission_pause",
    "ft-6mmyp",
    "ft-5puf0",
  ),
  check: "robot_contract_doctor.oracle_terms",
)

golden = read_json(GOLDEN)
expected_projection = read_json(PROJECTION)
assert_valid_verdict(golden, GOLDEN)
expect_equal(projection_for(golden), expected_projection, "golden verdict projection drifted", check: "robot_contract_doctor.golden_projection", input_path: GOLDEN)

invalid_cases = read_json(INVALID)
invalid_cases.each do |invalid_case|
  errors = validation_errors(invalid_case.fetch("document"), "#{INVALID}:#{invalid_case.fetch("case")}")
  expected_failure = invalid_case.fetch("expected_failure")
  assert_ok(
    errors.any? { |error| error.include?(expected_failure) },
    "invalid case #{invalid_case.fetch("case")} did not fail with #{expected_failure}",
    check: "robot_contract_doctor.invalid_case",
    input_path: "#{INVALID}:#{invalid_case.fetch("case")}",
    expected: expected_failure,
    actual: errors,
  )
end

if repo_file?(LIVE_ARTIFACT)
  live = read_json(LIVE_ARTIFACT)
  assert_valid_verdict(live, LIVE_ARTIFACT)
  live_projection = expected_projection.merge(
    "overall_status" => "pending_final_qualification",
    "api_surface_count" => LIVE_SURFACE_COUNT,
    "matrix_surface_count" => LIVE_SURFACE_COUNT,
  )
  expect_equal(projection_for(live), live_projection, "live verdict projection drifted from current contract", check: "robot_contract_doctor.live_projection", input_path: LIVE_ARTIFACT)
  stale_live = Marshal.load(Marshal.dump(live))
  stale_live["contract"]["api_surface_count"] = LIVE_SURFACE_COUNT - 1
  stale_live["contract"]["matrix_surface_count"] = LIVE_SURFACE_COUNT - 1
  assert_ok(
    validation_errors(stale_live, LIVE_ARTIFACT).include?("api_surface_count") &&
      validation_errors(stale_live, LIVE_ARTIFACT).include?("matrix_surface_count"),
    "live verdict must reject a stale registry count",
    check: "robot_contract_doctor.live_stale_count_negative",
  )
  premature_green = Marshal.load(Marshal.dump(live))
  premature_green["contract"]["full_release_qualified"] = true
  assert_ok(validation_errors(premature_green, LIVE_ARTIFACT).include?("full_release_qualified"),
    "focused receipts must not claim completed release qualification",
    check: "robot_contract_doctor.live_premature_release_negative")

  # The manifest slot is either populated (producer closed) or explicitly
  # deferred to the producer bead (attestation: defer blocked producer slots).
  # Both are honest states; a silently absent slot is the failure.
  manifest_doc = read_json(LIVE_MANIFEST)
  slot = manifest_doc.fetch("slots").find do |candidate|
    candidate["category"] == "proofs/robot-contracts" &&
      (candidate["path"] == LIVE_ARTIFACT ||
       (candidate["path"].nil? && candidate["deferred_to_bead"] == "ft-7h5da.13.7"))
  end
  assert_ok(!slot.nil?, "live manifest slot missing for #{LIVE_ARTIFACT}", check: "robot_contract_doctor.live_manifest_slot", input_path: LIVE_MANIFEST, expected: LIVE_ARTIFACT, actual: nil)
  producer = slot["produced_by_bead"] || slot["deferred_to_bead"]
  expect_equal(producer, "ft-7h5da.13.7", "live manifest slot producer drifted", check: "robot_contract_doctor.live_manifest_slot.producer", input_path: LIVE_MANIFEST)
  assert_ok(slot.fetch("proof_categories").include?(4), "live manifest slot must include proof category 4", check: "robot_contract_doctor.live_manifest_slot.proof_category", input_path: LIVE_MANIFEST)
  if slot["path"].nil?
    StaticAttestation.log_check(
      "robot_contract_doctor.live_manifest_slot.deferred",
      input_path: LIVE_MANIFEST,
      expected: "populated or deferred_to_bead=ft-7h5da.13.7",
      actual: "deferred: #{slot["deferred_reason"]}",
      status: "pass",
    )
  end

  workflow = StaticAttestation.read_text!(WORKFLOW, check: "robot_contract_doctor.workflow")
  EXPECTED_STEPS.each do |step|
    present = workflow.include?(step)
    assert_ok(present, "workflow missing #{step}", check: "robot_contract_doctor.workflow_step", input_path: WORKFLOW, expected: step, actual: present ? "present" : "missing")
  end
  cargo_gate = workflow.lines.find { |line| line.start_with?('cargo_gate "Robot/MCP Contract Doctor cargo verdict"') }
  static_gate = workflow.lines.find { |line| line.start_with?('gate "Robot/MCP Contract Doctor static verdict"') }
  assert_ok(static_gate && static_gate.include?("bash scripts/check-contract-doctor-coverage.sh --strict"),
    "Doctor release static gate must reject partial cells",
    check: "robot_contract_doctor.static_release_gate_strict", input_path: WORKFLOW)
  required_cargo_terms = [
    "bash scripts/check-contract-doctor-coverage.sh --strict && cargo test",
    "--features mcp", "--lib", "--test conformance_robot_api_surface_coverage",
    "--test conformance_robot_envelope_schema", "--test mcp_conformance_core_tools",
    "--test mcp_conformance_additional_tools", "--test wa_event_mutations_mcp_conformance",
    "--test proptest_toon_roundtrip", "--test toon_golden",
    "--no-fail-fast", "--test cli_contract_tests", "--bin ft",
    "doctor_cargo_test contract_doctor_cli_mcp_read_and_refresh_parity 1",
    "doctor_cargo_test tests::workflow_dry_run_report_includes_steps positive",
    "doctor_cargo_test tests::redact_pane_text_results_for_output_scrubs_ok_and_error_payloads 1",
    "--exact --nocapture --color never",
  ]
  required_cargo_terms.each do |term|
    assert_ok(cargo_gate && cargo_gate.include?(term), "Doctor cargo gate missing #{term}",
      check: "robot_contract_doctor.cargo_gate_dimensions", input_path: WORKFLOW)
  end
else
  StaticAttestation.log_check(
    "robot_contract_doctor.live_artifact_optional",
    input_path: LIVE_ARTIFACT,
    expected: "validated when present",
    actual: "absent",
    status: "pass",
  )
end

puts "robot contract doctor verdict contract: passed (golden projection, #{invalid_cases.length} invalid fragments, live=#{repo_file?(LIVE_ARTIFACT)})"
RUBY

# Exercise transcript acceptance without invoking Cargo. These are gate-parser
# controls, not runtime receipts for the product tests named by the gate.
doctor_helper=$(sed -n '/^doctor_cargo_test() {/,/^}/p' scripts/release-gates.sh)
[[ -n "$doctor_helper" ]] || { echo 'Doctor Cargo proof helper missing' >&2; exit 1; }
(
  eval "$doctor_helper"
  cargo() { printf '%s\n' "$doctor_fixture"; return "$doctor_status"; }
  doctor_status=0
  doctor_fixture=$'test oracle ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored; 9 filtered out; finished in 0.01s'
  doctor_cargo_test oracle 1 >/dev/null
  doctor_cargo_test oracle positive >/dev/null
  doctor_fixture='test result: ok. 0 passed; 0 failed; 0 ignored; 10 filtered out; finished in 0.01s'
  if doctor_cargo_test oracle 1 >/dev/null; then
    echo 'Doctor gate accepted zero tests' >&2; exit 1
  fi
  doctor_fixture=$'test different ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored; 9 filtered out; finished in 0.01s'
  if doctor_cargo_test oracle 1 >/dev/null; then
    echo 'Doctor gate accepted wrong oracle' >&2; exit 1
  fi
  doctor_fixture=$'test oracle ... ok\ntest result: ok. 2 passed; 0 failed; 0 ignored; 8 filtered out; finished in 0.01s'
  if doctor_cargo_test oracle 1 >/dev/null; then
    echo 'Doctor gate accepted non-exact test count' >&2; exit 1
  fi
  doctor_cargo_test oracle positive >/dev/null
  doctor_status=1
  if doctor_cargo_test oracle positive >/dev/null; then
    echo 'Doctor gate accepted unsuccessful Cargo exit' >&2; exit 1
  fi
)
echo 'Doctor filtered-test transcript controls: passed (no Cargo executed)'
