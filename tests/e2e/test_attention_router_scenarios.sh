#!/usr/bin/env bash
# Static verifier for the attention-router scenario inventory.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

SCHEMA="docs/json-schema/ft-attention-router-scenarios.json"
INVENTORY="fixtures/attention-router/scenarios.v1.json"
BLOCKER_RADAR_CONTRACT="docs/blocker-radar-contract.md"
BLOCKER_RADAR_RUNBOOK="docs/blocker-radar-runbook.md"

fail() {
  printf 'attention-router scenario inventory: %s\n' "$*" >&2
  exit 1
}

require_file() {
  local path="$1"
  [[ -f "${path}" ]] || fail "missing file: ${path}"
}

require_file "${SCHEMA}"
require_file "${INVENTORY}"
require_file "${BLOCKER_RADAR_CONTRACT}"
require_file "${BLOCKER_RADAR_RUNBOOK}"

jq empty "${SCHEMA}" "${INVENTORY}"

jq -e '
  .["$schema"] == "https://json-schema.org/draft/2020-12/schema"
  and .["$id"] == "https://frankenterm.dev/schemas/ft-attention-router-scenarios.json"
  and .type == "object"
  and (.required | sort) == [
    "classification_vocabulary",
    "contract_id",
    "golden_strategy",
    "parent_bead",
    "producing_bead",
    "purpose",
    "scenarios",
    "schema_version",
    "source_policy"
  ]
' "${SCHEMA}" >/dev/null || fail "schema metadata is incomplete"

jq -e '
  .schema_version == 1
  and .contract_id == "ft.attention_router.scenario_inventory.v1"
  and .producing_bead == "ft-x3nsb.6.1"
  and .parent_bead == "ft-x3nsb.6"
  and (.purpose | type == "string" and length > 0)
  and (.source_policy | keys | sort) == [
    "build_cancellation_allowed",
    "destructive_actions_allowed",
    "dirty_overlap_mutation_allowed",
    "full_agent_mail_bodies_allowed",
    "local_cargo_proof_allowed",
    "raw_pane_content_allowed",
    "secret_material_allowed",
    "service_mutation_allowed",
    "worker_mutation_allowed"
  ]
  and all(.source_policy[]; . == false)
  and .golden_strategy.artifact_kind == "canonicalized_structural_golden"
  and (.golden_strategy.formats_required | sort) == ["json", "toon"]
  and (.golden_strategy.canonicalization | type == "array" and length >= 1)
  and .golden_strategy.review_required_before_freeze == true
  and (.golden_strategy.update_policy | type == "string" and length > 0)
  and (.classification_vocabulary | sort) == [
    "blocked_domain",
    "blocked_infra",
    "dirty_overlap",
    "do_not_touch",
    "proof_starved",
    "ready_now",
    "stale_claim",
    "waiting_comm"
  ]
  and ([.scenarios[].scenario_id] | contains([
    "empty-ready-bv-blocked-recommendation",
    "rch-no-admissible-worker",
    "agent-mail-ack-required",
    "stale-in-progress-candidate",
    "dirty-overlap-active-owner",
    "closed-local-not-pushed",
    "bv-stale-bd-command-hints",
    "docs-only-ready-while-proof-blocked"
  ]))
  and (.scenarios | length >= 8)
' "${INVENTORY}" >/dev/null || fail "top-level inventory contract is incomplete"

jq -e '
  def known_classification:
    . as $value
    | [
      "ready_now",
      "blocked_infra",
      "blocked_domain",
      "waiting_comm",
      "stale_claim",
      "dirty_overlap",
      "proof_starved",
      "do_not_touch"
    ]
    | index($value) != null;

  all(.scenarios[];
    (.scenario_id | type == "string" and length > 0)
    and (.title | type == "string" and length > 0)
    and (.summary | type == "string" and length > 0)
    and (.source_fixture_requirements | type == "array" and length >= 1)
    and all(.source_fixture_requirements[];
      (.source_id | type == "string" and length > 0)
      and (.command_or_api | type == "string" and length > 0)
      and (.required_reason_codes | type == "array" and length >= 1)
      and all(.required_reason_codes[]; test("^[a-z][a-z0-9_]*(\\.[a-z][a-z0-9_]*)+$"))
      and ((has("required_subjects") | not) or (.required_subjects | type == "array" and length >= 1))
      and ((has("required_counters") | not) or (.required_counters | type == "object" and length >= 1 and all(.[]; type == "number")))
    )
    and (.expected.classification | known_classification)
    and (.expected.recommended_safe_action | type == "string" and length > 0)
    and (.expected.confidence | IN("low", "medium", "high"))
    and (.expected.explanation_must_include | type == "array" and length >= 1)
    and (.forbidden_actions | type == "array" and length >= 1)
    and all(.forbidden_actions[]; type == "string" and length > 0)
    and (.volatility.level | type == "number" and . >= 1 and . <= 5)
    and (.volatility.strategy | type == "string" and length > 0)
  )
