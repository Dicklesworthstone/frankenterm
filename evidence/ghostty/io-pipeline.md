# Ghostty I/O pipeline analysis (PTY read → parse → render → thread model)

Bead: `wa-3bja.3`

This doc traces Ghostty’s hot path from PTY bytes to screen updates and compares it with vendored WezTerm’s mux I/O model. The goal is to extract 3–5 concrete ideas to improve FrankenTerm’s capture + ingest pipeline.

Scope:
- Ghostty: `legacy_ghostty/src/termio/` + `legacy_ghostty/src/terminal/`
- WezTerm (vendored): `frankenterm/mux/src/lib.rs`
- FrankenTerm: `crates/frankenterm-core/src/runtime.rs`, `crates/frankenterm-core/src/tailer.rs`, `crates/frankenterm-core/src/wezterm.rs`

---

## 1) PTY reading

### Ghostty: dedicated reader thread, non-blocking drain loop + `poll`

Ghostty starts a dedicated read thread for a subprocess PTY:
- `legacy_ghostty/src/termio/Exec.zig:136`..`170` (spawns read thread)

The read thread:
- sets the PTY FD non-blocking (`fcntl`), then repeatedly reads in a tight loop until it would block:
  - `legacy_ghostty/src/termio/Exec.zig:1268`..`1297` (set `O_NONBLOCK`)
  - `legacy_ghostty/src/termio/Exec.zig:1306`..`1346` (read loop; breaks on `WouldBlock`)
- uses a simple `poll` on exactly **two fds** (PTY + quit pipe) as the “sleep until more data” mechanism:
  - `legacy_ghostty/src/termio/Exec.zig:1310`..`1324` (pollfds)
  - `legacy_ghostty/src/termio/Exec.zig:1351`..`1369` (`posix.poll`, quit/HUP handling)
- uses a small fixed read buffer: `var buf: [1024]u8`.
  - `legacy_ghostty/src/termio/Exec.zig:1304`

Ghostty explicitly comments that this model is “empirically fast” and lower overhead than epoll/io_uring here because reads are generally small:
- `legacy_ghostty/src/termio/Exec.zig:1249`..`1266`

### WezTerm: dedicated read thread, blocking read, forwards bytes to parser thread via socketpair

WezTerm spawns a read thread that does blocking reads from the PTY and writes bytes into a socketpair:
- `frankenterm/mux/src/lib.rs:279`..`343` (`read_from_pane_pty`)

Notable differences:
- WezTerm uses a **very large** read buffer (`BUFSIZE = 1024 * 1024`):
  - `frankenterm/mux/src/lib.rs:118`
- The read thread spawns a **second** per-pane thread (`parse_buffered_data`) and ships bytes to it via socketpair:
  - `frankenterm/mux/src/lib.rs:313`..`321` (spawn parser thread)

---

## 2) Parsing pipeline

### Ghostty: parse + terminal state updates happen on the reader thread

The read thread calls `Termio.processOutput` directly for each read chunk:
- `legacy_ghostty/src/termio/Exec.zig:1344`..`1346` (`Termio.processOutput`)

`processOutput` takes a mutex and runs the parse step:
- `legacy_ghostty/src/termio/Termio.zig:660`..`678` (lock + `processOutputLocked`)
- `legacy_ghostty/src/termio/Termio.zig:711`..`727`:
  - fast path: `terminal_stream.nextSlice(buf)`
  - slow path (only when inspector active): byte-at-a-time `terminal_stream.next(byte)`

Implication: Ghostty avoids the “read thread → parse thread” byte shuttle and can parse larger slices in one call.

### WezTerm: separate parser thread using `termwiz` escape parser

WezTerm’s `parse_buffered_data` runs in its own thread and:
- reads from the socketpair,
- parses VT sequences with `termwiz::escape::parser::Parser`,
- coalesces output actions under certain conditions,
- sends actions back to the mux thread.

Entry + core loop:
- `frankenterm/mux/src/lib.rs:140`..`237` (`parse_buffered_data`)

Notes:
- There is explicit output coalescing using a poll-based “wait a little for more data” window:
  - `frankenterm/mux/src/lib.rs:191`..`227`

---

## 3) Rendering / update scheduling

Ghostty queues a render wakeup before parsing the chunk:
- `legacy_ghostty/src/termio/Termio.zig:671` (calls `queueRender()`)
- `legacy_ghostty/src/termio/stream_handler.zig:101`..`106` (`queueRender` is `renderer_wakeup.notify()`)

This suggests Ghostty’s renderer thread/event loop is responsible for batching/coalescing paints. The reader thread’s job is to keep terminal state current and “poke” the renderer.

WezTerm’s mux parsing path instead produces action batches and forwards them to the mux thread; rendering happens downstream of mux applying actions to the pane model.

---

## 4) Thread model and scalability

### Ghostty

