#!/usr/bin/env bash
# ft-e87u6.16: static claim-drift sentinel for operator-facing docs.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
REGISTRY="docs/attestations/claim-registry.json"
FIXTURES="fixtures/claim-drift-sentinel/golden-cases.v1.json"
OUTPUT_MODE="text"
STRICT=0

usage() {
  cat <<'USAGE'
Usage: scripts/check-claim-drift-sentinel.sh [options]

Options:
  --json               Emit machine-readable JSON diagnostics.
  --strict             Exit 1 when any live registry or fixture diagnostic fails.
  --registry PATH      Registry path (default: docs/attestations/claim-registry.json).
  --fixtures PATH      Golden fixture path (default: fixtures/claim-drift-sentinel/golden-cases.v1.json).
  -h, --help           Show this help.
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --json) OUTPUT_MODE="json"; shift ;;
    --strict) STRICT=1; shift ;;
    --registry) REGISTRY="$2"; shift 2 ;;
    --fixtures) FIXTURES="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

command -v ruby >/dev/null 2>&1 || { echo "error: ruby required" >&2; exit 2; }
command -v jq >/dev/null 2>&1 || { echo "error: jq required" >&2; exit 2; }

cd "${REPO_ROOT}"

FRANKENTERM_REPO_ROOT="${REPO_ROOT}" ruby -I "${REPO_ROOT}/tests/scripts" -r static_attestation_helpers - "${REGISTRY}" "${FIXTURES}" "${OUTPUT_MODE}" "${STRICT}" <<'RUBY'
require "json"
require "pathname"

StaticAttestation.configure(log_enabled: false)

registry_path = ARGV.fetch(0)
fixtures_path = ARGV.fetch(1)
output_mode = ARGV.fetch(2)
strict = ARGV.fetch(3) == "1"

ALLOWED_STATES = %w[supported planned skipped stale unavailable deprecated].freeze
REQUIRED_CLAIM_FIELDS = %w[
  claim_id
  surface_path
  claim_type
  selector
  claim_text
  wording_state
  source_of_truth_command
  expected_artifact_path
  producing_bead
  proof_category
  freshness_rule
  release_source_required
  dirty_worktree_measurement_allowed
].freeze

