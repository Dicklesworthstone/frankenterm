#!/usr/bin/env bash
# Static verifier for Agent Mail fallback no-service-action guidance.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

MANIFEST="fixtures/agent-mail-no-service-action/manifest.json"
CANONICAL_GATE="fixtures/agent-mail-failover/no-service-action-gate.json"
CANONICAL_MANIFEST="fixtures/agent-mail-failover/manifest.json"
CLASSIFIER="scripts/agent-mail-failover-classifier.sh"

fail() {
  printf 'agent mail no-service-action contract: %s\n' "$*" >&2
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
require_file "${MANIFEST}"
require_file "${CANONICAL_GATE}"
require_file "${CANONICAL_MANIFEST}"
require_file "${CLASSIFIER}"

jq empty "${MANIFEST}" "${CANONICAL_GATE}" "${CANONICAL_MANIFEST}"
bash -n "${CLASSIFIER}"

ruby <<'RUBY'
require "json"
require "open3"

MANIFEST = "fixtures/agent-mail-no-service-action/manifest.json"
CANONICAL_GATE = "fixtures/agent-mail-failover/no-service-action-gate.json"
CANONICAL_MANIFEST = "fixtures/agent-mail-failover/manifest.json"
CLASSIFIER = "scripts/agent-mail-failover-classifier.sh"

def fail!(message)
  warn "agent mail no-service-action contract: #{message}"
  exit 1
end

def read_json(path)
  JSON.parse(File.read(path))
rescue JSON::ParserError => error
  fail!("#{path} does not parse as JSON: #{error.message}")
end

def all_strings(value, out = [])
  case value
  when String
    out << value
  when Array
    value.each { |entry| all_strings(entry, out) }
  when Hash
    value.each_value { |entry| all_strings(entry, out) }
  end
  out
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
  fields.flat_map { |field| all_strings(value[field]) }
end

def forbidden_hits(strings, patterns)
  lowered_patterns = patterns.map(&:downcase)
  strings.flat_map do |text|
    lowered = text.downcase
    lowered_patterns.select { |pattern| lowered.include?(pattern) }.map { |pattern| [pattern, text] }
  end
end

def assert_false_flags!(path, object)
  flags = object.fetch("side_effects", object.fetch("safety", object.fetch("side_effect_policy", {})))
  bad = flags.select { |_key, value| value == true }
  fail!("#{path} has non-false side-effect flags: #{bad.keys.inspect}") unless bad.empty?
end

manifest = read_json(MANIFEST)
canonical_gate = read_json(CANONICAL_GATE)
canonical_manifest = read_json(CANONICAL_MANIFEST)
fail!("manifest version drifted") unless manifest["schema_version"] == 1
fail!("contract id drifted") unless manifest["contract_id"] == "ft.agent_mail_failover_no_service_action_gate.v1"
fail!("bead drifted") unless manifest["bead"] == "ft-5lsqo.4"
fail!("canonical manifest pointer drifted") unless manifest["canonical_manifest"] == CANONICAL_MANIFEST
fail!("canonical gate pointer drifted") unless manifest["canonical_gate"] == CANONICAL_GATE
fail!("canonical gate contract drifted") unless canonical_gate["contract_id"] == manifest["contract_id"]
fail!("canonical gate companion pointer drifted") unless canonical_gate["companion_manifest"] == MANIFEST
fail!("canonical manifest companion pointer drifted") unless canonical_manifest["no_service_action_gate_companion_manifest"] == MANIFEST
fail!("canonical manifest companion verifier drifted") unless canonical_manifest["no_service_action_gate_companion_verifier"] == manifest["verifier"]
fail!("canonical manifest missing companion verifier") unless canonical_manifest.fetch("verification").include?(manifest["verifier"].sub(%r{\A}, "bash "))
fail!("runtime command unexpectedly shipped") unless manifest["runtime_command_shipped"] == false
assert_false_flags!(MANIFEST, manifest)

patterns = manifest.fetch("forbidden_guidance_patterns")
expected_identifiers = manifest.fetch("expected_forbidden_action_identifiers").sort
checked_files = [MANIFEST, CANONICAL_GATE, CANONICAL_MANIFEST]

positive_paths = manifest.fetch("positive_fixtures")
negative_paths = manifest.fetch("negative_fixtures")

(positive_paths + negative_paths).each { |path| checked_files << path; read_json(path) }

positive_paths.each do |path|
  payload = read_json(path)
  fail!("#{path} is not a positive fixture") unless payload["case_kind"] == "positive"
  fail!("#{path} expected verdict drifted") unless payload["expected_verdict"] == "allow"
  assert_false_flags!(path, payload)
  hits = forbidden_hits(guidance_strings(payload), patterns)
  fail!("#{path} emitted forbidden guidance: #{hits.inspect}") unless hits.empty?
end

negative_paths.each do |path|
  payload = read_json(path)
  fail!("#{path} is not a negative fixture") unless payload["case_kind"] == "negative"
  fail!("#{path} expected verdict drifted") unless payload["expected_verdict"] == "reject"
  assert_false_flags!(path, payload)
  hits = forbidden_hits(guidance_strings(payload), patterns)
  fail!("#{path} failed to trigger forbidden guidance detector") if hits.empty?
  expected = payload.fetch("expected_pattern").downcase
  fail!("#{path} did not trigger expected pattern #{expected.inspect}") unless hits.any? { |pattern, _text| pattern == expected }
end

manifest.fetch("fixture_scan_globs").each do |glob|
  matches = Dir.glob(glob).sort
  fail!("fixture glob matched nothing: #{glob}") if matches.empty?
  matches.each do |path|
    checked_files << path
    payload = read_json(path)
    assert_false_flags!(path, payload)
    if payload.dig("agent_mail", "forbidden_actions")
      actual = payload.fetch("agent_mail").fetch("forbidden_actions").sort
      fail!("#{path} forbidden action identifier drifted") unless actual == expected_identifiers
    end
    hits = forbidden_hits(guidance_strings(payload).compact, patterns)
    fail!("#{path} emitted forbidden recommendation text: #{hits.inspect}") unless hits.empty?
  end
end

cases_path = manifest.fetch("classifier_cases")
checked_files << cases_path
cases = read_json(cases_path)
cases.fetch("cases").each do |row|
  stdout, stderr, status = Open3.capture3("bash", CLASSIFIER, row.fetch("input"))
  fail!("classifier failed for #{row.fetch("id")}: #{stderr}") unless status.success?
  payload = JSON.parse(stdout)
  actual = payload.fetch("forbidden_actions").sort
  fail!("classifier forbidden action identifier drifted for #{row.fetch("id")}") unless actual == expected_identifiers
  hits = forbidden_hits(guidance_strings(payload).compact, patterns)
  fail!("classifier emitted forbidden guidance for #{row.fetch("id")}: #{hits.inspect}") unless hits.empty?
end

manifest.fetch("production_script_scan_files").each do |path|
  checked_files << path
  content = File.read(path)
  executable_hits = []
  content.each_line.with_index(1) do |line, number|
    stripped = line.strip
    next if stripped.empty? || stripped.start_with?("#")
    forbidden_hits([line], patterns).each do |pattern, text|
      executable_hits << [path, number, pattern, text.strip]
    end
  end
  fail!("production script contains executable forbidden command text: #{executable_hits.inspect}") unless executable_hits.empty?
end

allowed_doc_context = /(forbidden|must not|do not|does not|not authorize|reject|negative|fail if|detect|deny-list|no service|not invoke|without suggesting|must keep.*false)/i
manifest.fetch("document_scan_files").each do |path|
  checked_files << path
  previous_lines = []
  File.read(path).each_line.with_index(1) do |line, number|
    hits = forbidden_hits([line], patterns)
    if hits.empty?
      previous_lines << line
      previous_lines.shift while previous_lines.length > 2
      next
    end
    context = (previous_lines + [line]).join(" ")
    if context.match?(allowed_doc_context)
      previous_lines << line
      previous_lines.shift while previous_lines.length > 2
      next
    end
    fail!("#{path}:#{number} mentions forbidden guidance without negative context: #{line.strip}")
  end
end

log = {
  "contract_id" => manifest.fetch("contract_id"),
  "checked_files" => checked_files.uniq.sort,
  "positive_cases" => positive_paths.length,
  "negative_cases" => negative_paths.length,
  "forbidden_pattern_count" => patterns.length,
  "verdict" => "pass"
}

missing_log_fields = manifest.fetch("required_log_fields") - log.keys
fail!("verifier log missing fields: #{missing_log_fields.inspect}") unless missing_log_fields.empty?

puts JSON.generate(log)
RUBY
