# Resize Artifact Fault Model (`wa-1u90p.4.1`)

Date: 2026-02-14
Author: `LavenderCastle`
Parent track: `wa-1u90p.4`
Related: ADR-0011 (two-phase resize transactions), `wa-1u90p.5.7` (lock graph)

## Scope

Root-cause analysis of stretched-text and transient invalid-frame artifacts during resize, producing a concrete fault model with identified race conditions, ordering violations, and mitigation design.

## Artifact Classes Under Investigation

| Class | Description | User-visible symptom |
|---|---|---|
| `A1` Stretched text | Content rendered with wrong column count | Glyphs appear horizontally stretched or compressed for 1-2 frames |
| `A2` Stale full-frame | Entire viewport shows previous-size content | Brief flash of old layout before new layout appears |
| `A3` Transient line mismatch | Some lines at new width, others at old width | Visible seam between correctly and incorrectly reflowed lines |
| `A4` Cursor position jump | Cursor appears at wrong position after resize | Cursor teleports then snaps back |

## Identified Fault Classes

### F1: PTY-Terminal Dimension Mismatch Window (Primary cause of `A1`)

**Location:** `frankenterm/mux/src/localpane.rs:1283` (`apply_resize_sync`)

**Mechanism:**
```
Timeline (single pane resize):
  t0: terminal.lock() → probe current size → drop lock
  t1: superseded_by() check
  t2: pty.lock() → pty.resize(new_size) → drop lock    ← PTY now at NEW size
  t3: child process receives SIGWINCH                   ← output now formatted for NEW cols
  t4: [GAP] child output arrives, parsed by terminal    ← terminal still at OLD cols
  t5: superseded_by() check
  t6: terminal.lock() → terminal.resize(new_size)       ← terminal now at NEW size
```

Between `t2` and `t6`, the PTY has the new dimensions but the terminal screen buffer still has old dimensions. Any output from the child process during this window is formatted for the new column width but gets parsed and inserted into a buffer with the old column width. This creates:
- Text meant for 120 cols being wrapped at 80 cols (shrink case) → extra wraps
- Text meant for 80 cols being laid out in 120 col buffer (grow case) → short lines

The gap duration depends on:
- Lock contention on the terminal mutex (measured p95: 2.81 ms avg hold)
- Child process output rate during the window
- Reflow computation time in `Screen::resize()` (measured max: 3.77 ms for single pane scrollback)

**Severity:** High. This is the primary mechanism for `A1` stretched text artifacts. Every resize operation has this window.

**Evidence:**
- `apply_resize_sync` at localpane.rs:1331-1385 shows the sequential PTY-then-terminal pattern
- Lock graph doc (wa-1u90p.5.7) confirms "PTY lock is dropped before terminal apply lock"
- Baseline timeline shows `logical_reflow` max latency of 3.77 ms, which is the minimum duration of this window under reflow conditions
- Within `Screen::resize()` (screen.rs:1267-1413), `physical_rows/cols` are updated at lines 1404-1405 (end of function), after rewrap at lines 1314-1315. This is safe from concurrent reads because the entire operation is behind the terminal mutex. The F1 race occurs in the gap between the PTY mutex release and the terminal mutex acquisition in `apply_resize_sync`.

### F2: Non-Atomic Render Dimension/Content Read (Primary cause of `A2`, `A3`)

**Location:** `frankenterm/mux/src/localpane.rs:376` and `localpane.rs:368`

**Mechanism:**
The render path makes two separate mutex-guarded calls to read pane state:
```rust
// Call 1: get dimensions (acquires terminal mutex, reads, drops)
fn get_dimensions(&self) -> RenderableDimensions {
    terminal_get_dimensions(&mut self.terminal.lock())
}

// Call 2: get line content (acquires terminal mutex, reads, drops)
fn get_lines(&self, lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
    crate::pane::impl_get_lines_via_with_lines(self, lines)
}
```

Between these two calls, the resize worker can complete `terminal.resize()`. The render then has:
- Dimensions from state A (old size) + lines from state B (new size), or vice versa

This produces:
- `A2` when dimensions are stale but lines are new: renderer allocates viewport for old row/col count but receives content reflowed for new count
- `A3` when partial reflow is visible: some lines still have old wrapping metadata

