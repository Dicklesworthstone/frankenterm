#!/usr/bin/env bash
# ft-xbnl0.2.4 — Run all verification contract tests in sequence.
#
# Invokes the 5 filtered cargo test runs that collectively verify the
# ft-xbnl0.2.4 acceptance contract:
#   1. HTTP client contract tests (21) — distributed_http_client_*
#      Includes all this-session-added + pre-existing happy-path GET.
#   2. TLS tests (~45) — tls_*
#      Broader filter than `build_tls_` — catches bundle / server-name /
#      error-variant / version-string tests added this session AND
#      pre-existing happy-path bundle-exchange, large-payload, and
#      token-auth TLS tests. Stronger smoke than the narrow filter.
#   3. Regression guards (3) — ft_xbnl0_2_4_*
#      Import scan + manifest scan + asupersync-dep positive guard.
#   4. Metrics server cx-first family (3) — metrics_server_start_with_cx_*
#      Pre-cancel / mid-flight-cancel / happy path.
#   5. Web server cx pre-cancel (1) — web_server_with_cx_*
#   6. runtime_compat primitive contracts (11) — _with_cx_observes_budget_deadline
#      + yield_now_with_cx + oneshot_recv_with_cx + broadcast_recv_with_cx
#      + semaphore_acquire_ + mpsc_recv_with_cx + watch_changed_with_cx
#      + join_set_join_next_with_cx
#      sleep_with_cx + timeout_with_cx budget-observation tests (ticks
#      382/383), yield_now_with_cx cancel-checkpoint + happy-path tests
#      (tick 418), oneshot_recv_with_cx pre-cancel test (tick 419),
#      broadcast_recv_with_cx pre-cancel test (tick 420),
#      Semaphore::acquire_with_cx pre-cancel test (tick 421),
#      mpsc::Receiver::recv pre-cancel test (tick 422),
#      watch::Receiver::changed pre-cancel test (tick 423),
#      JoinSet::join_next_with_cx pre-cancel test (tick 426),
#      Semaphore::acquire_owned_with_cx pre-cancel test (tick 427).
#      The semaphore filter is `semaphore_acquire_` so it catches both
#      borrow and owned variants.
#      unix::next_line_with_cx pre-cancel test (tick 429).
#      Command::output_with_cx pre-spawn pre-cancel test (tick 430).
#      The command_output_with_cx filter also picks up the pre-existing
#      mid-flight cancel test
#      (process_command_output_with_cx_cancellation_surfaces_as_interrupted)
#      as bonus coverage — together they pin both cancel-observability
#      points on Command::output_with_cx (pre-spawn gate + mid-flight
#      cx→AtomicBool watcher).
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
TOTAL_PASSED=0

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
    # Capture stdout + stderr to a tempfile so we can tally the "N passed"
    # count while still showing the output to the operator.
    local tmp
    # Explicit template path for portability — BSD (`mktemp -t PREFIX`)
    # and GNU (`mktemp -t TEMPLATE`) differ on the `-t` argument form,
    # but both accept a full template path as a positional argument.
    tmp="$(mktemp "${TMPDIR:-/tmp}/ft-xbnl0-2-4-check.XXXXXX")"
    if cargo test "$@" 2>&1 | tee "${tmp}"; then
        # Sum all "N passed" counts from all `test result: ok.` lines in
        # this run (per-binary splits may produce multiple result lines
        # under --all-targets; normally there's just one for our filters).
        local passed
        passed="$(grep -oE 'test result: ok\. [0-9]+ passed' "${tmp}" \
            | awk '{ s += $4 } END { print (s ? s : 0) }')"
        TOTAL_PASSED=$(( TOTAL_PASSED + passed ))
        echo "[PASS] ${label} — ${passed} tests"
    else
        echo "[FAIL] ${label}"
        FAIL=1
    fi
    rm -f "${tmp}"
}

run_test "Run 1/6: HTTP client contract tests" \
    -p frankenterm-core \
    --features distributed,asupersync-runtime \
    --lib distributed_http_client_ \
    -- --nocapture

run_test "Run 2/6: TLS tests (bundle + server-name + errors + versions + happy-path)" \
    -p frankenterm-core \
    --features distributed,asupersync-runtime \
    --lib tls_ \
    -- --nocapture

run_test "Run 3/6: Regression guards" \
    -p frankenterm-core \
    --test ft_xbnl0_2_4_no_direct_tokio_net_or_rustls \
    -- --nocapture

run_test "Run 4/6: Metrics server cx-first family" \
    -p frankenterm-core \
    --features distributed,asupersync-runtime,web \
    --lib metrics_server_start_with_cx_ \
    -- --nocapture

run_test "Run 5/6: Web server cx pre-cancel" \
    -p frankenterm-core \
    --features web,asupersync-runtime \
    --test web \
    web_server_with_cx_ \
    -- --nocapture

run_test "Run 6/6: runtime_compat primitive contracts (budget + cancel observation)" \
    -p frankenterm-core \
    --features asupersync-runtime \
    --lib \
    -- --nocapture _with_cx_observes_budget_deadline yield_now_with_cx oneshot_recv_with_cx broadcast_recv_with_cx semaphore_acquire_ mpsc_recv_with_cx watch_changed_with_cx join_set_join_next_with_cx unix_next_line_with_cx command_output_with_cx

echo
echo "=============================================================="
if [[ ${FAIL} -eq 0 ]]; then
    echo "  ft-xbnl0.2.4 — all 6 runs PASS (${TOTAL_PASSED} tests)"
    echo "=============================================================="
    exit 0
else
    echo "  ft-xbnl0.2.4 — ONE OR MORE RUNS FAILED (${TOTAL_PASSED} tests passed before failure)"
    echo "=============================================================="
    exit 1
fi
