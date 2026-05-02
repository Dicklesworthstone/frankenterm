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

# Build + run the example. Output goes to a temp file so the tolerance
# check on the example's exit code can fail without leaving a
# partially-written attestation file on disk.
tmp_out="$(mktemp)"
trap 'rm -f "$tmp_out"' EXIT

if ! cargo run --release --example lindley_bounds_build \
      -p frankenterm-core --no-default-features --quiet \
      > "$tmp_out"; then
  ec=$?
  if [[ $ec -eq 1 ]]; then
    # Tolerance check failed — print the artifact to stderr for
    # diagnostic visibility and propagate the example's exit code.
    cat "$tmp_out" >&2
    exit 1
  fi
  echo "lindley-bounds-build: example invocation failed (exit $ec)" >&2
  exit 2
fi

if [[ $NO_WRITE -eq 1 ]]; then
  cat "$tmp_out"
  exit 0
fi

out="docs/attestations/perf/lindley-bounds.json"
mkdir -p "$(dirname "$out")"
mv "$tmp_out" "$out"
trap - EXIT

echo "lindley-bounds-build: wrote $out" >&2
echo "$out"
