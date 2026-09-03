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
# The manifest is derived from per-crate `git log` history, so a shallow
# clone cannot reproduce it: every crate collapses to one fork-side commit
# and the drift report is a thousand lines of noise that says nothing about
# the tree. Fail closed with the actual reason instead (found by the v0.15.2
# release lane, which ran its quality gates from a `--depth 1` clone).
if [[ "$(git rev-parse --is-shallow-repository 2>/dev/null || echo unknown)" == "true" ]]; then
  echo "ft-i2eni.6: PROVENANCE.json cannot be verified from a shallow clone." >&2
  echo "           The manifest records per-crate fork history; run this gate" >&2
  echo "           from a full clone (\`git fetch --unshallow\`)." >&2
  exit 1
fi

exec python3 scripts/regen-provenance.py --check --ignore-timestamp "$@"
