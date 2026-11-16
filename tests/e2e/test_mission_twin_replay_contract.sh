#!/usr/bin/env bash
# Static verifier for the mission-twin replay golden corpus.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

REPLAY_MODULE="crates/frankenterm-core/src/mission_twin_replay.rs"
REPLAY_TEST="crates/frankenterm-core/tests/mission_twin_replay_golden_corpus.rs"
ROBOT_CONTRACT_MODULE="crates/frankenterm-core/src/robot_family_contract.rs"
MANIFEST="fixtures/mission-twin/replay/manifest.json"
REPLAY_LOG="fixtures/mission-twin/replay/trace/step-log.v1.jsonl"
REPLAY_INVALID="fixtures/mission-twin/replay/invalid/fragments.v1.json"
PLAN_SCHEMA="docs/json-schema/ft-mission-objective-plan.json"
SNAPSHOT_DIR="fixtures/mission-twin/snapshot/valid"
REQUIRED_CASES=(
  "active-owner"
  "agent-mail-red"
  "dirty-overlap"
  "healthy"
  "no-ready-work"
  "rch-critical-pressure-5"
)
REQUIRED_SCRUB_FIELDS=(
  "elapsed_ms"
  "generated_at_ms"
  "machine_id"
  "worker_id"
  "workspace_root"
)
REQUIRED_REASON_CODES=(
  "agent_mail.red"
  "beads.dependency_blocked"
  "beads.owner_active"
  "dirty_overlap.owned_surface_blocked"
  "mission_twin.no_ready_work"
  "mission_twin.owner_handoff_required"
  "rch.critical_pressure"
  "rch.proof_substrate_blocked"
)
REQUIRED_COUNTERFACTUAL_CASES=(
  "agent-mail-recovered"
  "dirty-overlap-cleared"
  "owner-handoff-accepted"
  "rch-recovered-with-proof-budget"
)
REQUIRED_COUNTERFACTUAL_TOGGLES=(
  "agent_mail_recovered"
  "dirty_overlap_cleared"
  "owner_handoff_accepted"
  "proof_lanes_budgeted"
  "rch_recovered"
)
REQUIRED_OWNERSHIP_CASES=(
  "active-owner-handoff-required"
  "dirty-overlap-unsafe-overlap"
)
REQUIRED_SURFACE_ACTIONS=(
  "current_plan"
  "explain_reason"
  "explain_step"
  "simulate"
)
REQUIRED_SURFACE_CASES=(
  "current-plan-healthy"
  "explain-reason-dirty-overlap"
  "explain-step-active-owner"
  "simulate-dirty-overlap-with-ownership"
)
REQUIRED_REPLAY_INVALID_CASES=(
  "ambiguous-artifact-path"
  "destructive-suggestion"
  "live-mutation-attempt"
  "raw-pane-text"
  "stale-timestamp"
)

fail() {
  printf '{"event":"mission_twin_replay.error","status":"fail","message":%s}\n' "$(ruby -rjson -e 'print JSON.generate(ARGV.fetch(0))' "$*")" >&2
  exit 1
}

emit() {
  ruby -rjson -e 'event = ARGV.shift; fields = Hash[ARGV.map { |pair| pair.split("=", 2) }]; puts JSON.generate({ "event" => event }.merge(fields))' "$@"
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "missing command: $1"
}

require_file() {
  [[ -f "$1" ]] || fail "missing file: $1"
}

