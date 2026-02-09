# Command Handoff State-Machine Traces and Failure-Path Catalog

**Bead:** wa-bjvg (FTUI-06.2.a)
**Date:** 2026-02-09
**Parent:** wa-fbzn (FTUI-06.2 Migrate command execution handoff)

## 1  State Machine

```
                ┌────────────────────────────────────────────────┐
                │                                                │
  ┌──────┐ enter  ┌────────┐ suspend  ┌───────────┐ resume  ┌────────┐
  │ Idle │──────>│ Active  │────────>│ Suspended  │───────>│ Active  │
  └──────┘       └────┬───┘         └──────┬─────┘        └────────┘
                      │                     │
                      │ leave()             │ leave() (emergency)
                      ▼                     ▼
                   ┌──────┐             ┌──────┐
                   │ Idle │             │ Idle │
                   └──────┘             └──────┘
```

### Gate Phase Mapping

| Session Phase | Gate Phase | Terminal Ownership | stdout/stderr |
|--------------|------------|-------------------|---------------|
| Idle | Inactive | No owner | Safe to write |
| Active | Active | TUI rendering pipeline | Forbidden |
| Suspended | Suspended | Subprocess / operator | Safe to write |

## 2  Nominal Traces

### T1: Single Command Handoff

```
Session: Active    Gate: Active      — TUI rendering
  │ suspend()
Session: Suspended Gate: Suspended   — terminal released
  │ spawn(cmd)
  │ wait(cmd)
  │ wait_for_enter()
Session: Suspended Gate: Suspended   — command finished, operator confirmed
  │ resume()
Session: Active    Gate: Active      — TUI reclaims terminal
```

**Test:** `trace_nominal_suspend_resume`

### T2: Full Lifecycle with Handoff

```
Session: Idle      Gate: Inactive
  │ enter(AltScreen)
Session: Active    Gate: Active
  │ suspend()
Session: Suspended Gate: Suspended
  │ [command runs]
  │ resume()
Session: Active    Gate: Active
  │ leave()
Session: Idle      Gate: Inactive
```

**Test:** `trace_nominal_full_lifecycle`

### T3: Multiple Sequential Handoffs

```
Session: Active Gate: Active
  │ [5 iterations of:]
  │   suspend() → Suspended/Suspended
  │   [command runs]
  │   resume() → Active/Active
Session: Active Gate: Active
  │ leave()
Session: Idle   Gate: Inactive
```

**Test:** `trace_multiple_handoffs_sequential`

## 3  Failure Traces

### F1: Suspend from Wrong Phase (Idle)

```
Session: Idle Gate: Inactive
  │ execute("echo hello")
  │   → phase check: Idle ≠ Active
  │   → Err(SuspendFailed(InvalidPhase))
Session: Idle Gate: Inactive  ← no state change
```

**Test:** `trace_fail_suspend_from_idle`

### F2: Double Suspend

```
Session: Active    Gate: Active
  │ suspend()
Session: Suspended Gate: Suspended
  │ suspend()
  │   → phase check: Suspended ≠ Active
  │   → Err(InvalidPhase)
Session: Suspended Gate: Suspended  ← stays Suspended, not corrupted
```

**Test:** `trace_fail_double_suspend`

### F3: Resume from Active (Double Resume)

```
Session: Active Gate: Active
  │ resume()
  │   → phase check: Active ≠ Suspended
  │   → Err(InvalidPhase)
Session: Active Gate: Active  ← stays Active, not corrupted
```

**Test:** `trace_fail_double_resume`

### F4: Emergency Leave from Suspended

If `resume()` fails (e.g., terminal disconnected), the caller can `leave()`:

```
Session: Suspended Gate: Suspended
  │ resume() → Err(...)
  │ leave()  ← emergency cleanup
Session: Idle      Gate: Inactive   ← terminal released
```

**Test:** `trace_leave_from_suspended_emergency`

### F5: Empty Command (No Phase Transition)

```
Session: Active Gate: Active
  │ execute("")
  │   → Err(EmptyCommand)
Session: Active Gate: Active  ← no suspension occurred
```

**Test:** `trace_fail_empty_command_preserves_phase`

### F6: Panic During Handoff

```
Session: Suspended Gate: Suspended
  │ [command panics or wa panics]
  │ SessionGuard::Drop runs
  │   → session.leave()
  │   → gate = Inactive
Session: Idle Gate: Inactive  ← RAII cleanup
```

**Tests:** `guard_drop_cleans_up_after_panic_in_suspended_state` (terminal_session.rs),
`harness_panic_during_suspended_phase` (terminal_session.rs)

### F7: Resume Failure After Successful Command

```
Session: Suspended Gate: Suspended
  │ [command ran successfully]
  │ resume() → Err(ResumeFailed)
  │ → caller receives HandoffError::ResumeFailed
  │ → session stays Suspended (recoverable)
  │ → caller can retry resume() or call leave()
```

**Invariant:** Command results are not lost even if resume fails.

## 4  Terminal Ownership Invariants

