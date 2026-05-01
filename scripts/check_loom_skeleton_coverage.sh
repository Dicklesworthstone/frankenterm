#!/usr/bin/env bash
# ft-93mra — CI guard ensuring every primitive in runtime_async.rs has
# a corresponding Loom skeleton test.
#
# The skeleton harness shipped in ft-syqcz.6. The proof corpus
# (ft-syqcz.7, BR-RC-FOUNDATION.G8.2) only stays meaningful if it tracks
# the surface: any new primitive in runtime_async must land with a
# matching `tests/loom_<primitive>.rs` (or extend an existing file
# already listed in the manifest below). This script makes that an
# enforced invariant rather than a convention.
#
# Update the manifest below in the *same* commit that adds a primitive.
# Failure to do so will fail CI and the bead's seal-on-add gate (per
# ft-93mra's acceptance criteria).
#
# Reproduce locally:
#     bash scripts/check_loom_skeleton_coverage.sh

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
cd "${REPO_ROOT}"

RUNTIME_FILE="crates/frankenterm-core/src/runtime_async.rs"
LOOM_TESTS_DIR="crates/frankenterm-core/tests"

# Manifest of (primitive name) → (skeleton file). Each entry asserts:
#   1. the primitive definition exists in runtime_async.rs (as either
#      `pub struct <Name>` or `pub mod <name>`)
#   2. the named skeleton file exists under tests/
#
# When you add a new public primitive to runtime_async.rs, add a row here
# in the same commit. When you remove one, remove the row.
declare -a MANIFEST=(
    "Mutex|loom_sync.rs"
    "RwLock|loom_sync.rs"
    "Semaphore|loom_sync.rs"
    "mpsc|loom_mpsc.rs"
    "watch|loom_watch.rs"
    "broadcast|loom_broadcast.rs"
    "oneshot|loom_oneshot.rs"
    "notify|loom_notify.rs"
)

# Primitive declarations the manifest must cover. Any `pub struct` or
# `pub mod` matching this regex in runtime_async.rs that is *not* in the
# manifest is a new primitive without a Loom skeleton.
PRIMITIVE_PATTERN='^pub (struct (Mutex|RwLock|Semaphore)\b|mod (mpsc|watch|broadcast|oneshot|notify)\b)'

if [[ ! -f "${RUNTIME_FILE}" ]]; then
    echo "ft-93mra guard: runtime_async source missing at ${RUNTIME_FILE}" >&2
    exit 2
fi

declare -a errors=()

# ---- Check 1: every manifest entry resolves -------------------------
for entry in "${MANIFEST[@]}"; do
    name="${entry%%|*}"
    skel="${entry##*|}"
    skel_path="${LOOM_TESTS_DIR}/${skel}"

    if [[ ! -f "${skel_path}" ]]; then
        errors+=("manifest entry '${name}' points at missing skeleton: ${skel_path}")
        continue
    fi

    # The primitive must appear in runtime_async.rs as either pub struct
    # or pub mod with the manifest name.
    if ! grep -Eq "^pub struct ${name}\b|^pub mod ${name}\b" "${RUNTIME_FILE}"; then
        errors+=("manifest entry '${name}' has no matching pub struct/mod in ${RUNTIME_FILE}")
    fi
done

# ---- Check 2: no primitive is missing from the manifest -------------
declared_primitives="$(grep -E "${PRIMITIVE_PATTERN}" "${RUNTIME_FILE}" \
    | sed -E 's/^pub struct ([A-Za-z]+).*$/\1/; s/^pub mod ([a-z]+).*$/\1/' \
    | sort -u)"

manifest_names="$(printf '%s\n' "${MANIFEST[@]}" | cut -d'|' -f1 | sort -u)"

# Find declared-but-not-in-manifest entries.
missing="$(comm -23 <(printf '%s\n' "${declared_primitives}") <(printf '%s\n' "${manifest_names}") || true)"
if [[ -n "${missing}" ]]; then
    while IFS= read -r prim; do
        [[ -z "${prim}" ]] && continue
        errors+=("primitive '${prim}' declared in ${RUNTIME_FILE} has no entry in MANIFEST")
    done <<< "${missing}"
fi

# Find manifest-but-not-declared entries (shouldn't happen given Check 1
# but cheap to enumerate explicitly).
extra="$(comm -13 <(printf '%s\n' "${declared_primitives}") <(printf '%s\n' "${manifest_names}") || true)"
if [[ -n "${extra}" ]]; then
    while IFS= read -r prim; do
        [[ -z "${prim}" ]] && continue
        errors+=("manifest entry '${prim}' is no longer declared in ${RUNTIME_FILE}")
    done <<< "${extra}"
fi

# ---- Report ----------------------------------------------------------
if [[ ${#errors[@]} -eq 0 ]]; then
    echo "ft-93mra guard: every runtime_async primitive has a Loom skeleton (${#MANIFEST[@]} primitives, all manifest entries resolve)."
    exit 0
fi

cat >&2 <<EOF
─── ft-93mra guard: Loom skeleton coverage broken ───

The seal-on-add invariant requires every public primitive in
${RUNTIME_FILE} to have a matching Loom skeleton listed in the
manifest at the top of this script. The Loom proof corpus
(BR-RC-FOUNDATION.G8.2 / ft-syqcz.7) is meaningless if it drifts away
from the actual surface.

Issues found:
EOF
for err in "${errors[@]}"; do
    printf '  - %s\n' "${err}" >&2
done
cat >&2 <<EOF

What to do:
  - If you ADDED a primitive to runtime_async.rs: add a Loom skeleton at
    crates/frankenterm-core/tests/loom_<name>.rs (copy the closest
    existing skeleton — see docs/runtime/loom-conventions.md) and add a
    row to MANIFEST in this script in the same commit.
  - If you REMOVED a primitive: drop the corresponding manifest row and
    delete the now-orphan loom_<name>.rs.
  - If you RENAMED a primitive: update both runtime_async.rs and the
    manifest row in the same commit.
EOF
exit 1
