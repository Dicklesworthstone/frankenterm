#!/usr/bin/env bash
# Static verifier for post-recovery RCH admission proof gate schema and fixtures.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

SCHEMA="docs/json-schema/ft-rch-worker-storage-recovery-proof.json"
DOC="docs/robot-contracts/rch-worker-storage-recovery-proof.md"
MANIFEST="fixtures/rch-worker-storage-recovery-proof/manifest.json"

fail() {
  printf 'rch worker storage recovery proof contract: %s\n' "$*" >&2
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

jq empty "${SCHEMA}" "${MANIFEST}" fixtures/rch-worker-storage-recovery-proof/valid/*.json

ruby <<'RUBY'
require "json"
require "set"

SCHEMA = "docs/json-schema/ft-rch-worker-storage-recovery-proof.json"
DOC = "docs/robot-contracts/rch-worker-storage-recovery-proof.md"
MANIFEST = "fixtures/rch-worker-storage-recovery-proof/manifest.json"
EXPECTED_FIXTURE_IDS = %w[
  passed-remote-smoke
  blocked-no-admissible-worker
  blocked-new-reason
  failed-remote-smoke
  invalid-missing-approval
].freeze
EXPECTED_RESULTS = {
  "passed-remote-smoke" => "passed_remote_smoke",
  "blocked-no-admissible-worker" => "blocked_no_admissible_worker",
  "blocked-new-reason" => "blocked_new_reason",
  "failed-remote-smoke" => "failed_remote_smoke",
  "invalid-missing-approval" => "invalid_missing_approval"
}.freeze
EXPECTED_FORBIDDEN = %w[
  run_local_cargo_as_proof
  restart_agent_mail
  repair_agent_mail_db
  restart_rch_daemon
  mutate_rch_worker
  cancel_other_agent_build
  delete_files_without_approval
  run_agent_cleanup
  mutate_remote_mirror
  destructive_git
  close_ft4tp7g_without_remote_evidence
].freeze
EXPECTED_SIDE_EFFECT_FLAGS = %w[
  agent_mail_repaired
  agent_mail_restarted
  agent_performed_recovery
  build_cancelled
  files_deleted_by_agent
  local_cargo_counted_as_proof
  rch_daemon_restarted
  read_only_evidence_collection
  remote_mirror_mutated
  worker_mutated_by_agent
].freeze
STATUS_COMMAND = "RCH_NO_SELF_HEALING=1 rch --no-self-healing --json status --workers --jobs".freeze
REMOTE_REQUIRED_PREFIX = "RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing".freeze
SHA256 = /\A[0-9a-f]{64}\z/

def fail!(message)
  warn "rch worker storage recovery proof contract: #{message}"
  exit 1
end

def read_json(path)
  JSON.parse(File.read(path))
rescue JSON::ParserError => error
  fail!("#{path} does not parse as JSON: #{error.message}")
end

def assert_sha!(value, message)
  fail!(message) unless SHA256.match?(value)
end

schema = read_json(SCHEMA)
manifest = read_json(MANIFEST)
doc = File.read(DOC)

fail!("schema id drifted") unless schema["$id"]&.end_with?("/ft-rch-worker-storage-recovery-proof.json")
fail!("contract id const missing") unless schema.dig("properties", "contract_id", "const") == "ft.rch_worker_storage_recovery_proof.v1"
fail!("source bead const missing") unless schema.dig("properties", "source_bead", "const") == "ft-5xwsu.3"
fail!("approval contract const missing") unless schema.dig("properties", "approval_contract_id", "const") == "ft.rch_worker_storage_approval.v1"
fail!("gate result enum drifted") unless schema.dig("$defs", "gate_result", "enum").sort == EXPECTED_RESULTS.values.sort
fail!("forbidden-action enum drifted") unless schema.dig("$defs", "forbidden_action", "enum").sort == EXPECTED_FORBIDDEN.sort
fail!("RCH status command const drifted") unless schema.dig("$defs", "rch_status", "properties", "command", "const") == STATUS_COMMAND
fail!("br dep cycles count const missing") unless schema.dig("$defs", "br_dep_cycles", "properties", "count", "const") == 0

manifest_expected_verifier = "bash tests/e2e/test_rch_worker_storage_recovery_proof_contract.sh"
fail!("manifest schema_version drifted") unless manifest["schema_version"] == 1
fail!("manifest contract id drifted") unless manifest["contract_id"] == "ft.rch_worker_storage_recovery_proof.fixture_manifest.v1"
fail!("manifest bead drifted") unless manifest["bead"] == "ft-5xwsu.3"
fail!("manifest schema pointer drifted") unless manifest["schema"] == SCHEMA
fail!("manifest contract pointer drifted") unless manifest["contract"] == DOC
fail!("manifest verifier missing") unless manifest.fetch("verification").include?(manifest_expected_verifier)

fixture_paths = manifest.fetch("valid")
fixture_ids = fixture_paths.map { |path| File.basename(path, ".json") }
fail!("fixture ids drifted: #{fixture_ids.sort.inspect}") unless fixture_ids.sort == EXPECTED_FIXTURE_IDS.sort
fixture_paths.each { |path| fail!("manifest references missing fixture #{path}") unless File.file?(path) }

payloads = fixture_paths.map { |path| [File.basename(path, ".json"), read_json(path)] }.to_h
payloads.each do |fixture_id, payload|
  fail!("#{fixture_id} schema_version drifted") unless payload["schema_version"] == 1
  fail!("#{fixture_id} contract id drifted") unless payload["contract_id"] == "ft.rch_worker_storage_recovery_proof.v1"
  fail!("#{fixture_id} source bead drifted") unless payload["source_bead"] == "ft-5xwsu.3"
  fail!("#{fixture_id} approval contract drifted") unless payload["approval_contract_id"] == "ft.rch_worker_storage_approval.v1"
  fail!("#{fixture_id} gate result drifted") unless payload["gate_result"] == EXPECTED_RESULTS.fetch(fixture_id)
  fail!("#{fixture_id} forbidden actions drifted") unless payload.fetch("forbidden_actions").sort == EXPECTED_FORBIDDEN.sort
  fail!("#{fixture_id} missing notes") if payload.fetch("notes").empty?
  if fixture_id == "invalid-missing-approval"
    fail!("invalid fixture must not cite an approval path") unless payload["approval_artifact_path"].nil?
    fail!("invalid fixture must not cite an approval hash") unless payload["approval_artifact_sha256"].nil?
  else
    fail!("#{fixture_id} missing approval artifact path") if payload.fetch("approval_artifact_path").empty?
    assert_sha!(payload.fetch("approval_artifact_sha256"), "#{fixture_id} approval hash invalid")
  end

  side_effects = payload.fetch("agent_side_effect_policy")
  fail!("#{fixture_id} side-effect keys drifted") unless side_effects.keys.sort == EXPECTED_SIDE_EFFECT_FLAGS.sort
  side_effects.each do |flag, value|
    expected = flag == "read_only_evidence_collection"
    fail!("#{fixture_id} side-effect flag #{flag} drifted") unless value == expected
  end

  status = payload.fetch("rch_status")
  fail!("#{fixture_id} status command drifted") unless status["command"] == STATUS_COMMAND
  fail!("#{fixture_id} missing RCH version") if status.fetch("rch_version").empty?
  fail!("#{fixture_id} negative worker count") if status.fetch("workers_total").negative?
  assert_sha!(status.fetch("artifact_sha256"), "#{fixture_id} status artifact hash invalid")

  artifact_set = payload.fetch("artifact_paths").to_set
  %w[rch_status remote_required_dry_run remote_required_smoke br_dep_cycles].each do |section|
    artifact = payload.fetch(section).fetch("artifact_path")
    fail!("#{fixture_id} #{section} artifact not retained: #{artifact}") unless artifact_set.include?(artifact)
    assert_sha!(payload.fetch(section).fetch("artifact_sha256"), "#{fixture_id} #{section} hash invalid")
  end

  dry_run = payload.fetch("remote_required_dry_run")
  dry_run_command = dry_run.fetch("command")
  fail!("#{fixture_id} dry-run lacks canonical no-self-healing prefix") unless dry_run_command.start_with?("#{REMOTE_REQUIRED_PREFIX} diagnose ")
  fail!("#{fixture_id} dry-run lacks --dry-run") unless dry_run_command.include?("--dry-run")
  fail!("#{fixture_id} dry-run should not have an exit status") unless dry_run["exit_status"].nil?
  fail!("#{fixture_id} dry-run target dir not included in command") unless dry_run_command.include?(dry_run.fetch("target_dir"))
  fail!("#{fixture_id} dry-run would_intercept drifted") unless dry_run["would_intercept"] == true

  smoke = payload.fetch("remote_required_smoke")
  smoke_command = smoke.fetch("command")
  fail!("#{fixture_id} smoke lacks canonical no-self-healing prefix") unless smoke_command.start_with?("#{REMOTE_REQUIRED_PREFIX} exec -- ")
  fail!("#{fixture_id} smoke target dir not included in command") unless smoke_command.include?(smoke.fetch("target_dir"))
  fail!("#{fixture_id} smoke command must not be a dry-run") if smoke_command.include?("--dry-run")
  fail!("#{fixture_id} smoke would_intercept drifted") unless smoke["would_intercept"] == true

  if fixture_id == "invalid-missing-approval"
    fail!("invalid fixture dry-run must be skipped") unless dry_run["required"] == false
    fail!("invalid fixture dry-run transfer drifted") unless dry_run["transfer_state"] == "not_attempted"
    fail!("invalid fixture dry-run remote execution drifted") unless dry_run["remote_execution_state"] == "not_attempted"
    fail!("invalid fixture smoke must be skipped") unless smoke["required"] == false
    fail!("invalid fixture smoke transfer drifted") unless smoke["transfer_state"] == "not_attempted"
    fail!("invalid fixture smoke remote execution drifted") unless smoke["remote_execution_state"] == "not_attempted"
  else
    fail!("#{fixture_id} dry-run must be required") unless dry_run["required"] == true
  end

  cycles = payload.fetch("br_dep_cycles")
  fail!("#{fixture_id} br dep command drifted") unless cycles["command"] == "br dep cycles --json"
  fail!("#{fixture_id} br dep cycles not zero") unless cycles["count"] == 0
end

passed = payloads.fetch("passed-remote-smoke")
fail!("passed fixture missing operator recovery reference") unless passed["operator_recovery_reference"]
fail!("passed fixture must recover admission") unless passed["admission_recovered"] == true
fail!("passed fixture must allow ft-4tp7g closeout") unless passed["ft4tp7g_closeout_allowed"] == true
fail!("passed fixture stable reason should be null") unless passed["stable_reason_code"].nil?
pass_dry = passed.fetch("remote_required_dry_run")
pass_smoke = passed.fetch("remote_required_smoke")
fail!("passed dry-run must select a worker") unless pass_dry["selected_worker"]
fail!("passed dry-run transfer should be dry-run skipped") unless pass_dry["transfer_state"] == "skipped_dry_run_only"
fail!("passed smoke must be required") unless pass_smoke["required"] == true
fail!("passed smoke worker must match dry-run") unless pass_smoke["selected_worker"] == pass_dry["selected_worker"]
fail!("passed smoke transfer not completed") unless pass_smoke["transfer_state"] == "completed"
fail!("passed smoke remote execution not completed") unless pass_smoke["remote_execution_state"] == "completed"
fail!("passed smoke exit status not zero") unless pass_smoke["exit_status"] == 0
fail!("passed status still reports critical pressure") unless passed.dig("rch_status", "critical_pressure_workers") == 0

blocked = payloads.fetch("blocked-no-admissible-worker")
fail!("blocked fixture should not recover admission") unless blocked["admission_recovered"] == false
fail!("blocked fixture should not close ft-4tp7g") unless blocked["ft4tp7g_closeout_allowed"] == false
fail!("blocked fixture missing critical pressure reason") unless blocked["stable_reason_code"] == "critical_pressure=5"
fail!("blocked dry-run selected worker") unless blocked.dig("remote_required_dry_run", "selected_worker").nil?
fail!("blocked smoke must be skipped") unless blocked.dig("remote_required_smoke", "required") == false
fail!("blocked smoke transfer drifted") unless blocked.dig("remote_required_smoke", "transfer_state") == "skipped_no_worker"
fail!("blocked status lacks critical pressure workers") unless blocked.dig("rch_status", "critical_pressure_workers") > 0

new_reason = payloads.fetch("blocked-new-reason")
fail!("new-reason fixture still reports critical pressure") unless new_reason.dig("rch_status", "critical_pressure_workers") == 0
fail!("new-reason fixture missing stable reason") unless new_reason.fetch("stable_reason_code").start_with?("ssh_unreachable=")
fail!("new-reason fixture should not close ft-4tp7g") unless new_reason["ft4tp7g_closeout_allowed"] == false
fail!("new-reason smoke must be skipped") unless new_reason.dig("remote_required_smoke", "required") == false

failed = payloads.fetch("failed-remote-smoke")
fail!("failed fixture should not recover admission") unless failed["admission_recovered"] == false
fail!("failed fixture should not close ft-4tp7g") unless failed["ft4tp7g_closeout_allowed"] == false
fail!("failed fixture missing remote smoke reason") unless failed["stable_reason_code"] == "remote_smoke_exit_status=101"
fail!("failed dry-run must select worker") unless failed.dig("remote_required_dry_run", "selected_worker")
fail!("failed smoke must be required") unless failed.dig("remote_required_smoke", "required") == true
fail!("failed smoke must have nonzero exit") unless failed.dig("remote_required_smoke", "exit_status").to_i > 0
fail!("failed smoke remote execution should fail") unless failed.dig("remote_required_smoke", "remote_execution_state") == "failed"

invalid = payloads.fetch("invalid-missing-approval")
fail!("invalid fixture should not recover admission") unless invalid["admission_recovered"] == false
fail!("invalid fixture should not close ft-4tp7g") unless invalid["ft4tp7g_closeout_allowed"] == false
fail!("invalid fixture should not have an operator recovery reference") unless invalid["operator_recovery_reference"].nil?
fail!("invalid fixture missing stable reason") unless invalid["stable_reason_code"] == "missing_operator_recovery_reference"
fail!("invalid fixture dry-run selected worker") unless invalid.dig("remote_required_dry_run", "selected_worker").nil?
fail!("invalid fixture smoke selected worker") unless invalid.dig("remote_required_smoke", "selected_worker").nil?
fail!("invalid fixture smoke has exit status") unless invalid.dig("remote_required_smoke", "exit_status").nil?

fail!("doc missing schema path") unless doc.include?(SCHEMA)
fail!("doc missing remote-required dry-run rule") unless doc.include?("remote-required dry-run")
fail!("doc missing material remote smoke rule") unless doc.include?("material remote-required smoke")
fail!("doc missing ft-4tp7g closeout rule") unless doc.include?("ft-4tp7g")
fail!("doc missing local Cargo prohibition") unless doc.include?("Local Cargo")
fail!("doc missing missing-approval fixture") unless doc.include?("invalid-missing-approval")
fail!("doc missing missing-approval gate result") unless doc.include?("invalid_missing_approval")

puts "rch worker storage recovery proof contract: static verifier passed (#{fixture_paths.length} fixtures, #{EXPECTED_FORBIDDEN.length} forbidden actions)"
RUBY
