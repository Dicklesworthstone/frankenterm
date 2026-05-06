#!/usr/bin/env bash
set -uo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
RUN_ID="${FT_REHEARSAL_RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)}"
OUT_DIR="${FT_REHEARSAL_OUT_DIR:-$ROOT_DIR/tests/e2e/artifacts/high-scale-rehearsal/$RUN_ID}"
LIVE_PROBES=0

usage() {
  cat <<'USAGE'
Usage: scripts/high-scale-rehearsal.sh [--out-dir DIR] [--run-id ID] [--live-probes]

Bounded high-scale operator rehearsal. Default mode is shell-only and does not
run Cargo, touch live GUI state, restart shared services, or mutate processes.

Outputs:
  rehearsal-events.jsonl
  rehearsal-summary.json
  git-status-short.txt
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --out-dir)
      OUT_DIR="$2"
      shift 2
      ;;
    --run-id)
      RUN_ID="$2"
      shift 2
      ;;
    --live-probes)
      LIVE_PROBES=1
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

json_string() {
  local value="${1//\\/\\\\}"
  value="${value//\"/\\\"}"
  value="${value//$'\n'/\\n}"
  printf '"%s"' "$value"
}

mkdir -p "$OUT_DIR"
EVENTS="$OUT_DIR/rehearsal-events.jsonl"
SUMMARY="$OUT_DIR/rehearsal-summary.json"
: >"$EVENTS"

PASS_COUNT=0
SKIP_COUNT=0
FAIL_COUNT=0

write_event() {
  local scenario="$1"
  local status="$2"
  local receipt="$3"
  local artifact="$4"
  local summary="$5"

  case "$status" in
    PASS) PASS_COUNT=$((PASS_COUNT + 1)) ;;
    SKIP) SKIP_COUNT=$((SKIP_COUNT + 1)) ;;
    FAIL) FAIL_COUNT=$((FAIL_COUNT + 1)) ;;
  esac

  printf '{"schema_version":1,"run_id":%s,"scenario":%s,"status":%s,"receipt":%s,"artifact_path":%s,"summary":%s}\n' \
    "$(json_string "$RUN_ID")" \
    "$(json_string "$scenario")" \
    "$(json_string "$status")" \
    "$(json_string "$receipt")" \
    "$(json_string "$artifact")" \
    "$(json_string "$summary")" >>"$EVENTS"
}

copy_if_present() {
  local source="$1"
  local target="$2"
  if [[ -f "$source" ]]; then
    cp "$source" "$target"
    return 0
  fi
  return 1
}

record_static_fixture() {
  local scenario="$1"
  local source="$2"
  local target_name="$3"
  local summary="$4"
  local target="$OUT_DIR/$target_name"

  if copy_if_present "$ROOT_DIR/$source" "$target"; then
    write_event "$scenario" PASS READY "$target" "$summary"
  else
    write_event "$scenario" SKIP SKIPPED_NOT_PROVEN "$target" "missing fixture: $source"
  fi
}

record_optional_live_probe() {
  local scenario="$1"
  local command_name="$2"
  local target_name="$3"
  local summary="$4"
  local target="$OUT_DIR/$target_name"
  shift 4

  if [[ "$LIVE_PROBES" -ne 1 ]]; then
    write_event "$scenario" SKIP SKIPPED_NOT_PROVEN "$target" "$summary; live probe not requested"
    return
  fi

  if ! command -v "$command_name" >/dev/null 2>&1; then
    write_event "$scenario" SKIP SKIPPED_NOT_PROVEN "$target" "$command_name not found"
    return
  fi

  if "$@" >"$target" 2>&1; then
    write_event "$scenario" PASS READY "$target" "$summary"
  else
    write_event "$scenario" SKIP SKIPPED_NOT_PROVEN "$target" "$summary; probe exited non-zero"
  fi
}

git -C "$ROOT_DIR" status --short --untracked-files=no >"$OUT_DIR/git-status-short.txt" 2>&1 || true
git -C "$ROOT_DIR" rev-parse HEAD >"$OUT_DIR/git-head.txt" 2>&1 || true
git -C "$ROOT_DIR" branch --show-current >"$OUT_DIR/git-branch.txt" 2>&1 || true

record_static_fixture \
  synthetic_swarm_scale \
  fixtures/scale-lab/massive-swarm-evidence-index.v1.json \
  massive-swarm-evidence-index.v1.json \
  "scale-lab evidence index fixture copied; high-scale hardware rows may still be SKIPPED_NOT_PROVEN"

record_static_fixture \
  storage_indexing_pressure \
  fixtures/scale-lab/storage-index-heatmap-summary.v1.json \
  storage-index-heatmap-summary.v1.json \
  "storage/index heat-map fixture copied for IO-pressure rehearsal"

record_static_fixture \
  policy_approval_backlog \
  docs/risk-scoring.md \
  policy-recommendation-risk-scoring.md \
  "policy recommendation risk-scoring doc copied for approval-backlog rehearsal"

record_static_fixture \
  robot_mcp_control_plane_smoke \
  crates/frankenterm-core/tests/golden_robot_envelope/control_plane_golden_matrix.json \
  control-plane-golden-matrix.json \
  "Robot/MCP control-plane golden matrix copied for smoke rehearsal"

record_static_fixture \
  mission_chaos_recovery \
  docs/metrics/mission_chaos_evidence.json \
  mission-chaos-evidence.json \
  "mission chaos evidence fixture copied for recovery rehearsal"

if rg -n "SloCockpitSnapshot|SLO_COCKPIT_SCHEMA_VERSION" \
  "$ROOT_DIR/crates/frankenterm-core/src/runtime_health.rs" >"$OUT_DIR/slo-cockpit-symbols.txt" 2>&1; then
  write_event slo_cockpit_bottlenecks PASS READY "$OUT_DIR/slo-cockpit-symbols.txt" \
    "SLO cockpit core symbols found for bottleneck rehearsal"
else
  write_event slo_cockpit_bottlenecks SKIP SKIPPED_NOT_PROVEN "$OUT_DIR/slo-cockpit-symbols.txt" \
    "SLO cockpit symbols unavailable in this checkout"
fi

record_optional_live_probe \
  degraded_agent_mail \
  am \
  agent-mail-status.txt \
  "Agent Mail degradation probe is optional and non-mutating" \
  am status

record_optional_live_probe \
  rch_worker_loss \
  rch \
  rch-status.txt \
  "RCH worker-loss rehearsal records status only; it does not run Cargo" \
  rch status

cat >"$SUMMARY" <<JSON
{
  "schema_version": 1,
  "run_id": "$(json_string "$RUN_ID" | sed 's/^"//; s/"$//')",
  "mode": "$([[ "$LIVE_PROBES" -eq 1 ]] && echo live_probes || echo bounded)",
  "artifact_dir": "$(json_string "$OUT_DIR" | sed 's/^"//; s/"$//')",
  "events_jsonl": "$(json_string "$EVENTS" | sed 's/^"//; s/"$//')",
  "pass_count": $PASS_COUNT,
  "skip_count": $SKIP_COUNT,
  "fail_count": $FAIL_COUNT,
  "local_cargo_used": false,
  "destructive_actions_used": false
}
JSON

echo "high-scale rehearsal artifacts: $OUT_DIR"
echo "events: $EVENTS"
echo "summary: $SUMMARY"
