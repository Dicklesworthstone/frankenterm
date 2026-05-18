#!/usr/bin/env bash
# Static verifier for the provider quota/cost assignment contract and fixtures.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

SCHEMA="docs/json-schema/ft-provider-quota-assignment.json"
DOC="docs/robot-contracts/provider-quota-assignment.md"
FIXTURES="fixtures/mission-planner/provider-quota-assignment/cases.v1.json"
INVALID_FIXTURES="fixtures/mission-planner/provider-quota-assignment/invalid/fragments.v1.json"
PROVENANCE="docs/json-schema/PROVENANCE.md"
REQUIRED_CASES=(
  "healthy-quota"
  "near-reset-degrade"
  "hard-rate-limit"
  "unknown-quota"
  "conflicting-account-evidence"
  "high-cost-requires-approval"
  "stale-evidence"
  "privacy-redacted-evidence"
  "provider-unavailable"
)
REQUIRED_RECOMMENDATIONS=(
  "assign"
  "defer"
  "degrade_model_class"
  "require_approval"
  "request_fresh_quota_evidence"
)
REQUIRED_REASON_CODES=(
  "quota.healthy"
  "quota.near_reset"
  "quota.hard_rate_limit"
  "quota.unknown"
  "quota.conflicting_evidence"
  "quota.high_cost_requires_approval"
  "quota.stale_evidence"
  "quota.privacy_redacted"
  "quota.provider_unavailable"
)
REQUIRED_FORBIDDEN=(
  "provider_api_call"
  "credential_mutation"
  "account_rotation"
  "hidden_spend_decision"
  "service_mutation"
  "local_cargo_proof"
  "raw_secret_storage"
)
REQUIRED_INVALID_CASES=(
  "provider-api-call-permitted"
  "credential-mutation-permitted"
  "hidden-spend-decision-permitted"
  "assign-with-stale-evidence"
  "raw-secret-storage-permitted"
  "toon-row-width-mismatch"
)

