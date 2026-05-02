#!/usr/bin/env bash
set -euo pipefail

# End-to-end wrapper for the frankenterm-gui GPU golden regression harness.
#
# Usage:
#   scripts/test-gpu-harness.sh
#   scripts/test-gpu-harness.sh -- --headless-render-self-test
#
# The script mirrors the CI entrypoint planned for ft-ombfl.13:
# - creates a per-run artifact directory under /tmp/gpu-harness-<timestamp>
# - runs the harness through cargo test with headless rendering enabled
# - captures combined cargo/harness output in run.log
# - extracts structured harness JSON lines into events.jsonl
# - copies failure actual/diff PNGs into diffs/
# - writes summary.json plus render-parity-gpu.json and prints a concise stdout summary
#
# Environment:
#   CARGO_BIN              cargo executable (default: cargo)
#   GPU_HARNESS_RUN_DIR    run artifact directory override
#   GPU_HARNESS_CARGO_ARGS extra cargo args before "--" (space-separated)

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CARGO_BIN="${CARGO_BIN:-cargo}"

RUN_ID="$(date -u +%Y%m%dT%H%M%SZ)-$$"
RUN_DIR="${GPU_HARNESS_RUN_DIR:-/tmp/gpu-harness-$RUN_ID}"
LOG_FILE="$RUN_DIR/run.log"
EVENTS_JSONL="$RUN_DIR/events.jsonl"
ARTIFACT_DIR="$RUN_DIR/artifacts"
DIFF_DIR="$RUN_DIR/diffs"
SUMMARY_JSON="$RUN_DIR/summary.json"
PARITY_REPORT_JSON="$RUN_DIR/render-parity-gpu.json"
PERF_REPORT="$RUN_DIR/perf-report.json"

declare -a EXTRA_CARGO_ARGS=()
if [[ -n "${GPU_HARNESS_CARGO_ARGS:-}" ]]; then
  # shellcheck disable=SC2206
  EXTRA_CARGO_ARGS=(${GPU_HARNESS_CARGO_ARGS})
fi

declare -a HARNESS_ARGS=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    -h|--help)
      sed -n '2,32p' "$0"
      exit 0
      ;;
    --)
      shift
      HARNESS_ARGS+=("$@")
      break
      ;;
    *)
      HARNESS_ARGS+=("$1")
      shift
      ;;
  esac
done

json_string() {
  python3 - "$1" <<'PY'
import json
import sys
print(json.dumps(sys.argv[1]))
PY
}

emit_setup() {
  printf '{"phase":"setup","run_id":%s,"tmpdir":%s,"log":%s,"events":%s,"artifacts":%s}\n' \
    "$(json_string "$RUN_ID")" \
    "$(json_string "$RUN_DIR")" \
    "$(json_string "$LOG_FILE")" \
    "$(json_string "$EVENTS_JSONL")" \
    "$(json_string "$ARTIFACT_DIR")" >&2
}

emit_run() {
  local exit_code="$1"
  printf '{"phase":"run","exit_code":%d,"log":%s}\n' \
    "$exit_code" \
    "$(json_string "$LOG_FILE")" >&2
}

emit_collect_diffs() {
  local count="$1"
  printf '{"phase":"collect-diffs","count":%d,"dest":%s}\n' \
    "$count" \
    "$(json_string "$DIFF_DIR")" >&2
}

emit_summary_json() {
  printf '{"phase":"summary-json","path":%s}\n' "$(json_string "$SUMMARY_JSON")" >&2
}

emit_parity_report_json() {
  printf '{"phase":"parity-report-json","path":%s}\n' "$(json_string "$PARITY_REPORT_JSON")" >&2
}

now_ms() {
  python3 - <<'PY'
import time
print(int(time.time() * 1000))
PY
}

extract_json_lines() {
  python3 - "$LOG_FILE" "$EVENTS_JSONL" <<'PY'
import json
import sys
from pathlib import Path

log_path = Path(sys.argv[1])
events_path = Path(sys.argv[2])
events_path.parent.mkdir(parents=True, exist_ok=True)

with log_path.open("r", encoding="utf-8", errors="replace") as source, events_path.open(
    "w", encoding="utf-8"
) as dest:
    for raw in source:
        line = raw.strip()
        if not (line.startswith("{") and line.endswith("}")):
            continue
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        dest.write(json.dumps(event, separators=(",", ":"), sort_keys=True))
        dest.write("\n")
PY
}