require_repo_relative_path() {
  local path="$1"

  [[ -n "${path}" ]] || fail "empty path"
  [[ "${path}" != /* ]] || fail "absolute path is forbidden: ${path}"
  [[ "${path}" != "." ]] || fail "bare dot path is forbidden: ${path}"
  [[ "${path}" != ".." ]] || fail "parent-relative path is forbidden: ${path}"
  [[ "${path}" != ./* ]] || fail "dot-prefixed path is forbidden: ${path}"
  [[ "${path}" != ../* ]] || fail "parent-relative path is forbidden: ${path}"
  [[ "${path}" != */../* ]] || fail "parent-relative path is forbidden: ${path}"
  [[ "${path}" != */./* ]] || fail "embedded dot segment is forbidden: ${path}"
  [[ "${path}" != */.. ]] || fail "trailing parent-relative segment is forbidden: ${path}"
  [[ "${path}" != */. ]] || fail "trailing dot segment is forbidden: ${path}"
  [[ "${path}" != ".git" ]] || fail ".git path is forbidden: ${path}"
  [[ "${path}" != .git/* ]] || fail ".git path is forbidden: ${path}"
  [[ "${path}" != */.git/* ]] || fail ".git path is forbidden: ${path}"
}

sha256_file() {
  shasum -a 256 "$1" | awk '{print $1}'
}

require_command jq
require_command rg
require_command ruby
require_command shasum

require_file "${REPLAY_MODULE}"
require_file "${REPLAY_TEST}"
require_file "${ROBOT_CONTRACT_MODULE}"
require_file "${MANIFEST}"
require_file "${REPLAY_LOG}"
require_file "${REPLAY_INVALID}"
require_file "${PLAN_SCHEMA}"

mapfile -t manifest_snapshot_paths < <(jq -r '.cases[].snapshot_path' "${MANIFEST}" | sort)
mapfile -t manifest_expected_plan_paths < <(jq -r '.cases[].expected_plan_path' "${MANIFEST}" | sort)
mapfile -t actual_snapshot_paths < <(find "${SNAPSHOT_DIR}" -type f -name '*.json' | sort)

for path in "${MANIFEST}" "${REPLAY_LOG}" "${REPLAY_INVALID}" "${PLAN_SCHEMA}" \
  "${manifest_snapshot_paths[@]}" "${manifest_expected_plan_paths[@]}"; do
  require_repo_relative_path "${path}"
  require_file "${path}"
done

jq empty "${MANIFEST}" "${REPLAY_INVALID}" "${PLAN_SCHEMA}" \
  "${manifest_snapshot_paths[@]}" "${manifest_expected_plan_paths[@]}"
jq -s 'length > 0' "${REPLAY_LOG}" >/dev/null || fail "replay step log has no entries"

diff -u <(printf '%s\n' "${manifest_snapshot_paths[@]}") <(printf '%s\n' "${actual_snapshot_paths[@]}") \
  >/dev/null || fail "manifest snapshot paths do not match ${SNAPSHOT_DIR}/*.json"

jq -e --argjson required "$(printf '%s\n' "${REQUIRED_CASES[@]}" | jq -R . | jq -s .)" '
  def repo_relative_path_ok:
    type == "string"
    and length > 0
    and . != "."
    and . != ".."
    and (startswith("/") | not)
    and (startswith("./") | not)
    and (startswith("../") | not)
    and (contains("/../") | not)
    and (contains("/./") | not)
    and (endswith("/..") | not)
    and (endswith("/.") | not)
    and . != ".git"
    and (startswith(".git/") | not)
    and (contains("/.git/") | not);
  def snapshot_path_ok:
    repo_relative_path_ok
    and startswith("fixtures/mission-twin/snapshot/valid/")
    and endswith(".json");

  .schema_version == 1
  and .contract_id == "ft.mission_twin_replay.golden_corpus.v1"
  and .source_bead == "ft-u7r37.2"
  and .corpus_source_bead == "ft-u7r37.6"
  and .planner_contract_id == "ft.mission_objective_plan.v1"
  and .expected_plan_contract_id == "ft.mission_twin_replay.expected_plan.v1"
  and .counterfactual_contract_id == "ft.mission_twin_counterfactual_replay.v1"
  and .counterfactual_source_bead == "ft-u7r37.3"
  and .ownership_contract_id == "ft.mission_twin_ownership_handoff.v1"
  and .ownership_source_bead == "ft-u7r37.4"
  and .surface_contract_id == "ft.mission_twin.robot_mcp_cli_surface.v1"
  and .surface_source_bead == "ft-u7r37.5"
  and .step_log_contract_id == "ft.mission_twin_replay.step_log.v1"
  and .step_log_path == "fixtures/mission-twin/replay/trace/step-log.v1.jsonl"
  and .invalid_fragments_contract_id == "ft.mission_twin_replay.invalid_fragments.v1"
  and .invalid_fragments_path == "fixtures/mission-twin/replay/invalid/fragments.v1.json"
  and .static_verification_command == "bash tests/e2e/test_mission_twin_replay_contract.sh"
  and .rust_verification_filter == "cargo test -p frankenterm-core --test mission_twin_replay_golden_corpus -- --nocapture"
  and (.remote_rust_verification_command | contains("RCH_REQUIRE_REMOTE=1"))
  and (.remote_rust_verification_command | contains("rch --no-self-healing exec --"))
  and ([.cases[].case_id] | sort) == ($required | sort)
  and (.scrub_rules | type == "array" and length >= 5)
  and all(.scrub_rules[];
    (.field | type == "string" and length > 0)
    and (.replacement | type == "string" and length > 0)
    and (.reason | type == "string" and length > 0)
  )
  and all(.cases[];
    (.case_id | type == "string" and length > 0)
    and (.snapshot_path | snapshot_path_ok)
    and (.snapshot_sha256 | test("^[0-9a-f]{64}$"))
    and (.expected_plan_path | repo_relative_path_ok)
    and (.expected_plan_path | startswith("fixtures/mission-twin/replay/expected/"))
    and (.expected_plan_path | endswith(".plan.v1.json"))
    and (.expected.plan_status | IN(
      "actionable",
      "degraded",
      "dirty_overlap",
      "no_ready_work",
      "rch_substrate_blocked",
      "waiting_owner"
    ))
    and (.expected.risk_level | IN("low", "medium", "high", "blocked"))
    and (.expected.top_step_candidate_id | type == "string" and length > 0)
    and (.expected.top_step_action_kind | IN(
      "choose_ready_bead",
      "file_followup_bead",
      "run_bv_robot_triage",
      "wait_for_owner"
    ))
    and (.expected.top_step_status | IN(
      "actionable",
      "dirty_overlap",
      "no_ready_work",
      "rch_substrate_blocked",
      "waiting_owner"
    ))
    and (.expected.top_step_proof_lane | IN("blocked", "not_required"))
    and (.expected.reason_codes_include | type == "array" and length >= 3)
    and all(.expected.reason_codes_include[]; type == "string" and length > 0)
  )
' "${MANIFEST}" >/dev/null || fail "manifest metadata or case entries are incomplete"

jq -e \
  --argjson required_cases "$(printf '%s\n' "${REQUIRED_COUNTERFACTUAL_CASES[@]}" | jq -R . | jq -s .)" \
  --argjson required_toggles "$(printf '%s\n' "${REQUIRED_COUNTERFACTUAL_TOGGLES[@]}" | jq -R . | jq -s .)" '
  . as $root
  |
  (.counterfactual_cases | type == "array" and length == ($required_cases | length))
  and ([.counterfactual_cases[].case_id] | sort) == ($required_cases | sort)
  and (
    [.counterfactual_cases[].request.toggles[]] | unique | sort
  ) == ($required_toggles | sort)
  and all(.counterfactual_cases[];
    . as $cf
    | (.case_id | type == "string" and length > 0)
    and (.base_case_id | type == "string" and length > 0)
    and any($root.cases[].case_id; . == $cf.base_case_id)
    and (.request.scenario_id == .case_id)
    and (.request.toggles | type == "array" and length > 0)
    and all(.request.toggles[]; IN(
      "rch_recovered",
      "agent_mail_recovered",
      "dirty_overlap_cleared",
      "owner_handoff_accepted",
      "target_class_proof_available",
      "proof_lanes_budgeted"
    ))
    and (
      ((.request.toggles | index("proof_lanes_budgeted")) == null and (.request.proof_lane_budget | not))
      or
      ((.request.toggles | index("proof_lanes_budgeted")) != null
        and (.request.proof_lane_budget.remote_cargo_lanes | type == "number")
        and (.request.proof_lane_budget.static_verifier_lanes | type == "number")
        and ((.request.proof_lane_budget.remote_cargo_lanes + .request.proof_lane_budget.static_verifier_lanes) > 0))
    )
    and (.expected.live_plan_status | IN(
      "actionable",
      "degraded",
      "dirty_overlap",
      "no_ready_work",
      "rch_substrate_blocked",
      "waiting_owner"
    ))
    and (.expected.simulated_plan_status | IN("actionable", "degraded", "dirty_overlap", "waiting_owner"))
    and (.expected.top_lane_class | IN(
      "remote_cargo",
      "static_verifier",
      "coordination_only",
      "waiting_owner",
      "waiting_rch",
      "not_required"
    ))
    and (.expected.live_blockers_include | type == "array" and length > 0)
    and (.expected.unblocked_reason_codes_include | type == "array" and length > 0)
  )
' "${MANIFEST}" >/dev/null || fail "manifest counterfactual cases are incomplete"

jq -e \
  --argjson required_cases "$(printf '%s\n' "${REQUIRED_OWNERSHIP_CASES[@]}" | jq -R . | jq -s .)" '
  def repo_relative_path_ok:
    type == "string"
    and length > 0
    and . != "."
    and . != ".."
    and (startswith("/") | not)
    and (startswith("./") | not)
    and (startswith("../") | not)
    and (contains("/../") | not)
    and (contains("/./") | not)
    and (endswith("/..") | not)
    and (endswith("/.") | not)
    and . != ".git"
    and (startswith(".git/") | not)
    and (contains("/.git/") | not);

  . as $root
  |
  (.ownership_cases | type == "array" and length == ($required_cases | length))
  and ([.ownership_cases[].case_id] | sort) == ($required_cases | sort)
  and all(.ownership_cases[];
    . as $case
    | (.case_id | type == "string" and length > 0)
    and (.base_case_id | type == "string" and length > 0)
    and any($root.cases[].case_id; . == $case.base_case_id)
    and (.request.candidate_id | type == "string" and length > 0)
    and (.request.target_bead_id | type == "string" and length > 0)
    and (.request.owned_paths | type == "array" and length > 0)
    and all(.request.owned_paths[]; repo_relative_path_ok)
    and (.request.stale_after_seconds | type == "number" and . >= 60)
    and (.request.fallback_only_coordination | type == "boolean")
    and (.expected.handoff_state | IN(
      "active",
      "stale_check_needed",
      "handoff_required",
      "safe_to_open",
      "unsafe_overlap"
    ))
    and (.expected.dirty_overlap_count | type == "number")
    and (.expected.reservation_overlap_count | type == "number")
    and (.expected.owner_count | type == "number")
    and (.expected.next_actions_include | type == "array" and length > 0)
    and all(.expected.next_actions_include[]; IN(
      "wait",
      "comment",
      "ask_owner",
      "choose_planning_only_work",
      "run_static_only_verifier"
    ))
    and (.expected.reason_codes_include | type == "array" and length >= 3)
    and all(.expected.reason_codes_include[]; type == "string" and length > 0)
  )
' "${MANIFEST}" >/dev/null || fail "manifest ownership cases are incomplete"

jq -e \
  --argjson required_actions "$(printf '%s\n' "${REQUIRED_SURFACE_ACTIONS[@]}" | jq -R . | jq -s .)" \
  --argjson required_cases "$(printf '%s\n' "${REQUIRED_SURFACE_CASES[@]}" | jq -R . | jq -s .)" '
  def repo_relative_path_ok:
    type == "string"
    and length > 0
    and . != "."
    and . != ".."
    and (startswith("/") | not)
    and (startswith("./") | not)
    and (startswith("../") | not)
    and (contains("/../") | not)
    and (contains("/./") | not)
    and (endswith("/..") | not)
    and (endswith("/.") | not)
    and . != ".git"
    and (startswith(".git/") | not)
    and (contains("/.git/") | not);

  . as $root
  |
  (.surface_actions | type == "array" and length == ($required_actions | length))
  and ([.surface_actions[].action] | sort) == ($required_actions | sort)
  and all(.surface_actions[];
    (.action | IN("current_plan", "simulate", "explain_step", "explain_reason"))
    and (.robot_command | type == "string" and startswith("robot mission-twin "))
    and (.cli_command | type == "string" and startswith("mission-twin "))
    and (.mcp_tool_name | type == "string" and startswith("ft.mission_twin."))
    and (.mcp_resource_uri | type == "string" and startswith("ft://mission-twin/"))
    and .response_payload == "MissionTwinSurfaceReport"
    and .read_only == true
    and .idempotent == true
  )
  and (.surface_cases | type == "array" and length == ($required_cases | length))
  and ([.surface_cases[].case_id] | sort) == ($required_cases | sort)
  and all(.surface_cases[];
    . as $case
    | (.case_id | type == "string" and length > 0)
    and (.base_case_id | type == "string" and length > 0)
    and any($root.cases[].case_id; . == $case.base_case_id)
    and (.request.action | IN("current_plan", "simulate", "explain_step", "explain_reason"))
    and (.request.snapshot_paths | type == "array" and length > 0)
    and all(.request.snapshot_paths[]; repo_relative_path_ok)
    and (
      (.request.action != "explain_step")
      or (.request.explain_step | type == "string" and length > 0)
    )
    and (
      (.request.action != "explain_reason")
      or (.request.explain_reason | type == "string" and length > 0)
    )
    and (
      (.request.action != "simulate")
      or ((.request.counterfactual_requests | type == "array" and length > 0)
        or (.request.ownership_request | type == "object"))
    )
    and (.expected.simulated | type == "boolean")
    and ((.expected.explain_mode == null) or (.expected.explain_mode | IN("step", "reason")))
    and (.expected.explain_matched | type == "boolean")
    and (.expected.counterfactual_report | type == "boolean")
    and (.expected.ownership_report | type == "boolean")
    and (.expected.reason_codes_include | type == "array" and length >= 4)
    and all(.expected.reason_codes_include[]; type == "string" and length > 0)
  )
' "${MANIFEST}" >/dev/null || fail "manifest surface actions or cases are incomplete"

for field in "${REQUIRED_SCRUB_FIELDS[@]}"; do
  jq -e --arg field "${field}" '
    any(.scrub_rules[];
      .field == $field
      and (.replacement | type == "string" and length > 0)
      and (.reason | type == "string" and length > 0)
    )
  ' "${MANIFEST}" >/dev/null || fail "manifest missing scrub rule for ${field}"
done

for reason_code in "${REQUIRED_REASON_CODES[@]}"; do
  jq -e --arg reason_code "${reason_code}" '
    any(.cases[].expected.reason_codes_include[]; . == $reason_code)
  ' "${MANIFEST}" >/dev/null || fail "manifest missing expected replay reason code ${reason_code}"
done

while IFS=$'\t' read -r case_id snapshot_path fixture_sha256; do
  require_repo_relative_path "${snapshot_path}"
  require_file "${snapshot_path}"
  actual_sha256="$(sha256_file "${snapshot_path}")"
  [[ "${actual_sha256}" == "${fixture_sha256}" ]] \
    || fail "snapshot hash drift for ${case_id}: expected ${fixture_sha256}, got ${actual_sha256}"
done < <(jq -r '.cases[] | [.case_id, .snapshot_path, .snapshot_sha256] | @tsv' "${MANIFEST}")

while IFS=$'\t' read -r case_id snapshot_path expected_plan_path plan_status risk_level \
  candidate_id action_kind step_status proof_lane; do
  reasons_json="$(jq -c --arg case_id "${case_id}" \
    '.cases[] | select(.case_id == $case_id) | .expected.reason_codes_include' "${MANIFEST}")"

  jq -e \
    --arg case_id "${case_id}" \
    --arg snapshot_path "${snapshot_path}" \
    --arg plan_status "${plan_status}" \
    --arg risk_level "${risk_level}" \
    --arg candidate_id "${candidate_id}" \
    --arg action_kind "${action_kind}" \
    --arg step_status "${step_status}" \
    --arg proof_lane "${proof_lane}" \
    --argjson reasons "${reasons_json}" '
      .schema_version == 1
      and .contract_id == "ft.mission_twin_replay.expected_plan.v1"
      and .source_bead == "ft-u7r37.6"
      and .case_id == $case_id
      and .snapshot_path == $snapshot_path
      and .plan.contract_id == "ft.mission_objective_plan.v1"
      and .plan.plan_status == $plan_status
      and .plan.risk_level == $risk_level
      and .plan.top_step.candidate_id == $candidate_id
      and .plan.top_step.action_kind == $action_kind
      and .plan.top_step.status == $step_status
      and .plan.top_step.proof_lane == $proof_lane
      and (.plan.reason_codes_include | sort) == ($reasons | sort)
      and .safety.dry_run == true
      and .safety.side_effects_executed == false
      and .safety.raw_pane_content_stored == false
    ' "${expected_plan_path}" >/dev/null \
    || fail "expected plan artifact mismatch for ${case_id}"
done < <(jq -r '.cases[] | [
  .case_id,
  .snapshot_path,
  .expected_plan_path,
  .expected.plan_status,
  .expected.risk_level,
  .expected.top_step_candidate_id,
  .expected.top_step_action_kind,
  .expected.top_step_status,
  .expected.top_step_proof_lane
] | @tsv' "${MANIFEST}")

while IFS=$'\t' read -r case_id snapshot_path; do
  require_repo_relative_path "${snapshot_path}"
  require_file "${snapshot_path}"
done < <(jq -r '.surface_cases[] | .case_id as $case_id | .request.snapshot_paths[] | [$case_id, .] | @tsv' "${MANIFEST}")

jq -s -e \
  --argjson required_cases "$(printf '%s\n' "${REQUIRED_CASES[@]}" | jq -R . | jq -s .)" '
    length == ($required_cases | length)
    and ([.[].case_id] | sort) == ($required_cases | sort)
    and all(.[];
      .schema_version == 1
      and .contract_id == "ft.mission_twin_replay.step_log.v1"
      and .source_bead == "ft-u7r37.6"
      and (.snapshot_path | type == "string" and startswith("fixtures/mission-twin/snapshot/valid/"))
      and .step_index == 0
      and .step_kind == "top_plan_step"
      and (.candidate_id | type == "string" and length > 0)
      and (.action_kind | IN("choose_ready_bead", "file_followup_bead", "run_bv_robot_triage", "wait_for_owner"))
      and (.status | IN("actionable", "dirty_overlap", "no_ready_work", "rch_substrate_blocked", "waiting_owner"))
      and (.proof_lane | IN("blocked", "not_required"))
      and (.plan_status | IN("actionable", "degraded", "dirty_overlap", "no_ready_work", "rch_substrate_blocked", "waiting_owner"))
      and (.risk_level | IN("low", "medium", "high", "blocked"))
      and .dry_run == true
      and .side_effects_executed == false
      and .raw_pane_content_stored == false
      and (.reason_codes | type == "array" and length >= 3)
    )
  ' "${REPLAY_LOG}" >/dev/null || fail "replay step log contract mismatch"

while IFS=$'\t' read -r case_id snapshot_path plan_status risk_level candidate_id action_kind step_status proof_lane; do
  reasons_json="$(jq -c --arg case_id "${case_id}" \
    '.cases[] | select(.case_id == $case_id) | .expected.reason_codes_include' "${MANIFEST}")"

  jq -s -e \
    --arg case_id "${case_id}" \
    --arg snapshot_path "${snapshot_path}" \
    --arg plan_status "${plan_status}" \
    --arg risk_level "${risk_level}" \
    --arg candidate_id "${candidate_id}" \
    --arg action_kind "${action_kind}" \
    --arg step_status "${step_status}" \
    --arg proof_lane "${proof_lane}" \
    --argjson reasons "${reasons_json}" '
      any(.[];
        .case_id == $case_id
        and .snapshot_path == $snapshot_path
        and .plan_status == $plan_status
        and .risk_level == $risk_level
        and .candidate_id == $candidate_id
        and .action_kind == $action_kind
        and .status == $step_status
        and .proof_lane == $proof_lane
        and (.reason_codes | sort) == ($reasons | sort)
      )
    ' "${REPLAY_LOG}" >/dev/null || fail "replay log row mismatch for ${case_id}"
done < <(jq -r '.cases[] | [
  .case_id,
  .snapshot_path,
  .expected.plan_status,
  .expected.risk_level,
  .expected.top_step_candidate_id,
  .expected.top_step_action_kind,
  .expected.top_step_status,
  .expected.top_step_proof_lane
] | @tsv' "${MANIFEST}")

jq -e \
  --arg manifest "${MANIFEST}" \
  --argjson required_cases "$(printf '%s\n' "${REQUIRED_REPLAY_INVALID_CASES[@]}" | jq -R . | jq -s .)" '
    . as $root
    | $root.schema_version == 1
    and $root.contract_id == "ft.mission_twin_replay.invalid_fragments.v1"
    and $root.source_bead == "ft-u7r37.6"
    and $root.manifest_path == $manifest
    and ([$root.cases[].case_id] | sort) == ($required_cases | sort)
    and ([$root.cases[].case_id] | length) == ([$root.cases[].case_id] | unique | length)
    and all($root.cases[];
      (.description | type == "string" and length > 0)
      and (.reason_codes | type == "array" and length > 0)
      and all(.reason_codes[]; startswith("mission_twin_replay.invalid."))
      and (.invalid_fragment | type == "object")
    )
    and any($root.cases[]; .case_id == "raw-pane-text"
      and .invalid_fragment.raw_pane_content_stored == true
      and (.invalid_fragment.sources.beads.raw_pane_text | type == "string" and length > 0))
    and any($root.cases[]; .case_id == "destructive-suggestion"
      and (.invalid_fragment.suggested_action | contains("delete tracked source files"))
      and (.invalid_fragment.next_actions | index("mutate_workspace") != null))
    and any($root.cases[]; .case_id == "ambiguous-artifact-path"
      and any(.invalid_fragment.artifact_paths[]; . == "fixtures/mission-twin/replay/*")
      and any(.invalid_fragment.artifact_paths[]; startswith("/"))
      and any(.invalid_fragment.artifact_paths[]; startswith("../")))
    and any($root.cases[]; .case_id == "stale-timestamp"
      and .invalid_fragment.generated_at_ms == 1
      and .invalid_fragment.freshness_state == "fresh")
    and any($root.cases[]; .case_id == "live-mutation-attempt"
      and .invalid_fragment.live_attempt == true
      and .invalid_fragment.dry_run == false
      and .invalid_fragment.side_effects_executed == true)
  ' "${REPLAY_INVALID}" >/dev/null || fail "replay invalid fixture coverage mismatch"

for needle in \
  "MissionTwinSnapshotEnvelope" \
  "MissionObjectivePlannerInput" \
  "plan_mission_objective" \
  "build_mission_twin_replay_surface_data" \
  "build_mission_twin_surface_report" \
  "mission_twin_surface_action_contracts" \
  "MissionTwinSurfaceReport" \
  "simulate_mission_twin_counterfactuals" \
  "simulate_mission_twin_ownership_handoff" \
  "classify_mission_twin_owned_path_overlap" \
  "MissionTwinCounterfactualToggle" \
  "MissionTwinProofLaneClass" \
  "MissionTwinOwnershipHandoffState" \
  "mission_twin_family_contract" \
  "MissionTwinReplayError::EmptySnapshotSet" \
  "ExpectedPlanArtifact" \
  "ReplayStepLogEntry" \
  "load_replay_step_logs" \
  "invalid_fragments_path" \
  "side-effect-free"; do
  rg -q "${needle}" "${REPLAY_MODULE}" "${REPLAY_TEST}" "${ROBOT_CONTRACT_MODULE}" \
    || fail "replay source/test missing required text: ${needle}"
done

if rg -n 'std::process|Command::new|remove_file|remove_dir|am service|doctor fix|doctor repair|git reset|git clean|kill ' "${REPLAY_MODULE}" "${REPLAY_TEST}"; then
  fail "replay module contains forbidden mutating command surface"
fi

jq -c '.' "${REPLAY_LOG}"
emit "mission_twin_replay.manifest" "status=ok" "cases=${#REQUIRED_CASES[@]}" "ownership_cases=${#REQUIRED_OWNERSHIP_CASES[@]}" "surface_cases=${#REQUIRED_SURFACE_CASES[@]}" "path=${MANIFEST}"
