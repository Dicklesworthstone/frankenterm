# Subprocess Output Forwarding Contract

**Bead:** wa-3gsu (FTUI-03.2.b)
**Date:** 2026-02-09
**Depends on:** wa-1uqi (FTUI-03.2.a — output sink routing)
**Blocks:** wa-bjvg (FTUI-06.2.a — command handoff traces), wa-3fed (FTUI-03.4.a — panic teardown)

## 1  Problem Statement

`command_handoff.rs` currently runs subprocesses with inherited stdio: the child
process writes directly to the terminal. This works only because the session is
in `Suspended` phase (gate allows writes). However it means:

- No capture of subprocess output for recording/replay
- No sanitization or redaction of secrets in subprocess output
- No backpressure if the subprocess produces output faster than the terminal can render
- No ordering guarantees between subprocess output and wa detection events
- No diagnostics for dropped, reordered, or blocked output

PTY capture solves these by interposing a pseudoterminal between the subprocess
and the presentation layer.

## 2  Architecture Overview

```
 ┌──────────────────┐    PTY master fd    ┌───────────────────┐
 │   Subprocess      │───────────────────>│  CaptureLoop       │
 │  (child process)  │<──pty slave fd───  │  (async reader)    │
 └──────────────────┘                     └───────┬───────────┘
                                                   │ raw bytes
                                          ┌───────▼───────────┐
                                          │  SanitizePipeline  │
                                          │  1. ANSI normalize │
                                          │  2. Redact secrets │
                                          │  3. Frame stamp    │
                                          └───────┬───────────┘
                                                   │ SubprocessFrame
                            ┌──────────────────────┼──────────────────┐
                            │                      │                  │
                   ┌────────▼──────┐    ┌─────────▼────────┐  ┌─────▼──────┐
                   │ TerminalWriter│    │ RecordingEngine   │  │ Diagnostics│
                   │ (stdout sink) │    │ (Output frames)   │  │ (counters) │
                   └───────────────┘    └──────────────────┘  └────────────┘
```

## 3  PTY Capture Contract

### 3.1  Pseudoterminal Pair

The capture loop creates a POSIX pseudoterminal pair via `openpty(2)`:

| Fd | Owner | Purpose |
|----|-------|---------|
| master | CaptureLoop (parent) | Read subprocess output, write subprocess input |
| slave | Subprocess (child) | stdin/stdout/stderr of child process |

The child is spawned with the slave fd as its controlling terminal. The master
fd is read in a non-blocking async loop by the `CaptureLoop` task.

### 3.2  CaptureLoop Behavior

```
loop {
    select! {
        bytes = master_fd.read(&mut buf) => {
            if bytes == 0 { break; }  // child exited
            pipeline.push(buf[..bytes]);
        }
        _ = shutdown_signal => {
            // drain remaining bytes, then break
        }
    }
}
```

**Invariants:**
- CaptureLoop runs on a dedicated tokio task (not the render task)
- Read buffer is 8192 bytes (matches typical PTY read granularity)
- EOF on master fd means the child process has exited
- Shutdown signal is a `tokio::sync::watch` from the session lifecycle

### 3.3  Gate Phase Interaction

| Gate Phase | CaptureLoop | TerminalWriter | RecordingEngine |
|------------|-------------|----------------|-----------------|
| `Active` | Reads into ring buffer (no terminal write) | Blocked — buffered | Records frames |
| `Suspended` | Reads and forwards to terminal | Writes to stdout | Records frames |
| `Inactive` | Not running | Flushes remaining buffer | Finalizes recording |

During `Active` phase (TUI owns terminal), subprocess output is captured and
buffered but not written to the terminal. When the session transitions to
`Suspended` (command handoff), the buffer is flushed to stdout and new output
flows through directly.

## 4  Sanitization Pipeline

Output passes through three stages in order:

### 4.1  ANSI Normalization

Strip or normalize control sequences that could corrupt terminal state:

| Category | Action | Reason |
|----------|--------|--------|
| Title-set (`\e]0;...\a`) | Strip | Prevents subprocess from changing wa's title |
| Alternate screen (`\e[?1049h/l`) | Strip | Prevents nesting alternate screens |
| Bracketed paste mode (`\e[?2004h/l`) | Strip | wa manages paste mode |
| Mouse mode (`\e[?1000h` etc.) | Strip | wa manages mouse capture |
| Cursor visibility (`\e[?25h/l`) | Pass through | Subprocess may legitimately show/hide cursor |
| SGR (color/style) | Pass through | Normal terminal output |
| Cursor movement | Pass through | Normal terminal output |
| OSC 52 (clipboard) | Strip | Security boundary — subprocess must not set clipboard |

### 4.2  Secret Redaction

Apply `policy::Redactor::redact()` to each output chunk. The existing redactor
handles API keys, tokens, passwords, and database URLs.

**Redaction boundary:** Redaction is applied to the raw bytes *before* they
reach either the terminal or the recording engine. This ensures:
- Live terminal output is redacted in real-time
- Recorded sessions never contain unredacted secrets
- No window exists where secrets are written to any persistent store

