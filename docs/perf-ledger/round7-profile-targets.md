# Round-7 — New-Axis Profile-First Target List + Liveness Verdicts

> **The ≥0.5% attribution gate + production-path liveness gate.** No round-7 new-axis idea earns a bead
> unless a profiler attributes ≥0.5% self-time to the frame on a realistic workload AND the frame has
> verified non-test production callers. Round-6's lesson: `scan_pipeline.process` ranked #1 at 72% but
> was DEAD CODE. Trust no frame until its production callers are traced.

## Carryover liveness verdicts (from round-7 kickoff investigation — verify before mining)

| Axis | Frame | Verdict | Action |
|---|---|---|---|
| Scrollback decode | `warm_line` single-line (`scrollback_tiers.rs:1028`) | LIVENESS-SUSPECT — only test/bench callers found; prod uses full-page `decode_page` (`:1008`) | cod_1 grep-confirm before EV3; refute if dead |
| GUI render | glyph quads / `webgpu.rs:1545` | DEAD-END — cost is Metal readback (M3 refuted round-6) | SKIP; record |
| Distributed | `DistributedHttpClient` (`distributed.rs:954`) | DEAD-UNWIRED — test-only, 0 prod callers | SKIP; record |
| Web/SSE | `/stream/events` (`web.rs`) | INERT — publisher-less EventBus (ft-zeo5o) | SKIP; record |
| EventBus IPC | `EventBus::publish` (`events.rs:1280`) | LIVE but expected sub-0.5% | cod_4 measure; likely ledger entry |
| Startup | WAL replay (`storage.rs:1650`) | LIVE — only plausible new CPU win | cod_4 profile clean vs WAL-dirty |
| Ingest | `extract_delta` / `detect_with_context` / BOCPD | LIVE but heavily-explored (Q3/EV2/Q4 gates); BOCPD quality-metric deferred | cod_4 only if a frame clears the gate |

## Method
Reuse the round-6 B0 harness shape (`crates/frankenterm-core/tests/round6_profile_realistic_workloads.rs`):
per-op warmed mean ns × documented fleet-minute call model → realistic self-time share. Deterministic
metrics (RSS, alloc-count, ARL) via the A5 harness shape (`round6_quality_metric_harness.rs`) +
the new `tests/round7_rss_harness.rs`.

---

## B0' — round-7 scored frames (filled as cod_4 profiling lands)

_(table lands here; each row: Rank | Frame | Location | mean ns | calls/min | self-time share | Gate verdict | Liveness)_
