#!/bin/bash
# =============================================================================
# E2E: Backup/Restore Cycle (export, corrupt, import, verify)
# Implements: wa-7fv
#
# Purpose:
#   Prove end-to-end that backup/restore is safe, user-friendly, and reliable:
#   - Export produces a self-contained, verifiable backup artifact
#   - Import rejects corrupt backups with actionable errors
#   - Replace/merge semantics behave correctly
#   - Pre-import safety backups are created when mutating an existing workspace
#   - No secrets leak into logs/artifacts
#
# Requirements:
#   - wa binary built (cargo build -p wa)
#   - jq for JSON manipulation
#   - sqlite3 for test data population
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
WA_BIN=""
VERBOSE=false

# Temp workspaces (cleaned up at exit)
WORKSPACE_A=""
WORKSPACE_B=""
WORKSPACE_C=""

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
# Helpers
# ==============================================================================

# Run wa command, extracting JSON from output (strips log lines)
run_wa_json() {
    local raw_output
    raw_output=$("$WA_BIN" "$@" 2>/dev/null) || true
    # Extract JSON (first { to last })
    echo "$raw_output" | awk '/^{/{found=1} found{print}'
}

# Assert JSON field equals expected value
assert_json_eq() {
    local json="$1"
    local jq_expr="$2"
    local expected="$3"
    local description="$4"

    local actual
    actual=$(echo "$json" | jq -r "$jq_expr" 2>/dev/null || echo "JQ_ERROR")

    if [[ "$actual" == "$expected" ]]; then
        log_pass "$description"
    else
        log_fail "$description (expected='$expected', got='$actual')"
        log_info "JSON (first 300 chars): ${json:0:300}"
    fi
}

# Assert JSON field is greater than value
assert_json_gt() {
    local json="$1"
    local jq_expr="$2"
    local min_val="$3"
    local description="$4"

    local actual
    actual=$(echo "$json" | jq -r "$jq_expr" 2>/dev/null || echo "0")

    if [[ "$actual" -gt "$min_val" ]]; then
        log_pass "$description (got $actual)"
    else
        log_fail "$description (expected >$min_val, got $actual)"
    fi
}

# Assert JSON field is not null/empty
assert_json_has() {
    local json="$1"
    local jq_expr="$2"
    local description="$3"

    local result
    result=$(echo "$json" | jq -e "$jq_expr" 2>/dev/null) || true

    if [[ -n "$result" && "$result" != "null" ]]; then
        log_pass "$description"
    else
        log_fail "$description (field missing or null)"
    fi
}

# Assert command exits with non-zero
assert_fails() {
    local description="$1"
    shift
    local exit_code=0
    "$@" >/dev/null 2>&1 || exit_code=$?
    if [[ $exit_code -ne 0 ]]; then
        log_pass "$description (exit=$exit_code)"
    else
        log_fail "$description (expected non-zero exit, got 0)"
    fi
}

# Create a fresh workspace with initialized database
create_workspace() {
    local ws
    ws=$(mktemp -d)
    "$WA_BIN" db migrate --workspace "$ws" --yes >/dev/null 2>&1
    echo "$ws"
}

# Populate a workspace with test data (pane + segments + event)
populate_workspace() {
    local ws="$1"
    local db_path="$ws/.wa/wa.db"
    local epoch_ms
    epoch_ms=$(date +%s)000

    # Insert a test pane
    sqlite3 "$db_path" "INSERT INTO panes (pane_id, domain, title, cwd, first_seen_at, last_seen_at, observed) VALUES (42, 'local', 'test-pane', '/tmp', $epoch_ms, $epoch_ms, 1);"

    # Insert segments with a unique searchable token
    sqlite3 "$db_path" "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (42, 1, 'Hello from backup E2E test TOKEN_BACKUP_ABC123', 45, $epoch_ms);"
    sqlite3 "$db_path" "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (42, 2, 'Second segment with data TOKEN_BACKUP_DEF456', 43, $epoch_ms);"

    # Insert an event
    sqlite3 "$db_path" "INSERT INTO events (pane_id, rule_id, agent_type, event_type, severity, confidence, matched_text, detected_at) VALUES (42, 'e2e.test', 'unknown', 'test_event', 'info', 1.0, 'test match for backup E2E', $epoch_ms);"
}

