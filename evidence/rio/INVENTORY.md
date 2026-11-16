# Rio Architectural Inventory

Bead: `ft-34sko.1`  
Workspace scanned: `legacy_rio/rio`

## 1. Workspace + Crate Structure

Rio is a Cargo workspace with eight core members (`legacy_rio/rio/Cargo.toml:2`):

1. `frontends/rioterm` (binary app)
2. `rio-backend` (terminal/backend core library)
3. `sugarloaf` (WebGPU renderer)
4. `rio-window` (cross-platform window/event abstraction)
5. `teletypewriter` (PTY/process abstraction)
6. `copa` (ANSI parser state machine)
7. `corcovado` (non-blocking I/O poller/evented primitives)
8. `rio-proc-macros` (proc-macro support)

### Crate role map

| Crate | Role | Primary anchors |
|---|---|---|
| `frontends/rioterm` | Desktop terminal application, event-loop integration, routing, input handling, render orchestration | `legacy_rio/rio/frontends/rioterm/src/main.rs:133`, `legacy_rio/rio/frontends/rioterm/src/application.rs:33`, `legacy_rio/rio/frontends/rioterm/src/screen/mod.rs:77`, `legacy_rio/rio/frontends/rioterm/src/router/mod.rs:273` |
| `rio-backend` | Terminal model + event schema + performer/PTY machine + config | `legacy_rio/rio/rio-backend/src/lib.rs:1`, `legacy_rio/rio/rio-backend/src/event/mod.rs:24`, `legacy_rio/rio/rio-backend/src/crosswords/mod.rs:405`, `legacy_rio/rio/rio-backend/src/performer/mod.rs:63` |
| `sugarloaf` | Rendering engine (scene/object composition and frame submission over WGPU) | `legacy_rio/rio/sugarloaf/src/sugarloaf.rs:24`, `legacy_rio/rio/sugarloaf/src/sugarloaf.rs:363`, `legacy_rio/rio/sugarloaf/src/context/mod.rs:4` |
| `rio-window` | Winit-derived platform abstraction for windows, events, event loops | `legacy_rio/rio/rio-window/src/lib.rs:1`, `legacy_rio/rio/rio-window/src/application.rs:9`, `legacy_rio/rio/rio-window/src/event_loop.rs:44`, `legacy_rio/rio/rio-window/src/event.rs:59` |
| `teletypewriter` | Platform PTY creation + read/write + resize + child-exit integration | `legacy_rio/rio/teletypewriter/src/lib.rs:23`, `legacy_rio/rio/teletypewriter/src/unix/mod.rs:397`, `legacy_rio/rio/teletypewriter/src/windows/mod.rs:35` |
| `copa` | ANSI/VT parser implementation with `Perform` callback contract | `legacy_rio/rio/copa/src/lib.rs:57`, `legacy_rio/rio/copa/src/lib.rs:828` |
| `corcovado` | Mio-like evented I/O poll + channels/timers used by PTY machine | `legacy_rio/rio/corcovado/src/lib.rs:1`, `legacy_rio/rio/corcovado/src/poll.rs:328` |
| `rio-proc-macros` | Proc macro utilities (state-change table generation used by parser stack) | `legacy_rio/rio/rio-proc-macros/src/lib.rs:1` |

## 2. High-Level Runtime Architecture

### End-to-end flow

1. `rioterm` boots CLI/config/logging and creates a `rio-window` event loop (`legacy_rio/rio/frontends/rioterm/src/main.rs:235`).
2. `Application` owns routing + scheduling + event handling and implements `ApplicationHandler<EventPayload>` (`legacy_rio/rio/frontends/rioterm/src/application.rs:33`, `legacy_rio/rio/frontends/rioterm/src/application.rs:179`).
3. `Router` manages one or more `RouteWindow`s (terminal/welcome/assistant/confirm-quit routes) (`legacy_rio/rio/frontends/rioterm/src/router/mod.rs:273`).
4. Each `Screen` contains:
   - `ContextManager` (terminal contexts/splits/tabs)
   - `Renderer` (scene construction)
   - `Sugarloaf` (GPU submission)  
   Anchors: `legacy_rio/rio/frontends/rioterm/src/screen/mod.rs:77`, `legacy_rio/rio/frontends/rioterm/src/screen/mod.rs:2581`.
