#!/usr/bin/env bash
# Static verifier for the resource what-if proof manifest and retained fixtures.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

MANIFEST="fixtures/scale-lab/resource-what-if-proof/manifest.v1.json"
RUNBOOK="docs/high-core-swarm-runbook.md"
PROOF_DIR="fixtures/scale-lab/resource-what-if-proof"
REQUIRED_CASES=(
  "cpu_queue_saturated_replay"
  "healthy_live_hardware"
  "mcp_search_stalled_replay"
  "memory_oversubscription_candidate"
  "memory_tier_pressured_replay"
  "policy_audit_unavailable_replay"
  "policy_fail_open_candidate"
  "topology_churn_candidate"
  "topology_degraded_replay"
)

fail() {
  printf 'resource what-if proof manifest: %s\n' "$*" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "missing command: $1"
}

require_file() {
  local path="$1"
  [[ -f "${path}" ]] || fail "missing file: ${path}"
}

require_repo_relative_path() {
  local path="$1"

  [[ -n "${path}" ]] || fail "empty path"
  [[ "${path}" != /* ]] || fail "absolute path is forbidden: ${path}"
  [[ "${path}" != *'..'* ]] || fail "parent-relative path is forbidden: ${path}"
}

require_command jq
require_command ruby
require_command shasum

require_file "${MANIFEST}"
require_file "${RUNBOOK}"

mapfile -t trace_paths < <(jq -r '.cases[].trace' "${MANIFEST}" | sort -u)
mapfile -t override_paths < <(jq -r '.cases[].override_package' "${MANIFEST}" | sort -u)
mapfile -t proof_json_paths < <(find "${PROOF_DIR}" -maxdepth 1 -type f -name '*.json' | sort)

all_json=("${MANIFEST}" "${trace_paths[@]}")
for path in "${all_json[@]}" "${override_paths[@]}" "${proof_json_paths[@]}"; do
  require_repo_relative_path "${path}"
  require_file "${path}"
done

jq empty "${all_json[@]}"

ruby <<'RUBY'
require "digest"
require "json"
require "set"

MANIFEST = "fixtures/scale-lab/resource-what-if-proof/manifest.v1.json"
RUNBOOK = "docs/high-core-swarm-runbook.md"
REQUIRED_CASES = %w[
  cpu_queue_saturated_replay
  healthy_live_hardware
  mcp_search_stalled_replay
  memory_oversubscription_candidate
  memory_tier_pressured_replay
  policy_audit_unavailable_replay
  policy_fail_open_candidate
  topology_churn_candidate
  topology_degraded_replay
].freeze
EXPECTED_FIXTURE_CLASSES = {
  "cpu_queue_saturated_replay" => "cpu_queue_saturated",
  "healthy_live_hardware" => "healthy",
  "mcp_search_stalled_replay" => "mcp_search_stalled",
  "memory_oversubscription_candidate" => "failure_oriented_memory",
  "memory_tier_pressured_replay" => "memory_tier_pressured",
  "policy_audit_unavailable_replay" => "policy_audit_unavailable",
  "policy_fail_open_candidate" => "failure_oriented_policy",
  "topology_churn_candidate" => "failure_oriented_topology",
  "topology_degraded_replay" => "topology_degraded"
}.freeze
EXPECTED_PROOF_CLASSES = {
  "healthy_live_hardware" => "live_hardware",
  "memory_oversubscription_candidate" => "live_hardware_failure_candidate",
  "cpu_queue_saturated_replay" => "replay_backed_reduced",
  "memory_tier_pressured_replay" => "replay_backed_reduced",
  "topology_degraded_replay" => "replay_backed_reduced",
  "mcp_search_stalled_replay" => "replay_backed_reduced",
  "policy_audit_unavailable_replay" => "replay_backed_reduced",
  "topology_churn_candidate" => "replay_backed_failure_candidate",
  "policy_fail_open_candidate" => "replay_backed_failure_candidate"
}.freeze
REPLAY_PROOF_CLASSES = %w[replay_backed_reduced replay_backed_failure_candidate].freeze

def fail!(message)
  warn "resource what-if proof manifest: #{message}"
  exit 1
end

def repo_relative!(path)
  fail!("empty path") if path.nil? || path.empty?
  fail!("absolute path is forbidden: #{path}") if path.start_with?("/")
  fail!("parent-relative path is forbidden: #{path}") if path.include?("..")
end

def read_json(path)
  JSON.parse(File.read(path))
rescue JSON::ParserError => error
  fail!("#{path} does not parse as JSON: #{error.message}")
end

def require_string(object, field, context)
  value = object[field]
  fail!("#{context} missing #{field}") unless value.is_a?(String) && !value.empty?
  value
end

def require_array(object, field, context)
  value = object[field]
  fail!("#{context} missing #{field}") unless value.is_a?(Array) && !value.empty?
  value
end

def assert_label(value, allowed, context)
  fail!("#{context} has unexpected value #{value.inspect}") unless allowed.include?(value)
end

manifest = read_json(MANIFEST)
fail!("schema version drifted") unless manifest["schema_version"] == "ft.resource_what_if.proof_manifest.v1"
fail!("minimum_cpu_count must be 64") unless manifest["minimum_cpu_count"] == 64
fail!("minimum_memory_bytes must be 256 GiB") unless manifest["minimum_memory_bytes"] == 274_877_906_944
repo_relative!(manifest["runbook"])
fail!("runbook path drifted") unless manifest["runbook"] == "#{RUNBOOK}#resource-what-if-proof"
fail!("runbook does not mention resource what-if proof") unless File.read(RUNBOOK).include?("## Resource What-If Proof")

cases = require_array(manifest, "cases", "manifest")
case_ids = cases.map { |case_entry| require_string(case_entry, "case_id", "case") }
fail!("case ids drifted: #{case_ids.sort.inspect}") unless case_ids.sort == REQUIRED_CASES.sort
fail!("case ids are not unique") unless case_ids.uniq.length == case_ids.length

live_hardware_cases = 0
replay_blocked_cases = 0
failure_oriented_cases = 0

cases.each do |case_entry|
  case_id = require_string(case_entry, "case_id", "case")
  context = "case #{case_id}"
  fixture_class = require_string(case_entry, "fixture_class", context)
  proof_class = require_string(case_entry, "proof_classification", context)
  fail!("#{context} fixture class drifted") unless fixture_class == EXPECTED_FIXTURE_CLASSES.fetch(case_id)
  fail!("#{context} proof class drifted") unless proof_class == EXPECTED_PROOF_CLASSES.fetch(case_id)

  trace_path = require_string(case_entry, "trace", context)
  override_path = require_string(case_entry, "override_package", context)
  repo_relative!(trace_path)
  repo_relative!(override_path)
  fail!("#{context} trace missing: #{trace_path}") unless File.file?(trace_path)
  fail!("#{context} override package missing: #{override_path}") unless File.file?(override_path)

  trace = read_json(trace_path)
  fail!("#{context} trace schema drifted") unless trace["schema_version"] == "ft.digital_twin_trace.v1"
  fail!("#{context} trace hash drifted") unless trace["trace_hash"] == case_entry["trace_hash"]
  require_array(trace, "source_artifact_hashes", "#{context} trace")
  steps = require_array(trace, "steps", "#{context} trace")
  steps.each do |step|
    require_string(step, "step_id", "#{context} trace step")
    require_string(step, "source_hash", "#{context} trace step")
    require_array(step, "source_artifact_hashes", "#{context} trace step")
  end

  override_body = File.read(override_path)
  fail!("#{context} override schema missing") unless override_body.include?('schema_version = "ft.resource_control_override.v1"')
  fail!("#{context} override name missing") unless override_body.match?(/^name = ".+"/)
  override_hash = Digest::SHA256.hexdigest(override_body)
  fail!("#{context} override hash drifted") unless override_hash == case_entry["override_hash"]

  golden = case_entry["golden_report"]
  fail!("#{context} missing golden report") unless golden.is_a?(Hash)
  fail!("#{context} golden schema drifted") unless golden["schema_version"] == "ft.resource_what_if.v1"
  fail!("#{context} must be dry-run") unless golden["dry_run"] == true
  fail!("#{context} must not expose mutation surface") unless golden["mutation_surface"] == []
  assert_label(golden["proof_status"], %w[PASSED SKIPPED_NOT_PROVEN], "#{context} proof_status")
  assert_label(golden["proof_evidence_source"], %w[live_hardware replay_backed], "#{context} proof_evidence_source")
  assert_label(golden["hardware_predicate"], %w[proven_predicate_met skipped_not_proven], "#{context} hardware_predicate")
  fail!("#{context} high-scale gate must be boolean") unless [true, false].include?(golden["high_scale_claim_allowed"])

  risk_codes = golden["required_risk_codes"]
  apply_codes = golden["required_apply_reason_codes"]
  fail!("#{context} required_risk_codes must be an array") unless risk_codes.is_a?(Array)
  fail!("#{context} required_apply_reason_codes must be an array") unless apply_codes.is_a?(Array)
  risk_codes.each { |code| fail!("#{context} has empty risk code") unless code.is_a?(String) && !code.empty? }
  apply_codes.each { |code| fail!("#{context} has empty apply reason code") unless code.is_a?(String) && !code.empty? }

  transcript = case_entry["command_transcript"]
  fail!("#{context} missing command transcript") unless transcript.is_a?(Hash)
  human = require_string(transcript, "human_json", "#{context} transcript")
  robot = require_string(transcript, "robot_toon", "#{context} transcript")
  fail!("#{context} human transcript is not replayable") unless human.include?("ft resource what-if") &&
    human.include?("--trace #{trace_path}") &&
    human.include?("--override-package #{override_path}") &&
    human.include?("--format json")
  fail!("#{context} robot transcript is not replayable") unless robot.include?("ft robot --format toon resource what-if") &&
    robot.include?("--trace #{trace_path}") &&
    robot.include?("--override-package #{override_path}")

  if proof_class == "live_hardware"
    live_hardware_cases += 1
    fail!("#{context} live proof must pass") unless golden["proof_status"] == "PASSED"
    fail!("#{context} live proof source drifted") unless golden["proof_evidence_source"] == "live_hardware"
    fail!("#{context} live proof predicate drifted") unless golden["hardware_predicate"] == "proven_predicate_met"
    fail!("#{context} live proof must allow high-scale claim") unless golden["high_scale_claim_allowed"] == true
    proof_step = steps.find { |step| step["proof_status"] == "PASSED" && step["evidence_source"] == "live_hardware" }
    fail!("#{context} missing live hardware proof step") unless proof_step
    fail!("#{context} incomplete hardware evidence") unless proof_step["hardware_evidence_complete"] == true
    fail!("#{context} CPU predicate below minimum") unless proof_step["hardware_cpu_count"].to_i >= manifest["minimum_cpu_count"]
    fail!("#{context} memory predicate below minimum") unless proof_step["hardware_memory_bytes"].to_i >= manifest["minimum_memory_bytes"]
  elsif REPLAY_PROOF_CLASSES.include?(proof_class)
    replay_blocked_cases += 1
    fail!("#{context} replay proof must stay skipped") unless golden["proof_status"] == "SKIPPED_NOT_PROVEN"
    fail!("#{context} replay proof source drifted") unless golden["proof_evidence_source"] == "replay_backed"
    fail!("#{context} replay predicate drifted") unless golden["hardware_predicate"] == "skipped_not_proven"
    fail!("#{context} replay proof must not allow high-scale claim") unless golden["high_scale_claim_allowed"] == false
    fail!("#{context} replay proof must pin proof_skipped_not_proven") unless apply_codes.include?("proof_skipped_not_proven")
  elsif proof_class == "live_hardware_failure_candidate"
    live_hardware_cases += 1
    fail!("#{context} failure candidate must still prove live predicate") unless golden["proof_status"] == "PASSED" &&
      golden["proof_evidence_source"] == "live_hardware" &&
      golden["hardware_predicate"] == "proven_predicate_met" &&
      golden["high_scale_claim_allowed"] == true
  end

  if fixture_class.start_with?("failure_oriented")
    failure_oriented_cases += 1
    fail!("#{context} failure-oriented fixture must pin risk code") if risk_codes.empty?
    fail!("#{context} failure-oriented fixture must pin apply reason") if apply_codes.empty?
  end
end

fail!("expected two live-hardware cases") unless live_hardware_cases == 2
fail!("expected seven replay-backed skipped cases") unless replay_blocked_cases == 7
fail!("expected three failure-oriented cases") unless failure_oriented_cases == 3

# Cross-cutting anti-overclaim invariant, independent of the case->class map:
# a high-scale capability claim is legitimate ONLY behind a PASSED live-hardware
# predicate. The per-case branches above enforce this via the proof_class label;
# this guard asserts it directly on the golden fields, so it still bites if
# EXPECTED_PROOF_CLASSES is later mis-edited to relabel a replay case as live.
high_scale_ok = lambda do |golden|
  golden["high_scale_claim_allowed"] != true ||
    (golden["proof_status"] == "PASSED" &&
      golden["proof_evidence_source"] == "live_hardware" &&
      golden["hardware_predicate"] == "proven_predicate_met")
end
cases.each do |case_entry|
  golden = case_entry["golden_report"]
  unless high_scale_ok.call(golden)
    fail!("#{case_entry["case_id"]} claims high-scale without a passed live-hardware predicate")
  end
end
# Prove the guard actually bites: a replay-backed golden that flips the claim to
# true (without live evidence) must be rejected.
replay_case = cases.find { |entry| REPLAY_PROOF_CLASSES.include?(entry["proof_classification"]) }
fail!("no replay-backed case available to tamper") unless replay_case
tampered_golden = Marshal.load(Marshal.dump(replay_case["golden_report"]))
tampered_golden["high_scale_claim_allowed"] = true
if high_scale_ok.call(tampered_golden)
  fail!("high-scale anti-overclaim guard failed to bite on a tampered replay golden")
end
RUBY

printf 'resource what-if proof manifest: static verifier passed (%s cases, %s traces, %s override packages)\n' \
  "$(jq '.cases | length' "${MANIFEST}")" \
  "${#trace_paths[@]}" \
  "${#override_paths[@]}"
