# E2E Test Harness Specification

> wa-4vx.10.6: Deterministic scenarios + logging/artifacts contract

This document specifies the end-to-end test harness for `ft`. The harness validates the complete system: ingest, storage, pattern detection, workflows, and CLI surfaces.

---

## Design Goals

1. **Deterministic** - No real AI credentials; uses dummy agent panes with scripted output
2. **Local** - Runs on any dev machine with the active compatibility backend bridge installed (current: WezTerm)
3. **Excellent diagnostics** - Verbose logging and comprehensive artifacts on failure
4. **Self-documenting** - Clear exit codes and structured output

---

## Entry Point

```bash
./scripts/e2e_test.sh [OPTIONS] [SCENARIO...]
```

### Options

| Flag | Description | Default |
|------|-------------|---------|
| `--verbose`, `-v` | Enable verbose output (debug-level logs) | off |
| `--keep-artifacts` | Always keep artifacts (even on success) | delete on success |
| `--artifacts-dir DIR` | Override artifacts directory | `./e2e-artifacts/<timestamp>` |
| `--timeout SECS` | Global timeout per scenario | 120 |
| `--retries N` | Retry each scenario up to `N` times on failure | 0 |
| `--seed VALUE` | Deterministic run seed used for per-scenario seeds | auto (current UTC seconds) |
| `--list` | List available scenarios and exit | - |
| `--self-check` | Run harness self-check only | - |
| `--parallel N` | Run N scenarios in parallel | 1 (sequential) |
| `--workspace DIR` | Override workspace for isolation | temp directory |
| `--config FILE` | Override ft.toml for testing | generated default |
| `--default-only` | Run only scenarios marked `default=true` in registry | off |

### Arguments

- `SCENARIO...` - One or more scenario names to run. If omitted, runs all scenarios.

---

## Exit Codes

| Code | Meaning | Action |
|------|---------|--------|
| 0 | All scenarios passed | Success |
| 1 | One or more scenarios failed | Check artifacts for details |
| 2 | Harness self-check failed | Fix prerequisites before running |
| 3 | Invalid arguments | Check usage |
| 4 | Timeout exceeded | Increase timeout or investigate hang |
| 5 | Prerequisites missing | Install WezTerm, check permissions |

---

## Artifacts Directory Layout

```
e2e-artifacts/
└── 2026-01-19T09-00-00Z/           # Timestamped run directory
    ├── env.txt                      # Environment snapshot
    ├── summary.json                 # Machine-readable results
    ├── summary.txt                  # Human-readable summary
    ├── ft_config_effective.toml     # Resolved configuration
    ├── scenario_01_capture_search/
    │   ├── orchestration_manifest.json # Scenario metadata + attempt history + seed
    │   ├── attempt_01/                 # First attempt artifacts (full scenario output)
    │   ├── attempt_02/                 # Retry artifacts (if retries > 0 and needed)
    │   ├── test_artifacts_manifest.json # Canonical artifact schema (wa.test_artifacts.v1)
    │   ├── correlation.jsonl        # Correlation + timing row for this scenario
    │   ├── ft_watch.log             # Watcher stdout/stderr
    │   ├── ft_watch.jsonl           # JSON-lines structured logs
    │   ├── robot_state.json         # Final pane state
    │   ├── events.jsonl             # All detected events
    │   ├── scenario.log             # Test script output
    │   ├── db_snapshot.sqlite       # Database copy (if small)
    │   └── PASS | FAIL              # Result marker file
    ├── scenario_02_compaction_workflow/
    │   └── ...
    └── scenario_03_policy_denial/
        └── ...
```

### `env.txt` Contents

```
hostname: devbox
timestamp: 2026-01-19T09:00:00Z
wezterm_version: 20250101-120000-abc123
ft_version: 0.1.0
ft_commit: deadbeef
rust_version: 1.85.0-nightly
os: Linux 6.x x86_64
shell: /bin/bash
temp_workspace: /tmp/ft-e2e-abc123
run_seed: 1739523600
run_seed_source: auto
scenario_retries: 1
```

### `summary.json` Schema

