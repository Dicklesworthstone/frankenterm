# COMPREHENSIVE_ANALYSIS_OF_frankentui

## Scope

- Project analyzed: `/Users/jemanuel/projects/frankentui`
- Analysis workspace (this repo): `/Users/jemanuel/projects/frankenterm`
- Bead scope covered in this revision:
  - `ft-2vuw7.3.1.1` (`[frankentui][R1] Repository topology and crate/module boundary inventory`)
  - `ft-2vuw7.3.1.2` (`[frankentui][R2] Build/runtime/dependency map and feature-flag matrix`)
  - `ft-2vuw7.3.1.3` (`[frankentui][R3] Public surface inventory (APIs/CLI/MCP/config/events)`)

## Purpose Summary

FrankenTUI is a Rust 2024 multi-crate terminal UI kernel and runtime ecosystem focused on deterministic rendering, strict terminal lifecycle correctness, and composable layering. The project positions itself as a kernel-level foundation for robust TUI systems (including web/wasm and PTY-backed test infrastructure), not just a widget set.

Evidence:
- `/Users/jemanuel/projects/frankentui/README.md:46`
- `/Users/jemanuel/projects/frankentui/README.md:163`
- `/Users/jemanuel/projects/frankentui/crates/ftui/src/lib.rs:3`

## Workspace Topology

Workspace shape (Cargo workspace, resolver 2):
- 19 workspace member crates under `crates/`
- `fuzz` is explicitly excluded from the workspace members
- Rust edition 2024; release profile optimized for size (`opt-level = "z"`, `lto = true`, `panic = "abort"`, `strip = true`)
- Per-package release override: `ftui-extras` uses `opt-level = 3`

Evidence:
- `/Users/jemanuel/projects/frankentui/Cargo.toml:1`
- `/Users/jemanuel/projects/frankentui/Cargo.toml:3`
- `/Users/jemanuel/projects/frankentui/Cargo.toml:24`
- `/Users/jemanuel/projects/frankentui/Cargo.toml:28`
- `/Users/jemanuel/projects/frankentui/Cargo.toml:32`
- `/Users/jemanuel/projects/frankentui/Cargo.toml:39`

## Crate Inventory and Ownership Boundaries

### Layer A: Public Facade and Apps

| Crate | Boundary Role | Target Types | Notes |
|---|---|---|---|
| `ftui` | User-facing facade/re-export surface | `lib` | Intended single dependency for app authors; re-exports core/runtime/render/style/text/widgets (`ftui/src/lib.rs`). |
| `ftui-demo-showcase` | Primary demonstration and regression reference app | `lib`, `bin` (`ftui-demo-showcase`), `bin` (`profile_sweep`) | Main executable path for exercising the full stack. |
| `ftui-harness` | Reference harness + examples + deterministic test tooling | `lib`, `bin`, `example` | Model/update/view reference implementation and stress/snapshot infrastructure. |
| `doctor_frankentui` | Diagnostics/orchestration utility crate | `lib`, `bin` | Structured integration-aware diagnostics runner. |

### Layer B: Core Kernel and Runtime Spine

| Crate | Boundary Role | Target Types | Notes |
|---|---|---|---|
| `ftui-core` | Input layer (events, terminal session, capability probing) | `lib` | Defines canonical event domain and session lifecycle boundary. |
| `ftui-render` | Deterministic render kernel (frame, buffer, diff, presenter) | `lib` | Stateless/explicit render pipeline kernel. |
| `ftui-layout` | Geometry solver layer (flex/grid/pane/workspace) | `lib` | Constraint-driven rectangle allocation + pane model semantics. |
| `ftui-runtime` | Orchestration loop (`Program`, `Model`, `Cmd`, subscriptions) | `lib` | Bridges input and render layers and coordinates side effects. |
| `ftui-backend` | Platform abstraction traits | `lib` | Defines backend contracts (`BackendClock`, `BackendEventSource`, `BackendPresenter`, `Backend`). |

### Layer C: Domain Libraries

| Crate | Boundary Role | Target Types | Notes |
|---|---|---|---|
| `ftui-style` | Theme/style/color primitives | `lib` | Styling subsystem shared by widgets and rendering. |
| `ftui-text` | Text layout/search/wrapping/shaping subsystem | `lib` | Advanced text processing surface used by widgets/runtime paths. |
| `ftui-widgets` | Widget library | `lib` | Core widget composition layer over render/layout/style/text. |
| `ftui-extras` | Feature-gated optional add-ons | `lib` | Large optional surface (markdown/charts/forms/effects/terminal widgets/etc.). |
| `ftui-i18n` | Localization/pluralization | `lib` | Locale catalog and interpolation utilities. |

### Layer D: Platform and Integration Adapters

