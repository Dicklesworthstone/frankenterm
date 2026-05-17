#!/usr/bin/env bash
# Static verifier for scale-lab summary fixtures consumed by docs and Rust tests.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

FIXTURE_DIR="fixtures/scale-lab"
JSON_FIXTURES=(
  "${FIXTURE_DIR}/adaptive-capture-tier-summary.v1.json"
  "${FIXTURE_DIR}/agent-liveness-summary.v1.json"
  "${FIXTURE_DIR}/digital-twin-trace-summary.v1.json"
  "${FIXTURE_DIR}/policy-recommendation-summary.v1.json"
  "${FIXTURE_DIR}/resource-digital-twin-gate-summary.v1.json"
  "${FIXTURE_DIR}/resource-what-if-contracts.v1.json"
  "${FIXTURE_DIR}/resource-what-if-trace.v1.json"
  "${FIXTURE_DIR}/storage-index-heatmap-summary.v1.json"
  "${FIXTURE_DIR}/workload-catalog-smoke.v1.json"
)
TOON_FIXTURES=(
  "${FIXTURE_DIR}/agent-liveness-summary.v1.toon"
  "${FIXTURE_DIR}/digital-twin-trace-summary.v1.toon"
  "${FIXTURE_DIR}/policy-recommendation-summary.v1.toon"
  "${FIXTURE_DIR}/storage-index-heatmap-summary.v1.toon"
)

fail() {
  printf 'scale-lab summary fixtures: %s\n' "$*" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "missing command: $1"
}

require_file() {
  [[ -f "$1" ]] || fail "missing file: $1"
}

require_command jq
require_command ruby

for path in "${JSON_FIXTURES[@]}" "${TOON_FIXTURES[@]}"; do
  require_file "${path}"
done

jq empty "${JSON_FIXTURES[@]}"

ruby <<'RUBY'
require "json"
require "set"

FIXTURE_DIR = "fixtures/scale-lab"
TARGET_CPU_COUNT = 64
TARGET_MEMORY_BYTES = 274_877_906_944

def fail!(message)
  warn "scale-lab summary fixtures: #{message}"
  exit 1
end

def json_fixture(name)
  path = "#{FIXTURE_DIR}/#{name}"
  JSON.parse(File.read(path))
rescue JSON::ParserError => error
  fail!("#{path} does not parse as JSON: #{error.message}")
end

def toon_fixture(name)
  File.read("#{FIXTURE_DIR}/#{name}")
end

def require_integer(object, key, context)
  value = object.fetch(key) { fail!("#{context} missing #{key}") }
  fail!("#{context} #{key} must be an integer") unless value.is_a?(Integer)
  value
end

def require_string(object, key, context)
  value = object.fetch(key) { fail!("#{context} missing #{key}") }
  fail!("#{context} #{key} must be a non-empty string") unless value.is_a?(String) && !value.empty?
  value
end

def require_array(object, key, context)
  value = object.fetch(key) { fail!("#{context} missing #{key}") }
  fail!("#{context} #{key} must be a non-empty array") unless value.is_a?(Array) && !value.empty?
  value
end

def assert_exact_set(values, expected, context)
  fail!("#{context} drifted: #{values.sort.inspect}") unless values.to_set == expected.to_set
  fail!("#{context} has duplicates") unless values.length == values.uniq.length
end

def assert_toon_contains(toon, fragments, context)
  fragments.each do |fragment|
    fail!("#{context} TOON missing #{fragment.inspect}") unless toon.include?(fragment)
  end
end

adaptive = json_fixture("adaptive-capture-tier-summary.v1.json")
fail!("adaptive schema drifted") unless adaptive["schema_version"] == 1
pane_total = %w[hot warm cold deferred].sum { |key| require_integer(adaptive, key, "adaptive") }
search_total = %w[search_full_realtime search_buffered_catchup search_summary_only search_deferred_with_gap].sum do |key|
  require_integer(adaptive, key, "adaptive")
