# Rio Rendering Pipeline Analysis

Bead: `ft-34sko.2`  
Scope: `legacy_rio/rio`

## 1. End-to-End Frame Lifecycle

1. **PTY/read side emits wakeups, not immediate render passes**
   - `Machine::pty_read` parses bytes and emits `RioEvent::Wakeup(route_id)` when non-synchronized output is processed (`legacy_rio/rio/rio-backend/src/performer/mod.rs:168`, `legacy_rio/rio/rio-backend/src/performer/mod.rs:228`).
   - Sync-timeout handling also emits `Wakeup` after `stop_sync`, keeping synchronized updates coalesced (`legacy_rio/rio/rio-backend/src/performer/mod.rs:336`, `legacy_rio/rio/rio-backend/src/performer/mod.rs:355`, `legacy_rio/rio/rio-backend/src/performer/mod.rs:359`).

2. **Application loop decides when to redraw**
   - `Application::user_event` handles `RioEvent::Wakeup` by marking route render state dirty and scheduling redraw (`legacy_rio/rio/frontends/rioterm/src/application.rs:220`, `legacy_rio/rio/frontends/rioterm/src/application.rs:304`).
   - `RioEvent::RenderRoute` path checks frame pacing (`wait_until`) and either schedules or redraws immediately (`legacy_rio/rio/frontends/rioterm/src/application.rs:248`).
   - `RioEvent::Render` directly calls `route.request_redraw()` if render gating allows (`legacy_rio/rio/frontends/rioterm/src/application.rs:223`, `legacy_rio/rio/frontends/rioterm/src/application.rs:245`).

3. **RedrawRequested is the frame boundary**
   - `WindowEvent::RedrawRequested` calls `pre_present_notify`, records `begin_render`, and then dispatches route render (`legacy_rio/rio/frontends/rioterm/src/application.rs:1350`, `legacy_rio/rio/frontends/rioterm/src/router/mod.rs:94`).
   - Terminal route calls `screen.render()`, then updates IME cursor position after draw (`legacy_rio/rio/frontends/rioterm/src/application.rs:1364`).

4. **Screen -> Renderer -> Sugarloaf**
   - `Screen::render` delegates to `Renderer::run` (`legacy_rio/rio/frontends/rioterm/src/screen/mod.rs:2547`, `legacy_rio/rio/frontends/rioterm/src/screen/mod.rs:2581`).
   - `Renderer::run` updates text/object layers, calls `sugarloaf.set_objects`, then `sugarloaf.render()` (`legacy_rio/rio/frontends/rioterm/src/renderer/mod.rs:832`, `legacy_rio/rio/frontends/rioterm/src/renderer/mod.rs:1133`, `legacy_rio/rio/frontends/rioterm/src/renderer/mod.rs:1156`).

5. **GPU commit/present**
   - `Sugarloaf::render` computes dimensions + updates, acquires surface texture, encodes render passes, submits command buffer, and presents (`legacy_rio/rio/sugarloaf/src/sugarloaf.rs:363`, `legacy_rio/rio/sugarloaf/src/sugarloaf.rs:372`, `legacy_rio/rio/sugarloaf/src/sugarloaf.rs:492`).
   - Render state is reset post-frame (`legacy_rio/rio/sugarloaf/src/sugarloaf.rs:500`, `legacy_rio/rio/sugarloaf/src/sugarloaf/state.rs:189`).

## 2. Damage + Redraw Strategy

1. **Terminal damage model**
   - Backend tracks `TerminalDamage` as `Full | Partial(lines) | CursorOnly` and exposes `peek_damage_event` + `reset_damage` (`legacy_rio/rio/rio-backend/src/crosswords/mod.rs:560`, `legacy_rio/rio/rio-backend/src/crosswords/mod.rs:588`).
   - Full damage can be forced from terminal state transitions via `mark_fully_damaged` (`legacy_rio/rio/rio-backend/src/crosswords/mod.rs:490`).

2. **UI damage model**
   - Frontend keeps `PendingUpdate { dirty, ui_damage }` and merges UI-level damage independently of terminal changes (`legacy_rio/rio/frontends/rioterm/src/context/renderable.rs:98`, `legacy_rio/rio/frontends/rioterm/src/context/renderable.rs:119`).
   - `take_ui_damage` + `reset` are consumed during render (`legacy_rio/rio/frontends/rioterm/src/context/renderable.rs:128`, `legacy_rio/rio/frontends/rioterm/src/context/renderable.rs:133`).