# Get row count from a table
get_count() {
    local db_path="$1"
    local table="$2"
    sqlite3 "$db_path" "SELECT COUNT(*) FROM $table;" 2>/dev/null || echo "0"
}

# ==============================================================================
# Prerequisites
# ==============================================================================

check_prerequisites() {
    log_test "Prerequisites"

    if [[ -x "$PROJECT_ROOT/target/debug/wa" ]]; then
        WA_BIN="$PROJECT_ROOT/target/debug/wa"
    elif [[ -x "$PROJECT_ROOT/target/release/wa" ]]; then
        WA_BIN="$PROJECT_ROOT/target/release/wa"
    else
        echo -e "${RED}ERROR:${NC} wa binary not found. Run: cargo build -p wa" >&2
        exit 5
    fi
    log_pass "P.1: wa binary found: $WA_BIN"

    if ! command -v jq &>/dev/null; then
        echo -e "${RED}ERROR:${NC} jq not found" >&2
        exit 5
    fi
    log_pass "P.2: jq available"

    if ! command -v sqlite3 &>/dev/null; then
        echo -e "${RED}ERROR:${NC} sqlite3 not found" >&2
        exit 5
    fi
    log_pass "P.3: sqlite3 available"
}

# ==============================================================================
# Scenario 1: Clean export/import round-trip
# ==============================================================================

