# Output Sink Routing Contract for FTUI Migration

**Bead:** wa-1uqi (FTUI-03.2.a)
**Date:** 2026-02-09
**Extends:** ADR-0010 (One-Writer Rule Adaptation)

## 1  Problem Statement

When the TUI is active, only the rendering pipeline may write to stdout/stderr.
Stray `println!` or `eprintln!` calls corrupt cursor state and layout. The
ftui framework enforces this more strictly than ratatui — any bypass write
produces visible glitches.

## 2  Output Gate Contract

### Gate Module: `wa-core/src/tui/output_gate.rs`

The gate tracks three phases via a global `AtomicU8`:

| Phase | Value | stdout/stderr | When |
|-------|-------|---------------|------|
| `Inactive` | 0 | Safe to write | TUI not running |
| `Active` | 1 | **FORBIDDEN** | TUI rendering pipeline owns terminal |
| `Suspended` | 2 | Safe to write | Command handoff (subprocess owns terminal) |

### Decision Rule

```
fn may_write_stdout_stderr() -> bool {
    output_gate::phase() != GatePhase::Active
}
```

### Sanctioned Output Paths

When the TUI is `Active`, output MUST flow through one of:

| Path | Mechanism | Used By |
|------|-----------|---------|
| ftui `Frame.buffer` | Cell-based rendering via `write_styled()` | All view rendering |
| `TuiAwareWriter` | tracing subscriber writer that discards when suppressed | Logging (tracing macros) |
| Output gate check | `if !is_output_suppressed() { eprintln!(...) }` | Crash handler |

All other stdout/stderr writes are **forbidden** during `Active` phase.

## 3  Inventory of Direct Writes

### Already Protected (LOW risk)

| Location | Mechanism | Notes |
|----------|-----------|-------|
| `logging.rs:172-222` | `TuiAwareWriter` when TUI/FTUI feature enabled | `cfg(any(feature = "tui", feature = "ftui"))` |
| `logging.rs:185` | Direct stderr when TUI feature disabled | `cfg(not(any(feature = "tui", feature = "ftui")))` |
| `crash.rs:215-258` | `is_output_suppressed()` check before eprintln | Only writes on panic, properly gated |
| `tui/output_gate.rs:112` | TuiAwareWriter stderr reference | Part of gate infrastructure |
| `tui/terminal_session.rs:257-282` | `std::io::stdout()` for crossterm backend | TUI infrastructure (intentional terminal control) |
| `tui/app.rs:104-114` | `io::stdout()` for crossterm terminal | TUI infrastructure |

### Mutually Exclusive with TUI (SAFE — no gate needed)

These code paths execute CLI commands that cannot co-exist with TUI rendering.
The TUI is either not started, or these run before/after TUI lifecycle.

| Location | Code Path | Why Safe |
|----------|-----------|----------|
| `main.rs` ~80+ println/eprintln | CLI command output (status, list, search, events, schedule, etc.) | Commands run instead of TUI, not alongside it |
| `main.rs:8065-8067` | `wa version` output | One-shot command |
| `main.rs:20890-20901` | `wa config show` output | One-shot command |
| `main.rs:3398-3429` | Robot mode JSON/TOON output | Machine output, not TUI-concurrent |
| `main.rs:22963-23514` | Setup/confirmation prompts | Interactive setup, TUI not active |
| `output/format.rs:34-60` | `stdout().is_terminal()` | Read-only, no writes |

### Requires Remediation (MEDIUM risk)

These paths *could theoretically* run during TUI via command dispatch or
background tasks, though they currently do not:

| Location | Code | Risk | Remediation |
|----------|------|------|-------------|
| `replay.rs:242-253` | `stdout().write_all()`, `eprintln!` in TerminalSink | Medium — replay runs as standalone command, not during TUI | Add `assert!(!is_output_suppressed())` guard at TerminalSink creation |
| `main.rs:15528` | `writeln!(stdout, "{line}")` in audit stream | Medium — audit stream is a standalone command | No change needed (command-exclusive) |
| `main.rs:17466-17837` | Export to stdout (asciinema, HTML) | Medium — export is a standalone command | No change needed (command-exclusive) |