**Chunk boundary handling:** Secrets may span read boundaries (e.g., a token
split across two 8192-byte reads). The pipeline maintains a 256-byte overlap
window: each chunk is scanned as `[overlap][new_bytes]`, and only `new_bytes`
portion is emitted. The overlap window is retained for the next chunk.

### 4.3  Frame Stamping

Each sanitized chunk is wrapped as a `SubprocessFrame`:

```rust
struct SubprocessFrame {
    timestamp_ms: u64,      // monotonic clock
    seq: u64,               // monotonically increasing sequence number
    source: FrameSource,    // Stdout | Stderr (if distinguishable via PTY)
    payload: Vec<u8>,       // sanitized bytes
    flags: FrameFlags,      // bitflags: IS_GAP, WAS_REDACTED, WAS_TRUNCATED
}
```

The sequence number provides total ordering even when timestamps collide.

## 5  Ordering Guarantees

### 5.1  Causality

Subprocess output and wa detection events can interleave. The ordering contract:

1. **Within subprocess output:** Total order preserved by `seq` field
2. **Between subprocess output and events:** Temporal order via `timestamp_ms`
   (monotonic clock). Events are timestamped at detection time, frames at read time.
3. **Between multiple subprocesses:** Not applicable — wa runs one subprocess
   at a time during `Suspended` phase (enforced by `command_handoff.rs` state machine)

### 5.2  Deterministic Replay

For replay determinism, the recording engine stores:
- `SubprocessFrame` as `FrameType::Output` with the sanitized payload
- The sequence number is preserved in frame ordering (frames written in `seq` order)
- Replay reads frames in storage order, which matches the original `seq` order

## 6  Backpressure Strategy

### 6.1  Ring Buffer

The capture loop writes into a bounded ring buffer:

| Parameter | Value | Rationale |
|-----------|-------|-----------|
| Capacity | 256 KiB | ~2 full terminal screens at 80x24 |
| High watermark | 192 KiB (75%) | Triggers `Yellow` tier |
| Critical watermark | 240 KiB (94%) | Triggers `Red` tier |

### 6.2  Tier Actions

| Tier | Buffer Usage | Action |
|------|-------------|--------|
| `Green` | < 75% | Normal operation |
| `Yellow` | 75%-94% | Log warning, increment `backpressure_yellow` counter |
| `Red` | > 94% | Drop oldest unwritten frames, increment `frames_dropped` counter |
| `Black` | Buffer full, writer blocked > 100ms | Kill subprocess with SIGTERM, log error |

### 6.3  Drop Policy

When frames must be dropped (Red tier):
- Drop the oldest unwritten frames first (FIFO eviction)
- Set `IS_GAP` flag on the next emitted frame to signal discontinuity
- Record a diagnostic entry: `{ dropped_bytes, dropped_frames, tier, timestamp }`
- The recording engine inserts a gap marker so replay can indicate missing output

## 7  Diagnostics

### 7.1  Counters

The forwarding pipeline exposes the following counters:

| Counter | Type | Description |
|---------|------|-------------|
| `bytes_captured` | u64 | Total bytes read from PTY master fd |
| `bytes_emitted` | u64 | Total bytes written to terminal/recording |
| `bytes_redacted` | u64 | Bytes replaced by redaction placeholders |
| `bytes_dropped` | u64 | Bytes lost to backpressure eviction |
| `frames_captured` | u64 | Total SubprocessFrames created |
| `frames_emitted` | u64 | SubprocessFrames successfully delivered |
| `frames_dropped` | u64 | SubprocessFrames evicted by backpressure |
| `backpressure_yellow` | u64 | Number of Yellow tier transitions |
| `backpressure_red` | u64 | Number of Red tier transitions |
| `backpressure_black` | u64 | Number of Black tier events (subprocess killed) |
| `redaction_hits` | u64 | Number of secret patterns matched |
| `ansi_stripped` | u64 | Number of control sequences stripped |
| `overlap_rescans` | u64 | Number of chunk-boundary redaction rescans |

### 7.2  Log Separation

Subprocess-origin issues use a dedicated tracing target:

```rust
tracing::warn!(target: "wa::subprocess", dropped_bytes, "backpressure: dropping frames");
tracing::error!(target: "wa::subprocess", "black tier: killing subprocess");
```

This allows filtering subprocess diagnostics separately from UI/runtime logs
via tracing subscriber layer filters.

### 7.3  Diagnostic Report

At subprocess exit, the pipeline emits a `SubprocessReport`:

```rust
struct SubprocessReport {
    pid: u32,
    exit_status: Option<i32>,
    duration_ms: u64,
    bytes_captured: u64,
    bytes_emitted: u64,
    bytes_dropped: u64,
    frames_dropped: u64,
    peak_buffer_usage: usize,
    redaction_hits: u64,
    had_backpressure: bool,
    had_drops: bool,
    had_kill: bool,
}
```

This report is:
- Written to the recording as a `FrameType::Marker` (JSON-encoded)
- Logged at `info` level to the `wa::subprocess` target
- Available via `SubprocessForwarder::report()` for the command handoff caller

