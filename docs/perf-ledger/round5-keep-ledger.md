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
| Q1 prefix-sum | env `FT_MOONSHOT_SCROLLBACK_PREFIX_INDEX` | gate-toggle (deep-scroll) | A1S (.5) new | _pending_ | _pending_ |
| Q2 group-commit+condvar | config storage.group_commit_events/writer_blocking_recv | gate-toggle (WAL burst) | A2W (.6) wire | _pending_ | _pending_ |
| Q3 linear KMP overlap | config ingest.delta_linear_overlap | gate-toggle (box/emoji) | A2W (.6)/delta_extraction | _pending_ | _pending_ |
| Q4 lazy captures | feature patterns-lazy-captures | gate-toggle (high-suppression) | existing pattern_detection_context | _pending_ | _pending_ |
| Q5 Teddy prefilter | feature teddy-prefilter | gate-toggle (low-match) | A1P (.4) new | _pending_ | _pending_ |
| Q6 fingerprint dedup | feature patterns-fingerprint-dedup | gate-toggle (key churn) | A1P (.4) new | _pending_ | _pending_ |
| M1 ANSI DFA | feature ansi-dfa-table | gate-toggle (ANSI-dense) | A2W (.6)/simd_scan | _pending_ | _pending_ |
| M2 succinct attrs | feature succinct_attrs | gate-toggle (deep-scroll RSS) | existing cell/attribute_storage | _pending_ | _pending_ |
| M3 SoA glyph quads | env FT_MOONSHOT_INSTANCED_GLYPH_QUADS | gate-toggle (glyph-dense frame) | GUI (.7) new | _pending_ | _pending_ |
| M4 CDC dedup | config scrollback.cdc_dedup (codec opt-in) | baked-ids + metric≠ns (dedup_ratio) | existing codec/cdc_dedup | _running (local)_ | _pending_ |
| M5 MPHF dispatch | feature patterns-mphf-dispatch | gate-toggle (high-AC-hit) | A1P (.4) new | _pending_ | _pending_ |
| M7 predictive cadence | config ingest.cadence_model=predictive | gate-toggle + metric≠ns (captures) | A2W (.6)/tailer | _pending_ | _pending_ |
| M8 adaptive M/G/1 | config storage.group_commit=adaptive | gate-toggle (WAL CV-high burst) | A2W (.6) wire | _pending_ | _pending_ |
| M9 PID fleet-memory | config memory.dampening=pid | metric≠ns (evicted-bytes/flap) | A1S (.5) new | _pending_ | _pending_ |
| S3-FIFO eviction | config cache.eviction=s3fifo | metric≠ns (hit-rate) | A1S (.5) new | _pending_ | _pending_ |
| Shiryaev-Roberts | config bocpd.detector=shiryaev_roberts | metric≠ns (detection delay/ARL) | B2 (.9) wire | _pending_ | _pending_ |
| min-plus latency cert | config telemetry.latency_envelope | no-A/B (Form 6 observability) | — | N/A | confirm structural |
| RS cold-tier erasure | config storage.cold.erasure=rs | no-A/B (Form 6 robustness) | — | N/A | confirm structural |
| ft-perf-gate driver | env FT_PERF_GATE_MODE/BANDS | no-A/B (Form 6 harness) | — | N/A | confirm structural |

## Promotions / reverts (filled as A/B runs land)

_(KEEP-and-promote-to-default-on entries land here with the full same-run-window proof card; flags that
fail to show a keep-gate win stay shipped-but-default-off with a refreshed retry predicate in
round5-negative-results.md — zero-risk, no revert needed.)_

## New round-5 ideas (kept)

_(D1 parser printable-run batching, D2 CSI/OSC dispatch, D3 fresh-mined candidates — each lands here
with its own A/B proof card once kept.)_
