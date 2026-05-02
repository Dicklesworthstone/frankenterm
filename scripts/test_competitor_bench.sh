#!/usr/bin/env bash
# Focused tests for ft-t101b competitor bench integration.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
TMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/ft-competitor-bench-test.XXXXXX")"
OUT_DIR="${TMP_ROOT}/out"
STATE_FILE="${TMP_ROOT}/state/regression-state.jsonl"

pass=0
fail=0

check() {
    local name="$1"
    shift
    if "$@"; then
        echo "PASS: ${name}"
        pass=$((pass + 1))
    else
        echo "FAIL: ${name}" >&2
        fail=$((fail + 1))
    fi
}

bash "${SCRIPT_DIR}/competitor-bench.sh" \
    --simulate \
    --release-version "selftest.1" \
    --baseline "github-actions-runner" \
    --runner-sku "self-test" \
    --out-dir "${OUT_DIR}" \
    --state-file "${STATE_FILE}" >/tmp/ft-competitor-bench-selftest-1.log

SNAP1="${OUT_DIR}/competitor-resize-selftest.1-github-actions-runner.json"
check "first snapshot exists" test -f "${SNAP1}"
check "first snapshot has no P1 transition" \
    jq -e '.schema_version == "ft.competitor.resize.snapshot.v1" and (.p1_regressions | length) == 0' "${SNAP1}" >/dev/null
check "first snapshot has 24 samples" \
    jq -e '.samples | length == 24' "${SNAP1}" >/dev/null
check "first snapshot has 18 deltas" \
    jq -e '.deltas | length == 18' "${SNAP1}" >/dev/null

bash "${SCRIPT_DIR}/competitor-bench.sh" \
    --simulate \
    --release-version "selftest.2" \
    --baseline "github-actions-runner" \
    --runner-sku "self-test" \
    --out-dir "${OUT_DIR}" \
    --state-file "${STATE_FILE}" >/tmp/ft-competitor-bench-selftest-2.log

SNAP2="${OUT_DIR}/competitor-resize-selftest.2-github-actions-runner.json"
check "second snapshot auto-files P1 candidates" \
    jq -e '(.p1_regressions | length) > 0' "${SNAP2}" >/dev/null
check "P1 command is dry-run visible" \
    jq -e '.p1_regressions[0].br_command[0] == "br"' "${SNAP2}" >/dev/null
check "state file recorded two releases" \
    bash -c "[[ \$(wc -l < '${STATE_FILE}') -eq 36 ]]"

echo "competitor bench tests: ${pass} passed, ${fail} failed"
if (( fail > 0 )); then
    exit 1
fi