## 8  Implementation Plan

### Phase 1: PTY Capture Module (new file)

Create `crates/wa-core/src/subprocess_capture.rs`:

1. `PtyPair` struct wrapping `openpty(2)` via the `nix` crate
2. `CaptureLoop` async task that reads from master fd
3. `SanitizePipeline` with ANSI normalization + redaction stages
4. `SubprocessFrame` type with seq numbering
5. Ring buffer with backpressure tier tracking
6. `SubprocessForwarder` facade that wires everything together

### Phase 2: Integration with Command Handoff

Modify `crates/wa-core/src/tui/command_handoff.rs`:

1. Replace `Command::new().stdin(Inherited)` with PTY-based spawn
2. Create `SubprocessForwarder` before spawn
3. Wire CaptureLoop output to both terminal and recording engine
4. Collect `SubprocessReport` after child exit
5. Pass report to caller for triage/audit

### Phase 3: Integration with Recording Engine

Modify `crates/wa-core/src/recording.rs`:

1. Accept `SubprocessFrame` in `Recorder::record_output()`
2. Preserve gap markers from backpressure drops
3. Include subprocess report marker in recording

### Phase 4: Tests

| Test | Category | Covers |
|------|----------|--------|
| `pty_pair_create_and_write` | unit | PTY pair creation, basic read/write |
| `capture_loop_reads_child_output` | integration | CaptureLoop reads full subprocess output |
| `capture_loop_eof_on_child_exit` | integration | CaptureLoop exits cleanly when child exits |
| `sanitize_strips_title_set` | unit | ANSI normalization strips title sequences |
| `sanitize_strips_alt_screen` | unit | ANSI normalization strips alternate screen |
| `sanitize_passes_sgr` | unit | ANSI normalization preserves color/style |
| `sanitize_strips_osc52` | unit | Security: clipboard set is blocked |
| `redaction_applied_before_emit` | unit | Secrets never reach terminal or recording |
| `redaction_chunk_boundary` | unit | Secret spanning two reads is still caught |
| `frame_ordering_preserved` | unit | seq numbers are monotonic and gapless |
| `backpressure_yellow_logs_warning` | unit | Yellow tier triggers diagnostic |
| `backpressure_red_drops_oldest` | unit | Red tier evicts oldest frames, sets IS_GAP |
| `backpressure_black_kills_child` | integration | Black tier sends SIGTERM to child |
| `gate_active_buffers_output` | integration | Output buffered (not written) during Active |
| `gate_suspended_forwards_output` | integration | Output forwarded to terminal during Suspended |
| `report_generated_on_exit` | integration | SubprocessReport has correct counters |
| `report_records_drops` | integration | Report reflects backpressure events |
| `recording_includes_subprocess` | integration | Subprocess frames appear in recording |
| `recording_gap_marker_on_drop` | integration | Gap marker inserted when frames dropped |
| `deterministic_replay_order` | integration | Replay produces same output order as capture |

## 9  Dependencies

### Crate Dependencies

| Crate | Version | Purpose |
|-------|---------|---------|
| `nix` | 0.29+ | `openpty(2)`, `SIGTERM`, PTY ioctls |

The `nix` crate is already a transitive dependency via tokio. Direct dependency
should be feature-gated behind `cfg(unix)` since PTY capture is POSIX-only.

### Internal Dependencies

| Module | Change | Impact |
|--------|--------|--------|
| `command_handoff.rs` | Replace inherited stdio with PTY spawn | Medium — function signature changes |
| `recording.rs` | Accept gap markers | Low — additive |
| `output_gate.rs` | No changes needed | None — gate contract unchanged |
| `backpressure.rs` | Share tier thresholds config | Low — import existing config |

## 10  Risk Assessment

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| PTY read latency adds visible delay | Low | 8 KiB buffer, async non-blocking reads |
| Redaction false positives mask output | Medium | Redaction report in SubprocessReport; `WAS_REDACTED` flag |
| Ring buffer too small for long output | Low | 256 KiB is ~10x a typical terminal screen |
| Chunk-boundary redaction misses | Low | 256-byte overlap window covers all pattern lengths |
| Platform incompatibility (non-POSIX) | N/A | Feature-gated `cfg(unix)` — wa targets Linux/macOS only |

## References

- `crates/wa-core/src/tui/output_gate.rs` — gate phase tracking
- `crates/wa-core/src/tui/command_handoff.rs` — current subprocess execution
- `crates/wa-core/src/tui/terminal_session.rs` — session lifecycle
- `crates/wa-core/src/recording.rs` — recording frame engine
- `crates/wa-core/src/replay.rs` — OutputSink trait + TerminalSink
- `crates/wa-core/src/policy.rs` — Redactor secret patterns
- `crates/wa-core/src/backpressure.rs` — queue depth tiers
- `docs/ftui-output-sink-routing.md` — output sink routing contract (wa-1uqi)
- `docs/adr/0010-one-writer-rule-adaptation.md` — one-writer rule ADR