fail() {
  printf 'provider quota assignment contract: %s\n' "$*" >&2
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
require_command rg

require_file "${SCHEMA}"
require_file "${DOC}"
require_file "${FIXTURES}"
require_file "${INVALID_FIXTURES}"
require_file "${PROVENANCE}"

jq empty "${SCHEMA}" "${FIXTURES}" "${INVALID_FIXTURES}"

ruby <<'RUBY'
require "json"
require "set"

SCHEMA = "docs/json-schema/ft-provider-quota-assignment.json"
DOC = "docs/robot-contracts/provider-quota-assignment.md"
FIXTURES = "fixtures/mission-planner/provider-quota-assignment/cases.v1.json"
INVALID_FIXTURES = "fixtures/mission-planner/provider-quota-assignment/invalid/fragments.v1.json"
PROVENANCE = "docs/json-schema/PROVENANCE.md"
REQUIRED_CASES = %w[
  healthy-quota
  near-reset-degrade
  hard-rate-limit
  unknown-quota
  conflicting-account-evidence
  high-cost-requires-approval
  stale-evidence
  privacy-redacted-evidence
  provider-unavailable
].freeze
REQUIRED_RECOMMENDATIONS = %w[
  assign
  defer
  degrade_model_class
  require_approval
  request_fresh_quota_evidence
].freeze
REQUIRED_REASON_CODES = %w[
  quota.healthy
  quota.near_reset
  quota.hard_rate_limit
  quota.unknown
  quota.conflicting_evidence
  quota.high_cost_requires_approval
  quota.stale_evidence
  quota.privacy_redacted
  quota.provider_unavailable
].freeze
REQUIRED_FORBIDDEN = %w[
  provider_api_call
  credential_mutation
  account_rotation
  hidden_spend_decision
  service_mutation
  local_cargo_proof
  raw_secret_storage
].freeze
REQUIRED_INVALID_CASES = %w[
  provider-api-call-permitted
  credential-mutation-permitted
  hidden-spend-decision-permitted
  assign-with-stale-evidence
  raw-secret-storage-permitted
  toon-row-width-mismatch
].freeze
FAIL_CLOSED_ACTIONS = %w[
  defer
  degrade_model_class
  require_approval
  request_fresh_quota_evidence
].freeze

def fail!(message)
  warn "provider quota assignment contract: #{message}"
  exit 1
end

def read_json(path)
  JSON.parse(File.read(path))
rescue JSON::ParserError => error
  fail!("#{path} does not parse as JSON: #{error.message}")
end

schema = read_json(SCHEMA)
fixtures = read_json(FIXTURES)
invalid_fixtures = read_json(INVALID_FIXTURES)
doc = File.read(DOC)
provenance = File.read(PROVENANCE)

fail!("schema id drifted") unless schema["$id"]&.end_with?("/ft-provider-quota-assignment.json")
fail!("schema contract const drifted") unless schema.dig("properties", "contract_id", "const") == "ft.provider_quota_assignment.v1"
fail!("schema source bead const drifted") unless schema.dig("properties", "source_bead", "const") == "ft-auy2g.8"
fail!("dry_run must be const true") unless schema.dig("properties", "dry_run", "const") == true
fail!("read_only must be const true") unless schema.dig("properties", "read_only", "const") == true

schema_actions = schema.dig("$defs", "recommendation", "properties", "action", "enum")
fail!("recommendation enum drifted") unless schema_actions.sort == REQUIRED_RECOMMENDATIONS.sort
schema_reasons = schema.dig("$defs", "reason_code", "enum")
REQUIRED_REASON_CODES.each do |reason|
  fail!("schema missing reason #{reason}") unless schema_reasons.include?(reason)
end
schema_forbidden = schema.dig("$defs", "forbidden_action", "enum")
fail!("schema forbidden enum drifted") unless schema_forbidden.sort == REQUIRED_FORBIDDEN.sort

fail!("fixture schema version drifted") unless fixtures["schema_version"] == "ft.provider_quota_assignment.fixtures.v1"
fail!("fixture contract id drifted") unless fixtures["contract_id"] == "ft.provider_quota_assignment.fixture_manifest.v1"
fail!("fixture schema pointer drifted") unless fixtures["schema_path"] == SCHEMA
fail!("fixture doc pointer drifted") unless fixtures["contract_doc"] == DOC
fail!("fixture source bead drifted") unless fixtures["source_bead"] == "ft-auy2g.8"
fail!("fixture verifier missing") unless fixtures.fetch("verification").include?("bash tests/e2e/test_provider_quota_assignment_contract.sh")
fail!("fixture forbidden actions drifted") unless fixtures.fetch("required_forbidden_actions").sort == REQUIRED_FORBIDDEN.sort
fail!("toon columns too sparse") unless fixtures.fetch("toon_columns").length >= 6

fail!("invalid fixture schema version drifted") unless invalid_fixtures["schema_version"] == "ft.provider_quota_assignment.invalid_fragments.v1"
fail!("invalid fixture contract id drifted") unless invalid_fixtures["contract_id"] == "ft.provider_quota_assignment.invalid_fragments.v1"
fail!("invalid fixture schema pointer drifted") unless invalid_fixtures["schema_path"] == SCHEMA
fail!("invalid fixture valid fixture pointer drifted") unless invalid_fixtures["valid_fixture"] == FIXTURES
fail!("invalid fixture doc pointer drifted") unless invalid_fixtures["contract_doc"] == DOC
fail!("invalid fixture source bead drifted") unless invalid_fixtures["source_bead"] == "ft-auy2g.11"
fail!("invalid fixture verifier missing") unless invalid_fixtures.fetch("verification").include?("bash tests/e2e/test_provider_quota_assignment_contract.sh")

cases = fixtures.fetch("cases")
case_ids = cases.map { |entry| entry.fetch("case_id") }
fail!("case coverage drifted: #{case_ids.sort.inspect}") unless case_ids.sort == REQUIRED_CASES.sort
fail!("case ids are not unique") unless case_ids.uniq.length == case_ids.length

invalid_cases = invalid_fixtures.fetch("cases")
invalid_case_ids = invalid_cases.map { |entry| entry.fetch("case_id") }
fail!("invalid case coverage drifted: #{invalid_case_ids.sort.inspect}") unless invalid_case_ids.sort == REQUIRED_INVALID_CASES.sort
fail!("invalid case ids are not unique") unless invalid_case_ids.uniq.length == invalid_case_ids.length

invalid_by_id = invalid_cases.to_h { |entry| [entry.fetch("case_id"), entry] }
invalid_cases.each do |entry|
  %w[case_id expected_failure reason_codes invalid_fragment].each do |field|
    fail!("invalid case #{entry["case_id"] || "(missing)"} lacks #{field}") unless entry.key?(field)
  end
  fail!("invalid case #{entry.fetch("case_id")} has no reason codes") if entry.fetch("reason_codes").empty?
end

provider_api = invalid_by_id.fetch("provider-api-call-permitted")
fail!("provider-api expected failure drifted") unless provider_api.fetch("expected_failure") == "provider_api_call_must_stay_forbidden"
fail!("provider-api reason drifted") unless provider_api.fetch("reason_codes").include?("planner.provider_api_call_forbidden")
fail!("provider-api missing action marker drifted") unless provider_api.dig("invalid_fragment", "missing_forbidden_action") == "provider_api_call"
fail!("provider-api forbidden list should omit provider_api_call") if provider_api.dig("invalid_fragment", "forbidden_actions").include?("provider_api_call")

credential_mutation = invalid_by_id.fetch("credential-mutation-permitted")
fail!("credential mutation expected failure drifted") unless credential_mutation.fetch("expected_failure") == "credential_mutation_must_stay_forbidden"
fail!("credential mutation reason drifted") unless credential_mutation.fetch("reason_codes").include?("planner.credential_mutation_forbidden")
fail!("credential mutation marker drifted") unless credential_mutation.dig("invalid_fragment", "missing_forbidden_action") == "credential_mutation"
fail!("credential mutation forbidden list should omit credential_mutation") if credential_mutation.dig("invalid_fragment", "forbidden_actions").include?("credential_mutation")

hidden_spend = invalid_by_id.fetch("hidden-spend-decision-permitted")
fail!("hidden spend expected failure drifted") unless hidden_spend.fetch("expected_failure") == "hidden_spend_decision_must_stay_forbidden"
fail!("hidden spend reason drifted") unless hidden_spend.fetch("reason_codes").include?("planner.hidden_spend_decision_forbidden")
fail!("hidden spend marker drifted") unless hidden_spend.dig("invalid_fragment", "missing_forbidden_action") == "hidden_spend_decision"
fail!("hidden spend forbidden list should omit hidden_spend_decision") if hidden_spend.dig("invalid_fragment", "forbidden_actions").include?("hidden_spend_decision")

stale_assign = invalid_by_id.fetch("assign-with-stale-evidence")
fail!("stale assign expected failure drifted") unless stale_assign.fetch("expected_failure") == "assign_requires_fresh_evidence"
fail!("stale assign reason drifted") unless stale_assign.fetch("reason_codes").include?("quota.stale_evidence")
fail!("stale assign action drifted") unless stale_assign.dig("invalid_fragment", "recommendation", "action") == "assign"
fail!("stale assign evidence drifted") unless stale_assign.dig("invalid_fragment", "evidence", 0, "freshness_state") == "stale"

raw_secret = invalid_by_id.fetch("raw-secret-storage-permitted")
fail!("raw secret expected failure drifted") unless raw_secret.fetch("expected_failure") == "raw_secret_storage_must_stay_forbidden"
fail!("raw secret reason drifted") unless raw_secret.fetch("reason_codes").include?("planner.raw_secret_storage_forbidden")
fail!("raw secret marker drifted") unless raw_secret.dig("invalid_fragment", "missing_forbidden_action") == "raw_secret_storage"
fail!("raw secret forbidden list should omit raw_secret_storage") if raw_secret.dig("invalid_fragment", "forbidden_actions").include?("raw_secret_storage")

toon_width = invalid_by_id.fetch("toon-row-width-mismatch")
fail!("toon width expected failure drifted") unless toon_width.fetch("expected_failure") == "toon_rows_must_match_declared_columns"
fail!("toon width reason drifted") unless toon_width.fetch("reason_codes").include?("toon.row_width_mismatch")
toon_columns = toon_width.dig("invalid_fragment", "toon_projection", "columns")
toon_rows = toon_width.dig("invalid_fragment", "toon_projection", "rows")
fail!("toon width fragment drifted") unless toon_rows.any? { |row| row.length != toon_columns.length }

recommendations_seen = Set.new
reasons_seen = Set.new

cases.each do |entry|
  case_id = entry.fetch("case_id")
  expected_recommendation = entry.fetch("expected_recommendation")
  required_reason = entry.fetch("required_reason_code")
  artifact = entry.fetch("artifact")

  fail!("#{case_id} artifact schema version drifted") unless artifact["schema_version"] == 1
  fail!("#{case_id} artifact contract id drifted") unless artifact["contract_id"] == "ft.provider_quota_assignment.v1"
  fail!("#{case_id} source bead drifted") unless artifact["source_bead"] == "ft-auy2g.8"
  fail!("#{case_id} dry_run drifted") unless artifact["dry_run"] == true
  fail!("#{case_id} read_only drifted") unless artifact["read_only"] == true
  fail!("#{case_id} forbidden actions drifted") unless artifact.fetch("forbidden_actions").sort == REQUIRED_FORBIDDEN.sort

  context = artifact.fetch("planner_context")
  %w[objective_id task_urgency proof_criticality expected_token_class expected_cost_class required_model_class mission_objective_plan_ref].each do |field|
    fail!("#{case_id} planner context missing #{field}") unless context[field]
  end
  fail!("#{case_id} does not cite mission objective plan") unless context["mission_objective_plan_ref"] == "ft.mission_objective_plan.v1"

  evidence = artifact.fetch("evidence")
  fail!("#{case_id} evidence missing") if evidence.empty?
  evidence.each do |row|
    %w[
      evidence_id provider model_class account_class quota_remaining quota_window_reset_ms
      observed_rate_limit_state marginal_cost_class confidence freshness_state source_artifact
      redaction_state provider_available
    ].each do |field|
      fail!("#{case_id} evidence #{row["evidence_id"] || "(missing)"} lacks #{field}") unless row.key?(field)
    end
    fail!("#{case_id} source artifact must point at fixtures") unless row.fetch("source_artifact") == FIXTURES
    fail!("#{case_id} raw provider evidence is forbidden") if row.fetch("redaction_state") == "raw_forbidden"
  end

  recommendation = artifact.fetch("recommendation")
  action = recommendation.fetch("action")
  recommendations_seen.add(action)
  fail!("#{case_id} expected #{expected_recommendation}, got #{action}") unless action == expected_recommendation
  fail!("#{case_id} approval flag drifted") if action == "require_approval" && recommendation.fetch("requires_approval") != true
  fail!("#{case_id} approval flag should be false") if action != "require_approval" && recommendation.fetch("requires_approval") != false
  fail!("#{case_id} operator message missing") if recommendation.fetch("operator_message").strip.empty?

  reason_codes = artifact.fetch("reason_codes")
  reason_codes.each { |reason| reasons_seen.add(reason) }
  fail!("#{case_id} missing expected reason #{required_reason}") unless reason_codes.include?(required_reason)
  %w[
    planner.read_only
    planner.no_provider_api_call
    planner.no_credential_mutation
    planner.no_account_rotation
  ].each do |reason|
    fail!("#{case_id} missing safety reason #{reason}") unless reason_codes.include?(reason)
  end

  if action == "assign"
    evidence.each do |row|
      fail!("#{case_id} assign requires available provider") unless row.fetch("provider_available") == true
      fail!("#{case_id} assign requires fresh evidence") unless row.fetch("freshness_state") == "fresh"
      fail!("#{case_id} assign requires high confidence") unless row.fetch("confidence") == "high"
      fail!("#{case_id} assign requires redacted summary") unless row.fetch("redaction_state") == "redacted_summary"
      fail!("#{case_id} assign requires positive quota") unless row.fetch("quota_remaining").to_i.positive?
      fail!("#{case_id} assign requires clear rate limit") unless row.fetch("observed_rate_limit_state") == "clear"
    end
  else
    fail!("#{case_id} non-assign must use fail-closed action") unless FAIL_CLOSED_ACTIONS.include?(action)
  end

  if reason_codes.include?("quota.unknown")
    fail!("#{case_id} unknown quota must request fresh evidence") unless action == "request_fresh_quota_evidence"
  end
  if reason_codes.include?("quota.conflicting_evidence")
    fail!("#{case_id} conflicting evidence must request fresh evidence") unless action == "request_fresh_quota_evidence"
    states = evidence.map { |row| row.fetch("observed_rate_limit_state") }.uniq
    quotas = evidence.map { |row| row["quota_remaining"] }.uniq
    fail!("#{case_id} conflict fixture does not contain contradictory evidence") unless states.length > 1 || quotas.length > 1
  end
  if reason_codes.include?("quota.hard_rate_limit")
    fail!("#{case_id} hard rate limit must defer") unless action == "defer"
  end
  if reason_codes.include?("quota.high_cost_requires_approval")
    fail!("#{case_id} high cost must require approval") unless action == "require_approval"
    fail!("#{case_id} high cost context drifted") unless context.fetch("expected_cost_class") == "high"
  end
  if reason_codes.include?("quota.stale_evidence")
    fail!("#{case_id} stale evidence must request fresh evidence") unless action == "request_fresh_quota_evidence"
    fail!("#{case_id} stale fixture missing stale evidence") unless evidence.any? { |row| row.fetch("freshness_state") == "stale" }
  end
  if reason_codes.include?("quota.privacy_redacted")
    fail!("#{case_id} privacy-redacted evidence must request fresh evidence") unless action == "request_fresh_quota_evidence"
    fail!("#{case_id} privacy fixture missing privacy_redacted evidence") unless evidence.any? { |row| row.fetch("redaction_state") == "privacy_redacted" }
  end
  if reason_codes.include?("quota.provider_unavailable")
    fail!("#{case_id} unavailable provider must request fresh evidence") unless action == "request_fresh_quota_evidence"
    fail!("#{case_id} unavailable fixture missing provider_available=false") unless evidence.any? { |row| row.fetch("provider_available") == false }
  end

  artifact_paths = artifact.fetch("artifact_paths")
  fail!("#{case_id} missing self artifact path") unless artifact_paths.include?(FIXTURES)

  toon = artifact.fetch("toon_projection")
  fail!("#{case_id} TOON columns drifted") unless toon.fetch("columns") == fixtures.fetch("toon_columns")
  fail!("#{case_id} TOON rows missing") if toon.fetch("rows").empty?
  toon.fetch("rows").each do |row|
    fail!("#{case_id} TOON row width drifted") unless row.length == toon.fetch("columns").length
    fail!("#{case_id} TOON row does not name case") unless row.first == case_id
    fail!("#{case_id} TOON row does not carry recommendation") unless row.include?(action)
    fail!("#{case_id} TOON row does not carry reason") unless row.include?(required_reason)
  end
end

REQUIRED_RECOMMENDATIONS.each do |action|
  fail!("missing recommendation fixture #{action}") unless recommendations_seen.include?(action)
end
REQUIRED_REASON_CODES.each do |reason|
  fail!("missing reason fixture #{reason}") unless reasons_seen.include?(reason)
end

%w[
  ft.provider_quota_assignment.v1
  docs/json-schema/ft-provider-quota-assignment.json
  fixtures/mission-planner/provider-quota-assignment/cases.v1.json
  provider_api_call
  credential_mutation
  account_rotation
  hidden_spend_decision
  request_fresh_quota_evidence
  require_approval
].each do |needle|
  fail!("doc missing #{needle}") unless doc.include?(needle)
end
REQUIRED_INVALID_CASES.each do |needle|
  fail!("doc missing invalid case #{needle}") unless doc.include?(needle)
end

fail!("provenance missing schema row") unless provenance.include?("ft-provider-quota-assignment.json")
fail!("provenance missing verifier") unless provenance.include?("test_provider_quota_assignment_contract.sh")

puts "provider quota assignment contract: static verifier passed (#{cases.length} cases, #{recommendations_seen.length} recommendations, #{invalid_cases.length} invalid cases)"
RUBY

if rg -n --hidden --glob '!*.md' \
  '(sk-[A-Za-z0-9]{20,}|AKIA[0-9A-Z]{16}|ghp_[A-Za-z0-9]{20,}|xox[baprs]-[A-Za-z0-9-]{20,}|Bearer [A-Za-z0-9._-]{20,}|BEGIN (RSA|OPENSSH|EC) PRIVATE KEY)' \
  "${FIXTURES}" "${INVALID_FIXTURES}" >/tmp/ft-provider-quota-assignment-secret-scan.txt; then
  cat /tmp/ft-provider-quota-assignment-secret-scan.txt >&2
  fail "secret-shaped strings found in provider quota assignment fixtures"
fi
