#!/usr/bin/env bash
# ft-xbnl0.2.4 — Run all verification contract tests in sequence.
#
# Invokes the 6 filtered cargo test runs that collectively verify the
# ft-xbnl0.2.4 acceptance contract:
#   1. HTTP client contract tests (34) — distributed_http_client_*
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
#   5. Web server cx pre-cancel + mid-flight (2) — web_server_with_cx_*
#      Tick 323 pre-cancel + tick 417 mid-flight on run_web_server_with_cx.
#   6. runtime_async primitive contracts (22) — _with_cx_observes_budget_deadline
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
# Invoke this script directly. It manages RCH setup internally and refuses local
# Cargo execution. Do not wrap the script itself in `rch exec`.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT_DIR}"

RUN_ID="${RUN_ID:-$(date -u +"%Y%m%dT%H%M%SZ")-$$}"
LOG_DIR="${FT_XBNL0_2_4_LOG_DIR:-target/ft-xbnl0-2-4}"
DEFAULT_CARGO_TARGET_DIR="target/rch-ft-xbnl0-2-4-${RUN_ID}"
REQUESTED_CARGO_TARGET_DIR="${CARGO_TARGET_DIR_OVERRIDE:-${CARGO_TARGET_DIR:-}}"
if [[ -n "$REQUESTED_CARGO_TARGET_DIR" && "$REQUESTED_CARGO_TARGET_DIR" != /* ]]; then
    RCH_CARGO_TARGET_DIR="$REQUESTED_CARGO_TARGET_DIR"
else
    RCH_CARGO_TARGET_DIR="$DEFAULT_CARGO_TARGET_DIR"
fi

mkdir -p "$LOG_DIR"
RCH_SKIP_SMOKE_PREFLIGHT="${FT_XBNL0_2_4_RCH_SKIP_SMOKE_PREFLIGHT:-${RCH_SKIP_SMOKE_PREFLIGHT:-1}}"
RCH_STEP_TIMEOUT_SECS="${FT_XBNL0_2_4_RCH_TIMEOUT_SECS:-${RCH_STEP_TIMEOUT_SECS:-1800}}"
# shellcheck source=tests/e2e/lib_rch_guards.sh
source "$ROOT_DIR/tests/e2e/lib_rch_guards.sh"
rch_init "$LOG_DIR" "$RUN_ID" "ft_xbnl0_2_4" "$ROOT_DIR"
ensure_rch_ready

FAIL=0
TOTAL_PASSED=0
RUN_INDEX=0

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
    RUN_INDEX=$((RUN_INDEX + 1))
    local log_file="$LOG_DIR/run_${RUN_INDEX}_${RUN_ID}.rch.log"

    set +e
    run_rch_cargo_logged "$log_file" \
        env CARGO_TARGET_DIR="$RCH_CARGO_TARGET_DIR" \
        cargo test "$@"
    local rc=$?
    set -e
    cat "$log_file"

    if [[ "$rc" -eq 0 ]]; then
        # Sum all "N passed" counts from all `test result: ok.` lines in
        # this run (per-binary splits may produce multiple result lines
        # under --all-targets; normally there's just one for our filters).
        local passed
        passed="$(grep -oE 'test result: ok\. [0-9]+ passed' "${log_file}" \
            | awk '{ s += $4 } END { print (s ? s : 0) }' || true)"
        TOTAL_PASSED=$(( TOTAL_PASSED + passed ))
        echo "[PASS] ${label} — ${passed} tests"
    else
        echo "[FAIL] ${label}"
        FAIL=1
    fi
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

run_test "Run 5/6: Web server cx pre-cancel + mid-flight" \
    -p frankenterm-core \
    --features web,asupersync-runtime \
    --test web \
    web_server_with_cx_ \
    -- --nocapture

run_test "Run 6/6: runtime_async primitive contracts (budget + cancel observation)" \
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
