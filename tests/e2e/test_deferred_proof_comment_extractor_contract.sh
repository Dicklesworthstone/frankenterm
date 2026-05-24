#!/usr/bin/env bash
# Static verifier for the deferred proof comment extractor fixtures.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

SCHEMA="docs/json-schema/ft-deferred-proof-comment-extraction.json"
DOC="docs/robot-contracts/deferred-proof-comment-extractor.md"
MANIFEST="fixtures/deferred-proof-replay/extractor/manifest.json"
CASES="fixtures/deferred-proof-replay/extractor/valid/cases.v1.json"
EXPECTED="fixtures/deferred-proof-replay/extractor/expected/records.v1.jsonl"
INVALID="fixtures/deferred-proof-replay/extractor/invalid/fragments.v1.json"
PROVENANCE="docs/json-schema/PROVENANCE.md"
RECEIPT_SCHEMA="docs/json-schema/ft-deferred-proof-receipt.json"

fail() {
  printf 'deferred proof comment extractor contract: %s\n' "$*" >&2
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
require_file "${EXPECTED}"
require_file "${INVALID}"
require_file "${PROVENANCE}"
require_file "${RECEIPT_SCHEMA}"

jq empty "${SCHEMA}" "${MANIFEST}" "${CASES}" "${INVALID}" "${RECEIPT_SCHEMA}"
jq -c empty "${EXPECTED}" >/dev/null

ruby <<'RUBY'
require "digest"
require "json"
require "set"
require "shellwords"

SCHEMA = "docs/json-schema/ft-deferred-proof-comment-extraction.json"
DOC = "docs/robot-contracts/deferred-proof-comment-extractor.md"
MANIFEST = "fixtures/deferred-proof-replay/extractor/manifest.json"
CASES = "fixtures/deferred-proof-replay/extractor/valid/cases.v1.json"
EXPECTED = "fixtures/deferred-proof-replay/extractor/expected/records.v1.jsonl"
INVALID = "fixtures/deferred-proof-replay/extractor/invalid/fragments.v1.json"
PROVENANCE = "docs/json-schema/PROVENANCE.md"
RECEIPT_SCHEMA = "docs/json-schema/ft-deferred-proof-receipt.json"
CONTRACT_ID = "ft.deferred_proof_comment_extraction.v1"
RECEIPT_CONTRACT_ID = "ft.deferred_proof_receipt.v1"
REQUIRED_CASES = %w[
  remote-rch-blocked-closeout
  static-only-closeout
  mixed-static-rch-closeout
  ambiguous-prose-ineligible
  stale-command-ineligible
  duplicate-comment-ineligible
  operator-cancelled-ineligible
  dirty-overlap-ineligible
  code-failure-ineligible
  missing-owned-paths-ineligible
  local-fallback-evidence-ineligible
].freeze
REQUIRED_STATES = %w[
  duplicate
  ineligible
  receipt_emitted
].freeze
REQUIRED_REASONS = %w[
  ambiguous_comment
  code_test_failure
  dirty_overlap
  duplicate_comment
  local_fallback_evidence
  missing_owned_paths
  operator_cancelled
  receipt_emitted
  stale_command_shape
  static_clean_remote_pending
].freeze
EXPECTED_INVALID = %w[
  missing-source-text
  raw-pane-text-stored
  unknown-expected-state
].freeze

def fail!(message)
  warn "deferred proof comment extractor contract: #{message}"
  exit 1
end

def read_json(path)
  JSON.parse(File.read(path))
rescue JSON::ParserError => error
  fail!("#{path} does not parse as JSON: #{error.message}")
end

def read_jsonl(path)
  File.readlines(path, chomp: true).reject(&:empty?).map do |line|
    JSON.parse(line)
  end
rescue JSON::ParserError => error
  fail!("#{path} does not parse as JSONL: #{error.message}")
end

def split_list(value)
  value.to_s.split(",").map(&:strip).reject(&:empty?)
end

def footer_fields(text)
  fields = {}
  text.each_line do |line|
    key, value = line.split(":", 2)
    next unless value

    normalized = key.strip.downcase.gsub(/[^a-z0-9]+/, "_").gsub(/\A_|_\z/, "")
    fields[normalized] = value.strip
  end
  fields
end

def command_parts(command_text)
  tokens = Shellwords.split(command_text.to_s)
  env = []
  while tokens.first&.match?(/\A[A-Z][A-Z0-9_]*=/)
    name, value = tokens.shift.split("=", 2)
    env << { "name" => name, "value" => value }
  end

  [env, tokens]
rescue ArgumentError
  [[], []]
end

def remote_shape_valid?(argv)
  return false unless argv.first == "rch"

  exec_index = argv.index("exec")
  return false unless exec_index && argv[exec_index + 1] == "--"

  argv[1...exec_index].include?("--no-self-healing")
end

def proof_kind(fields)
  fields.fetch("proof_kind", "")
end

def material_cargo?(fields)
  %w[cargo_check cargo_test cargo_clippy].include?(proof_kind(fields))
end

def expected_kind(fields)
  kind = proof_kind(fields)
  return "static_verifier" if kind.empty?

  kind
end

def package_value(fields)
  value = fields.fetch("package", "")
  value.empty? ? nil : value
end

def nullable_value(fields, key)
  value = fields.fetch(key, "")
  value.empty? ? nil : value
end

def feature_set(fields)
  split_list(fields.fetch("feature_set", "")).reject { |entry| entry == "default" }
end

def rch_state(fields)
  admission = fields.fetch("rch_admission", "unknown")
  return "not_required" if admission == "not_required"
  return "blocked_worker_pressure" if admission.include?("critical_pressure") || admission.include?("no_admissible_workers")
  return "admitted" if admission == "admitted"

  "unknown"
end

def eligibility_for(fields)
  if material_cargo?(fields)
    if rch_state(fields) == "blocked_worker_pressure"
      ["wait_rch", ["rch.worker_pressure"], false, "blocked_infra"]
    else
      ["eligible", ["rch.admitted"], true, "remote_required_pending"]
    end
  else
    ["eligible", ["static.clean"], true, "static_only_clean"]
  end
end

def source_record(source)
  text = source.fetch("source_text")
  {
    "kind" => source.fetch("kind"),
    "comment_id" => source.fetch("comment_id"),
    "author" => source.fetch("author"),
    "created_at" => source.fetch("created_at"),
    "source_text_sha256" => Digest::SHA256.hexdigest(text),
    "raw_pane_content_stored" => source.fetch("raw_pane_content_stored")
  }
end

def build_receipt(case_entry, fields, env, argv)
  state, eligibility_reasons, replay_allowed, evidence_classification = eligibility_for(fields)
  material = material_cargo?(fields)
  shape = material ? "rch-no-self-healing-v1" : "static-verifier-v1"
  target_dir = nullable_value(fields, "target_dir")
  env_names = env.map { |item| item.fetch("name") }

  {
    "contract_id" => RECEIPT_CONTRACT_ID,
    "receipt_id" => "#{case_entry.fetch("bead_id")}:comment-#{case_entry.fetch("source_comment").fetch("comment_id")}",
    "bead_id" => case_entry.fetch("bead_id"),
    "source_state" => fields.fetch("proof_state", "planning_only"),
    "command" => {
      "command_shape_version" => shape,
      "argv" => argv,
      "env" => env,
      "env_allowlist" => env_names,
      "target_dir" => target_dir,
      "material_remote_required" => material
    },
    "proof" => {
      "expected_kind" => expected_kind(fields),
      "package" => package_value(fields),
      "test_filter" => nullable_value(fields, "test_filter"),
      "feature_set" => feature_set(fields),
      "material_cargo_required" => material,
      "evidence_classification" => evidence_classification
    },
    "paths" => {
      "owned_paths" => split_list(fields.fetch("owned_paths", "")),
      "dirty_paths_at_capture" => split_list(fields.fetch("dirty_paths", ""))
    },
    "coordination" => {
      "prerequisite_beads" => split_list(fields.fetch("prerequisite_beads", "")),
      "agent_mail_state" => fields.fetch("agent_mail", "unknown"),
      "rch_admission_state" => rch_state(fields)
    },
    "eligibility" => {
      "state" => state,
      "reason_codes" => eligibility_reasons,
      "replay_allowed" => replay_allowed
    }
  }
end

def stale_command?(fields, env, argv)
  material_cargo?(fields) &&
    (env.to_h { |item| [item.fetch("name"), item.fetch("value")] }["RCH_NO_SELF_HEALING"] != "1" ||
     env.to_h { |item| [item.fetch("name"), item.fetch("value")] }["RCH_REQUIRE_REMOTE"] != "1" ||
     !remote_shape_valid?(argv))
end

def extraction_record(case_entry, seen)
  source = case_entry.fetch("source_comment")
  source_info = source_record(source)
  dedupe_key = [case_entry.fetch("bead_id"), source_info.fetch("source_text_sha256")]
  if seen.include?(dedupe_key)
    return {
      "schema_version" => 1,
      "contract_id" => CONTRACT_ID,
      "record_id" => case_entry.fetch("case_id"),
      "bead_id" => case_entry.fetch("bead_id"),
      "source" => source_info,
      "extraction" => {
        "state" => "duplicate",
        "reason_codes" => ["duplicate_comment"],
        "provenance_preserved" => true
      },
      "receipt" => nil
    }
  end
  seen.add(dedupe_key)

  fields = footer_fields(source.fetch("source_text"))
  command_text = fields.fetch("command", "")
  env, argv = command_parts(command_text)
  owned_paths = split_list(fields.fetch("owned_paths", ""))
  dirty_paths = split_list(fields.fetch("dirty_paths", ""))
  blocker = fields.fetch("blocker", "").downcase
  proof_state = fields.fetch("proof_state", "").downcase
  reasons = []
  reasons << "ambiguous_comment" if command_text.empty?
  # Operator explicitly cancelled this replay: never auto-queue it, even when
  # RCH is otherwise blocked. Distinct from infra deferral.
  reasons << "operator_cancelled" if blocker == "operator_cancelled"
  # Genuine code/test failure (a remote worker reached Cargo and the proof went
  # red) is NOT a deferred-replayable receipt — it is a real failing result.
  # Kept distinct from RCH admission / worker-pressure infra blocks.
  reasons << "code_test_failure" if %w[code_failure test_failure].include?(blocker) ||
                                     %w[failing red failed].include?(proof_state)
  # Dirty-tree overlap: the captured tree carried dirty paths outside the owned
  # set, so replaying would bundle unrelated work. Resolve before queueing.
  reasons << "dirty_overlap" if dirty_paths.any? { |path| !owned_paths.include?(path) }
  reasons << "stale_command_shape" if stale_command?(fields, env, argv)
  reasons << "missing_owned_paths" if command_text != "" && owned_paths.empty?
  reasons << "local_fallback_evidence" if source.fetch("source_text").match?(/\[RCH\] local|running locally|local fallback/i)

  if reasons.any?
    return {
      "schema_version" => 1,
      "contract_id" => CONTRACT_ID,
      "record_id" => case_entry.fetch("case_id"),
      "bead_id" => case_entry.fetch("bead_id"),
      "source" => source_info,
      "extraction" => {
        "state" => "ineligible",
        "reason_codes" => reasons.uniq,
        "provenance_preserved" => true
      },
      "receipt" => nil
    }
  end

  receipt = build_receipt(case_entry, fields, env, argv)
  reason_codes = ["receipt_emitted"]
  reason_codes << "static_clean_remote_pending" if fields["static_checks"] && receipt.dig("eligibility", "state") == "wait_rch"

  {
    "schema_version" => 1,
    "contract_id" => CONTRACT_ID,
    "record_id" => case_entry.fetch("case_id"),
    "bead_id" => case_entry.fetch("bead_id"),
    "source" => source_info,
    "extraction" => {
      "state" => "receipt_emitted",
      "reason_codes" => reason_codes,
      "provenance_preserved" => true
    },
    "receipt" => receipt
  }
end

def fixture_rejection(fragment)
  item = fragment.fetch("case")
  return "missing_source_text" unless item.dig("source_comment", "source_text")
  return "raw_pane_text_stored" if item.dig("source_comment", "raw_pane_content_stored") != false
  return "unknown_expected_state" unless REQUIRED_STATES.include?(item.fetch("expected_state", nil))

  nil
end

schema = read_json(SCHEMA)
receipt_schema = read_json(RECEIPT_SCHEMA)
manifest = read_json(MANIFEST)
cases = read_json(CASES)
invalid = read_json(INVALID)
expected = read_jsonl(EXPECTED)
doc = File.read(DOC)
provenance = File.read(PROVENANCE)

fail!("schema id drifted") unless schema["$id"]&.end_with?("/ft-deferred-proof-comment-extraction.json")
fail!("schema contract const drifted") unless schema.dig("properties", "contract_id", "const") == CONTRACT_ID
fail!("schema source digest pattern missing") unless schema.dig("$defs", "source", "properties", "source_text_sha256", "pattern") == "^[0-9a-f]{64}$"
fail!("schema permits raw pane content") unless schema.dig("$defs", "source", "properties", "raw_pane_content_stored", "const") == false
fail!("schema receipt link drifted") unless schema.dig("$defs", "receipt_projection", "properties", "contract_id", "const") == RECEIPT_CONTRACT_ID
fail!("receipt schema missing expected contract") unless receipt_schema.dig("properties", "contract_id", "const") == RECEIPT_CONTRACT_ID

fail!("manifest contract drifted") unless manifest["contract_id"] == CONTRACT_ID
fail!("manifest schema path drifted") unless manifest["schema_path"] == SCHEMA
fail!("manifest cases path drifted") unless manifest["source_cases"] == CASES
fail!("manifest expected jsonl path drifted") unless manifest["expected_jsonl"] == EXPECTED
fail!("manifest invalid path drifted") unless manifest["invalid_fragments"] == INVALID
fail!("manifest case count drifted") unless manifest.dig("golden_summary", "source_case_count") == REQUIRED_CASES.length
fail!("manifest output count drifted") unless manifest.dig("golden_summary", "output_record_count") == REQUIRED_CASES.length
fail!("manifest reason codes drifted") unless manifest.dig("golden_summary", "reason_codes").sort == REQUIRED_REASONS.sort

case_entries = cases.fetch("cases")
case_ids = case_entries.map { |entry| entry.fetch("case_id") }
fail!("source case coverage drifted: #{case_ids.sort.inspect}") unless case_ids.sort == REQUIRED_CASES.sort
fail!("source case ids are not unique") unless case_ids.uniq.length == case_ids.length

seen = Set.new
actual = case_entries.map { |entry| extraction_record(entry, seen) }
expected_lines = expected.map { |entry| JSON.generate(entry) }
actual_lines = actual.map { |entry| JSON.generate(entry) }
fail!("generated JSONL records drifted\nexpected:\n#{expected_lines.join("\n")}\nactual:\n#{actual_lines.join("\n")}") unless actual_lines == expected_lines
fail!("second generation is not deterministic") unless actual.map { |entry| JSON.generate(entry) } == actual_lines

actual_by_id = actual.to_h { |entry| [entry.fetch("record_id"), entry] }
case_entries.each do |entry|
  record = actual_by_id.fetch(entry.fetch("case_id"))
  fail!("#{entry.fetch("case_id")} state drifted") unless record.dig("extraction", "state") == entry.fetch("expected_state")
  fail!("#{entry.fetch("case_id")} reasons drifted") unless record.dig("extraction", "reason_codes") == entry.fetch("expected_reason_codes")
  fail!("#{entry.fetch("case_id")} did not preserve provenance") unless record.dig("extraction", "provenance_preserved") == true
  fail!("#{entry.fetch("case_id")} stored raw pane content") unless record.dig("source", "raw_pane_content_stored") == false
end

emitted = actual.select { |entry| entry.dig("extraction", "state") == "receipt_emitted" }
fail!("emitted count drifted") unless emitted.length == manifest.dig("golden_summary", "receipt_emitted_count")
emitted.each do |entry|
  receipt = entry.fetch("receipt")
  fail!("#{entry.fetch("record_id")} missing receipt") unless receipt
  fail!("#{entry.fetch("record_id")} receipt contract drifted") unless receipt.fetch("contract_id") == RECEIPT_CONTRACT_ID
  if receipt.dig("proof", "material_cargo_required")
    fail!("#{entry.fetch("record_id")} material receipt missing remote command") unless remote_shape_valid?(receipt.dig("command", "argv"))
    fail!("#{entry.fetch("record_id")} material receipt unexpectedly replayable under blocked RCH") if receipt.dig("coordination", "rch_admission_state") == "blocked_worker_pressure" && receipt.dig("eligibility", "replay_allowed")
  end
end

invalid_ids = invalid.fetch("cases").map { |entry| entry.fetch("case_id") }
fail!("invalid coverage drifted: #{invalid_ids.sort.inspect}") unless invalid_ids.sort == EXPECTED_INVALID.sort
invalid.fetch("cases").each do |entry|
  rejection = fixture_rejection(entry)
  fail!("invalid fragment #{entry.fetch("case_id")} rejected as #{rejection.inspect}, expected #{entry.fetch("expected_rejection")}") unless rejection == entry.fetch("expected_rejection")
end

[
  "ft.deferred_proof_comment_extraction.v1",
  "ft.deferred_proof_receipt.v1",
  "source_text_sha256",
  "RCH_REQUIRE_REMOTE=1",
  "RCH_NO_SELF_HEALING=1",
  "duplicate",
  "ambiguous",
  "fixtures/deferred-proof-replay/extractor/"
].each do |term|
  fail!("doc missing contract term #{term}") unless doc.include?(term)
end

fail!("provenance missing extractor row") unless provenance.include?("`ft-deferred-proof-comment-extraction.json`")
fail!("provenance row missing verifier") unless provenance.include?("bash tests/e2e/test_deferred_proof_comment_extractor_contract.sh")

puts "deferred proof comment extractor contract: static verifier passed (#{actual.length} records, #{emitted.length} receipts)"
RUBY