```json
{
  "version": "1",
  "schema_version": "wa.e2e.summary.v2",
  "test_artifact_schema_version": "wa.test_artifacts.v1",
  "timestamp": "2026-01-19T09:00:00Z",
  "run_seed": "1739523600",
  "run_seed_source": "auto",
  "scenario_retries": 1,
  "duration_secs": 45.2,
  "total": 3,
  "passed": 2,
  "failed": 1,
  "skipped": 0,
  "scenarios": [
    {
      "name": "capture_search",
      "status": "passed",
      "scenario_seed": "f1d2d2f924e986ac",
      "max_attempts": 2,
      "attempts": [
        { "attempt": 1, "status": "failed", "exit_code": 1, "duration_secs": 5.0 },
        { "attempt": 2, "status": "passed", "exit_code": 0, "duration_secs": 4.2 }
      ],
      "orchestration_manifest": "scenario_01_capture_search/orchestration_manifest.json",
      "duration_secs": 12.3,
      "artifacts_dir": "scenario_01_capture_search",
      "test_artifacts_manifest": "scenario_01_capture_search/test_artifacts_manifest.json"
    },
    {
      "name": "compaction_workflow",
      "status": "failed",
      "duration_secs": 20.1,
      "error": "Timeout waiting for workflow completion",
      "artifacts_dir": "scenario_02_compaction_workflow",
      "test_artifacts_manifest": "scenario_02_compaction_workflow/test_artifacts_manifest.json"
    }
  ]
}
```

### `test_artifacts_manifest.json` Schema (Per Scenario)

Each scenario directory includes a canonical machine-parseable artifact manifest:

```json
{
  "schema_version": "wa.test_artifacts.v1",
  "run_id": "2026-01-19T09-00-00Z_capture_search",
  "generated_at_ms": 1768813200000,
  "outcome": "passed|failed|aborted",
  "correlation": {
    "test_case_id": "capture_search",
    "resize_transaction_id": "2026-01-19T09-00-00Z-capture_search-1",
    "pane_id": 12,
    "tab_id": null,
    "sequence_no": 1,
    "scheduler_decision": "e2e_harness",
    "frame_id": null
  },
  "timing": {
    "queue_wait_ms": 0.0,
    "reflow_ms": 12.3,
    "render_ms": 12.3,
    "present_ms": 12.3,
    "p50_ms": 12.3,
    "p95_ms": 12.3,
    "p99_ms": 12.3
  },
  "artifacts": [
    {
      "kind": "structured_log",
      "format": "json_lines",
      "path": "correlation.jsonl",
      "bytes": 320,
      "sha256": "abcd...",
      "redacted": true
    }
  ]
}
```

Failure cases MUST include artifact kinds:
- `trace_bundle`
- `frame_histogram`
- `failure_signature`

---

## Self-Check Mode

Before running scenarios, the harness validates prerequisites:

```bash
./scripts/e2e_test.sh --self-check
```

### Checks Performed

1. **Compatibility backend bridge installed (current: WezTerm)** - `wezterm --version` succeeds
2. **Compatibility backend bridge mux available** - Can spawn and list panes
3. **ft binary built** - `cargo build --release` or binary exists
4. **Artifacts writable** - Can create artifacts directory
5. **Temp space available** - At least 100MB free in temp
6. **Required features** - Check `ft --version` for feature flags

### Self-Check Output

```
E2E Harness Self-Check
======================
[PASS] Backend bridge installed (WezTerm): 20250101-120000-abc123
[PASS] Backend bridge mux operational: spawned test pane
[PASS] ft binary: ./target/release/ft (0.1.0)
[PASS] Artifacts directory: writable
[PASS] Temp space: 50GB available
[PASS] Feature flags: all required features present

All checks passed. Ready to run E2E tests.
```

On failure:

```
E2E Harness Self-Check
======================
[PASS] Backend bridge installed (WezTerm): 20250101-120000-abc123
[FAIL] Backend bridge mux operational: cannot connect to mux server
       Hint: Start the active compatibility backend bridge (WezTerm) with `wezterm start --mux`
[PASS] ft binary: ./target/release/ft (0.1.0)

Self-check failed. Fix issues above before running E2E tests.
Exit code: 2
```

---

## Minimum Scenarios

### Scenario 1: `capture_search`

**Purpose**: Validate ingest pipeline and FTS search.

**Steps**:

1. Start isolated mux server (or use existing)
2. Spawn dummy pane that prints N unique lines with marker token
3. Start `ft watch` in background
4. Wait for watcher to capture (poll `ft robot state`)
5. Stop `ft watch`
6. Run `ft search <marker_token>`
7. Assert: search returns expected hits with correct pane_id

**Success Criteria**:

- Segments stored in database
- FTS finds the unique marker token
- Pane state shows observed pane

**Dummy Pane Script** (`fixtures/dummy_print.sh`):

```bash
#!/bin/bash
# Emit N lines with a unique marker
MARKER="${1:-E2E_MARKER_$(date +%s)}"
for i in $(seq 1 100); do
    echo "Line $i: $MARKER"
    sleep 0.01
done
echo "Done: $MARKER"
```

---

### Scenario 2: `compaction_workflow`

**Purpose**: Validate pattern detection and workflow execution.

**Steps**:

1. Start isolated workspace
2. Spawn dummy pane that will emit compaction marker
3. Start `ft watch --auto-handle` in background
4. Trigger dummy pane to emit: `[CODEX] Compaction required: context...`
5. Wait for workflow execution (poll events/workflow status)
6. Assert: workflow sent refresh prompt to pane
7. Assert: dummy pane received and echoed the input

**Success Criteria**:

- Detection event logged with rule_id `codex:compaction`
- Workflow execution record shows `completed`
- Step log shows `send_text` action
- Audit trail records the action

**Dummy Pane Script** (`fixtures/dummy_agent.sh`):

```bash
#!/bin/bash
# Simulate agent that triggers compaction then echoes input
echo "[CODEX] Session started"
sleep 1
echo "[CODEX] Compaction required: context window 95% full"
# Wait for input and echo it
while IFS= read -r line; do
    echo "Received: $line"
    if [[ "$line" == *"exit"* ]]; then
        break
    fi
done
```

---

### Scenario 3: `policy_denial`

**Purpose**: Validate safety gates block sends to protected panes.

**Steps**:

1. Start isolated workspace
2. Spawn dummy pane and mark it as having `in_alt_screen=true` (via fixture state or marker)
3. Start `ft watch` in background
4. Attempt `ft robot send <pane_id> "test"`
5. Assert: send denied with policy error

**Success Criteria**:

- Robot response shows `ok: false`
- Error code is `policy.alt_screen_blocked` or similar
- Audit trail records the denial
- No text actually sent to pane

**Config Override for Test**:

```toml
[safety]
block_alt_screen = true

[safety.actors.robot]
send_text = true  # Allow attempt, but policy should block
```

---

## Logging Contract

### Structured Logs

When `--verbose` or `FT_LOG_FORMAT=json`:

```json
{"timestamp":"2026-01-19T09:00:00.123Z","level":"INFO","target":"frankenterm_core::ingest","pane_id":123,"seq":45,"message":"Captured segment","span":"capture_pane"}
```

Required fields:

- `timestamp` - ISO 8601 with milliseconds
- `level` - TRACE/DEBUG/INFO/WARN/ERROR
- `target` - Module path
- `message` - Human-readable message

Correlation fields (when applicable):

- `pane_id` - Pane being processed
- `seq` - Segment sequence number
- `workflow_id` - Workflow execution ID
- `event_id` - Detection event ID

### Console Output

For human runs (no `--verbose`):

```
[09:00:01] Starting scenario: capture_search
[09:00:01] Spawning dummy pane...
[09:00:02] Starting ft watch...
[09:00:05] Waiting for capture...
[09:00:08] Running search...
[09:00:08] PASS: Found 100 hits for marker
[09:00:08] Scenario capture_search: PASSED (7.2s)
```

### Failure Output

On failure, print:

```
[09:00:20] FAIL: Expected 100 hits, got 0

FAILURE DETAILS
===============
Scenario: capture_search
Duration: 20.1s
Error: Search returned no results

Artifacts saved to: ./e2e-artifacts/2026-01-19T09-00-00Z/scenario_01_capture_search/

Key files to examine:
  ft_watch.log    - Watcher output (check for errors)
  events.jsonl    - Detected events (should have entries)
  robot_state.json - Final pane state

Hint: Check if watcher started successfully and pane was observed.
```

---

## Environment Variables