' "${INVENTORY}" >/dev/null || fail "scenario entries are incomplete"

jq -e '
  .scenarios[]
  | select(.scenario_id == "empty-ready-bv-blocked-recommendation")
  | ([.source_fixture_requirements[].command_or_api] | sort) == [
      "br ready --json",
      "br show ft-4tp7g --json",
      "bv --robot-next"
    ]
    and (.source_fixture_requirements[] | select(.source_id == "beads-ready")
      | .required_counters.ready_count == 0
      and (.required_reason_codes | index("beads.ready_empty") != null))
    and (.source_fixture_requirements[] | select(.source_id == "bv-next")
      | (.required_subjects | index("ft-4tp7g") != null)
      and (.required_reason_codes | index("bv.recommends_blocked_issue") != null))
    and (.source_fixture_requirements[] | select(.source_id == "beads-blocker-state")
      | (.required_reason_codes | index("beads.status_blocked") != null)
      and (.required_reason_codes | index("beads.assignee_present") != null))
    and .expected.classification == "blocked_infra"
    and .expected.recommended_safe_action == "do_not_claim_bv_pick_record_blocker_or_find_disjoint_static_slice"
    and (.expected.explanation_must_include | index("br state is authoritative for actionability") != null)
    and (.expected.explanation_must_include | index("bv recommendation is advisory only") != null)
    and (.expected.explanation_must_include | index("ft-4tp7g remains blocked until RCH reaches remote Cargo proof") != null)
    and (.forbidden_actions | index("claim_ft_4tp7g") != null)
' "${INVENTORY}" >/dev/null || fail "empty-ready/BV-blocked reconciliation scenario drifted"

jq -e '
  .scenarios[]
  | select(.scenario_id == "closed-local-not-pushed")
  | ([.source_fixture_requirements[].source_id] | sort) == [
      "agent-mail-owner-context",
      "beads-local-closeout",
      "git-owned-closeout-diff",
      "git-remote-closeout-state"
    ]
    and (.source_fixture_requirements[] | select(.source_id == "beads-local-closeout")
      | (.required_reason_codes | index("beads.status_closed") != null)
      and (.required_reason_codes | index("beads.close_reason_present") != null))
    and (.source_fixture_requirements[] | select(.source_id == "git-owned-closeout-diff")
      | (.required_reason_codes | index("git.tracker_dirty") != null)
      and (.required_reason_codes | index("git.owned_paths_dirty") != null))
    and (.source_fixture_requirements[] | select(.source_id == "git-remote-closeout-state")
      | .required_counters.origin_main_contains_closeout == 0
      and .required_counters.legacy_mirror_contains_closeout == 0
      and (.required_reason_codes | index("git.origin_main_missing_closeout") != null)
      and (.required_reason_codes | index("git.legacy_mirror_missing_closeout") != null))
    and (.source_fixture_requirements[] | select(.source_id == "agent-mail-owner-context")
      | (.required_reason_codes | index("agent_mail.active_owner_claim") != null)
      and (.required_reason_codes | index("reservation.owner_present") != null))
    and .expected.classification == "do_not_touch"
    and .expected.recommended_safe_action == "notify_owner_wait_for_publish_or_pick_disjoint_work"
    and (.expected.explanation_must_include | index("local tracker closed state is not durable until committed and pushed") != null)
    and (.expected.explanation_must_include | index("do not stage or commit another agent'\''s closeout") != null)
    and (.forbidden_actions | index("commit_another_agents_closeout") != null)
    and (.forbidden_actions | index("stage_unowned_tracker_changes") != null)