end
fail!("adaptive pane total drifted") unless pane_total == adaptive["total_panes"]
fail!("adaptive search total drifted") unless search_total == adaptive["total_panes"]
fail!("adaptive degraded receipts exceed pane total") if adaptive["degraded_receipts"] > adaptive["total_panes"]
fail!("adaptive operator summary drifted") unless adaptive["operator_summary"].include?("adaptive_capture_indexing")

agent = json_fixture("agent-liveness-summary.v1.json")
agent_toon = toon_fixture("agent-liveness-summary.v1.toon")
fail!("agent schema drifted") unless agent["schema_version"] == 1
agent_class_total = %w[alive quiet_but_active unreachable stale_claim conflicting_claim recovery_needed].sum do |key|
  require_integer(agent, key, "agent liveness")
end
agent_action_total = %w[wait ping reopen reassign escalate_human].sum { |key| require_integer(agent, key, "agent liveness") }
fail!("agent class total drifted") unless agent_class_total == agent["total_agents"]
fail!("agent action total drifted") unless agent_action_total == agent["total_agents"]
fail!("agent automatic reopen must stay policy-gated") unless agent["automatic_reopen_allowed"] == 0
assert_toon_contains(
  agent_toon,
  [
    "schema_version: #{agent["schema_version"]}",
    "total_agents: #{agent["total_agents"]}",
    "alive=#{agent["alive"]}",
    "quiet_but_active=#{agent["quiet_but_active"]}",
    "stale_claim=#{agent["stale_claim"]}",
    "escalate_human=#{agent["escalate_human"]}",
    "automatic_reopen_allowed: #{agent["automatic_reopen_allowed"]}",
    agent["operator_summary"],
  ],
  "agent liveness",
)

policy = json_fixture("policy-recommendation-summary.v1.json")
policy_toon = toon_fixture("policy-recommendation-summary.v1.toon")
fail!("policy schema drifted") unless policy["schema_version"] == 1
policy_outcome_total = %w[allow deny require_approval delay degrade ask_human].sum do |key|
  require_integer(policy, key, "policy recommendations")
end
fail!("policy outcome total drifted") unless policy_outcome_total == policy["total_candidates"]
fail!("policy must not issue approval tokens in fixture") unless policy["issued_approval_tokens"] == 0
fail!("policy must not execute actions in fixture") unless policy["executed_actions"] == 0
fail!("policy redaction coverage disappeared") unless policy["redacted_evidence_fields"].positive?
assert_toon_contains(
  policy_toon,
  [
    "schema_version: #{policy["schema_version"]}",
    "total_candidates: #{policy["total_candidates"]}",
    "allow=#{policy["allow"]}",
    "require_approval=#{policy["require_approval"]}",
    "ask_human=#{policy["ask_human"]}",
    "executed_actions: #{policy["executed_actions"]}",
    policy["operator_summary"],
  ],
  "policy recommendations",
)

storage = json_fixture("storage-index-heatmap-summary.v1.json")
storage_toon = toon_fixture("storage-index-heatmap-summary.v1.toon")
fail!("storage schema drifted") unless storage["schema_version"] == 1
heat_total = %w[cool warm hot saturated].sum { |key| require_integer(storage, key, "storage heatmap") }
admission_total = %w[run_now defer throttle shard mark_coverage_degraded].sum do |key|
  require_integer(storage, key, "storage heatmap")
end
fail!("storage heat total drifted") unless heat_total == storage["total_workloads"]
fail!("storage admission total drifted") unless admission_total == storage["total_workloads"]
fail!("storage freshness lag should stay explicit") unless storage["max_estimated_freshness_lag_ms"].positive?
assert_toon_contains(
  storage_toon,
  [
    "schema_version: #{storage["schema_version"]}",
    "total_workloads: #{storage["total_workloads"]}",
    "cool=#{storage["cool"]}",
    "saturated=#{storage["saturated"]}",
    "mark_coverage_degraded=#{storage["mark_coverage_degraded"]}",
    "max_estimated_freshness_lag_ms: #{storage["max_estimated_freshness_lag_ms"]}",
    storage["operator_summary"],
  ],
  "storage heatmap",
)

