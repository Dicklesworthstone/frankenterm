#!/usr/bin/env bash
# Static verifier for incident-bundle proof_rch_evidence fixtures.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

FIXTURE="fixtures/incident-bundles/proof-rch-evidence/cases.v1.json"

fail() {
  printf 'incident proof_rch_evidence fixture corpus: %s\n' "$*" >&2
  exit 1
}

[[ -f "${FIXTURE}" ]] || fail "missing fixture: ${FIXTURE}"

jq empty "${FIXTURE}"

jq -e '
  .schema_version == 1
  and .contract_id == "ft.incident_bundle.proof_rch_evidence.fixtures.v1"
  and .producing_bead == "ft-ig6bv"
  and .related_bead == "ft-zh4t3"
  and (.purpose | type == "string" and length > 0)
  and .golden_confidence.artifact_kind == "structural_json_golden"
  and .golden_confidence.deterministic == true
  and .golden_confidence.platform_dependent == false
  and (.golden_confidence.volatility | type == "number" and . >= 1 and . <= 5)
  and (.source_policy | keys | sort) == [
    "build_cancellation_allowed",
    "destructive_cleanup_allowed",
    "launches_new_proof_allowed",
    "local_cargo_proof_allowed",
    "mutates_rch_allowed",
    "queue_chatter_counts_as_proof",
    "remote_artifact_required_for_pass",
    "setup_chatter_counts_as_proof",
    "sync_chatter_counts_as_proof",
    "worker_mutation_allowed"
  ]
  and .source_policy.launches_new_proof_allowed == false
  and .source_policy.mutates_rch_allowed == false
  and .source_policy.worker_mutation_allowed == false
  and .source_policy.build_cancellation_allowed == false
  and .source_policy.local_cargo_proof_allowed == false
  and .source_policy.sync_chatter_counts_as_proof == false
  and .source_policy.queue_chatter_counts_as_proof == false
  and .source_policy.setup_chatter_counts_as_proof == false
  and .source_policy.remote_artifact_required_for_pass == true
  and .source_policy.destructive_cleanup_allowed == false
  and (.required_cases | sort) == [
    "local-fallback-rejected",
    "missing-artifact",
    "no-admissible-workers",
    "passed-remote-proof",
    "transport-failure"
  ]
  and ([.cases[].case_id] | sort) == (.required_cases | sort)
' "${FIXTURE}" >/dev/null || fail "top-level fixture contract is incomplete"

jq -e '
  def source_status_ok: IN("collected");
  def evidence_state_ok: IN("measured", "mixed", "stale", "unavailable");
  def reason_category_ok: IN("result", "no_worker", "local_fallback", "transport", "missing_artifact");
  def verdict_ok: IN("passed", "blocked");
  def severity_ok: IN("medium", "high");
  def retained_artifact_path_ok:
    (type == "string")
    and (length > 0)
    and (startswith("/") | not)
    and (startswith(".git/") | not)
    and (contains("..") | not)
    and startswith("tests/e2e/artifacts/[RUN]/");
  def artifact_paths_ok:
    if .proof.artifact_posture == "missing_required_artifact" then
      (.proof.artifact_paths | type == "array" and length == 0)
    else
      (.proof.artifact_paths | type == "array" and length >= 1 and all(.[]; retained_artifact_path_ok))
    end;
  def safe_command:
    . == "read retained RCH summary artifact"
    or . == "read retained proof ledger entry"
    or . == "read artifact manifest";
  def forbidden_set_ok:
    sort == [
      "cancel build",
      "count sync chatter as proof",
      "delete files",
      "mutate rch worker",
      "restart rch",
      "run local cargo as proof",
      "run new cargo"
    ];

  all(.cases[];
    .source_name == "proof_rch_evidence"
    and (.source_surface | type == "string" and length > 0)
    and (.title | type == "string" and length > 0)
    and (.allowed_source_commands | type == "array" and length >= 1 and all(.[]; safe_command))
    and (.bundle_entry.status | source_status_ok)
    and (.bundle_entry.evidence_state | evidence_state_ok)
    and .bundle_entry.mutates_state == false
    and .bundle_entry.redaction == "partial"
    and .bundle_entry.privacy_tier == "metadata_only"
    and (.bundle_entry.warning_ids | type == "array")
    and (.proof.verdict | verdict_ok)
    and (.proof.reason_category | reason_category_ok)
    and (.proof.material_command_reached | type == "boolean")
    and (.proof.remote_cargo_reached | type == "boolean")
    and (.proof.remote_test_binary_reached | type == "boolean")
    and .proof.local_cargo_detected == false
    and (.proof.local_fallback_detected | type == "boolean")
    and (.proof.sync_chatter_only | type == "boolean")
    and (.proof.queue_chatter_only | type == "boolean")
    and (.proof.setup_chatter_only | type == "boolean")
    and .proof.launches_new_proof == false
    and (.proof.artifact_posture | type == "string" and length > 0)
    and artifact_paths_ok
    and (.proof.reason_codes | type == "array" and length >= 1)
    and all(.proof.reason_codes[]; test("^rch\\.[a-z0-9_]+$"))
    and ((has("warnings") | not) or all(.warnings[];
      (.id | test("^proof-rch\\."))
      and (.severity | severity_ok)
      and (.reason_code | test("^rch\\."))
      and (.message | type == "string" and length > 0)
    ))
    and (.forbidden_actions | forbidden_set_ok)
    and (.safe_next_action | test("^record_"))
  )
