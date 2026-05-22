#!/usr/bin/env bash
# Static verifier for deferred proof replay ownership-gate fixtures.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

SCHEMA="docs/json-schema/ft-deferred-proof-ownership-gate.json"
DOC="docs/robot-contracts/deferred-proof-ownership-gate.md"
MANIFEST="fixtures/deferred-proof-replay/ownership-gate/manifest.json"
CASES="fixtures/deferred-proof-replay/ownership-gate/cases.v1.json"
PROVENANCE="docs/json-schema/PROVENANCE.md"

fail() {
  printf 'deferred proof ownership gate: %s\n' "$*" >&2
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
require_file "${CASES}"
require_file "${PROVENANCE}"

jq empty "${SCHEMA}" "${MANIFEST}" "${CASES}"

ruby <<'RUBY'
require "json"
require "set"

SCHEMA = "docs/json-schema/ft-deferred-proof-ownership-gate.json"
DOC = "docs/robot-contracts/deferred-proof-ownership-gate.md"
MANIFEST = "fixtures/deferred-proof-replay/ownership-gate/manifest.json"
CASES = "fixtures/deferred-proof-replay/ownership-gate/cases.v1.json"
PROVENANCE = "docs/json-schema/PROVENANCE.md"
CONTRACT_ID = "ft.deferred_proof_ownership_gate.v1"
EXPECTED_CASES = {
  "allow-clean-static" => "allow",
  "wait-rch-critical-pressure" => "wait",
  "dirty-overlap-current" => "dirty_overlap",
  "owner-handoff-active" => "owner_handoff_required",
  "prerequisite-blocked" => "prerequisite_blocked",
  "mail-state-unknown" => "mail_state_unknown",
  "stale-receipt" => "stale",
  "stale-owner-overlap" => "stale",
  "owner-handoff-multiple-receipts" => "owner_handoff_required",
  "wait-shared-tracker-dirty" => "wait"
}.freeze
EXPECTED_STATES = %w[
  allow
  wait
  stale
  dirty_overlap
  owner_handoff_required
  prerequisite_blocked
  mail_state_unknown
].freeze
EXPECTED_FORBIDDEN = %w[
  delete_files
  destructive_git
  stash_or_reset_worktree
  broad_formatting
  local_cargo_proof
  rch_service_repair
  rch_worker_mutation
  agent_mail_repair
  build_cancellation
  prove_unowned_dirty_work
].freeze

def fail!(message)
  warn "deferred proof ownership gate: #{message}"
  exit 1
end

def read_json(path)
  JSON.parse(File.read(path))
rescue JSON::ParserError => error
  fail!("#{path} does not parse as JSON: #{error.message}")
end

def path_safe?(path)
  path.is_a?(String) &&
    !path.empty? &&
    !path.start_with?("/") &&
    !path.include?("\\") &&
    path.split("/").none? { |part| part.empty? || part == "." || part == ".." || part == ".git" }
end

def dirty_paths(gate)
  gate.fetch("current_checkout").fetch("dirty_paths").map { |entry| entry.fetch("path") }
end

def current_dirty_overlap(gate)
  gate.fetch("receipt").fetch("owned_paths") & dirty_paths(gate)
end

def captured_dirty_overlap(gate)
  gate.fetch("receipt").fetch("owned_paths") & gate.fetch("receipt").fetch("dirty_paths_at_capture")
end

def active_owner_overlap(gate)
  owned = gate.fetch("receipt").fetch("owned_paths")
  gate.fetch("coordination").fetch("in_progress_owners").any? do |owner|
    owner.fetch("stale_over_2h") == false && (owned & owner.fetch("owned_paths")).any?
  end
end

def stale_owner_overlap(gate)
  owned = gate.fetch("receipt").fetch("owned_paths")
  gate.fetch("coordination").fetch("in_progress_owners").any? do |owner|
    owner.fetch("stale_over_2h") == true && (owned & owner.fetch("owned_paths")).any?
  end
end

def queued_receipt_overlap(gate)
  owned = gate.fetch("receipt").fetch("owned_paths")
  gate.fetch("coordination").fetch("queued_receipt_owners").any? do |receipt|
    (owned & receipt.fetch("owned_paths")).any?
  end
end

def blocked_prerequisite?(gate)
  gate.fetch("coordination").fetch("prerequisite_beads").any? do |bead|
    bead.fetch("status") != "closed"
  end
end

def unknown_mail_without_fallback?(gate)
  coordination = gate.fetch("coordination")
  coordination.fetch("agent_mail_state") == "unknown" &&
    coordination.fetch("agent_mail_fallback_snapshot") == false
end

schema = read_json(SCHEMA)
manifest = read_json(MANIFEST)
cases = read_json(CASES)
doc = File.read(DOC)
provenance = File.read(PROVENANCE)

fail!("schema id drifted") unless schema["$id"]&.end_with?("/ft-deferred-proof-ownership-gate.json")
fail!("contract const drifted") unless schema.dig("properties", "contract_id", "const") == CONTRACT_ID
fail!("decision enum drifted") unless schema.dig("$defs", "decision_state", "enum").sort == EXPECTED_STATES.sort
fail!("forbidden enum drifted") unless schema.dig("$defs", "forbidden_action", "enum").sort == EXPECTED_FORBIDDEN.sort
fail!("dirty path categories missing owned overlap") unless schema.dig("$defs", "dirty_path", "properties", "category", "enum").include?("owned_overlap")
fail!("coordination schema missing queued receipt owners") unless schema.dig("$defs", "coordination_snapshot", "required").include?("queued_receipt_owners")

fail!("manifest contract drifted") unless manifest["contract_id"] == "ft.deferred_proof_ownership_gate.fixture_manifest.v1"
fail!("manifest bead drifted") unless manifest["bead"] == "ft-zbnz4.3"
fail!("manifest schema path drifted") unless manifest["schema"] == SCHEMA
fail!("manifest contract path drifted") unless manifest["contract"] == DOC
fail!("manifest cases path drifted") unless manifest["cases"] == CASES
fail!("manifest case count drifted") unless manifest.dig("golden_summary", "case_count") == EXPECTED_CASES.length
fail!("manifest decision states drifted") unless manifest.dig("golden_summary", "decision_states").sort == EXPECTED_STATES.sort

fixture_cases = cases.fetch("cases")
ids = fixture_cases.map { |entry| entry.fetch("case_id") }
fail!("fixture ids drifted: #{ids.sort.inspect}") unless ids.sort == EXPECTED_CASES.keys.sort
fail!("fixture ids are not unique") unless ids.uniq.length == ids.length

fixture_cases.each do |entry|
  case_id = entry.fetch("case_id")
  gate = entry.fetch("gate")
  decision = gate.fetch("decision")
  owned = gate.fetch("receipt").fetch("owned_paths")
  current_overlap = current_dirty_overlap(gate)
  captured_overlap = captured_dirty_overlap(gate)

  fail!("#{case_id} contract drifted") unless gate["contract_id"] == CONTRACT_ID
  fail!("#{case_id} bead drifted") unless gate["bead_id"].start_with?("ft-")
  fail!("#{case_id} forbidden actions drifted") unless gate.fetch("forbidden_actions").sort == EXPECTED_FORBIDDEN.sort
  fail!("#{case_id} decision drifted") unless decision.fetch("state") == EXPECTED_CASES.fetch(case_id)
  fail!("#{case_id} missing reason codes") if decision.fetch("reason_codes").empty?
  fail!("#{case_id} missing explanation") if decision.fetch("explanation").strip.empty?
  fail!("#{case_id} owned paths empty") if owned.empty?
  owned.each { |path| fail!("#{case_id} unsafe owned path #{path.inspect}") unless path_safe?(path) }
  dirty_paths(gate).each { |path| fail!("#{case_id} unsafe dirty path #{path.inspect}") unless path_safe?(path) }
  gate.fetch("receipt").fetch("dirty_paths_at_capture").each do |path|
    fail!("#{case_id} unsafe captured dirty path #{path.inspect}") unless path_safe?(path)
  end
  gate.fetch("coordination").fetch("in_progress_owners").each do |owner|
    owner.fetch("owned_paths").each do |path|
      fail!("#{case_id} unsafe owner path #{path.inspect}") unless path_safe?(path)
    end
  end
  gate.fetch("coordination").fetch("queued_receipt_owners").each do |receipt|
    receipt.fetch("owned_paths").each do |path|
      fail!("#{case_id} unsafe queued receipt path #{path.inspect}") unless path_safe?(path)
    end
  end

  if decision.fetch("state") == "allow"
    fail!("#{case_id} allow must permit replay") unless decision.fetch("replay_allowed") == true
    fail!("#{case_id} allow has current overlap") unless current_overlap.empty?
    fail!("#{case_id} allow has captured overlap") unless captured_overlap.empty?
    fail!("#{case_id} allow has active owner overlap") if active_owner_overlap(gate)
    fail!("#{case_id} allow has stale owner overlap") if stale_owner_overlap(gate)
    fail!("#{case_id} allow has queued receipt overlap") if queued_receipt_overlap(gate)
    fail!("#{case_id} allow has blocked prerequisite") if blocked_prerequisite?(gate)
    fail!("#{case_id} allow has stale receipt") unless gate.dig("receipt", "freshness_state") == "fresh"
  else
    fail!("#{case_id} non-allow permits replay") unless decision.fetch("replay_allowed") == false
  end

  case decision.fetch("state")
  when "dirty_overlap"
    fail!("#{case_id} dirty_overlap without overlap") if current_overlap.empty? && captured_overlap.empty?
    fail!("#{case_id} dirty_overlap missing reason") unless decision.fetch("reason_codes").include?("git.dirty_overlap")
  when "owner_handoff_required"
    fail!("#{case_id} owner handoff without active or queued overlap") unless active_owner_overlap(gate) || queued_receipt_overlap(gate)
    reasons = decision.fetch("reason_codes")
    fail!("#{case_id} owner handoff missing reason") unless reasons.include?("owner.active_overlap") || reasons.include?("receipt.multiple_owner_overlap")
  when "prerequisite_blocked"
    fail!("#{case_id} prerequisite decision without blocked prereq") unless blocked_prerequisite?(gate)
    fail!("#{case_id} prerequisite missing reason") unless decision.fetch("reason_codes").include?("beads.prerequisite_blocked")
  when "mail_state_unknown"
    fail!("#{case_id} mail decision without unknown mail") unless unknown_mail_without_fallback?(gate)
    fail!("#{case_id} mail missing reason") unless decision.fetch("reason_codes").include?("agent_mail.state_unknown")
  when "stale"
    fail!("#{case_id} stale decision without stale receipt or owner") unless gate.dig("receipt", "freshness_state") == "stale" || stale_owner_overlap(gate)
    stale_reasons = decision.fetch("reason_codes")
    fail!("#{case_id} stale missing reason") unless stale_reasons.include?("receipt.stale") || stale_reasons.include?("owner.stale_overlap")
  when "wait"
    wait_reasons = decision.fetch("reason_codes")
    fail!("#{case_id} wait lacks waiting reason") unless wait_reasons.any? { |reason| reason.start_with?("rch.") || reason == "git.shared_tracker_dirty" }
  end
end

EXPECTED_STATES.each do |state|
  fail!("doc missing state #{state}") unless doc.include?(state)
end
EXPECTED_FORBIDDEN.each do |action|
  fail!("doc missing forbidden action #{action}") unless doc.include?(action)
end
fail!("doc missing fixture path") unless doc.include?("fixtures/deferred-proof-replay/ownership-gate/")
fail!("provenance missing ownership gate row") unless provenance.include?("`ft-deferred-proof-ownership-gate.json`")
fail!("provenance missing ownership gate verifier") unless provenance.include?("bash tests/e2e/test_deferred_proof_ownership_gate.sh")

puts "deferred proof ownership gate: static verifier passed (#{ids.length} cases, #{EXPECTED_STATES.length} decision states)"
RUBY