| Crate | Boundary Role | Target Types | Notes |
|---|---|---|---|
| `ftui-tty` | Native terminal backend | `lib` | Native backend implementation boundary for runtime. |
| `ftui-web` | WASM/host-driven backend building blocks | `lib` | Deterministic host-driven backend primitives for web embedding. |
| `ftui-showcase-wasm` | wasm-bindgen showcase runner wrapper | `cdylib`, `rlib` | Exposes showcase runner to JS host environment. |
| `ftui-pty` | PTY lifecycle/test utilities + ws bridge binaries | `lib`, `bin` (`frankenterm_ws_bridge`, `pty_canonicalize`) | PTY-backed integration testing/utilities. |
| `ftui-simd` | Optional SIMD-friendly slot | `lib` | Reserved/minimal crate for safe optimization work. |

Evidence:
- `/Users/jemanuel/projects/frankentui/Cargo.toml:3`
- `/Users/jemanuel/projects/frankentui/README.md:173`
- `/Users/jemanuel/projects/frankentui/crates/ftui/src/lib.rs:5`
- `/Users/jemanuel/projects/frankentui/crates/ftui-core/src/lib.rs:7`
- `/Users/jemanuel/projects/frankentui/crates/ftui-render/src/lib.rs:5`
- `/Users/jemanuel/projects/frankentui/crates/ftui-layout/src/lib.rs:13`
- `/Users/jemanuel/projects/frankentui/crates/ftui-runtime/src/lib.rs:18`
- `/Users/jemanuel/projects/frankentui/crates/ftui-backend/src/lib.rs:4`
- `/Users/jemanuel/projects/frankentui/crates/ftui-web/src/lib.rs:3`
- `/Users/jemanuel/projects/frankentui/crates/ftui-pty/src/lib.rs:20`

## Dependency Boundary Map (Normal Workspace Dependencies)

This map uses normal (non-dev) intra-workspace dependencies only.

- `ftui` depends on: `ftui-core`, `ftui-extras`, `ftui-layout`, `ftui-render`, `ftui-runtime`, `ftui-style`, `ftui-text`, `ftui-widgets`
- `ftui-runtime` depends on: `ftui-backend`, `ftui-core`, `ftui-i18n`, `ftui-layout`, `ftui-render`, `ftui-style`, `ftui-text`, `ftui-tty`
- `ftui-backend` depends on: `ftui-core`, `ftui-render`
- `ftui-web` depends on: `ftui-backend`, `ftui-core`, `ftui-layout`, `ftui-render`, `ftui-runtime`
- `ftui-showcase-wasm` depends on: `ftui-demo-showcase`, `ftui-layout`, `ftui-web`
- `ftui-demo-showcase` depends on: `ftui-core`, `ftui-extras`, `ftui-i18n`, `ftui-layout`, `ftui-render`, `ftui-runtime`, `ftui-style`, `ftui-text`, `ftui-widgets`

Boundary implication:
- The architectural center is `ftui-runtime` plus the `ftui-backend` trait boundary.
- `ftui` is an aggregation facade, not the computational kernel.
- Web/native backends stay behind trait contracts instead of leaking platform semantics into model code.

Evidence:
- `/Users/jemanuel/projects/frankentui/crates/ftui/src/lib.rs:16`
- `/Users/jemanuel/projects/frankentui/crates/ftui-runtime/src/lib.rs:24`
- `/Users/jemanuel/projects/frankentui/crates/ftui-backend/src/lib.rs:42`
- `/Users/jemanuel/projects/frankentui/crates/ftui-web/src/lib.rs:23`

## Module Boundary Inventory (Core Architecture Primitives)

### `ftui-core` (input/session boundary)

Key module groups:
- session/capabilities/input parsing: `terminal_session`, `terminal_capabilities`, `input_parser`
- event model and keybinding: `event`, `keybinding`, `key_sequence`, `semantic_event`
- common geometry + runtime-adjacent helpers: `geometry`, `cursor`, `event_coalescer`, `inline_mode`, `mux_passthrough`

Evidence:
- `/Users/jemanuel/projects/frankentui/crates/ftui-core/src/lib.rs:23`

### `ftui-render` (render kernel boundary)

Key module groups:
- primitives: `cell`, `buffer`, `frame`
- algorithms: `diff`, `diff_strategy`, `spatial_hit_index`, `roaring_bitmap`
- output: `ansi`, `presenter`, `sanitize`

Evidence:
- `/Users/jemanuel/projects/frankentui/crates/ftui-render/src/lib.rs:22`

### `ftui-layout` (geometry/pane/workspace boundary)

Key module groups:
- core layout solvers: `grid`, `responsive`, `responsive_layout`
- caching/debugging: `cache`, `debug`, `dep_graph`, `incremental`
- pane/workspace state model: `pane`, `workspace`

Evidence:
- `/Users/jemanuel/projects/frankentui/crates/ftui-layout/src/lib.rs:42`
- `/Users/jemanuel/projects/frankentui/crates/ftui-layout/src/lib.rs:64`

### `ftui-runtime` (execution/control boundary)

Key module groups:
- program orchestration: `program`, `subscription`, `terminal_writer`
- safety/quality/diagnostics: `allocation_budget`, `resize_coalescer`, `render_trace`, `validation_pipeline`, `unified_evidence`
- statistical/decision subsystems: `bocpd`, `conformal_predictor`, `decision_core`, `voi_sampling`, `eprocess_throttle`

