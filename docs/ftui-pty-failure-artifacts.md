# PTY Failure Artifact Schema and Triage Playbook

**Bead:** wa-308u (FTUI-07.3.b)
**Date:** 2026-02-09
**Parent:** wa-3gii (FTUI-07.3 Build PTY E2E scenario pack)
**Depends on:** wa-1qr1 (FTUI-07.3.a — PTY fixture seed/timing strategy)
**Blocks:** wa-3mus (FTUI-08.3.a — compatibility runbook template)

## 1  Artifact Schema

### 1.1  Artifact Bundle Structure

Every PTY E2E test failure produces a bundle under
`target/pty-e2e-artifacts/{scenario}_{seed}/`:

```
target/pty-e2e-artifacts/
  basic_navigation_0xCAFE0001/
    meta.json           # Scenario metadata
    transcript.jsonl    # Timestamped I/O log
    screenshot.txt      # Terminal text at failure point
    input.json          # Input sequence sent
    diag.txt            # Model diagnostics
    env.json            # Environment variables
```

Artifacts are written ONLY on failure. Passing tests produce no artifacts.

### 1.2  meta.json — Scenario Metadata

```json
{
  "schema_version": 1,
  "scenario": "basic_navigation",
  "seed": "0xCAFE0001",
  "terminal": {
    "cols": 80,
    "rows": 24,
    "term_type": "xterm-256color"
  },
  "timing": {
    "started_at": "2026-02-09T12:00:00.000Z",
    "failed_at": "2026-02-09T12:00:03.142Z",
    "elapsed_ms": 3142,
    "timeout_factor": 1.0
  },
  "failure": {
    "step_index": 5,
    "step_type": "AssertView",
    "expected": "Events",
    "actual": "Home",
    "message": "expected view Events, got Home"
  },
  "build": {
    "features": ["ftui"],
    "profile": "test",
    "target": "x86_64-unknown-linux-gnu"
  }
}
```

### 1.3  transcript.jsonl — Timestamped I/O

One JSON object per line, ordered by timestamp:

```jsonl
{"t":0,"dir":"out","len":1024,"data":"G1s/MTA0OWgbWzE7MUg=..."}
{"t":15,"dir":"in","len":1,"data":"CQ==","key":"Tab"}
{"t":22,"dir":"out","len":512,"data":"G1sxOzFIIEhvbWUg..."}
{"t":35,"dir":"in","len":1,"data":"CQ==","key":"Tab"}
{"t":42,"dir":"out","len":480,"data":"..."}
```

Fields:

| Field | Type | Description |
|-------|------|-------------|
| `t` | u64 | Milliseconds since scenario start (monotonic) |
| `dir` | "in" \| "out" | Direction: input to PTY or output from PTY |
| `len` | u64 | Byte count (before base64) |
| `data` | string | Base64-encoded raw bytes |
| `key` | string? | Optional human-readable key name (input only) |

Size limit: transcript is truncated at 1 MiB. If truncated, the last line is:
```jsonl
{"t":999,"truncated":true,"total_bytes":1234567}
```

### 1.4  screenshot.txt — Terminal State at Failure

Plain text rendering of the terminal at the moment of assertion failure.
Captured by reading the PTY output buffer and stripping ANSI escape sequences:

```
┌──────────────────────────────────────────────────┐
│ [Home] Panes  Events  Triage  History  Search    │
│                                                   │
│ WezTerm Automata - Home                          │
│ ────────────────────────────────────────────────  │
│ System: healthy   Panes: 3   Events: 6           │
│                                                   │
│ Press Tab to switch views, q to quit              │
│                                                   │
│                                                   │
│ [Home] q:Quit Tab:Next ↑↓:Navigate               │
└──────────────────────────────────────────────────┘
```

### 1.5  input.json — Input Sequence

```json
{
  "steps": [
    {"index": 0, "type": "WaitForStable", "timeout_ms": 5000, "result": "ok", "elapsed_ms": 342},
    {"index": 1, "type": "PressKey", "key": "Tab", "bytes": "CQ==", "at_ms": 342},
    {"index": 2, "type": "WaitForOutput", "pattern": "Panes", "timeout_ms": 500, "result": "ok", "elapsed_ms": 28},
    {"index": 3, "type": "AssertView", "expected": "Panes", "result": "ok"},
    {"index": 4, "type": "PressKey", "key": "Tab", "bytes": "CQ==", "at_ms": 385},
    {"index": 5, "type": "AssertView", "expected": "Events", "result": "FAIL", "actual": "Home"}
  ]
}
```

### 1.6  env.json — Environment

```json
{
  "TERM": "xterm-256color",
  "LANG": "C.UTF-8",
  "NO_COLOR": "1",
  "WA_DB_PATH": ":memory:",
  "WA_PTY_TIMEOUT_FACTOR": "1.0",
  "uname": "Linux 6.17.0-8-generic x86_64",
  "rustc": "rustc 1.87.0-nightly"
}
```

### 1.7  diag.txt — Model Diagnostics

Internal model state dump (from `E2eSession::diagnostic_dump()` or equivalent):

```
=== PTY E2E Diagnostic Dump ===
Scenario: basic_navigation (seed=0xCAFE0001)
Step: 5 of 12
Current view: Home
Filter state: (none)
Selected index: 0
Data: 3 panes, 6 events, 1 triage item
Last refresh: 342ms ago
Output gate: Inactive
Session phase: Active
Frame count: 4
```

## 2  CI Failure Summary Format