**Precision note:** Within a single mutex acquisition, `terminal_get_dimensions()` (renderable.rs:131-145) reads consistent state because it takes `&mut Terminal`. The race occurs because `get_dimensions()` and `get_lines()` are called as separate `self.terminal.lock()` acquisitions in `LocalPane` (localpane.rs:376 and 368 respectively). Between these two calls, the resize worker can complete.

**Severity:** Medium. Occurs when render and resize race, probability proportional to resize frequency and render frame rate.

### F3: Tab Fanout Split Inconsistency (Cause of `A2`, `A3` in multi-pane)

**Location:** `frankenterm/mux/src/tab.rs` (`apply_sizes_from_splits`)

**Mechanism:**
Tab resize fans out to multiple panes via crossbeam scoped threads (bounded parallelism from wa-1u90p.5.2). Each pane's resize goes through the queue independently:
```
Tab::resize → TabInner::resize → apply_sizes_from_splits
  ├── pane_1.resize(new_size_1)  → enqueue → worker thread
  ├── pane_2.resize(new_size_2)  → enqueue → worker thread
  └── pane_3.resize(new_size_3)  → enqueue → worker thread
```

There is no atomic "all panes resized" completion barrier before the next render frame. The render can observe:
- Pane 1: already resized to new dimensions
- Pane 2: still at old dimensions
- Pane 3: mid-reflow

This creates transient layout inconsistency where split proportions don't match across panes.

**Severity:** Low-Medium. Visible only in multi-pane layouts during resize storms. The bounded parallelism from wa-1u90p.5.2 reduces the window but doesn't eliminate it.

### F4: Stale Intent Commit Under Storm Conditions (Cause of `A2`)

**Location:** Pre-ADR-0011 resize path

**Mechanism:**
Under rapid resize storms (e.g., window drag), the resize queue can accumulate multiple pending intents. Without the cancellation semantics from ADR-0011:
```
t0: submit resize(80x24)  → queued
t1: submit resize(90x24)  → queued
t2: submit resize(100x24) → queued
t3: worker picks resize(80x24) → begins execution
t4: resize(80x24) commits to presentation  ← STALE, user wanted 100x24
t5: worker picks resize(90x24) → begins execution
t6: resize(90x24) commits ← STALE
t7: worker picks resize(100x24) → begins execution
t8: resize(100x24) commits ← finally correct
```

Between t4 and t8, the user sees two transient wrong-size frames.

**Current mitigation:** The resize queue in `localpane.rs` already implements coalescing (`enqueue` replaces pending intent) and cancellation token checks (`superseded_by` at localpane.rs:1313, 1363). This partially mitigates F4 but doesn't cover the case where a worker is mid-execution when a new intent arrives and the boundary check doesn't fire in time.

**Severity:** Medium. Partially mitigated by existing coalescing. ADR-0011's boundary cancellation at every phase transition would fully address this.

### F5: Cursor Position Transient During Dual-Screen Resize (Cause of `A4`)

**Location:** `frankenterm/term/src/terminalstate/mod.rs:852` (`TerminalState::resize`)

**Mechanism:**
```rust
pub fn resize(&mut self, size: TerminalSize) {
    self.increment_seqno();
    // Determine cursor positions for main and alt screens
    let (cursor_main, cursor_alt) = if self.screen.alt_screen_is_active {
        (saved_cursor_position, self.cursor)
    } else {
        (self.cursor, saved_alt_cursor_position)
    };
    // Resize both screens together
    let (adjusted_main, adjusted_alt) = self.screen.resize(...);
    // Apply adjusted cursor positions
    // ...
}
```

The resize adjusts cursor positions for both screens simultaneously, but:
- The adjusted cursor is applied via `set_cursor_pos` which may trigger side effects
- Between `resize()` completing and the cursor being fully applied, a read of cursor position may return intermediate values
- This is contained within a single terminal mutex hold, so it's only visible if something reads cursor state during the resize operation itself (unlikely but possible via callback/hook paths)

**Severity:** Low. Contained within single mutex hold. Only manifests if cursor state is read during the resize operation via a callback or hook.

## Fault Interaction Matrix

| Fault | Artifact Classes | Frequency | Mitigation Complexity |
|---|---|---|---|
| F1 (PTY-terminal gap) | `A1` | Every resize | Medium (requires atomic PTY+terminal resize) |
| F2 (non-atomic render) | `A2`, `A3` | Per render frame during resize | Medium (requires snapshot-based rendering) |
| F3 (tab fanout) | `A2`, `A3` | Multi-pane resize storms | Low (completion barrier) |
| F4 (stale commit) | `A2` | Resize storms only | Low (ADR-0011 boundary checks) |
| F5 (cursor transient) | `A4` | Rare | Already contained |

