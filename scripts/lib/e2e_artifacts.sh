#!/bin/bash
# E2E Artifact Packer Library
# Implements: bd-35zb
#
# This library provides reusable helpers that collect detailed logs and artifacts
# for E2E tests. Scripts can opt-in with a single wrapper call, and failures
# always include the full artifact bundle.
#
# Usage:
#   source scripts/lib/e2e_artifacts.sh
#   e2e_init_artifacts "my-test-run"
#   e2e_capture_scenario "scenario_name" my_test_function
#   e2e_finalize $?
#
# Artifact Directory Layout:
#   <run_dir>/
#   ├── manifest.json           # Legacy-compatible run manifest (v2 fields)
#   ├── summary.json            # Canonical run summary (wa.e2e.summary.v2)
#   ├── env.json                # Environment snapshot
#   ├── scenarios/
#   │   └── <scenario_name>/
#   │       ├── stdout.log      # Captured stdout
#   │       ├── stderr.log      # Captured stderr
#   │       ├── combined.log    # Interleaved stdout/stderr
#   │       ├── exit_code       # Exit code (contents: integer)
#   │       ├── duration_ms     # Duration in milliseconds
#   │       ├── correlation.jsonl           # Correlation + timing row
#   │       ├── test_artifacts_manifest.json # Canonical scenario manifest
#   │       ├── trace_bundle.json           # Failure-only trace bundle
#   │       ├── frame_histogram.json        # Failure-only frame metrics
#   │       ├── failure_signature.json      # Failure-only signature
#   │       └── *.json                      # Any JSON outputs collected
#   └── redacted/               # Redacted copies (if secrets found)

set -euo pipefail

# ==============================================================================
# Configuration
# ==============================================================================

# Default artifacts base (can be overridden)
E2E_ARTIFACTS_BASE="${E2E_ARTIFACTS_BASE:-${PROJECT_ROOT:-$(pwd)}/e2e-artifacts}"

# Maximum file size before truncation (10MB default)
E2E_MAX_FILE_SIZE="${E2E_MAX_FILE_SIZE:-10485760}"

# Redaction enabled by default
E2E_REDACT_SECRETS="${E2E_REDACT_SECRETS:-true}"

# Patterns to redact (newline-separated regex patterns)
E2E_REDACT_PATTERNS="${E2E_REDACT_PATTERNS:-}"

# Canonical artifact schemas
E2E_TEST_ARTIFACT_SCHEMA_VERSION="${E2E_TEST_ARTIFACT_SCHEMA_VERSION:-wa.test_artifacts.v1}"
E2E_TEST_SUMMARY_SCHEMA_VERSION="${E2E_TEST_SUMMARY_SCHEMA_VERSION:-wa.e2e.summary.v2}"

# Internal state (use global variables that survive across function calls)
# These are deliberately NOT local - they need to persist
E2E_RUN_DIR="${E2E_RUN_DIR:-}"
E2E_SCENARIOS_DIR="${E2E_SCENARIOS_DIR:-}"
E2E_CURRENT_SCENARIO="${E2E_CURRENT_SCENARIO:-}"
E2E_START_TIME="${E2E_START_TIME:-}"
E2E_PASSED="${E2E_PASSED:-0}"
E2E_FAILED="${E2E_FAILED:-0}"
declare -a E2E_SCENARIOS 2>/dev/null || E2E_SCENARIOS=()

# ==============================================================================
# Default Redaction Patterns
# ==============================================================================

_E2E_DEFAULT_REDACT_PATTERNS='
# API Keys and Tokens - generic sk- prefix keys
sk-[a-zA-Z0-9_-]{20,}
api[_-]?key=[a-zA-Z0-9_-]{16,}
token=[a-zA-Z0-9_-]{20,}
[Bb]earer [a-zA-Z0-9._-]+

# Secrets and Passwords
password=[^[:space:]]{4,}
secret=[a-zA-Z0-9_-]{16,}

# AWS Credentials
AKIA[A-Z0-9]{16}
aws_secret_access_key=[a-zA-Z0-9/+=]{40}

# GitHub/GitLab Tokens
gh[pousr]_[a-zA-Z0-9]{36,}
glpat-[a-zA-Z0-9_-]{20,}

# Authorization headers
Authorization:[[:space:]]*[^[:space:]]+
'

# ==============================================================================
# Utility Functions
# ==============================================================================

