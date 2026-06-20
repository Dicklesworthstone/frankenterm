# Round-6 Negative-Evidence Ledger

> The Alien Optimization Gauntlet (v0.9.0 campaign). **Load-bearing:** every round-6 optimization that is
> *rejected*, *reverted*, or *measured-as-no-win* gets an entry here closed with exactly one of the 8
> grep-able **retry-condition predicate** forms. Negative evidence is a *win*, not a failure.

The **10 keep-gate rules**, the **8 retry-condition forms**, the **forbidden anti-vocabulary**, and the
rejected-entry template are defined once in [`round4-negative-results.md`](round4-negative-results.md) —
they carry over unchanged. Kept/promoted changes → [`round6-keep-ledger.md`](round6-keep-ledger.md).
Campaign record → [`../../tests/artifacts/perf/v090-round6-campaign.md`](../../tests/artifacts/perf/v090-round6-campaign.md).

## Round-6 bench-host caveat (operator-confirmed)

Benches run on the **local Mac under swarm load** → cv ~15-20%. Therefore round-6 **only certifies
large-effect, non-overlapping wins (≥2×)**. A small-effect or quality-metric (evicted-bytes / hit-rate /
RSS / alloc-count) candidate that cannot be adjudicated under this cv is NOT a reject — it stays
default-off with a **Form-7** predicate naming the quiet-host / quality-metric bench that would unblock it.
Only a measured **regression on a default-on path** triggers a `git revert`.

## PRE-REJECTED classes (round-5 evidence — do NOT re-propose without NEW evidence)

These lost at real sizes in round 4/5; grep round4+round5-negative-results.md before any pattern touches them.
- **Custom replacements of stdlib HashMap/Vec** — perfect-hash (M5 MPHF +69% slower @192 anchors),
  fingerprint dedup (Q6 +8.8% slower @6144 keys), SIMD packed-literal prefilters (Q5 Teddy +0.5% noise).
- **Prefilters/caches whose overhead ≈ savings** (Q5 class).
- **Controller/policy swaps whose "win" is a quality metric, not wall-clock** (M9 PID compute-only,
  S3-FIFO 2× compute). Adjudicable only with the `.20` quality harness on a quiet host.
- **COW/snapshot scrollback to dodge a lock** (M6) — measured sub-µs lock-wait (250ns @200 panes,
  3 orders below the 50µs bar); clone costs MORE than scan-under-lock. KILLED.
- **Micro-opts of already-sub-µs paths** (redaction lookback, LRU token mgmt, FNV embedding, RRF fusion —
  all confirmed already-optimal by the round-6 hot-path investigation).

---

## Entries

### 2026-06-20 | ft-p4vzl.2 | B1 incremental cross-chunk Aho-Corasick (kill trigger_data_buffer re-scan) — FLAGSHIP

**Status:** no-bounded-micro-lever — the named target is dead code; the real production analog has no feasible streaming-AC lever and no profiled hot-frame attribution.

**Gate (intended):** a new `FT_MOONSHOT_*` env gate (never created — implementation did not proceed past the profile-first gate).

**Profile attribution:** 0% — the named frame is unreachable. The marching-orders B1 target is the
`scan_pipeline::ChunkedPipelineState::flush` re-scan of the accumulated `trigger_data_buffer`
(README §"Cross-chunk subtlety", lines 1828-1830). Repo-wide grep proof:
`ScanPipeline` / `ChunkedPipelineState` / `TriggerScanner` / `scan_pipeline::` / `pattern_trigger::`
have **zero** non-test, non-bench, non-doc references across `crates/` + vendored `frankenterm/`
(0 lines). That whole-window re-scan is exercised only by unit tests and benches → 0% self-time on any
realistic workload → cannot clear the >=0.5% profile-first gate.

**Real production path (the only cross-chunk re-scan that ships):** `PatternEngine::detect_with_context`
(`patterns.rs:4423`), driven per pane segment from `runtime.rs:3748`
(`detect_with_context(bounded_segment.content, &mut ctx)`). Cross-segment handling is NOT a whole-window
re-scan: it prepends a bounded `DetectionContext::tail_buffer` (`MAX_TAIL_SIZE` =
`PatternsTuning::DEFAULT_MAX_TAIL_SIZE_BYTES` = **2048 B**) to each new segment and re-scans only that
tail; segments are capped at `IngestTuning::DEFAULT_MAX_PERSIST_SEGMENT_BYTES` = **64 KiB**. The common
no-match case is rejected by `quick_reject` before Aho-Corasick runs at all, so the steady-state
"double-work" is at most a 2048-byte prefilter re-scan per segment (<=~3% extra bytes at the 64 KiB cap;
worst-case relative overhead only for tiny <256 B segments where the absolute cost is sub-µs and
prefilter-bound).

**Feasibility of the proposed lever (streaming AC state across chunks):** infeasible with the
`aho-corasick` crate — there is no resumable `MatchKind::LeftmostFirst` stream API (`stream_find_iter`
is `MatchKind::Standard`-only). LeftmostFirst requires unbounded lookahead across the boundary, which is
exactly *why* both the (dead) scan_pipeline and the (live) `detect_with_context` use an overlap/tail
re-scan. Carrying automaton state would require a hand-rolled LeftmostFirst automaton — a
custom-structure rewrite (pre-rejected risk class) with a large byte-equivalence-bug surface, against a
re-scan that the prefilter already makes sub-µs.

