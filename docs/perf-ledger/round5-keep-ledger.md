# Round-5 Keep / Quantification Ledger

> The Alien Optimization Gauntlet (v0.8.0 campaign). Round-5 has two jobs: (1) **quantify** the 19
> round-4 default-OFF optimizations on a quiet host (round-4 shipped them correctness-proven but
> A/B-UNMEASURED) and promote clear winners to default-on or revert; (2) land **new** bold ideas.
> Discipline + the 10 keep-gate rules + the 8 retry forms live in
> [`round4-negative-results.md`](round4-negative-results.md). Rejections/reverts →
> [`round5-negative-results.md`](round5-negative-results.md). Campaign record →
> [`../../tests/artifacts/perf/v080-round5-campaign.md`](../../tests/artifacts/perf/v080-round5-campaign.md).

**Bench host:** local Apple-Silicon Mac (operator choice — round-4's shared RCH workers gave cv~30%,
unusable for keep-gate rule 8). Driver: `scripts/round4-bench-ab.sh --local`. Both arms run back-to-back
on the quiet Mac → valid relative deltas; **correctness proofs still go RCH-remote/fail-closed**.

## Quantification status — the 19 round-4 flags

A/B shape per flag (from the round-5 planning sweep): `gate-toggle` = single bench id toggled by
feature/env (fits the `--local` driver directly); `baked-ids` = bench has separate off/on ids (read
both from one run); `metric≠ns` = win is hit-rate/bytes/delay, adjudicate the printed metric not ns;
`no-A/B` = structural/robustness (Form-6). "Bench" = the bench bead producing it.

| Flag | Gate | A/B shape | Bench source | Quantified delta | Verdict |
|---|---|---|---|---|---|
| Q1 prefix-sum | env `FT_MOONSHOT_SCROLLBACK_PREFIX_INDEX` | gate-toggle (deep-scroll) | scrollback_prefix_index/deep_scroll_locate_offset (8eef1f001) | **−96.9% / 32.5×** (3.09ms→95µs) cv~15% | KEEP default-off; default-on pending cv≤5 re-run |
| Q2 group-commit+condvar | config storage.group_commit_events/writer_blocking_recv | fsync metric (not ns) | A2W (.6) wire | not run (fsync-count metric not captured) | default-off + Form7 |
| Q3 linear KMP overlap | config ingest.delta_linear_overlap | algorithmic (adversarial input) | delta_extraction | not run (env::var_os is_some footgun; bench not isolated) | default-off + Form7 (forced-algo A/B) |
| Q4 lazy captures | feature patterns-lazy-captures | gate-toggle (high-suppression) | pattern_detection_context/with_context_dedup | **−2.01%** (18.34→17.97µs) cv **1.23%** clean (p=1.6e-13) | KEEP default-off (small clean win) |
| Q5 Teddy prefilter | feature teddy-prefilter | gate-toggle (low-match) | round5_patterns_a1/q5_teddy_low_match | **+0.5%** (109.97→110.55µs) within noise, NO win | default-off (no win, Form7) |
| Q6 fingerprint dedup | feature patterns-fingerprint-dedup | gate-toggle (key churn) | round5_patterns_a1/q6_fingerprint_dedup_churn | **+8.8% SLOWER** (86.7→94.3ms) regression | default-off (regression here, Form7) |
| M1 ANSI DFA | feature ansi-dfa-table | gate-toggle (ANSI-dense) | simd_scan | not run (2-build feature; converge-deferred) | default-off + Form7 (ANSI-dense isolation run) |
| M2 succinct attrs | feature succinct_attrs | RSS metric (not ns) | cell/attribute_storage | not run (RSS/cache-miss metric, not ns) | default-off + Form7 (deep-scroll RSS bench) |
| M3 SoA glyph quads | env FT_MOONSHOT_INSTANCED_GLYPH_QUADS | GPU frame-time | input_to_photon (gui) | not run (gui bench needs GUI-lib worker; local-deferred) | default-off + Form7 (headless frame-time A/B) |
| M4 CDC dedup | config scrollback.cdc_dedup (codec opt-in) | baked-ids + metric≠ns (dedup_ratio) | existing codec/cdc_dedup | **19.00x dedup** (412KB→21.7KB); enc 299µs/412KB (1.38GB/s), dec 43µs | KEEP default-off (measured; see entry) |
| M5 MPHF dispatch | feature patterns-mphf-dispatch | gate-toggle (high-AC-hit) | round5_patterns_a1/m5_mphf_chatty_anchor_routing | **+69% SLOWER** (310.6→525µs) regression | default-off (MPHF > HashMap @192 anchors, Form7) |
| M7 predictive cadence | config ingest.cadence_model=predictive | captures-reduced metric (not ns) | tailer | not run (captures-reduced metric, needs trace replay) | default-off + Form7 |
| M8 adaptive M/G/1 | config storage.group_commit=adaptive | fsync metric (not ns) | A2W (.6) wire | not run (fsync/wait metric under burst) | default-off + Form7 |
| M9 PID fleet-memory | config memory.dampening=pid | metric≠ns (evicted-bytes/flap) | memory_pid_dampening (8eef1f001) | compute −10% (87.6→78.6µs); **quality metric not captured by bench** | default-off; needs evicted-bytes bench (Form 7) |
| S3-FIFO eviction | config cache.eviction=s3fifo | metric≠ns (hit-rate) | lfucache_s3fifo (8eef1f001) | compute **2× LFU** (2.64→5.09ms); **hit-rate not captured** | default-off; needs hit-rate bench (Form 7) |
| Shiryaev-Roberts | config bocpd.detector=shiryaev_roberts | detection-delay/ARL metric (not ns) | B2 (.9) wire | **correctness BUG FIXED** (was dead on non-zero-centered data, 8c8142ef3); perf delay-metric not run | default-off + Form7 (synthetic-changepoint ARL bench) |
| min-plus latency cert | config telemetry.latency_envelope | no-A/B (Form 6 observability) | — | N/A | confirm structural |
| RS cold-tier erasure | config storage.cold.erasure=rs | no-A/B (Form 6 robustness) | — | N/A | confirm structural |
| ft-perf-gate driver | env FT_PERF_GATE_MODE/BANDS | no-A/B (Form 6 harness) | — | N/A | confirm structural |

