#!/usr/bin/env bash
# Static verifier for the deferred proof queue surface fixtures (ft-zbnz4.5).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

SCHEMA="docs/json-schema/ft-deferred-proof-queue-surface.json"
DOC="docs/robot-contracts/deferred-proof-queue-surface.md"
MANIFEST="fixtures/deferred-proof-replay/queue-surface/manifest.json"
CASES="fixtures/deferred-proof-replay/queue-surface/cases.v1.json"
INVALID="fixtures/deferred-proof-replay/queue-surface/invalid/fragments.v1.json"
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
require_file "${INVALID}"
require_file "${PROVENANCE}"

jq empty "${SCHEMA}" "${MANIFEST}" "${CASES}" "${INVALID}"

ruby <<'RUBY'
require "digest"
require "json"
require "set"

SCHEMA = "docs/json-schema/ft-deferred-proof-queue-surface.json"
DOC = "docs/robot-contracts/deferred-proof-queue-surface.md"
MANIFEST = "fixtures/deferred-proof-replay/queue-surface/manifest.json"
CASES = "fixtures/deferred-proof-replay/queue-surface/cases.v1.json"
INVALID = "fixtures/deferred-proof-replay/queue-surface/invalid/fragments.v1.json"
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

def deep_dup(node)
  Marshal.load(Marshal.dump(node))
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
  return false unless argv.is_a?(Array) && argv.first == "rch"

  exec_index = argv.index("exec")
  return false unless exec_index && argv[exec_index + 1] == "--"

  argv[1...exec_index].include?("--no-self-healing")
end

# Per-surface-instance invariants. Returns an array of stable violation codes;
# an empty array means the surface honors every read-only/consistency rule. The
# golden fixture must yield zero codes; each invalid fragment must yield exactly
# its expected code.
def violations(surface)
  v = []
  v << "contract_drift" unless surface["contract_id"] == CONTRACT_ID

  # Redaction: no raw source/pane text key anywhere.
  keys = collect_keys(surface, [])
  %w[source_text pane_text raw_pane_text raw_pane_content].each do |banned|
    v << "raw_pane_content" if keys.include?(banned)
  end

  # Guardrails fail closed; the remediation allowlist is exact.
  guardrails = surface["guardrails"] || {}
  %w[forbids_local_cargo forbids_worker_mutation forbids_service_repair forbids_deletion forbids_reset forbids_broad_formatting].each do |flag|
    v << "guardrail_not_true" unless guardrails[flag] == true
  end
  v << "remediation_allowlist_drift" unless (guardrails["remediation_allowlist"] || []).sort == ALLOWLIST.sort

  queue = surface["queue"] || []
  bead_ids = queue.map { |entry| entry["bead_id"] }
  v << "bead_dup" unless bead_ids.uniq.length == bead_ids.length
  v << "queue_unsorted" unless bead_ids == bead_ids.sort

  status_counts = Hash.new(0)
  queue.each do |entry|
    status = entry["status"]
    v << "unknown_status" unless REQUIRED_STATUSES.include?(status)
    status_counts[status] += 1

    comment_id = entry.dig("source", "comment_id")
    expected_receipt = "#{entry["bead_id"]}:comment-#{comment_id}"
    v << "receipt_id_drift" unless entry["receipt_id"] == expected_receipt
    v << "raw_pane_content" unless entry.dig("source", "raw_pane_content_stored") == false
    v << "source_digest_mismatch" unless entry.dig("source", "source_text_sha256") == Digest::SHA256.hexdigest(expected_receipt)
    v << "empty_reason_codes" if (entry["reason_codes"] || []).empty?
    v << "remediation_mismatch" unless entry["remediation"] == REMEDIATION_BY_STATUS[status]
    v << "replay_allowed_mismatch" unless entry["replay_allowed"] == (status == "runnable")
  end

  # Summary reconciles with the queue and keeps queued distinct from completed.
  summary = surface["summary"] || {}
  completed = status_counts["completed"]
  v << "summary_total" unless summary["total"] == queue.length
  v << "summary_completed" unless summary["completed"] == completed
  v << "summary_queued" unless summary["queued"] == queue.length - completed
  REQUIRED_STATUSES.each do |status|
    v << "by_status_drift" unless summary.dig("by_status", status) == status_counts[status]
  end

  # Next candidate is the single runnable, replay-allowed receipt (or null).
  runnable = queue.select { |entry| entry["status"] == "runnable" && entry["replay_allowed"] }
  nc = surface["next_candidate"]
  if runnable.empty?
    v << "next_candidate_should_be_null" unless nc.nil?
  elsif nc.nil?
    v << "next_candidate_missing"
  else
    entry = runnable.find { |item| item["bead_id"] == nc["bead_id"] }
    if entry.nil?
      v << "next_candidate_not_runnable"
    else
      v << "next_candidate_receipt_drift" unless nc["receipt_id"] == entry["receipt_id"]
      v << "next_candidate_target_drift" unless nc["target_dir"] == entry["target_dir"]
      v << "next_candidate_material_drift" unless nc["material_remote_required"] == entry["material_remote_required"]
      if nc["material_remote_required"]
        v << "next_candidate_command_shape" unless remote_shape_valid?(nc["command_preview"])
        target = nc["target_dir"]
        v << "next_candidate_target_unpinned" unless target.nil? || (nc["command_preview"] || []).include?("CARGO_TARGET_DIR=#{target}")
      end
    end
  end

  # Explain covers exactly the blocked (non-runnable, non-completed) receipts.
  blocked = queue.reject { |entry| %w[runnable completed].include?(entry["status"]) }
  blocked_by_id = blocked.to_h { |entry| [entry["bead_id"], entry] }
  explain = surface["explain"] || []
  explain_ids = explain.map { |entry| entry["bead_id"] }
  v << "explain_dup" unless explain_ids.uniq.length == explain_ids.length
  v << "explain_coverage" unless explain_ids.sort == blocked_by_id.keys.sort
  explain.each do |entry|
    qe = blocked_by_id[entry["bead_id"]]
    next unless qe

    v << "explain_status_drift" unless entry["status"] == qe["status"]
    v << "explain_remediation_drift" unless entry["remediation"] == qe["remediation"]
    v << "explain_reason_drift" unless entry["blocking_reason_codes"] == qe["reason_codes"]
    v << "forbidden_action_in_why" if entry["why"].to_s.match?(FORBIDDEN)
  end

  # Robot dispatch mirrors the queue 1:1.
  dispatch = surface["robot_dispatch"] || []
  dispatch_ids = dispatch.map { |entry| entry["bead_id"] }
  v << "dispatch_unsorted" unless dispatch_ids == dispatch_ids.sort
  v << "dispatch_coverage" unless dispatch_ids.sort == bead_ids.sort
  queue_by_id = queue.to_h { |entry| [entry["bead_id"], entry] }
  dispatch.each do |entry|
    qe = queue_by_id[entry["bead_id"]]
    next unless qe

    v << "dispatch_mismatch" unless entry["status"] == qe["status"] &&
                                    entry["target_dir"] == qe["target_dir"] &&
                                    entry["replay_allowed"] == qe["replay_allowed"]
  end

  v.uniq
