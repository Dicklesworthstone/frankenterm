#!/usr/bin/env bash
# Static verifier for the RCH worker storage pressure runbook.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

RUNBOOK="docs/robot-contracts/rch-worker-storage-runbook.md"
CONTRACT="fixtures/rch-worker-storage-runbook/contract.v1.json"

fail() {
  printf 'rch worker storage runbook contract: %s\n' "$*" >&2
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
require_file "${CONTRACT}"

bash -n "${BASH_SOURCE[0]}"
jq empty "${CONTRACT}"

ruby <<'RUBY'
require "json"

RUNBOOK = "docs/robot-contracts/rch-worker-storage-runbook.md"
CONTRACT = "fixtures/rch-worker-storage-runbook/contract.v1.json"
FORBIDDEN_LITERALS = [
  "am service restart",
  "am service stop",
  "am doctor fix",
  "am doctor repair",
  "am doctor reconstruct",
  "git reset --hard",
  "git clean -fd",
  "rm -rf",
  "rch daemon restart",
  "rch workers disable",
  "cancel build"
].freeze
REQUIRED_TERMS = [
  "no_admissible_workers=critical_pressure=5",
  "source-code defects",
  "RCH fleet pressure",
  "dirty-tree contamination",
  "Agent Mail outages",
  "Beads tracker state",
  "Inventory evidence alone is never enough",
  "operator-approved recovery",
  "RCH_REQUIRE_REMOTE=1",
  "CARGO_TARGET_DIR=<target-dir>",
  "gate_result=passed_remote_smoke",
  "admission_recovered=true",
  "Do not substitute local Cargo",
  "RCH worker storage pressure handoff",
  "Agent Mail handoff"
].freeze

def fail!(message)
  warn "rch worker storage runbook contract: #{message}"
  exit 1
end

def read_json(path)
  JSON.parse(File.read(path))
rescue JSON::ParserError => error
  fail!("#{path} does not parse as JSON: #{error.message}")
end

runbook = File.read(RUNBOOK)
contract = read_json(CONTRACT)

fail!("contract id drifted") unless contract["contract_id"] == "ft.rch_worker_storage_runbook.v1"
fail!("source bead drifted") unless contract["source_bead"] == "ft-5xwsu.4"
fail!("runbook pointer drifted") unless contract["runbook"] == RUNBOOK

contract.fetch("required_sections").each do |section|
  fail!("runbook missing section #{section}") unless runbook.include?("## #{section}")
end

contract.fetch("required_contracts").each do |path|
  fail!("runbook missing contract path #{path}") unless runbook.include?(path)
  fail!("referenced contract path missing #{path}") unless File.file?(path)
end

contract.fetch("required_verifiers").each do |path|
  fail!("runbook missing verifier path #{path}") unless runbook.include?(path)
  fail!("referenced verifier path missing #{path}") unless File.file?(path)
end

contract.fetch("read_only_commands").each do |command|
  fail!("runbook missing read-only command #{command}") unless runbook.include?(command)
end

contract.fetch("post_recovery_commands").each do |command|
  fail!("runbook missing post-recovery command #{command}") unless runbook.include?(command)
end

contract.fetch("forbidden_actions").each do |action|
  fail!("runbook missing forbidden action #{action}") unless runbook.include?(action)
end

contract.fetch("artifact_layouts").each do |layout|
  fail!("runbook missing artifact layout #{layout}") unless runbook.include?(layout)
end

contract.fetch("classification_streams").each do |stream|
  fail!("runbook missing classification stream #{stream}") unless runbook.include?(stream)
end

REQUIRED_TERMS.each do |term|
  fail!("runbook missing required term #{term}") unless runbook.include?(term)
end

FORBIDDEN_LITERALS.each do |literal|
  fail!("runbook contains forbidden literal #{literal}") if runbook.include?(literal)
  fail!("contract contains forbidden literal #{literal}") if JSON.generate(contract).include?(literal)
end

handoff = runbook[/RCH worker storage pressure handoff for <bead-id>.*?```/m]
fail!("Beads handoff template missing") unless handoff
%w[evidence_class inventory_artifact approval_artifact recovery_reference post_recovery_gate_result selected_worker stable_reason_code proof_commands avoided_actions].each do |field|
  fail!("Beads handoff template missing #{field}") unless handoff.include?(field)
end

mail = runbook[/Thread: <bead-id>.*?```/m]
fail!("Agent Mail handoff template missing") unless mail
%w[Current Inventory Approval Recovery Blocking Next Owned Avoided].each do |field|
  fail!("Agent Mail handoff template missing #{field}") unless mail.include?(field)
end

read_only = contract.fetch("read_only_commands")
fail!("read-only command list must include br dep cycles") unless read_only.include?("br dep cycles --json")
fail!("read-only command list must include git status") unless read_only.include?("git status --short")

post = contract.fetch("post_recovery_commands")
fail!("post-recovery dry-run missing") unless post.any? { |command| command.include?("diagnose --dry-run") && command.include?("RCH_REQUIRE_REMOTE=1") }
fail!("post-recovery smoke missing") unless post.any? { |command| command.include?("rch --no-self-healing exec") && command.include?("RCH_REQUIRE_REMOTE=1") }

puts "rch worker storage runbook contract: static verifier passed (#{contract.fetch("required_sections").length} sections, #{contract.fetch("forbidden_actions").length} forbidden actions)"
RUBY