' "${FIXTURE}" >/dev/null || fail "one or more fixture cases are incomplete"

jq -e '
  .cases[]
  | select(.case_id == "passed-remote-proof") as $case
  | (
    $case.proof.verdict == "passed"
    and $case.proof.reason_category == "result"
    and $case.proof.material_command_reached == true
    and $case.proof.remote_cargo_reached == true
    and $case.proof.remote_test_binary_reached == true
    and $case.proof.artifact_posture == "retained"
    and (($case.proof.artifact_paths | length) >= 1)
    and ([$case.proof.reason_codes[]] | index("rch.material_command_passed") != null)
  )
' "${FIXTURE}" >/dev/null || fail "passed-remote-proof case drifted"

jq -e '
  .cases[]
  | select(.case_id == "no-admissible-workers") as $case
  | (
    $case.proof.verdict == "blocked"
    and $case.proof.reason_category == "no_worker"
    and $case.proof.material_command_reached == false
    and $case.proof.remote_cargo_reached == false
    and ([$case.proof.reason_codes[]] | index("rch.no_admissible_workers") != null)
    and ([$case.warnings[].reason_code] | index("rch.no_admissible_workers") != null)
  )
' "${FIXTURE}" >/dev/null || fail "no-admissible-workers case drifted"

jq -e '
  .cases[]
  | select(.case_id == "local-fallback-rejected") as $case
  | (
    $case.proof.verdict == "blocked"
    and $case.proof.reason_category == "local_fallback"
    and $case.proof.local_fallback_detected == true
    and $case.proof.local_cargo_detected == false
    and ([$case.proof.reason_codes[]] | index("rch.local_fallback_refused") != null)
    and ([$case.warnings[].reason_code] | index("rch.local_fallback_refused") != null)
  )
' "${FIXTURE}" >/dev/null || fail "local-fallback-rejected case drifted"

jq -e '
  .cases[]
  | select(.case_id == "transport-failure") as $case
  | (
    $case.proof.verdict == "blocked"
    and $case.proof.reason_category == "transport"
    and $case.proof.material_command_reached == false
    and $case.proof.sync_chatter_only == true
    and $case.proof.setup_chatter_only == true
    and ([$case.proof.reason_codes[]] | index("rch.setup_chatter_not_proof") != null)
    and ([$case.warnings[].reason_code] | index("rch.transport_failed") != null)
  )
' "${FIXTURE}" >/dev/null || fail "transport-failure case drifted"

jq -e '
  .cases[]
  | select(.case_id == "missing-artifact") as $case
  | (
    $case.proof.verdict == "blocked"
    and $case.proof.reason_category == "missing_artifact"
    and $case.proof.material_command_reached == true
    and $case.proof.artifact_posture == "missing_required_artifact"
    and ($case.proof.artifact_paths | length) == 0
    and ([$case.proof.reason_codes[]] | index("rch.required_artifact_missing") != null)
    and ([$case.warnings[].reason_code] | index("rch.required_artifact_missing") != null)
  )
' "${FIXTURE}" >/dev/null || fail "missing-artifact case drifted"

case_count="$(jq -r '.cases | length' "${FIXTURE}")"
printf 'incident proof_rch_evidence fixture corpus: static verifier passed (%s cases)\n' "${case_count}"
