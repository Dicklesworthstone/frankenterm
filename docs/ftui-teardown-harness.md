# Panic/Abort Teardown Harness and Failure Artifact Requirements

**Bead:** wa-3fed (FTUI-03.4.a)
**Date:** 2026-02-09
**Parent:** wa-1p3f (FTUI-03.4 Panic-safe cleanup and lifecycle stress validation)

## 1  Restoration Invariants

After any abrupt exit (panic, signal, Drop during unwind), the following
invariants must hold:

| Invariant | Verification | Module |
|-----------|-------------|--------|
| `SessionPhase` is `Idle` | `session.phase() == SessionPhase::Idle` | terminal_session.rs |
| `ScreenMode` is `None` | `session.screen_mode().is_none()` | terminal_session.rs |
| Output gate is `Inactive` | `output_gate::phase() == GatePhase::Inactive` | output_gate.rs |
| Output not suppressed | `!output_gate::is_output_suppressed()` | output_gate.rs |
| Raw mode disabled | `crossterm::terminal::is_raw_mode_enabled() == false` | CrosstermSession only |
| Alternate screen left | Terminal shows normal buffer | CrosstermSession only |
| Cursor visible | `terminal.show_cursor()` called | CrosstermSession only |

The first four invariants are testable in unit tests via `MockTerminalSession`.
The last three require a real terminal (CrosstermSession) and are verified via
PTY E2E tests (FTUI-07.3).

## 2  Enforcement Mechanisms

### 2.1  SessionGuard RAII

`SessionGuard<S>::Drop` calls `session.leave()` and resets gate to `Inactive`.
This runs during panic unwind, guaranteeing cleanup even on abort.

```
impl<S: TerminalSession> Drop for SessionGuard<S> {
    fn drop(&mut self) {
        if let Some(session) = &mut self.session {
            session.leave();
        }
        output_gate::set_phase(GatePhase::Inactive);
    }
}
```

### 2.2  Idempotent Leave

`leave()` is a no-op when already `Idle`. Safe to call multiple times:
- Guard Drop after `into_inner()` (session already taken)
- Multiple leave calls in error recovery
- Nested cleanup paths

### 2.3  Error Suppression in Cleanup

All CrosstermSession cleanup operations use `let _ =` to suppress errors:
- `disable_raw_mode()` — may fail if already disabled
- `LeaveAlternateScreen` — may fail if already left
- `show_cursor()` — may fail if terminal disconnected

This prevents panic-during-panic (double fault) which would abort the process.

## 3  Teardown Harness Test Matrix

Located in `crates/wa-core/src/tui/terminal_session.rs`, `tests` module.

### 3.1  Phase-Specific Abort Tests

| Test | Abort Point | Mode | Validates |
|------|------------|------|-----------|
| `harness_panic_during_active_alt_screen` | Active phase | AltScreen | Gate, phase, mode restored |
| `harness_panic_during_active_inline` | Active phase | Inline(12) | Gate, phase, mode restored |
| `harness_panic_during_suspended_phase` | Suspended phase | AltScreen | Gate restored from Suspended |
| `harness_panic_during_draw` | After draw calls | Default | Gate, phase restored mid-render |
| `harness_panic_during_poll` | After poll_event | Default | Gate, phase restored mid-poll |
| `harness_panic_after_multiple_suspend_resume_cycles` | After 5 cycles | AltScreen | No state leak after cycling |

### 3.2  Edge Case Tests

| Test | Scenario | Validates |
|------|----------|-----------|
| `harness_into_inner_then_drop_no_double_cleanup` | into_inner + drop | No double leave, gate clean |
| `harness_leave_restores_all_screen_modes` | Each ScreenMode variant | Mode cleared for every variant |
| `harness_panic_message_preserved_in_catch` | catch_unwind payload | Panic message accessible for crash bundle |
| `harness_sequential_panics_no_state_leak` | 10 sequential panics | Each panic leaves clean state |
| `harness_gate_phase_correct_at_each_lifecycle_point` | Full lifecycle | Gate correct at enter/draw/leave |

### 3.3  Pre-Existing Tests (FTUI-03.4 section)