5. PTY IO is handled in backend performer `Machine`, which parses terminal output and emits wake/render events (`legacy_rio/rio/rio-backend/src/performer/mod.rs:308`).

### Core layer boundaries

- **Frontend orchestration**: `frontends/rioterm`
- **Terminal state + parser/performer**: `rio-backend` + `copa`
- **Render backend**: `sugarloaf` (+ `wgpu`)
- **Window/events**: `rio-window`
- **PTY/process and polling**: `teletypewriter` + `corcovado`

## 3. Key Abstractions

### 3.1 Terminal model (crosswords, parser, performer boundary)

1. `copa::Parser` is the VT parser state machine (`legacy_rio/rio/copa/src/lib.rs:57`).
2. `performer::handler::Processor` wraps parser behavior (including synchronized-update handling) and exposes `advance` / `stop_sync` (`legacy_rio/rio/rio-backend/src/performer/handler.rs:485`, `legacy_rio/rio/rio-backend/src/performer/handler.rs:503`).
3. `Performer` implements `copa::Perform`, translating parser actions into `Handler` calls (`legacy_rio/rio/rio-backend/src/performer/handler.rs:658`).
4. `Crosswords<U>` is the terminal state model and implements `Handler` (`legacy_rio/rio/rio-backend/src/crosswords/mod.rs:405`, `legacy_rio/rio/rio-backend/src/crosswords/mod.rs:1388`).
5. Damage model is explicit via `TerminalDamage` (`Full`/`Partial`/`CursorOnly`) with `peek_damage_event` and `reset_damage` (`legacy_rio/rio/rio-backend/src/crosswords/mod.rs:560`, `legacy_rio/rio/rio-backend/src/crosswords/mod.rs:588`).

Interpretation: parser and terminal semantics are intentionally decoupled. `copa` parses bytes, `Processor` sequences them, `Crosswords` owns terminal semantics/state.

### 3.2 Event model (EventPayload, listener, routing flow)

1. Event envelope: `RioEventType`, `RioEvent`, `EventPayload` in backend (`legacy_rio/rio/rio-backend/src/event/mod.rs:24`, `legacy_rio/rio/rio-backend/src/event/mod.rs:61`, `legacy_rio/rio/rio-backend/src/event/mod.rs:239`).
2. Event transport trait: `EventListener`; concrete loop bridge: `EventProxy` using `rio_window::EventLoopProxy` (`legacy_rio/rio/rio-backend/src/event/mod.rs:262`, `legacy_rio/rio/rio-backend/src/event/mod.rs:290`).
3. Application receives these user events in `ApplicationHandler::user_event` (`legacy_rio/rio/frontends/rioterm/src/application.rs:220`).
4. `Scheduler` manages delayed/repeating events for render, config update, cursor blinking, title updates (`legacy_rio/rio/frontends/rioterm/src/scheduler.rs:25`, `legacy_rio/rio/frontends/rioterm/src/scheduler.rs:44`).
5. `Machine::pty_read` and synchronized-update timeout paths emit `RioEvent::Wakeup` to trigger coalesced rendering (`legacy_rio/rio/rio-backend/src/performer/mod.rs:228`, `legacy_rio/rio/rio-backend/src/performer/mod.rs:359`).

Interpretation: Rio uses a typed internal event bus (`RioEvent`) carried through `rio-window` user events, then routed by `Application` + `Router`.

### 3.3 Renderer model (scene/frame flow frontend -> Sugarloaf)

