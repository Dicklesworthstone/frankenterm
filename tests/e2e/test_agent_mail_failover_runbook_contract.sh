#!/usr/bin/env bash
# Static smoke verifier for the Agent Mail failover operator runbook.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

RUNBOOK="docs/robot-contracts/agent-mail-failover-runbook.md"
MANIFEST="fixtures/agent-mail-failover/manifest.json"
GATE="fixtures/agent-mail-failover/no-service-action-gate.json"

fail() {
  printf 'agent mail failover runbook contract: %s\n' "$*" >&2
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
require_file "${RUNBOOK}"
require_file "${MANIFEST}"
require_file "${GATE}"

bash -n "${BASH_SOURCE[0]}"
jq empty "${MANIFEST}" "${GATE}"

ruby <<'RUBY'
require "json"

RUNBOOK = "docs/robot-contracts/agent-mail-failover-runbook.md"
MANIFEST = "fixtures/agent-mail-failover/manifest.json"
GATE = "fixtures/agent-mail-failover/no-service-action-gate.json"
REQUIRED_TEXT = [
  "single allowed retry",
  "scripts/agent-mail-failover-classifier.sh",
  "scripts/swarm-tick.sh --agent-mail-fallback frankenterm",
  "br ready --json",
  "br list --status in_progress --json",
  "bv --robot-triage",
  "br show <id> --json",
  "docs/robot-contracts/agent-mail-stale-reopen-policy.md",
  "do_not_reopen",
  "dirty tracked",
  "untracked",
  "Agent Mail fallback handoff",
  "Agent Mail recovery acknowledgement",
  "communication outage",
  "source defect",
  "RCH fleet pressure",
  "dirty tree",
  "tests/e2e/test_agent_mail_no_service_action_gate.sh"
].freeze
REQUIRED_PATHS = [
  "docs/robot-contracts/agent-mail-failover-snapshot.md",
  "docs/robot-contracts/agent-mail-stale-reopen-policy.md",
  "fixtures/agent-mail-failover/manifest.json",
  "fixtures/agent-mail-failover/no-service-action-gate.json",
  "tests/e2e/test_agent_mail_failover_snapshot_contract.sh",
  "tests/e2e/test_agent_mail_retry_classifier_contract.sh",
  "tests/e2e/test_agent_mail_no_service_action_gate.sh"
].freeze
FORBIDDEN_LITERAL = [
  "am service restart",
  "am service stop",
  "am doctor fix",
  "am doctor repair",
  "am doctor reconstruct",
  "kill am",
  "kill serve-http",
  "kill mcp-agent-mail",
  "git reset --hard",
  "git clean -fd",
  "rm -rf"
].freeze

def fail!(message)
  warn "agent mail failover runbook contract: #{message}"
  exit 1
end

def read_json(path)
  JSON.parse(File.read(path))
rescue JSON::ParserError => error
  fail!("#{path} does not parse as JSON: #{error.message}")
end

runbook = File.read(RUNBOOK)
manifest = read_json(MANIFEST)
gate = read_json(GATE)

fail!("manifest runbook pointer drifted") unless manifest["runbook"] == RUNBOOK
fail!("manifest runbook verifier missing") unless manifest.fetch("verification").include?("bash tests/e2e/test_agent_mail_failover_runbook_contract.sh")
fail!("gate does not scan runbook") unless gate.fetch("scan_paths").include?(RUNBOOK)

REQUIRED_TEXT.each do |needle|
  fail!("runbook missing #{needle}") unless runbook.include?(needle)
end

REQUIRED_PATHS.each do |path|
  fail!("runbook references missing path #{path}") unless runbook.include?(path)
  fail!("referenced path does not exist: #{path}") unless File.file?(path)
end

FORBIDDEN_LITERAL.each do |needle|
  fail!("runbook contains forbidden literal #{needle}") if runbook.include?(needle)
end

handoff = runbook[/Agent Mail fallback handoff for <bead-id>.*?```/m]
fail!("handoff template missing") unless handoff
%w[reason_codes fallback_snapshot_mode dirty_risk proof_commands].each do |field|
  fail!("handoff template missing #{field}") unless handoff.include?(field)
end

recovery = runbook[/Agent Mail recovery acknowledgement.*?```/m]
fail!("recovery template missing") unless recovery
%w[recovered_at mailbox_action beads_state_checked fallback_artifacts_used].each do |field|
  fail!("recovery template missing #{field}") unless recovery.include?(field)
end

puts "agent mail failover runbook contract: static verifier passed (#{REQUIRED_TEXT.length} required terms)"
RUBY
