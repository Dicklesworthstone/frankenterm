# Architecture

This document captures the ft core architecture for operators and contributors.

## High-level pipeline

```
ft runtime backends (current compatibility bridge includes WezTerm CLI)
  -> discovery (backend adapter inventory)
  -> capture (backend stream snapshots/events)
  -> delta extraction (overlap matching + gap detection)
  -> storage (SQLite + FTS5)
  -> pattern engine (rule packs)
  -> event bus
  -> workflow engine
  -> policy engine (capability + rate limit + approvals)
  -> Robot Mode API + MCP (stdio)
```

## Code-grounded module map (current implementation)

### Workspace boundary

- `crates/frankenterm`: primary CLI binary (`ft`) and command dispatch.
- `crates/frankenterm-core`: runtime/control-plane library (ingest, storage, patterns, events, workflows, policy, robot types).
- `crates/frankenterm-gui`: custom GUI terminal binary.
- `crates/frankenterm-mux-server` + `crates/frankenterm-mux-server-impl`: headless mux server entry/implementation.
- `frankenterm/*`: in-tree vendored FrankenTerm (ex-WezTerm) subsystem crates.

### CLI entry + command dispatch (`crates/frankenterm/src/main.rs`)

- `main()` bootstraps runtime threads and calls async `run(robot_mode)`.
- `RuntimeProcessRole`, `RuntimeBootstrapSpec`, `build_process_runtime(...)`, and
  `emit_runtime_bootstrap_lifecycle(...)` define the shared bootstrap contract
  for CLI/watch/web/robot entrypoints, including stable thread names and
  startup/shutdown reason codes.
- `run()` parses `Commands` and routes to:
  - watcher path via `run_watcher_with_backoff(...)` -> `run_watcher(...)`
  - robot path via `Commands::Robot { ... }` and `RobotCommands`
  - other control-plane surfaces (events, workflows, diagnostics, export, replay, mcp, simulate, etc.).
- Robot responses are normalized through `RobotResponse<T>`:
  - `ok`, `data`, `error`, `error_code`, `hint`, `elapsed_ms`, `version`, `now`
  - emitted as JSON or TOON via `print_robot_response(...)`.
- Bootstrap contract validation is enforced by
  `scripts/validate_asupersync_runtime_bootstrap.sh` and
  `tests/e2e/test_ft_e34d9_10_2_1_runtime_bootstrap.sh`.

### Watcher lifecycle wiring (`run_watcher(...)`)

Watcher startup currently wires these components in-process:

1. `StorageHandle` initialization (`frankenterm_core::storage`).
2. `PatternEngine` initialization (`frankenterm_core::patterns`).
3. `EventBus` creation (`frankenterm_core::events::EventBus`).
4. Optional notification pipeline subscribers.
5. Optional workflow auto-handler:
   - `WorkflowRunner` + `WorkflowEngine` + `PaneWorkflowLockManager`
   - built-in workflows (compaction/usage/session/auth/process-triage families).
6. `ObservationRuntime::new(...).with_event_bus(...).with_wezterm_handle(...)`.
7. Optional supporting services:
   - distributed listener (feature-gated)
   - IPC server
   - saved-search scheduler
   - metrics server (feature-gated)
   - snapshot engine
   - scheduled backups
   - orphan reaper
   - mux watchdog.

### Observation runtime internals (`crates/frankenterm-core/src/runtime.rs`)

`ObservationRuntime::start()` spawns cooperative tasks for:

- pane discovery (`spawn_discovery_task`)
- capture collection (`spawn_capture_task`; plus native push event task when enabled)
- capture relay and queueing
- persistence + detection (`spawn_persistence_task`)
- maintenance/retention and snapshot triggers.
- burst protection on native output via `NativeOutputCoalescer` before segments
  hit storage/pattern scanning.

Core passive loop contract: observe/store/detect only. Side effects are delegated to workflow/policy layers.

### Core data-plane modules (`crates/frankenterm-core/src/*`)

