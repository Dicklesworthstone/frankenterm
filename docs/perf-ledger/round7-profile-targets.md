# Round-7 — New-Axis Profile-First Target List + Liveness Verdicts

> **The ≥0.5% attribution gate + production-path liveness gate.** No round-7 new-axis idea earns a bead
> unless a profiler attributes ≥0.5% self-time to the frame on a realistic workload AND the frame has
> verified non-test production callers. Round-6's lesson: `scan_pipeline.process` ranked #1 at 72% but
> was DEAD CODE. Trust no frame until its production callers are traced.

## Carryover liveness verdicts (from round-7 kickoff investigation — verify before mining)

| Axis | Frame | Verdict | Action |
|---|---|---|---|
| Scrollback decode | `warm_line` single-line (`scrollback_tiers.rs:1028`) | LIVENESS-SUSPECT — only test/bench callers found; prod uses full-page `decode_page` (`:1008`) | cod_1 grep-confirm before EV3; refute if dead |
| GUI render | glyph quads / `screen_line.rs:464` → WebGPU draw path | LIVE but REFUTED — M3/R6 showed cost in Metal readback/render-pass machinery, not glyph vertex bandwidth | SKIP; do not re-mine until a new Apple Metal profile shows vertex buffer/draw calls dominate |
| Distributed | `DistributedHttpClient` (`distributed.rs:954`) | DEAD-UNWIRED — strict grep found only definitions/comments/tests in `distributed.rs`; no non-test production caller | SKIP; record |
| Web/SSE | `/stream/events` (`web.rs`, `web/router.rs:71`, `web/sse.rs:625`) | INERT — endpoint exists, but standalone `ft web` has no producer; `web.rs:8-13` documents publisher-less EventBus behavior | SKIP; record |
| EventBus IPC | `EventBus::publish` (`events.rs:1280`) | LIVE — production callers include `runtime.rs:3704/3715/3828/3963/4053`, `ipc.rs:1878`, `connector_inbound_bridge.rs:627` | measured below |
| Startup | WAL replay (`storage.rs:1647`) | LIVE — `RusqliteStorageBackendProvider::open_writer_backend` runs recovery on every `StorageHandle` open | measured below |
| Ingest | `extract_delta` / `detect_with_context` / BOCPD | LIVE but heavily-explored (Q3/EV2/Q4 gates); BOCPD CPU is live at `runtime.rs:3758`, quality-metric ARL still deferred | measured below |

## Method
Reuse the round-6 B0 harness shape (`crates/frankenterm-core/tests/round6_profile_realistic_workloads.rs`):
per-op warmed mean ns × documented fleet-minute call model → realistic self-time share. Round-7 cod_4
evidence lives in `crates/frankenterm-core/tests/round7_profile_realistic_workloads.rs`; it keeps
`scan_pipeline.process` and `redactor.redact` in the denominator so new-axis candidates are not
inflated by measuring only themselves. Deterministic metrics (RSS, alloc-count, ARL) use the A5
harness shape (`round6_quality_metric_harness.rs`) + the new `tests/round7_rss_harness.rs`.

---

## B0' — round-7 scored frames (cod_4 / ft-mcz7t)

Proof command:

```text
RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 CARGO_NET_GIT_FETCH_WITH_CLI=true \
  rch --no-self-healing exec -- \
  env CARGO_TARGET_DIR=/tmp/ft-mcz7t-round7-tgt \
  cargo test --profile release-perf -p frankenterm-core \
    --test round7_profile_realistic_workloads -- --nocapture
```

Proof result: **GREEN**, `[RCH] remote vmi1227854 (1709.1s)`, job `j-29895646634836027`,
`1 passed; 0 failed`. Model: 192 captures/s, 64 redacted reads/s, 16 burst detections/s,
1 startup recovery/min over 60s. Gate: `>=0.5%` self-time. Average dirty WAL size:
`4,738,032` bytes.

