#!/usr/bin/env bash
# scripts/lindley-bounds-build.sh — build a retained Lindley diagnostic via RCH.
#
# Bead: br-ft-43x69 (substrate-pass) / parent ft-rq13w.
#
# Invokes crates/frankenterm-core/examples/lindley_bounds_build.rs using
# direct, locked Cargo with bounded jobs in the default development profile.
# This profile is for diagnostic model/JSON calculation, with no performance
# claim. Defaults are HISTORICAL inputs; this script neither runs a benchmark
# nor promotes a release artifact.
# Logs and JSON remain in FT_LINDLEY_BOUNDS_ARTIFACT_DIR (a run-specific target
# directory by default). Bundle promotion requires separate measured evidence.
# A real FT_RELEASE_VERSION requires explicit model/empirical input and
# matching FT_LINDLEY_INPUT_SHA256; see the example for exact payload encoding.
# FT_LINDLEY_INPUT_ORIGIN is only a declared, unverified external origin.
# Telemetry JSON/file input is limited to 64 KiB of UTF-8 without NUL bytes.
#
# Usage:
#   scripts/lindley-bounds-build.sh                       # historical diagnostic
#   scripts/lindley-bounds-build.sh --stage-telemetry-json /tmp/stages.json \
#       --empirical-p99-ms 42.0 --no-write
#   scripts/lindley-bounds-build.sh --measure-live-executable /path/to/lindley_bounds_build
#     Runs an already-built native producer against the explicitly owned mux/pane.
#     No build occurs in this mode. Logs and watchdog receipt are retained in
#     ARTIFACT_DIR; FT_LINDLEY_LIVE_WATCHDOG_SECS defaults to 2400 (range 1..2400).
#     The parent DSR lane owns executable/source/profile and mux provenance.
#
# Exit codes:
#   0  diagnostic comparison is within tolerance; not release proof
#   1  failed/undefined comparison; diagnostic JSON is retained
#   2  invalid input, RCH/build failure or invalid/missing JSON

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
NO_WRITE=0
LIVE_EXECUTABLE=""
RUN_ID="${RUN_ID:-$(date -u +"%Y%m%dT%H%M%SZ")-$$}"
ARTIFACT_DIR="${FT_LINDLEY_BOUNDS_ARTIFACT_DIR:-target/lindley-bounds-build/${RUN_ID}}"
CARGO_JOBS="${FT_LINDLEY_BOUNDS_CARGO_JOBS:-1}"
[[ "$CARGO_JOBS" =~ ^([1-9]|1[0-6])$ ]] || {
  echo "FT_LINDLEY_BOUNDS_CARGO_JOBS must be between 1 and 16" >&2
  exit 2
}
DEFAULT_RCH_TARGET_DIR="target/rch-lindley-bounds-build-${RUN_ID}"
REQUESTED_RCH_TARGET_DIR="${FT_LINDLEY_BOUNDS_RCH_TARGET_DIR:-${CARGO_TARGET_DIR:-}}"
if [[ -n "$REQUESTED_RCH_TARGET_DIR" ]]; then
  RCH_TARGET_DIR="$REQUESTED_RCH_TARGET_DIR"
else
  RCH_TARGET_DIR="$DEFAULT_RCH_TARGET_DIR"
