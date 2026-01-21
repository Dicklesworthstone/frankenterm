# Test Logging Contract

> bd-u194: Structured logs + artifact manifest for unit and E2E tests

This document defines the logging and artifact contract for all tests in `wa`. Following this contract ensures:
- Consistent, machine-parseable output across test types
- Reliable CI failure detection
- Reproducible debugging with complete artifacts
- Secure handling of sensitive data through redaction

---

## Scope

This contract applies to:
- **Unit tests** (`cargo test`)
- **Integration tests** (`crates/wa-core/tests/`)
- **E2E tests** (`scripts/e2e_test.sh`)
- **Benchmarks** (`cargo bench`)

---

## Log Levels and Usage

### Level Guidelines

| Level | Usage | Example |
|-------|-------|---------|
| `ERROR` | Test failure, assertion violation, critical invariant broken | "Assertion failed: expected 100 hits, got 0" |
| `WARN` | Non-fatal issues, deprecation, retries | "Retrying connection (attempt 2/3)" |
| `INFO` | Test progress, major phases, results | "Starting scenario: capture_search" |
| `DEBUG` | Detailed execution, intermediate states | "Captured segment seq=45 len=1024" |
| `TRACE` | Verbose details, raw data (redacted) | "Pattern scan: 45 candidates, 3 matches" |

### Test Type Prefixes

Use prefixes to identify log source in multi-test runs:

| Test Type | Prefix | Example |
|-----------|--------|---------|
| Unit test | `[UNIT]` | `[UNIT] policy::deny_alt_screen: PASS` |
| Integration | `[INT]` | `[INT] daemon_integration::ingest: starting` |
| E2E | `[E2E]` | `[E2E] capture_search: waiting for pane` |
| Benchmark | `[BENCH]` | `[BENCH] pattern_detection: 1.23ms/iter` |

---

## Correlation Fields

All structured logs MUST include relevant correlation fields for filtering and debugging.

### Required Fields (Always Present)

| Field | Type | Description |
|-------|------|-------------|
| `timestamp` | ISO 8601 | Time with millisecond precision |
| `level` | string | TRACE/DEBUG/INFO/WARN/ERROR |
| `target` | string | Module path (e.g., `wa_core::ingest`) |
| `message` | string | Human-readable log message |

### Context Fields (When Applicable)

| Field | Type | When to Include |
|-------|------|-----------------|
| `test_name` | string | All test contexts |
| `test_type` | string | "unit", "integration", "e2e", "bench" |
| `scenario` | string | E2E scenario name |
| `workspace` | string | Isolated workspace path |
| `pane_id` | u64 | Pane-related operations |
| `domain` | string | WezTerm domain (local, ssh, etc.) |
| `seq` | u64 | Segment sequence number |
| `rule_id` | string | Pattern detection events |
| `event_id` | string | Detection event identifier |
| `workflow_name` | string | Workflow execution |
| `execution_id` | string | Workflow run identifier |
| `action_id` | string | Audit action identifier |
| `elapsed_ms` | f64 | Duration measurements |
| `error_code` | string | Error classification |

### JSON Log Example

```json
{
  "timestamp": "2026-01-21T09:00:00.123Z",
  "level": "INFO",
  "target": "wa_core::ingest",
  "message": "Captured segment",
  "test_type": "e2e",
  "scenario": "capture_search",
  "pane_id": 123,
  "seq": 45,
  "elapsed_ms": 12.3
}
```

---

## Artifact Manifest Format

Every test run that produces artifacts MUST generate a manifest file.

### Manifest Location

```
<artifacts_dir>/manifest.json
```

### Manifest Schema (v1)

