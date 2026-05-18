#!/usr/bin/env bash
# ft-tf6g3.2 — Auto-stamp README/AGENTS counts via build-time queries.
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
#  - In `--check` mode: exits 1 if any placeholder occurrence's
#    documented value drifts from the live value by more than the
#    threshold (default 5%). CI uses --check as an advisory guard
#    initially.
#
# Reproduce locally:
#     bash scripts/stamp-readme-counts.sh                  # rewrite
#     bash scripts/stamp-readme-counts.sh --check          # advisory
#     bash scripts/stamp-readme-counts.sh --check --strict # exact match
#     bash scripts/stamp-readme-counts.sh --json           # machine-readable snapshot
#     bash scripts/stamp-readme-counts.sh --source=head --json
#                                                          # release snapshot from committed tree
#
# Cross-references:
#   ft-d3awp / ft-hdvvo — drift incidents that motivated this work
#   ft-tf6g3.2 — current count-drift closure bead
#   ft-i2eni.5 — original count-stamper substrate
#   docs/contributing/auto-stamped-counts.md — placeholder convention

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
cd "${REPO_ROOT}"

MODE="write"
THRESHOLD_PCT=5
STRICT=0
SOURCE_MODE="worktree"
for arg in "$@"; do
    case "$arg" in
        --check)   MODE="check" ;;
        --json)    MODE="json" ;;
        --strict)  STRICT=1 ;;
        --source=worktree) SOURCE_MODE="worktree" ;;
        --source=head) SOURCE_MODE="head" ;;
        --threshold=*) THRESHOLD_PCT="${arg#--threshold=}" ;;
        --help|-h)
            sed -n '1,40p' "$0" | sed -e 's/^# \?//'
            exit 0
            ;;
        *) echo "ft-tf6g3.2: unknown arg: $arg" >&2; exit 2 ;;
    esac
done

# Manifest: each entry is `name|command`. The command must produce a
# single integer to stdout. When you add a new tracked count, add the
# row here AND the matching <!--count:NAME-->...<!--/count--> blocks in
# README.md / AGENTS.md (or anywhere — the script scans both files for
# any matching placeholder names).
WORKTREE_MANIFEST=(
    "workspace_members|awk '/^members = \\[/,/^]/' Cargo.toml | grep -c '^[[:space:]]*\"'"
    "vendored_members|awk '/^members = \\[/,/^]/' Cargo.toml | grep -c '^[[:space:]]*\"frankenterm/'"
    "vendored_top_level|find frankenterm -maxdepth 2 -name Cargo.toml | wc -l"
    "core_subcrates|ls -d crates/frankenterm-core-* | wc -l"
    "core_top_level_modules|find crates/frankenterm-core/src -maxdepth 1 -name '*.rs' | wc -l"
    "core_loc|find crates/frankenterm-core/src -name '*.rs' -exec cat {} + | wc -l"
    "test_count|grep -rE '^[[:space:]]*#\\[(test|tokio::test|asupersync_test::test)' crates/ | wc -l"
    "core_rust_test_files|find crates/frankenterm-core/tests -type f -name '*.rs' | wc -l"
    "criterion_bench_files|find crates/frankenterm-core/benches -type f -name '*.rs' | wc -l"
    "fuzz_targets|find fuzz -type f -path '*/fuzz_targets/*.rs' | wc -l"
    "doc_markdown_files|find docs -type f -name '*.md' | wc -l"
    "e2e_scripts|git ls-files tests/e2e | awk '/\\.sh$/ { count++ } END { print count + 0 }'"
)

