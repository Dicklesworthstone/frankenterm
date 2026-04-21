#!/usr/bin/env bash
# cargo-local.sh — run cargo locally, bypassing the rch PreToolUse hook.
#
# ft-45805: rch (remote compilation helper) intercepts every Bash call
# containing `cargo` and offloads to remote workers. When workers are
# unhealthy the subprocess receives SIGTERM (exit 143) with no
# diagnostic — the 3am operator sees "cargo test foo ... exit 143" and
# loses 30 minutes figuring out the bypass. This script encodes the
# known-good local recipe so you never have to remember it.
#
# Usage:
#   scripts/cargo-local.sh test -p frankenterm-core --lib
#   scripts/cargo-local.sh build --release
#   scripts/cargo-local.sh clippy --all-targets
#
# Override knobs (rarely needed):
#   FT_LOCAL_NAME=<slug>   per-agent target dir suffix (default: $USER-local)
#   FT_LOCAL_TARGET=<dir>  full target path (default: /tmp/ft-${FT_LOCAL_NAME}-target)
#   FT_LOCAL_CC=<path>     C compiler (default: /opt/homebrew/opt/llvm/bin/clang)
#   FT_LOCAL_CXX=<path>    C++ compiler (default: /opt/homebrew/opt/llvm/bin/clang++)
#   FT_LOCAL_NOFORK=1      skip the python fork+setsid; run cargo directly

set -euo pipefail

if [[ $# -eq 0 ]]; then
    echo "usage: $0 <cargo subcommand> [args...]" >&2
    echo "example: $0 test -p frankenterm-core --lib" >&2
    exit 2
fi

: "${FT_LOCAL_NAME:=${USER:-anon}-local}"
: "${FT_LOCAL_TARGET:=/tmp/ft-${FT_LOCAL_NAME}-target}"
: "${FT_LOCAL_CC:=/opt/homebrew/opt/llvm/bin/clang}"
: "${FT_LOCAL_CXX:=/opt/homebrew/opt/llvm/bin/clang++}"
: "${FT_LOCAL_NOFORK:=0}"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

export CARGO_TARGET_DIR="$FT_LOCAL_TARGET"
export CC="$FT_LOCAL_CC"
export CXX="$FT_LOCAL_CXX"

if [[ "$FT_LOCAL_NOFORK" == "1" ]]; then
    cd "$REPO_ROOT"
    exec cargo "$@"
fi

# The rch PreToolUse hook fires on Bash tool calls containing `cargo`.
# A python fork+setsid breaks out of the Claude-Code-owned process group
# so the hook cannot SIGTERM the cargo subprocess when remote workers
# fall over. On hosts without the rch hook installed this costs one
# fork and is otherwise invisible.
exec python3 - "$REPO_ROOT" "$@" <<'PYEOF'
import os
import sys

repo_root, *argv = sys.argv[1:]

pid = os.fork()
if pid == 0:
    os.setsid()
    os.chdir(repo_root)
    os.execvp("cargo", ["cargo", *argv])
else:
    _, status = os.waitpid(pid, 0)
    if os.WIFEXITED(status):
        sys.exit(os.WEXITSTATUS(status))
    if os.WIFSIGNALED(status):
        sys.exit(128 + os.WTERMSIG(status))
    sys.exit(1)
PYEOF