```json
{
  "version": "1",
  "format": "wa-test-manifest",
  "generated_at": "2026-01-21T09:00:00Z",
  "test_run": {
    "type": "e2e",
    "name": "capture_search",
    "status": "passed|failed|skipped",
    "duration_secs": 12.3,
    "error": null
  },
  "environment": {
    "hostname": "devbox",
    "os": "Linux 6.x x86_64",
    "rust_version": "1.85.0-nightly",
    "wa_version": "0.1.0",
    "wa_commit": "deadbeef",
    "wezterm_version": "20250101-120000-abc123",
    "workspace": "/tmp/wa-e2e-abc123"
  },
  "artifacts": [
    {
      "type": "log",
      "path": "wa_watch.log",
      "format": "text",
      "description": "Watcher stdout/stderr",
      "size_bytes": 12345,
      "redacted": true
    },
    {
      "type": "structured_log",
      "path": "wa_watch.jsonl",
      "format": "jsonl",
      "description": "JSON-lines structured logs",
      "size_bytes": 23456,
      "redacted": true
    },
    {
      "type": "state",
      "path": "robot_state.json",
      "format": "json",
      "description": "Final pane state snapshot",
      "size_bytes": 1234,
      "redacted": false
    },
    {
      "type": "events",
      "path": "events.jsonl",
      "format": "jsonl",
      "description": "Detected pattern events",
      "size_bytes": 5678,
      "redacted": false
    },
    {
      "type": "database",
      "path": "db_snapshot.sqlite",
      "format": "sqlite",
      "description": "Database copy for offline analysis",
      "size_bytes": 102400,
      "redacted": true
    },
    {
      "type": "config",
      "path": "wa_config_effective.toml",
      "format": "toml",
      "description": "Resolved configuration",
      "size_bytes": 2048,
      "redacted": true
    },
    {
      "type": "audit",
      "path": "audit_extract.jsonl",
      "format": "jsonl",
      "description": "Audit trail extract",
      "size_bytes": 4096,
      "redacted": true
    }
  ],
  "checksums": {
    "wa_watch.log": "sha256:abc123...",
    "robot_state.json": "sha256:def456..."
  }
}
```

### Artifact Types

| Type | Description | Required For |
|------|-------------|--------------|
| `log` | Plain text log output | All tests with process output |
| `structured_log` | JSON-lines log | Verbose mode |
| `state` | Final state snapshot | E2E tests |
| `events` | Detected events log | Pattern detection tests |
| `database` | SQLite snapshot | Database-related tests |
| `config` | Resolved config | All E2E tests |
| `audit` | Audit trail extract | Policy/workflow tests |
| `screenshot` | Terminal screenshot | Visual tests (future) |
| `snippet` | Code/output snippet | Failure diagnostics |
| `environment` | Environment snapshot | All test runs |

---

## Redaction Rules

All artifacts MUST be redacted before storage. Redaction protects:
- API keys and tokens
- Credentials and passwords
- Personal identifiable information (PII)
- Internal paths and hostnames (configurable)

### Redaction Patterns

These patterns are redacted by default:

| Category | Pattern | Example | Redacted Form |
|----------|---------|---------|---------------|
| OpenAI API Key | `sk-[A-Za-z0-9]{48,}` | `sk-abc123...` | `[REDACTED:openai_key]` |
| Anthropic Key | `sk-ant-[A-Za-z0-9-]+` | `sk-ant-xxx` | `[REDACTED:anthropic_key]` |
| Google API Key | `AIza[A-Za-z0-9_-]{35}` | `AIzaXXX` | `[REDACTED:google_key]` |
| Bearer Token | `Bearer [A-Za-z0-9._-]+` | `Bearer xyz` | `Bearer [REDACTED:token]` |
| Authorization | `Authorization: .*` | `Authorization: Basic xxx` | `Authorization: [REDACTED]` |
| Password | `password[=:]["']?[^\s"']+` | `password=secret` | `password=[REDACTED]` |
| Private Key | `-----BEGIN.*PRIVATE KEY-----` | PEM block | `[REDACTED:private_key]` |
| Home Path | `/home/[^/]+/` | `/home/user/` | `/home/[USER]/` |
| Temp Path | `/tmp/wa-e2e-[A-Za-z0-9]+` | `/tmp/wa-e2e-abc123` | `/tmp/wa-e2e-[TEMP]` |

### Redaction Verification

Every test artifact directory MUST include verification:

```json
{
  "redaction_applied": true,
  "redaction_version": "1.0",
  "patterns_checked": 12,
  "redactions_made": 3,
  "verified_at": "2026-01-21T09:00:05Z"
}
```

### Audit Extract Redaction

Audit extracts have additional requirements:
- `deny` decisions MUST NOT contain raw user input
- Policy reasons MUST be included but input values redacted
- Timestamps and rule IDs are preserved

Example redacted audit entry:
```json
{
  "timestamp": "2026-01-21T09:00:00Z",
  "action_kind": "send_text",
  "pane_id": 123,
  "decision": "deny",
  "reason": "alt_screen_blocked",
  "input_hash": "sha256:abc123...",
  "input_preview": "[REDACTED:42_chars]"
}
```

---

## CI Integration

### Required Artifact Checks

CI MUST fail if any of these conditions are met:

| Condition | Exit Code | Message |
|-----------|-----------|---------|
| Missing manifest.json | 1 | "Test artifacts missing manifest" |
| Manifest schema invalid | 1 | "Manifest schema validation failed" |
| Required artifact missing | 1 | "Required artifact not found: {name}" |
| Redaction verification failed | 1 | "Unredacted secrets detected in artifacts" |
| Log contains unredacted secret | 1 | "Secret pattern found in {file}" |