Evidence:
- `/Users/jemanuel/projects/frankentui/crates/ftui-runtime/src/lib.rs:28`

## Runtime and App Entrypoints

Primary executable entrypoints:
- Demo showcase main: `/Users/jemanuel/projects/frankentui/crates/ftui-demo-showcase/src/main.rs:17`
- Harness main: `/Users/jemanuel/projects/frankentui/crates/ftui-harness/src/main.rs:1`
- Doctor CLI main: `/Users/jemanuel/projects/frankentui/crates/doctor_frankentui/src/main.rs:3`
- PTY websocket bridge: `/Users/jemanuel/projects/frankentui/crates/ftui-pty/src/bin/frankenterm_ws_bridge.rs:9`
- PTY canonicalizer CLI: `/Users/jemanuel/projects/frankentui/crates/ftui-pty/src/bin/pty_canonicalize.rs:1`

Core runtime control-point entrypoints:
- `Model` trait contract: `/Users/jemanuel/projects/frankentui/crates/ftui-runtime/src/program.rs:106`
- `Program` runtime struct: `/Users/jemanuel/projects/frankentui/crates/ftui-runtime/src/program.rs:3067`
- `Program::run`: `/Users/jemanuel/projects/frankentui/crates/ftui-runtime/src/program.rs:3483`
- `ProgramConfig`: `/Users/jemanuel/projects/frankentui/crates/ftui-runtime/src/program.rs:1971`

Backend boundary entrypoints:
- `BackendEventSource` trait: `/Users/jemanuel/projects/frankentui/crates/ftui-backend/src/lib.rs:47`
- `BackendPresenter` trait: `/Users/jemanuel/projects/frankentui/crates/ftui-backend/src/lib.rs:75`
- `Backend` trait: `/Users/jemanuel/projects/frankentui/crates/ftui-backend/src/lib.rs:106`

WASM/web boundary entrypoints:
- `ftui-web` crate root: `/Users/jemanuel/projects/frankentui/crates/ftui-web/src/lib.rs:3`
- `ftui-showcase-wasm` exported runner boundary: `/Users/jemanuel/projects/frankentui/crates/ftui-showcase-wasm/src/lib.rs:1`

## Repository Footprint Snapshot (Source Modules by Crate)

Top source-heavy crates by line volume in `src/*.rs`:
- `ftui-extras`: 135,783
- `ftui-demo-showcase`: 112,821
- `ftui-widgets`: 77,680
- `ftui-runtime`: 66,465
- `ftui-render`: 41,018

Interpretation:
- The practical integration surface is weighted toward `ftui-extras`, `ftui-demo-showcase`, and `ftui-widgets`.
- Kernel crates (`ftui-runtime`, `ftui-render`, `ftui-core`, `ftui-layout`) still hold the critical architectural invariants.

## R1 Completion Checklist

- [x] Workspace shape mapped (members/exclusions/profile)
- [x] Crate-level boundaries identified and grouped by responsibility layer
- [x] Runtime architecture primitives identified (`Model`, `Program`, backend traits)
- [x] Binary and library entrypoints inventoried
- [x] Evidence references captured for downstream R2/R3 work

## R2 Build/Runtime/Dependency Map and Feature-Flag Matrix

### Build and Runtime Modes (Operational Matrix)

| Mode | Primary Crates | Required Features | Runtime Boundary | Key Notes |
|---|---|---|---|---|
| Native terminal app mode | `ftui-runtime` + `ftui-tty` + app crate | `ftui-runtime/native-backend` (often via app defaults) | `Program::with_native_backend` path | Current preferred native path in demo showcase. |
| Crossterm compatibility mode | `ftui-core` + `ftui-runtime` | `ftui-core/crossterm` + `ftui-runtime/crossterm-compat` | legacy terminal session/event source constructors | Kept as compatibility lane; harness still wires this path directly. |
| WASM host-driven mode | `ftui-web` (+ optional `ftui-showcase-wasm`) | `ftui-web/input-parser` for encoded input parsing | backend traits implemented by web event/presenter surfaces | Deterministic host clock and event queue model. |
| Diagnostics/orchestration mode | `doctor_frankentui` | none | CLI/report pipeline | Operational tooling outside runtime render loop. |
| PTY integration utilities mode | `ftui-pty` | none | subprocess PTY + ws bridge utilities | Includes cross-project dependency on `frankenterm-core` (`ws-codec` feature). |

Evidence:
- `/Users/jemanuel/projects/frankentui/crates/ftui-runtime/Cargo.toml:26`
- `/Users/jemanuel/projects/frankentui/crates/ftui-runtime/Cargo.toml:29`
- `/Users/jemanuel/projects/frankentui/crates/ftui-demo-showcase/Cargo.toml:53`
- `/Users/jemanuel/projects/frankentui/crates/ftui-demo-showcase/Cargo.toml:56`
- `/Users/jemanuel/projects/frankentui/crates/ftui-web/Cargo.toml:12`
- `/Users/jemanuel/projects/frankentui/crates/ftui-showcase-wasm/Cargo.toml:16`
- `/Users/jemanuel/projects/frankentui/crates/doctor_frankentui/Cargo.toml:13`
- `/Users/jemanuel/projects/frankentui/crates/ftui-pty/Cargo.toml:13`