| Rank | Frame | Location | Workload | mean ns | calls/min | self-time share | Profile gate | Liveness | Verdict |
|---:|---|---|---|---:|---:|---:|---|---|---|
| 1 | `scan_pipeline.process` | `scan_pipeline.rs:528` | denominator: capture scan | 11,175.35 | 11,520 | 55.290% | PASS | LIVE by prior B0 caveat/scan path | denominator only |
| 2 | `bocpd.observe_text_chunk` | `runtime.rs:3758` → `bocpd.rs:844` | per-capture BOCPD segment observation | 5,776.51 | 11,520 | 28.579% | PASS | LIVE: runtime constructs manager at `runtime.rs:3596` and observes at `runtime.rs:3758` | **Form-7 / defer optimization**: CPU gate passes, but no deterministic ARL/false-positive metric yet, so correctness gate is not admissible |
| 3 | `redactor.redact` | `redactor.rs:690` | denominator: outbound read redaction | 7,284.43 | 3,840 | 12.013% | PASS | LIVE by read/export/audit surfaces | denominator only |
| 4 | `storage.wal_recovery_dirty` | `storage.rs:1647` | startup WAL-dirty DB | 8,214,318.67 | 1 | 3.528% | PASS | LIVE: `StorageHandle` writer-open path always runs `check_and_recover_wal` | **PROMOTE**: only round-7 new CPU axis with profile+liveness gates cleanly satisfied |
| 5 | `events.event_bus_publish` | `events.rs:1280` | burst pattern detection fanout | 1,333.61 | 960 | 0.550% | PASS (marginal) | LIVE: runtime, IPC, connector bridge publishers | **ledger / verify centrally before bead**: just over the gate under burst assumptions; small absolute win ceiling (`~1.28ms/fleet-minute`) |
| 6 | `storage.wal_recovery_clean` | `storage.rs:1647` | startup clean DB | 91,597.33 | 1 | 0.039% | below | LIVE | no bead; clean-start recovery is not a CPU target |

Machine-readable profile output:

```json
{"schema":"round7.new_axis.profile.v1","gate_share":0.005,"avg_dirty_wal_bytes":4738032,"frames":[{"frame":"scan_pipeline.process","location":"scan_pipeline.rs:528","workload":"round6 denominator: capture scan","candidate":false,"mean_ns":11175.35,"calls_per_min":11520,"realistic_self_ns":128740080,"share":0.552903,"gate_pass":true,"notes":"denominator"},{"frame":"bocpd.observe_text_chunk","location":"runtime.rs:3758 -> bocpd.rs:844","workload":"per-capture BOCPD segment observation","candidate":true,"mean_ns":5776.51,"calls_per_min":11520,"realistic_self_ns":66545342,"share":0.285794,"gate_pass":true,"notes":"quality ARL metric deferred"},{"frame":"redactor.redact","location":"redactor.rs:690","workload":"round6 denominator: outbound read redaction","candidate":false,"mean_ns":7284.43,"calls_per_min":3840,"realistic_self_ns":27972211,"share":0.120133,"gate_pass":true,"notes":"denominator"},{"frame":"storage.wal_recovery_dirty","location":"storage.rs:1647","workload":"startup WAL-dirty DB","candidate":true,"mean_ns":8214318.67,"calls_per_min":1,"realistic_self_ns":8214319,"share":0.035278,"gate_pass":true,"notes":"startup dirty contrast"},{"frame":"events.event_bus_publish","location":"events.rs:1280","workload":"burst pattern detection fanout","candidate":true,"mean_ns":1333.61,"calls_per_min":960,"realistic_self_ns":1280264,"share":0.005498,"gate_pass":true,"notes":"live via runtime/ipc/bridge publishers"},{"frame":"storage.wal_recovery_clean","location":"storage.rs:1647","workload":"startup clean DB","candidate":true,"mean_ns":91597.33,"calls_per_min":1,"realistic_self_ns":91597,"share":0.000393,"gate_pass":false,"notes":"startup clean contrast"}]}
```
