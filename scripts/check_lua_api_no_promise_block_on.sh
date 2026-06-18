#!/usr/bin/env bash
# ft-3dz22 - static guard for the mlua 0.11 block_on deadlock regression.

set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/check_lua_api_no_promise_block_on.sh

Fails if tracked Rust files under frankenterm/lua-api-crates/ contain live code
that calls promise::spawn::block_on. Line-comment mentions are ignored so fixed
call sites can explain the regression without tripping the guard.
EOF
}

if [[ "${1:-}" == "--help" || "${1:-}" == "-h" ]]; then
    usage
    exit 0
fi

if [[ $# -ne 0 ]]; then
    echo "[lua-api-block-on] unexpected arguments: $*" >&2
    usage >&2
    exit 64
fi

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
cd "${REPO_ROOT}"

mapfile -t files < <(git ls-files -- 'frankenterm/lua-api-crates/**/*.rs')

if [[ "${#files[@]}" -eq 0 ]]; then
    echo "[lua-api-block-on] no Rust files found under frankenterm/lua-api-crates/; guard path is stale." >&2
    exit 1
fi

banned="promise::spawn::block_on"
violations=()

for file in "${files[@]}"; do
    while IFS= read -r hit; do
        [[ -z "${hit}" ]] && continue

        line_no="${hit%%:*}"
        line="${hit#*:}"
        code_before_comment="${line%%//*}"

        if [[ "${code_before_comment}" == *"${banned}"* ]]; then
            violations+=("${file}:${line_no}:${line}")
        fi
    done < <(grep -nF "${banned}" "${file}" || true)
done

if [[ "${#violations[@]}" -gt 0 ]]; then
    cat >&2 <<EOF
[lua-api-block-on] ${banned} is forbidden in frankenterm/lua-api-crates/** code.

The mlua 0.11 migration briefly wrapped async Lua callbacks in sync
create_function/add_method closures that drove futures through ${banned}. When
those callbacks run from gui-startup/event/keybinding handlers, the main-thread
spawn-queue deadlock guard aborts the GUI. Use create_async_function or
add_async_method and await the future instead.

Violations:
$(printf '  %s\n' "${violations[@]}")
EOF
    exit 1
fi

echo "[lua-api-block-on] no live ${banned} calls found under frankenterm/lua-api-crates/."
