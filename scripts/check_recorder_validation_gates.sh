#!/usr/bin/env bash
# =============================================================================
# CI/Nightly recorder validation gates (wa-oegrb.7.5)
#
# Runs explicit validation harnesses for:
#   - chaos/failure matrix
#   - recovery drills
#   - correctness invariants
#   - semantic/hybrid quality
#   - load harness (compile-only in CI, optional run in nightly)
#
# Artifacts are written under target/recorder-validation-gates/.
# =============================================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_ROOT"

ARTIFACT_DIR="${FT_RECORDER_VALIDATION_ARTIFACT_DIR:-target/recorder-validation-gates}"
TARGET_DIR="${FT_RECORDER_VALIDATION_TARGET_DIR:-target-recorder-validation-gates}"
RUN_LOAD_BENCH="${FT_RECORDER_GATE_RUN_LOAD_BENCH:-0}"

mkdir -p "$ARTIFACT_DIR"

# Explicit gate thresholds
MIN_CHAOS_SUMMARY_ARTIFACTS=1
MIN_RECOVERY_ARTIFACTS=3
MIN_CORRECTNESS_TESTS=10

PASS=0
FAIL=0

status_chaos_matrix="not_run"
status_recovery_drills="not_run"
status_correctness_invariants="not_run"
status_semantic_quality="not_run"
status_hybrid_fusion="not_run"
status_load_harness_compile="not_run"
status_load_harness_run="skipped"

run_step() {
    local step_name="$1"
    local status_var="$2"
    shift 2

    local log_file="$ARTIFACT_DIR/${step_name}.log"
    echo "[recorder-gates] === ${step_name} ==="
    echo "[recorder-gates] cmd: $*" > "$log_file"

    if "$@" 2>&1 | tee -a "$log_file"; then
        printf -v "$status_var" '%s' "pass"
        PASS=$((PASS + 1))
    else
        printf -v "$status_var" '%s' "fail"
        FAIL=$((FAIL + 1))
    fi
    echo ""
}

echo "[recorder-gates] Artifacts: $ARTIFACT_DIR"
echo "[recorder-gates] CARGO_TARGET_DIR: $TARGET_DIR"
echo "[recorder-gates] RUN_LOAD_BENCH: $RUN_LOAD_BENCH"
echo ""

run_step \
    "chaos_matrix" \
    status_chaos_matrix \
    env CARGO_TARGET_DIR="$TARGET_DIR" \
    cargo test -p frankenterm-core \
    --test recorder_tantivy_integration \
    chaos_failure_matrix_detects_faults_and_recovers_without_silent_loss \
    -- --nocapture

run_step \
    "recovery_drills" \
    status_recovery_drills \
    env CARGO_TARGET_DIR="$TARGET_DIR" \
    cargo test -p frankenterm-core \
    --test recorder_recovery_drills \
    -- --nocapture

run_step \
    "correctness_invariants" \
    status_correctness_invariants \
    env CARGO_TARGET_DIR="$TARGET_DIR" \
    cargo test -p frankenterm-core \
    --test recorder_correctness_integration \
    -- --nocapture

run_step \
    "semantic_quality" \
    status_semantic_quality \
    env CARGO_TARGET_DIR="$TARGET_DIR" \
    cargo test -p frankenterm-core \
    --test semantic_quality_harness_tests \
    -- --nocapture

run_step \
    "hybrid_fusion" \
    status_hybrid_fusion \
    env CARGO_TARGET_DIR="$TARGET_DIR" \
    cargo test -p frankenterm-core \
    --test hybrid_fusion_tests \
    -- --nocapture

run_step \
    "load_harness_compile" \
    status_load_harness_compile \
    env CARGO_TARGET_DIR="$TARGET_DIR" \
    cargo bench -p frankenterm-core \
    --bench storage_regression \
    --no-run

if [[ "$RUN_LOAD_BENCH" == "1" ]]; then
    run_step \
        "load_harness_run" \
        status_load_harness_run \
        env CARGO_TARGET_DIR="$TARGET_DIR" \
        cargo bench -p frankenterm-core \
        --bench storage_regression \
        -- recorder_swarm_load_profile --sample-size 10 --measurement-time 2
fi

# Post-command threshold checks
CHAOS_SUMMARY_ARTIFACTS=$(grep -c '\[ARTIFACT\]\[recorder-chaos\] matrix_summary=' "$ARTIFACT_DIR/chaos_matrix.log" || true)
RECOVERY_ARTIFACTS=$(grep -c '\[ARTIFACT\]\[recorder-recovery-drill\]' "$ARTIFACT_DIR/recovery_drills.log" || true)
CORRECTNESS_TESTS=$(sed -n 's/.*test result: ok\. \([0-9][0-9]*\) passed.*/\1/p' "$ARTIFACT_DIR/correctness_invariants.log" | tail -n 1)
if [[ -z "$CORRECTNESS_TESTS" ]]; then
    CORRECTNESS_TESTS=0
