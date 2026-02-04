# Architecture

This document captures the wa core architecture for operators and contributors.

## High-level pipeline

```
WezTerm panes
  -> discovery (wezterm cli list)
  -> capture (wezterm cli get-text)
  -> delta extraction (overlap matching + gap detection)
  -> storage (SQLite + FTS5)
  -> pattern engine (rule packs)
  -> event bus
  -> workflow engine
  -> policy engine (capability + rate limit + approvals)
  -> Robot Mode API + MCP (stdio)
```

## Deterministic state (OSC 133)

- wa relies on OSC 133 prompt markers to infer prompt-active vs command-running.
- These markers are parsed during ingest and recorded into pane state.
- Policy gating and workflows use this state to decide if a send is safe.

## Explicit GAP semantics

- Delta extraction uses overlap matching to avoid full scrollback captures.
- If overlap fails (or alt-screen content blocks stable capture), wa records an
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
  - Surface warning in `HealthSnapshot` and `wa status/doctor`.
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
failure modes explicit: if wa cannot keep up, it must record a gap rather than
pretend the stream is continuous.

## Interfaces

- Human CLI is optimized for operator use and safety.
- Robot Mode provides stable, machine-parseable JSON (or TOON) envelopes.
- MCP mirrors Robot Mode for tool and schema parity (feature-gated).

## Library integration map (Appendix F)

| Library | Role in wa | Status |
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
