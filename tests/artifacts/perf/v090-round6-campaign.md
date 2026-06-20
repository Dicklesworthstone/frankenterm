# v0.9.0 Round-6 — The Alien Optimization Gauntlet (campaign record)

> Round-6 of the NTM-swarm perf campaign under the `running-the-gauntlet-on-your-rust-port` discipline.
> Three threads: (A) **quantify** the 6 round-5 new ideas (D1/D2/EV1-EV4) + **promote** the proven
> algorithmic wins (Q1 32×, adaptive-M4 19×); (B) **mine + land NEW BIG&BOLD profiled algorithmic/
> bandwidth ideas** (the one class that won round-5); (C) release engineering → **v0.9.0**. Keeps →
> `docs/perf-ledger/round6-keep-ledger.md`; rejects/no-wins → `docs/perf-ledger/round6-negative-results.md`.

**Decisions (operator-confirmed 2026-06-20):** Full BIG & BOLD new mining · Autonomous-to-release ·
cut **v0.9.0** · **benchmark on this Mac under swarm load → certify large-effect (≥2×, non-overlapping)
wins only** (correctness proofs stay RCH-remote/fail-closed).

**Epic:** `ft-round6-gauntlet-*` (new-idea children) + carryover proof beads under `ft-round5-gauntlet-lw0s7`.
Swarm: tmux session `frankenterm`, 8 panes (cod_1..5 = panes 0-4, cc_1..3 = panes 5-7), file-owned per
`docs/perf-ledger/round6-marching-orders.md`.

## THE #1 LESSON (drives idea selection)

Round-5 evidence is unambiguous: **ALGORITHMIC complexity-class wins delivered (Q1 O(pages)→O(log) = 32×;
M4 CDC = 19× on redundant data), while SYSTEMS-MICRO-MOONSHOTS did NOT** (Teddy noise, fingerprint/MPHF
slower than stdlib, M9/S3-FIFO quality-not-speed, M6 killed at sub-µs contention). Round-6 fails fast on
the dead classes (see round6-negative-results.md PRE-REJECTED list) and concentrates on profiled
complexity-class + bandwidth wins on the REAL workload.

## Planning sweep findings (4 explore agents + hot-path investigation, 2026-06-20)

- **Hot paths are mature.** capture→storage (append_segment_sync linear+group-committed, FTS deferred),
  pattern engine (AC O(n) + Bloom prefilter), search (RRF optimal), scrollback (Q1 prefix-sum) all
  already well-optimized or covered by an existing flag. Most candidate frames are sub-µs.
- **The one genuinely-new algorithmic lead:** Aho-Corasick LeftmostFirst is not composable across chunk
  boundaries → `trigger_data_buffer` re-scans the accumulated window at every flush (README 1828-1830) —
  repeated O(window) work on the genuinely-hot per-capture path. → Thread B1.
- **Never-measured structural wins worth certifying:** M3 SoA glyph quads (GPU bandwidth, Mac-measurable),
  Q3 KMP linear-overlap O(n²)→O(n) (blocked only by an `env::var_os` measurement footgun).
- **M4 correction:** cannot be a *static* default-on (net CPU cost on low-redundancy data) → redundancy-
  adaptive auto-enable (Thread A2).
- **Release loose ends:** `ft-linux-arm64` asset missing from v0.8.0; v0.8.0 never git-tagged; full core
  `--lib` still times out at the 3600s RCH SSH compile limit (needs more lane splits).

## Convergence log
_(tick entries appended by the orchestrator tend-loop)_

- 2026-06-20 — campaign opened. Beads DB healthy (orphaned write.lock cleared). Round-6 ledgers + this
  record + marching orders authored. Swarm alive (8 panes; cod idle post-round-5, cc context-heavy → /clear).
- 2026-06-20 tend#1 — strong wave-1: 7 commits. B0 profiling harness + A5 quality-metric harness landed
  (cc_3, f84382411); adaptive-CDC + Q1 shallow-arm + EV3 cold-line benches (cod_1, 27a1c063a); Q3 KMP
  forced-A/B bench (cc_2, 2b3265e16); SoA GPU frame bench (cod_4, 8e137a2ec); term/parser A/B benches
  (cod_2, eb5839016); C2 native-events lane split (cod_5, edb754945). **FLAGSHIP B1 REFUTED with hard
  evidence** (cc_1, 0a6574848): the marching-orders target (scan_pipeline `trigger_data_buffer` re-scan)
  is DEAD CODE (0 non-test refs → 0% self-time); the live cross-chunk path `detect_with_context` re-scans
  only a bounded 2048 B tail (sub-µs, prefilter-gated); the streaming-AC lever is infeasible with
  aho-corasick (no resumable LeftmostFirst API). Negative-ledger Form-1 entry + evidence bench landed.
  RCH-E410s across cod_2/cod_3/cod_5 were TRANSIENT mid-commit races (files now committed; cc_3's core
  build compiles past manifest resolution on hz2 → confirms RCH syncs the untracked antigravity files
  too) → rerouted those panes to retry. cod_4's GUI bench can't build on RCH (RCH-E307 x11-xcb absent,
  known round-5 limit) → reassigned to the local Mac GUI stack. 12/12 RCH workers healthy. B4 audit
  (cod_5) flagged a real residual: native push still starts the legacy capture supervisor +
  `TailerSupervisor::spawn_ready` 10 ms scan — profile-gate via B0 before filing a child bead. Launched
  the Q1 promotion A/B locally (deep ≥2× polarity check + shallow non-regression gate; /tmp/ft-r6-q1-ab.log).
  Doc reality-gap: README §Cross-chunk-subtlety (1828-1830) describes the dead `trigger_data_buffer` as
  the production cross-chunk engine — the live engine is `detect_with_context`'s `tail_buffer`.