collect_diff_artifacts() {
  mkdir -p "$DIFF_DIR"
  local count=0
  if [[ -d "$ARTIFACT_DIR" ]]; then
    while IFS= read -r -d '' artifact; do
      cp "$artifact" "$DIFF_DIR/"
      count=$((count + 1))
    done < <(
      find "$ARTIFACT_DIR" -maxdepth 1 -type f \
        \( -name '*.actual.png' -o -name '*.diff.png' -o -name '*.report.json' \) \
        -print0
    )
  fi
  emit_collect_diffs "$count"
}

write_summary() {
  local exit_code="$1"
  local duration_ms="$2"
  python3 - "$EVENTS_JSONL" "$SUMMARY_JSON" "$PARITY_REPORT_JSON" "$RUN_ID" "$RUN_DIR" "$ARTIFACT_DIR" "$DIFF_DIR" "$PROJECT_ROOT" "$PERF_REPORT" "$LOG_FILE" "$exit_code" "$duration_ms" <<'PY'
import json
import sys
from pathlib import Path

(
    events_path,
    summary_path,
    parity_report_path,
    run_id,
    run_dir,
    artifact_dir,
    diff_dir,
    project_root,
    perf_report,
    log_file,
    exit_code,
    duration_ms,
) = sys.argv[1:]

events = []
if Path(events_path).exists():
    with Path(events_path).open("r", encoding="utf-8") as source:
        for raw in source:
            raw = raw.strip()
            if raw:
                events.append(json.loads(raw))

summary_events = [
    event
    for event in events
    if event.get("phase") == "summary"
    and {"total", "passed", "failed"}.issubset(event.keys())
]
summary_event = summary_events[-1] if summary_events else {}

fixture_events = [
    event
    for event in events
    if event.get("phase") == "fixture" and event.get("status") in {"pass", "fail"}
]
failures = []
seen_failure_fixtures = set()
for event in fixture_events:
    if event.get("status") != "fail":
        continue
    fixture = event["name"]
    actual = Path(diff_dir) / f"{fixture}.actual.png"
    diff = Path(diff_dir) / f"{fixture}.diff.png"
    if not actual.exists():
        actual = Path(artifact_dir) / f"{fixture}.actual.png"
    if not diff.exists():
        diff = Path(artifact_dir) / f"{fixture}.diff.png"
    failures.append(
        {
            "fixture": fixture,
            "ssim": event.get("ssim"),
            "linf": event.get("linf"),
            "changed_pixels": event.get("changed_pixels"),
            "changed_pixel_fraction": event.get("changed_pixel_fraction"),
            "golden": str(Path(project_root) / "tests" / "golden" / "gpu" / fixture / "golden.png"),
            "actual": str(actual),
            "diff": str(diff),
        }
    )
    seen_failure_fixtures.add(fixture)

for report_path in sorted(Path(artifact_dir).glob("*.report.json")):
    try:
        report = json.loads(report_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        continue
    fixture = report.get("fixture")
    if not fixture or fixture in seen_failure_fixtures:
        continue
    metrics = report.get("metrics") or {}
    actual = Path(diff_dir) / f"{fixture}.actual.png"
    diff = Path(diff_dir) / f"{fixture}.diff.png"
    if not actual.exists():
        actual = Path(artifact_dir) / f"{fixture}.actual.png"
    if not diff.exists():
        diff = Path(artifact_dir) / f"{fixture}.diff.png"
    failures.append(
        {
            "fixture": fixture,
            "ssim": metrics.get("ssim"),
            "linf": metrics.get("l_inf"),
            "changed_pixels": metrics.get("changed_pixels"),
            "changed_pixel_fraction": metrics.get("changed_pixel_fraction"),
            "golden": str(Path(project_root) / "tests" / "golden" / "gpu" / fixture / "golden.png"),
            "actual": str(actual),
            "diff": str(diff),
        }
    )
    seen_failure_fixtures.add(fixture)

total = summary_event.get("total", len(fixture_events))
failed = summary_event.get("failed", len(failures))
passed = summary_event.get("passed", max(0, total - failed))

summary = {
    "run_id": run_id,
    "total": total,
    "passed": passed,
    "failed": failed,
    "duration_ms": int(duration_ms),
    "exit_code": int(exit_code),
    "failures": failures,
    "artifacts": {
        "run_dir": run_dir,
        "log": log_file,
        "events_jsonl": events_path,
        "artifact_dir": artifact_dir,
        "diff_dir": diff_dir,
        "perf_report": perf_report,
    },
}

Path(summary_path).write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8")

parity_report = {
    "schema_version": "1.0.0",
    "category": "tui/render-parity",
    "kind": "headless-gpu-renderer-parity",
    "produced_by_bead": "ft-35yac.1.2",
    "run_id": run_id,
    "result": {
        "total": total,
        "passed": passed,
        "failed": failed,
        "duration_ms": int(duration_ms),
        "exit_code": int(exit_code),
    },
    "default_thresholds": {
        "min_ssim": 0.99,
        "max_l_inf": 8,
        "max_changed_pixel_fraction": 0.001,
        "source": "crates/frankenterm-gui/src/gpu_regression.rs::Thresholds::default",
    },
    "threshold_semantics": "pass requires ssim >= min_ssim AND l_inf <= max_l_inf AND changed_pixel_fraction <= max_changed_pixel_fraction; fixture meta.json may tighten or loosen these defaults",
    "ci_contract": {
        "nightly_trigger": ".github/workflows/ci.yml schedule",
        "hard_gate_job": "gpu-regression-macos",
        "hard_gate_runner": "macos-15",
        "soft_pilot_job": "gpu-linux-llvmpipe-pilot",
        "stable_required_check": "GPU Regression Required",
    },
    "artifacts": {
        "run_dir": run_dir,
        "log": log_file,
        "events_jsonl": events_path,
        "summary_json": summary_path,
        "artifact_dir": artifact_dir,
        "diff_dir": diff_dir,
        "perf_report": perf_report,
    },
    "failure_artifact_globs": [
        f"{diff_dir}/*.actual.png",
        f"{diff_dir}/*.diff.png",
        f"{diff_dir}/*.report.json",
    ],
    "failures": failures,
}
Path(parity_report_path).write_text(
    json.dumps(parity_report, indent=2, sort_keys=True) + "\n",
    encoding="utf-8",
)

print(
    f"GPU harness: total={total} passed={passed} failed={failed} "
    f"exit_code={exit_code} duration_ms={duration_ms}"
)
print(f"Artifacts: {run_dir}")
print(f"Summary: {summary_path}")
print(f"GPU renderer parity report: {parity_report_path}")
if failures:
    print("Diff artifacts:")
    for failure in failures:
        print(f"  {failure['fixture']}: {failure['diff']}")

exit_event = {
    "phase": "exit",
    "code": int(exit_code),
    "failed_fixtures": [failure["fixture"] for failure in failures],
    "parity_report": parity_report_path,
    "summary": summary_path,
}
print(json.dumps(exit_event, separators=(",", ":"), sort_keys=True), file=sys.stderr)
PY
}

mkdir -p "$RUN_DIR" "$ARTIFACT_DIR"
start_ms="$(now_ms)"
emit_setup

set +e
(
  cd "$PROJECT_ROOT"
  env GPU_HARNESS_ARTIFACT_DIR="$ARTIFACT_DIR" GPU_HARNESS_PERF_REPORT="$PERF_REPORT" \
    "$CARGO_BIN" test \
      -p frankenterm-gui \
      --features headless-render \
      --test gpu_regression \
      "${EXTRA_CARGO_ARGS[@]}" \
      -- \
      "${HARNESS_ARGS[@]}"
) > >(tee "$LOG_FILE" >&2) 2>&1
run_exit_code=$?
set -e

end_ms="$(now_ms)"
duration_ms=$((end_ms - start_ms))
emit_run "$run_exit_code"
extract_json_lines
collect_diff_artifacts
write_summary "$run_exit_code" "$duration_ms"
emit_summary_json
emit_parity_report_json

exit "$run_exit_code"