### CI Workflow Steps

```yaml
jobs:
  test:
    steps:
      - name: Run tests
        run: cargo test --all-features

      - name: Run E2E tests
        run: ./scripts/e2e_test.sh --verbose --keep-artifacts

      - name: Validate artifacts
        run: ./scripts/validate_artifacts.sh e2e-artifacts/

      - name: Upload artifacts on failure
        if: failure()
        uses: actions/upload-artifact@v4
        with:
          name: test-artifacts
          path: e2e-artifacts/
          retention-days: 7
```

### Artifact Validation Script

The validation script (`scripts/validate_artifacts.sh`) MUST check:

1. Manifest exists and is valid JSON
2. All listed artifacts exist
3. Checksums match (if provided)
4. Redaction verification passed
5. No secret patterns in plain text files

---

## Test Output Contract

### Unit Test Output

Unit tests output minimal structured results:

```
running 10 tests
[UNIT] policy::deny_alt_screen ... ok
[UNIT] policy::allow_staging ... ok
[UNIT] patterns::codex_compaction ... ok
...
test result: ok. 10 passed; 0 failed; 0 ignored
```

### Integration Test Output

Integration tests include timing and correlation:

```
[INT] daemon_integration::ingest_pipeline
  [DEBUG] workspace=/tmp/wa-test-xyz123
  [INFO] Starting watcher...
  [INFO] Ingested 100 segments in 1.23s
  [INFO] PASS (1.45s)
```

### E2E Test Output

E2E tests follow the harness spec with full artifacts:

```
E2E Test Run: 2026-01-21T09:00:00Z
================================

[E2E] Self-check passed
[E2E] Running 3 scenarios...

[E2E] Scenario 1/3: capture_search
  [INFO] Spawning dummy pane...
  [INFO] Starting wa watch...
  [INFO] Captured 100 segments
  [INFO] Search found 100 hits
  [PASS] capture_search (7.2s)

[E2E] Scenario 2/3: compaction_workflow
  ...

Summary: 3 passed, 0 failed, 0 skipped
Artifacts: ./e2e-artifacts/2026-01-21T09-00-00Z/
```

---

## Failure Diagnostics

### Required Information on Failure

When a test fails, these MUST be captured:

1. **Error context**: Full error message with code
2. **Stack trace**: If available (panics)
3. **Recent logs**: Last 50 lines before failure
4. **State snapshot**: Current system state
5. **Correlation IDs**: All relevant IDs for tracing

### Failure Artifact Structure

```
scenario_XX_failed/
├── FAIL                      # Marker file
├── error.json                # Structured error info
├── error.txt                 # Human-readable error
├── wa_watch.log              # Full log
├── wa_watch.jsonl            # Structured log
├── recent_logs.txt           # Last 50 lines
├── robot_state.json          # State at failure
├── events.jsonl              # Events up to failure
├── db_snapshot.sqlite        # Database state
└── hints.txt                 # Debugging suggestions
```

### error.json Schema

```json
{
  "error_code": "E2E_TIMEOUT",
  "message": "Timeout waiting for workflow completion",
  "timestamp": "2026-01-21T09:00:20Z",
  "duration_before_failure_secs": 20.1,
  "scenario": "compaction_workflow",
  "phase": "workflow_execution",
  "correlation": {
    "pane_id": 123,
    "workflow_name": "handle_compaction",
    "execution_id": "exec-abc123"
  },
  "hints": [
    "Check wa_watch.log for workflow errors",
    "Verify pattern detection triggered (events.jsonl)",
    "Confirm dummy pane emitted compaction marker"
  ]
}
```

---

## Implementation Checklist

For a test to comply with this contract:

- [ ] Uses appropriate log level (ERROR/WARN/INFO/DEBUG/TRACE)
- [ ] Includes test type prefix in console output
- [ ] Emits correlation fields in structured logs
- [ ] Generates manifest.json in artifact directory
- [ ] Lists all artifacts in manifest
- [ ] Applies redaction to all artifacts
- [ ] Includes redaction verification
- [ ] Captures required failure diagnostics
- [ ] Provides debugging hints on failure

---

## Related Documents

- **[E2E Harness Spec](e2e-harness-spec.md)** - E2E test harness specification
- **[wa-core/src/logging.rs](../crates/wa-core/src/logging.rs)** - Logging implementation
- **[wa-core/src/policy.rs](../crates/wa-core/src/policy.rs)** - Redactor implementation

---

## Version History

| Version | Date | Changes |
|---------|------|---------|
| 1.0 | 2026-01-21 | Initial contract (bd-u194) |