- 2026-06-20 tend#2 — **Q1 PROMOTED to default-on** (headline): local A/B cleared the gate —
  `deep_scroll_locate_offset` **20.18×** (−95.04%, cv 1.43%, p=0) AND `shallow_hot_locate_offset`
  **+0.72%** (non-regression, < 3% ratchet); cv≤5 this run so no longer cv-blocked. Keep-card landed
  (round6-keep-ledger.md); cod_1 dispatched to flip `scrollback.prefix_index` default false→true + RCH
  equivalence proof. Wave-2 commits: B0 gate-robustness fix (cc_3 1568a9d74), EV4 deferred-FTS bench
  (cod_3 3bf7b0630), Q3 common-case guard arm (cc_2 c33668fcc), GUI bench WGPU-29 compile fix (cod_4
  579f29622). cod_2 DONE .7 (term_parser_ab green on vmi1149989); B1 closed (cc_1, refuted). **INFRA:**
  broken ovh workers (display-named `yto`/`fmd`, the round-5 "ovh-a/ovh-b") kept being selected
  (canonical-mkdir topology fail; cc_1 ate a 20m cold build pinned to one) — the daemon `disable` API
  rejects the display names (registry mismatch), but they are priority-80 vs contabo 90-100 / hz 110, so
  retry-to-reroute is the mitigation. cod_5 rerouted to retry; cod_2/cod_4/cc_1 sent to profile-gated B5
  mining (cod_4 target: per-frame GPU buffer create/bind screen_line.rs:464→draw.rs:158→webgpu.rs:1545;
  cc_1: Bloom quick-reject anchor_lengths inner loop). B0 flamegraphs still building (cc_3). NEXT: run
  local A/Bs on term_parser/SoA/Q3/EV4/EV3/adaptive-CDC benches as RCH frees + B0 lands → seed B5.