| Test | Coverage |
|------|----------|
| `guard_drop_cleans_up_after_caught_panic` | Basic panic during Active |
| `guard_drop_cleans_up_after_panic_in_suspended_state` | Panic during Suspended |
| `teardown_idempotency_leave_after_leave` | Triple leave is no-op |
| `teardown_idempotency_drop_after_into_inner` | Drop after session taken |
| `lifecycle_stress_repeated_enter_leave_cycles` | 50 rapid start/stop cycles |
| `lifecycle_stress_suspend_resume_cycles` | 20 suspend/resume cycles |
| `guard_drop_after_panic_in_draw_callback` | Panic after draw |

## 4  Failure Artifact Capture Requirements

When wa crashes (panic or signal), the following artifacts must be produced
for post-mortem analysis without rerunning interactively.

### 4.1  Crash Bundle (existing — crash.rs)

Written to `.wa/crash/wa_crash_YYYYMMDD_HHMMSS/`:

| File | Content | Redacted |
|------|---------|----------|
| `manifest.json` | Bundle metadata (timestamp, pid, version) | No (metadata only) |
| `crash_report.json` | Panic message, location (file:line:col), backtrace | Yes |
| `health_snapshot.json` | Last known health state (pane statuses, queue depths) | Yes |

**Size limits:** Backtrace capped at 64 KiB, total bundle at 1 MiB.
**Permissions:** 0o600 (owner-only read/write).
**Atomicity:** Temp directory + rename (all-or-nothing on POSIX).

### 4.2  Environment Markers (required — not yet implemented)

The crash bundle should additionally capture:

| Marker | Source | Purpose |
|--------|--------|---------|
| `gate_phase` | `output_gate::phase()` | Was TUI active at crash time? |
| `session_phase` | `TerminalSession::phase()` | Which lifecycle state was active? |
| `screen_mode` | `TerminalSession::screen_mode()` | AltScreen vs Inline at crash? |
| `feature_flags` | Compile-time cfg | tui vs ftui vs headless? |
| `terminal_type` | `$TERM` / `$TERM_PROGRAM` | Terminal emulator identification |
| `backpressure_tier` | Last known tier | Was system under load? |

These markers allow fast triage: "crash during Active+AltScreen in tmux" vs
"crash during Suspended in WezTerm" lead to very different investigations.

### 4.3  Recording Checkpoint (existing — crash.rs)

`CaptureCheckpoint` persists per-pane capture state so restart can resume
without segment duplication:

| Field | Type | Purpose |
|-------|------|---------|
| `pane_id` | String | Which pane was being captured |
| `last_seq` | u64 | Last committed segment sequence |
| `cursor_offset` | u64 | Byte offset in recording file |
| `last_capture_at` | u64 | Epoch timestamp of last capture |

### 4.4  Transcript Preservation

If a recording session was active at crash time:
- The recording file is flushed (FrameWriter uses buffered writes)
- A gap marker is inserted at the crash point (IS_GAP flag)
- The checkpoint file records the last good position
- Replay can show output up to the crash point and indicate the gap

### 4.5  CI Regression Detection

Crash-related regressions are caught by:

| Check | How | Where |
|-------|-----|-------|
| Teardown harness tests | `cargo test -p wa-core terminal_session` | CI unit tests |
| Output gate tests | `cargo test -p wa-core output_gate` | CI unit tests |
| Crash bundle tests | `cargo test -p wa-core crash` | CI unit tests |
| PTY E2E crash scenarios | FTUI-07.3 scenario pack | CI E2E tests |
| Debug assertions | `debug_assert!` in TerminalSink | Debug builds only |

## 5  Deterministic Reproduction

The teardown harness achieves deterministic reproduction by:

1. **Serialized gate access:** All gate-touching tests acquire `GATE_TEST_LOCK`
2. **Reset before test:** Gate set to `Inactive` at test start
3. **Panic via catch_unwind:** No process abort; test runner continues
4. **Invariant checks after unwind:** All assertions run after panic is caught
5. **No real terminal:** MockTerminalSession avoids flaky terminal interactions

For real-terminal abort scenarios (SIGTERM, SIGKILL, power loss), PTY E2E
tests use a child process that is killed externally, and the parent verifies
terminal state recovery.

## References

- `crates/wa-core/src/tui/terminal_session.rs` — SessionGuard, MockTerminalSession, teardown tests
- `crates/wa-core/src/tui/output_gate.rs` — GatePhase, is_output_suppressed
- `crates/wa-core/src/crash.rs` — panic hook, crash bundle, checkpoint
- `docs/ftui-output-sink-routing.md` — output sink routing contract (wa-1uqi)
- `docs/ftui-subprocess-forwarding-contract.md` — subprocess forwarding (wa-3gsu)