### Feature-Flag Matrix (Key Crates)

| Crate | Defaults | High-impact Feature Flags | Effect on Integration |
|---|---|---|---|
| `ftui` | `runtime`, `extras` | `crossterm` | Facade-level composition switch controlling optional runtime and extras exposure. |
| `ftui-core` | none | `crossterm`, `tracing`, `tracing-json`, `caps-probe`, `test-helpers` | Governs terminal-session availability and capability probing behavior. |
| `ftui-runtime` | none | `native-backend`, `crossterm-compat`, `render-thread`, `stdio-capture`, `state-persistence`, `hamt`, `telemetry`, `tracing` | Central runtime behavior toggles; strongly influences execution model and observability. |
| `ftui-demo-showcase` | `caps-probe`, `native-backend`, `clipboard`, `image`, `logging`, `screen-mermaid` | `native-backend`, `crossterm-compat`, `screen-mermaid`, `test-support` | Showcase binary behavior and dependency weight are highly feature-driven. |
| `ftui-web` | none | `input-parser`, `tracing` | Enables encoded-event parsing and schema traceability in wasm embedding path. |
| `ftui-showcase-wasm` | n/a | no internal feature map; depends on `ftui-web/input-parser` and `ftui-demo-showcase` without defaults | Tight wasm packaging boundary for browser demo integration. |
| `ftui-harness` | none | `pty-capture`, `telemetry` | Test/reference app can opt into PTY capture and telemetry export. |
| `ftui-widgets` | none | `regex-search`, `state-persistence`, `tracing`, `debug-overlay` | Widget surface can be expanded for search/state persistence and tracing instrumentation. |
| `ftui-text` | none | `markup`, `bidi`, `normalization`, `shaping`, `thread_local_cache` | Text subsystem capability/quality/perf profile tuning is feature-driven. |
| `ftui-extras` | none | large modular set (`canvas`, `charts`, `forms`, `markdown`, `diagram`, `visual-fx`, `fx-gpu`, `terminal-widget`, etc.) | Major optional surface; integration should explicitly choose subsets to avoid binary/dependency bloat. |
| `ftui-style` | none | `serde` | Serialization gate for style/theme persistence and interchange. |

Evidence:
- `/Users/jemanuel/projects/frankentui/crates/ftui/Cargo.toml:12`
- `/Users/jemanuel/projects/frankentui/crates/ftui-core/Cargo.toml:12`
- `/Users/jemanuel/projects/frankentui/crates/ftui-runtime/Cargo.toml:12`
- `/Users/jemanuel/projects/frankentui/crates/ftui-demo-showcase/Cargo.toml:50`
- `/Users/jemanuel/projects/frankentui/crates/ftui-web/Cargo.toml:12`
- `/Users/jemanuel/projects/frankentui/crates/ftui-showcase-wasm/Cargo.toml:16`
- `/Users/jemanuel/projects/frankentui/crates/ftui-harness/Cargo.toml:12`
- `/Users/jemanuel/projects/frankentui/crates/ftui-widgets/Cargo.toml:12`
- `/Users/jemanuel/projects/frankentui/crates/ftui-text/Cargo.toml:12`
- `/Users/jemanuel/projects/frankentui/crates/ftui-extras/Cargo.toml:38`
- `/Users/jemanuel/projects/frankentui/crates/ftui-style/Cargo.toml:19`

### Dependency Wiring Notes (Integration-Relevant)

- `ftui-web` explicitly consumes `ftui-runtime` with `default-features = false`, indicating a deliberately minimal runtime footprint in web mode.
- `ftui-showcase-wasm` disables `ftui-demo-showcase` defaults and composes web/input-parser explicitly, keeping wasm artifacts bounded.
- `ftui-harness` still couples to crossterm compatibility (`ftui-core` with `crossterm`, `ftui-runtime` with `crossterm-compat`) for current test/reference behavior.
- `ftui-demo-showcase` opts into a very broad extras surface by default, so integration plans should treat it as a high-capability reference app rather than a minimal dependency.
- `ftui-pty` directly depends on `frankenterm-core` (`ws-codec`), which is a concrete existing seam between FrankenTUI and FrankenTerm.

Evidence:
- `/Users/jemanuel/projects/frankentui/crates/ftui-web/Cargo.toml:24`
- `/Users/jemanuel/projects/frankentui/crates/ftui-showcase-wasm/Cargo.toml:17`
- `/Users/jemanuel/projects/frankentui/crates/ftui-harness/Cargo.toml:19`
- `/Users/jemanuel/projects/frankentui/crates/ftui-harness/Cargo.toml:20`
- `/Users/jemanuel/projects/frankentui/crates/ftui-demo-showcase/Cargo.toml:24`
- `/Users/jemanuel/projects/frankentui/crates/ftui-demo-showcase/Cargo.toml:63`
- `/Users/jemanuel/projects/frankentui/crates/ftui-pty/Cargo.toml:13`

### Target/Platform Constraints Snapshot

