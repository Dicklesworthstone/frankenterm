#!/usr/bin/env bash
# Static verifier for incident-bundle tailer_capture_health fixtures.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

FIXTURE="fixtures/incident-bundles/tailer-capture-health/cases.v1.json"

fail() {
  printf 'incident tailer_capture_health fixture corpus: %s\n' "$*" >&2
  exit 1
}

[[ -f "${FIXTURE}" ]] || fail "missing fixture: ${FIXTURE}"

jq empty "${FIXTURE}"

jq -e '
  .schema_version == 1
  and .contract_id == "ft.incident_bundle.tailer_capture_health.fixtures.v1"
  and .producing_bead == "ft-6y66m"
  and .related_bead == "ft-4ta2y"
  and (.purpose | type == "string" and length > 0)
  and .golden_confidence.artifact_kind == "structural_json_golden"
  and .golden_confidence.deterministic == true
  and .golden_confidence.platform_dependent == false
  and (.golden_confidence.volatility | type == "number" and . >= 1 and . <= 5)
  and (.source_policy | keys | sort) == [
    "destructive_cleanup_allowed",
    "local_cargo_proof_allowed",
    "mutates_rch_allowed",
    "mutates_runtime_allowed",
    "read_only_source_required",
    "service_restart_allowed",
    "stale_counts_as_green",
    "starts_capture_allowed",
    "synthetic_green_health_allowed",
    "unavailable_counts_as_green"
  ]
  and .source_policy.read_only_source_required == true
  and .source_policy.starts_capture_allowed == false
  and .source_policy.mutates_runtime_allowed == false
  and .source_policy.mutates_rch_allowed == false
  and .source_policy.service_restart_allowed == false
  and .source_policy.destructive_cleanup_allowed == false
  and .source_policy.local_cargo_proof_allowed == false
  and .source_policy.unavailable_counts_as_green == false
  and .source_policy.stale_counts_as_green == false
  and .source_policy.synthetic_green_health_allowed == false
  and (.required_cases | sort) == [
    "healthy-measured",
    "stale-lagging",
    "unavailable-snapshot",
    "warning-bearing"
  ]
  and ([.cases[].case_id] | sort) == (.required_cases | sort)
' "${FIXTURE}" >/dev/null || fail "top-level fixture contract is incomplete"

