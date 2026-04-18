#!/usr/bin/env bash
# ft-xbnl0.2.4 — Run all verification contract tests in sequence.
#
# Invokes the 5 filtered cargo test runs that collectively verify the
# ft-xbnl0.2.4 acceptance contract:
#   1. HTTP client contract tests (19) — distributed_http_client_*
#   2. TLS bundle + server-name + error-path + version tests (14) — build_tls_*
#   3. Regression guards (3) — ft_xbnl0_2_4_* (imports, manifests, dep presence)
#   4. Metrics server cx-first family (3) — metrics_server_start_with_cx_*
#   5. Web server cx pre-cancel (1) — web_server_with_cx_*
#
# Exit code 0 on all passing; non-zero on any failure.
#
# Invoke as:
#   ./scripts/check_ft_xbnl0_2_4.sh
#
# Or via rch for remote verification per the shared verification
# contract (docs/ft-xbnl0-verification-contract.md):
#   rch exec -- ./scripts/check_ft_xbnl0_2_4.sh
#
# Local invocation uses the fork-bypass CC/CXX/CARGO_TARGET_DIR pattern
# from docs/ft-xbnl0-2-4-completion-evidence.md §3.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT_DIR}"

# Each agent should use a unique target dir to avoid lock contention
# with other concurrently-running agents. Default is a project-scoped
# dir; override with CARGO_TARGET_DIR_OVERRIDE for agent-specific use.
: "${CARGO_TARGET_DIR:=${CARGO_TARGET_DIR_OVERRIDE:-target/ft-xbnl0.2.4-check}}"
export CARGO_TARGET_DIR

# The `cc` shell alias on this dev machine maps to Claude Code rather
# than the C compiler — native deps (aws-lc-sys etc.) fail to build
# without explicit CC/CXX. CI runners where `cc` resolves correctly
# can override these with CC=cc CXX=c++ or similar.
: "${CC:=/opt/homebrew/opt/llvm/bin/clang}"
: "${CXX:=/opt/homebrew/opt/llvm/bin/clang++}"
export CC CXX

FAIL=0

log_header() {
    echo
    echo "=============================================================="
    echo "  ft-xbnl0.2.4 — $1"
    echo "=============================================================="
}

run_test() {
    local label="$1"
    shift
    log_header "${label}"
    if cargo test "$@"; then
        echo "[PASS] ${label}"
    else
        echo "[FAIL] ${label}"
        FAIL=1
    fi
}

run_test "Run 1/5: HTTP client contract tests" \
    -p frankenterm-core \
    --features distributed,asupersync-runtime \
    --lib distributed_http_client_ \
    -- --nocapture

run_test "Run 2/5: TLS bundle + server-name + error-path + version tests" \
    -p frankenterm-core \
    --features distributed,asupersync-runtime \
    --lib build_tls_ \
    -- --nocapture

run_test "Run 3/5: Regression guards" \
    -p frankenterm-core \
    --test ft_xbnl0_2_4_no_direct_tokio_net_or_rustls \
    -- --nocapture

run_test "Run 4/5: Metrics server cx-first family" \
    -p frankenterm-core \
    --features distributed,asupersync-runtime,web \
    --lib metrics_server_start_with_cx_ \
    -- --nocapture

run_test "Run 5/5: Web server cx pre-cancel" \
    -p frankenterm-core \
    --features web,asupersync-runtime \
    --test web \
    web_server_with_cx_ \
    -- --nocapture

echo
echo "=============================================================="
if [[ ${FAIL} -eq 0 ]]; then
    echo "  ft-xbnl0.2.4 — all 5 runs PASS"
    echo "=============================================================="
    exit 0
else
    echo "  ft-xbnl0.2.4 — ONE OR MORE RUNS FAILED"
    echo "=============================================================="
    exit 1
fi
