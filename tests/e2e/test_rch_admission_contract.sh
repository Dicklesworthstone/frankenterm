#!/usr/bin/env bash
# Static verifier for the RCH admission diagnostic contract and fixtures.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

SCHEMA="docs/json-schema/ft-rch-admission.json"
DOC="docs/rch-admission-contract.md"
FIXTURES="fixtures/rch-admission/reason-code-fixtures.json"
PROVENANCE="docs/json-schema/PROVENANCE.md"
README="README.md"
SOURCE="crates/frankenterm-core/src/rch_admission.rs"

fail() {
  printf 'rch admission contract: %s\n' "$*" >&2
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
require_file "${FIXTURES}"
require_file "${PROVENANCE}"
require_file "${README}"
require_file "${SOURCE}"

jq empty "${SCHEMA}" "${FIXTURES}"

ruby <<'RUBY'
require "json"
require "set"

SCHEMA = "docs/json-schema/ft-rch-admission.json"
DOC = "docs/rch-admission-contract.md"
FIXTURES = "fixtures/rch-admission/reason-code-fixtures.json"
PROVENANCE = "docs/json-schema/PROVENANCE.md"
README = "README.md"
SOURCE = "crates/frankenterm-core/src/rch_admission.rs"
EXPECTED_CODES = %w[
  local_eno_space
  no_admissible_workers
  critical_pressure
  telemetry_gap
  insufficient_slots
  active_project_exclusion
  speedscore_response_shape
  dry_run_inconsistent_worker
  unknown
].freeze
EXPECTED_ROOT_FIELDS = %w[
  command
  local_disk
  beads
  agent_mail
  rch_queue
  worker_rejections
  cargo_jobs
  estimated_slots
  recommendations
  forbidden_actions
].freeze
EXPECTED_FORBIDDEN = %w[
  run_local_cargo_as_proof
  restart_agent_mail
  repair_agent_mail_db
  restart_rch_daemon
  mutate_rch_worker
  cancel_other_agent_build
  delete_files_without_approval
  treat_dry_run_as_compile_proof
].freeze

def fail!(message)
  warn "rch admission contract: #{message}"
  exit 1
end

def read_json(path)
  JSON.parse(File.read(path))
rescue JSON::ParserError => error
  fail!("#{path} does not parse as JSON: #{error.message}")
end

schema = read_json(SCHEMA)
fixtures = read_json(FIXTURES)
doc = File.read(DOC)
provenance = File.read(PROVENANCE)
readme = File.read(README)
source = File.read(SOURCE)

fail!("schema id drifted") unless schema["$id"]&.end_with?("/ft-rch-admission.json")
fail!("contract id const missing") unless schema.dig("properties", "contract_id", "const") == "ft.rch_admission.v1"
fail!("advisory_only const missing") unless schema.dig("properties", "advisory_only", "const") == true
proof_statuses = schema.dig("properties", "proof_status", "enum")
fail!("proof_status must not include passed") if proof_statuses.include?("passed")

required = schema.fetch("required")
EXPECTED_ROOT_FIELDS.each do |field|
  fail!("schema missing required root field #{field}") unless required.include?(field)
end

schema_codes = schema.dig("$defs", "reason_code", "enum")
fail!("schema reason-code enum drifted") unless schema_codes.sort == EXPECTED_CODES.sort
forbidden_enum = schema.dig("$defs", "forbidden_action", "enum")
fail!("schema forbidden-action enum drifted") unless forbidden_enum.sort == EXPECTED_FORBIDDEN.sort

fail!("fixture schema pointer drifted") unless fixtures["schema_path"] == SCHEMA
fail!("fixture doc pointer drifted") unless fixtures["contract_doc"] == DOC
cases = fixtures.fetch("cases")
case_codes = cases.map { |entry| entry.fetch("reason_code") }
fail!("fixture reason-code coverage drifted: #{case_codes.sort.inspect}") unless case_codes.sort == EXPECTED_CODES.sort
fail!("fixture ids are not unique") unless cases.map { |entry| entry.fetch("fixture_id") }.uniq.length == cases.length

cases.each do |entry|
  code = entry.fetch("reason_code")
  payload = entry.fetch("payload")
  fail!("payload #{code} schema_version drifted") unless payload["schema_version"] == 1
  fail!("payload #{code} contract_id drifted") unless payload["contract_id"] == "ft.rch_admission.v1"
  fail!("payload #{code} is not advisory") unless payload["advisory_only"] == true
  fail!("payload #{code} falsely claims proof passed") if payload["proof_status"] == "passed"
  EXPECTED_ROOT_FIELDS.each do |field|
    fail!("payload #{code} missing root field #{field}") unless payload.key?(field)
  end
  fail!("payload #{code} does not include its reason code") unless payload.fetch("reason_codes").include?(code)
  fail!("payload #{code} forbidden-actions drifted") unless payload.fetch("forbidden_actions").sort == EXPECTED_FORBIDDEN.sort
  rec_codes = payload.fetch("recommendations").map { |rec| rec.fetch("reason_code") }
  fail!("payload #{code} has no recommendation for its reason code") unless rec_codes.include?(code)
end

EXPECTED_CODES.each do |code|
  fail!("doc missing reason code #{code}") unless doc.include?("`#{code}`")
end
EXPECTED_ROOT_FIELDS.each do |field|
  fail!("doc missing root field #{field}") unless doc.include?("`#{field}`")
end
EXPECTED_FORBIDDEN.each do |action|
  fail!("doc missing forbidden action #{action}") unless doc.include?(action)
end
fail!("doc must explicitly say advisory") unless doc.downcase.include?("advisory")
fail!("doc must reject dry-run as proof") unless doc.include?("dry-run") && doc.include?("compile/test proof")
%w[
  analyze_rch_admission_cargo_command
  command.normalized
  command.classification
  command.target_dir
  cargo_jobs
  estimated_slots
  slot_estimate_mismatch
].each do |term|
  fail!("doc missing cargo analyzer term #{term}") unless doc.include?(term)
end
%w[
  RchAdmissionCargoCommandAnalysis
  RchAdmissionCargoJobSource
  analyze_rch_admission_cargo_command
  CARGO_BUILD_JOBS
  --target-dir
  slot_estimate_mismatch
].each do |term|
  fail!("source missing cargo analyzer term #{term}") unless source.include?(term)
end

fail!("provenance missing ft-rch-admission row") unless provenance.include?("`ft-rch-admission.json`")
fail!("provenance row must cite static verifier") unless provenance.include?("bash tests/e2e/test_rch_admission_contract.sh")

live_e2e = Dir.glob("tests/e2e/**/*.sh").length
fail!("README stamped E2E count stale") unless readme.include?("<!--count:e2e_scripts-->#{live_e2e}<!--/count-->")
fail!("README tree E2E count stale") unless readme.include?("# #{live_e2e} shell E2E scripts")

puts "rch admission contract: static verifier passed (#{cases.length} fixtures, #{EXPECTED_CODES.length} reason codes, #{live_e2e} E2E scripts)"
RUBY
