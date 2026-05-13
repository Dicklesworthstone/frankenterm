#!/usr/bin/env bash
# ft-tf6g3.29: type-check the Lean model for RuntimeProof seal soundness.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROOF="${ROOT_DIR}/docs/proofs/runtime-proof-soundness.lean"

if [[ ! -f "${PROOF}" ]]; then
  echo "runtime-proof-soundness: missing proof file: ${PROOF}" >&2
  exit 1
fi

if ! command -v lean >/dev/null 2>&1; then
  echo "runtime-proof-soundness: lean is required; install elan and rerun" >&2
  exit 1
fi

if grep -nE '^[[:space:]]*(axiom|opaque|unsafe)([[:space:]]|$)|(^|[^[:alnum:]_])(sorry|admit)([^[:alnum:]_]|$)' "${PROOF}" >&2; then
  echo "runtime-proof-soundness: proof file contains an unsupported proof escape" >&2
  exit 1
fi

lean "${PROOF}"

printf 'runtime-proof-soundness: Lean proof checked: %s\n' "${PROOF}"
