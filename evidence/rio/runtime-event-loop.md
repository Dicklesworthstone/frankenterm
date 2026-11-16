# Rio Runtime/Event-Loop Analysis

Bead: `ft-34sko.4`  
Scope: `legacy_rio/rio`

## Runtime Orchestration (End-to-End)

### Startup and loop ownership

- `main` performs CLI parse, config load/override, logging, and env setup before building the event loop (`legacy_rio/rio/frontends/rioterm/src/main.rs:133`, `legacy_rio/rio/frontends/rioterm/src/main.rs:155`, `legacy_rio/rio/frontends/rioterm/src/main.rs:232`).
- Event loop is created as `EventLoop<EventPayload>` and injected into `Application::new`; execution enters `event_loop.run_app(self)` via `Application::run` (`legacy_rio/rio/frontends/rioterm/src/main.rs:235`, `legacy_rio/rio/frontends/rioterm/src/main.rs:239`, `legacy_rio/rio/frontends/rioterm/src/application.rs:170`).
- `Application` is the control-plane root and owns `Router`, `Scheduler`, config, and event proxy (`legacy_rio/rio/frontends/rioterm/src/application.rs:33`).

### Event-loop phases and scheduling

- `new_events` handles startup causes and creates initial route/window, then installs periodic title timer (`legacy_rio/rio/frontends/rioterm/src/application.rs:182`, `legacy_rio/rio/frontends/rioterm/src/application.rs:195`, `legacy_rio/rio/frontends/rioterm/src/application.rs:209`).
- `Scheduler` is deadline-ordered (`VecDeque`) and emits due events through `EventLoopProxy` (`legacy_rio/rio/frontends/rioterm/src/scheduler.rs:44`, `legacy_rio/rio/frontends/rioterm/src/scheduler.rs:61`).
- `about_to_wait` sets `ControlFlow::WaitUntil(next_deadline)` or `Wait` from scheduler state (`legacy_rio/rio/frontends/rioterm/src/application.rs:1443`).

### Control-flow diagram

```text
main
  -> Config + env + logging
  -> EventLoop<EventPayload>::build
  -> Application::new(config, proxy, router, scheduler)
  -> run_app()
       -> new_events(Init/CreateWindow)
            -> Router::create_window
            -> ContextManager::create_context
                 -> create_pty_* + Machine::spawn (PTY thread)
       -> user_event(RioEvent::*)
            -> mutate state + request/schedule redraw
       -> window_event(WindowEvent::*)
            -> input routing / resize / redraw
       -> about_to_wait()
            -> Scheduler::update -> Wait/WaitUntil
```

## Input Routing and Ordering Semantics

### Input paths

- Keyboard: `WindowEvent::KeyboardInput` -> `screen.process_key_event` (`legacy_rio/rio/frontends/rioterm/src/application.rs:1204`, `legacy_rio/rio/frontends/rioterm/src/screen/mod.rs:552`).
- IME: commit/preedit handled in `WindowEvent::Ime`; preedit change can trigger redraw (`legacy_rio/rio/frontends/rioterm/src/application.rs:1232`).
- Mouse: selection/hints/mouse-report logic in `window_event` mouse branches (`legacy_rio/rio/frontends/rioterm/src/application.rs:900`, `legacy_rio/rio/frontends/rioterm/src/application.rs:1161`).
- Output to PTY is always via `Messenger`: `send_write` => `Msg::Input`, `send_resize` => `Msg::Resize` (`legacy_rio/rio/frontends/rioterm/src/messenger.rs:20`, `legacy_rio/rio/frontends/rioterm/src/messenger.rs:31`).

### Explicit ordering guarantees observed

1. `WindowEvent` and `UserEvent` handlers mutate terminal/render state before they request or schedule redraw.
2. Actual drawing happens only during `WindowEvent::RedrawRequested`, never from PTY/parser thread (`legacy_rio/rio/frontends/rioterm/src/application.rs:1350`).
3. Non-game mode re-schedules only if `pending_update.is_dirty()`, preserving render coalescing (`legacy_rio/rio/frontends/rioterm/src/application.rs:1418`).
4. Timer dispatch is deadline-ordered, so scheduling order is deterministic for equal topic/id semantics (`legacy_rio/rio/frontends/rioterm/src/scheduler.rs:78`, `legacy_rio/rio/frontends/rioterm/src/scheduler.rs:104`).

## PTY Output Flow, Boundaries, and Concurrency

### Thread/task boundaries

- UI thread: `ApplicationHandler` callbacks, router, scheduler, rendering.
- PTY performer thread: `Machine::spawn` poll loop handles channel input + PTY read/write (`legacy_rio/rio/rio-backend/src/performer/mod.rs:308`).
- Config watcher thread: filesystem watch emits `PrepareUpdateConfig` (`legacy_rio/rio/frontends/rioterm/src/watcher.rs:19`).

### PTY -> parser -> wakeup path

- `ContextManager::create_context` builds `Crosswords`, PTY, machine, and messenger (`legacy_rio/rio/frontends/rioterm/src/context/mod.rs:209`, `legacy_rio/rio/frontends/rioterm/src/context/mod.rs:287`, `legacy_rio/rio/frontends/rioterm/src/context/mod.rs:301`).
- PTY thread drains `Msg::{Input,Resize}` and applies writes/resizes (`legacy_rio/rio/rio-backend/src/performer/mod.rs:238`, `legacy_rio/rio/rio-backend/src/performer/mod.rs:272`).
- PTY reads feed `Processor` (`BatchedParser` + sync-update handling) (`legacy_rio/rio/rio-backend/src/performer/mod.rs:168`, `legacy_rio/rio/rio-backend/src/performer/handler.rs:485`).
- After non-sync parse work, performer emits `RioEvent::Wakeup(route_id)`; UI thread then marks dirty and schedules redraw (`legacy_rio/rio/rio-backend/src/performer/mod.rs:221`, `legacy_rio/rio/frontends/rioterm/src/application.rs:304`).