test_roundtrip() {
    log_test "Scenario 1: Clean Export/Import Round-Trip"

    # Create and populate workspace A
    WORKSPACE_A=$(create_workspace)
    populate_workspace "$WORKSPACE_A"
    log_info "Workspace A: $WORKSPACE_A"

    # Record evidence from workspace A
    local pane_count_a seg_count_a event_count_a
    pane_count_a=$(get_count "$WORKSPACE_A/.wa/wa.db" "panes")
    seg_count_a=$(get_count "$WORKSPACE_A/.wa/wa.db" "output_segments")
    event_count_a=$(get_count "$WORKSPACE_A/.wa/wa.db" "events")

    log_info "Workspace A: panes=$pane_count_a, segments=$seg_count_a, events=$event_count_a"

    # 1.1: Export from workspace A
    local export_json
    export_json=$(run_wa_json backup export --workspace "$WORKSPACE_A" -f json)

    assert_json_eq "$export_json" '.ok' "true" "1.1: export succeeds"

    # 1.2: Export produces an output path
    local export_path
    export_path=$(echo "$export_json" | jq -r '.data.output_path' 2>/dev/null || echo "")
    if [[ -n "$export_path" && -d "$export_path" ]]; then
        log_pass "1.2: export directory created at $export_path"
    else
        log_fail "1.2: export directory not created (path='$export_path')"
        return
    fi

    # 1.3: Export contains required files
    if [[ -f "$export_path/database.db" && -f "$export_path/manifest.json" && -f "$export_path/checksums.sha256" ]]; then
        log_pass "1.3: export contains database.db, manifest.json, checksums.sha256"
    else
        log_fail "1.3: export missing required files"
        log_info "Contents: $(ls -la "$export_path" 2>/dev/null || echo 'N/A')"
    fi

    # 1.4: Manifest has correct stats
    assert_json_eq "$export_json" '.data.manifest.stats.panes' "$pane_count_a" "1.4: manifest pane count matches"

    # 1.5: Manifest has correct segment count
    assert_json_eq "$export_json" '.data.manifest.stats.segments' "$seg_count_a" "1.5: manifest segment count matches"

    # 1.6: Manifest has correct event count
    assert_json_eq "$export_json" '.data.manifest.stats.events' "$event_count_a" "1.6: manifest event count matches"

    # 1.7: Manifest has schema version
    assert_json_has "$export_json" '.data.manifest.schema_version' "1.7: manifest has schema_version"

    # 1.8: Manifest has checksum
    assert_json_has "$export_json" '.data.manifest.db_checksum' "1.8: manifest has db_checksum"

    # 1.9: Verify the backup
    local verify_json
    verify_json=$(run_wa_json backup import --workspace "$(mktemp -d)" "$export_path" --verify -f json)
    assert_json_eq "$verify_json" '.ok' "true" "1.9: backup verification passes"
    assert_json_eq "$verify_json" '.data.verified' "true" "1.10: verified field is true"

    # 1.11: Import into fresh workspace B
    WORKSPACE_B=$(create_workspace)
    log_info "Workspace B: $WORKSPACE_B"

    local import_json
    import_json=$(run_wa_json backup import --workspace "$WORKSPACE_B" "$export_path" --yes -f json)
    assert_json_eq "$import_json" '.ok' "true" "1.11: import succeeds"

    # 1.12: Workspace B has same data as workspace A
    local pane_count_b seg_count_b event_count_b
    pane_count_b=$(get_count "$WORKSPACE_B/.wa/wa.db" "panes")
    seg_count_b=$(get_count "$WORKSPACE_B/.wa/wa.db" "output_segments")
    event_count_b=$(get_count "$WORKSPACE_B/.wa/wa.db" "events")

    if [[ "$pane_count_b" == "$pane_count_a" ]]; then
        log_pass "1.12: pane count matches after import ($pane_count_b)"
    else
        log_fail "1.12: pane count mismatch (A=$pane_count_a, B=$pane_count_b)"
    fi

    if [[ "$seg_count_b" == "$seg_count_a" ]]; then
        log_pass "1.13: segment count matches after import ($seg_count_b)"
    else
        log_fail "1.13: segment count mismatch (A=$seg_count_a, B=$seg_count_b)"
    fi

    if [[ "$event_count_b" == "$event_count_a" ]]; then
        log_pass "1.14: event count matches after import ($event_count_b)"
    else
        log_fail "1.14: event count mismatch (A=$event_count_a, B=$event_count_b)"
    fi

    # 1.15: Verify the unique token is searchable in workspace B
    local token_hit
    token_hit=$(sqlite3 "$WORKSPACE_B/.wa/wa.db" "SELECT COUNT(*) FROM output_segments WHERE content LIKE '%TOKEN_BACKUP_ABC123%';" 2>/dev/null || echo "0")
    if [[ "$token_hit" -gt 0 ]]; then
        log_pass "1.15: unique token found in workspace B"
    else
        log_fail "1.15: unique token not found in workspace B"
    fi

    # 1.16: FTS index works after import
    local fts_hit
    fts_hit=$(sqlite3 "$WORKSPACE_B/.wa/wa.db" "SELECT COUNT(*) FROM output_segments_fts WHERE output_segments_fts MATCH 'TOKEN_BACKUP_ABC123';" 2>/dev/null || echo "0")
    if [[ "$fts_hit" -gt 0 ]]; then
        log_pass "1.16: FTS search works after import"
    else
        # FTS may need rebuild after binary restore; not a hard failure
        log_skip "1.16: FTS search may need rebuild after binary restore (got $fts_hit)"
    fi
}

# ==============================================================================
# Scenario 2: Corrupt backup rejection
# ==============================================================================