## 4  Removal Plan

### Phase 1: Assert-guard (current session)

Add debug assertions to code paths that are *designed* to be TUI-exclusive
but lack explicit checks:

```rust
// In replay.rs TerminalSink::new() or write_output():
debug_assert!(
    !output_gate::is_output_suppressed(),
    "TerminalSink must not write while TUI is active"
);
```

This catches accidental invocation during TUI without runtime cost in release.

### Phase 2: Centralized sink (future — FTUI-03.2.b)

When subprocess output routing (wa-3gsu) is implemented:

1. Replace `TerminalSink::write_output()` with gate-aware sink that buffers
   output when suppressed and flushes when gate transitions to Inactive/Suspended
2. Route export/audit stdout writes through the same sink
3. Add compile-time lint (custom clippy or grep-based CI check) to detect
   new `println!` / `eprintln!` calls in wa-core outside of test modules

### Phase 3: ftui TerminalWriter takeover (FTUI-09.3)

When ftui's `TerminalWriter` fully owns output routing:

1. Remove `output_gate.rs` atomic gate (ftui provides equivalent)
2. Remove `TuiAwareWriter` (ftui provides built-in log routing)
3. All output flows through ftui's write abstraction

## 5  Verification Checks

### Compile-Time Check (CI)

```bash
# Detect new println!/eprintln! in wa-core/src/ (excluding tests and output_gate)
rg '(println!|eprintln!|print!|eprint!)' crates/wa-core/src/ \
  --type rust \
  -g '!**/tests/**' \
  -g '!**/output_gate.rs' \
  -g '!**/test*.rs' \
  | grep -v '#\[cfg(test)\]' \
  | grep -v '// SAFE:' \
  | grep -v 'is_output_suppressed'
```

Expected: Only hits in `crash.rs` (gated) and `replay.rs` (assert-guarded).
New hits MUST include either:
- `// SAFE: <reason>` comment explaining why no gate is needed
- An `is_output_suppressed()` check

### Runtime Verification

The existing output_gate tests verify:

| Test | What it proves |
|------|---------------|
| `active_suppresses_output` | Gate correctly reports Active as suppressed |
| `suspended_does_not_suppress` | Suspended allows writes (command handoff) |
| `full_lifecycle` | Enter → Suspend → Resume → Leave transitions correct |
| `tui_aware_writer_suppresses_when_active` | TuiAwareWriter discards during Active |
| `tui_aware_writer_passes_through_when_inactive` | TuiAwareWriter forwards during Inactive |

### Unit Test: Assert No Bypass

```rust
// Proof that TerminalSink respects output gate (to be added in Phase 1)
#[test]
fn terminal_sink_debug_asserts_when_gate_active() {
    use crate::tui::output_gate::{set_phase, GatePhase};
    set_phase(GatePhase::Active);
    // TerminalSink creation or write_output should panic in debug mode
    // (debug_assert! fires)
    set_phase(GatePhase::Inactive);
}
```

## 6  Policy Summary

| Rule | Scope | Enforcement |
|------|-------|-------------|
| No direct stdout/stderr writes during `GatePhase::Active` | All wa-core modules | `TuiAwareWriter` for logging, `is_output_suppressed()` for ad-hoc |
| CLI command output is safe (TUI not co-active) | main.rs command handlers | Structural: commands and TUI are mutually exclusive |
| New println!/eprintln! in wa-core must be annotated | All new code | CI grep check + code review |
| TerminalSink must assert gate is not Active | replay.rs | debug_assert! guard |

## References

- `crates/wa-core/src/tui/output_gate.rs` — gate implementation
- `crates/wa-core/src/logging.rs:172-222` — TuiAwareWriter integration
- `crates/wa-core/src/crash.rs:215-258` — panic hook gate check
- ADR-0010: One-Writer Rule Adaptation
- wa-3gsu (FTUI-03.2.b): Subprocess output through PTY capture (blocked by this)