**Measurement (focused, evidence bench):** `pattern_detection::b1_cross_chunk_rescan` — committed
isolation bench (this commit). Two arms over an identical non-matching segment stream:
`tail_overlap` (`detect_with_context`, prod path) vs `no_tail` (`detect`, no cross-segment tail), across
128 B (worst-case relative) and 8 KiB (typical) segment regimes. Run on a quiet host to confirm the
tail-overlap overhead is far below the round-6 >=2x certifiable bar.

**Behavior-preservation:** N/A — no optimization landed; the only code added is a measurement bench and
this ledger entry.

**A/B verdict:** not run — candidate did not pass the profile-first gate (dead-code target + infeasible
lever). No default flipped.

**Retry-condition predicate (Form 1):** retry only if the B0 flamegraph (ft-p4vzl.1) attributes a
clearly-above-noise share (>=0.5% self-time) to the `detect_with_context` tail-overlap re-scan on the
high-pane realistic-render workload AND a resumable LeftmostFirst-equivalent scan becomes available
(crate support or an equivalence-proven minimal automaton). Until both hold, the bounded 2048-byte
prefilter-gated tail is the correct design.

**Rollback:** N/A (no optimization landed; flag never created). Evidence bench + ledger entry only.

**Sibling references:** ft-p4vzl.1 (B0 profiling gate), ft-p4vzl (round-6 epic). Adjacent reality-gap
noted for the orchestrator: README §"Cross-chunk subtlety" (lines 1828-1830) describes the dead-code
`trigger_data_buffer` mechanism as though it were the production cross-chunk engine; the live engine is
`detect_with_context`'s `tail_buffer`.

---

### 2026-06-20 | ft-p4vzl.3 | B2 Q3 KMP linear-overlap — BELOW the realistic gate (robustness, not perf)

**Status:** below-gate. B0 (round6-profile-targets.md) measures `ingest.extract_delta` at 24 ns/call =
**0.177%** realistic fleet self-time — below the ≥0.5% gate (keep-gate rule 9). cc_2's forced A/B
(doc-hidden `extract_delta_with_overlap_mode`, `delta_adversarial_overlap` group) shows KMP wins on the
adversarial repeated-first-byte O(n²) worst case, but that frame does not move the realistic needle.
**Treat Q3 as correctness/robustness hardening of the worst-case overlap path, NOT a throughput win — do
NOT promote on perf grounds.** Default-off (config `ingest.delta_linear_overlap`).
**Retry (Form 7):** revisit only if a workload drives `extract_delta` overlap matching above 0.5%
realistic self-time (pathological near-duplicate captures at very high pane count).

### 2026-06-20 | lw0s7.20 | S3-FIFO eviction — CONDITIONAL (default-off, workload-gated)

**Status:** workload-conditional (A5 deterministic quality harness; round6-profile-targets.md). hit-rate
@cap128: scan-resistance one-hit-wonder **+77.4%** (0.090→0.160, WIN); phase-shift migrating-hot-set
**−64.2%** (0.708→0.253, REGRESSION). Plus round-5's ~2× per-op compute. **Not a blanket default —
default-off with an exact promotion condition: enable only for confirmed scan-heavy, low-recency access
patterns.**
**Retry (Form 7):** promote only behind a workload classifier that confirms scan-heavy/low-recency; never blanket default-on.

### 2026-06-20 | lw0s7.20 | M9 PID fleet-memory dampening — TIE (no captured quality benefit)

**Status:** tie (A5 quality harness). 52-cycle / 192-pane sawtooth pressure replay: total evicted bytes
identical (173 805 568 both arms), reclaim-target oscillation 0 for both. M9's claimed quality benefit
(fewer evicted bytes / less oscillation) does NOT manifest; round-5's compute −10% buys no captured win.
Default-off.
**Retry (Form 7):** revisit only if a band-edge replay (drives `fleet_warm_bytes_target` above-then-below
a fixed band, inducing ≥1 hysteresis direction change) shows PID with strictly fewer direction changes
and/or evicted bytes than hysteresis.

### 2026-06-20 | ft-p4vzl.4 | M3 SoA instanced glyph quads — no realistic win on Apple Metal (profiling)

**Status:** measured-no-win (cod_4 GPU profiling). On the glyph-dense SoA bench the per-frame buffer
create/bind path (`create_glyph_quad_instance_buffers` → `set_vertex_buffer`×7 → draw) samples BELOW
threshold: `setVertexProgramBuffer` 1/7044, `drawPrimitives` 13/7044 main-thread samples. The dominant
sampled cost is Metal readback/wait + render-pass/command-buffer machinery, NOT vertex-buffer bandwidth.
The SoA layout premise (vertex-bandwidth win) does not materialize on the Apple Metal backend at this
glyph density. Default-off (env `FT_MOONSHOT_INSTANCED_GLYPH_QUADS`); no GPU frame appears in the B0
realistic CPU hot-frame list.
**Retry (Form 1):** retry only if a live-render profiling scenario reaching `draw.rs:158` →
`webgpu.rs:1545` attributes ≥0.5% frame-time to vertex-buffer create/bind/upload (discrete-GPU backend, or
far higher glyph-per-frame density than this bench).

_(round-6 measured-no-win / reject / revert entries land below, one per the rejected-entry template.)_
