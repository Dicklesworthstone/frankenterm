# v0.7.0 Round-4 — The Alien Optimization Gauntlet (campaign record)

> Pivot of the perf campaign to NTM swarm orchestration under the `running-the-gauntlet-on-your-rust-port`
> discipline. Mines `alien-graveyard`, `alien-artifact-coding`, `extreme-software-optimization` for radical,
> high-upside ideas — each shipped behind a default-OFF gate, proven byte-equivalent, and A/B-benchmarked
> through the new bench-AB harness before keeping. Reject/revert is cheap and *expected*; every reject lands
> in [`../../../docs/perf-ledger/round4-negative-results.md`](../../../docs/perf-ledger/round4-negative-results.md)
> with a retry-condition predicate. Keeps land in
> [`../../../docs/perf-ledger/round4-keep-ledger.md`](../../../docs/perf-ledger/round4-keep-ledger.md).

**Decisions:** Autonomous-to-release · Full BIG & BOLD · cut v0.7.0. Opened 2026-06-19.

## Gating convention (every experiment)
Default OFF behind a `FT_MOONSHOT_*` env var, a cargo feature, or a `[tuning]`/config knob (mirrors the
existing `FT_MOONSHOT_INSTANCED_GLYPH_QUADS` / `succinct_attrs`). Promote to default only after byte-equiv
golden proof is GREEN **and** bench-AB shows a keep-gated win. `#![forbid(unsafe_code)]` → safe-Rust SIMD
only (`std::simd`/memchr/aho-corasick Teddy). asupersync only. Adaptive controllers are monotone-safe +
fail-closed to the deterministic legacy path. Builds/tests/benches via RCH (remote-required, fail-closed).

## Per-idea contract (what "done" means for each)
implement → byte-equivalence/golden/property proof GREEN via RCH → bench-AB verdict (SPRT + conformal,
same run window, release-perf, cv_pct≤5) → keep (flag default-off, ledger entry) OR revert (ledger entry
with retry-condition predicate). Code-first → batch-bench → keep/revert (matches round-3 commit cadence).

---

## PHASE 0 — Gauntlet harness (foundation)

| Item | State |
|---|---|
| `release-perf` profile (thin-LTO, opt=3, frame-pointers via RUSTFLAGS) | DONE (Cargo.toml) |
| Negative-evidence ledger + keep ledger | DONE (docs/perf-ledger/round4-*.md) |
| `scripts/round4-bench-ab.sh` A/B driver (2 arms same run window → verdict via check_bench_stats.py + ft-perf-gate) | in progress |
| SPRT early-stop driver wiring `FT_PERF_GATE_MODE={fixed\|sprt\|anytime}` (sprt.rs exists) | DONE a819e28b0 (cod_3) |
| Conformal band wiring `FT_PERF_GATE_BANDS={fixed\|conformal}` (conformal.rs exists) | DONE a819e28b0 (cod_3) |
| Round-3 backfill (quantify 8 moonshots) | in progress (cod_2, gui env-A/B running) |
| ft-dkfiy succinct byte-equiv gap | DONE 2e2f729dc (cod_5, part of M2) |

## RELEASE GATES (Phase 3 — must pass before v0.7.0)
- **GUI v0.6.1 startup crash**: the shipped /Applications/FrankenTerm.app v0.6.1 crashes instantly on
  launch (operator restored v0.5.0 to work). v0.7.0 GUI MUST launch cleanly — investigate + fix the crash
  before cutting the release. Crashing bundle preserved at /Applications/FrankenTerm.app.bak-0.6.1-crashing-*.

Note: `ft-perf-gate` already ships `sprt.rs`, `conformal.rs`, `regime_shift.rs` as a library; Phase 0 wires
them into a CLI driver, it does not re-derive the math. `check_bench_stats.py` already ports Mann-Whitney +
EBCI from `bench_stats.rs`.

---

## PHASE 1 — Quick wins (low-effort, high-confidence)

| # | Idea | Target file:line | Gate (default off) | Proof | Baseline beaten |
|---|---|---|---|---|---|
| Q1 | Seqlock warm-tier prefix-sum (locate O(pages)→O(log)) | `scrollback_tiers.rs:339-340,361+` | `scrollback.prefix_index` | property: indexed==linear `ScrollbackLocationHint` | per-call `warm.iter().sum()` + linear walk |
| Q2 | Group-commit widen (events/gaps) + condvar wake (kill 1ms park) | `storage.rs:8753 writer_loop`, `:8078` | `storage.group_commit_events`, `storage.writer_blocking_recv` | golden DB dump + per-cmd result order; crash-atomicity | 1ms park + per-event autocommit |
| Q3 | Linear (Z/KMP) overlap match (kill O(n²) delta) | `ingest.rs:1855-1881` | `ingest.delta_linear_overlap` | property: linear==quadratic `DeltaResult` on box/emoji fuzz | nested memchr + per-candidate slice compare |
| Q4 | Lazy capture materialization (defer JSON past dedup) | `patterns.rs:3568-3594,3722-3733` | `patterns-lazy-captures` | detection-stream golden diff=0; dhat alloc drop | eager extract_captures before dedup |
| Q5 | Teddy SIMD multi-pattern prefilter (ahead of fancy_regex) | `patterns.rs` Bloom→AC→regex | `teddy-prefilter` | byte-equiv detection stream over conformance corpus | Bloom prefilter → per-rule regex |
| Q6 | Fingerprint/cuckoo dedup + O(1) LRU | `patterns.rs:451,556,3722` | `patterns-fingerprint-dedup` | superset-suppression FP≤1e-4 / 1M keys | `HashMap<String,_>` + O(n) retain |

