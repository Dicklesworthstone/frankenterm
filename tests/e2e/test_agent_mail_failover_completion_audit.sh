#!/usr/bin/env bash
# Static completion audit for the Agent Mail failover Beads graph.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

AUDIT="fixtures/agent-mail-failover/completion-audit.v1.json"

fail() {
  printf 'agent mail failover completion audit: %s\n' "$*" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "missing command: $1"
}

require_file() {
  [[ -f "$1" ]] || fail "missing file: $1"
}

require_command bash
require_command jq
require_command ruby
require_file "${AUDIT}"

jq empty "${AUDIT}"
bash tests/e2e/test_agent_mail_failover_snapshot_contract.sh >/dev/null
bash tests/e2e/test_agent_mail_retry_classifier_contract.sh >/dev/null
bash fixtures/agent-mail-stale-reopen/verify-policy.sh >/dev/null
bash tests/e2e/test_agent_mail_no_service_action_gate.sh >/dev/null
bash tests/e2e/test_agent_mail_failover_runbook_contract.sh >/dev/null

ruby <<'RUBY'
require "json"

AUDIT = "fixtures/agent-mail-failover/completion-audit.v1.json"
ISSUES = ".beads/issues.jsonl"

def fail!(message)
  warn "agent mail failover completion audit: #{message}"
  exit 1
end

def read_json(path)
  JSON.parse(File.read(path))
rescue JSON::ParserError => error
  fail!("#{path} does not parse as JSON: #{error.message}")
end

def issue_map
  File.readlines(ISSUES).each_with_object({}) do |line, out|
    next if line.strip.empty?
    issue = JSON.parse(line)
    out[issue.fetch("id")] = issue
  end
rescue JSON::ParserError => error
  fail!("#{ISSUES} does not parse as JSONL: #{error.message}")
end

def expand_artifact(pattern)
  if pattern.include?("*")
    matches = Dir.glob(pattern).sort
    fail!("artifact glob matched nothing: #{pattern}") if matches.empty?
    matches
  else
    fail!("missing artifact: #{pattern}") unless File.file?(pattern)
    [pattern]
  end
end

audit = read_json(AUDIT)
issues = issue_map

fail!("schema version drifted") unless audit["schema_version"] == 1
fail!("contract id drifted") unless audit["contract_id"] == "ft.agent_mail_failover_completion_audit.v1"
fail!("bead drifted") unless audit["bead"] == "ft-5lsqo.6"
fail!("source epic drifted") unless audit["source_epic"] == "ft-5lsqo"
fail!("runtime command unexpectedly shipped") unless audit["runtime_command_shipped"] == false
fail!("fallback snapshot mode drifted") unless audit.dig("latest_fallback_snapshot", "mode") == "agent_mail_unavailable_beads_only"
fail!("dep cycle evidence drifted") unless audit.dig("graph_evidence", "dep_cycles_count") == 0
fail!("robot blocked-by evidence drifted") unless audit.dig("graph_evidence", "robot_selected_blocked_by") == "ft-5xwsu.3"

expected_side_effect_flags = %w[
  agent_mail_repair_allowed
  agent_mail_restart_allowed
  agent_mail_reconstruct_allowed
  process_kill_allowed
  destructive_git_allowed
  file_deletion_allowed
  worker_mutation_allowed
  build_cancellation_allowed
  local_cargo_proof_allowed
].freeze
side_effect_policy = audit.fetch("side_effect_policy")
# Pin the flag set so an empty or trimmed policy cannot vacuously pass the
# all-false gate below (a missing flag is an undeclared, ungated side-effect).
fail!("side-effect policy flag set drifted: #{side_effect_policy.keys.sort.inspect}") unless side_effect_policy.keys.sort == expected_side_effect_flags.sort
bad_side_effects = side_effect_policy.select { |_key, value| value != false }
fail!("side-effect policy has non-false flags: #{bad_side_effects.keys.inspect}") unless bad_side_effects.empty?

required_commands = audit.fetch("required_closeout_commands")
%w[
  br\ dep\ cycles\ --json
  bv\ --robot-triage\ --robot-next
  scripts/swarm-tick.sh\ --agent-mail-fallback\ frankenterm
].each do |command|
  fail!("missing required closeout command #{command}") unless required_commands.include?(command)
end

ft_5lsqo_6 = issues.fetch("ft-5lsqo.6") { fail!("missing ft-5lsqo.6 in Beads") }
dependency_ids = ft_5lsqo_6.fetch("dependencies", []).map { |dependency| dependency.fetch("depends_on_id") }.sort
missing_edges = audit.fetch("required_dependency_edges").sort - dependency_ids
fail!("ft-5lsqo.6 missing dependency edges: #{missing_edges.inspect}") unless missing_edges.empty?

artifact_count = 0
verifier_count = 0

audit.fetch("children").each do |child|
  id = child.fetch("id")
  issue = issues.fetch(id) { fail!("missing child issue #{id}") }
  fail!("#{id} is not closed") unless issue.fetch("status") == child.fetch("expected_status")
  fail!("#{id} missing close reason") if issue.fetch("close_reason", "").strip.empty?
  fail!("#{id} proof level is not static") unless child.fetch("proof_level") == "static"
  fail!("#{id} still has blocker #{child.fetch("blocker")}") unless child.fetch("blocker") == "none"
  fail!("#{id} safety posture missing") if child.fetch("safety_posture").strip.empty?

  child.fetch("artifacts").each do |artifact|
    artifact_count += expand_artifact(artifact).length
  end

  child.fetch("verifiers").each do |verifier|
    verifier_count += 1
    path = verifier.split.last
    fail!("#{id} verifier target missing: #{path}") unless File.file?(path)
  end
end

log = {
  "contract_id" => audit.fetch("contract_id"),
  "children_checked" => audit.fetch("children").length,
  "artifact_count" => artifact_count,
  "verifier_count" => verifier_count,
  "fallback_mode" => audit.fetch("latest_fallback_snapshot").fetch("mode"),
  "dep_cycles_count" => audit.fetch("graph_evidence").fetch("dep_cycles_count"),
  "verdict" => "pass"
}

puts JSON.generate(log)
RUBY
