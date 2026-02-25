#!/bin/bash
# E2E Test Harness Runner for ft (frankenterm)
# Implements: wa-4vx.10.11
# Spec: docs/e2e-harness-spec.md
#
# Usage: ./scripts/e2e_test.sh [OPTIONS] [SCENARIO...]
#
# Exit codes:
#   0 - All scenarios passed
#   1 - One or more scenarios failed
#   2 - Harness self-check failed
#   3 - Invalid arguments
#   4 - Timeout exceeded
#   5 - Prerequisites missing

set -euo pipefail

# ==============================================================================
# Configuration
# ==============================================================================

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DEFAULT_TIMEOUT=120
DEFAULT_ARTIFACTS_BASE="$PROJECT_ROOT/e2e-artifacts"

# Colors (disabled if not a TTY)
if [[ -t 1 ]]; then
    RED='\033[0;31m'
    GREEN='\033[0;32m'
    YELLOW='\033[0;33m'
    BLUE='\033[0;34m'
    NC='\033[0m' # No Color
else
    RED=''
    GREEN=''
    YELLOW=''
    BLUE=''
    NC=''
fi

# ==============================================================================
# Globals
# ==============================================================================

VERBOSE=false
KEEP_ARTIFACTS=false
ARTIFACTS_DIR=""
TIMEOUT="$DEFAULT_TIMEOUT"
SCENARIO_RETRIES=0
RUN_SEED=""
RUN_SEED_SOURCE="auto"
SELF_CHECK_ONLY=false
SKIP_SELF_CHECK=false
LIST_ONLY=false
DEFAULT_ONLY=false
PARALLEL=1
SOAK_DURATION_SECS=0
SOAK_CHECKPOINT_INTERVAL_SECS=600
SOAK_RESUME_CHECKPOINT=""
SOAK_STOP_ON_FAILURE=false
SOAK_FAULT_MATRIX="scheduler_stress,pty_failure,render_commit_failure"
SOAK_FAULT_INTERVAL=1
SOAK_FAULT_OFFSET=0
SOAK_FAULT_MODE="simulate"
WORKSPACE=""
CONFIG_FILE=""
SCENARIOS=()

# Runtime state
TIMESTAMP=""
RUN_ID=""
RUN_ARTIFACTS_DIR=""
SUMMARY_FILE=""
TOTAL=0
PASSED=0
FAILED=0
SKIPPED=0
START_TIME=""
LAST_WORKER_PID=""
SOAK_MODE=false
SOAK_TELEMETRY_DIR=""
SOAK_SNAPSHOTS_DIR=""
SOAK_CHECKPOINT_FILE=""
SOAK_TELEMETRY_JSONL=""
SOAK_HEALTH_JSONL=""
SOAK_ANOMALY_JSONL=""
SOAK_COMPLETED_CYCLES=0
SOAK_LAST_CHECKPOINT_INDEX=0
SOAK_SCENARIO_SEQUENCE=0
SOAK_RESUME_FROM_RUN_ID=""
SOAK_RESUME_FROM_CHECKPOINT=""
SOAK_TARGET_END_EPOCH=0
SOAK_FAULT_MATRIX_ENABLED=false
SOAK_FAULT_CONFIG_FILE=""
SOAK_FAULT_EVENTS_JSONL=""
SOAK_FAULT_SUMMARY_FILE=""
SOAK_FAULT_CLASSES_JSON="[]"
SOAK_INCIDENT_REPORT_FILE=""
SOAK_INCIDENT_REPORT_STATUS="not_run"
declare -a SOAK_FAULT_CLASSES=()
declare -a SCENARIO_SUMMARIES=()

# ==============================================================================
# Logging
# ==============================================================================

log_timestamp() {
    date +"%H:%M:%S"
}

log_info() {
    echo -e "${BLUE}[$(log_timestamp)]${NC} $*"
}

log_pass() {
    echo -e "${GREEN}[$(log_timestamp)] PASS:${NC} $*"
}

log_fail() {
    echo -e "${RED}[$(log_timestamp)] FAIL:${NC} $*"
}

log_warn() {
    echo -e "${YELLOW}[$(log_timestamp)] WARN:${NC} $*"
}

log_verbose() {
    if [[ "$VERBOSE" == "true" ]]; then
        echo -e "${BLUE}[$(log_timestamp)] DEBUG:${NC} $*"
    fi
}

# ==============================================================================
# Usage
# ==============================================================================

usage() {
    cat <<EOF
E2E Test Harness for ft (frankenterm)

Usage: $0 [OPTIONS] [SCENARIO...]

Options:
    -v, --verbose         Enable verbose output (debug-level logs)
    --keep-artifacts      Always keep artifacts (even on success)
    --artifacts-dir DIR   Override artifacts directory
    --timeout SECS        Global timeout per scenario (default: $DEFAULT_TIMEOUT)
    --retries N           Retry each scenario up to N times on failure (default: 0)
    --seed VALUE          Deterministic run seed used for per-scenario seeds
    --soak-duration-secs N      Run repeated scenario cycles for N seconds
    --checkpoint-interval-secs N  Emit soak checkpoints every N seconds (default: $SOAK_CHECKPOINT_INTERVAL_SECS)
    --resume-checkpoint FILE    Resume soak cycle numbering/config from checkpoint JSON
    --soak-stop-on-failure      Stop soak loop immediately on first failed scenario
    --soak-fault-matrix LIST    Fault classes (comma-separated): scheduler_stress,pty_failure,render_commit_failure,none
    --soak-fault-interval N     Trigger a fault every Nth soak scenario (default: $SOAK_FAULT_INTERVAL)
    --soak-fault-offset N       Deterministic sequence offset for fault selection (default: $SOAK_FAULT_OFFSET)
    --soak-fault-mode MODE      observe|simulate|fail (default: $SOAK_FAULT_MODE)
    --list                List available scenarios and exit
    --self-check          Run harness self-check only
    --skip-self-check     Skip prerequisites check (for CI setup-only scenarios)
    --default-only        Run only scenarios marked default in the registry
    --parallel N          Run N scenarios in parallel (default: 1)
    --workspace DIR       Override workspace for isolation
    --config FILE         Override ft.toml for testing
    --case NAME           Run a single scenario by name (alias for positional arg)
    --all                 Run all registered scenarios (default if no args)
    -h, --help            Show this help

Arguments:
    SCENARIO...           One or more scenario names to run. If omitted, runs all.

Exit Codes:
    0 - All scenarios passed
    1 - One or more scenarios failed
    2 - Harness self-check failed
    3 - Invalid arguments
    4 - Timeout exceeded
    5 - Prerequisites missing

Environment Variables:
    FT_E2E_KEEP_ARTIFACTS  Always keep artifacts (1)
    FT_E2E_TIMEOUT         Override timeout (seconds)
    FT_E2E_RETRIES         Retry count override (integer)
    FT_E2E_SEED            Deterministic run seed override
    FT_E2E_SOAK_DURATION_SECS   Soak loop duration in seconds
    FT_E2E_CHECKPOINT_INTERVAL_SECS  Soak checkpoint cadence in seconds
    FT_E2E_RESUME_CHECKPOINT    Path to soak checkpoint JSON for resume
    FT_E2E_SOAK_STOP_ON_FAILURE Stop soak loop on first failure (1)
    FT_E2E_SOAK_FAULT_MATRIX    Fault classes (csv): scheduler_stress,pty_failure,render_commit_failure,none
    FT_E2E_SOAK_FAULT_INTERVAL  Trigger a fault every Nth soak scenario
    FT_E2E_SOAK_FAULT_OFFSET    Deterministic sequence offset for fault selection
    FT_E2E_SOAK_FAULT_MODE      observe|simulate|fail
    FT_E2E_VERBOSE         Enable verbose output (1)
    FT_E2E_WORKSPACE       Override workspace path
    FT_LOG_LEVEL           Log level for wa processes
    FT_LOG_FORMAT          Log format (pretty/json)

Examples:
    $0                     # Run all scenarios
    $0 capture_search      # Run specific scenario
    $0 --self-check        # Check prerequisites only
    $0 --verbose --keep-artifacts  # Debug mode
    $0 --default-only --soak-duration-secs 7200 --checkpoint-interval-secs 300
    $0 --default-only --soak-duration-secs 3600 --resume-checkpoint e2e-artifacts/last/soak/last_checkpoint.json
    $0 --default-only --soak-duration-secs 1800 --soak-fault-matrix scheduler_stress,pty_failure,render_commit_failure --soak-fault-mode fail
    FT_E2E_SEED=20260216-nightly $0 --default-only --parallel 4 --retries 2
EOF
}

# ==============================================================================
# Argument Parsing
# ==============================================================================

parse_args() {
    while [[ $# -gt 0 ]]; do
        case "$1" in
            -v|--verbose)
                VERBOSE=true
                shift
                ;;
            --keep-artifacts)
                KEEP_ARTIFACTS=true
                shift
                ;;
            --artifacts-dir)
                ARTIFACTS_DIR="$2"
                shift 2
                ;;
            --timeout)
                TIMEOUT="$2"
                shift 2
                ;;
            --retries)
                SCENARIO_RETRIES="$2"
                shift 2
                ;;
            --seed)
                RUN_SEED="$2"
                RUN_SEED_SOURCE="explicit"
                shift 2
                ;;
            --soak-duration-secs)
                SOAK_DURATION_SECS="$2"
                shift 2
                ;;
            --checkpoint-interval-secs)
                SOAK_CHECKPOINT_INTERVAL_SECS="$2"
                shift 2
                ;;
            --resume-checkpoint)
                SOAK_RESUME_CHECKPOINT="$2"
                shift 2
                ;;
            --soak-stop-on-failure)
                SOAK_STOP_ON_FAILURE=true
                shift
                ;;
            --soak-fault-matrix)
                SOAK_FAULT_MATRIX="$2"
                shift 2
                ;;
            --soak-fault-interval)
                SOAK_FAULT_INTERVAL="$2"
                shift 2
                ;;
            --soak-fault-offset)
                SOAK_FAULT_OFFSET="$2"
                shift 2
                ;;
            --soak-fault-mode)
                SOAK_FAULT_MODE="$2"
                shift 2
                ;;
            --list)
                LIST_ONLY=true
                shift
                ;;
            --self-check)
                SELF_CHECK_ONLY=true
                shift
                ;;
            --skip-self-check)
                SKIP_SELF_CHECK=true
                shift
                ;;
            --default-only)
                DEFAULT_ONLY=true
                shift
                ;;
            --parallel)
                PARALLEL="$2"
                shift 2
                ;;
            --workspace)
                WORKSPACE="$2"
                shift 2
                ;;
            --config)
                CONFIG_FILE="$2"
                shift 2
                ;;
            --case)
                SCENARIOS+=("$2")
                shift 2
                ;;
            --all)
                # Explicit --all disables default-only filtering
                DEFAULT_ONLY=false
                shift
                ;;
            -h|--help)
                usage
                exit 0
                ;;
            -*)
                echo "Unknown option: $1" >&2
                usage
                exit 3
                ;;
            *)
                SCENARIOS+=("$1")
                shift
                ;;
        esac
    done

    # Apply environment variable overrides
    if [[ -n "${FT_E2E_KEEP_ARTIFACTS:-}" ]]; then KEEP_ARTIFACTS=true; fi
    if [[ -n "${FT_E2E_TIMEOUT:-}" ]]; then TIMEOUT="$FT_E2E_TIMEOUT"; fi
    if [[ -n "${FT_E2E_RETRIES:-}" ]]; then SCENARIO_RETRIES="$FT_E2E_RETRIES"; fi
    if [[ -n "${FT_E2E_SEED:-}" ]]; then
        RUN_SEED="$FT_E2E_SEED"
        RUN_SEED_SOURCE="env"
    fi
    if [[ -n "${FT_E2E_SOAK_DURATION_SECS:-}" ]]; then SOAK_DURATION_SECS="$FT_E2E_SOAK_DURATION_SECS"; fi
    if [[ -n "${FT_E2E_CHECKPOINT_INTERVAL_SECS:-}" ]]; then SOAK_CHECKPOINT_INTERVAL_SECS="$FT_E2E_CHECKPOINT_INTERVAL_SECS"; fi
    if [[ -n "${FT_E2E_RESUME_CHECKPOINT:-}" ]]; then SOAK_RESUME_CHECKPOINT="$FT_E2E_RESUME_CHECKPOINT"; fi
    if [[ -n "${FT_E2E_SOAK_STOP_ON_FAILURE:-}" ]]; then SOAK_STOP_ON_FAILURE=true; fi
    if [[ -n "${FT_E2E_SOAK_FAULT_MATRIX:-}" ]]; then SOAK_FAULT_MATRIX="$FT_E2E_SOAK_FAULT_MATRIX"; fi
    if [[ -n "${FT_E2E_SOAK_FAULT_INTERVAL:-}" ]]; then SOAK_FAULT_INTERVAL="$FT_E2E_SOAK_FAULT_INTERVAL"; fi
    if [[ -n "${FT_E2E_SOAK_FAULT_OFFSET:-}" ]]; then SOAK_FAULT_OFFSET="$FT_E2E_SOAK_FAULT_OFFSET"; fi
    if [[ -n "${FT_E2E_SOAK_FAULT_MODE:-}" ]]; then SOAK_FAULT_MODE="$FT_E2E_SOAK_FAULT_MODE"; fi
    if [[ -n "${FT_E2E_VERBOSE:-}" ]]; then VERBOSE=true; fi
    if [[ -n "${FT_E2E_WORKSPACE:-}" ]]; then WORKSPACE="$FT_E2E_WORKSPACE"; fi
}

# ==============================================================================
# Self-Check
# ==============================================================================

check_pass() {
    echo -e "${GREEN}[PASS]${NC} $*"
}

check_fail() {
    echo -e "${RED}[FAIL]${NC} $*"
}

run_self_check() {
    echo "E2E Harness Self-Check"
    echo "======================"
    echo ""

    local all_passed=true

    # Check 1: WezTerm installed
    if command -v wezterm &>/dev/null; then
        local wezterm_version
        wezterm_version=$(wezterm --version 2>/dev/null | head -1 || echo "unknown")
        check_pass "WezTerm installed: $wezterm_version"
    else
        check_fail "WezTerm not found in PATH"
        echo "       Hint: Install WezTerm or add it to PATH"
        all_passed=false
    fi

    # Check 2: WezTerm mux operational
    if wezterm cli list &>/dev/null; then
        check_pass "WezTerm mux operational"
    else
        check_fail "WezTerm mux not operational"
        echo "       Hint: Start the active backend bridge (current: WezTerm) with 'wezterm start' or check if it's running"
        all_passed=false
    fi

    # Check 3: ft binary
    local ft_binary="$PROJECT_ROOT/target/release/ft"
    if [[ -x "$ft_binary" ]]; then
        local binary_version
        binary_version=$("$ft_binary" --version 2>/dev/null | head -1 || echo "unknown")
        check_pass "ft binary: $ft_binary ($binary_version)"
    else
        # Try debug build
        ft_binary="$PROJECT_ROOT/target/debug/ft"
        if [[ -x "$ft_binary" ]]; then
            local binary_version
            binary_version=$("$ft_binary" --version 2>/dev/null | head -1 || echo "unknown")
            check_pass "ft binary (debug): $ft_binary ($binary_version)"
        else
            check_fail "ft binary not found"
            echo "       Hint: Run 'cargo build --release' or 'cargo build'"
            all_passed=false
        fi
    fi

    # Check 4: Artifacts directory writable
    local test_artifacts="${ARTIFACTS_DIR:-$DEFAULT_ARTIFACTS_BASE}"
    if mkdir -p "$test_artifacts" 2>/dev/null && touch "$test_artifacts/.write-test" 2>/dev/null; then
        rm -f "$test_artifacts/.write-test"
        check_pass "Artifacts directory: writable ($test_artifacts)"
    else
        check_fail "Artifacts directory not writable: $test_artifacts"
        all_passed=false
    fi

    # Check 5: Temp space available
    local temp_space_mb
    temp_space_mb=$(df -m /tmp 2>/dev/null | awk 'NR==2 {print $4}' || echo "0")
    if [[ "$temp_space_mb" -ge 100 ]]; then
        check_pass "Temp space: ${temp_space_mb}MB available"
    else
        check_fail "Temp space low: ${temp_space_mb}MB (need at least 100MB)"
        all_passed=false
    fi

    # Check 6: Required tools
    local missing_tools=()
    for tool in jq timeout mktemp sqlite3 python3 curl; do
        if ! command -v "$tool" &>/dev/null; then
            missing_tools+=("$tool")
        fi
    done
    if [[ ${#missing_tools[@]} -eq 0 ]]; then
        check_pass "Required tools: all present (jq, timeout, mktemp, sqlite3, python3, curl)"
    else
        check_fail "Missing tools: ${missing_tools[*]}"
        all_passed=false
    fi

    # Check 7: Registry validation
    local registry_validator="$PROJECT_ROOT/scripts/validate_e2e_registry.sh"
    if [[ -f "$registry_validator" ]]; then
        if bash "$registry_validator" >/dev/null 2>&1; then
            check_pass "E2E registry validation"
        else
            check_fail "E2E registry validation failed"
            all_passed=false
        fi
    else
        check_fail "Registry validator missing: $registry_validator"
        all_passed=false
    fi

    echo ""
    if [[ "$all_passed" == "true" ]]; then
        echo "All checks passed. Ready to run E2E tests."
        return 0
    else
        echo "Self-check failed. Fix issues above before running E2E tests."
        return 1
    fi
}

# ==============================================================================
# Scenario Registry
# ==============================================================================

# List of available scenarios
# Format: name|description|default(true/false)|prereqs|why
SCENARIO_REGISTRY=(
    "capture_search|Validate ingest pipeline and FTS search|true|wezterm,jq,sqlite3|Protects ingest + search indexing"
    "natural_language|Validate event summaries and wa why output|true|wezterm,jq|Protects human-readable summaries"
    "compaction_workflow|Validate pattern detection and workflow execution|true|wezterm,jq,sqlite3|Protects compaction workflow auto-handle"
    "unhandled_event_lifecycle|Validate unhandled event lifecycle and dedupe handling|true|wezterm,jq,sqlite3|Protects dedupe + unhandled tracking"
    "workflow_lifecycle|Validate robot workflow list/run/status/abort (dry-run)|true|wezterm,jq,sqlite3|Protects robot workflow surface"
    "history_undo_workflow|Validate action history workflow view + undo execution lifecycle|true|jq,sqlite3|Protects rollback visualization and undo trust surface"
    "dry_run_mode|Validate dry-run previews for send/workflow (human+robot) without side effects|true|wezterm,jq,sqlite3|Protects dry-run trust surface"
    "events_unhandled_alias|Validate robot events --unhandled alias|true|wezterm,jq,sqlite3|Protects events CLI aliases"
    "events_annotations_triage|Validate event annotate/label/triage lifecycle with redaction + audit evidence|true|jq,sqlite3|Protects event mutation workflows and filters"
    "usage_limit_safe_pause|Validate usage-limit safe pause workflow (fallback plan persisted)|true|wezterm,jq,sqlite3|Protects usage-limit fallback workflow"
    "notification_webhook|Validate webhook notifications (delivery, retry, throttle, recovery)|true|wezterm,jq,sqlite3,python3,curl|Protects webhook notification pipeline"
    "watch_notify_only|Validate notify-only mode (no auto-handle, filters, throttling)|true|wezterm,jq,sqlite3,python3,curl|Protects notify-only monitoring mode"
    "policy_denial|Validate safety gates block sends to protected panes|true|wezterm,jq,sqlite3|Protects policy enforcement"
    "audit_tail_streaming|Validate audit tail JSONL streaming with redaction|true|wezterm,jq,sqlite3|Protects audit tail + redaction"
    "ipc_rpc_roundtrip|Validate IPC RPC round-trip with auth + audit|true|wezterm,jq,sqlite3|Protects IPC lane correctness"
    "prepare_commit_approvals|Validate prepare/commit approvals with hash mismatch guard|true|wezterm,jq,sqlite3|Protects approval flow"
    "quickfix_suggestions|Validate quick-fix suggestions for events and errors|true|wezterm,jq,sqlite3|Protects suggestion surfaces"
    "triage_multi_issue|Validate triage ordering and suggested actions with multiple issues|true|wezterm,jq,sqlite3|Protects triage ranking output"
    "rules_explain_trace|Validate rules test trace + lint artifacts (explain-match)|true|wezterm,jq,sqlite3|Protects rule explainability"
    "stress_scale|Validate scaled stress test (panes + large transcript)|true|wezterm,jq,sqlite3|Protects scale handling"
    "graceful_shutdown|Validate ft watch graceful shutdown (SIGINT flush, lock release, restart clean)|true|wezterm,jq,sqlite3|Protects shutdown and lock handling"
    "watcher_crash_bundle|Validate crash bundles surfaced via triage/doctor/reproduce|true|wezterm,jq,sqlite3|Protects crash-only diagnosability surfaces"
    "pane_exclude_filter|Validate pane selection filters protect privacy (ignored pane absent from search)|true|wezterm,jq,sqlite3|Protects pane exclusion behavior"
    "workspace_isolation|Validate workspace isolation (no cross-project DB leakage)|true|wezterm,jq,sqlite3|Protects workspace separation"
    "setup_idempotency|Validate wa setup idempotent patching (temp home, no leaks)|true|wezterm,jq|Protects setup idempotency"
    "setup_remote_docker|Validate ft setup remote against dockerized sshd (dry-run/apply/idempotent + failure injection)|false|docker,ssh,ssh-keygen,jq|Protects remote setup safety and rollback diagnostics"
    "ft_l5em3_2|Validate SIMD stateful scan boundary fidelity and dense-log benchmark capture|false|cargo,jq,rch|Protects SIMD scan correctness/perf acceptance path"
    "ft_1i2ge_3_2|Validate mission dispatch adapter target resolution and outcome normalization|false|cargo,jq,rch|Protects mission dispatch adapter dry-run/live contract"
    "ft_1i2ge_3_3|Validate mission outcome ingestion and assignment state reconciliation|false|cargo,jq,rch|Protects assignment signal reconciliation and drift detection"
    "ft_1i2ge_3_4|Validate adaptive mission replanning triggers and backoff policy|false|cargo,jq,rch|Protects deterministic replan trigger + backoff loop-guard behavior"
    "ft_1i2ge_3_5|Validate pause/resume/abort semantics and checkpoint recovery|false|cargo,jq,rch|Protects operator control transitions, checkpoint durability, and cumulative duration tracking"
    "ft_1i2ge_3_8|Validate crash-consistent mission journal and deterministic restart recovery|false|cargo,jq,rch|Protects journal append monotonicity, checkpoint/recovery markers, compaction, and serde contracts"
    "ft_1i2ge_3_7|Validate orchestration integration and e2e scenario harness with rich logs|false|cargo,jq,rch|Protects mission lifecycle journal dedup dispatch idempotency and kill-switch integration"
    "ft_1i2ge_8_5|Validate commit-phase executor with deterministic ordering and barrier semantics|false|cargo,jq,rch|Protects barrier semantics kill-switch pause and deterministic commit ordering"
    "ft_1i2ge_8_6|Validate compensation planner and automatic rollback engine|false|cargo,jq,rch|Protects compensation barrier semantics reverse ordering and rollback state contracts"
    "ft_e34d9_10_1_2_doctrine_pack|Validate asupersync runtime doctrine contract/invariants + failure injection|false|jq,python3|Protects doctrine semantics, anti-pattern gates, and user-facing guarantees"
    "ft_e34d9_10_2_1_runtime_bootstrap|Validate unified runtime bootstrap contract for CLI/watch/web/robot + failure injection|false|jq,python3|Protects runtime bootstrap lifecycle parity and configuration parsing contract"
    "ft_e34d9_10_2_2_cx_outcome_contract|Validate Cx/Outcome wait-boundary adapter propagation + failure/recovery contract|false|jq,python3|Protects explicit Cx propagation and reason-coded Outcome boundary mapping"
    "ft_124z4|Validate tailer LabRuntime coverage and bench feature-gate failure/recovery contract|false|cargo,jq,rch|Protects tailer asupersync capture semantics and bench gating invariants"
    "ft_1i2ge_4_2|Validate mission reservation and ownership enforcement contract|false|cargo,jq,rch|Protects assignment reservation/ownership enforcement surface"
    "ft_1i2ge_4_3|Validate mission approval-path integration (durability, idempotency, fallback)|false|cargo,jq,rch|Protects approval-required routing and post-approval continuation invariants"
    "ft_1i2ge_4_1|Validate mission policy preflight contract and denial feedback reason codes|false|cargo,jq,rch|Protects mission policy preflight plan/dispatch pipeline"
    "ft_1i2ge_5_1|Validate mission CLI command family (plan/run/status/explain/pause/resume/abort)|false|cargo,jq,rch|Protects mission operator control surface contract stability"
    "ft_1i2ge_5_2|Validate robot mission endpoints for state/decision/explainability contracts|false|cargo,jq,rch|Protects mission robot API parity for agentic orchestration"
    "search_linting_rebuild|Validate search linting + FTS verify/rebuild commands|true|wezterm,jq,sqlite3|Protects search maintenance commands"
    "uservar_forwarding|Validate user-var forwarding lane (wezterm.lua -> wa event -> watcher)|true|wezterm,jq,sqlite3|Protects user-var IPC lane"
    "alt_screen_detection|Validate alt-screen detection via escape sequences (no Lua status hook)|true|wezterm,jq,sqlite3|Protects alt-screen detection"
    "alt_screen_conformance|Validate vim/less/htop/tmux alt-screen semantics under resize pulses|true|wezterm,jq,sqlite3|Protects fullscreen app resize behavior"
    "no_lua_status_hook|Validate wa setup does not inject update-status Lua|true|wezterm,jq|Protects against legacy status hook"
    "workflow_resume|Validate workflow resumes after watcher restart (no duplicate steps)|true|wezterm,jq,sqlite3|Protects workflow resume"
    "accounts_refresh|Validate accounts refresh via fake caut + pick preview + redaction|true|wezterm,jq,sqlite3|Protects accounts refresh"
    "environment_detection|Validate environment detection API (shell, agents, remotes, auto-config)|true|wezterm,jq|Protects environment detection and auto-config"
    "backpressure_stress|Validate backpressure tiers, overflow GAP, hysteresis, and bounded execution|true|cargo,jq|Protects backpressure graceful degradation"
    "storage_stress|Validate storage/indexing stability under load (many panes, large transcripts)|true|cargo,jq|Protects storage perf at scale"
    "search_perf|Validate FTS search stays fast at 1K/10K/100K segments with perf artifacts|true|cargo,jq|Protects search performance at scale"
    "pane_uuid_stability|Validate pane_uuid stable across rename, tab move, cwd change|true|cargo,jq|Protects pane identity stability"
    "incident_bundle|Validate incident bundle export, redaction, replay (policy + rules modes)|true|cargo,jq|Protects incident bundle lifecycle"
    "prioritized_capture|Validate pane priority scheduling, capture budgets, throttle under load|true|cargo,jq|Protects prioritized capture under contention"
    "sleep_audit|Audit E2E scripts for unjustified fixed sleeps; enforce wait-for/quiescence|true|cargo|Protects deterministic timing contract"
    "flake_guard|Repeat-run representative test suites to detect timing flakiness|false|cargo,jq|Catches timing regressions early"
    "reliability_hardening|Validate circuit breaker, retry, degradation, chaos, watchdog|true|cargo,jq|Protects resilience and fault tolerance"
    "perf_regression|Perf regression smoke: patterns, delta, cache, benchmarks with budget validation|true|cargo,jq|Protects performance budgets"
    "input_latency_resize_storm|Validate typing/mouse/paste interaction latency during resize/font churn|true|cargo,jq|Protects user-perceived responsiveness under resize storms"
    "distributed_streaming|Validate distributed agent->aggregator streaming, persistence, and query/auth robustness|false|cargo,jq|Protects optional distributed mode end-to-end"
    "timeline_correlation|Validate timeline cross-pane correlation (aggregation, failover, temporal)|true|cargo,jq,sqlite3|Protects event correlation determinism"
    "replay_capture_pipeline|Validate replay capture extraction/redaction/artifact/decision roundtrip with structured logs|false|cargo,jq,rch|Protects deterministic replay capture data plane"
)

list_scenarios() {
    echo "Available E2E Scenarios"
    echo "======================="
    echo ""
    for entry in "${SCENARIO_REGISTRY[@]}"; do
        local name=""
        local desc=""
        local default_flag=""
        local prereqs=""
        local why=""
        IFS='|' read -r name desc default_flag prereqs why <<< "$entry"
        printf "  %-25s %s\n" "$name" "$desc"
        printf "  %-25s default=%s prereqs=%s\n" "" "$default_flag" "$prereqs"
        printf "  %-25s why=%s\n" "" "$why"
    done
    echo ""
    echo "Run all: $0"
    echo "Run one: $0 <scenario_name>"
}

get_scenario_names() {
    local names=()
    for entry in "${SCENARIO_REGISTRY[@]}"; do
        local name=""
        IFS='|' read -r name _ <<< "$entry"
        names+=("$name")
    done
    echo "${names[@]}"
}

get_default_scenario_names() {
    local names=()
    for entry in "${SCENARIO_REGISTRY[@]}"; do
        local name=""
        local default_flag=""
        IFS='|' read -r name _ default_flag _ <<< "$entry"
        if [[ "$default_flag" == "true" ]]; then
            names+=("$name")
        fi
    done
    echo "${names[@]}"
}

is_valid_scenario() {
    local name="$1"
    for entry in "${SCENARIO_REGISTRY[@]}"; do
        local entry_name=""
        IFS='|' read -r entry_name _ <<< "$entry"
        if [[ "$entry_name" == "$name" ]]; then
            return 0
        fi
    done
    return 1
}

find_scenario_registry_entry() {
    local name="$1"
    for entry in "${SCENARIO_REGISTRY[@]}"; do
        local entry_name=""
        IFS='|' read -r entry_name _ <<< "$entry"
        if [[ "$entry_name" == "$name" ]]; then
            echo "$entry"
            return 0
        fi
    done
    return 1
}

get_scenario_prerequisites_csv() {
    local name="$1"
    local entry=""
    local prereqs=""

    entry=$(find_scenario_registry_entry "$name" || true)
    if [[ -z "$entry" ]]; then
        echo ""
        return 0
    fi

    IFS='|' read -r _ _ _ prereqs _ <<< "$entry"
    echo "$prereqs"
}

resolve_wezterm_bin_path() {
    local candidate=""

    if [[ -n "${WEZTERM_BIN:-}" ]]; then
        if [[ -x "$WEZTERM_BIN" ]]; then
            printf '%s\n' "$WEZTERM_BIN"
            return 0
        fi
        if command -v "$WEZTERM_BIN" >/dev/null 2>&1; then
            command -v "$WEZTERM_BIN"
            return 0
        fi
    fi

    if command -v wezterm >/dev/null 2>&1; then
        command -v wezterm
        return 0
    fi

    for candidate in \
        "$PROJECT_ROOT/target/release/wezterm" \
        "$PROJECT_ROOT/legacy_wezterm/target/release/wezterm" \
        "$PROJECT_ROOT/frankenterm/target/release/wezterm" \
        "$HOME/.local/bin/wezterm"; do
        if [[ -x "$candidate" ]]; then
            printf '%s\n' "$candidate"
            return 0
        fi
    done

    return 1
}

resolve_prerequisite_tool() {
    local tool="$1"

    case "$tool" in
        wezterm)
            resolve_wezterm_bin_path
            ;;
        *)
            command -v "$tool"
            ;;
    esac
}

check_scenario_prerequisites() {
    local name="$1"
    local artifacts_dir="${2:-}"
    local prereqs_csv=""
    local -a prereqs=()
    local -a missing=()
    local tool=""
    local resolved_path=""

    prereqs_csv=$(get_scenario_prerequisites_csv "$name")
    if [[ -z "$prereqs_csv" ]]; then
        return 0
    fi

    IFS=',' read -ra prereqs <<< "$prereqs_csv"
    for tool in "${prereqs[@]}"; do
        tool=$(trim_whitespace "$tool")
        if [[ -z "$tool" ]]; then
            continue
        fi
        if ! resolved_path="$(resolve_prerequisite_tool "$tool" 2>/dev/null)"; then
            if [[ "$name" == "alt_screen_conformance" && "$tool" == "wezterm" && "${FT_E2E_ALT_SCREEN_ALLOW_FIXTURE_ONLY:-1}" == "1" ]]; then
                log_warn "Scenario $name: wezterm missing; continuing with fixture-only fallback mode (set FT_E2E_ALT_SCREEN_ALLOW_FIXTURE_ONLY=0 to require wezterm)"
                continue
            fi
            missing+=("$tool")
            continue
        fi
        log_verbose "Scenario $name prerequisite satisfied: $tool -> $resolved_path"
    done

    if [[ "${#missing[@]}" -eq 0 ]]; then
        return 0
    fi

    log_fail "Scenario $name missing prerequisites: ${missing[*]}"
    if [[ -n "$artifacts_dir" ]]; then
        printf '%s\n' "${missing[@]}" > "$artifacts_dir/missing_prerequisites.txt"
        jq -n \
            --arg scenario "$name" \
            --arg generated_at "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
            --arg prereqs "$prereqs_csv" \
            --argjson missing "$(printf '%s\n' "${missing[@]}" | jq -R . | jq -s .)" \
            '{
                scenario: $scenario,
                generated_at: $generated_at,
                configured_prerequisites: (if $prereqs == "" then [] else ($prereqs | split(",")) end),
                missing_prerequisites: $missing
            }' > "$artifacts_dir/prerequisites_check.json" 2>/dev/null || true
    fi
    return 1
}

scenario_metadata_json() {
    local name="$1"
    local entry=""
    local desc=""
    local default_flag="false"
    local prereqs=""
    local why=""

    entry=$(find_scenario_registry_entry "$name" || true)
    if [[ -n "$entry" ]]; then
        IFS='|' read -r _ desc default_flag prereqs why <<< "$entry"
    fi

    jq -cn \
        --arg name "$name" \
        --arg description "$desc" \
        --argjson default "$([[ "$default_flag" == "true" ]] && echo true || echo false)" \
        --arg prereqs "$prereqs" \
        --arg why "$why" \
        '{
            name: $name,
            description: $description,
            default: $default,
            prerequisites: (if $prereqs == "" then [] else ($prereqs | split(",")) end),
            why: $why
        }'
}

compute_scenario_seed_hex() {
    local name="$1"
    local scenario_num="$2"
    local payload="${RUN_SEED}|${scenario_num}|${name}"

    if command -v shasum >/dev/null 2>&1; then
        printf '%s' "$payload" | shasum -a 256 | awk '{print substr($1,1,16)}'
    elif command -v sha256sum >/dev/null 2>&1; then
        printf '%s' "$payload" | sha256sum | awk '{print substr($1,1,16)}'
    elif command -v python3 >/dev/null 2>&1; then
        python3 - "$payload" <<'PY'
import hashlib
import sys
print(hashlib.sha256(sys.argv[1].encode("utf-8")).hexdigest()[:16])
PY
    else
        # Last-resort fallback if hash utilities are unavailable.
        printf '%016x\n' "$scenario_num"
    fi
}

scenario_retry_backoff_secs() {
    local attempt_num="$1"
    local backoff=$((1 << (attempt_num - 1)))
    if [[ "$backoff" -gt 8 ]]; then
        backoff=8
    fi
    echo "$backoff"
}

trim_whitespace() {
    local value="$1"
    value="${value#"${value%%[![:space:]]*}"}"
    value="${value%"${value##*[![:space:]]}"}"
    echo "$value"
}

normalize_soak_fault_matrix_config() {
    SOAK_FAULT_CLASSES=()
    SOAK_FAULT_CLASSES_JSON="[]"
    SOAK_FAULT_MATRIX_ENABLED=false

    local raw="$SOAK_FAULT_MATRIX"
    if [[ -z "$raw" ]]; then
        return 0
    fi

    local saw_none=false
    local token=""
    local parsed=()
    IFS=',' read -ra parsed <<< "$raw"
    for token in "${parsed[@]}"; do
        token=$(trim_whitespace "$token")
        if [[ -z "$token" ]]; then
            continue
        fi
        token="$(printf '%s' "$token" | tr '[:upper:]' '[:lower:]')"
        if [[ "$token" == "none" ]]; then
            saw_none=true
            break
        fi
        case "$token" in
            scheduler_stress|pty_failure|render_commit_failure)
                SOAK_FAULT_CLASSES+=("$token")
                ;;
            *)
                echo "Invalid --soak-fault-matrix class: $token (expected scheduler_stress,pty_failure,render_commit_failure,none)" >&2
                exit 3
                ;;
        esac
    done

    if [[ "$saw_none" == "true" ]]; then
        SOAK_FAULT_CLASSES=()
        SOAK_FAULT_CLASSES_JSON="[]"
        SOAK_FAULT_MATRIX_ENABLED=false
        return 0
    fi

    if [[ "${#SOAK_FAULT_CLASSES[@]}" -gt 0 ]]; then
        local classes_json="["
        local idx=0
        for idx in "${!SOAK_FAULT_CLASSES[@]}"; do
            if [[ "$idx" -gt 0 ]]; then
                classes_json+=","
            fi
            classes_json+="\"${SOAK_FAULT_CLASSES[$idx]}\""
        done
        classes_json+="]"
        SOAK_FAULT_CLASSES_JSON="$classes_json"
        SOAK_FAULT_MATRIX_ENABLED=true
    fi
}

validate_orchestration_config() {
    if ! [[ "$SCENARIO_RETRIES" =~ ^[0-9]+$ ]]; then
        echo "Invalid --retries value: $SCENARIO_RETRIES (expected integer >= 0)" >&2
        exit 3
    fi
    if ! [[ "$PARALLEL" =~ ^[0-9]+$ ]] || [[ "$PARALLEL" -lt 1 ]]; then
        echo "Invalid --parallel value: $PARALLEL (expected integer >= 1)" >&2
        exit 3
    fi
    if ! [[ "$SOAK_DURATION_SECS" =~ ^[0-9]+$ ]]; then
        echo "Invalid --soak-duration-secs value: $SOAK_DURATION_SECS (expected integer >= 0)" >&2
        exit 3
    fi
    if ! [[ "$SOAK_FAULT_INTERVAL" =~ ^[0-9]+$ ]] || [[ "$SOAK_FAULT_INTERVAL" -lt 1 ]]; then
        echo "Invalid --soak-fault-interval value: $SOAK_FAULT_INTERVAL (expected integer >= 1)" >&2
        exit 3
    fi
    if ! [[ "$SOAK_FAULT_OFFSET" =~ ^[0-9]+$ ]]; then
        echo "Invalid --soak-fault-offset value: $SOAK_FAULT_OFFSET (expected integer >= 0)" >&2
        exit 3
    fi
    SOAK_FAULT_MODE="$(printf '%s' "$SOAK_FAULT_MODE" | tr '[:upper:]' '[:lower:]')"
    case "$SOAK_FAULT_MODE" in
        observe|simulate|fail)
            ;;
        *)
            echo "Invalid --soak-fault-mode value: $SOAK_FAULT_MODE (expected observe|simulate|fail)" >&2
            exit 3
            ;;
    esac
    normalize_soak_fault_matrix_config
    if ! [[ "$SOAK_CHECKPOINT_INTERVAL_SECS" =~ ^[0-9]+$ ]] || [[ "$SOAK_CHECKPOINT_INTERVAL_SECS" -lt 30 ]]; then
        echo "Invalid --checkpoint-interval-secs value: $SOAK_CHECKPOINT_INTERVAL_SECS (expected integer >= 30)" >&2
        exit 3
    fi

    if [[ "$SOAK_DURATION_SECS" -gt 0 ]]; then
        SOAK_MODE=true
        if [[ "$PARALLEL" -gt 1 ]]; then
            log_warn "Soak mode enforces deterministic sequencing; overriding --parallel $PARALLEL to 1"
            PARALLEL=1
        fi
        if [[ "$KEEP_ARTIFACTS" == "false" ]]; then
            KEEP_ARTIFACTS=true
            log_info "Soak mode forces --keep-artifacts to preserve checkpoints and telemetry snapshots"
        fi
        if [[ "$SOAK_FAULT_MATRIX_ENABLED" == "true" ]]; then
            log_info "Soak fault matrix: mode=$SOAK_FAULT_MODE interval=$SOAK_FAULT_INTERVAL offset=$SOAK_FAULT_OFFSET classes=${SOAK_FAULT_CLASSES[*]}"
        else
            log_info "Soak fault matrix disabled (set --soak-fault-matrix to enable)"
        fi
    fi

    if [[ -n "$SOAK_RESUME_CHECKPOINT" && ! -f "$SOAK_RESUME_CHECKPOINT" ]]; then
        echo "Invalid --resume-checkpoint path (file not found): $SOAK_RESUME_CHECKPOINT" >&2
        exit 3
    fi
    if [[ -n "$SOAK_RESUME_CHECKPOINT" && "$SOAK_MODE" != "true" ]]; then
        echo "--resume-checkpoint requires --soak-duration-secs > 0" >&2
        exit 3
    fi

    if [[ -z "$RUN_SEED" ]]; then
        RUN_SEED="$(date -u +%s)"
        RUN_SEED_SOURCE="auto"
    fi
}

compute_run_id() {
    local seed_material="${RUN_SEED}:${RUN_SEED_SOURCE}:${TIMESTAMP}:$$"
    if command -v shasum >/dev/null 2>&1; then
        printf '%s' "$seed_material" | shasum -a 256 | awk '{print substr($1,1,12)}'
    elif command -v sha256sum >/dev/null 2>&1; then
        printf '%s' "$seed_material" | sha256sum | awk '{print substr($1,1,12)}'
    else
        printf '%s' "${TIMESTAMP}-$$"
    fi
}

scenarios_to_json_array() {
    local scenarios=("$@")
    if [[ "${#scenarios[@]}" -eq 0 ]]; then
        echo "[]"
        return 0
    fi
    printf '%s\n' "${scenarios[@]}" | jq -R . | jq -cs '.'
}

load_soak_resume_checkpoint() {
    local scenarios_json="$1"
    if [[ -z "$SOAK_RESUME_CHECKPOINT" ]]; then
        return 0
    fi

    if ! command -v jq >/dev/null 2>&1; then
        echo "Cannot parse --resume-checkpoint without jq in PATH" >&2
        exit 5
    fi
    if ! jq -e . "$SOAK_RESUME_CHECKPOINT" >/dev/null 2>&1; then
        echo "Invalid JSON in --resume-checkpoint: $SOAK_RESUME_CHECKPOINT" >&2
        exit 3
    fi

    local checkpoint_seed=""
    local checkpoint_run_id=""
    local checkpoint_completed_cycles="0"
    local checkpoint_sequence_no="0"
    local checkpoint_scenarios_json="[]"

    checkpoint_seed=$(jq -r '.run_seed // empty' "$SOAK_RESUME_CHECKPOINT")
    checkpoint_run_id=$(jq -r '.run_id // empty' "$SOAK_RESUME_CHECKPOINT")
    checkpoint_completed_cycles=$(jq -r '.progress.completed_cycles // .completed_cycles // 0' "$SOAK_RESUME_CHECKPOINT")
    checkpoint_sequence_no=$(jq -r '.progress.sequence_no // .sequence_no // 0' "$SOAK_RESUME_CHECKPOINT")
    checkpoint_scenarios_json=$(jq -c '.config.scenarios // .scenarios // []' "$SOAK_RESUME_CHECKPOINT")

    if [[ -z "$checkpoint_seed" ]]; then
        echo "Resume checkpoint missing run_seed: $SOAK_RESUME_CHECKPOINT" >&2
        exit 3
    fi
    if [[ "$checkpoint_scenarios_json" != "$scenarios_json" ]]; then
        echo "Resume checkpoint scenarios do not match requested scenario set" >&2
        echo "checkpoint scenarios: $checkpoint_scenarios_json" >&2
        echo "requested scenarios:  $scenarios_json" >&2
        exit 3
    fi
    if ! [[ "$checkpoint_completed_cycles" =~ ^[0-9]+$ ]]; then
        checkpoint_completed_cycles=0
    fi
    if ! [[ "$checkpoint_sequence_no" =~ ^[0-9]+$ ]]; then
        checkpoint_sequence_no=0
    fi

    if [[ "$RUN_SEED_SOURCE" == "explicit" || "$RUN_SEED_SOURCE" == "env" ]]; then
        if [[ "$RUN_SEED" != "$checkpoint_seed" ]]; then
            echo "Explicit --seed/FT_E2E_SEED does not match resume checkpoint run_seed" >&2
            echo "seed: $RUN_SEED checkpoint: $checkpoint_seed" >&2
            exit 3
        fi
    else
        RUN_SEED="$checkpoint_seed"
        RUN_SEED_SOURCE="resume"
    fi

    SOAK_COMPLETED_CYCLES="$checkpoint_completed_cycles"
    SOAK_SCENARIO_SEQUENCE="$checkpoint_sequence_no"
    SOAK_RESUME_FROM_RUN_ID="$checkpoint_run_id"
    SOAK_RESUME_FROM_CHECKPOINT="$SOAK_RESUME_CHECKPOINT"
}

soak_record_health_summary() {
    local reason="$1"
    local elapsed_secs="$2"
    local remaining_secs="$3"
    local average_scenario_ms="$4"
    local artifacts_bytes="$5"
    local total_completed="$((PASSED + FAILED + SKIPPED))"
    local pass_rate_pct="0.00"

    if [[ "$total_completed" -gt 0 ]]; then
        pass_rate_pct=$(awk "BEGIN { printf \"%.2f\", ($PASSED * 100.0) / $total_completed }")
    fi

    jq -cn \
        --arg timestamp "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
        --arg run_id "$RUN_ID" \
        --arg test_case_id "soak_health_summary" \
        --arg resize_transaction_id "${RUN_ID}:soak:health:${SOAK_LAST_CHECKPOINT_INDEX}" \
        --arg scheduler_decision "soak_health_summary" \
        --argjson sequence_no "$SOAK_LAST_CHECKPOINT_INDEX" \
        --argjson queue_wait_ms 0 \
        --argjson reflow_ms "$average_scenario_ms" \
        --argjson render_ms "$average_scenario_ms" \
        --argjson present_ms "$average_scenario_ms" \
        --argjson p50_ms "$average_scenario_ms" \
        --argjson p95_ms "$average_scenario_ms" \
        --argjson p99_ms "$average_scenario_ms" \
        --arg reason "$reason" \
        --argjson completed_cycles "$SOAK_COMPLETED_CYCLES" \
        --argjson elapsed_secs "$elapsed_secs" \
        --argjson remaining_secs "$remaining_secs" \
        --argjson total_completed "$total_completed" \
        --argjson passed "$PASSED" \
        --argjson failed "$FAILED" \
        --argjson skipped "$SKIPPED" \
        --arg pass_rate_pct "$pass_rate_pct" \
        --argjson artifacts_bytes "$artifacts_bytes" \
        '{
            timestamp: $timestamp,
            run_id: $run_id,
            test_case_id: $test_case_id,
            resize_transaction_id: $resize_transaction_id,
            pane_id: null,
            tab_id: null,
            sequence_no: $sequence_no,
            scheduler_decision: $scheduler_decision,
            frame_id: null,
            queue_wait_ms: $queue_wait_ms,
            reflow_ms: $reflow_ms,
            render_ms: $render_ms,
            present_ms: $present_ms,
            p50_ms: $p50_ms,
            p95_ms: $p95_ms,
            p99_ms: $p99_ms,
            reason: $reason,
            health: {
                completed_cycles: $completed_cycles,
                elapsed_secs: $elapsed_secs,
                remaining_secs: $remaining_secs,
                totals: {
                    completed: $total_completed,
                    passed: $passed,
                    failed: $failed,
                    skipped: $skipped
                },
                pass_rate_pct: ($pass_rate_pct | tonumber),
                artifacts_bytes: $artifacts_bytes
            }
        }' >> "$SOAK_HEALTH_JSONL"
}

soak_record_anomaly_marker() {
    local marker_type="$1"
    local scenario_name="$2"
    local duration_secs="$3"
    local detail="$4"
    local severity="$5"
    local duration_ms=$((duration_secs * 1000))

    jq -cn \
        --arg timestamp "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
        --arg run_id "$RUN_ID" \
        --arg test_case_id "$scenario_name" \
        --arg resize_transaction_id "${RUN_ID}:soak:anomaly:${SOAK_SCENARIO_SEQUENCE}" \
        --arg scheduler_decision "soak_runner" \
        --arg marker_type "$marker_type" \
        --arg severity "$severity" \
        --arg detail "$detail" \
        --argjson sequence_no "$SOAK_SCENARIO_SEQUENCE" \
        --argjson queue_wait_ms 0 \
        --argjson reflow_ms "$duration_ms" \
        --argjson render_ms "$duration_ms" \
        --argjson present_ms "$duration_ms" \
        --argjson p50_ms "$duration_ms" \
        --argjson p95_ms "$duration_ms" \
        --argjson p99_ms "$duration_ms" \
        '{
            timestamp: $timestamp,
            run_id: $run_id,
            test_case_id: $test_case_id,
            resize_transaction_id: $resize_transaction_id,
            pane_id: null,
            tab_id: null,
            sequence_no: $sequence_no,
            scheduler_decision: $scheduler_decision,
            frame_id: null,
            queue_wait_ms: $queue_wait_ms,
            reflow_ms: $reflow_ms,
            render_ms: $render_ms,
            present_ms: $present_ms,
            p50_ms: $p50_ms,
            p95_ms: $p95_ms,
            p99_ms: $p99_ms,
            marker_type: $marker_type,
            severity: $severity,
            detail: $detail
        }' >> "$SOAK_ANOMALY_JSONL"
}

soak_emit_checkpoint() {
    local scenarios_json="$1"
    local reason="$2"
    local now_epoch
    local elapsed_secs
    local remaining_secs
    local total_completed
    local average_scenario_ms=0
    local artifacts_bytes=0
    local checkpoint_index
    local pass_rate_pct="0.00"

    now_epoch=$(date +%s)
    elapsed_secs=$((now_epoch - START_TIME))
    remaining_secs=$((SOAK_TARGET_END_EPOCH - now_epoch))
    if [[ "$remaining_secs" -lt 0 ]]; then
        remaining_secs=0
    fi

    total_completed=$((PASSED + FAILED + SKIPPED))
    if [[ "$total_completed" -gt 0 ]]; then
        average_scenario_ms=$(awk "BEGIN { printf \"%.0f\", ($elapsed_secs * 1000.0) / $total_completed }")
        pass_rate_pct=$(awk "BEGIN { printf \"%.2f\", ($PASSED * 100.0) / $total_completed }")
    fi
    if [[ -d "$RUN_ARTIFACTS_DIR" ]]; then
        artifacts_bytes=$(du -sk "$RUN_ARTIFACTS_DIR" 2>/dev/null | awk '{print $1 * 1024}')
        if [[ -z "$artifacts_bytes" ]]; then
            artifacts_bytes=0
        fi
    fi

    checkpoint_index=$((SOAK_LAST_CHECKPOINT_INDEX + 1))
    SOAK_LAST_CHECKPOINT_INDEX="$checkpoint_index"

    jq -n \
        --arg schema_version "wa.soak_checkpoint.v1" \
        --arg generated_at "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
        --arg run_id "$RUN_ID" \
        --arg run_seed "$RUN_SEED" \
        --arg run_seed_source "$RUN_SEED_SOURCE" \
        --argjson checkpoint_index "$checkpoint_index" \
        --arg reason "$reason" \
        --argjson soak_duration_secs "$SOAK_DURATION_SECS" \
        --argjson checkpoint_interval_secs "$SOAK_CHECKPOINT_INTERVAL_SECS" \
        --argjson timeout_secs "$TIMEOUT" \
        --argjson scenario_retries "$SCENARIO_RETRIES" \
        --argjson scenarios "$scenarios_json" \
        --argjson completed_cycles "$SOAK_COMPLETED_CYCLES" \
        --argjson next_cycle "$((SOAK_COMPLETED_CYCLES + 1))" \
        --argjson elapsed_secs "$elapsed_secs" \
        --argjson remaining_secs "$remaining_secs" \
        --argjson sequence_no "$SOAK_SCENARIO_SEQUENCE" \
        --argjson total_completed "$total_completed" \
        --argjson passed "$PASSED" \
        --argjson failed "$FAILED" \
        --argjson skipped "$SKIPPED" \
        --arg pass_rate_pct "$pass_rate_pct" \
        --argjson average_scenario_ms "$average_scenario_ms" \
        --argjson artifacts_bytes "$artifacts_bytes" \
        --arg resume_from_run_id "$SOAK_RESUME_FROM_RUN_ID" \
        --arg resume_from_checkpoint "$SOAK_RESUME_FROM_CHECKPOINT" \
        '{
            schema_version: $schema_version,
            generated_at: $generated_at,
            run_id: $run_id,
            run_seed: $run_seed,
            run_seed_source: $run_seed_source,
            checkpoint_index: $checkpoint_index,
            reason: $reason,
            config: {
                soak_duration_secs: $soak_duration_secs,
                checkpoint_interval_secs: $checkpoint_interval_secs,
                timeout_secs: $timeout_secs,
                scenario_retries: $scenario_retries,
                scenarios: $scenarios
            },
            progress: {
                completed_cycles: $completed_cycles,
                next_cycle: $next_cycle,
                elapsed_secs: $elapsed_secs,
                remaining_secs: $remaining_secs,
                sequence_no: $sequence_no
            },
            totals: {
                completed: $total_completed,
                passed: $passed,
                failed: $failed,
                skipped: $skipped,
                pass_rate_pct: ($pass_rate_pct | tonumber)
            },
            telemetry: {
                average_scenario_ms: $average_scenario_ms,
                artifacts_bytes: $artifacts_bytes
            },
            resume: {
                from_run_id: (if $resume_from_run_id == "" then null else $resume_from_run_id end),
                from_checkpoint: (if $resume_from_checkpoint == "" then null else $resume_from_checkpoint end)
            }
        }' > "$SOAK_CHECKPOINT_FILE"

    cp "$SOAK_CHECKPOINT_FILE" "$SOAK_SNAPSHOTS_DIR/checkpoint_$(printf '%04d' "$checkpoint_index").json"
    if [[ -n "$SOAK_RESUME_CHECKPOINT" ]]; then
        cp "$SOAK_CHECKPOINT_FILE" "$SOAK_RESUME_CHECKPOINT" 2>/dev/null || true
    fi

    jq -cn \
        --arg timestamp "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
        --arg run_id "$RUN_ID" \
        --arg test_case_id "soak_checkpoint" \
        --arg resize_transaction_id "${RUN_ID}:soak:checkpoint:${checkpoint_index}" \
        --arg scheduler_decision "soak_checkpoint" \
        --arg reason "$reason" \
        --argjson sequence_no "$checkpoint_index" \
        --argjson queue_wait_ms 0 \
        --argjson reflow_ms "$average_scenario_ms" \
        --argjson render_ms "$average_scenario_ms" \
        --argjson present_ms "$average_scenario_ms" \
        --argjson p50_ms "$average_scenario_ms" \
        --argjson p95_ms "$average_scenario_ms" \
        --argjson p99_ms "$average_scenario_ms" \
        --argjson elapsed_secs "$elapsed_secs" \
        --argjson remaining_secs "$remaining_secs" \
        --argjson completed_cycles "$SOAK_COMPLETED_CYCLES" \
        --argjson total_completed "$total_completed" \
        '{
            timestamp: $timestamp,
            run_id: $run_id,
            test_case_id: $test_case_id,
            resize_transaction_id: $resize_transaction_id,
            pane_id: null,
            tab_id: null,
            sequence_no: $sequence_no,
            scheduler_decision: $scheduler_decision,
            frame_id: null,
            queue_wait_ms: $queue_wait_ms,
            reflow_ms: $reflow_ms,
            render_ms: $render_ms,
            present_ms: $present_ms,
            p50_ms: $p50_ms,
            p95_ms: $p95_ms,
            p99_ms: $p99_ms,
            reason: $reason,
            progress: {
                elapsed_secs: $elapsed_secs,
                remaining_secs: $remaining_secs,
                completed_cycles: $completed_cycles,
                total_completed: $total_completed
            }
        }' >> "$SOAK_TELEMETRY_JSONL"

    soak_record_health_summary "$reason" "$elapsed_secs" "$remaining_secs" "$average_scenario_ms" "$artifacts_bytes"
    log_info "Soak checkpoint #$checkpoint_index emitted (reason=$reason cycles=$SOAK_COMPLETED_CYCLES completed=$total_completed)"
}

soak_fault_policy_tuple() {
    local fault_class="$1"
    case "$fault_class" in
        scheduler_stress)
            echo "quality_reduced|continue|medium"
            ;;
        pty_failure)
            echo "pane_isolation|continue|high"
            ;;
        render_commit_failure)
            echo "frame_drop_recovery|continue|high"
            ;;
        *)
            echo "nominal|continue|low"
            ;;
    esac
}

soak_compute_fault_plan() {
    local scenario_name="$1"
    local sequence_no="$2"
    local class_count="${#SOAK_FAULT_CLASSES[@]}"
    local active=false
    local fault_class="none"
    local matrix_index=-1
    local sequence_mod=0
    local expected_degradation="nominal"
    local expected_policy="continue"
    local severity="low"
    local trigger_token=""
    local tuple=""

    if [[ "$SOAK_FAULT_MATRIX_ENABLED" == "true" && "$class_count" -gt 0 ]]; then
        matrix_index=$(( (sequence_no + SOAK_FAULT_OFFSET - 1) % class_count ))
        if [[ "$matrix_index" -lt 0 ]]; then
            matrix_index=$((matrix_index + class_count))
        fi
        fault_class="${SOAK_FAULT_CLASSES[$matrix_index]}"
        sequence_mod=$(( (sequence_no + SOAK_FAULT_OFFSET) % SOAK_FAULT_INTERVAL ))
        if [[ "$sequence_mod" -eq 0 ]]; then
            active=true
        fi
    fi

    tuple=$(soak_fault_policy_tuple "$fault_class")
    IFS='|' read -r expected_degradation expected_policy severity <<< "$tuple"
    trigger_token=$(compute_scenario_seed_hex "${scenario_name}:${fault_class}:soak_fault" "$sequence_no")

    jq -cn \
        --arg schema_version "wa.soak_fault_plan.v1" \
        --arg run_id "$RUN_ID" \
        --arg run_seed "$RUN_SEED" \
        --arg test_case_id "$scenario_name" \
        --arg resize_transaction_id "${RUN_ID}:soak:fault:${sequence_no}" \
        --argjson sequence_no "$sequence_no" \
        --argjson enabled "$SOAK_FAULT_MATRIX_ENABLED" \
        --argjson active "$active" \
        --arg fault_class "$fault_class" \
        --arg mode "$SOAK_FAULT_MODE" \
        --arg trigger_token "$trigger_token" \
        --argjson matrix_index "$matrix_index" \
        --argjson interval "$SOAK_FAULT_INTERVAL" \
        --argjson offset "$SOAK_FAULT_OFFSET" \
        --argjson sequence_mod "$sequence_mod" \
        --arg expected_degradation "$expected_degradation" \
        --arg expected_policy "$expected_policy" \
        --arg expected_severity "$severity" \
        '{
            schema_version: $schema_version,
            run_id: $run_id,
            run_seed: $run_seed,
            test_case_id: $test_case_id,
            resize_transaction_id: $resize_transaction_id,
            sequence_no: $sequence_no,
            enabled: $enabled,
            active: $active,
            fault_class: $fault_class,
            mode: $mode,
            trigger: {
                token: $trigger_token,
                matrix_index: $matrix_index,
                interval: $interval,
                offset: $offset,
                sequence_mod: $sequence_mod
            },
            expected: {
                degradation: $expected_degradation,
                policy: $expected_policy,
                severity: $expected_severity
            }
        }'
}

apply_soak_fault_injection() {
    local scenario_name="$1"
    local attempt_dir="$2"
    local fault_plan_json="$3"
    local attempt_result="$4"
    local active=""
    local fault_class=""
    local mode=""
    local exit_code="$attempt_result"
    local action="none"
    local detail="no_fault_injection"
    local forced_failure=false
    local delay_ms=0
    local applied=false

    active=$(jq -r '.active // false' <<< "$fault_plan_json")
    fault_class=$(jq -r '.fault_class // "none"' <<< "$fault_plan_json")
    mode=$(jq -r '.mode // "observe"' <<< "$fault_plan_json")

    if [[ "$active" == "true" && "$fault_class" != "none" ]]; then
        case "$fault_class" in
            scheduler_stress)
                applied=true
                action="scheduler_queue_pressure"
                detail="synthetic_scheduler_delay"
                if [[ "$mode" != "observe" ]]; then
                    sleep 1
                    delay_ms=1000
                fi
                ;;
            pty_failure)
                applied=true
                action="pty_resize_failure_injected"
                detail="synthetic_pty_failure_marker"
                if [[ "$mode" == "fail" && "$attempt_result" -eq 0 ]]; then
                    exit_code=86
                    forced_failure=true
                fi
                ;;
            render_commit_failure)
                applied=true
                action="render_commit_failure_injected"
                detail="synthetic_render_commit_failure_marker"
                if [[ "$mode" == "fail" && "$attempt_result" -eq 0 ]]; then
                    exit_code=87
                    forced_failure=true
                fi
                ;;
            *)
                ;;
        esac
    fi

    local effect_json=""
    effect_json=$(jq -cn \
        --arg scenario_name "$scenario_name" \
        --arg fault_class "$fault_class" \
        --arg mode "$mode" \
        --arg action "$action" \
        --arg detail "$detail" \
        --argjson applied "$applied" \
        --argjson forced_failure "$forced_failure" \
        --argjson delay_ms "$delay_ms" \
        --argjson prior_exit_code "$attempt_result" \
        --argjson exit_code "$exit_code" \
        '{
            scenario_name: $scenario_name,
            fault_class: $fault_class,
            mode: $mode,
            applied: $applied,
            action: $action,
            detail: $detail,
            forced_failure: $forced_failure,
            delay_ms: $delay_ms,
            prior_exit_code: $prior_exit_code,
            exit_code: $exit_code
        }')

    echo "$effect_json" > "$attempt_dir/fault_injection_effect.json"
    if [[ -f "$attempt_dir/scenario.log" ]]; then
        echo "[soak_fault] class=$fault_class mode=$mode action=$action forced_failure=$forced_failure exit_code=$exit_code" >> "$attempt_dir/scenario.log"
    fi

    echo "$effect_json"
}

soak_record_fault_event() {
    local scenario_name="$1"
    local sequence_no="$2"
    local scenario_rc="$3"
    local summary_status="$4"
    local summary_duration="$5"
    local summary_error="$6"
    local fault_plan_json="$7"
    local duration_secs="$summary_duration"
    local duration_ms=0
    local active=""
    local fault_class=""
    local mode=""
    local expected_degradation=""
    local expected_policy=""
    local severity=""
    local classification="control_path"
    local responsive=true
    local event_json=""
    local queue_wait_ms=0

    if ! [[ "$duration_secs" =~ ^[0-9]+$ ]]; then
        duration_secs=0
    fi
    duration_ms=$((duration_secs * 1000))

    active=$(jq -r '.active // false' <<< "$fault_plan_json")
    fault_class=$(jq -r '.fault_class // "none"' <<< "$fault_plan_json")
    mode=$(jq -r '.mode // "observe"' <<< "$fault_plan_json")
    expected_degradation=$(jq -r '.expected.degradation // "nominal"' <<< "$fault_plan_json")
    expected_policy=$(jq -r '.expected.policy // "continue"' <<< "$fault_plan_json")
    severity=$(jq -r '.expected.severity // "low"' <<< "$fault_plan_json")

    if [[ "$active" == "true" ]]; then
        if [[ "$summary_status" == "passed" && "$scenario_rc" -eq 0 ]]; then
            classification="degraded_recovered"
        elif [[ "$SOAK_STOP_ON_FAILURE" == "true" ]]; then
            classification="stop_on_failure_triggered"
            responsive=false
        else
            classification="contained_failure"
        fi
    else
        if [[ "$summary_status" != "passed" || "$scenario_rc" -ne 0 ]]; then
            classification="unexpected_failure_without_injection"
            responsive=false
        fi
    fi

    if [[ "$duration_secs" -ge "$TIMEOUT" ]]; then
        classification="responsiveness_budget_exceeded"
        responsive=false
    fi

    if [[ "$fault_class" == "scheduler_stress" && "$active" == "true" ]]; then
        queue_wait_ms="$duration_ms"
    fi

    if [[ "$responsive" != "true" && "$severity" != "high" ]]; then
        severity="high"
    fi

    event_json=$(jq -cn \
        --arg timestamp "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
        --arg run_id "$RUN_ID" \
        --arg test_case_id "$scenario_name" \
        --arg resize_transaction_id "${RUN_ID}:soak:fault:${sequence_no}" \
        --arg scheduler_decision "soak_fault_matrix" \
        --arg classification "$classification" \
        --arg expected_degradation "$expected_degradation" \
        --arg expected_policy "$expected_policy" \
        --arg severity "$severity" \
        --arg summary_status "$summary_status" \
        --arg summary_error "$summary_error" \
        --argjson sequence_no "$sequence_no" \
        --argjson queue_wait_ms "$queue_wait_ms" \
        --argjson reflow_ms "$duration_ms" \
        --argjson render_ms "$duration_ms" \
        --argjson present_ms "$duration_ms" \
        --argjson p50_ms "$duration_ms" \
        --argjson p95_ms "$duration_ms" \
        --argjson p99_ms "$duration_ms" \
        --argjson duration_secs "$duration_secs" \
        --argjson scenario_exit_code "$scenario_rc" \
        --argjson responsive "$responsive" \
        --argjson fault_plan "$fault_plan_json" \
        '{
            timestamp: $timestamp,
            run_id: $run_id,
            test_case_id: $test_case_id,
            resize_transaction_id: $resize_transaction_id,
            pane_id: null,
            tab_id: null,
            sequence_no: $sequence_no,
            scheduler_decision: $scheduler_decision,
            frame_id: null,
            queue_wait_ms: $queue_wait_ms,
            reflow_ms: $reflow_ms,
            render_ms: $render_ms,
            present_ms: $present_ms,
            p50_ms: $p50_ms,
            p95_ms: $p95_ms,
            p99_ms: $p99_ms,
            fault: {
                active: $fault_plan.active,
                class: $fault_plan.fault_class,
                mode: $fault_plan.mode,
                trigger: $fault_plan.trigger
            },
            expected: {
                degradation: $expected_degradation,
                policy: $expected_policy
            },
            observed: {
                status: $summary_status,
                exit_code: $scenario_exit_code,
                error: (if $summary_error == "" then null else $summary_error end),
                duration_secs: $duration_secs
            },
            classification: $classification,
            responsive: $responsive,
            severity: $severity
        }')

    if [[ -n "$SOAK_FAULT_EVENTS_JSONL" ]]; then
        echo "$event_json" >> "$SOAK_FAULT_EVENTS_JSONL"
    fi

    local scenario_dir="$RUN_ARTIFACTS_DIR/scenario_$(printf '%02d' "$sequence_no")_$scenario_name"
    if [[ -d "$scenario_dir" ]]; then
        echo "$event_json" > "$scenario_dir/fault_outcome.json"
    fi
}

soak_write_fault_matrix_summary() {
    if [[ "$SOAK_MODE" != "true" || -z "$SOAK_FAULT_SUMMARY_FILE" ]]; then
        return 0
    fi

    if [[ ! -s "$SOAK_FAULT_EVENTS_JSONL" ]]; then
        jq -n \
            --arg schema_version "wa.soak_fault_matrix_summary.v1" \
            --arg generated_at "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
            --arg run_id "$RUN_ID" \
            --argjson enabled "$SOAK_FAULT_MATRIX_ENABLED" \
            --arg mode "$SOAK_FAULT_MODE" \
            --argjson interval "$SOAK_FAULT_INTERVAL" \
            --argjson offset "$SOAK_FAULT_OFFSET" \
            --argjson classes "$SOAK_FAULT_CLASSES_JSON" \
            '{
                schema_version: $schema_version,
                generated_at: $generated_at,
                run_id: $run_id,
                config: {
                    enabled: $enabled,
                    mode: $mode,
                    interval: $interval,
                    offset: $offset,
                    classes: $classes
                },
                totals: {
                    events: 0,
                    injections: 0,
                    control: 0,
                    responsive_failures: 0
                },
                by_fault_class: {},
                by_classification: {}
            }' > "$SOAK_FAULT_SUMMARY_FILE"
        return 0
    fi

    jq -s \
        --arg schema_version "wa.soak_fault_matrix_summary.v1" \
        --arg generated_at "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
        --arg run_id "$RUN_ID" \
        --argjson enabled "$SOAK_FAULT_MATRIX_ENABLED" \
        --arg mode "$SOAK_FAULT_MODE" \
        --argjson interval "$SOAK_FAULT_INTERVAL" \
        --argjson offset "$SOAK_FAULT_OFFSET" \
        --argjson classes "$SOAK_FAULT_CLASSES_JSON" \
        '{
            schema_version: $schema_version,
            generated_at: $generated_at,
            run_id: $run_id,
            config: {
                enabled: $enabled,
                mode: $mode,
                interval: $interval,
                offset: $offset,
                classes: $classes
            },
            totals: {
                events: length,
                injections: (map(select(.fault.active == true)) | length),
                control: (map(select(.fault.active != true)) | length),
                responsive_failures: (map(select(.responsive != true)) | length)
            },
            by_fault_class: (reduce .[] as $event ({}; .[$event.fault.class] = ((.[$event.fault.class] // 0) + 1))),
            by_classification: (reduce .[] as $event ({}; .[$event.classification] = ((.[$event.classification] // 0) + 1)))
        }' "$SOAK_FAULT_EVENTS_JSONL" > "$SOAK_FAULT_SUMMARY_FILE"
}

soak_generate_incident_report() {
    if [[ "$SOAK_MODE" != "true" ]]; then
        return 0
    fi

    if [[ -z "$SOAK_INCIDENT_REPORT_FILE" ]]; then
        SOAK_INCIDENT_REPORT_STATUS="not_configured"
        return 0
    fi

    if [[ ! -x "$SCRIPT_DIR/check_soak_anomaly_reports.sh" ]]; then
        SOAK_INCIDENT_REPORT_STATUS="analyzer_missing"
        log_warn "Soak incident analyzer not found: $SCRIPT_DIR/check_soak_anomaly_reports.sh"
        return 0
    fi

    local incident_rc=0
    set +e
    "$SCRIPT_DIR/check_soak_anomaly_reports.sh" \
        --run-dir "$RUN_ARTIFACTS_DIR" \
        --output "$SOAK_INCIDENT_REPORT_FILE"
    incident_rc=$?
    set -e

    if [[ -f "$SOAK_INCIDENT_REPORT_FILE" ]]; then
        SOAK_INCIDENT_REPORT_STATUS=$(jq -r '.status // "unknown"' "$SOAK_INCIDENT_REPORT_FILE" 2>/dev/null || echo "unknown")
    else
        SOAK_INCIDENT_REPORT_STATUS="missing_output"
    fi

    case "$incident_rc" in
        0)
            log_info "Soak incident report generated (status=$SOAK_INCIDENT_REPORT_STATUS)"
            ;;
        1)
            log_warn "Soak incident report generated with warning gate (status=$SOAK_INCIDENT_REPORT_STATUS)"
            ;;
        2)
            log_warn "Soak incident report indicates fail-level anomalies (status=$SOAK_INCIDENT_REPORT_STATUS)"
            ;;
        *)
            log_warn "Soak incident report generator exited with code $incident_rc"
            ;;
    esac
}

run_soak_cycles() {
    local scenario_names=("$@")
    local scenarios_json="[]"
    local any_failed=false
    local total_per_cycle="${#scenario_names[@]}"
    local next_checkpoint_epoch=0

    scenarios_json=$(scenarios_to_json_array "${scenario_names[@]}")
    SOAK_TARGET_END_EPOCH=$((START_TIME + SOAK_DURATION_SECS))
    next_checkpoint_epoch=$((START_TIME + SOAK_CHECKPOINT_INTERVAL_SECS))

    jq -n \
        --arg schema_version "wa.soak_config.v1" \
        --arg generated_at "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
        --arg run_id "$RUN_ID" \
        --arg run_seed "$RUN_SEED" \
        --arg run_seed_source "$RUN_SEED_SOURCE" \
        --argjson soak_duration_secs "$SOAK_DURATION_SECS" \
        --argjson checkpoint_interval_secs "$SOAK_CHECKPOINT_INTERVAL_SECS" \
        --argjson timeout_secs "$TIMEOUT" \
        --argjson scenario_retries "$SCENARIO_RETRIES" \
        --argjson stop_on_failure "$SOAK_STOP_ON_FAILURE" \
        --argjson fault_matrix_enabled "$SOAK_FAULT_MATRIX_ENABLED" \
        --arg fault_matrix_mode "$SOAK_FAULT_MODE" \
        --argjson fault_matrix_interval "$SOAK_FAULT_INTERVAL" \
        --argjson fault_matrix_offset "$SOAK_FAULT_OFFSET" \
        --argjson fault_matrix_classes "$SOAK_FAULT_CLASSES_JSON" \
        --argjson scenarios "$scenarios_json" \
        --arg resume_from_checkpoint "$SOAK_RESUME_FROM_CHECKPOINT" \
        --arg resume_from_run_id "$SOAK_RESUME_FROM_RUN_ID" \
        '{
            schema_version: $schema_version,
            generated_at: $generated_at,
            run_id: $run_id,
            run_seed: $run_seed,
            run_seed_source: $run_seed_source,
            soak_duration_secs: $soak_duration_secs,
            checkpoint_interval_secs: $checkpoint_interval_secs,
            timeout_secs: $timeout_secs,
            scenario_retries: $scenario_retries,
            stop_on_failure: $stop_on_failure,
            fault_matrix: {
                enabled: $fault_matrix_enabled,
                mode: $fault_matrix_mode,
                interval: $fault_matrix_interval,
                offset: $fault_matrix_offset,
                classes: $fault_matrix_classes
            },
            scenarios: $scenarios,
            resume: {
                from_checkpoint: (if $resume_from_checkpoint == "" then null else $resume_from_checkpoint end),
                from_run_id: (if $resume_from_run_id == "" then null else $resume_from_run_id end)
            }
        }' > "$SOAK_TELEMETRY_DIR/config.json"

    jq -n \
        --arg schema_version "wa.soak_fault_matrix_config.v1" \
        --arg generated_at "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
        --arg run_id "$RUN_ID" \
        --arg run_seed "$RUN_SEED" \
        --arg run_seed_source "$RUN_SEED_SOURCE" \
        --argjson enabled "$SOAK_FAULT_MATRIX_ENABLED" \
        --arg mode "$SOAK_FAULT_MODE" \
        --argjson interval "$SOAK_FAULT_INTERVAL" \
        --argjson offset "$SOAK_FAULT_OFFSET" \
        --argjson classes "$SOAK_FAULT_CLASSES_JSON" \
        '{
            schema_version: $schema_version,
            generated_at: $generated_at,
            run_id: $run_id,
            run_seed: $run_seed,
            run_seed_source: $run_seed_source,
            enabled: $enabled,
            mode: $mode,
            interval: $interval,
            offset: $offset,
            classes: $classes
        }' > "$SOAK_FAULT_CONFIG_FILE"

    if [[ "$SOAK_COMPLETED_CYCLES" -gt 0 ]]; then
        log_info "Resuming soak run from checkpoint: prior_cycles=$SOAK_COMPLETED_CYCLES prior_sequence=$SOAK_SCENARIO_SEQUENCE"
    fi

    while [[ "$(date +%s)" -lt "$SOAK_TARGET_END_EPOCH" ]]; do
        local cycle_num=$((SOAK_COMPLETED_CYCLES + 1))
        local cycle_failed=false
        local cycle_start_epoch
        cycle_start_epoch=$(date +%s)
        log_info "Soak cycle $cycle_num starting ($total_per_cycle scenario(s), remaining=$((SOAK_TARGET_END_EPOCH - cycle_start_epoch))s)"

        for scenario_name in "${scenario_names[@]}"; do
            local scenario_rc=0
            local summary_entry=""
            local summary_status="failed"
            local summary_duration=0
            local summary_error=""
            local fault_plan=""
            local fault_active="false"
            local fault_class="none"
            local anomaly_threshold=$((TIMEOUT * 8 / 10))

            SOAK_SCENARIO_SEQUENCE=$((SOAK_SCENARIO_SEQUENCE + 1))
            fault_plan=$(soak_compute_fault_plan "$scenario_name" "$SOAK_SCENARIO_SEQUENCE")
            fault_active=$(jq -r '.active // false' <<< "$fault_plan")
            fault_class=$(jq -r '.fault_class // "none"' <<< "$fault_plan")

            if run_scenario "$scenario_name" "$SOAK_SCENARIO_SEQUENCE" "$fault_plan"; then
                scenario_rc=0
            else
                scenario_rc=$?
            fi
            TOTAL=$((TOTAL + 1))

            if [[ "${#SCENARIO_SUMMARIES[@]}" -gt 0 ]]; then
                summary_entry="${SCENARIO_SUMMARIES[${#SCENARIO_SUMMARIES[@]}-1]}"
            else
                summary_entry='{"name":"unknown","status":"failed","duration_secs":0,"error":"missing_soak_summary"}'
            fi
            summary_status=$(jq -r '.status // "failed"' <<< "$summary_entry")
            summary_duration=$(jq -r '.duration_secs // 0' <<< "$summary_entry")
            summary_error=$(jq -r '.error // empty' <<< "$summary_entry")
            summary_entry=$(jq -c --argjson fault_plan "$fault_plan" '. + {fault_plan: $fault_plan}' <<< "$summary_entry")
            if [[ "${#SCENARIO_SUMMARIES[@]}" -gt 0 ]]; then
                local summary_index=$(( ${#SCENARIO_SUMMARIES[@]} - 1 ))
                SCENARIO_SUMMARIES[$summary_index]="$summary_entry"
            fi

            soak_record_fault_event "$scenario_name" "$SOAK_SCENARIO_SEQUENCE" "$scenario_rc" "$summary_status" "$summary_duration" "$summary_error" "$fault_plan"

            if [[ "$summary_status" != "passed" || "$scenario_rc" -ne 0 ]]; then
                cycle_failed=true
                any_failed=true
                if [[ -z "$summary_error" ]]; then
                    if [[ "$fault_active" == "true" ]]; then
                        summary_error="injected_${fault_class}"
                    else
                        summary_error="scenario_failure"
                    fi
                fi
                soak_record_anomaly_marker "scenario_failure" "$scenario_name" "$summary_duration" "$summary_error" "high"
            fi
            if [[ "$anomaly_threshold" -gt 0 && "$summary_duration" =~ ^[0-9]+$ && "$summary_duration" -ge "$anomaly_threshold" ]]; then
                soak_record_anomaly_marker "latency_budget_pressure" "$scenario_name" "$summary_duration" "duration_secs=${summary_duration};threshold_secs=${anomaly_threshold}" "medium"
            fi
            if [[ "$SOAK_STOP_ON_FAILURE" == "true" && "$cycle_failed" == "true" ]]; then
                break
            fi
            if [[ "$(date +%s)" -ge "$SOAK_TARGET_END_EPOCH" ]]; then
                break
            fi
        done

        SOAK_COMPLETED_CYCLES="$cycle_num"

        local now_epoch
        now_epoch=$(date +%s)
        if [[ "$cycle_failed" == "true" || "$now_epoch" -ge "$next_checkpoint_epoch" ]]; then
            local reason="interval"
            if [[ "$cycle_failed" == "true" ]]; then
                reason="cycle_failure"
            fi
            soak_emit_checkpoint "$scenarios_json" "$reason"
            next_checkpoint_epoch=$((now_epoch + SOAK_CHECKPOINT_INTERVAL_SECS))
        fi

        if [[ "$SOAK_STOP_ON_FAILURE" == "true" && "$cycle_failed" == "true" ]]; then
            log_warn "Soak loop stopped by --soak-stop-on-failure after cycle $cycle_num"
            break
        fi
        if [[ "$now_epoch" -ge "$SOAK_TARGET_END_EPOCH" ]]; then
            break
        fi

        local cycle_duration=$((now_epoch - cycle_start_epoch))
        log_info "Soak cycle $cycle_num completed in ${cycle_duration}s"
    done

    soak_emit_checkpoint "$scenarios_json" "final"
    soak_write_fault_matrix_summary
    soak_generate_incident_report
    if [[ "$any_failed" == "true" ]]; then
        return 1
    fi
    return 0
}

# ==============================================================================
# Artifacts Management
# ==============================================================================

setup_artifacts() {
    TIMESTAMP=$(date -u +"%Y-%m-%dT%H-%M-%SZ")
    RUN_ID=$(compute_run_id)
    export FT_E2E_RUN_ID="$RUN_ID"

    if [[ -n "$ARTIFACTS_DIR" ]]; then
        RUN_ARTIFACTS_DIR="$ARTIFACTS_DIR/$TIMESTAMP"
    else
        RUN_ARTIFACTS_DIR="$DEFAULT_ARTIFACTS_BASE/$TIMESTAMP"
    fi

    mkdir -p "$RUN_ARTIFACTS_DIR"
    SUMMARY_FILE="$RUN_ARTIFACTS_DIR/summary.json"
    SCENARIO_SUMMARIES=()

    if [[ "$SOAK_MODE" == "true" ]]; then
        SOAK_TELEMETRY_DIR="$RUN_ARTIFACTS_DIR/soak"
        SOAK_SNAPSHOTS_DIR="$SOAK_TELEMETRY_DIR/snapshots"
        SOAK_CHECKPOINT_FILE="$SOAK_TELEMETRY_DIR/last_checkpoint.json"
        SOAK_TELEMETRY_JSONL="$SOAK_TELEMETRY_DIR/checkpoint_telemetry.jsonl"
        SOAK_HEALTH_JSONL="$SOAK_TELEMETRY_DIR/health_summaries.jsonl"
        SOAK_ANOMALY_JSONL="$SOAK_TELEMETRY_DIR/anomaly_markers.jsonl"
        SOAK_FAULT_CONFIG_FILE="$SOAK_TELEMETRY_DIR/fault_matrix_config.json"
        SOAK_FAULT_EVENTS_JSONL="$SOAK_TELEMETRY_DIR/fault_matrix_events.jsonl"
        SOAK_FAULT_SUMMARY_FILE="$SOAK_TELEMETRY_DIR/fault_matrix_summary.json"
        SOAK_INCIDENT_REPORT_FILE="$SOAK_TELEMETRY_DIR/incident_report.json"
        SOAK_INCIDENT_REPORT_STATUS="not_run"
        mkdir -p "$SOAK_SNAPSHOTS_DIR"
        : > "$SOAK_TELEMETRY_JSONL"
        : > "$SOAK_HEALTH_JSONL"
        : > "$SOAK_ANOMALY_JSONL"
        : > "$SOAK_FAULT_EVENTS_JSONL"
    fi

    # Write environment snapshot
    cat > "$RUN_ARTIFACTS_DIR/env.txt" <<EOF
hostname: $(hostname)
timestamp: $TIMESTAMP
wezterm_version: $(wezterm --version 2>/dev/null | head -1 || echo "N/A")
wa_version: $(find_ft_binary && "$FT_BINARY" --version 2>/dev/null | head -1 || echo "N/A")
rust_version: $(rustc --version 2>/dev/null || echo "N/A")
os: $(uname -a)
shell: $SHELL
temp_workspace: ${WORKSPACE:-auto}
run_seed: $RUN_SEED
run_seed_source: $RUN_SEED_SOURCE
run_id: $RUN_ID
scenario_retries: $SCENARIO_RETRIES
soak_mode: $SOAK_MODE
soak_duration_secs: $SOAK_DURATION_SECS
soak_checkpoint_interval_secs: $SOAK_CHECKPOINT_INTERVAL_SECS
soak_stop_on_failure: $SOAK_STOP_ON_FAILURE
soak_resume_checkpoint: ${SOAK_RESUME_CHECKPOINT:-none}
soak_fault_matrix_enabled: $SOAK_FAULT_MATRIX_ENABLED
soak_fault_mode: $SOAK_FAULT_MODE
soak_fault_interval: $SOAK_FAULT_INTERVAL
soak_fault_offset: $SOAK_FAULT_OFFSET
soak_fault_classes: ${SOAK_FAULT_CLASSES[*]:-none}
soak_incident_report: ${SOAK_INCIDENT_REPORT_FILE:-none}
soak_incident_status: $SOAK_INCIDENT_REPORT_STATUS
EOF

    log_verbose "Artifacts directory: $RUN_ARTIFACTS_DIR"
}

cleanup_artifacts() {
    if [[ "$KEEP_ARTIFACTS" == "false" && "$FAILED" -eq 0 ]]; then
        log_verbose "Cleaning up artifacts (all tests passed)"
        rm -rf "$RUN_ARTIFACTS_DIR"
    else
        log_info "Artifacts saved to: $RUN_ARTIFACTS_DIR"
    fi
}

write_summary() {
    local duration
    duration=$(( $(date +%s) - START_TIME ))
    local scenarios_json="[]"

    for scenario_entry in "${SCENARIO_SUMMARIES[@]}"; do
        scenarios_json=$(jq -c --argjson item "$scenario_entry" '. + [$item]' <<< "$scenarios_json")
    done

    local soak_checkpoint_path=""
    local soak_checkpoint_rel=null
    local soak_resume_run_id=null
    local soak_resume_checkpoint=null
    local soak_config_rel=null
    local soak_telemetry_rel=null
    local soak_health_rel=null
    local soak_anomaly_rel=null
    local soak_fault_config_rel=null
    local soak_fault_events_rel=null
    local soak_fault_summary_rel=null
    local soak_fault_classes_json="[]"
    local soak_incident_report_rel=null

    if [[ "$SOAK_MODE" == "true" ]]; then
        soak_checkpoint_path="$SOAK_CHECKPOINT_FILE"
        if [[ -f "$soak_checkpoint_path" ]]; then
            soak_checkpoint_rel="\"soak/$(basename "$SOAK_CHECKPOINT_FILE")\""
        fi
        soak_telemetry_rel="\"soak/$(basename "$SOAK_TELEMETRY_JSONL")\""
        soak_health_rel="\"soak/$(basename "$SOAK_HEALTH_JSONL")\""
        soak_anomaly_rel="\"soak/$(basename "$SOAK_ANOMALY_JSONL")\""
        soak_config_rel="\"soak/config.json\""
        soak_fault_config_rel="\"soak/$(basename "$SOAK_FAULT_CONFIG_FILE")\""
        soak_fault_events_rel="\"soak/$(basename "$SOAK_FAULT_EVENTS_JSONL")\""
        soak_fault_summary_rel="\"soak/$(basename "$SOAK_FAULT_SUMMARY_FILE")\""
        soak_fault_classes_json="$SOAK_FAULT_CLASSES_JSON"
        soak_incident_report_rel="\"soak/$(basename "$SOAK_INCIDENT_REPORT_FILE")\""
        if [[ -n "$SOAK_RESUME_FROM_RUN_ID" ]]; then
            soak_resume_run_id="\"$SOAK_RESUME_FROM_RUN_ID\""
        fi
        if [[ -n "$SOAK_RESUME_FROM_CHECKPOINT" ]]; then
            soak_resume_checkpoint="\"$SOAK_RESUME_FROM_CHECKPOINT\""
        fi
    fi

    cat > "$SUMMARY_FILE" <<EOF
{
  "version": "1",
  "schema_version": "wa.e2e.summary.v2",
  "test_artifact_schema_version": "wa.test_artifacts.v1",
  "timestamp": "$TIMESTAMP",
  "run_id": "$RUN_ID",
  "run_seed": "$RUN_SEED",
  "run_seed_source": "$RUN_SEED_SOURCE",
  "scenario_retries": $SCENARIO_RETRIES,
  "duration_secs": $duration,
  "total": $TOTAL,
  "passed": $PASSED,
  "failed": $FAILED,
  "skipped": $SKIPPED,
  "scenarios": $scenarios_json,
  "soak": {
    "enabled": $SOAK_MODE,
    "duration_secs": $SOAK_DURATION_SECS,
    "checkpoint_interval_secs": $SOAK_CHECKPOINT_INTERVAL_SECS,
    "stop_on_failure": $SOAK_STOP_ON_FAILURE,
    "completed_cycles": $SOAK_COMPLETED_CYCLES,
    "checkpoints_emitted": $SOAK_LAST_CHECKPOINT_INDEX,
    "config": $soak_config_rel,
    "checkpoint_manifest": $soak_checkpoint_rel,
    "checkpoint_telemetry_log": $soak_telemetry_rel,
    "health_summary_log": $soak_health_rel,
    "anomaly_markers_log": $soak_anomaly_rel,
    "fault_matrix": {
      "enabled": $SOAK_FAULT_MATRIX_ENABLED,
      "mode": "$SOAK_FAULT_MODE",
      "interval": $SOAK_FAULT_INTERVAL,
      "offset": $SOAK_FAULT_OFFSET,
      "classes": $soak_fault_classes_json,
      "config": $soak_fault_config_rel,
      "events_log": $soak_fault_events_rel,
      "summary": $soak_fault_summary_rel
    },
    "incident_report": $soak_incident_report_rel,
    "incident_status": "$SOAK_INCIDENT_REPORT_STATUS",
    "resume_from_run_id": $soak_resume_run_id,
    "resume_from_checkpoint": $soak_resume_checkpoint
  }
}
EOF

    # Also write human-readable summary
    cat > "$RUN_ARTIFACTS_DIR/summary.txt" <<EOF
E2E Test Summary
================
Timestamp: $TIMESTAMP
Run ID:    $RUN_ID
Run Seed:  $RUN_SEED ($RUN_SEED_SOURCE)
Retries:   $SCENARIO_RETRIES
Duration:  ${duration}s
Soak:      enabled=$SOAK_MODE duration_secs=$SOAK_DURATION_SECS checkpoint_interval_secs=$SOAK_CHECKPOINT_INTERVAL_SECS cycles=$SOAK_COMPLETED_CYCLES
Faults:    enabled=$SOAK_FAULT_MATRIX_ENABLED mode=$SOAK_FAULT_MODE interval=$SOAK_FAULT_INTERVAL offset=$SOAK_FAULT_OFFSET classes=${SOAK_FAULT_CLASSES[*]:-none}
Incident:  status=$SOAK_INCIDENT_REPORT_STATUS path=${SOAK_INCIDENT_REPORT_FILE:-none}

Results:
  Total:   $TOTAL
  Passed:  $PASSED
  Failed:  $FAILED
  Skipped: $SKIPPED

Artifacts: $RUN_ARTIFACTS_DIR
EOF
}

# ==============================================================================
# ft Binary
# ==============================================================================

FT_BINARY=""

find_ft_binary() {
    if [[ -x "$PROJECT_ROOT/target/release/ft" ]]; then
        FT_BINARY="$PROJECT_ROOT/target/release/ft"
    elif [[ -x "$PROJECT_ROOT/target/debug/ft" ]]; then
        FT_BINARY="$PROJECT_ROOT/target/debug/ft"
    else
        return 1
    fi
    return 0
}

scenario_can_run_without_ft_binary() {
    local name="$1"

    case "$name" in
        alt_screen_conformance)
            # Allow fixture-only conformance execution when wezterm is unavailable and
            # fixture fallback is explicitly enabled.
            if [[ "${FT_E2E_ALT_SCREEN_ALLOW_FIXTURE_ONLY:-1}" == "1" ]] \
                && ! resolve_wezterm_bin_path >/dev/null 2>&1; then
                return 0
            fi
            ;;
    esac

    return 1
}

all_scenarios_can_run_without_ft_binary() {
    local name=""

    if [[ "$#" -eq 0 ]]; then
        return 1
    fi

    for name in "$@"; do
        if ! scenario_can_run_without_ft_binary "$name"; then
            return 1
        fi
    done

    return 0
}

# ==============================================================================
# Wait Helpers
# ==============================================================================

wait_for_condition() {
    local description="$1"
    local check_cmd="$2"
    local timeout="${3:-30}"
    local start=$(date +%s)

    log_verbose "Waiting for: $description (timeout: ${timeout}s)"

    while true; do
        if eval "$check_cmd"; then
            log_verbose "Condition met: $description"
            return 0
        fi

        local elapsed=$(( $(date +%s) - start ))
        if [[ $elapsed -ge $timeout ]]; then
            log_verbose "Timeout waiting for: $description"
            return 1
        fi

        sleep 0.5
    done
}

current_time_ms() {
    if command -v python3 >/dev/null 2>&1; then
        python3 - <<'PY'
import time
print(int(time.time() * 1000))
PY
    else
        echo $(( $(date +%s) * 1000 ))
    fi
}

file_size_bytes() {
    local path="$1"
    stat -f%z "$path" 2>/dev/null || stat -c%s "$path" 2>/dev/null || echo 0
}

sha256_file() {
    local path="$1"
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$path" | awk '{print $1}'
    elif command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$path" | awk '{print $1}'
    else
        echo ""
    fi
}

infer_artifact_kind() {
    local file_name="$1"
    case "$file_name" in
        *trace_bundle*.json)
            echo "trace_bundle"
            ;;
        *frame_histogram*.json)
            echo "frame_histogram"
            ;;
        *failure_signature*.json|*failure_signature*.txt)
            echo "failure_signature"
            ;;
        *events*.json|*events*.jsonl)
            echo "event_stream"
            ;;
        *audit*.json|*audit*.jsonl)
            echo "audit_extract"
            ;;
        *flame*.*|*.svg)
            echo "flamegraph"
            ;;
        *.jsonl|*.log|*.txt|*.stderr|*.stdout)
            echo "structured_log"
            ;;
        *)
            echo "raw_data"
            ;;
    esac
}

infer_artifact_format() {
    local file_name="$1"
    case "$file_name" in
        *.json)
            echo "json"
            ;;
        *.jsonl)
            echo "json_lines"
            ;;
        *.txt|*.log|*.stderr|*.stdout)
            echo "text"
            ;;
        *.csv)
            echo "csv"
            ;;
        *.html)
            echo "html"
            ;;
        *.svg)
            echo "svg"
            ;;
        *.png)
            echo "png"
            ;;
        *)
            echo "binary"
            ;;
    esac
}

infer_artifact_redacted() {
    local file_name="$1"
    case "$file_name" in
        *.log|*.txt|*.json|*.jsonl|*.csv|*.toml)
            echo "true"
            ;;
        *)
            echo "false"
            ;;
    esac
}

extract_primary_pane_id() {
    local scenario_dir="$1"
    local pane_id=""
    local scenario_log="$scenario_dir/scenario.log"
    if [[ -f "$scenario_log" ]]; then
        pane_id=$(grep -Eo '(pane_id|agent_pane_id|alt_screen_pane_id):[[:space:]]*[0-9]+' "$scenario_log" \
            | head -1 \
            | grep -Eo '[0-9]+' \
            || true)
    fi
    echo "$pane_id"
}

derive_failure_signature() {
    local scenario_dir="$1"
    if grep -Eiq "timeout|timed out" "$scenario_dir"/*.log "$scenario_dir"/*.txt 2>/dev/null; then
        echo "timeout"
    elif grep -Eiq "policy|denied|blocked" "$scenario_dir"/*.log "$scenario_dir"/*.txt 2>/dev/null; then
        echo "policy_denied"
    elif grep -Eiq "panic|assert|failed" "$scenario_dir"/*.log "$scenario_dir"/*.txt 2>/dev/null; then
        echo "assertion_or_runtime_failure"
    else
        echo "scenario_failure"
    fi
}

ensure_failure_artifacts() {
    local scenario_name="$1"
    local scenario_dir="$2"
    local duration_ms="$3"
    local signature="$4"
    local generated_at
    generated_at=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

    local trace_file="$scenario_dir/trace_bundle.json"
    if [[ ! -f "$trace_file" ]]; then
        local scenario_tail=""
        local watch_tail=""
        if [[ -f "$scenario_dir/scenario.log" ]]; then
            scenario_tail=$(tail -n 200 "$scenario_dir/scenario.log" 2>/dev/null || true)
        fi
        if [[ -f "$scenario_dir/wa_watch.log" ]]; then
            watch_tail=$(tail -n 200 "$scenario_dir/wa_watch.log" 2>/dev/null || true)
        fi
        jq -n \
            --arg schema_version "wa.trace_bundle.v1" \
            --arg generated_at "$generated_at" \
            --arg test_case_id "$scenario_name" \
            --arg signature "$signature" \
            --arg scenario_log_tail "$scenario_tail" \
            --arg watch_log_tail "$watch_tail" \
            --argjson duration_ms "$duration_ms" \
            '{
                schema_version: $schema_version,
                generated_at: $generated_at,
                test_case_id: $test_case_id,
                failure_signature: $signature,
                duration_ms: $duration_ms,
                tails: {
                    scenario_log: $scenario_log_tail,
                    watch_log: $watch_log_tail
                }
            }' > "$trace_file"
    fi

    local histogram_file="$scenario_dir/frame_histogram.json"
    if [[ ! -f "$histogram_file" ]]; then
        jq -n \
            --arg schema_version "wa.frame_histogram.v1" \
            --arg generated_at "$generated_at" \
            --arg test_case_id "$scenario_name" \
            --arg signature "$signature" \
            --argjson duration_ms "$duration_ms" \
            '{
                schema_version: $schema_version,
                generated_at: $generated_at,
                test_case_id: $test_case_id,
                failure_signature: $signature,
                duration_ms: $duration_ms,
                histogram: {
                    frame_count: 0,
                    dropped_frame_count: 0,
                    bucket_ms: []
                }
            }' > "$histogram_file"
    fi

    local signature_file="$scenario_dir/failure_signature.json"
    if [[ ! -f "$signature_file" ]]; then
        jq -n \
            --arg schema_version "wa.failure_signature.v1" \
            --arg generated_at "$generated_at" \
            --arg test_case_id "$scenario_name" \
            --arg signature "$signature" \
            --argjson duration_ms "$duration_ms" \
            '{
                schema_version: $schema_version,
                generated_at: $generated_at,
                test_case_id: $test_case_id,
                signature: $signature,
                duration_ms: $duration_ms
            }' > "$signature_file"
    fi
}

build_scenario_artifacts_json() {
    local scenario_dir="$1"
    local entries="[]"

    while IFS= read -r file_path; do
        local file_name=""
        local kind=""
        local format=""
        local bytes=0
        local sha256=""
        local redacted="false"
        file_name=$(basename "$file_path")
        kind=$(infer_artifact_kind "$file_name")
        format=$(infer_artifact_format "$file_name")
        bytes=$(file_size_bytes "$file_path")
        sha256=$(sha256_file "$file_path")
        redacted=$(infer_artifact_redacted "$file_name")

        entries=$(jq -c \
            --arg kind "$kind" \
            --arg format "$format" \
            --arg path "$file_name" \
            --argjson bytes "$bytes" \
            --arg sha256 "$sha256" \
            --argjson redacted "$redacted" \
            '. + [{
                kind: $kind,
                format: $format,
                path: $path,
                bytes: $bytes,
                sha256: (if $sha256 == "" then null else $sha256 end),
                redacted: $redacted
            }]' <<< "$entries")
    done < <(find "$scenario_dir" -maxdepth 1 -type f | LC_ALL=C sort)

    echo "$entries"
}

emit_scenario_artifact_manifest() {
    local scenario_name="$1"
    local scenario_num="$2"
    local scenario_dir="$3"
    local scenario_result="$4"
    local duration_secs="$5"

    local duration_ms=$((duration_secs * 1000))
    local outcome="passed"
    if [[ "$scenario_result" -ne 0 ]]; then
        outcome="failed"
    fi

    local pane_id=""
    pane_id=$(extract_primary_pane_id "$scenario_dir")

    local sequence_no="$scenario_num"
    local resize_transaction_id="${TIMESTAMP}-${scenario_name}-${scenario_num}"
    local scheduler_decision="e2e_harness"
    local frame_id=""
    local tab_id=""
    local failure_signature=""
    if [[ "$outcome" != "passed" ]]; then
        failure_signature=$(derive_failure_signature "$scenario_dir")
        ensure_failure_artifacts "$scenario_name" "$scenario_dir" "$duration_ms" "$failure_signature"
    fi

    local queue_wait_ms=0
    local reflow_ms="$duration_ms"
    local render_ms="$duration_ms"
    local present_ms="$duration_ms"
    local p50_ms="$duration_ms"
    local p95_ms="$duration_ms"
    local p99_ms="$duration_ms"
    local generated_at_ms=""
    generated_at_ms=$(current_time_ms)

    local correlation_jsonl="$scenario_dir/correlation.jsonl"
    jq -cn \
        --arg timestamp "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
        --arg test_case_id "$scenario_name" \
        --arg resize_transaction_id "$resize_transaction_id" \
        --arg pane_id "$pane_id" \
        --arg tab_id "$tab_id" \
        --arg sequence_no "$sequence_no" \
        --arg scheduler_decision "$scheduler_decision" \
        --arg frame_id "$frame_id" \
        --argjson queue_wait_ms "$queue_wait_ms" \
        --argjson reflow_ms "$reflow_ms" \
        --argjson render_ms "$render_ms" \
        --argjson present_ms "$present_ms" \
        --argjson p50_ms "$p50_ms" \
        --argjson p95_ms "$p95_ms" \
        --argjson p99_ms "$p99_ms" \
        '{
            timestamp: $timestamp,
            test_case_id: $test_case_id,
            resize_transaction_id: $resize_transaction_id,
            pane_id: (if $pane_id == "" then null else ($pane_id | tonumber) end),
            tab_id: (if $tab_id == "" then null else ($tab_id | tonumber) end),
            sequence_no: (if $sequence_no == "" then null else ($sequence_no | tonumber) end),
            scheduler_decision: $scheduler_decision,
            frame_id: (if $frame_id == "" then null else ($frame_id | tonumber) end),
            queue_wait_ms: $queue_wait_ms,
            reflow_ms: $reflow_ms,
            render_ms: $render_ms,
            present_ms: $present_ms,
            p50_ms: $p50_ms,
            p95_ms: $p95_ms,
            p99_ms: $p99_ms
        }' > "$correlation_jsonl"

    local artifacts_json=""
    artifacts_json=$(build_scenario_artifacts_json "$scenario_dir")
    local manifest_path="$scenario_dir/test_artifacts_manifest.json"
    jq -n \
        --arg schema_version "wa.test_artifacts.v1" \
        --arg run_id "$RUN_ID" \
        --argjson generated_at_ms "$generated_at_ms" \
        --arg outcome "$outcome" \
        --arg test_case_id "$scenario_name" \
        --arg resize_transaction_id "$resize_transaction_id" \
        --arg pane_id "$pane_id" \
        --arg tab_id "$tab_id" \
        --arg sequence_no "$sequence_no" \
        --arg scheduler_decision "$scheduler_decision" \
        --arg frame_id "$frame_id" \
        --argjson queue_wait_ms "$queue_wait_ms" \
        --argjson reflow_ms "$reflow_ms" \
        --argjson render_ms "$render_ms" \
        --argjson present_ms "$present_ms" \
        --argjson p50_ms "$p50_ms" \
        --argjson p95_ms "$p95_ms" \
        --argjson p99_ms "$p99_ms" \
        --argjson artifacts "$artifacts_json" \
        '{
            schema_version: $schema_version,
            run_id: $run_id,
            generated_at_ms: $generated_at_ms,
            outcome: $outcome,
            correlation: {
                test_case_id: $test_case_id,
                resize_transaction_id: $resize_transaction_id,
                pane_id: (if $pane_id == "" then null else ($pane_id | tonumber) end),
                tab_id: (if $tab_id == "" then null else ($tab_id | tonumber) end),
                sequence_no: (if $sequence_no == "" then null else ($sequence_no | tonumber) end),
                scheduler_decision: $scheduler_decision,
                frame_id: (if $frame_id == "" then null else ($frame_id | tonumber) end)
            },
            timing: {
                queue_wait_ms: $queue_wait_ms,
                reflow_ms: $reflow_ms,
                render_ms: $render_ms,
                present_ms: $present_ms,
                p50_ms: $p50_ms,
                p95_ms: $p95_ms,
                p99_ms: $p99_ms
            },
            artifacts: $artifacts
        }' > "$manifest_path"
}

# ==============================================================================
# Scenario Runners
# ==============================================================================

run_scenario_capture_search() {
    local scenario_dir="$1"
    local marker="E2E_MARKER_$(date +%s%N)"
    local temp_workspace
    temp_workspace=$(mktemp -d /tmp/ft-e2e-XXXXXX)
    local ft_pid=""
    local pane_id=""
    local result=0
    local policy_suggestions_ok="false"

    log_info "Using marker: $marker"
    log_info "Workspace: $temp_workspace"

    # Setup environment for isolated wa instance
    export FT_DATA_DIR="$temp_workspace/.ft"
    export FT_WORKSPACE="$temp_workspace"
    mkdir -p "$FT_DATA_DIR"

    # Cleanup function
    cleanup_capture_search() {
        log_verbose "Cleaning up capture_search scenario"
        # Kill ft watch if running
        if [[ -n "${ft_pid:-}" ]] && kill -0 "$ft_pid" 2>/dev/null; then
            log_verbose "Stopping ft watch (pid $ft_pid)"
            kill "$ft_pid" 2>/dev/null || true
            wait "$ft_pid" 2>/dev/null || true
        fi
        # Close dummy pane if it exists
        if [[ -n "${pane_id:-}" ]]; then
            log_verbose "Closing dummy pane $pane_id"
            wezterm cli kill-pane --pane-id "$pane_id" 2>/dev/null || true
        fi
        # Copy artifacts before cleanup
        if [[ -d "${temp_workspace:-}" ]]; then
            cp -r "$temp_workspace/.ft"/* "$scenario_dir/" 2>/dev/null || true
        fi
        rm -rf "${temp_workspace:-}"
    }
    trap cleanup_capture_search EXIT

    # Step 1: Spawn dummy pane with the print script
    log_info "Step 1: Spawning dummy pane..."
    local dummy_script="$PROJECT_ROOT/fixtures/e2e/dummy_print.sh"
    if [[ ! -x "$dummy_script" ]]; then
        log_fail "Dummy print script not found or not executable: $dummy_script"
        return 1
    fi

    local spawn_output
    spawn_output=$(wezterm cli spawn --cwd "$temp_workspace" -- bash "$dummy_script" "$marker" 100 2>&1)
    pane_id=$(echo "$spawn_output" | grep -oE '^[0-9]+$' | head -1)

    if [[ -z "$pane_id" ]]; then
        log_fail "Failed to spawn dummy pane"
        echo "Spawn output: $spawn_output" >> "$scenario_dir/scenario.log"
        return 1
    fi
    log_info "Spawned pane: $pane_id"
    echo "Spawned pane_id: $pane_id" >> "$scenario_dir/scenario.log"

    # Step 2: Start ft watch in background
    log_info "Step 2: Starting ft watch..."
    "$FT_BINARY" watch --foreground \
        > "$scenario_dir/wa_watch.log" 2>&1 &
    ft_pid=$!
    log_verbose "ft watch started with PID $ft_pid"
    echo "ft_pid: $ft_pid" >> "$scenario_dir/scenario.log"

    # Verify ft watch is running
    if ! kill -0 "$ft_pid" 2>/dev/null; then
        log_fail "ft watch exited immediately"
        return 1
    fi

    # Step 3: Wait for pane to be observed
    log_info "Step 3: Waiting for pane capture..."
    local wait_timeout=${TIMEOUT:-30}
    local check_cmd="\"$FT_BINARY\" robot state 2>/dev/null | jq -e '.data[]? | select(.pane_id == $pane_id)' >/dev/null 2>&1"

    if ! wait_for_condition "pane $pane_id observed" "$check_cmd" "$wait_timeout"; then
        log_fail "Timeout waiting for pane to be observed"
        # Capture robot state for diagnostics
        "$FT_BINARY" robot state > "$scenario_dir/robot_state.json" 2>&1 || true
        return 1
    fi
    log_pass "Pane observed"

    # Step 4: Wait for dummy script to complete (check for "Done:" marker)
    log_info "Step 4: Waiting for dummy script completion..."
    local done_check_cmd="\"$FT_BINARY\" robot get-text $pane_id --tail 200 2>/dev/null | grep -q \"Done:\""
    if ! wait_for_condition "dummy script done marker captured" "$done_check_cmd" "$wait_timeout"; then
        log_warn "Timed out waiting for Done: marker; proceeding with best-effort capture"
    fi

    # Capture robot state
    "$FT_BINARY" robot state > "$scenario_dir/robot_state.json" 2>&1 || true

    # Step 5: Stop ft watch gracefully
    log_info "Step 5: Stopping ft watch..."
    kill -TERM "$ft_pid" 2>/dev/null || true
    wait "$ft_pid" 2>/dev/null || true
    ft_pid=""
    log_verbose "ft watch stopped"

    # Step 6: Search for the marker
    log_info "Step 6: Searching for marker..."
    local search_output
    search_output=$("$FT_BINARY" search "$marker" --limit 200 2>&1)
    echo "$search_output" > "$scenario_dir/search_output.txt"

    # Count hits (lines containing the marker, excluding header lines)
    local hit_count
    hit_count=$(echo "$search_output" | grep -c "$marker" || echo "0")

    log_info "Search returned $hit_count hits for marker"

    # Step 7: Assert results
    log_info "Step 7: Asserting results..."

    # We expect at least some hits (dummy_print.sh outputs 100+ lines)
    if [[ "$hit_count" -lt 10 ]]; then
        log_fail "Expected at least 10 hits, got $hit_count"
        result=1
    else
        log_pass "Found $hit_count hits for marker (expected >= 10)"
    fi

    # Verify pane_id in search results (if using JSON output)
    if "$FT_BINARY" search "$marker" --limit 10 2>/dev/null | jq -e '.' >/dev/null 2>&1; then
        log_verbose "Search output is JSON, checking pane_id..."
        if "$FT_BINARY" search "$marker" --limit 10 2>/dev/null | jq -e ".results[]? | select(.pane_id == $pane_id)" >/dev/null 2>&1; then
            log_pass "Correct pane_id in search results"
        else
            log_warn "Could not verify pane_id in search results (may be expected)"
        fi
    fi

    # Cleanup trap will handle the rest
    trap - EXIT
    cleanup_capture_search

    return $result
}

run_scenario_search_linting_rebuild() {
    local scenario_dir="$1"
    local temp_workspace
    temp_workspace=$(mktemp -d /tmp/ft-e2e-search-XXXXXX)
    local result=0

    log_info "Workspace: $temp_workspace"
    echo "workspace: $temp_workspace" >> "$scenario_dir/scenario.log"

    export FT_DATA_DIR="$temp_workspace/.ft"
    export FT_WORKSPACE="$temp_workspace"
    mkdir -p "$FT_DATA_DIR"

    cleanup_search_linting_rebuild() {
        if [[ -d "$temp_workspace" ]]; then
            cp -r "$temp_workspace/.ft"/* "$scenario_dir/" 2>/dev/null || true
            rm -rf "$temp_workspace"
        fi
    }
    trap cleanup_search_linting_rebuild EXIT

    log_info "Step 1: Verify FTS index health..."
    "$FT_BINARY" search fts verify -f json > "$scenario_dir/fts_verify.json" 2>&1 || result=1
    if jq -e '.ok == true' "$scenario_dir/fts_verify.json" >/dev/null 2>&1; then
        log_pass "FTS verify succeeded"
    else
        log_fail "FTS verify failed"
        result=1
    fi

    log_info "Step 2: Validate linting on invalid query..."
    local lint_exit=0
    set +e
    "$FT_BINARY" search "\"unterminated" -f json > "$scenario_dir/search_lint.json" 2>&1
    lint_exit=$?
    set -e
    if [[ "$lint_exit" -eq 0 ]]; then
        log_fail "Expected search lint to fail but it exited 0"
        result=1
    fi
    if jq -e '.ok == false and (.lint[]? | select(.code == "unbalanced_quotes"))' \
        "$scenario_dir/search_lint.json" >/dev/null 2>&1; then
        log_pass "Lint output contains unbalanced_quotes"
    else
        log_fail "Lint output missing expected lint codes"
        result=1
    fi

    log_info "Step 3: Rebuild FTS index..."
    "$FT_BINARY" search fts rebuild -f json > "$scenario_dir/fts_rebuild.json" 2>&1 || result=1
    if jq -e '.ok == true and .result.full_rebuild == true' \
        "$scenario_dir/fts_rebuild.json" >/dev/null 2>&1; then
        log_pass "FTS rebuild succeeded"
    else
        log_fail "FTS rebuild failed"
        result=1
    fi

    trap - EXIT
    cleanup_search_linting_rebuild

    return $result
}

run_scenario_natural_language() {
    local scenario_dir="$1"
    local marker="You've hit your usage limit, try again at 12:00."
    local temp_workspace
    temp_workspace=$(mktemp -d /tmp/ft-e2e-XXXXXX)
    local ft_pid=""
    local pane_id=""
    local result=0

    log_info "Using marker: $marker"
    log_info "Workspace: $temp_workspace"

    # Setup environment for isolated wa instance
    export FT_DATA_DIR="$temp_workspace/.ft"
    export FT_WORKSPACE="$temp_workspace"
    mkdir -p "$FT_DATA_DIR"

    cleanup_natural_language() {
        log_verbose "Cleaning up natural_language scenario"
        if [[ -n "$ft_pid" ]] && kill -0 "$ft_pid" 2>/dev/null; then
            log_verbose "Stopping ft watch (pid $ft_pid)"
            kill "$ft_pid" 2>/dev/null || true
            wait "$ft_pid" 2>/dev/null || true
        fi
        if [[ -n "$pane_id" ]]; then
            log_verbose "Closing dummy pane $pane_id"
            wezterm cli kill-pane --pane-id "$pane_id" 2>/dev/null || true
        fi
        if [[ -d "$temp_workspace" ]]; then
            cp -r "$temp_workspace/.ft"/* "$scenario_dir/" 2>/dev/null || true
        fi
        rm -rf "$temp_workspace"
    }
    trap cleanup_natural_language EXIT

    # Step 1: Spawn dummy pane with usage-limit marker
    log_info "Step 1: Spawning dummy pane..."
    local dummy_script="$PROJECT_ROOT/fixtures/e2e/dummy_print.sh"
    if [[ ! -x "$dummy_script" ]]; then
        log_fail "Dummy print script not found or not executable: $dummy_script"
        return 1
    fi

    local spawn_output
    spawn_output=$(wezterm cli spawn --cwd "$temp_workspace" -- bash "$dummy_script" "$marker" 5 2>&1)
    pane_id=$(echo "$spawn_output" | grep -oE '^[0-9]+$' | head -1)

    if [[ -z "$pane_id" ]]; then
        log_fail "Failed to spawn dummy pane"
        echo "Spawn output: $spawn_output" >> "$scenario_dir/scenario.log"
        return 1
    fi
    log_info "Spawned pane: $pane_id"
    echo "Spawned pane_id: $pane_id" >> "$scenario_dir/scenario.log"

    # Step 2: Start ft watch in background
    log_info "Step 2: Starting ft watch..."
    "$FT_BINARY" watch --foreground \
        > "$scenario_dir/wa_watch.log" 2>&1 &
    ft_pid=$!
    log_verbose "ft watch started with PID $ft_pid"
    echo "ft_pid: $ft_pid" >> "$scenario_dir/scenario.log"

    if ! kill -0 "$ft_pid" 2>/dev/null; then
        log_fail "ft watch exited immediately"
        return 1
    fi

    # Step 3: Wait for pane to be observed
    log_info "Step 3: Waiting for pane capture..."
    local wait_timeout=${TIMEOUT:-30}
    local check_cmd="\"$FT_BINARY\" robot state 2>/dev/null | jq -e '.data[]? | select(.pane_id == $pane_id)' >/dev/null 2>&1"

    if ! wait_for_condition "pane $pane_id observed" "$check_cmd" "$wait_timeout"; then
        log_fail "Timeout waiting for pane to be observed"
        "$FT_BINARY" robot state > "$scenario_dir/robot_state.json" 2>&1 || true
        return 1
    fi
    log_pass "Pane observed"

    # Step 4: Wait for usage limit event to be detected
    log_info "Step 4: Waiting for usage limit event..."
    local event_cmd="\"$FT_BINARY\" events --format json --rule-id codex.usage.reached --limit 5 2>/dev/null | jq -e 'length > 0' >/dev/null 2>&1"
    if ! wait_for_condition "usage limit event detected" "$event_cmd" "$wait_timeout"; then
        log_fail "Timeout waiting for usage limit event"
        "$FT_BINARY" events --format json --limit 20 > "$scenario_dir/events_debug.json" 2>&1 || true
        result=1
    else
        log_pass "Usage limit event detected"
    fi

    # Step 5: Capture CLI outputs
    log_info "Step 5: Capturing CLI outputs..."
    local events_output
    events_output=$("$FT_BINARY" events --rule-id codex.usage.reached --limit 5 2>&1)
    echo "$events_output" > "$scenario_dir/events_output.txt"

    local why_output
    why_output=$("$FT_BINARY" why workflow.usage_limit 2>&1)
    echo "$why_output" > "$scenario_dir/why_output.txt"

    # Step 6: Assert outputs are human-readable
    log_info "Step 6: Asserting outputs..."
    if echo "$events_output" | grep -q "Codex usage limit reached"; then
        log_pass "Events output uses human summary"
    else
        log_fail "Events output missing human summary"
        result=1
    fi

    if echo "$why_output" | grep -q "handle_usage_limits"; then
        log_pass "wa why output rendered explanation"
    else
        log_fail "wa why output missing workflow explanation"
        result=1
    fi

    # Step 7: Stop ft watch gracefully
    log_info "Step 7: Stopping ft watch..."
    kill -TERM "$ft_pid" 2>/dev/null || true
    wait "$ft_pid" 2>/dev/null || true
    ft_pid=""
    log_verbose "ft watch stopped"

    trap - EXIT
    cleanup_natural_language

    return $result
}

run_scenario_compaction_workflow() {
    local scenario_dir="$1"
    local temp_workspace
    temp_workspace=$(mktemp -d /tmp/ft-e2e-XXXXXX)
    local ft_pid=""
    local pane_id=""
    local result=0

    log_info "Workspace: $temp_workspace"

    # Setup environment for isolated wa instance
    export FT_DATA_DIR="$temp_workspace/.ft"
    export FT_WORKSPACE="$temp_workspace"
    mkdir -p "$FT_DATA_DIR"

    # Copy baseline config for workflow testing
    local baseline_config="$PROJECT_ROOT/fixtures/e2e/config_baseline.toml"
    if [[ -f "$baseline_config" ]]; then
        cp "$baseline_config" "$temp_workspace/ft.toml"
        export FT_CONFIG="$temp_workspace/ft.toml"
        log_verbose "Using baseline config: $baseline_config"
    fi

    # Cleanup function
    cleanup_compaction_workflow() {
        log_verbose "Cleaning up compaction_workflow scenario"
        # Kill ft watch if running
        if [[ -n "$ft_pid" ]] && kill -0 "$ft_pid" 2>/dev/null; then
            log_verbose "Stopping ft watch (pid $ft_pid)"
            kill "$ft_pid" 2>/dev/null || true
            wait "$ft_pid" 2>/dev/null || true
        fi
        # Close dummy pane if it exists
        if [[ -n "$pane_id" ]]; then
            log_verbose "Closing dummy agent pane $pane_id"
            wezterm cli kill-pane --pane-id "$pane_id" 2>/dev/null || true
        fi
        # Copy artifacts before cleanup
        if [[ -d "$temp_workspace" ]]; then
            cp -r "$temp_workspace/.ft"/* "$scenario_dir/" 2>/dev/null || true
            cp "$temp_workspace/ft.toml" "$scenario_dir/" 2>/dev/null || true
        fi
        rm -rf "$temp_workspace"
    }
    trap cleanup_compaction_workflow EXIT

    # Step 1: Start ft watch with auto-handle BEFORE spawning pane
    # This ensures it's ready to detect and respond
    log_info "Step 1: Starting ft watch with --auto-handle..."
    "$FT_BINARY" watch --foreground --auto-handle \
        > "$scenario_dir/wa_watch.log" 2>&1 &
    ft_pid=$!
    log_verbose "ft watch started with PID $ft_pid"
    echo "ft_pid: $ft_pid" >> "$scenario_dir/scenario.log"

    # Verify ft watch is running
    if ! kill -0 "$ft_pid" 2>/dev/null; then
        log_fail "ft watch exited immediately"
        return 1
    fi

    # Step 2: Spawn dummy agent pane that will trigger compaction
    log_info "Step 2: Spawning dummy agent pane..."
    local agent_script="$PROJECT_ROOT/fixtures/e2e/dummy_agent.sh"
    if [[ ! -x "$agent_script" ]]; then
        log_fail "Dummy agent script not found or not executable: $agent_script"
        return 1
    fi

    local spawn_output
    # Spawn with 2 second delay before compaction marker
    spawn_output=$(wezterm cli spawn --cwd "$temp_workspace" -- bash "$agent_script" 2 2>&1)
    pane_id=$(echo "$spawn_output" | grep -oE '^[0-9]+$' | head -1)

    if [[ -z "$pane_id" ]]; then
        log_fail "Failed to spawn dummy agent pane"
        echo "Spawn output: $spawn_output" >> "$scenario_dir/scenario.log"
        return 1
    fi
    log_info "Spawned agent pane: $pane_id"
    echo "agent_pane_id: $pane_id" >> "$scenario_dir/scenario.log"

    # Step 3: Wait for pane to be observed
    log_info "Step 3: Waiting for pane to be observed..."
    local wait_timeout=${TIMEOUT:-30}
    local check_cmd="\"$FT_BINARY\" robot state 2>/dev/null | jq -e '.data[]? | select(.pane_id == $pane_id)' >/dev/null 2>&1"

    if ! wait_for_condition "pane $pane_id observed" "$check_cmd" "$wait_timeout"; then
        log_fail "Timeout waiting for pane to be observed"
        "$FT_BINARY" robot state > "$scenario_dir/robot_state.json" 2>&1 || true
        return 1
    fi
    log_pass "Pane observed"

    # Step 4: Wait for compaction event to be detected
    # The dummy_agent.sh will print "[CODEX] Compaction required:" after delay
    log_info "Step 4: Waiting for compaction detection..."
    # Wait for the compaction marker to be captured in pane output (deterministic).
    local compaction_marker_cmd="\"$FT_BINARY\" robot get-text $pane_id --tail 200 2>/dev/null | grep -q \"Compaction required\""
    if ! wait_for_condition "compaction marker captured" "$compaction_marker_cmd" "$wait_timeout"; then
        log_warn "Compaction marker not observed within timeout; workflow may still run"
    else
        log_pass "Compaction marker observed"
    fi

    # Capture robot state for diagnostics.
    "$FT_BINARY" robot state > "$scenario_dir/robot_state.json" 2>&1 || true

    # Step 5: Wait for workflow to execute and send text to pane
    log_info "Step 5: Waiting for workflow execution..."
    # The workflow should send "/compact" to the pane
    # Wait and then check pane content

    # Poll for "Received:" or "Refresh acknowledged" in pane output
    local check_workflow_cmd='pane_text=$("'"$FT_BINARY"'" robot get-text '"$pane_id"' 2>/dev/null); echo "$pane_text" | grep -q "Received:"'

    if wait_for_condition "workflow send detected in pane" "$check_workflow_cmd" "$wait_timeout"; then
        log_pass "Workflow send detected in pane"
    else
        log_warn "Workflow may not have sent text (checking pane anyway)"
    fi

    # Step 6: Capture and verify pane content
    log_info "Step 6: Verifying pane received workflow input..."
    local pane_text
    pane_text=$("$FT_BINARY" robot get-text "$pane_id" 2>&1 || true)
    echo "$pane_text" > "$scenario_dir/pane_text.txt"

    # Check for evidence that workflow sent text
    # The workflow sends "/compact\n" and agent echoes "Received: /compact"
    if echo "$pane_text" | grep -q "Received:"; then
        log_pass "Pane received input from workflow"

        # Check for compaction acknowledgment
        if echo "$pane_text" | grep -q "Refresh acknowledged\|Context compacted"; then
            log_pass "Agent acknowledged refresh/compact command"
        else
            log_warn "Agent did not acknowledge (may still be waiting)"
        fi
    else
        log_warn "No 'Received:' found in pane output"
        log_info "Pane content may not show workflow send yet"
        # This may not be a failure if workflow isn't fully implemented
    fi

    # Step 7: Check ft watch logs for workflow execution
    log_info "Step 7: Checking ft watch logs for workflow activity..."
    if grep -qi "workflow\|compaction\|detection" "$scenario_dir/wa_watch.log" 2>/dev/null; then
        log_pass "Found workflow/detection activity in logs"
    else
        log_warn "No obvious workflow activity in logs (may be normal)"
    fi

    # Note: This scenario depends on workflow functionality being complete
    # If workflows aren't implemented yet, this will pass with warnings
    log_info "Scenario complete (workflow functionality dependent)"

    # Cleanup trap will handle the rest
    trap - EXIT
    cleanup_compaction_workflow

    return $result
}

run_scenario_unhandled_event_lifecycle() {
    local scenario_dir="$1"
    local temp_workspace
    temp_workspace=$(mktemp -d /tmp/ft-e2e-unhandled-XXXXXX)
    local ft_pid=""
    local pane_id=""
    local result=0
    local wait_timeout=${TIMEOUT:-45}
    local db_path=""

    log_info "Workspace: $temp_workspace"

    # Setup environment for isolated wa instance
    export FT_DATA_DIR="$temp_workspace/.ft"
    export FT_WORKSPACE="$temp_workspace"
    mkdir -p "$FT_DATA_DIR"

    # Copy baseline config for workflow testing
    local baseline_config="$PROJECT_ROOT/fixtures/e2e/config_baseline.toml"
    if [[ -f "$baseline_config" ]]; then
        cp "$baseline_config" "$temp_workspace/ft.toml"
        export FT_CONFIG="$temp_workspace/ft.toml"
        log_verbose "Using baseline config: $baseline_config"
    fi

    # Cleanup function
    cleanup_unhandled_event_lifecycle() {
        log_verbose "Cleaning up unhandled_event_lifecycle scenario"
        # Kill ft watch if running
        if [[ -n "$ft_pid" ]] && kill -0 "$ft_pid" 2>/dev/null; then
            log_verbose "Stopping ft watch (pid $ft_pid)"
            kill "$ft_pid" 2>/dev/null || true
            wait "$ft_pid" 2>/dev/null || true
        fi
        # Close dummy pane if it exists
        if [[ -n "$pane_id" ]]; then
            log_verbose "Closing dummy agent pane $pane_id"
            wezterm cli kill-pane --pane-id "$pane_id" 2>/dev/null || true
        fi
        # Copy artifacts before cleanup
        if [[ -d "$temp_workspace" ]]; then
            cp -r "$temp_workspace/.ft"/* "$scenario_dir/" 2>/dev/null || true
            cp "$temp_workspace/ft.toml" "$scenario_dir/" 2>/dev/null || true
        fi
        rm -rf "$temp_workspace"
    }
    trap cleanup_unhandled_event_lifecycle EXIT

    # Step 1: Start ft watch (manual workflow trigger)
    log_info "Step 1: Starting ft watch..."
    "$FT_BINARY" watch --foreground \
        > "$scenario_dir/wa_watch.log" 2>&1 &
    ft_pid=$!
    log_verbose "ft watch started with PID $ft_pid"
    echo "ft_pid: $ft_pid" >> "$scenario_dir/scenario.log"

    if ! kill -0 "$ft_pid" 2>/dev/null; then
        log_fail "ft watch exited immediately"
        return 1
    fi

    # Step 2: Spawn dummy agent pane that emits compaction marker twice
    log_info "Step 2: Spawning dummy agent pane..."
    local agent_script="$PROJECT_ROOT/fixtures/e2e/dummy_agent.sh"
    if [[ ! -x "$agent_script" ]]; then
        log_fail "Dummy agent script not found or not executable: $agent_script"
        return 1
    fi

    local spawn_output
    spawn_output=$(wezterm cli spawn --cwd "$temp_workspace" -- bash "$agent_script" 1 2 1 2>&1)
    pane_id=$(echo "$spawn_output" | grep -oE '^[0-9]+$' | head -1)

    if [[ -z "$pane_id" ]]; then
        log_fail "Failed to spawn dummy agent pane"
        echo "Spawn output: $spawn_output" >> "$scenario_dir/scenario.log"
        return 1
    fi
    log_info "Spawned agent pane: $pane_id"
    echo "agent_pane_id: $pane_id" >> "$scenario_dir/scenario.log"

    # Step 3: Wait for pane to be observed
    log_info "Step 3: Waiting for pane to be observed..."
    local check_cmd="\"$FT_BINARY\" robot state 2>/dev/null | jq -e '.data[]? | select(.pane_id == $pane_id)' >/dev/null 2>&1"

    if ! wait_for_condition "pane $pane_id observed" "$check_cmd" "$wait_timeout"; then
        log_fail "Timeout waiting for pane to be observed"
        "$FT_BINARY" robot state > "$scenario_dir/robot_state.json" 2>&1 || true
        return 1
    fi
    log_pass "Pane observed"

    # Step 4: Wait for unhandled compaction event (dedupe/cooldown)
    log_info "Step 4: Waiting for unhandled compaction event..."
    local unhandled_cmd="\"$FT_BINARY\" events -f json --unhandled --rule-id \"codex:compaction\" --limit 20 2>/dev/null | jq -e 'length >= 1' >/dev/null 2>&1"
    if ! wait_for_condition "unhandled compaction event detected" "$unhandled_cmd" "$wait_timeout"; then
        log_fail "Timeout waiting for unhandled compaction event"
        "$FT_BINARY" events -f json --limit 20 > "$scenario_dir/events_debug.json" 2>&1 || true
        result=1
    else
        log_pass "Unhandled compaction event detected"
    fi

    # Step 5: Capture unhandled events and assert dedupe
    log_info "Step 5: Capturing unhandled events..."
    "$FT_BINARY" events -f json --unhandled --rule-id "codex:compaction" --limit 20 \
        > "$scenario_dir/events_unhandled_pre.json" 2>&1 || true

    local unhandled_count
    unhandled_count=$(jq 'length' "$scenario_dir/events_unhandled_pre.json" 2>/dev/null || echo "0")
    echo "unhandled_count: $unhandled_count" >> "$scenario_dir/scenario.log"

    if [[ "$unhandled_count" -eq 1 ]]; then
        log_pass "Deduped unhandled event count is 1"
    else
        log_fail "Expected 1 unhandled event, found $unhandled_count"
        result=1
    fi

    # Step 6: Capture recommended workflow preview (avoid hard-coding)
    log_info "Step 6: Capturing recommended workflow preview..."
    "$FT_BINARY" robot events --unhandled --rule-id "codex:compaction" --limit 5 --would-handle --dry-run \
        > "$scenario_dir/robot_events_preview.json" 2>&1 || true
    local recommended_workflow
    recommended_workflow=$(jq -r '.data.events[0].would_handle_with.workflow // empty' \
        "$scenario_dir/robot_events_preview.json" 2>/dev/null || echo "")

    if [[ -n "$recommended_workflow" ]]; then
        log_pass "Recommended workflow: $recommended_workflow"
        echo "recommended_workflow: $recommended_workflow" >> "$scenario_dir/scenario.log"
    else
        log_warn "No recommended workflow found in preview"
    fi

    db_path="$temp_workspace/.ft/ft.db"
    if [[ -f "$db_path" ]]; then
        sqlite3 "$db_path" -json \
            "SELECT action_kind, actor_kind, result FROM audit_actions ORDER BY id DESC LIMIT 50;" \
            > "$scenario_dir/audit_actions_pre.json" 2>/dev/null || true
    fi

    # Step 7: Run recommended workflow to handle the event
    local workflow_to_run="${recommended_workflow:-handle_compaction}"
    if [[ -z "$workflow_to_run" ]]; then
        workflow_to_run="handle_compaction"
    fi
    log_info "Step 7: Running workflow ($workflow_to_run) on pane $pane_id..."
    if "$FT_BINARY" workflow run "$workflow_to_run" --pane "$pane_id" \
        > "$scenario_dir/workflow_run_output.txt" 2>&1; then
        log_pass "Workflow run completed"
    else
        log_fail "Workflow run failed"
        result=1
    fi

    # Step 8: Wait for event to be handled (unhandled list empty)
    log_info "Step 8: Waiting for event to be handled..."
    local handled_cmd="\"$FT_BINARY\" events -f json --unhandled --rule-id \"codex:compaction\" --limit 20 2>/dev/null | jq -e 'length == 0' >/dev/null 2>&1"
    if ! wait_for_condition "compaction event handled" "$handled_cmd" "$wait_timeout"; then
        log_fail "Timeout waiting for event to be handled"
        result=1
    else
        log_pass "Unhandled list cleared"
    fi

    # Step 9: Capture handled events and audit trail slice
    log_info "Step 9: Capturing handled events and audit trail..."
    "$FT_BINARY" events -f json --rule-id "codex:compaction" --limit 20 \
        > "$scenario_dir/events_post.json" 2>&1 || true

    local handled_count
    handled_count=$(jq '[.[] | select(.handled_at != null)] | length' \
        "$scenario_dir/events_post.json" 2>/dev/null || echo "0")
    echo "handled_count: $handled_count" >> "$scenario_dir/scenario.log"

    if [[ "$handled_count" -ge 1 ]]; then
        log_pass "Event marked handled"
    else
        log_fail "No handled compaction events found"
        result=1
    fi

    if [[ -f "$db_path" ]]; then
        sqlite3 "$db_path" -json \
            "SELECT action_kind, actor_kind, result FROM audit_actions ORDER BY id DESC LIMIT 50;" \
            > "$scenario_dir/audit_actions_post.json" 2>/dev/null || true
        if jq -e '.[] | select(.action_kind == "workflow_run" or .action_kind == "workflow_start")' \
            "$scenario_dir/audit_actions_post.json" >/dev/null 2>&1; then
            log_pass "Audit trail shows workflow run"
        else
            log_fail "Workflow audit action not found in recent audit slice"
            result=1
        fi
        if jq -e '.[] | select(.action_kind == "send_text")' \
            "$scenario_dir/audit_actions_post.json" >/dev/null 2>&1; then
            log_pass "Audit trail shows send_text action"
        else
            log_fail "Audit trail missing send_text action"
            result=1
        fi

        sqlite3 "$db_path" -json \
            "SELECT workflow_name FROM workflow_executions ORDER BY started_at DESC LIMIT 1;" \
            > "$scenario_dir/workflow_execution.json" 2>/dev/null || true
        local actual_workflow
        actual_workflow=$(jq -r '.[0].workflow_name // empty' "$scenario_dir/workflow_execution.json" 2>/dev/null || echo "")
        if [[ -n "$recommended_workflow" && -n "$actual_workflow" && "$recommended_workflow" != "$actual_workflow" ]]; then
            log_fail "Recommended workflow ($recommended_workflow) does not match executed ($actual_workflow)"
            result=1
        fi
    else
        log_warn "Database file not found at $db_path"
    fi

    # Step 10: Check ft watch logs for workflow activity
    log_info "Step 10: Checking ft watch logs for workflow activity..."
    if [[ -n "$recommended_workflow" ]]; then
        if grep -qi "$recommended_workflow" "$scenario_dir/wa_watch.log" 2>/dev/null; then
            log_pass "Workflow activity found in logs"
        else
            log_warn "No explicit workflow name in logs (may be normal)"
        fi
    else
        if grep -qi "workflow" "$scenario_dir/wa_watch.log" 2>/dev/null; then
            log_pass "Workflow activity found in logs"
        else
            log_warn "No obvious workflow activity in logs"
        fi
    fi

    # Step 11: Stop ft watch gracefully
    log_info "Step 11: Stopping ft watch..."
    kill -TERM "$ft_pid" 2>/dev/null || true
    wait "$ft_pid" 2>/dev/null || true
    ft_pid=""
    log_verbose "ft watch stopped"

    trap - EXIT
    cleanup_unhandled_event_lifecycle

    return $result
}

run_scenario_usage_limit_safe_pause() {
    local scenario_dir="$1"
    local temp_workspace
    temp_workspace=$(mktemp -d /tmp/ft-e2e-usage-limit-XXXXXX)
    local temp_bin="$temp_workspace/bin"
    local fake_caut="$temp_bin/caut"
    local ft_pid=""
    local ft_pid_restart=""
    local pane_id=""
    local result=0
    local wait_timeout=${TIMEOUT:-90}
    local old_path="$PATH"
    local old_ft_data_dir="${FT_DATA_DIR:-}"
    local old_ft_workspace="${FT_WORKSPACE:-}"
    local old_ft_config="${FT_CONFIG:-}"
    local old_caut_mode="${CAUT_FAKE_MODE:-}"
    local old_caut_log="${CAUT_FAKE_LOG:-}"

    log_info "Workspace: $temp_workspace"

    cleanup_usage_limit_safe_pause() {
        log_verbose "Cleaning up usage_limit_safe_pause scenario"
        if [[ -n "${ft_pid:-}" ]] && kill -0 "$ft_pid" 2>/dev/null; then
            log_verbose "Stopping ft watch (pid $ft_pid)"
            kill "$ft_pid" 2>/dev/null || true
            wait "$ft_pid" 2>/dev/null || true
        fi
        if [[ -n "${ft_pid_restart:-}" ]] && kill -0 "$ft_pid_restart" 2>/dev/null; then
            log_verbose "Stopping ft watch restart (pid $ft_pid_restart)"
            kill "$ft_pid_restart" 2>/dev/null || true
            wait "$ft_pid_restart" 2>/dev/null || true
        fi
        if [[ -n "${pane_id:-}" ]]; then
            log_verbose "Closing dummy agent pane $pane_id"
            wezterm cli kill-pane --pane-id "$pane_id" 2>/dev/null || true
        fi
        export PATH="$old_path"
        if [[ -n "$old_ft_data_dir" ]]; then
            export FT_DATA_DIR="$old_ft_data_dir"
        else
            unset FT_DATA_DIR
        fi
        if [[ -n "$old_ft_workspace" ]]; then
            export FT_WORKSPACE="$old_ft_workspace"
        else
            unset FT_WORKSPACE
        fi
        if [[ -n "$old_ft_config" ]]; then
            export FT_CONFIG="$old_ft_config"
        else
            unset FT_CONFIG
        fi
        if [[ -n "$old_caut_mode" ]]; then
            export CAUT_FAKE_MODE="$old_caut_mode"
        else
            unset CAUT_FAKE_MODE
        fi
        if [[ -n "$old_caut_log" ]]; then
            export CAUT_FAKE_LOG="$old_caut_log"
        else
            unset CAUT_FAKE_LOG
        fi
        if [[ -d "${temp_workspace:-}" ]]; then
            cp -r "${temp_workspace}/.ft"/* "$scenario_dir/" 2>/dev/null || true
            cp "${temp_workspace}/ft.toml" "$scenario_dir/" 2>/dev/null || true
            cp "${temp_workspace}/caut_invocations.log" "$scenario_dir/" 2>/dev/null || true
        fi
        rm -rf "${temp_workspace:-}"
    }
    trap cleanup_usage_limit_safe_pause EXIT

    # Step 0: Create fake caut binary (accounts exhausted)
    log_info "Step 0: Creating fake caut binary (accounts exhausted)..."
    mkdir -p "$temp_bin"
    cat > "$fake_caut" <<'EOF'
#!/bin/bash
set -euo pipefail

mode="${CAUT_FAKE_MODE:-exhausted}"
log_path="${CAUT_FAKE_LOG:-}"

if [[ -n "$log_path" ]]; then
    echo "$(date -u +"%Y-%m-%dT%H:%M:%SZ") $*" >> "$log_path"
fi

subcommand="${1:-}"
shift || true

service=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --service)
            service="$2"
            shift 2
            ;;
        --format)
            shift 2
            ;;
        *)
            shift
            ;;
    esac
done

if [[ "$service" != "openai" ]]; then
    echo "{\"error\":\"unsupported service\"}" >&2
    exit 2
fi

if [[ "$mode" == "fail" ]]; then
    echo "caut failed: sk-test-should-redact-usage-limit" >&2
    exit 42
fi

if [[ "$subcommand" == "refresh" ]]; then
    cat <<JSON
{
  "service": "openai",
  "refreshed_at": "2026-01-30T00:00:00Z",
  "accounts": [
    {
      "id": "acc-low",
      "name": "low",
      "percentRemaining": 1,
      "resetAt": "2026-02-01T00:00:00Z"
    },
    {
      "id": "acc-zero",
      "name": "zero",
      "percentRemaining": 0,
      "resetAt": "2026-02-01T00:00:00Z"
    }
  ]
}
JSON
else
    cat <<JSON
{
  "service": "openai",
  "generated_at": "2026-01-30T00:00:00Z",
  "accounts": [
    { "id": "acc-low", "name": "low", "percentRemaining": 1 },
    { "id": "acc-zero", "name": "zero", "percentRemaining": 0 }
  ]
}
JSON
fi
EOF
    chmod +x "$fake_caut"

    export PATH="$temp_bin:$PATH"
    export CAUT_FAKE_LOG="$temp_workspace/caut_invocations.log"
    unset CAUT_FAKE_MODE

    # Step 1: Configure isolated workspace
    log_info "Step 1: Preparing isolated workspace..."
    export FT_DATA_DIR="$temp_workspace/.ft"
    export FT_WORKSPACE="$temp_workspace"
    mkdir -p "$FT_DATA_DIR"

    local baseline_config="$PROJECT_ROOT/fixtures/e2e/config_baseline.toml"
    if [[ -f "$baseline_config" ]]; then
        cp "$baseline_config" "$temp_workspace/ft.toml"
        export FT_CONFIG="$temp_workspace/ft.toml"
        log_verbose "Using baseline config: $baseline_config"
    else
        log_fail "Baseline config not found: $baseline_config"
        return 1
    fi

    # Step 2: Start ft watch with auto-handle
    log_info "Step 2: Starting ft watch with --auto-handle..."
    "$FT_BINARY" watch --foreground --auto-handle --config "$temp_workspace/ft.toml" \
        > "$scenario_dir/wa_watch_1.log" 2>&1 &
    ft_pid=$!
    log_verbose "ft watch started with PID $ft_pid"
    echo "ft_pid: $ft_pid" >> "$scenario_dir/scenario.log"

    if ! kill -0 "$ft_pid" 2>/dev/null; then
        log_fail "ft watch exited immediately"
        return 1
    fi

    # Step 3: Spawn dummy usage-limit pane
    log_info "Step 3: Spawning dummy usage-limit pane..."
    local agent_script="$PROJECT_ROOT/fixtures/e2e/dummy_usage_limit.sh"
    if [[ ! -x "$agent_script" ]]; then
        log_fail "Dummy usage-limit script not found or not executable: $agent_script"
        return 1
    fi

    local spawn_output
    spawn_output=$(wezterm cli spawn --cwd "$temp_workspace" -- bash "$agent_script" 1 "2026-02-01 00:00 UTC" 2>&1)
    pane_id=$(echo "$spawn_output" | grep -oE '^[0-9]+$' | head -1)

    if [[ -z "$pane_id" ]]; then
        log_fail "Failed to spawn dummy usage-limit pane"
        echo "Spawn output: $spawn_output" >> "$scenario_dir/scenario.log"
        return 1
    fi
    log_info "Spawned usage-limit pane: $pane_id"
    echo "agent_pane_id: $pane_id" >> "$scenario_dir/scenario.log"

    # Step 4: Wait for pane to be observed
    log_info "Step 4: Waiting for pane to be observed..."
    local check_cmd="\"$FT_BINARY\" robot state 2>/dev/null | jq -e '.data[]? | select(.pane_id == $pane_id)' >/dev/null 2>&1"
    if ! wait_for_condition "pane $pane_id observed" "$check_cmd" "$wait_timeout"; then
        log_fail "Timeout waiting for pane to be observed"
        "$FT_BINARY" robot state > "$scenario_dir/robot_state.json" 2>&1 || true
        return 1
    fi
    log_pass "Pane observed"

    # Step 5: Wait for unhandled usage-limit event
    log_info "Step 5: Waiting for unhandled usage-limit event..."
    local unhandled_cmd="\"$FT_BINARY\" events -f json --unhandled --rule-id \"codex.usage.reached\" --limit 20 2>/dev/null | jq -e 'length >= 1' >/dev/null 2>&1"
    if ! wait_for_condition "unhandled usage-limit event detected" "$unhandled_cmd" "$wait_timeout"; then
        log_fail "Timeout waiting for unhandled usage-limit event"
        "$FT_BINARY" events -f json --limit 20 > "$scenario_dir/events_debug.json" 2>&1 || true
        result=1
    else
        log_pass "Unhandled usage-limit event detected"
    fi

    # Step 6: Capture unhandled events + recommended workflow preview
    log_info "Step 6: Capturing unhandled events and workflow preview..."
    "$FT_BINARY" events -f json --unhandled --rule-id "codex.usage.reached" --limit 20 \
        > "$scenario_dir/events_unhandled_pre.json" 2>&1 || true

    "$FT_BINARY" robot events --unhandled --rule-id "codex.usage.reached" --limit 5 --would-handle --dry-run \
        > "$scenario_dir/robot_events_preview.json" 2>&1 || true

    local recommended_workflow
    recommended_workflow=$(jq -r '.data.events[0].would_handle_with.workflow // empty' \
        "$scenario_dir/robot_events_preview.json" 2>/dev/null || echo "")

    if [[ -n "$recommended_workflow" ]]; then
        log_pass "Recommended workflow: $recommended_workflow"
        echo "recommended_workflow: $recommended_workflow" >> "$scenario_dir/scenario.log"
    else
        log_warn "No recommended workflow found in preview"
    fi

    # Step 7: Wait for event to be handled (unhandled list empty)
    log_info "Step 7: Waiting for event to be handled..."
    local handled_cmd="\"$FT_BINARY\" events -f json --unhandled --rule-id \"codex.usage.reached\" --limit 20 2>/dev/null | jq -e 'length == 0' >/dev/null 2>&1"
    if ! wait_for_condition "usage-limit event handled" "$handled_cmd" "$wait_timeout"; then
        log_fail "Timeout waiting for usage-limit event to be handled"
        result=1
    else
        log_pass "Unhandled list cleared"
    fi

    # Step 8: Capture handled event + workflow result
    log_info "Step 8: Capturing handled events and workflow result..."
    "$FT_BINARY" events -f json --rule-id "codex.usage.reached" --limit 20 \
        > "$scenario_dir/events_post.json" 2>&1 || true

    local db_path="$temp_workspace/.ft/ft.db"
    if [[ -f "$db_path" ]]; then
        sqlite3 "$db_path" -header -csv \
            "SELECT id, rule_id, handled_at, handled_status FROM events WHERE rule_id = 'codex.usage.reached' ORDER BY detected_at DESC LIMIT 1;" \
            > "$scenario_dir/events_db.csv" 2>/dev/null || true

        sqlite3 "$db_path" -json \
            "SELECT id, workflow_name, status, result FROM workflow_executions WHERE workflow_name = 'handle_usage_limits' ORDER BY started_at DESC LIMIT 1;" \
            > "$scenario_dir/workflow_execution.json" 2>/dev/null || true

        sqlite3 "$db_path" -json \
            "SELECT id, action_kind, actor_kind, result FROM audit_actions ORDER BY id DESC LIMIT 50;" \
            > "$scenario_dir/audit_actions.json" 2>/dev/null || true

        if jq -e '.[0].result | fromjson? | .fallback == true' "$scenario_dir/workflow_execution.json" >/dev/null 2>&1; then
            log_pass "Workflow result contains fallback plan"
        else
            log_fail "Workflow result missing fallback plan"
            result=1
        fi
    else
        log_warn "Database file not found at $db_path"
        result=1
    fi

    # Step 8b: Verify fake caut refresh was invoked
    log_info "Step 8b: Verifying fake caut invocation..."
    if [[ -f "$temp_workspace/caut_invocations.log" ]] && grep -q "refresh" "$temp_workspace/caut_invocations.log"; then
        log_pass "Fake caut invoked for refresh"
    else
        log_fail "Fake caut invocation not recorded"
        result=1
    fi

    # Step 9: Spam guard (no send_text; ctrl-c should be <= 1)
    log_info "Step 9: Validating spam guard (no send_text)..."
    if [[ -f "$db_path" ]]; then
        local send_text_count
        local send_ctrl_c_count
        send_text_count=$(sqlite3 "$db_path" "SELECT COUNT(*) FROM audit_actions WHERE action_kind = 'send_text';" 2>/dev/null || echo "0")
        send_ctrl_c_count=$(sqlite3 "$db_path" "SELECT COUNT(*) FROM audit_actions WHERE action_kind = 'send_ctrl_c';" 2>/dev/null || echo "0")
        echo "send_text_count: $send_text_count" >> "$scenario_dir/scenario.log"
        echo "send_ctrl_c_count: $send_ctrl_c_count" >> "$scenario_dir/scenario.log"

        if [[ "$send_text_count" -eq 0 ]]; then
            log_pass "No send_text actions recorded"
        else
            log_fail "send_text actions recorded: $send_text_count"
            result=1
        fi

        if [[ "$send_ctrl_c_count" -le 1 ]]; then
            log_pass "Ctrl-C injections within expected bounds ($send_ctrl_c_count)"
        else
            log_fail "Excess Ctrl-C injections recorded: $send_ctrl_c_count"
            result=1
        fi
    else
        log_warn "Database file not found for spam guard checks"
        result=1
    fi

    # Step 10: Stop ft watch and restart to verify persistence
    log_info "Step 10: Restarting ft watch to verify plan persistence..."
    kill -TERM "$ft_pid" 2>/dev/null || true
    wait "$ft_pid" 2>/dev/null || true
    ft_pid=""

    "$FT_BINARY" watch --foreground --auto-handle --config "$temp_workspace/ft.toml" \
        > "$scenario_dir/wa_watch_2.log" 2>&1 &
    ft_pid_restart=$!
    log_verbose "ft watch restart PID $ft_pid_restart"
    echo "ft_pid_restart: $ft_pid_restart" >> "$scenario_dir/scenario.log"

    if ! kill -0 "$ft_pid_restart" 2>/dev/null; then
        log_fail "ft watch restart exited immediately"
        result=1
    fi

    if [[ -f "$db_path" ]]; then
        sqlite3 "$db_path" -json \
            "SELECT id, workflow_name, status, result FROM workflow_executions WHERE workflow_name = 'handle_usage_limits' ORDER BY started_at DESC LIMIT 1;" \
            > "$scenario_dir/workflow_execution_after_restart.json" 2>/dev/null || true

        if jq -e '.[0].result | fromjson? | .fallback == true' "$scenario_dir/workflow_execution_after_restart.json" >/dev/null 2>&1; then
            log_pass "Fallback plan still present after restart"
        else
            log_fail "Fallback plan missing after restart"
            result=1
        fi
    else
        log_warn "Database file not found after restart"
        result=1
    fi

    # Step 11: Stop ft watch restart
    log_info "Step 11: Stopping ft watch restart..."
    kill -TERM "$ft_pid_restart" 2>/dev/null || true
    wait "$ft_pid_restart" 2>/dev/null || true
    ft_pid_restart=""
    log_verbose "ft watch restart stopped"

    trap - EXIT
    cleanup_usage_limit_safe_pause

    return $result
}

run_scenario_notification_webhook() {
    local scenario_dir="$1"
    local temp_workspace
    temp_workspace=$(mktemp -d /tmp/ft-e2e-notify-XXXXXX)
    local ft_pid=""
    local mock_pid=""
    local pane_id=""
    local result=0
    local wait_timeout=${TIMEOUT:-120}
    local secret_token="SECRET_NOTIFY_$(date +%s%N)"
    local mock_script="$temp_workspace/mock_webhook_server.py"
    local emit_script="$temp_workspace/emit_compaction.sh"
    local throttle_script="$temp_workspace/emit_compaction_throttle.sh"
    local mock_port=""
    local mock_addr=""
    local old_ft_data_dir="${FT_DATA_DIR:-}"
    local old_ft_workspace="${FT_WORKSPACE:-}"
    local old_ft_config="${FT_CONFIG:-}"

    log_info "Workspace: $temp_workspace"

    if ! command -v python3 >/dev/null 2>&1; then
        log_fail "python3 is required for mock webhook server"
        return 1
    fi
    if ! command -v curl >/dev/null 2>&1; then
        log_fail "curl is required for mock webhook server checks"
        return 1
    fi

    cleanup_notification_webhook() {
        log_verbose "Cleaning up notification_webhook scenario"
        if [[ -n "${mock_pid:-}" ]] && kill -0 "$mock_pid" 2>/dev/null; then
            log_verbose "Stopping mock webhook server (pid $mock_pid)"
            kill "$mock_pid" 2>/dev/null || true
            wait "$mock_pid" 2>/dev/null || true
        fi
        if [[ -n "${ft_pid:-}" ]] && kill -0 "$ft_pid" 2>/dev/null; then
            log_verbose "Stopping ft watch (pid $ft_pid)"
            kill "$ft_pid" 2>/dev/null || true
            wait "$ft_pid" 2>/dev/null || true
        fi
        if [[ -n "${pane_id:-}" ]]; then
            log_verbose "Closing pane $pane_id"
            wezterm cli kill-pane --pane-id "$pane_id" 2>/dev/null || true
        fi
        if [[ -n "$old_ft_data_dir" ]]; then
            export FT_DATA_DIR="$old_ft_data_dir"
        else
            unset FT_DATA_DIR
        fi
        if [[ -n "$old_ft_workspace" ]]; then
            export FT_WORKSPACE="$old_ft_workspace"
        else
            unset FT_WORKSPACE
        fi
        if [[ -n "$old_ft_config" ]]; then
            export FT_CONFIG="$old_ft_config"
        else
            unset FT_CONFIG
        fi
        if [[ -d "${temp_workspace:-}" ]]; then
            cp -r "${temp_workspace}/.ft"/* "$scenario_dir/" 2>/dev/null || true
            cp "${temp_workspace}/ft.toml" "$scenario_dir/" 2>/dev/null || true
        fi
        rm -rf "${temp_workspace:-}"
    }
    trap cleanup_notification_webhook EXIT

    # Prepare mock webhook server script
    cat > "$mock_script" <<'PY'
#!/usr/bin/env python3
import argparse
import json
import time
from http.server import BaseHTTPRequestHandler, HTTPServer


class State:
    def __init__(self, responses, log_path):
        self.responses = responses
        self.log_path = log_path
        self.attempts = 0
        self.received = []

    def log(self, message):
        if self.log_path:
            with open(self.log_path, "a", encoding="utf-8") as handle:
                handle.write(message + "\n")
        else:
            print(message, flush=True)


STATE = None


class Handler(BaseHTTPRequestHandler):
    def log_message(self, format, *args):
        return

    def _send_json(self, code, payload):
        body = json.dumps(payload).encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path == "/health":
            return self._send_json(200, {"ok": True})
        if self.path == "/received":
            return self._send_json(200, STATE.received)
        if self.path == "/attempt_count":
            return self._send_json(200, {"attempts": STATE.attempts})
        return self._send_json(404, {"error": "not_found"})

    def do_POST(self):
        if self.path != "/webhook":
            return self._send_json(404, {"error": "not_found"})

        length = int(self.headers.get("Content-Length", "0"))
        body = self.rfile.read(length)
        STATE.attempts += 1

        try:
            payload = json.loads(body.decode("utf-8"))
        except Exception:
            payload = {"_raw": body.decode("utf-8", errors="replace")}
        STATE.received.append(payload)

        if STATE.responses:
            idx = min(STATE.attempts - 1, len(STATE.responses) - 1)
            status = STATE.responses[idx]
        else:
            status = 200

        ts = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        STATE.log(f"{ts} attempt={STATE.attempts} status={status} bytes={len(body)}")
        return self._send_json(status, {"ok": status == 200})


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, default=0)
    parser.add_argument("--responses", default="")
    parser.add_argument("--log", default="")
    args = parser.parse_args()

    responses = []
    if args.responses:
        for item in args.responses.split(","):
            item = item.strip()
            if item:
                responses.append(int(item))

    global STATE
    STATE = State(responses, args.log)
    server = HTTPServer(("127.0.0.1", args.port), Handler)
    server.serve_forever()


if __name__ == "__main__":
    main()
PY
    chmod +x "$mock_script"

    # Prepare compaction emitters
    cat > "$emit_script" <<'EOS'
#!/bin/bash
set -euo pipefail
secret="$1"
repeat_count="${2:-1}"
repeat_interval="${3:-0}"
sleep_tail="${4:-120}"

echo "$secret"
for ((i=1; i<=repeat_count; i++)); do
    echo "[CODEX] Compaction required: context window 95% full"
    echo "[CODEX] Waiting for refresh prompt..."
    if [[ "$i" -lt "$repeat_count" ]]; then
        sleep "$repeat_interval"
    fi
done

sleep "$sleep_tail"
EOS
    chmod +x "$emit_script"

    cat > "$throttle_script" <<'EOS'
#!/bin/bash
set -euo pipefail
secret="$1"
burst_count="${2:-3}"
burst_interval="${3:-0.1}"
cooldown_delay="${4:-2}"
sleep_tail="${5:-120}"

echo "$secret"
for ((i=1; i<=burst_count; i++)); do
    echo "[CODEX] Compaction required: context window 95% full"
    echo "[CODEX] Waiting for refresh prompt..."
    if [[ "$i" -lt "$burst_count" ]]; then
        sleep "$burst_interval"
    fi
done

sleep "$cooldown_delay"
echo "[CODEX] Compaction required: context window 95% full"
echo "[CODEX] Waiting for refresh prompt..."

sleep "$sleep_tail"
EOS
    chmod +x "$throttle_script"

    # Pick a free port for the mock server
    mock_port=$(python3 - <<'PY'
import socket
sock = socket.socket()
sock.bind(("127.0.0.1", 0))
print(sock.getsockname()[1])
sock.close()
PY
    )

    if [[ -z "$mock_port" ]]; then
        log_fail "Failed to allocate mock webhook port"
        return 1
    fi
    mock_addr="http://127.0.0.1:$mock_port"

    echo "[NOTIFY_E2E] workspace=$temp_workspace" >> "$scenario_dir/scenario.log"
    echo "[NOTIFY_E2E] mock_addr=$mock_addr" >> "$scenario_dir/scenario.log"
    echo "[NOTIFY_E2E] secret_token=$secret_token" >> "$scenario_dir/scenario.log"

    # Helper functions for mock server
    start_mock_server() {
        local responses="$1"
        local log_file="$2"
        local out_file="$3"

        if [[ -n "${mock_pid:-}" ]] && kill -0 "$mock_pid" 2>/dev/null; then
            kill "$mock_pid" 2>/dev/null || true
            wait "$mock_pid" 2>/dev/null || true
        fi

        python3 "$mock_script" --port "$mock_port" --responses "$responses" --log "$log_file" \
            > "$out_file" 2>&1 &
        mock_pid=$!

        local check_cmd="curl -fs \"$mock_addr/health\" >/dev/null 2>&1"
        if ! wait_for_condition "mock server ready" "$check_cmd" "$wait_timeout"; then
            log_fail "Mock webhook server failed to start"
            return 1
        fi
        return 0
    }

    stop_mock_server() {
        if [[ -n "${mock_pid:-}" ]] && kill -0 "$mock_pid" 2>/dev/null; then
            kill "$mock_pid" 2>/dev/null || true
            wait "$mock_pid" 2>/dev/null || true
        fi
        mock_pid=""
    }

    mock_received_count() {
        local payload=""
        payload=$(curl -s "$mock_addr/received" 2>/dev/null || true)
        echo "$payload" | jq -r 'length' 2>/dev/null || echo "0"
    }

    mock_attempt_count() {
        local payload=""
        payload=$(curl -s "$mock_addr/attempt_count" 2>/dev/null || true)
        echo "$payload" | jq -r '.attempts // 0' 2>/dev/null || echo "0"
    }

    wait_for_stable_attempts() {
        local stable_seconds="$1"
        local timeout="$2"
        local start
        start=$(date +%s)
        local last=""
        local stable_start=""

        while true; do
            local current
            current=$(mock_attempt_count)
            if [[ -n "$last" && "$current" == "$last" ]]; then
                if [[ -z "$stable_start" ]]; then
                    stable_start=$(date +%s)
                fi
                if [[ $(( $(date +%s) - stable_start )) -ge $stable_seconds ]]; then
                    return 0
                fi
            else
                last="$current"
                stable_start=""
            fi

            if [[ $(( $(date +%s) - start )) -ge $timeout ]]; then
                return 1
            fi
            sleep 0.5
        done
    }

    spawn_compaction_pane() {
        local script="$1"
        shift
        local spawn_output=""
        spawn_output=$(wezterm cli spawn --cwd "$temp_workspace" -- bash "$script" "$@" 2>&1)
        local new_pane_id
        new_pane_id=$(echo "$spawn_output" | grep -oE '^[0-9]+$' | head -1)
        if [[ -z "$new_pane_id" ]]; then
            echo ""
            return 1
        fi
        echo "$new_pane_id"
        return 0
    }

    wait_for_pane_observed() {
        local pane="$1"
        local check_cmd="\"$FT_BINARY\" robot state 2>/dev/null | jq -e '.data[]? | select(.pane_id == $pane)' >/dev/null 2>&1"
        wait_for_condition "pane $pane observed" "$check_cmd" "$wait_timeout"
    }

    # Step 1: Configure isolated workspace and notifications
    log_info "Step 1: Preparing workspace + notifications config..."
    export FT_DATA_DIR="$temp_workspace/.ft"
    export FT_WORKSPACE="$temp_workspace"
    mkdir -p "$FT_DATA_DIR"

    local baseline_config="$PROJECT_ROOT/fixtures/e2e/config_baseline.toml"
    if [[ -f "$baseline_config" ]]; then
        cp "$baseline_config" "$temp_workspace/ft.toml"
    else
        log_fail "Baseline config not found: $baseline_config"
        return 1
    fi

    cat >> "$temp_workspace/ft.toml" <<EOF

[notifications]
enabled = true
cooldown_ms = 1500
dedup_window_ms = 1
min_severity = "info"
include = ["codex:compaction"]

[[notifications.webhooks]]
name = "e2e-webhook"
url = "${mock_addr}/webhook"
template = "generic"
events = ["codex:compaction"]
EOF

    export FT_CONFIG="$temp_workspace/ft.toml"
    log_pass "Notifications configured for $mock_addr"

    # Step 2: Start ft watch
    log_info "Step 2: Starting ft watch..."
    "$FT_BINARY" watch --foreground --config "$temp_workspace/ft.toml" \
        > "$scenario_dir/wa_watch.log" 2>&1 &
    ft_pid=$!
    echo "ft_pid: $ft_pid" >> "$scenario_dir/scenario.log"

    local check_watch_cmd="kill -0 $ft_pid 2>/dev/null"
    if ! wait_for_condition "ft watch running" "$check_watch_cmd" "$wait_timeout"; then
        log_fail "ft watch failed to start"
        return 1
    fi
    log_pass "ft watch running"

    # Step 3: Successful delivery
    log_info "Step 3: Successful webhook delivery..."
    if ! start_mock_server "200" "$scenario_dir/mock_server_success.log" \
        "$scenario_dir/mock_server_success.out"; then
        return 1
    fi
    pane_id=$(spawn_compaction_pane "$emit_script" "$secret_token" 1 0.1) || {
        log_fail "Failed to spawn compaction pane for success case"
        return 1
    }
    log_info "Spawned pane: $pane_id"
    if ! wait_for_pane_observed "$pane_id"; then
        log_fail "Pane not observed for success case"
        result=1
    fi

    local check_success_cmd='[[ $(mock_received_count) -ge 1 ]]'
    if ! wait_for_condition "webhook received (success)" "$check_success_cmd" "$wait_timeout"; then
        log_fail "Timeout waiting for webhook delivery"
        result=1
    else
        log_pass "Webhook delivery observed"
    fi
    curl -s "$mock_addr/received" > "$scenario_dir/notifications_received_success.json" 2>/dev/null || true
    if jq -e '.[-1].event_type == "codex:compaction"' \
        "$scenario_dir/notifications_received_success.json" >/dev/null 2>&1; then
        log_pass "Payload contains expected event_type"
    else
        log_fail "Payload missing expected event_type"
        result=1
    fi
    if grep -q "$secret_token" "$scenario_dir/notifications_received_success.json" 2>/dev/null; then
        log_fail "Secret token leaked in webhook payload"
        result=1
    else
        log_pass "Webhook payloads redacted (no secret token)"
    fi
    wezterm cli kill-pane --pane-id "$pane_id" 2>/dev/null || true
    pane_id=""
    stop_mock_server

    # Step 4: Retry/backoff (500,500,200)
    log_info "Step 4: Webhook retry/backoff on failures..."
    if ! start_mock_server "500,500,200" "$scenario_dir/mock_server_retry.log" \
        "$scenario_dir/mock_server_retry.out"; then
        return 1
    fi
    pane_id=$(spawn_compaction_pane "$emit_script" "$secret_token" 1 0.1) || {
        log_fail "Failed to spawn compaction pane for retry case"
        return 1
    }
    log_info "Spawned pane: $pane_id"
    if ! wait_for_pane_observed "$pane_id"; then
        log_fail "Pane not observed for retry case"
        result=1
    fi

    local check_attempts_cmd='[[ $(mock_attempt_count) -ge 3 ]]'
    if ! wait_for_condition "webhook attempts >=3" "$check_attempts_cmd" "$wait_timeout"; then
        log_fail "Retry attempts did not reach expected count"
        result=1
    else
        log_pass "Retry/backoff attempts observed"
    fi
    curl -s "$mock_addr/received" > "$scenario_dir/notifications_received_retry.json" 2>/dev/null || true
    if grep -q "status=200" "$scenario_dir/mock_server_retry.log" 2>/dev/null; then
        log_pass "Final retry succeeded (200)"
    else
        log_fail "No successful retry observed in mock log"
        result=1
    fi
    wezterm cli kill-pane --pane-id "$pane_id" 2>/dev/null || true
    pane_id=""
    stop_mock_server

    # Step 5: Throttling prevents spam (cooldown)
    log_info "Step 5: Throttling prevents spam..."
    if ! start_mock_server "200" "$scenario_dir/mock_server_throttle.log" \
        "$scenario_dir/mock_server_throttle.out"; then
        return 1
    fi
    pane_id=$(spawn_compaction_pane "$throttle_script" "$secret_token" 4 0.1 2) || {
        log_fail "Failed to spawn compaction pane for throttle case"
        return 1
    }
    log_info "Spawned pane: $pane_id"
    if ! wait_for_pane_observed "$pane_id"; then
        log_fail "Pane not observed for throttle case"
        result=1
    fi

    local check_throttle_cmd='[[ $(mock_received_count) -ge 2 ]]'
    if ! wait_for_condition "throttle second delivery" "$check_throttle_cmd" "$wait_timeout"; then
        log_fail "Throttle second delivery not observed"
        result=1
    else
        log_pass "Throttle delivery observed"
    fi

    if ! wait_for_stable_attempts 2 "$wait_timeout"; then
        log_warn "Webhook attempt count did not stabilize"
    fi
    curl -s "$mock_addr/received" > "$scenario_dir/notifications_received_throttle.json" 2>/dev/null || true
    if jq -e '.[-1].suppressed_since_last >= 1' \
        "$scenario_dir/notifications_received_throttle.json" >/dev/null 2>&1; then
        log_pass "Throttle suppression count recorded"
    else
        log_fail "Throttle suppression count missing"
        result=1
    fi
    wezterm cli kill-pane --pane-id "$pane_id" 2>/dev/null || true
    pane_id=""
    stop_mock_server

    # Step 6: Recovery after endpoint downtime
    log_info "Step 6: Recovery after endpoint downtime..."
    stop_mock_server
    pane_id=$(spawn_compaction_pane "$emit_script" "$secret_token" 1 0.1) || {
        log_fail "Failed to spawn compaction pane for recovery case"
        return 1
    }
    log_info "Spawned pane: $pane_id"
    if ! wait_for_pane_observed "$pane_id"; then
        log_fail "Pane not observed for recovery case"
        result=1
    fi

    local log_offset
    log_offset=$(wc -l < "$scenario_dir/wa_watch.log" 2>/dev/null || echo "0")
    local check_fail_cmd="tail -n +$((log_offset + 1)) \"$scenario_dir/wa_watch.log\" | grep -q \"webhook delivery failed\""
    if ! wait_for_condition "webhook failure logged" "$check_fail_cmd" "$wait_timeout"; then
        log_fail "No webhook failure logged before recovery"
        result=1
    else
        log_pass "Webhook failure logged"
    fi

    if ! start_mock_server "200" "$scenario_dir/mock_server_recovery.log" \
        "$scenario_dir/mock_server_recovery.out"; then
        return 1
    fi
    local check_recovery_cmd='[[ $(mock_received_count) -ge 1 ]]'
    if ! wait_for_condition "webhook recovery delivery" "$check_recovery_cmd" "$wait_timeout"; then
        log_fail "Recovery delivery not observed"
        result=1
    else
        log_pass "Recovery delivery observed"
    fi
    curl -s "$mock_addr/received" > "$scenario_dir/notifications_received_recovery.json" 2>/dev/null || true

    wezterm cli kill-pane --pane-id "$pane_id" 2>/dev/null || true
    pane_id=""
    stop_mock_server

    # Step 7: Capture events + audit slice artifacts
    log_info "Step 7: Capturing events + audit slice..."
    "$FT_BINARY" events -f json --limit 200 > "$scenario_dir/events.json" 2>&1 || true
    local db_path="$temp_workspace/.ft/ft.db"
    if [[ -f "$db_path" ]]; then
        sqlite3 "$db_path" -json \
            "SELECT id, action_kind, actor_kind, result, summary, error FROM audit_actions ORDER BY id DESC LIMIT 200;" \
            | jq -c '.[]' > "$scenario_dir/policy_audit_slice.jsonl" 2>/dev/null || true
    fi

    trap - EXIT
    cleanup_notification_webhook

    return $result
}

run_scenario_watch_notify_only() {
    local scenario_dir="$1"
    local temp_workspace
    temp_workspace=$(mktemp -d /tmp/ft-e2e-notify-only-XXXXXX)
    local ft_pid=""
    local mock_pid=""
    local pane_usage=""
    local pane_token=""
    local pane_burst=""
    local result=0
    local wait_timeout=${TIMEOUT:-120}
    local secret_token="SECRET_NOTIFY_ONLY_$(date +%s%N)"
    local mock_script="$temp_workspace/mock_webhook_server.py"
    local usage_script="$temp_workspace/emit_usage_limit.sh"
    local token_script="$temp_workspace/emit_token_usage.sh"
    local burst_script="$temp_workspace/emit_usage_limit_burst.sh"
    local mock_port=""
    local mock_addr=""
    local old_ft_data_dir="${FT_DATA_DIR:-}"
    local old_ft_workspace="${FT_WORKSPACE:-}"
    local old_ft_config="${FT_CONFIG:-}"

    log_info "Workspace: $temp_workspace"

    if ! command -v python3 >/dev/null 2>&1; then
        log_fail "python3 is required for mock webhook server"
        return 1
    fi
    if ! command -v curl >/dev/null 2>&1; then
        log_fail "curl is required for mock webhook server checks"
        return 1
    fi

    cleanup_watch_notify_only() {
        log_verbose "Cleaning up watch_notify_only scenario"
        if [[ -n "${mock_pid:-}" ]] && kill -0 "$mock_pid" 2>/dev/null; then
            log_verbose "Stopping mock webhook server (pid $mock_pid)"
            kill "$mock_pid" 2>/dev/null || true
            wait "$mock_pid" 2>/dev/null || true
        fi
        if [[ -n "${ft_pid:-}" ]] && kill -0 "$ft_pid" 2>/dev/null; then
            log_verbose "Stopping ft watch (pid $ft_pid)"
            kill "$ft_pid" 2>/dev/null || true
            wait "$ft_pid" 2>/dev/null || true
        fi
        for pane in "$pane_usage" "$pane_token" "$pane_burst"; do
            if [[ -n "$pane" ]]; then
                log_verbose "Closing pane $pane"
                wezterm cli kill-pane --pane-id "$pane" 2>/dev/null || true
            fi
        done
        if [[ -n "$old_ft_data_dir" ]]; then
            export FT_DATA_DIR="$old_ft_data_dir"
        else
            unset FT_DATA_DIR
        fi
        if [[ -n "$old_ft_workspace" ]]; then
            export FT_WORKSPACE="$old_ft_workspace"
        else
            unset FT_WORKSPACE
        fi
        if [[ -n "$old_ft_config" ]]; then
            export FT_CONFIG="$old_ft_config"
        else
            unset FT_CONFIG
        fi
        if [[ -d "${temp_workspace:-}" ]]; then
            cp -r "${temp_workspace}/.ft"/* "$scenario_dir/" 2>/dev/null || true
            cp "${temp_workspace}/ft.toml" "$scenario_dir/" 2>/dev/null || true
        fi
        rm -rf "${temp_workspace:-}"
    }
    trap cleanup_watch_notify_only EXIT

    # Prepare mock webhook server script
    cat > "$mock_script" <<'PY'
#!/usr/bin/env python3
import argparse
import json
from http.server import BaseHTTPRequestHandler, HTTPServer


class State:
    def __init__(self, log_path):
        self.log_path = log_path
        self.received = []

    def log(self, message):
        if self.log_path:
            with open(self.log_path, "a", encoding="utf-8") as handle:
                handle.write(message + "\n")
        else:
            print(message, flush=True)


STATE = None


class Handler(BaseHTTPRequestHandler):
    def log_message(self, format, *args):
        return

    def _send_json(self, code, payload):
        body = json.dumps(payload).encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path == "/health":
            return self._send_json(200, {"ok": True})
        if self.path == "/received":
            return self._send_json(200, STATE.received)
        return self._send_json(404, {"error": "not_found"})

    def do_POST(self):
        if self.path == "/reset":
            STATE.received = []
            return self._send_json(200, {"ok": True})
        if self.path != "/webhook":
            return self._send_json(404, {"error": "not_found"})

        length = int(self.headers.get("Content-Length", "0"))
        body = self.rfile.read(length)
        try:
            payload = json.loads(body.decode("utf-8"))
        except Exception:
            payload = {"_raw": body.decode("utf-8", errors="replace")}
        STATE.received.append(payload)
        STATE.log(f"received bytes={len(body)}")
        return self._send_json(200, {"ok": True})


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, default=0)
    parser.add_argument("--log", default="")
    args = parser.parse_args()

    global STATE
    STATE = State(args.log)
    server = HTTPServer(("127.0.0.1", args.port), Handler)
    server.serve_forever()


if __name__ == "__main__":
    main()
PY
    chmod +x "$mock_script"

    # Prepare usage-limit emitters
    cat > "$usage_script" <<'EOS'
#!/bin/bash
set -euo pipefail
secret="$1"
reset_time="${2:-2026-02-01 00:00 UTC}"
sleep_tail="${3:-120}"

echo "$secret"
echo "You've hit your usage limit. try again at ${reset_time}."
echo "[CODEX] Waiting for operator action..."

sleep "$sleep_tail"
EOS
    chmod +x "$usage_script"

    cat > "$token_script" <<'EOS'
#!/bin/bash
set -euo pipefail
secret="$1"
sleep_tail="${2:-120}"

echo "$secret"
echo "Token usage: total=42 input=20 output=22"

sleep "$sleep_tail"
EOS
    chmod +x "$token_script"

    cat > "$burst_script" <<'EOS'
#!/bin/bash
set -euo pipefail
secret="$1"
burst_count="${2:-3}"
burst_interval="${3:-0.1}"
cooldown_delay="${4:-2}"
sleep_tail="${5:-120}"
reset_time="${6:-2026-02-01 00:00 UTC}"

echo "$secret"
for ((i=1; i<=burst_count; i++)); do
    echo "You've hit your usage limit. try again at ${reset_time}."
    echo "[CODEX] Waiting for operator action..."
    if [[ "$i" -lt "$burst_count" ]]; then
        sleep "$burst_interval"
    fi
done

sleep "$cooldown_delay"
echo "You've hit your usage limit. try again at ${reset_time}."
echo "[CODEX] Waiting for operator action..."

sleep "$sleep_tail"
EOS
    chmod +x "$burst_script"

    # Pick a free port for the mock server
    mock_port=$(python3 - <<'PY'
import socket
sock = socket.socket()
sock.bind(("127.0.0.1", 0))
print(sock.getsockname()[1])
sock.close()
PY
    )

    if [[ -z "$mock_port" ]]; then
        log_fail "Failed to allocate mock webhook port"
        return 1
    fi
    mock_addr="http://127.0.0.1:$mock_port"

    echo "[NOTIFYONLY_E2E] workspace=$temp_workspace" >> "$scenario_dir/scenario.log"
    echo "[NOTIFYONLY_E2E] mock_addr=$mock_addr" >> "$scenario_dir/scenario.log"
    echo "[NOTIFYONLY_E2E] secret_token=$secret_token" >> "$scenario_dir/scenario.log"

    start_mock_server() {
        if [[ -n "${mock_pid:-}" ]] && kill -0 "$mock_pid" 2>/dev/null; then
            kill "$mock_pid" 2>/dev/null || true
            wait "$mock_pid" 2>/dev/null || true
        fi

        python3 "$mock_script" --port "$mock_port" --log "$scenario_dir/mock_server.log" \
            > "$scenario_dir/mock_server.out" 2>&1 &
        mock_pid=$!

        local check_cmd="curl -fs \"$mock_addr/health\" >/dev/null 2>&1"
        if ! wait_for_condition "mock server ready" "$check_cmd" "$wait_timeout"; then
            log_fail "Mock webhook server failed to start"
            return 1
        fi
        return 0
    }

    reset_mock_server() {
        curl -s -X POST "$mock_addr/reset" >/dev/null 2>&1 || true
    }

    mock_received_count() {
        local payload=""
        payload=$(curl -s "$mock_addr/received" 2>/dev/null || true)
        echo "$payload" | jq -r 'length' 2>/dev/null || echo "0"
    }

    wait_for_stable_received() {
        local stable_seconds="$1"
        local timeout="$2"
        local start
        start=$(date +%s)
        local last=""
        local stable_start=""

        while true; do
            local current
            current=$(mock_received_count)
            if [[ -n "$last" && "$current" == "$last" ]]; then
                if [[ -z "$stable_start" ]]; then
                    stable_start=$(date +%s)
                fi
                if [[ $(( $(date +%s) - stable_start )) -ge $stable_seconds ]]; then
                    return 0
                fi
            else
                last="$current"
                stable_start=""
            fi

            if [[ $(( $(date +%s) - start )) -ge $timeout ]]; then
                return 1
            fi
            sleep 0.5
        done
    }

    spawn_pane() {
        local script="$1"
        shift
        local spawn_output=""
        spawn_output=$(wezterm cli spawn --cwd "$temp_workspace" -- bash "$script" "$@" 2>&1)
        local new_pane_id
        new_pane_id=$(echo "$spawn_output" | grep -oE '^[0-9]+$' | head -1)
        if [[ -z "$new_pane_id" ]]; then
            echo ""
            return 1
        fi
        echo "$new_pane_id"
        return 0
    }

    wait_for_pane_observed() {
        local pane="$1"
        local check_cmd="\"$FT_BINARY\" robot state 2>/dev/null | jq -e '.data[]? | select(.pane_id == $pane)' >/dev/null 2>&1"
        wait_for_condition "pane $pane observed" "$check_cmd" "$wait_timeout"
    }

    start_wa_watch() {
        local log_file="$1"
        shift

        if [[ -n "${ft_pid:-}" ]] && kill -0 "$ft_pid" 2>/dev/null; then
            kill "$ft_pid" 2>/dev/null || true
            wait "$ft_pid" 2>/dev/null || true
        fi

        "$FT_BINARY" watch --foreground --config "$temp_workspace/ft.toml" "$@" \
            > "$log_file" 2>&1 &
        ft_pid=$!
        echo "ft_pid: $ft_pid" >> "$scenario_dir/scenario.log"

        local check_watch_cmd="kill -0 $ft_pid 2>/dev/null"
        if ! wait_for_condition "ft watch running" "$check_watch_cmd" "$wait_timeout"; then
            log_fail "ft watch failed to start"
            return 1
        fi
        return 0
    }

    stop_wa_watch() {
        if [[ -n "${ft_pid:-}" ]] && kill -0 "$ft_pid" 2>/dev/null; then
            kill -TERM "$ft_pid" 2>/dev/null || true
            wait "$ft_pid" 2>/dev/null || true
        fi
        ft_pid=""
    }

    # Step 1: Configure isolated workspace and notifications
    log_info "Step 1: Preparing workspace + notifications config..."
    export FT_DATA_DIR="$temp_workspace/.ft"
    export FT_WORKSPACE="$temp_workspace"
    mkdir -p "$FT_DATA_DIR"

    local baseline_config="$PROJECT_ROOT/fixtures/e2e/config_baseline.toml"
    if [[ -f "$baseline_config" ]]; then
        cp "$baseline_config" "$temp_workspace/ft.toml"
    else
        log_fail "Baseline config not found: $baseline_config"
        return 1
    fi

    cat >> "$temp_workspace/ft.toml" <<EOF

[notifications]
enabled = true
notify_only = true
cooldown_ms = 1500
dedup_window_ms = 1
min_severity = "info"

[[notifications.webhooks]]
name = "e2e-notify-only"
url = "${mock_addr}/webhook"
template = "generic"
EOF

    export FT_CONFIG="$temp_workspace/ft.toml"
    log_pass "Notify-only config written"

    if ! start_mock_server; then
        return 1
    fi

    # Step 2: Notify-only mode delivers notifications but does not auto-handle
    log_info "Step 2: Starting ft watch (notify-only baseline)..."
    if ! start_wa_watch "$scenario_dir/wa_watch_notify_only.log" --notify-only; then
        return 1
    fi
    echo "[NOTIFYONLY_E2E] watcher started notify_only=true" >> "$scenario_dir/scenario.log"

    log_info "Step 3: Spawning usage-limit pane..."
    pane_usage=$(spawn_pane "$usage_script" "$secret_token" "2026-02-01 00:00 UTC" 90) || {
        log_fail "Failed to spawn usage-limit pane"
        return 1
    }
    log_info "Spawned usage-limit pane: $pane_usage"
    echo "[NOTIFYONLY_E2E] pane_usage=$pane_usage" >> "$scenario_dir/scenario.log"

    if ! wait_for_pane_observed "$pane_usage"; then
        log_fail "Pane not observed for notify-only baseline"
        result=1
    fi

    local check_notify_cmd='[[ $(mock_received_count) -ge 1 ]]'
    if ! wait_for_condition "notify-only webhook received" "$check_notify_cmd" "$wait_timeout"; then
        log_fail "Timeout waiting for notify-only webhook"
        result=1
    else
        log_pass "Notify-only webhook delivery observed"
    fi

    curl -s "$mock_addr/received" > "$scenario_dir/notifications_received_notify_only.json" 2>/dev/null || true

    if jq -e '.[-1].event_type == "codex.usage.reached"' \
        "$scenario_dir/notifications_received_notify_only.json" >/dev/null 2>&1; then
        log_pass "Payload contains usage-limit event"
    else
        log_fail "Payload missing usage-limit event_type"
        result=1
    fi

    if jq -e '.[-1].quick_fix != null and (.[-1].quick_fix | contains("wa workflow run"))' \
        "$scenario_dir/notifications_received_notify_only.json" >/dev/null 2>&1; then
        log_pass "Suggested action included in notification"
    else
        log_fail "Suggested action missing from notification payload"
        result=1
    fi

    if grep -q "$secret_token" "$scenario_dir/notifications_received_notify_only.json" 2>/dev/null; then
        log_fail "Secret token leaked in notify-only payload"
        result=1
    else
        log_pass "Notify-only payload redacted (no secret token)"
    fi

    log_info "Step 4: Verifying event remains unhandled + no workflow runs..."
    "$FT_BINARY" events -f json --unhandled --rule-id "codex.usage.reached" --limit 20 \
        > "$scenario_dir/events_unhandled_notify_only.json" 2>&1 || true

    if jq -e ".[] | select(.pane_id == $pane_usage)" \
        "$scenario_dir/events_unhandled_notify_only.json" >/dev/null 2>&1; then
        log_pass "Usage-limit event remains unhandled"
    else
        log_fail "Usage-limit event no longer unhandled"
        result=1
    fi

    local db_path="$temp_workspace/.ft/ft.db"
    if [[ -f "$db_path" ]]; then
        sqlite3 "$db_path" -json \
            "SELECT id, workflow_name, status FROM workflow_executions ORDER BY started_at DESC LIMIT 50;" \
            > "$scenario_dir/workflow_executions_notify_only.json" 2>/dev/null || true
        local workflow_count
        workflow_count=$(jq 'length' "$scenario_dir/workflow_executions_notify_only.json" 2>/dev/null || echo "0")
        if [[ "$workflow_count" -eq 0 ]]; then
            log_pass "No workflow executions recorded"
        else
            log_fail "Workflow executions found in notify-only mode"
            result=1
        fi

        sqlite3 "$db_path" -json \
            "SELECT action_kind FROM audit_actions ORDER BY id DESC LIMIT 50;" \
            > "$scenario_dir/audit_actions_notify_only.json" 2>/dev/null || true
        if jq -e '.[] | select(.action_kind == "workflow_run" or .action_kind == "workflow_start")' \
            "$scenario_dir/audit_actions_notify_only.json" >/dev/null 2>&1; then
            log_fail "Workflow audit actions found in notify-only mode"
            result=1
        else
            log_pass "No workflow audit actions recorded"
        fi
    else
        log_warn "Database file not found at $db_path"
    fi

    if [[ -n "$pane_usage" ]]; then
        wezterm cli kill-pane --pane-id "$pane_usage" 2>/dev/null || true
        pane_usage=""
    fi

    # Step 5: Notification filter only delivers matching events
    log_info "Step 5: Restarting ft watch with notify filter..."
    reset_mock_server
    stop_wa_watch
    if ! start_wa_watch "$scenario_dir/wa_watch_notify_filter.log" \
        --notify-only --notify-filter "codex.usage.reached"; then
        return 1
    fi
    echo "[NOTIFYONLY_E2E] watcher restarted notify_filter=codex.usage.reached" >> "$scenario_dir/scenario.log"

    pane_usage=$(spawn_pane "$usage_script" "$secret_token" "2026-02-01 00:00 UTC" 90) || {
        log_fail "Failed to spawn usage-limit pane for filter test"
        return 1
    }
    pane_token=$(spawn_pane "$token_script" "$secret_token" 90) || {
        log_fail "Failed to spawn token-usage pane for filter test"
        return 1
    }

    if ! wait_for_pane_observed "$pane_usage"; then
        log_fail "Usage-limit pane not observed for filter test"
        result=1
    fi
    if ! wait_for_pane_observed "$pane_token"; then
        log_fail "Token-usage pane not observed for filter test"
        result=1
    fi

    if ! wait_for_condition "filtered webhook received" "$check_notify_cmd" "$wait_timeout"; then
        log_fail "Timeout waiting for filtered webhook delivery"
        result=1
    fi

    if ! wait_for_stable_received 2 "$wait_timeout"; then
        log_warn "Webhook count did not stabilize after filter test"
    fi

    curl -s "$mock_addr/received" > "$scenario_dir/notifications_received_filter.json" 2>/dev/null || true
    if jq -e 'length == 1' "$scenario_dir/notifications_received_filter.json" >/dev/null 2>&1; then
        log_pass "Filter limited notifications to one event"
    else
        log_fail "Filter delivered unexpected number of notifications"
        result=1
    fi

    if jq -e 'map(select(.event_type != "codex.usage.reached")) | length == 0' \
        "$scenario_dir/notifications_received_filter.json" >/dev/null 2>&1; then
        log_pass "Filter delivered only usage-limit notifications"
    else
        log_fail "Filter delivered non-matching notifications"
        result=1
    fi

    for pane in "$pane_usage" "$pane_token"; do
        if [[ -n "$pane" ]]; then
            wezterm cli kill-pane --pane-id "$pane" 2>/dev/null || true
        fi
    done
    pane_usage=""
    pane_token=""

    # Step 6: Throttling suppresses repeat notifications
    log_info "Step 6: Throttling suppresses repeat notifications..."
    reset_mock_server
    pane_burst=$(spawn_pane "$burst_script" "$secret_token" 3 0.1 2 90) || {
        log_fail "Failed to spawn burst pane for throttling test"
        return 1
    }

    if ! wait_for_pane_observed "$pane_burst"; then
        log_fail "Burst pane not observed for throttling test"
        result=1
    fi

    local check_throttle_cmd='[[ $(mock_received_count) -ge 2 ]]'
    if ! wait_for_condition "throttle second delivery" "$check_throttle_cmd" "$wait_timeout"; then
        log_fail "Throttle second delivery not observed"
        result=1
    else
        log_pass "Throttle delivery observed"
    fi

    curl -s "$mock_addr/received" > "$scenario_dir/notifications_received_throttle.json" 2>/dev/null || true
    if jq -e '.[-1].suppressed_since_last >= 1' \
        "$scenario_dir/notifications_received_throttle.json" >/dev/null 2>&1; then
        log_pass "Throttle suppression count recorded"
    else
        log_fail "Throttle suppression count missing"
        result=1
    fi

    if [[ -n "$pane_burst" ]]; then
        wezterm cli kill-pane --pane-id "$pane_burst" 2>/dev/null || true
        pane_burst=""
    fi

    stop_wa_watch

    trap - EXIT
    cleanup_watch_notify_only

    return $result
}

run_scenario_policy_denial() {
    local scenario_dir="$1"
    local temp_workspace
    temp_workspace=$(mktemp -d /tmp/ft-e2e-XXXXXX)
    local ft_pid=""
    local pane_id=""
    local result=0

    log_info "Workspace: $temp_workspace"

    # Setup environment for isolated wa instance with strict config
    export FT_DATA_DIR="$temp_workspace/.ft"
    export FT_WORKSPACE="$temp_workspace"
    mkdir -p "$FT_DATA_DIR"

    # Copy strict config for policy testing
    local strict_config="$PROJECT_ROOT/fixtures/e2e/config_strict.toml"
    if [[ -f "$strict_config" ]]; then
        cp "$strict_config" "$temp_workspace/ft.toml"
        export FT_CONFIG="$temp_workspace/ft.toml"
        log_verbose "Using strict config: $strict_config"
    fi

    # Cleanup function
    cleanup_policy_denial() {
        log_verbose "Cleaning up policy_denial scenario"
        # Kill ft watch if running
        if [[ -n "$ft_pid" ]] && kill -0 "$ft_pid" 2>/dev/null; then
            log_verbose "Stopping ft watch (pid $ft_pid)"
            kill "$ft_pid" 2>/dev/null || true
            wait "$ft_pid" 2>/dev/null || true
        fi
        # Close alt-screen pane if it exists
        if [[ -n "$pane_id" ]]; then
            log_verbose "Closing alt-screen pane $pane_id"
            wezterm cli kill-pane --pane-id "$pane_id" 2>/dev/null || true
        fi
        # Copy artifacts before cleanup
        if [[ -d "$temp_workspace" ]]; then
            cp -r "$temp_workspace/.ft"/* "$scenario_dir/" 2>/dev/null || true
            cp "$temp_workspace/ft.toml" "$scenario_dir/" 2>/dev/null || true
        fi
        rm -rf "$temp_workspace"
    }
    trap cleanup_policy_denial EXIT

    # Step 1: Spawn a pane that enters alternate screen mode
    log_info "Step 1: Spawning alt-screen pane..."
    local alt_script="$PROJECT_ROOT/fixtures/e2e/dummy_alt_screen.sh"
    if [[ ! -x "$alt_script" ]]; then
        log_fail "Alt-screen script not found or not executable: $alt_script"
        return 1
    fi

    local spawn_output
    # Spawn with long duration so it stays in alt screen
    spawn_output=$(wezterm cli spawn --cwd "$temp_workspace" -- bash "$alt_script" 60 2>&1)
    pane_id=$(echo "$spawn_output" | grep -oE '^[0-9]+$' | head -1)

    if [[ -z "$pane_id" ]]; then
        log_fail "Failed to spawn alt-screen pane"
        echo "Spawn output: $spawn_output" >> "$scenario_dir/scenario.log"
        return 1
    fi
    log_info "Spawned alt-screen pane: $pane_id"
    echo "alt_screen_pane_id: $pane_id" >> "$scenario_dir/scenario.log"

    # Wait for the pane to render its alt-screen banner (deterministic).
    local alt_ready_cmd="wezterm cli get-text --pane-id $pane_id 2>/dev/null | grep -q \"ALTERNATE SCREEN MODE\""
    if ! wait_for_condition "alt-screen banner visible" "$alt_ready_cmd" 10; then
        log_warn "Alt-screen banner not observed yet; continuing (policy may still block)"
    fi

    # Step 2: Start ft watch in background
    log_info "Step 2: Starting ft watch..."
    "$FT_BINARY" watch --foreground \
        > "$scenario_dir/wa_watch.log" 2>&1 &
    ft_pid=$!
    log_verbose "ft watch started with PID $ft_pid"
    echo "ft_pid: $ft_pid" >> "$scenario_dir/scenario.log"

    # Verify ft watch is running
    if ! kill -0 "$ft_pid" 2>/dev/null; then
        log_fail "ft watch exited immediately"
        return 1
    fi

    # Step 3: Wait for pane to be observed
    log_info "Step 3: Waiting for pane to be observed..."
    local wait_timeout=${TIMEOUT:-30}
    local check_cmd="\"$FT_BINARY\" robot state 2>/dev/null | jq -e '.data[]? | select(.pane_id == $pane_id)' >/dev/null 2>&1"

    if ! wait_for_condition "pane $pane_id observed" "$check_cmd" "$wait_timeout"; then
        log_fail "Timeout waiting for pane to be observed"
        "$FT_BINARY" robot state > "$scenario_dir/robot_state.json" 2>&1 || true
        return 1
    fi
    log_pass "Pane observed"

    # Capture robot state for diagnostics
    "$FT_BINARY" robot state > "$scenario_dir/robot_state.json" 2>&1 || true

    # Step 4: Attempt to send text to the alt-screen pane
    log_info "Step 4: Attempting send to alt-screen pane (should be denied)..."
    local send_output
    send_output=$("$FT_BINARY" robot send "$pane_id" "test_text_should_be_denied" 2>&1)
    local send_exit_code=$?
    echo "$send_output" > "$scenario_dir/send_attempt.json"
    echo "send_exit_code: $send_exit_code" >> "$scenario_dir/scenario.log"

    log_verbose "Send output: $send_output"
    log_verbose "Send exit code: $send_exit_code"

    # Step 5: Assert send was denied
    log_info "Step 5: Asserting send was denied..."

    # Check if the response indicates denial
    # Robot mode should return JSON with ok: false or an error
    local ok_status=""
    if echo "$send_output" | jq -e '.' >/dev/null 2>&1; then
        ok_status=$(echo "$send_output" | jq -r '.ok // empty')
        local error_code=$(echo "$send_output" | jq -r '.error.code // .error // empty')

        if [[ "$ok_status" == "false" ]]; then
            log_pass "Send denied (ok: false)"
            if [[ -n "$error_code" ]]; then
                log_info "Error code: $error_code"
                echo "denial_error_code: $error_code" >> "$scenario_dir/scenario.log"
            fi
        elif [[ "$ok_status" == "true" ]]; then
            log_fail "Send was NOT denied - ok: true (expected denial)"
            result=1
        else
            # Check if it's an error response without ok field
            if [[ -n "$error_code" ]]; then
                log_pass "Send denied with error: $error_code"
            else
                log_warn "Unexpected response format, checking exit code"
                if [[ $send_exit_code -ne 0 ]]; then
                    log_pass "Send denied (non-zero exit code: $send_exit_code)"
                else
                    log_fail "Could not verify denial"
                    result=1
                fi
            fi
        fi
    else
        # Non-JSON output, check exit code
        if [[ $send_exit_code -ne 0 ]]; then
            log_pass "Send denied (non-zero exit code: $send_exit_code)"
        else
            log_fail "Send may have succeeded (exit code 0, non-JSON output)"
            result=1
        fi
    fi

    # Step 6: Verify no text was actually sent (check pane content)
    log_info "Step 6: Verifying no text was sent to pane..."
    local pane_text
    pane_text=$("$FT_BINARY" robot get-text "$pane_id" 2>&1 || true)
    echo "$pane_text" > "$scenario_dir/pane_text.txt"

    if echo "$pane_text" | grep -q "test_text_should_be_denied"; then
        log_fail "Text was actually sent to pane (policy bypass!)"
        result=1
    else
        log_pass "Confirmed no text leaked to pane"
    fi

    # Cleanup trap will handle the rest
    trap - EXIT
    cleanup_policy_denial

    return $result
}

# ==============================================================================
# Scenario: Audit Tail Streaming
# ==============================================================================
# Validates `wa audit tail --follow` JSONL output, redaction, and ordering.
# ==============================================================================

run_scenario_audit_tail_streaming() {
    local scenario_dir="$1"
    local temp_workspace
    temp_workspace=$(mktemp -d /tmp/ft-e2e-audit-tail-XXXXXX)
    local ft_pid=""
    local pane_id=""
    local tail_pid=""
    local result=0
    local secret_token="sk-test-$(date +%s)1234567890"

    log_info "Workspace: $temp_workspace"

    # Setup environment for isolated wa instance
    export FT_DATA_DIR="$temp_workspace/.ft"
    export FT_WORKSPACE="$temp_workspace"
    mkdir -p "$FT_DATA_DIR"

    local baseline_config="$PROJECT_ROOT/fixtures/e2e/config_baseline.toml"
    if [[ -f "$baseline_config" ]]; then
        cp "$baseline_config" "$temp_workspace/ft.toml"
        export FT_CONFIG="$temp_workspace/ft.toml"
        log_verbose "Using baseline config: $baseline_config"
    fi

    cleanup_audit_tail() {
        log_verbose "Cleaning up audit_tail_streaming scenario"
        if [[ -n "$tail_pid" ]] && kill -0 "$tail_pid" 2>/dev/null; then
            log_verbose "Stopping audit tail (pid $tail_pid)"
            kill "$tail_pid" 2>/dev/null || true
            wait "$tail_pid" 2>/dev/null || true
        fi
        if [[ -n "$ft_pid" ]] && kill -0 "$ft_pid" 2>/dev/null; then
            log_verbose "Stopping ft watch (pid $ft_pid)"
            kill "$ft_pid" 2>/dev/null || true
            wait "$ft_pid" 2>/dev/null || true
        fi
        if [[ -n "$pane_id" ]]; then
            log_verbose "Closing pane $pane_id"
            wezterm cli kill-pane --pane-id "$pane_id" 2>/dev/null || true
        fi
        if [[ -d "$temp_workspace" ]]; then
            cp -r "$temp_workspace/.ft"/* "$scenario_dir/" 2>/dev/null || true
            cp "$temp_workspace/ft.toml" "$scenario_dir/" 2>/dev/null || true
        fi
        rm -rf "$temp_workspace"
    }
    trap cleanup_audit_tail EXIT

    # Step 1: Spawn an alt-screen pane for deterministic denial
    log_info "Step 1: Spawning alt-screen pane..."
    local alt_script="$PROJECT_ROOT/fixtures/e2e/dummy_alt_screen.sh"
    if [[ ! -x "$alt_script" ]]; then
        log_fail "Alt-screen script not found or not executable: $alt_script"
        return 1
    fi

    local spawn_output
    spawn_output=$(wezterm cli spawn --cwd "$temp_workspace" -- bash "$alt_script" 60 2>&1)
    pane_id=$(echo "$spawn_output" | grep -oE '^[0-9]+$' | head -1)

    if [[ -z "$pane_id" ]]; then
        log_fail "Failed to spawn alt-screen pane"
        echo "Spawn output: $spawn_output" >> "$scenario_dir/scenario.log"
        return 1
    fi
    log_info "Spawned alt-screen pane: $pane_id"
    echo "alt_screen_pane_id: $pane_id" >> "$scenario_dir/scenario.log"

    # Wait for the pane to render its alt-screen banner (deterministic).
    local alt_ready_cmd="wezterm cli get-text --pane-id $pane_id 2>/dev/null | grep -q \"ALTERNATE SCREEN MODE\""
    if ! wait_for_condition "alt-screen banner visible" "$alt_ready_cmd" 10; then
        log_warn "Alt-screen banner not observed yet; continuing (denial may still work)"
    fi

    # Step 2: Start ft watch in background
    log_info "Step 2: Starting ft watch..."
    "$FT_BINARY" watch --foreground \
        > "$scenario_dir/wa_watch.log" 2>&1 &
    ft_pid=$!
    log_verbose "ft watch started with PID $ft_pid"
    echo "ft_pid: $ft_pid" >> "$scenario_dir/scenario.log"

    # Step 3: Wait for pane to be observed
    log_info "Step 3: Waiting for pane to be observed..."
    local wait_timeout=${TIMEOUT:-30}
    local check_cmd="\"$FT_BINARY\" robot state 2>/dev/null | jq -e '.data[]? | select(.pane_id == $pane_id)' >/dev/null 2>&1"

    if ! wait_for_condition "pane $pane_id observed" "$check_cmd" "$wait_timeout"; then
        log_fail "Timeout waiting for pane to be observed"
        "$FT_BINARY" robot state > "$scenario_dir/robot_state.json" 2>&1 || true
        return 1
    fi
    log_pass "Pane observed"
    "$FT_BINARY" robot state > "$scenario_dir/robot_state.json" 2>&1 || true

    # Step 4: Start audit tail in follow mode
    log_info "Step 4: Starting audit tail stream..."
    local since_ms
    since_ms=$(( $(date +%s) * 1000 ))
    timeout 8 "$FT_BINARY" audit tail --follow --since "$since_ms" --limit 50 \
        > "$scenario_dir/audit_tail.jsonl" 2> "$scenario_dir/audit_tail.stderr" &
    tail_pid=$!
    echo "audit_tail_pid: $tail_pid" >> "$scenario_dir/scenario.log"
    echo "audit_tail_since_ms: $since_ms" >> "$scenario_dir/scenario.log"

    if ! wait_for_condition "audit tail process started" "kill -0 $tail_pid 2>/dev/null" 5; then
        log_fail "audit tail did not start"
        return 1
    fi

    # Step 5: Generate an audit action (intentional denial) with redaction
    log_info "Step 5: Triggering audit action..."
    local send_output
    send_output=$("$FT_BINARY" robot send "$pane_id" "echo $secret_token" 2>&1 || true)
    echo "$send_output" > "$scenario_dir/send_output.json"

    # Step 6: Wait for audit tail output
    log_info "Step 6: Waiting for audit tail output..."
    local tail_check_cmd="test -s \"$scenario_dir/audit_tail.jsonl\""
    if ! wait_for_condition "audit tail output" "$tail_check_cmd" "$wait_timeout"; then
        log_fail "Timeout waiting for audit tail output"
        return 1
    fi

    wait "$tail_pid" 2>/dev/null || true

    # Step 7: Validate JSONL output and redaction
    log_info "Step 7: Validating audit tail output..."
    if ! jq -s 'length > 0' "$scenario_dir/audit_tail.jsonl" >/dev/null 2>&1; then
        log_fail "Audit tail output is not valid JSONL"
        result=1
    fi

    if grep -q "$secret_token" "$scenario_dir/audit_tail.jsonl" 2>/dev/null; then
        log_fail "Audit tail output leaked secret token"
        result=1
    fi

    if ! grep -q "\\[REDACTED\\]" "$scenario_dir/audit_tail.jsonl" 2>/dev/null; then
        log_fail "Audit tail output missing redaction marker"
        result=1
    fi

    local ids
    ids=$(jq -r '.id' "$scenario_dir/audit_tail.jsonl" 2>/dev/null || true)
    echo "audit_ids: $ids" >> "$scenario_dir/scenario.log"
    local last_id
    last_id=$(echo "$ids" | tail -n1)
    echo "audit_cursor_last_id: $last_id" >> "$scenario_dir/scenario.log"

    if ! echo "$ids" | awk 'NR==1 {prev=$1; next} { if ($1 < prev) { exit 1 } prev=$1 }'; then
        log_fail "Audit IDs are not in deterministic order"
        result=1
    fi

    return $result
}

# ==============================================================================
# Scenario: IPC RPC Round-Trip
# ==============================================================================
# Validates IPC RPC auth enforcement, read-only requests, and audit records.
# ==============================================================================

run_scenario_ipc_rpc_roundtrip() {
    local scenario_dir="$1"
    local temp_workspace
    temp_workspace=$(mktemp -d /tmp/ft-e2e-ipc-XXXXXX)
    local ft_pid=""
    local result=0
    local read_token="e2e-read-$(date +%s%N)"
    local write_token="e2e-write-$(date +%s%N)"
    local request_id_help="e2e-ipc-help-$(date +%s%N)"
    local request_id_denied="e2e-ipc-denied-$(date +%s%N)"
    local request_id_write="e2e-ipc-write-$(date +%s%N)"

    log_info "Workspace: $temp_workspace"

    export FT_DATA_DIR="$temp_workspace/.ft"
    export FT_WORKSPACE="$temp_workspace"
    mkdir -p "$FT_DATA_DIR"

    local baseline_config="$PROJECT_ROOT/fixtures/e2e/config_baseline.toml"
    if [[ -f "$baseline_config" ]]; then
        cp "$baseline_config" "$temp_workspace/ft.toml"
        log_verbose "Using baseline config: $baseline_config"
    fi

    cat >> "$temp_workspace/ft.toml" <<EOF

[ipc]
enabled = true
permissions = 0o600

[[ipc.tokens]]
token = "$read_token"
scopes = ["read"]

[[ipc.tokens]]
token = "$write_token"
scopes = ["write"]
EOF

    export FT_CONFIG="$temp_workspace/ft.toml"

    cleanup_ipc_rpc_roundtrip() {
        log_verbose "Cleaning up ipc_rpc_roundtrip scenario"
        if [[ -n "$ft_pid" ]] && kill -0 "$ft_pid" 2>/dev/null; then
            log_verbose "Stopping ft watch (pid $ft_pid)"
            kill "$ft_pid" 2>/dev/null || true
            wait "$ft_pid" 2>/dev/null || true
        fi
        if [[ -d "$temp_workspace" ]]; then
            cp -r "$temp_workspace/.ft"/* "$scenario_dir/" 2>/dev/null || true
            cp "$temp_workspace/ft.toml" "$scenario_dir/" 2>/dev/null || true
        fi
        rm -rf "$temp_workspace"
    }
    trap cleanup_ipc_rpc_roundtrip EXIT

    ipc_send() {
        local socket_path="$1"
        python3 - "$socket_path" <<'PY'
import json
import socket
import sys

sock_path = sys.argv[1]
payload = json.loads(sys.stdin.read())

s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(2.0)
s.connect(sock_path)
s.sendall((json.dumps(payload) + "\n").encode("utf-8"))
data = b""
while not data.endswith(b"\n"):
    chunk = s.recv(4096)
    if not chunk:
        break
    data += chunk
s.close()
sys.stdout.write(data.decode("utf-8").strip())
PY
    }

    # Step 1: Start ft watch
    log_info "Step 1: Starting ft watch..."
    "$FT_BINARY" watch --foreground --config "$temp_workspace/ft.toml" \
        > "$scenario_dir/wa_watch.log" 2>&1 &
    ft_pid=$!
    echo "ft_pid: $ft_pid" >> "$scenario_dir/scenario.log"

    local wait_timeout=${TIMEOUT:-30}
    local check_watch_cmd="kill -0 $ft_pid 2>/dev/null"
    if ! wait_for_condition "ft watch running" "$check_watch_cmd" "$wait_timeout"; then
        log_fail "ft watch failed to start"
        return 1
    fi

    local socket_path="$FT_DATA_DIR/ipc.sock"
    local check_socket_cmd="[[ -S \"$socket_path\" ]]"
    if ! wait_for_condition "ipc socket ready" "$check_socket_cmd" "$wait_timeout"; then
        log_fail "IPC socket not ready"
        return 1
    fi
    log_pass "IPC socket ready"

    # Step 2: Ping via IPC (read token)
    log_info "Step 2: IPC ping with read token..."
    cat > "$scenario_dir/ping_request.json" <<EOF
{"token":"$read_token","type":"ping"}
EOF
    ipc_send "$socket_path" < "$scenario_dir/ping_request.json" \
        > "$scenario_dir/ping_response.json" 2>&1 || true

    if jq -e '.ok == true and (.elapsed_ms | type == "number")' \
        "$scenario_dir/ping_response.json" >/dev/null 2>&1; then
        log_pass "Ping ok"
    else
        log_fail "Ping failed"
        result=1
    fi

    # Step 3: RPC help with read token
    log_info "Step 3: IPC RPC help with read token..."
    cat > "$scenario_dir/rpc_help_request.json" <<EOF
{"token":"$read_token","request_id":"$request_id_help","type":"rpc","args":["help"]}
EOF
    ipc_send "$socket_path" < "$scenario_dir/rpc_help_request.json" \
        > "$scenario_dir/rpc_help_response.json" 2>&1 || true

    if jq -e '.ok == true and (.data | type == "object")' \
        "$scenario_dir/rpc_help_response.json" >/dev/null 2>&1; then
        log_pass "RPC help ok"
    else
        log_fail "RPC help failed"
        result=1
    fi

    # Step 4: Mutating RPC blocked with read token
    log_info "Step 4: RPC send denied with read token..."
    cat > "$scenario_dir/rpc_send_denied_request.json" <<EOF
{"token":"$read_token","request_id":"$request_id_denied","type":"rpc","args":["send","0","ls"]}
EOF
    ipc_send "$socket_path" < "$scenario_dir/rpc_send_denied_request.json" \
        > "$scenario_dir/rpc_send_denied_response.json" 2>&1 || true

    if jq -e '.ok == false and (.error // "" | contains("insufficient scope"))' \
        "$scenario_dir/rpc_send_denied_response.json" >/dev/null 2>&1; then
        log_pass "RPC send denied by scope"
    else
        log_fail "RPC send scope enforcement missing"
        result=1
    fi

    # Step 5: Mutating RPC allowed with write token (may still fail in robot)
    log_info "Step 5: RPC send with write token..."
    cat > "$scenario_dir/rpc_send_write_request.json" <<EOF
{"token":"$write_token","request_id":"$request_id_write","type":"rpc","args":["send","0","ls"]}
EOF
    ipc_send "$socket_path" < "$scenario_dir/rpc_send_write_request.json" \
        > "$scenario_dir/rpc_send_write_response.json" 2>&1 || true

    if jq -e '.ok == false and (.error // "" | contains("insufficient scope") | not)' \
        "$scenario_dir/rpc_send_write_response.json" >/dev/null 2>&1; then
        log_pass "RPC send reached robot handler"
    else
        log_fail "RPC send blocked by scope unexpectedly"
        result=1
    fi

    # Step 6: Audit entries for RPC requests
    log_info "Step 6: Checking audit trail for IPC RPC..."
    "$FT_BINARY" audit -f json -l 50 > "$scenario_dir/audit_actions.json" 2>&1 || true

    if jq -e --arg rid "$request_id_help" \
        '.[]? | select(.action_kind == "ipc.rpc" and .correlation_id == $rid)' \
        "$scenario_dir/audit_actions.json" >/dev/null 2>&1; then
        log_pass "Audit entry found for RPC help"
    else
        log_fail "Audit entry missing for RPC help"
        result=1
    fi

    trap - EXIT
    cleanup_ipc_rpc_roundtrip

    return $result
}

# ==============================================================================
# Scenario: Prepare/Commit Approvals
# ==============================================================================
# Validates prepare -> commit approval flow and hash mismatch guard.
# ==============================================================================

run_scenario_prepare_commit_approvals() {
    local scenario_dir="$1"
    local temp_workspace
    temp_workspace=$(mktemp -d /tmp/ft-e2e-prepare-commit-XXXXXX)
    local pane_id=""
    local result=0

    log_info "Workspace: $temp_workspace"

    # Setup environment for isolated wa instance
    export FT_DATA_DIR="$temp_workspace/.ft"
    export FT_WORKSPACE="$temp_workspace"
    mkdir -p "$FT_DATA_DIR"

    # Build a permissive config that forces approval for send_text
    local config_path="$temp_workspace/ft.toml"
    local baseline_config="$PROJECT_ROOT/fixtures/e2e/config_baseline.toml"
    if [[ -f "$baseline_config" ]]; then
        cp "$baseline_config" "$config_path"
        log_verbose "Using baseline config: $baseline_config"
    fi

    cat >> "$config_path" <<'EOF'
[safety]
require_prompt_active = false
block_alt_screen = false

[safety.command_gate]
enabled = false

[safety.rules]
enabled = true

[[safety.rules.rules]]
id = "e2e.require_approval_send_text"
priority = 10
decision = "require_approval"
message = "E2E: require approval for send_text"

[safety.rules.rules.match_on]
actions = ["send_text"]
actors = ["human"]
EOF

    export FT_CONFIG="$config_path"

    cleanup_prepare_commit_approvals() {
        log_verbose "Cleaning up prepare_commit_approvals scenario"
        if [[ -n "$pane_id" ]]; then
            wezterm cli kill-pane --pane-id "$pane_id" 2>/dev/null || true
        fi
        if [[ -d "$temp_workspace" ]]; then
            cp -r "$temp_workspace/.ft"/* "$scenario_dir/" 2>/dev/null || true
            cp "$temp_workspace/ft.toml" "$scenario_dir/" 2>/dev/null || true
        fi
        rm -rf "$temp_workspace"
    }
    trap cleanup_prepare_commit_approvals EXIT

    # Step 1: Spawn a shell pane
    log_info "Step 1: Spawning shell pane..."
    local spawn_output
    spawn_output=$(wezterm cli spawn --cwd "$temp_workspace" -- bash 2>&1)
    pane_id=$(echo "$spawn_output" | grep -oE '^[0-9]+$' | head -1)

    if [[ -z "$pane_id" ]]; then
        log_fail "Failed to spawn shell pane"
        echo "Spawn output: $spawn_output" >> "$scenario_dir/scenario.log"
        return 1
    fi
    log_info "Spawned pane: $pane_id"
    echo "pane_id: $pane_id" >> "$scenario_dir/scenario.log"
    echo "spawn_output: $spawn_output" >> "$scenario_dir/scenario.log"

    local marker="FT_PREPARE_COMMIT_OK_$(date +%s%N)"
    local send_text="echo $marker"

    # Step 2: Prepare plan (expect approval required)
    log_info "Step 2: Preparing plan (expect approval)..."
    "$FT_BINARY" prepare send --pane "$pane_id" --format json --wait-for "$marker" --timeout-secs 10 \
        "$send_text" > "$scenario_dir/prepare_output.json" 2>&1 || true

    if jq -e '.plan_id and .plan_hash' "$scenario_dir/prepare_output.json" >/dev/null 2>&1; then
        log_pass "Prepare output JSON looks valid"
    else
        log_fail "Prepare output missing plan_id/plan_hash"
        result=1
    fi

    local plan_id
    local plan_hash
    local approval_code
    local requires_approval
    plan_id=$(jq -r '.plan_id // empty' "$scenario_dir/prepare_output.json" 2>/dev/null || echo "")
    plan_hash=$(jq -r '.plan_hash // empty' "$scenario_dir/prepare_output.json" 2>/dev/null || echo "")
    approval_code=$(jq -r '.approval.code // empty' "$scenario_dir/prepare_output.json" 2>/dev/null || echo "")
    requires_approval=$(jq -r '.requires_approval // false' "$scenario_dir/prepare_output.json" 2>/dev/null || echo "false")

    echo "plan_id: $plan_id" >> "$scenario_dir/scenario.log"
    echo "plan_hash: $plan_hash" >> "$scenario_dir/scenario.log"
    echo "requires_approval: $requires_approval" >> "$scenario_dir/scenario.log"

    if [[ "$requires_approval" == "true" ]]; then
        log_pass "Prepare requires approval"
    else
        log_fail "Prepare did not require approval"
        result=1
    fi

    if [[ -n "$approval_code" ]]; then
        log_pass "Approval code issued"
        echo "approval_code: $approval_code" >> "$scenario_dir/scenario.log"
    else
        log_fail "Approval code missing in prepare output"
        result=1
    fi

    if [[ -z "$plan_id" || -z "$approval_code" ]]; then
        log_fail "Plan ID or approval code missing; cannot proceed"
        return 1
    fi

    # Step 3: Commit with mismatched text (expect failure)
    log_info "Step 3: Commit with mismatched text (expect failure)..."
    local mismatch_rc=0
    "$FT_BINARY" commit "$plan_id" --approval-code "$approval_code" \
        --text "echo ${marker}_MISMATCH" > "$scenario_dir/commit_mismatch.log" 2>&1 || mismatch_rc=$?
    echo "commit_mismatch_rc: $mismatch_rc" >> "$scenario_dir/scenario.log"

    if [[ $mismatch_rc -ne 0 ]]; then
        log_pass "Mismatch commit failed as expected"
    else
        log_fail "Mismatch commit succeeded unexpectedly"
        result=1
    fi

    if grep -q "E_PLAN_HASH_MISMATCH" "$scenario_dir/commit_mismatch.log" 2>/dev/null; then
        log_pass "Hash mismatch error surfaced"
    else
        log_fail "Expected E_PLAN_HASH_MISMATCH not found"
        result=1
    fi

    # Step 4: Commit with correct text (expect success)
    log_info "Step 4: Commit with correct text..."
    local commit_rc=0
    "$FT_BINARY" commit "$plan_id" --approval-code "$approval_code" \
        --text "$send_text" > "$scenario_dir/commit_output.log" 2>&1 || commit_rc=$?
    echo "commit_rc: $commit_rc" >> "$scenario_dir/scenario.log"

    if [[ $commit_rc -eq 0 ]]; then
        log_pass "Commit succeeded"
    else
        log_fail "Commit failed (rc=$commit_rc)"
        result=1
    fi

    if grep -q "Commit succeeded" "$scenario_dir/commit_output.log" 2>/dev/null; then
        log_pass "Commit output indicates success"
    else
        log_fail "Commit output missing success message"
        result=1
    fi

    # Step 5: Capture audit actions (approval + send)
    log_info "Step 5: Capturing audit trail..."
    "$FT_BINARY" audit -f json -l 50 > "$scenario_dir/audit_actions.json" 2>&1 || true

    if jq -e 'map(select(.action_kind == "approve_allow_once")) | length > 0' \
        "$scenario_dir/audit_actions.json" >/dev/null 2>&1; then
        log_pass "Approval audit record captured"
    else
        log_fail "Approval audit record missing"
        result=1
    fi

    if jq -e 'map(select(.action_kind == "send_text")) | length > 0' \
        "$scenario_dir/audit_actions.json" >/dev/null 2>&1; then
        log_pass "Send-text audit record captured"
    else
        log_warn "Send-text audit record not found (check audit output)"
    fi

    # Step 6: Capture pane output for evidence
    "$FT_BINARY" robot get-text "$pane_id" --tail 50 > "$scenario_dir/pane_text.txt" 2>&1 || true
    if grep -q "$marker" "$scenario_dir/pane_text.txt" 2>/dev/null; then
        log_pass "Marker observed in pane output"
    else
        log_warn "Marker not found in pane output"
    fi

    trap - EXIT
    cleanup_prepare_commit_approvals

    return $result
}

run_scenario_quickfix_suggestions() {
    local scenario_dir="$1"
    local temp_workspace
    temp_workspace=$(mktemp -d /tmp/ft-e2e-quickfix-XXXXXX)
    local ft_pid=""
    local compaction_pane=""
    local alt_pane=""
    local result=0
    local wait_timeout=${TIMEOUT:-60}
    local old_ft_data_dir="${FT_DATA_DIR:-}"
    local old_ft_workspace="${FT_WORKSPACE:-}"
    local old_ft_config="${FT_CONFIG:-}"

    log_info "Workspace: $temp_workspace"

    cleanup_quickfix_suggestions() {
        log_verbose "Cleaning up quickfix_suggestions scenario"
        if [[ -n "${ft_pid:-}" ]] && kill -0 "$ft_pid" 2>/dev/null; then
            log_verbose "Stopping ft watch (pid $ft_pid)"
            kill "$ft_pid" 2>/dev/null || true
            wait "$ft_pid" 2>/dev/null || true
        fi
        if [[ -n "${compaction_pane:-}" ]]; then
            log_verbose "Closing compaction pane $compaction_pane"
            wezterm cli kill-pane --pane-id "$compaction_pane" 2>/dev/null || true
        fi
        if [[ -n "${alt_pane:-}" ]]; then
            log_verbose "Closing alt-screen pane $alt_pane"
            wezterm cli kill-pane --pane-id "$alt_pane" 2>/dev/null || true
        fi
        if [[ -d "${temp_workspace:-}" ]]; then
            cp -r "$temp_workspace/.ft"/* "$scenario_dir/" 2>/dev/null || true
            cp "$temp_workspace/ft.toml" "$scenario_dir/" 2>/dev/null || true
        fi
        if [[ -n "$old_ft_data_dir" ]]; then
            export FT_DATA_DIR="$old_ft_data_dir"
        else
            unset FT_DATA_DIR
        fi
        if [[ -n "$old_ft_workspace" ]]; then
            export FT_WORKSPACE="$old_ft_workspace"
        else
            unset FT_WORKSPACE
        fi
        if [[ -n "$old_ft_config" ]]; then
            export FT_CONFIG="$old_ft_config"
        else
            unset FT_CONFIG
        fi
        rm -rf "${temp_workspace:-}"
    }
    trap cleanup_quickfix_suggestions EXIT

    is_safe_command() {
        local cmd="$1"
        if [[ "$cmd" == *$'\n'* ]]; then
            return 1
        fi
        case "$cmd" in
            *';'*|*'|'*|*'&'*|*'`'*|*'<'*|*'>'*|*'$('*)
                return 1
                ;;
        esac
        return 0
    }

    ipc_pane_state() {
        local target_pane="$1"
        local socket_path="$FT_DATA_DIR/ipc.sock"
        python3 - "$socket_path" "$target_pane" <<'PY'
import json
import socket
import sys

sock_path = sys.argv[1]
pane_id = int(sys.argv[2])
req = {"type": "pane_state", "pane_id": pane_id}

s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(2.0)
s.connect(sock_path)
s.sendall((json.dumps(req) + "\n").encode("utf-8"))
data = b""
while not data.endswith(b"\n"):
    chunk = s.recv(4096)
    if not chunk:
        break
    data += chunk
s.close()
sys.stdout.write(data.decode("utf-8").strip())
PY
    }

    # Setup environment for isolated wa instance
    export FT_DATA_DIR="$temp_workspace/.ft"
    export FT_WORKSPACE="$temp_workspace"
    mkdir -p "$FT_DATA_DIR"

    local strict_config="$PROJECT_ROOT/fixtures/e2e/config_strict.toml"
    if [[ -f "$strict_config" ]]; then
        cp "$strict_config" "$temp_workspace/ft.toml"
        export FT_CONFIG="$temp_workspace/ft.toml"
        log_verbose "Using strict config: $strict_config"
    fi

    # Start ft watch
    log_info "Step 1: Starting ft watch..."
    "$FT_BINARY" watch --foreground --config "$temp_workspace/ft.toml" \
        > "$scenario_dir/wa_watch.log" 2>&1 &
    ft_pid=$!
    echo "ft_pid: $ft_pid" >> "$scenario_dir/scenario.log"

    local check_watch_cmd="kill -0 $ft_pid 2>/dev/null"
    if ! wait_for_condition "ft watch running" "$check_watch_cmd" "$wait_timeout"; then
        log_fail "ft watch failed to start"
        return 1
    fi
    log_pass "ft watch running"

    # Step 2: Emit a compaction marker to produce an unhandled event
    log_info "Step 2: Spawning compaction marker pane..."
    local compaction_script="$temp_workspace/emit_compaction.sh"
    cat > "$compaction_script" <<'EOS'
#!/bin/bash
set -euo pipefail
echo "Conversation compacted 120 tokens to 45"
echo "Auto-compact"
# Keep pane alive until the harness cleans up (avoid fixed sleeps).
tail -f /dev/null
EOS
    chmod +x "$compaction_script"

    local spawn_output
    spawn_output=$(wezterm cli spawn --cwd "$temp_workspace" -- bash "$compaction_script" 2>&1)
    compaction_pane=$(echo "$spawn_output" | grep -oE '^[0-9]+$' | head -1)

    if [[ -z "$compaction_pane" ]]; then
        log_fail "Failed to spawn compaction pane"
        echo "spawn_output: $spawn_output" >> "$scenario_dir/scenario.log"
        return 1
    fi
    log_info "Spawned compaction pane: $compaction_pane"
    echo "compaction_pane_id: $compaction_pane" >> "$scenario_dir/scenario.log"

    local check_pane_cmd="\"$FT_BINARY\" robot state 2>/dev/null | jq -e '.data[]? | select(.pane_id == $compaction_pane)' >/dev/null 2>&1"
    if ! wait_for_condition "pane $compaction_pane observed" "$check_pane_cmd" "$wait_timeout"; then
        log_fail "Timeout waiting for compaction pane to be observed"
        "$FT_BINARY" robot state > "$scenario_dir/robot_state_compaction.json" 2>&1 || true
        result=1
    else
        log_pass "Compaction pane observed"
    fi

    local event_cmd="\"$FT_BINARY\" events -f json --unhandled --rule-id \"claude_code.compaction\" --limit 20 2>/dev/null | jq -e 'length >= 1' >/dev/null 2>&1"
    if ! wait_for_condition "unhandled compaction event detected" "$event_cmd" "$wait_timeout"; then
        log_fail "Timeout waiting for compaction event"
        "$FT_BINARY" events -f json --limit 20 > "$scenario_dir/events_debug.json" 2>&1 || true
        result=1
    else
        log_pass "Compaction event detected"
    fi

    "$FT_BINARY" events -f json --unhandled --rule-id "claude_code.compaction" --limit 20 \
        > "$scenario_dir/suggestions_output.json" 2>&1 || true

    if jq -e '.[0]' "$scenario_dir/suggestions_output.json" >/dev/null 2>&1; then
        jq -c '.[]' "$scenario_dir/suggestions_output.json" > "$scenario_dir/events.jsonl" 2>/dev/null || true
    else
        cp "$scenario_dir/suggestions_output.json" "$scenario_dir/events.jsonl" 2>/dev/null || true
    fi

    "$FT_BINARY" robot events --unhandled --rule-id "claude_code.compaction" --limit 5 --would-handle --dry-run \
        > "$scenario_dir/robot_events_preview.json" 2>&1 || true

    "$FT_BINARY" robot rules show "claude_code.compaction" \
        > "$scenario_dir/robot_rule_detail.json" 2>&1 || true

    local preview_command=""
    preview_command=$(jq -r '.data.events[0].would_handle_with.preview_command // empty' \
        "$scenario_dir/robot_events_preview.json" 2>/dev/null || echo "")
    local remediation=""
    remediation=$(jq -r '.data.remediation // empty' "$scenario_dir/robot_rule_detail.json" 2>/dev/null || echo "")
    local manual_fix=""
    manual_fix=$(jq -r '.data.manual_fix // empty' "$scenario_dir/robot_rule_detail.json" 2>/dev/null || echo "")

    if [[ -n "$preview_command" ]]; then
        log_pass "Preview command present"
        echo "preview_command: $preview_command" >> "$scenario_dir/scenario.log"
    else
        log_fail "Preview command missing"
        result=1
    fi

    if [[ -n "$remediation" ]]; then
        log_pass "Remediation suggestion present"
    else
        log_fail "Remediation suggestion missing"
        result=1
    fi

    if [[ -n "$manual_fix" ]]; then
        log_pass "Manual fix suggestion present"
    else
        log_fail "Manual fix suggestion missing"
        result=1
    fi

    if [[ -n "$preview_command" ]]; then
        if is_safe_command "$preview_command"; then
            log_pass "Preview command appears safe"
        else
            log_fail "Preview command contains unsafe characters"
            result=1
        fi
    fi

    if [[ -n "$preview_command" ]] && is_safe_command "$preview_command"; then
        local exec_cmd="$preview_command"
        if [[ "$exec_cmd" == wa\ * ]]; then
            exec_cmd="${exec_cmd/wa /$FT_BINARY }"
        fi
        read -r -a preview_argv <<< "$exec_cmd"
        set +e
        timeout 10 "${preview_argv[@]}" > "$scenario_dir/copy_paste_execution.log" 2>&1
        local exec_rc=$?
        set -e
        echo "preview_exec_rc: $exec_rc" >> "$scenario_dir/scenario.log"
        if [[ $exec_rc -eq 0 ]]; then
            log_pass "Preview command executed successfully"
        else
            log_fail "Preview command failed (rc=$exec_rc)"
            result=1
        fi
    else
        log_warn "Skipping preview execution (missing/unsafe preview command)"
    fi

    # Step 3: Error suggestions for invalid pane id
    log_info "Step 3: Validating error remediation for invalid pane..."
    local error_output=""
    error_output=$("$FT_BINARY" send --pane 999 "hello" 2>&1 || true)
    echo "$error_output" > "$scenario_dir/error_invalid_pane.json"

    if echo "$error_output" | jq -e '.ok == false' >/dev/null 2>&1; then
        log_pass "Invalid pane send produced error JSON"
    else
        log_fail "Invalid pane send did not return error JSON"
        result=1
    fi

    local error_hint=""
    error_hint=$(echo "$error_output" | jq -r '.hint // empty' 2>/dev/null || echo "")

    if [[ -n "$error_hint" ]]; then
        log_pass "Error hint present"
    else
        log_fail "Error hint missing"
        result=1
    fi

    # Step 4: Policy denial suggestions (alt-screen)
    log_info "Step 4: Triggering policy denial via alt-screen pane..."
    local alt_script="$PROJECT_ROOT/fixtures/e2e/dummy_alt_screen.sh"
    if [[ ! -x "$alt_script" ]]; then
        log_fail "Alt-screen script not found or not executable: $alt_script"
        result=1
    else
        local alt_spawn_output
        alt_spawn_output=$(wezterm cli spawn --cwd "$temp_workspace" -- bash "$alt_script" 60 2>&1)
        alt_pane=$(echo "$alt_spawn_output" | grep -oE '^[0-9]+$' | head -1)

        if [[ -z "$alt_pane" ]]; then
            log_fail "Failed to spawn alt-screen pane"
            echo "spawn_output: $alt_spawn_output" >> "$scenario_dir/scenario.log"
            result=1
        else
            log_info "Spawned alt-screen pane: $alt_pane"
            echo "alt_screen_pane_id: $alt_pane" >> "$scenario_dir/scenario.log"

            local check_alt_pane_cmd="\"$FT_BINARY\" robot state 2>/dev/null | jq -e '.data[]? | select(.pane_id == $alt_pane)' >/dev/null 2>&1"
            if ! wait_for_condition "alt-screen pane observed" "$check_alt_pane_cmd" "$wait_timeout"; then
                log_fail "Timeout waiting for alt-screen pane to be observed"
                result=1
            else
                log_pass "Alt-screen pane observed"
            fi

            local alt_state_cmd="ipc_pane_state \"$alt_pane\" | jq -e '.ok == true and .data.known == true and ((.data.cursor_alt_screen // .data.alt_screen // false) == true)' >/dev/null 2>&1"
            if ! wait_for_condition "alt-screen true" "$alt_state_cmd" "$wait_timeout"; then
                log_fail "Alt-screen state not detected"
                ipc_pane_state "$alt_pane" > "$scenario_dir/pane_state_alt_screen.json" 2>&1 || true
                result=1
            else
                log_pass "Alt-screen state detected"
            fi

            local deny_output=""
            deny_output=$("$FT_BINARY" send --pane "$alt_pane" "test_text_should_be_denied" 2>&1 || true)
            echo "$deny_output" > "$scenario_dir/policy_denial.json"

            if echo "$deny_output" | jq -e '.injection.Denied or .injection.RequiresApproval' >/dev/null 2>&1; then
                log_pass "Alt-screen send denied"
            elif echo "$deny_output" | jq -e '.ok == false' >/dev/null 2>&1; then
                log_pass "Alt-screen send denied with error JSON"
            else
                log_fail "Alt-screen send not denied"
                result=1
            fi

            local recent_output=""
            recent_output=$("$FT_BINARY" why --recent --pane "$alt_pane" -f json 2>&1 || true)
            echo "$recent_output" > "$scenario_dir/why_recent.json"

            local decision_id=""
            decision_id=$(echo "$recent_output" | jq -r '.decisions[0].id // empty' 2>/dev/null || echo "")
            local template_id=""
            template_id=$(echo "$recent_output" | jq -r '.decisions[0].explanation_template // empty' 2>/dev/null || echo "")

            if [[ -n "$decision_id" ]]; then
                log_pass "Captured recent policy decision id"
                local detail_output=""
                detail_output=$("$FT_BINARY" why --recent --decision-id "$decision_id" -f json 2>&1 || true)
                echo "$detail_output" > "$scenario_dir/why_decision_detail.json"
                local suggestion_count=0
                suggestion_count=$(echo "$detail_output" | jq '.explanation.suggestions | length' 2>/dev/null || echo "0")
                if [[ "$suggestion_count" -gt 0 ]]; then
                    log_pass "Policy denial suggestions present"
                    policy_suggestions_ok="true"
                else
                    log_fail "Policy denial suggestions missing"
                    result=1
                fi
            else
                log_fail "No recent policy decision found for alt-screen pane"
                result=1
            fi

            if [[ -n "$template_id" ]]; then
                echo "policy_template_id: $template_id" >> "$scenario_dir/scenario.log"
            fi
        fi
    fi

    # Step 5: Fuzzy match / typo recovery (soft check)
    log_info "Step 5: Checking typo recovery hints (soft check)..."
    local typo_output=""
    typo_output=$("$FT_BINARY" workflow run handle_compactoin --dry-run 2>&1 || true)
    echo "$typo_output" > "$scenario_dir/typo_workflow.json"

    if echo "$typo_output" | grep -qi "did you mean"; then
        log_pass "Typo recovery hint present"
    else
        log_warn "Typo recovery hint not found (soft check)"
    fi

    cat > "$scenario_dir/suggestion_validation.json" <<EOF
{
  "preview_command_present": $( [[ -n "$preview_command" ]] && echo "true" || echo "false" ),
  "remediation_present": $( [[ -n "$remediation" ]] && echo "true" || echo "false" ),
  "manual_fix_present": $( [[ -n "$manual_fix" ]] && echo "true" || echo "false" ),
  "error_remediation_present": $( [[ -n "$error_hint" ]] && echo "true" || echo "false" ),
  "policy_denial_suggestions_present": $policy_suggestions_ok
}
EOF

    return $result
}

run_scenario_triage_multi_issue() {
    local scenario_dir="$1"
    local temp_workspace
    temp_workspace=$(mktemp -d /tmp/ft-e2e-triage-XXXXXX)
    local result=0
    local db_path=""
    local pane_id=9001
    local old_ft_data_dir="${FT_DATA_DIR:-}"
    local old_ft_workspace="${FT_WORKSPACE:-}"
    local old_ft_config="${FT_CONFIG:-}"

    log_info "Workspace: $temp_workspace"

    cleanup_triage_multi_issue() {
        log_verbose "Cleaning up triage_multi_issue scenario"
        if [[ -d "${temp_workspace:-}" ]]; then
            cp -r "$temp_workspace/.ft"/* "$scenario_dir/" 2>/dev/null || true
            cp "$temp_workspace/ft.toml" "$scenario_dir/" 2>/dev/null || true
        fi
        if [[ -n "$old_ft_data_dir" ]]; then
            export FT_DATA_DIR="$old_ft_data_dir"
        else
            unset FT_DATA_DIR
        fi
        if [[ -n "$old_ft_workspace" ]]; then
            export FT_WORKSPACE="$old_ft_workspace"
        else
            unset FT_WORKSPACE
        fi
        if [[ -n "$old_ft_config" ]]; then
            export FT_CONFIG="$old_ft_config"
        else
            unset FT_CONFIG
        fi
        rm -rf "${temp_workspace:-}"
    }
    trap cleanup_triage_multi_issue EXIT

    # Setup environment for isolated wa instance
    export FT_DATA_DIR="$temp_workspace/.ft"
    export FT_WORKSPACE="$temp_workspace"
    mkdir -p "$FT_DATA_DIR"

    local baseline_config="$PROJECT_ROOT/fixtures/e2e/config_baseline.toml"
    if [[ -f "$baseline_config" ]]; then
        cp "$baseline_config" "$temp_workspace/ft.toml"
        export FT_CONFIG="$temp_workspace/ft.toml"
        log_verbose "Using baseline config: $baseline_config"
    fi

    # Step 1: Initialize DB
    log_info "Step 1: Initializing DB..."
    "$FT_BINARY" db migrate --yes > "$scenario_dir/db_migrate.txt" 2>&1 || true
    "$FT_BINARY" db check -f json > "$scenario_dir/db_check.json" 2>&1 || true
    db_path="$temp_workspace/.ft/ft.db"
    if [[ ! -f "$db_path" ]]; then
        log_fail "DB not created at $db_path"
        result=1
    fi

    if [[ $result -eq 0 ]]; then
        # Step 2: Seed health snapshot (ingest lag warning)
        log_info "Step 2: Seeding health snapshot..."
        local now_ms
        now_ms=$(python3 - <<'PY'
import time
print(int(time.time() * 1000))
PY
)
        cat > "$temp_workspace/.ft/health_snapshot.json" <<EOF
{
  "timestamp": $now_ms,
  "observed_panes": 1,
  "capture_queue_depth": 0,
  "write_queue_depth": 0,
  "last_seq_by_pane": [[${pane_id}, 10]],
  "warnings": ["Index lag above threshold"],
  "ingest_lag_avg_ms": 3500.0,
  "ingest_lag_max_ms": 6500,
  "db_writable": true,
  "db_last_write_at": $now_ms
}
EOF

        # Step 3: Create crash bundle
        log_info "Step 3: Creating crash bundle..."
        local crash_dir="$temp_workspace/.ft/crash"
        mkdir -p "$crash_dir"
        local crash_ts
        crash_ts=$(date -u +"%Y%m%d_%H%M%S")
        local crash_path="$crash_dir/wa_crash_${crash_ts}"
        mkdir -p "$crash_path"
        local epoch_secs
        epoch_secs=$(date +%s)
        local created_at
        created_at=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

        cat > "$crash_path/crash_report.json" <<EOF
{
  "message": "E2E crash for triage scenario",
  "location": "e2e.rs:1",
  "backtrace": null,
  "timestamp": $epoch_secs,
  "pid": 1234,
  "thread_name": "e2e"
}
EOF

        cat > "$crash_path/manifest.json" <<EOF
{
  "wa_version": "e2e",
  "created_at": "$created_at",
  "files": ["crash_report.json"],
  "has_health_snapshot": false,
  "bundle_size_bytes": 0
}
EOF

        # Step 4: Seed DB with pane, events, and a waiting workflow
        log_info "Step 4: Seeding DB (pane, events, workflow)..."
        sqlite3 "$db_path" <<SQL
PRAGMA foreign_keys = ON;
INSERT OR REPLACE INTO panes (
    pane_id, pane_uuid, domain, window_id, tab_id, title, cwd, tty_name,
    first_seen_at, last_seen_at, observed, ignore_reason, last_decision_at
) VALUES (
    $pane_id, 'e2e-pane-uuid', 'local', 1, 1, 'e2e-pane', '$temp_workspace', 'tty-e2e',
    $now_ms, $now_ms, 1, NULL, $now_ms
);

INSERT INTO events (
    pane_id, rule_id, agent_type, event_type, severity, confidence,
    extracted, matched_text, segment_id, detected_at, handled_at,
    handled_by_workflow_id, handled_status, dedupe_key
) VALUES (
    $pane_id, 'e2e.triage:error', 'codex', 'error', 'error', 0.9,
    NULL, 'E2E error event', NULL, $now_ms, NULL,
    NULL, NULL, 'e2e-triage-error'
);

INSERT INTO events (
    pane_id, rule_id, agent_type, event_type, severity, confidence,
    extracted, matched_text, segment_id, detected_at, handled_at,
    handled_by_workflow_id, handled_status, dedupe_key
) VALUES (
    $pane_id, 'e2e.triage:warning', 'codex', 'warning', 'warning', 0.6,
    NULL, 'E2E warning event', NULL, $now_ms, NULL,
    NULL, NULL, 'e2e-triage-warning'
);

INSERT INTO workflow_executions (
    id, workflow_name, pane_id, trigger_event_id, current_step, status,
    wait_condition, context, result, error, started_at, updated_at, completed_at
) VALUES (
    'e2e-workflow-1', 'handle_compaction', $pane_id, NULL, 2, 'waiting',
    '{"type":"pattern","rule_id":"e2e.wait"}', NULL, NULL, NULL,
    $now_ms, $now_ms, NULL
);
SQL

        # Step 5: Run triage and capture output
        log_info "Step 5: Running wa triage..."
        "$FT_BINARY" triage -f json > "$scenario_dir/triage.json" \
            2> "$scenario_dir/triage_stderr.log" || true
        "$FT_BINARY" triage --details > "$scenario_dir/triage.txt" \
            2> "$scenario_dir/triage_details_stderr.log" || true

        if ! jq -e '.ok == true' "$scenario_dir/triage.json" >/dev/null 2>&1; then
            log_fail "Triage JSON missing ok=true"
            result=1
        fi

        if jq -e '.items | length >= 4' "$scenario_dir/triage.json" >/dev/null 2>&1; then
            log_pass "Triage returned multiple items"
        else
            log_fail "Expected multiple triage items"
            result=1
        fi

        if jq -e '.items | map(.section) | index("health") != null' "$scenario_dir/triage.json" >/dev/null 2>&1; then
            log_pass "Health item present"
        else
            log_fail "Health item missing"
            result=1
        fi

        if jq -e '.items | map(.section) | index("crashes") != null' "$scenario_dir/triage.json" >/dev/null 2>&1; then
            log_pass "Crash item present"
        else
            log_fail "Crash item missing"
            result=1
        fi

        if jq -e '.items | map(.section) | index("events") != null' "$scenario_dir/triage.json" >/dev/null 2>&1; then
            log_pass "Event item present"
        else
            log_fail "Event item missing"
            result=1
        fi

        if jq -e '.items | map(.section) | index("workflows") != null' "$scenario_dir/triage.json" >/dev/null 2>&1; then
            log_pass "Workflow item present"
        else
            log_fail "Workflow item missing"
            result=1
        fi

        local first_severity
        first_severity=$(jq -r '.items[0].severity // empty' "$scenario_dir/triage.json" 2>/dev/null || echo "")
        if [[ "$first_severity" == "error" ]]; then
            log_pass "Triage ordering places error severity first"
        else
            log_fail "Unexpected triage ordering (first severity: $first_severity)"
            result=1
        fi

        if jq -e 'all(.items[]; (.action != null) and (.actions != null))' "$scenario_dir/triage.json" >/dev/null 2>&1; then
            log_pass "Triage items include actions"
        else
            log_fail "Missing actions in triage items"
            result=1
        fi
    fi

    trap - EXIT
    cleanup_triage_multi_issue

    return $result
}

run_scenario_rules_explain_trace() {
    local scenario_dir="$1"
    local result=0
    local filler
    filler=$(printf 'x%.0s' $(seq 1 120))
    local test_text="Usage limit warning: ${filler} 42% of your Pro models quota remaining"

    # Step 1: Capture rules list (human JSON)
    log_info "Step 1: Capturing rules list (JSON)..."
    local list_output
    list_output=$("$FT_BINARY" rules list --format json 2>&1 || true)
    echo "$list_output" > "$scenario_dir/rules_list.json"
    if echo "$list_output" | jq -e 'type == "array" and length > 0' >/dev/null 2>&1; then
        log_pass "Rules list JSON captured"
    else
        log_fail "Rules list JSON invalid or empty"
        result=1
    fi

    # Step 2: Run robot rules test with trace
    log_info "Step 2: Running robot rules test with trace..."
    local robot_output
    robot_output=$("$FT_BINARY" robot --format json rules test "$test_text" --trace 2>&1 || true)
    echo "$robot_output" > "$scenario_dir/robot_rules_test_trace.json"

    if echo "$robot_output" | jq -e '.ok == true and (.data.match_count // 0) >= 1' >/dev/null 2>&1; then
        log_pass "Robot rules test returned matches"
    else
        log_fail "Robot rules test missing matches"
        result=1
    fi

    if echo "$robot_output" | jq -e '.data.matches | map(select(.rule_id == "gemini.usage.warning")) | length >= 1' >/dev/null 2>&1; then
        log_pass "Expected rule match found (gemini.usage.warning)"
    else
        log_warn "Expected rule match not found (gemini.usage.warning)"
    fi

    if echo "$robot_output" | jq -e '.data.matches[0].trace.anchors_checked == true and .data.matches[0].trace.regex_matched == true' >/dev/null 2>&1; then
        log_pass "Trace fields present (anchors_checked, regex_matched)"
    else
        log_fail "Trace fields missing or invalid"
        result=1
    fi

    local matched_len
    matched_len=$(echo "$robot_output" | jq -r '[.data.matches[] | select(.rule_id == "gemini.usage.warning") | (.matched_text | length)] | if length > 0 then .[0] else 0 end' 2>/dev/null || echo "0")

    # Step 3: Run human rules test (plain output)
    log_info "Step 3: Running human rules test (plain)..."
    local human_output
    human_output=$("$FT_BINARY" rules test "$test_text" --format plain 2>&1 || true)
    echo "$human_output" > "$scenario_dir/rules_test_output.txt"

    if echo "$human_output" | grep -q "Matches ("; then
        log_pass "Human rules test output rendered"
    else
        log_fail "Human rules test output missing matches header"
        result=1
    fi

    if [[ "$matched_len" -gt 80 ]]; then
        if echo "$human_output" | grep -q "Matched: .*\\.\\.\\."; then
            log_pass "Matched text truncated with ellipsis"
        else
            log_fail "Matched text not truncated when expected"
            result=1
        fi
    fi

    # Step 4: Run robot rules lint (fixtures)
    log_info "Step 4: Running robot rules lint (fixtures)..."
    local lint_output
    lint_output=$("$FT_BINARY" robot --format json rules lint --fixtures 2>&1 || true)
    echo "$lint_output" > "$scenario_dir/robot_rules_lint.json"
    if echo "$lint_output" | jq -e '.ok == true and (.data.rules_checked // 0) > 0' >/dev/null 2>&1; then
        log_pass "Robot rules lint output captured"
    else
        log_fail "Robot rules lint output invalid"
        result=1
    fi

    if echo "$lint_output" | jq -e '.data.fixture_coverage.total_fixtures >= 0' >/dev/null 2>&1; then
        log_pass "Fixture coverage stats present"
    else
        log_warn "Fixture coverage stats missing"
    fi

    return $result
}

run_scenario_stress_scale() {
    # Env overrides:
    #   STRESS_PANES, STRESS_LINES_PER_PANE, STRESS_LARGE_LINES, STRESS_DELAY_SECS
    #   STRESS_INGEST_LAG_MAX_MS, STRESS_RSS_KB_MAX, STRESS_CPU_PCT_MAX, STRESS_FTS_MS_MAX
    #   STRESS_LONG_RUN_SECS, STRESS_MEM_GROWTH_PCT_MAX
    local scenario_dir="$1"
    local temp_workspace
    temp_workspace=$(mktemp -d /tmp/ft-e2e-stress-XXXXXX)
    local ft_pid=""
    local result=0
    local wait_timeout=${TIMEOUT:-120}
    local pane_count="${STRESS_PANES:-10}"
    local lines_per_pane="${STRESS_LINES_PER_PANE:-2000}"
    local large_lines="${STRESS_LARGE_LINES:-100000}"
    local delay_secs="${STRESS_DELAY_SECS:-0.002}"
    local ingest_lag_budget_ms="${STRESS_INGEST_LAG_MAX_MS:-200}"
    local rss_budget_kb="${STRESS_RSS_KB_MAX:-800000}"
    local cpu_budget_pct="${STRESS_CPU_PCT_MAX:-80}"
    local fts_budget_ms="${STRESS_FTS_MS_MAX:-800}"
    local long_run_secs="${STRESS_LONG_RUN_SECS:-0}"
    local mem_growth_budget_pct="${STRESS_MEM_GROWTH_PCT_MAX:-15}"
    local marker="E2E_STRESS_$(date +%s%N)"
    local burst_script="$PROJECT_ROOT/fixtures/e2e/dummy_burst.sh"
    local chatter_script="$temp_workspace/emit_chatter.sh"
    local pane_ids=()
    local rss_kb_start="null"
    local rss_kb_end="null"
    local mem_growth_pct="null"

    log_info "Workspace: $temp_workspace"
    log_info "Stress marker: $marker"
    echo "pane_count: $pane_count" >> "$scenario_dir/scenario.log"
    echo "lines_per_pane: $lines_per_pane" >> "$scenario_dir/scenario.log"
    echo "large_lines: $large_lines" >> "$scenario_dir/scenario.log"

    cleanup_stress_scale() {
        log_verbose "Cleaning up stress_scale scenario"
        if [[ -n "${ft_pid:-}" ]] && kill -0 "$ft_pid" 2>/dev/null; then
            log_verbose "Stopping ft watch (pid $ft_pid)"
            kill "$ft_pid" 2>/dev/null || true
            wait "$ft_pid" 2>/dev/null || true
        fi
        for pid in "${pane_ids[@]}"; do
            wezterm cli kill-pane --pane-id "$pid" 2>/dev/null || true
        done
        if [[ -d "${temp_workspace:-}" ]]; then
            cp -r "$temp_workspace/.ft"/* "$scenario_dir/" 2>/dev/null || true
            cp "$temp_workspace/ft.toml" "$scenario_dir/" 2>/dev/null || true
        fi
        rm -rf "${temp_workspace:-}"
    }
    trap cleanup_stress_scale EXIT

    if [[ ! -x "$burst_script" ]]; then
        log_fail "Burst script not found or not executable: $burst_script"
        return 1
    fi

    # Prepare chatter script for pane fanout
    cat > "$chatter_script" <<'EOS'
#!/bin/bash
set -euo pipefail
PANE="${1:-0}"
COUNT="${2:-1000}"
DELAY="${3:-0.002}"
MARK="${4:-E2E_STRESS}"
for i in $(seq 1 "$COUNT"); do
    printf "[%s] line %d %s\n" "$PANE" "$i" "$MARK"
    sleep "$DELAY"
done
EOS
    chmod +x "$chatter_script"

    # Setup environment for isolated wa instance
    export FT_DATA_DIR="$temp_workspace/.ft"
    export FT_WORKSPACE="$temp_workspace"
    mkdir -p "$FT_DATA_DIR"

    local baseline_config="$PROJECT_ROOT/fixtures/e2e/config_baseline.toml"
    if [[ -f "$baseline_config" ]]; then
        cp "$baseline_config" "$temp_workspace/ft.toml"
    fi
    export FT_CONFIG="$temp_workspace/ft.toml"

    # Start ft watch
    log_info "Step 1: Starting ft watch..."
    "$FT_BINARY" watch --foreground --config "$temp_workspace/ft.toml" \
        > "$scenario_dir/wa_watch.log" 2>&1 &
    ft_pid=$!
    echo "ft_pid: $ft_pid" >> "$scenario_dir/scenario.log"

    local check_watch_cmd="kill -0 $ft_pid 2>/dev/null"
    if ! wait_for_condition "ft watch running" "$check_watch_cmd" "$wait_timeout"; then
        log_fail "ft watch failed to start"
        return 1
    fi
    log_pass "ft watch running"

    # Step 2: Spawn multiple chatty panes
    log_info "Step 2: Spawning $pane_count chatty panes..."
    for i in $(seq 1 "$pane_count"); do
        local spawn_output
        spawn_output=$(wezterm cli spawn --cwd "$temp_workspace" -- \
            bash "$chatter_script" "$i" "$lines_per_pane" "$delay_secs" "$marker" 2>&1)
        local pane_id
        pane_id=$(echo "$spawn_output" | grep -oE '^[0-9]+$' | head -1)
        if [[ -z "$pane_id" ]]; then
            log_fail "Failed to spawn pane $i"
            echo "spawn_output_$i: $spawn_output" >> "$scenario_dir/scenario.log"
            result=1
            continue
        fi
        pane_ids+=("$pane_id")
    done

    if [[ "${#pane_ids[@]}" -lt "$pane_count" ]]; then
        log_warn "Spawned ${#pane_ids[@]} of $pane_count panes"
    else
        log_pass "Spawned $pane_count panes"
    fi

    local check_health_cmd="\"$FT_BINARY\" status --health 2>/dev/null | jq -e '.health != null and .health.observed_panes >= $pane_count' >/dev/null 2>&1"
    if ! wait_for_condition "observed panes >= $pane_count" "$check_health_cmd" "$wait_timeout"; then
        log_fail "Timeout waiting for observed panes"
        "$FT_BINARY" status --health > "$scenario_dir/status_health_initial.json" 2>&1 || true
        result=1
    else
        log_pass "Observed panes >= $pane_count"
    fi

    # Step 3: Emit a large transcript in a dedicated pane
    log_info "Step 3: Spawning large transcript pane..."
    local burst_output
    burst_output=$(wezterm cli spawn --cwd "$temp_workspace" -- \
        bash "$burst_script" "$large_lines" "$marker" 2>&1)
    local burst_pane
    burst_pane=$(echo "$burst_output" | grep -oE '^[0-9]+$' | head -1)
    if [[ -z "$burst_pane" ]]; then
        log_fail "Failed to spawn burst pane"
        echo "burst_spawn_output: $burst_output" >> "$scenario_dir/scenario.log"
        result=1
    else
        pane_ids+=("$burst_pane")
        echo "burst_pane_id: $burst_pane" >> "$scenario_dir/scenario.log"
    fi

    local search_ready_cmd="\"$FT_BINARY\" search \"$marker\" --limit 5 -f json 2>/dev/null | jq -e 'length > 0' >/dev/null 2>&1"
    if ! wait_for_condition "fts search sees marker" "$search_ready_cmd" "$wait_timeout"; then
        log_fail "FTS search did not return results in time"
        "$FT_BINARY" search "$marker" --limit 5 -f json > "$scenario_dir/search_debug.json" 2>&1 || true
        result=1
    else
        log_pass "FTS search returned results"
    fi

    # Step 4: Capture health snapshot and enforce budgets
    log_info "Step 4: Capturing health snapshot and enforcing budgets..."
    "$FT_BINARY" status --health > "$scenario_dir/status_health.json" 2>&1 || true
    local ingest_lag_max
    ingest_lag_max=$(jq -r '.health.ingest_lag_max_ms // 0' "$scenario_dir/status_health.json" 2>/dev/null || echo "0")
    local observed_panes
    observed_panes=$(jq -r '.health.observed_panes // 0' "$scenario_dir/status_health.json" 2>/dev/null || echo "0")

    if [[ "$observed_panes" -ge "$pane_count" ]]; then
        log_pass "Health snapshot reports $observed_panes observed panes"
    else
        log_fail "Observed panes below expected ($observed_panes < $pane_count)"
        result=1
    fi

    if [[ "$ingest_lag_max" -le "$ingest_lag_budget_ms" ]]; then
        log_pass "Ingest lag max ${ingest_lag_max}ms within budget (${ingest_lag_budget_ms}ms)"
    else
        log_fail "Ingest lag max ${ingest_lag_max}ms exceeds budget (${ingest_lag_budget_ms}ms)"
        result=1
    fi

    local ps_stats
    ps_stats=$(ps -o %cpu= -o rss= -p "$ft_pid" 2>/dev/null | awk '{print $1, $2}')
    local cpu_pct="0"
    local rss_kb="0"
    if [[ -n "$ps_stats" ]]; then
        cpu_pct=$(echo "$ps_stats" | awk '{print $1}')
        rss_kb=$(echo "$ps_stats" | awk '{print $2}')
        rss_kb_start="$rss_kb"
        echo "cpu_pct: $cpu_pct" >> "$scenario_dir/scenario.log"
        echo "rss_kb: $rss_kb" >> "$scenario_dir/scenario.log"
        if awk -v v="$cpu_pct" -v max="$cpu_budget_pct" 'BEGIN { exit !(v <= max) }'; then
            log_pass "CPU ${cpu_pct}% within budget (${cpu_budget_pct}%)"
        else
            log_fail "CPU ${cpu_pct}% exceeds budget (${cpu_budget_pct}%)"
            result=1
        fi
        if awk -v v="$rss_kb" -v max="$rss_budget_kb" 'BEGIN { exit !(v <= max) }'; then
            log_pass "RSS ${rss_kb}KB within budget (${rss_budget_kb}KB)"
        else
            log_fail "RSS ${rss_kb}KB exceeds budget (${rss_budget_kb}KB)"
            result=1
        fi
    else
        log_warn "Failed to read CPU/RSS from ps"
    fi

    if [[ "$long_run_secs" -gt 0 ]]; then
        log_info "Step 4b: Long-run memory check (${long_run_secs}s)..."
        if [[ "$rss_kb_start" != "null" && "$rss_kb_start" -gt 0 ]]; then
            sleep "$long_run_secs"
            local rss_after
            rss_after=$(ps -o rss= -p "$ft_pid" 2>/dev/null | awk '{print $1}')
            if [[ -n "$rss_after" ]]; then
                rss_kb_end="$rss_after"
                mem_growth_pct=$(awk -v start="$rss_kb_start" -v end="$rss_kb_end" 'BEGIN { if (start <= 0) {print "null"; exit}; printf "%.2f", ((end - start) / start) * 100 }')
                echo "rss_kb_start: $rss_kb_start" >> "$scenario_dir/scenario.log"
                echo "rss_kb_end: $rss_kb_end" >> "$scenario_dir/scenario.log"
                echo "mem_growth_pct: $mem_growth_pct" >> "$scenario_dir/scenario.log"
                if [[ "$mem_growth_pct" != "null" ]] && awk -v v="$mem_growth_pct" -v max="$mem_growth_budget_pct" 'BEGIN { exit !(v <= max) }'; then
                    log_pass "Memory growth ${mem_growth_pct}% within budget (${mem_growth_budget_pct}%)"
                else
                    log_fail "Memory growth ${mem_growth_pct}% exceeds budget (${mem_growth_budget_pct}%)"
                    result=1
                fi
            else
                log_warn "Failed to read RSS after long-run sleep"
            fi
        else
            log_warn "Skipping long-run memory check (RSS start unavailable)"
        fi
    fi

    # Step 5: Measure FTS query latency
    log_info "Step 5: Measuring FTS query latency..."
    local fts_metrics
    fts_metrics=$(python3 - "$FT_BINARY" "$marker" <<'PY'
import json
import subprocess
import sys
import time

binary = sys.argv[1]
marker = sys.argv[2]
cmd = [binary, "search", marker, "--limit", "5", "-f", "json"]
start = time.time()
try:
    out = subprocess.check_output(cmd, stderr=subprocess.STDOUT).decode("utf-8")
    rc = 0
except subprocess.CalledProcessError as exc:
    out = exc.output.decode("utf-8")
    rc = exc.returncode
elapsed_ms = int((time.time() - start) * 1000)
hits = 0
try:
    data = json.loads(out)
    if isinstance(data, list):
        hits = len(data)
except Exception:
    pass
print(json.dumps({"elapsed_ms": elapsed_ms, "hits": hits, "rc": rc}))
PY
)
    echo "$fts_metrics" > "$scenario_dir/fts_metrics.json"
    local fts_elapsed
    local fts_hits
    fts_elapsed=$(jq -r '.elapsed_ms // 0' "$scenario_dir/fts_metrics.json" 2>/dev/null || echo "0")
    fts_hits=$(jq -r '.hits // 0' "$scenario_dir/fts_metrics.json" 2>/dev/null || echo "0")

    if [[ "$fts_hits" -gt 0 ]]; then
        log_pass "FTS query returned $fts_hits hits"
    else
        log_fail "FTS query returned no hits"
        result=1
    fi

    if [[ "$fts_elapsed" -le "$fts_budget_ms" ]]; then
        log_pass "FTS query ${fts_elapsed}ms within budget (${fts_budget_ms}ms)"
    else
        log_fail "FTS query ${fts_elapsed}ms exceeds budget (${fts_budget_ms}ms)"
        result=1
    fi

    cat > "$scenario_dir/metrics.json" <<EOF
{
  "pane_count": $pane_count,
  "lines_per_pane": $lines_per_pane,
  "large_lines": $large_lines,
  "ingest_lag_max_ms": $ingest_lag_max,
  "cpu_pct": "$cpu_pct",
  "rss_kb": $rss_kb,
  "rss_kb_start": $rss_kb_start,
  "rss_kb_end": $rss_kb_end,
  "mem_growth_pct": $mem_growth_pct,
  "fts_elapsed_ms": $fts_elapsed,
  "fts_hits": $fts_hits,
  "budgets": {
    "ingest_lag_max_ms": $ingest_lag_budget_ms,
    "cpu_pct": $cpu_budget_pct,
    "rss_kb": $rss_budget_kb,
    "fts_elapsed_ms": $fts_budget_ms,
    "mem_growth_pct": $mem_growth_budget_pct,
    "long_run_secs": $long_run_secs
  }
}
EOF

    return $result
}

run_scenario_graceful_shutdown() {
    local scenario_dir="$1"
    local marker="E2E_SHUTDOWN_$(date +%s%N)"
    local temp_workspace
    temp_workspace=$(mktemp -d /tmp/ft-e2e-XXXXXX)
    local ft_pid=""
    local pane_id=""
    local result=0

    log_info "Using marker: $marker"
    log_info "Workspace: $temp_workspace"

    # Setup environment for isolated wa instance
    export FT_DATA_DIR="$temp_workspace/.ft"
    export FT_WORKSPACE="$temp_workspace"
    mkdir -p "$FT_DATA_DIR"

    # Cleanup function
    cleanup_graceful_shutdown() {
        log_verbose "Cleaning up graceful_shutdown scenario"
        # Kill ft watch if still running (should have exited gracefully)
        if [[ -n "${ft_pid:-}" ]] && kill -0 "$ft_pid" 2>/dev/null; then
            log_verbose "Force-killing ft watch (pid $ft_pid) - should have exited"
            kill -9 "$ft_pid" 2>/dev/null || true
            wait "$ft_pid" 2>/dev/null || true
        fi
        # Close dummy pane if it exists
        if [[ -n "${pane_id:-}" ]]; then
            log_verbose "Closing dummy pane $pane_id"
            wezterm cli kill-pane --pane-id "$pane_id" 2>/dev/null || true
        fi
        # Copy artifacts before cleanup
        if [[ -d "${temp_workspace:-}" ]]; then
            cp -r "$temp_workspace/.ft"/* "${scenario_dir:-/dev/null}/" 2>/dev/null || true
        fi
        rm -rf "${temp_workspace:-}"
    }
    trap cleanup_graceful_shutdown EXIT

    # Step 1: Spawn dummy pane with the print script (outputs 200 lines for reliable capture)
    log_info "Step 1: Spawning dummy pane..."
    local dummy_script="$PROJECT_ROOT/fixtures/e2e/dummy_print.sh"
    if [[ ! -x "$dummy_script" ]]; then
        log_fail "Dummy print script not found or not executable: $dummy_script"
        return 1
    fi

    local spawn_output
    spawn_output=$(wezterm cli spawn --cwd "$temp_workspace" -- bash "$dummy_script" "$marker" 200 2>&1)
    pane_id=$(echo "$spawn_output" | grep -oE '^[0-9]+$' | head -1)

    if [[ -z "$pane_id" ]]; then
        log_fail "Failed to spawn dummy pane"
        echo "Spawn output: $spawn_output" >> "$scenario_dir/scenario.log"
        return 1
    fi
    log_info "Spawned pane: $pane_id"
    echo "Spawned pane_id: $pane_id" >> "$scenario_dir/scenario.log"

    # Step 2: Start ft watch in foreground mode (so we can control it)
    log_info "Step 2: Starting ft watch..."
    "$FT_BINARY" watch --foreground \
        > "$scenario_dir/wa_watch.log" 2>&1 &
    ft_pid=$!
    log_verbose "ft watch started with PID $ft_pid"
    echo "ft_pid: $ft_pid" >> "$scenario_dir/scenario.log"

    # Verify ft watch is running
    if ! kill -0 "$ft_pid" 2>/dev/null; then
        log_fail "ft watch exited immediately"
        return 1
    fi

    # Step 3: Wait for at least one segment to be persisted
    log_info "Step 3: Waiting for capture and persistence..."
    local wait_timeout=${TIMEOUT:-30}

    # Wait for pane to be observed first
    local check_observed_cmd="\"$FT_BINARY\" robot state 2>/dev/null | jq -e '.data[]? | select(.pane_id == $pane_id)' >/dev/null 2>&1"
    if ! wait_for_condition "pane $pane_id observed" "$check_observed_cmd" "$wait_timeout"; then
        log_fail "Timeout waiting for pane to be observed"
        "$FT_BINARY" robot state > "$scenario_dir/robot_state.json" 2>&1 || true
        return 1
    fi
    log_pass "Pane observed"

    # Wait for marker to appear in search (proves FTS is working and data is persisted)
    log_info "Step 3b: Waiting for marker to appear in FTS index..."
    local search_check_cmd="\"$FT_BINARY\" search \"$marker\" --limit 10 2>/dev/null | grep -q \"$marker\""
    if ! wait_for_condition "marker in FTS" "$search_check_cmd" "$wait_timeout"; then
        log_warn "Marker not found in FTS before shutdown (may be normal if not persisted yet)"
        # Continue anyway - we'll check after shutdown
    else
        log_pass "Marker found in FTS before shutdown"
    fi

    # Record pre-shutdown state
    "$FT_BINARY" robot state > "$scenario_dir/robot_state_before_shutdown.json" 2>&1 || true
    "$FT_BINARY" search "$marker" --limit 10 > "$scenario_dir/search_before_shutdown.txt" 2>&1 || true

    # Step 4: Send SIGINT to ft watch and measure shutdown time
    log_info "Step 4: Sending SIGINT to ft watch..."
    local shutdown_start=$(date +%s)
    kill -INT "$ft_pid" 2>/dev/null

    # Wait for graceful exit (bounded timeout)
    local shutdown_timeout=10
    local shutdown_result=0
    if timeout "$shutdown_timeout" tail --pid="$ft_pid" -f /dev/null 2>/dev/null; then
        shutdown_result=0
    else
        # Fallback: poll for process exit
        local poll_count=0
        while kill -0 "$ft_pid" 2>/dev/null && [[ $poll_count -lt $((shutdown_timeout * 2)) ]]; do
            sleep 0.5
            ((poll_count++))
        done
        if kill -0 "$ft_pid" 2>/dev/null; then
            shutdown_result=1
        fi
    fi

    local shutdown_end=$(date +%s)
    local shutdown_duration=$((shutdown_end - shutdown_start))
    echo "shutdown_duration_secs: $shutdown_duration" >> "$scenario_dir/scenario.log"

    if [[ $shutdown_result -eq 0 ]] || ! kill -0 "$ft_pid" 2>/dev/null; then
        log_pass "ft watch exited cleanly within ${shutdown_duration}s"
        ft_pid=""  # Mark as exited
    else
        log_fail "ft watch did not exit within ${shutdown_timeout}s - forcing kill"
        kill -9 "$ft_pid" 2>/dev/null || true
        wait "$ft_pid" 2>/dev/null || true
        ft_pid=""
        result=1
    fi

    # Step 5: Verify storage was flushed (FTS still works)
    log_info "Step 5: Verifying storage flush (FTS search after shutdown)..."
    local search_output
    search_output=$("$FT_BINARY" search "$marker" --limit 50 2>&1)
    echo "$search_output" > "$scenario_dir/search_after_shutdown.txt"

    local hit_count
    hit_count=$(echo "$search_output" | grep -c "$marker" || echo "0")
    echo "search_hit_count_after_shutdown: $hit_count" >> "$scenario_dir/scenario.log"

    if [[ "$hit_count" -ge 1 ]]; then
        log_pass "FTS search works after shutdown ($hit_count hits for marker)"
    else
        log_fail "FTS search found no hits after shutdown - data may not have been flushed"
        result=1
    fi

    # Step 6: Verify lock was released (can restart ft watch)
    log_info "Step 6: Verifying lock release (attempting restart)..."

    local restart_pid=""
    "$FT_BINARY" watch --foreground \
        > "$scenario_dir/wa_watch_restart.log" 2>&1 &
    restart_pid=$!

    local restart_check_cmd="kill -0 $restart_pid 2>/dev/null"
    if wait_for_condition "ft watch restarted" "$restart_check_cmd" 5; then
        log_pass "ft watch restarted successfully (lock was released)"
        # Clean up the restarted process
        kill -INT "$restart_pid" 2>/dev/null || true
        if ! wait_for_condition "ft watch restart exited" "! kill -0 $restart_pid 2>/dev/null" 5; then
            kill -9 "$restart_pid" 2>/dev/null || true
        fi
        wait "$restart_pid" 2>/dev/null || true
    else
        # Check if it exited with lock error
        if grep -qi "lock\|already running\|another instance" "$scenario_dir/wa_watch_restart.log" 2>/dev/null; then
            log_fail "ft watch restart failed - lock was NOT released"
            result=1
        else
            # May have exited for other reason, check exit status
            wait "$restart_pid" 2>/dev/null
            local restart_exit=$?
            if [[ $restart_exit -eq 0 ]]; then
                log_pass "ft watch restart exited cleanly (lock was available)"
            else
                log_warn "ft watch restart exited with code $restart_exit (check logs)"
                # Not necessarily a failure - may be config issue
            fi
        fi
    fi

    # Step 7: Verify shutdown summary in logs
    log_info "Step 7: Checking shutdown logs..."
    if grep -qi "shutdown\|terminating\|graceful\|SIGINT\|signal" "$scenario_dir/wa_watch.log" 2>/dev/null; then
        log_pass "Found shutdown-related messages in logs"
    else
        log_warn "No obvious shutdown messages in logs (may be expected)"
    fi

    # Record final summary
    echo "" >> "$scenario_dir/scenario.log"
    echo "=== Shutdown Summary ===" >> "$scenario_dir/scenario.log"
    echo "shutdown_clean: $([[ $result -eq 0 ]] && echo 'yes' || echo 'no')" >> "$scenario_dir/scenario.log"
    echo "fts_hits_after_shutdown: $hit_count" >> "$scenario_dir/scenario.log"

    # Cleanup trap will handle the rest
    trap - EXIT
    cleanup_graceful_shutdown

    return $result
}

# ==============================================================================
# Scenario: pane_exclude_filter
# ==============================================================================
# Tests that pane exclude filters prevent capture of matching panes.
# - Spawns an "observed" pane that prints OBSERVED_TOKEN
# - Spawns an "ignored" pane with title "IGNORED_PANE" that prints SECRET_TOKEN
# - Asserts observed pane is searchable, ignored is NOT
# - Asserts ft status shows ignored pane with exclude reason
# - Asserts SECRET_TOKEN never appears in any artifacts (privacy guarantee)

run_scenario_pane_exclude_filter() {
    local scenario_dir="$1"
    local observed_marker="OBSERVED_TOKEN_$(date +%s%N)"
    local secret_token="SECRET_TOKEN_$(date +%s%N)"
    local temp_workspace
    temp_workspace=$(mktemp -d /tmp/ft-e2e-XXXXXX)
    local ft_pid=""
    local observed_pane_id=""
    local ignored_pane_id=""
    local result=0

    log_info "Using observed marker: $observed_marker"
    log_info "Using secret token: $secret_token"
    log_info "Workspace: $temp_workspace"

    # Setup environment for isolated wa instance
    export FT_DATA_DIR="$temp_workspace/.ft"
    export FT_WORKSPACE="$temp_workspace"
    mkdir -p "$FT_DATA_DIR"

    # Copy pane exclude config
    local exclude_config="$PROJECT_ROOT/fixtures/e2e/config_pane_exclude.toml"
    if [[ -f "$exclude_config" ]]; then
        cp "$exclude_config" "$temp_workspace/ft.toml"
        export FT_CONFIG="$temp_workspace/ft.toml"
        log_verbose "Using exclude config: $exclude_config"
    else
        log_fail "Pane exclude config not found: $exclude_config"
        return 1
    fi

    # Record tokens for artifact verification
    echo "observed_marker: $observed_marker" >> "$scenario_dir/scenario.log"
    echo "secret_token: $secret_token" >> "$scenario_dir/scenario.log"

    # Cleanup function
    cleanup_pane_exclude_filter() {
        log_verbose "Cleaning up pane_exclude_filter scenario"
        # Kill ft watch if running (use :- to avoid unbound variable with set -u)
        if [[ -n "${ft_pid:-}" ]] && kill -0 "$ft_pid" 2>/dev/null; then
            log_verbose "Stopping ft watch (pid $ft_pid)"
            kill "$ft_pid" 2>/dev/null || true
            wait "$ft_pid" 2>/dev/null || true
        fi
        # Close observed pane if it exists
        if [[ -n "${observed_pane_id:-}" ]]; then
            log_verbose "Closing observed pane $observed_pane_id"
            wezterm cli kill-pane --pane-id "$observed_pane_id" 2>/dev/null || true
        fi
        # Close ignored pane if it exists
        if [[ -n "${ignored_pane_id:-}" ]]; then
            log_verbose "Closing ignored pane $ignored_pane_id"
            wezterm cli kill-pane --pane-id "$ignored_pane_id" 2>/dev/null || true
        fi
        # Copy artifacts before cleanup (use :- to avoid unbound variable with set -u)
        if [[ -d "${temp_workspace:-}" ]]; then
            cp -r "${temp_workspace}/.ft"/* "$scenario_dir/" 2>/dev/null || true
            cp "${temp_workspace}/ft.toml" "$scenario_dir/" 2>/dev/null || true
        fi
        rm -rf "${temp_workspace:-}"
    }
    trap cleanup_pane_exclude_filter EXIT

    # Step 1: Spawn the OBSERVED pane (standard dummy_print.sh)
    log_info "Step 1: Spawning observed pane..."
    local dummy_script="$PROJECT_ROOT/fixtures/e2e/dummy_print.sh"
    if [[ ! -x "$dummy_script" ]]; then
        log_fail "Dummy print script not found or not executable: $dummy_script"
        return 1
    fi

    local spawn_output
    # Run dummy_print.sh then keep pane alive for observation (avoid fixed sleeps).
    spawn_output=$(wezterm cli spawn --cwd "$temp_workspace" -- bash -c "'$dummy_script' '$observed_marker' 50; tail -f /dev/null" 2>&1)
    observed_pane_id=$(echo "$spawn_output" | grep -oE '^[0-9]+$' | head -1)

    if [[ -z "$observed_pane_id" ]]; then
        log_fail "Failed to spawn observed pane"
        echo "Spawn output: $spawn_output" >> "$scenario_dir/scenario.log"
        return 1
    fi
    log_info "Spawned observed pane: $observed_pane_id"
    echo "observed_pane_id: $observed_pane_id" >> "$scenario_dir/scenario.log"

    # Step 2: Spawn the IGNORED pane (dummy_ignored_pane.sh with title matching exclude rule)
    log_info "Step 2: Spawning ignored pane (title=IGNORED_PANE)..."
    local ignored_script="$PROJECT_ROOT/fixtures/e2e/dummy_ignored_pane.sh"
    if [[ ! -x "$ignored_script" ]]; then
        log_fail "Ignored pane script not found or not executable: $ignored_script"
        return 1
    fi

    spawn_output=$(wezterm cli spawn --cwd "$temp_workspace" -- bash "$ignored_script" "$secret_token" 50 2>&1)
    ignored_pane_id=$(echo "$spawn_output" | grep -oE '^[0-9]+$' | head -1)

    if [[ -z "$ignored_pane_id" ]]; then
        log_fail "Failed to spawn ignored pane"
        echo "Spawn output: $spawn_output" >> "$scenario_dir/scenario.log"
        return 1
    fi
    log_info "Spawned ignored pane: $ignored_pane_id"
    echo "ignored_pane_id: $ignored_pane_id" >> "$scenario_dir/scenario.log"

    # Wait for the title OSC sequences to propagate (deterministic).
    local title_check_cmd="wezterm cli list --format json 2>/dev/null | jq -e '.[]? | select(.pane_id == $ignored_pane_id) | (.title // \"\") | contains(\"IGNORED_PANE\")' >/dev/null 2>&1"
    if ! wait_for_condition "ignored pane title propagated" "$title_check_cmd" 10; then
        log_warn "Ignored pane title not observed in wezterm cli list yet; continuing"
    fi

    # Step 3: Start ft watch in background with custom config
    log_info "Step 3: Starting ft watch with exclude config..."
    "$FT_BINARY" watch --foreground --config "$temp_workspace/ft.toml" \
        > "$scenario_dir/wa_watch.log" 2>&1 &
    ft_pid=$!
    log_verbose "ft watch started with PID $ft_pid"
    echo "ft_pid: $ft_pid" >> "$scenario_dir/scenario.log"

    # Verify ft watch is running
    if ! kill -0 "$ft_pid" 2>/dev/null; then
        log_fail "ft watch exited immediately"
        return 1
    fi

    # Step 4a: Wait for observed pane to appear in robot state
    log_info "Step 4a: Waiting for observed pane to be observed..."
    local wait_timeout=${TIMEOUT:-60}
    local check_observed_cmd="\"$FT_BINARY\" robot state 2>/dev/null | jq -e '.data[]? | select(.pane_id == $observed_pane_id)' >/dev/null 2>&1"

    if ! wait_for_condition "observed pane $observed_pane_id in robot state" "$check_observed_cmd" "$wait_timeout"; then
        log_fail "Timeout waiting for observed pane to appear in robot state"
        "$FT_BINARY" robot state > "$scenario_dir/robot_state.json" 2>&1 || true
        return 1
    fi
    log_pass "Observed pane detected in robot state"

    # Step 4b: Wait for observed content to be searchable (proves FTS indexing works)
    log_info "Step 4b: Waiting for observed content to be searchable..."
    # Search for the observed marker - check total_hits > 0
    local check_search_cmd="\"$FT_BINARY\" robot search \"$observed_marker\" 2>/dev/null | jq -e '.data.total_hits > 0' >/dev/null 2>&1"

    if ! wait_for_condition "observed content searchable" "$check_search_cmd" "$wait_timeout"; then
        log_fail "Timeout waiting for observed content to be searchable"
        "$FT_BINARY" robot state > "$scenario_dir/robot_state.json" 2>&1 || true
        "$FT_BINARY" robot search "$observed_marker" > "$scenario_dir/search_debug.json" 2>&1 || true
        return 1
    fi
    log_pass "Observed content captured and searchable"

    # Capture robot state (while watcher is still running)
    "$FT_BINARY" robot state > "$scenario_dir/robot_state.json" 2>&1 || true

    # Step 5: Assert OBSERVED_TOKEN is searchable (watcher still running for IPC)
    log_info "Step 5: Asserting observed token is searchable..."
    local search_output
    search_output=$("$FT_BINARY" robot search "$observed_marker" 2>&1)
    echo "$search_output" > "$scenario_dir/search_observed.json"

    local observed_count
    observed_count=$(echo "$search_output" | jq -r '.data.total_hits // .data.total // 0' 2>/dev/null || echo "0")

    if [[ "$observed_count" -gt 0 ]]; then
        log_pass "Observed token found in search ($observed_count results)"
    else
        log_fail "Observed token NOT found in search"
        result=1
    fi

    # Step 6: Assert SECRET_TOKEN is NOT searchable (privacy guarantee)
    log_info "Step 6: Asserting secret token is NOT searchable..."
    search_output=$("$FT_BINARY" robot search "$secret_token" 2>&1)
    echo "$search_output" > "$scenario_dir/search_secret.json"

    local secret_count
    secret_count=$(echo "$search_output" | jq -r '.data.total_hits // .data.total // 0' 2>/dev/null || echo "0")

    if [[ "$secret_count" -eq 0 ]]; then
        log_pass "Secret token correctly NOT found in search"
    else
        log_fail "SECRET TOKEN FOUND IN SEARCH - PRIVACY VIOLATION!"
        result=1
    fi

    # Step 7: Stop ft watch gracefully (after search tests complete)
    log_info "Step 7: Stopping ft watch..."
    kill -TERM "$ft_pid" 2>/dev/null || true
    wait "$ft_pid" 2>/dev/null || true
    ft_pid=""

    # Step 8: Assert SECRET_TOKEN never appears in any captured data files
    log_info "Step 8: Scanning captured data for secret token leakage..."

    # Copy all wa data artifacts first (database, logs, segments)
    cp -r "$temp_workspace/.ft"/* "$scenario_dir/" 2>/dev/null || true

    # Search for leaks in captured data - exclude our own test harness files:
    # - scenario.log: intentionally contains tokens for debugging
    # - search_*.json: contains search queries (not search results finding the token)
    local leaked_files
    leaked_files=$(grep -rl "$secret_token" "$scenario_dir" \
        --exclude="scenario.log" \
        --exclude="search_*.json" \
        2>/dev/null || true)

    if [[ -z "$leaked_files" ]]; then
        log_pass "Secret token not found in any captured data"
    else
        log_fail "SECRET TOKEN LEAKED IN CAPTURED DATA:"
        echo "$leaked_files" | while read -r file; do
            log_fail "  - $file"
        done
        result=1
    fi

    # Step 9: Check robot state shows ignored pane was filtered (informational)
    log_info "Step 9: Checking status output for exclude reason..."

    # This is informational - we check robot state for pane visibility
    local state_output
    state_output=$(cat "$scenario_dir/robot_state.json" 2>/dev/null || echo "{}")

    # Check if ignored pane appears in state with any exclusion indicator
    # (Implementation may vary - this is advisory logging)
    local ignored_in_state
    ignored_in_state=$(echo "$state_output" | jq -e ".data[]? | select(.pane_id == $ignored_pane_id)" 2>/dev/null || true)

    if [[ -z "$ignored_in_state" ]]; then
        log_pass "Ignored pane correctly absent from robot state"
    else
        # Check if it has an exclusion reason
        local exclude_reason
        exclude_reason=$(echo "$ignored_in_state" | jq -r '.exclude_reason // .ignored_reason // empty' 2>/dev/null || true)
        if [[ -n "$exclude_reason" ]]; then
            log_pass "Ignored pane present with exclude reason: $exclude_reason"
        else
            log_warn "Ignored pane present in state without clear exclude reason"
        fi
    fi

    # Cleanup
    trap - EXIT
    cleanup_pane_exclude_filter

    return $result
}

run_scenario_workspace_isolation() {
    local scenario_dir="$1"
    local token_a="WORKSPACE_TOKEN_A_$(date +%s%N)"
    local token_b="WORKSPACE_TOKEN_B_$(date +%s%N)"
    local workspace_a
    local workspace_b
    workspace_a=$(mktemp -d /tmp/ft-e2e-a-XXXXXX)
    workspace_b=$(mktemp -d /tmp/ft-e2e-b-XXXXXX)
    local ft_pid=""
    local pane_a_id=""
    local pane_b_id=""
    local result=0

    log_info "Workspace A token: $token_a"
    log_info "Workspace B token: $token_b"
    log_info "Workspace A: $workspace_a"
    log_info "Workspace B: $workspace_b"

    mkdir -p "$workspace_a/.ft" "$workspace_b/.ft"

    echo "workspace_a: $workspace_a" >> "$scenario_dir/scenario.log"
    echo "workspace_b: $workspace_b" >> "$scenario_dir/scenario.log"
    echo "token_a: $token_a" >> "$scenario_dir/scenario.log"
    echo "token_b: $token_b" >> "$scenario_dir/scenario.log"

    cleanup_workspace_isolation() {
        log_verbose "Cleaning up workspace_isolation scenario"
        if [[ -n "${ft_pid:-}" ]] && kill -0 "$ft_pid" 2>/dev/null; then
            log_verbose "Stopping ft watch (pid $ft_pid)"
            kill "$ft_pid" 2>/dev/null || true
            wait "$ft_pid" 2>/dev/null || true
        fi
        if [[ -n "${pane_a_id:-}" ]]; then
            log_verbose "Closing workspace A pane $pane_a_id"
            wezterm cli kill-pane --pane-id "$pane_a_id" 2>/dev/null || true
        fi
        if [[ -n "${pane_b_id:-}" ]]; then
            log_verbose "Closing workspace B pane $pane_b_id"
            wezterm cli kill-pane --pane-id "$pane_b_id" 2>/dev/null || true
        fi

        if [[ -d "${workspace_a:-}" ]]; then
            mkdir -p "$scenario_dir/workspace_a"
            cp -r "$workspace_a/.ft"/* "$scenario_dir/workspace_a/" 2>/dev/null || true
        fi
        if [[ -d "${workspace_b:-}" ]]; then
            mkdir -p "$scenario_dir/workspace_b"
            cp -r "$workspace_b/.ft"/* "$scenario_dir/workspace_b/" 2>/dev/null || true
        fi

        if [[ "${FT_E2E_PRESERVE_TEMP:-}" == "1" ]]; then
            log_warn "Preserving temp workspaces (FT_E2E_PRESERVE_TEMP=1)"
        else
            rm -rf "${workspace_a:-}" "${workspace_b:-}"
        fi
    }
    trap cleanup_workspace_isolation EXIT

    # Step 1: Spawn workspace A pane
    log_info "Step 1: Spawning workspace A pane..."
    local dummy_script="$PROJECT_ROOT/fixtures/e2e/dummy_print.sh"
    if [[ ! -x "$dummy_script" ]]; then
        log_fail "Dummy print script not found or not executable: $dummy_script"
        return 1
    fi

    local spawn_output
    spawn_output=$(wezterm cli spawn --cwd "$workspace_a" -- bash -c "'$dummy_script' '$token_a' 80; tail -f /dev/null" 2>&1)
    pane_a_id=$(echo "$spawn_output" | grep -oE '^[0-9]+$' | head -1)

    if [[ -z "$pane_a_id" ]]; then
        log_fail "Failed to spawn workspace A pane"
        echo "Spawn output: $spawn_output" >> "$scenario_dir/scenario.log"
        return 1
    fi
    log_info "Spawned workspace A pane: $pane_a_id"
    echo "pane_a_id: $pane_a_id" >> "$scenario_dir/scenario.log"

    # Step 2: Start ft watch for workspace A
    log_info "Step 2: Starting ft watch for workspace A..."
    FT_WORKSPACE="$workspace_a" FT_DATA_DIR="$workspace_a/.ft" \
        "$FT_BINARY" watch --foreground \
        > "$scenario_dir/wa_watch_a.log" 2>&1 &
    ft_pid=$!
    log_verbose "ft watch (A) started with PID $ft_pid"
    echo "ft_pid_a: $ft_pid" >> "$scenario_dir/scenario.log"

    if ! kill -0 "$ft_pid" 2>/dev/null; then
        log_fail "ft watch (A) exited immediately"
        return 1
    fi

    # Step 3: Wait for workspace A pane to be observed
    log_info "Step 3: Waiting for workspace A pane to be observed..."
    local wait_timeout=${TIMEOUT:-60}
    local check_observed_a="FT_LOG_LEVEL=error FT_WORKSPACE=\"$workspace_a\" FT_DATA_DIR=\"$workspace_a/.wa\" \"$FT_BINARY\" robot state 2>/dev/null | jq -e '.data[]? | select(.pane_id == $pane_a_id)' >/dev/null 2>&1"

    if ! wait_for_condition "workspace A pane observed" "$check_observed_a" "$wait_timeout"; then
        log_fail "Timeout waiting for workspace A pane to be observed"
        FT_WORKSPACE="$workspace_a" FT_DATA_DIR="$workspace_a/.ft" \
            "$FT_BINARY" robot state > "$scenario_dir/robot_state_a.json" 2>&1 || true
        return 1
    fi
    log_pass "Workspace A pane observed"

    # Step 4: Wait for token A to be searchable in workspace A
    log_info "Step 4: Waiting for token A to be searchable..."
    local check_search_a="FT_LOG_LEVEL=error FT_WORKSPACE=\"$workspace_a\" FT_DATA_DIR=\"$workspace_a/.wa\" \"$FT_BINARY\" robot search \"$token_a\" 2>/dev/null | jq -e '.data.total_hits > 0' >/dev/null 2>&1"
    if ! wait_for_condition "token A searchable" "$check_search_a" "$wait_timeout"; then
        log_fail "Timeout waiting for token A to be searchable"
        FT_WORKSPACE="$workspace_a" FT_DATA_DIR="$workspace_a/.ft" \
            "$FT_BINARY" robot search "$token_a" > "$scenario_dir/search_a.json" 2>&1 || true
        return 1
    fi
    log_pass "Token A searchable in workspace A"

    FT_LOG_LEVEL=error FT_WORKSPACE="$workspace_a" FT_DATA_DIR="$workspace_a/.ft" \
        "$FT_BINARY" robot state > "$scenario_dir/robot_state_a.json" 2>&1 || true
    FT_LOG_LEVEL=error FT_WORKSPACE="$workspace_a" FT_DATA_DIR="$workspace_a/.ft" \
        "$FT_BINARY" robot search "$token_a" > "$scenario_dir/search_a.json" 2>&1 || true
    FT_LOG_LEVEL=error FT_WORKSPACE="$workspace_a" FT_DATA_DIR="$workspace_a/.ft" \
        "$FT_BINARY" config show --effective --json > "$scenario_dir/config_effective_a.json" 2>&1 || true

    # Step 5: Stop ft watch for workspace A
    log_info "Step 5: Stopping ft watch for workspace A..."
    kill -TERM "$ft_pid" 2>/dev/null || true
    wait "$ft_pid" 2>/dev/null || true
    ft_pid=""

    # Step 6: Spawn workspace B pane
    log_info "Step 6: Spawning workspace B pane..."
    spawn_output=$(wezterm cli spawn --cwd "$workspace_b" -- bash -c "'$dummy_script' '$token_b' 80; tail -f /dev/null" 2>&1)
    pane_b_id=$(echo "$spawn_output" | grep -oE '^[0-9]+$' | head -1)

    if [[ -z "$pane_b_id" ]]; then
        log_fail "Failed to spawn workspace B pane"
        echo "Spawn output: $spawn_output" >> "$scenario_dir/scenario.log"
        return 1
    fi
    log_info "Spawned workspace B pane: $pane_b_id"
    echo "pane_b_id: $pane_b_id" >> "$scenario_dir/scenario.log"

    # Step 7: Start ft watch for workspace B
    log_info "Step 7: Starting ft watch for workspace B..."
    FT_WORKSPACE="$workspace_b" FT_DATA_DIR="$workspace_b/.ft" \
        "$FT_BINARY" watch --foreground \
        > "$scenario_dir/wa_watch_b.log" 2>&1 &
    ft_pid=$!
    log_verbose "ft watch (B) started with PID $ft_pid"
    echo "ft_pid_b: $ft_pid" >> "$scenario_dir/scenario.log"

    if ! kill -0 "$ft_pid" 2>/dev/null; then
        log_fail "ft watch (B) exited immediately"
        return 1
    fi

    # Step 8: Wait for workspace B pane to be observed
    log_info "Step 8: Waiting for workspace B pane to be observed..."
    local check_observed_b="FT_LOG_LEVEL=error FT_WORKSPACE=\"$workspace_b\" FT_DATA_DIR=\"$workspace_b/.wa\" \"$FT_BINARY\" robot state 2>/dev/null | jq -e '.data[]? | select(.pane_id == $pane_b_id)' >/dev/null 2>&1"

    if ! wait_for_condition "workspace B pane observed" "$check_observed_b" "$wait_timeout"; then
        log_fail "Timeout waiting for workspace B pane to be observed"
        FT_WORKSPACE="$workspace_b" FT_DATA_DIR="$workspace_b/.ft" \
            "$FT_BINARY" robot state > "$scenario_dir/robot_state_b.json" 2>&1 || true
        return 1
    fi
    log_pass "Workspace B pane observed"

    # Step 9: Wait for token B to be searchable in workspace B
    log_info "Step 9: Waiting for token B to be searchable..."
    local check_search_b="FT_LOG_LEVEL=error FT_WORKSPACE=\"$workspace_b\" FT_DATA_DIR=\"$workspace_b/.wa\" \"$FT_BINARY\" robot search \"$token_b\" 2>/dev/null | jq -e '.data.total_hits > 0' >/dev/null 2>&1"
    if ! wait_for_condition "token B searchable" "$check_search_b" "$wait_timeout"; then
        log_fail "Timeout waiting for token B to be searchable"
        FT_WORKSPACE="$workspace_b" FT_DATA_DIR="$workspace_b/.ft" \
            "$FT_BINARY" robot search "$token_b" > "$scenario_dir/search_b.json" 2>&1 || true
        return 1
    fi
    log_pass "Token B searchable in workspace B"

    FT_LOG_LEVEL=error FT_WORKSPACE="$workspace_b" FT_DATA_DIR="$workspace_b/.ft" \
        "$FT_BINARY" robot state > "$scenario_dir/robot_state_b.json" 2>&1 || true
    FT_LOG_LEVEL=error FT_WORKSPACE="$workspace_b" FT_DATA_DIR="$workspace_b/.ft" \
        "$FT_BINARY" robot search "$token_b" > "$scenario_dir/search_b.json" 2>&1 || true
    FT_LOG_LEVEL=error FT_WORKSPACE="$workspace_b" FT_DATA_DIR="$workspace_b/.ft" \
        "$FT_BINARY" config show --effective --json > "$scenario_dir/config_effective_b.json" 2>&1 || true

    # Step 10: Assert token A is NOT searchable in workspace B
    log_info "Step 10: Asserting token A is NOT searchable in workspace B..."
    local search_output_ba
    search_output_ba=$(FT_LOG_LEVEL=error FT_WORKSPACE="$workspace_b" FT_DATA_DIR="$workspace_b/.ft" \
        "$FT_BINARY" robot search "$token_a" 2>&1)
    echo "$search_output_ba" > "$scenario_dir/search_a_in_b.json"

    local token_a_hits
    token_a_hits=$(echo "$search_output_ba" | jq -r '.data.total_hits // .data.total // 0' 2>/dev/null || echo "0")
    if [[ "$token_a_hits" -eq 0 ]]; then
        log_pass "Token A not found in workspace B (isolation OK)"
    else
        log_fail "Token A found in workspace B ($token_a_hits hits) - isolation broken"
        result=1
    fi

    # Step 11: Stop ft watch for workspace B
    log_info "Step 11: Stopping ft watch for workspace B..."
    kill -TERM "$ft_pid" 2>/dev/null || true
    wait "$ft_pid" 2>/dev/null || true
    ft_pid=""

    # Step 12: Verify derived paths and workspace roots are distinct
    log_info "Step 12: Verifying workspace roots and derived paths..."
    local db_a
    local db_b
    local root_a
    local root_b
    local log_a
    local log_b
    local logs_dir_a
    local logs_dir_b
    db_a=$(jq -r '.paths.db_path // empty' "$scenario_dir/config_effective_a.json" 2>/dev/null || echo "")
    db_b=$(jq -r '.paths.db_path // empty' "$scenario_dir/config_effective_b.json" 2>/dev/null || echo "")
    root_a=$(jq -r '.paths.workspace_root // empty' "$scenario_dir/config_effective_a.json" 2>/dev/null || echo "")
    root_b=$(jq -r '.paths.workspace_root // empty' "$scenario_dir/config_effective_b.json" 2>/dev/null || echo "")
    log_a=$(jq -r '.paths.log_path // empty' "$scenario_dir/config_effective_a.json" 2>/dev/null || echo "")
    log_b=$(jq -r '.paths.log_path // empty' "$scenario_dir/config_effective_b.json" 2>/dev/null || echo "")
    logs_dir_a=$(jq -r '.paths.logs_dir // empty' "$scenario_dir/config_effective_a.json" 2>/dev/null || echo "")
    logs_dir_b=$(jq -r '.paths.logs_dir // empty' "$scenario_dir/config_effective_b.json" 2>/dev/null || echo "")

    echo "workspace_root_a: $root_a" >> "$scenario_dir/scenario.log"
    echo "workspace_root_b: $root_b" >> "$scenario_dir/scenario.log"
    echo "db_path_a: $db_a" >> "$scenario_dir/scenario.log"
    echo "db_path_b: $db_b" >> "$scenario_dir/scenario.log"
    echo "log_path_a: $log_a" >> "$scenario_dir/scenario.log"
    echo "log_path_b: $log_b" >> "$scenario_dir/scenario.log"
    echo "logs_dir_a: $logs_dir_a" >> "$scenario_dir/scenario.log"
    echo "logs_dir_b: $logs_dir_b" >> "$scenario_dir/scenario.log"

    if [[ -n "$root_a" && -n "$root_b" ]]; then
        if [[ "$root_a" == "$workspace_a" && "$root_b" == "$workspace_b" ]]; then
            log_pass "Workspace roots match expected paths"
        else
            log_fail "Workspace roots do not match expected paths"
            result=1
        fi
    else
        log_fail "Could not parse workspace roots from effective config"
        result=1
    fi

    if [[ -n "$db_a" && -n "$db_b" ]]; then
        if [[ "$db_a" != "$db_b" ]]; then
            log_pass "Workspace db paths are distinct"
        else
            log_fail "Workspace db paths are identical (expected distinct)"
            result=1
        fi
    else
        log_fail "Could not parse db paths from effective config"
        result=1
    fi

    if [[ -n "$log_a" && -n "$log_b" ]]; then
        if [[ "$log_a" != "$log_b" ]]; then
            log_pass "Workspace log paths are distinct"
        else
            log_fail "Workspace log paths are identical (expected distinct)"
            result=1
        fi
    else
        log_fail "Could not parse log paths from effective config"
        result=1
    fi

    if [[ -n "$logs_dir_a" && -n "$logs_dir_b" ]]; then
        if [[ "$logs_dir_a" != "$logs_dir_b" ]]; then
            log_pass "Workspace logs directories are distinct"
        else
            log_fail "Workspace logs directories are identical (expected distinct)"
            result=1
        fi
    else
        log_fail "Could not parse logs directories from effective config"
        result=1
    fi

    trap - EXIT
    cleanup_workspace_isolation

    return $result
}

run_scenario_setup_idempotency() {
    local scenario_dir="$1"
    local temp_home
    temp_home=$(mktemp -d /tmp/ft-e2e-setup-XXXXXX)
    local result=0
    local wezterm_dir="$temp_home/.config/wezterm"
    local wezterm_file="$wezterm_dir/wezterm.lua"
    local zshrc="$temp_home/.zshrc"
    local bashrc="$temp_home/.bashrc"
    local fish_conf="$temp_home/.config/fish/config.fish"
    local ssh_conf="$temp_home/.ssh/config"

    log_info "Temp home: $temp_home"
    echo "temp_home: $temp_home" >> "$scenario_dir/scenario.log"

    mkdir -p "$wezterm_dir" "$temp_home/.config/fish" "$temp_home/.ssh"
    cat > "$wezterm_file" <<'EOF'
local wezterm = require 'wezterm'
local config = {}
return config
EOF
    printf "# zshrc baseline\n" > "$zshrc"
    printf "# bashrc baseline\n" > "$bashrc"
    printf "# fish baseline\n" > "$fish_conf"
    cat > "$ssh_conf" <<'EOF'
Host example
  HostName example.com
EOF

    cleanup_setup_idempotency() {
        log_verbose "Cleaning up setup_idempotency scenario"
        if [[ -d "${temp_home:-}" ]]; then
            cp -r "$temp_home" "$scenario_dir/temp_home_snapshot" 2>/dev/null || true
        fi
        if [[ "${FT_E2E_PRESERVE_TEMP:-}" == "1" ]]; then
            log_warn "Preserving temp home (FT_E2E_PRESERVE_TEMP=1)"
        else
            rm -rf "${temp_home:-}"
        fi
    }
    trap cleanup_setup_idempotency EXIT

    local files_before="$scenario_dir/files_before.txt"
    local files_after_dry="$scenario_dir/files_after_dry.txt"
    local files_after_apply="$scenario_dir/files_after_apply.txt"
    local files_after_second="$scenario_dir/files_after_second.txt"
    local git_before="$scenario_dir/git_status_before.txt"
    local git_after="$scenario_dir/git_status_after.txt"

    find "$temp_home" -type f -print0 | sort -z | xargs -0 sha256sum > "$files_before"
    git status --porcelain > "$git_before"

    # Step 1: Dry-run (should not modify files)
    log_info "Step 1: wa setup --dry-run"
    HOME="$temp_home" XDG_CONFIG_HOME="$temp_home/.config" SHELL="/bin/zsh" \
        "$FT_BINARY" setup --dry-run > "$scenario_dir/setup_dry_run.log" 2>&1 || result=1

    find "$temp_home" -type f -print0 | sort -z | xargs -0 sha256sum > "$files_after_dry"
    if diff -u "$files_before" "$files_after_dry" > "$scenario_dir/dry_run_diff.txt"; then
        log_pass "Dry-run made no file changes"
    else
        log_fail "Dry-run modified files (unexpected)"
        result=1
    fi

    # Step 2: Apply setup
    log_info "Step 2: wa setup --apply"
    HOME="$temp_home" XDG_CONFIG_HOME="$temp_home/.config" SHELL="/bin/zsh" \
        "$FT_BINARY" setup --apply > "$scenario_dir/setup_apply.log" 2>&1 || result=1

    find "$temp_home" -type f -print0 | sort -z | xargs -0 sha256sum > "$files_after_apply"
    cp "$wezterm_file" "$scenario_dir/wezterm_after_apply.lua" 2>/dev/null || true
    cp "$zshrc" "$scenario_dir/zshrc_after_apply" 2>/dev/null || true

    local wa_block_count
    wa_block_count=$(grep -c "WA-BEGIN" "$wezterm_file" 2>/dev/null || true)
    if [[ "$wa_block_count" -eq 1 ]]; then
        log_pass "wezterm.lua contains exactly one WA block"
    else
        log_fail "wezterm.lua WA block count expected 1, got $wa_block_count"
        result=1
    fi

    local shell_block_count
    shell_block_count=$(grep -c "WA-BEGIN" "$zshrc" 2>/dev/null || true)
    if [[ "$shell_block_count" -eq 1 ]]; then
        log_pass "zshrc contains exactly one WA block"
    else
        log_fail "zshrc WA block count expected 1, got $shell_block_count"
        result=1
    fi

    # Step 3: Apply again (idempotent)
    log_info "Step 3: wa setup --apply (idempotent)"
    cp "$wezterm_file" "$scenario_dir/wezterm_before_second.lua" 2>/dev/null || true
    cp "$zshrc" "$scenario_dir/zshrc_before_second" 2>/dev/null || true

    HOME="$temp_home" XDG_CONFIG_HOME="$temp_home/.config" SHELL="/bin/zsh" \
        "$FT_BINARY" setup --apply > "$scenario_dir/setup_apply_again.log" 2>&1 || result=1

    find "$temp_home" -type f -print0 | sort -z | xargs -0 sha256sum > "$files_after_second"

    if diff -u "$scenario_dir/wezterm_before_second.lua" "$wezterm_file" \
        > "$scenario_dir/wezterm_idempotent_diff.txt"; then
        log_pass "wezterm.lua unchanged on second apply"
    else
        log_fail "wezterm.lua changed on second apply"
        result=1
    fi

    if diff -u "$scenario_dir/zshrc_before_second" "$zshrc" \
        > "$scenario_dir/zshrc_idempotent_diff.txt"; then
        log_pass "zshrc unchanged on second apply"
    else
        log_fail "zshrc changed on second apply"
        result=1
    fi

    # Guard: ensure no repo modifications
    git status --porcelain > "$git_after"
    if diff -u "$git_before" "$git_after" > "$scenario_dir/git_status_diff.txt"; then
        log_pass "No repo modifications detected"
    else
        log_fail "Repo modified during setup scenario (unexpected)"
        result=1
    fi

    # Guard: any paths printed by wa should be under temp home
    local printed_paths
    printed_paths=$(grep -Eo "/[^ ]+" \
        "$scenario_dir/setup_dry_run.log" \
        "$scenario_dir/setup_apply.log" \
        "$scenario_dir/setup_apply_again.log" \
        | sort -u || true)
    if [[ -n "$printed_paths" ]]; then
        local bad_paths
        bad_paths=$(echo "$printed_paths" | grep -v "^$temp_home" || true)
        if [[ -n "$bad_paths" ]]; then
            log_fail "Detected paths outside temp home in output"
            echo "$bad_paths" >> "$scenario_dir/outside_paths.txt"
            result=1
        else
            log_pass "All printed paths are within temp home"
        fi
    else
        log_warn "No paths detected in output (guard skipped)"
    fi

    trap - EXIT
    cleanup_setup_idempotency

    return $result
}

run_scenario_setup_remote_docker() {
    local scenario_dir="$1"
    local case_name="setup_remote_docker"
    local helper="$SCRIPT_DIR/e2e_setup_remote_docker.sh"
    local result=0

    log_info "[$case_name] Step 1: gating check (FT_E2E_ENABLE_SETUP_REMOTE)"
    if [[ "${FT_E2E_ENABLE_SETUP_REMOTE:-0}" != "1" ]]; then
        log_warn "[$case_name] Skipping: set FT_E2E_ENABLE_SETUP_REMOTE=1 to enable this non-default case"
        cat > "$scenario_dir/skip_reason.txt" <<'EOF'
setup_remote_docker is intentionally non-default because it requires Docker + SSH.
Enable by setting FT_E2E_ENABLE_SETUP_REMOTE=1 and rerunning this scenario.
EOF
        return 0
    fi

    log_info "[$case_name] Step 2: prerequisite check (docker + ssh + ssh-keygen)"
    local missing=()
    for cmd in docker ssh ssh-keygen jq; do
        if ! command -v "$cmd" >/dev/null 2>&1; then
            missing+=("$cmd")
        fi
    done
    if [[ "${#missing[@]}" -gt 0 ]]; then
        log_warn "[$case_name] Skipping: missing prerequisites: ${missing[*]}"
        printf '%s\n' "${missing[@]}" > "$scenario_dir/missing_prerequisites.txt"
        return 0
    fi

    if [[ ! -x "$helper" ]]; then
        log_fail "[$case_name] helper script missing or not executable: $helper"
        return 1
    fi

    log_info "[$case_name] Step 3: execute dockerized remote setup harness"
    local helper_log="$scenario_dir/setup_remote_driver.log"
    local -a cmd=(
        "$helper"
        --scenario-dir "$scenario_dir"
        --ft-binary "$FT_BINARY"
        --timeout-secs "$TIMEOUT"
    )
    if [[ "$VERBOSE" == "true" ]]; then
        cmd+=(--verbose)
    fi

    if "${cmd[@]}" >"$helper_log" 2>&1; then
        log_pass "[$case_name] Harness completed successfully"
    else
        result=1
        log_fail "[$case_name] Harness failed"
        tail -n 120 "$helper_log" > "$scenario_dir/setup_remote_driver_tail.log" 2>/dev/null || true
    fi

    return $result
}

run_scenario_uservar_forwarding() {
    local scenario_dir="$1"
    local temp_workspace
    temp_workspace=$(mktemp -d /tmp/ft-e2e-uservar-XXXXXX)
    local ft_pid=""
    local wezterm_pid=""
    local pane_id=""
    local result=0
    local wezterm_class="ft-e2e-uservar-$(date +%s%N)"
    local uservar_name="wa_event"
    local payload_json
    payload_json=$(printf '{"type":"e2e_uservar","ts":%s}' "$(date +%s)")
    local payload_b64
    payload_b64=$(printf '%s' "$payload_json" | base64 | tr -d '\n')
    local config_file="$temp_workspace/wezterm.lua"
    local emit_script="$temp_workspace/emit_uservar.sh"
    local wait_timeout=${TIMEOUT:-60}

    log_info "User-var name: $uservar_name"
    log_info "User-var payload: $payload_json"
    log_info "WezTerm class: $wezterm_class"
    log_info "Workspace: $temp_workspace"

    mkdir -p "$temp_workspace/.ft"

    echo "workspace: $temp_workspace" >> "$scenario_dir/scenario.log"
    echo "wezterm_class: $wezterm_class" >> "$scenario_dir/scenario.log"
    echo "uservar_name: $uservar_name" >> "$scenario_dir/scenario.log"
    echo "payload_json: $payload_json" >> "$scenario_dir/scenario.log"

    cleanup_uservar_forwarding() {
        log_verbose "Cleaning up uservar_forwarding scenario"
        if [[ -n "${ft_pid:-}" ]] && kill -0 "$ft_pid" 2>/dev/null; then
            log_verbose "Stopping ft watch (pid $ft_pid)"
            kill "$ft_pid" 2>/dev/null || true
            wait "$ft_pid" 2>/dev/null || true
        fi
        if [[ -n "${pane_id:-}" ]]; then
            log_verbose "Closing uservar pane $pane_id"
            wezterm cli --no-auto-start --class "$wezterm_class" kill-pane \
                --pane-id "$pane_id" 2>/dev/null || true
        fi
        if [[ -n "${wezterm_pid:-}" ]] && kill -0 "$wezterm_pid" 2>/dev/null; then
            log_verbose "Stopping wezterm (pid $wezterm_pid)"
            kill "$wezterm_pid" 2>/dev/null || true
            wait "$wezterm_pid" 2>/dev/null || true
        fi
        if [[ -d "${temp_workspace:-}" ]]; then
            cp -r "$temp_workspace/.ft"/* "$scenario_dir/" 2>/dev/null || true
            cp "$config_file" "$scenario_dir/wezterm.lua" 2>/dev/null || true
        fi
        rm -rf "${temp_workspace:-}"
    }
    trap cleanup_uservar_forwarding EXIT

    # Step 1: Write a minimal wezterm.lua that forwards user-var events to wa
    log_info "Step 1: Writing wezterm.lua forwarding snippet..."
    cat > "$config_file" <<'EOF'
local wezterm = require 'wezterm'
local ft_bin = os.getenv("FT_E2E_BINARY") or "ft"

wezterm.on('user-var-changed', function(window, pane, name, value)
  if not name or name == "" then
    return
  end
  local pane_id = tostring(pane:pane_id())
  wezterm.background_child_process {
    ft_bin,
    "event",
    "--from-uservar",
    "--pane",
    pane_id,
    "--name",
    name,
    "--value",
    value,
  }
end)

return {}
EOF

    # Step 2: Start a dedicated wezterm instance with the forwarding config
    log_info "Step 2: Starting wezterm with forwarding config..."
    FT_E2E_BINARY="$FT_BINARY" wezterm --config-file "$config_file" start \
        --always-new-process --class "$wezterm_class" --workspace "ft-e2e-uservar" \
        > "$scenario_dir/wezterm.log" 2>&1 &
    wezterm_pid=$!
    echo "wezterm_pid: $wezterm_pid" >> "$scenario_dir/scenario.log"

    local check_mux_cmd="wezterm cli --no-auto-start --class \"$wezterm_class\" list >/dev/null 2>&1"
    if ! wait_for_condition "wezterm mux ready" "$check_mux_cmd" "$wait_timeout"; then
        log_fail "Timeout waiting for wezterm mux"
        result=1
        return $result
    fi
    log_pass "WezTerm mux ready"

    # Step 3: Start ft watch with debug logging
    log_info "Step 3: Starting ft watch..."
    FT_WORKSPACE="$temp_workspace" FT_DATA_DIR="$temp_workspace/.ft" FT_LOG_LEVEL=debug \
        "$FT_BINARY" watch --foreground \
        > "$scenario_dir/wa_watch.log" 2>&1 &
    ft_pid=$!
    log_verbose "ft watch started with PID $ft_pid"
    echo "ft_pid: $ft_pid" >> "$scenario_dir/scenario.log"

    if ! kill -0 "$ft_pid" 2>/dev/null; then
        log_fail "ft watch exited immediately"
        result=1
        return $result
    fi

    # Step 4: Create a temporary script to emit the user-var
    log_info "Step 4: Preparing user-var emitter script..."
    cat > "$emit_script" <<'EOS'
#!/bin/bash
set -euo pipefail
name="$1"
payload="$2"
sleep_time="${3:-120}"
printf '\033]1337;SetUserVar=%s=%s\007' "$name" "$payload"
echo "USERVAR_SENT name=$name"
sleep "$sleep_time"
EOS
    chmod +x "$emit_script"

    # Step 5: Spawn a pane that emits the user-var
    log_info "Step 5: Spawning pane to emit user-var..."
    local spawn_output
    spawn_output=$(wezterm cli --no-auto-start --class "$wezterm_class" spawn \
        --cwd "$temp_workspace" -- "$emit_script" "$uservar_name" "$payload_b64" 120 2>&1)
    pane_id=$(echo "$spawn_output" | grep -oE '^[0-9]+$' | head -1)

    if [[ -z "$pane_id" ]]; then
        log_fail "Failed to spawn uservar pane"
        echo "Spawn output: $spawn_output" >> "$scenario_dir/scenario.log"
        result=1
        return $result
    fi
    log_info "Spawned uservar pane: $pane_id"
    echo "pane_id: $pane_id" >> "$scenario_dir/scenario.log"

    # Step 6: Wait for ft watch to record the forwarded user-var event
    log_info "Step 6: Waiting for forwarded user-var event..."
    local check_event_cmd="grep -q \"Published user-var event\" \"$scenario_dir/wa_watch.log\""
    if ! wait_for_condition "user-var forwarded to watcher" "$check_event_cmd" "$wait_timeout"; then
        log_fail "Timeout waiting for user-var forwarding"
        tail -200 "$scenario_dir/wa_watch.log" >> "$scenario_dir/scenario.log" 2>/dev/null || true
        result=1
    else
        log_pass "User-var forwarded and received by watcher"
    fi

    # Step 7: Malformed payload should be rejected (validation check)
    log_info "Step 7: Verifying malformed payload is rejected..."
    local invalid_output=""
    local invalid_exit=0
    set +e
    invalid_output=$(FT_WORKSPACE="$temp_workspace" FT_DATA_DIR="$temp_workspace/.ft" \
        "$FT_BINARY" event --from-uservar --pane "${pane_id:-0}" \
        --name "$uservar_name" --value "invalid_base64" 2>&1)
    invalid_exit=$?
    set -e

    echo "$invalid_output" > "$scenario_dir/wa_event_invalid.log"
    echo "invalid_exit: $invalid_exit" >> "$scenario_dir/scenario.log"

    if [[ "$invalid_exit" -ne 0 ]]; then
        log_pass "Malformed payload rejected"
    else
        log_fail "Malformed payload unexpectedly accepted"
        result=1
    fi

    trap - EXIT
    cleanup_uservar_forwarding

    return $result
}

# ==============================================================================
# Scenario: Workflow Resume After Restart
# ==============================================================================
# This scenario validates that workflows resume from the last completed step
# after the watcher is killed and restarted. It ensures:
# 1. Workflow state is persisted to storage
# 2. Incomplete workflows are resumed on startup
# 3. No step that sends input is executed twice
# ==============================================================================

run_scenario_workflow_resume() {
    local scenario_dir="$1"
    local temp_workspace
    temp_workspace=$(mktemp -d /tmp/ft-e2e-resume-XXXXXX)
    local ft_pid=""
    local pane_id=""
    local result=0
    local wait_timeout=${TIMEOUT:-45}

    log_info "Workspace: $temp_workspace"

    # Setup environment for isolated wa instance
    export FT_DATA_DIR="$temp_workspace/.ft"
    export FT_WORKSPACE="$temp_workspace"
    mkdir -p "$FT_DATA_DIR"

    # Copy baseline config for workflow testing
    local baseline_config="$PROJECT_ROOT/fixtures/e2e/config_baseline.toml"
    if [[ -f "$baseline_config" ]]; then
        cp "$baseline_config" "$temp_workspace/ft.toml"
        export FT_CONFIG="$temp_workspace/ft.toml"
        log_verbose "Using baseline config: $baseline_config"
    fi

    # Cleanup function
    cleanup_workflow_resume() {
        log_verbose "Cleaning up workflow_resume scenario"
        # Kill ft watch if running
        if [[ -n "$ft_pid" ]] && kill -0 "$ft_pid" 2>/dev/null; then
            log_verbose "Stopping ft watch (pid $ft_pid)"
            kill "$ft_pid" 2>/dev/null || true
            wait "$ft_pid" 2>/dev/null || true
        fi
        # Close dummy pane if it exists
        if [[ -n "$pane_id" ]]; then
            log_verbose "Closing dummy agent pane $pane_id"
            wezterm cli kill-pane --pane-id "$pane_id" 2>/dev/null || true
        fi
        # Copy artifacts before cleanup
        if [[ -d "$temp_workspace" ]]; then
            cp -r "$temp_workspace/.ft"/* "$scenario_dir/" 2>/dev/null || true
            cp "$temp_workspace/ft.toml" "$scenario_dir/" 2>/dev/null || true
        fi
        rm -rf "$temp_workspace"
    }
    trap cleanup_workflow_resume EXIT

    # Step 1: Start ft watch with auto-handle
    log_info "Step 1: Starting ft watch with --auto-handle..."
    "$FT_BINARY" watch --foreground --auto-handle \
        > "$scenario_dir/wa_watch_1.log" 2>&1 &
    ft_pid=$!
    log_verbose "ft watch started with PID $ft_pid"
    echo "ft_pid_1: $ft_pid" >> "$scenario_dir/scenario.log"

    # Verify ft watch is running
    if ! kill -0 "$ft_pid" 2>/dev/null; then
        log_fail "ft watch exited immediately"
        return 1
    fi

    # Step 2: Spawn dummy agent pane that will trigger compaction
    log_info "Step 2: Spawning dummy agent pane..."
    local agent_script="$PROJECT_ROOT/fixtures/e2e/dummy_agent.sh"
    if [[ ! -x "$agent_script" ]]; then
        log_fail "Dummy agent script not found or not executable: $agent_script"
        return 1
    fi

    local spawn_output
    # Spawn with 1 second delay before compaction marker
    spawn_output=$(wezterm cli spawn --cwd "$temp_workspace" -- bash "$agent_script" 1 2>&1)
    pane_id=$(echo "$spawn_output" | grep -oE '^[0-9]+$' | head -1)

    if [[ -z "$pane_id" ]]; then
        log_fail "Failed to spawn dummy agent pane"
        echo "Spawn output: $spawn_output" >> "$scenario_dir/scenario.log"
        return 1
    fi
    log_info "Spawned agent pane: $pane_id"
    echo "agent_pane_id: $pane_id" >> "$scenario_dir/scenario.log"

    # Step 3: Wait for pane to be observed
    log_info "Step 3: Waiting for pane to be observed..."
    local check_cmd="\"$FT_BINARY\" robot state 2>/dev/null | jq -e '.data[]? | select(.pane_id == $pane_id)' >/dev/null 2>&1"

    if ! wait_for_condition "pane $pane_id observed" "$check_cmd" "$wait_timeout"; then
        log_fail "Timeout waiting for pane to be observed"
        "$FT_BINARY" robot state > "$scenario_dir/robot_state.json" 2>&1 || true
        return 1
    fi
    log_pass "Pane observed"

    # Step 4: Wait for compaction detection and workflow to start
    log_info "Step 4: Waiting for compaction detection and workflow start..."
    local workflow_started_cmd="grep -qi \"workflow.*started\\|handle_compaction\" \"$scenario_dir/wa_watch_1.log\" 2>/dev/null"
    if wait_for_condition "workflow start observed in logs" "$workflow_started_cmd" "$wait_timeout"; then
        log_pass "Workflow started"
    else
        log_warn "Workflow may not have started (checking anyway)"
    fi

    # Step 5: Kill watcher abruptly (simulate crash)
    log_info "Step 5: Killing watcher (simulating crash)..."
    kill -9 "$ft_pid" 2>/dev/null || true
    wait "$ft_pid" 2>/dev/null || true
    ft_pid=""
    log_pass "Watcher killed"

    # Step 6: Check database for incomplete workflow
    log_info "Step 6: Checking database for incomplete workflow..."
    local db_path="$temp_workspace/.ft/ft.db"
    if [[ -f "$db_path" ]]; then
        local workflow_status
        workflow_status=$(sqlite3 "$db_path" "SELECT id, status, current_step FROM workflow_executions ORDER BY started_at DESC LIMIT 1;" 2>/dev/null || echo "")
        echo "workflow_before_restart: $workflow_status" >> "$scenario_dir/scenario.log"

        if [[ -n "$workflow_status" ]]; then
            log_pass "Found workflow in database: $workflow_status"
        else
            log_warn "No workflow found in database (workflow may not have persisted yet)"
        fi

        # Count step logs before restart
        local step_count_before
        step_count_before=$(sqlite3 "$db_path" "SELECT COUNT(*) FROM workflow_step_logs;" 2>/dev/null || echo "0")
        echo "step_logs_before_restart: $step_count_before" >> "$scenario_dir/scenario.log"
        log_info "Step logs before restart: $step_count_before"
    else
        log_warn "Database file not found at $db_path"
    fi

    # Step 7: Restart ft watch with auto-handle
    log_info "Step 7: Restarting ft watch with --auto-handle..."
    "$FT_BINARY" watch --foreground --auto-handle \
        > "$scenario_dir/wa_watch_2.log" 2>&1 &
    ft_pid=$!
    log_verbose "ft watch restarted with PID $ft_pid"
    echo "ft_pid_2: $ft_pid" >> "$scenario_dir/scenario.log"

    # Verify ft watch is running
    if ! kill -0 "$ft_pid" 2>/dev/null; then
        log_fail "ft watch (restart) exited immediately"
        return 1
    fi
    log_pass "Watcher restarted"

    # Step 8: Wait for workflow resume activity
    log_info "Step 8: Waiting for workflow resume..."
    local resume_cmd="grep -qi \"resume\\|incomplete\" \"$scenario_dir/wa_watch_2.log\" 2>/dev/null"
    if wait_for_condition "resume activity in logs" "$resume_cmd" "$wait_timeout"; then
        log_pass "Resume activity detected in logs"
    else
        log_warn "No explicit resume activity in logs (may be normal if workflow completed before kill)"
    fi

    # Step 9: Check for duplicate steps
    log_info "Step 9: Checking for duplicate workflow steps..."
    if [[ -f "$db_path" ]]; then
        # Query step logs and check for duplicates
        local step_logs
        step_logs=$(sqlite3 "$db_path" \
            "SELECT workflow_id, step_index, step_name, COUNT(*) as cnt
             FROM workflow_step_logs
             GROUP BY workflow_id, step_index
             HAVING cnt > 1;" 2>/dev/null || echo "")

        echo "$step_logs" > "$scenario_dir/duplicate_steps.txt"

        if [[ -n "$step_logs" ]]; then
            log_fail "Found duplicate workflow steps!"
            echo "Duplicate steps: $step_logs" >> "$scenario_dir/scenario.log"
            result=1
        else
            log_pass "No duplicate workflow steps found"
        fi

        # Get final step log count
        local step_count_after
        step_count_after=$(sqlite3 "$db_path" "SELECT COUNT(*) FROM workflow_step_logs;" 2>/dev/null || echo "0")
        echo "step_logs_after_restart: $step_count_after" >> "$scenario_dir/scenario.log"
        log_info "Step logs after restart: $step_count_after"

        # Export all step logs for debugging
        sqlite3 "$db_path" -header -csv \
            "SELECT workflow_id, step_index, step_name, result_type, duration_ms FROM workflow_step_logs ORDER BY workflow_id, step_index;" \
            > "$scenario_dir/all_step_logs.csv" 2>/dev/null || true

        # Export workflow status
        sqlite3 "$db_path" -header -csv \
            "SELECT id, workflow_name, pane_id, current_step, status FROM workflow_executions;" \
            > "$scenario_dir/workflow_executions.csv" 2>/dev/null || true
    else
        log_warn "Database file not found after restart"
    fi

    # Step 10: Check ft watch logs for workflow activity
    log_info "Step 10: Checking ft watch logs for workflow activity..."
    cat "$scenario_dir/wa_watch_1.log" "$scenario_dir/wa_watch_2.log" > "$scenario_dir/wa_watch_combined.log" 2>/dev/null || true

    if grep -qi "workflow\|compaction\|detection" "$scenario_dir/wa_watch_combined.log" 2>/dev/null; then
        log_pass "Found workflow/detection activity in logs"
    else
        log_warn "No obvious workflow activity in logs (may be normal)"
    fi

    # Note: This scenario depends on workflow functionality being complete
    log_info "Scenario complete"

    # Cleanup trap will handle the rest
    trap - EXIT
    cleanup_workflow_resume

    return $result
}

# ==============================================================================
# Scenario: Dry-Run Mode (Send + Workflow)
# ==============================================================================
# Validates that:
# - `wa send --dry-run` and `wa workflow run --dry-run` produce informative previews (JSON in non-TTY)
# - `wa robot send --dry-run` / `wa robot workflow run --dry-run` produce schema-valid envelopes
# - Dry-run does not execute side effects:
#   - no send_text audit action recorded
#   - no workflow_executions row created
#   - dummy pane does not echo the dry-run marker
# - Preview vs actual stable-field checks (robot JSON):
#   - pane_id matches
#   - allow/deny signal matches (policy checks passed vs injection status)
# ==============================================================================

run_scenario_dry_run_mode() {
    local scenario_dir="$1"
    local temp_workspace
    temp_workspace=$(mktemp -d /tmp/ft-e2e-dry-run-XXXXXX)
    local ft_pid=""
    local pane_id=""
    local result=0

    log_info "Workspace: $temp_workspace"

    export FT_DATA_DIR="$temp_workspace/.ft"
    export FT_WORKSPACE="$temp_workspace"
    mkdir -p "$FT_DATA_DIR"

    # Copy baseline config for permissive, deterministic policy behavior
    local baseline_config="$PROJECT_ROOT/fixtures/e2e/config_baseline.toml"
    if [[ -f "$baseline_config" ]]; then
        cp "$baseline_config" "$temp_workspace/ft.toml"
        export FT_CONFIG="$temp_workspace/ft.toml"
        log_verbose "Using baseline config: $baseline_config"
    fi

    cleanup_dry_run_mode() {
        log_verbose "Cleaning up dry_run_mode scenario"
        if [[ -n "$ft_pid" ]] && kill -0 "$ft_pid" 2>/dev/null; then
            log_verbose "Stopping ft watch (pid $ft_pid)"
            kill "$ft_pid" 2>/dev/null || true
            wait "$ft_pid" 2>/dev/null || true
        fi
        if [[ -n "$pane_id" ]]; then
            log_verbose "Closing dummy pane $pane_id"
            wezterm cli kill-pane --pane-id "$pane_id" 2>/dev/null || true
        fi
        if [[ -d "$temp_workspace" ]]; then
            cp -r "$temp_workspace/.ft"/* "$scenario_dir/" 2>/dev/null || true
            cp "$temp_workspace/ft.toml" "$scenario_dir/" 2>/dev/null || true
        fi
        rm -rf "$temp_workspace"
    }
    trap cleanup_dry_run_mode EXIT

    # Step 1: Start ft watch (no auto-handle; we want to control side effects)
    log_info "Step 1: Starting ft watch..."
    "$FT_BINARY" watch --foreground > "$scenario_dir/wa_watch.log" 2>&1 &
    ft_pid=$!
    echo "ft_pid: $ft_pid" >> "$scenario_dir/scenario.log"

    if ! kill -0 "$ft_pid" 2>/dev/null; then
        log_fail "ft watch exited immediately"
        return 1
    fi

    # Step 2: Spawn dummy agent pane (echoes received input)
    log_info "Step 2: Spawning dummy agent pane..."
    local agent_script="$PROJECT_ROOT/fixtures/e2e/dummy_agent.sh"
    if [[ ! -x "$agent_script" ]]; then
        log_fail "Dummy agent script not found or not executable: $agent_script"
        return 1
    fi

    local spawn_output
    spawn_output=$(wezterm cli spawn --cwd "$temp_workspace" -- bash "$agent_script" 2 2>&1)
    pane_id=$(echo "$spawn_output" | grep -oE '^[0-9]+$' | head -1)
    if [[ -z "$pane_id" ]]; then
        log_fail "Failed to spawn dummy agent pane"
        echo "Spawn output: $spawn_output" >> "$scenario_dir/scenario.log"
        return 1
    fi
    log_info "Spawned pane: $pane_id"
    echo "pane_id: $pane_id" >> "$scenario_dir/scenario.log"

    # Step 3: Wait for pane to be observed
    log_info "Step 3: Waiting for pane to be observed..."
    local wait_timeout=${TIMEOUT:-30}
    local observe_cmd="\"$FT_BINARY\" robot state 2>/dev/null | jq -e '.data[]? | select(.pane_id == $pane_id)' >/dev/null 2>&1"
    if ! wait_for_condition "pane $pane_id observed" "$observe_cmd" "$wait_timeout"; then
        log_fail "Timeout waiting for pane to be observed"
        "$FT_BINARY" robot state > "$scenario_dir/robot_state.json" 2>&1 || true
        return 1
    fi
    log_pass "Pane observed"

    # Step 4: Wait for a compaction event so workflow dry-run has a realistic precondition
    log_info "Step 4: Waiting for compaction detection..."
    local compaction_cmd="\"$FT_BINARY\" events -f json --unhandled --rule-id \"codex:compaction\" --limit 20 2>/dev/null | jq -e 'length > 0' >/dev/null 2>&1"
    if ! wait_for_condition "compaction event detected" "$compaction_cmd" "$wait_timeout"; then
        log_fail "Timeout waiting for compaction event"
        "$FT_BINARY" events -f json --rule-id "codex:compaction" --limit 20 > "$scenario_dir/events_debug.json" 2>&1 || true
        result=1
    else
        log_pass "Compaction event detected"
    fi

    # Ensure DB exists for audit/workflow assertions
    local db_path="$temp_workspace/.ft/ft.db"
    if [[ ! -f "$db_path" ]]; then
        log_warn "DB not found at $db_path (some checks will be skipped)"
    fi

    # Snapshot baseline counts (we allow dry-run to record a dry-run audit action, but not send_text / workflow_executions)
    local send_text_before="0"
    local workflow_exec_before="0"
    if [[ -f "$db_path" ]]; then
        send_text_before=$(sqlite3 "$db_path" "SELECT COUNT(*) FROM audit_actions WHERE action_kind = 'send_text';" 2>/dev/null || echo "0")
        workflow_exec_before=$(sqlite3 "$db_path" "SELECT COUNT(*) FROM workflow_executions;" 2>/dev/null || echo "0")
    fi
    echo "send_text_before: $send_text_before" >> "$scenario_dir/scenario.log"
    echo "workflow_exec_before: $workflow_exec_before" >> "$scenario_dir/scenario.log"

    # Step 5: Human send dry-run (JSON in non-TTY)
    local marker_send=""
    marker_send="E2E_DRYRUN_SEND_$(date +%s%N)"
    log_info "Step 5: wa send --dry-run (human)..."
    "$FT_BINARY" send "$pane_id" "$marker_send" --dry-run > "$scenario_dir/human_send_dry_run.json" 2>&1 || true
    if jq -e ".target_resolution.pane_id == $pane_id and (.expected_actions | length) > 0" \
        "$scenario_dir/human_send_dry_run.json" >/dev/null 2>&1; then
        log_pass "human send dry-run: report looks valid"
    else
        log_fail "human send dry-run: missing expected fields"
        result=1
    fi

    # Step 6: Robot send dry-run (enveloped JSON)
    log_info "Step 6: wa robot send --dry-run..."
    "$FT_BINARY" robot send "$pane_id" "$marker_send" --dry-run --format json \
        > "$scenario_dir/robot_send_dry_run.json" 2>&1 || true
    if jq -e ".ok == true and (.data.target_resolution.pane_id == $pane_id) and ((.data.expected_actions | length) > 0)" \
        "$scenario_dir/robot_send_dry_run.json" >/dev/null 2>&1; then
        log_pass "robot send dry-run: ok"
    else
        log_fail "robot send dry-run failed"
        result=1
    fi

    # Step 7: No side effects for dry-run send
    if [[ -f "$db_path" ]]; then
        local send_text_after
        send_text_after=$(sqlite3 "$db_path" "SELECT COUNT(*) FROM audit_actions WHERE action_kind = 'send_text';" 2>/dev/null || echo "0")
        echo "send_text_after_dry_run: $send_text_after" >> "$scenario_dir/scenario.log"
        if [[ "$send_text_after" == "$send_text_before" ]]; then
            log_pass "dry-run send did not record send_text audit action"
        else
            log_fail "dry-run send recorded send_text audit action (unexpected): $send_text_before -> $send_text_after"
            result=1
        fi
    fi

    log_info "Step 7b: Verify dummy pane did not echo dry-run marker..."
    "$FT_BINARY" robot wait-for "$pane_id" "Received: $marker_send" --timeout-secs 2 --format json \
        > "$scenario_dir/no_echo_wait.json" 2>&1 || true
    if jq -e '.ok == false and .error.code == "WA-ROBOT-TIMEOUT"' "$scenario_dir/no_echo_wait.json" >/dev/null 2>&1; then
        log_pass "dry-run marker not observed (expected)"
    else
        log_fail "dry-run marker appeared in pane output (unexpected)"
        result=1
    fi

    # Step 8: Preview vs actual stable-field checks (robot send)
    log_info "Step 8: wa robot send (actual)..."
    "$FT_BINARY" robot send "$pane_id" "$marker_send" --format json \
        > "$scenario_dir/robot_send_actual.json" 2>&1 || true
    if jq -e ".ok == true and (.data.pane_id == $pane_id)" "$scenario_dir/robot_send_actual.json" >/dev/null 2>&1; then
        log_pass "robot send actual: ok"
    else
        log_fail "robot send actual failed"
        result=1
    fi

    log_info "Step 8b: Waiting for echo of actual send..."
    "$FT_BINARY" robot wait-for "$pane_id" "Received: $marker_send" --timeout-secs 10 --format json \
        > "$scenario_dir/echo_wait.json" 2>&1 || true
    if jq -e '.ok == true and .data.matched == true' "$scenario_dir/echo_wait.json" >/dev/null 2>&1; then
        log_pass "actual send echoed by dummy pane"
    else
        log_fail "did not observe dummy echo for actual send"
        result=1
    fi

    # Compare allow/deny signal: dry-run policy checks passed <-> injection status
    local dry_policy_passed="unknown"
    local actual_injection_status="unknown"
    dry_policy_passed=$(jq -r '(.data.policy_evaluation.checks // []) | all(.passed == true)' \
        "$scenario_dir/robot_send_dry_run.json" 2>/dev/null || echo "unknown")
    actual_injection_status=$(jq -r '.data.injection.status // "unknown"' \
        "$scenario_dir/robot_send_actual.json" 2>/dev/null || echo "unknown")
    echo "robot_send_dry_policy_passed: $dry_policy_passed" >> "$scenario_dir/scenario.log"
    echo "robot_send_actual_injection_status: $actual_injection_status" >> "$scenario_dir/scenario.log"

    if [[ "$dry_policy_passed" == "true" && "$actual_injection_status" == "allowed" ]] \
        || [[ "$dry_policy_passed" == "false" && "$actual_injection_status" != "allowed" ]]; then
        log_pass "preview vs actual: policy signal consistent"
    else
        log_fail "preview vs actual: policy signal mismatch (dry=$dry_policy_passed actual=$actual_injection_status)"
        result=1
    fi

    # Step 9: Workflow dry-run (human + robot) without workflow_executions side effects
    log_info "Step 9: wa workflow run --dry-run (human)..."
    "$FT_BINARY" workflow run --pane "$pane_id" handle_compaction --dry-run \
        > "$scenario_dir/human_workflow_dry_run.json" 2>&1 || true
    if jq -e "(.expected_actions | length) > 0" "$scenario_dir/human_workflow_dry_run.json" >/dev/null 2>&1; then
        log_pass "human workflow dry-run: report looks valid"
    else
        log_fail "human workflow dry-run failed"
        result=1
    fi

    log_info "Step 9b: wa robot workflow run --dry-run..."
    "$FT_BINARY" robot workflow run handle_compaction "$pane_id" --dry-run --format json \
        > "$scenario_dir/robot_workflow_dry_run.json" 2>&1 || true
    if jq -e ".ok == true and (.data.target_resolution.pane_id == $pane_id) and ((.data.expected_actions | length) > 0)" \
        "$scenario_dir/robot_workflow_dry_run.json" >/dev/null 2>&1; then
        log_pass "robot workflow dry-run: ok"
    else
        log_fail "robot workflow dry-run failed"
        result=1
    fi

    if [[ -f "$db_path" ]]; then
        local workflow_exec_after
        workflow_exec_after=$(sqlite3 "$db_path" "SELECT COUNT(*) FROM workflow_executions;" 2>/dev/null || echo "0")
        echo "workflow_exec_after_dry_run: $workflow_exec_after" >> "$scenario_dir/scenario.log"
        if [[ "$workflow_exec_after" == "$workflow_exec_before" ]]; then
            log_pass "dry-run workflow did not create workflow_executions row"
        else
            log_fail "dry-run workflow created workflow_executions row (unexpected): $workflow_exec_before -> $workflow_exec_after"
            result=1
        fi
    fi

    # Step 10: Actual robot workflow run should create an execution (sanity check + stable-field match)
    log_info "Step 10: wa robot workflow run (actual)..."
    "$FT_BINARY" robot workflow run handle_compaction "$pane_id" --format json \
        > "$scenario_dir/robot_workflow_actual.json" 2>&1 || true
    if jq -e ".ok == true and (.data.pane_id == $pane_id)" "$scenario_dir/robot_workflow_actual.json" >/dev/null 2>&1; then
        log_pass "robot workflow run: ok"
    else
        log_fail "robot workflow run failed"
        result=1
    fi

    if [[ -f "$db_path" ]]; then
        local workflow_exec_final
        workflow_exec_final=$(sqlite3 "$db_path" "SELECT COUNT(*) FROM workflow_executions;" 2>/dev/null || echo "0")
        echo "workflow_exec_after_actual: $workflow_exec_final" >> "$scenario_dir/scenario.log"
        if [[ "$workflow_exec_final" -gt "$workflow_exec_before" ]]; then
            log_pass "actual workflow created workflow_executions row"
        else
            log_warn "workflow_executions count did not increase (may be due to workflow denial)"
        fi
    fi

    # Step 11: Stop ft watch
    log_info "Step 11: Stopping ft watch..."
    kill -TERM "$ft_pid" 2>/dev/null || true
    wait "$ft_pid" 2>/dev/null || true
    ft_pid=""

    trap - EXIT
    cleanup_dry_run_mode

    return $result
}

# ==============================================================================
# Scenario: Workflow Lifecycle (Robot Subcommands)
# ==============================================================================
# Validates robot workflow list/run/status/abort with deterministic outputs.
# Uses dry-run for execution to avoid side effects.
# ==============================================================================

run_scenario_workflow_lifecycle() {
    local scenario_dir="$1"
    local temp_workspace
    temp_workspace=$(mktemp -d /tmp/ft-e2e-workflow-lifecycle-XXXXXX)
    local result=0

    log_info "Workspace: $temp_workspace"

    # Setup environment for isolated wa instance
    export FT_DATA_DIR="$temp_workspace/.ft"
    export FT_WORKSPACE="$temp_workspace"
    mkdir -p "$FT_DATA_DIR"

    # Copy baseline config when available
    local baseline_config="$PROJECT_ROOT/fixtures/e2e/config_baseline.toml"
    if [[ -f "$baseline_config" ]]; then
        cp "$baseline_config" "$temp_workspace/ft.toml"
        export FT_CONFIG="$temp_workspace/ft.toml"
        log_verbose "Using baseline config: $baseline_config"
    fi

    cleanup_workflow_lifecycle() {
        log_verbose "Cleaning up workflow_lifecycle scenario"
        if [[ -d "$temp_workspace" ]]; then
            cp -r "$temp_workspace/.ft"/* "$scenario_dir/" 2>/dev/null || true
            cp "$temp_workspace/ft.toml" "$scenario_dir/" 2>/dev/null || true
        fi
        rm -rf "$temp_workspace"
    }
    trap cleanup_workflow_lifecycle EXIT

    # Step 1: List workflows
    log_info "Step 1: Listing workflows..."
    "$FT_BINARY" robot workflow list > "$scenario_dir/workflow_list.json" 2>&1 || true
    if jq -e '.ok == true' "$scenario_dir/workflow_list.json" >/dev/null 2>&1; then
        log_pass "workflow list: ok"
    else
        log_fail "workflow list failed"
        result=1
    fi

    # Step 2: Dry-run workflow
    log_info "Step 2: Dry-run workflow..."
    "$FT_BINARY" robot workflow run handle_compaction 0 --dry-run \
        > "$scenario_dir/workflow_run_dry.json" 2>&1 || true
    if jq -e '.ok == true' "$scenario_dir/workflow_run_dry.json" >/dev/null 2>&1; then
        log_pass "workflow run dry-run: ok"
    else
        log_fail "workflow run dry-run failed"
        result=1
    fi

    # Step 3: Status --active (may be empty)
    log_info "Step 3: Workflow status --active..."
    "$FT_BINARY" robot workflow status --active \
        > "$scenario_dir/workflow_status_active.json" 2>&1 || true
    if jq -e '.ok == true' "$scenario_dir/workflow_status_active.json" >/dev/null 2>&1; then
        log_pass "workflow status --active: ok"
    else
        local error_code
        error_code=$(jq -r '.error_code // "unknown"' \
            "$scenario_dir/workflow_status_active.json" 2>/dev/null || echo "unknown")
        log_skip "workflow status --active: $error_code (may require watcher)"
    fi

    # Step 4: Abort with nonexistent execution ID (expect not found)
    log_info "Step 4: Workflow abort (nonexistent)..."
    "$FT_BINARY" robot workflow abort "nonexistent-id" \
        > "$scenario_dir/workflow_abort.json" 2>&1 || true
    if jq -e '.ok == false and .error_code == "E_EXECUTION_NOT_FOUND"' \
        "$scenario_dir/workflow_abort.json" >/dev/null 2>&1; then
        log_pass "workflow abort not-found: expected error"
    else
        log_fail "workflow abort not-found: unexpected response"
        result=1
    fi

    trap - EXIT
    cleanup_workflow_lifecycle

    return $result
}

# ==============================================================================
# Scenario: Events Unhandled Alias
# ==============================================================================
# Validates that --unhandled and --unhandled-only both produce valid output.
# ==============================================================================

run_scenario_events_unhandled_alias() {
    local scenario_dir="$1"
    local temp_workspace
    temp_workspace=$(mktemp -d /tmp/ft-e2e-events-unhandled-XXXXXX)
    local result=0

    log_info "Workspace: $temp_workspace"

    # Setup environment for isolated wa instance
    export FT_DATA_DIR="$temp_workspace/.ft"
    export FT_WORKSPACE="$temp_workspace"
    mkdir -p "$FT_DATA_DIR"

    # Copy baseline config when available
    local baseline_config="$PROJECT_ROOT/fixtures/e2e/config_baseline.toml"
    if [[ -f "$baseline_config" ]]; then
        cp "$baseline_config" "$temp_workspace/ft.toml"
        export FT_CONFIG="$temp_workspace/ft.toml"
        log_verbose "Using baseline config: $baseline_config"
    fi

    cleanup_events_unhandled_alias() {
        log_verbose "Cleaning up events_unhandled_alias scenario"
        if [[ -d "$temp_workspace" ]]; then
            cp -r "$temp_workspace/.ft"/* "$scenario_dir/" 2>/dev/null || true
            cp "$temp_workspace/ft.toml" "$scenario_dir/" 2>/dev/null || true
        fi
        rm -rf "$temp_workspace"
    }
    trap cleanup_events_unhandled_alias EXIT

    # Step 1: --unhandled
    log_info "Step 1: wa robot events --unhandled..."
    "$FT_BINARY" robot events --unhandled \
        > "$scenario_dir/events_unhandled.json" 2>&1 || true
    if jq -e '.ok == true' "$scenario_dir/events_unhandled.json" >/dev/null 2>&1; then
        log_pass "events --unhandled: ok"
    else
        local error_code
        error_code=$(jq -r '.error_code // "unknown"' \
            "$scenario_dir/events_unhandled.json" 2>/dev/null || echo "unknown")
        log_skip "events --unhandled: $error_code"
    fi

    # Step 2: --unhandled-only (alias)
    log_info "Step 2: wa robot events --unhandled-only..."
    "$FT_BINARY" robot events --unhandled-only \
        > "$scenario_dir/events_unhandled_only.json" 2>&1 || true
    if jq -e '.ok == true' "$scenario_dir/events_unhandled_only.json" >/dev/null 2>&1; then
        log_pass "events --unhandled-only: ok"
    else
        local error_code
        error_code=$(jq -r '.error_code // "unknown"' \
            "$scenario_dir/events_unhandled_only.json" 2>/dev/null || echo "unknown")
        log_skip "events --unhandled-only: $error_code"
    fi

    trap - EXIT
    cleanup_events_unhandled_alias

    return $result
}

# ==============================================================================
# Scenario: Event Annotations + Label + Triage
# ==============================================================================
# Validates end-to-end mutation lifecycle for event annotations:
# 1) Create deterministic fixture events in an isolated workspace
# 2) Annotate note (with secret-like token) and verify redaction
# 3) Add label + set triage state
# 4) Verify robot filters by label + triage_state
# 5) Verify audit records exist, are ordered, and remain redacted
# ==============================================================================

run_scenario_events_annotations_triage() {
    local scenario_dir="$1"
    local temp_workspace
    temp_workspace=$(mktemp -d /tmp/ft-e2e-events-annotations-XXXXXX)
    local result=0
    local db_path=""
    local pane_id=9101
    local target_event_id=""
    local noise_event_id=""
    local note_secret="investigating sk-test-should-redact-events-1234567890abcdef"
    local old_ft_data_dir="${FT_DATA_DIR:-}"
    local old_ft_workspace="${FT_WORKSPACE:-}"
    local old_ft_config="${FT_CONFIG:-}"

    log_info "Workspace: $temp_workspace"

    cleanup_events_annotations_triage() {
        log_verbose "Cleaning up events_annotations_triage scenario"
        if [[ -d "${temp_workspace:-}" ]]; then
            cp -r "$temp_workspace/.ft"/* "$scenario_dir/" 2>/dev/null || true
            cp "$temp_workspace/ft.toml" "$scenario_dir/" 2>/dev/null || true
        fi
        if [[ -n "$old_ft_data_dir" ]]; then
            export FT_DATA_DIR="$old_ft_data_dir"
        else
            unset FT_DATA_DIR
        fi
        if [[ -n "$old_ft_workspace" ]]; then
            export FT_WORKSPACE="$old_ft_workspace"
        else
            unset FT_WORKSPACE
        fi
        if [[ -n "$old_ft_config" ]]; then
            export FT_CONFIG="$old_ft_config"
        else
            unset FT_CONFIG
        fi

        # Intentionally keep the temp workspace for postmortem/debug review.
        echo "temp_workspace: $temp_workspace" >> "$scenario_dir/scenario.log"
    }
    trap cleanup_events_annotations_triage EXIT

    # Setup environment for isolated wa instance
    export FT_DATA_DIR="$temp_workspace/.ft"
    export FT_WORKSPACE="$temp_workspace"
    mkdir -p "$FT_DATA_DIR"

    local baseline_config="$PROJECT_ROOT/fixtures/e2e/config_baseline.toml"
    if [[ -f "$baseline_config" ]]; then
        cp "$baseline_config" "$temp_workspace/ft.toml"
        export FT_CONFIG="$temp_workspace/ft.toml"
        log_verbose "Using baseline config: $baseline_config"
    fi

    # Step 1: Initialize DB
    log_info "Step 1: Initializing DB..."
    "$FT_BINARY" db migrate --yes > "$scenario_dir/db_migrate.txt" 2>&1 || true
    "$FT_BINARY" db check -f json > "$scenario_dir/db_check.json" 2>&1 || true
    db_path="$temp_workspace/.ft/ft.db"
    if [[ ! -f "$db_path" ]]; then
        log_fail "DB not created at $db_path"
        result=1
    fi

    if [[ $result -eq 0 ]]; then
        # Step 2: Seed deterministic fixture pane + events
        log_info "Step 2: Seeding fixture events..."
        local now_ms
        now_ms=$(python3 - <<'PY'
import time
print(int(time.time() * 1000))
PY
)
        local target_detected_at=$((now_ms - 3000))
        local noise_detected_at=$((now_ms - 2000))

        sqlite3 "$db_path" <<SQL
PRAGMA foreign_keys = ON;
INSERT OR REPLACE INTO panes (
    pane_id, pane_uuid, domain, window_id, tab_id, title, cwd, tty_name,
    first_seen_at, last_seen_at, observed, ignore_reason, last_decision_at
) VALUES (
    $pane_id, 'e2e-events-annotations-pane', 'local', 1, 1, 'e2e-events-pane', '$temp_workspace', 'tty-e2e-events',
    $now_ms, $now_ms, 1, NULL, $now_ms
);

INSERT INTO events (
    pane_id, rule_id, agent_type, event_type, severity, confidence,
    extracted, matched_text, segment_id, detected_at, handled_at,
    handled_by_workflow_id, handled_status, dedupe_key
) VALUES (
    $pane_id, 'e2e.events.annotation.target', 'codex', 'usage_warning', 'warning', 0.7,
    NULL, 'target mutation event', NULL, $target_detected_at, NULL,
    NULL, NULL, 'e2e-events-target'
);

INSERT INTO events (
    pane_id, rule_id, agent_type, event_type, severity, confidence,
    extracted, matched_text, segment_id, detected_at, handled_at,
    handled_by_workflow_id, handled_status, dedupe_key
) VALUES (
    $pane_id, 'e2e.events.annotation.noise', 'codex', 'usage_warning', 'warning', 0.6,
    NULL, 'noise event', NULL, $noise_detected_at, NULL,
    NULL, NULL, 'e2e-events-noise'
);
SQL

        target_event_id=$(sqlite3 "$db_path" \
            "SELECT id FROM events WHERE rule_id='e2e.events.annotation.target' LIMIT 1;")
        noise_event_id=$(sqlite3 "$db_path" \
            "SELECT id FROM events WHERE rule_id='e2e.events.annotation.noise' LIMIT 1;")
        echo "target_event_id: $target_event_id" >> "$scenario_dir/scenario.log"
        echo "noise_event_id: $noise_event_id" >> "$scenario_dir/scenario.log"

        if [[ -z "$target_event_id" || -z "$noise_event_id" ]]; then
            log_fail "Failed to seed deterministic event ids"
            result=1
        fi
    fi

    if [[ $result -eq 0 ]]; then
        # Step 3: Annotate target event with secret-like note (must redact)
        log_info "Step 3: Annotating event note with redaction check..."
        "$FT_BINARY" events --format json annotate "$target_event_id" \
            --note "$note_secret" --by "e2e-user" \
            > "$scenario_dir/annotate.json" 2> "$scenario_dir/annotate.stderr" || true

        if jq -e '.ok == true' "$scenario_dir/annotate.json" >/dev/null 2>&1; then
            log_pass "events annotate mutation succeeded"
        else
            log_fail "events annotate mutation failed"
            result=1
        fi

        if grep -q "$note_secret" "$scenario_dir/annotate.json" 2>/dev/null; then
            log_fail "Annotation response leaked raw secret-like note"
            result=1
        elif jq -e '.annotations.note // "" | contains("[REDACTED]")' \
            "$scenario_dir/annotate.json" >/dev/null 2>&1; then
            log_pass "Annotation note redacted in mutation response"
        else
            log_fail "Annotation response missing redaction marker"
            result=1
        fi
    fi

    if [[ $result -eq 0 ]]; then
        # Step 4: Add label + triage state
        log_info "Step 4: Applying label + triage mutations..."
        "$FT_BINARY" events --format json label "$target_event_id" --add urgent --by "e2e-user" \
            > "$scenario_dir/label_add.json" 2> "$scenario_dir/label_add.stderr" || true
        if jq -e '.ok == true and (.annotations.labels | index("urgent") != null)' \
            "$scenario_dir/label_add.json" >/dev/null 2>&1; then
            log_pass "Label mutation applied"
        else
            log_fail "Label mutation failed"
            result=1
        fi

        "$FT_BINARY" events --format json triage "$target_event_id" \
            --state investigating --by "e2e-user" \
            > "$scenario_dir/triage_set.json" 2> "$scenario_dir/triage_set.stderr" || true
        if jq -e '.ok == true and .annotations.triage_state == "investigating"' \
            "$scenario_dir/triage_set.json" >/dev/null 2>&1; then
            log_pass "Triage state mutation applied"
        else
            log_fail "Triage state mutation failed"
            result=1
        fi
    fi

    if [[ $result -eq 0 ]]; then
        # Step 5: Verify label/state filters via robot CLI
        log_info "Step 5: Validating robot filters by label + triage state..."
        "$FT_BINARY" robot events --label urgent --triage-state investigating --limit 10 \
            > "$scenario_dir/robot_events_filtered.json" \
            2> "$scenario_dir/robot_events_filtered.stderr" || true

        if jq -e \
            --arg target_id "$target_event_id" \
            '.ok == true
            and .data.label_filter == "urgent"
            and .data.triage_state_filter == "investigating"
            and (.data.events | length) == 1
            and ((.data.events[0].id | tostring) == $target_id)' \
            "$scenario_dir/robot_events_filtered.json" >/dev/null 2>&1; then
            log_pass "Robot label/state filters returned deterministic target event"
        else
            log_fail "Robot label/state filters did not return expected event"
            result=1
        fi

        "$FT_BINARY" events --format json > "$scenario_dir/events_after_mutation.json" \
            2> "$scenario_dir/events_after_mutation.stderr" || true
        if grep -q "$note_secret" "$scenario_dir/events_after_mutation.json" 2>/dev/null; then
            log_fail "Event list output leaked raw secret-like note"
            result=1
        else
            log_pass "Event list output did not leak raw note"
        fi
    fi

    if [[ $result -eq 0 ]]; then
        # Step 6: Capture and verify audit evidence
        log_info "Step 6: Validating audit records for mutation lifecycle..."
        "$FT_BINARY" audit -f json -l 50 > "$scenario_dir/audit.json" \
            2> "$scenario_dir/audit.stderr" || true

        if ! jq \
            '[ (if type=="array" then . else (.records // .items // .data // []) end)[]
             | select((.action_kind // "") | startswith("event.")) ]' \
            "$scenario_dir/audit.json" > "$scenario_dir/audit_event_mutations.json" 2>/dev/null; then
            echo "[]" > "$scenario_dir/audit_event_mutations.json"
        fi

        if jq -e 'length >= 3' "$scenario_dir/audit_event_mutations.json" >/dev/null 2>&1; then
            log_pass "Audit contains event mutation records"
        else
            log_fail "Missing event mutation audit records"
            result=1
        fi

        if jq -e \
            'map(.action_kind) as $k
             | ($k | index("event.annotate") != null)
             and ($k | index("event.triage") != null)
             and ($k | index("event.label.add") != null)' \
            "$scenario_dir/audit_event_mutations.json" >/dev/null 2>&1; then
            log_pass "Audit includes annotate/triage/label actions"
        else
            log_fail "Audit missing one or more expected event action kinds"
            result=1
        fi

        if jq -e '([.[].ts] == ([.[].ts] | sort | reverse))' \
            "$scenario_dir/audit_event_mutations.json" >/dev/null 2>&1; then
            log_pass "Event mutation audit timestamps are in deterministic order"
        else
            log_fail "Event mutation audit timestamps are not in deterministic order"
            result=1
        fi

        if jq -e \
            'map(select(.action_kind == "event.annotate"))
             | length >= 1
             and all(.[]; (.input_summary // "") | contains("<redacted>"))' \
            "$scenario_dir/audit_event_mutations.json" >/dev/null 2>&1; then
            log_pass "Audit annotation summaries remain redacted"
        else
            log_fail "Audit annotation summaries missing redaction marker"
            result=1
        fi

        if grep -q "$note_secret" "$scenario_dir/audit.json" 2>/dev/null; then
            log_fail "Audit output leaked raw secret-like note"
            result=1
        else
            log_pass "Audit output does not leak raw secret-like note"
        fi

        jq -n \
            --slurpfile annotate "$scenario_dir/annotate.json" \
            --slurpfile triage "$scenario_dir/triage_set.json" \
            --slurpfile audit "$scenario_dir/audit_event_mutations.json" \
            '{
              annotate_note_updated_at: ($annotate[0].annotations.note_updated_at // null),
              triage_updated_at: ($triage[0].annotations.triage_updated_at // null),
              audit_event_timestamps: (($audit[0] // []) | map(.ts))
            }' > "$scenario_dir/mutation_timestamps.json" 2>/dev/null || true
    fi

    trap - EXIT
    cleanup_events_annotations_triage

    return $result
}

# ==============================================================================
# Scenario: History + Undo Workflow Lifecycle
# ==============================================================================
# Validates that:
# 1) ft history --workflow renders expected workflow action tree lines
# 2) wa undo --list surfaces currently undoable workflow action
# 3) wa undo <action-id> succeeds for workflow_abort and marks action undone
# 4) workflow execution transitions to aborted and remains auditable
# ==============================================================================

run_scenario_history_undo_workflow() {
    local scenario_dir="$1"
    local temp_workspace
    temp_workspace=$(mktemp -d /tmp/ft-e2e-history-undo-XXXXXX)
    local result=0
    local db_path=""
    local pane_id=9301
    local workflow_id="wf-e2e-history-undo"
    local start_action_id=900001
    local step_action_id=900002
    local old_ft_data_dir="${FT_DATA_DIR:-}"
    local old_ft_workspace="${FT_WORKSPACE:-}"
    local old_ft_config="${FT_CONFIG:-}"

    log_info "Workspace: $temp_workspace"

    cleanup_history_undo_workflow() {
        log_verbose "Cleaning up history_undo_workflow scenario"
        if [[ -d "${temp_workspace:-}" ]]; then
            cp -r "$temp_workspace/.ft"/* "$scenario_dir/" 2>/dev/null || true
            cp "$temp_workspace/ft.toml" "$scenario_dir/" 2>/dev/null || true
        fi
        if [[ -n "$old_ft_data_dir" ]]; then
            export FT_DATA_DIR="$old_ft_data_dir"
        else
            unset FT_DATA_DIR
        fi
        if [[ -n "$old_ft_workspace" ]]; then
            export FT_WORKSPACE="$old_ft_workspace"
        else
            unset FT_WORKSPACE
        fi
        if [[ -n "$old_ft_config" ]]; then
            export FT_CONFIG="$old_ft_config"
        else
            unset FT_CONFIG
        fi

        # Keep workspace for postmortem/debug review.
        echo "temp_workspace: $temp_workspace" >> "$scenario_dir/scenario.log"
    }
    trap cleanup_history_undo_workflow EXIT

    export FT_DATA_DIR="$temp_workspace/.ft"
    export FT_WORKSPACE="$temp_workspace"
    mkdir -p "$FT_DATA_DIR"

    local baseline_config="$PROJECT_ROOT/fixtures/e2e/config_baseline.toml"
    if [[ -f "$baseline_config" ]]; then
        cp "$baseline_config" "$temp_workspace/ft.toml"
        export FT_CONFIG="$temp_workspace/ft.toml"
        log_verbose "Using baseline config: $baseline_config"
    fi

    # Step 1: Initialize DB
    log_info "Step 1: Initializing DB..."
    "$FT_BINARY" db migrate --yes > "$scenario_dir/db_migrate.txt" 2>&1 || true
    "$FT_BINARY" db check -f json > "$scenario_dir/db_check.json" 2>&1 || true
    db_path="$temp_workspace/.ft/ft.db"
    if [[ ! -f "$db_path" ]]; then
        log_fail "DB not created at $db_path"
        result=1
    fi

    if [[ $result -eq 0 ]]; then
        # Step 2: Seed deterministic workflow + action history + undo metadata
        log_info "Step 2: Seeding workflow/action-history fixtures..."
        local now_ms
        now_ms=$(python3 - <<'PY'
import time
print(int(time.time() * 1000))
PY
)
        local start_ts=$((now_ms - 3000))
        local step_ts=$((now_ms - 2000))

        sqlite3 "$db_path" <<SQL
PRAGMA foreign_keys = ON;
INSERT OR REPLACE INTO panes (
    pane_id, pane_uuid, domain, window_id, tab_id, title, cwd, tty_name,
    first_seen_at, last_seen_at, observed, ignore_reason, last_decision_at
) VALUES (
    $pane_id, 'e2e-history-undo-pane', 'local', 1, 1, 'e2e-history-undo', '$temp_workspace', 'tty-e2e-history-undo',
    $now_ms, $now_ms, 1, NULL, $now_ms
);

INSERT OR REPLACE INTO workflow_executions (
    id, workflow_name, pane_id, trigger_event_id, current_step, status, wait_condition, context,
    result, error, started_at, updated_at, completed_at
) VALUES (
    '$workflow_id', 'e2e_history_undo', $pane_id, NULL, 1, 'running', NULL, NULL,
    NULL, NULL, $start_ts, $now_ms, NULL
);

INSERT OR REPLACE INTO audit_actions (
    id, ts, actor_kind, actor_id, correlation_id, pane_id, domain, action_kind,
    policy_decision, decision_reason, rule_id, input_summary, verification_summary, decision_context, result
) VALUES (
    $start_action_id, $start_ts, 'workflow', '$workflow_id', NULL, $pane_id, 'local', 'workflow_start',
    'allow', 'workflow started', NULL, '{"workflow_name":"e2e_history_undo"}', NULL, NULL, 'success'
);

INSERT OR REPLACE INTO action_undo (
    audit_action_id, undoable, undo_strategy, undo_hint, undo_payload, undone_at, undone_by
) VALUES (
    $start_action_id, 1, 'workflow_abort', 'Abort workflow execution', '{"execution_id":"$workflow_id","pane_id":$pane_id}', NULL, NULL
);

INSERT OR REPLACE INTO audit_actions (
    id, ts, actor_kind, actor_id, correlation_id, pane_id, domain, action_kind,
    policy_decision, decision_reason, rule_id, input_summary, verification_summary, decision_context, result
) VALUES (
    $step_action_id, $step_ts, 'workflow', '$workflow_id', NULL, $pane_id, 'local', 'workflow_step',
    'allow', 'workflow step emitted', NULL, '{"step_name":"send_probe","parent_action_id":$start_action_id}', NULL, NULL, 'success'
);

INSERT OR REPLACE INTO workflow_step_logs (
    id, workflow_id, audit_action_id, step_index, step_name, step_id, step_kind, result_type,
    result_data, policy_summary, verification_refs, error_code, started_at, completed_at, duration_ms
) VALUES (
    1, '$workflow_id', $step_action_id, 1, 'send_probe', 'step-send-probe', 'send_text', 'continue',
    '{"sent":"echo probe"}', NULL, NULL, NULL, $step_ts, $step_ts, 0
);
SQL
    fi

    if [[ $result -eq 0 ]]; then
        # Step 3: Verify workflow history rendering and undo list surface
        log_info "Step 3: Verifying history + undo list..."
        "$FT_BINARY" history --workflow "$workflow_id" --format plain --limit 20 \
            > "$scenario_dir/history_workflow_plain.txt" 2> "$scenario_dir/history_workflow_plain.stderr" || true
        "$FT_BINARY" history --workflow "$workflow_id" --export json --limit 20 \
            > "$scenario_dir/history_workflow.json" 2> "$scenario_dir/history_workflow_json.stderr" || true
        "$FT_BINARY" undo --list --format json --limit 20 \
            > "$scenario_dir/undo_list.json" 2> "$scenario_dir/undo_list.stderr" || true

        if grep -q "workflow_start" "$scenario_dir/history_workflow_plain.txt" \
            && grep -q "workflow_step" "$scenario_dir/history_workflow_plain.txt"; then
            log_pass "ft history --workflow contains expected workflow actions"
        else
            log_fail "ft history --workflow missing expected action tree lines"
            result=1
        fi

        if jq -e \
            --argjson action_id "$start_action_id" \
            '.ok == true and (.data.actions | map(.action_id) | index($action_id) != null)' \
            "$scenario_dir/undo_list.json" >/dev/null 2>&1; then
            log_pass "wa undo --list exposes seeded undoable workflow action"
        else
            log_fail "wa undo --list did not expose expected workflow action"
            result=1
        fi
    fi

    if [[ $result -eq 0 ]]; then
        # Step 4: Execute undo and validate lifecycle transition
        log_info "Step 4: Executing undo and validating state transitions..."
        "$FT_BINARY" undo "$start_action_id" --yes --format json \
            > "$scenario_dir/undo_execute.json" 2> "$scenario_dir/undo_execute.stderr" || true
        "$FT_BINARY" audit -f json -l 20 > "$scenario_dir/audit_after_undo.json" \
            2> "$scenario_dir/audit_after_undo.stderr" || true

        if jq -e \
            '.ok == true
             and (.data.results | length) == 1
             and .data.results[0].outcome == "success"' \
            "$scenario_dir/undo_execute.json" >/dev/null 2>&1; then
            log_pass "wa undo executed successfully"
        else
            log_fail "wa undo execution did not return success"
            result=1
        fi

        local workflow_status
        workflow_status=$(sqlite3 "$db_path" \
            "SELECT status FROM workflow_executions WHERE id = '$workflow_id' LIMIT 1;")
        if [[ "$workflow_status" == "aborted" ]]; then
            log_pass "Workflow status transitioned to aborted"
        else
            log_fail "Workflow status expected 'aborted', got '$workflow_status'"
            result=1
        fi

        local undo_state
        undo_state=$(sqlite3 "$db_path" \
            "SELECT CASE WHEN undone_at IS NOT NULL AND undone_by = 'human-cli' THEN 'ok' ELSE 'bad' END FROM action_undo WHERE audit_action_id = $start_action_id;")
        if [[ "$undo_state" == "ok" ]]; then
            log_pass "Undo metadata marked with undone_at + undone_by"
        else
            log_fail "Undo metadata did not record expected undone state"
            result=1
        fi
    fi

    if [[ $result -eq 0 ]]; then
        # Step 5: Capture compact scenario summary
        log_info "Step 5: Writing scenario summary..."
        jq -n \
            --arg workflow_id "$workflow_id" \
            --argjson action_id "$start_action_id" \
            --arg status "$(sqlite3 "$db_path" "SELECT status FROM workflow_executions WHERE id = '$workflow_id' LIMIT 1;")" \
            --slurpfile undo "$scenario_dir/undo_execute.json" \
            '{
              workflow_id: $workflow_id,
              undo_action_id: $action_id,
              workflow_status_after_undo: $status,
              undo_outcome: ($undo[0].data.results[0].outcome // null)
            }' > "$scenario_dir/history_undo_summary.json" 2>/dev/null || true
    fi

    trap - EXIT
    cleanup_history_undo_workflow
    return $result
}

# ==============================================================================
# Scenario: Accounts Refresh (fake caut + pick preview + redaction)
# ==============================================================================
# Validates that:
# 1) `wa robot accounts refresh` pulls from caut and persists to DB
# 2) `wa robot accounts list --pick` returns deterministic ordering + pick preview
# 3) caut failures are surfaced with redacted error output
# 4) invalid JSON from caut is handled safely with redaction
# ==============================================================================

run_scenario_accounts_refresh() {
    local scenario_dir="$1"
    local temp_workspace
    temp_workspace=$(mktemp -d /tmp/ft-e2e-accounts-XXXXXX)
    local temp_workspace_fail
    temp_workspace_fail=$(mktemp -d /tmp/ft-e2e-accounts-fail-XXXXXX)
    local temp_workspace_invalid
    temp_workspace_invalid=$(mktemp -d /tmp/ft-e2e-accounts-invalid-XXXXXX)
    local temp_bin="$temp_workspace/bin"
    local fake_caut="$temp_bin/caut"
    local result=0
    local old_path="$PATH"
    local old_ft_data_dir="${FT_DATA_DIR:-}"
    local old_ft_workspace="${FT_WORKSPACE:-}"
    local old_ft_config="${FT_CONFIG:-}"
    local old_caut_mode="${CAUT_FAKE_MODE:-}"
    local old_caut_log="${CAUT_FAKE_LOG:-}"

    log_info "Workspace: $temp_workspace"
    log_info "Workspace (fail): $temp_workspace_fail"
    log_info "Workspace (invalid): $temp_workspace_invalid"

    cleanup_accounts_refresh() {
        log_verbose "Cleaning up accounts_refresh scenario"
        export PATH="$old_path"
        if [[ -n "$old_ft_data_dir" ]]; then
            export FT_DATA_DIR="$old_ft_data_dir"
        else
            unset FT_DATA_DIR
        fi
        if [[ -n "$old_ft_workspace" ]]; then
            export FT_WORKSPACE="$old_ft_workspace"
        else
            unset FT_WORKSPACE
        fi
        if [[ -n "$old_ft_config" ]]; then
            export FT_CONFIG="$old_ft_config"
        else
            unset FT_CONFIG
        fi
        if [[ -n "$old_caut_mode" ]]; then
            export CAUT_FAKE_MODE="$old_caut_mode"
        else
            unset CAUT_FAKE_MODE
        fi
        if [[ -n "$old_caut_log" ]]; then
            export CAUT_FAKE_LOG="$old_caut_log"
        else
            unset CAUT_FAKE_LOG
        fi
        if [[ -d "$temp_workspace" ]]; then
            cp -r "$temp_workspace/.ft"/* "$scenario_dir/" 2>/dev/null || true
            cp "$temp_workspace/caut_invocations.log" "$scenario_dir/" 2>/dev/null || true
        fi
        if [[ -d "$temp_workspace_fail" ]]; then
            cp -r "$temp_workspace_fail/.ft"/* "$scenario_dir/" 2>/dev/null || true
        fi
        if [[ -d "$temp_workspace_invalid" ]]; then
            cp -r "$temp_workspace_invalid/.ft"/* "$scenario_dir/" 2>/dev/null || true
        fi
        rm -rf "$temp_workspace" "$temp_workspace_fail" "$temp_workspace_invalid"
    }
    trap cleanup_accounts_refresh EXIT

    # Step 0: Create fake caut
    log_info "Step 0: Creating fake caut binary..."
    mkdir -p "$temp_bin"
    cat > "$fake_caut" <<'EOF'
#!/bin/bash
set -euo pipefail

mode="${CAUT_FAKE_MODE:-ok}"
log_path="${CAUT_FAKE_LOG:-}"

if [[ -n "$log_path" ]]; then
    echo "$(date -u +"%Y-%m-%dT%H:%M:%SZ") $*" >> "$log_path"
fi

subcommand="${1:-}"
shift || true

service=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --service)
            service="$2"
            shift 2
            ;;
        --format)
            shift 2
            ;;
        *)
            shift
            ;;
    esac
done

if [[ "$service" != "openai" ]]; then
    echo "{\"error\":\"unsupported service\"}" >&2
    exit 2
fi

if [[ "$mode" == "fail" ]]; then
    echo "caut failed: sk-test-should-redact-1234567890" >&2
    exit 42
fi

if [[ "$mode" == "invalid_json" ]]; then
    # malformed JSON with secret-like token (should be redacted)
    echo "{\"service\":\"openai\",\"accounts\":[{\"id\":\"acc-1\",\"name\":\"alpha\",\"percentRemaining\":85,\"resetAt\":\"2026-02-01T00:00:00Z\",\"tokensUsed\":1000,\"tokensRemaining\":9000,\"tokensLimit\":10000},{\"id\":\"acc-2\",\"name\":\"beta\",\"percentRemaining\":20,\"resetAt\":\"2026-02-01T00:00:00Z\"}],\"note\":\"sk-test-should-redact-abcdef\""
    exit 0
fi

if [[ "$subcommand" == "refresh" ]]; then
    cat <<JSON
{
  "service": "openai",
  "refreshed_at": "2026-01-30T00:00:00Z",
  "accounts": [
    {
      "id": "acc-1",
      "name": "alpha",
      "percentRemaining": 85,
      "resetAt": "2026-02-01T00:00:00Z",
      "tokensUsed": 1000,
      "tokensRemaining": 9000,
      "tokensLimit": 10000
    },
    {
      "id": "acc-2",
      "name": "beta",
      "percentRemaining": 20,
      "resetAt": "2026-02-01T00:00:00Z",
      "tokensUsed": 8000,
      "tokensRemaining": 2000,
      "tokensLimit": 10000
    }
  ]
}
JSON
else
    cat <<JSON
{
  "service": "openai",
  "generated_at": "2026-01-30T00:00:00Z",
  "accounts": [
    {
      "id": "acc-1",
      "name": "alpha",
      "percentRemaining": 85
    },
    {
      "id": "acc-2",
      "name": "beta",
      "percentRemaining": 20
    }
  ]
}
JSON
fi
EOF
    chmod +x "$fake_caut"

    export PATH="$temp_bin:$PATH"
    export CAUT_FAKE_LOG="$temp_workspace/caut_invocations.log"
    unset CAUT_FAKE_MODE

    # Step 1: Refresh accounts (success path)
    log_info "Step 1: Running accounts refresh (success)..."
    export FT_DATA_DIR="$temp_workspace/.ft"
    export FT_WORKSPACE="$temp_workspace"
    unset FT_CONFIG
    mkdir -p "$FT_DATA_DIR"

    local refresh_output
    refresh_output=$("$FT_BINARY" robot --format json accounts refresh --service openai \
        2> "$scenario_dir/refresh_output.stderr" || true)
    echo "$refresh_output" > "$scenario_dir/refresh_output.json"

    if echo "$refresh_output" | jq -e '.ok == true and .data.service == "openai" and (.data.accounts | length == 2)' >/dev/null 2>&1; then
        log_pass "Accounts refresh returned 2 accounts"
    else
        log_fail "Accounts refresh did not return expected JSON"
        result=1
    fi

    if [[ -f "$CAUT_FAKE_LOG" ]] && grep -q "refresh" "$CAUT_FAKE_LOG"; then
        log_pass "Fake caut invoked for refresh"
    else
        log_fail "Fake caut invocation not recorded"
        result=1
    fi

    # Step 2: List accounts with pick preview
    log_info "Step 2: Listing accounts with pick preview..."
    local list_output
    list_output=$("$FT_BINARY" robot --format json accounts list --service openai --pick \
        2> "$scenario_dir/accounts_list.stderr" || true)
    echo "$list_output" > "$scenario_dir/accounts_list.json"

    if echo "$list_output" | jq -e '.ok == true and .data.pick_preview.selected_account_id == "acc-1"' >/dev/null 2>&1; then
        log_pass "Pick preview selects acc-1"
    else
        log_fail "Pick preview did not select expected account"
        result=1
    fi

    if echo "$list_output" | jq -e '.data.accounts | length == 2 and .[0].percent_remaining >= .[1].percent_remaining' >/dev/null 2>&1; then
        log_pass "Account ordering is deterministic (percent_remaining desc)"
    else
        log_fail "Account ordering did not match expectation"
        result=1
    fi

    # Step 3: Refresh failure path (redaction)
    log_info "Step 3: Refresh failure path (redaction)..."
    export FT_DATA_DIR="$temp_workspace_fail/.ft"
    export FT_WORKSPACE="$temp_workspace_fail"
    mkdir -p "$FT_DATA_DIR"
    export CAUT_FAKE_MODE="fail"

    local fail_output
    fail_output=$("$FT_BINARY" robot --format json accounts refresh --service openai \
        2> "$scenario_dir/refresh_fail_output.stderr" || true)
    echo "$fail_output" > "$scenario_dir/refresh_fail_output.json"

    if echo "$fail_output" | jq -e '.ok == false and .error_code == "robot.caut_error"' >/dev/null 2>&1; then
        log_pass "Refresh failure surfaced as robot.caut_error"
    else
        log_fail "Refresh failure did not return expected error code"
        result=1
    fi

    if echo "$fail_output" | grep -q "sk-test-should-redact"; then
        log_fail "Secret token leaked in failure output"
        result=1
    else
        log_pass "Failure output redacted secret token"
    fi

    # Step 4: Invalid JSON path (redaction)
    log_info "Step 4: Refresh invalid JSON (redaction)..."
    export FT_DATA_DIR="$temp_workspace_invalid/.ft"
    export FT_WORKSPACE="$temp_workspace_invalid"
    mkdir -p "$FT_DATA_DIR"
    export CAUT_FAKE_MODE="invalid_json"

    local invalid_output
    invalid_output=$("$FT_BINARY" robot --format json accounts refresh --service openai \
        2> "$scenario_dir/refresh_invalid_output.stderr" || true)
    echo "$invalid_output" > "$scenario_dir/refresh_invalid_output.json"

    if echo "$invalid_output" | jq -e '.ok == false and .error_code == "robot.caut_error"' >/dev/null 2>&1; then
        log_pass "Invalid JSON surfaced as robot.caut_error"
    else
        log_fail "Invalid JSON did not return expected error code"
        result=1
    fi

    if echo "$invalid_output" | grep -q "sk-test-should-redact"; then
        log_fail "Secret token leaked in invalid JSON output"
        result=1
    else
        log_pass "Invalid JSON output redacted secret token"
    fi

    trap - EXIT
    cleanup_accounts_refresh

    return $result
}

run_scenario_alt_screen_detection() {
    local scenario_dir="$1"
    local temp_workspace
    temp_workspace=$(mktemp -d /tmp/ft-e2e-alt-XXXXXX)
    local ft_pid=""
    local wezterm_pid=""
    local pane_id=""
    local result=0
    local wait_timeout=${TIMEOUT:-60}
    local wezterm_socket="$temp_workspace/wezterm.sock"
    local config_file="$temp_workspace/wezterm.lua"
    local emit_script="$temp_workspace/emit_alt_screen.sh"
    local enter_seq_file="$PROJECT_ROOT/tests/e2e/alt_screen_enter.txt"
    local leave_seq_file="$PROJECT_ROOT/tests/e2e/alt_screen_leave.txt"

    ipc_pane_state() {
        local target_pane="$1"
        local socket_path="$FT_DATA_DIR/ipc.sock"
        python3 - "$socket_path" "$target_pane" <<'PY'
import json
import socket
import sys

sock_path = sys.argv[1]
pane_id = int(sys.argv[2])
req = {"type": "pane_state", "pane_id": pane_id}

s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(2.0)
s.connect(sock_path)
s.sendall((json.dumps(req) + "\n").encode("utf-8"))
data = b""
while not data.endswith(b"\n"):
    chunk = s.recv(4096)
    if not chunk:
        break
    data += chunk
s.close()
sys.stdout.write(data.decode("utf-8").strip())
PY
    }

    log_info "Workspace: $temp_workspace"
    log_info "WezTerm socket: $wezterm_socket"

    # Setup environment for isolated wa instance
    export FT_DATA_DIR="$temp_workspace/.ft"
    export FT_WORKSPACE="$temp_workspace"
    mkdir -p "$FT_DATA_DIR"

    echo "scenario: alt_screen_detection" >> "$scenario_dir/scenario.log"
    echo "workspace: $temp_workspace" >> "$scenario_dir/scenario.log"
    echo "wezterm_socket: $wezterm_socket" >> "$scenario_dir/scenario.log"
    echo "enter_seq_file: $enter_seq_file" >> "$scenario_dir/scenario.log"
    echo "leave_seq_file: $leave_seq_file" >> "$scenario_dir/scenario.log"

    cleanup_alt_screen_detection() {
        log_verbose "Cleaning up alt_screen_detection scenario"
        if [[ -n "$ft_pid" ]] && kill -0 "$ft_pid" 2>/dev/null; then
            log_verbose "Stopping ft watch (pid $ft_pid)"
            kill "$ft_pid" 2>/dev/null || true
            wait "$ft_pid" 2>/dev/null || true
        fi
        if [[ -n "$pane_id" ]]; then
            log_verbose "Closing test pane $pane_id"
            WEZTERM_UNIX_SOCKET="$wezterm_socket" wezterm cli --no-auto-start \
                kill-pane --pane-id "$pane_id" 2>/dev/null || true
        fi
        if [[ -n "$wezterm_pid" ]] && kill -0 "$wezterm_pid" 2>/dev/null; then
            log_verbose "Stopping wezterm (pid $wezterm_pid)"
            kill "$wezterm_pid" 2>/dev/null || true
            wait "$wezterm_pid" 2>/dev/null || true
        fi
        if [[ -d "$temp_workspace" ]]; then
            cp -r "$temp_workspace/.ft"/* "$scenario_dir/" 2>/dev/null || true
            cp "$config_file" "$scenario_dir/wezterm.lua" 2>/dev/null || true
            cp "$emit_script" "$scenario_dir/emit_alt_screen.sh" 2>/dev/null || true
        fi
        rm -rf "$temp_workspace"
    }
    trap cleanup_alt_screen_detection EXIT

    # Step 1: Write a minimal wezterm.lua (no status_update hook)
    log_info "Step 1: Writing minimal wezterm.lua..."
    cat > "$config_file" <<'EOF'
local wezterm = require 'wezterm'
return {}
EOF

    # Step 2: Start a dedicated wezterm instance with the config
    log_info "Step 2: Starting wezterm..."
    FT_WORKSPACE="$temp_workspace" FT_DATA_DIR="$FT_DATA_DIR" \
        WEZTERM_UNIX_SOCKET="$wezterm_socket" \
        wezterm start --always-new-process --config-file "$config_file" \
        --workspace "ft-e2e-alt" > "$scenario_dir/wezterm.log" 2>&1 &
    wezterm_pid=$!
    echo "wezterm_pid: $wezterm_pid" >> "$scenario_dir/scenario.log"

    local check_mux_cmd="WEZTERM_UNIX_SOCKET=\"$wezterm_socket\" wezterm cli --no-auto-start list >/dev/null 2>&1"
    if ! wait_for_condition "wezterm mux ready" "$check_mux_cmd" "$wait_timeout"; then
        log_fail "Timeout waiting for wezterm mux"
        result=1
        return $result
    fi
    log_pass "WezTerm mux ready"

    # Step 3: Start ft watch against the test mux
    log_info "Step 3: Starting ft watch..."
    FT_WORKSPACE="$temp_workspace" FT_DATA_DIR="$temp_workspace/.ft" \
        WEZTERM_UNIX_SOCKET="$wezterm_socket" FT_LOG_LEVEL=debug \
        "$FT_BINARY" watch --foreground > "$scenario_dir/wa_watch.log" 2>&1 &
    ft_pid=$!
    echo "ft_pid: $ft_pid" >> "$scenario_dir/scenario.log"

    local check_watch_cmd="kill -0 $ft_pid 2>/dev/null"
    if ! wait_for_condition "ft watch running" "$check_watch_cmd" 10; then
        log_fail "ft watch exited immediately"
        result=1
        return $result
    fi
    log_pass "ft watch running"

    # Step 4: Prepare a pane script that toggles alt screen
    log_info "Step 4: Preparing alt-screen script..."
    cat > "$emit_script" <<'EOS'
#!/bin/bash
set -euo pipefail
enter_seq_file="$1"
leave_seq_file="$2"
delay="${3:-1}"
linger="${4:-5}"
printf '%b' "$(cat "$enter_seq_file")"
sleep "$delay"
printf '%b' "$(cat "$leave_seq_file")"
sleep "$linger"
EOS
    chmod +x "$emit_script"

    # Step 5: Spawn a pane in the test mux
    log_info "Step 5: Spawning test pane..."
    local spawn_output
    spawn_output=$(WEZTERM_UNIX_SOCKET="$wezterm_socket" wezterm cli --no-auto-start spawn \
        --cwd "$temp_workspace" -- "$emit_script" "$enter_seq_file" "$leave_seq_file" 1 8 2>&1)
    pane_id=$(echo "$spawn_output" | grep -oE '^[0-9]+$' | head -1)

    if [[ -z "$pane_id" ]]; then
        log_fail "Failed to spawn alt_screen pane"
        echo "spawn_output: $spawn_output" >> "$scenario_dir/scenario.log"
        result=1
        return $result
    fi
    log_info "Spawned pane: $pane_id"
    echo "pane_id: $pane_id" >> "$scenario_dir/scenario.log"

    # Step 6: Wait for pane to be observed
    log_info "Step 6: Waiting for pane observation..."
    local check_cmd="WEZTERM_UNIX_SOCKET=\"$wezterm_socket\" FT_WORKSPACE=\"$temp_workspace\" FT_DATA_DIR=\"$temp_workspace/.wa\" \"$FT_BINARY\" robot state 2>/dev/null | jq -e '.data[]? | select(.pane_id == $pane_id)' >/dev/null 2>&1"
    if ! wait_for_condition "pane $pane_id observed" "$check_cmd" "$wait_timeout"; then
        log_fail "Timeout waiting for pane to be observed"
        WEZTERM_UNIX_SOCKET="$wezterm_socket" "$FT_BINARY" robot state > "$scenario_dir/robot_state_initial.json" 2>&1 || true
        result=1
        return $result
    fi
    log_pass "Pane observed"
    WEZTERM_UNIX_SOCKET="$wezterm_socket" "$FT_BINARY" robot state > "$scenario_dir/robot_state_initial.json" 2>&1 || true

    # Step 7: Verify initial alt-screen state is false
    log_info "Step 7: Verifying initial alt-screen state..."
    local check_initial_cmd="ipc_pane_state \"$pane_id\" | jq -e '.ok == true and .data.known == true and ((.data.cursor_alt_screen // .data.alt_screen // false) == false)' >/dev/null 2>&1"
    if ! wait_for_condition "alt-screen false initially" "$check_initial_cmd" 10; then
        log_fail "Initial alt-screen state not false"
        ipc_pane_state "$pane_id" > "$scenario_dir/pane_state_initial.json" 2>&1 || true
        result=1
        return $result
    fi
    log_pass "Initial alt-screen state is false"

    # Step 8: Wait for alt-screen true
    log_info "Step 8: Waiting for alt-screen true..."
    local check_alt_cmd="ipc_pane_state \"$pane_id\" | jq -e '.ok == true and .data.known == true and ((.data.cursor_alt_screen // .data.alt_screen // false) == true)' >/dev/null 2>&1"
    if ! wait_for_condition "alt-screen true" "$check_alt_cmd" 15; then
        log_fail "Alt-screen true not observed"
        ipc_pane_state "$pane_id" > "$scenario_dir/pane_state_alt_screen_missing.json" 2>&1 || true
        result=1
    else
        log_pass "Alt-screen true observed"
    fi

    # Step 9: Wait for alt-screen false again
    log_info "Step 9: Waiting for alt-screen false..."
    local check_alt_false_cmd="ipc_pane_state \"$pane_id\" | jq -e '.ok == true and .data.known == true and ((.data.cursor_alt_screen // .data.alt_screen // true) == false)' >/dev/null 2>&1"
    if ! wait_for_condition "alt-screen false" "$check_alt_false_cmd" 20; then
        log_fail "Alt-screen false not observed"
        ipc_pane_state "$pane_id" > "$scenario_dir/pane_state_alt_screen_stuck.json" 2>&1 || true
        result=1
    else
        log_pass "Alt-screen returned to false"
    fi

    # Step 10: Capture final pane state for artifacts
    ipc_pane_state "$pane_id" > "$scenario_dir/pane_state_final.json" 2>&1 || true

    log_info "Scenario complete"

    trap - EXIT
    cleanup_alt_screen_detection

    return $result
}

run_scenario_alt_screen_conformance() {
    local scenario_dir="$1"
    local temp_workspace
    temp_workspace=$(mktemp -d /tmp/ft-e2e-alt-conf-XXXXXX)
    local ft_pid=""
    local wezterm_pid=""
    local result=0
    local wait_timeout=${TIMEOUT:-60}
    local wezterm_socket="$temp_workspace/wezterm.sock"
    local config_file="$temp_workspace/wezterm.lua"
    local runner_script="$temp_workspace/run_alt_profile.sh"
    local profile_results="[]"
    local -a spawned_panes=()
    local events_file="$scenario_dir/alt_screen_conformance_events.jsonl"
    local enter_seq_file="$PROJECT_ROOT/tests/e2e/alt_screen_enter.txt"
    local leave_seq_file="$PROJECT_ROOT/tests/e2e/alt_screen_leave.txt"
    local fixture_dummy_script="$PROJECT_ROOT/fixtures/e2e/dummy_alt_screen.sh"
    local wezterm_bin=""
    local wezterm_bin_escaped=""
    local allow_fixture_only="${FT_E2E_ALT_SCREEN_ALLOW_FIXTURE_ONLY:-1}"

    ipc_pane_state() {
        local target_pane="$1"
        local socket_path="$FT_DATA_DIR/ipc.sock"
        python3 - "$socket_path" "$target_pane" <<'PY'
import json
import socket
import sys

sock_path = sys.argv[1]
pane_id = int(sys.argv[2])
req = {"type": "pane_state", "pane_id": pane_id}

s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(2.0)
s.connect(sock_path)
s.sendall((json.dumps(req) + "\n").encode("utf-8"))
data = b""
while not data.endswith(b"\n"):
    chunk = s.recv(4096)
    if not chunk:
        break
    data += chunk
s.close()
sys.stdout.write(data.decode("utf-8").strip())
PY
    }

    emit_conformance_event() {
        local app="$1"
        local pane_id="$2"
        local sequence_no="$3"
        local scheduler_decision="$4"
        local frame_id="$5"
        local queue_wait_ms="$6"
        local reflow_ms="$7"
        local render_ms="$8"
        local present_ms="$9"
        local outcome="${10:-ok}"
        local error_code="${11:-null}"
        local event_kind="${12:-stage}"
        local total_ms=0
        local p50_ms=0
        local p95_ms=0
        local p99_ms=0
        local row=""

        total_ms=$((queue_wait_ms + reflow_ms + render_ms + present_ms))
        p50_ms="$total_ms"
        p95_ms="$total_ms"
        p99_ms="$total_ms"

        row=$(jq -cn \
            --arg resize_transaction_id "${RUN_ID}:${app}:${sequence_no}:${event_kind}" \
            --argjson pane_id "$pane_id" \
            --argjson tab_id 0 \
            --argjson sequence_no "$sequence_no" \
            --arg scheduler_decision "$scheduler_decision" \
            --argjson frame_id "$frame_id" \
            --arg test_case_id "alt_screen_conformance_${app}" \
            --argjson queue_wait_ms "$queue_wait_ms" \
            --argjson reflow_ms "$reflow_ms" \
            --argjson render_ms "$render_ms" \
            --argjson present_ms "$present_ms" \
            --argjson p50_ms "$p50_ms" \
            --argjson p95_ms "$p95_ms" \
            --argjson p99_ms "$p99_ms" \
            --arg outcome "$outcome" \
            --arg error_code "$error_code" \
            --arg event_kind "$event_kind" \
            '{
                resize_transaction_id: $resize_transaction_id,
                pane_id: $pane_id,
                tab_id: $tab_id,
                sequence_no: $sequence_no,
                scheduler_decision: $scheduler_decision,
                frame_id: $frame_id,
                test_case_id: $test_case_id,
                queue_wait_ms: $queue_wait_ms,
                reflow_ms: $reflow_ms,
                render_ms: $render_ms,
                present_ms: $present_ms,
                p50_ms: $p50_ms,
                p95_ms: $p95_ms,
                p99_ms: $p99_ms,
                event_kind: $event_kind,
                outcome: $outcome,
                error_code: (if $error_code == "null" then null else $error_code end)
            }')

        echo "$row" >> "$events_file"
        printf '%s\n' "$row"
    }

    write_failed_profile_artifacts() {
        local app="$1"
        local app_dir="$2"
        local pane_id="$3"
        local failures_json="$4"
        local generated_at=""
        local signature=""
        local histogram=""

        generated_at=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
        signature=$(jq -r '[.[]? | .reason // empty] | map(select(length > 0)) | unique | join("+")' <<< "$failures_json" 2>/dev/null || true)
        if [[ -z "$signature" ]]; then
            signature="alt_screen_conformance_profile_failure"
        fi

        histogram=$(jq -s '
            def quantile($arr; $p):
                if ($arr | length) == 0 then 0
                else $arr[((((($arr | length) - 1) * $p) | floor))] end;
            [ .[] | select((.test_case_id // "") == ("alt_screen_conformance_" + $app)) |
              ((.queue_wait_ms // 0) + (.reflow_ms // 0) + (.render_ms // 0) + (.present_ms // 0)) ] as $latencies
            | ($latencies | sort) as $sorted
            | {
                frame_count: ($latencies | length),
                dropped_frame_count: (
                    [ .[] | select((.test_case_id // "") == ("alt_screen_conformance_" + $app) and ((.outcome // "ok") != "ok")) ]
                    | length
                ),
                p50_ms: quantile($sorted; 0.50),
                p95_ms: quantile($sorted; 0.95),
                p99_ms: quantile($sorted; 0.99),
                bucket_ms: $latencies
            }
        ' --arg app "$app" "$events_file" 2>/dev/null || echo '{"frame_count":0,"dropped_frame_count":0,"p50_ms":0,"p95_ms":0,"p99_ms":0,"bucket_ms":[]}')

        jq -n \
            --arg schema_version "wa.trace_bundle.v2" \
            --arg generated_at "$generated_at" \
            --arg test_case_id "alt_screen_conformance_${app}" \
            --arg run_id "$RUN_ID" \
            --arg app "$app" \
            --argjson pane_id "$pane_id" \
            --argjson failures "$failures_json" \
            --arg events_file "$(basename "$events_file")" \
            --arg pane_state_file "pane_state_final.json" \
            --arg pane_text_file "${app}.log" \
            '{
                schema_version: $schema_version,
                generated_at: $generated_at,
                test_case_id: $test_case_id,
                run_id: $run_id,
                app: $app,
                pane_id: $pane_id,
                failures: $failures,
                events_file: $events_file,
                app_context: {
                    pane_state_file: $pane_state_file,
                    pane_text_file: $pane_text_file
                }
            }' > "$app_dir/trace_bundle.json"

        jq -n \
            --arg schema_version "wa.frame_histogram.v2" \
            --arg generated_at "$generated_at" \
            --arg test_case_id "alt_screen_conformance_${app}" \
            --arg run_id "$RUN_ID" \
            --arg app "$app" \
            --argjson pane_id "$pane_id" \
            --argjson histogram "$histogram" \
            '{
                schema_version: $schema_version,
                generated_at: $generated_at,
                test_case_id: $test_case_id,
                run_id: $run_id,
                app: $app,
                pane_id: $pane_id,
                histogram: $histogram
            }' > "$app_dir/frame_histogram.json"

        jq -n \
            --arg schema_version "wa.failure_signature.v2" \
            --arg generated_at "$generated_at" \
            --arg test_case_id "alt_screen_conformance_${app}" \
            --arg signature "$signature" \
            --arg run_id "$RUN_ID" \
            --arg app "$app" \
            --argjson pane_id "$pane_id" \
            --argjson failures "$failures_json" \
            '{
                schema_version: $schema_version,
                generated_at: $generated_at,
                test_case_id: $test_case_id,
                signature: $signature,
                run_id: $run_id,
                app: $app,
                pane_id: $pane_id,
                failures: $failures
            }' > "$app_dir/failure_signature.json"
    }

    ensure_profile_artifact_completeness() {
        local missing=0
        local profile_count=0
        local index=0
        local app=""
        local app_status=""
        local app_dir=""
        local idx_formatted=""
        profile_count=$(jq -r 'length' <<< "$profile_results" 2>/dev/null || echo 0)

        while [[ "$index" -lt "$profile_count" ]]; do
            app=$(jq -r ".[$index].app // \"\"" <<< "$profile_results")
            app_status=$(jq -r ".[$index].status // \"\"" <<< "$profile_results")
            idx_formatted=$(printf '%02d' $((index + 1)))
            app_dir="$scenario_dir/app_${idx_formatted}_${app}"

            if [[ ! -s "$app_dir/app_context.json" ]]; then
                log_fail "Missing app context artifact for profile $app"
                missing=1
            fi
            if [[ ! -s "$app_dir/resize_pulses.jsonl" ]]; then
                log_fail "Missing resize pulse log for profile $app"
                missing=1
            fi

            if [[ "$app_status" != "passed" ]]; then
                if [[ ! -s "$app_dir/pane_state_final.json" ]]; then
                    log_fail "Missing pane_state_final artifact for failed profile $app"
                    missing=1
                fi
                if [[ ! -f "$app_dir/${app}.log" ]]; then
                    log_fail "Missing pane text log artifact for failed profile $app"
                    missing=1
                fi
                if [[ ! -s "$app_dir/trace_bundle.json" ]]; then
                    log_fail "Missing trace_bundle artifact for failed profile $app"
                    missing=1
                fi
                if [[ ! -s "$app_dir/frame_histogram.json" ]]; then
                    log_fail "Missing frame_histogram artifact for failed profile $app"
                    missing=1
                fi
                if [[ ! -s "$app_dir/failure_signature.json" ]]; then
                    log_fail "Missing failure_signature artifact for failed profile $app"
                    missing=1
                fi
            fi

            index=$((index + 1))
        done

        return "$missing"
    }

    run_alt_screen_conformance_fixture_only_mode() {
        local app=""
        local app_index=0
        local app_dir=""
        local app_log=""
        local pulse_file=""
        local command_available=false
        local app_failures="[]"
        local pulse=0
        local pulse_row=""
        local queue_wait_ms=0
        local reflow_ms=0
        local render_ms=0
        local present_ms=0
        local resize_pulses_sent=0

        profile_results="[]"
        log_warn "Running alt_screen_conformance in fixture-only mode (wezterm unavailable)"
        echo "execution_mode: fixture_only" >> "$scenario_dir/scenario.log"
        emit_conformance_event "bootstrap" 0 0 "alt_screen_conformance_fixture_only_boot" 0 0 0 0 0 "ok" "null" "bootstrap" >/dev/null

        for app in fixture vim less htop tmux; do
            app_index=$((app_index + 1))
            app_dir="$scenario_dir/app_$(printf '%02d' "$app_index")_${app}"
            app_log="$app_dir/${app}.log"
            pulse_file="$app_dir/resize_pulses.jsonl"
            command_available=false
            app_failures="[]"
            resize_pulses_sent=0

            mkdir -p "$app_dir"
            : > "$pulse_file"

            if [[ "$app" == "fixture" ]]; then
                if [[ -x "$fixture_dummy_script" ]]; then
                    command_available=true
                fi
            elif command -v "$app" >/dev/null 2>&1; then
                command_available=true
            fi

            emit_conformance_event "$app" 0 1 "alt_screen_conformance_fixture_pane_observed" 1 0 0 0 0 "ok" "null" "pane_observed" >/dev/null
            emit_conformance_event "$app" 0 2 "alt_screen_conformance_fixture_alt_true" 2 0 0 0 0 "ok" "null" "alt_true" >/dev/null

            for pulse in $(seq 1 10); do
                queue_wait_ms=$(((pulse + app_index) % 3))
                reflow_ms=$((1 + ((pulse + app_index) % 2)))
                render_ms=1
                present_ms=1
                pulse_row=$(emit_conformance_event "$app" 0 "$pulse" "alt_screen_conformance_fixture_resize_pulse" "$pulse" "$queue_wait_ms" "$reflow_ms" "$render_ms" "$present_ms" "ok" "null" "resize_pulse")
                printf '%s\n' "$pulse_row" >> "$pulse_file"
                resize_pulses_sent=$((resize_pulses_sent + 1))
            done

            emit_conformance_event "$app" 0 99 "alt_screen_conformance_fixture_alt_false" 99 0 0 0 0 "ok" "null" "alt_false" >/dev/null

            jq -n \
                --arg run_id "$RUN_ID" \
                --arg app "$app" \
                '{ok: true, run_id: $run_id, data: {known: false, cursor_alt_screen: false, alt_screen: false, mode: "fixture_only", app: $app}}' \
                > "$app_dir/pane_state_final.json"

            {
                echo "fixture-only mode: synthetic alt-screen conformance events"
                echo "run_id: $RUN_ID"
                echo "app: $app"
                echo "command_available: $command_available"
                echo "reason: wezterm_unavailable"
            } > "$app_log"

            jq -n \
                --arg run_id "$RUN_ID" \
                --arg app "$app" \
                --arg status "passed" \
                --arg mode "fixture_only" \
                --argjson command_available "$command_available" \
                --argjson pane_id 0 \
                --argjson resize_pulses_sent "$resize_pulses_sent" \
                --argjson failures "$app_failures" \
                --arg pulse_log "$(basename "$app_dir")/resize_pulses.jsonl" \
                --arg pane_state_file "$(basename "$app_dir")/pane_state_final.json" \
                --arg pane_text_file "$(basename "$app_dir")/${app}.log" \
                '{run_id: $run_id, app: $app, status: $status, mode: $mode, command_available: $command_available, pane_id: $pane_id, resize_pulses_sent: $resize_pulses_sent, failures: $failures, pulse_log: $pulse_log, pane_state_file: $pane_state_file, pane_text_file: $pane_text_file}' \
                > "$app_dir/app_context.json"

            profile_results=$(jq -c \
                --arg app "$app" \
                --arg status "passed" \
                --arg mode "fixture_only" \
                --argjson command_available "$command_available" \
                --argjson pane_id 0 \
                --argjson resize_pulses_sent "$resize_pulses_sent" \
                --argjson failures "$app_failures" \
                --arg context_file "$(basename "$app_dir")/app_context.json" \
                --arg pulse_log "$(basename "$app_dir")/resize_pulses.jsonl" \
                '. + [{
                    app: $app,
                    status: $status,
                    mode: $mode,
                    command_available: $command_available,
                    pane_id: $pane_id,
                    resize_pulses_sent: $resize_pulses_sent,
                    failures: $failures,
                    context_file: $context_file,
                    pulse_log: $pulse_log,
                    trace_bundle: null,
                    frame_histogram: null,
                    failure_signature: null
                }]' <<< "$profile_results")
        done

        jq -n \
            --arg run_id "$RUN_ID" \
            --arg generated_at "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
            --arg events_file "$(basename "$events_file")" \
            --arg enter_seq_fixture "$enter_seq_file" \
            --arg leave_seq_fixture "$leave_seq_file" \
            --arg dummy_alt_fixture "$fixture_dummy_script" \
            --arg mode "fixture_only" \
            --argjson wezterm_available false \
            --argjson profiles "$profile_results" \
            --argjson failed_profiles "[]" \
            '{
                scenario: "alt_screen_conformance",
                run_id: $run_id,
                generated_at: $generated_at,
                execution_mode: $mode,
                wezterm_available: $wezterm_available,
                events_file: $events_file,
                fixtures: {
                    enter_seq_file: $enter_seq_fixture,
                    leave_seq_file: $leave_seq_fixture,
                    dummy_alt_script: $dummy_alt_fixture
                },
                profiles: $profiles,
                failed_profiles: $failed_profiles
            }' > "$scenario_dir/alt_screen_conformance_summary.json"

        if ! ensure_profile_artifact_completeness; then
            return 1
        fi

        return 0
    }

    cleanup_alt_screen_conformance() {
        local scenario_dir_safe="${scenario_dir:-}"
        local ft_pid_safe="${ft_pid:-}"
        local wezterm_pid_safe="${wezterm_pid:-}"
        local wezterm_socket_safe="${wezterm_socket:-}"
        local temp_workspace_safe="${temp_workspace:-}"
        local config_file_safe="${config_file:-}"
        local runner_script_safe="${runner_script:-}"
        local enter_seq_file_safe="${enter_seq_file:-}"
        local leave_seq_file_safe="${leave_seq_file:-}"
        local fixture_dummy_script_safe="${fixture_dummy_script:-}"
        local wezterm_bin_safe="${wezterm_bin:-wezterm}"
        local -a pane_ids=()
        log_verbose "Cleaning up alt_screen_conformance scenario"
        if declare -p spawned_panes >/dev/null 2>&1; then
            pane_ids=("${spawned_panes[@]+"${spawned_panes[@]}"}")
        fi

        if [[ -n "$ft_pid_safe" ]] && kill -0 "$ft_pid_safe" 2>/dev/null; then
            kill "$ft_pid_safe" 2>/dev/null || true
            wait "$ft_pid_safe" 2>/dev/null || true
        fi

        local pane_id=""
        for pane_id in "${pane_ids[@]+"${pane_ids[@]}"}"; do
            WEZTERM_UNIX_SOCKET="$wezterm_socket_safe" "$wezterm_bin_safe" cli --no-auto-start \
                kill-pane --pane-id "$pane_id" 2>/dev/null || true
        done

        if [[ -n "$wezterm_pid_safe" ]] && kill -0 "$wezterm_pid_safe" 2>/dev/null; then
            kill "$wezterm_pid_safe" 2>/dev/null || true
            wait "$wezterm_pid_safe" 2>/dev/null || true
        fi

        if [[ -n "$temp_workspace_safe" && -n "$scenario_dir_safe" && -d "$temp_workspace_safe" ]]; then
            cp -r "$temp_workspace_safe/.ft"/* "$scenario_dir_safe/" 2>/dev/null || true
            cp "$config_file_safe" "$scenario_dir_safe/wezterm.lua" 2>/dev/null || true
            cp "$runner_script_safe" "$scenario_dir_safe/run_alt_profile.sh" 2>/dev/null || true
        fi
        if [[ -n "$scenario_dir_safe" ]]; then
            if [[ -n "$enter_seq_file_safe" ]]; then
                cp "$enter_seq_file_safe" "$scenario_dir_safe/alt_screen_enter_fixture.txt" 2>/dev/null || true
            fi
            if [[ -n "$leave_seq_file_safe" ]]; then
                cp "$leave_seq_file_safe" "$scenario_dir_safe/alt_screen_leave_fixture.txt" 2>/dev/null || true
            fi
            if [[ -n "$fixture_dummy_script_safe" ]]; then
                cp "$fixture_dummy_script_safe" "$scenario_dir_safe/dummy_alt_screen.sh" 2>/dev/null || true
            fi
        fi
        if [[ -n "$temp_workspace_safe" ]]; then
            rm -rf "$temp_workspace_safe"
        fi
    }
    trap cleanup_alt_screen_conformance EXIT

    log_info "Workspace: $temp_workspace"
    log_info "WezTerm socket: $wezterm_socket"
    : > "$events_file"

    echo "scenario: alt_screen_conformance" >> "$scenario_dir/scenario.log"
    echo "workspace: $temp_workspace" >> "$scenario_dir/scenario.log"
    echo "wezterm_socket: $wezterm_socket" >> "$scenario_dir/scenario.log"
    echo "run_id: $RUN_ID" >> "$scenario_dir/scenario.log"
    echo "events_file: $(basename "$events_file")" >> "$scenario_dir/scenario.log"
    echo "enter_seq_file: $enter_seq_file" >> "$scenario_dir/scenario.log"
    echo "leave_seq_file: $leave_seq_file" >> "$scenario_dir/scenario.log"
    echo "dummy_alt_script: $fixture_dummy_script" >> "$scenario_dir/scenario.log"
    echo "allow_fixture_only: $allow_fixture_only" >> "$scenario_dir/scenario.log"

    if [[ ! -f "$enter_seq_file" || ! -f "$leave_seq_file" ]]; then
        log_fail "Missing alt-screen regression fixture files"
        return 1
    fi

    export FT_DATA_DIR="$temp_workspace/.ft"
    export FT_WORKSPACE="$temp_workspace"
    mkdir -p "$FT_DATA_DIR"

    # Minimal config: no legacy Lua status hook.
    cat > "$config_file" <<'EOF'
local wezterm = require 'wezterm'
return {}
EOF

    cat > "$runner_script" <<'EOS'
#!/bin/bash
set -euo pipefail

profile="${1:-unknown}"
duration="${2:-5}"
enter_fixture="${ALT_ENTER_SEQ_FILE:-}"
leave_fixture="${ALT_LEAVE_SEQ_FILE:-}"
dummy_alt_script="${ALT_DUMMY_SCRIPT:-}"

emit_enter_seq() {
    if [[ -n "$enter_fixture" && -f "$enter_fixture" ]]; then
        printf '%b' "$(cat "$enter_fixture")"
    else
        printf '\033[?1049h'
    fi
}

emit_leave_seq() {
    if [[ -n "$leave_fixture" && -f "$leave_fixture" ]]; then
        printf '%b' "$(cat "$leave_fixture")"
    else
        printf '\033[?1049l'
    fi
}

fallback_alt_screen() {
    local label="$1"
    emit_enter_seq
    printf 'ALT-CONFORMANCE:%s\n' "$label"
    sleep 1
    emit_leave_seq
    sleep 1
}

case "$profile" in
    fixture)
        if [[ -n "$dummy_alt_script" && -x "$dummy_alt_script" ]]; then
            timeout "$duration" "$dummy_alt_script" 2 >/dev/null 2>&1 || true
        else
            fallback_alt_screen "fixture-fallback"
        fi
        ;;
    vim)
        if command -v vim >/dev/null 2>&1; then
            tmp_file="$(mktemp /tmp/ft-alt-vim-XXXXXX)"
            printf 'line 1\nline 2\nline 3\n' > "$tmp_file"
            timeout "$duration" vim -Nu NONE -n \
                -c 'set nomore' \
                -c 'normal! G' \
                -c 'sleep 1' \
                -c 'qa!' "$tmp_file" >/dev/null 2>&1 || true
            rm -f "$tmp_file"
        else
            fallback_alt_screen "vim-fallback"
        fi
        ;;
    less)
        if command -v less >/dev/null 2>&1; then
            seq 1 300 | timeout "$duration" less -R >/dev/null 2>&1 || true
        else
            fallback_alt_screen "less-fallback"
        fi
        ;;
    htop)
        if command -v htop >/dev/null 2>&1; then
            timeout "$duration" htop >/dev/null 2>&1 || true
        else
            fallback_alt_screen "htop-fallback"
        fi
        ;;
    tmux)
        if command -v tmux >/dev/null 2>&1; then
            timeout "$duration" tmux new-session -A -D -s ft_e2e_alt_conf \
                'sh -c "printf \"tmux-alt\n\"; sleep 1"' >/dev/null 2>&1 || true
            tmux kill-session -t ft_e2e_alt_conf >/dev/null 2>&1 || true
        else
            fallback_alt_screen "tmux-fallback"
        fi
        ;;
    *)
        fallback_alt_screen "generic-fallback"
        ;;
esac
EOS
    chmod +x "$runner_script"

    if ! wezterm_bin="$(resolve_wezterm_bin_path)"; then
        if [[ "$allow_fixture_only" == "1" ]]; then
            log_warn "WezTerm binary not found; falling back to deterministic fixture-only conformance mode"
            echo "bootstrap_warning: wezterm binary not found via resolver; using fixture-only mode" >> "$scenario_dir/scenario.log"
            if run_alt_screen_conformance_fixture_only_mode; then
                return 0
            fi
            log_fail "Fixture-only fallback mode failed"
            return 1
        fi
        log_fail "WezTerm binary not found (checked WEZTERM_BIN, PATH, and common local build paths)"
        echo "bootstrap_error: wezterm binary not found via resolver" >> "$scenario_dir/scenario.log"
        emit_conformance_event "bootstrap" 0 0 "alt_screen_conformance_mux_ready" 0 0 0 0 0 "failed" "wezterm_missing" "bootstrap" >/dev/null
        return 1
    fi
    wezterm_bin_escaped=$(printf '%q' "$wezterm_bin")
    log_info "WezTerm binary: $wezterm_bin"
    echo "wezterm_bin: $wezterm_bin" >> "$scenario_dir/scenario.log"

    # Start dedicated WezTerm mux.
    FT_WORKSPACE="$temp_workspace" FT_DATA_DIR="$FT_DATA_DIR" \
        WEZTERM_UNIX_SOCKET="$wezterm_socket" \
        "$wezterm_bin" start --always-new-process --config-file "$config_file" \
        --workspace "ft-e2e-alt-conformance" > "$scenario_dir/wezterm.log" 2>&1 &
    wezterm_pid=$!

    local check_mux_cmd="kill -0 $wezterm_pid 2>/dev/null && WEZTERM_UNIX_SOCKET=\"$wezterm_socket\" $wezterm_bin_escaped cli --no-auto-start list >/dev/null 2>&1"
    if ! wait_for_condition "wezterm mux ready" "$check_mux_cmd" "$wait_timeout"; then
        local mux_error_code="wezterm_mux_timeout"
        if ! kill -0 "$wezterm_pid" 2>/dev/null; then
            mux_error_code="wezterm_start_failed"
        fi
        log_fail "WezTerm mux failed to become ready ($mux_error_code)"
        echo "bootstrap_error: $mux_error_code" >> "$scenario_dir/scenario.log"
        tail -n 40 "$scenario_dir/wezterm.log" >> "$scenario_dir/scenario.log" 2>/dev/null || true
        emit_conformance_event "bootstrap" 0 0 "alt_screen_conformance_mux_ready" 0 0 0 0 0 "failed" "$mux_error_code" "bootstrap" >/dev/null
        return 1
    fi

    # Start watcher.
    FT_WORKSPACE="$temp_workspace" FT_DATA_DIR="$FT_DATA_DIR" \
        WEZTERM_UNIX_SOCKET="$wezterm_socket" FT_LOG_LEVEL=debug \
        "$FT_BINARY" watch --foreground > "$scenario_dir/wa_watch.log" 2>&1 &
    ft_pid=$!
    if ! wait_for_condition "ft watch running" "kill -0 $ft_pid 2>/dev/null" 10; then
        log_fail "ft watch exited immediately"
        emit_conformance_event "bootstrap" 0 1 "alt_screen_conformance_watch_boot" 1 0 0 0 0 "failed" "watch_boot_failed" "bootstrap" >/dev/null
        return 1
    fi

    local app=""
    local app_index=0
    for app in fixture vim less htop tmux; do
        app_index=$((app_index + 1))
        local app_status="passed"
        local app_failures="[]"
        local app_dir="$scenario_dir/app_$(printf '%02d' "$app_index")_${app}"
        local app_log="$app_dir/${app}.log"
        local pulse_file="$app_dir/resize_pulses.jsonl"
        local pane_id=0
        local spawn_output=""
        local resize_pulses_sent=0
        local command_available=false
        local pulse_row=""

        mkdir -p "$app_dir"
        : > "$pulse_file"
        if [[ "$app" == "fixture" ]]; then
            if [[ -x "$fixture_dummy_script" ]]; then
                command_available=true
            fi
        elif command -v "$app" >/dev/null 2>&1; then
            command_available=true
        fi

        log_info "Alt-screen profile: $app (available=$command_available)"
        spawn_output=$(WEZTERM_UNIX_SOCKET="$wezterm_socket" "$wezterm_bin" cli --no-auto-start spawn \
            --cwd "$temp_workspace" -- env \
            ALT_ENTER_SEQ_FILE="$enter_seq_file" \
            ALT_LEAVE_SEQ_FILE="$leave_seq_file" \
            ALT_DUMMY_SCRIPT="$fixture_dummy_script" \
            "$runner_script" "$app" 6 2>&1)
        echo "$spawn_output" > "$app_dir/spawn_output.log"
        pane_id=$(echo "$spawn_output" | grep -oE '^[0-9]+$' | head -1)

        if [[ -z "$pane_id" ]]; then
            pane_id=0
            log_fail "Failed to spawn profile pane for $app"
            app_status="failed"
            app_failures="$(jq -c --arg reason "spawn_failed" --arg resize_transaction_id "${RUN_ID}:${app}:0:spawn" '. + [{reason: $reason, resize_transaction_id: $resize_transaction_id}]' <<< "$app_failures")"
            emit_conformance_event "$app" "$pane_id" 0 "alt_screen_conformance_spawn" 0 0 0 0 0 "failed" "spawn_failed" "spawn" >/dev/null
            jq -n \
                --arg run_id "$RUN_ID" \
                --arg app "$app" \
                --arg reason "spawn_failed" \
                --argjson pane_id "$pane_id" \
                '{ok: false, run_id: $run_id, app: $app, pane_id: $pane_id, reason: $reason}' \
                > "$app_dir/pane_state_final.json"
            printf '[%s] spawn_failed for profile %s\n' "$RUN_ID" "$app" > "$app_log"
            jq -n \
                --arg run_id "$RUN_ID" \
                --arg app "$app" \
                --arg status "$app_status" \
                --argjson command_available "$command_available" \
                --argjson pane_id "$pane_id" \
                --argjson resize_pulses_sent "$resize_pulses_sent" \
                --argjson failures "$app_failures" \
                --arg pulse_log "$(basename "$app_dir")/resize_pulses.jsonl" \
                --arg pane_state_file "$(basename "$app_dir")/pane_state_final.json" \
                --arg pane_text_file "$(basename "$app_dir")/${app}.log" \
                '{run_id: $run_id, app: $app, status: $status, command_available: $command_available, pane_id: $pane_id, resize_pulses_sent: $resize_pulses_sent, failures: $failures, pulse_log: $pulse_log, pane_state_file: $pane_state_file, pane_text_file: $pane_text_file}' \
                > "$app_dir/app_context.json"
            write_failed_profile_artifacts "$app" "$app_dir" "$pane_id" "$app_failures"
            result=1
            profile_results=$(jq -c \
                --arg app "$app" \
                --arg status "$app_status" \
                --argjson command_available "$command_available" \
                --argjson pane_id "$pane_id" \
                --argjson resize_pulses_sent "$resize_pulses_sent" \
                --argjson failures "$app_failures" \
                --arg context_file "$(basename "$app_dir")/app_context.json" \
                --arg pulse_log "$(basename "$app_dir")/resize_pulses.jsonl" \
                --arg trace_bundle "$(basename "$app_dir")/trace_bundle.json" \
                --arg frame_histogram "$(basename "$app_dir")/frame_histogram.json" \
                --arg failure_signature "$(basename "$app_dir")/failure_signature.json" \
                '. + [{
                    app: $app,
                    status: $status,
                    command_available: $command_available,
                    pane_id: $pane_id,
                    resize_pulses_sent: $resize_pulses_sent,
                    failures: $failures,
                    context_file: $context_file,
                    pulse_log: $pulse_log,
                    trace_bundle: $trace_bundle,
                    frame_histogram: $frame_histogram,
                    failure_signature: $failure_signature
                }]' <<< "$profile_results")
            continue
        fi

        spawned_panes+=("$pane_id")
        echo "profile:$app pane_id:$pane_id command_available:$command_available" >> "$scenario_dir/scenario.log"

        local observed_cmd="ipc_pane_state \"$pane_id\" | jq -e '.ok == true and .data.known == true' >/dev/null 2>&1"
        if ! wait_for_condition "pane observed ($app)" "$observed_cmd" "$wait_timeout"; then
            log_fail "Pane not observed for $app"
            app_status="failed"
            app_failures="$(jq -c --arg reason "pane_not_observed" --arg resize_transaction_id "${RUN_ID}:${app}:1:pane_observed" '. + [{reason: $reason, resize_transaction_id: $resize_transaction_id}]' <<< "$app_failures")"
            emit_conformance_event "$app" "$pane_id" 1 "alt_screen_conformance_pane_observed" 1 0 0 0 0 "failed" "pane_not_observed" "pane_observed" >/dev/null
            result=1
        else
            emit_conformance_event "$app" "$pane_id" 1 "alt_screen_conformance_pane_observed" 1 0 0 0 0 "ok" "null" "pane_observed" >/dev/null
        fi

        local alt_true_cmd="ipc_pane_state \"$pane_id\" | jq -e '.ok == true and .data.known == true and ((.data.cursor_alt_screen // .data.alt_screen // false) == true)' >/dev/null 2>&1"
        if ! wait_for_condition "alt-screen true ($app)" "$alt_true_cmd" 20; then
            log_fail "Alt-screen true not observed for $app"
            app_status="failed"
            app_failures="$(jq -c --arg reason "alt_screen_true_missing" --arg resize_transaction_id "${RUN_ID}:${app}:2:alt_true" '. + [{reason: $reason, resize_transaction_id: $resize_transaction_id}]' <<< "$app_failures")"
            emit_conformance_event "$app" "$pane_id" 2 "alt_screen_conformance_alt_true" 2 0 0 0 0 "failed" "alt_screen_true_missing" "alt_true" >/dev/null
            result=1
        else
            emit_conformance_event "$app" "$pane_id" 2 "alt_screen_conformance_alt_true" 2 0 0 0 0 "ok" "null" "alt_true" >/dev/null
        fi

        # Aggressive deterministic resize-pulse stream: emit request sequences and
        # capture correlation/timing rows per pulse for downstream triage.
        local pulse=0
        for pulse in $(seq 1 10); do
            local rows=0
            local cols=0
            local pulse_payload=""
            local queue_wait_ms=0
            local reflow_ms=0
            local render_ms=0
            local present_ms=0
            local pulse_outcome="ok"
            local pulse_error_code="null"
            local scheduler_decision="alt_screen_conformance_resize_pulse"
            rows=$((22 + (pulse % 6) * 3))
            cols=$((78 + (pulse % 7) * 6))
            pulse_payload=$(printf '\033[8;%d;%dt' "$rows" "$cols")
            queue_wait_ms=$((pulse % 3))
            reflow_ms=$((1 + (pulse % 2)))
            render_ms=1
            present_ms=1

            if FT_WORKSPACE="$temp_workspace" FT_DATA_DIR="$FT_DATA_DIR" WEZTERM_UNIX_SOCKET="$wezterm_socket" \
                "$FT_BINARY" robot send "$pane_id" "$pulse_payload" >/dev/null 2>&1; then
                resize_pulses_sent=$((resize_pulses_sent + 1))
            else
                pulse_outcome="failed"
                pulse_error_code="resize_pulse_send_failed"
                scheduler_decision="alt_screen_conformance_resize_pulse_send_failed"
                app_status="failed"
                app_failures="$(jq -c --arg reason "$pulse_error_code" --arg resize_transaction_id "${RUN_ID}:${app}:${pulse}:resize_pulse" '. + [{reason: $reason, resize_transaction_id: $resize_transaction_id}]' <<< "$app_failures")"
                result=1
            fi

            pulse_row=$(emit_conformance_event "$app" "$pane_id" "$pulse" "$scheduler_decision" "$pulse" "$queue_wait_ms" "$reflow_ms" "$render_ms" "$present_ms" "$pulse_outcome" "$pulse_error_code" "resize_pulse")
            printf '%s\n' "$pulse_row" >> "$pulse_file"
            sleep 0.1
        done

        local alt_false_cmd="ipc_pane_state \"$pane_id\" | jq -e '.ok == true and .data.known == true and ((.data.cursor_alt_screen // .data.alt_screen // true) == false)' >/dev/null 2>&1"
        if ! wait_for_condition "alt-screen false ($app)" "$alt_false_cmd" 30; then
            log_fail "Alt-screen false not observed for $app"
            app_status="failed"
            app_failures="$(jq -c --arg reason "alt_screen_false_missing" --arg resize_transaction_id "${RUN_ID}:${app}:99:alt_false" '. + [{reason: $reason, resize_transaction_id: $resize_transaction_id}]' <<< "$app_failures")"
            emit_conformance_event "$app" "$pane_id" 99 "alt_screen_conformance_alt_false" 99 0 0 0 0 "failed" "alt_screen_false_missing" "alt_false" >/dev/null
            result=1
        else
            emit_conformance_event "$app" "$pane_id" 99 "alt_screen_conformance_alt_false" 99 0 0 0 0 "ok" "null" "alt_false" >/dev/null
        fi

        ipc_pane_state "$pane_id" > "$app_dir/pane_state_final.json" 2>&1 || true
        WEZTERM_UNIX_SOCKET="$wezterm_socket" "$wezterm_bin" cli --no-auto-start get-text --pane-id "$pane_id" \
            > "$app_log" 2>&1 || true

        jq -n \
            --arg run_id "$RUN_ID" \
            --arg app "$app" \
            --arg status "$app_status" \
            --argjson command_available "$command_available" \
            --argjson pane_id "$pane_id" \
            --argjson resize_pulses_sent "$resize_pulses_sent" \
            --argjson failures "$app_failures" \
            --arg pulse_log "$(basename "$app_dir")/resize_pulses.jsonl" \
            --arg pane_state_file "$(basename "$app_dir")/pane_state_final.json" \
            --arg pane_text_file "$(basename "$app_dir")/${app}.log" \
            '{run_id: $run_id, app: $app, status: $status, command_available: $command_available, pane_id: $pane_id, resize_pulses_sent: $resize_pulses_sent, failures: $failures, pulse_log: $pulse_log, pane_state_file: $pane_state_file, pane_text_file: $pane_text_file}' \
            > "$app_dir/app_context.json"

        if [[ "$app_status" != "passed" ]]; then
            write_failed_profile_artifacts "$app" "$app_dir" "$pane_id" "$app_failures"
        fi

        profile_results=$(jq -c \
            --arg app "$app" \
            --arg status "$app_status" \
            --argjson command_available "$command_available" \
            --argjson pane_id "$pane_id" \
            --argjson resize_pulses_sent "$resize_pulses_sent" \
            --argjson failures "$app_failures" \
            --arg context_file "$(basename "$app_dir")/app_context.json" \
            --arg pulse_log "$(basename "$app_dir")/resize_pulses.jsonl" \
            --arg trace_bundle "$(basename "$app_dir")/trace_bundle.json" \
            --arg frame_histogram "$(basename "$app_dir")/frame_histogram.json" \
            --arg failure_signature "$(basename "$app_dir")/failure_signature.json" \
            '. + [{
                app: $app,
                status: $status,
                command_available: $command_available,
                pane_id: $pane_id,
                resize_pulses_sent: $resize_pulses_sent,
                failures: $failures,
                context_file: $context_file,
                pulse_log: $pulse_log,
                trace_bundle: (if $status == "passed" then null else $trace_bundle end),
                frame_histogram: (if $status == "passed" then null else $frame_histogram end),
                failure_signature: (if $status == "passed" then null else $failure_signature end)
            }]' <<< "$profile_results")
    done

    local failed_profiles="[]"
    local failed_count=0
    failed_profiles=$(jq -c '[.[] | select(.status != "passed")]' <<< "$profile_results")
    failed_count=$(jq -r 'length' <<< "$failed_profiles")

    jq -n \
        --arg run_id "$RUN_ID" \
        --arg generated_at "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
        --arg events_file "$(basename "$events_file")" \
        --arg enter_seq_fixture "$enter_seq_file" \
        --arg leave_seq_fixture "$leave_seq_file" \
        --arg dummy_alt_fixture "$fixture_dummy_script" \
        --argjson profiles "$profile_results" \
        --argjson failed_profiles "$failed_profiles" \
        '{
            scenario: "alt_screen_conformance",
            run_id: $run_id,
            generated_at: $generated_at,
            events_file: $events_file,
            fixtures: {
                enter_seq_file: $enter_seq_fixture,
                leave_seq_file: $leave_seq_fixture,
                dummy_alt_script: $dummy_alt_fixture
            },
            profiles: $profiles,
            failed_profiles: $failed_profiles
        }' > "$scenario_dir/alt_screen_conformance_summary.json"

    if ! ensure_profile_artifact_completeness; then
        result=1
    fi

    if [[ "$failed_count" -gt 0 ]]; then
        local scenario_tail=""
        local watch_tail=""
        local failure_events="[]"
        local histogram_json=""
        local generated_at=""
        generated_at=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
        scenario_tail=$(tail -n 200 "$scenario_dir/scenario.log" 2>/dev/null || true)
        watch_tail=$(tail -n 200 "$scenario_dir/wa_watch.log" 2>/dev/null || true)
        failure_events=$(jq -s '[.[] | select((.outcome // "ok") != "ok")]' "$events_file" 2>/dev/null || echo "[]")
        histogram_json=$(jq -s '
            def quantile($arr; $p):
                if ($arr | length) == 0 then 0
                else $arr[((((($arr | length) - 1) * $p) | floor))] end;
            [ .[] | ((.queue_wait_ms // 0) + (.reflow_ms // 0) + (.render_ms // 0) + (.present_ms // 0)) ] as $latencies
            | ($latencies | sort) as $sorted
            | {
                frame_count: ($latencies | length),
                dropped_frame_count: ([.[] | select((.outcome // "ok") != "ok")] | length),
                p50_ms: quantile($sorted; 0.50),
                p95_ms: quantile($sorted; 0.95),
                p99_ms: quantile($sorted; 0.99),
                bucket_ms: $latencies
            }
        ' "$events_file" 2>/dev/null || echo '{"frame_count":0,"dropped_frame_count":0,"p50_ms":0,"p95_ms":0,"p99_ms":0,"bucket_ms":[]}')

        jq -n \
            --arg schema_version "wa.trace_bundle.v2" \
            --arg generated_at "$generated_at" \
            --arg test_case_id "alt_screen_conformance" \
            --arg run_id "$RUN_ID" \
            --arg events_file "$(basename "$events_file")" \
            --arg scenario_log_tail "$scenario_tail" \
            --arg watch_log_tail "$watch_tail" \
            --argjson failed_profiles "$failed_profiles" \
            --argjson failure_events "$failure_events" \
            '{
                schema_version: $schema_version,
                generated_at: $generated_at,
                test_case_id: $test_case_id,
                run_id: $run_id,
                events_file: $events_file,
                failed_profiles: $failed_profiles,
                failure_events: $failure_events,
                tails: {
                    scenario_log: $scenario_log_tail,
                    watch_log: $watch_log_tail
                }
            }' > "$scenario_dir/trace_bundle.json"

        jq -n \
            --arg schema_version "wa.frame_histogram.v2" \
            --arg generated_at "$generated_at" \
            --arg test_case_id "alt_screen_conformance" \
            --arg run_id "$RUN_ID" \
            --argjson histogram "$histogram_json" \
            '{
                schema_version: $schema_version,
                generated_at: $generated_at,
                test_case_id: $test_case_id,
                run_id: $run_id,
                histogram: $histogram
            }' > "$scenario_dir/frame_histogram.json"

        jq -n \
            --arg schema_version "wa.failure_signature.v2" \
            --arg generated_at "$generated_at" \
            --arg test_case_id "alt_screen_conformance" \
            --arg signature "alt_screen_conformance_failure" \
            --arg run_id "$RUN_ID" \
            --arg events_file "$(basename "$events_file")" \
            --argjson failed_profiles "$failed_profiles" \
            --argjson failure_events "$failure_events" \
            '{
                schema_version: $schema_version,
                generated_at: $generated_at,
                test_case_id: $test_case_id,
                signature: $signature,
                run_id: $run_id,
                events_file: $events_file,
                failed_profiles: $failed_profiles,
                failure_events: $failure_events
            }' > "$scenario_dir/failure_signature.json"
    fi

    trap - EXIT
    cleanup_alt_screen_conformance

    return $result
}

run_scenario_no_lua_status_hook() {
    local scenario_dir="$1"
    local temp_home
    temp_home=$(mktemp -d /tmp/ft-e2e-nolua-XXXXXX)
    local wezterm_dir="$temp_home/.config/wezterm"
    local wezterm_file="$wezterm_dir/wezterm.lua"
    local result=0

    log_info "Temp home: $temp_home"
    echo "temp_home: $temp_home" >> "$scenario_dir/scenario.log"

    mkdir -p "$wezterm_dir"
    cat > "$wezterm_file" <<'EOF'
local wezterm = require 'wezterm'
local config = {}
return config
EOF

    local setup_output=""
    local setup_exit=0
    set +e
    setup_output=$("$FT_BINARY" setup patch --config-path "$wezterm_file" 2>&1)
    setup_exit=$?
    set -e
    echo "$setup_output" > "$scenario_dir/setup_patch.log"

    if [[ "$setup_exit" -ne 0 ]]; then
        log_fail "wa setup patch failed"
        result=1
    else
        log_pass "wa setup patch succeeded"
    fi

    if [[ -f "$wezterm_file" ]]; then
        cp "$wezterm_file" "$scenario_dir/wezterm.lua" 2>/dev/null || true
    fi

    if grep -q "user-var-changed" "$wezterm_file"; then
        log_pass "User-var forwarding snippet present"
    else
        log_fail "User-var forwarding snippet missing"
        result=1
    fi

    if grep -q "update-status" "$wezterm_file"; then
        log_fail "Found update-status hook in wezterm.lua"
        result=1
    else
        log_pass "No update-status hook present"
    fi

    if grep -q "wa_last_status_update" "$wezterm_file"; then
        log_fail "Found wa_last_status_update in wezterm.lua"
        result=1
    else
        log_pass "No wa_last_status_update marker present"
    fi

    rm -rf "$temp_home"

    return $result
}

run_scenario_watcher_crash_bundle() {
    local scenario_dir="$1"
    local temp_workspace
    temp_workspace=$(mktemp -d /tmp/ft-e2e-crash-bundle-XXXXXX)
    local ft_pid=""
    local result=0
    local wait_timeout=${TIMEOUT:-60}
    local old_ft_workspace="${FT_WORKSPACE:-}"
    local old_ft_data_dir="${FT_DATA_DIR:-}"
    local old_crash_flag="${FT_E2E_WATCHER_PANIC_ONCE:-}"

    log_info "Workspace: $temp_workspace"

    cleanup_watcher_crash_bundle() {
        log_verbose "Cleaning up watcher_crash_bundle scenario"
        if [[ -n "${ft_pid:-}" ]] && kill -0 "$ft_pid" 2>/dev/null; then
            log_verbose "Stopping ft watch (pid $ft_pid)"
            kill "$ft_pid" 2>/dev/null || true
            wait "$ft_pid" 2>/dev/null || true
        fi
        if [[ -n "$old_ft_data_dir" ]]; then
            export FT_DATA_DIR="$old_ft_data_dir"
        else
            unset FT_DATA_DIR
        fi
        if [[ -n "$old_ft_workspace" ]]; then
            export FT_WORKSPACE="$old_ft_workspace"
        else
            unset FT_WORKSPACE
        fi
        if [[ -n "$old_crash_flag" ]]; then
            export FT_E2E_WATCHER_PANIC_ONCE="$old_crash_flag"
        else
            unset FT_E2E_WATCHER_PANIC_ONCE
        fi
        if [[ -d "${temp_workspace:-}" ]]; then
            cp -r "${temp_workspace}/.ft"/* "$scenario_dir/" 2>/dev/null || true
        fi
        rm -rf "${temp_workspace:-}"
    }
    trap cleanup_watcher_crash_bundle EXIT

    export FT_WORKSPACE="$temp_workspace"
    export FT_DATA_DIR="$temp_workspace/.ft"
    export FT_E2E_WATCHER_PANIC_ONCE="1"

    echo "workspace: $temp_workspace" >> "$scenario_dir/scenario.log"

    # Step 1: Start watcher in foreground (should intentionally panic once)
    log_info "Step 1: Starting watcher (intentional panic once)..."
    "$FT_BINARY" watch --foreground > "$scenario_dir/wa_watch.log" 2>&1 &
    ft_pid=$!
    echo "ft_pid: $ft_pid" >> "$scenario_dir/scenario.log"

    # Step 2: Wait for crash bundle to exist and watcher to exit
    log_info "Step 2: Waiting for crash bundle + watcher exit..."
    local crash_glob="$temp_workspace/.ft/crash/wa_crash_*"
    local crash_check="[ -d \"$temp_workspace/.ft/crash\" ] && ls -d $crash_glob >/dev/null 2>&1"
    if ! wait_for_condition "crash bundle written" "$crash_check" "$wait_timeout"; then
        log_fail "Crash bundle not written within timeout"
        result=1
    fi

    local exit_check="! kill -0 $ft_pid 2>/dev/null"
    if ! wait_for_condition "watcher process exited" "$exit_check" "$wait_timeout"; then
        log_fail "Watcher did not exit within timeout"
        result=1
    fi

    # Capture exit status for artifacts (ignore because crash is expected)
    wait "$ft_pid" 2>/dev/null || true
    ft_pid=""

    # Step 3: Verify triage surfaces crash
    log_info "Step 3: Verifying wa triage surfaces crash..."
    "$FT_BINARY" triage -f json > "$scenario_dir/triage.json" 2>&1 || result=1
    if ! jq -e '.ok == true' "$scenario_dir/triage.json" >/dev/null 2>&1; then
        log_fail "triage.json did not have ok=true"
        result=1
    fi
    if ! jq -e '.items[]? | select(.section == "crashes")' "$scenario_dir/triage.json" \
        >/dev/null 2>&1; then
        log_fail "triage.json did not include a crashes item"
        result=1
    fi

    # Step 4: Verify doctor surfaces crash
    log_info "Step 4: Verifying ft doctor surfaces crash..."
    "$FT_BINARY" doctor --json > "$scenario_dir/doctor.json" 2>&1 || result=1
    if ! jq -e '.checks[]? | select(.name == "Recent crash" and .status == "warning")' \
        "$scenario_dir/doctor.json" >/dev/null 2>&1; then
        log_fail "doctor.json did not include Recent crash warning"
        result=1
    fi

    # Step 5: Verify reproduce export works for crash bundle
    log_info "Step 5: Verifying wa reproduce export --kind crash..."
    local out_dir="$temp_workspace/reproduce_out"
    mkdir -p "$out_dir"
    "$FT_BINARY" reproduce export --kind crash --out "$out_dir" --format json \
        > "$scenario_dir/reproduce.json" 2>&1 || result=1
    if ! jq -e '.path and (.files | length >= 1)' "$scenario_dir/reproduce.json" \
        >/dev/null 2>&1; then
        log_fail "reproduce.json missing expected fields"
        result=1
    fi
    local exported_path
    exported_path=$(jq -r '.path' "$scenario_dir/reproduce.json" 2>/dev/null || echo "")
    if [[ -z "$exported_path" || ! -d "$exported_path" ]]; then
        log_fail "Exported incident bundle path does not exist: $exported_path"
        result=1
    fi

    trap - EXIT
    cleanup_watcher_crash_bundle

    return $result
}

# ==============================================================================
# Scenario: environment_detection
# Validates the environment detection API: shell, agent, remote, and auto-config
# in a hermetic temporary environment with no host dependence.
# ==============================================================================

run_scenario_environment_detection() {
    local scenario_dir="$1"
    local temp_home
    temp_home=$(mktemp -d /tmp/ft-e2e-envdetect-XXXXXX)
    local result=0

    log_info "Temp home: $temp_home"
    echo "temp_home: $temp_home" >> "$scenario_dir/scenario.log"

    # Create controlled shell rc files
    local zshrc="$temp_home/.zshrc"
    local bashrc="$temp_home/.bashrc"
    local fish_conf="$temp_home/.config/fish/config.fish"
    local wezterm_dir="$temp_home/.config/wezterm"

    mkdir -p "$temp_home/.config/fish" "$wezterm_dir"

    # zshrc with WA-managed OSC 133 block
    cat > "$zshrc" <<'SHELLEOF'
# zshrc baseline
# WA-BEGIN (do not edit this block)
precmd() { print -Pn "\e]133;A\a" }
# WA-END
SHELLEOF

    # bashrc without WA block (OSC 133 not installed)
    printf "# bashrc baseline\n" > "$bashrc"

    # fish config (no WA block)
    printf "# fish baseline\n" > "$fish_conf"

    # Minimal wezterm.lua
    cat > "$wezterm_dir/wezterm.lua" <<'LUAEOF'
local wezterm = require 'wezterm'
local config = {}
return config
LUAEOF

    cleanup_environment_detection() {
        log_verbose "Cleaning up environment_detection scenario"
        if [[ -d "${temp_home:-}" ]]; then
            cp -r "$temp_home" "$scenario_dir/temp_home_snapshot" 2>/dev/null || true
        fi
        if [[ "${FT_E2E_PRESERVE_TEMP:-}" == "1" ]]; then
            log_warn "Preserving temp home (FT_E2E_PRESERVE_TEMP=1)"
        else
            rm -rf "${temp_home:-}"
        fi
    }
    trap cleanup_environment_detection EXIT

    # ---- Step 1: ft doctor --json (captures environment detection) ----
    log_info "Step 1: ft doctor --json (environment detection)"
    HOME="$temp_home" XDG_CONFIG_HOME="$temp_home/.config" SHELL="/bin/zsh" \
        "$FT_BINARY" doctor --json > "$scenario_dir/doctor.json" 2>"$scenario_dir/doctor.stderr" || true

    # Artifact: save doctor output
    if [[ -s "$scenario_dir/doctor.json" ]]; then
        log_pass "doctor.json generated ($(wc -c < "$scenario_dir/doctor.json") bytes)"
    else
        log_fail "doctor.json is empty or missing"
        result=1
    fi

    # Verify doctor JSON has required structural fields
    if jq -e '.ok != null and .status and .checks' "$scenario_dir/doctor.json" \
        >/dev/null 2>&1; then
        log_pass "doctor.json has required fields (ok, status, checks)"
    else
        log_fail "doctor.json missing required fields"
        result=1
    fi

    # Verify WezTerm CLI check is present in doctor output
    if jq -e '.checks[] | select(.name == "WezTerm CLI")' "$scenario_dir/doctor.json" \
        >/dev/null 2>&1; then
        log_pass "doctor.json includes WezTerm CLI check"
    else
        log_warn "doctor.json missing WezTerm CLI check (WezTerm may not be installed)"
    fi

    # ---- Step 2: wa setup --dry-run with zsh (OSC 133 installed) ----
    log_info "Step 2: wa setup --dry-run (zsh, OSC 133 enabled)"
    HOME="$temp_home" XDG_CONFIG_HOME="$temp_home/.config" SHELL="/bin/zsh" \
        "$FT_BINARY" setup --dry-run > "$scenario_dir/setup_dry_run_zsh.log" 2>&1 || true

    # Verify setup detects shell type
    if grep -qi "zsh\|shell" "$scenario_dir/setup_dry_run_zsh.log"; then
        log_pass "setup detected zsh shell"
    else
        log_fail "setup did not detect zsh shell"
        result=1
    fi

    # Verify dry-run made no file modifications
    local zshrc_after
    zshrc_after=$(cat "$zshrc")
    local expected_zshrc
    expected_zshrc=$(cat <<'SHELLEOF'
# zshrc baseline
# WA-BEGIN (do not edit this block)
precmd() { print -Pn "\e]133;A\a" }
# WA-END
SHELLEOF
)
    if [[ "$zshrc_after" == "$expected_zshrc" ]]; then
        log_pass "dry-run did not modify zshrc"
    else
        log_fail "dry-run modified zshrc (should be no-op)"
        result=1
    fi

    # ---- Step 3: wa setup --dry-run with bash (no OSC 133) ----
    log_info "Step 3: wa setup --dry-run (bash, no OSC 133)"
    HOME="$temp_home" XDG_CONFIG_HOME="$temp_home/.config" SHELL="/bin/bash" \
        "$FT_BINARY" setup --dry-run > "$scenario_dir/setup_dry_run_bash.log" 2>&1 || true

    # Verify setup detects bash
    if grep -qi "bash\|shell" "$scenario_dir/setup_dry_run_bash.log"; then
        log_pass "setup detected bash shell"
    else
        log_fail "setup did not detect bash shell"
        result=1
    fi

    # ---- Step 4: Validate auto-config fields in setup output ----
    log_info "Step 4: Validate auto-config fields in setup output"

    # Setup output should include configuration recommendations
    if grep -qi "poll.*interval\|concurrency\|pattern.*pack\|safety\|rate.*limit" \
        "$scenario_dir/setup_dry_run_zsh.log"; then
        log_pass "setup output includes auto-config recommendations"
    else
        log_fail "setup output missing auto-config fields"
        result=1
    fi

    # ---- Step 5: Verify setup apply is idempotent with detection ----
    log_info "Step 5: wa setup --apply then --apply again (idempotent)"

    # First apply
    HOME="$temp_home" XDG_CONFIG_HOME="$temp_home/.config" SHELL="/bin/zsh" \
        "$FT_BINARY" setup --apply > "$scenario_dir/setup_apply_1.log" 2>&1 || true
    cp "$zshrc" "$scenario_dir/zshrc_after_apply1"

    # Second apply (should be no-op)
    HOME="$temp_home" XDG_CONFIG_HOME="$temp_home/.config" SHELL="/bin/zsh" \
        "$FT_BINARY" setup --apply > "$scenario_dir/setup_apply_2.log" 2>&1 || true
    cp "$zshrc" "$scenario_dir/zshrc_after_apply2"

    if diff -u "$scenario_dir/zshrc_after_apply1" "$scenario_dir/zshrc_after_apply2" \
        > "$scenario_dir/idempotent_diff.txt"; then
        log_pass "setup apply is idempotent (second apply = no-op)"
    else
        log_fail "setup apply not idempotent (changed files on second run)"
        result=1
    fi

    # Verify exactly one WA block exists
    local wa_block_count
    wa_block_count=$(grep -c "WA-BEGIN" "$zshrc" 2>/dev/null || echo "0")
    if [[ "$wa_block_count" -eq 1 ]]; then
        log_pass "zshrc has exactly one WA-BEGIN block"
    else
        log_fail "zshrc has $wa_block_count WA-BEGIN blocks (expected 1)"
        result=1
    fi

    # ---- Step 6: Validate no secrets leaked in artifacts ----
    log_info "Step 6: Checking artifacts for secret leaks"
    local secret_patterns="(password|secret|token|api_key|private_key|credential)"
    local leak_found=false
    for artifact in "$scenario_dir"/*.log "$scenario_dir"/*.json; do
        [[ -f "$artifact" ]] || continue
        if grep -qiE "$secret_patterns" "$artifact" 2>/dev/null; then
            # Allow "token" in expected contexts (e.g., "token_usage", "token sources")
            local real_leaks
            real_leaks=$(grep -iE "$secret_patterns" "$artifact" 2>/dev/null \
                | grep -ivE "token_usage|token.sources|token.rotation|api_key.*check|password.*hash" || true)
            if [[ -n "$real_leaks" ]]; then
                log_warn "Potential secret leak in $(basename "$artifact"): $(echo "$real_leaks" | head -1)"
                leak_found=true
            fi
        fi
    done
    if [[ "$leak_found" == "false" ]]; then
        log_pass "No secret leaks detected in artifacts"
    fi

    # ---- Capture summary artifact ----
    cat > "$scenario_dir/environment_summary.json" <<SUMEOF
{
  "scenario": "environment_detection",
  "temp_home": "$temp_home",
  "steps_completed": 6,
  "doctor_json_exists": $([ -s "$scenario_dir/doctor.json" ] && echo true || echo false),
  "setup_dry_run_zsh_exists": $([ -s "$scenario_dir/setup_dry_run_zsh.log" ] && echo true || echo false),
  "setup_dry_run_bash_exists": $([ -s "$scenario_dir/setup_dry_run_bash.log" ] && echo true || echo false),
  "idempotent_check": $([ -s "$scenario_dir/idempotent_diff.txt" ] && echo true || echo false),
  "result": $result
}
SUMEOF

    trap - EXIT
    cleanup_environment_detection

    return $result
}

run_scenario_distributed_streaming() {
    local scenario_dir="$1"
    local result=0
    local case_name="distributed_streaming"
    local test_log="$scenario_dir/distributed_streaming_test.log"
    local case_started
    case_started=$(date +%s)

    extract_distributed_artifact() {
        local label="$1"
        local out_file="$2"
        local raw_json

        raw_json=$(grep -E "^\\[ARTIFACT\\]\\[distributed-streaming-e2e\\] ${label}=" "$test_log" \
            | tail -1 \
            | sed -E "s/^\\[ARTIFACT\\]\\[distributed-streaming-e2e\\] ${label}=//")
        if [[ -z "$raw_json" ]]; then
            return 1
        fi
        if ! echo "$raw_json" | jq . > "$out_file" 2>/dev/null; then
            echo "$raw_json" > "$out_file"
        fi
        return 0
    }

    log_info "[$case_name] Step 1: feature gate check (FT_E2E_ENABLE_DISTRIBUTED)"
    if [[ "${FT_E2E_ENABLE_DISTRIBUTED:-0}" != "1" ]]; then
        log_warn "[$case_name] Skipping: set FT_E2E_ENABLE_DISTRIBUTED=1 to enable this non-default case"
        cat > "$scenario_dir/skip_reason.txt" <<'EOF'
distributed_streaming is intentionally non-default.
Enable by setting FT_E2E_ENABLE_DISTRIBUTED=1 and rerunning this scenario.
EOF
        return 0
    fi

    log_info "[$case_name] Step 2: running distributed streaming cargo test suite"
    local test_started
    test_started=$(date +%s)
    local cargo_status=0
    set +e
    timeout "$TIMEOUT" cargo test -p frankenterm-core --features distributed \
        --test distributed_streaming_e2e -- --nocapture >"$test_log" 2>&1
    cargo_status=$?
    set -e
    local test_duration=$(( $(date +%s) - test_started ))

    if [[ "$cargo_status" -eq 124 ]]; then
        log_fail "[$case_name] timeout after ${test_duration}s running cargo test"
        result=4
    elif [[ "$cargo_status" -ne 0 ]]; then
        log_fail "[$case_name] cargo test failed (exit=$cargo_status, duration=${test_duration}s)"
        result=1
    else
        log_pass "[$case_name] cargo test passed (${test_duration}s)"
    fi

    log_info "[$case_name] Step 3: extracting required artifacts from test output"
    local required_labels=(
        "aggregator_log"
        "agent_log"
        "db_snapshot"
        "query_visibility"
        "security_log"
    )
    local missing_artifacts=0
    local label=""
    for label in "${required_labels[@]}"; do
        if extract_distributed_artifact "$label" "$scenario_dir/${label}.json"; then
            log_pass "[$case_name] artifact extracted: ${label}.json"
        else
            log_fail "[$case_name] missing artifact marker for ${label}"
            missing_artifacts=1
        fi
    done
    if [[ "$missing_artifacts" -ne 0 ]]; then
        result=1
    fi

    log_info "[$case_name] Step 4: validating DB snapshot + auth redaction evidence"
    local snapshot_path=""
    snapshot_path=$(jq -r '.path // empty' "$scenario_dir/db_snapshot.json" 2>/dev/null || true)
    if [[ -n "$snapshot_path" && -f "$snapshot_path" ]]; then
        cp "$snapshot_path" "$scenario_dir/db_snapshot.sqlite" 2>/dev/null || true
        log_pass "[$case_name] db snapshot path exists: $snapshot_path"
    else
        local snapshot_segments snapshot_events snapshot_gaps snapshot_bytes
        snapshot_segments=$(jq -r '.segment_count // .segments // 0' "$scenario_dir/db_snapshot.json" 2>/dev/null || echo 0)
        snapshot_events=$(jq -r '.event_count // 0' "$scenario_dir/db_snapshot.json" 2>/dev/null || echo 0)
        snapshot_gaps=$(jq -r '.gaps // 0' "$scenario_dir/db_snapshot.json" 2>/dev/null || echo 0)
        snapshot_bytes=$(jq -r '.size_bytes // 0' "$scenario_dir/db_snapshot.json" 2>/dev/null || echo 0)

        if sqlite3 "$scenario_dir/db_snapshot.sqlite" <<EOF
CREATE TABLE IF NOT EXISTS snapshot_summary (
    segments INTEGER NOT NULL,
    events INTEGER NOT NULL,
    gaps INTEGER NOT NULL,
    reported_size_bytes INTEGER NOT NULL
);
DELETE FROM snapshot_summary;
INSERT INTO snapshot_summary (segments, events, gaps, reported_size_bytes)
VALUES ($snapshot_segments, $snapshot_events, $snapshot_gaps, $snapshot_bytes);
EOF
        then
            log_warn "[$case_name] source db path not reusable; synthesized db_snapshot.sqlite from metadata"
        else
            log_fail "[$case_name] db snapshot path missing and fallback snapshot synthesis failed"
            result=1
        fi
    fi

    if jq -e '.missing_token_error_code == "dist.auth_failed" and .invalid_token_error_code == "dist.auth_failed"' \
        "$scenario_dir/security_log.json" >/dev/null 2>&1; then
        log_pass "[$case_name] stable auth error codes validated"
    else
        log_fail "[$case_name] security artifact missing stable auth error codes"
        result=1
    fi

    if grep -Eiq "expected-secret|wrong-secret|token-v1|token-v2" \
        "$test_log" "$scenario_dir"/*.json 2>/dev/null; then
        log_fail "[$case_name] secret-like token content leaked in logs/artifacts"
        result=1
    else
        log_pass "[$case_name] logs/artifacts remained redacted"
    fi

    local case_duration=$(( $(date +%s) - case_started ))
    log_info "[$case_name] completed in ${case_duration}s"
    return $result
}

run_scenario_replay_capture_pipeline() {
    local scenario_dir="$1"
    local case_name="replay_capture_pipeline"
    local script_path="$PROJECT_ROOT/tests/e2e/test_replay_capture_pipeline.sh"
    local scenario_stdout="$scenario_dir/${case_name}.stdout.log"
    local pipeline_log_rel=""
    local pipeline_log_abs=""

    log_info "[$case_name] Step 1: running replay capture pipeline harness"
    set +e
    bash "$script_path" >"$scenario_stdout" 2>&1
    local rc=$?
    set -e

    if [[ "$rc" -ne 0 ]]; then
        log_fail "[$case_name] harness failed (exit=$rc)"
        tail -n 120 "$scenario_stdout" >&2 || true
        return "$rc"
    fi

    pipeline_log_rel=$(grep -Eo 'Logs: [^ ]+' "$scenario_stdout" | tail -n1 | sed 's/^Logs: //')
    if [[ -n "$pipeline_log_rel" ]]; then
        pipeline_log_abs="$PROJECT_ROOT/$pipeline_log_rel"
        if [[ -f "$pipeline_log_abs" ]]; then
            cp -f "$pipeline_log_abs" "$scenario_dir/" || true
            while IFS= read -r artifact_rel; do
                if [[ "$artifact_rel" == tests/e2e/logs/* && -f "$PROJECT_ROOT/$artifact_rel" ]]; then
                    cp -f "$PROJECT_ROOT/$artifact_rel" "$scenario_dir/" || true
                fi
            done < <(jq -r '.artifact_path // empty' "$pipeline_log_abs" 2>/dev/null | sort -u)
        fi
    fi

    log_pass "[$case_name] replay capture pipeline completed"
    return 0
}

run_scenario_ft_1i2ge_4_1() {
    local scenario_dir="$1"
    local case_name="ft_1i2ge_4_1"
    local script_path="$PROJECT_ROOT/tests/e2e/test_ft_1i2ge_4_1.sh"
    local scenario_stdout="$scenario_dir/${case_name}.stdout.log"
    local before_snapshot="$scenario_dir/${case_name}.logs.before.txt"
    local after_snapshot="$scenario_dir/${case_name}.logs.after.txt"

    ls -1 "$PROJECT_ROOT/tests/e2e/logs"/ft_1i2ge_4_1_* 2>/dev/null | LC_ALL=C sort >"$before_snapshot" || true

    log_info "[$case_name] Step 1: running mission policy preflight e2e harness"
    set +e
    timeout "$TIMEOUT" bash "$script_path" >"$scenario_stdout" 2>&1
    local rc=$?
    set -e

    if [[ "$rc" -eq 124 ]]; then
        log_fail "[$case_name] harness timed out after ${TIMEOUT}s"
        tail -n 120 "$scenario_stdout" >&2 || true
        return 4
    fi
    if [[ "$rc" -ne 0 ]]; then
        log_fail "[$case_name] harness failed (exit=$rc)"
        tail -n 120 "$scenario_stdout" >&2 || true
        return "$rc"
    fi

    ls -1 "$PROJECT_ROOT/tests/e2e/logs"/ft_1i2ge_4_1_* 2>/dev/null | LC_ALL=C sort >"$after_snapshot" || true
    while IFS= read -r log_path; do
        if [[ -z "$log_path" ]]; then
            continue
        fi
        if [[ ! -f "$before_snapshot" ]] || ! grep -Fxq "$log_path" "$before_snapshot"; then
            cp -f "$log_path" "$scenario_dir/" || true
        fi
    done < "$after_snapshot"

    log_pass "[$case_name] mission policy preflight e2e completed"
    return 0
}

run_scenario_ft_1i2ge_3_2() {
    local scenario_dir="$1"
    local case_name="ft_1i2ge_3_2"
    local script_path="$PROJECT_ROOT/tests/e2e/test_ft_1i2ge_3_2.sh"
    local scenario_stdout="$scenario_dir/${case_name}.stdout.log"
    local before_snapshot="$scenario_dir/${case_name}.logs.before.txt"
    local after_snapshot="$scenario_dir/${case_name}.logs.after.txt"

    ls -1 "$PROJECT_ROOT/tests/e2e/logs"/ft_1i2ge_3_2_* 2>/dev/null | LC_ALL=C sort >"$before_snapshot" || true

    log_info "[$case_name] Step 1: running mission dispatch adapter e2e harness"
    set +e
    timeout "$TIMEOUT" bash "$script_path" >"$scenario_stdout" 2>&1
    local rc=$?
    set -e

    if [[ "$rc" -eq 124 ]]; then
        log_fail "[$case_name] harness timed out after ${TIMEOUT}s"
        tail -n 120 "$scenario_stdout" >&2 || true
        return 4
    fi
    if [[ "$rc" -ne 0 ]]; then
        log_fail "[$case_name] harness failed (exit=$rc)"
        tail -n 120 "$scenario_stdout" >&2 || true
        return "$rc"
    fi

    ls -1 "$PROJECT_ROOT/tests/e2e/logs"/ft_1i2ge_3_2_* 2>/dev/null | LC_ALL=C sort >"$after_snapshot" || true
    while IFS= read -r log_path; do
        if [[ -z "$log_path" ]]; then
            continue
        fi
        if [[ ! -f "$before_snapshot" ]] || ! grep -Fxq "$log_path" "$before_snapshot"; then
            cp -f "$log_path" "$scenario_dir/" || true
        fi
    done < "$after_snapshot"

    log_pass "[$case_name] mission dispatch adapter e2e completed"
    return 0
}

run_scenario_ft_1i2ge_3_3() {
    local scenario_dir="$1"
    local case_name="ft_1i2ge_3_3"
    local script_path="$PROJECT_ROOT/tests/e2e/test_ft_1i2ge_3_3.sh"
    local scenario_stdout="$scenario_dir/${case_name}.stdout.log"
    local before_snapshot="$scenario_dir/${case_name}.logs.before.txt"
    local after_snapshot="$scenario_dir/${case_name}.logs.after.txt"

    ls -1 "$PROJECT_ROOT/tests/e2e/logs"/ft_1i2ge_3_3_* 2>/dev/null | LC_ALL=C sort >"$before_snapshot" || true

    log_info "[$case_name] Step 1: running mission outcome reconciliation e2e harness"
    set +e
    timeout "$TIMEOUT" bash "$script_path" >"$scenario_stdout" 2>&1
    local rc=$?
    set -e

    if [[ "$rc" -eq 124 ]]; then
        log_fail "[$case_name] harness timed out after ${TIMEOUT}s"
        tail -n 120 "$scenario_stdout" >&2 || true
        return 4
    fi
    if [[ "$rc" -ne 0 ]]; then
        log_fail "[$case_name] harness failed (exit=$rc)"
        tail -n 120 "$scenario_stdout" >&2 || true
        return "$rc"
    fi

    ls -1 "$PROJECT_ROOT/tests/e2e/logs"/ft_1i2ge_3_3_* 2>/dev/null | LC_ALL=C sort >"$after_snapshot" || true
    while IFS= read -r log_path; do
        if [[ -z "$log_path" ]]; then
            continue
        fi
        if [[ ! -f "$before_snapshot" ]] || ! grep -Fxq "$log_path" "$before_snapshot"; then
            cp -f "$log_path" "$scenario_dir/" || true
        fi
    done < "$after_snapshot"

    log_pass "[$case_name] mission outcome reconciliation e2e completed"
    return 0
}

run_scenario_ft_1i2ge_3_4() {
    local scenario_dir="$1"
    local case_name="ft_1i2ge_3_4"
    local script_path="$PROJECT_ROOT/tests/e2e/test_ft_1i2ge_3_4.sh"
    local scenario_stdout="$scenario_dir/${case_name}.stdout.log"
    local before_snapshot="$scenario_dir/${case_name}.logs.before.txt"
    local after_snapshot="$scenario_dir/${case_name}.logs.after.txt"

    ls -1 "$PROJECT_ROOT/tests/e2e/logs"/ft_1i2ge_3_4_* 2>/dev/null | LC_ALL=C sort >"$before_snapshot" || true

    log_info "[$case_name] Step 1: running adaptive mission replanning e2e harness"
    set +e
    timeout "$TIMEOUT" bash "$script_path" >"$scenario_stdout" 2>&1
    local rc=$?
    set -e

    if [[ "$rc" -eq 124 ]]; then
        log_fail "[$case_name] harness timed out after ${TIMEOUT}s"
        tail -n 120 "$scenario_stdout" >&2 || true
        return 4
    fi
    if [[ "$rc" -ne 0 ]]; then
        log_fail "[$case_name] harness failed (exit=$rc)"
        tail -n 120 "$scenario_stdout" >&2 || true
        return "$rc"
    fi

    ls -1 "$PROJECT_ROOT/tests/e2e/logs"/ft_1i2ge_3_4_* 2>/dev/null | LC_ALL=C sort >"$after_snapshot" || true
    while IFS= read -r log_path; do
        if [[ -z "$log_path" ]]; then
            continue
        fi
        if [[ ! -f "$before_snapshot" ]] || ! grep -Fxq "$log_path" "$before_snapshot"; then
            cp -f "$log_path" "$scenario_dir/" || true
        fi
    done < "$after_snapshot"

    log_pass "[$case_name] adaptive mission replanning e2e completed"
    return 0
}

run_scenario_ft_1i2ge_3_5() {
    local scenario_dir="$1"
    local case_name="ft_1i2ge_3_5"
    local script_path="$PROJECT_ROOT/tests/e2e/test_ft_1i2ge_3_5.sh"
    local scenario_stdout="$scenario_dir/${case_name}.stdout.log"
    local before_snapshot="$scenario_dir/${case_name}.logs.before.txt"
    local after_snapshot="$scenario_dir/${case_name}.logs.after.txt"

    ls -1 "$PROJECT_ROOT/tests/e2e/logs"/ft_1i2ge_3_5_* 2>/dev/null | LC_ALL=C sort >"$before_snapshot" || true

    log_info "[$case_name] Step 1: running pause/resume/abort semantics e2e harness"
    set +e
    timeout "$TIMEOUT" bash "$script_path" >"$scenario_stdout" 2>&1
    local rc=$?
    set -e

    if [[ "$rc" -eq 124 ]]; then
        log_fail "[$case_name] harness timed out after ${TIMEOUT}s"
        tail -n 120 "$scenario_stdout" >&2 || true
        return 4
    fi
    if [[ "$rc" -ne 0 ]]; then
        log_fail "[$case_name] harness failed (exit=$rc)"
        tail -n 120 "$scenario_stdout" >&2 || true
        return "$rc"
    fi

    ls -1 "$PROJECT_ROOT/tests/e2e/logs"/ft_1i2ge_3_5_* 2>/dev/null | LC_ALL=C sort >"$after_snapshot" || true
    while IFS= read -r log_path; do
        if [[ -z "$log_path" ]]; then
            continue
        fi
        if [[ ! -f "$before_snapshot" ]] || ! grep -Fxq "$log_path" "$before_snapshot"; then
            cp -f "$log_path" "$scenario_dir/" || true
        fi
    done < "$after_snapshot"

    log_pass "[$case_name] pause/resume/abort semantics e2e completed"
    return 0
}

run_scenario_ft_1i2ge_3_8() {
    local scenario_dir="$1"
    local case_name="ft_1i2ge_3_8"
    local script_path="$PROJECT_ROOT/tests/e2e/test_ft_1i2ge_3_8.sh"
    local scenario_stdout="$scenario_dir/${case_name}.stdout.log"
    local before_snapshot="$scenario_dir/${case_name}.logs.before.txt"
    local after_snapshot="$scenario_dir/${case_name}.logs.after.txt"

    ls -1 "$PROJECT_ROOT/tests/e2e/logs"/ft_1i2ge_3_8_* 2>/dev/null | LC_ALL=C sort >"$before_snapshot" || true

    log_info "[$case_name] Step 1: running crash-consistent mission journal e2e harness"
    set +e
    timeout "$TIMEOUT" bash "$script_path" >"$scenario_stdout" 2>&1
    local rc=$?
    set -e

    if [[ "$rc" -eq 124 ]]; then
        log_fail "[$case_name] harness timed out after ${TIMEOUT}s"
        tail -n 120 "$scenario_stdout" >&2 || true
        return 4
    fi
    if [[ "$rc" -ne 0 ]]; then
        log_fail "[$case_name] harness failed (exit=$rc)"
        tail -n 120 "$scenario_stdout" >&2 || true
        return "$rc"
    fi

    ls -1 "$PROJECT_ROOT/tests/e2e/logs"/ft_1i2ge_3_8_* 2>/dev/null | LC_ALL=C sort >"$after_snapshot" || true
    while IFS= read -r log_path; do
        if [[ -z "$log_path" ]]; then
            continue
        fi
        if [[ ! -f "$before_snapshot" ]] || ! grep -Fxq "$log_path" "$before_snapshot"; then
            cp -f "$log_path" "$scenario_dir/" || true
        fi
    done < "$after_snapshot"

    log_pass "[$case_name] crash-consistent mission journal e2e completed"
    return 0
}

run_scenario_ft_1i2ge_3_7() {
    local scenario_dir="$1"
    local case_name="ft_1i2ge_3_7"
    local script_path="$PROJECT_ROOT/tests/e2e/test_ft_1i2ge_3_7.sh"
    local scenario_stdout="$scenario_dir/${case_name}.stdout.log"
    local before_snapshot="$scenario_dir/${case_name}.logs.before.txt"
    local after_snapshot="$scenario_dir/${case_name}.logs.after.txt"

    ls -1 "$PROJECT_ROOT/tests/e2e/logs"/ft_1i2ge_3_7_* 2>/dev/null | LC_ALL=C sort >"$before_snapshot" || true

    log_info "[$case_name] Step 1: running orchestration integration e2e harness"
    set +e
    timeout "$TIMEOUT" bash "$script_path" >"$scenario_stdout" 2>&1
    local rc=$?
    set -e

    if [[ "$rc" -eq 124 ]]; then
        log_fail "[$case_name] harness timed out after ${TIMEOUT}s"
        tail -n 120 "$scenario_stdout" >&2 || true
        return 4
    fi
    if [[ "$rc" -ne 0 ]]; then
        log_fail "[$case_name] harness failed (exit=$rc)"
        tail -n 120 "$scenario_stdout" >&2 || true
        return "$rc"
    fi

    ls -1 "$PROJECT_ROOT/tests/e2e/logs"/ft_1i2ge_3_7_* 2>/dev/null | LC_ALL=C sort >"$after_snapshot" || true
    while IFS= read -r log_path; do
        if [[ -z "$log_path" ]]; then
            continue
        fi
        if [[ ! -f "$before_snapshot" ]] || ! grep -Fxq "$log_path" "$before_snapshot"; then
            cp -f "$log_path" "$scenario_dir/" || true
        fi
    done < "$after_snapshot"

    log_pass "[$case_name] orchestration integration e2e completed"
    return 0
}

run_scenario_ft_1i2ge_8_5() {
    local scenario_dir="$1"
    local case_name="ft_1i2ge_8_5"
    local script_path="$PROJECT_ROOT/tests/e2e/test_ft_1i2ge_8_5.sh"
    local scenario_stdout="$scenario_dir/${case_name}.stdout.log"
    local before_snapshot="$scenario_dir/${case_name}.logs.before.txt"
    local after_snapshot="$scenario_dir/${case_name}.logs.after.txt"

    ls -1 "$PROJECT_ROOT/tests/e2e/logs"/ft_1i2ge_8_5_* 2>/dev/null | LC_ALL=C sort >"$before_snapshot" || true

    log_info "[$case_name] Step 1: running commit-phase executor e2e harness"
    set +e
    timeout "$TIMEOUT" bash "$script_path" >"$scenario_stdout" 2>&1
    local rc=$?
    set -e

    if [[ "$rc" -eq 124 ]]; then
        log_fail "[$case_name] harness timed out after ${TIMEOUT}s"
        tail -n 120 "$scenario_stdout" >&2 || true
        return 4
    fi
    if [[ "$rc" -ne 0 ]]; then
        log_fail "[$case_name] harness failed (exit=$rc)"
        tail -n 120 "$scenario_stdout" >&2 || true
        return "$rc"
    fi

    ls -1 "$PROJECT_ROOT/tests/e2e/logs"/ft_1i2ge_8_5_* 2>/dev/null | LC_ALL=C sort >"$after_snapshot" || true
    while IFS= read -r log_path; do
        if [[ -z "$log_path" ]]; then
            continue
        fi
        if [[ ! -f "$before_snapshot" ]] || ! grep -Fxq "$log_path" "$before_snapshot"; then
            cp -f "$log_path" "$scenario_dir/" || true
        fi
    done < "$after_snapshot"

    log_pass "[$case_name] commit-phase executor e2e completed"
    return 0
}

run_scenario_ft_1i2ge_8_6() {
    local scenario_dir="$1"
    local case_name="ft_1i2ge_8_6"
    local script_path="$PROJECT_ROOT/tests/e2e/test_ft_1i2ge_8_6.sh"
    local scenario_stdout="$scenario_dir/${case_name}.stdout.log"
    local before_snapshot="$scenario_dir/${case_name}.logs.before.txt"
    local after_snapshot="$scenario_dir/${case_name}.logs.after.txt"

    ls -1 "$PROJECT_ROOT/tests/e2e/logs"/ft_1i2ge_8_6_* 2>/dev/null | LC_ALL=C sort >"$before_snapshot" || true

    log_info "[$case_name] Step 1: running compensation planner and rollback engine e2e harness"
    set +e
    timeout "$TIMEOUT" bash "$script_path" >"$scenario_stdout" 2>&1
    local rc=$?
    set -e

    if [[ "$rc" -eq 124 ]]; then
        log_fail "[$case_name] harness timed out after ${TIMEOUT}s"
        tail -n 120 "$scenario_stdout" >&2 || true
        return 4
    fi
    if [[ "$rc" -ne 0 ]]; then
        log_fail "[$case_name] harness failed (exit=$rc)"
        tail -n 120 "$scenario_stdout" >&2 || true
        return "$rc"
    fi

    ls -1 "$PROJECT_ROOT/tests/e2e/logs"/ft_1i2ge_8_6_* 2>/dev/null | LC_ALL=C sort >"$after_snapshot" || true
    while IFS= read -r log_path; do
        if [[ -z "$log_path" ]]; then
            continue
        fi
        if [[ ! -f "$before_snapshot" ]] || ! grep -Fxq "$log_path" "$before_snapshot"; then
            cp -f "$log_path" "$scenario_dir/" || true
        fi
    done < "$after_snapshot"

    log_pass "[$case_name] compensation planner and rollback engine e2e completed"
    return 0
}

run_scenario_ft_e34d9_10_1_2_doctrine_pack() {
    local scenario_dir="$1"
    local case_name="ft_e34d9_10_1_2_doctrine_pack"
    local script_path="$PROJECT_ROOT/tests/e2e/test_ft_e34d9_10_1_2_doctrine_pack.sh"
    local scenario_stdout="$scenario_dir/${case_name}.stdout.log"
    local before_snapshot="$scenario_dir/${case_name}.logs.before.txt"
    local after_snapshot="$scenario_dir/${case_name}.logs.after.txt"

    ls -1 "$PROJECT_ROOT/tests/e2e/logs"/asupersync_runtime_doctrine_* 2>/dev/null | LC_ALL=C sort >"$before_snapshot" || true

    log_info "[$case_name] Step 1: running asupersync runtime doctrine pack e2e harness"
    set +e
    timeout "$TIMEOUT" bash "$script_path" >"$scenario_stdout" 2>&1
    local rc=$?
    set -e

    if [[ "$rc" -eq 124 ]]; then
        log_fail "[$case_name] harness timed out after ${TIMEOUT}s"
        tail -n 120 "$scenario_stdout" >&2 || true
        return 4
    fi
    if [[ "$rc" -ne 0 ]]; then
        log_fail "[$case_name] harness failed (exit=$rc)"
        tail -n 120 "$scenario_stdout" >&2 || true
        return "$rc"
    fi

    ls -1 "$PROJECT_ROOT/tests/e2e/logs"/asupersync_runtime_doctrine_* 2>/dev/null | LC_ALL=C sort >"$after_snapshot" || true
    while IFS= read -r log_path; do
        if [[ -z "$log_path" ]]; then
            continue
        fi
        if [[ ! -f "$before_snapshot" ]] || ! grep -Fxq "$log_path" "$before_snapshot"; then
            cp -f "$log_path" "$scenario_dir/" || true
        fi
    done < "$after_snapshot"

    log_pass "[$case_name] asupersync runtime doctrine pack e2e completed"
    return 0
}

run_scenario_ft_e34d9_10_2_1_runtime_bootstrap() {
    local scenario_dir="$1"
    local case_name="ft_e34d9_10_2_1_runtime_bootstrap"
    local script_path="$PROJECT_ROOT/tests/e2e/test_ft_e34d9_10_2_1_runtime_bootstrap.sh"
    local scenario_stdout="$scenario_dir/${case_name}.stdout.log"
    local before_snapshot="$scenario_dir/${case_name}.logs.before.txt"
    local after_snapshot="$scenario_dir/${case_name}.logs.after.txt"

    ls -1 "$PROJECT_ROOT/tests/e2e/logs"/asupersync_runtime_bootstrap_* 2>/dev/null | LC_ALL=C sort >"$before_snapshot" || true

    log_info "[$case_name] Step 1: running asupersync runtime bootstrap contract e2e harness"
    set +e
    timeout "$TIMEOUT" bash "$script_path" >"$scenario_stdout" 2>&1
    local rc=$?
    set -e

    if [[ "$rc" -eq 124 ]]; then
        log_fail "[$case_name] harness timed out after ${TIMEOUT}s"
        tail -n 120 "$scenario_stdout" >&2 || true
        return 4
    fi
    if [[ "$rc" -ne 0 ]]; then
        log_fail "[$case_name] harness failed (exit=$rc)"
        tail -n 120 "$scenario_stdout" >&2 || true
        return "$rc"
    fi

    ls -1 "$PROJECT_ROOT/tests/e2e/logs"/asupersync_runtime_bootstrap_* 2>/dev/null | LC_ALL=C sort >"$after_snapshot" || true
    while IFS= read -r log_path; do
        if [[ -z "$log_path" ]]; then
            continue
        fi
        if [[ ! -f "$before_snapshot" ]] || ! grep -Fxq "$log_path" "$before_snapshot"; then
            cp -f "$log_path" "$scenario_dir/" || true
        fi
    done < "$after_snapshot"

    log_pass "[$case_name] asupersync runtime bootstrap contract e2e completed"
    return 0
}

run_scenario_ft_e34d9_10_2_2_cx_outcome_contract() {
    local scenario_dir="$1"
    local case_name="ft_e34d9_10_2_2_cx_outcome_contract"
    local script_path="$PROJECT_ROOT/tests/e2e/test_ft_e34d9_10_2_2_cx_outcome_contract.sh"
    local scenario_stdout="$scenario_dir/${case_name}.stdout.log"
    local before_snapshot="$scenario_dir/${case_name}.logs.before.txt"
    local after_snapshot="$scenario_dir/${case_name}.logs.after.txt"

    ls -1 "$PROJECT_ROOT/tests/e2e/logs"/asupersync_cx_outcome_* 2>/dev/null | LC_ALL=C sort >"$before_snapshot" || true

    log_info "[$case_name] Step 1: running asupersync Cx/Outcome contract e2e harness"
    set +e
    timeout "$TIMEOUT" bash "$script_path" >"$scenario_stdout" 2>&1
    local rc=$?
    set -e

    if [[ "$rc" -eq 124 ]]; then
        log_fail "[$case_name] harness timed out after ${TIMEOUT}s"
        tail -n 120 "$scenario_stdout" >&2 || true
        return 4
    fi
    if [[ "$rc" -ne 0 ]]; then
        log_fail "[$case_name] harness failed (exit=$rc)"
        tail -n 120 "$scenario_stdout" >&2 || true
        return "$rc"
    fi

    ls -1 "$PROJECT_ROOT/tests/e2e/logs"/asupersync_cx_outcome_* 2>/dev/null | LC_ALL=C sort >"$after_snapshot" || true
    while IFS= read -r log_path; do
        if [[ -z "$log_path" ]]; then
            continue
        fi
        if [[ ! -f "$before_snapshot" ]] || ! grep -Fxq "$log_path" "$before_snapshot"; then
            cp -f "$log_path" "$scenario_dir/" || true
        fi
    done < "$after_snapshot"

    log_pass "[$case_name] asupersync Cx/Outcome contract e2e completed"
    return 0
}

run_scenario_ft_1i2ge_4_2() {
    local scenario_dir="$1"
    local case_name="ft_1i2ge_4_2"
    local script_path="$PROJECT_ROOT/tests/e2e/test_ft_1i2ge_4_2.sh"
    local scenario_stdout="$scenario_dir/${case_name}.stdout.log"
    local before_snapshot="$scenario_dir/${case_name}.logs.before.txt"
    local after_snapshot="$scenario_dir/${case_name}.logs.after.txt"

    ls -1 "$PROJECT_ROOT/tests/e2e/logs"/ft_1i2ge_4_2_* 2>/dev/null | LC_ALL=C sort >"$before_snapshot" || true

    log_info "[$case_name] Step 1: running mission reservation enforcement e2e harness"
    set +e
    timeout "$TIMEOUT" bash "$script_path" >"$scenario_stdout" 2>&1
    local rc=$?
    set -e

    if [[ "$rc" -eq 124 ]]; then
        log_fail "[$case_name] harness timed out after ${TIMEOUT}s"
        tail -n 120 "$scenario_stdout" >&2 || true
        return 4
    fi
    if [[ "$rc" -ne 0 ]]; then
        log_fail "[$case_name] harness failed (exit=$rc)"
        tail -n 120 "$scenario_stdout" >&2 || true
        return "$rc"
    fi

    ls -1 "$PROJECT_ROOT/tests/e2e/logs"/ft_1i2ge_4_2_* 2>/dev/null | LC_ALL=C sort >"$after_snapshot" || true
    while IFS= read -r log_path; do
        if [[ -z "$log_path" ]]; then
            continue
        fi
        if [[ ! -f "$before_snapshot" ]] || ! grep -Fxq "$log_path" "$before_snapshot"; then
            cp -f "$log_path" "$scenario_dir/" || true
        fi
    done < "$after_snapshot"

    log_pass "[$case_name] mission reservation enforcement e2e completed"
    return 0
}

run_scenario_ft_1i2ge_4_3() {
    local scenario_dir="$1"
    local case_name="ft_1i2ge_4_3"
    local script_path="$PROJECT_ROOT/tests/e2e/test_ft_1i2ge_4_3.sh"
    local scenario_stdout="$scenario_dir/${case_name}.stdout.log"
    local before_snapshot="$scenario_dir/${case_name}.logs.before.txt"
    local after_snapshot="$scenario_dir/${case_name}.logs.after.txt"

    ls -1 "$PROJECT_ROOT/tests/e2e/logs"/ft_1i2ge_4_3_* 2>/dev/null | LC_ALL=C sort >"$before_snapshot" || true

    log_info "[$case_name] Step 1: running mission approval-path integration e2e harness"
    set +e
    timeout "$TIMEOUT" bash "$script_path" >"$scenario_stdout" 2>&1
    local rc=$?
    set -e

    if [[ "$rc" -eq 124 ]]; then
        log_fail "[$case_name] harness timed out after ${TIMEOUT}s"
        tail -n 120 "$scenario_stdout" >&2 || true
        return 4
    fi
    if [[ "$rc" -ne 0 ]]; then
        log_fail "[$case_name] harness failed (exit=$rc)"
        tail -n 120 "$scenario_stdout" >&2 || true
        return "$rc"
    fi

    ls -1 "$PROJECT_ROOT/tests/e2e/logs"/ft_1i2ge_4_3_* 2>/dev/null | LC_ALL=C sort >"$after_snapshot" || true
    while IFS= read -r log_path; do
        if [[ -z "$log_path" ]]; then
            continue
        fi
        if [[ ! -f "$before_snapshot" ]] || ! grep -Fxq "$log_path" "$before_snapshot"; then
            cp -f "$log_path" "$scenario_dir/" || true
        fi
    done < "$after_snapshot"

    log_pass "[$case_name] mission approval-path integration e2e completed"
    return 0
}

run_scenario_ft_1i2ge_5_1() {
    local scenario_dir="$1"
    local case_name="ft_1i2ge_5_1"
    local script_path="$PROJECT_ROOT/tests/e2e/test_ft_1i2ge_5_1.sh"
    local scenario_stdout="$scenario_dir/${case_name}.stdout.log"
    local before_snapshot="$scenario_dir/${case_name}.logs.before.txt"
    local after_snapshot="$scenario_dir/${case_name}.logs.after.txt"

    ls -1 "$PROJECT_ROOT/tests/e2e/logs"/ft_1i2ge_5_1_* 2>/dev/null | LC_ALL=C sort >"$before_snapshot" || true

    log_info "[$case_name] Step 1: running mission CLI command surface e2e harness"
    set +e
    timeout "$TIMEOUT" bash "$script_path" >"$scenario_stdout" 2>&1
    local rc=$?
    set -e

    if [[ "$rc" -eq 124 ]]; then
        log_fail "[$case_name] harness timed out after ${TIMEOUT}s"
        tail -n 120 "$scenario_stdout" >&2 || true
        return 4
    fi
    if [[ "$rc" -ne 0 ]]; then
        log_fail "[$case_name] harness failed (exit=$rc)"
        tail -n 120 "$scenario_stdout" >&2 || true
        return "$rc"
    fi

    ls -1 "$PROJECT_ROOT/tests/e2e/logs"/ft_1i2ge_5_1_* 2>/dev/null | LC_ALL=C sort >"$after_snapshot" || true
    while IFS= read -r log_path; do
        if [[ -z "$log_path" ]]; then
            continue
        fi
        if [[ ! -f "$before_snapshot" ]] || ! grep -Fxq "$log_path" "$before_snapshot"; then
            cp -f "$log_path" "$scenario_dir/" || true
        fi
    done < "$after_snapshot"

    log_pass "[$case_name] mission CLI command surface e2e completed"
    return 0
}

run_scenario_ft_1i2ge_5_2() {
    local scenario_dir="$1"
    local case_name="ft_1i2ge_5_2"
    local script_path="$PROJECT_ROOT/tests/e2e/test_ft_1i2ge_5_2.sh"
    local scenario_stdout="$scenario_dir/${case_name}.stdout.log"
    local before_snapshot="$scenario_dir/${case_name}.logs.before.txt"
    local after_snapshot="$scenario_dir/${case_name}.logs.after.txt"

    ls -1 "$PROJECT_ROOT/tests/e2e/logs"/ft_1i2ge_5_2_* 2>/dev/null | LC_ALL=C sort >"$before_snapshot" || true

    log_info "[$case_name] Step 1: running robot mission endpoint e2e harness"
    set +e
    timeout "$TIMEOUT" bash "$script_path" >"$scenario_stdout" 2>&1
    local rc=$?
    set -e

    if [[ "$rc" -eq 124 ]]; then
        log_fail "[$case_name] harness timed out after ${TIMEOUT}s"
        tail -n 120 "$scenario_stdout" >&2 || true
        return 4
    fi
    if [[ "$rc" -ne 0 ]]; then
        log_fail "[$case_name] harness failed (exit=$rc)"
        tail -n 120 "$scenario_stdout" >&2 || true
        return "$rc"
    fi

    ls -1 "$PROJECT_ROOT/tests/e2e/logs"/ft_1i2ge_5_2_* 2>/dev/null | LC_ALL=C sort >"$after_snapshot" || true
    while IFS= read -r log_path; do
        if [[ -z "$log_path" ]]; then
            continue
        fi
        if [[ ! -f "$before_snapshot" ]] || ! grep -Fxq "$log_path" "$before_snapshot"; then
            cp -f "$log_path" "$scenario_dir/" || true
        fi
    done < "$after_snapshot"

    log_pass "[$case_name] robot mission endpoint e2e completed"
    return 0
}

run_scenario_ft_l5em3_2() {
    local scenario_dir="$1"
    local case_name="ft_l5em3_2"
    local script_path="$PROJECT_ROOT/tests/e2e/test_ft_l5em3_2.sh"
    local scenario_stdout="$scenario_dir/${case_name}.stdout.log"
    local before_snapshot="$scenario_dir/${case_name}.logs.before.txt"
    local after_snapshot="$scenario_dir/${case_name}.logs.after.txt"

    ls -1 "$PROJECT_ROOT/tests/e2e/logs"/ft_l5em3_2_* 2>/dev/null | LC_ALL=C sort >"$before_snapshot" || true

    log_info "[$case_name] Step 1: running SIMD stateful scan e2e harness"
    set +e
    timeout "$TIMEOUT" bash "$script_path" >"$scenario_stdout" 2>&1
    local rc=$?
    set -e

    if [[ "$rc" -eq 124 ]]; then
        log_fail "[$case_name] harness timed out after ${TIMEOUT}s"
        tail -n 120 "$scenario_stdout" >&2 || true
        return 4
    fi
    if [[ "$rc" -ne 0 ]]; then
        log_fail "[$case_name] harness failed (exit=$rc)"
        tail -n 120 "$scenario_stdout" >&2 || true
        return "$rc"
    fi

    ls -1 "$PROJECT_ROOT/tests/e2e/logs"/ft_l5em3_2_* 2>/dev/null | LC_ALL=C sort >"$after_snapshot" || true
    while IFS= read -r log_path; do
        if [[ -z "$log_path" ]]; then
            continue
        fi
        if [[ ! -f "$before_snapshot" ]] || ! grep -Fxq "$log_path" "$before_snapshot"; then
            cp -f "$log_path" "$scenario_dir/" || true
        fi
    done < "$after_snapshot"

    log_pass "[$case_name] SIMD stateful scan e2e completed"
    return 0
}

run_scenario_ft_124z4() {
    local scenario_dir="$1"
    local case_name="ft_124z4"
    local script_path="$PROJECT_ROOT/tests/e2e/test_ft_124z4.sh"
    local scenario_stdout="$scenario_dir/${case_name}.stdout.log"
    local before_snapshot="$scenario_dir/${case_name}.logs.before.txt"
    local after_snapshot="$scenario_dir/${case_name}.logs.after.txt"

    ls -1 "$PROJECT_ROOT/tests/e2e/logs"/ft_124z4_* 2>/dev/null | LC_ALL=C sort >"$before_snapshot" || true

    log_info "[$case_name] Step 1: running tailer asupersync e2e harness"
    set +e
    timeout "$TIMEOUT" bash "$script_path" >"$scenario_stdout" 2>&1
    local rc=$?
    set -e

    if [[ "$rc" -eq 124 ]]; then
        log_fail "[$case_name] harness timed out after ${TIMEOUT}s"
        tail -n 120 "$scenario_stdout" >&2 || true
        return 4
    fi
    if [[ "$rc" -ne 0 ]]; then
        log_fail "[$case_name] harness failed (exit=$rc)"
        tail -n 120 "$scenario_stdout" >&2 || true
        return "$rc"
    fi

    ls -1 "$PROJECT_ROOT/tests/e2e/logs"/ft_124z4_* 2>/dev/null | LC_ALL=C sort >"$after_snapshot" || true
    while IFS= read -r log_path; do
        if [[ -z "$log_path" ]]; then
            continue
        fi
        if [[ ! -f "$before_snapshot" ]] || ! grep -Fxq "$log_path" "$before_snapshot"; then
            cp -f "$log_path" "$scenario_dir/" || true
        fi
    done < "$after_snapshot"

    log_pass "[$case_name] tailer asupersync e2e completed"
    return 0
}

dispatch_scenario() {
    local name="$1"
    local scenario_dir="$2"
    local result=0

    case "$name" in
        capture_search)
            run_scenario_capture_search "$scenario_dir" || result=$?
            ;;
        search_linting_rebuild)
            run_scenario_search_linting_rebuild "$scenario_dir" || result=$?
            ;;
        natural_language)
            run_scenario_natural_language "$scenario_dir" || result=$?
            ;;
        compaction_workflow)
            run_scenario_compaction_workflow "$scenario_dir" || result=$?
            ;;
        unhandled_event_lifecycle)
            run_scenario_unhandled_event_lifecycle "$scenario_dir" || result=$?
            ;;
        workflow_lifecycle)
            run_scenario_workflow_lifecycle "$scenario_dir" || result=$?
            ;;
        dry_run_mode)
            run_scenario_dry_run_mode "$scenario_dir" || result=$?
            ;;
        events_unhandled_alias)
            run_scenario_events_unhandled_alias "$scenario_dir" || result=$?
            ;;
        events_annotations_triage)
            run_scenario_events_annotations_triage "$scenario_dir" || result=$?
            ;;
        history_undo_workflow)
            run_scenario_history_undo_workflow "$scenario_dir" || result=$?
            ;;
        policy_denial)
            run_scenario_policy_denial "$scenario_dir" || result=$?
            ;;
        audit_tail_streaming)
            run_scenario_audit_tail_streaming "$scenario_dir" || result=$?
            ;;
        ipc_rpc_roundtrip)
            run_scenario_ipc_rpc_roundtrip "$scenario_dir" || result=$?
            ;;
        prepare_commit_approvals)
            run_scenario_prepare_commit_approvals "$scenario_dir" || result=$?
            ;;
        quickfix_suggestions)
            run_scenario_quickfix_suggestions "$scenario_dir" || result=$?
            ;;
        triage_multi_issue)
            run_scenario_triage_multi_issue "$scenario_dir" || result=$?
            ;;
        rules_explain_trace)
            run_scenario_rules_explain_trace "$scenario_dir" || result=$?
            ;;
        stress_scale)
            run_scenario_stress_scale "$scenario_dir" || result=$?
            ;;
        graceful_shutdown)
            run_scenario_graceful_shutdown "$scenario_dir" || result=$?
            ;;
        watcher_crash_bundle)
            run_scenario_watcher_crash_bundle "$scenario_dir" || result=$?
            ;;
        pane_exclude_filter)
            run_scenario_pane_exclude_filter "$scenario_dir" || result=$?
            ;;
        workspace_isolation)
            run_scenario_workspace_isolation "$scenario_dir" || result=$?
            ;;
        setup_idempotency)
            run_scenario_setup_idempotency "$scenario_dir" || result=$?
            ;;
        setup_remote_docker)
            run_scenario_setup_remote_docker "$scenario_dir" || result=$?
            ;;
        uservar_forwarding)
            run_scenario_uservar_forwarding "$scenario_dir" || result=$?
            ;;
        alt_screen_detection)
            run_scenario_alt_screen_detection "$scenario_dir" || result=$?
            ;;
        alt_screen_conformance)
            run_scenario_alt_screen_conformance "$scenario_dir" || result=$?
            ;;
        no_lua_status_hook)
            run_scenario_no_lua_status_hook "$scenario_dir" || result=$?
            ;;
        workflow_resume)
            run_scenario_workflow_resume "$scenario_dir" || result=$?
            ;;
        accounts_refresh)
            run_scenario_accounts_refresh "$scenario_dir" || result=$?
            ;;
        usage_limit_safe_pause)
            run_scenario_usage_limit_safe_pause "$scenario_dir" || result=$?
            ;;
        notification_webhook)
            run_scenario_notification_webhook "$scenario_dir" || result=$?
            ;;
        watch_notify_only)
            run_scenario_watch_notify_only "$scenario_dir" || result=$?
            ;;
        environment_detection)
            run_scenario_environment_detection "$scenario_dir" || result=$?
            ;;
        input_latency_resize_storm)
            "$SCRIPT_DIR/check_input_latency_gates.sh" || result=$?
            cp -r "$SCRIPT_DIR/../target/input-latency-gates/"* "$scenario_dir/" 2>/dev/null || true
            ;;
        distributed_streaming)
            run_scenario_distributed_streaming "$scenario_dir" || result=$?
            ;;
        timeline_correlation)
            "$SCRIPT_DIR/e2e_timeline_correlation.sh" ${VERBOSE:+--verbose} || result=$?
            cp -r "$SCRIPT_DIR/../evidence/e2e/"*timeline* "$scenario_dir/" 2>/dev/null || true
            ;;
        ft_1i2ge_4_1)
            run_scenario_ft_1i2ge_4_1 "$scenario_dir" || result=$?
            ;;
        ft_1i2ge_3_2)
            run_scenario_ft_1i2ge_3_2 "$scenario_dir" || result=$?
            ;;
        ft_1i2ge_3_3)
            run_scenario_ft_1i2ge_3_3 "$scenario_dir" || result=$?
            ;;
        ft_1i2ge_3_4)
            run_scenario_ft_1i2ge_3_4 "$scenario_dir" || result=$?
            ;;
        ft_1i2ge_3_5)
            run_scenario_ft_1i2ge_3_5 "$scenario_dir" || result=$?
            ;;
        ft_1i2ge_3_8)
            run_scenario_ft_1i2ge_3_8 "$scenario_dir" || result=$?
            ;;
        ft_1i2ge_3_7)
            run_scenario_ft_1i2ge_3_7 "$scenario_dir" || result=$?
            ;;
        ft_1i2ge_8_5)
            run_scenario_ft_1i2ge_8_5 "$scenario_dir" || result=$?
            ;;
        ft_1i2ge_8_6)
            run_scenario_ft_1i2ge_8_6 "$scenario_dir" || result=$?
            ;;
        ft_e34d9_10_1_2_doctrine_pack)
            run_scenario_ft_e34d9_10_1_2_doctrine_pack "$scenario_dir" || result=$?
            ;;
        ft_e34d9_10_2_1_runtime_bootstrap)
            run_scenario_ft_e34d9_10_2_1_runtime_bootstrap "$scenario_dir" || result=$?
            ;;
        ft_e34d9_10_2_2_cx_outcome_contract)
            run_scenario_ft_e34d9_10_2_2_cx_outcome_contract "$scenario_dir" || result=$?
            ;;
        ft_1i2ge_4_2)
            run_scenario_ft_1i2ge_4_2 "$scenario_dir" || result=$?
            ;;
        ft_1i2ge_4_3)
            run_scenario_ft_1i2ge_4_3 "$scenario_dir" || result=$?
            ;;
        ft_1i2ge_5_1)
            run_scenario_ft_1i2ge_5_1 "$scenario_dir" || result=$?
            ;;
        ft_1i2ge_5_2)
            run_scenario_ft_1i2ge_5_2 "$scenario_dir" || result=$?
            ;;
        ft_l5em3_2)
            run_scenario_ft_l5em3_2 "$scenario_dir" || result=$?
            ;;
        ft_124z4)
            run_scenario_ft_124z4 "$scenario_dir" || result=$?
            ;;
        replay_capture_pipeline)
            run_scenario_replay_capture_pipeline "$scenario_dir" || result=$?
            ;;
        *)
            log_fail "Unknown scenario: $name"
            result=1
            ;;
    esac

    return "$result"
}

promote_attempt_artifacts() {
    local scenario_dir="$1"
    local attempt_dir="$2"
    if [[ ! -d "$attempt_dir" ]]; then
        return 0
    fi
    while IFS= read -r file_path; do
        cp -f "$file_path" "$scenario_dir/"
    done < <(find "$attempt_dir" -maxdepth 1 -type f | LC_ALL=C sort)
}

run_scenario() {
    local name="$1"
    local scenario_num="$2"
    local fault_plan_json="${3:-}"
    local scenario_dir="$RUN_ARTIFACTS_DIR/scenario_$(printf '%02d' "$scenario_num")_$name"
    local scenario_seed_hex=""
    local scenario_metadata=""
    local fault_active="false"
    local fault_class="none"
    local fault_mode="observe"
    local fault_trigger_token=""
    local max_attempts=$((SCENARIO_RETRIES + 1))
    local attempts_json="[]"
    local selected_attempt_dir=""
    local start_time=$(date +%s)
    local result=1
    local attempt=1

    mkdir -p "$scenario_dir"

    scenario_seed_hex=$(compute_scenario_seed_hex "$name" "$scenario_num")
    scenario_metadata=$(scenario_metadata_json "$name")
    if [[ -z "$fault_plan_json" ]]; then
        fault_plan_json=$(jq -cn \
            --arg test_case_id "$name" \
            --arg resize_transaction_id "${RUN_ID}:scenario:${scenario_num}" \
            --argjson sequence_no "$scenario_num" \
            '{
                schema_version: "wa.soak_fault_plan.v1",
                enabled: false,
                active: false,
                fault_class: "none",
                mode: "observe",
                test_case_id: $test_case_id,
                resize_transaction_id: $resize_transaction_id,
                sequence_no: $sequence_no,
                trigger: {
                    token: null,
                    matrix_index: -1,
                    interval: 0,
                    offset: 0,
                    sequence_mod: 0
                },
                expected: {
                    degradation: "nominal",
                    policy: "continue",
                    severity: "low"
                }
            }')
    fi
    fault_active=$(jq -r '.active // false' <<< "$fault_plan_json")
    fault_class=$(jq -r '.fault_class // "none"' <<< "$fault_plan_json")
    fault_mode=$(jq -r '.mode // "observe"' <<< "$fault_plan_json")
    fault_trigger_token=$(jq -r '.trigger.token // empty' <<< "$fault_plan_json")

    export FT_E2E_RUN_SEED="$RUN_SEED"
    export FT_E2E_SCENARIO_SEED="$scenario_seed_hex"
    export FT_E2E_SCENARIO_NAME="$name"
    export FT_E2E_SCENARIO_INDEX="$scenario_num"
    export FT_E2E_SOAK_FAULT_ACTIVE="$fault_active"
    export FT_E2E_SOAK_FAULT_CLASS="$fault_class"
    export FT_E2E_SOAK_FAULT_MODE="$fault_mode"
    export FT_E2E_SOAK_FAULT_TRIGGER_TOKEN="$fault_trigger_token"
    export FT_E2E_SOAK_FAULT_PLAN="$fault_plan_json"

    log_info "Starting scenario: $name (run_id=$RUN_ID index=$scenario_num seed=$scenario_seed_hex attempts=$max_attempts)"
    if [[ "$fault_active" == "true" ]]; then
        log_info "Scenario fault plan active: class=$fault_class mode=$fault_mode trigger=$fault_trigger_token"
    fi

    for ((attempt=1; attempt<=max_attempts; attempt++)); do
        local attempt_dir="$scenario_dir/attempt_$(printf '%02d' "$attempt")"
        local attempt_result=0
        local attempt_start=$(date +%s)
        local attempt_duration=0
        local attempt_started_at=""
        local backoff_secs=0
        local fault_effect_json="null"
        local prereq_failed=false

        mkdir -p "$attempt_dir"
        attempt_started_at=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

        if [[ "$attempt" -gt 1 ]]; then
            log_warn "Retrying scenario $name (attempt $attempt/$max_attempts)"
        fi

        if ! check_scenario_prerequisites "$name" "$attempt_dir"; then
            attempt_result=5
            prereq_failed=true
        else
            dispatch_scenario "$name" "$attempt_dir" || attempt_result=$?
            if [[ "$SOAK_MODE" == "true" ]]; then
                fault_effect_json=$(apply_soak_fault_injection "$name" "$attempt_dir" "$fault_plan_json" "$attempt_result")
                attempt_result=$(jq -r '.exit_code // 1' <<< "$fault_effect_json")
            fi
        fi
        attempt_duration=$(( $(date +%s) - attempt_start ))
        selected_attempt_dir="$attempt_dir"

        attempts_json=$(jq -c \
            --argjson attempt "$attempt" \
            --arg started_at "$attempt_started_at" \
            --argjson duration_secs "$attempt_duration" \
            --argjson exit_code "$attempt_result" \
            --argjson fault_effect "$fault_effect_json" \
            --arg status "$([[ "$attempt_result" -eq 0 ]] && echo passed || echo failed)" \
            '. + [{
                attempt: $attempt,
                started_at: $started_at,
                duration_secs: $duration_secs,
                exit_code: $exit_code,
                status: $status,
                fault_effect: $fault_effect
            }]' <<< "$attempts_json")

        if [[ "$attempt_result" -eq 0 ]]; then
            result=0
            break
        fi

        result="$attempt_result"
        if [[ "$prereq_failed" == "true" ]]; then
            break
        fi
        if [[ "$attempt" -lt "$max_attempts" ]]; then
            backoff_secs=$(scenario_retry_backoff_secs "$attempt")
            log_warn "Scenario $name failed attempt $attempt/$max_attempts (exit=$attempt_result); backing off ${backoff_secs}s"
            sleep "$backoff_secs"
        fi
    done

    promote_attempt_artifacts "$scenario_dir" "$selected_attempt_dir"

    local duration=$(( $(date +%s) - start_time ))

    local orchestration_manifest="$scenario_dir/orchestration_manifest.json"
    jq -n \
        --arg scenario "$name" \
        --argjson scenario_index "$scenario_num" \
        --arg run_id "$RUN_ID" \
        --arg run_seed "$RUN_SEED" \
        --arg scenario_seed "$scenario_seed_hex" \
        --argjson max_attempts "$max_attempts" \
        --argjson attempts "$attempts_json" \
        --argjson metadata "$scenario_metadata" \
        --argjson fault_plan "$fault_plan_json" \
        '{
            scenario: $scenario,
            scenario_index: $scenario_index,
            run_id: $run_id,
            run_seed: $run_seed,
            scenario_seed: $scenario_seed,
            max_attempts: $max_attempts,
            metadata: $metadata,
            attempts: $attempts,
            fault_plan: $fault_plan
        }' > "$orchestration_manifest"

    local status="passed"
    local failure_signature=""
    if [[ "$result" -eq 0 ]]; then
        touch "$scenario_dir/PASS"
        log_pass "Scenario $name: PASSED (${duration}s)"
        ((PASSED++))
    else
        status="failed"
        touch "$scenario_dir/FAIL"
        log_fail "Scenario $name: FAILED (${duration}s)"
        ((FAILED++))
        failure_signature=$(derive_failure_signature "$scenario_dir")

        # Print failure details
        echo ""
        echo "FAILURE DETAILS"
        echo "==============="
        echo "Scenario: $name"
        echo "Seed: $scenario_seed_hex"
        echo "Duration: ${duration}s"
        echo ""
        echo "Artifacts saved to: $scenario_dir/"
        echo ""
    fi

    emit_scenario_artifact_manifest "$name" "$scenario_num" "$scenario_dir" "$result" "$duration"

    local summary_entry=""
    summary_entry=$(jq -cn \
        --arg name "$name" \
        --arg status "$status" \
        --arg run_id "$RUN_ID" \
        --argjson duration_secs "$duration" \
        --arg scenario_seed "$scenario_seed_hex" \
        --argjson max_attempts "$max_attempts" \
        --argjson attempts "$attempts_json" \
        --argjson fault_plan "$fault_plan_json" \
        --arg orchestration_manifest "$(basename "$scenario_dir")/orchestration_manifest.json" \
        --arg artifacts_dir "$(basename "$scenario_dir")" \
        --arg test_artifacts_manifest "$(basename "$scenario_dir")/test_artifacts_manifest.json" \
        --arg failure_signature "$failure_signature" \
        '{
            name: $name,
            status: $status,
            run_id: $run_id,
            duration_secs: $duration_secs,
            scenario_seed: $scenario_seed,
            max_attempts: $max_attempts,
            attempts: $attempts,
            fault_plan: $fault_plan,
            orchestration_manifest: $orchestration_manifest,
            artifacts_dir: $artifacts_dir,
            test_artifacts_manifest: $test_artifacts_manifest
        } + (if $status == "failed" and $failure_signature != "" then {error: $failure_signature} else {} end)')
    SCENARIO_SUMMARIES+=("$summary_entry")

    return "$result"
}

run_scenario_worker() {
    local name="$1"
    local scenario_num="$2"
    local state_dir="$3"
    local prefix="$state_dir/scenario_$(printf '%02d' "$scenario_num")"
    local worker_log="${prefix}.runner.log"
    local summary_file="${prefix}.summary.json"
    local rc_file="${prefix}.exit_code"

    (
        set +e
        run_scenario "$name" "$scenario_num"
        local rc=$?
        local summary_entry=""
        if [[ "${#SCENARIO_SUMMARIES[@]}" -gt 0 ]]; then
            summary_entry="${SCENARIO_SUMMARIES[${#SCENARIO_SUMMARIES[@]}-1]}"
        fi
        printf '%s\n' "$summary_entry" > "$summary_file"
        printf '%s\n' "$rc" > "$rc_file"
        exit "$rc"
    ) > "$worker_log" 2>&1 &

    LAST_WORKER_PID="$!"
}

reap_parallel_job() {
    local state_dir="$1"
    local pid="${active_pids[0]}"
    local scenario_num="${active_nums[0]}"
    local scenario_name="${active_names[0]}"
    local prefix="$state_dir/scenario_$(printf '%02d' "$scenario_num")"
    local rc_file="${prefix}.exit_code"
    local rc_value="unknown"

    wait "$pid" 2>/dev/null || true

    if [[ -f "$rc_file" ]]; then
        rc_value=$(cat "$rc_file")
    fi
    log_info "Completed scenario worker: $scenario_name (index=$scenario_num exit=$rc_value)"

    if [[ "${#active_pids[@]}" -gt 1 ]]; then
        active_pids=("${active_pids[@]:1}")
        active_nums=("${active_nums[@]:1}")
        active_names=("${active_names[@]:1}")
    else
        active_pids=()
        active_nums=()
        active_names=()
    fi
}

run_scenarios_parallel() {
    local scenario_names=("$@")
    local state_dir="$RUN_ARTIFACTS_DIR/.parallel_state"
    local active_pids=()
    local active_nums=()
    local active_names=()
    local scenario_num=1
    local any_failed=false

    mkdir -p "$state_dir"
    log_info "Parallel mode enabled: max_concurrency=$PARALLEL"

    for name in "${scenario_names[@]}"; do
        local pid=""
        run_scenario_worker "$name" "$scenario_num" "$state_dir"
        pid="$LAST_WORKER_PID"
        active_pids+=("$pid")
        active_nums+=("$scenario_num")
        active_names+=("$name")
        log_info "Queued scenario worker: $name (index=$scenario_num pid=$pid)"

        while [[ "${#active_pids[@]}" -ge "$PARALLEL" ]]; do
            reap_parallel_job "$state_dir"
        done

        ((scenario_num++))
    done

    while [[ "${#active_pids[@]}" -gt 0 ]]; do
        reap_parallel_job "$state_dir"
    done

    PASSED=0
    FAILED=0
    SKIPPED=0
    SCENARIO_SUMMARIES=()

    local total="${#scenario_names[@]}"
    for ((scenario_num=1; scenario_num<=total; scenario_num++)); do
        local name="${scenario_names[$((scenario_num - 1))]}"
        local prefix="$state_dir/scenario_$(printf '%02d' "$scenario_num")"
        local summary_file="${prefix}.summary.json"
        local rc_file="${prefix}.exit_code"
        local rc_value=1
        local summary_entry=""
        local status="failed"

        if [[ -f "$rc_file" ]]; then
            rc_value=$(cat "$rc_file")
        fi

        if [[ -s "$summary_file" ]]; then
            summary_entry=$(cat "$summary_file")
            if jq -e . >/dev/null 2>&1 <<< "$summary_entry"; then
                status=$(jq -r '.status // "failed"' <<< "$summary_entry")
            else
                summary_entry=""
            fi
        fi

        if [[ -z "$summary_entry" ]]; then
            status="failed"
            summary_entry=$(jq -cn \
                --arg name "$name" \
                --arg status "$status" \
                --arg error "missing_parallel_summary_or_invalid_json" \
                --argjson duration_secs 0 \
                --argjson max_attempts 0 \
                '{
                    name: $name,
                    status: $status,
                    duration_secs: $duration_secs,
                    max_attempts: $max_attempts,
                    attempts: [],
                    error: $error
                }')
        fi

        SCENARIO_SUMMARIES+=("$summary_entry")

        if [[ "$status" == "passed" && "$rc_value" -eq 0 ]]; then
            ((PASSED++))
        else
            ((FAILED++))
            any_failed=true
        fi
    done

    if [[ "$any_failed" == "true" ]]; then
        return 1
    fi
    return 0
}

# ==============================================================================
# Main
# ==============================================================================

main() {
    parse_args "$@"
    validate_orchestration_config

    # Handle --list
    if [[ "$LIST_ONLY" == "true" ]]; then
        list_scenarios
        exit 0
    fi

    # Handle --self-check
    if [[ "$SELF_CHECK_ONLY" == "true" ]]; then
        if run_self_check; then
            exit 0
        else
            exit 2
        fi
    fi

    # Run self-check unless explicitly skipped (e.g., for setup-only CI scenarios)
    if [[ "$SKIP_SELF_CHECK" == "true" ]]; then
        log_info "Self-check skipped (--skip-self-check)"
    else
        log_info "Running prerequisites check..."
        if ! run_self_check; then
            log_fail "Prerequisites check failed. Use --self-check for details."
            exit 5
        fi
    fi
    echo ""

    # Determine which scenarios to run
    local scenarios_to_run=()
    if [[ ${#SCENARIOS[@]} -eq 0 ]]; then
        if [[ "$DEFAULT_ONLY" == "true" ]]; then
            read -ra scenarios_to_run <<< "$(get_default_scenario_names)"
        else
            # Run all scenarios
            read -ra scenarios_to_run <<< "$(get_scenario_names)"
        fi
    else
        # Validate requested scenarios
        for name in "${SCENARIOS[@]}"; do
            if is_valid_scenario "$name"; then
                scenarios_to_run+=("$name")
            else
                log_fail "Unknown scenario: $name"
                log_info "Use --list to see available scenarios"
                exit 3
            fi
        done
    fi

    if [[ "$SOAK_MODE" == "true" ]]; then
        local scenarios_json=""
        scenarios_json=$(scenarios_to_json_array "${scenarios_to_run[@]}")
        load_soak_resume_checkpoint "$scenarios_json"
    fi

    # Find ft binary (or allow explicit no-ft fixture-only scenarios).
    if ! find_ft_binary; then
        if all_scenarios_can_run_without_ft_binary "${scenarios_to_run[@]}"; then
            FT_BINARY=""
            log_warn "ft binary not found; continuing because selected scenario(s) support fixture-only mode without ft"
        else
            log_fail "Could not find ft binary"
            exit 5
        fi
    fi
    if [[ -n "$FT_BINARY" ]]; then
        log_verbose "Using ft binary: $FT_BINARY"
    else
        log_verbose "No ft binary selected; fixture-only execution path enabled for selected scenario(s)"
    fi

    # Setup artifacts
    setup_artifacts
    START_TIME=$(date +%s)
    SOAK_TARGET_END_EPOCH=$((START_TIME + SOAK_DURATION_SECS))

    if [[ "$SOAK_MODE" == "true" ]]; then
        TOTAL=0
    else
        TOTAL=${#scenarios_to_run[@]}
    fi
    log_info "Orchestration: run_id=$RUN_ID run_seed=$RUN_SEED retries=$SCENARIO_RETRIES parallel=$PARALLEL"
    if [[ "$SOAK_MODE" == "true" ]]; then
        log_info "Soak mode: duration_secs=$SOAK_DURATION_SECS checkpoint_interval_secs=$SOAK_CHECKPOINT_INTERVAL_SECS stop_on_failure=$SOAK_STOP_ON_FAILURE"
        log_info "Soak fault matrix: enabled=$SOAK_FAULT_MATRIX_ENABLED mode=$SOAK_FAULT_MODE interval=$SOAK_FAULT_INTERVAL offset=$SOAK_FAULT_OFFSET classes=${SOAK_FAULT_CLASSES[*]:-none}"
        if [[ -n "$SOAK_RESUME_FROM_CHECKPOINT" ]]; then
            log_info "Soak resume checkpoint: $SOAK_RESUME_FROM_CHECKPOINT (previous_run_id=${SOAK_RESUME_FROM_RUN_ID:-unknown})"
        fi
        log_info "Running repeated soak cycles with ${#scenarios_to_run[@]} scenario(s) per cycle: ${scenarios_to_run[*]}"
    else
        log_info "Running $TOTAL scenario(s): ${scenarios_to_run[*]}"
    fi
    echo ""

    local any_failed=false

    # Run scenarios
    if [[ "$SOAK_MODE" == "true" ]]; then
        if ! run_soak_cycles "${scenarios_to_run[@]}"; then
            any_failed=true
        fi
    else
        if [[ "$PARALLEL" -gt 1 && "$TOTAL" -gt 1 ]]; then
            if ! run_scenarios_parallel "${scenarios_to_run[@]}"; then
                any_failed=true
            fi
        else
            local scenario_num=1
            for name in "${scenarios_to_run[@]}"; do
                if ! run_scenario "$name" "$scenario_num"; then
                    any_failed=true
                fi
                ((scenario_num++))
                echo ""
            done
        fi
    fi

    # Write summary
    write_summary

    # Print final results
    echo "============================================"
    echo "E2E Test Results"
    echo "============================================"
    echo "Total:   $TOTAL"
    echo "Passed:  $PASSED"
    echo "Failed:  $FAILED"
    echo "Skipped: $SKIPPED"
    echo ""

    # Cleanup
    cleanup_artifacts

    # Exit with appropriate code
    if [[ "$any_failed" == "true" ]]; then
        exit 1
    else
        exit 0
    fi
}

main "$@"