---

## PHASE 2 — Moonshots (bold, high-upside)

### Systems (alien-graveyard)
| # | Idea | Target | Gate | Proof |
|---|---|---|---|---|
| M1 | Branchless ANSI DFA table (build.rs-generated from `ansi_state_step`, provably equal) | `simd_scan.rs:254-258` + `term/performer.rs` | `ansi-dfa-table` | exhaustive (state,byte) table==FSM; chunk fuzz byte-equal |
| M2 | Succinct RLE cell-attr store graduation (warm/cold) | `frankenterm/cell/src/lib.rs` | `succinct_attrs` | per-col attr==AoS golden; reflow byte-identical |
| M3 | GPU instanced SoA glyph expansion (vertex-shader) | `frankenterm-gui` | `FT_MOONSHOT_INSTANCED_GLYPH_QUADS` | pixel-golden glyph/emoji/CJK/ligature corpus |
| M4 | CDC rolling-hash dedup before zstd | `scrollback_tiers.rs` warm pages | `scrollback.cdc_dedup` | round-trip byte-identity over capture corpus |
| M5 | FST/MPHF perfect-hash anchor dispatch (immutable base + overflow map) | `patterns.rs:3556-3560` | `patterns-mphf-dispatch` | MPHF route==hashmap route for every anchor |
| M6 | Persistent COW scrollback grid (hot tier, lock-free snapshots) | `frankenterm/term/` + scrollback | `persistent-scrollback` | version-isolation property; golden dumps==VecDeque |

### Math (alien-artifact-coding; each fail-closed to deterministic legacy)
| # | Idea | Target | Gate | Proof |
|---|---|---|---|---|
| M7 | JIT predictive poll cadence (renewal/hazard) | `tailer.rs:305-355`, `pane_tiers.rs` | `ingest.cadence_model=predictive` | replay traces: captures −15% @ p95 latency non-regressed; backoff golden |
| M8 | Adaptive M/G/1 group-commit (P-K/Kingman batch+linger) | `storage.rs` | `storage.group_commit=adaptive` | write-replay p99 + fsync count; durability-order golden |
| M9 | PID/MPC fleet-memory de-escalation (anti-windup) | `fleet_memory_controller.rs:737-789` | `memory.dampening=pid` | plant-ID stability cert; evicted-bytes reduced; escalation stays bang-bang |

### Stretch (only if budget remains)
Shiryaev-Roberts BOCPD fast-detector; S3-FIFO/W-TinyLFU eviction; Reed-Solomon cold-tier erasure (closes
tracked `ft-odrq7` data-loss window); min-plus end-to-end capture→storage latency certificate + monitor.

---

## Convergence log
_(tick entries appended here by the operator tend-loop)_

- 2026-06-19 — campaign opened; Phase 0 foundation laid (profile, ledgers, this doc).
- 2026-06-19 tend#1 — wave-1 dispatched to 8 panes; strong WIP (patterns.rs Q4-6, ingest.rs Q3
  +proptest, scrollback_tiers.rs Q1, storage.rs Q2, simd_scan.rs+build.rs M1 DFA, ft-perf-gate
  SPRT/conformal, cell M2 +succinct_attrs_equivalence test). BLOCKER fixed: RCH canonical-path
  mkdir failed on ubuntu-user ovh workers (ovh-a/ovh-b) — drained them (10 root workers remain);
  fail-closed discipline held (no local fallback). Re-nudged failed panes; all resumed on root
  workers. cc_3 Q3 ~done → nudged to commit. No commits landed yet.
- 2026-06-19 tend#2 — 3 commits landed + adjudicated (provisional keep, default-off, correctness
  RCH-proven): Q3 linear KMP overlap (2bebc40d0, cc_3), ft-perf-gate SPRT/conformal driver (a819e28b0,
  cod_3, durable infra), M2 succinct attrs + ft-dkfiy gap closed (2e2f729dc, cod_5). Keep-ledger updated.
  Freed panes 0.2/0.4/0.7 → dispatched M7 poll-cadence (tailer.rs), M3 GUI SoA glyph (gui), M9 PID
  fleet-memory (fleet_memory_controller.rs). Still grinding: cod_1 Q4/5/6 (patterns.rs), cod_4 M1 DFA
  (simd_scan+build.rs), cc_1 Q1 (scrollback_tiers), cc_2 Q2 (storage), cod_2 round-3 gui backfill.
  Recorded GUI v0.6.1 startup-crash as a hard Phase-3 release gate.