- `ftui-core` gates `crossterm` dependency to non-wasm targets and uses unix-specific signal handling for cleanup.
- `ftui-tty` provides unix-target terminal control dependencies (`nix`, `rustix`, `signal-hook`).
- `ftui-showcase-wasm` carries wasm32-only dependencies (`wasm-bindgen`, `js-sys`, wasm-enabled `getrandom` variants).

Evidence:
- `/Users/jemanuel/projects/frankentui/crates/ftui-core/Cargo.toml:40`
- `/Users/jemanuel/projects/frankentui/crates/ftui-core/Cargo.toml:44`
- `/Users/jemanuel/projects/frankentui/crates/ftui-tty/Cargo.toml:18`
- `/Users/jemanuel/projects/frankentui/crates/ftui-showcase-wasm/Cargo.toml:21`

## R2 Completion Checklist

- [x] Build-mode map produced (native, compatibility, wasm, diagnostics, PTY)
- [x] Key feature-flag matrix captured across core/app/integration crates
- [x] Integration-relevant dependency wiring constraints identified
- [x] Target/platform conditional dependency boundaries mapped
- [x] Evidence references captured for downstream R3/R4 work

## R3 Public Surface Inventory (APIs/CLI/MCP/config/events)

### Public Library API Surface

Primary consumer-facing API layers:
- `ftui` facade crate: re-exports core event/session types, render primitives, style system, runtime types (feature-gated), and module namespaces via `prelude` + crate aliases.
- runtime contract types: `Model`, `Cmd`, `Program`, `ProgramConfig`, `TaskSpec`, and subscription integration are the central author-facing execution APIs.
- backend contract types: `BackendClock`, `BackendEventSource`, `BackendPresenter`, and `Backend` define the platform adapter boundary.
- web host API (Rust side): `ftui-web::step_program::StepProgram` exposes host-driven non-blocking execution (`new/init/step/push_event/advance_time/take_outputs/...`) for wasm embedding.

Evidence:
- `/Users/jemanuel/projects/frankentui/crates/ftui/src/lib.rs:5`
- `/Users/jemanuel/projects/frankentui/crates/ftui/src/lib.rs:29`
- `/Users/jemanuel/projects/frankentui/crates/ftui/src/lib.rs:74`
- `/Users/jemanuel/projects/frankentui/crates/ftui-runtime/src/program.rs:106`
- `/Users/jemanuel/projects/frankentui/crates/ftui-backend/src/lib.rs:47`
- `/Users/jemanuel/projects/frankentui/crates/ftui-backend/src/lib.rs:75`
- `/Users/jemanuel/projects/frankentui/crates/ftui-backend/src/lib.rs:106`
- `/Users/jemanuel/projects/frankentui/crates/ftui-web/src/step_program.rs:90`
- `/Users/jemanuel/projects/frankentui/crates/ftui-web/src/step_program.rs:123`

### CLI/Public Binary Surfaces

| Binary | Interface Style | Surface Summary |
|---|---|---|
| `ftui-demo-showcase` | Manual arg parser + help text + env override layer | Rich option set (`--screen-mode`, `--mouse`, `--tour`, VFX/Mermaid harness flags) plus many `FTUI_DEMO_*` env overrides and deterministic/evidence logging toggles. |
| `doctor_frankentui` | Clap subcommand CLI | Public subcommands: `capture`, `seed-demo`, `suite`, `report`, `doctor`, `list-profiles`. |
| `frankenterm_ws_bridge` (`ftui-pty`) | Manual arg parser | Network/PTY bridge flags: bind/command/args/size/env/origin/token/telemetry/message limits/accept strategy. |
| `pty_canonicalize` (`ftui-pty`) | Manual arg parser | PTY capture canonicalization with explicit profile/quirk controls for terminal-behavior normalization. |

Evidence:
- `/Users/jemanuel/projects/frankentui/crates/ftui-demo-showcase/src/cli.rs:18`
- `/Users/jemanuel/projects/frankentui/crates/ftui-demo-showcase/src/cli.rs:108`
- `/Users/jemanuel/projects/frankentui/crates/doctor_frankentui/src/cli.rs:10`
- `/Users/jemanuel/projects/frankentui/crates/doctor_frankentui/src/cli.rs:23`
- `/Users/jemanuel/projects/frankentui/crates/ftui-pty/src/bin/frankenterm_ws_bridge.rs:21`
- `/Users/jemanuel/projects/frankentui/crates/ftui-pty/src/bin/frankenterm_ws_bridge.rs:163`
- `/Users/jemanuel/projects/frankentui/crates/ftui-pty/src/bin/pty_canonicalize.rs:15`
- `/Users/jemanuel/projects/frankentui/crates/ftui-pty/src/bin/pty_canonicalize.rs:48`

### WASM/JS Host Interface Surface

`ftui-showcase-wasm` exports a `ShowcaseRunner` via `wasm-bindgen` with a broad JS-callable API:
- lifecycle/time/input: `new`, `init`, `advanceTime`, `setTime`, `pushEncodedInput`, `step`, `resize`
- pane interaction operations: pointer down/move/up/cancel/leave/blur/visibility, capture/lost-capture paths, layout mode/undo/redo/replay/import/export
- patch transport and diagnostics: `takeFlatPatches`, `prepareFlatPatches`, `flatCellsPtr/Len`, `flatSpansPtr/Len`, `takeLogs`, `patchHash`, `patchStats`, `frameIdx`, `isRunning`

