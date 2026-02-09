#!/bin/bash
# =============================================================================
# E2E: Timeline correlation (multi-pane events + correlation markers)
# Implements: wa-ugg
#
# Purpose:
#   Validate end-to-end that the timeline feature:
#   - Aggregates events across multiple panes into a unified chronological view
#   - Surfaces correlation markers for known heuristics (failover, temporal)
#   - Provides stable machine-readable JSON output for automation
#   - Renders human-readable view with correlation markers
#   - Completes within acceptable performance bounds
#
# Requirements:
#   - cargo (Rust toolchain)
#   - jq for JSON manipulation
#   - sqlite3 for direct DB seeding
# =============================================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
source "$SCRIPT_DIR/lib/e2e_artifacts.sh"

# Colors (disabled when piped)
if [[ -t 1 ]]; then
    RED='\033[0;31m'
    GREEN='\033[0;32m'
    YELLOW='\033[1;33m'
    BLUE='\033[0;34m'
    NC='\033[0m'
else
    RED=''
    GREEN=''
    YELLOW=''
    BLUE=''
    NC=''
fi

# Test counters
TESTS_RUN=0
TESTS_PASSED=0
TESTS_FAILED=0
TESTS_SKIPPED=0

# Configuration
VERBOSE=false

# ==============================================================================
# Argument parsing
# ==============================================================================

while [[ $# -gt 0 ]]; do
    case "$1" in
        --verbose|-v)
            VERBOSE=true
            shift
            ;;
        *)
            echo "Unknown option: $1" >&2
            echo "Usage: $0 [--verbose]" >&2
            exit 3
            ;;
    esac
done

# ==============================================================================
# Logging
# ==============================================================================

log_test() {
    echo -e "\n${BLUE}=== $1 ===${NC}"
}

log_pass() {
    echo -e "${GREEN}[PASS]${NC} $*"
    ((TESTS_PASSED++)) || true
    ((TESTS_RUN++)) || true
}

log_fail() {
    echo -e "${RED}[FAIL]${NC} $*"
    ((TESTS_FAILED++)) || true
    ((TESTS_RUN++)) || true
}

log_skip() {
    echo -e "${YELLOW}[SKIP]${NC} $*"
    ((TESTS_SKIPPED++)) || true
}

log_info() {
    if [[ "$VERBOSE" == "true" ]]; then
        echo -e "       $*"
    fi
}

# ==============================================================================
# Prerequisites
# ==============================================================================

check_prerequisites() {
    log_test "Prerequisites"

    if ! command -v cargo &>/dev/null; then
        echo -e "${RED}ERROR:${NC} cargo not found. Install Rust toolchain." >&2
        exit 5
    fi
    log_pass "cargo available"

    if ! command -v jq &>/dev/null; then
        echo -e "${RED}ERROR:${NC} jq not found. Install: sudo apt install jq" >&2
        exit 5
    fi
    log_pass "jq available"

    if ! command -v sqlite3 &>/dev/null; then
        echo -e "${RED}ERROR:${NC} sqlite3 not found. Install: sudo apt install sqlite3" >&2
        exit 5
    fi
    log_pass "sqlite3 available"
}

# ==============================================================================
# Binary discovery
# ==============================================================================

WA_BINARY=""

find_wa_binary() {
    if [[ -n "${WA_BINARY:-}" ]] && [[ -x "${WA_BINARY:-}" ]]; then
        return 0
    fi

    # Try release build first, then debug
    for candidate in \
        "$PROJECT_ROOT/target/release/wa" \
        "$PROJECT_ROOT/target/debug/wa"; do
        if [[ -x "$candidate" ]]; then
            WA_BINARY="$candidate"
            log_pass "wa binary: $WA_BINARY"
            return 0
        fi
    done

    # Build if not found
    log_info "Building wa binary..."
    if cargo build -p wa --quiet 2>/dev/null; then
        WA_BINARY="$PROJECT_ROOT/target/debug/wa"
        if [[ -x "$WA_BINARY" ]]; then
            log_pass "wa binary built: $WA_BINARY"
            return 0
        fi
    fi

    echo -e "${RED}ERROR:${NC} Cannot find or build wa binary" >&2
    exit 5
}