1. `WindowEvent::RedrawRequested` in `Application` is the frame boundary (`legacy_rio/rio/frontends/rioterm/src/application.rs:1350`).
2. For terminal routes, `screen.render()` is invoked (`legacy_rio/rio/frontends/rioterm/src/application.rs:1364`).
3. `Screen::render` delegates to `Renderer::run` to build objects and compute damage-aware updates (`legacy_rio/rio/frontends/rioterm/src/screen/mod.rs:2547`, `legacy_rio/rio/frontends/rioterm/src/screen/mod.rs:2581`, `legacy_rio/rio/frontends/rioterm/src/renderer/mod.rs:832`).
4. `Renderer::run` composes navigation/search/context objects, calls `sugarloaf.set_objects`, then `sugarloaf.render()` (`legacy_rio/rio/frontends/rioterm/src/renderer/mod.rs:1133`, `legacy_rio/rio/frontends/rioterm/src/renderer/mod.rs:1152`).
5. `Sugarloaf::render` encodes GPU passes (layers/quads/rich-text/filters) and presents the frame (`legacy_rio/rio/sugarloaf/src/sugarloaf.rs:363`).
6. GPU context setup and surface/device configuration live in `sugarloaf::Context` (`legacy_rio/rio/sugarloaf/src/context/mod.rs:4`, `legacy_rio/rio/sugarloaf/src/context/mod.rs:227`).

Interpretation: screen/model mutation and scene construction stay in frontend/backend crates; actual draw/present is fully isolated in Sugarloaf.

## 4. Dependency + Feature Analysis

### 4.1 Platform feature flags

- `frontends/rioterm` default features include `x11` + `wayland` and forward to backend/window crates (`legacy_rio/rio/frontends/rioterm/Cargo.toml:75`, `legacy_rio/rio/frontends/rioterm/Cargo.toml:78`, `legacy_rio/rio/frontends/rioterm/Cargo.toml:82`).
- `rio-backend` also exposes `x11` / `wayland` feature switches (`legacy_rio/rio/rio-backend/Cargo.toml:47`).
- `rio-window` has broad platform feature matrix (`x11`, `wayland`, Wayland dlopen/CSD variants) (`legacy_rio/rio/rio-window/Cargo.toml:16`).
- Platform module selection is compile-time gated in `rio-window` (`legacy_rio/rio/rio-window/src/platform/mod.rs:1`, `legacy_rio/rio/rio-window/src/platform_impl/mod.rs:1`).

### 4.2 Runtime/render dependencies

- `wgpu` is core to Sugarloaf and backend rendering path (`legacy_rio/rio/sugarloaf/Cargo.toml:35`, `legacy_rio/rio/rio-backend/Cargo.toml:39`).
- `rio-backend` depends on `sugarloaf`, `teletypewriter`, `copa`, `corcovado`, and `rio-window` (`legacy_rio/rio/rio-backend/Cargo.toml:25-45`).
- `rioterm` depends directly on both `rio-backend` and `rio-window` (`legacy_rio/rio/frontends/rioterm/Cargo.toml:23`, `legacy_rio/rio/frontends/rioterm/Cargo.toml:48`).

### 4.3 Build/release profile choices

Workspace release profile is tuned for lean optimized binaries:
- `codegen-units = 1`
- `lto = true`
- `panic = "abort"`  
Anchor: `legacy_rio/rio/Cargo.toml:82`.

## 5. Cross-Crate Boundary Map (Practical)

1. **Window/event boundary**: `rio-window::EventLoop` -> `rioterm::ApplicationHandler` (`legacy_rio/rio/rio-window/src/event_loop.rs:44`, `legacy_rio/rio/frontends/rioterm/src/application.rs:179`).
2. **App/event payload boundary**: `rio-backend::EventPayload` and `EventProxy` bridge backend-generated events into window loop (`legacy_rio/rio/rio-backend/src/event/mod.rs:239`, `legacy_rio/rio/rio-backend/src/event/mod.rs:290`).
3. **PTY/process boundary**: `ContextManager::create_context` -> `teletypewriter` PTY creation -> `Machine` spawn (`legacy_rio/rio/frontends/rioterm/src/context/mod.rs:209`, `legacy_rio/rio/frontends/rioterm/src/context/mod.rs:287`).
4. **Parser boundary**: `Machine` -> `Processor` -> `copa::Perform` implementation -> `Crosswords` (`legacy_rio/rio/rio-backend/src/performer/mod.rs:207`, `legacy_rio/rio/rio-backend/src/performer/handler.rs:503`, `legacy_rio/rio/rio-backend/src/crosswords/mod.rs:1388`).
5. **Render boundary**: `Renderer::run` prepares objects; `Sugarloaf` owns GPU frame execution (`legacy_rio/rio/frontends/rioterm/src/renderer/mod.rs:832`, `legacy_rio/rio/sugarloaf/src/sugarloaf.rs:363`).

