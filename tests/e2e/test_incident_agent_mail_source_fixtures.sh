#!/usr/bin/env bash
# Static verifier for incident-bundle agent_mail source fixtures.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

FIXTURE="fixtures/incident-bundles/agent-mail-source/cases.v1.json"

fail() {
  printf 'incident agent_mail source fixture corpus: %s\n' "$*" >&2
  exit 1
}

[[ -f "${FIXTURE}" ]] || fail "missing fixture: ${FIXTURE}"

jq empty "${FIXTURE}"

jq -e '
  .schema_version == 1
  and .contract_id == "ft.incident_bundle.agent_mail_source.fixtures.v1"
  and .producing_bead == "ft-hchro"
  and .related_bead == "ft-ye1oi"
  and (.purpose | type == "string" and length > 0)
  and .golden_confidence.artifact_kind == "structural_json_golden"
  and .golden_confidence.deterministic == true
  and .golden_confidence.platform_dependent == false
  and (.golden_confidence.volatility | type == "number" and . >= 1 and . <= 5)
  and (.source_policy | keys | sort) == [
    "acknowledge_allowed",
    "fallback_allowed",
    "fetch_message_bodies_allowed",
    "health_check_allowed",
    "kill_allowed",
    "list_agents_allowed",
    "local_cargo_proof_allowed",
    "max_retries_allowed",
    "register_allowed",
    "repair_allowed",
    "restart_allowed",
    "service_mutation_allowed"
  ]
  and .source_policy.health_check_allowed == true
  and .source_policy.list_agents_allowed == true
  and .source_policy.fallback_allowed == true
  and .source_policy.fetch_message_bodies_allowed == false
  and .source_policy.acknowledge_allowed == false
  and .source_policy.register_allowed == false
  and .source_policy.repair_allowed == false
  and .source_policy.restart_allowed == false
  and .source_policy.kill_allowed == false
  and .source_policy.service_mutation_allowed == false
  and .source_policy.local_cargo_proof_allowed == false
  and .source_policy.max_retries_allowed == 1
  and (.required_cases | sort) == [
    "available",
    "database-error",
    "fallback-only",
    "unavailable-after-retry"
  ]
  and ([.cases[].case_id] | sort) == (.required_cases | sort)
' "${FIXTURE}" >/dev/null || fail "top-level fixture contract is incomplete"

jq -e '
  . as $root
  | def source_status_ok: IN("collected");
  def evidence_state_ok: IN("measured", "mixed", "unavailable");
  def health_level_ok: IN("healthy", "degraded", "unavailable");
  def service_status_ok: IN("available", "unavailable");
  def readiness_ok: IN("ready", "fail", "skipped");
  def last_error_ok: IN("none", "api_unreachable", "database", "fallback_only");
  def severity_ok: IN("medium", "high");
  def safe_command:
    . == "Agent Mail health_check"
    or . == "Agent Mail list_agents metadata"
    or . == "Agent Mail fetch_inbox metadata only"
    or . == "scripts/swarm-tick.sh --agent-mail-fallback frankenterm";
  def forbidden_set_ok:
    sort == [
      "acknowledge messages",
      "am doctor repair",
      "am service restart",
      "fetch message bodies",
      "kill agent-mail",
      "register agent",
      "restart mcp-agent-mail"
    ];

  all($root.cases[];
    .source_name == "agent_mail"
    and (.source_surface | type == "string" and length > 0)
    and (.title | type == "string" and length > 0)
    and (.allowed_source_commands | type == "array" and length >= 1 and all(.[]; safe_command))
    and (.bundle_entry.status | source_status_ok)
    and (.bundle_entry.evidence_state | evidence_state_ok)
    and .bundle_entry.mutates_state == false
    and .bundle_entry.redaction == "partial"
    and .bundle_entry.privacy_tier == "metadata_only"
    and (.bundle_entry.warning_ids | type == "array")
    and (.agent_mail.service_status | service_status_ok)
    and (.agent_mail.health_level | health_level_ok)
    and (.agent_mail.semantic_readiness | readiness_ok)
    and (.agent_mail.api_reachable | type == "boolean")
    and (.agent_mail.database_open | type == "boolean")
    and (.agent_mail.attempt_count | type == "number" and . >= 1)
    and (.agent_mail.retry_count | type == "number" and . >= 0 and . <= $root.source_policy.max_retries_allowed)
    and .agent_mail.attempt_count == (.agent_mail.retry_count + 1)
    and (.agent_mail.last_error_category | last_error_ok)
    and (.agent_mail.inventory.active_agent_count | type == "number" and . >= 0)
    and (.agent_mail.inventory.known_agent_count | type == "number" and . >= 0)
    and (.agent_mail.inventory.inbox_metadata_count | type == "number" and . >= 0)
    and .agent_mail.inventory.message_body_count == 0
    and .agent_mail.inventory.attachment_body_count == 0
    and .agent_mail.privacy.message_bodies_included == false
    and .agent_mail.privacy.attachments_included == false
    and .agent_mail.privacy.secrets_redacted == true
    and .agent_mail.privacy.raw_mailbox_paths_included == false
    and .agent_mail.safety.mutates_state == false
    and .agent_mail.safety.repair_attempted == false
    and .agent_mail.safety.restart_attempted == false
    and .agent_mail.safety.kill_attempted == false
    and .agent_mail.safety.register_attempted == false
    and .agent_mail.safety.acknowledge_attempted == false
    and .agent_mail.safety.fetch_bodies_attempted == false
    and (.agent_mail.fallback.used | type == "boolean")
    and (.agent_mail.fallback.source | type == "string" and length > 0)
    and .agent_mail.fallback.counts_as_agent_mail_health == false
    and (.agent_mail.reason_codes | type == "array" and length >= 1)
    and all(.agent_mail.reason_codes[]; test("^agent_mail\\.[a-z0-9_]+$"))
    and ((has("warnings") | not) or all(.warnings[];
      (.id | test("^agent-mail\\."))
      and (.severity | severity_ok)
      and (.reason_code | test("^agent_mail\\."))
      and (.message | type == "string" and length > 0)
    ))
    and (.forbidden_actions | forbidden_set_ok)
    and (.safe_next_action | test("^record_"))
  )