Evidence:
- `/Users/jemanuel/projects/frankentui/crates/ftui-showcase-wasm/src/wasm.rs:461`
- `/Users/jemanuel/projects/frankentui/crates/ftui-showcase-wasm/src/wasm.rs:493`
- `/Users/jemanuel/projects/frankentui/crates/ftui-showcase-wasm/src/wasm.rs:538`
- `/Users/jemanuel/projects/frankentui/crates/ftui-showcase-wasm/src/wasm.rs:549`
- `/Users/jemanuel/projects/frankentui/crates/ftui-showcase-wasm/src/wasm.rs:842`
- `/Users/jemanuel/projects/frankentui/crates/ftui-showcase-wasm/src/wasm.rs:889`
- `/Users/jemanuel/projects/frankentui/crates/ftui-showcase-wasm/src/wasm.rs:955`

### Event Contract Surface

Public canonical runtime event schema is centered on `ftui_core::event::Event` and associated structs/enums:
- `Event` variants: `Key`, `Mouse`, `Resize`, `Paste`, `Ime`, `Focus`, `Clipboard`, `Tick`
- typed subcontracts for key/mouse input (`KeyEvent`, `KeyCode`, `KeyEventKind`, `Modifiers`, `MouseEvent`, `MouseEventKind`, `MouseButton`)
- optional crossterm mapping shim when `crossterm` feature is enabled.

Evidence:
- `/Users/jemanuel/projects/frankentui/crates/ftui-core/src/event.rs:20`
- `/Users/jemanuel/projects/frankentui/crates/ftui-core/src/event.rs:25`
- `/Users/jemanuel/projects/frankentui/crates/ftui-core/src/event.rs:86`
- `/Users/jemanuel/projects/frankentui/crates/ftui-core/src/event.rs:155`
- `/Users/jemanuel/projects/frankentui/crates/ftui-core/src/event.rs:225`
- `/Users/jemanuel/projects/frankentui/crates/ftui-core/src/event.rs:262`
- `/Users/jemanuel/projects/frankentui/crates/ftui-core/src/event.rs:304`

### Config and Environment Surface

FrankenTUI is predominantly env-configured rather than config-file-driven:
- README-defined harness env controls (`FTUI_HARNESS_*`) for mode, UI sizing, view selection, mouse/focus/logging, and auto-exit behavior.
- harness runtime wiring consumes a broad set of `FTUI_HARNESS_*` keys for screen mode, budget settings, evidence sinks, render trace, locale, and feature toggles.
- demo showcase CLI supports a dedicated `FTUI_DEMO_*` env namespace with precedence behavior documented and implemented in parser logic.
- core text width behavior exposes env toggles (`FTUI_GLYPH_DOUBLE_WIDTH`, `FTUI_TEXT_CJK_WIDTH`/`FTUI_CJK_WIDTH`, `FTUI_EMOJI_VS16_WIDTH`).

Evidence:
- `/Users/jemanuel/projects/frankentui/README.md:417`
- `/Users/jemanuel/projects/frankentui/README.md:423`
- `/Users/jemanuel/projects/frankentui/crates/ftui-harness/src/main.rs:1819`
- `/Users/jemanuel/projects/frankentui/crates/ftui-harness/src/main.rs:1912`
- `/Users/jemanuel/projects/frankentui/crates/ftui-harness/src/main.rs:1942`
- `/Users/jemanuel/projects/frankentui/crates/ftui-demo-showcase/src/cli.rs:6`
- `/Users/jemanuel/projects/frankentui/crates/ftui-demo-showcase/src/cli.rs:279`
- `/Users/jemanuel/projects/frankentui/crates/ftui-core/src/lib.rs:97`
- `/Users/jemanuel/projects/frankentui/crates/ftui-core/src/lib.rs:100`
- `/Users/jemanuel/projects/frankentui/crates/ftui-core/src/lib.rs:125`

### MCP Surface (Current State)

- There is no first-class FrankenTUI runtime MCP server surface exposed in this repository’s primary runtime crates.
- MCP/JSON-RPC interaction appears in `doctor_frankentui` as an operational seeding utility (`seed-demo`) targeting a configurable HTTP path (default `/mcp/`) and issuing `tools/call` requests.

Evidence:
- `/Users/jemanuel/projects/frankentui/crates/doctor_frankentui/src/cli.rs:27`
- `/Users/jemanuel/projects/frankentui/crates/doctor_frankentui/src/seed.rs:20`
- `/Users/jemanuel/projects/frankentui/crates/doctor_frankentui/src/seed.rs:122`

## R3 Completion Checklist

- [x] Public Rust API surface summarized (`ftui`, runtime, backend, web step runner)
- [x] CLI/binary surfaces inventoried (showcase, doctor, PTY tools)
- [x] WASM/JS callable interface mapped from `ShowcaseRunner`
- [x] Canonical event contract captured from `ftui-core::event`
- [x] Env/config knobs documented with concrete sources
- [x] MCP status clarified (utility-level seeding vs runtime-native MCP API)