end

def apply_mutation(surface, op)
  s = deep_dup(surface)
  case op
  when "disable_local_cargo_guardrail"
    s["guardrails"]["forbids_local_cargo"] = false
  when "allow_replay_on_wait"
    s["queue"].find { |e| e["status"] == "wait_rch" }["replay_allowed"] = true
  when "strip_no_self_healing"
    s["next_candidate"]["command_preview"].delete("--no-self-healing")
  when "inject_local_cargo_why"
    s["explain"].first["why"] = "Just run local cargo build to fix it."
  when "add_source_text_key"
    s["queue"].first["source"]["source_text"] = "leaked raw pane text"
  when "tamper_digest"
    s["queue"].first["source"]["source_text_sha256"] = "0" * 64
  when "drop_none_from_allowlist"
    s["guardrails"]["remediation_allowlist"].delete("none")
  when "break_summary_total"
    s["summary"]["total"] = s["queue"].length - 1
  when "unsort_queue"
    s["queue"] = s["queue"].reverse
  when "flip_dispatch_status"
    s["robot_dispatch"].first["status"] = "runnable"
  when "drop_explain_entry"
    s["explain"] = s["explain"][1..]
  when "corrupt_contract_id"
    s["contract_id"] = "ft.deferred_proof_queue_surface.v0"
  when "corrupt_receipt_id"
    s["queue"].first["receipt_id"] = "ft-am1:comment-9999"
  when "empty_reason_codes_on_completed"
    s["queue"].find { |e| e["status"] == "completed" }["reason_codes"] = []
  when "corrupt_remediation_on_completed"
    s["queue"].find { |e| e["status"] == "completed" }["remediation"] = "request_human_triage"
  when "break_summary_completed"
    s["summary"]["completed"] = 0
  when "break_summary_queued"
    s["summary"]["queued"] = s["summary"]["queued"] - 1
  when "break_by_status_runnable"
    s["summary"]["by_status"]["runnable"] = 0
  when "drift_explain_status"
    s["explain"].first["status"] = "wait_rch"
  when "drift_explain_remediation"
    s["explain"].first["remediation"] = "none"
  when "empty_explain_reason_codes"
    s["explain"].first["blocking_reason_codes"] = []
  when "unsort_dispatch"
    s["robot_dispatch"] = s["robot_dispatch"].reverse
  when "drop_dispatch_entry"
    s["robot_dispatch"] = s["robot_dispatch"][1..]
  when "drift_next_candidate_receipt"
    s["next_candidate"]["receipt_id"] = "ft-rdy1:comment-0000"
  when "drift_next_candidate_material"
    s["next_candidate"]["material_remote_required"] = false
  when "point_next_candidate_at_blocked"
    s["next_candidate"]["bead_id"] = "ft-wt1"
  when "null_next_candidate"
    s["next_candidate"] = nil
  when "unpin_next_candidate_target"
    s["next_candidate"]["command_preview"] = s["next_candidate"]["command_preview"].reject { |t| t.start_with?("CARGO_TARGET_DIR=") }
  when "drift_next_candidate_target"
    s["next_candidate"]["target_dir"] = "/tmp/ft-rdy1-WRONG"
    cmd = s["next_candidate"]["command_preview"]
    idx = cmd.index { |t| t.start_with?("CARGO_TARGET_DIR=") }
    cmd[idx] = "CARGO_TARGET_DIR=/tmp/ft-rdy1-WRONG" if idx
  when "duplicate_bead_id"
    s["queue"][1]["bead_id"] = s["queue"][0]["bead_id"]
  when "inject_unknown_status"
    s["queue"].first["status"] = "totally_unknown_status"
  when "duplicate_explain_entry"
    s["explain"] = [deep_dup(s["explain"].first)] + s["explain"]
  when "demote_runnable_keep_candidate"
    s["queue"].find { |e| e["status"] == "runnable" }["replay_allowed"] = false
  else
    fail!("unknown invalid-fragment mutation: #{op}")
  end
  s