- `wezterm.rs`: backend adapter trait (`WeztermInterface`) and concrete handle construction.
- `ingest.rs`: pane discovery, fingerprinting, delta extraction, gap generation, ingest telemetry.
- `storage.rs`: SQLite schema, migrations, WAL mode, FTS5 index/triggers, writer queue, query APIs.
- `search/*`: lexical, semantic, and hybrid retrieval services layered on the
  same storage substrate.
- `patterns.rs`: rule packs, anchor/regex matching, telemetry, detection objects.
- `events.rs`: bounded broadcast fanout and typed event stream (`PatternDetected`, `GapDetected`, workflow lifecycle, user-var events).
- `workflows/`: durable workflow trait/execution engine/runner/locks and step orchestration.
- `policy.rs`: action authorization model (`ActionKind`, `ActorKind`, `PolicyDecision`) and gating helpers.
- Documentation parity note: workflow runtime code is directory-backed under `workflows/`; there is no standalone `workflows.rs`.

### Storage model (authoritative persistence seam)

`storage.rs` defines append- and event-oriented tables including:

- `panes`, `output_segments`, `output_gaps`
- `events`
- `workflow_executions`, `workflow_step_logs`, `workflow_action_plans`
- `audit_actions`, `action_undo`, `approval_tokens`
- plus FTS (`output_segments_fts`) and maintenance/config tables.

This schema is the contract behind status/search/events/workflow/audit CLI and robot surfaces.

### Robot mode + policy seam

- Robot command handling lives in `crates/frankenterm/src/main.rs` under `RobotCommands`.
- `crates/frankenterm-core/src/robot_api_contracts.rs` is the clearest machine
  contract inventory for the current robot surface (search/events/workflow/rules/
  reservations/mission/tx families).
- Robot read paths (`state`, `get-text`, `search`, `events`) use WezTerm/storage + policy checks.
- Robot/action paths (`send`, workflow run, approvals) are routed through policy-gated injectors and workflow runners.
- MCP mirrors this model through feature-gated core modules (`mcp*` in `frankenterm-core` + `Commands::Mcp` in CLI).

### Feature-gated boundaries (current)

- Optional surfaces are enabled via crate features (`mcp`, `web`, `distributed`, `metrics`, `tui`, `ftui`, `sync`, `semantic-search`, `native-wezterm`, etc.).
- Current operational backend remains WezTerm compatibility bridge (`wezterm.rs` + vendored integrations), while native/runtime expansion continues in `frankenterm-core`.
- Headless mux/server work is already split into `crates/frankenterm-mux-server`
  and `crates/frankenterm-mux-server-impl`, which means native mux/server
  evolution is a validation/productization problem more than a bootstrap-one.

## Deterministic state (OSC 133)

- ft relies on OSC 133 prompt markers to infer prompt-active vs command-running.
- These markers are parsed during ingest and recorded into pane state.
- Policy gating and workflows use this state to decide if a send is safe.

## Explicit GAP semantics

- Delta extraction uses overlap matching to avoid full scrollback captures.
- If overlap fails (or alt-screen content blocks stable capture), ft records an
  explicit gap segment and emits a gap event.
- Gap events are treated as uncertainty: policy checks can require approval
  when recent gaps are present.

## Backpressure Signals and Degradation Policy

Backpressure is treated as a first-class signal. The system should remain
deterministic under load and make data loss explicit rather than silent.

### Signals (authoritative)

- Capture queue depth (runtime capture channel).
- Storage writer queue depth (bounded write queue).
- Event bus queue depth + oldest message lag (delta/detection/signal).
- Ingest lag (avg/max from runtime metrics).
- Per-pane consecutive backpressure (tailer send timeouts).
- Indexing lag (FTS insert latency), when available.

### Thresholds

- Warning: queue depth >= 75% of capacity (matches current `BACKPRESSURE_WARN_RATIO`).
- Critical: queue depth >= 90% of capacity or sustained lag > 5s.
- Overflow: per-pane consecutive backpressure >= `OVERFLOW_BACKPRESSURE_THRESHOLD`
  (currently 5) triggers an explicit gap.