HEAD_MANIFEST=(
    "workspace_members|git show HEAD:Cargo.toml | awk '/^members = \\[/,/^]/' | grep -c '^[[:space:]]*\"'"
    "vendored_members|git show HEAD:Cargo.toml | awk '/^members = \\[/,/^]/' | grep -c '^[[:space:]]*\"frankenterm/'"
    "vendored_top_level|git ls-tree -r --name-only HEAD frankenterm | awk -F/ '\$NF == \"Cargo.toml\" && NF <= 3 { count++ } END { print count + 0 }'"
    "core_subcrates|git ls-tree --name-only HEAD:crates | awk '/^frankenterm-core-/ { count++ } END { print count + 0 }'"
    "core_top_level_modules|git ls-tree --name-only HEAD:crates/frankenterm-core/src | awk '/\\.rs$/ { count++ } END { print count + 0 }'"
    "core_loc|git ls-tree -r --name-only HEAD crates/frankenterm-core/src | awk '/\\.rs$/ { print }' | while IFS= read -r file_path; do git show \"HEAD:\${file_path}\"; done | wc -l"
    "test_count|git grep -E '^[[:space:]]*#\\[(test|tokio::test|asupersync_test::test)' HEAD -- crates | wc -l"
    "core_rust_test_files|git ls-tree -r --name-only HEAD crates/frankenterm-core/tests | awk '/\\.rs$/ { count++ } END { print count + 0 }'"
    "criterion_bench_files|git ls-tree -r --name-only HEAD crates/frankenterm-core/benches | awk '/\\.rs$/ { count++ } END { print count + 0 }'"
    "fuzz_targets|git ls-tree -r --name-only HEAD fuzz | awk '/\\/fuzz_targets\\/.*\\.rs$/ { count++ } END { print count + 0 }'"
    "doc_markdown_files|git ls-tree -r --name-only HEAD docs | awk '/\\.md$/ { count++ } END { print count + 0 }'"
    "e2e_scripts|git ls-tree -r --name-only HEAD tests/e2e | awk '/\\.sh$/ { count++ } END { print count + 0 }'"
)

case "${SOURCE_MODE}" in
    worktree) MANIFEST=("${WORKTREE_MANIFEST[@]}") ;;
    head) MANIFEST=("${HEAD_MANIFEST[@]}") ;;
    *) echo "ft-tf6g3.2: unknown source mode: ${SOURCE_MODE}" >&2; exit 2 ;;
esac

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

# Read every documented value for `name` from `file`, one per line.
# Emits no lines if no placeholder block is present.
read_documented_values() {
    local file="$1" name="$2"
    python3 - "${file}" "${name}" <<'PYEOF'
import pathlib, re, sys
file, name = sys.argv[1], sys.argv[2]
text = pathlib.Path(file).read_text()
pattern = re.compile(rf"<!--count:{re.escape(name)}-->([^<]*)<!--/count-->")
for match in pattern.finditer(text):
    print(match.group(1).strip())
PYEOF
}