fi

if (( CHAOS_SUMMARY_ARTIFACTS < MIN_CHAOS_SUMMARY_ARTIFACTS )); then
    echo "[recorder-gates] FAIL: chaos summary artifacts $CHAOS_SUMMARY_ARTIFACTS < $MIN_CHAOS_SUMMARY_ARTIFACTS"
    FAIL=$((FAIL + 1))
fi

if (( RECOVERY_ARTIFACTS < MIN_RECOVERY_ARTIFACTS )); then
    echo "[recorder-gates] FAIL: recovery artifacts $RECOVERY_ARTIFACTS < $MIN_RECOVERY_ARTIFACTS"
    FAIL=$((FAIL + 1))
fi

if (( CORRECTNESS_TESTS < MIN_CORRECTNESS_TESTS )); then
    echo "[recorder-gates] FAIL: correctness tests $CORRECTNESS_TESTS < $MIN_CORRECTNESS_TESTS"
    FAIL=$((FAIL + 1))
fi

REPORT_FILE="$ARTIFACT_DIR/recorder-validation-report.json"
cat > "$REPORT_FILE" <<EOF
{
  "version": "1",
  "format": "recorder-validation-gates",
  "generated_at": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "run_load_bench": $([[ "$RUN_LOAD_BENCH" == "1" ]] && echo true || echo false),
  "thresholds": {
    "min_chaos_summary_artifacts": $MIN_CHAOS_SUMMARY_ARTIFACTS,
    "min_recovery_artifacts": $MIN_RECOVERY_ARTIFACTS,
    "min_correctness_tests": $MIN_CORRECTNESS_TESTS
  },
  "observed": {
    "chaos_summary_artifacts": $CHAOS_SUMMARY_ARTIFACTS,
    "recovery_artifacts": $RECOVERY_ARTIFACTS,
    "correctness_tests": $CORRECTNESS_TESTS
  },
  "steps": [
    {"name":"chaos_matrix","status":"$status_chaos_matrix"},
    {"name":"recovery_drills","status":"$status_recovery_drills"},
    {"name":"correctness_invariants","status":"$status_correctness_invariants"},
    {"name":"semantic_quality","status":"$status_semantic_quality"},
    {"name":"hybrid_fusion","status":"$status_hybrid_fusion"},
    {"name":"load_harness_compile","status":"$status_load_harness_compile"},
    {"name":"load_harness_run","status":"$status_load_harness_run"}
  ],
  "summary": {
    "passed_steps": $PASS,
    "failed_steps_or_thresholds": $FAIL
  }
}
EOF

echo ""
echo "[recorder-gates] ========================================"
echo "[recorder-gates] pass steps: $PASS"
echo "[recorder-gates] failures (steps + threshold checks): $FAIL"
echo "[recorder-gates] report: $REPORT_FILE"

if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
    {
        echo "## Recorder Validation Gates"
        echo ""
        echo "| Gate | Status |"
        echo "|------|--------|"
        echo "| chaos_matrix | $status_chaos_matrix |"
        echo "| recovery_drills | $status_recovery_drills |"
        echo "| correctness_invariants | $status_correctness_invariants |"
        echo "| semantic_quality | $status_semantic_quality |"
        echo "| hybrid_fusion | $status_hybrid_fusion |"
        echo "| load_harness_compile | $status_load_harness_compile |"
        echo "| load_harness_run | $status_load_harness_run |"
        echo ""
        echo "Thresholds:"
        echo "- chaos summary artifacts: $CHAOS_SUMMARY_ARTIFACTS / $MIN_CHAOS_SUMMARY_ARTIFACTS"
        echo "- recovery artifacts: $RECOVERY_ARTIFACTS / $MIN_RECOVERY_ARTIFACTS"
        echo "- correctness tests: $CORRECTNESS_TESTS / $MIN_CORRECTNESS_TESTS"
        echo ""
        echo "Artifacts: \`$ARTIFACT_DIR\`"
        echo "Report: \`$REPORT_FILE\`"
    } >> "$GITHUB_STEP_SUMMARY"
fi

if (( FAIL > 0 )); then
    echo "[recorder-gates] FAILED"
    exit 1
fi

echo "[recorder-gates] PASSED"
exit 0
