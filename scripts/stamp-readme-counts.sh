#!/usr/bin/env bash
# ft-i2eni.5 — Auto-stamp README/AGENTS counts via build-time queries.
#
# Replaces drift-prone hand-edited counts in README.md / AGENTS.md with
# values computed from the live workspace tree. Each tracked count is
# defined as a `(placeholder_name, command)` pair below; the script
# substitutes the live value back into the documented placeholder
# block.
#
# Placeholder syntax (HTML-comment delimited so Markdown rendering is
# unaffected and the marker is invisible):
#
#     <!--count:NAME-->VALUE<!--/count-->
#
# The script:
#  - In default (write) mode: rewrites README.md and AGENTS.md so each
#    placeholder block contains the current live value.
#  - In `--check` mode: exits 1 if any placeholder's documented value
#    drifts from the live value by more than the threshold (default
#    5%). CI uses --check as an advisory guard initially.
#
# Reproduce locally:
#     bash scripts/stamp-readme-counts.sh                  # rewrite
#     bash scripts/stamp-readme-counts.sh --check          # advisory
#     bash scripts/stamp-readme-counts.sh --check --strict # exact match
#
# Cross-references:
#   ft-d3awp / ft-hdvvo — drift incidents that motivated this work
#   ft-i2eni.5 — this bead
#   docs/contributing/auto-stamped-counts.md — placeholder convention

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
cd "${REPO_ROOT}"

MODE="write"
THRESHOLD_PCT=5
STRICT=0
for arg in "$@"; do
    case "$arg" in
        --check)   MODE="check" ;;
        --strict)  STRICT=1 ;;
        --threshold=*) THRESHOLD_PCT="${arg#--threshold=}" ;;
        --help|-h)
            sed -n '1,40p' "$0" | sed -e 's/^# \?//'
            exit 0
            ;;
        *) echo "ft-i2eni.5: unknown arg: $arg" >&2; exit 2 ;;
    esac
done

# Manifest: each entry is `name|command`. The command must produce a
# single integer to stdout. When you add a new tracked count, add the
# row here AND the matching <!--count:NAME-->...<!--/count--> blocks in
# README.md / AGENTS.md (or anywhere — the script scans both files for
# any matching placeholder names).
MANIFEST=(
    "workspace_members|awk '/^members = \\[/,/^]/' Cargo.toml | grep -c '^[[:space:]]*\"'"
    "vendored_members|awk '/^members = \\[/,/^]/' Cargo.toml | grep -c '^[[:space:]]*\"frankenterm/'"
    "vendored_top_level|find frankenterm -maxdepth 2 -name Cargo.toml | wc -l"
    "core_subcrates|ls -d crates/frankenterm-core-* | wc -l"
    "core_top_level_modules|find crates/frankenterm-core/src -maxdepth 1 -name '*.rs' | wc -l"
    "core_loc|find crates/frankenterm-core/src -name '*.rs' -exec cat {} + | wc -l"
    "test_count|grep -rE '^[[:space:]]*#\\[(test|tokio::test|asupersync_test::test)' crates/ | wc -l"
)

DOCS=(README.md AGENTS.md)

# ---- helpers ----------------------------------------------------------

# Compute one count by running its command in a subshell; trim
# whitespace; return the integer.
compute_count() {
    local cmd="$1"
    local val
    val="$(bash -c "${cmd}" 2>/dev/null || echo 0)"
    printf '%s' "${val}" | tr -d '[:space:]'
}

# Replace every <!--count:NAME-->...<!--/count--> block in a file with
# the supplied value. Uses a python here-doc to avoid sed quoting hell
# and to keep the rewrite idempotent.
rewrite_placeholders_in_file() {
    local file="$1" name="$2" value="$3"
    python3 - "${file}" "${name}" "${value}" <<'PYEOF'
import pathlib, re, sys
file, name, value = sys.argv[1], sys.argv[2], sys.argv[3]
path = pathlib.Path(file)
text = path.read_text()
pattern = re.compile(rf"(<!--count:{re.escape(name)}-->)([^<]*)(<!--/count-->)")
new = pattern.sub(lambda m: f"{m.group(1)}{value}{m.group(3)}", text)
if new != text:
    path.write_text(new)
PYEOF
}

