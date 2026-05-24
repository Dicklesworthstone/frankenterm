#!/usr/bin/env bash
# Static verifier for the deferred proof queue surface fixtures (ft-zbnz4.5).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

SCHEMA="docs/json-schema/ft-deferred-proof-queue-surface.json"
DOC="docs/robot-contracts/deferred-proof-queue-surface.md"
MANIFEST="fixtures/deferred-proof-replay/queue-surface/manifest.json"
CASES="fixtures/deferred-proof-replay/queue-surface/cases.v1.json"
PROVENANCE="docs/json-schema/PROVENANCE.md"

fail() {
  printf 'deferred proof queue surface contract: %s\n' "$*" >&2
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
require "digest"
require "json"
require "set"

SCHEMA = "docs/json-schema/ft-deferred-proof-queue-surface.json"
DOC = "docs/robot-contracts/deferred-proof-queue-surface.md"
MANIFEST = "fixtures/deferred-proof-replay/queue-surface/manifest.json"
CASES = "fixtures/deferred-proof-replay/queue-surface/cases.v1.json"
PROVENANCE = "docs/json-schema/PROVENANCE.md"
CONTRACT_ID = "ft.deferred_proof_queue_surface.v1"

REQUIRED_STATUSES = %w[
  runnable wait_rch dirty_overlap prerequisite_blocked stale_command ambiguous completed
].freeze
ALLOWLIST = %w[
  wait_for_rch_admission resolve_dirty_overlap complete_prerequisite_bead
  refresh_command_shape request_human_triage none
].freeze
REMEDIATION_BY_STATUS = {
  "runnable" => "none",
  "completed" => "none",
  "wait_rch" => "wait_for_rch_admission",
  "dirty_overlap" => "resolve_dirty_overlap",
  "prerequisite_blocked" => "complete_prerequisite_bead",
  "stale_command" => "refresh_command_shape",
  "ambiguous" => "request_human_triage"
}.freeze
# Phrases that would propose an unsafe automatic action. Scanned over the
# surface's human-readable `why` strings only (never the remote command_preview
# argv, which legitimately runs cargo through rch).
FORBIDDEN = /\blocal cargo\b|\bcargo build\b|\brun locally\b|\brunning locally\b|\blocal fallback\b|\brm -rf\b|\bgit reset\b|--hard|\breset --|\brestart\b|fmt --all|mutate.*worker|repair.*(rch|service|worker)/i
DOC_TERMS = [
  "ft.deferred_proof_queue_surface.v1",
  "runnable",
  "wait_rch",
  "dirty_overlap",
  "prerequisite_blocked",
  "stale_command",
  "ambiguous",
  "completed",
  "local Cargo",
  "fixtures/deferred-proof-replay/queue-surface/"
].freeze

def fail!(message)
  warn "deferred proof queue surface contract: #{message}"
  exit 1
end

def read_json(path)
  JSON.parse(File.read(path))
rescue JSON::ParserError => error
  fail!("#{path} does not parse as JSON: #{error.message}")
end

def collect_keys(node, acc)
  case node
  when Hash
    node.each do |key, value|
      acc << key
      collect_keys(value, acc)
    end
  when Array
    node.each { |item| collect_keys(item, acc) }
  end
  acc
end

def remote_shape_valid?(argv)
  return false unless argv.first == "rch"

  exec_index = argv.index("exec")
  return false unless exec_index && argv[exec_index + 1] == "--"

  argv[1...exec_index].include?("--no-self-healing")
end

schema = read_json(SCHEMA)
manifest = read_json(MANIFEST)
surface = read_json(CASES)
doc = File.read(DOC)
provenance = File.read(PROVENANCE)

# Schema sanity.
fail!("schema id drifted") unless schema["$id"]&.end_with?("/ft-deferred-proof-queue-surface.json")
fail!("schema contract const drifted") unless schema.dig("properties", "contract_id", "const") == CONTRACT_ID
fail!("schema source digest pattern missing") unless schema.dig("$defs", "source", "properties", "source_text_sha256", "pattern") == "^[0-9a-f]{64}$"
fail!("schema permits raw pane content") unless schema.dig("$defs", "source", "properties", "raw_pane_content_stored", "const") == false

# Manifest sanity.
fail!("manifest contract drifted") unless manifest["contract_id"] == CONTRACT_ID
fail!("manifest schema path drifted") unless manifest["schema"] == SCHEMA
fail!("manifest cases path drifted") unless manifest["cases"] == CASES
fail!("manifest status coverage drifted") unless manifest.dig("golden_summary", "statuses").sort == REQUIRED_STATUSES.sort

# Surface contract id + redaction (no raw source/pane text anywhere).
fail!("surface contract drifted") unless surface["contract_id"] == CONTRACT_ID
keys = collect_keys(surface, [])
%w[source_text pane_text raw_pane_text raw_pane_content].each do |banned|
  fail!("surface leaks raw key #{banned}") if keys.include?(banned)
end

# Guardrails: every fail-closed assertion is true and the allowlist is exact.
guardrails = surface.fetch("guardrails")
%w[forbids_local_cargo forbids_worker_mutation forbids_service_repair forbids_deletion forbids_reset forbids_broad_formatting].each do |flag|
  fail!("guardrail #{flag} is not true") unless guardrails.fetch(flag) == true
end
fail!("remediation allowlist drifted") unless guardrails.fetch("remediation_allowlist").sort == ALLOWLIST.sort

queue = surface.fetch("queue")
bead_ids = queue.map { |entry| entry.fetch("bead_id") }
fail!("queue bead ids are not unique") unless bead_ids.uniq.length == bead_ids.length
fail!("queue is not sorted by bead_id") unless bead_ids == bead_ids.sort

# Per-entry invariants.
status_counts = Hash.new(0)
queue.each do |entry|
  bead = entry.fetch("bead_id")
  status = entry.fetch("status")
  fail!("#{bead} has unknown status #{status}") unless REQUIRED_STATUSES.include?(status)
  status_counts[status] += 1

  comment_id = entry.fetch("source").fetch("comment_id")
  expected_receipt = "#{bead}:comment-#{comment_id}"
  fail!("#{bead} receipt_id drifted") unless entry.fetch("receipt_id") == expected_receipt

  source = entry.fetch("source")
  fail!("#{bead} stored raw pane content") unless source.fetch("raw_pane_content_stored") == false
  expected_digest = Digest::SHA256.hexdigest(expected_receipt)
  fail!("#{bead} source digest is not provenance-consistent") unless source.fetch("source_text_sha256") == expected_digest

  fail!("#{bead} reason_codes empty") if entry.fetch("reason_codes").empty?

  expected_remediation = REMEDIATION_BY_STATUS.fetch(status)
  fail!("#{bead} remediation #{entry.fetch("remediation").inspect} != #{expected_remediation.inspect}") unless entry.fetch("remediation") == expected_remediation

  # replay_allowed is true if and only if the entry is runnable.
  fail!("#{bead} replay_allowed must mirror runnable status") unless entry.fetch("replay_allowed") == (status == "runnable")
end

# Summary consistency.
summary = surface.fetch("summary")
fail!("summary.total drifted") unless summary.fetch("total") == queue.length
completed = status_counts["completed"]
fail!("summary.completed drifted") unless summary.fetch("completed") == completed
fail!("summary.queued drifted") unless summary.fetch("queued") == queue.length - completed
REQUIRED_STATUSES.each do |status|
  fail!("summary.by_status.#{status} drifted") unless summary.dig("by_status", status) == status_counts[status]
end
present = status_counts.keys.sort
fail!("not all statuses are exercised: #{present.inspect}") unless present == REQUIRED_STATUSES.sort

# Next candidate must be the single runnable, replay-allowed receipt.
runnable = queue.select { |entry| entry.fetch("status") == "runnable" && entry.fetch("replay_allowed") }
next_candidate = surface.fetch("next_candidate")
if runnable.empty?
  fail!("next_candidate must be null when nothing is runnable") unless next_candidate.nil?
else
  fail!("next_candidate must be present when a runnable receipt exists") if next_candidate.nil?
  entry = runnable.find { |item| item.fetch("bead_id") == next_candidate.fetch("bead_id") }
  fail!("next_candidate bead is not a runnable queue entry") unless entry
  fail!("next_candidate receipt_id drifted") unless next_candidate.fetch("receipt_id") == entry.fetch("receipt_id")
  fail!("next_candidate target_dir drifted") unless next_candidate.fetch("target_dir") == entry.fetch("target_dir")
  fail!("next_candidate material flag drifted") unless next_candidate.fetch("material_remote_required") == entry.fetch("material_remote_required")
  argv = next_candidate.fetch("command_preview")
  if next_candidate.fetch("material_remote_required")
    fail!("next_candidate command is not remote-only shape") unless remote_shape_valid?(argv)
    target = next_candidate.fetch("target_dir")
    fail!("next_candidate command does not pin its target dir") unless target.nil? || argv.include?("CARGO_TARGET_DIR=#{target}")
  end
end

# Explain view covers exactly the blocked (non-runnable, non-completed) entries.
blocked = queue.reject { |entry| %w[runnable completed].include?(entry.fetch("status")) }
blocked_by_id = blocked.to_h { |entry| [entry.fetch("bead_id"), entry] }
explain = surface.fetch("explain")
explain_ids = explain.map { |entry| entry.fetch("bead_id") }
fail!("explain bead ids are not unique") unless explain_ids.uniq.length == explain_ids.length
fail!("explain coverage drifted: #{explain_ids.sort.inspect}") unless explain_ids.sort == blocked_by_id.keys.sort
explain.each do |entry|
  bead = entry.fetch("bead_id")
  queue_entry = blocked_by_id.fetch(bead)
  fail!("explain #{bead} status drifted") unless entry.fetch("status") == queue_entry.fetch("status")
  fail!("explain #{bead} remediation drifted") unless entry.fetch("remediation") == queue_entry.fetch("remediation")
  fail!("explain #{bead} blocking reasons drifted") unless entry.fetch("blocking_reason_codes") == queue_entry.fetch("reason_codes")
  why = entry.fetch("why")
  fail!("explain #{bead} 'why' proposes an unsafe action") if why.match?(FORBIDDEN)
end

# Robot dispatch mirrors the queue 1:1.
dispatch = surface.fetch("robot_dispatch")
dispatch_ids = dispatch.map { |entry| entry.fetch("bead_id") }
fail!("robot_dispatch is not sorted by bead_id") unless dispatch_ids == dispatch_ids.sort
fail!("robot_dispatch coverage drifted") unless dispatch_ids.sort == bead_ids.sort
queue_by_id = queue.to_h { |entry| [entry.fetch("bead_id"), entry] }
dispatch.each do |entry|
  bead = entry.fetch("bead_id")
  queue_entry = queue_by_id.fetch(bead)
  fail!("dispatch #{bead} status drifted") unless entry.fetch("status") == queue_entry.fetch("status")
  fail!("dispatch #{bead} target_dir drifted") unless entry.fetch("target_dir") == queue_entry.fetch("target_dir")
  fail!("dispatch #{bead} replay_allowed drifted") unless entry.fetch("replay_allowed") == queue_entry.fetch("replay_allowed")
end

# Determinism: a second parse of the same bytes yields identical canonical JSON.
fail!("surface is not deterministic across parses") unless JSON.generate(read_json(CASES)) == JSON.generate(surface)

# Doc + provenance.
DOC_TERMS.each do |term|
  fail!("doc missing contract term #{term}") unless doc.include?(term)
end
fail!("provenance missing queue-surface row") unless provenance.include?("`ft-deferred-proof-queue-surface.json`")
fail!("provenance row missing verifier") unless provenance.include?("bash tests/e2e/test_deferred_proof_queue_surface_contract.sh")

puts "deferred proof queue surface contract: static verifier passed (#{queue.length} entries, #{completed} completed, #{explain.length} explain views)"
RUBY