### Boundary diagram

```text
Input (kbd/mouse/IME)
  -> Screen / ContextManager
  -> Messenger::send_* (Msg::Input/Resize)
  -> PTY thread (Machine poll loop)
       -> pty_write / pty_read
       -> Processor::advance (batched parse + sync mode)
       -> EventProxy::send_event(Wakeup)
  -> UI thread user_event(Wakeup)
       -> mark pending_update dirty
       -> schedule/request redraw
  -> window_event(RedrawRequested)
       -> render + present
```

## FrankenTerm Improvements (Actionable + Migration Notes)

### Improvement 1: Typed control-plane reducer for event ordering

- Proposal: replace ad-hoc callback mutation with reducer-style state transitions keyed by typed events (`InputReceived`, `PtyWakeup`, `RenderScheduled`, `FramePresented`).
- Migration notes:
  - Keep Rio’s `EventPayload` boundary pattern but add `event_id`, `ordering_index`, and reducer transition reason.
  - Preserve “parse/mutate then render-on-redraw” invariant.
- Downstream validation:
  - Unit tests: reducer transition matrix and illegal-transition rejection.
  - Integration tests: event ordering across keyboard + wakeup + redraw sequences.
  - E2E: interactive typing + burst output scenario verifies deterministic ordering trace.

### Improvement 2: Explicit backpressure and queue observability

- Proposal: instrument messenger write queues and scheduler timer queue with bounded policies and telemetry.
- Migration notes:
  - Keep current coalescing behavior (`Wakeup`, `RenderRoute`, `WaitUntil`) but expose queue depth, dropped/coalesced counters, and enqueue latency.
  - Add policy gates for “defer render under overload” vs “force render on critical events”.
- Downstream validation:
  - Unit tests: queue depth accounting and saturation policy selection.
  - Integration tests: high-output burst preserves no-crash/no-deadlock with expected coalescing.
  - E2E: high-output pane churn script validates bounded queue growth and stable frame cadence.

### Improvement 3: Per-window frame pacing contract with platform adapters

- Proposal: codify pacing semantics into a platform adapter contract (`cvdisplaylink`, monitor-refresh-derived interval, fallback fixed interval).
- Migration notes:
  - Keep macOS direct redraw behavior and non-macOS `wait_until` schedule model but formalize as explicit strategy selection.
  - Emit strategy + interval in logs for reproducibility.
- Downstream validation:
  - Unit tests: pacing strategy selection by platform/monitor metadata.
  - Integration tests: scheduler + redraw interactions under resize/focus/occlusion transitions.
  - E2E: resize storm + occlusion/unocclusion script checks frame recovery and no starvation.

### Improvement 4: Sync-update safety envelope in ingest pipeline

- Proposal: make sync-update timeout/buffer budget a first-class ingest safeguard with explicit error signatures.
- Migration notes:
  - Retain Rio’s synchronized update timeout/buffer approach, but surface timeouts, forced flushes, and fallback mode as structured events.
  - Couple to backpressure controller for overload decisions.
- Downstream validation:
  - Unit tests: sync buffer overflow and timeout transitions.
  - Integration tests: parser emits wakeup/fallback in expected order after forced sync termination.
  - E2E: synthetic sync-stall transcript verifies bounded latency and recovery.

## Required Structured Logging Artifacts (Downstream E2E)

For all runtime/event-loop downstream scripts, emit JSONL rows with at least:

- `run_id`
- `scenario_id`
- `event_id`
- `pane_id` or `window_id`
- `ordering_index`
- `scheduler_decision` (`immediate`, `deferred`, `coalesced`, `dropped`)
- `queue_depth` (at decision time)
- `latency_input_to_mutation_ms`
- `latency_mutation_to_wakeup_ms`
- `latency_wakeup_to_redraw_ms`
- `latency_redraw_to_present_ms`
- `latency_ms` (end-to-end)
- `failure_signatures` (array of stable codes)
- `outcome`
- `error_code`

Example row:

```json
{"run_id":"run-20260218-001","scenario_id":"burst-output-01","event_id":"evt-9981","window_id":"w-3","ordering_index":1422,"scheduler_decision":"coalesced","queue_depth":7,"latency_input_to_mutation_ms":0.21,"latency_mutation_to_wakeup_ms":1.94,"latency_wakeup_to_redraw_ms":8.11,"latency_redraw_to_present_ms":2.03,"latency_ms":12.29,"failure_signatures":[],"outcome":"ok","error_code":null}
```

## Mandatory Downstream E2E Contract

- Script path: `tests/e2e/rio/test_runtime_event_loop.sh`
- Fixture root: `fixtures/rio/runtime_loop`
- Artifact path: `e2e-artifacts/rio/runtime_event_loop/<run_id>.jsonl`
- Required row fields: `run_id`, `scenario_id`, `event_id`, `ordering_index`, `scheduler_decision`, `queue_depth`, `latency_ms`, `outcome`, `error_code`

Suggested mandatory scenarios:

1. `interactive-input-ordering` (keyboard + IME + redraw sequencing)
2. `high-output-coalescing` (rapid PTY output with wakeup coalescing)
3. `resize-occlusion-recovery` (resize storms + occlusion toggle + frame recovery)
4. `sync-update-timeout` (sync buffer timeout and fallback path)

## Cross-References

- `ft-1u90p.2` (control-plane scheduling/coalescing)
- `ft-283h4.4` (stream ingest/pipeline throughput)
