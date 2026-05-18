#!/usr/bin/env bash
# Static verifier for the RCH admission diagnostic contract and fixtures.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

SCHEMA="docs/json-schema/ft-rch-admission.json"
DOC="docs/rch-admission-contract.md"
FIXTURES="fixtures/rch-admission/reason-code-fixtures.json"
NO_SERVICE_FIXTURES="fixtures/rch-admission/no-service-action-fixtures.json"
STRUCTURED_LOG_GOLDEN="fixtures/rch-admission/expected-structured-log.golden.jsonl"
SUMMARY_GOLDEN="fixtures/rch-admission/summary.golden.json"
PROVENANCE="docs/json-schema/PROVENANCE.md"
README="README.md"
SOURCE="crates/frankenterm-core/src/rch_admission.rs"
RUN_ID="${FT_RCH_ADMISSION_CONTRACT_RUN_ID:-static}"
ARTIFACT_DIR="${FT_RCH_ADMISSION_CONTRACT_ARTIFACT_DIR:-tests/e2e/artifacts/static-proof/ft-69gwh.4/rch-admission-contract/${RUN_ID}}"
STRUCTURED_LOG="${ARTIFACT_DIR}/structured.log"
SUMMARY_FILE="${ARTIFACT_DIR}/summary.json"

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
require_file "${NO_SERVICE_FIXTURES}"
require_file "${STRUCTURED_LOG_GOLDEN}"
require_file "${SUMMARY_GOLDEN}"
require_file "${PROVENANCE}"
require_file "${README}"
require_file "${SOURCE}"

mkdir -p "${ARTIFACT_DIR}"
export FT_RCH_ADMISSION_STRUCTURED_LOG="${STRUCTURED_LOG}"
export FT_RCH_ADMISSION_SUMMARY_FILE="${SUMMARY_FILE}"

jq empty "${SCHEMA}" "${FIXTURES}" "${NO_SERVICE_FIXTURES}"

ruby <<'RUBY'
require "fileutils"
require "json"
require "set"