class Sentinel
  attr_reader :checks, :diagnostics

  def initialize(registry_path:, fixtures_path:)
    @registry_path = registry_path
    @fixtures_path = fixtures_path
    @checks = []
    @diagnostics = []
  end

  def repo_root
    StaticAttestation.repo_root
  end

  def read_json(path)
    StaticAttestation.read_json!(path, check: "claim_drift.read_json")
  end

  def read_text(path)
    StaticAttestation.read_text!(path, check: "claim_drift.read_text")
  end

  def repo_file?(path)
    candidate = StaticAttestation.repo_path(path)
    File.file?(candidate)
  rescue StaticAttestation::Failure
    false
  end

  def add_check(name, status:, input_path:, expected:, actual:, claim_id: nil, reason: nil)
    check = {
      "name" => name,
      "status" => status,
      "input_path" => input_path,
      "expected" => expected,
      "actual" => actual,
    }
    check["claim_id"] = claim_id if claim_id
    check["reason"] = reason if reason
    @checks << check
    return if status == "pass"

    @diagnostics << {
      "claim_id" => claim_id,
      "code" => name,
      "severity" => "error",
      "message" => reason || "#{name} failed",
      "input_path" => input_path,
      "expected" => expected,
      "actual" => actual,
    }.compact
  end

  def pass(name, input_path:, expected:, actual:, claim_id: nil)
    add_check(name, status: "pass", input_path: input_path, expected: expected, actual: actual, claim_id: claim_id)
  end

  def fail(name, input_path:, expected:, actual:, claim_id: nil, reason:)
    add_check(name, status: "fail", input_path: input_path, expected: expected, actual: actual, claim_id: claim_id, reason: reason)
  end

  def validate_path(path, field:, claim_id: nil)
    StaticAttestation.repo_relative_path!(path, field: field, check: "claim_drift.repo_relative_path")
    pass("path.repo_relative", input_path: path, expected: "repo-relative", actual: "repo-relative", claim_id: claim_id)
    true
  rescue StaticAttestation::Failure => error
    fail("path.repo_relative", input_path: path, expected: "repo-relative", actual: path, claim_id: claim_id, reason: error.message)
    false
  end

  def json_pointer(value, path)
    path.reduce(value) do |cursor, key|
      return nil unless cursor.is_a?(Hash)
      cursor[key]
    end
  end

  def duplicates(values)
    counts = Hash.new(0)
    values.each { |value| counts[value] += 1 }
    counts.select { |_value, count| count > 1 }.keys
  end

  def validate_registry(registry)
    pass("registry.contract_id", input_path: @registry_path, expected: "ft.claim_drift.registry.v1", actual: registry["contract_id"])
    unless registry["contract_id"] == "ft.claim_drift.registry.v1"
      fail("registry.contract_id", input_path: @registry_path, expected: "ft.claim_drift.registry.v1", actual: registry["contract_id"], reason: "unexpected registry contract id")
    end

    states = registry.fetch("allowed_wording_states", [])
    if states.sort == ALLOWED_STATES.sort
      pass("registry.allowed_wording_states", input_path: @registry_path, expected: ALLOWED_STATES.sort, actual: states.sort)
    else
      fail("registry.allowed_wording_states", input_path: @registry_path, expected: ALLOWED_STATES.sort, actual: states.sort, reason: "allowed wording states drifted")
    end

    claims = registry.fetch("claims", [])
    claim_ids = claims.map { |claim| claim["claim_id"] }
    duplicate_ids = duplicates(claim_ids)
    if duplicate_ids.empty?
      pass("registry.unique_claim_ids", input_path: @registry_path, expected: "unique", actual: claim_ids.length)
    else
      fail("registry.unique_claim_ids", input_path: @registry_path, expected: "unique", actual: duplicate_ids, reason: "duplicate claim ids")
    end

    claims.each { |claim| validate_claim(registry, claim) }
  end

  def validate_claim(registry, claim)
    claim_id = claim["claim_id"] || "(missing)"
    missing = REQUIRED_CLAIM_FIELDS.reject { |field| claim.key?(field) }
    if missing.empty?
      pass("claim.required_fields", input_path: @registry_path, expected: REQUIRED_CLAIM_FIELDS, actual: REQUIRED_CLAIM_FIELDS, claim_id: claim_id)
    else
      fail("claim.required_fields", input_path: @registry_path, expected: REQUIRED_CLAIM_FIELDS, actual: missing, claim_id: claim_id, reason: "claim is missing required fields")
      return
    end

    validate_path(claim.fetch("surface_path"), field: "surface_path", claim_id: claim_id)
    validate_path(claim.fetch("expected_artifact_path"), field: "expected_artifact_path", claim_id: claim_id)

    state = claim.fetch("wording_state")
    if ALLOWED_STATES.include?(state)
      pass("claim.wording_state", input_path: @registry_path, expected: ALLOWED_STATES, actual: state, claim_id: claim_id)
    else
      fail("claim.wording_state", input_path: @registry_path, expected: ALLOWED_STATES, actual: state, claim_id: claim_id, reason: "invalid wording state")
    end

    surface_path = claim.fetch("surface_path")
    if repo_file?(surface_path)
      pass("claim.surface_exists", input_path: surface_path, expected: "file", actual: "file", claim_id: claim_id)
    else
      fail("claim.surface_exists", input_path: surface_path, expected: "file", actual: "missing", claim_id: claim_id, reason: "claim surface file is missing")
    end

    artifact_path = claim.fetch("expected_artifact_path")
    if repo_file?(artifact_path)
      pass("claim.artifact_exists", input_path: artifact_path, expected: "file", actual: "file", claim_id: claim_id)
    else
      fail("claim.artifact_exists", input_path: artifact_path, expected: "file", actual: "missing", claim_id: claim_id, reason: "expected artifact is missing")
    end

    if claim.fetch("release_source_required")
      command = claim.fetch("source_of_truth_command")
      source_ok = command.include?("HEAD") || command.include?("--source=head")
      if source_ok
        pass("claim.release_source_head", input_path: @registry_path, expected: "HEAD or --source=head", actual: command, claim_id: claim_id)
      else
        fail("claim.release_source_head", input_path: @registry_path, expected: "HEAD or --source=head", actual: command, claim_id: claim_id, reason: "release claim source command is not head-sourced")
      end

      if claim.fetch("dirty_worktree_measurement_allowed") == false
        pass("claim.dirty_worktree_forbidden", input_path: @registry_path, expected: false, actual: false, claim_id: claim_id)
      else
        fail("claim.dirty_worktree_forbidden", input_path: @registry_path, expected: false, actual: true, claim_id: claim_id, reason: "release claim allows dirty worktree measurement")
      end
    end

    case claim.fetch("claim_type")
    when "count_placeholder"
      validate_count_claim(claim)
    when "manifest_slot"
      validate_manifest_slot_claim(claim)
    when "artifact_status"
      validate_artifact_status_claim(claim)
    when "text_terms"
      validate_text_terms_claim(claim)
    when "planned_contract"
      validate_planned_contract_claim(claim)
    else
      fail("claim.type", input_path: @registry_path, expected: "known claim_type", actual: claim.fetch("claim_type"), claim_id: claim_id, reason: "unknown claim type")
    end
  end

  def validate_count_claim(claim)
    claim_id = claim.fetch("claim_id")
    tracked_count = claim.fetch("tracked_count")
    surface = read_text(claim.fetch("surface_path"))
    match = surface.match(/<!--count:#{Regexp.escape(tracked_count)}-->(\d+)<!--\/count-->/)
    documented_value = match && match[1].to_i
    artifact = read_json(claim.fetch("expected_artifact_path"))
    entry = artifact.fetch("counts", []).find { |row| row["name"] == tracked_count }
    artifact_value = entry && entry["live_value"]
    source_mode = artifact.dig("source", "count_source")

    if match
      pass("count.placeholder_present", input_path: claim.fetch("surface_path"), expected: tracked_count, actual: documented_value, claim_id: claim_id)
    else
      fail("count.placeholder_present", input_path: claim.fetch("surface_path"), expected: tracked_count, actual: "missing", claim_id: claim_id, reason: "count placeholder is missing")
    end

    if source_mode == "head"
      pass("count.artifact_source_head", input_path: claim.fetch("expected_artifact_path"), expected: "head", actual: source_mode, claim_id: claim_id)
    else
      fail("count.artifact_source_head", input_path: claim.fetch("expected_artifact_path"), expected: "head", actual: source_mode, claim_id: claim_id, reason: "count artifact is not head-sourced")
    end

    if !entry.nil? && documented_value == artifact_value
      pass("count.documented_matches_artifact", input_path: claim.fetch("expected_artifact_path"), expected: artifact_value, actual: documented_value, claim_id: claim_id)
    else
      fail("count.documented_matches_artifact", input_path: claim.fetch("expected_artifact_path"), expected: artifact_value, actual: documented_value, claim_id: claim_id, reason: "documented count drifts from retained artifact")
    end
  end

  def validate_manifest_slot_claim(claim)
    claim_id = claim.fetch("claim_id")
    manifest = read_json("docs/attestations/manifest.json")
    category = claim.fetch("manifest_category")
    slot = manifest.fetch("slots", []).find { |row| row["category"] == category && row["path"] == claim.fetch("expected_artifact_path") }
    if slot
      pass("manifest.slot_populated", input_path: "docs/attestations/manifest.json", expected: claim.fetch("expected_artifact_path"), actual: slot["path"], claim_id: claim_id)
    else
      matching_categories = manifest.fetch("slots", []).select { |row| row["category"] == category }.map { |row| row["path"] }
      fail("manifest.slot_populated", input_path: "docs/attestations/manifest.json", expected: claim.fetch("expected_artifact_path"), actual: matching_categories, claim_id: claim_id, reason: "manifest slot is missing, null, or points elsewhere")
    end
  end

  def validate_artifact_status_claim(claim)
    claim_id = claim.fetch("claim_id")
    artifact = read_json(claim.fetch("expected_artifact_path"))
    actual_status = json_pointer(artifact, claim.fetch("status_json_path"))
    expected_status = claim.fetch("expected_artifact_status")
    if actual_status == expected_status
      pass("artifact.status", input_path: claim.fetch("expected_artifact_path"), expected: expected_status, actual: actual_status, claim_id: claim_id)
    else
      fail("artifact.status", input_path: claim.fetch("expected_artifact_path"), expected: expected_status, actual: actual_status, claim_id: claim_id, reason: "artifact status does not match constrained wording state")
    end
    validate_text_terms_claim(claim) if claim.key?("required_terms")
  end

  def validate_text_terms_claim(claim)
    claim_id = claim.fetch("claim_id")
    text = read_text(claim.fetch("surface_path"))
    missing_terms = claim.fetch("required_terms", []).reject { |term| text.include?(term) }
    if missing_terms.empty?
      pass("surface.required_terms", input_path: claim.fetch("surface_path"), expected: claim.fetch("required_terms", []), actual: "present", claim_id: claim_id)
    else
      fail("surface.required_terms", input_path: claim.fetch("surface_path"), expected: claim.fetch("required_terms", []), actual: missing_terms, claim_id: claim_id, reason: "surface text is missing required proof wording")
    end
  end

  def validate_planned_contract_claim(claim)
    claim_id = claim.fetch("claim_id")
    if claim.fetch("wording_state") == "planned"
      pass("planned.state", input_path: @registry_path, expected: "planned", actual: "planned", claim_id: claim_id)
    else
      fail("planned.state", input_path: @registry_path, expected: "planned", actual: claim.fetch("wording_state"), claim_id: claim_id, reason: "planned contract claim is not marked planned")
    end
    validate_text_terms_claim(claim)
  end

  def evaluate_fixture_case(kase)
    observed = kase.fetch("observed")
    reason_codes = []
    verdict = "pass"
    fail_case = lambda do |code|
      verdict = "fail"
      reason_codes << code
    end

    if observed.fetch("artifact_path_null", false)
      fail_case.call("fixture.artifact_path_null")
    end
    if observed.fetch("artifact_present", true) == false
      fail_case.call("fixture.artifact_missing")
    end
    if observed.fetch("release_source_required", false) && observed.fetch("source_mode", nil) != "head"
      fail_case.call("fixture.release_source_not_head")
    end
    if observed.fetch("release_source_required", false) && observed.fetch("dirty_worktree_measurement", false)
      fail_case.call("fixture.dirty_worktree_contaminated")
    end
    if observed.key?("documented_value") && observed.key?("artifact_value") && observed["documented_value"] != observed["artifact_value"]
      fail_case.call("fixture.count_drift")
    end
    if observed.fetch("wording_state", nil) == "planned" && observed.fetch("advertised_as_supported", false)
      fail_case.call("fixture.planned_only_advertised_supported")
    end
    if observed.fetch("wording_state", nil) == "unavailable" && observed.fetch("advertised_as_supported", false)
      fail_case.call("fixture.unsupported_advertised_supported")
    end

    if verdict == "pass"
      reason_codes += ["fixture.claim_fresh", "fixture.head_source", "fixture.artifact_present"]
    end

    { "verdict" => verdict, "reason_codes" => reason_codes.uniq }
  end

  def validate_fixtures(fixtures)
    cases = fixtures.fetch("cases", [])
    case_ids = cases.map { |kase| kase["case_id"] }
    duplicate_case_ids = duplicates(case_ids)
    if duplicate_case_ids.empty?
      pass("fixtures.unique_case_ids", input_path: @fixtures_path, expected: "unique", actual: case_ids.length)
    else
      fail("fixtures.unique_case_ids", input_path: @fixtures_path, expected: "unique", actual: duplicate_case_ids, reason: "duplicate fixture case ids")
    end

    required_cases = %w[
      fresh-head-count
      stale-count
      dirty-worktree-release-count
      missing-attestation-path
      missing-artifact-file
      planned-only-advertised-supported
      unsupported-command-advertised-supported
    ]
    missing_cases = required_cases - case_ids
    if missing_cases.empty?
      pass("fixtures.required_cases", input_path: @fixtures_path, expected: required_cases, actual: case_ids)
    else
      fail("fixtures.required_cases", input_path: @fixtures_path, expected: required_cases, actual: missing_cases, reason: "golden fixture corpus is missing required drift classes")
    end

    cases.each do |kase|
      case_id = kase.fetch("case_id")
      actual = evaluate_fixture_case(kase)
      expected = kase.fetch("expected")
      missing_reason_codes = expected.fetch("reason_codes") - actual.fetch("reason_codes")
      if actual.fetch("verdict") == expected.fetch("verdict") && missing_reason_codes.empty?
        pass("fixtures.expected_verdict", input_path: @fixtures_path, expected: expected, actual: actual, claim_id: case_id)
      else
        fail("fixtures.expected_verdict", input_path: @fixtures_path, expected: expected, actual: actual.merge("missing_reason_codes" => missing_reason_codes), claim_id: case_id, reason: "fixture verdict or reason codes drifted")
      end
    end
  end

  def report(registry, fixtures)
    ok = @diagnostics.empty?
    {
      "ok" => ok,
      "contract_id" => "ft.claim_drift.sentinel_report.v1",
      "registry_path" => @registry_path,
      "fixtures_path" => @fixtures_path,
      "summary" => {
        "claim_count" => registry.fetch("claims", []).length,
        "fixture_case_count" => fixtures.fetch("cases", []).length,
        "check_count" => @checks.length,
        "diagnostic_count" => @diagnostics.length,
      },
      "diagnostics" => @diagnostics,
      "checks" => @checks,
    }
  end
end

sentinel = Sentinel.new(registry_path: registry_path, fixtures_path: fixtures_path)
registry = sentinel.read_json(registry_path)
fixtures = sentinel.read_json(fixtures_path)
sentinel.validate_registry(registry)
sentinel.validate_fixtures(fixtures)
report = sentinel.report(registry, fixtures)

if output_mode == "json"
  puts JSON.pretty_generate(report)
else
  if report.fetch("ok")
    puts "claim-drift sentinel: passed (#{report.dig("summary", "claim_count")} claims, #{report.dig("summary", "fixture_case_count")} fixtures)"
  else
    warn "claim-drift sentinel: failed (#{report.dig("summary", "diagnostic_count")} diagnostics)"
    report.fetch("diagnostics").each do |diagnostic|
      warn "- #{diagnostic["code"]}: #{diagnostic["message"]} (#{diagnostic["input_path"]})"
    end
  end
end

exit 1 if strict && !report.fetch("ok")
RUBY
