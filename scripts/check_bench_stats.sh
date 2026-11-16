#!/usr/bin/env bash
# ft-9zzkg — Bench-stats per-PR gate harness.
#
# Reads the current run's Distribution JSONL (emitted by
# crates/frankenterm-core/benches/bench_common.rs::emit_bench_distributions
# at the end of every Criterion bench run), optionally fetches the
# baseline JSONL from origin/main, and invokes the verdict producer at
# scripts/check_bench_stats.py.
#
# Modes (env var BENCH_STATS_MODE, default `advisory`):
#   advisory  — always exits 0; prints verdicts; the per-PR gate runs
#               in advisory mode until origin/main carries a real
#               baseline JSONL (CI graduation tracked in the same bead).
#   enforce   — exits 1 if any bench is flagged regressed.
#
# Reproduce locally:
#   bash scripts/check_bench_stats.sh

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
cd "${REPO_ROOT}"

CURRENT_JSONL="${CURRENT_JSONL:-target/criterion/wa-bench-distributions.jsonl}"
BASELINE_JSONL="${BASELINE_JSONL:-}"
BENCH_STATS_MODE="${BENCH_STATS_MODE:-advisory}"
BENCH_STATS_ALPHA="${BENCH_STATS_ALPHA:-0.05}"
BENCH_STATS_REGRESSION_PCT="${BENCH_STATS_REGRESSION_PCT:-10.0}"
OUTPUT_PATH="${BENCH_STATS_OUTPUT:-target/criterion/wa-bench-stats-verdict.json}"

if [[ ! -f "${CURRENT_JSONL}" ]]; then
    echo "ft-9zzkg: ${CURRENT_JSONL} missing — no Distribution rows to evaluate." >&2
    echo "         Run \`cargo bench -p frankenterm-core\` (or the bench you care about) first." >&2
    exit 0  # Soft-skip: in CI this means benches haven't run yet, not a regression.
fi

# Fetch the baseline from origin/main when running in CI and no explicit
# path was provided. We use git show so the workflow doesn't have to do
# a second checkout.
if [[ -z "${BASELINE_JSONL}" ]]; then
    if git rev-parse --verify origin/main >/dev/null 2>&1; then
        BASE_TMP="$(mktemp -t bench-stats-baseline.XXXXXX.jsonl)"
        if git show "origin/main:${CURRENT_JSONL}" > "${BASE_TMP}" 2>/dev/null; then
            if [[ -s "${BASE_TMP}" ]]; then
                BASELINE_JSONL="${BASE_TMP}"
                echo "ft-9zzkg: baseline fetched from origin/main → ${BASELINE_JSONL}"
            else
                rm -f "${BASE_TMP}"
            fi
        else
            rm -f "${BASE_TMP}"
        fi
    fi
fi

ARGS=(
    "${SCRIPT_DIR}/check_bench_stats.py"
    --current "${CURRENT_JSONL}"
    --alpha "${BENCH_STATS_ALPHA}"
    --regression-threshold-pct "${BENCH_STATS_REGRESSION_PCT}"
    --mode "${BENCH_STATS_MODE}"
    --output "${OUTPUT_PATH}"
)
if [[ -n "${BASELINE_JSONL}" && -f "${BASELINE_JSONL}" ]]; then
    ARGS+=( --baseline "${BASELINE_JSONL}" )
fi

mkdir -p "$(dirname "${OUTPUT_PATH}")"
python3 "${ARGS[@]}"
RC=$?

# Surface a one-line summary for CI logs even when stdout was the JSON
# blob.
if [[ -f "${OUTPUT_PATH}" ]]; then
    python3 - <<EOF
import json, pathlib
data = json.loads(pathlib.Path("${OUTPUT_PATH}").read_text())
print(
    "ft-9zzkg verdict:",
    "mode=" + data["mode"],
    "benches=" + str(data["bench_count"]),
    "ok=" + str(data["ok_count"]),
    "improved=" + str(data["improved_count"]),
    "advisory=" + str(data["advisory_count"]),
    "regressed=" + str(len(data["regressed"])),
)
EOF
fi

exit "${RC}"
