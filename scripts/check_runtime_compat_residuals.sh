#!/usr/bin/env bash
# ft-y378j.3 — workspace-wide audit + CI guard for the
# `runtime_compat` → `runtime_async` rename (ft-g43fq + ft-y378j.x).
#
# After ft-y378j.1 (test rename) + ft-y378j.2 (production source rename) +
# this bead's mass-rename of benches and three doc-comment stragglers,
# the residual `runtime_compat` references in the workspace fall into a
# tight allowlist. This guard fails CI if a new file (outside the
# allowlist) acquires a `runtime_compat` reference, or if any allowlisted
# file's count exceeds its grandfathered maximum.
#
# The deprecated alias `pub use crate::runtime_async as runtime_compat;`
# in frankenterm-core/src/lib.rs is the load-bearing residual: it keeps
# the old name compiling for a one-release deprecation window. ft-y378j.4
# removes the alias entirely after the deprecation cycle elapses; at that
# point this guard tightens to "zero references anywhere except CHANGELOG".
#
# Why an allowlist over a bare grep -c: the 8 grandfathered files contain
# `runtime_compat` for legitimate reasons (alias source, audit-guard
# internals, panic-message text the operator may grep for, contract-test
# framework citing the canonical name). Removing those references is
# either premature (panic-message stability), redundant (the alias source
# IS the alias source), or breaks the guard's own lookup table.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
cd "${REPO_ROOT}"

# Allowlist: file → max-occurrences-permitted.
# These are the grandfathered files at the time of ft-y378j.3 commit.
# Each entry's max is "current count + 0" (no growth allowed). If a
# legitimate edit BUMPS one of these counts, increment the max here and
# explain in the commit message why the new occurrence is intentional.
declare -A ALLOWLIST=(
    # The deprecated alias declaration itself. ft-y378j.4 deletes this.
    ["crates/frankenterm-core/src/lib.rs"]=2
    # Panic-message text + migration doc-comments. Stable user-facing
    # strings; renaming would invalidate operator runbooks that grep
    # for "runtime_compat mutex lock failed".
    ["crates/frankenterm-core/src/runtime_async.rs"]=19
    # Surface-guard allowlist enumerator. References the legacy filename
    # in its allow-pattern. Updates concurrently with ft-y378j.4.
    ["crates/frankenterm-core/src/runtime_async_surface_guard.rs"]=6
    # forbidden_dep_guards: enumerates the canonical async-API module
    # names; both names are accepted during the deprecation window.
    ["crates/frankenterm-core/src/forbidden_dep_guards.rs"]=1
    # dependency_eradication: exclude_paths + summary doc reference the
    # rename topic. Updates concurrently with ft-y378j.4.
    ["crates/frankenterm-core/src/dependency_eradication.rs"]=3
    # vendored_async_contracts: contract-test framework citing the
    # original surface name. Stable for as long as the contract suite
    # treats runtime_compat as the canonical name.
    ["crates/frankenterm-core/src/vendored_async_contracts.rs"]=9
    # cx.rs: doc comment + 2 test-fn names mentioning the old name.
    ["crates/frankenterm-core/src/cx.rs"]=4
    # distributed.rs: one doc-comment reference.
    ["crates/frankenterm-core/src/distributed.rs"]=1
)

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
# CHANGELOG (historical + canonical docs that legitimately name the rename
# topic), docs/ (proposals + audit reports), scripts/ (audit/guard scripts
# that name the surface they're measuring), tests/e2e/ (e2e suites that
# verify the rename and the deprecation alias). Those references are
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
    echo "ft-y378j.3 guard: no \`runtime_compat\` references found anywhere — clean."
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
            echo "ft-y378j.3 guard: VIOLATION — ${file} has ${count} \`runtime_compat\` references but allowlist max is ${max}."
            violations=$((violations + 1))
        fi
        continue
    fi

    # Not on the allowlist → any occurrence is a violation.
    echo "ft-y378j.3 guard: VIOLATION — ${file} has ${count} \`runtime_compat\` reference(s); add it to the allowlist or rename to \`runtime_async\`."
    violations=$((violations + 1))
done <<< "${matches}"

if [[ ${violations} -gt 0 ]]; then
    echo
    echo "ft-y378j.3 guard: ${violations} file(s) violate the allowlist."
    echo "If the new reference is intentional (canonical-name doc, panic-message"
    echo "stability, audit-guard internals), add the file to the allowlist in"
    echo "scripts/check_runtime_compat_residuals.sh with a brief justification."
    echo "Otherwise rename to \`runtime_async\` to match the canonical surface."
    exit 1
fi

echo "ft-y378j.3 guard: ${total_occurrences} \`runtime_compat\` reference(s) across allowlisted files — clean."
exit 0
