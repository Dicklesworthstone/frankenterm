# ElasticBuffer + wgpu Vertex/Instance Buffer Telemetry

**Bead:** [BR-TERM-EMULATOR-UPLIFT.1.3.cont2] / `ft-hznqt`
**Parent:** `ft-kciew` (policy lifecycle shipped at
`c88dde9c0` — `TermWindow.quad_buffer_policy:
ElasticBuffer<u32>` + begin/end gesture hooks + idle-shrink).
**Status:** Foundation slice shipped. Telemetry contract +
gesture-regrow invariant + 3-scenario bench corpus + audit
doc all live; production wgpu surgery (replace `<u32>` with
`<QuadInstance>`, mirror grow/shrink onto `wgpu::Buffer`,
swap per-cell vertex batching with per-cell instancing) is
the integration follow-on requiring GPU runtime.

## Headline rule

> **Zero allocations during a resize gesture.** RQ-S1
> acceptance bound. The structural guarantee comes from
> `ElasticBuffer`'s gesture-clamp; this telemetry contract
> ships the runtime detector that flags any violation.

## Contract layer

`crates/frankenterm-core/src/elastic_buffer_gpu_telemetry.rs`:

- **`BufferLifecycleEvent`** — 6-variant taxonomy:
  `GestureBegin`, `GestureEnd`, `Grow{new_capacity}`,
  `Shrink{new_capacity}`, `FrameWrite{instances_written}`,
  `IdleTick`. The integration emits one per buffer-touching
  operation.
- **`ElasticBufferGpuHealth`** — `*Health` snapshot mirroring
  this session's pattern: `grow_count`, `shrink_count`,
  `high_water_mark`, `capacity`, `used`,
  `grows_during_gesture_total`, `gesture_active`. The
  `grows_during_gesture_total` counter is the load-bearing
  RQ-S1 detector — non-zero means an alloc fired during a
  resize.
- **`fold_event`** — bit-for-bit-faithful state-machine
  reducer; produces the snapshot from a stream of lifecycle
  events.
- **`check_invariants`** — names 3 violations:
  `GrowDuringGesture`, `ShrinkDuringGesture` (the parent
  bead's "shrink should also be suppressed during gesture"
  rule from `ft-mpc9b.1.3`), `UsedExceedsCapacity` (buffer
  overrun).

## Bench corpus

`bench_scenario_corpus()` returns 3 named scenarios:

| Scenario | SLO | Acceptance |
|---|---|---|
| `ResizeBurst` | RQ-S1 | `grows_during_gesture_total == 0` after 10× rapid resize gesture |
| `IdleShrink` | RQ-S5 | `shrink_count >= 1` after 1-hour idle session |
| `HeavyBurst` | RQ-S6 | Frame latency p95 < 16ms under sustained char throughput |

`BenchRunResult::evaluate(scenario, final_health)` runs the
acceptance check against the recorded health snapshot;
`BenchSuiteSnapshot::all_pass()` is the per-release release
gate.

## QuadInstance shape contract

`QuadInstanceShape::DEFAULT` declares 32 bytes per instance,
6 fields. The integration verifies actual `std140`/`std430`
layout matches via `static_assertions` at compile time;
buffer sizes use `QuadInstanceShape::buffer_bytes(cell_count)`
for the wgpu allocation.

## Tests

23 lib tests covering: every event-fold transition,
high-water-mark monotonicity, gesture-grow detector firing
correctly, all 3 invariant variants, bench scenario pass/fail
predicates per scenario, snapshot record-replaces-on-dup,
serde roundtrip, 1024-trial × 32-event random schedule sweep
asserting check_invariants determinism.

## Bead acceptance status

| Item | Status |
|---|---|
| `grow_count` / `shrink_count` / `high_water_mark` / `capacity` / `used` telemetry fields | ✓ `ElasticBufferGpuHealth` |
| Gesture-grow violation counter | ✓ `grows_during_gesture_total` + `is_safe()` predicate |
| 3 bench scenarios with named acceptance bounds | ✓ `bench_scenario_corpus` |
| ElasticBuffer<QuadInstance> wgpu surgery | ⏳ integration follow-on (requires GPU runtime) |
| Mirror grow/shrink onto wgpu::Buffer | ⏳ integration follow-on |
| Per-cell instancing replaces per-cell vertex batching | ⏳ integration follow-on |
| Bench source files at crates/frankenterm-core/benches/ | ⏳ requires GPU runtime to populate fixtures |
| ft doctor wiring (one-line projection) | ⏳ integration follow-on |
| Per-release attestation entry | ⏳ depends on `ft-syqcz.1` |

## Cross-references

- **Parent policy lifecycle:** `ft-kciew` shipped at
  `c88dde9c0` — `TermWindow.quad_buffer_policy:
  ElasticBuffer<u32>`.
- **ElasticBuffer impl:**
  `crates/frankenterm-gui/src/termwindow/render/elastic_buffer.rs`
  — `grow_count` / `shrink_count` / `high_water_mark`
  accessors live there.
- **SLO:** `docs/perf/resize-quality-slo.md` — RQ-S1 (resize
  FPS), RQ-S5 (idle GPU), RQ-S6 (heavy-burst input latency).
- **Sibling integrations:** `ft-c9arc` (atlas), `ft-tfzhy`
  (dirty-lines) — this is the third leg of the trio.
- **Sibling foundation fixtures** (same `*Health` /
  contract-layer pattern this session):
  `wayland_compositor_matrix`, `tui_parity_oracle`,
  `gpu_regression_fuzz_report`, `dec_2026_presentation_hold`,
  `iterm2_osc1337`, `osc_2x_cluster`, etc.
- **Attestation cross-link:** `ft-syqcz.1`.