SCHEMA = "docs/json-schema/ft-rch-admission.json"
DOC = "docs/rch-admission-contract.md"
FIXTURES = "fixtures/rch-admission/reason-code-fixtures.json"
NO_SERVICE_FIXTURES = "fixtures/rch-admission/no-service-action-fixtures.json"
STRUCTURED_LOG_GOLDEN = "fixtures/rch-admission/expected-structured-log.golden.jsonl"
SUMMARY_GOLDEN = "fixtures/rch-admission/summary.golden.json"
PROVENANCE = "docs/json-schema/PROVENANCE.md"
README = "README.md"
SOURCE = "crates/frankenterm-core/src/rch_admission.rs"
STRUCTURED_LOG = ENV.fetch("FT_RCH_ADMISSION_STRUCTURED_LOG")
SUMMARY_FILE = ENV.fetch("FT_RCH_ADMISSION_SUMMARY_FILE")
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
EXPECTED_NO_SERVICE_CASE_IDS = %w[
  rch-e100-worker-unreachable
  all-workers-offline
  telemetry-gap
  critical-pressure
  critical-pressure-current-fleet
  insufficient-slots
  active-project-exclusion
  local-eno-space
  speedscore-parse-failure
  dry-run-inconsistency
].freeze
EXPECTED_SIDE_EFFECT_FLAGS = %w[
  agent_mail_repaired
  agent_mail_restarted
  rch_daemon_restarted
  worker_mutated
  build_cancelled
  files_deleted
  local_cargo_counted_as_proof
].freeze
REQUIRED_FORBIDDEN_COMMAND_FRAGMENTS = [
  "am service restart",
  "am doctor repair",
  "rch daemon restart",
  "rch workers disable",
  "cancel build",
  "git reset --hard",
  "git clean -fd",
  "rm -rf"
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
no_service = read_json(NO_SERVICE_FIXTURES)
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

fail!("no-service fixture schema_version drifted") unless no_service["schema_version"] == "ft.rch_admission.no_service_actions.v1"
fail!("no-service fixture doc pointer drifted") unless no_service["contract_doc"] == DOC
fail!("no-service fixture verifier pointer drifted") unless no_service["static_verifier"] == "tests/e2e/test_rch_admission_contract.sh"
fail!("no-service required forbidden actions drifted") unless no_service.fetch("required_forbidden_actions").sort == EXPECTED_FORBIDDEN.sort
forbidden_fragments = no_service.fetch("forbidden_command_fragments")
REQUIRED_FORBIDDEN_COMMAND_FRAGMENTS.each do |fragment|
  fail!("no-service forbidden fragment missing #{fragment}") unless forbidden_fragments.include?(fragment)
end

no_service_cases = no_service.fetch("cases")
no_service_ids = no_service_cases.map { |entry| entry.fetch("fixture_id") }
fail!("no-service case coverage drifted: #{no_service_ids.sort.inspect}") unless no_service_ids.sort == EXPECTED_NO_SERVICE_CASE_IDS.sort
fail!("no-service fixture ids are not unique") unless no_service_ids.uniq.length == no_service_ids.length

no_service_cases.each do |entry|
  fixture_id = entry.fetch("fixture_id")
  code = entry.fetch("reason_code")
  fail!("no-service case #{fixture_id} has unknown reason code #{code}") unless EXPECTED_CODES.include?(code)
  fail!("no-service case #{fixture_id} forbidden-actions drifted") unless entry.fetch("forbidden_actions").sort == EXPECTED_FORBIDDEN.sort

  commands = entry.fetch("executed_read_only_commands")
  fail!("no-service case #{fixture_id} has no retained read-only commands") if commands.empty?
  commands.each do |command|
    fail!("no-service case #{fixture_id} records direct local Cargo proof command: #{command}") if command.start_with?("cargo ")
    forbidden_fragments.each do |fragment|
      if command.downcase.include?(fragment.downcase)
        fail!("no-service case #{fixture_id} command contains forbidden fragment #{fragment}: #{command}")
      end
    end
  end

  evidence = entry.fetch("retained_evidence")
  fail!("no-service case #{fixture_id} has no retained evidence") if evidence.empty?
  evidence.each do |record|
    fail!("no-service case #{fixture_id} evidence missing kind") unless record.key?("kind")
    fail!("no-service case #{fixture_id} evidence missing summary") unless record.key?("summary")
  end

  side_effects = entry.fetch("collector_side_effects")
  fail!("no-service case #{fixture_id} side-effect keys drifted") unless side_effects.keys.sort == EXPECTED_SIDE_EFFECT_FLAGS.sort
  side_effects.each do |flag, value|
    fail!("no-service case #{fixture_id} side-effect flag #{flag} is not false") unless value == false
  end

  structured = entry.fetch("expected_structured_log")
  %w[component fixture_id reason_code outcome service_actions_invoked local_cargo_proof side_effects_executed].each do |field|
    fail!("no-service case #{fixture_id} structured log missing #{field}") unless structured.key?(field)
  end
  fail!("no-service case #{fixture_id} structured fixture_id drifted") unless structured["fixture_id"] == fixture_id
  fail!("no-service case #{fixture_id} structured reason_code drifted") unless structured["reason_code"] == code
  fail!("no-service case #{fixture_id} structured outcome invalid") unless %w[blocked advisory].include?(structured["outcome"])
  fail!("no-service case #{fixture_id} invoked service action") unless structured["service_actions_invoked"] == false
  fail!("no-service case #{fixture_id} counted local Cargo as proof") unless structured["local_cargo_proof"] == false
  fail!("no-service case #{fixture_id} executed side effects") unless structured["side_effects_executed"] == false
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
EXPECTED_NO_SERVICE_CASE_IDS.each do |fixture_id|
  fail!("doc missing no-service fixture #{fixture_id}") unless doc.include?("`#{fixture_id}`")
end
%w[
  no-service-action-fixtures.json
  structured.log
  summary.json
  service_actions_invoked
  local_cargo_proof
  side_effects_executed
].each do |term|
  fail!("doc missing no-service contract term #{term}") unless doc.include?(term)
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

FileUtils.mkdir_p(File.dirname(STRUCTURED_LOG))
structured_events = no_service_cases.map do |entry|
  entry.fetch("expected_structured_log").merge(
    "contract_id" => "ft.rch_admission.v1",
    "schema_version" => no_service.fetch("schema_version"),
    "decision_path" => "no_service_actions.#{entry.fetch("fixture_id")}",
    "artifact_path" => NO_SERVICE_FIXTURES,
    "forbidden_actions" => entry.fetch("forbidden_actions"),
    "read_only_command_count" => entry.fetch("executed_read_only_commands").length
  )
end
File.write(STRUCTURED_LOG, structured_events.map { |event| JSON.generate(event) }.join("\n") + "\n")
fail!("structured log golden drifted") unless File.read(STRUCTURED_LOG) == File.read(STRUCTURED_LOG_GOLDEN)

parsed_events = File.readlines(STRUCTURED_LOG, chomp: true).map { |line| JSON.parse(line) }
fail!("structured log event count drifted") unless parsed_events.length == no_service_cases.length
parsed_events.each do |event|
  fail!("structured log event invoked service action") unless event.fetch("service_actions_invoked") == false
  fail!("structured log event counted local Cargo proof") unless event.fetch("local_cargo_proof") == false
  fail!("structured log event executed side effects") unless event.fetch("side_effects_executed") == false
end

summary = {
  "contract_id" => "ft.rch_admission.v1",
  "schema_version" => no_service.fetch("schema_version"),
  "status" => "passed",
  "reason_fixture_count" => cases.length,
  "no_service_fixture_count" => no_service_cases.length,
  "reason_code_count" => EXPECTED_CODES.length,
  "forbidden_action_count" => EXPECTED_FORBIDDEN.length,
  "structured_log" => STRUCTURED_LOG,
  "summary_file" => SUMMARY_FILE
}
File.write(SUMMARY_FILE, JSON.pretty_generate(summary) + "\n")
fail!("summary file is not JSON") unless read_json(SUMMARY_FILE).fetch("status") == "passed"
summary_golden = summary.reject { |key, _| %w[structured_log summary_file].include?(key) }
fail!("summary golden drifted") unless summary_golden == read_json(SUMMARY_GOLDEN)

puts "rch admission contract: static verifier passed (#{cases.length} reason fixtures, #{no_service_cases.length} no-service fixtures, #{EXPECTED_CODES.length} reason codes, #{live_e2e} E2E scripts; log #{STRUCTURED_LOG})"
RUBY