| ID | Invariant | Enforcement | Test |
|----|-----------|-------------|------|
| I1 | Screen mode preserved across suspend/resume | MockSession tracks mode through lifecycle | `invariant_screen_mode_preserved_across_handoff` |
| I2 | No draw during Suspended | `draw()` requires Active phase | `invariant_no_draw_during_suspended` |
| I3 | No poll during Suspended | `poll_event()` requires Active phase | `invariant_no_poll_during_suspended` |
| I4 | Raw mode disabled during Suspended | CrosstermSession::suspend() calls disable_raw_mode | PTY E2E (FTUI-07.3) |
| I5 | Alt screen left during Suspended | CrosstermSession::suspend() sends LeaveAlternateScreen | PTY E2E (FTUI-07.3) |
| I6 | Cursor visible during Suspended | Subprocess inherits visible cursor | CrosstermSession impl |
| I7 | Gate = Inactive after leave() | SessionGuard::Drop sets gate | teardown harness (wa-3fed) |
| I8 | leave() is idempotent | No-op if already Idle | `teardown_idempotency_leave_after_leave` |

## 5  Diagnostic Logging Schema

### 5.1  Structured Log Events

All handoff events use the `wa::handoff` tracing target for filterable diagnostics.

| Event | Level | Fields | When |
|-------|-------|--------|------|
| `handoff.start` | INFO | `command`, `session_phase`, `screen_mode` | Before suspend() |
| `handoff.suspended` | DEBUG | `elapsed_ms` | After successful suspend() |
| `handoff.cmd.spawn` | INFO | `program`, `args[]` | Before Command::new().status() |
| `handoff.cmd.exit` | INFO | `exit_code`, `elapsed_ms` | After command completes |
| `handoff.cmd.launch_failed` | WARN | `error`, `program` | Command failed to spawn |
| `handoff.resumed` | DEBUG | `elapsed_ms` | After successful resume() |
| `handoff.complete` | INFO | `success`, `total_elapsed_ms` | After full cycle |
| `handoff.error.suspend` | ERROR | `error`, `session_phase` | suspend() failed |
| `handoff.error.resume` | ERROR | `error`, `session_phase` | resume() failed |
| `handoff.error.empty_cmd` | WARN | | Empty command rejected |

### 5.2  Example Log Output

```
INFO  wa::handoff handoff.start command="git status" session_phase=Active screen_mode=AltScreen
DEBUG wa::handoff handoff.suspended elapsed_ms=2
INFO  wa::handoff handoff.cmd.spawn program="git" args=["status"]
INFO  wa::handoff handoff.cmd.exit exit_code=0 elapsed_ms=145
DEBUG wa::handoff handoff.resumed elapsed_ms=1
INFO  wa::handoff handoff.complete success=true total_elapsed_ms=4523
```

### 5.3  Failure Isolation

The diagnostic schema separates three failure domains:

| Domain | Log Target | Example |
|--------|-----------|---------|
| Session lifecycle | `wa::handoff handoff.error.*` | suspend/resume phase violation |
| Command execution | `wa::handoff handoff.cmd.*` | spawn failure, non-zero exit |
| Terminal state | `wa::session` (terminal_session.rs) | raw mode, alt screen errors |

This separation allows fast triage: "was this a TUI lifecycle bug or did the
subprocess fail?" is answerable from log target alone.

## 6  Test Coverage Summary

### New Tests in command_handoff.rs (FTUI-06.2.a)

| Test | Category | Path |
|------|----------|------|
| `trace_nominal_suspend_resume` | Trace T1 | Happy path |
| `trace_nominal_full_lifecycle` | Trace T2 | Full enter→handoff→leave |
| `trace_multiple_handoffs_sequential` | Trace T3 | 5 sequential handoffs |
| `trace_fail_suspend_from_idle` | Trace F1 | Invalid starting phase |
| `trace_fail_double_suspend` | Trace F2 | Duplicate suspend |
| `trace_fail_double_resume` | Trace F3 | Duplicate resume |
| `trace_leave_from_suspended_emergency` | Trace F4 | Emergency cleanup |
| `trace_fail_empty_command_preserves_phase` | Trace F5 | No phase change on bad input |
| `invariant_screen_mode_preserved_across_handoff` | Invariant I1 | Mode stability |
| `invariant_no_draw_during_suspended` | Invariant I2 | Phase guard |
| `invariant_no_poll_during_suspended` | Invariant I3 | Phase guard |
| `invariant_command_result_launch_failure` | Invariant I4 | Result fields |
| `invariant_handoff_error_variants` | Invariant I5 | Error discrimination |

### Pre-Existing Tests

| Test | Coverage |
|------|----------|
| `empty_command_returns_error` | EmptyCommand path |
| `whitespace_only_command_returns_error` | Trimmed empty path |
| `suspend_from_idle_fails` | SuspendFailed path |
| `handoff_suspends_and_resumes` | Basic lifecycle |
| `command_result_success_check` | Result API |
| `handoff_error_display` | Error formatting |

### Cross-Module Coverage (terminal_session.rs)

| Test | Handoff Relevance |
|------|-------------------|
| `guard_drop_cleans_up_after_panic_in_suspended_state` | F6: panic during handoff |
| `harness_panic_during_suspended_phase` | F6: systematic panic harness |
| `lifecycle_stress_suspend_resume_cycles` | T3 stress variant |

## References

- `crates/wa-core/src/tui/command_handoff.rs` — handoff implementation + tests
- `crates/wa-core/src/tui/terminal_session.rs` — session lifecycle + teardown harness
- `crates/wa-core/src/tui/output_gate.rs` — gate phase management
- `docs/ftui-teardown-harness.md` — teardown invariants (wa-3fed)
- `docs/ftui-subprocess-forwarding-contract.md` — PTY capture (wa-3gsu)
