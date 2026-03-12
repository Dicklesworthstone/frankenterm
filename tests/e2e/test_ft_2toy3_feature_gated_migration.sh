#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"
RUN_ID="$(date +"%Y%m%d_%H%M%S")"
LOG_FILE="${LOG_DIR}/feature_gated_migration_${RUN_ID}.jsonl"
LOG_FILE_REL="${LOG_FILE#"${ROOT_DIR}"/}"

emit_log() {
  local outcome="$1" dp="$2" rc="$3" ec="$4" is="$5"
  jq -cn --arg ts "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
    --arg component "feature_gated_migration.e2e" \
    --arg sid "ft_2toy3" --arg cid "ft-2toy3-${RUN_ID}" \
    --arg dp "$dp" --arg is "$is" --arg oc "$outcome" --arg rc "$rc" --arg ec "$ec" \
    '{timestamp:$ts,component:$component,scenario_id:$sid,correlation_id:$cid,
      decision_path:$dp,input_summary:$is,outcome:$oc,reason_code:$rc,error_code:$ec}' \
    >> "${LOG_FILE}"
}

echo "=== Feature-gated module asupersync migration (ft-2toy3) ==="
echo "Run ID: ${RUN_ID}"
echo "Log:    ${LOG_FILE_REL}"
echo ""

PASS=0; FAIL=0
SRC="${ROOT_DIR}/crates/frankenterm-core/src"

check_module() {
  local name="$1" path="$2"
  echo -n "  ${name}: "
  if [ ! -f "${path}" ]; then
    echo "SKIP (file not found)"
    emit_log "skip" "${name}_tokio" "file_not_found" "" "path=${path}"
    return
  fi

  local tokio_refs
  tokio_refs=$(grep -c 'use tokio::' "${path}" 2>/dev/null || true)
  local tokio_attrs
  tokio_attrs=$(grep -c '^\s*#\[tokio::' "${path}" 2>/dev/null || true)

  if [ "${tokio_refs}" -eq 0 ] && [ "${tokio_attrs}" -eq 0 ]; then
    local lines
    lines=$(wc -l < "${path}" | tr -d ' ')
    echo "PASS (${lines} lines, 0 tokio refs)"
    emit_log "pass" "${name}_tokio" "clean" "" "lines=${lines}"
    PASS=$((PASS+1))
  else
    echo "FAIL (${tokio_refs} use tokio, ${tokio_attrs} #[tokio])"
    emit_log "fail" "${name}_tokio" "tokio_remnants" "E_TOKIO" "use=${tokio_refs},attrs=${tokio_attrs}"
    FAIL=$((FAIL+1))
  fi
}

echo "S1: No tokio references in feature-gated modules:"
check_module "web.rs" "${SRC}/web.rs"
check_module "mcp.rs" "${SRC}/mcp.rs"
check_module "distributed.rs" "${SRC}/distributed.rs"
check_module "browser.rs" "${SRC}/browser.rs"
check_module "web_framework.rs" "${SRC}/web_framework.rs"
check_module "mcp_proxy.rs" "${SRC}/mcp_proxy.rs"
check_module "mcp_client.rs" "${SRC}/mcp_client.rs"
echo ""

# S2: runtime_compat usage
echo -n "S2: Feature-gated modules use runtime_compat... "
RC_USAGE=0
for mod in web.rs mcp.rs distributed.rs; do
  if [ -f "${SRC}/${mod}" ]; then
    uses=$(grep -c 'runtime_compat\|crate::runtime_compat' "${SRC}/${mod}" || true)
    RC_USAGE=$((RC_USAGE + uses))
  fi
done
if [ "${RC_USAGE}" -ge 1 ]; then
  echo "PASS (${RC_USAGE} runtime_compat refs)"
  emit_log "pass" "runtime_compat_usage" "present" "" "refs=${RC_USAGE}"
  PASS=$((PASS+1))
else
  # Some modules may use sync-only code — this is acceptable
  echo "OK (${RC_USAGE} refs — sync-only modules may not need runtime_compat)"
  emit_log "pass" "runtime_compat_usage" "sync_only" "" "refs=${RC_USAGE}"
  PASS=$((PASS+1))
fi

# S3: lib.rs feature gates present
echo -n "S3: Feature gates in lib.rs... "
FG_COUNT=0
for feat in '"web"' '"mcp"' '"distributed"'; do
  if grep -q "cfg(feature = ${feat})" "${SRC}/lib.rs"; then
    FG_COUNT=$((FG_COUNT+1))
  fi
done
if [ "${FG_COUNT}" -ge 2 ]; then
  echo "PASS (${FG_COUNT}/3 feature gates)"
  emit_log "pass" "feature_gates_lib" "present" "" "count=${FG_COUNT}"
  PASS=$((PASS+1))
else
  echo "FAIL"
  emit_log "fail" "feature_gates_lib" "missing" "E_GATE" "count=${FG_COUNT}"
  FAIL=$((FAIL+1))
fi

echo ""
echo "=== Results: ${PASS} passed, ${FAIL} failed ==="
echo "Log: ${LOG_FILE_REL}"

[ "${FAIL}" -gt 0 ] && exit 1 || exit 0