---

## R4 Execution-Flow Tracing Across Core Workflows

### Native Runtime Loop (`ftui-runtime::Program`)

Execution entry:
1. `Program::with_native_backend` resolves terminal capabilities, sanitizes requested backend features, opens `TtyBackend`, builds `TerminalWriter`, and returns a fully wired `Program`.
2. `Program::run` delegates to `run_event_loop`.
3. `run_event_loop` performs startup sequence (`auto_load` -> `model.init()` -> `execute_cmd` -> `reconcile_subscriptions` -> initial `render_frame`) before entering the main loop.
4. Main loop order is deterministic: poll/drain terminal events, process subscription messages, process async task results, process resize coalescer, run tick update, checkpoint/locale checks, render if dirty.
5. Exit path performs auto-save (if enabled), subscription shutdown, and task-handle reap.

Evidence:
- `/Users/jemanuel/projects/frankentui/crates/ftui-runtime/src/program.rs:3442`
- `/Users/jemanuel/projects/frankentui/crates/ftui-runtime/src/program.rs:3483`
- `/Users/jemanuel/projects/frankentui/crates/ftui-runtime/src/program.rs:3494`
- `/Users/jemanuel/projects/frankentui/crates/ftui-runtime/src/program.rs:3501`
- `/Users/jemanuel/projects/frankentui/crates/ftui-runtime/src/program.rs:3526`
- `/Users/jemanuel/projects/frankentui/crates/ftui-runtime/src/program.rs:3540`
- `/Users/jemanuel/projects/frankentui/crates/ftui-runtime/src/program.rs:3543`
- `/Users/jemanuel/projects/frankentui/crates/ftui-runtime/src/program.rs:3546`
- `/Users/jemanuel/projects/frankentui/crates/ftui-runtime/src/program.rs:3579`
- `/Users/jemanuel/projects/frankentui/crates/ftui-runtime/src/program.rs:3589`

### Update and Command Dispatch Pipeline

All message sources eventually converge through `model.update(...)` plus recursive `execute_cmd(...)`:
- Input path: `handle_event` classifies fairness class, handles resize policy/coalescing, maps event to message, updates model, marks dirty, executes command tree, then reconciles subscriptions.
- Subscription path: `process_subscription_messages` drains queued subscription messages and applies the same update/command pipeline.
- Task path: `process_task_results` drains background task channel and feeds messages through update/command.
- Tick path: `should_tick` gates periodic `Event::Tick` messages into the same update/command flow.
- Resize-coalescer path: `process_resize_coalescer` can yield for fairness or apply coalesced resize via `apply_resize`, which updates dimensions/writer state and emits a resize message to the model.

Evidence:
- `/Users/jemanuel/projects/frankentui/crates/ftui-runtime/src/program.rs:3642`
- `/Users/jemanuel/projects/frankentui/crates/ftui-runtime/src/program.rs:3768`
- `/Users/jemanuel/projects/frankentui/crates/ftui-runtime/src/program.rs:3800`
- `/Users/jemanuel/projects/frankentui/crates/ftui-runtime/src/program.rs:3834`
- `/Users/jemanuel/projects/frankentui/crates/ftui-runtime/src/program.rs:3863`
- `/Users/jemanuel/projects/frankentui/crates/ftui-runtime/src/program.rs:4426`
- `/Users/jemanuel/projects/frankentui/crates/ftui-runtime/src/program.rs:4436`
- `/Users/jemanuel/projects/frankentui/crates/ftui-runtime/src/program.rs:4488`

### Render/Present Pipeline and Budget Control

`render_frame` is split into explicit phases with control gates:
1. Frame-level guardrails check memory + queue assumptions and may drop/degrade frame.
2. Conformal predictor risk gate can force degradation pre-render.
3. Render phase builds/updates frame buffer and widget refresh plan.
4. Present phase calls `TerminalWriter::present_ui_owned` when budget permits.
5. Frame timing, budget evidence, and optional sink emissions are recorded; `dirty` is cleared at end.

`TerminalWriter` then applies mode-specific emit logic:
- `present_ui` / `present_ui_owned` branch by `ScreenMode` (`Inline`, `InlineAuto`, `AltScreen`), run diff/full payload handling, and rotate prior buffers.
- `write_log` writes only in available inline log region (with sanitization and diff-state invalidation when needed); alt-screen logging is a no-op.

Evidence:
- `/Users/jemanuel/projects/frankentui/crates/ftui-runtime/src/program.rs:3967`
- `/Users/jemanuel/projects/frankentui/crates/ftui-runtime/src/program.rs:3980`
- `/Users/jemanuel/projects/frankentui/crates/ftui-runtime/src/program.rs:3995`
- `/Users/jemanuel/projects/frankentui/crates/ftui-runtime/src/program.rs:4137`
- `/Users/jemanuel/projects/frankentui/crates/ftui-runtime/src/program.rs:4164`
- `/Users/jemanuel/projects/frankentui/crates/ftui-runtime/src/program.rs:4212`
- `/Users/jemanuel/projects/frankentui/crates/ftui-runtime/src/program.rs:4218`
- `/Users/jemanuel/projects/frankentui/crates/ftui-runtime/src/terminal_writer.rs:1045`
- `/Users/jemanuel/projects/frankentui/crates/ftui-runtime/src/terminal_writer.rs:1162`
- `/Users/jemanuel/projects/frankentui/crates/ftui-runtime/src/terminal_writer.rs:1909`