' "${FIXTURE}" >/dev/null || fail "one or more fixture cases are incomplete"

jq -e '
  .cases[]
  | select(.case_id == "available") as $case
  | (
    $case.bundle_entry.evidence_state == "measured"
    and $case.agent_mail.service_status == "available"
    and $case.agent_mail.semantic_readiness == "ready"
    and $case.agent_mail.api_reachable == true
    and $case.agent_mail.database_open == true
    and $case.agent_mail.retry_count == 0
    and $case.agent_mail.inventory.active_agent_count > 0
    and $case.agent_mail.inventory.inbox_metadata_count > 0
    and $case.agent_mail.fallback.used == false
    and ([$case.agent_mail.reason_codes[]] | index("agent_mail.available") != null)
  )
' "${FIXTURE}" >/dev/null || fail "available case drifted"

jq -e '
  .cases[]
  | select(.case_id == "unavailable-after-retry") as $case
  | (
    $case.bundle_entry.evidence_state == "unavailable"
    and $case.agent_mail.service_status == "unavailable"
    and $case.agent_mail.api_reachable == false
    and $case.agent_mail.database_open == false
    and $case.agent_mail.retry_count == 1
    and $case.agent_mail.last_error_category == "api_unreachable"
    and ([$case.agent_mail.reason_codes[]] | index("agent_mail.unavailable_after_retry") != null)
    and ([$case.warnings[].reason_code] | index("agent_mail.unavailable_after_retry") != null)
  )
' "${FIXTURE}" >/dev/null || fail "unavailable-after-retry case drifted"

jq -e '
  .cases[]
  | select(.case_id == "database-error") as $case
  | (
    $case.bundle_entry.evidence_state == "unavailable"
    and $case.agent_mail.api_reachable == true
    and $case.agent_mail.database_open == false
    and $case.agent_mail.retry_count == 1
    and $case.agent_mail.last_error_category == "database"
    and ([$case.agent_mail.reason_codes[]] | index("agent_mail.database_error") != null)
    and ([$case.warnings[].reason_code] | index("agent_mail.database_error") != null)
  )
' "${FIXTURE}" >/dev/null || fail "database-error case drifted"

jq -e '
  .cases[]
  | select(.case_id == "fallback-only") as $case
  | (
    $case.bundle_entry.evidence_state == "mixed"
    and $case.agent_mail.service_status == "unavailable"
    and $case.agent_mail.semantic_readiness == "skipped"
    and $case.agent_mail.fallback.used == true
    and $case.agent_mail.fallback.source == "scripts/swarm-tick.sh --agent-mail-fallback frankenterm"
    and $case.agent_mail.fallback.counts_as_agent_mail_health == false
    and ([$case.agent_mail.reason_codes[]] | index("agent_mail.fallback_only") != null)
    and ([$case.warnings[].reason_code] | index("agent_mail.fallback_only") != null)
  )
' "${FIXTURE}" >/dev/null || fail "fallback-only case drifted"

live_e2e="$(find tests/e2e -type f -name '*.sh' | wc -l | tr -d ' ')"
grep -q "<!--count:e2e_scripts-->${live_e2e}<!--/count-->" README.md \
  || fail "README stamped E2E count stale; expected ${live_e2e}"
grep -q "# ${live_e2e} shell E2E scripts" README.md \
  || fail "README tree E2E count stale; expected ${live_e2e}"

case_count="$(jq -r '.cases | length' "${FIXTURE}")"
printf 'incident agent_mail source fixture corpus: static verifier passed (%s cases, %s E2E scripts)\n' "${case_count}" "${live_e2e}"