trace = json_fixture("digital-twin-trace-summary.v1.json")
trace_toon = toon_fixture("digital-twin-trace-summary.v1.toon")
fail!("digital twin trace schema drifted") unless trace["schema_version"] == "ft.digital_twin_trace.v1"
fail!("digital twin trace hash must be sha256-shaped") unless trace["trace_hash"].match?(/\A[0-9a-f]{64}\z/)
steps = require_array(trace, "steps", "digital twin trace")
fail!("digital twin trace step count drifted") unless steps.length == 4
fail!("digital twin trace monotonic order drifted") unless steps.map { |step| step["monotonic_ms"] } == steps.map { |step| step["monotonic_ms"] }.sort
assert_exact_set(steps.map { |step| step["step_id"] }, %w[healthy pressured degraded missing_telemetry], "digital twin trace step ids")
steps.each do |step|
  require_string(step, "source_hash", "digital twin trace step #{step["step_id"]}")
  require_array(step, "source_artifact_hashes", "digital twin trace step #{step["step_id"]}")
end
fail!("digital twin trace must retain quality flags") unless trace["quality_flags"].include?("derived_source_hash")
assert_toon_contains(
  trace_toon,
  [
    "schema_version: #{trace["schema_version"]}",
    "trace_hash: #{trace["trace_hash"]}",
    "steps[#{steps.length}]",
    "id=healthy",
    "id=missing_telemetry",
    "derived_source_hash",
  ],
  "digital twin trace",
)

resource_gate = json_fixture("resource-digital-twin-gate-summary.v1.json")
fail!("resource gate schema drifted") unless resource_gate["schema_version"] == "ft.resource_digital_twin.gate_summary.v1"
gate_cases = require_array(resource_gate, "cases", "resource gate")
assert_exact_set(gate_cases.map { |case_entry| case_entry["case_id"] }, %w[pass warning blocked skipped_not_proven], "resource gate cases")
gate_cases.each do |case_entry|
  case_id = case_entry["case_id"]
  fail!("resource gate #{case_id} missing blocker_codes") unless case_entry["blocker_codes"].is_a?(Array)
  if case_entry["proof_status"] == "SKIPPED_NOT_PROVEN"
    fail!("resource gate skipped case must fail closed") unless case_entry["hardware_predicate"] == "skipped_not_proven" &&
      case_entry["high_scale_claim_allowed"] == false &&
      case_entry["blocker_codes"].include?("proof_skipped_not_proven")
  else
    fail!("resource gate proven case must keep predicate") unless case_entry["proof_status"] == "PASSED" &&
      case_entry["hardware_predicate"] == "proven_predicate_met" &&
      case_entry["high_scale_claim_allowed"] == true
  end
end

contracts = json_fixture("resource-what-if-contracts.v1.json")
fail!("resource what-if contract schema drifted") unless contracts["schema_version"] == "ft.resource_what_if.contracts.v1"
contract_cases = require_array(contracts, "cases", "resource what-if contracts")
assert_exact_set(contract_cases.map { |case_entry| case_entry["case_id"] }, %w[pass warning blocked missing_telemetry], "resource what-if contract cases")
required_json_fields = %w[schema_version trace_hash override_hash decision_deltas risk_score confidence_score proof_status next_proof_steps]
required_human_prefixes = [
  "Resource what-if:",
  "Confidence:",
  "Risk:",
  "Proof:",
  "Deltas:",
  "Top improvements:",
  "Top regressions:",
  "Next proof:",
  "Simulation:",
  "Dry-run:",
]
contract_cases.each do |case_entry|
  context = "resource what-if contract #{case_entry["case_id"]}"
  fail!("#{context} JSON fields drifted") unless case_entry["json_required_fields"] == required_json_fields
  fail!("#{context} TOON fields drifted") unless (required_json_fields - case_entry["toon_required_fragments"]).empty?
  fail!("#{context} human prefixes drifted") unless case_entry["human_line_prefixes"] == required_human_prefixes
  fail!("#{context} proof status drifted") unless case_entry["proof_status"] == "PASSED"
  fail!("#{context} high-scale gate drifted") unless case_entry["high_scale_claim_allowed"] == true
