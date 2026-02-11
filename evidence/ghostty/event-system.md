# Ghostty event system analysis (change propagation + notifications)

Bead: `wa-3bja.4`

This doc traces how Ghostty propagates terminal state changes to the renderer/UI and other consumers, then contrasts that with the vendored WezTerm mux notification model in this repo. The goal is to extract concrete patterns FrankenTerm can adopt to reduce lock contention and notification storms.

Scope:
- Ghostty: `legacy_ghostty/src/termio/Termio.zig`, `legacy_ghostty/src/termio/Thread.zig`, `legacy_ghostty/src/termio/mailbox.zig`, `legacy_ghostty/src/renderer/Thread.zig`, `legacy_ghostty/src/terminal/render.zig`, `legacy_ghostty/src/terminal/PageList.zig`
- WezTerm (vendored): `frankenterm/mux/src/lib.rs` (MuxNotification + subscribers fanout)
- FrankenTerm: `crates/frankenterm-core/src/runtime.rs` (native output coalescer; event bus)

---

## 1) Change propagation (Ghostty)

### 1.1 The hot path: write dirty bits, poke the renderer

- PTY output is processed under a single shared mutex: `Termio.processOutput` locks `renderer_state.mutex` and calls `processOutputLocked` (`legacy_ghostty/src/termio/Termio.zig:660-665`).
- `processOutputLocked` schedules a render *before* parsing by calling `terminal_stream.handler.queueRender()` (`legacy_ghostty/src/termio/Termio.zig:668-672`).
- `queueRender` is a thin wrapper around an async wakeup: `renderer_wakeup.notify()` (`legacy_ghostty/src/termio/stream_handler.zig:101-106`).
- Terminal mutations mark fine-grained dirty state:
  - Row-level dirty is `PageList.Pin.markDirty -> row.dirty = true` (`legacy_ghostty/src/terminal/PageList.zig:5077-5080`).
  - Render state consumes both page-level + row-level dirty during `RenderState.update` (`legacy_ghostty/src/terminal/render.zig:391-505`).

Key design: terminal state is the source of truth; the renderer is a pull-based consumer that re-samples state on wakeup.

### 1.2 Renderer thread: drain mailbox then render immediately

- Renderer thread `wakeup` async is explicitly described as coalescing: `draw_now` “does not coalesce like the wakeup does” (`legacy_ghostty/src/renderer/Thread.zig:64-66`).
- On wakeup, Ghostty:
  1) drains renderer mailbox (`t.drainMailbox()`),
  2) calls `renderCallback` immediately (update frame + draw) (`legacy_ghostty/src/renderer/Thread.zig:515-535`).

The “render now” path avoids additional scheduling layers; coalescing is delegated to the async wakeup semantics.

### 1.3 Dirty consumption semantics (no “sticky dirty”)

Ghostty treats dirty flags as *consumed* by rendering:
- RenderState decides `redraw` when terminal dirty bits or screen dirty bits are set (`legacy_ghostty/src/terminal/render.zig:269-303`).
- It checks page-level dirty and row-level dirty to decide which rows to rebuild (`legacy_ghostty/src/terminal/render.zig:391-505`).
- At the end of the update, it clears terminal and screen dirty bits: `t.flags.dirty = .{}; s.dirty = .{};` (`legacy_ghostty/src/terminal/render.zig:645-647`).

This makes the “notification” effectively level-triggered by the wakeup plus edge-triggered by dirties, with no need for separate subscriber state.

### 1.4 Control-plane messages vs data-plane dirties

Ghostty uses explicit mailboxes for control signals that aren’t naturally represented by dirty bits:
- Termio pushes renderer messages (e.g. cursor blink reset) to `renderer_mailbox` (`legacy_ghostty/src/termio/Termio.zig:684-687`).
- The renderer wakeup causes the mailbox to be drained before rendering (`legacy_ghostty/src/renderer/Thread.zig:528-534`).

This separates “big data” (terminal state + dirty bits) from “small control messages” (mailbox).

---

## 2) Lock contention + synchronization (Ghostty)

### 2.1 Single mutex instead of multi-reader fanout

Ghostty’s renderer reads terminal state under a single `std.Thread.Mutex` (not an RwLock):
- Renderer update frame enters a tight critical section: `state.mutex.lock(); ... terminal_state.update(...); ... unlock` (`legacy_ghostty/src/renderer/generic.zig:1160-1176`).

This deliberately serializes terminal writes and renderer reads, but simplifies correctness and avoids the overhead of frequent RwLock transitions.

### 2.2 Bounded mailboxes + “unlock-on-backpressure”

Ghostty’s termio mailbox is a bounded SPSC `BlockingQueue(..., 64)` with a very explicit “don’t deadlock under load” strategy:
- If a send would block, it wakes the writer thread and temporarily unlocks the renderer mutex to let the writer proceed, then blocks until push succeeds (`legacy_ghostty/src/termio/mailbox.zig:55-93`).

Similarly, `surface_mailbox.push(..., .instant)` failure triggers an unlock/retry pattern (`legacy_ghostty/src/termio/stream_handler.zig:125-135`).

This is a concrete strategy for handling notification storms: apply backpressure, but ensure backpressure does not induce lock inversion.

---

## 3) Notification batching / coalescing (Ghostty)

Ghostty coalesces in multiple places, not just at the renderer async:

### 3.1 “One redraw per drain” in the IO writer thread

The termio writer thread drains its mailbox and triggers exactly one redraw per drain:
- `drainMailbox` loops `while (mailbox.pop()) ...` and only once at the end calls `io.renderer_wakeup.notify()` (`legacy_ghostty/src/termio/Thread.zig:288-362`).

### 3.2 Timed coalescing for resize events