fi
RCH_SKIP_SMOKE_PREFLIGHT="${FT_LINDLEY_BOUNDS_RCH_SKIP_SMOKE_PREFLIGHT:-${RCH_SKIP_SMOKE_PREFLIGHT:-1}}"
RCH_STEP_TIMEOUT_SECS="${FT_LINDLEY_BOUNDS_RCH_TIMEOUT_SECS:-${RCH_STEP_TIMEOUT_SECS:-1800}}"
RCH_JSON_BEGIN_MARKER="__FT_LINDLEY_BOUNDS_JSON_BEGIN__"
RCH_JSON_END_MARKER="__FT_LINDLEY_BOUNDS_JSON_END__"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --no-write) NO_WRITE=1; shift ;;
    --measure-live-executable)
      [[ $# -ge 2 ]] || { echo "--measure-live-executable requires a path" >&2; exit 2; }
      LIVE_EXECUTABLE="$2"
      shift 2
      ;;
    --stage-telemetry-json)
      [[ $# -ge 2 ]] || { echo "--stage-telemetry-json requires a path" >&2; exit 2; }
      FT_LINDLEY_STAGE_TELEMETRY_PATH="$2"
      shift 2
      ;;
    --empirical-p99-ms)
      [[ $# -ge 2 ]] || { echo "--empirical-p99-ms requires a value" >&2; exit 2; }
      export FT_LINDLEY_EMPIRICAL_P99_MS="$2"
      shift 2
      ;;
    -h|--help)
      sed -n '2,/^set -/p' "$0" | sed '$d; s/^# \{0,1\}//'
      exit 0
      ;;
    *) echo "unknown flag: $1" >&2; exit 2 ;;
  esac
done

cd "$REPO_ROOT"
# shellcheck source=tests/e2e/lib_rch_guards.sh
source "$REPO_ROOT/tests/e2e/lib_rch_guards.sh"
mkdir -p "$ARTIFACT_DIR"

if [[ -n "$LIVE_EXECUTABLE" ]]; then
  # A separate process enforces wall time even while initialization or a
  # synchronous stdout write occupies the Rust executor. Files are opened
  # exclusively; reruns cannot overwrite prior diagnostic evidence.
  exec python3 - "$LIVE_EXECUTABLE" "$ARTIFACT_DIR" <<'PY'
import hashlib
import json
import os
import pathlib
import signal
import subprocess
import sys
import time

executable = pathlib.Path(sys.argv[1]).resolve(strict=True)
directory = pathlib.Path(sys.argv[2])
seconds = int(os.environ.get("FT_LINDLEY_LIVE_WATCHDOG_SECS", "2400"))
if not 1 <= seconds <= 2400 or not executable.is_file() or not os.access(executable, os.X_OK):
    raise SystemExit("invalid executable or watchdog (expected 1..2400 seconds)")
environment = os.environ.copy()
environment["FT_LINDLEY_EXTERNAL_WATCHDOG_SECS"] = str(seconds)
with executable.open("rb") as binary:
    digest = hashlib.file_digest(binary, "sha256").hexdigest()
receipt = {"schema": "frankenterm.lindley-process-watchdog.v1",
           "executable": str(executable), "executable_sha256": digest,
           "command": [str(executable), "--measure-live"],
           "watchdog_seconds": seconds, "timed_out": False,
           "settled": False, "release_ready": False}

def interrupt(_signum, _frame):
    raise KeyboardInterrupt

signal.signal(signal.SIGTERM, interrupt)
started = time.monotonic()
process = None
with (directory / "live.stdout.log").open("xb") as output, \
     (directory / "live.stderr.log").open("xb") as errors, \
     (directory / "live.watchdog.json").open("x") as retained:
    try:
        process = subprocess.Popen(receipt["command"], env=environment,
                                   stdin=subprocess.DEVNULL, stdout=output,
                                   stderr=errors, start_new_session=True)
        receipt["pid"] = process.pid
        receipt["exit_code"] = process.wait(timeout=seconds)
        receipt["settled"] = True
    except subprocess.TimeoutExpired:
        receipt["timed_out"] = True
    except (OSError, KeyboardInterrupt) as error:
        receipt["failure_class"] = type(error).__name__
    finally:
        if process is not None and not receipt["settled"]:
            for termination_signal in (signal.SIGTERM, signal.SIGKILL):
                try:
                    os.killpg(process.pid, termination_signal)
                except ProcessLookupError:
                    pass
                try:
                    receipt["exit_code"] = process.wait(timeout=5)
                    receipt["settled"] = True
                    break
                except subprocess.TimeoutExpired:
                    pass
        receipt["elapsed_seconds"] = time.monotonic() - started
        json.dump(receipt, retained, indent=2)
        retained.write("\n")
        retained.flush()

if receipt["timed_out"] or not receipt["settled"] or "failure_class" in receipt:
    print(f"live producer did not complete normally; receipt: {directory / 'live.watchdog.json'}", file=sys.stderr)
    raise SystemExit(2)
code = receipt["exit_code"]
if code not in (0, 1):
    raise SystemExit(2)
try:
    with (directory / "live.stdout.log").open("rb") as captured:
        data = captured.read(16 * 1024 * 1024 + 1)
    if len(data) > 16 * 1024 * 1024:
        raise ValueError("live producer output exceeds 16MiB")
    body = data.decode("utf-8")
    begin = "__FT_LINDLEY_BOUNDS_JSON_BEGIN__\n"
    end = "__FT_LINDLEY_BOUNDS_JSON_END__\n"
    if body.count(begin) != 1 or body.count(end) != 1 or body.index(end) < body.index(begin):
        raise ValueError("live producer lacks exactly one ordered final JSON block")
    payload = body.split(begin, 1)[1].split(end, 1)[0]
    row = json.loads(payload)
    measurement = row["measurement"]
    checks = [row["within_tolerance"], measurement["observed_delay_bound_holds"],
              measurement["arrival_envelope_holds"], *measurement["held_out_service_curves_hold"]]
    if len(checks) != 6 or any(type(check) is not bool for check in checks):
        raise ValueError("live producer checks must be six explicit booleans")
    if all(checks) != (code == 0) or measurement["release_ready"] is not False:
        raise ValueError("live producer exit/JSON contract mismatch")
    with (directory / "lindley-bounds.json").open("x") as artifact:
        artifact.write(payload)
except (OSError, ValueError, KeyError, TypeError) as error:
    print(f"live producer invalid output: {error}", file=sys.stderr)
    raise SystemExit(2) from None
print(directory / "lindley-bounds.json")
raise SystemExit(code)
PY
fi

read_bounded_telemetry() {
  python3 - "$1" "$2" <<'PY'
import os
import sys

limit = 64 * 1024
mode, source = sys.argv[1:]
try:
    if mode == "file":
        with open(source, "rb") as stream:
            payload = stream.read(limit + 1)
    else:
        payload = os.fsencode(source)
    if len(payload) > limit:
        raise ValueError("telemetry exceeds 65536-byte limit")
    if b"\x00" in payload:
        raise ValueError("telemetry must not contain NUL bytes")
    try:
        payload.decode("utf-8", errors="strict")
    except UnicodeDecodeError:
        raise ValueError("telemetry must be valid UTF-8") from None
except (OSError, ValueError) as error:
    print(f"lindley-bounds-build: {error}", file=sys.stderr)
    sys.exit(2)
# Validate before command substitution can strip NUL bytes. Trailing JSON
# whitespace may be removed by the shell; the bound applies to original bytes.
sys.stdout.buffer.write(payload)
PY
}

if [[ ${FT_LINDLEY_STAGE_TELEMETRY_PATH+x} ]]; then
  [[ ! ${FT_LINDLEY_STAGE_TELEMETRY_JSON+x} ]] || {
    echo "supply one telemetry input: JSON or PATH, not both" >&2
    exit 2
  }
  [[ -f "$FT_LINDLEY_STAGE_TELEMETRY_PATH" && -r "$FT_LINDLEY_STAGE_TELEMETRY_PATH" ]] || {
    echo "stage telemetry path must name a readable file" >&2
    exit 2
  }
  # The local path is not a path on the remote worker. Forward bounded JSON.
  FT_LINDLEY_STAGE_TELEMETRY_JSON="$(read_bounded_telemetry file "$FT_LINDLEY_STAGE_TELEMETRY_PATH")" || exit 2
elif [[ ${FT_LINDLEY_STAGE_TELEMETRY_JSON+x} ]]; then
  FT_LINDLEY_STAGE_TELEMETRY_JSON="$(read_bounded_telemetry json "$FT_LINDLEY_STAGE_TELEMETRY_JSON")" || exit 2
fi

extract_rch_json() {
  local input_log="$1"
  local out_json="$2"

  awk -v begin="$RCH_JSON_BEGIN_MARKER" -v end="$RCH_JSON_END_MARKER" '
    $0 == begin { if (started || ended) exit 1; started = 1; capturing = 1; next }
    $0 == end { if (!capturing || ended) exit 1; ended = 1; capturing = 0; next }
    capturing { print }
    END { if (!started || !ended || capturing) exit 1 }
  ' "$input_log" >"$out_json"
}

rch_log="$ARTIFACT_DIR/lindley_bounds_${RUN_ID}.rch.log"
artifact_json="$ARTIFACT_DIR/lindley-bounds.json"

rch_init "$ARTIFACT_DIR" "$RUN_ID" "lindley_bounds_build" "$REPO_ROOT"
set +e
(set -e; ensure_rch_ready)
preflight_status=$?
set -e
if [[ $preflight_status -ne 0 ]]; then
  echo "lindley-bounds-build: RCH preflight failed; see $ARTIFACT_DIR" >&2
  exit 2
fi

# Only deliberate inputs cross the command boundary. In particular, absence
# stays absence; an explicit empty value reaches the example and is rejected.
remote_env=("CARGO_TARGET_DIR=$RCH_TARGET_DIR" "FT_LINDLEY_BOUNDS_EMIT_JSON_MARKERS=1")
for input_key in FT_RELEASE_VERSION FT_LINDLEY_STAGE_TELEMETRY_JSON \
  FT_LINDLEY_EMPIRICAL_P99_MS FT_LINDLEY_INPUT_SHA256 FT_LINDLEY_INPUT_ORIGIN; do
  if [[ ${!input_key+x} ]]; then
    remote_env+=("$input_key=${!input_key}")
  fi
done

echo "lindley-bounds-build: development profile; diagnostic calculation only, no performance claim" >&2
set +e
(run_rch_cargo_logged "$rch_log" \
  env "${remote_env[@]}" \
    cargo run --locked -j "$CARGO_JOBS" --example lindley_bounds_build \
      -p frankenterm-core --no-default-features --quiet)
ec=$?
set -e

extract_status=0
extract_rch_json "$rch_log" "$artifact_json" || extract_status=$?

if [[ $ec -ne 0 && $ec -ne 1 ]]; then
  echo "lindley-bounds-build: example invocation failed (exit $ec); see $rch_log" >&2
  exit 2
fi

if [[ $extract_status -ne 0 ]] || ! jq -e --argjson exit_code "$ec" '
  type == "object"
  and (.within_tolerance | type == "boolean")
  and (.within_tolerance == ($exit_code == 0))
  and (.input_provenance.release_ready == false)
  and (.input_provenance.measurement_provenance_verified == false)
' "$artifact_json" >/dev/null; then
  echo "lindley-bounds-build: missing/invalid diagnostic or exit/JSON mismatch; see $rch_log" >&2
  exit 2
fi

echo "lindley-bounds-build: retained diagnostic $artifact_json (exit $ec)" >&2
if [[ $NO_WRITE -eq 1 ]]; then
  cat "$artifact_json"
else
  echo "$artifact_json"
fi
exit "$ec"
