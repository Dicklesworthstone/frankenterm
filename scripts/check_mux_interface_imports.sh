#!/usr/bin/env bash
# ft-zoxxq.4 — CI guard preventing new `dyn MuxInterface` / `dyn WeztermInterface`
# imports outside the mux module.
#
# Per docs/proposals/ft-zoxxq-mux-boundary-truth.md stance (b): we have
# committed to the wezterm-fork identity and have no second mux backend
# planned. The MuxInterface trait exists for the inherent + concrete-type
# impl blocks inside the mux module; trait-object usage (`Box<dyn MuxInterface>`,
# `Arc<dyn MuxInterface>`, `&dyn MuxInterface`, etc.) outside the mux module
# would re-introduce the abstraction theatre that audit found nobody uses
# (0 trait-object consumers across 31 importing files, 192 concrete refs).
#
# This guard fails CI if a new file outside the allowlist introduces such
# a reference. Update the allowlist below if a *real* second-backend
# extraction lands — and revisit the ft-zoxxq stance at that time.

set -euo pipefail

# Resolve repo root from this script's location so the guard works whether
# it's invoked from CI, a pre-commit hook, or a developer's shell.
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
cd "${REPO_ROOT}"

# Files that are permitted to mention `dyn (Mux|Wezterm)Interface`.
# These are the canonical home of the trait + the strangler-fig anchor
# (ft-zoxxq.1 / ft-zoxxq.2). Add to this list ONLY when relocating a
# trait-object call site as part of an explicit ft-zoxxq.x bead — never
# silently.
ALLOWED_FILES=(
    "crates/frankenterm-core/src/wezterm.rs"
    "crates/frankenterm-core/src/mux_client.rs"
    # Self-referential: this guard script and the proposal that motivates it.
    "scripts/check_mux_interface_imports.sh"
    "docs/proposals/ft-zoxxq-mux-boundary-truth.md"
)

# The pattern matches:
#   dyn MuxInterface        — direct trait-object on the canonical name
#   dyn WeztermInterface    — direct trait-object on the alias name
# Word-boundary protected so identifiers like `WeztermInterfaceFactory`
# don't accidentally match.
PATTERN='dyn[[:space:]]+(Mux|Wezterm)Interface\b'

# Use ripgrep if available (faster, respects .gitignore by default), fall
# back to git-grep otherwise.
if command -v rg >/dev/null 2>&1; then
    matches="$(rg --line-number --no-heading --pcre2 "${PATTERN}" \
        --glob '!target/**' --glob '!.beads/**' \
        || true)"
else
    matches="$(git grep -n -E "${PATTERN}" -- '*.rs' '*.sh' '*.md' \
        || true)"
fi

if [[ -z "${matches}" ]]; then
    echo "ft-zoxxq.4 guard: no \`dyn MuxInterface\` / \`dyn WeztermInterface\` references found anywhere — clean."
    exit 0
fi

# Filter out the allowlisted files. Build a regex that matches any of the
# allowed prefixes followed by a colon (the line:lineno separator from
# rg/git-grep output).
allow_regex=""
for f in "${ALLOWED_FILES[@]}"; do
    # Escape regex metacharacters in the filename (paths only contain '/' and
    # alphanumerics in this repo, but be defensive).
    esc="$(printf '%s' "$f" | sed -e 's/[][\\.*^$/+?()|{}]/\\&/g')"
    if [[ -z "${allow_regex}" ]]; then
        allow_regex="^${esc}:"
    else
        allow_regex="${allow_regex}|^${esc}:"
    fi
done

# Use grep -Ev to drop allowed lines; what remains are violations.
violations="$(printf '%s\n' "${matches}" | grep -Ev "${allow_regex}" || true)"

if [[ -z "${violations}" ]]; then
    echo "ft-zoxxq.4 guard: all \`dyn (Mux|Wezterm)Interface\` references are inside the allowlist — clean."
    exit 0
fi

cat >&2 <<EOF
─── ft-zoxxq.4 guard: forbidden \`dyn (Mux|Wezterm)Interface\` reference detected ───

Files outside the mux-module allowlist must not introduce trait-object
references to the MuxInterface (or its WeztermInterface alias). The
audit recorded in docs/proposals/ft-zoxxq-mux-boundary-truth.md found
zero existing trait-object consumers across 31 importing files. New
ones re-introduce the abstraction theatre we explicitly removed.

Violations found:
${violations}

What to do instead:
  - If you need a concrete client, depend on \`crate::wezterm::WeztermClient\`
    (the canonical concrete impl) directly.
  - If you genuinely need to swap implementations (mocks, alt backends),
    use a generic parameter \`T: MuxInterface\` instead of a trait object.
  - If you are doing a *legitimate* relocation of an existing trait-object
    site as part of an ft-zoxxq.x bead, add the new file to the
    ALLOWED_FILES array in scripts/check_mux_interface_imports.sh in the
    same commit.

Allowlist currently includes:
$(printf '  - %s\n' "${ALLOWED_FILES[@]}")
EOF
exit 1