jq -e '
  def source_status_ok: IN("collected", "unavailable");
  def evidence_state_ok: IN("measured", "mixed", "stale", "unavailable");
  def capture_status_ok: IN("healthy", "warning", "stale", "unavailable");
  def redaction_ok: IN("none", "not_applicable");
  def severity_ok: IN("medium", "high");
  def safe_command:
    . == "read StreamingHealth::get_global"
    or . == "read HealthSnapshot.scheduler"
    or . == "read retained tailer_capture_health artifact";
  def forbidden_set_ok:
    sort == [
      "delete files",
      "mutate rch worker",
      "restart rch",
      "restart service",
      "run local cargo as proof",
      "start capture",
      "synthesize green capture health",
      "treat stale capture as healthy"
    ];
  def streaming_ok:
    . == null or (
      (.mode | type == "string" and length > 0)
      and (.events_processed | type == "number" and . >= 0)
      and (.dirty_ranges_total | type == "number" and . >= 0)
      and (.dirty_rows_total | type == "number" and . >= 0)
      and (.gaps_emitted | type == "number" and . >= 0)
      and (.fallback_count | type == "number" and . >= 0)
      and (.active_panes | type == "number" and . >= 0)
    );
  def scheduler_ok:
    . == null or (
      (.budget_active | type == "boolean")
      and (.max_captures_per_sec | type == "number" and . >= 0)
      and (.max_bytes_per_sec | type == "number" and . >= 0)
      and (.captures_remaining | type == "number" and . >= 0)
      and (.bytes_remaining | type == "number" and . >= 0)
      and (.total_rate_limited | type == "number" and . >= 0)
      and (.total_byte_budget_exceeded | type == "number" and . >= 0)
      and (.total_throttle_events | type == "number" and . >= 0)
      and (.tracked_panes | type == "number" and . >= 0)
      and (.pane_rows_total | type == "number" and . >= 0)
      and (.pane_rows_truncated | type == "boolean")
      and (.panes | type == "array")
      and (.tiers | type == "array")
    );

  all(.cases[];
    .source_name == "tailer_capture_health"
    and .source_surface == "StreamingHealth::get_global + HealthSnapshot.scheduler"
    and (.title | type == "string" and length > 0)
    and (.allowed_source_commands | type == "array" and length >= 1 and all(.[]; safe_command))
    and (.bundle_entry.status | source_status_ok)
    and (.bundle_entry.evidence_state | evidence_state_ok)
    and .bundle_entry.mutates_state == false
    and (.bundle_entry.redaction | redaction_ok)
    and .bundle_entry.privacy_tier == "metadata_only"
    and (.bundle_entry.warning_ids | type == "array")
    and (.capture.status | capture_status_ok)
    and (.capture.evidence_state | evidence_state_ok)
    and (.capture.freshness | type == "object")
    and (.capture.freshness.max_age_ms == 30000)
    and (.capture.freshness.fresh | type == "boolean")
    and (.capture.streaming_health | streaming_ok)
    and (.capture.scheduler | scheduler_ok)
    and (.capture.lag_counters | type == "object")
    and (.capture.lag_counters.stale_pane_count | type == "number" and . >= 0)
    and (.capture.reason_codes | type == "array" and length >= 1)
    and all(.capture.reason_codes[]; test("^tailer_capture_health\\.[a-z0-9_]+$"))
    and ((has("warnings") | not) or all(.warnings[];
      (.id | test("^tailer-capture-health\\."))
      and (.severity | severity_ok)
      and (.reason_code | test("^tailer_capture_health\\."))
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
    and $case.capture.status == "healthy"
    and $case.capture.evidence_state == "measured"
    and $case.capture.freshness.fresh == true
    and ($case.capture.streaming_health != null)
    and ($case.capture.scheduler != null)
    and $case.capture.streaming_health.gaps_emitted == 0
    and $case.capture.streaming_health.fallback_count == 0
    and $case.capture.lag_counters.stale_pane_count == 0
    and ([$case.capture.reason_codes[]] | index("tailer_capture_health.ok") != null)
  )
' "${FIXTURE}" >/dev/null || fail "healthy-measured case drifted"

jq -e '
  .cases[]
  | select(.case_id == "warning-bearing") as $case
  | (
    $case.bundle_entry.evidence_state == "mixed"
    and $case.capture.status == "warning"
    and $case.capture.freshness.fresh == true
    and $case.capture.streaming_health.gaps_emitted > 0
    and $case.capture.streaming_health.fallback_count > 0
    and ([$case.capture.reason_codes[]] | index("tailer_capture_health.gaps_emitted") != null)
    and ([$case.warnings[].reason_code] | index("tailer_capture_health.polling_fallback") != null)
  )
' "${FIXTURE}" >/dev/null || fail "warning-bearing case drifted"

jq -e '
  .cases[]
  | select(.case_id == "stale-lagging") as $case
  | (
    $case.bundle_entry.evidence_state == "stale"
    and $case.capture.status == "stale"
    and $case.capture.freshness.fresh == false
    and $case.capture.freshness.age_ms > $case.capture.freshness.max_age_ms
    and $case.capture.lag_counters.stale_pane_count > 0
    and $case.capture.lag_counters.max_capture_lag_ms > $case.capture.freshness.max_age_ms
    and ($case.capture.scheduler.panes | any(.stale == true))
    and ([$case.capture.reason_codes[]] | index("tailer_capture_health.stale_capture") != null)
    and ([$case.warnings[].reason_code] | index("tailer_capture_health.stale_capture") != null)
  )
' "${FIXTURE}" >/dev/null || fail "stale-lagging case drifted"

jq -e '
  .cases[]
  | select(.case_id == "unavailable-snapshot") as $case
  | (
    $case.bundle_entry.status == "unavailable"
    and $case.bundle_entry.evidence_state == "unavailable"
    and $case.capture.status == "unavailable"
    and $case.capture.streaming_health == null
    and $case.capture.scheduler == null
    and $case.capture.freshness.generated_at_ms == null
    and $case.capture.freshness.age_ms == null
    and $case.capture.freshness.fresh == false
    and ([$case.capture.reason_codes[]] | index("tailer_capture_health.snapshot_unavailable") != null)
    and ([$case.warnings[].reason_code] | index("tailer_capture_health.snapshot_unavailable") != null)
  )
' "${FIXTURE}" >/dev/null || fail "unavailable-snapshot case drifted"

jq -e '
  all(.cases[];
    (.forbidden_actions | index("restart rch") != null)
    and (.forbidden_actions | index("restart service") != null)
    and (.forbidden_actions | index("mutate rch worker") != null)
    and (.forbidden_actions | index("delete files") != null)
    and (.forbidden_actions | index("run local cargo as proof") != null)
    and (.forbidden_actions | index("start capture") != null)
    and (.forbidden_actions | index("synthesize green capture health") != null)
    and (.forbidden_actions | index("treat stale capture as healthy") != null)
  )
' "${FIXTURE}" >/dev/null || fail "forbidden action guard drifted"

live_e2e="$(git ls-files tests/e2e | awk '/\.sh$/ { count++ } END { print count + 0 }')"
grep -q "<!--count:e2e_scripts-->${live_e2e}<!--/count-->" README.md \
  || fail "README stamped E2E count stale; expected ${live_e2e}"
grep -q "# ${live_e2e} shell E2E scripts" README.md \
  || fail "README tree E2E count stale; expected ${live_e2e}"

case_count="$(jq -r '.cases | length' "${FIXTURE}")"
printf 'incident tailer_capture_health fixture corpus: static verifier passed (%s cases, %s E2E scripts)\n' "${case_count}" "${live_e2e}"