Resize is explicitly coalesced:
- `Coalesce.min_ms = 25` (`legacy_ghostty/src/termio/Thread.zig:27-33`).
- Resize messages set `coalesce_data.resize` and arm a timer; repeated resize messages within the window do not restart work (`legacy_ghostty/src/termio/Thread.zig:376-392`).

### 3.3 Synchronized output as a batching mode

Renderer updateFrame short-circuits when synchronized output mode is active:
- It checks `state.terminal.modes.get(.synchronized_output)` and skips render (`legacy_ghostty/src/renderer/generic.zig:1163-1167`).

The IO thread has a timer guard (`sync_reset_ms = 1000`) to avoid a “frozen forever” terminal if the application misbehaves (`legacy_ghostty/src/termio/Thread.zig:35-38`, `364-374`).

---

## 4) Contrast: vendored WezTerm mux notifications in this repo

WezTerm’s model (as vendored here) is structurally different:

### 4.1 Output notification is a fan-out event

After parsing output actions and applying them to a pane, WezTerm notifies the global mux:
- `send_actions_to_mux` calls `Mux::notify_from_any_thread(MuxNotification::PaneOutput(...))` (`frankenterm/mux/src/lib.rs:120-129`).

### 4.2 Subscriber callbacks run under a global lock

`Mux::notify`:
- takes a write lock on `subscribers`,
- runs subscriber closures inside `retain` while holding the lock (`frankenterm/mux/src/lib.rs:702-705`).

### 4.3 Cross-thread notification can amplify storms

When not on the main thread, notify spawns into the main thread (`promise::spawn::spawn_into_main_thread(...)`) (`frankenterm/mux/src/lib.rs:707-719`).

Under heavy output, this becomes “N output batches ⇒ N spawned main-thread tasks ⇒ N global lock acquisitions ⇒ N callback executions”.

Ghostty’s analogous path is “N terminal mutations ⇒ (coalesced) wakeup ⇒ 1 drain + 1 render”, with control-plane messages drained before rendering.

---

## 5) FrankenTerm opportunities (actionable ideas)

These are framed as “Ghostty pattern → Why it matters → Where to apply in ft”.

### Idea A — Make coalesced wakeups the default for high-frequency events

Ghostty’s renderer wakeup is coalescing (`legacy_ghostty/src/renderer/Thread.zig:64-66`) and the IO writer drains-and-notifies once per drain (`legacy_ghostty/src/termio/Thread.zig:288-362`).

In ft:
- We already coalesce native pane output deltas via `NativeOutputCoalescer` (`crates/frankenterm-core/src/runtime.rs:55-92`).
- Extend this “coalesced wakeup” idea to other internal high-rate paths: detections → workflows, segment persistence → indexing, UI notifications → desktop notify.

Concrete direction: prefer `tokio::sync::Notify` + per-pane `AtomicBool pending` (or `watch` channels) over “send a message per micro-event” patterns.

### Idea B — Separate “data-plane” deltas from “control-plane” messages

Ghostty uses:
- dirty state in the terminal model for bulk changes (`legacy_ghostty/src/terminal/render.zig:391-505`),
- small bounded mailboxes for control messages (`legacy_ghostty/src/termio/Termio.zig:684-687`, `legacy_ghostty/src/renderer/Thread.zig:528-534`).

In ft terms:
- data-plane: captured output deltas/segments (already),
- control-plane: pane lifecycle, title changes, prompt markers, state transitions, policy decisions.

This directly supports `wa-3dfxb.13` (native event hooks): design the native socket protocol with separate message kinds + separate coalescing/backpressure knobs.

### Idea C — Never run subscriber callbacks while holding a global lock

WezTerm mux currently executes callbacks under the `subscribers` write lock (`frankenterm/mux/src/lib.rs:702-705`).

If/when ft needs in-process fan-out (beyond the existing `EventBus`), prefer:
- snapshot subscriber list under lock, then drop the lock before invoking,
- or use per-subscriber channels/broadcast so publisher never blocks on slow consumers.

This is a general scalability guardrail for “notification storms”.

### Idea D — Explicit backpressure strategies should unlock critical locks

Ghostty’s mailbox send path is explicit about unlocking a mutex if a bounded queue fills (`legacy_ghostty/src/termio/mailbox.zig:55-93`).

In ft:
- if persistence, detection, or workflow channels are bounded (they should be), define what happens when they fill:
  - drop (best-effort),
  - coalesce (merge),
  - or block (but never while holding a pane/cursor lock).
- make “unlock-on-backpressure” a documented invariant for hot paths.

### Idea E — Borrow Ghostty’s “consumed dirty” semantics for downstream incremental work

Ghostty’s renderer consumes dirty state and clears it (`legacy_ghostty/src/terminal/render.zig:645-647`), and even feeds a “viewport dirty” hint back for search (`legacy_ghostty/src/renderer/generic.zig:1172-1176`).

In ft:
- treat per-pane “work needed” flags (persist/index/detect/workflow) as consumed edges:
  - set dirty when new output arrives,
  - downstream stage clears dirty when processed,
  - avoid repeated rescans when no new data exists.

This aligns with `wa-x4rq` and with event bus metrics already tracked in `crates/frankenterm-core/src/metrics.rs`.

---

## Notes / open questions

- xev’s async coalescing semantics are central to Ghostty’s simplicity; any Rust analogue should ensure similar behavior (Notify + pending flag; `watch` channel; `mpsc` with `try_send` + gating).
- Ghostty’s “single mutex” approach works because there is essentially one renderer consumer; ft’s architecture has multiple consumers (persist, detect, workflows), so the “mailbox + coalesced wakeup” pattern is the more portable takeaway than the mutex choice itself.