## 6. Notes For Downstream Analysis Beads

- Rendering deep-dive should focus on: `frontends/rioterm/src/renderer/*`, `sugarloaf/src/components/*`, and damage propagation (`TerminalDamage` + `PendingUpdate`).
- Runtime/event-loop deep-dive should focus on: `application.rs`, `scheduler.rs`, `rio-window/src/event_loop.rs`, and `context/mod.rs`.
- Terminal-core deep-dive should focus on: `crosswords/mod.rs`, `performer/handler.rs`, `copa/src/lib.rs`.
- Platform/config deep-dive should focus on: `rio-window/Cargo.toml`, `rio-window/src/platform*`, `rio-backend/src/config/*`.

## 7. Testing Surface Map

| Subsystem | Unit test surface | Integration test surface | E2E scenarios (suggested scripts + fixture sets) |
|---|---|---|---|
| Rendering (`frontends/rioterm` + `sugarloaf`) | `legacy_rio/rio/sugarloaf/tests/test_example_text.rs`, renderer benches in `legacy_rio/rio/frontends/rioterm/benches/renderer_bench.rs` | `cargo test --manifest-path legacy_rio/rio/Cargo.toml -p sugarloaf --tests` | `scripts/e2e/rio_render_frame_lifecycle.sh` (route switch + redraw pressure), `scripts/e2e/rio_render_damage_regression.sh` (damage/full repaint cases). Fixture seeds: simple text scenes + glyph stress derived from `sugarloaf/tests/test_example_text.rs`. |
| Terminal core (`rio-backend` + `copa` + `teletypewriter`) | `legacy_rio/rio/copa/tests/demo.vte`, parser bench `legacy_rio/rio/copa/benches/parser_benchmark.rs` | `cargo test --manifest-path legacy_rio/rio/Cargo.toml -p rio-backend --lib --tests`; sixel fixture corpus under `legacy_rio/rio/rio-backend/tests/sixel/*` | `scripts/e2e/rio_terminal_parser_matrix.sh` (CSI/OSC/UTF-8/sync-update), `scripts/e2e/rio_sixel_roundtrip.sh`. Fixture sets: `copa/tests/demo.vte` + `rio-backend/tests/sixel/*.sixel` and expected rgba files. |
| Runtime loop + routing (`frontends/rioterm` + `rio-window` + `corcovado`) | `legacy_rio/rio/rio-window/tests/send_objects.rs`, `legacy_rio/rio/rio-window/tests/sync_object.rs`, `legacy_rio/rio/corcovado/test/test_poll.rs` | `cargo test --manifest-path legacy_rio/rio/Cargo.toml -p rio-window --tests`; `cargo test --manifest-path legacy_rio/rio/Cargo.toml -p corcovado --test test_poll --test test_smoke` | `scripts/e2e/rio_event_wakeup_pipeline.sh` (PTY wakeup -> user_event -> redraw), `scripts/e2e/rio_resize_input_churn.sh` (resize/input/event-order checks). Fixture sets: scripted key/resize timelines with route IDs and expected phase ordering. |
| Config + platform layer (`rio-backend` config + `rio-window` platform features) | config parse/validation unit modules in `rio-backend/src/config/*`; platform-gated modules in `rio-window/src/platform*` | `cargo test --manifest-path legacy_rio/rio/Cargo.toml -p rio-backend --lib`; feature builds: `cargo check --manifest-path legacy_rio/rio/Cargo.toml -p rioterm --no-default-features --features x11` and `--features wayland` | `scripts/e2e/rio_platform_feature_matrix.sh` and `scripts/e2e/rio_config_boot_matrix.sh`. Fixture sets: minimal/invalid/override config TOML variants and feature-specific startup expectations. |

