#!/usr/bin/env bash
# Static verifier for the deferred proof receipt schema and fixtures.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

SCHEMA="docs/json-schema/ft-deferred-proof-receipt.json"
DOC="docs/robot-contracts/deferred-proof-receipt.md"
MANIFEST="fixtures/deferred-proof-replay/receipt/manifest.json"
VALID="fixtures/deferred-proof-replay/receipt/valid/cases.v1.json"
INVALID="fixtures/deferred-proof-replay/receipt/invalid/fragments.v1.json"
PROVENANCE="docs/json-schema/PROVENANCE.md"

fail() {
  printf 'deferred proof receipt contract: %s\n' "$*" >&2
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
require_file "${SCHEMA}"
require_file "${DOC}"
require_file "${MANIFEST}"
require_file "${VALID}"
require_file "${INVALID}"
require_file "${PROVENANCE}"

jq empty "${SCHEMA}" "${MANIFEST}" "${VALID}" "${INVALID}"

ruby <<'RUBY'
require "json"
require "set"

SCHEMA = "docs/json-schema/ft-deferred-proof-receipt.json"
DOC = "docs/robot-contracts/deferred-proof-receipt.md"
MANIFEST = "fixtures/deferred-proof-replay/receipt/manifest.json"
VALID = "fixtures/deferred-proof-replay/receipt/valid/cases.v1.json"
INVALID = "fixtures/deferred-proof-replay/receipt/invalid/fragments.v1.json"
PROVENANCE = "docs/json-schema/PROVENANCE.md"
CONTRACT_ID = "ft.deferred_proof_receipt.v1"
REQUIRED_VALID = %w[
  remote-required-cargo-proof
  static-only-proof
  dirty-overlap-block
  prerequisite-bead-block
  operator-cancelled-replay
].freeze
REQUIRED_INVALID = %w[
  stale-command-shape
  missing-no-self-healing
  local-fallback-evidence
  missing-owned-paths
  ambiguous-dirty-overlap
  fake-rch-command-shape
  env-not-allowlisted
  duplicate-env
  payload-env-not-allowlisted
  target-dir-drift
  unsafe-artifact-path
].freeze
REQUIRED_FORBIDDEN = %w[
  local_cargo_proof
  local_heavy_cargo_fallback
  rch_service_repair
  rch_worker_mutation
  agent_mail_repair
  delete_files
  destructive_git
  build_cancellation
  raw_pane_text_storage
  secret_storage
].freeze
VALID_STATES = %w[
  dirty_overlap
  eligible
  operator_cancelled
  prerequisite_blocked
  wait_rch
].freeze
REMOTE_SHAPE = "rch-no-self-healing-v1"
STATIC_SHAPE = "static-verifier-v1"
REMOTE_ARGV_PREFIX = %w[
  rch
  --no-self-healing
  exec
  --
].freeze
SAFE_ARTIFACT_ROOTS = %w[
  docs/json-schema/
  docs/robot-contracts/
  fixtures/deferred-proof-replay/receipt/
  tests/e2e/
].freeze

def fail!(message)
  warn "deferred proof receipt contract: #{message}"
  exit 1
end

def read_json(path)
  JSON.parse(File.read(path))
rescue JSON::ParserError => error
  fail!("#{path} does not parse as JSON: #{error.message}")
end

def env_map(receipt)
  receipt.fetch("command").fetch("env", []).to_h { |item| [item.fetch("name"), item.fetch("value")] }
end

def command_argv(receipt)
  receipt.fetch("command").fetch("argv", [])
end

def remote_command_shape_valid?(argv)
  return false unless argv.is_a?(Array)

  argv[0, REMOTE_ARGV_PREFIX.length] == REMOTE_ARGV_PREFIX &&
    argv.length > REMOTE_ARGV_PREFIX.length
end

def remote_command_payload(argv)
  return [] unless remote_command_shape_valid?(argv)

  argv[REMOTE_ARGV_PREFIX.length..] || []
end

def payload_env_assignments(payload)
  return [] unless payload.first == "env"

  payload[1..].to_a.take_while { |token| token.match?(/\A[A-Z][A-Z0-9_]*=/) }
end

def payload_env_assignment_names(payload)
  payload_env_assignments(payload).map { |token| token.split("=", 2).first }
end

def path_safe?(path)
  return false unless path.is_a?(String)
  return false if path.empty? || path.start_with?("/") || path.include?("\\")
  return false if path.split("/").any? { |part| part.empty? || part == "." || part == ".." || part == ".git" }

  true
end

def artifact_path_safe?(path)
  path_safe?(path) &&
    path.match?(/\.(json|md|sh)\z/) &&
    SAFE_ARTIFACT_ROOTS.any? { |root| path.start_with?(root) && path.length > root.length }
end

def local_fallback_text?(receipt)
  JSON.generate(receipt).match?(/\[RCH\] local|running locally|local fallback/i)
end

def rejection_reasons(receipt)
  reasons = []
  command = receipt.fetch("command", {})
  proof = receipt.fetch("proof", {})
  paths = receipt.fetch("paths", {})
  coordination = receipt.fetch("coordination", {})
  eligibility = receipt.fetch("eligibility", {})
  shape = command["command_shape_version"]
  argv = command.fetch("argv", [])
  env_entries = command.fetch("env", [])
  env_names = env_entries.map { |item| item.fetch("name") }
  env_allowlist = command.fetch("env_allowlist", [])
  payload = remote_command_payload(argv)
  payload_names = payload_env_assignment_names(payload)
  env = env_map(receipt)
  owned = paths.fetch("owned_paths", [])
  dirty = paths.fetch("dirty_paths_at_capture", [])
  overlap = owned & dirty

  reasons << "stale_command_shape" unless [REMOTE_SHAPE, STATIC_SHAPE].include?(shape)
  reasons << "duplicate_env" unless env_names.uniq.length == env_names.length
  reasons << "duplicate_env_allowlist" unless env_allowlist.uniq.length == env_allowlist.length
  reasons << "env_not_allowlisted" unless (env_names - env_allowlist).empty?
  reasons << "duplicate_env" unless payload_names.uniq.length == payload_names.length
  reasons << "env_not_allowlisted" unless (payload_names - env_allowlist).empty?
  if proof["material_cargo_required"] || command["material_remote_required"]
    reasons << "missing_no_self_healing" unless env["RCH_NO_SELF_HEALING"] == "1" && remote_command_shape_valid?(argv)
    reasons << "missing_require_remote" unless env["RCH_REQUIRE_REMOTE"] == "1"
    reasons << "stale_command_shape" unless shape == REMOTE_SHAPE && remote_command_shape_valid?(argv)
    target_dir = command["target_dir"]
    if target_dir
      target_assignment = "CARGO_TARGET_DIR=#{target_dir}"
      reasons << "target_dir_drift" unless payload_env_assignments(payload).include?(target_assignment)
    end
  end
  reasons << "local_fallback_evidence" if local_fallback_text?(receipt)
  reasons << "missing_owned_paths" if owned.empty?
  reasons << "ambiguous_dirty_overlap" if overlap.any? && eligibility["state"] != "dirty_overlap"
  reasons << "prerequisite_blocked" if coordination.fetch("prerequisite_beads", []).any? && eligibility["state"] == "eligible"
  reasons << "operator_cancelled" if coordination["operator_cancelled"] == true && eligibility["replay_allowed"] != false
  Array(paths.fetch("artifact_paths", [])).each do |path|
    reasons << "unsafe_artifact_path" unless artifact_path_safe?(path) && File.file?(path)
  end
  reasons
end

schema = read_json(SCHEMA)
manifest = read_json(MANIFEST)
valid = read_json(VALID)
invalid = read_json(INVALID)
doc = File.read(DOC)
provenance = File.read(PROVENANCE)

fail!("schema id drifted") unless schema["$id"]&.end_with?("/ft-deferred-proof-receipt.json")
fail!("schema contract const drifted") unless schema.dig("properties", "contract_id", "const") == CONTRACT_ID
fail!("schema required missing command") unless schema.fetch("required").include?("command")
fail!("schema required missing eligibility") unless schema.fetch("required").include?("eligibility")
fail!("schema forbidden enum drifted") unless schema.dig("$defs", "forbidden_action", "enum").sort == REQUIRED_FORBIDDEN.sort
fail!("schema command shape enum drifted") unless schema.dig("$defs", "command_receipt", "properties", "command_shape_version", "enum").sort == [REMOTE_SHAPE, STATIC_SHAPE].sort
fail!("schema artifact path pattern missing safe roots") unless SAFE_ARTIFACT_ROOTS.all? { |root| schema.dig("$defs", "artifact_path", "pattern").include?(root.delete_suffix("/")) }

fail!("manifest contract drifted") unless manifest["contract_id"] == CONTRACT_ID
fail!("manifest schema path drifted") unless manifest["schema_path"] == SCHEMA
fail!("manifest valid path drifted") unless manifest["valid_cases"] == VALID
fail!("manifest invalid path drifted") unless manifest["invalid_fragments"] == INVALID
fail!("manifest valid count drifted") unless manifest.dig("golden_summary", "valid_case_count") == REQUIRED_VALID.length
fail!("manifest invalid count drifted") unless manifest.dig("golden_summary", "invalid_case_count") == REQUIRED_INVALID.length

valid_cases = valid.fetch("cases")
valid_ids = valid_cases.map { |entry| entry.fetch("case_id") }
fail!("valid fixture coverage drifted: #{valid_ids.sort.inspect}") unless valid_ids.sort == REQUIRED_VALID.sort
fail!("valid fixture ids are not unique") unless valid_ids.uniq.length == valid_ids.length

valid_cases.each do |entry|
  receipt = entry.fetch("receipt")
  receipt_id = receipt.fetch("receipt_id")
  fail!("#{receipt_id} contract drifted") unless receipt["contract_id"] == CONTRACT_ID
  fail!("#{receipt_id} forbidden actions drifted") unless receipt.fetch("forbidden_actions").sort == REQUIRED_FORBIDDEN.sort
  fail!("#{receipt_id} has rejection reasons: #{rejection_reasons(receipt).uniq.sort.join(', ')}") unless rejection_reasons(receipt).empty?
  fail!("#{receipt_id} eligibility state is outside valid corpus") unless VALID_STATES.include?(receipt.dig("eligibility", "state"))
  if receipt.dig("proof", "material_cargo_required")
    fail!("#{receipt_id} material proof missing RCH target dir") unless receipt.dig("command", "target_dir")&.start_with?("/tmp/")
    fail!("#{receipt_id} material proof missing cargo argv") unless remote_command_payload(command_argv(receipt)).include?("cargo")
  else
    fail!("#{receipt_id} static proof should not require RCH target dir") unless receipt.dig("command", "target_dir").nil?
  end
end

invalid_cases = invalid.fetch("cases")
invalid_ids = invalid_cases.map { |entry| entry.fetch("case_id") }
fail!("invalid fixture coverage drifted: #{invalid_ids.sort.inspect}") unless invalid_ids.sort == REQUIRED_INVALID.sort
fail!("invalid fixture ids are not unique") unless invalid_ids.uniq.length == invalid_ids.length
invalid_cases.each do |entry|
  expected = entry.fetch("expected_rejection")
  reasons = rejection_reasons(entry.fetch("receipt")).uniq
  fail!("invalid fixture #{entry.fetch("case_id")} did not reject as #{expected}; reasons=#{reasons.inspect}") unless reasons.include?(expected)
end

[
  "ft.deferred_proof_receipt.v1",
  "RCH_REQUIRE_REMOTE=1",
  "RCH_NO_SELF_HEALING=1",
  "local fallback",
  "dirty_overlap",
  "operator_cancelled",
  "fixtures/deferred-proof-replay/receipt/"
].each do |term|
  fail!("doc missing contract term #{term}") unless doc.include?(term)
end

fail!("provenance missing deferred proof receipt row") unless provenance.include?("`ft-deferred-proof-receipt.json`")
fail!("provenance row missing verifier") unless provenance.include?("bash tests/e2e/test_deferred_proof_receipt_contract.sh")

puts "deferred proof receipt contract: static verifier passed (#{valid_ids.length} valid receipts, #{invalid_ids.length} invalid fragments)"
RUBY