join_values() {
    local IFS=", "
    printf '%s' "$*"
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
declare -a count_reports=()
total_count=0
present_count=0
missing_placeholder_count=0
matching_count=0
within_threshold_count=0
updated_occurrence_count=0

for entry in "${MANIFEST[@]}"; do
    name="${entry%%|*}"
    cmd="${entry#*|}"
    live="$(compute_count "${cmd}")"
    total_count=$((total_count + 1))
    live_status="ok"
    if [[ -z "${live}" ]] || ! [[ "${live}" =~ ^[0-9]+$ ]]; then
        violations+=("${name}: command failed to produce an integer (got '${live}')")
        live_status="invalid"
        if [[ "${MODE}" == "json" ]]; then
            declare -a invalid_doc_reports=()
            for doc in "${DOCS[@]}"; do
                documented_values=()
                while IFS= read -r documented; do
                    documented_values+=("${documented}")
                done < <(read_documented_values "${doc}" "${name}")
                if [[ ${#documented_values[@]} -eq 0 ]]; then
                    missing_placeholder_count=$((missing_placeholder_count + 1))
                    invalid_doc_reports+=("$(jq -cn \
                        --arg path "${doc}" \
                        '{path:$path, occurrence:null, placeholder_present:false, documented_value:null, drift_pct:null, status:"missing_placeholder"}')")
                else
                    occurrence=0
                    for documented in "${documented_values[@]}"; do
                        occurrence=$((occurrence + 1))
                        present_count=$((present_count + 1))
                        invalid_doc_reports+=("$(jq -cn \
                            --arg path "${doc}" \
                            --arg documented "${documented}" \
                            --argjson occurrence "${occurrence}" \
                            '{path:$path, occurrence:$occurrence, placeholder_present:true, documented_value:($documented|tonumber), live_value:null, drift_pct:null, status:"command_invalid"}')")
                    done
                fi
            done
            docs_json="$(printf '%s\n' "${invalid_doc_reports[@]}" | jq -c -s '.')"
            count_reports+=("$(jq -cn \
                --arg name "${name}" \
                --arg command "${cmd}" \
                --arg live_status "${live_status}" \
                --argjson documents "${docs_json}" \
                '{name:$name, command:$command, live_value:null, live_status:$live_status, documents:$documents}')")
        fi
        continue
    fi
    declare -a doc_reports=()
    for doc in "${DOCS[@]}"; do
        documented_values=()
        while IFS= read -r documented; do
            documented_values+=("${documented}")
        done < <(read_documented_values "${doc}" "${name}")
        if [[ ${#documented_values[@]} -eq 0 ]]; then
            missing_placeholder_count=$((missing_placeholder_count + 1))
            if [[ "${MODE}" == "json" ]]; then
                doc_reports+=("$(jq -cn \
                    --arg path "${doc}" \
                    '{path:$path, occurrence:null, placeholder_present:false, documented_value:null, drift_pct:null, status:"missing_placeholder"}')")
            fi
            continue
        fi
        occurrence=0
        declare -a mismatched_values=()
        for documented in "${documented_values[@]}"; do
            occurrence=$((occurrence + 1))
            present_count=$((present_count + 1))
            drift_pct="$(percent_drift "${documented}" "${live}")"
            doc_status="matches"
            if [[ "${documented}" == "${live}" ]]; then
                matching_count=$((matching_count + 1))
            elif [[ "${STRICT}" -eq 1 ]]; then
                doc_status="strict_mismatch"
            elif [[ "${drift_pct}" -le "${THRESHOLD_PCT}" ]]; then
                within_threshold_count=$((within_threshold_count + 1))
                doc_status="drift_within_threshold"
            else
                doc_status="drift_exceeded"
            fi
            if [[ "${MODE}" == "check" ]]; then
                if [[ "${STRICT}" -eq 1 ]]; then
                    if [[ "${documented}" != "${live}" ]]; then
                        violations+=("${doc}::${name}#${occurrence}: documented=${documented} live=${live} (strict)")
                        doc_status="strict_mismatch"
                    fi
                elif [[ "${drift_pct}" -gt "${THRESHOLD_PCT}" ]]; then
                    violations+=("${doc}::${name}#${occurrence}: documented=${documented} live=${live} drift=${drift_pct}% > ${THRESHOLD_PCT}%")
                fi
            elif [[ "${MODE}" == "json" ]]; then
                if [[ "${doc_status}" == "drift_exceeded" ]]; then
                    violations+=("${doc}::${name}#${occurrence}: documented=${documented} live=${live} drift=${drift_pct}% > ${THRESHOLD_PCT}%")
                elif [[ "${doc_status}" == "strict_mismatch" ]]; then
                    violations+=("${doc}::${name}#${occurrence}: documented=${documented} live=${live} (strict)")
                fi
                doc_reports+=("$(jq -cn \
                    --arg path "${doc}" \
                    --arg documented "${documented}" \
                    --argjson occurrence "${occurrence}" \
                    --argjson live "${live}" \
                    --argjson drift_pct "${drift_pct}" \
                    --arg status "${doc_status}" \
                    '{path:$path, occurrence:$occurrence, placeholder_present:true, documented_value:($documented|tonumber), live_value:$live, drift_pct:$drift_pct, status:$status}')")
            else
                if [[ "${documented}" != "${live}" ]]; then
                    mismatched_values+=("${documented}")
                    updated_occurrence_count=$((updated_occurrence_count + 1))
                fi
            fi
        done
        if [[ "${MODE}" == "write" ]] && [[ ${#mismatched_values[@]} -gt 0 ]]; then
            rewrite_placeholders_in_file "${doc}" "${name}" "${live}"
            updates+=("${doc}::${name}: [$(join_values "${mismatched_values[@]}")] → ${live}")
        fi
    done
    if [[ "${MODE}" == "json" ]]; then
        if [[ ${#doc_reports[@]} -eq 0 ]]; then
            docs_json="[]"
        else
            docs_json="$(printf '%s\n' "${doc_reports[@]}" | jq -c -s '.')"
        fi
        count_reports+=("$(jq -cn \
            --arg name "${name}" \
            --arg command "${cmd}" \
            --arg live_status "${live_status}" \
            --argjson live_value "${live:-0}" \
            --argjson documents "${docs_json}" \
            '{name:$name, command:$command, live_value:$live_value, live_status:$live_status, documents:$documents}')")
    fi
done

# ---- report ----------------------------------------------------------

if [[ "${MODE}" == "json" ]]; then
    if [[ ${#count_reports[@]} -eq 0 ]]; then
        counts_json="[]"
    else
        counts_json="$(printf '%s\n' "${count_reports[@]}" | jq -c -s '.')"
    fi
    overall_status="passed"
    if [[ ${#violations[@]} -gt 0 ]]; then
        overall_status="failed"
    fi
    jq -n \
        --arg schema_version "1.0.0" \
        --arg bead_id "ft-tf6g3.2" \
        --arg generated_at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
        --arg generator "scripts/stamp-readme-counts.sh --json --source=${SOURCE_MODE}" \
        --arg source_mode "${SOURCE_MODE}" \
        --arg check_status "${overall_status}" \
        --argjson threshold_pct "${THRESHOLD_PCT}" \
        --argjson strict "${STRICT}" \
        --argjson tracked_counts "${total_count}" \
        --argjson placeholder_occurrences "${present_count}" \
        --argjson missing_placeholders "${missing_placeholder_count}" \
        --argjson matching_placeholders "${matching_count}" \
        --argjson within_threshold_placeholders "${within_threshold_count}" \
        --argjson violation_count "${#violations[@]}" \
        --argjson counts "${counts_json}" \
        '{
          schema_version: $schema_version,
          bead_id: $bead_id,
          generated_at: $generated_at,
          generator: $generator,
          source: {
            script: "scripts/stamp-readme-counts.sh",
            count_source: $source_mode,
            docs: ["README.md", "AGENTS.md"]
          },
          check: {
            status: $check_status,
            strict: ($strict == 1),
            threshold_pct: $threshold_pct
          },
          summary: {
            tracked_counts: $tracked_counts,
            placeholder_occurrences: $placeholder_occurrences,
            missing_placeholders: $missing_placeholders,
            matching_placeholders: $matching_placeholders,
            within_threshold_placeholders: $within_threshold_placeholders,
            violation_count: $violation_count
          },
          counts: $counts
        }'
    exit 0
fi

if [[ "${MODE}" == "check" ]]; then
    if [[ ${#violations[@]} -eq 0 ]]; then
        if [[ "${STRICT}" -eq 1 ]]; then
            echo "ft-tf6g3.2: ${present_count} placeholder(s) exactly match live values (${total_count} tracked counts)."
        else
            echo "ft-tf6g3.2: ${present_count} placeholder(s) within ${THRESHOLD_PCT}% drift threshold (${total_count} tracked counts)."
        fi
        exit 0
    fi
    echo "ft-tf6g3.2: drift threshold exceeded for ${#violations[@]} placeholder(s):" >&2
    for v in "${violations[@]}"; do
        printf '  - %s\n' "$v" >&2
    done
    cat >&2 <<EOF

What to do:
  - Run \`bash scripts/stamp-readme-counts.sh --source=${SOURCE_MODE}\` (no --check) to rewrite
    README.md / AGENTS.md so the placeholder values match the selected source.
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
    echo "ft-tf6g3.2: no updates needed (${present_count} placeholder(s) already current)."
    exit 0
fi
echo "ft-tf6g3.2: updated ${updated_occurrence_count} placeholder occurrence(s) across ${#updates[@]} file/name group(s):"
for u in "${updates[@]}"; do
    printf '  - %s\n' "$u"
done
