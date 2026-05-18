#!/usr/bin/env bash
# Static verifier for the agent task-fit passport contract and fixtures.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

SCHEMA="docs/json-schema/ft-agent-task-fit-passport.json"
DOC="docs/robot-contracts/agent-task-fit-passport.md"
FIXTURES="fixtures/mission-planner/agent-task-fit-passport/cases.v1.json"
REQUIRED_CASES=(
  "good-fit"
  "poor-fit"
  "stale-evidence"
  "active-owner-conflict"
  "missing-proof"
  "recent-failed-closeout"
  "agent-unavailable"
  "operator-approval-needed"
  "privacy-redacted"
)
REQUIRED_RECOMMENDATIONS=(
  "assign"
  "wait_for_owner"
  "request_fresh_evidence"
  "avoid_assignment"
  "require_operator_approval"
)
REQUIRED_REASON_CODES=(
  "fit.strong_capability"
  "fit.poor_capability"
  "fit.stale_evidence"
  "fit.active_owner_conflict"
  "fit.missing_proof"
  "fit.recent_failure"
  "fit.agent_unavailable"
  "fit.requires_approval"
  "fit.privacy_redacted"
)
REQUIRED_FORBIDDEN=(
  "human_performance_scoreboard"
  "raw_pane_text_storage"
  "mail_body_storage"
  "secret_material_storage"
  "auto_reassignment"
  "beads_mutation"
  "agent_mail_mutation"
  "pane_mutation"
  "service_mutation"
  "local_cargo_proof"
)
REQUIRED_SOURCE_KINDS=(
  "beads_closeout"
  "rch_proof"
  "agent_mail_handoff"
  "pane_runtime_state"
  "git_publication"
)
REQUIRED_EVIDENCE_CATEGORIES=(
  "recent_successful_proof"
  "recent_failure_class"
  "beads_closeout_quality"
  "rch_proof_quality"
  "agent_mail_handoff_quality"
  "pane_runtime_availability"
  "git_publication_status"
  "active_owner_conflict"
  "stale_or_blocked_state"
  "missing_evidence"
  "privacy_redaction"
)