# Read the documented value for `name` from `file` (first match wins).
# Returns empty string if no placeholder block is present.
read_documented_value() {
    local file="$1" name="$2"
    python3 - "${file}" "${name}" <<'PYEOF'
import pathlib, re, sys
file, name = sys.argv[1], sys.argv[2]
text = pathlib.Path(file).read_text()
m = re.search(rf"<!--count:{re.escape(name)}-->([^<]*)<!--/count-->", text)
print(m.group(1).strip() if m else "")
PYEOF
}

# Compute |new - old| as a percentage of max(old, 1). Returns integer
# (rounded down) percent drift.
percent_drift() {
    local old="$1" new="$2"
    python3 - "${old}" "${new}" <<'PYEOF'
import sys
old, new = int(sys.argv[1] or 0), int(sys.argv[2] or 0)
denom = max(old, 1)
print(int(abs(new - old) * 100 // denom))
PYEOF
}

# ---- main loop --------------------------------------------------------

declare -a violations=()
declare -a updates=()
total_count=0
present_count=0

for entry in "${MANIFEST[@]}"; do
    name="${entry%%|*}"
    cmd="${entry#*|}"
    live="$(compute_count "${cmd}")"
    total_count=$((total_count + 1))
    if [[ -z "${live}" ]] || ! [[ "${live}" =~ ^[0-9]+$ ]]; then
        violations+=("${name}: command failed to produce an integer (got '${live}')")
        continue
    fi
    for doc in "${DOCS[@]}"; do
        documented="$(read_documented_value "${doc}" "${name}")"
        if [[ -z "${documented}" ]]; then
            continue  # placeholder not present in this file
        fi
        present_count=$((present_count + 1))
        drift_pct="$(percent_drift "${documented}" "${live}")"
        if [[ "${MODE}" == "check" ]]; then
            if [[ "${STRICT}" -eq 1 ]]; then
                if [[ "${documented}" != "${live}" ]]; then
                    violations+=("${doc}::${name}: documented=${documented} live=${live} (strict)")
                fi
            elif [[ "${drift_pct}" -gt "${THRESHOLD_PCT}" ]]; then
                violations+=("${doc}::${name}: documented=${documented} live=${live} drift=${drift_pct}% > ${THRESHOLD_PCT}%")
            fi
        else
            if [[ "${documented}" != "${live}" ]]; then
                rewrite_placeholders_in_file "${doc}" "${name}" "${live}"
                updates+=("${doc}::${name}: ${documented} → ${live}")
            fi
        fi
    done
done

# ---- report ----------------------------------------------------------

if [[ "${MODE}" == "check" ]]; then
    if [[ ${#violations[@]} -eq 0 ]]; then
        echo "ft-i2eni.5: ${present_count} placeholder(s) within ${THRESHOLD_PCT}% drift threshold (${total_count} tracked counts)."
        exit 0
    fi
    echo "ft-i2eni.5: drift threshold exceeded for ${#violations[@]} placeholder(s):" >&2
    for v in "${violations[@]}"; do
        printf '  - %s\n' "$v" >&2
    done
    cat >&2 <<EOF

What to do:
  - Run \`bash scripts/stamp-readme-counts.sh\` (no --check) to rewrite
    README.md / AGENTS.md so the placeholder values match the live tree.
  - If the live counts moved unexpectedly, investigate the underlying
    change before stamping (the threshold is the early-warning signal).

Tracked placeholders:
EOF
    for entry in "${MANIFEST[@]}"; do
        name="${entry%%|*}"
        printf '  - <!--count:%s-->...<!--/count-->\n' "$name" >&2
    done
    exit 1
fi

if [[ ${#updates[@]} -eq 0 ]]; then
    echo "ft-i2eni.5: no updates needed (${present_count} placeholder(s) already current)."
    exit 0
fi
echo "ft-i2eni.5: updated ${#updates[@]} placeholder(s):"
for u in "${updates[@]}"; do
    printf '  - %s\n' "$u"
done
