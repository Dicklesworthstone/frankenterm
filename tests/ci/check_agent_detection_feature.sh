#!/usr/bin/env bash
set -euo pipefail

run_cargo() {
  if command -v rch >/dev/null 2>&1; then
    rch exec -- "$@"
  else
    "$@"
  fi
}

# Default features include `agent-detection`, so this validates feature-on.
run_cargo cargo check --workspace

# Validate feature-off build remains clean.
run_cargo cargo check -p frankenterm-core --no-default-features

# Validate explicit feature-on build path compiles.
run_cargo cargo check -p frankenterm-core --features agent-detection

# Validate no duplicate dependency versions after adding the new crate.
run_cargo cargo tree -d