3. **Merge and selective draw**
   - `Renderer::run` merges terminal damage and UI damage per context before snapshotting (`legacy_rio/rio/frontends/rioterm/src/renderer/mod.rs:885`, `legacy_rio/rio/frontends/rioterm/src/renderer/mod.rs:900`).
   - Partial damage redraws only affected lines with `content.clear_line(line)` + `create_line(...)` (`legacy_rio/rio/frontends/rioterm/src/renderer/mod.rs:1058`, `legacy_rio/rio/frontends/rioterm/src/renderer/mod.rs:1062`).
   - Full damage path clears/rebuilds full rich text content for the context (`legacy_rio/rio/frontends/rioterm/src/renderer/mod.rs:1036`).

4. **Observed tradeoff**
   - Design minimizes per-frame work when line damage is accurate.
   - If render is marked dirty without concrete damage, the merge path falls back to `Full`, favoring correctness over minimal redraw (`legacy_rio/rio/frontends/rioterm/src/renderer/mod.rs:878`, `legacy_rio/rio/frontends/rioterm/src/renderer/mod.rs:913`).

## 3. Resize, Scale, and Presentation Behavior

1. **Resize path**
   - Window resize event updates screen/surface sizes (`legacy_rio/rio/frontends/rioterm/src/application.rs:1330`, `legacy_rio/rio/frontends/rioterm/src/screen/mod.rs:458`).
   - `Screen::resize` calls `sugarloaf.resize` and updates context grids (`legacy_rio/rio/frontends/rioterm/src/screen/mod.rs:468`, `legacy_rio/rio/frontends/rioterm/src/screen/mod.rs:473`).

2. **Scale-factor path**
   - Scale changes call `screen.set_scale` and refresh vblank interval (`legacy_rio/rio/frontends/rioterm/src/application.rs:1338`, `legacy_rio/rio/frontends/rioterm/src/router/mod.rs:569`).
   - `set_scale` rescale+resize+render+context resize in one path (`legacy_rio/rio/frontends/rioterm/src/screen/mod.rs:480`, `legacy_rio/rio/frontends/rioterm/src/screen/mod.rs:487`).

3. **Frame pacing**
   - Non-macOS pacing uses `RouteWindow::wait_until` (based on `render_timestamp` + `vblank_interval`) and scheduler-triggered redraw (`legacy_rio/rio/frontends/rioterm/src/router/mod.rs:511`, `legacy_rio/rio/frontends/rioterm/src/router/mod.rs:61`).
   - `vblank_interval` is derived from monitor refresh rate (`legacy_rio/rio/frontends/rioterm/src/router/mod.rs:569`, `legacy_rio/rio/frontends/rioterm/src/router/mod.rs:650`).

4. **Presentation defaults**
   - Surface config uses `PresentMode::Fifo` and `desired_maximum_frame_latency: 2` (`legacy_rio/rio/sugarloaf/src/context/mod.rs:236`, `legacy_rio/rio/sugarloaf/src/context/mod.rs:237`, `legacy_rio/rio/sugarloaf/src/context/mod.rs:290`, `legacy_rio/rio/sugarloaf/src/context/mod.rs:291`).
   - Current render error branch explicitly panics on OOM; non-OOM surface errors are not actively recovered in `Sugarloaf::render` (`legacy_rio/rio/sugarloaf/src/sugarloaf.rs:495`).

5. **Occlusion-aware render suppression**
   - Window occlusion/focus state is tracked; reappearance sets one-time render flag to avoid stale appearance (`legacy_rio/rio/frontends/rioterm/src/application.rs:1286`, `legacy_rio/rio/frontends/rioterm/src/application.rs:1301`, `legacy_rio/rio/frontends/rioterm/src/application.rs:1307`).

## 4. Text/Glyph Cache Internals

1. **Character and font hot paths**
   - `CharCache` uses static ASCII lookup + Unicode LRU (`legacy_rio/rio/frontends/rioterm/src/renderer/char_cache.rs:29`, `legacy_rio/rio/frontends/rioterm/src/renderer/char_cache.rs:50`).
   - `FontCache` uses ASCII hot cache (`HashMap`) plus LRU for broader glyph/style keys (`legacy_rio/rio/frontends/rioterm/src/renderer/font_cache.rs:14`, `legacy_rio/rio/frontends/rioterm/src/renderer/font_cache.rs:33`, `legacy_rio/rio/frontends/rioterm/src/renderer/font_cache.rs:53`).

2. **Text-run caching**
   - `TextRunManager` exposes multi-level cache hits (full render, shaping-only, glyphs-only) and supports vertex reuse with positional offsets (`legacy_rio/rio/sugarloaf/src/components/rich_text/text_run_manager.rs:16`, `legacy_rio/rio/sugarloaf/src/components/rich_text/text_run_manager.rs:30`, `legacy_rio/rio/sugarloaf/src/components/rich_text/text_run_manager.rs:93`).