## Mitigation Design

### M1: Atomic PTY+Terminal Resize (Addresses F1)

**Approach:** Hold the terminal lock across both PTY resize and terminal resize, eliminating the window where dimensions are inconsistent.

**Trade-off:** This increases terminal lock hold time by the PTY resize duration. The lock graph (wa-1u90p.5.7) shows these are currently non-overlapping. Making them overlapping means render reads will block during the entire resize operation.

**Alternative approach (preferred):** Instead of extending the lock hold, implement a "resize pending" flag on the terminal. When set:
- The terminal accepts output but buffers it without presentation
- PTY resize completes
- Terminal resize completes and processes buffered output
- Flag is cleared and presentation is released

This is consistent with ADR-0011's Phase B (`prepare -> reflow -> present`) model where presentation is the commit point.

### M2: Snapshot-Based Rendering (Addresses F2)

**Approach:** Instead of two separate mutex acquisitions for dimensions and lines, create an atomic render snapshot:

```rust
fn render_snapshot(&self) -> RenderSnapshot {
    let terminal = self.terminal.lock();
    RenderSnapshot {
        dimensions: terminal_get_dimensions(&mut terminal),
        lines: /* extract lines while holding lock */,
        cursor: /* cursor position */,
    }
}
```

This ensures dimensions and content are always from the same terminal state.

**Trade-off:** Increases terminal lock hold time by combining two operations. Snapshot memory allocation could add latency.

**Alternative:** Use a generation counter (seqno) to detect stale reads and skip rendering that frame.

### M3: Tab Resize Completion Barrier (Addresses F3)

**Approach:** After `apply_sizes_from_splits` completes all pane resize enqueues, wait for all resize workers to finish before allowing the next render frame.

**Trade-off:** This would stall rendering during resize, creating frame drops. A better approach is to use the ADR-0011 transaction model where the tab-level resize is a compound transaction that commits atomically.

### M4: Full ADR-0011 Implementation (Addresses F4, partially F1-F3)

The two-phase transaction model from ADR-0011 addresses F4 directly through:
- Latest-intent wins with monotonic `intent_seq`
- Boundary cancellation checks between `Preparing/Reflowing/Presenting`
- Stale-commit prevention via `active_seq == latest_seq` guard at commit

It also creates the architectural foundation for addressing F1-F3 by establishing clear phase boundaries where render reads can be coordinated.

## Recommended Execution Order

1. **M4 first** (ADR-0011 transaction model): Foundation for all other mitigations. Eliminates F4, provides cancellation infrastructure. Maps to `wa-1u90p.2.2` (per-pane coalescer + cancellation tokens).

2. **M1 second** (atomic PTY+terminal resize): Eliminates F1 stretched-text, the highest-frequency artifact. Can leverage M4's phase model for buffering. Maps to new subtask under `wa-1u90p.5`.

3. **M2 third** (snapshot rendering): Eliminates F2 non-atomic reads. Maps to `wa-1u90p.4.2` (last-good-frame hold and atomic frame swap).

4. **M3 last** (tab completion barrier): Lowest priority since F3 is multi-pane only. Can be folded into M4's compound transaction model.

## Validation Contract

Each mitigation should be validated against:
1. The simulation resize suite (`cargo test -p frankenterm-core --test simulation_resize_suite`)
2. Runtime telemetry (lock wait/hold percentiles should not regress)
3. Artifact incidence counting (requires wa-1u90p.7.9 visual detector pipeline)
4. Manual resize-storm reproduction on all workload classes (R1-R4)

## Cross-References

- ADR-0011: `docs/adr/0011-resize-transaction-state-machine.md`
- Lock graph: `docs/resize-lock-graph-wa-1u90p.5.7.md`
- Bottleneck dossier: `docs/resize-baseline-bottleneck-dossier.md`
- SLO contract: `docs/resize-performance-slos.md`
- Profiling evidence: `docs/resize-lock-memory-profile-wa-1u90p.1.3.md`
- Timeline rollup: `evidence/wa-1u90p.1.3/summaries/resize_baseline_timeline_rollup_2026-02-14.json`