# ==============================================================================
# Workspace setup: isolated temp workspace with seeded DB
# ==============================================================================

TEMP_WORKSPACE=""
DB_PATH=""

# Timestamps: anchor all events relative to a fixed "now" for determinism
# Use a recent-enough time so --last 30m covers them
NOW_MS=""

setup_workspace() {
    TEMP_WORKSPACE="$(mktemp -d /tmp/wa-e2e-timeline-XXXXXX)"
    local wa_dir="$TEMP_WORKSPACE/.wa"
    mkdir -p "$wa_dir"
    DB_PATH="$wa_dir/wa.db"

    # Calculate a stable "now" in epoch ms
    NOW_MS=$(date +%s)000

    log_info "Workspace: $TEMP_WORKSPACE"
    log_info "DB: $DB_PATH"
    log_info "NOW_MS: $NOW_MS"

    # Initialize the DB by running wa db migrate (creates schema + migrations)
    export WA_DATA_DIR="$wa_dir"
    export WA_WORKSPACE="$TEMP_WORKSPACE"

    "$WA_BINARY" db migrate --yes > "$TEMP_WORKSPACE/db_migrate.log" 2>&1 || true
    "$WA_BINARY" db check -f json > "$TEMP_WORKSPACE/db_check.json" 2>&1 || true

    if [[ ! -f "$DB_PATH" ]]; then
        echo -e "${RED}ERROR:${NC} DB not created at $DB_PATH" >&2
        return 1
    fi

    # Seed panes: 3 panes across different agent types
    # Pane 101: codex agent
    # Pane 102: claude_code agent
    # Pane 103: codex-backup agent
    sqlite3 "$DB_PATH" <<SQL
PRAGMA foreign_keys = ON;

INSERT OR REPLACE INTO panes (
    pane_id, pane_uuid, domain, window_id, tab_id, title, cwd, tty_name,
    first_seen_at, last_seen_at, observed, ignore_reason, last_decision_at
) VALUES
    (101, 'e2e-timeline-pane-a', 'local', 1, 1, 'codex-main', '$TEMP_WORKSPACE', 'tty-a',
     $NOW_MS, $NOW_MS, 1, NULL, $NOW_MS),
    (102, 'e2e-timeline-pane-b', 'local', 1, 2, 'claude-code-main', '$TEMP_WORKSPACE', 'tty-b',
     $NOW_MS, $NOW_MS, 1, NULL, $NOW_MS),
    (103, 'e2e-timeline-pane-c', 'local', 1, 3, 'codex-backup', '$TEMP_WORKSPACE', 'tty-c',
     $NOW_MS, $NOW_MS, 1, NULL, $NOW_MS);
SQL

    log_info "Seeded 3 panes (101, 102, 103)"
}

cleanup_workspace() {
    if [[ -n "$TEMP_WORKSPACE" ]] && [[ -d "$TEMP_WORKSPACE" ]]; then
        rm -rf "$TEMP_WORKSPACE"
    fi
}

# ==============================================================================
# Scenario 1: Basic timeline aggregation
#   Seed 3 distinct events across 3 panes, verify chronological JSON output
# ==============================================================================

