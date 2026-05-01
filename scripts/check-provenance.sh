#!/usr/bin/env bash
# ft-i2eni.6 — Provenance manifest drift guard.
#
# Re-runs scripts/regen-provenance.py in --check mode and exits 1 if
# `frankenterm/PROVENANCE.json` is stale. Fixes are simple:
#
#     bash scripts/check-provenance.sh           # detect drift
#     python3 scripts/regen-provenance.py        # rewrite the manifest
#     git add frankenterm/PROVENANCE.json
#     git commit -m "chore(provenance): refresh manifest"
#
# CI runs this on every PR (advisory initially) so a vendored-fork
# commit that doesn't refresh the manifest gets flagged.
#
# Cross-references:
#   ft-i2eni.6 — this guard's bead
#   scripts/regen-provenance.py — the generator

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
cd "${REPO_ROOT}"

# Pass --ignore-timestamp by default so a fresh regeneration that only
# differs in `generated_at` doesn't trip the gate. The generator's
# default mode (no --check) regenerates the timestamp; CI gates on
# structural drift, not on rerun freshness.
exec python3 scripts/regen-provenance.py --check --ignore-timestamp "$@"
