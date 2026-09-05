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
#
# Exit codes:
#   0  diagnostic comparison is within tolerance; not release proof
#   1  failed/undefined comparison; diagnostic JSON is retained
#   2  invalid input, RCH/build failure or invalid/missing JSON

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
NO_WRITE=0
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
