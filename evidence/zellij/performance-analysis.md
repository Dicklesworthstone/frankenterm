# Zellij Performance Analysis (`wa-1pgzt`)

## Scope
This dossier analyzes Zellij performance architecture with focus on:
- output throughput path
- resource/backpressure controls
- memory behavior
- CPU scheduling model
- scaling evidence and limits
- concrete FrankenTerm recommendations

Source root: `legacy_zellij/`.

## 1) Output Throughput Path

### PTY ingest flow
- Each terminal spawn creates an async task that runs `TerminalBytes::listen()` (`zellij-server/src/pty.rs:1084`).
- The PTY read loop uses a 64 KiB buffer and forwards each read as `ScreenInstruction::PtyBytes` (`zellij-server/src/terminal_bytes.rs:56`, `zellij-server/src/terminal_bytes.rs:69`).
- Sends into screen are done via `spawn_blocking` wrappers (`zellij-server/src/terminal_bytes.rs:104`).

### Observed implications
- Throughput is strongly dependent on screen-thread queue health because PTY readers enqueue directly into screen instruction channels.
- The comment in `TerminalBytes::listen()` describes render-rate adaptation goals, but current loop always forwards bytes immediately and only sends `Render` at end-of-stream (`zellij-server/src/terminal_bytes.rs:45`, `zellij-server/src/terminal_bytes.rs:93`).

## 2) Backpressure and Buffering Strategy

### Channel topology
- `to_server` is bounded at 50 (`zellij-server/src/lib.rs:647`).
- Screen has both unbounded and bounded channels (`zellij-server/src/lib.rs:1687`, `zellij-server/src/lib.rs:1690`).

### Client fanout protection
- Client IO uses dedicated sender thread with bounded queue depth 5000 (`zellij-server/src/os_input_output.rs:376`, `zellij-server/src/os_input_output.rs:386`).
- On sustained overload, design intent is to stop serving that client instead of allowing unbounded memory growth (`zellij-server/src/os_input_output.rs:383`).

### Scrolled-pane event buffering
- When a pane is scrolled, incoming PTY bytes are buffered in `pending_vte_events` (`zellij-server/src/tab/mod.rs:2474`).
- Buffer has hard cap `MAX_PENDING_VTE_EVENTS = 7000`; on overflow it clears scroll and drains backlog (`zellij-server/src/tab/mod.rs:145`, `zellij-server/src/tab/mod.rs:2479`).

## 3) Memory Management

### Scrollback storage model
- Terminal grid stores scrollback above viewport as `VecDeque<Row>` (`zellij-server/src/panes/grid.rs:315`).
- `bounded_push(...)` enforces FIFO retention by dropping oldest row when capacity reaches `SCROLL_BUFFER_SIZE` (`zellij-server/src/panes/grid.rs:231`).
- `SCROLL_BUFFER_SIZE` is initialized from config `scroll_buffer_size` (or default) at session init (`zellij-server/src/lib.rs:1681`).

### Guard rails
- Scrollback viewport reset uses a bounded loop (`SCROLL_BUFFER_SIZE * 2`) to avoid pathological endless scrolling loops (`zellij-server/src/panes/grid.rs:1257`).
- Scrollback size is surfaced and recalculated (`zellij-server/src/panes/grid.rs:604`, `zellij-server/src/panes/grid.rs:612`).

### Per-pane memory shape
- `TerminalPane` owns one `Grid` (`zellij-server/src/panes/terminal_pane.rs:118`).
- `PluginPane` keeps per-client grids (`HashMap<ClientId, Grid>`), so plugin-pane memory scales with connected clients (`zellij-server/src/panes/plugin_pane.rs:94`).

## 4) CPU Scheduling and Concurrency Model

### Threading model
- Session owns dedicated subsystem threads: screen, pty, plugin, pty_writer, background_jobs (`zellij-server/src/lib.rs:326`).
- Thread startup is explicit via named `thread::Builder` spawns (`zellij-server/src/lib.rs:1739`, `zellij-server/src/lib.rs:1763`, `zellij-server/src/lib.rs:1801`, `zellij-server/src/lib.rs:1850`, `zellij-server/src/lib.rs:1867`).

### Pane-level execution pattern
- PTY output is consumed by per-terminal async tasks (`zellij-server/src/pty.rs:1084`).
- No explicit focused-pane scheduling preference appears in PTY ingest path; bytes are processed for tiled, floating, and suppressed panes (`zellij-server/src/tab/mod.rs:2463`, `zellij-server/src/tab/mod.rs:2468`).

## 5) Scalability Evidence

### Built-in controls
- CLI exposes `max_panes` with explicit warning that opening beyond limit closes old panes (`zellij-utils/src/cli.rs:40`).
- Enforcement path actively closes panes beyond limit (`zellij-server/src/tab/mod.rs:3510`).

### Functional stress coverage (limited)
- Integration tests include multi-floating and floating-resize scenarios (`zellij-server/src/tab/unit/tab_integration_tests.rs:1551`, `zellij-server/src/tab/unit/tab_integration_tests.rs:2751`).
- Stacked layout coverage is extensive in layout-applier tests (`zellij-server/src/tab/unit/layout_applier_tests.rs:2218`, `zellij-server/src/tab/unit/layout_applier_tests.rs:2310`).

### Benchmark gap
- In this clone, no dedicated benchmark/criterion harnesses were found during filesystem and Cargo manifest scans.
- Result: scaling behavior is better covered functionally than quantitatively.

## 6) Comparison to FrankenTerm Priorities

From a FrankenTerm swarm perspective (50+ panes):
- Zellij has stronger in-core pane/mux data structures than many terminal wrappers.
- It has practical safeguards (bounded queues, max panes, bounded scrollback), but not explicit published throughput/latency benchmarks in this clone.
- It does not appear to prioritize focused-pane IO scheduling in PTY ingest; throughput is mostly queue and parser driven.

## 7) Recommendations for FrankenTerm (`wa-3cyp`)

1. Keep hard capacity controls (`max panes`, bounded queues) as first-class runtime knobs.
Why: prevents runaway memory under swarm overload.

2. Preserve FIFO scrollback bounds with explicit drop accounting.
Why: bounded memory is non-negotiable for long-lived agent swarms.

3. Add quantitative perf harnesses before adopting large architectural changes.
Why: current Zellij evidence is rich in functional tests but weak in throughput benchmarking.

4. Consider focused-pane scheduling bias for render/update paths.
Why: improves UX responsiveness under heavy multi-pane output.

5. Keep overload behavior explicit and observable (queue saturation, dropped lines, forced scroll reset).
Why: debuggable degradation is better than silent latency cliffs.

6. Treat per-client pane state (plugin-like panes) as multiplicative memory and budget it directly.
Why: multi-operator/multi-agent sessions can amplify memory unexpectedly.

## Cross-References
- `wa-3cyp` (FrankenTerm performance optimization epic)
- `wa-2bai5` (Zellij analysis synthesis)