3. **Dual image atlas strategy**
   - `ImageCache` maintains separate mask/color atlases with dirty-upload processing (`legacy_rio/rio/sugarloaf/src/components/rich_text/image_cache/cache.rs:55`, `legacy_rio/rio/sugarloaf/src/components/rich_text/image_cache/cache.rs:74`, `legacy_rio/rio/sugarloaf/src/components/rich_text/image_cache/cache.rs:292`).
   - Atlas reset is explicit on font changes (`legacy_rio/rio/sugarloaf/src/components/rich_text/image_cache/cache.rs:248`).

## 5. Actionable FrankenTerm Improvements

1. **Adopt a merged two-source damage contract (terminal + UI)**
   - Mirror Rio’s split between terminal damage and UI damage merge (`legacy_rio/rio/frontends/rioterm/src/context/renderable.rs:98`, `legacy_rio/rio/frontends/rioterm/src/renderer/mod.rs:900`).
   - FrankenTerm impact: cleaner ownership boundaries and lower redraw volume during hints/selection/search overlays.

2. **Use wakeup-coalesced scheduling instead of direct render triggers from backend IO**
   - Rio’s backend emits wakeup events and lets the app scheduler decide redraw timing (`legacy_rio/rio/rio-backend/src/performer/mod.rs:228`, `legacy_rio/rio/frontends/rioterm/src/application.rs:304`).
   - FrankenTerm impact: smoother bursts under heavy pane output and less redundant frame work.

3. **Integrate monitor-aware frame pacing + occlusion gating**
   - Reuse Rio-style per-window pacing via `vblank_interval` and `wait_until`, plus focus/occlusion suppression (`legacy_rio/rio/frontends/rioterm/src/router/mod.rs:511`, `legacy_rio/rio/frontends/rioterm/src/application.rs:1301`).
   - FrankenTerm impact: better battery/CPU efficiency for large multi-pane swarms.

4. **Harden surface error recovery around resize/present**
   - Rio currently configures FIFO + latency targets but only hard-handles OOM in `render` (`legacy_rio/rio/sugarloaf/src/context/mod.rs:236`, `legacy_rio/rio/sugarloaf/src/sugarloaf.rs:495`).
   - FrankenTerm should explicitly recover `Lost`/`Outdated`/`Timeout` with controlled reconfigure/backoff.

5. **Adopt cache tiering for text hot paths**
   - Char/font hot caches plus text-run vertex reuse are concrete patterns worth porting (`legacy_rio/rio/frontends/rioterm/src/renderer/char_cache.rs:50`, `legacy_rio/rio/frontends/rioterm/src/renderer/font_cache.rs:33`, `legacy_rio/rio/sugarloaf/src/components/rich_text/text_run_manager.rs:93`).
   - FrankenTerm impact: lower CPU usage for dense redraw bursts.

6. **Add explicit resize transaction semantics**
   - Rio splits surface resize (`sugarloaf.resize`) from PTY resize and later wakeup (`legacy_rio/rio/frontends/rioterm/src/screen/mod.rs:468`, `legacy_rio/rio/frontends/rioterm/src/screen/mod.rs:503`).
   - FrankenTerm should formalize this into a generation-tagged resize transaction to avoid stale-layout presentation during rapid resizes.

## 6. Cross-Reference

- `ft-1u90p.4` (render/presentation pipeline)
- `ft-1u90p.2` (resize control-plane + rendering interactions)

## 7. Implementation Validation Blueprint (Per Adoption Candidate)

All candidates below define exact downstream validation surfaces to avoid vague implementation follow-up.

### Candidate 1: Merged two-source damage contract (terminal + UI)

- Unit-test targets:
  - `crates/frankenterm-core/src/screen_state.rs` (damage merge semantics)
  - `crates/frankenterm-core/src/runtime.rs` (damage propagation into render scheduler snapshots)
- Integration-test targets:
  - `tests/e2e/alt_screen_enter.txt` + `tests/e2e/alt_screen_leave.txt` driven through runtime update flow to verify no false full-damage promotions.
- Required e2e script:
  - `tests/e2e/rio/test_rendering_pipeline.sh --scenario damage_merge`
- Required logs/artifacts:
  - JSONL at `e2e-artifacts/rio/rendering_pipeline/<run_id>.jsonl`
  - Summary metrics at `e2e-artifacts/rio/rendering_pipeline/<run_id>.summary.json`

### Candidate 2: Wakeup-coalesced scheduling (backend emits wakeups, frontend schedules)

- Unit-test targets:
  - `crates/frankenterm-core/src/runtime.rs` (wakeup queue coalescing and dedupe)
  - `crates/frankenterm-core/src/tailer.rs` (burst output to wakeup mapping)
- Integration-test targets:
  - watcher-runtime startup with synthetic burst output and bounded redraw scheduling checks in `crates/frankenterm-core/src/runtime.rs` integration-style tests.
