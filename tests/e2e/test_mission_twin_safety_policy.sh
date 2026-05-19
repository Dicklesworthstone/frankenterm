#!/usr/bin/env bash
# Static verifier for mission-twin privacy, retention, and side-effect policy fixtures.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

DOC="docs/mission-twin-privacy-safety.md"
SCHEMA="docs/json-schema/ft-mission-twin-safety-policy.json"
POLICY="fixtures/mission-twin/safety-policy/valid/policy.v1.json"
INVALID="fixtures/mission-twin/safety-policy/invalid/fragments.v1.json"

fail() {
  printf 'mission twin safety policy: %s\n' "$*" >&2
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
require_command git
require_file "${DOC}"
require_file "${SCHEMA}"
require_file "${POLICY}"
require_file "${INVALID}"

jq empty "${SCHEMA}" "${POLICY}" "${INVALID}"

ruby <<'RUBY'
require "json"
require "set"

DOC = "docs/mission-twin-privacy-safety.md"
SCHEMA = "docs/json-schema/ft-mission-twin-safety-policy.json"
POLICY = "fixtures/mission-twin/safety-policy/valid/policy.v1.json"
INVALID = "fixtures/mission-twin/safety-policy/invalid/fragments.v1.json"
EXPECTED_FORBIDDEN = %w[
  agent_mail_service_repair_restart
  rch_service_repair_restart
  worker_mutation
  build_cancellation
  file_deletion
  destructive_git
  local_cargo_proof
  pane_mutation
  raw_pane_content_storage
  beads_mutation
].freeze
EXPECTED_FAILURES = %w[
  raw-pane-content-stored
  destructive-suggestion
  missing-forbidden-action
  live-permission-confused
  unsafe-artifact-path
].freeze

def fail!(message)
  warn "mission twin safety policy: #{message}"
  exit 1
end

def read_json(path)
  JSON.parse(File.read(path))
rescue JSON::ParserError => error
  fail!("#{path} does not parse as JSON: #{error.message}")
end

def safe_repo_relative_path?(path)
  return false unless path.is_a?(String) && !path.empty?
  return false if path == "." || path == ".."
  return false if path.start_with?("/", "./", "../")
  return false if path.end_with?("/")
  return false if path.include?("\\")

  segments = path.split("/", -1)
  return false if segments.any?(&:empty?)
  return false if segments.any? { |segment| segment == "." || segment == ".." || segment == ".git" }

  true
end

schema = read_json(SCHEMA)
policy = read_json(POLICY)
invalid = read_json(INVALID)
doc = File.read(DOC)

fail!("schema id drifted") unless schema["$id"]&.end_with?("/ft-mission-twin-safety-policy.json")
fail!("schema contract const drifted") unless schema.dig("properties", "contract_id", "const") == "ft.mission_twin_safety_policy.v1"
fail!("schema source bead const drifted") unless schema.dig("properties", "source_bead", "const") == "ft-u7r37.7"
fail!("schema simulation_only const missing") unless schema.dig("properties", "simulation_only", "const") == true
fail!("schema live mutation const missing") unless schema.dig("properties", "live_mutation_authority", "const") == false
fail!("schema raw pane const missing") unless schema.dig("properties", "raw_pane_content_stored", "const") == false
fail!("schema forbidden enum drifted") unless schema.dig("$defs", "forbidden_action", "enum").sort == EXPECTED_FORBIDDEN.sort

fail!("policy schema version drifted") unless policy["schema_version"] == 1
fail!("policy contract id drifted") unless policy["contract_id"] == "ft.mission_twin_safety_policy.v1"
fail!("policy source bead drifted") unless policy["source_bead"] == "ft-u7r37.7"
fail!("policy must be simulation only") unless policy["simulation_only"] == true
fail!("policy must not grant live mutation authority") unless policy["live_mutation_authority"] == false
fail!("policy must not retain raw pane content") unless policy["raw_pane_content_stored"] == false
fail!("policy forbidden action set drifted") unless policy.fetch("forbidden_actions").sort == EXPECTED_FORBIDDEN.sort

policy.fetch("artifact_paths").each do |path|
  fail!("unsafe policy artifact path: #{path}") unless safe_repo_relative_path?(path)
  fail!("policy artifact path missing file: #{path}") unless File.file?(path)
  system("git", "ls-files", "--error-unmatch", "--", path, out: File::NULL, err: File::NULL)
  fail!("policy artifact path is not tracked: #{path}") unless $?.success?
end

policy.fetch("retention_policy").each do |surface, rule|
  fail!("#{surface} retention may contain raw content") unless rule.fetch("max_contains_raw_content") == false
  forbidden = rule.fetch("forbidden_fields")
  fail!("#{surface} retention does not forbid raw pane text") if surface == "snapshots" && !forbidden.include?("raw_pane_text")
end

policy.fetch("failure_behaviors").each do |name, behavior|
  fail!("#{name} failure behavior must fail closed") unless behavior.fetch("fail_closed") == true
  fail!("#{name} reason code drifted") unless behavior.fetch("reason_code").start_with?("mission_twin.")
end
fail!("unredacted input must be rejected") unless policy.dig("failure_behaviors", "unredacted", "outcome") == "reject"

requirements = policy.fetch("output_requirements")
requirements.each do |name, value|
  fail!("output requirement #{name} must be true") unless value == true
end

EXPECTED_FORBIDDEN.each do |action|
  fail!("doc missing forbidden action #{action}") unless doc.include?("`#{action}`")
end
fail!("doc missing simulation-only wording") unless doc.include?("simulation surface")
fail!("doc missing static verifier pointer") unless doc.include?("tests/e2e/test_mission_twin_safety_policy.sh")

fail!("invalid schema version drifted") unless invalid["schema_version"] == 1
fail!("invalid contract id drifted") unless invalid["contract_id"] == "ft.mission_twin_safety_policy.invalid_fragments.v1"
fail!("invalid valid_fixture drifted") unless invalid["valid_fixture"] == POLICY
invalid_cases = invalid.fetch("cases")
case_ids = invalid_cases.map { |entry| entry.fetch("case_id") }
fail!("invalid case coverage drifted: #{case_ids.sort.inspect}") unless case_ids.sort == EXPECTED_FAILURES.sort
fail!("invalid case ids are not unique") unless case_ids.uniq.length == case_ids.length

by_id = invalid_cases.to_h { |entry| [entry.fetch("case_id"), entry] }
raw = by_id.fetch("raw-pane-content-stored")
fail!("raw-pane expected failure drifted") unless raw["expected_failure"] == "raw_pane_content_must_not_be_retained"
fail!("raw-pane fragment drifted") unless raw.dig("invalid_fragment", "raw_pane_content_stored") == true

destructive = by_id.fetch("destructive-suggestion")
fail!("destructive case missing file_deletion marker") unless destructive.dig("invalid_fragment", "forbidden_actions").include?("file_deletion")

missing = by_id.fetch("missing-forbidden-action")
fail!("missing forbidden action fragment drifted") unless missing.dig("invalid_fragment", "omitted_forbidden_action") == "local_cargo_proof"

authority = by_id.fetch("live-permission-confused")
fail!("live permission fragment drifted") unless authority.dig("invalid_fragment", "simulation_only") == false &&
  authority.dig("invalid_fragment", "live_mutation_authority") == true

unsafe_paths = by_id.fetch("unsafe-artifact-path").dig("invalid_fragment", "artifact_paths")
fail!("unsafe path fixture lacks absolute and traversal examples") unless unsafe_paths.any? { |path| path.start_with?("/") } &&
  unsafe_paths.any? { |path| path.start_with?("../") } &&
  unsafe_paths.any? { |path| path.start_with?(".git/") }
unsafe_paths.each do |path|
  fail!("unsafe path accepted by predicate: #{path}") if safe_repo_relative_path?(path)
end

puts "mission twin safety policy: static verifier passed (#{EXPECTED_FORBIDDEN.length} forbidden actions, #{EXPECTED_FAILURES.length} invalid cases)"
RUBY
