#!/usr/bin/env bash
# scripts/lindley-bounds-build.sh — build the Lindley-bounds attestation
# artifact for the per-release `perf/lindley-bounds` slot.
#
# Bead: br-ft-43x69 (substrate-pass) / parent ft-rq13w.
#
# Substrate-pass scope: invokes the
# `crates/frankenterm-core/examples/lindley_bounds_build.rs` example
# (which hard-codes per-stage values from
# `docs/perf/latency-derivation.md`) and writes the canonical JSON to
# `docs/attestations/perf/lindley-bounds.json`. The attestation bundle
# build (`scripts/attestation-build.sh`) hashes that file into the
# release bundle.
#
# Wired-pass deferrals (named follow-ups under ft-43x69):
#   * Live-rate wiring (read per-stage rate + p99 latency from
#     latency_stages.rs telemetry instead of the hard-coded
#     constructor in lindley_bounds_build.rs).
#   * Bench-driven empirical_p99_ms (the example currently honours
#     FT_LINDLEY_EMPIRICAL_P99_MS; the release script will set it
#     from the bench harness output rather than the env-var default).
#   * Sigstore signing per BR-RC-FOUNDATION.G3.1 — runs after the
#     JSON lands; same shape as the existing
#     scripts/attestation-build.sh signing path.
#   * PR-CI cross-check that auto-files a regression bead via
#     `br create` when deviation_pct > 20%.
#
# Usage:
#   scripts/lindley-bounds-build.sh                       # writes file
#   FT_RELEASE_VERSION=0.2.0 scripts/lindley-bounds-build.sh
#   FT_LINDLEY_EMPIRICAL_P99_MS=42.0 scripts/lindley-bounds-build.sh \
#       --no-write    # smoke-test only; emits to stdout
#
# Exit codes:
#   0  artifact written + within_tolerance check passed
#   1  tolerance check failed (deviation_pct > TOLERANCE_PCT=20.0)
#   2  usage error / build error

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
NO_WRITE=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --no-write) NO_WRITE=1; shift ;;
    -h|--help)
      sed -n '2,32p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *) echo "unknown flag: $1" >&2; exit 2 ;;
  esac
done

cd "$REPO_ROOT"

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