Notes:
- Existing test fixtures are strongest today for parser and sixel paths; render/runtime/config e2e scenarios above are explicit downstream script targets to keep Ghostty/Zellij inventory parity.
- Keep all new scripts under `legacy_rio/rio/misc/scripts/` for discoverability alongside `test-iterm2-image-protocol.sh`.

## 8. Logging Contract (Minimum Required Fields)

Downstream e2e/soak/perf scripts should emit structured JSONL events with at least:

| Field | Type | Requirement |
|---|---|---|
| `run_id` | string | Globally unique run/session identifier for cross-file correlation |
| `scenario_id` | string | Stable scenario key (e.g., `rio_resize_input_churn`) |
| `pane_id` | integer/null | Pane identifier when terminal context exists |
| `window_id` | integer/null | Window identifier when route/window context exists |
| `phase` | string | Lifecycle phase (`setup`, `stimulus`, `observe`, `assert`, `teardown`) |
| `elapsed_ms` | integer | Milliseconds since run start or phase start (must be monotonic) |
| `outcome` | string | `ok` / `warn` / `fail` |
| `error_code` | string/null | Stable error code on non-`ok` outcomes (never free-form only) |

Recommended additional fields: `route_id`, `crate`, `event_type`, `feature_set`, `seed`.

Example event:

```json
{"run_id":"rio-20260218T0500Z-01","scenario_id":"rio_event_wakeup_pipeline","pane_id":3,"window_id":1,"phase":"assert","elapsed_ms":842,"outcome":"ok","error_code":null}
```

## 9. Operator-Facing Validation Commands

Use these commands to validate architecture assumptions in this inventory:

```bash
# 1) Verify workspace membership and crate boundaries
cargo metadata --manifest-path legacy_rio/rio/Cargo.toml --format-version 1 \
  | jq -r '.workspace_members[]'

# 2) Verify feature-gated platform matrix wiring (x11/wayland)
cargo tree --manifest-path legacy_rio/rio/frontends/rioterm/Cargo.toml -e features \
  | rg "x11|wayland|rio-window|rio-backend"

# 3) Validate parser -> performer -> terminal model chain anchors
rg -n "pub struct Parser|impl Perform for Performer|impl.*Handler for Crosswords" \
  legacy_rio/rio/copa/src/lib.rs \
  legacy_rio/rio/rio-backend/src/performer/handler.rs \
  legacy_rio/rio/rio-backend/src/crosswords/mod.rs

# 4) Validate event envelope and app routing flow anchors
rg -n "EventPayload|EventProxy|user_event|RedrawRequested|RioEvent::Wakeup" \
  legacy_rio/rio/rio-backend/src/event/mod.rs \
  legacy_rio/rio/frontends/rioterm/src/application.rs \
  legacy_rio/rio/rio-backend/src/performer/mod.rs

# 5) Validate render boundary (renderer scene assembly -> sugarloaf submission)
rg -n "Renderer::run|set_objects|sugarloaf\\.render|WindowEvent::RedrawRequested" \
  legacy_rio/rio/frontends/rioterm/src/renderer/mod.rs \
  legacy_rio/rio/frontends/rioterm/src/application.rs \
  legacy_rio/rio/sugarloaf/src/sugarloaf.rs

# 6) Execute current concrete integration surfaces
cargo test --manifest-path legacy_rio/rio/Cargo.toml -p rio-window --tests
cargo test --manifest-path legacy_rio/rio/Cargo.toml -p sugarloaf --tests
cargo test --manifest-path legacy_rio/rio/Cargo.toml -p rio-backend --lib --tests

# 7) Smoke benchmark surfaces used by this inventory
cargo bench --manifest-path legacy_rio/rio/Cargo.toml -p copa --bench parser_benchmark --no-run
cargo bench --manifest-path legacy_rio/rio/Cargo.toml -p rioterm --bench renderer_bench --no-run
```