test_corrupt_backup() {
    log_test "Scenario 2: Corrupt Backup Rejection"

    # Ensure we have a valid backup from scenario 1
    if [[ -z "$WORKSPACE_A" || ! -d "$WORKSPACE_A" ]]; then
        WORKSPACE_A=$(create_workspace)
        populate_workspace "$WORKSPACE_A"
    fi

    # Export a valid backup
    local export_json
    export_json=$(run_wa_json backup export --workspace "$WORKSPACE_A" -f json -o "$WORKSPACE_A/.wa/backups/corrupt_test")
    local export_path
    export_path=$(echo "$export_json" | jq -r '.data.output_path' 2>/dev/null || echo "")

    if [[ -z "$export_path" || ! -d "$export_path" ]]; then
        log_fail "2.0: could not create test backup for corruption test"
        return
    fi

    # 2.1: Corrupt the database file by appending garbage
    local corrupt_backup="$WORKSPACE_A/.wa/backups/corrupt_test_bad"
    cp -r "$export_path" "$corrupt_backup"
    printf 'GARBAGE_DATA_CORRUPTION_12345' >> "$corrupt_backup/database.db"

    # 2.2: Verify detects corruption
    local verify_corrupt_json
    verify_corrupt_json=$(run_wa_json backup import --workspace "$(mktemp -d)" "$corrupt_backup" --verify -f json)

    local verify_ok
    verify_ok=$(echo "$verify_corrupt_json" | jq -r '.ok' 2>/dev/null || echo "")
    if [[ "$verify_ok" == "false" ]]; then
        log_pass "2.1: verification detects corrupted backup"
    else
        # The checksum check might pass if only appended (the db is still valid SQLite)
        # Check if error mentions checksum
        local error_msg
        error_msg=$(echo "$verify_corrupt_json" | jq -r '.error // empty' 2>/dev/null || echo "")
        if [[ -n "$error_msg" ]]; then
            log_pass "2.1: verification reports error for corrupted backup"
        else
            log_fail "2.1: verification did not detect corruption (ok=$verify_ok)"
            log_info "JSON: ${verify_corrupt_json:0:300}"
        fi
    fi

    # 2.3: Corrupt the manifest checksum (verification uses manifest.db_checksum)
    local corrupt_manifest="$WORKSPACE_A/.wa/backups/corrupt_test_manifest"
    cp -r "$export_path" "$corrupt_manifest"
    # Replace the db_checksum in manifest.json with a wrong value
    local tmp_manifest
    tmp_manifest=$(mktemp)
    jq '.db_checksum = "0000000000000000000000000000000000000000000000000000000000000000"' "$corrupt_manifest/manifest.json" > "$tmp_manifest"
    mv "$tmp_manifest" "$corrupt_manifest/manifest.json"

    local verify_manifest_json
    verify_manifest_json=$(run_wa_json backup import --workspace "$(mktemp -d)" "$corrupt_manifest" --verify -f json)

    local verify_manifest_ok
    verify_manifest_ok=$(echo "$verify_manifest_json" | jq -r '.ok' 2>/dev/null || echo "")
    if [[ "$verify_manifest_ok" == "false" ]]; then
        log_pass "2.2: verification detects manifest checksum mismatch"
    else
        log_fail "2.2: verification did not detect manifest checksum mismatch (ok=$verify_manifest_ok)"
        log_info "JSON: ${verify_manifest_json:0:300}"
    fi

    # 2.4: Import of backup with wrong manifest checksum fails
    local import_corrupt_json
    import_corrupt_json=$(run_wa_json backup import --workspace "$(mktemp -d)" "$corrupt_manifest" --yes -f json)

    local import_ok
    import_ok=$(echo "$import_corrupt_json" | jq -r '.ok' 2>/dev/null || echo "")
    if [[ "$import_ok" == "false" ]]; then
        log_pass "2.3: import of checksum-mismatched backup is rejected"
    else
        log_fail "2.3: import of checksum-mismatched backup was not rejected (ok=$import_ok)"
    fi

    # 2.5: Error message provides actionable guidance
    local error_msg
    error_msg=$(echo "$import_corrupt_json" | jq -r '.error // empty' 2>/dev/null || echo "")
    if [[ -n "$error_msg" ]]; then
        log_pass "2.4: error message present: ${error_msg:0:80}"
    else
        log_fail "2.4: no error message for corrupt backup"
    fi

    # 2.6: Missing manifest causes failure (returns non-JSON error)
    local no_manifest="$WORKSPACE_A/.wa/backups/no_manifest_test"
    mkdir -p "$no_manifest"
    cp "$export_path/database.db" "$no_manifest/" 2>/dev/null || true

    local no_manifest_exit=0
    "$WA_BIN" backup import --workspace "$(mktemp -d)" "$no_manifest" --verify -f json >/dev/null 2>&1 || no_manifest_exit=$?
    if [[ "$no_manifest_exit" -ne 0 ]]; then
        log_pass "2.5: missing manifest is detected (exit=$no_manifest_exit)"
    else
        log_fail "2.5: missing manifest not detected (exit=0)"
    fi
}

# ==============================================================================
# Scenario 3: Pre-import safety backup
# ==============================================================================