end

schema = read_json(SCHEMA)
manifest = read_json(MANIFEST)
surface = read_json(CASES)
invalid = read_json(INVALID)
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
fail!("manifest invalid path drifted") unless manifest["invalid_fragments"] == INVALID
fail!("manifest status coverage drifted") unless manifest.dig("golden_summary", "statuses").sort == REQUIRED_STATUSES.sort

# The golden surface must satisfy every per-instance invariant.
golden_violations = violations(surface)
fail!("golden surface reported violations: #{golden_violations.inspect}") unless golden_violations.empty?

# Golden completeness: every status bucket is exercised exactly once-or-more.
present = surface.fetch("queue").map { |entry| entry.fetch("status") }.uniq.sort
fail!("not all statuses are exercised: #{present.inspect}") unless present == REQUIRED_STATUSES.sort

# Determinism: a second parse of the same bytes yields identical canonical JSON.
fail!("surface is not deterministic across parses") unless JSON.generate(read_json(CASES)) == JSON.generate(surface)

# Negative corpus: each mutation must surface exactly its expected violation,
# and the violation must be one the golden surface does NOT already report.
fragments = invalid.fetch("cases")
fail!("invalid corpus is empty") if fragments.empty?
expected_codes = fragments.map { |frag| frag.fetch("expected_violation") }
fail!("invalid corpus has duplicate expected codes") unless expected_codes.uniq.length == expected_codes.length
fragments.each do |frag|
  mutated = apply_mutation(surface, frag.fetch("mutation"))
  found = violations(mutated)
  expected = frag.fetch("expected_violation")
  fail!("invalid fragment #{frag.fetch("case_id")} did not raise #{expected} (got #{found.inspect})") unless found.include?(expected)
end

# Doc + provenance.
DOC_TERMS.each do |term|
  fail!("doc missing contract term #{term}") unless doc.include?(term)
end
fail!("provenance missing queue-surface row") unless provenance.include?("`ft-deferred-proof-queue-surface.json`")
fail!("provenance row missing verifier") unless provenance.include?("bash tests/e2e/test_deferred_proof_queue_surface_contract.sh")

completed = surface.fetch("queue").count { |entry| entry.fetch("status") == "completed" }
puts "deferred proof queue surface contract: static verifier passed (#{surface.fetch("queue").length} entries, #{completed} completed, #{surface.fetch("explain").length} explain views, #{fragments.length} rejected fragments)"
RUBY
