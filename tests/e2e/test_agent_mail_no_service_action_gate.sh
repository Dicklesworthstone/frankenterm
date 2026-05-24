#!/usr/bin/env bash
# Static gate proving Agent Mail fallback artifacts do not emit service actions.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

GATE="fixtures/agent-mail-failover/no-service-action-gate.json"
MANIFEST="fixtures/agent-mail-failover/manifest.json"
COMPANION_MANIFEST="fixtures/agent-mail-no-service-action/manifest.json"
COMPANION_DOC="docs/robot-contracts/agent-mail-no-service-action-gate.md"
COMPANION_VERIFIER="tests/e2e/test_agent_mail_no_service_action_contract.sh"
CLASSIFIER_CASES="fixtures/agent-mail-failover/retry-classifier-cases.json"
CLASSIFIER="scripts/agent-mail-failover-classifier.sh"

fail() {
  printf 'agent mail no-service-action gate: %s\n' "$*" >&2
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
require_file "${GATE}"
require_file "${MANIFEST}"
require_file "${COMPANION_MANIFEST}"
require_file "${COMPANION_DOC}"
require_file "${COMPANION_VERIFIER}"
require_file "${CLASSIFIER_CASES}"
require_file "${CLASSIFIER}"

bash -n "${CLASSIFIER}"
bash -n "${BASH_SOURCE[0]}"
bash -n "${COMPANION_VERIFIER}"
jq empty "${GATE}" "${MANIFEST}" "${COMPANION_MANIFEST}" "${CLASSIFIER_CASES}" \
  fixtures/agent-mail-no-service-action/positive/*.json \
  fixtures/agent-mail-no-service-action/negative/*.json

ruby <<'RUBY'
require "json"
require "open3"
require "set"

GATE = "fixtures/agent-mail-failover/no-service-action-gate.json"
MANIFEST = "fixtures/agent-mail-failover/manifest.json"
COMPANION_MANIFEST = "fixtures/agent-mail-no-service-action/manifest.json"
COMPANION_DOC = "docs/robot-contracts/agent-mail-no-service-action-gate.md"
COMPANION_VERIFIER = "tests/e2e/test_agent_mail_no_service_action_contract.sh"
CLASSIFIER_CASES = "fixtures/agent-mail-failover/retry-classifier-cases.json"
CLASSIFIER = "scripts/agent-mail-failover-classifier.sh"

def fail!(message)
  warn "agent mail no-service-action gate: #{message}"
  exit 1
end

def read_json(path)
  JSON.parse(File.read(path))
rescue JSON::ParserError => error
  fail!("#{path} does not parse as JSON: #{error.message}")
end

def unsafe_guidance?(text, patterns)
  downcased = text.downcase
  patterns.any? { |pattern| downcased.include?(pattern.downcase) }
end

def flatten_strings(value)
  case value
  when String
    [value]
  when Array
    value.flat_map { |item| flatten_strings(item) }
  when Hash
    value.values.flat_map { |item| flatten_strings(item) }
  else
    []
  end
end

def guidance_strings(value)
  fields = %w[
    generated_guidance
    next_actions
    recommendations
    error_summary
    proof_disclaimer
    message
  ]
  fields.flat_map { |field| flatten_strings(value[field]) }
end

def guidance_hits(strings, patterns)
  lowered_patterns = patterns.map(&:downcase)
  strings.flat_map do |text|
    lowered = text.downcase
    lowered_patterns.select { |pattern| lowered.include?(pattern) }.map { |pattern| [pattern, text] }
  end
end

def assert_false_flags!(path, object)
  flags = object["side_effects"] || object["safety"] || object["side_effect_policy"]
  # A fixture with no declared side-effect block must NOT vacuously pass the
  # all-false gate: an undeclared block is an unproven (ungated) side-effect set.
  fail!("#{path} declares no side-effect/safety flag block") unless flags.is_a?(Hash) && !flags.empty?
  # A boolean side-effect flag set true means the action is allowed (bad).
  # Non-boolean entries (e.g. proof_disclaimer text) are metadata, not flags.
  bad = flags.select { |_key, value| value == true }
  fail!("#{path} has non-false side-effect flags: #{bad.keys.inspect}") unless bad.empty?
end

gate = read_json(GATE)
manifest = read_json(MANIFEST)
companion_manifest = read_json(COMPANION_MANIFEST)
classifier_cases = read_json(CLASSIFIER_CASES)

fail!("gate schema version drifted") unless gate["schema_version"] == 1
fail!("gate contract drifted") unless gate["contract_id"] == "ft.agent_mail_failover_no_service_action_gate.v1"
fail!("gate bead drifted") unless gate["source_bead"] == "ft-5lsqo.4"
fail!("gate manifest pointer drifted") unless gate["manifest"] == MANIFEST
fail!("gate contract document pointer drifted") unless gate["contract_document"] == COMPANION_DOC
fail!("gate companion manifest pointer drifted") unless gate["companion_manifest"] == COMPANION_MANIFEST
fail!("gate companion verifier pointer drifted") unless gate["companion_verifier"] == COMPANION_VERIFIER
fail!("manifest verifier missing no-service gate") unless manifest.fetch("verification").include?("bash tests/e2e/test_agent_mail_no_service_action_gate.sh")
fail!("manifest verifier missing companion no-service gate") unless manifest.fetch("verification").include?("bash tests/e2e/test_agent_mail_no_service_action_contract.sh")
fail!("manifest companion pointer drifted") unless manifest["no_service_action_gate_companion_manifest"] == COMPANION_MANIFEST
fail!("manifest companion document pointer drifted") unless manifest["no_service_action_gate_document"] == COMPANION_DOC
fail!("manifest companion verifier pointer drifted") unless manifest["no_service_action_gate_companion_verifier"] == COMPANION_VERIFIER

fail!("companion manifest contract drifted") unless companion_manifest["contract_id"] == gate["contract_id"]
fail!("companion manifest canonical gate drifted") unless companion_manifest["canonical_gate"] == GATE
fail!("companion manifest canonical manifest drifted") unless companion_manifest["canonical_manifest"] == MANIFEST
fail!("companion manifest document drifted") unless companion_manifest["contract_document"] == COMPANION_DOC
fail!("companion manifest verifier drifted") unless companion_manifest["verifier"] == COMPANION_VERIFIER

source_forbidden = gate.fetch("source_scan_forbidden_literals")
guidance_forbidden = gate.fetch("guidance_forbidden_patterns")
fail!("forbidden literal count drifted") unless gate.fetch("forbidden_literal_count") == source_forbidden.length

scan_paths = (gate.fetch("scan_paths") + manifest.fetch("valid")).uniq.sort
scan_paths.each do |path|
  fail!("scan path missing: #{path}") unless File.file?(path)
end

scan_paths.each do |path|
  text = File.read(path)
  source_forbidden.each do |needle|
    fail!("#{path} contains raw forbidden service/destructive command #{needle}") if text.include?(needle)
  end
end

manifest.fetch("valid").each do |path|
  payload = read_json(path)
  guidance_strings = []
  guidance_strings.concat(Array(payload.dig("agent_mail", "error_summary")).compact)
  guidance_strings.concat(Array(payload["next_actions"]).compact)
  guidance_strings.concat(Array(payload.dig("safety", "proof_disclaimer")).compact)
  guidance_strings.each do |text|
    fail!("#{path} generated guidance suggests forbidden action: #{text}") if unsafe_guidance?(text, guidance_forbidden)
  end
end

classifier_cases.fetch("cases").each do |row|
  stdout, stderr, status = Open3.capture3("bash", CLASSIFIER, row.fetch("input"))
  fail!("classifier failed for #{row.fetch("id")}: #{stderr}") unless status.success?
  payload = JSON.parse(stdout)
  text = [payload["error_summary"], payload.fetch("reason_codes").join(" ")].compact.join(" ")
  fail!("classifier guidance suggests forbidden action for #{row.fetch("id")}: #{text}") if unsafe_guidance?(text, guidance_forbidden)
end

companion_positive = gate.fetch("companion_positive_fixtures")
companion_negative = gate.fetch("companion_negative_fixtures")
fail!("companion positive fixture list drifted") unless companion_manifest.fetch("positive_fixtures").sort == companion_positive.sort
fail!("companion negative fixture list drifted") unless companion_manifest.fetch("negative_fixtures").sort == companion_negative.sort

companion_positive.each do |path|
  payload = read_json(path)
  fail!("#{path} is not a positive fixture") unless payload["case_kind"] == "positive"
  fail!("#{path} expected verdict drifted") unless payload["expected_verdict"] == "allow"
  assert_false_flags!(path, payload)
  hits = guidance_hits(guidance_strings(payload), guidance_forbidden)
  fail!("#{path} emitted forbidden guidance: #{hits.inspect}") unless hits.empty?
end

companion_negative.each do |path|
  payload = read_json(path)
  fail!("#{path} is not a negative fixture") unless payload["case_kind"] == "negative"
  fail!("#{path} expected verdict drifted") unless payload["expected_verdict"] == "reject"
  assert_false_flags!(path, payload)
  hits = guidance_hits(guidance_strings(payload), guidance_forbidden)
  fail!("#{path} failed to trigger forbidden guidance detector") if hits.empty?
  expected = payload.fetch("expected_pattern").downcase
  fail!("#{path} did not trigger expected pattern #{expected.inspect}") unless hits.any? { |pattern, _text| pattern == expected }
end

gate.fetch("positive_guidance_cases").each do |row|
  fail!("positive case #{row.fetch("id")} tripped gate") if unsafe_guidance?(row.fetch("text"), guidance_forbidden)
end

gate.fetch("negative_guidance_cases").each do |row|
  fail!("negative case #{row.fetch("id")} did not trip gate") unless unsafe_guidance?(row.fetch("text"), guidance_forbidden)
end

puts "agent mail no-service-action gate: static verifier passed (#{scan_paths.length} files, #{source_forbidden.length} forbidden literals, #{gate.fetch("negative_guidance_cases").length} negative cases)"
RUBY