From the code paths above:
- **Reader thread** per exec-based terminal (PTY) (`io-reader`): `legacy_ghostty/src/termio/Exec.zig:136`..`170`
- A separate **writer/event thread** exists and is explicitly described as the “writer thread for terminal IO”:
  - `legacy_ghostty/src/termio/Thread.zig:1`..`20`

Key point: Ghostty allocates the “hot path” (read+parse+terminal mutation) to one thread and tries to offload writer/event responsibilities to another thread to reduce contention:
- `legacy_ghostty/src/termio/Thread.zig:6`..`19`

### WezTerm

Per pane, WezTerm uses:
- a blocking **read thread** (`read_from_pane_pty`)
- a **parser thread** (`parse_buffered_data`)
…plus mux/main threads downstream.

This is the “2N threads for N panes” model called out in the bead description, with the read thread shipping bytes to the parser thread via socketpair (`frankenterm/mux/src/lib.rs:279`..`321`).

---

## 5) FrankenTerm opportunities (3–5 actionable ideas)

These are framed as “Ghostty approach → WezTerm today → FrankenTerm opportunity”.

### Idea A — Collapse the “byte shuttle” where possible (read+parse locality)

- **Ghostty:** read thread drains non-blocking, then parses the full slice in `terminal_stream.nextSlice(buf)` under a mutex (`legacy_ghostty/src/termio/Termio.zig:711`..`727`).
- **WezTerm:** read thread → socketpair → parse thread (`frankenterm/mux/src/lib.rs:279`..`343` and `:140`..`237`).
- **FrankenTerm opportunity:** as we invest more in in-tree/native integrations (vendored mux client, `native-wezterm` output events), prefer designs that keep “bytes → state update” on one execution context and use mailboxes for *events*, not raw bytes.
  - Likely impact: less thread overhead and fewer copies; fewer synchronization choke points.

### Idea B — Adopt Ghostty’s non-blocking “drain until WouldBlock” loop as a template

- **Ghostty:** tight `read()` loop, breaks on `WouldBlock`, then waits via `poll` on PTY+quit (`legacy_ghostty/src/termio/Exec.zig:1306`..`1369`).
- **WezTerm:** blocking reads + forwarding.
- **FrankenTerm opportunity:** for any future direct PTY-facing path (or for native event socket readers), model the capture loop the same way:
  - non-blocking drains (reduces syscalls),
  - a minimal event wait primitive (kqueue/epoll/io_uring depending on platform),
  - explicit “quit fd” integration for clean shutdown.
  - Where to apply: `crates/frankenterm-core/src/runtime.rs` (event ingestion), `crates/frankenterm-core/src/tailer.rs` (capture scheduling), and any native event listeners.

### Idea C — Right-size buffers for latency vs throughput (don’t assume 1MB is best)

- **Ghostty:** uses 1KB buffer for PTY reads (`legacy_ghostty/src/termio/Exec.zig:1304`).
- **WezTerm:** uses 1MB buffer (`frankenterm/mux/src/lib.rs:118`).
- **FrankenTerm opportunity:** for ft’s capture surfaces (CLI get-text, native output events, direct mux reads), benchmark buffer sizing under swarm workloads:
  - smaller buffers can reduce latency/jitter and improve cache locality,
  - larger buffers can reduce syscalls but may increase tail latency and memory overhead.
  - Outcome should be a configuration knob with sane defaults.

### Idea D — Coalescing should live at the “right layer”

- **Ghostty:** render wakeup is a simple async notify; coalescing can be handled by the renderer/event loop (`legacy_ghostty/src/termio/stream_handler.zig:101`..`106`).
- **WezTerm:** coalesces in the parser thread before sending actions (`frankenterm/mux/src/lib.rs:191`..`227`).
- **FrankenTerm opportunity:** continue moving toward event-driven coalescing at boundaries:
  - at native event ingestion (per-pane output batching; similar to `wa-x4rq` work in `crates/frankenterm-core/src/runtime.rs`),
  - at workflow triggering (avoid redundant workflow runs),
  - at persistence (batch DB writes).

### Idea E — Make async backend a first-class portability knob (kqueue/epoll/io_uring)

Ghostty exposes an operator-facing async backend selection:
- `legacy_ghostty/src/config/Config.zig:3570`..`3617` (`async-backend` with `auto/epoll/io_uring`; kqueue on macOS).

**FrankenTerm opportunity:** as ft expands beyond polling `wezterm cli get-text`, provide a similar portability story for low-level event loops:
- Linux: epoll vs io_uring (when/if we adopt it),
- macOS: kqueue fallback,
- unify behind one abstraction so the capture pipeline stays predictable.

---

## Next steps

- Land this doc + close `wa-3bja.3`.
- Follow-on work should likely connect to:
  - native pane output event ingestion (`native-wezterm`),
  - minimizing per-pane overhead in the tailer supervisor (`crates/frankenterm-core/src/tailer.rs`),
  - reducing work amplification between capture → persist → detect → workflow.

