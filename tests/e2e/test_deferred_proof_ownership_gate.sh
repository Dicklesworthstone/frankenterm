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
# Free-text explanation must be bounded so it cannot become a raw-content sink.
EXPLANATION_MAX_LEN = 512
fail!("explanation must pin maxLength #{EXPLANATION_MAX_LEN}") unless schema.dig("$defs", "gate_decision", "properties", "explanation", "maxLength") == EXPLANATION_MAX_LEN
# Worker/agent identity fields must use the safe identity charset (matching the
# receipt schema) so an unbounded free string cannot ride in as an identity.
IDENTITY_PATTERN = "^[A-Za-z0-9._-]+$"
fail!("selected_worker must pin the identity pattern") unless schema.dig("$defs", "rch_admission_snapshot", "properties", "selected_worker", "pattern") == IDENTITY_PATTERN
fail!("assignee must pin the identity pattern") unless schema.dig("$defs", "in_progress_owner", "properties", "assignee", "pattern") == IDENTITY_PATTERN
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
  fail!("#{case_id} explanation exceeds #{EXPLANATION_MAX_LEN} chars") if decision.fetch("explanation").length > EXPLANATION_MAX_LEN
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

# Golden identity hygiene: every selected_worker (when present) and every
# in-progress owner assignee must match the safe identity charset.
identity_re = Regexp.new(IDENTITY_PATTERN)
fixture_cases.each do |entry|
  gate = entry.fetch("gate")
  case_id = entry.fetch("case_id")
  worker = gate.dig("rch_admission", "selected_worker")
  fail!("#{case_id} selected_worker is not a safe identity: #{worker.inspect}") unless worker.nil? || worker.match?(identity_re)
  gate.fetch("coordination").fetch("in_progress_owners").each do |owner|
    name = owner.fetch("assignee")
    fail!("#{case_id} owner assignee is not a safe identity: #{name.inspect}") unless name.match?(identity_re)
  end
end

# Negative corpus: prove the safety predicates actually fire on a violated
# gate. Start from the clean "allow" golden and apply one targeted corruption
# per predicate, asserting the predicate trips (and does not false-positive on
# the safe shape). This guards against a predicate being silently weakened — a
# weakened guard would still pass every positive golden case above.
dup_gate = ->(g) { Marshal.load(Marshal.dump(g)) }
clean = fixture_cases.find { |entry| entry.fetch("case_id") == "allow-clean-static" }
fail!("negative corpus needs the allow-clean-static golden") unless clean
base = clean.fetch("gate")
fail!("negative-corpus base is not a clean allow") unless base.fetch("decision").fetch("state") == "allow"

# 1. path_safe? must reject absolute, traversal, .git, and empty-segment paths.
["/etc/passwd", "../escape", ".git/config", "foo//bar", "foo/./bar", ""].each do |bad|
  fail!("path_safe? accepted unsafe path #{bad.inspect}") if path_safe?(bad)
end
fail!("path_safe? rejected a known-good repo path") unless path_safe?("tests/e2e/test_deferred_proof_ownership_gate.sh")

# 2. A dirty path overlapping an owned path must register as current overlap
#    (the condition that forbids an allow decision).
overlap_gate = dup_gate.call(base)
owned_first = overlap_gate.fetch("receipt").fetch("owned_paths").first
overlap_gate.fetch("current_checkout").fetch("dirty_paths") <<
  { "path" => owned_first, "status" => " M", "category" => "owned_overlap" }
fail!("current_dirty_overlap missed an injected owned-path overlap") if current_dirty_overlap(overlap_gate).empty?

# 3. An open prerequisite bead must register as blocked; a closed one must not.
open_prereq = dup_gate.call(base)
open_prereq.fetch("coordination").fetch("prerequisite_beads") << { "bead_id" => "ft-open1", "status" => "open" }
fail!("blocked_prerequisite? missed an open prerequisite") unless blocked_prerequisite?(open_prereq)
closed_prereq = dup_gate.call(base)
closed_prereq.fetch("coordination").fetch("prerequisite_beads") << { "bead_id" => "ft-done1", "status" => "closed" }
fail!("blocked_prerequisite? false-positived on a closed prerequisite") if blocked_prerequisite?(closed_prereq)

# 4. Unknown mail without a fallback snapshot must be flagged; with one, not.
mail_unknown = dup_gate.call(base)
mail_unknown.fetch("coordination")["agent_mail_state"] = "unknown"
mail_unknown.fetch("coordination")["agent_mail_fallback_snapshot"] = false
fail!("unknown_mail_without_fallback? missed unknown mail with no fallback") unless unknown_mail_without_fallback?(mail_unknown)
mail_fallback = dup_gate.call(base)
mail_fallback.fetch("coordination")["agent_mail_state"] = "unknown"
mail_fallback.fetch("coordination")["agent_mail_fallback_snapshot"] = true
fail!("unknown_mail_without_fallback? ignored a present fallback snapshot") if unknown_mail_without_fallback?(mail_fallback)

# 5. A stale (>2h) in-progress owner overlapping owned paths registers as a
#    stale overlap, not an active one.
stale_owner = dup_gate.call(base)
stale_owner.fetch("coordination").fetch("in_progress_owners") <<
  { "agent" => "GhostOwner", "stale_over_2h" => true, "owned_paths" => [stale_owner.fetch("receipt").fetch("owned_paths").first] }
fail!("stale_owner_overlap missed a stale overlapping owner") unless stale_owner_overlap(stale_owner)
fail!("active_owner_overlap false-positived on a stale-only owner") if active_owner_overlap(stale_owner)

# 6. A fresh (<2h) in-progress owner overlapping owned paths registers as an
#    ACTIVE overlap (the predicate must actually be reachable as true, not a
#    dead guard), and not as a stale one.
active_owner = dup_gate.call(base)
active_owner.fetch("coordination").fetch("in_progress_owners") <<
  { "agent" => "LiveOwner", "stale_over_2h" => false, "owned_paths" => [active_owner.fetch("receipt").fetch("owned_paths").first] }
fail!("active_owner_overlap missed a fresh overlapping owner") unless active_owner_overlap(active_owner)
fail!("stale_owner_overlap false-positived on a fresh owner") if stale_owner_overlap(active_owner)

# 7. A queued receipt owner overlapping owned paths registers as a queued
#    receipt overlap (owner-handoff territory).
queued = dup_gate.call(base)
queued.fetch("coordination").fetch("queued_receipt_owners") <<
  { "receipt_id" => "ft-queued1:comment-1", "owned_paths" => [queued.fetch("receipt").fetch("owned_paths").first] }
fail!("queued_receipt_overlap missed an overlapping queued receipt") unless queued_receipt_overlap(queued)

EXPECTED_STATES.each do |state|
  fail!("doc missing state #{state}") unless doc.include?(state)
end
EXPECTED_FORBIDDEN.each do |action|
  fail!("doc missing forbidden action #{action}") unless doc.include?(action)
end
fail!("doc missing fixture path") unless doc.include?("fixtures/deferred-proof-replay/ownership-gate/")
fail!("provenance missing ownership gate row") unless provenance.include?("`ft-deferred-proof-ownership-gate.json`")
fail!("provenance missing ownership gate verifier") unless provenance.include?("bash tests/e2e/test_deferred_proof_ownership_gate.sh")

puts "deferred proof ownership gate: static verifier passed (#{ids.length} cases, #{EXPECTED_STATES.length} decision states, predicate negative corpus green)"
RUBY