' "${INVENTORY}" >/dev/null || fail "closed-local/not-pushed coordination scenario drifted"

jq -e '
  .scenarios[]
  | select(.scenario_id == "bv-stale-bd-command-hints")
  | ([.source_fixture_requirements[].source_id] | sort) == [
      "beads-authoritative-show",
      "beads-ready-reconciliation",
      "bv-command-hints",
      "external-tool-defect-note"
    ]
    and (.source_fixture_requirements[] | select(.source_id == "bv-command-hints")
      | (.required_reason_codes | index("bv.stale_command_hints") != null)
      and (.required_reason_codes | index("bv.uses_legacy_bd") != null)
      and (.required_reason_codes | index("bv.claim_hint_present") != null)
      and (.required_subjects | index("bd update <candidate> --status=in_progress") != null)
      and (.required_subjects | index("bd show <candidate>") != null))
    and (.source_fixture_requirements[] | select(.source_id == "beads-authoritative-show")
      | (.required_reason_codes | index("beads.status_blocked_or_unready") != null)
      and (.required_reason_codes | index("beads.command_surface_br") != null))
    and (.source_fixture_requirements[] | select(.source_id == "beads-ready-reconciliation")
      | (.required_reason_codes | index("beads.candidate_absent_from_ready") != null))
    and (.source_fixture_requirements[] | select(.source_id == "external-tool-defect-note")
      | .command_or_api == "docs/proposals/ft-htcwc-bv-stale-availability-followup.md"
      and (.required_reason_codes | index("docs.external_tool_defect_recorded") != null)
      and (.required_reason_codes | index("docs.frankenterm_reconciler_already_fail_closed") != null))
    and .expected.classification == "do_not_touch"
    and .expected.recommended_safe_action == "ignore_bv_command_hints_use_br_json_state"
    and (.expected.explanation_must_include | index("bv command hints are advisory text, not claim authority") != null)
    and (.expected.explanation_must_include | index("bd command hints are stale in this repository") != null)
    and (.expected.explanation_must_include | index("br JSON state decides whether a candidate is claimable") != null)
    and (.forbidden_actions | index("run_bd_claim_command") != null)
    and (.forbidden_actions | index("run_bv_claim_hint_without_br_reconciliation") != null)
    and (.forbidden_actions | index("auto_claim_bv_pick") != null)
' "${INVENTORY}" >/dev/null || fail "BV stale bd command-hints scenario drifted"

grep -Fq "The claimability check must treat \`bv --robot-triage\` and \`bv --robot-next\` as" \
  "${BLOCKER_RADAR_CONTRACT}" || fail "blocker-radar contract no longer treats BV as advisory"
grep -Fq "\`br ready --json\` and \`br show <id> --json\` are" \
  "${BLOCKER_RADAR_CONTRACT}" || fail "blocker-radar contract no longer treats BR as authoritative"
grep -Fq "final verdict is \`tracker_inconsistent\` and non-claimable" \
  "${BLOCKER_RADAR_CONTRACT}" || fail "blocker-radar contract no longer fails closed on tracker inconsistency"
grep -Fq "\`no_ready\`" \
  "${BLOCKER_RADAR_CONTRACT}" || fail "blocker-radar contract no longer defines no_ready"
grep -Fq "When \`br ready --json\` is empty, fail closed." \
  "${BLOCKER_RADAR_RUNBOOK}" || fail "blocker-radar runbook no longer fails closed on empty BR ready"
grep -Fq "Read \`bv --robot-triage\` only as an advisory ranking snapshot." \
  "${BLOCKER_RADAR_RUNBOOK}" || fail "blocker-radar runbook no longer frames BV as advisory"
grep -Fq "PageRank, unblock count, and \"available for work\" language are never enough" \
  "${BLOCKER_RADAR_RUNBOOK}" || fail "blocker-radar runbook no longer rejects BV-only claims"

scenario_count="$(jq -r '.scenarios | length' "${INVENTORY}")"
printf 'attention-router scenario inventory: static verifier passed (%s scenarios)\n' "${scenario_count}"
