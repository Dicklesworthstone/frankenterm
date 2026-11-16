#!/usr/bin/env bash
# check_bundled_default_config_generic.sh
#
# Guard (GH issue #70): the bundled default configs that ship inside
# FrankenTerm.app must be generic and fully local. A clean install must not
# contain live remote hosts, SSH key paths, proxy commands, or anything that
# initiates outbound network activity on first launch.
#
# Scans the bundled defaults for live (non-comment) lines containing remote
# connectivity primitives. Commented example blocks are explicitly allowed --
# that is the documented way to show users how to opt in.
#
# Called from scripts/create-macos-bundle.sh before the configs are copied
# into the app bundle; also runnable standalone:
#     bash scripts/check_bundled_default_config_generic.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"

fail=0

check_file() {
    local file="$1" comment_re="$2"
    if [ ! -f "$file" ]; then
        echo "[bundled-config-generic] SKIP (missing): $file"
        return
    fi
    # Strip whole-line comments, then look for remote-connectivity primitives.
    local hits
    hits=$(grep -vE "$comment_re" "$file" | grep -nE \
        '([0-9]{1,3}\.){3}[0-9]{1,3}|remote_address|proxy_command|ssh_key|ssh_domains|StrictHostKeyChecking|BatchMode|[^a-z_]attach\(|ssh [^-]*@' \
        || true)
    if [ -n "$hits" ]; then
        echo "[bundled-config-generic] FAIL: $file contains live remote-connectivity entries:" >&2
        echo "$hits" >&2
        fail=1
    else
        echo "[bundled-config-generic] OK: $file"
    fi
}

# Lua default: whole-line comments start with optional whitespace then `--`.
check_file "$PROJECT_ROOT/crates/frankenterm-gui/frankenterm.lua" '^[[:space:]]*--'
# TOML default: whole-line comments start with optional whitespace then `#`.
check_file "$PROJECT_ROOT/crates/frankenterm-gui/frankenterm.toml" '^[[:space:]]*#'

if [ "$fail" -ne 0 ]; then
    echo "[bundled-config-generic] Bundled defaults must ship with NO live remote hosts." >&2
    echo "[bundled-config-generic] Keep personal/fleet config in an untracked user config" >&2
    echo "[bundled-config-generic] (~/.frankenterm.lua), never in the repo defaults." >&2
    exit 1
fi
echo "[bundled-config-generic] all bundled defaults are generic/local-only."
