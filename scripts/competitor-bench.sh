#!/usr/bin/env bash
# ft-t101b competitor resize-bench harness.
#
# Produces four raw per-terminal JSON files and aggregates them into
# docs/perf/competitor-resize-<version>-<baseline>.json.  Live capture is
# intentionally explicit: CI and local smoke tests use --simulate so the
# regression-state and auto-file-P1 wiring are deterministic.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"

MODE="simulate"
RELEASE_VERSION="${COMPETITOR_BENCH_RELEASE_VERSION:-dev}"
HARDWARE_BASELINE="${COMPETITOR_BENCH_BASELINE:-github-actions-runner}"
RUNNER_SKU="${COMPETITOR_BENCH_RUNNER_SKU:-$(uname -s)-$(uname -m)}"
OUT_DIR="${COMPETITOR_BENCH_OUT_DIR:-${REPO_ROOT}/docs/perf}"
RAW_DIR=""
STATE_FILE="${COMPETITOR_BENCH_STATE_FILE:-${REPO_ROOT}/docs/perf/regression-state.jsonl}"
FILE_P1=0

usage() {
    cat <<'EOF'
Usage: scripts/competitor-bench.sh [options]

Options:
  --simulate                  Write deterministic synthetic raw bench JSON (default)
  --input-dir DIR             Aggregate existing raw per-competitor JSON files
  --live                      Require live terminal tools and print the reproducible workload plan
  --release-version VERSION   Release/version label for the snapshot
  --baseline LABEL            HardwareBaseline label (m2-macbook-pro-16gb,
                              framework-laptop-13-i7, threadripper-rtx-4070,
                              github-actions-runner)
  --runner-sku LABEL          Runner/SKU fingerprint
  --out-dir DIR               Snapshot output directory
  --state-file PATH           Regression-state JSONL path
  --file-p1                   Execute br create for newly consecutive regressions
  --self-test                 Run deterministic harness/state smoke tests
EOF
}

while (($#)); do
    case "$1" in
        --simulate) MODE="simulate"; shift ;;
        --input-dir) RAW_DIR="$2"; MODE="aggregate"; shift 2 ;;
        --live) MODE="live"; shift ;;
        --release-version) RELEASE_VERSION="$2"; shift 2 ;;
        --baseline) HARDWARE_BASELINE="$2"; shift 2 ;;
        --runner-sku) RUNNER_SKU="$2"; shift 2 ;;
        --out-dir) OUT_DIR="$2"; shift 2 ;;
        --state-file) STATE_FILE="$2"; shift 2 ;;
        --file-p1) FILE_P1=1; shift ;;
        --self-test) exec bash "${SCRIPT_DIR}/test_competitor_bench.sh" ;;
        -h|--help) usage; exit 0 ;;
        *) echo "competitor-bench: unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done

case "${HARDWARE_BASELINE}" in
    m2-macbook-pro-16gb|framework-laptop-13-i7|threadripper-rtx-4070|github-actions-runner) ;;
    *) echo "competitor-bench: unsupported --baseline ${HARDWARE_BASELINE}" >&2; exit 2 ;;
esac

