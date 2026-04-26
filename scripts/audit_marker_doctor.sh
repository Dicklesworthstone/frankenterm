#!/usr/bin/env bash
# audit_marker_doctor.sh — flag stale audit-sweep markers in agent memory.
#
# Walks a memory directory of `project_*sweep*.md` (and similar) files, reads
# the SHA-anchored frontmatter that ft-hph8i backfilled, and asks git how many
# commits the swept paths have received since the sweep ran. Markers whose
# count exceeds `stale_after_commits` are flagged STALE.
#
# Bead: ft-nedq3 (parent ft-gkqej / docs/proposals/audit-marker-staleness.md)
#
# Usage:
#   scripts/audit_marker_doctor.sh [--json] [--dir PATH]
#
# --json     emit machine-readable JSON instead of plain text
# --dir PATH override the default memory directory (defaults to
#            ~/.claude/projects/-Users-jemanuel-projects-frankenterm/memory)
#
# Exit code:
#   0  if no STALE markers
#   1  if at least one marker is STALE (suitable for CI gates)
#   2  if no markers were found (probably a wrong --dir)
#
# Frontmatter shape this reads (see docs/proposals/audit-marker-staleness.md):
#
#   ---
#   name: <marker-name>
#   swept_at_sha: <full-or-short-git-sha>
#   sweep_paths:
#     - <path-1>
#     - <path-2>
#   stale_after_commits: <integer>
#   findings_summary: <free text>
#   ---

set -euo pipefail

DEFAULT_DIR="$HOME/.claude/projects/-Users-jemanuel-projects-frankenterm/memory"
MEMORY_DIR="$DEFAULT_DIR"
EMIT_JSON=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --json) EMIT_JSON=1; shift ;;
        --dir)  MEMORY_DIR="$2"; shift 2 ;;
        -h|--help)
            sed -n '2,30p' "$0" | sed 's/^# \?//'
            exit 0
            ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

if [[ ! -d "$MEMORY_DIR" ]]; then
    echo "audit_marker_doctor: memory dir not found: $MEMORY_DIR" >&2
    exit 2
fi

# Walk every markdown file that has a `swept_at_sha:` line in its frontmatter.
# This is intentionally tolerant: any audit marker that backfilled the
# frontmatter under ft-hph8i (or a future sweep that emits the same shape)
# gets picked up automatically — the doctor doesn't need an allowlist.
mapfile -t MARKER_FILES < <(grep -rl '^swept_at_sha:' "$MEMORY_DIR" --include='*.md' 2>/dev/null | sort)