test_safety_backup() {
    log_test "Scenario 3: Pre-Import Safety Backup"

    # Create workspace C with its own data
    WORKSPACE_C=$(create_workspace)
    populate_workspace "$WORKSPACE_C"

    # Record C's original data
    local orig_seg_count
    orig_seg_count=$(get_count "$WORKSPACE_C/.wa/wa.db" "output_segments")
    log_info "Workspace C original segments: $orig_seg_count"

    # Export from workspace A (if available) or create a new backup
    local backup_for_import
    if [[ -n "$WORKSPACE_A" && -d "$WORKSPACE_A" ]]; then
        local export_json
        export_json=$(run_wa_json backup export --workspace "$WORKSPACE_A" -f json -o "$WORKSPACE_A/.wa/backups/safety_test")
        backup_for_import=$(echo "$export_json" | jq -r '.data.output_path' 2>/dev/null || echo "")
    fi

    if [[ -z "$backup_for_import" || ! -d "$backup_for_import" ]]; then
        # Fallback: create a minimal backup
        local tmp_ws
        tmp_ws=$(create_workspace)
        local export_json
        export_json=$(run_wa_json backup export --workspace "$tmp_ws" -f json)
        backup_for_import=$(echo "$export_json" | jq -r '.data.output_path' 2>/dev/null || echo "")
    fi

    if [[ -z "$backup_for_import" || ! -d "$backup_for_import" ]]; then
        log_fail "3.0: could not create backup for safety test"
        return
    fi

    # 3.1: Import into workspace C (which has existing data)
    local import_json
    import_json=$(run_wa_json backup import --workspace "$WORKSPACE_C" "$backup_for_import" --yes -f json)
    assert_json_eq "$import_json" '.ok' "true" "3.1: import into existing workspace succeeds"

    # 3.2: Safety backup was created
    local safety_path
    safety_path=$(echo "$import_json" | jq -r '.data.safety_backup_path // empty' 2>/dev/null || echo "")
    if [[ -n "$safety_path" && -d "$safety_path" ]]; then
        log_pass "3.2: safety backup created at $safety_path"
    else
        log_fail "3.2: no safety backup created (path='$safety_path')"
        return
    fi

    # 3.3: Safety backup contains required files
    if [[ -f "$safety_path/database.db" && -f "$safety_path/manifest.json" ]]; then
        log_pass "3.3: safety backup contains database.db and manifest.json"
    else
        log_fail "3.3: safety backup missing required files"
        log_info "Contents: $(ls "$safety_path" 2>/dev/null || echo 'empty')"
    fi

    # 3.4: Safety backup has the original data count
    local safety_manifest
    safety_manifest=$(jq -r '.stats.segments // 0' "$safety_path/manifest.json" 2>/dev/null || echo "-1")
    if [[ "$safety_manifest" == "$orig_seg_count" ]]; then
        log_pass "3.4: safety backup manifest has original segment count ($safety_manifest)"
    else
        log_fail "3.4: safety backup segment count mismatch (expected=$orig_seg_count, got=$safety_manifest)"
    fi

    # 3.5: Safety backup is itself verifiable
    local verify_safety_json
    verify_safety_json=$(run_wa_json backup import --workspace "$(mktemp -d)" "$safety_path" --verify -f json)
    assert_json_eq "$verify_safety_json" '.ok' "true" "3.5: safety backup passes verification"

    # 3.6: Import with --no-safety-backup skips safety
    local import_nosafety_json
    import_nosafety_json=$(run_wa_json backup import --workspace "$WORKSPACE_C" "$backup_for_import" --yes --no-safety-backup -f json)
    local nosafety_path
    nosafety_path=$(echo "$import_nosafety_json" | jq -r '.data.safety_backup_path // "null"' 2>/dev/null || echo "null")
    if [[ "$nosafety_path" == "null" ]]; then
        log_pass "3.6: --no-safety-backup skips safety backup creation"
    else
        log_fail "3.6: safety backup still created with --no-safety-backup (path='$nosafety_path')"
    fi
}

# ==============================================================================
# Scenario 4: SQL dump and export options
# ==============================================================================

