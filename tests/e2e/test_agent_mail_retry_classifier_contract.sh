#!/usr/bin/env bash
# Static verifier for the Agent Mail startup retry classifier.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

CLASSIFIER="scripts/agent-mail-failover-classifier.sh"
SWARM_TICK="scripts/swarm-tick.sh"
CASES="fixtures/agent-mail-failover/retry-classifier-cases.json"
SCHEMA="fixtures/agent-mail-failover/fallback-snapshot.schema.json"

fail() {
  printf 'agent mail retry classifier contract: %s\n' "$*" >&2
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
require_file "${CLASSIFIER}"
require_file "${SWARM_TICK}"
require_file "${CASES}"
require_file "${SCHEMA}"

bash -n "${CLASSIFIER}"
bash -n "${SWARM_TICK}"
jq empty "${CASES}" "${SCHEMA}"

ruby <<'RUBY'
require "json"
require "open3"

CLASSIFIER = "scripts/agent-mail-failover-classifier.sh"
SWARM_TICK = "scripts/swarm-tick.sh"
CASES = "fixtures/agent-mail-failover/retry-classifier-cases.json"
SCHEMA = "fixtures/agent-mail-failover/fallback-snapshot.schema.json"
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
FORBIDDEN_GUIDANCE = [
  "am service restart",
  "am service stop",
  "am doctor fix",
  "am doctor repair",
  "am doctor reconstruct",
  "kill am",
  "git reset --hard",
  "git clean -fd"
].freeze

def fail!(message)
  warn "agent mail retry classifier contract: #{message}"
  exit 1
end

def read_json(path)
  JSON.parse(File.read(path))
rescue JSON::ParserError => error
  fail!("#{path} does not parse as JSON: #{error.message}")
end

schema = read_json(SCHEMA)
cases = read_json(CASES)
script = File.read(SWARM_TICK)

fail!("cases version drifted") unless cases["schema_version"] == 1
fail!("cases contract drifted") unless cases["contract_id"] == "ft.agent_mail_failover_retry_classifier.fixture_cases.v1"
fail!("cases bead drifted") unless cases["source_bead"] == "ft-5lsqo.2"
fail!("retry limit drifted") unless cases["retry_limit"] == 1
fail!("schema missing contact_permission_failed") unless schema.dig("$defs", "failure_class", "enum").include?("contact_permission_failed")
fail!("swarm-tick does not source classifier") unless script.include?("agent-mail-failover-classifier.sh")
fail!("swarm-tick fallback does not consume classifier JSON") unless script.include?("agent_mail_json=$(agent_mail_failover_classify_json")
fail!("swarm-tick handoff does not use classifier summary") unless script.include?(".agent_mail.error_summary")

case_ids = cases.fetch("cases").map { |row| row.fetch("id") }
expected_ids = %w[
  success
  transient-database-recovery
  unreachable-api
  timeout-or-hang
  registration-failed
  contact-permission-failed
  unknown-response
]
fail!("classifier case ids drifted: #{case_ids.sort.inspect}") unless case_ids.sort == expected_ids.sort

cases.fetch("cases").each do |row|
  id = row.fetch("id")
  input = row.fetch("input")
  stdout, stderr, status = Open3.capture3("bash", CLASSIFIER, input)
  fail!("#{id} classifier failed: #{stderr}") unless status.success?

  payload = JSON.parse(stdout)
  fail!("#{id} status drifted") unless payload.fetch("status") == row.fetch("expected_status")
  fail!("#{id} attempt_count drifted") unless payload.fetch("attempt_count") == row.fetch("expected_attempt_count")
  fail!("#{id} retry_limit drifted") unless payload.fetch("retry_limit") == cases.fetch("retry_limit")
  fail!("#{id} failure_class drifted") unless payload["failure_class"] == row["expected_failure_class"]
  fail!("#{id} registered drifted") unless payload.fetch("registered") == row.fetch("expected_registered")
  fail!("#{id} inbox_checked drifted") unless payload.fetch("inbox_checked") == row.fetch("expected_inbox_checked")
  fail!("#{id} forbidden action enum drifted") unless payload.fetch("forbidden_actions").sort == EXPECTED_FORBIDDEN.sort

  missing_codes = row.fetch("required_reason_codes") - payload.fetch("reason_codes")
  fail!("#{id} missing reason codes #{missing_codes.inspect}") unless missing_codes.empty?

  if id == "success"
    fail!("success should not fall back") if payload.fetch("reason_codes").include?("fallback.beads_only")
    fail!("success should not have error summary") unless payload["error_summary"].nil?
  else
    fail!("#{id} must use exactly one retry") unless payload.fetch("attempt_count") == 2
    fail!("#{id} must enter Beads-only fallback") unless payload.fetch("reason_codes").include?("fallback.beads_only")
    summary = payload.fetch("error_summary")
    fail!("#{id} missing error summary") if summary.nil? || summary.strip.empty?
    FORBIDDEN_GUIDANCE.each do |needle|
      fail!("#{id} suggests forbidden guidance #{needle}") if summary.include?(needle)
    end
  end
end

puts "agent mail retry classifier contract: static verifier passed (#{cases.fetch("cases").length} cases)"
RUBY
