# ADR-0010: One-Writer Rule Adaptation and Terminal Ownership Rationale

**Status:** Accepted
**Date:** 2026-02-09
**Context:** FTUI-01.1.a (wa-2q5r), extends ADR-0002

## Decision

Map the one-writer rule (ADR-0002) to concrete wa modules, process boundaries,
and lifecycle phases.  Define an enforceable write-path matrix with per-phase
allowed/forbidden paths, negative examples, and validation evidence hooks.

## Module-to-Principle Mapping

| ADR-0002 Rule | wa Module | Enforcement Mechanism |
|---|---|---|
| R1 Exclusive write ownership | `output_gate.rs` `OutputGate` | Atomic phase gate; `TuiAwareWriter` checks before stderr writes |
| R2 Log routing | `output_gate.rs` `TuiAwareWriter` | tracing subscriber wraps stderr with gate check |
| R3 Command handoff | `command_handoff.rs` `execute()` | `TerminalSession::suspend()`/`resume()` lifecycle |
| R4 Inline-mode | Not yet implemented | Future: ftui inline Program variant |
| R5 Panic safety | `terminal_session.rs` `SessionGuard` | RAII drop calls `leave()`; `crash.rs` checks gate before panic output |

### Process Boundary: Who Owns What

```
  TerminalSession (owns lifecycle)
       │
       ├── OutputGate (process-global AtomicU8)
       │     Inactive ─► Active ─► Suspended ─► Active ─► Inactive
       │
       ├── SessionGuard (RAII, owns TerminalSession)
       │     enter() sets gate=Active
       │     drop()  sets gate=Inactive
       │
       └── command_handoff::execute()
             suspend() sets gate=Suspended
             resume()  sets gate=Active
```

## Write-Path Decision Table

### Phase: Active (TUI rendering in progress)

| Writer | Allowed? | Rationale | Negative Example |
|---|---|---|---|
| ftui Frame buffer → terminal | Yes | Owner of terminal writes | - |
| `tracing::info!()` → stderr | **No** | Would corrupt display | `tracing::info!("event fired")` during render |
| `println!()` / `eprintln!()` | **No** | Raw stdout/stderr bypasses render pipeline | `eprintln!("debug: {}", val)` in event handler |
| `std::process::Command` | **No** | Subprocess inherits stdio, overwrites screen | Spawning `ls` without suspending first |
| `crash.rs` panic hook → stderr | Conditional | Only after `SessionGuard::leave()` restores terminal | Panic hook writing before `leave()` called |

### Phase: Suspended (command handoff in progress)

| Writer | Allowed? | Rationale | Negative Example |
|---|---|---|---|
| ftui Frame buffer → terminal | **No** | Ownership released; terminal belongs to subprocess | Calling `session.draw()` while suspended |
| `tracing::info!()` → stderr | Yes | Gate is Suspended, TuiAwareWriter permits | - |
| `println!()` / `eprintln!()` | Yes | Terminal is in normal mode, safe for line output | - |
| Subprocess stdout/stderr | Yes | This is the entire point of suspension | - |
| `session.resume()` | Yes | Reclaims ownership; transitions to Active | - |

### Phase: Inactive (no TUI, CLI/robot mode)

| Writer | Allowed? | Rationale | Negative Example |
|---|---|---|---|
| Any stdout/stderr write | Yes | No TUI to conflict with | - |
| ftui operations | **No** | Session not entered; `draw()` returns error | Calling `session.draw()` before `enter()` |

## Risk Notes

### Panic Path

**Risk:** Panic during Active phase leaves terminal in raw mode, alt-screen, with
cursor hidden.

**Mitigation chain:**
1. `std::panic::set_hook` installed by `crash::install_panic_hook()`
2. Panic hook writes crash bundle to disk (size-bounded, redacted)
3. `SessionGuard` drop runs `leave()` which restores terminal state
4. Output gate transitions to Inactive
5. Panic hook output (if any) goes to restored stderr

**Gap:** With `panic = "abort"` in release, `SessionGuard::drop()` runs in the panic
hook's scope, not during unwinding.  The current hook must explicitly call
`leave()` before writing to stderr.  Evidence: unit tests in `terminal_session.rs`
verify that `MockTerminalSession` records `leave()` on drop.

**Validation:** `#[test] fn session_guard_calls_leave_on_drop()` in terminal_session.rs.

### Subprocess Path

**Risk:** Command handoff fails to resume, leaving TUI in Suspended state with the
user stuck at a raw terminal.

**Mitigation chain:**
1. `command_handoff::execute()` calls `suspend()` before running command
2. After command exits, waits for Enter keypress, then calls `resume()`
3. If `resume()` fails, returns `HandoffError::ResumeFailed` — caller must handle
4. `SessionGuard` drop ensures `leave()` if resume never succeeds

**Negative example:** Calling `session.draw()` while Suspended returns
`SessionError::InvalidPhase`.  This is enforced by the phase state machine in
`TerminalSession`.

**Validation:** `command_handoff.rs` tests verify suspend/resume lifecycle including
error paths.

### Inline Mode (Future)

**Risk:** Inline mode scopes ownership to a render region, not the full terminal.
Output above the region may interleave with scrollback from other processes.

**Mitigation (planned):**
1. ftui's inline Program variant manages region boundaries
2. Output gate gains a fourth state: `InlineActive` with region-scoped enforcement
3. Writes outside the region are permitted (scrollback area)
4. Writes inside the region without ownership are forbidden

**Current status:** Not implemented.  The `ScreenMode::Inline` variant exists in
`terminal_session.rs` but is not yet wired to ftui.  Tracked by FTUI-08 milestone.

## Required Evidence Fields

Each rule must be validated by at least one of:

| Rule | Evidence Type | Location |
|---|---|---|
| R1 | Unit test: gate blocks writes during Active | `output_gate.rs` tests |
| R2 | Unit test: TuiAwareWriter suppresses during Active | `output_gate.rs` tests |
| R3 | Unit test: suspend/resume lifecycle | `command_handoff.rs` tests |
| R4 | PTY E2E: inline mode scrollback integrity | Future: FTUI-08 |
| R5 | Unit test: SessionGuard drop calls leave() | `terminal_session.rs` tests |
| R5 | Unit test: panic hook writes crash bundle | `crash.rs` tests |

## Deletion Criteria

This ADR and the modules it maps (`output_gate.rs`, `terminal_session.rs`,
`command_handoff.rs`) are bridge code.  They will be superseded when:

1. ftui's `TerminalWriter` fully owns output routing (FTUI-09.3)
2. ftui's `Program` runtime owns lifecycle enter/suspend/resume/leave
3. The `tui` feature flag is removed

At that point, the atomic `OutputGate` is replaced by ftui's built-in writer
routing, and `TerminalSession` is replaced by ftui's `Program` lifecycle.

## References

- ADR-0002: One-Writer Terminal Ownership Rule (principles)
- `crates/wa-core/src/tui/output_gate.rs` (atomic phase gate)
- `crates/wa-core/src/tui/terminal_session.rs` (session lifecycle)
- `crates/wa-core/src/tui/command_handoff.rs` (suspend/resume handoff)
- `crates/wa-core/src/crash.rs` (panic hook with gate check)
- Bead wa-2q5r (FTUI-01.1.a)
- Bead wa-1q7m (FTUI-09.3, decommission plan)
