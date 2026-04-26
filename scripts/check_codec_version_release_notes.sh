#!/usr/bin/env bash
# ft-8smkj (track A of ft-kuxho) — fail CI when CODEC_VERSION is bumped
# without a corresponding row in docs/codec-versions.md.
#
# Per docs/proposals/ft-kuxho-B-codec-version-min-supported-window.md
# §3, every CODEC_VERSION bump must be paired with a release-note row
# that records the change kind (additive vs breaking) and the PDU id(s)
# affected. The guard makes that pairing mechanical instead of
# convention-driven so a silent bump cannot ship.
#
# Implementation note: rather than diffing against a base ref (which is
# fragile across local / PR / branch runs), the guard simply reads the
# *current* CODEC_VERSION from frankenterm/codec/src/lib.rs and asserts
# a row for that exact value exists in docs/codec-versions.md. Every
# version present in the source must therefore be documented — silent
# drift is impossible by construction.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
cd "${REPO_ROOT}"

CODEC_LIB="frankenterm/codec/src/lib.rs"
RELEASE_NOTES="docs/codec-versions.md"

if [[ ! -f "${CODEC_LIB}" ]]; then
    echo "ft-8smkj guard: ${CODEC_LIB} not found — repo layout drifted?" >&2
    exit 2
fi

if [[ ! -f "${RELEASE_NOTES}" ]]; then
    echo "ft-8smkj guard: ${RELEASE_NOTES} missing — required by ft-8smkj." >&2
    exit 2
fi

# Extract the current CODEC_VERSION literal. The line shape is
#   pub const CODEC_VERSION: usize = 46;
# Anchor on `pub const CODEC_VERSION` and pull the integer immediately
# after the `=`. No regex flexibility on the spacing — keep the source
# format stable.
current_version="$(awk '
    /^pub const CODEC_VERSION[ \t]*:[ \t]*usize[ \t]*=/ {
        # Match: pub const CODEC_VERSION: usize = NNN;
        for (i = 1; i <= NF; i++) {
            if ($i ~ /^[0-9]+;?$/) {
                gsub(/;/, "", $i)
                print $i
                exit
            }
        }
    }
' "${CODEC_LIB}")"

if [[ -z "${current_version}" ]]; then
    cat >&2 <<EOF
ft-8smkj guard: could not parse CODEC_VERSION from ${CODEC_LIB}.
Expected exactly one line shaped like:
    pub const CODEC_VERSION: usize = NNN;
EOF
    exit 2
fi

# A "documented" version is one that appears as the first cell of a
# table row in docs/codec-versions.md. The expected row shape is
#   | <version> | <date>   | <kind>   | <change> |
# We accept any whitespace around the version cell. Match on a leading
# `|` followed by optional whitespace, the literal version, optional
# whitespace, and a trailing `|`.
if grep -E "^\|[[:space:]]*${current_version}[[:space:]]*\|" "${RELEASE_NOTES}" >/dev/null; then
    echo "ft-8smkj guard: CODEC_VERSION=${current_version} is documented in ${RELEASE_NOTES} — clean."
    exit 0
fi

cat >&2 <<EOF
─── ft-8smkj guard: undocumented CODEC_VERSION bump detected ───

${CODEC_LIB} declares CODEC_VERSION = ${current_version}, but
${RELEASE_NOTES} has no row for that version.

Per docs/proposals/ft-kuxho-B-codec-version-min-supported-window.md §3
every CODEC_VERSION bump is a protocol change that downstream operators
need to see in release notes. The pairing is required at CI time so a
silent bump cannot ship.

To fix:
  1. Open ${RELEASE_NOTES}.
  2. Add a row at the top of the History table:
       | ${current_version} | YYYY-MM-DD | additive|breaking | <one-line summary referencing the PDU id(s) and bead> |
     - 'additive' = end-of-struct field with serde(default) or new PDU
       variant — rolling upgrade safe.
     - 'breaking' = field removal, type change, or middle-insert — must
       be paired with a CODEC_VERSION_MIN_SUPPORTED bump once
       ft-kuxho.B.1 lands.
  3. Commit the row in the same commit as the CODEC_VERSION bump.

If you are *reverting* a CODEC_VERSION change, also revert the row.
Both directions must stay in sync.
EOF
exit 1
