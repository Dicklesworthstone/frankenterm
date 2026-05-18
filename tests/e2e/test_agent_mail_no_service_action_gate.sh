#!/usr/bin/env bash
# Static gate proving Agent Mail fallback artifacts do not emit service actions.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

GATE="fixtures/agent-mail-failover/no-service-action-gate.json"
MANIFEST="fixtures/agent-mail-failover/manifest.json"
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
require_file "${CLASSIFIER_CASES}"
require_file "${CLASSIFIER}"

bash -n "${CLASSIFIER}"
bash -n "${BASH_SOURCE[0]}"
jq empty "${GATE}" "${MANIFEST}" "${CLASSIFIER_CASES}"

ruby <<'RUBY'
require "json"
require "open3"
require "set"

GATE = "fixtures/agent-mail-failover/no-service-action-gate.json"
MANIFEST = "fixtures/agent-mail-failover/manifest.json"
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

gate = read_json(GATE)
manifest = read_json(MANIFEST)
classifier_cases = read_json(CLASSIFIER_CASES)

fail!("gate schema version drifted") unless gate["schema_version"] == 1
fail!("gate contract drifted") unless gate["contract_id"] == "ft.agent_mail_failover_no_service_action_gate.v1"
fail!("gate bead drifted") unless gate["source_bead"] == "ft-5lsqo.4"
fail!("gate manifest pointer drifted") unless gate["manifest"] == MANIFEST
fail!("manifest verifier missing no-service gate") unless manifest.fetch("verification").include?("bash tests/e2e/test_agent_mail_no_service_action_gate.sh")

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

gate.fetch("positive_guidance_cases").each do |row|
  fail!("positive case #{row.fetch("id")} tripped gate") if unsafe_guidance?(row.fetch("text"), guidance_forbidden)
end

gate.fetch("negative_guidance_cases").each do |row|
  fail!("negative case #{row.fetch("id")} did not trip gate") unless unsafe_guidance?(row.fetch("text"), guidance_forbidden)
end

puts "agent mail no-service-action gate: static verifier passed (#{scan_paths.length} files, #{source_forbidden.length} forbidden literals, #{gate.fetch("negative_guidance_cases").length} negative cases)"
RUBY
