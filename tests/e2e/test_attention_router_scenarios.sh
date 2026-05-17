#!/usr/bin/env bash
# Static verifier for the attention-router scenario inventory.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

SCHEMA="docs/json-schema/ft-attention-router-scenarios.json"
INVENTORY="fixtures/attention-router/scenarios.v1.json"

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
    "docs-only-ready-while-proof-blocked"
  ]))
  and (.scenarios | length >= 6)
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

scenario_count="$(jq -r '.scenarios | length' "${INVENTORY}")"
printf 'attention-router scenario inventory: static verifier passed (%s scenarios)\n' "${scenario_count}"