write_raw_json() {
    local competitor="$1"
    local fps_p50="$2"
    local fps_p95="$3"
    local fps_p99="$4"
    local frame_time_p95_ms="$5"
    local gpu_memory_peak_mb="$6"
    local cpu_peak_pct="$7"
    local path="${RAW_DIR}/${competitor}.json"
    jq -n \
        --arg competitor "${competitor}" \
        --arg release_version "${RELEASE_VERSION}" \
        --arg hardware_baseline "${HARDWARE_BASELINE}" \
        --arg runner_sku "${RUNNER_SKU}" \
        --argjson fps_p50 "${fps_p50}" \
        --argjson fps_p95 "${fps_p95}" \
        --argjson fps_p99 "${fps_p99}" \
        --argjson frame_time_p95_ms "${frame_time_p95_ms}" \
        --argjson gpu_memory_peak_mb "${gpu_memory_peak_mb}" \
        --argjson cpu_peak_pct "${cpu_peak_pct}" \
        '{
          schema_version: "ft.competitor.resize.raw.v1",
          competitor: $competitor,
          release_version: $release_version,
          hardware_baseline: $hardware_baseline,
          runner_sku: $runner_sku,
          workload: {
            terminal_count: 4,
            panes_per_terminal: 50,
            duration_seconds: 5,
            corpus: "/usr/share/dict/words",
            resize_gesture: "5s resize storm"
          },
          metrics: {
            fps_p50: $fps_p50,
            fps_p95: $fps_p95,
            fps_p99: $fps_p99,
            frame_time_p95_ms: $frame_time_p95_ms,
            gpu_memory_peak_mb: $gpu_memory_peak_mb,
            cpu_peak_pct: $cpu_peak_pct
          }
        }' > "${path}"
}

simulate_raw_results() {
    RAW_DIR="${RAW_DIR:-$(mktemp -d "${TMPDIR:-/tmp}/ft-competitor-bench.XXXXXX")}"
    mkdir -p "${RAW_DIR}"
    # ft is deliberately >10% behind ghostty/rio on fps_p95 and
    # frame_time_p95_ms so a second consecutive simulated run exercises
    # the P1 transition path.
    write_raw_json ft      96  88  82  13.2 620 118
    write_raw_json wezterm 98  90  84  12.9 640 121
    write_raw_json ghostty 116 108 101 10.8 590 109
    write_raw_json rio     114 106  99 11.0 600 111
    echo "competitor-bench: simulated raw JSON in ${RAW_DIR}"
}

live_plan() {
    for tool in tmux jq; do
        command -v "${tool}" >/dev/null 2>&1 || {
            echo "competitor-bench: live mode requires ${tool}" >&2
            exit 2
        }
    done
    cat <<EOF
competitor-bench live workload plan
  release_version: ${RELEASE_VERSION}
  hardware_baseline: ${HARDWARE_BASELINE}
  runner_sku: ${RUNNER_SKU}
  terminals: ft wezterm ghostty rio
  panes_per_terminal: 50
  corpus: /usr/share/dict/words
  resize_gesture_seconds: 5
  timing_capture: ftrace on Linux, Instruments/cliclick on macOS, or pre-recorded --input-dir JSON

Live terminal driving is operator-gated. Capture each terminal into raw JSON
matching ft.competitor.resize.raw.v1, then rerun:
  scripts/competitor-bench.sh --input-dir <raw-json-dir> --release-version ${RELEASE_VERSION} --baseline ${HARDWARE_BASELINE}
EOF
}

if [[ "${MODE}" == "simulate" ]]; then
    simulate_raw_results
elif [[ "${MODE}" == "live" ]]; then
    live_plan
    exit 0
elif [[ "${MODE}" == "aggregate" ]]; then
    if [[ -z "${RAW_DIR}" || ! -d "${RAW_DIR}" ]]; then
        echo "competitor-bench: --input-dir must name a directory" >&2
        exit 2
    fi
else
    echo "competitor-bench: unsupported mode ${MODE}" >&2
    exit 2
fi

mkdir -p "${OUT_DIR}"
OUTPUT="${OUT_DIR}/competitor-resize-${RELEASE_VERSION}-${HARDWARE_BASELINE}.json"
ARGS=(
    "${SCRIPT_DIR}/competitor-bench-state.py"
    --input-dir "${RAW_DIR}"
    --release-version "${RELEASE_VERSION}"
    --hardware-baseline "${HARDWARE_BASELINE}"
    --runner-sku "${RUNNER_SKU}"
    --output "${OUTPUT}"
    --state-file "${STATE_FILE}"
)
if [[ "${FILE_P1}" == "1" ]]; then
    ARGS+=(--file-p1)
fi

python3 "${ARGS[@]}"
echo "competitor-bench: snapshot ${OUTPUT}"
