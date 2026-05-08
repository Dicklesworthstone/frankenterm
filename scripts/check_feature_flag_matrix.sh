#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
ARTIFACT_DIR="${FEATURE_FLAG_MATRIX_ARTIFACT_DIR:-target/feature-flag-matrix}"
RUN_ID="${FEATURE_FLAG_MATRIX_RUN_ID:-$(date -u +"%Y%m%d_%H%M%S")-$$}"
DEFAULT_RCH_TARGET_DIR="target/rch-feature-flag-matrix-${RUN_ID}"
REQUESTED_RCH_TARGET_DIR="${FEATURE_FLAG_MATRIX_TARGET_DIR:-}"
if [[ -n "$REQUESTED_RCH_TARGET_DIR" && "$REQUESTED_RCH_TARGET_DIR" != /* ]]; then
  RCH_TARGET_DIR="$REQUESTED_RCH_TARGET_DIR"
else
  RCH_TARGET_DIR="$DEFAULT_RCH_TARGET_DIR"
fi
RUN_CARGO_STEP=0

cd "$PROJECT_ROOT"
mkdir -p "$ARTIFACT_DIR"

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "$PROJECT_ROOT/tests/e2e/lib_rch_guards.sh"
rch_init "$ARTIFACT_DIR" "$RUN_ID" "feature_flag_matrix" "$PROJECT_ROOT"
ensure_rch_ready

run_cargo() {
  ((RUN_CARGO_STEP++)) || true
  local step_name
  printf -v step_name 'step_%02d' "$RUN_CARGO_STEP"
  local log_file="$ARTIFACT_DIR/${step_name}.log"

  echo "+ cargo $*"
  run_rch_cargo_logged "$log_file" env CARGO_TARGET_DIR="$RCH_TARGET_DIR" cargo "$@"
}

run_cargo check -p frankenterm-core --no-default-features --lib

run_cargo check -p frankenterm-core --no-default-features --lib \
  --features asupersync-runtime,vendored,native-wezterm

run_cargo check -p frankenterm-core --no-default-features --lib \
  --features asupersync-runtime,distributed,metrics

run_cargo check -p frankenterm-core --no-default-features --lib \
  --features recorder-lexical,frankensearch,semantic-search

run_cargo check -p frankenterm --bin ft --no-default-features \
  --features frankenterm,asupersync-runtime,mcp,web,browser,metrics,distributed,subprocess-bridge,sync

if [[ "$(uname -s)" == "Linux" ]]; then
  run_cargo check -p frankenterm-mux-server --no-default-features --features io-uring
else
  echo "+ skip frankenterm-mux-server io-uring combo: Linux-only CI lane"
fi

run_cargo check -p frankenterm-gui --no-default-features --features headless-render
