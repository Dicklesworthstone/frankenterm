#!/usr/bin/env bash
# ft-fytns — publish frankenterm-core sub-crates in topological dep order.
#
# Why this exists:
# `cargo publish` rejects `path = "../X"` deps unless the target crate
# is already on crates.io at the requested version. The 10 sub-crates
# carved out under ft-y0loj have a layered dep graph that imposes a
# strict publish order:
#
#   1. 6 leaves (no frankenterm-* deps)
#   2. frankenterm-core (depends on all 6 leaves)
#   3. 4 mid-tier (depend on frankenterm-core ± leaves)
#
# See docs/release/sub-crate-publish-order.md for the full rationale and
# the verification one-liner that confirms the graph hasn't drifted.
#
# Usage:
#   scripts/publish-sub-crates.sh --dry-run    # print commands only
#   scripts/publish-sub-crates.sh              # actually publish
#
# Requires CARGO_REGISTRY_TOKEN unless --dry-run.

set -euo pipefail

DRY_RUN=false
if [[ "${1:-}" == "--dry-run" ]]; then
    DRY_RUN=true
fi

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
cd "${REPO_ROOT}"

# Topological order. Crates within the same level are listed in
# alphabetical order for predictable output; they have no
# inter-dependencies and could be published in parallel, but we serialize
# for clearer failure reporting.
declare -a LEVEL_0=(
    "frankenterm-core-config-types"
    "frankenterm-core-error-types"
    "frankenterm-core-policy-types"
    "frankenterm-core-replay-types"
    "frankenterm-core-resource-types"
    "frankenterm-core-telemetry-types"
)
declare -a LEVEL_1=(
    "frankenterm-core"
)
declare -a LEVEL_2=(
    "frankenterm-core-ars"
    "frankenterm-core-fleet"
    "frankenterm-core-replay"
    "frankenterm-core-tantivy"
)

publish() {
    local crate="$1"
    local level="$2"
    if [[ ! -d "crates/${crate}" ]]; then
        echo "ft-fytns: missing crate dir crates/${crate}" >&2
        echo "  the sub-crate may have been renamed or the script drifted from the workspace" >&2
        return 2
    fi
    if [[ "${DRY_RUN}" == "true" ]]; then
        printf "[level %s] cargo publish -p %s\n" "${level}" "${crate}"
    else
        printf "[level %s] publishing %s ...\n" "${level}" "${crate}"
        cargo publish -p "${crate}"
        # Wait briefly so the index has time to propagate before the
        # next crate's publish attempts to resolve its dep on this one.
        # crates.io's index typically catches up within seconds, but
        # being conservative here turns "transient resolution failure"
        # into "deterministic success".
        if [[ "${level}" != "2" ]]; then
            sleep 30
        fi
    fi
}

main() {
    if [[ "${DRY_RUN}" == "false" && -z "${CARGO_REGISTRY_TOKEN:-}" ]]; then
        echo "ft-fytns: CARGO_REGISTRY_TOKEN not set; refusing to publish" >&2
        echo "  rerun with --dry-run to preview commands without publishing" >&2
        return 2
    fi

    echo "ft-fytns: sub-crate publish order (10 crates across 3 dep levels)"
    echo

    for crate in "${LEVEL_0[@]}"; do
        publish "${crate}" 0
    done

    for crate in "${LEVEL_1[@]}"; do
        publish "${crate}" 1
    done

    for crate in "${LEVEL_2[@]}"; do
        publish "${crate}" 2
    done

    echo
    if [[ "${DRY_RUN}" == "true" ]]; then
        echo "ft-fytns: dry-run complete; run without --dry-run to actually publish."
    else
        echo "ft-fytns: published 10 sub-crates in topological order."
    fi
}

main "$@"