# Get current timestamp in ISO8601 format
_e2e_timestamp() {
    date -u +"%Y-%m-%dT%H:%M:%SZ"
}

# Get timestamp suitable for directory names
_e2e_dir_timestamp() {
    date -u +"%Y-%m-%dT%H-%M-%SZ"
}

# Get current time in milliseconds (best effort)
_e2e_time_ms() {
    if command -v python3 &>/dev/null; then
        python3 -c 'import time; print(int(time.time() * 1000))'
    elif [[ -f /proc/uptime ]]; then
        # Linux fallback: use /proc/uptime with nanoseconds
        awk '{printf "%.0f", $1 * 1000}' /proc/uptime
    else
        # Last resort: seconds * 1000
        echo $(($(date +%s) * 1000))
    fi
}

# Log with prefix
_e2e_log() {
    local level="$1"
    shift
    echo "[e2e_artifacts] [$level] $*" >&2
}

_e2e_debug() {
    if [[ "${E2E_DEBUG:-}" == "true" ]]; then
        _e2e_log "DEBUG" "$@"
    fi
}

_e2e_info() {
    _e2e_log "INFO" "$@"
}

_e2e_warn() {
    _e2e_log "WARN" "$@"
}

_e2e_error() {
    _e2e_log "ERROR" "$@"
}

_e2e_file_size_bytes() {
    local path="$1"
    stat -f%z "$path" 2>/dev/null || stat -c%s "$path" 2>/dev/null || echo 0
}

_e2e_sha256_file() {
    local path="$1"
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$path" | awk '{print $1}'
    elif command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$path" | awk '{print $1}'
    else
        echo ""
    fi
}

_e2e_infer_artifact_kind() {
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
        *screenshot*.png|*.png|*.jpg|*.jpeg)
            echo "screenshot"
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

