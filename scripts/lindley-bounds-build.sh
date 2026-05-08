#!/usr/bin/env bash
# scripts/lindley-bounds-build.sh — build the Lindley-bounds attestation
# artifact for the per-release `perf/lindley-bounds` slot.
#
# Bead: br-ft-43x69 (substrate-pass) / parent ft-rq13w.
#
# Wired scope: invokes the
# `crates/frankenterm-core/examples/lindley_bounds_build.rs` example
# (which consumes live latency-stage telemetry when supplied, otherwise
# falls back to `docs/perf/latency-derivation.md`) and writes the canonical JSON to
# `docs/attestations/perf/lindley-bounds.json`. The attestation bundle
# build (`scripts/attestation-build.sh`) hashes that file into the
# release bundle.
#
# Remaining release-orchestration hooks:
#   * Sigstore signing per BR-RC-FOUNDATION.G3.1 — runs after the
#     JSON lands; same shape as the existing
#     scripts/attestation-build.sh signing path.
#   * PR-CI cross-check that auto-files a regression bead via
#     `br create` when deviation_pct > 20%.
#
# Usage:
#   scripts/lindley-bounds-build.sh                       # writes file
#   FT_RELEASE_VERSION=0.2.0 scripts/lindley-bounds-build.sh
#   scripts/lindley-bounds-build.sh --stage-telemetry-json /tmp/stages.json \
#       --empirical-p99-ms 42.0 --no-write
#
# Exit codes:
#   0  artifact written + within_tolerance check passed
#   1  tolerance check failed (deviation_pct > TOLERANCE_PCT=20.0)
#   2  usage error / build error

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
NO_WRITE=0
STAGE_TELEMETRY_JSON=""
EMPIRICAL_P99_MS=""
RUN_ID="${RUN_ID:-$(date -u +"%Y%m%dT%H%M%SZ")-$$}"
ARTIFACT_DIR="${FT_LINDLEY_BOUNDS_ARTIFACT_DIR:-target/lindley-bounds-build}"
DEFAULT_RCH_TARGET_DIR="target/rch-lindley-bounds-build-${RUN_ID}"
REQUESTED_RCH_TARGET_DIR="${FT_LINDLEY_BOUNDS_RCH_TARGET_DIR:-${CARGO_TARGET_DIR:-}}"
if [[ -n "$REQUESTED_RCH_TARGET_DIR" && "$REQUESTED_RCH_TARGET_DIR" != /* ]]; then
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
      STAGE_TELEMETRY_JSON="$2"
      shift 2
      ;;
    --empirical-p99-ms)
      [[ $# -ge 2 ]] || { echo "--empirical-p99-ms requires a value" >&2; exit 2; }
      EMPIRICAL_P99_MS="$2"
      shift 2
      ;;
    -h|--help)
      sed -n '2,32p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *) echo "unknown flag: $1" >&2; exit 2 ;;
  esac
done

cd "$REPO_ROOT"
# shellcheck source=tests/e2e/lib_rch_guards.sh
source "$REPO_ROOT/tests/e2e/lib_rch_guards.sh"
mkdir -p "$ARTIFACT_DIR"

if [[ -n "$STAGE_TELEMETRY_JSON" ]]; then
  [[ -r "$STAGE_TELEMETRY_JSON" ]] || {
    echo "stage telemetry file is not readable: $STAGE_TELEMETRY_JSON" >&2
    exit 2
  }
  export FT_LINDLEY_STAGE_TELEMETRY_JSON
  FT_LINDLEY_STAGE_TELEMETRY_JSON="$(cat "$STAGE_TELEMETRY_JSON")"
fi

if [[ -n "$EMPIRICAL_P99_MS" ]]; then
  export FT_LINDLEY_EMPIRICAL_P99_MS="$EMPIRICAL_P99_MS"
fi

extract_rch_json() {
  local input_log="$1"
  local out_json="$2"

  awk -v begin="$RCH_JSON_BEGIN_MARKER" -v end="$RCH_JSON_END_MARKER" '
    $0 == begin { capturing = 1; next }
    $0 == end { found_end = 1; exit }
    capturing { print }
    END { if (!found_end) exit 1 }
  ' "$input_log" >"$out_json"
}

rch_log="$ARTIFACT_DIR/lindley_bounds_${RUN_ID}.rch.log"
artifact_json="$ARTIFACT_DIR/lindley-bounds.json"

rch_init "$ARTIFACT_DIR" "$RUN_ID" "lindley_bounds_build" "$REPO_ROOT"
ensure_rch_ready

set +e
# shellcheck disable=SC2016
run_rch_cargo_logged "$rch_log" \
  env CARGO_TARGET_DIR="$RCH_TARGET_DIR" \
    FT_LINDLEY_STAGE_TELEMETRY_JSON="${FT_LINDLEY_STAGE_TELEMETRY_JSON:-}" \
    FT_LINDLEY_EMPIRICAL_P99_MS="${FT_LINDLEY_EMPIRICAL_P99_MS:-}" \
    FT_LINDLEY_BOUNDS_JSON_BEGIN="$RCH_JSON_BEGIN_MARKER" \
    FT_LINDLEY_BOUNDS_JSON_END="$RCH_JSON_END_MARKER" \
    bash -lc '
      set -euo pipefail
      json_begin="${FT_LINDLEY_BOUNDS_JSON_BEGIN:?}"
      json_end="${FT_LINDLEY_BOUNDS_JSON_END:?}"
      remote_artifact_dir="${CARGO_TARGET_DIR}/ft-lindley-bounds-build"
      mkdir -p "$remote_artifact_dir"
      remote_json="${remote_artifact_dir}/lindley-bounds.json"
      set +e
      cargo run --release --example lindley_bounds_build \
        -p frankenterm-core --no-default-features --quiet \
        >"$remote_json"
      rc=$?
      set -e
      printf "%s\n" "$json_begin"
      if [[ -f "$remote_json" ]]; then
        cat "$remote_json"
      fi
      printf "\n%s\n" "$json_end"
      exit "$rc"
    '
ec=$?
set -e

extract_status=0
extract_rch_json "$rch_log" "$artifact_json" || extract_status=$?

if [[ $ec -ne 0 ]]; then
  if [[ $ec -eq 1 && $extract_status -eq 0 ]]; then
    # Tolerance check failed — print the artifact to stderr for
    # diagnostic visibility and propagate the example's exit code.
    cat "$artifact_json" >&2
    exit 1
  fi
  echo "lindley-bounds-build: example invocation failed (exit $ec)" >&2
  exit 2
fi

if [[ $extract_status -ne 0 ]]; then
  echo "lindley-bounds-build: failed to extract JSON artifact from $rch_log" >&2
  exit 2
fi

if [[ $NO_WRITE -eq 1 ]]; then
  cat "$artifact_json"
  exit 0
fi

out="docs/attestations/perf/lindley-bounds.json"
mkdir -p "$(dirname "$out")"
cp "$artifact_json" "$out"

echo "lindley-bounds-build: wrote $out" >&2
echo "$out"