- Required e2e script:
  - `tests/e2e/rio/test_rendering_pipeline.sh --scenario wakeup_coalesce`
- Required logs/artifacts:
  - JSONL + summary artifacts in `e2e-artifacts/rio/rendering_pipeline/`

### Candidate 3: Monitor-aware frame pacing + occlusion gating

- Unit-test targets:
  - `crates/frankenterm-core/src/runtime.rs` (frame interval computation + pacing gate state machine)
  - `crates/frankenterm-core/src/watchdog.rs` (render suppress/resume state transitions)
- Integration-test targets:
  - simulated visible/occluded transitions with render suppression assertions under `crates/frankenterm-core/src/runtime.rs` test module.
- Required e2e script:
  - `tests/e2e/rio/test_rendering_pipeline.sh --scenario pacing_occlusion`
- Required logs/artifacts:
  - per-phase frame timing series in `*.jsonl`
  - occlusion transition counters in `*.summary.json`

### Candidate 4: Surface error recovery for present/reconfigure paths

- Unit-test targets:
  - `frankenterm/surface/src/line/line.rs` + related surface/runtime glue for lost/outdated/timeouts handling logic.
- Integration-test targets:
  - forced surface-lost/reconfigure loops with retry/backoff assertions in renderer integration harness.
- Required e2e script:
  - `tests/e2e/rio/test_rendering_pipeline.sh --scenario surface_recovery`
- Required logs/artifacts:
  - error_code distribution and retry counts in JSONL
  - recovery success rate report in summary JSON

### Candidate 5: Cache tiering for text hot paths

- Unit-test targets:
  - glyph/cache structures in frontend rendering path (new cache policy tests under `crates/frankenterm-core/src/` cache modules as implemented).
- Integration-test targets:
  - long-running redraw churn benchmark path verifying cache-hit ratio monotonicity and bounded memory.
- Required e2e script:
  - `tests/e2e/rio/test_rendering_pipeline.sh --scenario cache_tiering`
- Required logs/artifacts:
  - cache hit/miss counters + eviction counts in JSONL
  - budget check outcome in summary JSON

### Candidate 6: Resize transaction semantics (generation-tagged)

- Unit-test targets:
  - `crates/frankenterm-core/src/resize_scheduler.rs` (generation monotonicity, stale frame suppression)
  - `crates/frankenterm-core/src/runtime.rs` (resize event ordering)
- Integration-test targets:
  - resize storm with injected PTY lag to verify no stale-layout presentation.
- Required e2e script:
  - `tests/e2e/rio/test_rendering_pipeline.sh --scenario resize_transaction`
- Required logs/artifacts:
  - resize generation and commit generation pairs in JSONL
  - stale-frame count budget in summary JSON

## 8. Verification Budgets (Measurable)

Downstream implementation must report these metrics from `e2e-artifacts/rio/rendering_pipeline/<run_id>.jsonl`:

1. Frame pacing budget:
   - `frame_time_ms` p95 <= `16.7` in steady-state 60Hz scenarios
   - `frame_time_ms` p99 <= `25.0` during resize/input burst scenarios
2. Wakeup-to-present latency budget:
   - `elapsed_ms` delta from `phase=stimulus` (wakeup emitted) to `phase=assert` (frame committed) p95 <= `40`
3. Resize stability budget:
   - first stable frame after resize (`decision=stable_frame`) <= `120ms` p95
4. CPU/GPU budget:
   - `cpu_ms` p95 <= `8.0` and `gpu_ms` p95 <= `8.0` for non-stress rendering scenarios
5. Redundant redraw budget:
   - ratio of `decision=redundant_redraw` <= `5%` per run

Required reporting artifacts:
- `e2e-artifacts/rio/rendering_pipeline/<run_id>.jsonl` (raw per-event series)
- `e2e-artifacts/rio/rendering_pipeline/<run_id>.summary.json` (aggregated percentiles + pass/fail)
- `e2e-artifacts/rio/rendering_pipeline/<run_id>.md` (human-readable run summary)

## 9. Mandatory Downstream E2E Contract

The downstream implementation is not complete unless all of the following are true:

1. Script exists and is executable:
   - `tests/e2e/rio/test_rendering_pipeline.sh`
2. Deterministic fixtures are versioned under:
   - `fixtures/rio/rendering/`
3. Script writes structured JSONL logs to:
   - `e2e-artifacts/rio/rendering_pipeline/<run_id>.jsonl`
4. Each JSONL event includes the mandatory fields:
   - `run_id`
   - `scenario_id`
   - `pane_id` or `window_id` (at least one must be non-null)
   - `phase`
   - `frame_time_ms`
   - `cpu_ms`
   - `gpu_ms`
   - `decision`
   - `outcome`
   - `error_code`

If any field is absent, treat the run as invalid regardless of visual output correctness.