_e2e_infer_artifact_format() {
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

_e2e_infer_artifact_redacted() {
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

_e2e_extract_pane_id() {
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

_e2e_derive_failure_signature() {
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

_e2e_ensure_failure_artifacts() {
    local scenario_name="$1"
    local scenario_dir="$2"
    local duration_ms="$3"
    local signature="$4"
    local generated_at
    generated_at=$(_e2e_timestamp)

    local trace_file="$scenario_dir/trace_bundle.json"
    if [[ ! -f "$trace_file" ]]; then
        local stdout_tail=""
        local stderr_tail=""
        local combined_tail=""
        if [[ -f "$scenario_dir/stdout.log" ]]; then
            stdout_tail=$(tail -n 200 "$scenario_dir/stdout.log" 2>/dev/null || true)
        fi
        if [[ -f "$scenario_dir/stderr.log" ]]; then
            stderr_tail=$(tail -n 200 "$scenario_dir/stderr.log" 2>/dev/null || true)
        fi
        if [[ -f "$scenario_dir/combined.log" ]]; then
            combined_tail=$(tail -n 200 "$scenario_dir/combined.log" 2>/dev/null || true)
        fi
        jq -n \
            --arg schema_version "wa.trace_bundle.v1" \
            --arg generated_at "$generated_at" \
            --arg test_case_id "$scenario_name" \
            --arg failure_signature "$signature" \
            --arg stdout_tail "$stdout_tail" \
            --arg stderr_tail "$stderr_tail" \
            --arg combined_tail "$combined_tail" \
            --argjson duration_ms "$duration_ms" \
            '{
                schema_version: $schema_version,
                generated_at: $generated_at,
                test_case_id: $test_case_id,
                failure_signature: $failure_signature,
                duration_ms: $duration_ms,
                tails: {
                    stdout: $stdout_tail,
                    stderr: $stderr_tail,
                    combined: $combined_tail
                }
            }' > "$trace_file"
    fi

    local histogram_file="$scenario_dir/frame_histogram.json"
    if [[ ! -f "$histogram_file" ]]; then
        jq -n \
            --arg schema_version "wa.frame_histogram.v1" \
            --arg generated_at "$generated_at" \
            --arg test_case_id "$scenario_name" \
            --arg failure_signature "$signature" \
            --argjson duration_ms "$duration_ms" \
            '{
                schema_version: $schema_version,
                generated_at: $generated_at,
                test_case_id: $test_case_id,
                failure_signature: $failure_signature,
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

_e2e_build_scenario_artifacts_json() {
    local scenario_dir="$1"
    local entries="[]"
    while IFS= read -r file_path; do
        local file_name
        local kind
        local format
        local bytes
        local sha256
        local redacted
        file_name=$(basename "$file_path")
        kind=$(_e2e_infer_artifact_kind "$file_name")
        format=$(_e2e_infer_artifact_format "$file_name")
        bytes=$(_e2e_file_size_bytes "$file_path")
        sha256=$(_e2e_sha256_file "$file_path")
        redacted=$(_e2e_infer_artifact_redacted "$file_name")
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

_e2e_emit_scenario_manifest() {
    local scenario_name="$1"
    local scenario_index="$2"
    local exit_code="$3"
    local duration_ms="$4"
    local scenario_dir="$5"
    local outcome="passed"
    local failure_signature=""
    if [[ "$exit_code" -ne 0 ]]; then
        outcome="failed"
    fi

    local pane_id=""
    pane_id=$(_e2e_extract_pane_id "$scenario_dir")
    local resize_transaction_id
    resize_transaction_id="$(basename "$E2E_RUN_DIR")-${scenario_name}-${scenario_index}"
    local sequence_no="$scenario_index"
    local scheduler_decision="e2e_artifacts"

    if [[ "$outcome" != "passed" ]]; then
        failure_signature=$(_e2e_derive_failure_signature "$scenario_dir")
        _e2e_ensure_failure_artifacts "$scenario_name" "$scenario_dir" "$duration_ms" "$failure_signature"
    fi

    local correlation_jsonl="$scenario_dir/correlation.jsonl"
    jq -cn \
        --arg timestamp "$(_e2e_timestamp)" \
        --arg test_case_id "$scenario_name" \
        --arg resize_transaction_id "$resize_transaction_id" \
        --arg pane_id "$pane_id" \
        --arg sequence_no "$sequence_no" \
        --arg scheduler_decision "$scheduler_decision" \
        --argjson queue_wait_ms 0 \
        --argjson reflow_ms "$duration_ms" \
        --argjson render_ms "$duration_ms" \
        --argjson present_ms "$duration_ms" \
        --argjson p50_ms "$duration_ms" \
        --argjson p95_ms "$duration_ms" \
        --argjson p99_ms "$duration_ms" \
        '{
            timestamp: $timestamp,
            test_case_id: $test_case_id,
            resize_transaction_id: $resize_transaction_id,
            pane_id: (if $pane_id == "" then null else ($pane_id | tonumber) end),
            tab_id: null,
            sequence_no: (if $sequence_no == "" then null else ($sequence_no | tonumber) end),
            scheduler_decision: $scheduler_decision,
            frame_id: null,
            queue_wait_ms: $queue_wait_ms,
            reflow_ms: $reflow_ms,
            render_ms: $render_ms,
            present_ms: $present_ms,
            p50_ms: $p50_ms,
            p95_ms: $p95_ms,
            p99_ms: $p99_ms
        }' > "$correlation_jsonl"

    local artifacts_json
    artifacts_json=$(_e2e_build_scenario_artifacts_json "$scenario_dir")
    local manifest_path="$scenario_dir/test_artifacts_manifest.json"
    jq -n \
        --arg schema_version "$E2E_TEST_ARTIFACT_SCHEMA_VERSION" \
        --arg run_id "$(basename "$E2E_RUN_DIR")_${scenario_name}" \
        --argjson generated_at_ms "$(_e2e_time_ms)" \
        --arg outcome "$outcome" \
        --arg test_case_id "$scenario_name" \
        --arg resize_transaction_id "$resize_transaction_id" \
        --arg pane_id "$pane_id" \
        --arg sequence_no "$sequence_no" \
        --arg scheduler_decision "$scheduler_decision" \
        --argjson queue_wait_ms 0 \
        --argjson reflow_ms "$duration_ms" \
        --argjson render_ms "$duration_ms" \
        --argjson present_ms "$duration_ms" \
        --argjson p50_ms "$duration_ms" \
        --argjson p95_ms "$duration_ms" \
        --argjson p99_ms "$duration_ms" \
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
                tab_id: null,
                sequence_no: (if $sequence_no == "" then null else ($sequence_no | tonumber) end),
                scheduler_decision: $scheduler_decision,
                frame_id: null
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

    echo "$failure_signature"
}

# ==============================================================================
# Initialization
# ==============================================================================

# Initialize artifact collection for a test run
# Usage: e2e_init_artifacts [run_name]
# Returns: 0 on success, sets E2E_RUN_DIR
e2e_init_artifacts() {
    local run_name="${1:-}"

    # Create timestamped run directory
    local timestamp
    timestamp=$(_e2e_dir_timestamp)

    if [[ -n "$run_name" ]]; then
        E2E_RUN_DIR="$E2E_ARTIFACTS_BASE/${timestamp}_${run_name}"
    else
        E2E_RUN_DIR="$E2E_ARTIFACTS_BASE/$timestamp"
    fi

    E2E_SCENARIOS_DIR="$E2E_RUN_DIR/scenarios"
    mkdir -p "$E2E_SCENARIOS_DIR"

    E2E_START_TIME=$(_e2e_time_ms)
    E2E_SCENARIOS=()
    E2E_PASSED=0
    E2E_FAILED=0

    _e2e_info "Initialized artifacts: $E2E_RUN_DIR"

    # Capture environment snapshot
    _e2e_capture_env

    # Return the run directory path (can be captured with $())
    echo "$E2E_RUN_DIR"
}

# Capture environment snapshot
_e2e_capture_env() {
    local env_file="$E2E_RUN_DIR/env.json"

    # Build environment JSON
    cat > "$env_file" <<EOF
{
  "timestamp": "$(_e2e_timestamp)",
  "hostname": "$(hostname 2>/dev/null || echo 'unknown')",
  "os": {
    "name": "$(uname -s 2>/dev/null || echo 'unknown')",
    "release": "$(uname -r 2>/dev/null || echo 'unknown')",
    "machine": "$(uname -m 2>/dev/null || echo 'unknown')"
  },
  "shell": "${SHELL:-unknown}",
  "pwd": "$(pwd)",
  "user": "${USER:-unknown}",
  "versions": {
    "bash": "${BASH_VERSION:-unknown}",
    "rust": "$(rustc --version 2>/dev/null | head -1 || echo 'not installed')",
    "cargo": "$(cargo --version 2>/dev/null | head -1 || echo 'not installed')",
    "wezterm": "$(wezterm --version 2>/dev/null | head -1 || echo 'not installed')"
  },
  "env_vars": {
    "FT_DATA_DIR": "${FT_DATA_DIR:-}",
    "FT_WORKSPACE": "${FT_WORKSPACE:-}",
    "FT_CONFIG": "${FT_CONFIG:-}",
    "FT_LOG_LEVEL": "${FT_LOG_LEVEL:-}",
    "CI": "${CI:-}",
    "GITHUB_ACTIONS": "${GITHUB_ACTIONS:-}",
    "TERM": "${TERM:-}"
  }
}
EOF

    _e2e_debug "Environment captured: $env_file"
}

# ==============================================================================
# Scenario Capture
# ==============================================================================

# Capture all outputs for a scenario execution
# Usage: e2e_capture_scenario <name> <command...>
# Returns: The exit code of the command
e2e_capture_scenario() {
    local name="$1"
    shift
    local -a cmd=("$@")

    if [[ -z "$E2E_RUN_DIR" ]]; then
        _e2e_error "Artifacts not initialized. Call e2e_init_artifacts first."
        return 1
    fi

    local scenario_dir="$E2E_SCENARIOS_DIR/$name"
    mkdir -p "$scenario_dir"
    E2E_CURRENT_SCENARIO="$name"

    local stdout_file="$scenario_dir/stdout.log"
    local stderr_file="$scenario_dir/stderr.log"
    local combined_file="$scenario_dir/combined.log"
    local exit_code_file="$scenario_dir/exit_code"
    local duration_file="$scenario_dir/duration_ms"
    local metadata_file="$scenario_dir/metadata.json"

    _e2e_info "Running scenario: $name"
    _e2e_debug "Command: ${cmd[*]}"

    local start_ms
    start_ms=$(_e2e_time_ms)

    # Execute with output capture
    # Use a subshell with process substitution to capture both streams
    local exit_code=0
    {
        # Capture stdout and stderr separately while also writing to combined
        "${cmd[@]}" \
            > >(tee "$stdout_file" >> "$combined_file") \
            2> >(tee "$stderr_file" >> "$combined_file" >&2)
    } || exit_code=$?

    local end_ms
    end_ms=$(_e2e_time_ms)
    local duration_ms=$((end_ms - start_ms))

    # Write metadata
    echo "$exit_code" > "$exit_code_file"
    echo "$duration_ms" > "$duration_file"

    cat > "$metadata_file" <<EOF
{
  "scenario": "$name",
  "started_at": "$(_e2e_timestamp)",
  "duration_ms": $duration_ms,
  "exit_code": $exit_code,
  "command": $(printf '%s\n' "${cmd[@]}" | jq -R . | jq -s .),
  "files": {
    "stdout": "stdout.log",
    "stderr": "stderr.log",
    "combined": "combined.log"
  }
}
EOF

    # Apply redaction if enabled
    if [[ "$E2E_REDACT_SECRETS" == "true" ]]; then
        e2e_redact_secrets "$stdout_file"
        e2e_redact_secrets "$stderr_file"
        e2e_redact_secrets "$combined_file"
    fi

    # Apply size limits
    e2e_limit_size "$stdout_file" "$E2E_MAX_FILE_SIZE"
    e2e_limit_size "$stderr_file" "$E2E_MAX_FILE_SIZE"
    e2e_limit_size "$combined_file" "$E2E_MAX_FILE_SIZE"

    # Track results
    if [[ $exit_code -eq 0 ]]; then
        touch "$scenario_dir/PASS"
        ((E2E_PASSED++)) || true
        _e2e_info "Scenario PASSED: $name (${duration_ms}ms)"
    else
        touch "$scenario_dir/FAIL"
        ((E2E_FAILED++)) || true
        _e2e_warn "Scenario FAILED: $name (${duration_ms}ms, exit=$exit_code)"
    fi

    E2E_SCENARIOS+=("$name:$exit_code:$duration_ms")
    E2E_CURRENT_SCENARIO=""

    return $exit_code
}

# ==============================================================================
# File Management
# ==============================================================================

# Add a file to the current scenario's artifacts
# Usage: e2e_add_file <name> [content]
# If content is omitted, reads from stdin
e2e_add_file() {
    local name="$1"
    local content="${2:-}"

    local target_dir
    if [[ -n "$E2E_CURRENT_SCENARIO" ]]; then
        target_dir="$E2E_SCENARIOS_DIR/$E2E_CURRENT_SCENARIO"
    else
        target_dir="$E2E_RUN_DIR"
    fi

    if [[ ! -d "$target_dir" ]]; then
        _e2e_error "No active scenario or run directory"
        return 1
    fi

    local file_path="$target_dir/$name"

    if [[ -n "$content" ]]; then
        echo "$content" > "$file_path"
    else
        cat > "$file_path"
    fi

    # Apply redaction and size limit
    if [[ "$E2E_REDACT_SECRETS" == "true" ]]; then
        e2e_redact_secrets "$file_path"
    fi
    e2e_limit_size "$file_path" "$E2E_MAX_FILE_SIZE"

    _e2e_debug "Added file: $file_path"
}

# Add JSON content to artifacts
# Usage: e2e_add_json <name> <json_content>
# Validates JSON and pretty-prints
e2e_add_json() {
    local name="$1"
    local content="$2"

    local target_dir
    if [[ -n "$E2E_CURRENT_SCENARIO" ]]; then
        target_dir="$E2E_SCENARIOS_DIR/$E2E_CURRENT_SCENARIO"
    else
        target_dir="$E2E_RUN_DIR"
    fi

    local file_path="$target_dir/$name"

    # Validate and pretty-print JSON
    if echo "$content" | jq . > "$file_path" 2>/dev/null; then
        _e2e_debug "Added JSON: $file_path"
    else
        # If invalid JSON, save as-is with warning
        echo "$content" > "$file_path"
        _e2e_warn "Invalid JSON content saved to: $file_path"
    fi

    # Apply redaction
    if [[ "$E2E_REDACT_SECRETS" == "true" ]]; then
        e2e_redact_secrets "$file_path"
    fi
}

# Copy existing file(s) to artifacts
# Usage: e2e_copy_file <source> [dest_name]
e2e_copy_file() {
    local source="$1"
    local dest_name="${2:-$(basename "$source")}"

    local target_dir
    if [[ -n "$E2E_CURRENT_SCENARIO" ]]; then
        target_dir="$E2E_SCENARIOS_DIR/$E2E_CURRENT_SCENARIO"
    else
        target_dir="$E2E_RUN_DIR"
    fi

    if [[ -f "$source" ]]; then
        cp "$source" "$target_dir/$dest_name"

        if [[ "$E2E_REDACT_SECRETS" == "true" ]]; then
            e2e_redact_secrets "$target_dir/$dest_name"
        fi
        e2e_limit_size "$target_dir/$dest_name" "$E2E_MAX_FILE_SIZE"

        _e2e_debug "Copied file: $source -> $target_dir/$dest_name"
    elif [[ -d "$source" ]]; then
        cp -r "$source" "$target_dir/$dest_name"
        _e2e_debug "Copied directory: $source -> $target_dir/$dest_name"
    else
        _e2e_warn "Source not found: $source"
        return 1
    fi
}

# ==============================================================================
# Redaction
# ==============================================================================

# Redact sensitive patterns from a file (in-place)
# Usage: e2e_redact_secrets <file>
e2e_redact_secrets() {
    local file="$1"

    if [[ ! -f "$file" ]]; then
        return 0
    fi

    # Combine default and custom patterns
    local patterns="$_E2E_DEFAULT_REDACT_PATTERNS"
    if [[ -n "$E2E_REDACT_PATTERNS" ]]; then
        patterns="$patterns
$E2E_REDACT_PATTERNS"
    fi

    local temp_file
    temp_file=$(mktemp)
    local redaction_count=0

    cp "$file" "$temp_file"

    # Apply each pattern
    while IFS= read -r pattern; do
        # Skip empty lines and comments
        [[ -z "$pattern" || "$pattern" =~ ^[[:space:]]*# ]] && continue
        pattern=$(echo "$pattern" | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')
        [[ -z "$pattern" ]] && continue

        # Count matches before redaction
        local matches
        matches=$(grep -cE "$pattern" "$temp_file" 2>/dev/null || true)
        # Ensure matches is a valid integer (handle empty, whitespace, or multi-line)
        matches="${matches%%[^0-9]*}"
        matches="${matches:-0}"

        if [[ "$matches" -gt 0 ]]; then
            # Redact pattern, preserving structure
            if command -v perl &>/dev/null; then
                perl -pi -e "s/$pattern/[REDACTED]/g" "$temp_file" 2>/dev/null || true
            else
                sed -i -E "s/$pattern/[REDACTED]/g" "$temp_file" 2>/dev/null || true
            fi
            redaction_count=$((redaction_count + matches))
        fi
    done <<< "$patterns"

    if [[ $redaction_count -gt 0 ]]; then
        mv "$temp_file" "$file"
        _e2e_debug "Redacted $redaction_count sensitive patterns in: $file"

        # Add redaction notice to file
        echo "" >> "$file"
        echo "# [e2e_artifacts] $redaction_count sensitive pattern(s) redacted" >> "$file"
    else
        rm -f "$temp_file"
    fi
}

# Add custom redaction pattern
# Usage: e2e_add_redact_pattern <regex>
e2e_add_redact_pattern() {
    local pattern="$1"
    E2E_REDACT_PATTERNS="${E2E_REDACT_PATTERNS}
$pattern"
    _e2e_debug "Added redaction pattern: $pattern"
}

# ==============================================================================
# Size Limiting
# ==============================================================================

# Limit file size, truncating with notice if exceeded
# Usage: e2e_limit_size <file> <max_bytes>
e2e_limit_size() {
    local file="$1"
    local max_bytes="$2"

    if [[ ! -f "$file" ]]; then
        return 0
    fi

    local file_size
    file_size=$(stat -c%s "$file" 2>/dev/null || stat -f%z "$file" 2>/dev/null || echo 0)

    if [[ $file_size -gt $max_bytes ]]; then
        local keep_bytes=$((max_bytes - 1024))  # Reserve 1KB for truncation notice

        # Create truncated version
        local temp_file
        temp_file=$(mktemp)

        # Keep first portion
        head -c "$((keep_bytes / 2))" "$file" > "$temp_file"

        # Add truncation notice
        cat >> "$temp_file" <<EOF

... [e2e_artifacts] TRUNCATED ...
Original size: $file_size bytes
Limit: $max_bytes bytes
Truncated: $((file_size - max_bytes + 1024)) bytes removed
...

EOF

        # Keep last portion
        tail -c "$((keep_bytes / 2))" "$file" >> "$temp_file"

        mv "$temp_file" "$file"
        _e2e_warn "Truncated oversized file: $file ($file_size -> $max_bytes bytes)"
    fi
}

# ==============================================================================
# Finalization
# ==============================================================================

# Finalize artifacts and write manifest
# Usage: e2e_finalize [overall_exit_code]
e2e_finalize() {
    local overall_exit="${1:-0}"

    if [[ -z "$E2E_RUN_DIR" ]]; then
        _e2e_error "Artifacts not initialized"
        return 1
    fi

    local end_ms
    end_ms=$(_e2e_time_ms)
    local total_duration_ms=$(( end_ms - E2E_START_TIME ))

    local total_scenarios=${#E2E_SCENARIOS[@]}

    # Build manifest JSON
    local manifest_file="$E2E_RUN_DIR/manifest.json"
    local summary_json_file="$E2E_RUN_DIR/summary.json"

    # Build scenarios array for manifest/summary and emit per-scenario manifests.
    local scenarios_json="[]"
    local scenario_index=1
    for entry in "${E2E_SCENARIOS[@]}"; do
        IFS=':' read -r name exit_code duration <<< "$entry"
        local status="passed"
        [[ $exit_code -ne 0 ]] && status="failed"
        local scenario_dir="$E2E_SCENARIOS_DIR/$name"
        local failure_signature=""
        if [[ -d "$scenario_dir" ]]; then
            failure_signature=$(_e2e_emit_scenario_manifest \
                "$name" "$scenario_index" "$exit_code" "$duration" "$scenario_dir")
        fi

        scenarios_json=$(echo "$scenarios_json" | jq -c \
            --arg name "$name" \
            --arg status "$status" \
            --argjson exit_code "$exit_code" \
            --argjson duration "$duration" \
            --arg artifacts_dir "scenarios/$name" \
            --arg test_artifacts_manifest "scenarios/$name/test_artifacts_manifest.json" \
            --arg failure_signature "$failure_signature" \
            '. + [{
                name: $name,
                status: $status,
                exit_code: $exit_code,
                duration_ms: $duration,
                artifacts_dir: $artifacts_dir,
                test_artifacts_manifest: $test_artifacts_manifest
            } + (if $status == "failed" and $failure_signature != "" then {error: $failure_signature} else {} end)]')
        ((scenario_index++))
    done

    cat > "$manifest_file" <<EOF
{
  "version": "1.0.0",
  "schema_version": "$E2E_TEST_SUMMARY_SCHEMA_VERSION",
  "test_artifact_schema_version": "$E2E_TEST_ARTIFACT_SCHEMA_VERSION",
  "schema": "https://github.com/Dicklesworthstone/frankenterm/e2e-manifest-v2",
  "generated_at": "$(_e2e_timestamp)",
  "generator": "e2e_artifacts.sh",
  "run": {
    "directory": "$E2E_RUN_DIR",
    "started_at": "$(date -d "@$((E2E_START_TIME / 1000))" -u +"%Y-%m-%dT%H:%M:%SZ" 2>/dev/null || date -u +"%Y-%m-%dT%H:%M:%SZ")",
    "duration_ms": $total_duration_ms,
    "overall_exit_code": $overall_exit
  },
  "results": {
    "total": $total_scenarios,
    "passed": $E2E_PASSED,
    "failed": $E2E_FAILED,
    "pass_rate": $(echo "scale=4; $E2E_PASSED / ($total_scenarios + 0.0001)" | bc 2>/dev/null || echo "0")
  },
  "scenarios": $scenarios_json,
  "files": {
    "env": "env.json",
    "manifest": "manifest.json",
    "summary": "summary.json",
    "scenarios_dir": "scenarios"
  },
  "settings": {
    "max_file_size": $E2E_MAX_FILE_SIZE,
    "redact_secrets": $E2E_REDACT_SECRETS
  }
}
EOF

    cat > "$summary_json_file" <<EOF
{
  "version": "1",
  "schema_version": "$E2E_TEST_SUMMARY_SCHEMA_VERSION",
  "test_artifact_schema_version": "$E2E_TEST_ARTIFACT_SCHEMA_VERSION",
  "timestamp": "$(_e2e_timestamp)",
  "duration_secs": $((total_duration_ms / 1000)),
  "total": $total_scenarios,
  "passed": $E2E_PASSED,
  "failed": $E2E_FAILED,
  "skipped": 0,
  "scenarios": $scenarios_json
}
EOF

    # Also write human-readable summary
    local summary_file="$E2E_RUN_DIR/summary.txt"
    cat > "$summary_file" <<EOF
E2E Test Run Summary
====================
Directory: $E2E_RUN_DIR
Generated: $(_e2e_timestamp)

Results
-------
Total:    $total_scenarios
Passed:   $E2E_PASSED
Failed:   $E2E_FAILED
Duration: ${total_duration_ms}ms

Scenarios
---------
EOF

    for entry in "${E2E_SCENARIOS[@]}"; do
        IFS=':' read -r name exit_code duration <<< "$entry"
        local status="PASS"
        [[ $exit_code -ne 0 ]] && status="FAIL"
        printf "  [%s] %s (%dms, exit=%d)\n" "$status" "$name" "$duration" "$exit_code" >> "$summary_file"
    done

    _e2e_info "Artifacts finalized: $E2E_RUN_DIR"
    _e2e_info "Results: $E2E_PASSED passed, $E2E_FAILED failed"

    # Print path for CI artifact upload
    echo ""
    echo "ARTIFACTS_DIR=$E2E_RUN_DIR"
}

# ==============================================================================
# Convenience Wrappers
# ==============================================================================

# All-in-one wrapper for simple test scripts
# Usage: e2e_run_test <test_name> <command...>
# Initializes, captures, finalizes in one call
e2e_run_test() {
    local test_name="$1"
    shift

    local run_dir
    run_dir=$(e2e_init_artifacts "$test_name")

    local exit_code=0
    e2e_capture_scenario "$test_name" "$@" || exit_code=$?

    e2e_finalize $exit_code

    return $exit_code
}

# Get the current artifacts directory
e2e_get_artifacts_dir() {
    echo "$E2E_RUN_DIR"
}

# Get the current scenario directory
e2e_get_scenario_dir() {
    if [[ -n "$E2E_CURRENT_SCENARIO" ]]; then
        echo "$E2E_SCENARIOS_DIR/$E2E_CURRENT_SCENARIO"
    else
        echo ""
    fi
}

# ==============================================================================
# CI Integration Helpers
# ==============================================================================

# Generate GitHub Actions step summary
# Usage: e2e_github_summary
e2e_github_summary() {
    if [[ -z "${GITHUB_STEP_SUMMARY:-}" ]]; then
        return 0
    fi

    cat >> "$GITHUB_STEP_SUMMARY" <<EOF

## E2E Test Results

| Metric | Value |
|--------|-------|
| **Total** | ${#E2E_SCENARIOS[@]} |
| **Passed** | $E2E_PASSED |
| **Failed** | $E2E_FAILED |

### Scenarios

EOF

    for entry in "${E2E_SCENARIOS[@]}"; do
        IFS=':' read -r name exit_code duration <<< "$entry"
        local icon="✅"
        [[ $exit_code -ne 0 ]] && icon="❌"
        echo "- $icon **$name** (${duration}ms)" >> "$GITHUB_STEP_SUMMARY"
    done

    if [[ -n "$E2E_RUN_DIR" ]]; then
        echo "" >> "$GITHUB_STEP_SUMMARY"
        echo "> Artifacts: \`$E2E_RUN_DIR\`" >> "$GITHUB_STEP_SUMMARY"
    fi
}

# Generate JSON output for CI parsing
# Usage: e2e_ci_output > results.json
e2e_ci_output() {
    if [[ -z "$E2E_RUN_DIR" || ! -f "$E2E_RUN_DIR/manifest.json" ]]; then
        echo '{"error": "No artifacts available"}'
        return 1
    fi
    cat "$E2E_RUN_DIR/manifest.json"
}

# ==============================================================================
# Export functions for sourcing
# ==============================================================================

export -f e2e_init_artifacts
export -f e2e_capture_scenario
export -f e2e_add_file
export -f e2e_add_json
export -f e2e_copy_file
export -f e2e_redact_secrets
export -f e2e_add_redact_pattern
export -f e2e_limit_size
export -f e2e_finalize
export -f e2e_run_test
export -f e2e_get_artifacts_dir
export -f e2e_get_scenario_dir
export -f e2e_github_summary
export -f e2e_ci_output
