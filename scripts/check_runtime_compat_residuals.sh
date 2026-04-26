#!/usr/bin/env bash
# ft-y378j.4 — workspace-wide audit + CI guard for the
# `runtime_compat` → `runtime_async` rename (ft-g43fq + ft-y378j.x).
#
# The deprecation alias is gone. This guard fails CI if any Rust source
# under crates/ or frankenterm/ reintroduces the old runtime surface name.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
cd "${REPO_ROOT}"

declare -A ALLOWLIST=()

# Files that are EXEMPT from the count check entirely (the guard scripts
# themselves, which inevitably mention the name being guarded against).
EXEMPT_FILES=(
    "scripts/check_runtime_compat_residuals.sh"
    "docs/proposals/ft-7iof6-runtime-compat-canonical-surface.md"
    ".beads/issues.jsonl"
    "MEMORY_FORMAT.md"
)

# Scope: actual *code* surfaces (Rust source under crates/ + frankenterm/,
# plus the workspace Cargo.toml's path-deps). NOT in scope: README/AGENTS/
# CHANGELOG (historical docs that legitimately name the rename topic),
# docs/ (proposals + audit reports), scripts/ (audit/guard scripts that
# name the surface they're measuring), tests/e2e/ (e2e suites that verify
# historical migration behavior). Those references are
# NOT code call-sites — they're meta-references about the migration.
#
# Use ripgrep if available; fall back to git grep.
if command -v rg >/dev/null 2>&1; then
    matches="$(rg --count-matches --no-heading 'runtime_compat' \
        --glob 'crates/**/*.rs' --glob 'frankenterm/**/*.rs' \
        --glob 'crates/**/Cargo.toml' --glob 'frankenterm/**/Cargo.toml' \
        --glob '!target/**' \
        || true)"
else
    matches="$(git grep -c 'runtime_compat' -- 'crates/**/*.rs' \
        'frankenterm/**/*.rs' 'crates/**/Cargo.toml' \
        'frankenterm/**/Cargo.toml' || true)"
fi

if [[ -z "${matches}" ]]; then
    echo "ft-y378j.4 guard: no \`runtime_compat\` references found anywhere — clean."
    exit 0
fi

violations=0
total_occurrences=0

while IFS=: read -r file count; do
    [[ -z "${file}" ]] && continue
    total_occurrences=$((total_occurrences + count))

    # Exempt?
    skip=0
    for exempt in "${EXEMPT_FILES[@]}"; do
        if [[ "${file}" == "${exempt}" ]]; then
            skip=1
            break
        fi
    done
    if [[ ${skip} -eq 1 ]]; then
        continue
    fi

    # Allowlisted with a cap?
    if [[ -n "${ALLOWLIST[$file]+_}" ]]; then
        max="${ALLOWLIST[$file]}"
        if [[ "${count}" -gt "${max}" ]]; then
            echo "ft-y378j.4 guard: VIOLATION — ${file} has ${count} \`runtime_compat\` references but allowlist max is ${max}."
            violations=$((violations + 1))
        fi
        continue
    fi

    # Not on the allowlist → any occurrence is a violation.
    echo "ft-y378j.4 guard: VIOLATION — ${file} has ${count} \`runtime_compat\` reference(s); rename to \`runtime_async\`."
    violations=$((violations + 1))
done <<< "${matches}"

if [[ ${violations} -gt 0 ]]; then
    echo
    echo "ft-y378j.4 guard: ${violations} file(s) still use the deprecated name."
    echo "Rename to \`runtime_async\` to match the canonical surface."
    exit 1
fi

echo "ft-y378j.4 guard: ${total_occurrences} \`runtime_compat\` reference(s) across allowlisted files — clean."
exit 0