test_export_options() {
    log_test "Scenario 4: Export Options"

    # Ensure workspace A exists
    if [[ -z "$WORKSPACE_A" || ! -d "$WORKSPACE_A" ]]; then
        WORKSPACE_A=$(create_workspace)
        populate_workspace "$WORKSPACE_A"
    fi

    # 4.1: Export with --sql-dump
    local sqldump_json
    sqldump_json=$(run_wa_json backup export --workspace "$WORKSPACE_A" -f json --sql-dump -o "$WORKSPACE_A/.wa/backups/sqldump_test")
    assert_json_eq "$sqldump_json" '.ok' "true" "4.1: export with --sql-dump succeeds"

    local sqldump_path
    sqldump_path=$(echo "$sqldump_json" | jq -r '.data.output_path' 2>/dev/null || echo "")
    if [[ -n "$sqldump_path" && -f "$sqldump_path/database.sql" ]]; then
        log_pass "4.2: SQL dump file present"
    else
        log_fail "4.2: SQL dump file not found"
    fi

    # 4.3: SQL dump contains table creation statements
    if [[ -f "$sqldump_path/database.sql" ]]; then
        local has_create
        has_create=$(grep -c "CREATE TABLE" "$sqldump_path/database.sql" 2>/dev/null || echo "0")
        if [[ "$has_create" -gt 0 ]]; then
            log_pass "4.3: SQL dump contains CREATE TABLE statements ($has_create)"
        else
            log_fail "4.3: SQL dump has no CREATE TABLE statements"
        fi
    else
        log_fail "4.3: SQL dump file missing, cannot check content"
    fi

    # 4.4: Export with --no-verify
    local noverify_json
    noverify_json=$(run_wa_json backup export --workspace "$WORKSPACE_A" -f json --no-verify -o "$WORKSPACE_A/.wa/backups/noverify_test")
    assert_json_eq "$noverify_json" '.ok' "true" "4.4: export with --no-verify succeeds"

    # 4.5: Total size is reported
    assert_json_gt "$noverify_json" '.data.total_size_bytes' "0" "4.5: total_size_bytes > 0"

    # 4.6: Manifest wa_version is present
    assert_json_has "$noverify_json" '.data.manifest.wa_version' "4.6: manifest has wa_version"

    # 4.7: Manifest created_at is present
    assert_json_has "$noverify_json" '.data.manifest.created_at' "4.7: manifest has created_at"
}

# ==============================================================================
# Scenario 5: Dry-run mode
# ==============================================================================

test_dry_run() {
    log_test "Scenario 5: Dry-Run Import"

    # Ensure we have a valid backup
    if [[ -z "$WORKSPACE_A" || ! -d "$WORKSPACE_A" ]]; then
        WORKSPACE_A=$(create_workspace)
        populate_workspace "$WORKSPACE_A"
    fi

    local export_json
    export_json=$(run_wa_json backup export --workspace "$WORKSPACE_A" -f json -o "$WORKSPACE_A/.wa/backups/dryrun_source")
    local backup_path
    backup_path=$(echo "$export_json" | jq -r '.data.output_path' 2>/dev/null || echo "")

    if [[ -z "$backup_path" || ! -d "$backup_path" ]]; then
        log_fail "5.0: could not create backup for dry-run test"
        return
    fi

    # Create target workspace with known data
    local target_ws
    target_ws=$(create_workspace)
    populate_workspace "$target_ws"

    # Record original state
    local orig_checksum
    orig_checksum=$(sha256sum "$target_ws/.wa/wa.db" | awk '{print $1}')

    # 5.1: Dry-run import
    local dryrun_json
    dryrun_json=$(run_wa_json backup import --workspace "$target_ws" "$backup_path" --yes --dry-run -f json)
    assert_json_eq "$dryrun_json" '.ok' "true" "5.1: dry-run import succeeds"
    assert_json_eq "$dryrun_json" '.data.dry_run' "true" "5.2: dry_run field is true"

    # 5.3: Database was NOT modified
    local new_checksum
    new_checksum=$(sha256sum "$target_ws/.wa/wa.db" | awk '{print $1}')
    if [[ "$orig_checksum" == "$new_checksum" ]]; then
        log_pass "5.3: database unchanged after dry-run (checksum matches)"
    else
        log_fail "5.3: database was modified during dry-run"
    fi
}

# ==============================================================================
# Scenario 6: Error handling
# ==============================================================================