if [[ ${#MARKER_FILES[@]} -eq 0 ]]; then
    echo "audit_marker_doctor: no SHA-anchored markers found under $MEMORY_DIR" >&2
    exit 2
fi

# parse_frontmatter <file> — emit `name`, `sha`, `threshold`, then one path per line.
# Uses awk so we don't need a YAML lib. The frontmatter must end with a `---`
# line; sweep_paths must be a list of `  - <path>` lines immediately after the
# `sweep_paths:` key.
parse_frontmatter() {
    local file="$1"
    awk '
        BEGIN { in_fm = 0; in_paths = 0 }
        /^---$/ {
            if (in_fm == 0) { in_fm = 1; next }
            else { exit }
        }
        in_fm == 0 { next }
        /^name:/                 { sub(/^name:[[:space:]]*/, ""); name = $0 }
        /^swept_at_sha:/         { sub(/^swept_at_sha:[[:space:]]*/, ""); sha = $0 }
        /^stale_after_commits:/  { sub(/^stale_after_commits:[[:space:]]*/, ""); threshold = $0 }
        /^sweep_paths:/          { in_paths = 1; next }
        in_paths == 1 && /^[[:space:]]*-[[:space:]]/ {
            sub(/^[[:space:]]*-[[:space:]]*/, "")
            paths[++n] = $0
            next
        }
        in_paths == 1 { in_paths = 0 }
        END {
            print "name=" name
            print "sha="  sha
            print "threshold=" threshold
            for (i = 1; i <= n; i++) print "path=" paths[i]
        }
    ' "$file"
}

# Collect verdicts in arrays for both output modes.
declare -a NAMES SHAS THRESHOLDS COUNTS VERDICTS FILES PATHS_JSON
STALE_TOTAL=0

for file in "${MARKER_FILES[@]}"; do
    name=""; sha=""; threshold=""; declare -a paths=()
    while IFS= read -r line; do
        case "$line" in
            name=*)      name="${line#name=}" ;;
            sha=*)       sha="${line#sha=}" ;;
            threshold=*) threshold="${line#threshold=}" ;;
            path=*)      paths+=("${line#path=}") ;;
        esac
    done < <(parse_frontmatter "$file")

    if [[ -z "$sha" || -z "$threshold" || ${#paths[@]} -eq 0 ]]; then
        echo "audit_marker_doctor: skipping malformed marker: $file" >&2
        continue
    fi

    # `git rev-list --count <sha>..HEAD -- <paths>` counts commits on HEAD
    # that touched any of the swept paths since the sweep was run.
    if ! count=$(git -C "$(git rev-parse --show-toplevel)" rev-list --count "${sha}..HEAD" -- "${paths[@]}" 2>/dev/null); then
        echo "audit_marker_doctor: rev-list failed for $name (sha $sha) — likely the sha is not in the local history" >&2
        continue
    fi

    if (( count > threshold )); then
        verdict="STALE"
        # Use $(( … )) assignment instead of `((var++))` — under `set -e`
        # the post-increment expression returns exit-status 1 when the
        # pre-increment value was 0, killing the loop after the first
        # STALE marker. Classic bash gotcha.
        STALE_TOTAL=$((STALE_TOTAL + 1))
    else
        verdict="FRESH"
    fi

    NAMES+=("$name")
    SHAS+=("$sha")
    THRESHOLDS+=("$threshold")
    COUNTS+=("$count")
    VERDICTS+=("$verdict")
    FILES+=("$file")

    # Build a JSON array fragment of paths for the --json branch.
    paths_json="["
    for ((i = 0; i < ${#paths[@]}; i++)); do
        [[ $i -gt 0 ]] && paths_json+=","
        paths_json+="\"${paths[i]//\"/\\\"}\""
    done
    paths_json+="]"
    PATHS_JSON+=("$paths_json")
done

# Emit results.
if (( EMIT_JSON == 1 )); then
    printf '{\n'
    printf '  "memory_dir": "%s",\n' "${MEMORY_DIR//\"/\\\"}"
    printf '  "total_markers": %d,\n' "${#NAMES[@]}"
    printf '  "stale_count": %d,\n' "$STALE_TOTAL"
    printf '  "markers": [\n'
    for ((i = 0; i < ${#NAMES[@]}; i++)); do
        sep=","
        [[ $i -eq $((${#NAMES[@]} - 1)) ]] && sep=""
        printf '    {\n'
        printf '      "name": "%s",\n' "${NAMES[i]//\"/\\\"}"
        printf '      "file": "%s",\n' "${FILES[i]//\"/\\\"}"
        printf '      "swept_at_sha": "%s",\n' "${SHAS[i]}"
        printf '      "sweep_paths": %s,\n' "${PATHS_JSON[i]}"
        printf '      "stale_after_commits": %s,\n' "${THRESHOLDS[i]}"
        printf '      "commits_since_sweep": %s,\n' "${COUNTS[i]}"
        printf '      "verdict": "%s"\n' "${VERDICTS[i]}"
        printf '    }%s\n' "$sep"
    done
    printf '  ]\n}\n'
else
    # Plain-text table.
    printf 'audit_marker_doctor — %s\n' "$MEMORY_DIR"
    printf '%-50s %-10s %-8s %-7s %s\n' "marker" "verdict" "commits" "thresh" "sha"
    printf -- '-%.0s' {1..100}; printf '\n'
    for ((i = 0; i < ${#NAMES[@]}; i++)); do
        printf '%-50s %-10s %-8s %-7s %s\n' \
            "${NAMES[i]:0:50}" \
            "${VERDICTS[i]}" \
            "${COUNTS[i]}" \
            "${THRESHOLDS[i]}" \
            "${SHAS[i]:0:12}"
    done
    printf -- '-%.0s' {1..100}; printf '\n'
    printf 'total: %d markers, %d stale\n' "${#NAMES[@]}" "$STALE_TOTAL"
fi

if (( STALE_TOTAL > 0 )); then
    exit 1
fi
exit 0