### Responses (deterministic)

- Warning:
  - Surface warning in `HealthSnapshot` and `ft status/doctor`.
  - Continue observing, but prioritize draining queues.
- Critical:
  - Slow down polling (adaptive backoff).
  - Reduce capture concurrency if configured to do so.
  - Emit explicit GAPs if continuity becomes uncertain.
- Overflow:
  - Insert `backpressure_overflow` GAP on next successful capture for the pane.
  - Reset per-pane backpressure counters.
- Persistent DB backpressure:
  - Enter `DbWrite` degradation (queue bounded writes, keep observing).
  - If queue saturates, degrade further and record explicit gaps.
- Persistent detection lag:
  - Enter `PatternEngine` degradation (skip or disable rules).
  - Continue ingesting and storing segments.

These rules are designed to be implementable with existing metrics and to keep
failure modes explicit: if ft cannot keep up, it must record a gap rather than
pretend the stream is continuous.

## Interfaces

- Human CLI is optimized for operator use and safety.
- Robot Mode provides stable, machine-parseable JSON (or TOON) envelopes.
- MCP mirrors Robot Mode for tool and schema parity (feature-gated).
- WezTerm integration is treated as a compatibility bridge, not the product boundary.

## Latency Immunity Contract

For local-interaction guarantees under remote mux degradation (CPU/swap/IO pressure),
see `docs/latency-immunity-architecture-ft-1u90p.9.md`.
That contract defines:
- local-first interaction/reflow invariants
- EV-gated architecture levers and fallback triggers
- rollout and proof artifacts required before broad enablement

## asupersync Migration Baseline

The migration baseline for runtime convergence is documented in
`docs/asupersync-migration-baseline.md`.
It is the canonical source for:
- inventory truth (`docs/asupersync-runtime-inventory.json`)
- doctrine ADR (`docs/adr/0012-asupersync-runtime-doctrine.md`)
- machine-readable invariants (`docs/asupersync-runtime-invariants.json`)
- migration scoreboard (`docs/asupersync-migration-scoreboard.json`, `docs/asupersync-migration-scoreboard.md`)
- heavy-compute execution policy (`docs/asupersync-rch-execution-policy.md`)
- risk ledger and sequencing scorecard used by downstream `ft-e34d9.*` beads

## FrankenTerm Convergence Architecture (ft-3681t.1.3)

The program-level convergence architecture contract for native mux + swarm
orchestration + connector fabric integration is defined in:

`docs/ft-3681t-convergence-architecture.md`

This document is the north-star interface and boundary spec for
`ft-3681t.2.*` through `ft-3681t.9.*` implementation beads, including failure,
degradation, rollback, and validation/evidence requirements.

## FrankenTerm Convergence Execution Plan (ft-3681t.1.4)

The execution plan with critical path, delivery tracks, parallelism map,
measurable success gates, and anti-goals is defined in:

`docs/ft-3681t-execution-plan.md`

## Library integration map (Appendix F)

| Library | Role in ft | Status |
|---------|------------|--------|
| cass (/dp/coding_agent_session_search) | Correlation + session archaeology; used in status/workflows | integrated |
| caut (/dp/coding_agent_usage_tracker) | Usage truth + selection; used in accounts/workflows | integrated |
| rich_rust | Human-first CLI output (tables/panels/highlight) | planned |
| charmed_rust | Optional TUI (pane picker, event feed, transcript viewer) | feature-gated (tui) |
| fastmcp_rust | MCP tool surface (mirrors robot mode) | feature-gated (mcp) |
| fastapi_rust | Optional HTTP server for dashboards/webhooks | planned |
| asupersync | Remote bootstrap/sync layer (configs, binaries, DB snapshots); see docs/sync-spec.md | planned |
| playwright | Automate device auth flows with persistent profiles | feature-gated (browser) |
| ast-grep | Structure-aware scans for rule hygiene tooling | tooling |
