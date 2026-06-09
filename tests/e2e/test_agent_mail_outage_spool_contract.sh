#!/usr/bin/env bash
# Static verifier for the Agent Mail outage-spool entry contract.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

SCHEMA="docs/json-schema/ft-agent-mail-outbox-entry.json"
MANIFEST="fixtures/agent-mail-outage-spool/manifest.json"
EXPECTED_LOG="fixtures/agent-mail-outage-spool/expected/verify.v1.jsonl"
PROVENANCE="docs/json-schema/PROVENANCE.md"
RUST_CONTRACT="crates/frankenterm-core/src/agent_mail_outbox.rs"

fail() {
  printf 'agent mail outage spool contract: %s\n' "$*" >&2
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
require_file "${MANIFEST}"
require_file "${EXPECTED_LOG}"
require_file "${PROVENANCE}"
require_file "${RUST_CONTRACT}"

jq empty "${SCHEMA}" "${MANIFEST}" fixtures/agent-mail-outage-spool/valid/*.json
jq -c empty "${EXPECTED_LOG}"

ruby <<'RUBY'
require "json"
require "set"
require "time"

SCHEMA = "docs/json-schema/ft-agent-mail-outbox-entry.json"
MANIFEST = "fixtures/agent-mail-outage-spool/manifest.json"
EXPECTED_LOG = "fixtures/agent-mail-outage-spool/expected/verify.v1.jsonl"
PROVENANCE = "docs/json-schema/PROVENANCE.md"
RUST_CONTRACT = "crates/frankenterm-core/src/agent_mail_outbox.rs"
EXPECTED_FIXTURE_IDS = %w[
  agent-mail-unavailable
  send-timeout
  contact-blocked
  ack-required-message
  reservation-intent
  reservation-conflict
  beads-fallback-closeout
  stale-owner-handoff
  replayed-send
  writer-adapter-queued-send
].freeze
EXPECTED_STATES = %w[
  queued
  replay_dry_run_ok
  replayed
  replay_failed
  superseded
  discarded_by_operator
].freeze
EXPECTED_SOURCE_OPERATIONS = %w[
  send_message
  reply_message
  file_reservation
  release_reservation
  beads_status_comment
  beads_closeout_comment
  stale_owner_handoff_notice
  coordination_notice
].freeze
EXPECTED_FAILURE_CLASSES = %w[
  agent_mail_unavailable
  database_recovery_notice
  api_unreachable
  api_error
  reservation_conflict
  registration_failed
  contact_permission_blocked
  ack_unavailable
  timeout
  unknown
].freeze
SECRET_MARKERS = %w[
  sk-
  ghp_
  github_pat_
  xoxb-
  akia
  -----begin
  password=
  token=
  secret=
].freeze
RAW_PANE_MARKERS = [
  "pane_text",
  "raw pane",
  "terminal scrollback",
  "ft robot get-text",
  "\e["
].freeze

def fail!(message)
  warn "agent mail outage spool contract: #{message}"
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
  rescue JSON::ParserError => error
    fail!("#{path} contains invalid JSONL: #{error.message}")
  end
end

def safe_repo_relative_path?(path)
  return false unless path.is_a?(String)
  return false if path.empty?
  return false if path.start_with?("/", "~")
  return false if path.include?("\\") || path.include?("://")

  segments = path.split("/", -1)
  return false if segments.any?(&:empty?)
  return false if segments.any? { |segment| segment == "." || segment == ".." || segment == ".git" }

  true
end

def utc_timestamp?(value)
  value.is_a?(String) && value.end_with?("Z") && Time.iso8601(value).utc?
rescue ArgumentError
  false
end

def sha256?(value)
  value.is_a?(String) && value.match?(/\A[A-Fa-f0-9]{64}\z/)
end

def secret_like?(value)
  downcased = value.to_s.downcase
  SECRET_MARKERS.any? { |marker| downcased.include?(marker) }
end

def raw_pane_like?(value)
  downcased = value.to_s.downcase
  RAW_PANE_MARKERS.any? { |marker| downcased.include?(marker) || value.to_s.include?(marker) }
end

schema = read_json(SCHEMA)
manifest = read_json(MANIFEST)
expected_log = read_jsonl(EXPECTED_LOG)
provenance = File.read(PROVENANCE)
rust_contract = File.read(RUST_CONTRACT)

fail!("schema id drifted") unless schema["$id"]&.end_with?("/ft-agent-mail-outbox-entry.json")
fail!("contract id const missing") unless schema.dig("properties", "contract_id", "const") == "ft.agent_mail_outbox_entry.v1"
fail!("source bead const missing") unless schema.dig("properties", "source_bead", "const") == "ft-dezx8.1"
fail!("state enum drifted") unless schema.dig("$defs", "state", "enum").sort == EXPECTED_STATES.sort
fail!("source operation enum drifted") unless schema.dig("$defs", "source_operation", "enum").sort == EXPECTED_SOURCE_OPERATIONS.sort
fail!("failure class enum drifted") unless schema.dig("$defs", "failure_class", "enum").sort == EXPECTED_FAILURE_CLASSES.sort
fail!("raw body retention must be const false") unless schema.dig("$defs", "body_policy", "properties", "raw_body_retained", "const") == false
fail!("automatic discard must be const false") unless schema.dig("$defs", "retention", "properties", "automatic_discard", "const") == false

fail!("manifest schema_version drifted") unless manifest["schema_version"] == 1
fail!("manifest contract id drifted") unless manifest["contract_id"] == "ft.agent_mail_outage_spool.fixture_manifest.v1"
fail!("manifest bead drifted") unless manifest["bead"] == "ft-dezx8.1"
fail!("manifest schema pointer drifted") unless manifest["schema"] == SCHEMA
fail!("manifest rust contract pointer drifted") unless manifest["rust_contract"] == RUST_CONTRACT
fail!("manifest expected log pointer drifted") unless manifest["expected_verifier_log"] == EXPECTED_LOG
fail!("manifest static verifier missing") unless manifest.fetch("verification").include?("bash tests/e2e/test_agent_mail_outage_spool_contract.sh")
fail!("manifest RCH-only cargo check proof missing") unless manifest.fetch("verification").any? do |line|
  line.include?("RCH_REQUIRE_REMOTE=1") &&
    line.include?("cargo check --locked -p frankenterm-core --lib --no-default-features")
end
fail!("manifest RCH-only unit proof missing") unless manifest.fetch("verification").any? do |line|
  line.include?("RCH_REQUIRE_REMOTE=1") &&
    line.include?("cargo test --locked -p frankenterm-core --lib") &&
    line.include?("--no-default-features") &&
    line.include?("agent_mail_outbox")
end
fail!("manifest RCH-only MCP surface proof missing") unless manifest.fetch("verification").any? do |line|
  line.include?("RCH_REQUIRE_REMOTE=1") &&
    line.include?("cargo test --locked -p frankenterm-core --lib") &&
    line.include?("--no-default-features") &&
    line.include?("--features mcp") &&
    line.match?(/\sresource\s--\s--nocapture\z/)
end
fail!("manifest RCH-only Robot surface proof missing") unless manifest.fetch("verification").any? do |line|
  line.include?("RCH_REQUIRE_REMOTE=1") &&
    line.include?("cargo test --locked -p frankenterm --bin ft") &&
    line.include?("agent_mail_outbox")
end

fixture_paths = manifest.fetch("valid")
fixture_ids = fixture_paths.map { |path| File.basename(path, ".json") }
fail!("fixture ids drifted: #{fixture_ids.sort.inspect}") unless fixture_ids.sort == EXPECTED_FIXTURE_IDS.sort
fixture_paths.each { |path| fail!("missing fixture #{path}") unless File.file?(path) }

states = Set.new
operations = Set.new
failure_classes = Set.new
surface_counts = Hash.new(0)
surface_failure_counts = Hash.new(0)
surface_operation_counts = Hash.new(0)
actual_log = []

fixture_paths.each do |path|
  fixture_id = File.basename(path, ".json")
  payload = read_json(path)

  fail!("#{fixture_id} schema_version drifted") unless payload["schema_version"] == 1
  fail!("#{fixture_id} contract id drifted") unless payload["contract_id"] == "ft.agent_mail_outbox_entry.v1"
  fail!("#{fixture_id} source bead drifted") unless payload["source_bead"] == "ft-dezx8.1"
  fail!("#{fixture_id} replay_id format drifted") unless payload["replay_id"].match?(/\Aamq1-[a-f0-9]{24}\z/)
  fail!("#{fixture_id} ambiguous created_at") unless utc_timestamp?(payload["created_at"])
  fail!("#{fixture_id} subject missing") if payload["subject"].to_s.empty?
  fail!("#{fixture_id} subject too long") if payload["subject"].bytesize > 200
  fail!("#{fixture_id} subject contains sensitive/raw marker") if secret_like?(payload["subject"]) || raw_pane_like?(payload["subject"])

  agent = payload.fetch("agent")
  %w[name program model project_key].each do |field|
    fail!("#{fixture_id} missing agent #{field}") if agent[field].to_s.empty?
  end

  recipients = payload.fetch("recipients")
  recipient_count = recipients.fetch("to").length + recipients.fetch("cc").length + recipients.fetch("bcc").length
  fail!("#{fixture_id} has no recipients") if recipient_count.zero?
  recipients.each_value do |names|
    names.each do |name|
      fail!("#{fixture_id} invalid recipient #{name.inspect}") if name.empty? || name.include?("/") || name.include?("\\") || name.include?("@")
    end
  end

  body_policy = payload.fetch("body_policy")
  fail!("#{fixture_id} invalid body digest") unless sha256?(body_policy["body_sha256"])
  fail!("#{fixture_id} raw body retained") unless body_policy["raw_body_retained"] == false
  fail!("#{fixture_id} unbounded body") if body_policy["body_byte_count"] <= 0 || body_policy["body_byte_count"] > body_policy["max_retained_bytes"] || body_policy["max_retained_bytes"] > 16_384
  preview = body_policy["body_preview_redacted"]
  fail!("#{fixture_id} preview missing") if body_policy["body_storage"] == "bounded_markdown" && preview.to_s.empty?
  if preview
    fail!("#{fixture_id} preview too large") if preview.bytesize > 512
    fail!("#{fixture_id} preview contains secret-like content") if secret_like?(preview)
    fail!("#{fixture_id} preview contains raw pane text") if raw_pane_like?(preview)
  end

  payload.fetch("attachments").each do |attachment|
    fail!("#{fixture_id} unsafe attachment path #{attachment["path"].inspect}") unless safe_repo_relative_path?(attachment["path"])
    fail!("#{fixture_id} invalid attachment digest") if attachment["sha256"] && !sha256?(attachment["sha256"])
    fail!("#{fixture_id} oversized attachment ref") if attachment["byte_count"] > 64 * 1024 * 1024
  end

  failure = payload.fetch("failure_reason")
  fail!("#{fixture_id} unknown failure class") unless EXPECTED_FAILURE_CLASSES.include?(failure["class"])
  fail!("#{fixture_id} retry budget exceeded") if failure["retry_count"] > failure["retry_limit"]
  fail!("#{fixture_id} ambiguous failure timestamp") unless utc_timestamp?(failure["last_attempt_at"])
  fail!("#{fixture_id} failure summary leaks content") if secret_like?(failure["summary"]) || raw_pane_like?(failure["summary"])

  state = payload.fetch("state")
  receipt = payload["replay_receipt"]
  decision = payload["operator_decision"]
  case state
  when "queued"
    fail!("#{fixture_id} queued entry has replay receipt") unless receipt.nil?
    fail!("#{fixture_id} queued entry has operator decision") unless decision.nil?
  when "replay_dry_run_ok"
    fail!("#{fixture_id} dry-run entry missing receipt") unless receipt && utc_timestamp?(receipt["dry_run_at"])
  when "replayed"
    fail!("#{fixture_id} replayed entry missing delivery") unless receipt && utc_timestamp?(receipt["replayed_at"]) && !receipt["delivered_message_id"].to_s.empty?
  when "replay_failed"
    fail!("#{fixture_id} replay_failed entry missing failure summary") unless receipt && utc_timestamp?(receipt["replayed_at"]) && !receipt["failure_summary"].to_s.empty?
  when "superseded"
    fail!("#{fixture_id} superseded entry missing state_reason") if payload["state_reason"].to_s.empty?
  when "discarded_by_operator"
    fail!("#{fixture_id} discard entry missing operator decision") unless decision && utc_timestamp?(decision["decided_at"]) && !decision["reason"].to_s.empty?
  else
    fail!("#{fixture_id} unknown state #{state.inspect}")
  end

  retention = payload.fetch("retention")
  fail!("#{fixture_id} ambiguous retention timestamp") unless utc_timestamp?(retention["retain_until"])
  fail!("#{fixture_id} automatic discard enabled") unless retention["automatic_discard"] == false
  fail!("#{fixture_id} retention redaction missing") if retention.fetch("redaction").empty?

  if (intent = payload["reservation_intent"])
    fail!("#{fixture_id} reservation paths missing") if intent.fetch("paths").empty?
    intent.fetch("paths").each do |intent_path|
      fail!("#{fixture_id} unsafe reservation path #{intent_path.inspect}") unless safe_repo_relative_path?(intent_path)
    end
    fail!("#{fixture_id} invalid reservation ttl") unless (60..604_800).cover?(intent["ttl_seconds"])
    fail!("#{fixture_id} invalid reservation reason digest") unless sha256?(intent["reason_sha256"])
  end

  if (fallback = payload["beads_fallback"])
    fail!("#{fixture_id} invalid fallback bead") unless fallback["bead_id"].match?(/\Aft-[a-z0-9]+(\.[0-9]+)?\z/)
    fail!("#{fixture_id} invalid fallback digest") unless sha256?(fallback["comment_sha256"])
    fail!("#{fixture_id} fallback preview leaks content") if secret_like?(fallback["comment_preview_redacted"]) || raw_pane_like?(fallback["comment_preview_redacted"])
  end

  states << state
  operations << payload.fetch("source_operation")
  failure_classes << failure.fetch("class")
  surface_operation_counts[payload.fetch("source_operation")] += 1
  surface_failure_counts[failure.fetch("class")] += 1
  case state
  when "queued"
    surface_counts["queued"] += 1
  when "replay_dry_run_ok"
    surface_counts["replayable"] += 1
  when "replay_failed"
    surface_counts["replay_failed"] += 1
  when "replayed"
    surface_counts["replayed"] += 1
  when "superseded"
    surface_counts["superseded"] += 1
  when "discarded_by_operator"
    surface_counts["discarded_by_operator"] += 1
  end
  surface_counts["reservation_intents"] += 1 if payload["reservation_intent"]
  surface_counts["beads_fallbacks"] += 1 if payload["beads_fallback"]
  surface_counts["stale_owner_handoffs"] += 1 if payload.fetch("source_operation") == "stale_owner_handoff_notice"
  surface_counts["ack_required"] += 1 if payload["ack_required"] == true
  surface_counts["delivery_unclaimed"] += 1 unless state == "replayed"
  actual_log << {
    "event" => "fixture_checked",
    "fixture" => fixture_id,
    "state" => state,
    "source_operation" => payload.fetch("source_operation"),
    "failure_class" => failure.fetch("class")
  }
end

actual_log << {
  "event" => "contract_summary",
  "fixtures" => fixture_paths.length,
  "states" => states.to_a.sort,
  "source_operations" => operations.to_a.sort,
  "failure_classes" => failure_classes.to_a.sort
}

actual_log << {
  "event" => "surface_summary",
  "contract_id" => "ft.agent_mail_outbox_surface.v1",
  "source_bead" => "ft-dezx8.4",
  "fixtures" => fixture_paths.length,
  "queued" => surface_counts["queued"],
  "replayable" => surface_counts["replayable"],
  "replay_failed" => surface_counts["replay_failed"],
  "replayed" => surface_counts["replayed"],
  "superseded" => surface_counts["superseded"],
  "discarded_by_operator" => surface_counts["discarded_by_operator"],
  "reservation_intents" => surface_counts["reservation_intents"],
  "beads_fallbacks" => surface_counts["beads_fallbacks"],
  "stale_owner_handoffs" => surface_counts["stale_owner_handoffs"],
  "ack_required" => surface_counts["ack_required"],
  "delivery_unclaimed" => surface_counts["delivery_unclaimed"],
  "by_failure_class" => surface_failure_counts.sort.to_h,
  "by_source_operation" => surface_operation_counts.sort.to_h
}

fail!("structured verifier log drifted") unless actual_log == expected_log
fail!("expected ack-required fixture missing") unless read_json("fixtures/agent-mail-outage-spool/valid/ack-required-message.json")["ack_required"] == true
fail!("expected timeout fixture missing") unless read_json("fixtures/agent-mail-outage-spool/valid/send-timeout.json").fetch("failure_reason").fetch("class") == "timeout"
fail!("expected reservation fixture missing") unless read_json("fixtures/agent-mail-outage-spool/valid/reservation-intent.json")["reservation_intent"]
fail!("expected reservation-conflict fixture missing") unless read_json("fixtures/agent-mail-outage-spool/valid/reservation-conflict.json").fetch("failure_reason").fetch("class") == "reservation_conflict"
fail!("expected Beads fallback fixture missing") unless read_json("fixtures/agent-mail-outage-spool/valid/beads-fallback-closeout.json")["beads_fallback"]
fail!("expected stale-owner fixture missing") unless read_json("fixtures/agent-mail-outage-spool/valid/stale-owner-handoff.json")["source_operation"] == "stale_owner_handoff_notice"
fail!("provenance missing outbox schema") unless provenance.include?("`ft-agent-mail-outbox-entry.json`")
fail!("rust contract missing replay states") unless EXPECTED_STATES.all? { |state| rust_contract.include?(state.split("_").map(&:capitalize).join) || rust_contract.include?(state) }
fail!("rust contract missing surface contract") unless rust_contract.include?("SURFACE_CONTRACT_ID") && rust_contract.include?("ft.agent_mail_outbox_surface.v1")
fail!("rust contract missing surface source bead") unless rust_contract.include?("SURFACE_SOURCE_BEAD") && rust_contract.include?("ft-dezx8.4")
fail!("rust contract missing surface loader") unless rust_contract.include?("load_agent_mail_outbox_surface")
fail!("rust contract missing delivery-claim semantics") unless rust_contract.include?("OutboxDeliveryClaim")

actual_log.each { |entry| puts(JSON.generate(entry)) }
puts "agent mail outage spool contract: static verifier passed (#{fixture_paths.length} fixtures)"
RUBY