### 2.1  Console Output

On failure, the test runner prints a concise summary:

```
FAIL pty_e2e::basic_navigation

  Scenario: basic_navigation (seed=0xCAFE0001, 80x24)
  Failed at step 5: AssertView
    expected: Events
    actual:   Home
  Elapsed: 3.1s

  Artifacts: target/pty-e2e-artifacts/basic_navigation_0xCAFE0001/
    meta.json, transcript.jsonl, screenshot.txt, input.json, env.json, diag.txt

  Reproduce:
    RUST_TEST_THREADS=1 cargo test -p wa-core --features ftui -- pty_e2e::basic_navigation
```

### 2.2  CI Job Summary

GitHub Actions job summary (markdown):

```markdown
## PTY E2E Results

| Scenario | Seed | Result | Step | Elapsed |
|----------|------|--------|------|---------|
| startup_and_quit | 0x0001 | PASS | - | 1.2s |
| basic_navigation | 0xCAFE0001 | **FAIL** | 5/12 | 3.1s |
| events_filter | 0x0003 | PASS | - | 2.8s |

**1 failure** — artifacts uploaded to `pty-e2e-failures`.
```

## 3  Triage Playbook

### 3.1  Severity Classification

| Severity | Criteria | Response |
|----------|----------|----------|
| **S1 — Crash** | Process exited with signal/panic | Block release; fix immediately |
| **S2 — Terminal corruption** | Raw mode or alt-screen not restored | Block release; fix immediately |
| **S3 — Wrong output** | Assertion failed but terminal is usable | Fix before next milestone |
| **S4 — Timing flake** | Passes on retry, fails intermittently | Add sync point; increase timeout |
| **S5 — Cosmetic** | Minor rendering difference | Track; fix when convenient |

### 3.2  Step-by-Step Triage

#### Step 1: Read the failure summary

From CI output or `meta.json`:
- Which scenario failed?
- At which step?
- What was expected vs actual?

#### Step 2: Check screenshot.txt

- Is the terminal in a recognizable state?
- Is the right view displayed?
- Are there rendering artifacts (garbled text, misaligned columns)?

If the screenshot shows a blank or corrupted terminal → **S1 or S2**.

#### Step 3: Replay the transcript

```bash
# Convert transcript to raw bytes for replay
jq -r 'select(.dir=="out") | .data' transcript.jsonl \
  | base64 -d > replay.raw

# View in a terminal (raw bytes)
cat replay.raw
```

Look for:
- Missing escape sequences (view not switching)
- Extra escape sequences (unexpected terminal state changes)
- Timing gaps (long pauses between output)

#### Step 4: Check input.json

- Were all inputs sent correctly?
- Did any `WaitForOutput` step timeout?
- Is there a missing synchronization point?

If a `WaitForOutput` timed out → likely **S4** (timing) or **S3** (output wrong).

#### Step 5: Compare env.json

- Is `TERM` correct?
- Is `LANG` set to `C.UTF-8`?
- Is this a different OS/architecture than passing runs?

If env differs from passing runs → **environment-specific** issue.

#### Step 6: Check diag.txt

- Is the model in the expected state?
- Is the data loaded (pane count, event count)?
- Is the output gate in the right phase?

If model state is wrong → **logic bug** (S3).
If output gate is Active when it shouldn't be → **lifecycle bug** (S2).

#### Step 7: Reproduce locally

```bash
RUST_TEST_THREADS=1 cargo test -p wa-core --features ftui -- pty_e2e::basic_navigation
```

If it passes locally → **flake** (S4). Check:
- CI timeout factor (`WA_PTY_TIMEOUT_FACTOR`)
- CI machine load
- Add explicit synchronization at the failing step

#### Step 8: Root-cause classification

| Root Cause | Category | Fix Strategy |
|-----------|----------|--------------|
| Missing sync point | Timing | Add `WaitForOutput` or `WaitForStable` |
| Race between input and render | Timing | Increase stability window |
| Terminal escape sequence mismatch | Compat | Add terminal-specific handling |
| Model state machine bug | Logic | Fix in model + add headless test |
| Output gate stuck | Lifecycle | Fix in session lifecycle |
| PTY buffer overflow | Backpressure | Increase read buffer or add drain |

### 3.3  Escalation Criteria

| Condition | Action |
|-----------|--------|
| S1 or S2 on main branch | Block merges; assign fix immediately |
| S3 on main, reproduces locally | Create bead, assign to next sprint |
| S4 on CI only | Increase timeout factor; add sync point |
| Same scenario fails 3+ times in a week | Escalate to S3 regardless of intermittency |
| New failure after merge | Bisect with `git bisect run` + PTY test |

## 4  Artifact Retention Policy

| Context | Retention | Storage |
|---------|-----------|---------|
| CI failure artifacts | 7 days | GitHub Actions artifact |
| Local failure artifacts | Until manually deleted | `target/pty-e2e-artifacts/` |
| Passing test runs | No artifacts | - |
| Flake investigation | 30 days | Attached to investigation bead |

## References

- `docs/ftui-pty-fixture-strategy.md` — seed/timing strategy (wa-1qr1)
- `docs/ftui-teardown-harness.md` — restoration invariants (wa-3fed)
- `docs/ftui-command-handoff-traces.md` — handoff state machine (wa-bjvg)
- `crates/wa-core/src/crash.rs` — crash bundle format (existing)
- `crates/wa-core/src/tui/ftui_stub.rs:6432-7045` — headless E2E tests
