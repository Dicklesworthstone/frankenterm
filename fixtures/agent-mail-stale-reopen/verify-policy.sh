#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
json_path="${1:-"$script_dir/policy.v1.json"}"

if [[ ! -f "$json_path" ]]; then
  echo "missing stale-reopen policy inventory: $json_path" >&2
  exit 1
fi

jq -e '
  .schema_version == 1
  and .contract_id == "ft.agent_mail_stale_reopen_policy.v1"
  and .bead == "ft-5lsqo.3"
  and .source_epic == "ft-5lsqo"
  and .source_snapshot_contract == "ft.agent_mail_failover_snapshot.v1"
  and .proof_kind == "static-policy"
  and .runtime_command_shipped == false
  and .default_stale_threshold_seconds == 7200
  and .default_action == "do_not_reopen"
' "$json_path" >/dev/null

jq -e '
  .side_effect_policy
  | to_entries
  | all(.value == false)
' "$json_path" >/dev/null

rule_ids=(
  RULE-EMPTY-IN-PROGRESS
  RULE-ACTIVE-NOT-STALE
  RULE-CLEAN-STALE
  RULE-DIRTY-TRACKED
  RULE-UNTRACKED-REVIEW
  RULE-ASSIGNEE-RECENCY
  RULE-BR-EMPTY-BV-BLOCKED
)

fixture_ids=(
  CASE-EMPTY-IN-PROGRESS
  CASE-ACTIVE-IN-PROGRESS
  CASE-CLEAN-STALE
  CASE-DIRTY-TRACKED-OVERLAP
  CASE-UNTRACKED-REVIEW-REQUIRED
  CASE-BR-EMPTY-BV-BLOCKED
)

for rule_id in "${rule_ids[@]}"; do
  jq -e --arg id "$rule_id" '
    any(.rules[]; .id == $id and .level == "MUST")
  ' "$json_path" >/dev/null
done

for fixture_id in "${fixture_ids[@]}"; do
  jq -e --arg id "$fixture_id" '
    any(.fixtures[]; .id == $id)
  ' "$json_path" >/dev/null
done

jq -e --argjson expected_count "${#rule_ids[@]}" '
  (.rules | length) == $expected_count
  and .coverage.rule_count == $expected_count
  and .coverage.must_rule_count == $expected_count
' "$json_path" >/dev/null

jq -e --argjson expected_count "${#fixture_ids[@]}" '
  (.fixtures | length) == $expected_count
  and .coverage.fixture_count == $expected_count
' "$json_path" >/dev/null

jq -e '
  ([.rules[].id] | unique) as $rules
  | [.fixtures[].covers[]? | select(($rules | index(.)) | not)]
  | length == 0
' "$json_path" >/dev/null

jq -e '
  ([.fixtures[].covers[]?] | unique) as $covered
  | [
      .rules[]
      | select(.level == "MUST")
      | .id as $id
      | select(($covered | index($id)) | not)
    ]
  | length == 0
' "$json_path" >/dev/null

jq -e '
  any(.overlap_categories[]; .id == "tracked_overlap_risk" and .default_action == "do_not_reopen" and .severity == "high")
  and any(.overlap_categories[]; .id == "untracked_review_required" and .default_action == "do_not_reopen")
  and any(.overlap_categories[]; .id == "tracker_only_mismatch" and .default_action == "verify_br_show_then_do_not_claim")
' "$json_path" >/dev/null

jq -e '
  any(.fixtures[]; .id == "CASE-CLEAN-STALE" and .expected_action == "comment_status_check" and .status_check_comment_required == true and .immediate_reopen_allowed == false)
  and any(.fixtures[]; .id == "CASE-DIRTY-TRACKED-OVERLAP" and .expected_action == "do_not_reopen" and .immediate_reopen_allowed == false)
  and any(.fixtures[]; .id == "CASE-UNTRACKED-REVIEW-REQUIRED" and .expected_action == "do_not_reopen" and .immediate_reopen_allowed == false)
  and any(.fixtures[]; .id == "CASE-ACTIVE-IN-PROGRESS" and .expected_action == "wait_for_owner")
  and any(.fixtures[]; .id == "CASE-BR-EMPTY-BV-BLOCKED" and .expected_action == "verify_br_show_then_do_not_claim")
' "$json_path" >/dev/null

jq -e '
  all(.fixtures[]; .immediate_reopen_allowed == false)
  and ([
    .fixtures[]
    | select(.expected_action == "comment_status_check")
    | select(.status_check_comment_required != true)
  ] | length == 0)
  and ([
    .fixtures[]
    | select(.expected_action != "comment_status_check")
    | select(.status_check_comment_required != false)
  ] | length == 0)
' "$json_path" >/dev/null

jq -e '
  all(.fixtures[]; (.required_evidence | length) > 0)
  and all(.rules[]; (.required_evidence | length) > 0)
  and any(.rules[]; .id == "RULE-CLEAN-STALE" and (.required_comment_template | length) > 0)
' "$json_path" >/dev/null

echo "agent-mail stale-reopen policy inventory: ok"
