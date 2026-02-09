# ADR-0002: One-Writer Terminal Ownership Rule

**Status:** Accepted
**Date:** 2026-02-09
**Context:** FTUI-01.1 (wa-2dlw)

## Decision

Adopt the one-writer rule: when the TUI is active, exactly one component owns
terminal writes. All other output (daemon logs, subprocess output, debug traces)
is routed through a controlled sink that the UI runtime manages.

## Context

wa has multiple subsystems that produce terminal output:

1. **TUI** - Renders interactive views (ratatui widgets via crossterm)
2. **Watcher daemon** - Logs capture progress, pattern matches, errors
3. **CLI commands** - Human-readable output from `wa status`, `wa search`, etc.
4. **Subprocess output** - Commands launched from TUI actions (e.g., `wa send`)

Currently, the TUI uses crossterm's alternate screen to isolate itself, but:

- Daemon log output written to stdout during TUI operation would corrupt the display.
- The `run_command` method in `app.rs` leaves alt-screen, runs a command, then
  re-enters alt-screen. This creates visible flicker and loses render state.
- There is no enforcement mechanism. Correctness depends on convention.

## Output Routing Matrix

| Source | During TUI | During Command Handoff | During Inline |
|--------|-----------|----------------------|---------------|
| TUI rendering | ftui owns | suspended | ftui owns |
| Logging | sink to buffer or file | restored to stderr | sink to buffer |
| Subprocess output | captured, not displayed | direct to terminal | captured |
| Workflow runners | silent (sink/file) | direct to terminal | captured |
| wa status/show | prohibited | allowed | prohibited |

## Rules

### R1: Exclusive terminal write ownership

When the TUI runtime is active, it holds exclusive write access to the terminal.
No other code path may write to stdout/stderr directly.

### R2: Log routing through output sink

All log output (tracing, daemon messages) must go through a routing layer:

- **TUI active:** Logs are buffered and shown in a dedicated log panel or
  discarded based on severity. They never bypass the render pipeline.
- **TUI inactive:** Logs go to stderr (or a file) as usual.

### R3: Command handoff protocol

When the TUI needs to run an external command that requires terminal access:

1. TUI saves its state.
2. TUI releases terminal ownership (leave raw mode, optionally leave alt-screen).
3. Command runs with full terminal access.
4. TUI reclaims terminal ownership and restores state.
5. Screen state is fully redrawn from current data (no stale buffers).

This replaces the current `run_command` pattern in `app.rs` with an explicit
ownership transfer. No partial handoffs: the terminal is either fully owned
by ftui or fully released.

### R4: Inline-mode compatibility

The one-writer rule applies in both modes:

- **Alt-screen mode:** Traditional full-screen TUI. Terminal is fully owned.
- **Inline mode:** TUI renders within scrollback. Ownership is scoped to the
  rendered region. Other output above the render region is permitted (but must
  not interleave with active rendering).

### R5: Structured lifecycle with panic safety

The ftui lifecycle must guarantee terminal restoration:
- `enter()` sets up terminal mode (raw, alternate if needed)
- `exit()` restores terminal to pre-enter state
- Panic hook calls `exit()` before unwinding
- Signal handler calls `exit()` on SIGINT/SIGTERM

Current wa uses `panic = "abort"` in release. The cleanup hook must run
before abort via `std::panic::set_hook`.

## Consequences

### What changes

- `app.rs` terminal setup/teardown must go through an ownership manager
  (not raw crossterm calls).
- All `tracing` output must be routed through the ownership-aware sink when
  the TUI feature is active.
- `run_command` must use the handoff protocol instead of ad-hoc alt-screen toggles.

### What stays the same

- Non-TUI paths (robot mode, MCP, plain CLI) are unaffected.
- The `QueryClient` abstraction is unchanged.
- Feature gating (`#[cfg(feature = "tui")]`) continues to control compilation.

## Implementation guidance

The ownership manager should be a lightweight struct:

```
TerminalOwnership {
    state: Owned | Released | InlineScoped
    log_sink: channel or buffer
}
```

All terminal writes go through this. Violations (direct stdout writes while Owned)
should be caught by tests, not runtime panics.

## References

- Current alt-screen handling: `crates/wa-core/src/tui/app.rs:101-131`
- Current command handoff: `crates/wa-core/src/tui/app.rs` `run_command` method
- Related bead: wa-2qyt (FTUI-03.1 terminal session ownership abstraction)