### Host-Driven Web Runtime (`ftui-web::StepProgram`)

`StepProgram` mirrors the runtime contract but in deterministic host-stepped form:
1. `new`/`with_backend` establish deterministic clock + event queue + size state.
2. `init` runs `model.init()`, executes commands, and renders first frame.
3. Host pushes events (`push_event`) and time (`advance_time`).
4. `step` drains all queued events, conditionally emits tick message, and renders iff dirty.
5. `execute_cmd` handles command tree synchronously (`Cmd::Task` executes inline in wasm/no-thread context).

Evidence:
- `/Users/jemanuel/projects/frankentui/crates/ftui-web/src/step_program.rs:123`
- `/Users/jemanuel/projects/frankentui/crates/ftui-web/src/step_program.rs:170`
- `/Users/jemanuel/projects/frankentui/crates/ftui-web/src/step_program.rs:187`
- `/Users/jemanuel/projects/frankentui/crates/ftui-web/src/step_program.rs:252`
- `/Users/jemanuel/projects/frankentui/crates/ftui-web/src/step_program.rs:267`
- `/Users/jemanuel/projects/frankentui/crates/ftui-web/src/step_program.rs:346`
- `/Users/jemanuel/projects/frankentui/crates/ftui-web/src/step_program.rs:368`
- `/Users/jemanuel/projects/frankentui/crates/ftui-web/src/step_program.rs:451`
- `/Users/jemanuel/projects/frankentui/crates/ftui-web/src/step_program.rs:483`

### WASM Bridge Workflow (`ftui-showcase-wasm::ShowcaseRunner`)

`ShowcaseRunner` is a JS-facing orchestration shell over `RunnerCore`:
- init/time/input pipeline: `new` -> `init` -> `pushEncodedInput` / `advanceTime` -> `step`
- geometry and pane interaction commands are surfaced as dedicated JS methods
- patch transport exposes both object-return and zero-copy pointer/length forms
- log + diagnostics hooks (`takeLogs`, `patchHash`, `patchStats`) expose runtime state for host-side tooling

This layer keeps browser integration explicit and method-oriented rather than exposing internal runtime structs directly.

Evidence:
- `/Users/jemanuel/projects/frankentui/crates/ftui-showcase-wasm/src/wasm.rs:472`
- `/Users/jemanuel/projects/frankentui/crates/ftui-showcase-wasm/src/wasm.rs:493`
- `/Users/jemanuel/projects/frankentui/crates/ftui-showcase-wasm/src/wasm.rs:519`
- `/Users/jemanuel/projects/frankentui/crates/ftui-showcase-wasm/src/wasm.rs:538`
- `/Users/jemanuel/projects/frankentui/crates/ftui-showcase-wasm/src/wasm.rs:842`
- `/Users/jemanuel/projects/frankentui/crates/ftui-showcase-wasm/src/wasm.rs:866`
- `/Users/jemanuel/projects/frankentui/crates/ftui-showcase-wasm/src/wasm.rs:889`
- `/Users/jemanuel/projects/frankentui/crates/ftui-showcase-wasm/src/wasm.rs:904`
- `/Users/jemanuel/projects/frankentui/crates/ftui-showcase-wasm/src/wasm.rs:955`

### Coupling Hotspots for Native Harmonized Integration

- Runtime core coupling is concentrated in `Program` and `TerminalWriter`; integration seams should prefer backend trait injection and writer boundary adaptation rather than bypassing these layers.
- Web and wasm paths already provide host-driven deterministic stepping semantics, useful as a model for FrankenTerm-side orchestration APIs.
- The command pipeline is intentionally single convergence (`model.update` + `execute_cmd`) across event/tick/subscription/task sources; preserving this invariant is critical for predictable behavior.
- Resize and budget/fairness gates are first-class runtime policy points, not peripheral concerns; integration work should treat them as contract-level behavior.

## R4 Completion Checklist

- [x] Native runtime entry-to-exit flow traced with ordered phase sequence
- [x] Unified message/command dispatch pipeline mapped across all message sources
- [x] Render/present + budget/guardrail/conformal control flow traced
- [x] Web `StepProgram` host-driven deterministic flow traced
- [x] WASM `ShowcaseRunner` host bridge workflow traced
- [x] Integration coupling hotspots identified for downstream planning

---

### Handoff Notes for Next Beads

- `R5` should focus on explicit state/persistence contracts (runtime state persistence, workspace snapshot import/export, evidence/log artifacts).
- `R6` should focus on performance/observability envelope (guardrails, telemetry, diff strategies, benchmark/test evidence surfaces).