| Variable | Description | Example |
|----------|-------------|---------|
| `FT_E2E_KEEP_ARTIFACTS` | Always keep artifacts | `1` |
| `FT_E2E_TIMEOUT` | Override timeout (seconds) | `300` |
| `FT_E2E_RETRIES` | Retry each scenario up to N times | `2` |
| `FT_E2E_SEED` | Deterministic run seed override | `release-2026-02-14` |
| `FT_E2E_VERBOSE` | Enable verbose output | `1` |
| `FT_E2E_WORKSPACE` | Override workspace path | `/tmp/ft-e2e` |
| `FT_LOG_LEVEL` | Log level for ft processes | `debug` |
| `FT_LOG_FORMAT` | Log format (`pretty`/`json`) | `json` |

---

## Reproducible Invocation

Use explicit seed and retry policy in CI/nightly so failures are replayable:

```bash
# Nightly baseline (single retry for transient backend startup races)
./scripts/e2e_test.sh --default-only --seed nightly-$(date -u +%F) --retries 1 --keep-artifacts

# Exact replay of a failed run
./scripts/e2e_test.sh capture_search --seed 1739523600 --retries 1 --keep-artifacts --verbose
```

Each scenario writes `orchestration_manifest.json` with:
- scenario metadata from the registry (description/default/prereqs/why)
- derived deterministic `scenario_seed`
- per-attempt status, duration, and exit code

---

## Implementation Notes

### Isolation

Each scenario runs in an isolated workspace:

```bash
WORKSPACE=$(mktemp -d /tmp/ft-e2e-XXXXXX)
export FT_WORKSPACE="$WORKSPACE"
export FT_DATA_DIR="$WORKSPACE/.ft"
```

### Cleanup

On success (without `--keep-artifacts`):

- Remove temp workspace
- Remove scenario artifacts

On failure:

- Keep all artifacts
- Print path to artifacts

### Timeout Handling

```bash
timeout --signal=KILL $TIMEOUT ft watch &
FT_PID=$!

# Wait for condition or timeout
if ! wait_for_condition "pane_captured" $TIMEOUT; then
    kill $FT_PID 2>/dev/null
    collect_artifacts
    exit 4  # Timeout
fi
```

### Wait Helpers

Instead of fixed sleeps, use polling helpers:

```bash
wait_for_pane_observed() {
    local pane_id=$1
    local timeout=${2:-30}
    local start=$(date +%s)

    while true; do
        if ft robot state | jq -e ".data[] | select(.pane_id == $pane_id and .observed)" >/dev/null; then
            return 0
        fi

        local elapsed=$(($(date +%s) - start))
        if [[ $elapsed -ge $timeout ]]; then
            return 1
        fi

        sleep 0.5
    done
}
```

---

## Fixture Files

Location: `fixtures/e2e/`

```
fixtures/e2e/
├── dummy_print.sh        # Simple print script
├── dummy_agent.sh        # Agent simulator (responds to events)
├── dummy_alt_screen.sh   # Enters alt screen mode
├── config_baseline.toml  # Baseline test config
├── config_strict.toml    # Strict policy config
└── patterns/
    └── test_pack.yaml    # Test pattern pack
```

---

## CI Integration

GitHub Actions workflow should:

1. Run `--self-check` first
2. Run all scenarios with `--verbose --keep-artifacts`
3. Upload artifacts directory on failure
4. Parse `summary.json` for status

```yaml
- name: E2E Tests
  run: ./scripts/e2e_test.sh --verbose --keep-artifacts

- name: Upload artifacts on failure
  if: failure()
  uses: actions/upload-artifact@v4
  with:
    name: e2e-artifacts
    path: ./e2e-artifacts/
```

---

## Acceptance Criteria

This specification is complete when another contributor can implement the harness from this document alone:

1. Script structure and arguments are fully defined
2. Exit codes cover all failure modes
3. Artifacts layout is documented with examples
4. All minimum scenarios have step-by-step instructions
5. Logging contract specifies required fields
6. Self-check mode validates all prerequisites
7. Environment variables allow customization
8. Implementation notes provide guidance on common patterns

---

## Related Beads

- **wa-4vx.10.11** - E2E runner implementation (implements this spec)
- **wa-4vx.10.7** - Capture + FTS search scenario
- **wa-4vx.10.8** - Compaction workflow scenario
- **wa-4vx.10.10** - Policy gating scenario
- **wa-4vx.6.5** - Structured logging baseline (dependency)
