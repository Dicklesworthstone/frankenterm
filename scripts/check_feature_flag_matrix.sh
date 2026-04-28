#!/usr/bin/env bash
set -euo pipefail

CARGO_BIN="${CARGO_BIN:-${CARGO:-cargo}}"

run_cargo() {
  echo "+ ${CARGO_BIN} $*"
  "${CARGO_BIN}" "$@"
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