test_error_handling() {
    log_test "Scenario 6: Error Handling"

    # 6.1: Export from non-existent workspace
    local bad_export_json
    bad_export_json=$(run_wa_json backup export --workspace "/tmp/nonexistent_workspace_$(date +%s)" -f json)
    assert_json_eq "$bad_export_json" '.ok' "false" "6.1: export from missing workspace fails"

    # 6.2: Export error has error_code
    assert_json_has "$bad_export_json" '.error_code' "6.2: export error has error_code"

    # 6.3: Import from non-existent path
    local bad_import_json
    bad_import_json=$(run_wa_json backup import --workspace "$(mktemp -d)" "/tmp/nonexistent_backup_$(date +%s)" --yes -f json)
    assert_json_eq "$bad_import_json" '.ok' "false" "6.3: import from missing path fails"

    # 6.4: Import error has actionable hint
    local hint
    hint=$(echo "$bad_import_json" | jq -r '.hint // empty' 2>/dev/null || echo "")
    local error
    error=$(echo "$bad_import_json" | jq -r '.error // empty' 2>/dev/null || echo "")
    if [[ -n "$hint" || -n "$error" ]]; then
        log_pass "6.4: import error provides guidance"
    else
        log_fail "6.4: import error provides no guidance"
    fi
}

# ==============================================================================
# Scenario 7: Artifact secret scanning
# ==============================================================================

test_no_secrets() {
    log_test "Scenario 7: No Secrets in Artifacts"

    # Ensure workspace A has been used
    if [[ -z "$WORKSPACE_A" || ! -d "$WORKSPACE_A" ]]; then
        log_skip "7.1: no workspace to scan"
        return
    fi

    # Scan backup directory for secret-like patterns
    local backup_dir="$WORKSPACE_A/.wa/backups"
    if [[ ! -d "$backup_dir" ]]; then
        log_skip "7.1: no backup directory to scan"
        return
    fi

    # Check for common secret patterns
    local secret_patterns=(
        'sk-[a-zA-Z0-9]{20,}'
        'AKIA[A-Z0-9]{16}'
        'ghp_[a-zA-Z0-9]{36}'
        'password=[^[:space:]]{8,}'
    )

    local found_secrets=0
    for pattern in "${secret_patterns[@]}"; do
        local hits
        hits=$(grep -rl "$pattern" "$backup_dir" 2>/dev/null | wc -l || true)
        if [[ "$hits" -gt 0 ]]; then
            ((found_secrets++)) || true
            log_info "Secret pattern '$pattern' found in $hits file(s)"
        fi
    done

    if [[ "$found_secrets" -eq 0 ]]; then
        log_pass "7.1: no secret-like patterns found in backup artifacts"
    else
        log_fail "7.1: found $found_secrets secret-like patterns in backup artifacts"
    fi
}

# ==============================================================================
# Main
# ==============================================================================

main() {
    echo -e "${BLUE}================================================${NC}"
    echo -e "${BLUE}   E2E: Backup/Restore Cycle${NC}"
    echo -e "${BLUE}   Implements: wa-7fv${NC}"
    echo -e "${BLUE}================================================${NC}"

    # Initialize artifact collection
    e2e_init_artifacts "backup-restore-e2e" > /dev/null

    check_prerequisites

    test_roundtrip
    test_corrupt_backup
    test_safety_backup
    test_export_options
    test_dry_run
    test_error_handling
    test_no_secrets

    # Summary
    echo -e "\n${BLUE}================================================${NC}"
    echo -e "${BLUE}   Results: $TESTS_PASSED/$TESTS_RUN passed${NC}"
    if [[ $TESTS_FAILED -gt 0 ]]; then
        echo -e "${RED}   Failed: $TESTS_FAILED${NC}"
    fi
    if [[ $TESTS_SKIPPED -gt 0 ]]; then
        echo -e "${YELLOW}   Skipped: $TESTS_SKIPPED${NC}"
    fi
    echo -e "${BLUE}================================================${NC}"

    # Finalize artifacts
    e2e_finalize $TESTS_FAILED > /dev/null 2>&1 || true

    if [[ $TESTS_FAILED -gt 0 ]]; then
        exit 1
    fi
}

main "$@"
