#!/usr/bin/env bash
# Static verifier for incident-bundle beads_coordination_snapshot fixtures.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

FIXTURE="fixtures/incident-bundles/beads-coordination-snapshot/cases.v1.json"

fail() {
  printf 'incident beads_coordination_snapshot fixture corpus: %s\n' "$*" >&2
  exit 1
}

[[ -f "${FIXTURE}" ]] || fail "missing fixture: ${FIXTURE}"

jq empty "${FIXTURE}"

jq -e '
  .schema_version == 1
  and .contract_id == "ft.incident_bundle.beads_coordination_snapshot.fixtures.v1"
  and .producing_bead == "ft-qc8r2"
  and .related_bead == "ft-tkkqx"
  and (.purpose | type == "string" and length > 0)
  and .golden_confidence.artifact_kind == "structural_json_golden"
  and .golden_confidence.deterministic == true
  and .golden_confidence.platform_dependent == false
  and (.golden_confidence.volatility | type == "number" and . >= 1 and . <= 5)
  and (.source_policy | keys | sort) == [
    "agent_mail_repair_allowed",
    "claim_allowed",
    "full_agent_mail_bodies_allowed",
    "local_cargo_proof_allowed",
    "mutates_beads_allowed",
    "raw_pane_content_allowed",
    "rch_service_mutation_allowed",
    "reopen_allowed",
    "service_restart_allowed",
    "sync_allowed"
  ]
  and all(.source_policy[]; . == false)
  and (.required_cases | sort) == [
    "agent-mail-fallback",
    "dirty-overlap",
    "healthy-ready-queue",
    "stale-candidate"
  ]
  and ([.cases[].case_id] | sort) == (.required_cases | sort)
' "${FIXTURE}" >/dev/null || fail "top-level fixture contract is incomplete"

jq -e '
  def source_status_ok: IN("collected");
  def evidence_state_ok: IN("measured", "mixed");
  def risk_ok: IN("clean", "medium", "high");
  def severity_ok: IN("medium", "high");
  def safe_command:
    . == "br ready --json"
    or . == "br list --status in_progress --json"
    or . == "br dep cycles --json"
    or . == "bv --robot-triage"
    or . == "scripts/swarm-tick.sh --agent-mail-fallback frankenterm";
  def forbidden_set_ok:
    sort == [
      "am doctor repair",
      "am service restart",
      "br reopen",
      "br sync --flush-only",
      "br update --claim",
      "delete_files",
      "kill agent-mail"
    ];

  all(.cases[];
    .source_name == "beads_coordination_snapshot"
    and (.source_surface | type == "string" and length > 0)
    and (.title | type == "string" and length > 0)
    and (.allowed_source_commands | type == "array" and length >= 1 and all(.[]; safe_command))
    and (.bundle_entry.status | source_status_ok)
    and (.bundle_entry.evidence_state | evidence_state_ok)
    and .bundle_entry.mutates_state == false
    and .bundle_entry.redaction == "partial"
    and .bundle_entry.privacy_tier == "metadata_only"
    and (.bundle_entry.warning_ids | type == "array")
    and (.coordination.ready_count | type == "number" and . >= 0)
    and (.coordination.in_progress_count | type == "number" and . >= 0)
    and (.coordination.active_agents | type == "array")
    and (.coordination.ready_candidates | type == "array")
    and (.coordination.stale_reopen.default_action | IN("do_not_reopen", "comment_for_status"))
    and (.coordination.stale_reopen.candidate_count | type == "number" and . >= 0)
    and (.coordination.stale_reopen.requires_status_check | type == "boolean")
    and (.coordination.dirty_overlap.risk_level | risk_ok)
    and (.coordination.dirty_overlap.tracked_dirty_count | type == "number" and . >= 0)
    and (.coordination.dirty_overlap.untracked_dirty_count | type == "number" and . >= 0)
    and (.coordination.reason_codes | type == "array" and length >= 1)
    and all(.coordination.reason_codes[]; test("^[a-z][a-z0-9_]*(\\.[a-z][a-z0-9_]*)+$"))
    and ((has("warnings") | not) or all(.warnings[];
      (.id | test("^beads-coordination\\."))
      and (.severity | severity_ok)
      and (.reason_code | test("^(beads|git|agent_mail|fallback)\\."))
      and (.message | type == "string" and length > 0)
    ))
    and (.forbidden_actions | forbidden_set_ok)
    and (.safe_next_action | test("^record_"))
  )
' "${FIXTURE}" >/dev/null || fail "one or more fixture cases are incomplete"

jq -e '
  .cases[]
  | select(.case_id == "healthy-ready-queue") as $case
  | (
    $case.coordination.ready_count > 0
    and $case.coordination.in_progress_count == 0
    and (($case.coordination.ready_candidates | length) == $case.coordination.ready_count)
    and $case.coordination.stale_reopen.default_action == "do_not_reopen"
    and $case.coordination.dirty_overlap.risk_level == "clean"
    and ([$case.coordination.reason_codes[]] | index("beads.ready_present") != null)
  )
' "${FIXTURE}" >/dev/null || fail "healthy-ready-queue case drifted"

jq -e '
  .cases[]
  | select(.case_id == "agent-mail-fallback") as $case
  | (
    $case.bundle_entry.evidence_state == "mixed"
    and $case.coordination.ready_count == 0
    and $case.coordination.in_progress_count == 0
    and ([$case.coordination.reason_codes[]] | index("agent_mail.unavailable_after_retry") != null)
    and ([$case.coordination.reason_codes[]] | index("fallback.beads_only") != null)
    and ([$case.warnings[].reason_code] | index("agent_mail.unavailable_after_retry") != null)
  )
' "${FIXTURE}" >/dev/null || fail "agent-mail-fallback case drifted"

jq -e '
  .cases[]
  | select(.case_id == "dirty-overlap") as $case
  | (
    $case.coordination.dirty_overlap.risk_level == "high"
    and $case.coordination.dirty_overlap.tracked_dirty_count > 0
    and $case.coordination.stale_reopen.default_action == "do_not_reopen"
    and $case.coordination.stale_reopen.requires_status_check == true
    and ([$case.coordination.reason_codes[]] | index("git.tracked_overlap_risk") != null)
    and ([$case.warnings[].reason_code] | index("git.tracked_overlap_risk") != null)
  )
' "${FIXTURE}" >/dev/null || fail "dirty-overlap case drifted"

jq -e '
  .cases[]
  | select(.case_id == "stale-candidate") as $case
  | (
    $case.coordination.in_progress_count == 1
    and $case.coordination.stale_reopen.candidate_count == 1
    and $case.coordination.stale_reopen.default_action == "comment_for_status"
    and $case.coordination.stale_reopen.requires_status_check == true
    and $case.coordination.dirty_overlap.risk_level == "clean"
    and ([$case.coordination.reason_codes[]] | index("beads.status_check_required") != null)
    and ([$case.warnings[].reason_code] | index("beads.status_check_required") != null)
  )
' "${FIXTURE}" >/dev/null || fail "stale-candidate case drifted"

case_count="$(jq -r '.cases | length' "${FIXTURE}")"
printf 'incident beads_coordination_snapshot fixture corpus: static verifier passed (%s cases)\n' "${case_count}"