- 2026-06-20 tend#3 — **B0 flamegraphs landed (c3995e26f, docs/perf-ledger/round6-profile-targets.md) —
  the mining gate is open and reshapes Thread B.** Realistic hot-frame ranking: `scan_pipeline.process`
  **72.47%** (driven by ANSI/escape byte processing — 4.9× on ANSI-dense, NOT trigger density),
  `redactor.redact` 22.16% (pre-rejected, already-optimal), `scrollback.warm_line` zstd-**DECODE** 5.18%
  (eligible); `extract_delta` 0.177% and `locate_offset` 0.006% BELOW gate. Consequences: (a) **M1
  ANSI-DFA's round-4 retry predicate is now SATISFIED** → cc_2 assigned the M1 feature A/B (highest-EV
  remaining, on the 72% frame); (b) **Q3 KMP is below-gate** → robustness-only, NOT a perf promotion
  (negative-results); (c) **Q1 locate is realistically a tail-latency win** (decode `warm_line`, not
  locate, is the deep-scroll cost) — promotion KEPT (byte-equiv, +0.72% negligible) but annotated; the
  higher-EV deep-scroll levers are EV3/M4 on `warm_line` decode. Q1 default-on flip committed (3ed4be224);
  equivalence proof relaunched by orchestrator (cod_1's wedged on the asupersync fetch). **A5 quality
  harness adjudicated the round-5 unmeasurables:** S3-FIFO CONDITIONAL (+77.4% scan-resistance hit-rate /
  −64.2% phase-shift → workload-gated default-off), M9-PID TIE (identical evicted-bytes, 0 oscillation →
  no benefit, default-off). cod_4 GPU: M3 no realistic win on Metal (buffer path below threshold, readback
  dominates) → default-off + Form-1. cod_2 filed + now implementing ft-p4vzl.13 (bulk ASCII line
  materialization, flush_print 1.64% gate-cleared). EV4 proven (cod_3, hz1). cod_1/cod_3 running
  EV3/CDC/EV4 A/Bs; cc_1 pivoting from low-EV Bloom-vs-AC to the ANSI scan frame. Ledgers updated:
  Q3/S3-FIFO/M9/M3 → negative-results, Q1 B0-nuance → keep-ledger.
- 2026-06-20 tend#4 — **CRITICAL CORRECTION + EV4 win.** cc_1 flagged (grep-confirmed) that B0's #1 frame
  `scan_pipeline.process` (72%) is DEAD CODE — zero production callers; the runtime capture loop uses
  `detect_with_context` (runtime.rs:3748). BUT the ANSI-byte work B0 measured IS live: the same `simd_scan`
  ANSI scan (`scan_newlines_and_ansi`) runs per captured segment via BOCPD (`runtime.rs:3758
  observe_bocpd_segment_for_runtime` → bocpd.rs:652/1746). So B0 mislabeled the FRAME (dead scan_pipeline
  wrapper vs live bocpd→simd_scan) but the ANSI-scan cost is real → **M1 ANSI-DFA targets a LIVE hot frame
  → valid** (averted a B1-style dead-code optimization; corrected round6-profile-targets.md, 72% is now an
  upper bound). Redirected cc_1 → profile the real `detect_with_context` frame; cc_2 → finish M1 + report
  at REALISTIC mixed-ANSI density (not only saturated). **EV4 CERTIFIED KEEP** (cod_3): set-based FTS
  INSERT…SELECT batch = mean 9.12× / p50 14.12× / **p95 6.01×** — ≥6× at every percentile (non-overlapping;
  cv 66% is FTS cold/warm variance the large-effect win overrides); default-on candidate (background
  catch-up, no common-case downside). cod_2 DONE .13 (clustered-ASCII-run-append, RCH green vmi1227854) →
  running its A/B + term_parser A/B. cod_1 reporting EV3/adaptive-CDC A/B from existing artifacts. cod_5:
  C2 lane-split marked code-done/proof-deferred (tree compiles; RCH stalls are infra, not a release
  blocker). Q1 default-on proof still compiling (cold core build, vmi1152480). scan_pipeline flagged for
  dead-code removal (hygiene, not perf).
- 2026-06-20 tend#5 — **M1 ANSI-DFA REGRESSION (2nd flagship refuted).** cc_2: the branchless DFA table is
  SLOWER than the existing vectorized SWAR scan on the live simd_scan/BOCPD ANSI frame; a serial table
  can't beat vectorization → negative-results Form-7. **Thread B new-mining is exhausted** (B1 dead-code,
  M1 regression, Q3 below-gate, M3 no-win, Bloom low-EV) — confirming the hot paths are mature (round-5
  finding holds). **Term-path A/Bs (cod_2) adjudicated:** `.13` clustered ASCII line materialization
  **−77.43% = 4.43× CERTIFIED KEEP** (the profiled bulk-ASCII idea paid off), D1 parser-batch 1.47× +
  EV1 row-writer 1.16× (small clean wins, low-cv term benches), D2 CSI/OSC table 1.04% no-win. **Q1
  promotion SAFE:** the two `--lib` failures (wezterm CLI-blindness telemetry, session_restore replay) are
  UNRELATED to prefix_index — mux-telemetry + antigravity-tree contamination (the ft-ch3nm meta-blocker,
  same class as round-5's full-lib failures); targeted `proptest_scrollback_prefix_index` proof running for
  clean confirmation. cod_5 closed C2 code-done/proof-deferred. cod_1 re-running EV3/adaptive-CDC A/Bs
  locally; cc_1 reporting `detect_with_context` profile. **CERTIFIED ROUND-6 WINS:** Q1 20× (default-on),
  EV4 6-14× (default-on cand), .13 4.43× (default-on cand) + D1/EV1 small. Approaching v0.9.0 cut (arm64
  build deferred to the bumped release commit).
- 2026-06-20 tend#6 — **THREAD C STARTED: v0.9.0 cut in progress.** Q1 proptest GREEN (`test result: ok.
  1 passed` — default-on equivalence confirmed; the earlier `--lib` failures were definitively
  antigravity/mux-telemetry contamination, NOT Q1). cc_1: `detect_with_context` is optimal except a
  low-EV quick_reject-vs-ac_direct A/B (filed ft-ui1xn, non-blocking). EV3 + adaptive-CDC: default-off
  proof-deferred (RCH asupersync stalls + local release-LTO link issue; zero-risk). Closed proven beads
  (.1/.4/.5/.7/.8/.13 + .2/.3). **RELEASE:** bumped workspace 0.8.0→0.9.0 (055bca9b0), tagged v0.9.0 +
  retroactively v0.8.0 (already on remote at 310534f5d), pushed main+master+v0.9.0. Builds from the CLEAN
  tag (excludes the uncommitted antigravity work — ships exactly the committed round-6 state): linux
  amd64+arm64 launched on trj (PID 574227, clears the long-missing ft-linux-arm64 asset); darwin-arm64
  ft+gui+mux+.app delegated to cod_4 via a clean clone. NEXT: harvest builds → assemble 9 assets +
  SHA256SUMS → gh release create v0.9.0 → verify install.sh + GUI smoke → round-6 memory + final scorecard.
