#!/usr/bin/env bash
# Static verifier for Agent Mail failover snapshot schema and fixtures.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

SCHEMA="fixtures/agent-mail-failover/fallback-snapshot.schema.json"
DOC="docs/robot-contracts/agent-mail-failover-snapshot.md"
MANIFEST="fixtures/agent-mail-failover/manifest.json"
CLASSIFIER_CASES="fixtures/agent-mail-failover/retry-classifier-cases.json"
NO_SERVICE_GATE="fixtures/agent-mail-failover/no-service-action-gate.json"
RUNBOOK="docs/robot-contracts/agent-mail-failover-runbook.md"

fail() {
  printf 'agent mail failover snapshot contract: %s\n' "$*" >&2
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
require_file "${CLASSIFIER_CASES}"
require_file "${NO_SERVICE_GATE}"
require_file "${RUNBOOK}"

jq empty "${SCHEMA}" "${MANIFEST}" "${CLASSIFIER_CASES}" "${NO_SERVICE_GATE}" fixtures/agent-mail-failover/valid/*.json

ruby <<'RUBY'
require "json"
require "set"

SCHEMA = "fixtures/agent-mail-failover/fallback-snapshot.schema.json"
DOC = "docs/robot-contracts/agent-mail-failover-snapshot.md"
MANIFEST = "fixtures/agent-mail-failover/manifest.json"
CLASSIFIER_CASES = "fixtures/agent-mail-failover/retry-classifier-cases.json"
NO_SERVICE_GATE = "fixtures/agent-mail-failover/no-service-action-gate.json"
RUNBOOK = "docs/robot-contracts/agent-mail-failover-runbook.md"
EXPECTED_FIXTURE_IDS = %w[
  healthy-agent-mail
  unavailable-after-retry
  database-recovery-retry-exhausted
  registration-failed
  contact-permission-failed
  empty-in-progress
  stale-candidate-clean-tree
  dirty-tracked-overlap
  untracked-review-required
].freeze
EXPECTED_FORBIDDEN = %w[
  am_service_restart
  am_service_stop
  am_doctor_fix
  am_doctor_repair
  am_doctor_reconstruct
  kill_agent_mail_process
  destructive_git
  delete_files
  mutate_rch_worker
  cancel_build
  run_local_cargo_as_proof
  reopen_dirty_overlap_without_owner_clear
].freeze
SAFETY_FALSE_FLAGS = %w[
  agent_mail_repair_attempted
  agent_mail_restart_attempted
  process_kill_attempted
  destructive_git_attempted
  file_deletion_attempted
  local_cargo_counted_as_proof
  raw_mail_body_stored
  raw_pane_text_stored
].freeze
CLASSIFIER_EXPECTATIONS = {
  "healthy-agent-mail" => [nil, "agent_mail.available"],
  "database-recovery-retry-exhausted" => ["database_recovery_notice", "agent_mail.database_recovery_retry_exhausted"],
  "unavailable-after-retry" => ["api_unreachable", "agent_mail.unavailable_after_retry"],
  "empty-in-progress" => ["database_error", "agent_mail.unavailable_after_retry"],
  "stale-candidate-clean-tree" => ["timeout", "agent_mail.unavailable_after_retry"],
  "registration-failed" => ["registration_failed", "agent_mail.registration_failed_after_retry"],
  "contact-permission-failed" => ["contact_permission_failed", "agent_mail.contact_permission_failed_after_retry"],
  "dirty-tracked-overlap" => ["database_recovery_notice", "agent_mail.database_recovery_retry_exhausted"],
  "untracked-review-required" => ["unknown", "agent_mail.unavailable_after_retry"]
}.freeze
EXPECTED_FAILURE_CLASSES = %w[
  database_recovery_notice
  database_error
  api_unreachable
  timeout
  registration_failed
  contact_permission_failed
  unknown
].freeze

def fail!(message)
  warn "agent mail failover snapshot contract: #{message}"
  exit 1
end

def read_json(path)
  JSON.parse(File.read(path))
rescue JSON::ParserError => error
  fail!("#{path} does not parse as JSON: #{error.message}")
end

schema = read_json(SCHEMA)
manifest = read_json(MANIFEST)
classifier_cases = read_json(CLASSIFIER_CASES)
no_service_gate = read_json(NO_SERVICE_GATE)
doc = File.read(DOC)

fail!("schema id drifted") unless schema["$id"]&.end_with?("/ft-agent-mail-failover-snapshot.json")
fail!("contract id const missing") unless schema.dig("properties", "contract_id", "const") == "ft.agent_mail_failover_snapshot.v1"
fail!("source bead const missing") unless schema.dig("properties", "source_bead", "const") == "ft-5lsqo.1"
fail!("mode enum drifted") unless schema.dig("$defs", "mode", "enum").sort == %w[agent_mail_available agent_mail_unavailable_beads_only].sort
fail!("forbidden-action enum drifted") unless schema.dig("$defs", "forbidden_action", "enum").sort == EXPECTED_FORBIDDEN.sort
fail!("classifier failure classes drifted") unless schema.dig("$defs", "failure_class", "enum").compact.sort == (EXPECTED_FAILURE_CLASSES + %w[none]).sort

fail!("manifest schema_version drifted") unless manifest["schema_version"] == 1
fail!("manifest contract id drifted") unless manifest["contract_id"] == "ft.agent_mail_failover_snapshot.fixture_manifest.v1"
fail!("manifest bead drifted") unless manifest["bead"] == "ft-5lsqo.1"
fail!("manifest schema pointer drifted") unless manifest["schema"] == SCHEMA
fail!("manifest contract pointer drifted") unless manifest["contract"] == DOC
fail!("manifest verifier missing") unless manifest.fetch("verification").include?("bash tests/e2e/test_agent_mail_failover_snapshot_contract.sh")
fail!("manifest classifier verifier missing") unless manifest.fetch("verification").include?("bash tests/e2e/test_agent_mail_retry_classifier_contract.sh")
fail!("manifest no-service verifier missing") unless manifest.fetch("verification").include?("bash tests/e2e/test_agent_mail_no_service_action_gate.sh")
fail!("manifest classifier cases pointer drifted") unless manifest["classifier_cases"] == CLASSIFIER_CASES
fail!("manifest no-service gate pointer drifted") unless manifest["no_service_action_gate"] == NO_SERVICE_GATE
fail!("manifest runbook pointer drifted") unless manifest["runbook"] == RUNBOOK
fail!("classifier cases bead drifted") unless classifier_cases["source_bead"] == "ft-5lsqo.2"
fail!("no-service gate bead drifted") unless no_service_gate["source_bead"] == "ft-5lsqo.4"

fixture_paths = manifest.fetch("valid")
fixture_ids = fixture_paths.map { |path| File.basename(path, ".json") }
fail!("fixture ids drifted: #{fixture_ids.sort.inspect}") unless fixture_ids.sort == EXPECTED_FIXTURE_IDS.sort
fixture_paths.each { |path| fail!("manifest references missing fixture #{path}") unless File.file?(path) }

payloads = fixture_paths.map { |path| [File.basename(path, ".json"), read_json(path)] }.to_h
covered_failure_classes = Set.new
payloads.each do |fixture_id, payload|
  fail!("#{fixture_id} schema_version drifted") unless payload["schema_version"] == 1
  fail!("#{fixture_id} contract id drifted") unless payload["contract_id"] == "ft.agent_mail_failover_snapshot.v1"
  fail!("#{fixture_id} source bead drifted") unless payload["source_bead"] == "ft-5lsqo.1"
  fail!("#{fixture_id} retry limit drifted") unless payload.dig("agent_mail", "retry_limit") == 1
  fail!("#{fixture_id} forbidden actions drifted") unless payload.dig("agent_mail", "forbidden_actions").sort == EXPECTED_FORBIDDEN.sort
  fail!("#{fixture_id} next actions missing") if payload.fetch("next_actions").empty?
  fail!("#{fixture_id} artifact path missing self") unless payload.fetch("artifact_paths").include?("fixtures/agent-mail-failover/valid/#{fixture_id}.json")

  expected_failure_class, expected_reason = CLASSIFIER_EXPECTATIONS.fetch(fixture_id)
  mail = payload.fetch("agent_mail")
  if fixture_id == "healthy-agent-mail"
    fail!("#{fixture_id} should succeed on first attempt") unless mail.fetch("attempt_count") == 1
    fail!("#{fixture_id} should not carry failure class") unless mail["failure_class"].nil?
    fail!("#{fixture_id} should not fall back") if mail.fetch("reason_codes").include?("fallback.beads_only")
  else
    fail!("#{fixture_id} degraded mode drifted") unless payload["mode"] == "agent_mail_unavailable_beads_only"
    fail!("#{fixture_id} should represent exactly one retry before fallback") unless mail.fetch("attempt_count") == mail.fetch("retry_limit") + 1
    fail!("#{fixture_id} should not register after degraded startup") if mail.fetch("registered")
    fail!("#{fixture_id} should not check inbox after degraded startup") if mail.fetch("inbox_checked")
    fail!("#{fixture_id} failure class drifted") unless mail.fetch("failure_class") == expected_failure_class
    fail!("#{fixture_id} missing specific classifier reason") unless mail.fetch("reason_codes").include?(expected_reason)
    fail!("#{fixture_id} missing unavailable-after-retry reason") unless mail.fetch("reason_codes").include?("agent_mail.unavailable_after_retry")
    fail!("#{fixture_id} missing fallback reason") unless mail.fetch("reason_codes").include?("fallback.beads_only")
    covered_failure_classes.add(mail.fetch("failure_class"))
  end

  forbidden_guidance = payload.fetch("next_actions").grep(/doctor|repair|restart|kill|reset --hard|clean -fd|delete|local Cargo/i)
  fail!("#{fixture_id} next actions suggest forbidden service or cleanup work: #{forbidden_guidance.inspect}") unless forbidden_guidance.empty?

  SAFETY_FALSE_FLAGS.each do |flag|
    fail!("#{fixture_id} safety flag #{flag} drifted") unless payload.fetch("safety").fetch(flag) == false
  end

  disclaimer = payload.fetch("safety").fetch("proof_disclaimer")
  fail!("#{fixture_id} proof disclaimer missing") if disclaimer.strip.empty?
  fail!("#{fixture_id} raw mail body stored") if payload.dig("safety", "raw_mail_body_stored")
  fail!("#{fixture_id} raw pane text stored") if payload.dig("safety", "raw_pane_text_stored")

  dirty_count = payload.fetch("git").fetch("dirty_count")
  tracked_count = payload.fetch("git").fetch("tracked_dirty_count")
  untracked_count = payload.fetch("git").fetch("untracked_dirty_count")
  fail!("#{fixture_id} dirty counts inconsistent") unless dirty_count == tracked_count + untracked_count
end
fail!("classifier branch coverage drifted") unless covered_failure_classes.sort == EXPECTED_FAILURE_CLASSES.sort

healthy = payloads.fetch("healthy-agent-mail")
fail!("healthy fixture mode drifted") unless healthy["mode"] == "agent_mail_available"
fail!("healthy fixture status drifted") unless healthy.dig("agent_mail", "status") == "available"
fail!("healthy fixture should register") unless healthy.dig("agent_mail", "registered") == true
fail!("healthy fixture should check inbox") unless healthy.dig("agent_mail", "inbox_checked") == true

unavailable = payloads.fetch("unavailable-after-retry")
fail!("unavailable fixture mode drifted") unless unavailable["mode"] == "agent_mail_unavailable_beads_only"
fail!("unavailable fixture attempt count drifted") unless unavailable.dig("agent_mail", "attempt_count") == 2
fail!("unavailable fixture missing fallback reason") unless unavailable.dig("agent_mail", "reason_codes").include?("fallback.beads_only")
fail!("unavailable fixture should not register") unless unavailable.dig("agent_mail", "registered") == false

database = payloads.fetch("database-recovery-retry-exhausted")
fail!("database fixture failure class drifted") unless database.dig("agent_mail", "failure_class") == "database_recovery_notice"
fail!("database fixture missing retry-exhausted reason") unless database.dig("agent_mail", "reason_codes").include?("agent_mail.database_recovery_retry_exhausted")
fail!("database fixture should not suggest repair") if database.fetch("next_actions").any? { |action| action.include?("repair") || action.include?("restart") }

registration = payloads.fetch("registration-failed")
fail!("registration fixture failure class drifted") unless registration.dig("agent_mail", "failure_class") == "registration_failed"
fail!("registration fixture missing reason code") unless registration.dig("agent_mail", "reason_codes").include?("agent_mail.registration_failed_after_retry")

contact = payloads.fetch("contact-permission-failed")
fail!("contact fixture failure class drifted") unless contact.dig("agent_mail", "failure_class") == "contact_permission_failed"
fail!("contact fixture missing reason code") unless contact.dig("agent_mail", "reason_codes").include?("agent_mail.contact_permission_failed_after_retry")

empty = payloads.fetch("empty-in-progress")
fail!("empty fixture in-progress drifted") unless empty.dig("beads", "in_progress_count") == 0
fail!("empty fixture has stale candidates") unless empty.dig("beads", "stale_reopen", "candidates").empty?

stale = payloads.fetch("stale-candidate-clean-tree")
fail!("stale fixture default action drifted") unless stale.dig("beads", "stale_reopen", "default_action") == "status_check_before_reopen"
fail!("stale fixture candidate count drifted") unless stale.dig("beads", "stale_reopen", "candidates").length == 1
fail!("stale fixture dirty risk drifted") unless stale.dig("git", "risk_level") == "low"

dirty = payloads.fetch("dirty-tracked-overlap")
fail!("dirty fixture must do_not_reopen") unless dirty.dig("beads", "stale_reopen", "default_action") == "do_not_reopen"
fail!("dirty fixture risk should be high") unless dirty.dig("git", "risk_level") == "high"
fail!("dirty fixture missing tracked overlap") unless dirty.dig("git", "dirty_paths").any? { |row| row["category"] == "tracked_overlap_risk" }
fail!("dirty fixture candidate should not reopen") unless dirty.dig("beads", "stale_reopen", "candidates").all? { |row| row["recommended_action"] == "do_not_reopen" }

untracked = payloads.fetch("untracked-review-required")
fail!("untracked fixture risk should be medium") unless untracked.dig("git", "risk_level") == "medium"
fail!("untracked fixture missing untracked review path") unless untracked.dig("git", "dirty_paths").any? { |row| row["category"] == "untracked_review_required" }
fail!("untracked fixture must do_not_reopen") unless untracked.dig("beads", "stale_reopen", "default_action") == "do_not_reopen"

%w[
  ft.agent_mail_failover_snapshot.v1
  fixtures/agent-mail-failover/fallback-snapshot.schema.json
  unavailable-after-retry
  database-recovery-retry-exhausted
  registration-failed
  contact-permission-failed
  no-service-action-gate
  agent-mail-failover-runbook
  Local Cargo
].each do |needle|
  fail!("doc missing #{needle}") unless doc.include?(needle)
end

puts "agent mail failover snapshot contract: static verifier passed (#{fixture_paths.length} fixtures, #{EXPECTED_FORBIDDEN.length} forbidden actions)"
RUBY