fail() {
  printf 'agent task-fit passport contract: %s\n' "$*" >&2
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

jq empty "${SCHEMA}" "${FIXTURES}"

ruby <<'RUBY'
require "json"
require "set"

SCHEMA = "docs/json-schema/ft-agent-task-fit-passport.json"
DOC = "docs/robot-contracts/agent-task-fit-passport.md"
FIXTURES = "fixtures/mission-planner/agent-task-fit-passport/cases.v1.json"
REQUIRED_CASES = %w[
  good-fit
  poor-fit
  stale-evidence
  active-owner-conflict
  missing-proof
  recent-failed-closeout
  agent-unavailable
  operator-approval-needed
  privacy-redacted
].freeze
REQUIRED_RECOMMENDATIONS = %w[
  assign
  wait_for_owner
  request_fresh_evidence
  avoid_assignment
  require_operator_approval
].freeze
REQUIRED_REASON_CODES = %w[
  fit.strong_capability
  fit.poor_capability
  fit.stale_evidence
  fit.active_owner_conflict
  fit.missing_proof
  fit.recent_failure
  fit.agent_unavailable
  fit.requires_approval
  fit.privacy_redacted
].freeze
REQUIRED_FORBIDDEN = %w[
  human_performance_scoreboard
  raw_pane_text_storage
  mail_body_storage
  secret_material_storage
  auto_reassignment
  beads_mutation
  agent_mail_mutation
  pane_mutation
  service_mutation
  local_cargo_proof
].freeze
REQUIRED_SOURCE_KINDS = %w[
  beads_closeout
  rch_proof
  agent_mail_handoff
  pane_runtime_state
  git_publication
].freeze
REQUIRED_EVIDENCE_CATEGORIES = %w[
  recent_successful_proof
  recent_failure_class
  beads_closeout_quality
  rch_proof_quality
  agent_mail_handoff_quality
  pane_runtime_availability
  git_publication_status
  active_owner_conflict
  stale_or_blocked_state
  missing_evidence
  privacy_redaction
].freeze
FAIL_CLOSED_ACTIONS = %w[
  wait_for_owner
  request_fresh_evidence
  avoid_assignment
  require_operator_approval
].freeze
SAFETY_REASONS = %w[
  safety.read_only
  safety.no_raw_pane_content
  safety.no_mail_body_storage
  safety.no_auto_reassignment
  safety.no_human_scoreboard
  planner.mission_objective_ref
].freeze

def fail!(message)
  warn "agent task-fit passport contract: #{message}"
  exit 1
end

def read_json(path)
  JSON.parse(File.read(path))
rescue JSON::ParserError => error
  fail!("#{path} does not parse as JSON: #{error.message}")
end

schema = read_json(SCHEMA)
fixtures = read_json(FIXTURES)
doc = File.read(DOC)

fail!("schema id drifted") unless schema["$id"]&.end_with?("/ft-agent-task-fit-passport.json")
fail!("schema contract const drifted") unless schema.dig("properties", "contract_id", "const") == "ft.agent_task_fit_passport.v1"
fail!("schema source bead const drifted") unless schema.dig("properties", "source_bead", "const") == "ft-auy2g.9"
fail!("dry_run must be const true") unless schema.dig("properties", "dry_run", "const") == true
fail!("read_only must be const true") unless schema.dig("properties", "read_only", "const") == true
fail!("human subject must be const false") unless schema.dig("$defs", "agent_identity", "properties", "human_subject", "const") == false

schema_actions = schema.dig("$defs", "recommendation", "properties", "action", "enum")
fail!("recommendation enum drifted") unless schema_actions.sort == REQUIRED_RECOMMENDATIONS.sort
schema_reasons = schema.dig("$defs", "reason_code", "enum")
REQUIRED_REASON_CODES.each do |reason|
  fail!("schema missing reason #{reason}") unless schema_reasons.include?(reason)
end
schema_forbidden = schema.dig("$defs", "forbidden_action", "enum")
fail!("schema forbidden enum drifted") unless schema_forbidden.sort == REQUIRED_FORBIDDEN.sort

fail!("fixture schema version drifted") unless fixtures["schema_version"] == "ft.agent_task_fit_passport.fixtures.v1"
fail!("fixture contract id drifted") unless fixtures["contract_id"] == "ft.agent_task_fit_passport.fixture_manifest.v1"
fail!("fixture schema pointer drifted") unless fixtures["schema_path"] == SCHEMA
fail!("fixture doc pointer drifted") unless fixtures["contract_doc"] == DOC
fail!("fixture source bead drifted") unless fixtures["source_bead"] == "ft-auy2g.9"
fail!("fixture verifier missing") unless fixtures.fetch("verification").include?("bash tests/e2e/test_agent_task_fit_passport_contract.sh")
fail!("fixture forbidden actions drifted") unless fixtures.fetch("required_forbidden_actions").sort == REQUIRED_FORBIDDEN.sort
fail!("toon columns too sparse") unless fixtures.fetch("toon_columns").length >= 6

cases = fixtures.fetch("cases")
case_ids = cases.map { |entry| entry.fetch("case_id") }
fail!("case coverage drifted: #{case_ids.sort.inspect}") unless case_ids.sort == REQUIRED_CASES.sort
fail!("case ids are not unique") unless case_ids.uniq.length == case_ids.length

recommendations_seen = Set.new
reasons_seen = Set.new
source_kinds_seen = Set.new
evidence_categories_seen = Set.new

cases.each do |entry|
  case_id = entry.fetch("case_id")
  expected_recommendation = entry.fetch("expected_recommendation")
  required_reason = entry.fetch("required_reason_code")
  artifact = entry.fetch("artifact")

  fail!("#{case_id} artifact schema version drifted") unless artifact["schema_version"] == 1
  fail!("#{case_id} artifact contract id drifted") unless artifact["contract_id"] == "ft.agent_task_fit_passport.v1"
  fail!("#{case_id} source bead drifted") unless artifact["source_bead"] == "ft-auy2g.9"
  fail!("#{case_id} dry_run drifted") unless artifact["dry_run"] == true
  fail!("#{case_id} read_only drifted") unless artifact["read_only"] == true
  fail!("#{case_id} forbidden actions drifted") unless artifact.fetch("forbidden_actions").sort == REQUIRED_FORBIDDEN.sort

  context = artifact.fetch("planner_context")
  %w[objective_id task_bead_id task_domain work_class proof_requirement mission_objective_plan_ref].each do |field|
    fail!("#{case_id} planner context missing #{field}") unless context[field]
  end
  fail!("#{case_id} does not cite mission objective plan") unless context["mission_objective_plan_ref"] == "ft.mission_objective_plan.v1"

  identity = artifact.fetch("agent_identity")
  fail!("#{case_id} human subject scoring is forbidden") unless identity.fetch("human_subject") == false
  fail!("#{case_id} raw identity evidence is forbidden") if identity.fetch("redaction_state") == "raw_forbidden"

  claimed_domains = artifact.fetch("claimed_domains")
  fail!("#{case_id} claimed_domains must be an array") unless claimed_domains.is_a?(Array)
  claimed_domains.each do |domain|
    %w[domain ownership_state source_artifact freshness_state].each do |field|
      fail!("#{case_id} claimed domain missing #{field}") unless domain[field]
    end
  end

  evidence = artifact.fetch("evidence")
  fail!("#{case_id} evidence missing") if evidence.empty?
  evidence.each do |row|
    source_kinds_seen.add(row.fetch("source_kind"))
    evidence_categories_seen.add(row.fetch("category"))
    %w[
      evidence_id source_kind category quality freshness_state redaction_state confidence
      source_artifact summary raw_pane_content_stored mail_body_stored secret_material_stored
      reason_codes
    ].each do |field|
      fail!("#{case_id} evidence #{row["evidence_id"] || "(missing)"} lacks #{field}") unless row.key?(field)
    end
    fail!("#{case_id} source artifact must point at fixtures") unless row.fetch("source_artifact") == FIXTURES
    fail!("#{case_id} raw pane content stored") unless row.fetch("raw_pane_content_stored") == false
    fail!("#{case_id} mail body stored") unless row.fetch("mail_body_stored") == false
    fail!("#{case_id} secret material stored") unless row.fetch("secret_material_stored") == false
    fail!("#{case_id} raw evidence is forbidden") if row.fetch("redaction_state") == "raw_forbidden"
  end

  fit = artifact.fetch("fit_summary")
  %w[capability availability attention reliability overall_fit confidence explanation].each do |field|
    fail!("#{case_id} fit summary missing #{field}") unless fit[field]
  end

  recommendation = artifact.fetch("recommendation")
  action = recommendation.fetch("action")
  recommendations_seen.add(action)
  fail!("#{case_id} expected #{expected_recommendation}, got #{action}") unless action == expected_recommendation
  fail!("#{case_id} assignable flag drifted") if action == "assign" && recommendation.fetch("assignable") != true
  fail!("#{case_id} non-assign must not be assignable") if action != "assign" && recommendation.fetch("assignable") != false
  fail!("#{case_id} approval flag drifted") if action == "require_operator_approval" && recommendation.fetch("requires_approval") != true
  fail!("#{case_id} approval flag should be false") if action != "require_operator_approval" && recommendation.fetch("requires_approval") != false

  reason_codes = recommendation.fetch("reason_codes")
  reason_codes.each { |reason| reasons_seen.add(reason) }
  evidence.each { |row| row.fetch("reason_codes").each { |reason| reasons_seen.add(reason) } }
  fail!("#{case_id} missing expected reason #{required_reason}") unless reason_codes.include?(required_reason) || evidence.any? { |row| row.fetch("reason_codes").include?(required_reason) }
  SAFETY_REASONS.each do |reason|
    fail!("#{case_id} missing safety reason #{reason}") unless reason_codes.include?(reason)
  end

  if action == "assign"
    fail!("#{case_id} assign requires strong overall fit") unless fit.fetch("overall_fit") == "strong"
    fail!("#{case_id} assign requires high confidence") unless fit.fetch("confidence") == "high"
    fail!("#{case_id} assign requires fresh/within-budget evidence") unless evidence.all? { |row| %w[fresh within_budget].include?(row.fetch("freshness_state")) }
    fail!("#{case_id} assign requires usable evidence") unless evidence.none? { |row| %w[weak missing contradictory stale].include?(row.fetch("quality")) }
  else
    fail!("#{case_id} non-assign must use fail-closed action") unless FAIL_CLOSED_ACTIONS.include?(action)
  end

  if reason_codes.include?("fit.poor_capability")
    fail!("#{case_id} poor capability must avoid assignment") unless action == "avoid_assignment"
  end
  if reason_codes.include?("fit.stale_evidence")
    fail!("#{case_id} stale evidence must request fresh evidence") unless action == "request_fresh_evidence"
    fail!("#{case_id} stale fixture missing stale evidence") unless evidence.any? { |row| row.fetch("freshness_state") == "stale" }
  end
  if reason_codes.include?("fit.active_owner_conflict")
    fail!("#{case_id} active owner conflict must wait") unless action == "wait_for_owner"
    fail!("#{case_id} owner conflict fixture missing conflict") unless claimed_domains.any? { |domain| domain.fetch("ownership_state") == "active_conflict" }
  end
  if reason_codes.include?("fit.missing_proof")
    fail!("#{case_id} missing proof must request fresh evidence") unless action == "request_fresh_evidence"
    fail!("#{case_id} missing proof fixture lacks missing evidence") unless evidence.any? { |row| row.fetch("quality") == "missing" }
  end
  if reason_codes.include?("fit.recent_failure")
    fail!("#{case_id} recent failure must avoid assignment") unless action == "avoid_assignment"
  end
  if reason_codes.include?("fit.agent_unavailable")
    fail!("#{case_id} unavailable agent must request fresh evidence") unless action == "request_fresh_evidence"
    fail!("#{case_id} unavailable fixture missing availability unknown") unless fit.fetch("availability") == "unknown"
  end
  if reason_codes.include?("fit.requires_approval")
    fail!("#{case_id} approval case must require approval") unless action == "require_operator_approval"
  end
  if reason_codes.include?("fit.privacy_redacted")
    fail!("#{case_id} privacy redacted must request fresh evidence") unless action == "request_fresh_evidence"
    fail!("#{case_id} privacy fixture missing privacy_redacted evidence") unless evidence.any? { |row| row.fetch("redaction_state") == "privacy_redacted" }
  end

  decay = artifact.fetch("decay_policy")
  fail!("#{case_id} old outcomes must not permanently bias") unless decay.fetch("old_outcomes_do_not_permanently_bias") == true
  fail!("#{case_id} success half-life too small") unless decay.fetch("success_half_life_hours").positive?
  fail!("#{case_id} failure half-life too small") unless decay.fetch("failure_half_life_hours").positive?

  safeguards = artifact.fetch("safeguards")
  safeguards.each do |name, value|
    fail!("#{case_id} safeguard #{name} is not true") unless value == true
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
REQUIRED_SOURCE_KINDS.each do |source_kind|
  fail!("missing source-kind fixture #{source_kind}") unless source_kinds_seen.include?(source_kind)
end
REQUIRED_EVIDENCE_CATEGORIES.each do |category|
  fail!("missing evidence-category fixture #{category}") unless evidence_categories_seen.include?(category)
end

mixed = fixtures.fetch("mixed_swarm_dry_run")
fail!("mixed swarm contract id drifted") unless mixed.fetch("contract_id") == "ft.agent_task_fit_passport.mixed_swarm_dry_run.v1"
fail!("mixed swarm dry_run drifted") unless mixed.fetch("dry_run") == true
fail!("mixed swarm read_only drifted") unless mixed.fetch("read_only") == true
fail!("mixed swarm does not cite mission objective plan") unless mixed.fetch("mission_objective_plan_ref") == "ft.mission_objective_plan.v1"
fail!("mixed swarm raw pane content stored") unless mixed.fetch("raw_pane_content_stored") == false
fail!("mixed swarm mail body stored") unless mixed.fetch("mail_body_stored") == false
fail!("mixed swarm secret material stored") unless mixed.fetch("secret_material_stored") == false
fail!("mixed swarm forbidden actions drifted") unless mixed.fetch("forbidden_actions").sort == REQUIRED_FORBIDDEN.sort

candidate_agents = mixed.fetch("candidate_agents")
fail!("mixed swarm needs at least four candidate agents") unless candidate_agents.length >= 4
candidate_case_ids = candidate_agents.map { |candidate| candidate.fetch("case_id") }
candidate_case_ids.each do |case_id|
  fail!("mixed swarm references missing case #{case_id}") unless case_ids.include?(case_id)
end
fail!("mixed swarm must select good-fit") unless mixed.fetch("selected_case_id") == "good-fit"
fail!("mixed swarm selected agent drifted") unless mixed.fetch("selected_agent_id") == "agent.fixture-emerald"
fail!("mixed swarm action must assign") unless mixed.fetch("assignment_action") == "assign"
fail!("mixed swarm ranks are not deterministic") unless candidate_agents.map { |candidate| candidate.fetch("rank") } == candidate_agents.map { |candidate| candidate.fetch("rank") }.sort
%w[assign request_fresh_evidence wait_for_owner avoid_assignment].each do |action|
  fail!("mixed swarm missing action #{action}") unless candidate_agents.any? { |candidate| candidate.fetch("recommendation") == action }
end
explanation = mixed.fetch("explanation")
%w[why_this_agent why_not_others missing_evidence safe_fallback].each do |field|
  fail!("mixed swarm explanation missing #{field}") unless explanation[field]
end
fail!("mixed swarm fallback must forbid auto reassignment") unless explanation.fetch("safe_fallback").include?("auto-reassigning")

%w[
  ft.agent_task_fit_passport.v1
  docs/json-schema/ft-agent-task-fit-passport.json
  fixtures/mission-planner/agent-task-fit-passport/cases.v1.json
  human_performance_scoreboard
  raw_pane_text_storage
  mail_body_storage
  secret_material_storage
  auto_reassignment
  request_fresh_evidence
  require_operator_approval
].each do |needle|
  fail!("doc missing #{needle}") unless doc.include?(needle)
end

puts "agent task-fit passport contract: static verifier passed (#{cases.length} cases, #{recommendations_seen.length} recommendations)"
RUBY

if rg -n --hidden --glob '!*.md' \
  '(sk-[A-Za-z0-9]{20,}|AKIA[0-9A-Z]{16}|ghp_[A-Za-z0-9]{20,}|xox[baprs]-[A-Za-z0-9-]{20,}|Bearer [A-Za-z0-9._-]{20,}|BEGIN (RSA|OPENSSH|EC) PRIVATE KEY)' \
  "${FIXTURES}" >/tmp/ft-agent-task-fit-passport-secret-scan.txt; then
  cat /tmp/ft-agent-task-fit-passport-secret-scan.txt >&2
  fail "secret-shaped strings found in agent task-fit passport fixtures"
fi
