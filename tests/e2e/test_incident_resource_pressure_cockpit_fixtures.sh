#!/usr/bin/env bash
# Static verifier for incident-bundle resource_pressure_cockpit fixtures.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

FIXTURE="fixtures/incident-bundles/resource-pressure-cockpit/cases.v1.json"

fail() {
  printf 'incident resource_pressure_cockpit fixture corpus: %s\n' "$*" >&2
  exit 1
}

[[ -f "${FIXTURE}" ]] || fail "missing fixture: ${FIXTURE}"

jq empty "${FIXTURE}"

jq -e '
  .schema_version == 1
  and .contract_id == "ft.incident_bundle.resource_pressure_cockpit.fixtures.v1"
  and .producing_bead == "ft-40u85"
  and .related_bead == "ft-174s5"
  and (.purpose | type == "string" and length > 0)
  and .golden_confidence.artifact_kind == "structural_json_golden"
  and .golden_confidence.deterministic == true
  and .golden_confidence.platform_dependent == false
  and (.golden_confidence.volatility | type == "number" and . >= 1 and . <= 5)
  and (.source_policy | keys | sort) == [
    "destructive_cleanup_allowed",
    "launches_pressure_probe_allowed",
    "local_cargo_proof_allowed",
    "mutates_rch_allowed",
    "mutates_runtime_allowed",
    "read_only_source_required",
    "service_restart_allowed",
    "target_class_skipped_counts_as_proof",
    "unavailable_counts_as_green",
    "worker_mutation_allowed"
  ]
  and .source_policy.launches_pressure_probe_allowed == false
  and .source_policy.mutates_runtime_allowed == false
  and .source_policy.mutates_rch_allowed == false
  and .source_policy.worker_mutation_allowed == false
  and .source_policy.service_restart_allowed == false
  and .source_policy.destructive_cleanup_allowed == false
  and .source_policy.target_class_skipped_counts_as_proof == false
  and .source_policy.unavailable_counts_as_green == false
  and .source_policy.local_cargo_proof_allowed == false
  and .source_policy.read_only_source_required == true
  and (.required_domains | sort) == [
    "action_receipts",
    "capacity_admission",
    "memory",
    "pane_budget",
    "queue_backpressure",
    "resource_admission",
    "rss_residency",
    "storage_io",
    "worker_pool"
  ]
  and (.required_cases | sort) == [
    "degraded-no-worker",
    "healthy-measured",
    "target-class-skipped",
    "unavailable-snapshot"
  ]
  and ([.cases[].case_id] | sort) == (.required_cases | sort)
' "${FIXTURE}" >/dev/null || fail "top-level fixture contract is incomplete"

jq -e '
  def source_status_ok: IN("collected", "unavailable");
  def evidence_state_ok: IN("measured", "mixed", "stale", "unavailable");
  def proof_gate_ok: IN("healthy", "pressured", "degraded", "skipped_proof");
  def cockpit_status_ok: IN("ready", "watch", "violated", "unknown", "unavailable");
  def privacy_ok: IN("metadata_only");
  def redaction_ok: IN("none", "partial", "not_applicable");
  def severity_ok: IN("medium", "high");
  def safe_command:
    . == "read HealthSnapshot pressure fields"
    or . == "read SwarmResourceCockpitSnapshot"
    or . == "read retained resource cockpit artifact";
  def retained_artifact_path_ok:
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
    and (contains("/.git/") | not)
    and (
      startswith("tests/e2e/artifacts/resource-cockpit/[RUN]/")
      or startswith("docs/attestations/proofs/")
    );
  def artifact_paths_ok:
    if .cockpit.artifact_posture == "absent" then
      (.cockpit.artifact_paths | type == "array" and length == 0)
    else
      (.cockpit.artifact_paths | type == "array" and length >= 1 and all(.[]; retained_artifact_path_ok))
    end;
  def forbidden_set_ok:
    sort == [
      "delete files",
      "launch pressure test",
      "mutate rch worker",
      "restart rch",
      "restart service",
      "run local cargo as proof",
      "treat skipped target-class proof as production proof"
    ];
  def required_domain_keys:
    [
      "action_receipts",
      "capacity_admission",
      "memory",
      "pane_budget",
      "queue_backpressure",
      "resource_admission",
      "rss_residency",
      "storage_io",
      "worker_pool"
    ];
  def pressure_ok:
    IN("normal", "elevated", "critical", "emergency", "unknown", "green", "yellow", "red", "black");

  all(.cases[];
    .source_name == "resource_pressure_cockpit"
    and (.source_surface | type == "string" and length > 0)
    and (.title | type == "string" and length > 0)
    and (.allowed_source_commands | type == "array" and length >= 1 and all(.[]; safe_command))
    and (.bundle_entry.status | source_status_ok)
    and (.bundle_entry.evidence_state | evidence_state_ok)
    and .bundle_entry.mutates_state == false
    and (.bundle_entry.redaction | redaction_ok)
    and (.bundle_entry.privacy_tier | privacy_ok)
    and (.bundle_entry.warning_ids | type == "array")
    and (.cockpit.status | cockpit_status_ok)
    and (.cockpit.proof_gate | proof_gate_ok)
    and (.cockpit.evidence_state | evidence_state_ok)
    and (.cockpit.freshness | type == "object")
    and (.cockpit.freshness.max_age_ms | type == "number" and . > 0)
    and (.cockpit.freshness.fresh | type == "boolean")
    and (.cockpit.run_identity | type == "object")
    and (.cockpit.run_identity.evidence_level | type == "string" and length > 0)
    and (.cockpit.run_identity.proof_status | type == "string" and length > 0)
    and (.cockpit.run_identity.target_hardware_skipped | type == "boolean")
    and (.cockpit.domains | keys | sort) == required_domain_keys
    and all(.cockpit.domains[];
      (.pressure | pressure_ok)
      and (.evidence_state | evidence_state_ok)
      and (.reason_codes | type == "array" and length >= 1)
      and all(.reason_codes[]; test("^resource\\."))
    )
    and (.cockpit.artifact_posture | type == "string" and length > 0)
    and artifact_paths_ok
    and ((has("warnings") | not) or all(.warnings[];
      (.id | test("^resource-pressure\\."))
      and (.severity | severity_ok)
      and (.reason_code | test("^resource(_pressure_cockpit)?\\."))
      and (.message | type == "string" and length > 0)
    ))
    and (.forbidden_actions | forbidden_set_ok)
    and (.safe_next_action | test("^record_"))
  )
