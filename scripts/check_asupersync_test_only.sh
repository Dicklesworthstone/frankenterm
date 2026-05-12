#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/check_asupersync_test_only.sh

Fails if any supported-path Rust file reintroduces an active #[tokio::test]
attribute. Supported paths are crates/, frankenterm/, and tests/. Historical
mentions in docs, comments, or string literals are not violations unless the
line itself starts with the active attribute.
EOF
}

if [[ "${1:-}" == "--help" || "${1:-}" == "-h" ]]; then
    usage
    exit 0
fi

if [[ $# -ne 0 ]]; then
    echo "[asupersync-test-only] unexpected arguments: $*" >&2
    usage >&2
    exit 64
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

cd "$PROJECT_ROOT"

violations=()
while IFS= read -r file; do
    [[ -z "$file" ]] && continue
    while IFS= read -r hit; do
        [[ -z "$hit" ]] && continue
        violations+=("${file}:${hit}")
    done < <(grep -nE '^[[:space:]]*#\[tokio::test' "$file" || true)
done < <(git ls-files -- 'crates/**/*.rs' 'frankenterm/**/*.rs' 'tests/**/*.rs')

if [[ "${#violations[@]}" -gt 0 ]]; then
    echo "[asupersync-test-only] active #[tokio::test] attributes are forbidden in supported paths." >&2
    printf '  %s\n' "${violations[@]}" >&2
    echo "[asupersync-test-only] port the test to common::asupersync_test!, RuntimeFixture, or run_lab_test." >&2
    exit 1
fi

port_count=0
if [[ -d crates/frankenterm-core/tests ]]; then
    port_count="$(find crates/frankenterm-core/tests -maxdepth 1 -type f -name '*_labruntime.rs' | wc -l | tr -d ' ')"
fi

if [[ "$port_count" -lt 20 ]]; then
    echo "[asupersync-test-only] expected at least 20 *_labruntime.rs port files, found $port_count" >&2
    exit 1
fi

echo "[asupersync-test-only] no active #[tokio::test] attributes in supported paths; labruntime_port_files=$port_count"