end

what_if_trace = json_fixture("resource-what-if-trace.v1.json")
fail!("resource what-if trace schema drifted") unless what_if_trace["schema_version"] == "ft.digital_twin_trace.v1"
what_if_steps = require_array(what_if_trace, "steps", "resource what-if trace")
assert_exact_set(
  what_if_steps.map { |step| step["step_id"] },
  %w[proof-live admission-burst admission-stable memory-stable],
  "resource what-if trace steps",
)
proof_step = what_if_steps.find { |step| step["step_id"] == "proof-live" }
fail!("resource what-if trace proof step must be live hardware") unless proof_step["proof_status"] == "PASSED" &&
  proof_step["evidence_source"] == "live_hardware" &&
  proof_step["hardware_evidence_complete"] == true &&
  proof_step["hardware_cpu_count"] >= TARGET_CPU_COUNT &&
  proof_step["hardware_memory_bytes"] >= TARGET_MEMORY_BYTES

catalog = json_fixture("workload-catalog-smoke.v1.json")
fail!("workload catalog schema drifted") unless catalog["schema_version"] == "ft.scale_lab.workload_catalog.v1"
fail!("workload catalog id drifted") unless catalog["catalog_id"] == "ft-s6h49.scale-lab-smoke"
fail!("workload catalog evidence mode drifted") unless catalog["evidence_mode"] == "rch_replay"
mix = require_array(catalog, "workload_mix", "workload catalog")
pane_count = mix.sum { |entry| require_integer(entry, "pane_count", "workload #{entry["persona"]}") }
fail!("workload catalog pane count drifted") unless pane_count == catalog["target_pane_count"]
fail!("workload catalog must stay below target-class CPU") unless catalog.dig("host", "cpu_cores") < TARGET_CPU_COUNT
fail!("workload catalog must stay below target-class memory") unless catalog.dig("host", "memory_gib") < 256
fail!("workload catalog must not claim live mux") unless catalog.dig("host", "live_mux_available") == false
fail!("workload command must stay RCH-offloaded") unless catalog.dig("command", "command_line").start_with?("rch exec --")
fail!("workload catalog should use no-default-features") unless catalog.dig("command", "feature_flags", "default_features") == false
fail!("workload events must stay lossless") unless catalog.dig("events", "dropped_events") == 0 &&
  catalog.dig("events", "capture_gaps") == 0
fail!("workload memory exceeds limit") unless catalog.dig("memory", "peak_rss_bytes") < catalog.dig("memory", "memory_limit_bytes")
fail!("workload limitations must reject high-scale graduation") unless catalog["limitations"].any? { |line| line.include?("cannot graduate larger support claims") }
catalog["artifacts"].each do |artifact|
  fail!("workload artifact path must be relative") if artifact["path"].start_with?("/")
  fail!("workload artifact sha drifted") unless artifact["sha256"].match?(/\A[0-9a-f]{64}\z/)
  fail!("workload artifact must remain redacted") unless artifact["redacted"] == true
end
RUBY

printf 'scale-lab summary fixtures: static verifier passed (%d json fixtures, %d toon fixtures)\n' \
  "${#JSON_FIXTURES[@]}" \
  "${#TOON_FIXTURES[@]}"