' "${FIXTURE}" >/dev/null || fail "one or more fixture cases are incomplete"

jq -e '
  .cases[]
  | select(.case_id == "healthy-measured") as $case
  | (
    $case.bundle_entry.status == "collected"
    and $case.bundle_entry.evidence_state == "measured"
    and $case.cockpit.status == "ready"
    and $case.cockpit.proof_gate == "healthy"
    and $case.cockpit.evidence_state == "measured"
    and $case.cockpit.freshness.fresh == true
    and $case.cockpit.run_identity.evidence_level == "remote_reduced"
    and $case.cockpit.run_identity.proof_status == "proven_predicate_met"
    and $case.cockpit.run_identity.target_hardware_skipped == false
    and all($case.cockpit.domains[]; .evidence_state == "measured")
    and ([$case.cockpit.domains[].pressure] | all(. != "unknown"))
    and ($case.cockpit.artifact_paths | length) >= 1
  )
' "${FIXTURE}" >/dev/null || fail "healthy-measured case drifted"

jq -e '
  .cases[]
  | select(.case_id == "degraded-no-worker") as $case
  | (
    $case.bundle_entry.evidence_state == "mixed"
    and $case.cockpit.status == "violated"
    and $case.cockpit.proof_gate == "degraded"
    and $case.cockpit.evidence_state == "mixed"
    and $case.cockpit.domains.worker_pool.pressure == "critical"
    and ([$case.cockpit.domains.worker_pool.reason_codes[]] | index("resource.worker_pool.no_admissible_workers") != null)
    and ([$case.warnings[].reason_code] | index("resource.worker_pool.no_admissible_workers") != null)
  )
' "${FIXTURE}" >/dev/null || fail "degraded-no-worker case drifted"

jq -e '
  .cases[]
  | select(.case_id == "target-class-skipped") as $case
  | (
    $case.cockpit.proof_gate == "skipped_proof"
    and $case.cockpit.evidence_state == "stale"
    and $case.cockpit.freshness.fresh == false
    and $case.cockpit.run_identity.evidence_level == "skipped_not_proven"
    and $case.cockpit.run_identity.proof_status == "skipped_not_proven"
    and $case.cockpit.run_identity.target_hardware == "skipped_not_proven"
    and $case.cockpit.run_identity.target_hardware_skipped == true
    and all($case.cockpit.domains[]; .pressure == "unknown" and .evidence_state == "stale")
    and ([$case.warnings[].reason_code] | index("resource.target_class.skipped_not_proven") != null)
  )
' "${FIXTURE}" >/dev/null || fail "target-class-skipped case drifted"

jq -e '
  .cases[]
  | select(.case_id == "unavailable-snapshot") as $case
  | (
    $case.bundle_entry.status == "unavailable"
    and $case.bundle_entry.evidence_state == "unavailable"
    and $case.cockpit.status == "unavailable"
    and $case.cockpit.proof_gate == "skipped_proof"
    and $case.cockpit.evidence_state == "unavailable"
    and $case.cockpit.freshness.generated_at_ms == null
    and $case.cockpit.freshness.fresh == false
    and $case.cockpit.run_identity.proof_status == "unavailable"
    and all($case.cockpit.domains[]; .pressure == "unknown" and .evidence_state == "unavailable")
    and ($case.cockpit.artifact_paths | length) == 0
    and ([$case.warnings[].reason_code] | index("resource_pressure_cockpit.snapshot_unavailable") != null)
  )
' "${FIXTURE}" >/dev/null || fail "unavailable-snapshot case drifted"

jq -e '
  all(.cases[];
    (.forbidden_actions | index("restart rch") != null)
    and (.forbidden_actions | index("restart service") != null)
    and (.forbidden_actions | index("mutate rch worker") != null)
    and (.forbidden_actions | index("delete files") != null)
    and (.forbidden_actions | index("run local cargo as proof") != null)
  )
' "${FIXTURE}" >/dev/null || fail "forbidden action guard drifted"

case_count="$(jq -r '.cases | length' "${FIXTURE}")"
printf 'incident resource_pressure_cockpit fixture corpus: static verifier passed (%s cases)\n' "${case_count}"