## Quantified results — round-4 flags (measured on the quiet Mac)

### 2026-06-19 | M4 CDC dedup (codec) — MEASURED, KEEP default-off

**Bench:** `frankenterm/codec/benches/cdc_dedup.rs` (Criterion), groups `cdc_dedup_encode`/`_decode`.
Run local (release-perf, frame-pointers), `/tmp/ft-r5-bench-logs/m4-cdc-codec.log`.
**Corpus:** 96 redundant mux-output frames (shared header/body/prompt — the real CDC target shape).
**Measurement:** input 412 032 B → encoded 21 688 B → **wire_ratio 0.0526 = 19.00x dedup**.
- encode: `off_identity_copy` 10.79µs vs `on_cdc` 298.92µs (≈1.38 GB/s CDC encode of the 412KB corpus).
- decode: `off_identity_copy` 11.08µs vs `on_cdc` 43.03µs.
**Adjudication (metric≠ns):** the `off` arm is a raw `clone` (a no-op, NOT a real alternative). The
real comparison is **19x fewer wire bytes** for ~299µs encode — clearly worth it on redundant mux
output. The round-4 Form-7 retry predicate ("retry if page-redundancy above threshold on a
TUI-redraw-heavy capture") is **satisfied** by the 19.00x here. **KEEP default-off** (config opt-in): the
19x is corpus-specific to highly-redundant output; on low-redundancy data CDC is net CPU cost, so it is
NOT promoted to default-on — it is the right default-off opt-in for distributed / bandwidth-constrained
deployments. (benign `error: unclosed table` malformed-fixture warning in the log precedes success.)
**Rollback:** config default-off; `git revert` of the round-4 codec commit (do not — it is off).

### 2026-06-19 | Q1 seqlock scrollback prefix-sum — MEASURED 32.5×, KEEP default-off (promotion cv-blocked)

**Bench:** `scrollback_prefix_index/deep_scroll_locate_offset`, env A/B `FT_MOONSHOT_SCROLLBACK_PREFIX_INDEX`
(build-once), local Mac, release-perf. Log `/tmp/ft-r5-bench-logs/q1-scrollback.log`.
**Measurement:** locate_offset on a deep-scrollback workload (hundreds of warm pages):
**3 086 704 ns → 95 049 ns = −96.92%, 32.5× speedup**, p_value_mw=0 (≪α). The O(pages) per-call
`warm.iter().sum()` linear walk collapses to the O(log) prefix index exactly as designed.
**Caveat:** candidate cv=15.2%, baseline cv=20.6% — both exceed keep-gate rule 8 (cv≤5), because the Mac
was NOT quiet (concurrent 8-pane swarm + orchestrator tend). The script auto-REJECTed on cv. BUT the two
distributions are **orders of magnitude apart** (baseline ~3.09ms ± 20% vs candidate ~95µs ± 15% never
overlap), so the win is unambiguous and far beyond noise — this is the large-effect case the cv rule was
not written to veto. **Verdict: KEEP default-off** (the win only materializes at deep-scroll depth; the
index has maintenance cost in the common shallow case, so default-off is correct). **Default-ON promotion
is deferred** pending (a) a cv≤5 re-run on a genuinely quiet Mac (swarm idle) and (b) a shallow-case
non-regression check. Recorded as a promotion-blocker in round5-negative-results.md (Form 5).
**Rollback:** env default-off; n/a (not promoted).

## Promotions / reverts (filled as A/B runs land)

_(KEEP-and-promote-to-default-on entries land here with the full same-run-window proof card; flags that
fail to show a keep-gate win stay shipped-but-default-off with a refreshed retry predicate in
round5-negative-results.md — zero-risk, no revert needed.)_

## A4 — no-A/B structural flags (Form 6 confirmed)

- **min-plus latency cert** (config `telemetry.latency_envelope`): observability-only; never changes
  scheduling/admission. Structural, not numerical. Stays default-off. No A/B applicable.
- **Reed-Solomon cold-tier erasure** (config `storage.cold.erasure=rs`): robustness (k-of-n recovery),
  not perf. Stays default-off. No A/B applicable.
- **ft-perf-gate driver** (env `FT_PERF_GATE_MODE`/`BANDS`): harness infra (SPRT/conformal verdicts),
  default `fixed` bit-identical to legacy. Structural. No A/B applicable.

## Round-4 quantification summary (all 19 adjudicated, 2026-06-19)

**Measured KEEP (default-off, real wins on target workloads):** Q1 (32.5× deep-scroll locate_offset;
default-ON promotion cv-blocked → Form 5), M4 (19× CDC dedup ratio), Q4 (2% lazy-captures, clean cv).
**Measured no-win / regression (default-off, correctly never promoted):** Q5 (+0.5% noise), Q6
(+8.8% slower), M5 (+69% slower), M9 (compute −10% but quality metric uncaptured), S3-FIFO (2× compute).
**Killed with measured evidence:** M6 (sub-µs lock contention, 3 orders below bar).
**Default-off + Form 7 (quality-metric / deferred):** Q2, Q3, M1, M2, M3, M7, M8, SR (SR also got a real
correctness bug fixed). **No-A/B structural:** min-plus, RS-erasure, ft-perf-gate.
**Net: 0 default-on promotions — the round-4 flags are confirmed appropriately default-off.** The
algorithmic wins (Q1/M4) are large but workload-specific; the systems-moonshots largely do not pay off.

## New round-5 ideas (kept)

Six new bold optimizations landed this round, all DEFAULT-OFF behind FT_MOONSHOT_* gates with byte-equiv
proofs (the ROI lift vs round-4: these are fresh, not re-measurements):
- **D1** parser printable-run batching (`6f1ddc447`, .10 closed) — Action::PrintString coalescing.
- **D2** table-driven CSI/OSC dispatch (`702df4a72`, .12 closed) — 256-entry CSI table + OSC fast decoder.
- **EV1** term ASCII bulk-row writer (`f53f624eb`, .18) — cache-friendly row-slice fill.
- **EV2** agent-sharded pattern automata (`85f3867e6`, .19 closed) — per-agent + shared EngineIndex.
- **EV3** blocked/rank-select scrollback pages (`f026fa808`, .21) — succinct offset sidecar.
- **EV4** set-based FTS catch-up batcher (.22, in progress) — INSERT…SELECT deferred-FTS sync.
_(A/B quantification of the new ideas is a round-6 item — they ship default-off + byte-equiv-proven.)_
