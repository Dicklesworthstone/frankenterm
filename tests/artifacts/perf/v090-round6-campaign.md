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