scenario_basic_aggregation() {
    log_test "Scenario 1: Basic timeline aggregation"

    local t1=$((NOW_MS - 600000))  # 10 min ago
    local t2=$((NOW_MS - 300000))  # 5 min ago
    local t3=$((NOW_MS - 60000))   # 1 min ago

    # Seed 3 events across 3 panes (widely spaced to avoid unintended correlations)
    sqlite3 "$DB_PATH" <<SQL
PRAGMA foreign_keys = ON;

INSERT INTO events (
    pane_id, rule_id, agent_type, event_type, severity, confidence,
    extracted, matched_text, segment_id, detected_at, handled_at,
    handled_by_workflow_id, handled_status, dedupe_key
) VALUES
    (101, 'codex.compaction.detected', 'codex', 'session.compaction', 'info', 0.9,
     NULL, 'compaction event pane A', NULL, $t1, NULL, NULL, NULL, 'e2e-basic-1'),
    (102, 'claude_code.auth.warning', 'claude_code', 'auth.token_expiry', 'warning', 0.8,
     NULL, 'auth warning pane B', NULL, $t2, NULL, NULL, NULL, 'e2e-basic-2'),
    (103, 'codex.error.detected', 'codex', 'error.runtime', 'critical', 0.95,
     NULL, 'runtime error pane C', NULL, $t3, NULL, NULL, NULL, 'e2e-basic-3');
SQL

    log_info "[TIMELINE_E2E] seeded events=3 panes=[101,102,103]"

    # Query timeline in JSON mode
    local output
    output=$("$WA_BINARY" timeline --last 30m -f json --limit 100 2>/dev/null) || {
        log_fail "S1.1: wa timeline JSON query failed"
        return 1
    }

    e2e_add_file "basic_timeline.json" "$output"

    # Assert: output is valid JSON with events array
    if ! echo "$output" | jq -e '.events' >/dev/null 2>&1; then
        log_fail "S1.1: timeline JSON missing .events array"
        return 1
    fi
    log_pass "S1.1: timeline returns valid JSON with .events"

    # Assert: all 3 events appear
    local event_count
    event_count=$(echo "$output" | jq '.events | length')
    if [[ "$event_count" -ge 3 ]]; then
        log_pass "S1.2: found $event_count events (expected >= 3)"
    else
        log_fail "S1.2: found $event_count events (expected >= 3)"
    fi

    # Assert: ordering is chronological (timestamps non-decreasing)
    local is_sorted
    is_sorted=$(echo "$output" | jq '
        [.events[].timestamp] |
        . as $ts |
        [range(1; length)] |
        all(. as $i | $ts[$i] >= $ts[$i - 1])
    ')
    if [[ "$is_sorted" == "true" ]]; then
        log_pass "S1.3: events are in chronological order"
    else
        log_fail "S1.3: events NOT in chronological order"
    fi

    # Assert: pane_ids match expected panes
    local pane_ids
    pane_ids=$(echo "$output" | jq '[.events[].pane_info.pane_id] | unique | sort')
    local has_101 has_102 has_103
    has_101=$(echo "$pane_ids" | jq 'any(. == 101)')
    has_102=$(echo "$pane_ids" | jq 'any(. == 102)')
    has_103=$(echo "$pane_ids" | jq 'any(. == 103)')

    if [[ "$has_101" == "true" && "$has_102" == "true" && "$has_103" == "true" ]]; then
        log_pass "S1.4: all 3 panes represented in timeline"
    else
        log_fail "S1.4: missing panes in timeline (ids: $pane_ids)"
    fi

    # Assert: total_count field present
    local total
    total=$(echo "$output" | jq '.total_count // 0')
    if [[ "$total" -ge 3 ]]; then
        log_pass "S1.5: total_count=$total (>= 3)"
    else
        log_fail "S1.5: total_count=$total (expected >= 3)"
    fi
}

# ==============================================================================
# Scenario 2: Failover correlation detection
#   usage_limit in pane A → session.start in pane C (within 5 min window)
# ==============================================================================

scenario_failover_correlation() {
    log_test "Scenario 2: Failover correlation detection"

    local t_limit=$((NOW_MS - 120000))   # 2 min ago: usage limit hit
    local t_session=$((NOW_MS - 60000))  # 1 min ago: new session started (within 5 min window)

    # Seed failover pair: usage_limit on pane 101, session.start on pane 103
    # Both are codex agent type so failover correlation fires
    sqlite3 "$DB_PATH" <<SQL
PRAGMA foreign_keys = ON;

INSERT INTO events (
    pane_id, rule_id, agent_type, event_type, severity, confidence,
    extracted, matched_text, segment_id, detected_at, handled_at,
    handled_by_workflow_id, handled_status, dedupe_key
) VALUES
    (101, 'codex.usage.reached', 'codex', 'usage.reached', 'warning', 0.95,
     NULL, 'Usage limit reached', NULL, $t_limit, NULL, NULL, NULL, 'e2e-failover-1'),
    (103, 'codex.session.start', 'codex', 'session.start', 'info', 0.9,
     NULL, 'New session started', NULL, $t_session, NULL, NULL, NULL, 'e2e-failover-2');
SQL

    log_info "[TIMELINE_E2E] seeded failover pair: usage_limit@101 → session.start@103"

    # Query timeline in JSON
    local output
    output=$("$WA_BINARY" timeline --last 30m -f json --limit 200 2>/dev/null) || {
        log_fail "S2.1: wa timeline JSON query failed"
        return 1
    }

    e2e_add_file "failover_timeline.json" "$output"

    # Assert: correlations array exists
    if ! echo "$output" | jq -e '.correlations' >/dev/null 2>&1; then
        log_fail "S2.1: timeline JSON missing .correlations array"
        return 1
    fi
    log_pass "S2.1: timeline has .correlations array"

    # Assert: a failover correlation exists
    local failover_count
    failover_count=$(echo "$output" | jq '[.correlations[] | select(.correlation_type == "Failover")] | length')
    if [[ "$failover_count" -ge 1 ]]; then
        log_pass "S2.2: found $failover_count failover correlation(s)"
    else
        log_fail "S2.2: no failover correlation found (expected >= 1)"
    fi

    # Assert: failover correlation references two event ids
    local failover_event_count
    failover_event_count=$(echo "$output" | jq '
        [.correlations[] | select(.correlation_type == "Failover")] |
        first | .event_ids | length
    ')
    if [[ "$failover_event_count" -eq 2 ]]; then
        log_pass "S2.3: failover correlation links 2 events"
    else
        log_fail "S2.3: failover correlation has $failover_event_count events (expected 2)"
    fi

    # Assert: confidence is present and > 0
    local confidence
    confidence=$(echo "$output" | jq '
        [.correlations[] | select(.correlation_type == "Failover")] |
        first | .confidence
    ')
    if echo "$confidence" | jq -e '. > 0' >/dev/null 2>&1; then
        log_pass "S2.4: failover confidence=$confidence (> 0)"
    else
        log_fail "S2.4: failover confidence invalid ($confidence)"
    fi

    # Assert: events themselves carry correlation refs
    local events_with_failover_refs
    events_with_failover_refs=$(echo "$output" | jq '
        [.events[] | select(.correlations[]? | .correlation_type == "Failover")] | length
    ')
    if [[ "$events_with_failover_refs" -ge 2 ]]; then
        log_pass "S2.5: $events_with_failover_refs events carry failover correlation refs"
    else
        log_fail "S2.5: only $events_with_failover_refs events carry failover refs (expected >= 2)"
    fi

    log_info "[TIMELINE_E2E] correlations type=failover count=$failover_count"
}

# ==============================================================================
# Scenario 3: Temporal clustering correlation
#   Two compaction events in different panes, close together (< 10s)
# ==============================================================================

scenario_temporal_clustering() {
    log_test "Scenario 3: Temporal clustering (compaction burst)"

    local t_comp1=$((NOW_MS - 30000))  # 30s ago
    local t_comp2=$((NOW_MS - 25000))  # 25s ago (5s apart = within 10s window)

    # Seed two compaction events on different panes, close in time
    sqlite3 "$DB_PATH" <<SQL
PRAGMA foreign_keys = ON;

INSERT INTO events (
    pane_id, rule_id, agent_type, event_type, severity, confidence,
    extracted, matched_text, segment_id, detected_at, handled_at,
    handled_by_workflow_id, handled_status, dedupe_key
) VALUES
    (101, 'codex.compaction.burst', 'codex', 'session.compaction', 'info', 0.85,
     NULL, 'Compaction burst pane A', NULL, $t_comp1, NULL, NULL, NULL, 'e2e-temporal-1'),
    (102, 'codex.compaction.burst', 'claude_code', 'session.compaction', 'info', 0.85,
     NULL, 'Compaction burst pane B', NULL, $t_comp2, NULL, NULL, NULL, 'e2e-temporal-2');
SQL

    log_info "[TIMELINE_E2E] seeded temporal pair: compaction@101 + compaction@102 (5s apart)"

    # Query timeline
    local output
    output=$("$WA_BINARY" timeline --last 30m -f json --limit 200 2>/dev/null) || {
        log_fail "S3.1: wa timeline JSON query failed"
        return 1
    }

    e2e_add_file "temporal_timeline.json" "$output"

    # Assert: a temporal correlation exists
    local temporal_count
    temporal_count=$(echo "$output" | jq '[.correlations[] | select(.correlation_type == "Temporal")] | length')
    if [[ "$temporal_count" -ge 1 ]]; then
        log_pass "S3.1: found $temporal_count temporal correlation(s)"
    else
        log_fail "S3.1: no temporal correlation found (expected >= 1)"
    fi

    # Assert: temporal correlation connects events from different panes
    local temporal_event_ids
    temporal_event_ids=$(echo "$output" | jq '
        [.correlations[] | select(.correlation_type == "Temporal")] | first | .event_ids
    ')
    local temporal_event_count
    temporal_event_count=$(echo "$temporal_event_ids" | jq 'length')
    if [[ "$temporal_event_count" -ge 2 ]]; then
        log_pass "S3.2: temporal correlation links $temporal_event_count events"
    else
        log_fail "S3.2: temporal correlation has $temporal_event_count events (expected >= 2)"
    fi

    # Also check for DedupeGroup (same rule_id across panes)
    local dedupe_count
    dedupe_count=$(echo "$output" | jq '[.correlations[] | select(.correlation_type == "DedupeGroup")] | length')
    if [[ "$dedupe_count" -ge 1 ]]; then
        log_pass "S3.3: found $dedupe_count dedupe-group correlation(s) (same rule across panes)"
    else
        # DedupeGroup is a bonus — temporal is the primary assertion
        log_info "S3.3: no dedupe-group correlation (acceptable if temporal found)"
    fi

    log_info "[TIMELINE_E2E] correlations type=temporal count=$temporal_count dedupe=$dedupe_count"
}

# ==============================================================================
# Scenario 4: Human-readable view sanity
#   Run wa timeline in plain text mode; check for correlation markers + pane labels
# ==============================================================================

scenario_human_readable() {
    log_test "Scenario 4: Human-readable view sanity"

    # Query timeline in plain text
    local output
    output=$("$WA_BINARY" timeline --last 30m -f plain --limit 200 2>/dev/null) || {
        log_fail "S4.1: wa timeline plain query failed"
        return 1
    }

    e2e_add_file "timeline_plain.txt" "$output"

    # Assert: output is non-empty
    if [[ -n "$output" ]]; then
        log_pass "S4.1: timeline plain output is non-empty"
    else
        log_fail "S4.1: timeline plain output is empty"
        return 1
    fi

    # Assert: correlation markers appear (e.g., [CORRELATED: ...])
    if echo "$output" | command grep -qi 'CORRELATED\|correlated\|correlation\|failover\|temporal'; then
        log_pass "S4.2: correlation markers present in plain output"
    else
        log_fail "S4.2: no correlation markers found in plain output"
    fi

    # Assert: pane labels appear
    if echo "$output" | command grep -q 'Pane'; then
        log_pass "S4.3: pane labels present"
    else
        log_fail "S4.3: no pane labels found in plain output"
    fi

    # Assert: timeline connector characters appear (visual structure)
    if echo "$output" | command grep -qE '─[┬┼┴]─'; then
        log_pass "S4.4: timeline connectors present (visual structure)"
    else
        log_fail "S4.4: no timeline connectors found"
    fi
}

# ==============================================================================
# Scenario 5: Performance guardrail
#   Seed 1000 events, query timeline, assert < generous time bound
# ==============================================================================

scenario_performance_guardrail() {
    log_test "Scenario 5: Performance guardrail (1k events)"

    # Seed 1000 events spread across 3 panes over 30 minutes
    local batch_sql=""
    local base_ts=$((NOW_MS - 1800000))  # 30 min ago

    for i in $(seq 1 1000); do
        local pane_id=$(( (i % 3) + 101 ))  # rotate 101, 102, 103
        local ts=$((base_ts + (i * 1800)))    # ~1.8s apart
        local sev="info"
        if (( i % 10 == 0 )); then sev="warning"; fi
        if (( i % 50 == 0 )); then sev="critical"; fi

        batch_sql+="INSERT INTO events (
            pane_id, rule_id, agent_type, event_type, severity, confidence,
            extracted, matched_text, segment_id, detected_at, handled_at,
            handled_by_workflow_id, handled_status, dedupe_key
        ) VALUES (
            $pane_id, 'e2e.perf.event.$i', 'codex', 'perf.test', '$sev', 0.5,
            NULL, 'perf event $i', NULL, $ts, NULL, NULL, NULL, 'e2e-perf-$i'
        );"$'\n'
    done

    sqlite3 "$DB_PATH" <<SQL
PRAGMA foreign_keys = ON;
$batch_sql
SQL

    log_info "[TIMELINE_E2E] seeded 1000 perf events"

    # Time the query
    local start_time end_time duration_ms
    start_time=$(date +%s%N)

    local output
    output=$("$WA_BINARY" timeline --last 60m -f json --limit 1100 2>/dev/null) || {
        log_fail "S5.1: wa timeline query with 1k events failed"
        return 1
    }

    end_time=$(date +%s%N)
    duration_ms=$(( (end_time - start_time) / 1000000 ))

    e2e_add_file "perf_timeline.json" "$output"
    e2e_add_file "query_timing.log" "query_ms=$duration_ms events_queried=1000"

    log_info "[TIMELINE_E2E] query_ms=$duration_ms"

    # Assert: query completed (we got here)
    log_pass "S5.1: 1k-event timeline query completed"

    # Assert: reasonable time bound (< 10s generous for CI)
    if [[ "$duration_ms" -lt 10000 ]]; then
        log_pass "S5.2: query completed in ${duration_ms}ms (< 10s budget)"
    else
        log_fail "S5.2: query took ${duration_ms}ms (exceeded 10s budget)"
    fi

    # Assert: events actually returned
    local event_count
    event_count=$(echo "$output" | jq '.events | length')
    if [[ "$event_count" -ge 100 ]]; then
        log_pass "S5.3: returned $event_count events (>= 100)"
    else
        log_fail "S5.3: returned only $event_count events (expected >= 100)"
    fi

    # Assert: correlations computed (temporal clusters should exist among 1k events)
    local corr_count
    corr_count=$(echo "$output" | jq '.correlations | length')
    if [[ "$corr_count" -ge 1 ]]; then
        log_pass "S5.4: $corr_count correlations found in 1k-event set"
    else
        log_info "S5.4: no correlations in perf set (acceptable)"
    fi
}

# ==============================================================================
# Scenario 6: JSON schema stability
#   Verify top-level fields and event structure are present
# ==============================================================================

scenario_json_schema() {
    log_test "Scenario 6: JSON schema stability"

    local output
    output=$("$WA_BINARY" timeline --last 30m -f json --limit 50 2>/dev/null) || {
        log_fail "S6.1: wa timeline JSON query failed"
        return 1
    }

    # Top-level fields
    for field in start end events correlations total_count has_more; do
        if echo "$output" | jq -e "has(\"$field\")" >/dev/null 2>&1; then
            log_pass "S6: top-level field '$field' present"
        else
            log_fail "S6: top-level field '$field' MISSING"
        fi
    done

    # Event fields (check first event)
    local first_event
    first_event=$(echo "$output" | jq '.events[0]')
    if [[ "$first_event" == "null" ]]; then
        log_skip "S6: no events to validate schema against"
        return 0
    fi

    for field in id timestamp pane_info rule_id event_type severity confidence correlations; do
        if echo "$first_event" | jq -e "has(\"$field\")" >/dev/null 2>&1; then
            log_pass "S6: event field '$field' present"
        else
            log_fail "S6: event field '$field' MISSING"
        fi
    done

    # PaneInfo sub-fields
    for field in pane_id domain; do
        if echo "$first_event" | jq -e ".pane_info | has(\"$field\")" >/dev/null 2>&1; then
            log_pass "S6: pane_info field '$field' present"
        else
            log_fail "S6: pane_info field '$field' MISSING"
        fi
    done

    # Correlation fields (if any exist)
    local corr_count
    corr_count=$(echo "$output" | jq '.correlations | length')
    if [[ "$corr_count" -gt 0 ]]; then
        local first_corr
        first_corr=$(echo "$output" | jq '.correlations[0]')
        for field in id event_ids correlation_type confidence description; do
            if echo "$first_corr" | jq -e "has(\"$field\")" >/dev/null 2>&1; then
                log_pass "S6: correlation field '$field' present"
            else
                log_fail "S6: correlation field '$field' MISSING"
            fi
        done
    fi
}

# ==============================================================================
# Main
# ==============================================================================

main() {
    echo -e "${BLUE}================================================${NC}"
    echo -e "${BLUE}E2E: Timeline Correlation${NC}"
    echo -e "${BLUE}Bead: wa-ugg${NC}"
    echo -e "${BLUE}================================================${NC}"

    check_prerequisites
    find_wa_binary

    e2e_init_artifacts "timeline-correlation" >/dev/null

    setup_workspace
    e2e_add_file "workspace.txt" "$TEMP_WORKSPACE"
    e2e_add_file "db_path.txt" "$DB_PATH"

    trap cleanup_workspace EXIT

    local overall_exit=0

    # Run all scenarios, collecting results
    e2e_capture_scenario "basic_aggregation" scenario_basic_aggregation || overall_exit=1
    e2e_capture_scenario "failover_correlation" scenario_failover_correlation || overall_exit=1
    e2e_capture_scenario "temporal_clustering" scenario_temporal_clustering || overall_exit=1
    e2e_capture_scenario "human_readable" scenario_human_readable || overall_exit=1
    e2e_capture_scenario "performance_guardrail" scenario_performance_guardrail || overall_exit=1
    e2e_capture_scenario "json_schema" scenario_json_schema || overall_exit=1

    # Copy DB artifacts before cleanup
    if [[ -f "$DB_PATH" ]]; then
        local events_jsonl
        events_jsonl=$(sqlite3 "$DB_PATH" "SELECT json_object(
            'id', id, 'pane_id', pane_id, 'rule_id', rule_id,
            'event_type', event_type, 'severity', severity,
            'detected_at', detected_at
        ) FROM events ORDER BY detected_at;" 2>/dev/null || echo "")
        e2e_add_file "events.jsonl" "$events_jsonl"
    fi

    e2e_finalize "$overall_exit" >/dev/null

    # Summary
    echo -e "\n${BLUE}================================================${NC}"
    echo -e "Results: ${GREEN}${TESTS_PASSED} passed${NC}, ${RED}${TESTS_FAILED} failed${NC}, ${YELLOW}${TESTS_SKIPPED} skipped${NC} (${TESTS_RUN} total)"
    echo -e "${BLUE}================================================${NC}"

    if [[ $TESTS_FAILED -gt 0 ]]; then
        echo -e "${RED}OVERALL: FAIL${NC}"
        return 1
    fi

    echo -e "${GREEN}OVERALL: PASS${NC}"
    return 0
}

main "$@"
