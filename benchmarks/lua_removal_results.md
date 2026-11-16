# WezTerm Lua Removal Benchmark Results (bd-21ml)

## Summary

This document captures performance measurements before and after removing the
Lua `update-status` hook (STATUS_UPDATE_LUA). It is intended to quantify the
impact and support the Phase 2/3 decision.

Status: **PENDING MANUAL MEASUREMENTS (headless environment)**

## Environment

- Date: 2026-01-28T22:54:56Z
- OS: Linux 6.17.0-8-generic (Ubuntu), x86_64
- WezTerm version: wezterm 20260117-154428-05343b38
- ft version: ft 0.1.0
- Hardware (CPU/RAM): AMD EPYC 7282 (2 sockets, 64 vCPU), 251Gi RAM
- Display (refresh rate / resolution): headless (no DISPLAY)
- Notes: GUI/interactive WezTerm benchmarks cannot run in this headless environment.

## Methodology

Run the same scenarios **before** and **after** the Lua removal:

1) Idle pane (cursor blink)
2) Active output (fast printing)
3) Multiple panes (4+), mixed activity
4) ft watch running
5) ft watch stopped

Suggested commands:

```bash
# Echo throughput
time for i in {1..1000}; do echo "line $i"; done

# Scrollback stress
seq 1 10000 | while read n; do echo "Stress test line $n with padding text"; done
```

Capture CPU usage (htop/top) during tests.

## Results

### Idle Pane (cursor blink)

| Metric | Before (Lua) | After (No Lua) | Improvement |
|--------|---------------|----------------|-------------|
| CPU (%) |  |  |  |
| Notes |  |  |  |

### Active Output (echo loop)

| Metric | Before (Lua) | After (No Lua) | Improvement |
|--------|---------------|----------------|-------------|
| Lines/sec |  |  |  |
| CPU (%) |  |  |  |
| Notes |  |  |  |

### Scrollback Stress

| Metric | Before (Lua) | After (No Lua) | Improvement |
|--------|---------------|----------------|-------------|
| Time (s) |  |  |  |
| CPU (%) |  |  |  |
| Notes |  |  |  |

### Multiple Panes (4+)

| Metric | Before (Lua) | After (No Lua) | Improvement |
|--------|---------------|----------------|-------------|
| CPU (%) |  |  |  |
| Notes |  |  |  |

### Input Latency (Subjective)

| Metric | Before (Lua) | After (No Lua) | Improvement |
|--------|---------------|----------------|-------------|
| Perceived lag |  |  |  |
| Notes |  |  |  |

## Artifacts

- CPU snapshots:
- Logs / screenshots (if any):

## Decision

- Proceed to Phase 2/3? (Yes/No)
- Rationale:

## TODO

- [ ] Run baseline (Lua enabled) measurements on a desktop environment
- [ ] Run post-removal measurements on the same machine/config
- [ ] Fill tables with results
- [ ] Add any artifacts/screenshots
- [ ] Record decision and rationale
