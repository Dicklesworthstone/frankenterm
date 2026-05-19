#!/usr/bin/env bash
# Static verifier for the attention-router nudge-plan receipt inventory.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

SCHEMA="docs/json-schema/ft-attention-router-nudge-plan-receipt.json"
FIXTURE="fixtures/attention-router/nudge-plan-receipts.v1.json"

fail() {
  printf 'attention-router nudge-plan receipts: %s\n' "$*" >&2
  exit 1
}

require_file() {
  local path="$1"
  [[ -f "${path}" ]] || fail "missing file: ${path}"
}

require_file "${SCHEMA}"
require_file "${FIXTURE}"

jq empty "${SCHEMA}" "${FIXTURE}"

jq -e '
  .["$schema"] == "https://json-schema.org/draft/2020-12/schema"
  and .["$id"] == "https://frankenterm.dev/schemas/ft-attention-router-nudge-plan-receipt.json"
  and .type == "object"
  and (.required | sort) == [
    "action_vocabulary",
    "contract_id",
    "parent_bead",
    "producing_bead",
    "purpose",
    "receipts",
    "schema_version",
    "side_effect_policy"
  ]
' "${SCHEMA}" >/dev/null || fail "schema metadata is incomplete"

jq -e '
  .schema_version == 1
  and .contract_id == "ft.attention_router.nudge_plan_receipts.v1"
  and .producing_bead == "ft-x3nsb.5.1"
  and .parent_bead == "ft-x3nsb.5"
  and (.purpose | type == "string" and length > 0)
  and (.side_effect_policy | keys | sort) == [
    "auto_acknowledge_allowed",
    "auto_comment_beads_allowed",
    "auto_force_release_allowed",
    "auto_release_reservations_allowed",
    "auto_send_mail_allowed",
    "build_cancellation_allowed",
    "destructive_actions_allowed",
    "dirty_overlap_mutation_allowed",
    "local_cargo_proof_allowed",
    "service_mutation_allowed",
    "worker_mutation_allowed"
  ]
  and all(.side_effect_policy[]; . == false)
  and (.action_vocabulary | sort) == [
    "acknowledge_request",
    "force_release_review",
    "handoff_request",
    "no_action",
    "reply_to_thread",
    "status_check"
  ]
  and ([.receipts[].receipt_id] | contains([
    "ack-required-direct-request",
    "stale-claim-status-check",
    "dirty-overlap-handoff",
    "force-release-review-only",
    "proof-starved-no-action"
  ]))
  and (.receipts | length >= 5)
' "${FIXTURE}" >/dev/null || fail "top-level receipt inventory is incomplete"

jq -e '
  def known_classification:
    . as $value
    | ["blocked_infra", "proof_starved", "waiting_comm", "stale_claim", "dirty_overlap", "do_not_touch"]
    | index($value) != null;

  def known_kind:
    . as $value
    | [
      "acknowledge_request",
      "reply_to_thread",
      "status_check",
      "handoff_request",
      "force_release_review",
      "no_action"
    ]
    | index($value) != null;

  all(.receipts[];
    (.receipt_id | type == "string" and test("^[a-z][a-z0-9]*(?:[-_][a-z0-9]+)*$"))
    and (.scenario_id | type == "string" and length > 0)
    and (.trigger_classification | known_classification)
    and (.target.kind | IN("agent", "bead", "thread", "operator", "none"))
    and (.evidence.sources_checked | type == "array" and length >= 1)
    and (.evidence.reason_codes | type == "array" and length >= 1)
    and all(.evidence.reason_codes[]; test("^[a-z][a-z0-9_]*(\\.[a-z][a-z0-9_]*)+$"))
    and (.evidence.summary | type == "string" and length > 0)
    and (.nudge.kind | known_kind)
    and (.nudge.command_hint | type == "string" and length > 0)
    and (.nudge.urgency | IN("low", "normal", "high", "urgent"))
    and (.nudge.mutates == false)
    and (.nudge.review_required | type == "boolean")
    and (.escalation.status_check_before_force_release == true)
    and (.escalation.elapsed_time_alone_sufficient == false)
    and (.escalation.human_review_required_for_mutation == true)
    and (.redaction.raw_pane_text_allowed == false)
    and (.redaction.full_message_body_allowed == false)
    and (.redaction.secret_material_allowed == false)
    and (.forbidden_actions | type == "array" and length >= 1)
    and all(.forbidden_actions[]; type == "string" and length > 0)
  )
' "${FIXTURE}" >/dev/null || fail "receipt entries are incomplete"

jq -e '
  any(.receipts[]; .nudge.kind == "force_release_review"
    and .nudge.review_required == true
    and .escalation.minimum_evidence_sources >= 3
    and .escalation.minimum_wait_minutes_after_status_check >= 1)
  and any(.receipts[]; .nudge.kind == "no_action"
    and .trigger_classification == "proof_starved"
    and .target.kind == "none"
    and .nudge.review_required == false
    and (.evidence.reason_codes | index("rch.no_admissible_workers") != null)
    and (.evidence.reason_codes | index("rch.remote_cargo_reached_false") != null)
    and (.forbidden_actions | index("run-local-cargo-as-proof") != null)
    and (.forbidden_actions | index("send-unrequested-broadcast") != null))
  and all(.receipts[]; .nudge.mutates == false)
' "${FIXTURE}" >/dev/null || fail "force-release review guardrails are missing"

receipt_count="$(jq -r '.receipts | length' "${FIXTURE}")"
printf 'attention-router nudge-plan receipts: static verifier passed (%s receipts)\n' "${receipt_count}"
