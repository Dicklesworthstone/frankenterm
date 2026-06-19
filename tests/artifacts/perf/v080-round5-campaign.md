# v0.8.0 Round-5 — The Alien Optimization Gauntlet (campaign record)

> Round-5 of the NTM-swarm perf campaign under the `running-the-gauntlet-on-your-rust-port` discipline.
> Two threads: (A) **quantify** the 19 round-4 default-OFF optimizations on a quiet host and
> promote/revert by the keep-gate; (B) **mine + land new bold ideas** (alien-graveyard /
> alien-artifact-coding / extreme-software-optimization). Plus hygiene (core-suite stabilization, GUI
> startup-hang harden) and a **v0.8.0 release**. Keeps → `docs/perf-ledger/round5-keep-ledger.md`;
> rejects/no-wins → `docs/perf-ledger/round5-negative-results.md`.

**Decisions (operator-confirmed 2026-06-19):** Full BIG & BOLD · Autonomous-to-end · cut v0.8.0 ·
**benchmark locally on this Mac** (round-4's shared RCH workers gave cv~30%; correctness proofs stay
RCH-remote/fail-closed).

**Epic:** `ft-round5-gauntlet-lw0s7` (children .1–.17). Swarm: tmux session `frankenterm`, 8 panes
(cod_1..5 + cc_1..3), file-owned per `docs/perf-ledger/round5-marching-orders.md`.

## Planning sweep findings (4 Explore agents, 2026-06-19)
- Of the 19 round-4 flags: **3** A/B-runnable with an existing isolating bench (Q4, M2, M4); **6** need
  config/env injection into existing benches (Q2, Q3, M1, M7, M8, Shiryaev-Roberts); **8** need a new
  bench authored (Q1, Q5, Q6, M3, M5, M9, S3-FIFO); **3** are no-A/B structural (min-plus, RS, perf-gate).
- `scripts/round4-bench-ab.sh` was RCH-hard-wired → `--local` mode added (bead .1, commit 2229e3507).
- A/B shapes are heterogeneous: some benches bake separate off/on ids (M4 cdc_dedup), some wins are
  metric≠ns (M4 dedup_ratio, M9 evicted-bytes, S3-FIFO hit-rate, SR detection-delay). Adjudicated per-flag.
- Test stabilization: ~36 real-TLS tests in `distributed.rs` are the timeout culprit; `bocpd.rs:1284`
  shiryaev_roberts flakes on a strict `sr_delay < bocpd_delay` inequality under FP-order load.
- GUI hang: `main.rs:778` → `client.rs:566` `UnixStream::connect()` is a blocking syscall w/ no timeout.
- M6: no measured contention (per-pane `Arc<Mutex<Terminal>>` serializes); E1 builds the evidence harness.

## Convergence log
_(tick entries appended by the orchestrator tend-loop)_

- 2026-06-19 — campaign opened. Beads DB rebuilt+healthy (orphaned write-lock cleared). Epic + 17
  children created, deps wired. A0 `--local` bench mode shipped (2229e3507) + dry-run validated. Swarm
  dispatched: 8 panes claimed beads .4–.11 (A1 patterns/scroll-mem benches, A2 storage/tailer/simd
  wiring, GUI M3+C1, B1 mTLS split, B2 bocpd, D1 parser batching, E1 search bench). First local bench
  (M4 codec cdc_dedup) launched on the Mac to validate the local pipeline + capture M4's dedup_ratio.
- 2026-06-19 tend#1 — strong wave-1: 7 commits landed (A1P patterns benches cb44b1c86, A2W storage/
  tailer/simd benches c177f11b1, B2 bocpd SR-stabilize+bench 9ce81ca73, E1 M6 evidence harness
  eb382bcaa, D1 parser print-batching 6f1ddc447, B1 mTLS test split b1ac293e1, + spine e6751fab8).
  M4 MEASURED locally: 19.00x dedup, KEEP default-off (metric≠ns, see keep-ledger). Local A/B pipeline
  validated end-to-end on the Mac. CONTAMINATION (ft-ch3nm) flagged by cod_1(.4)+cod_5(.8): a sibling
  untracked escape-parser test blocked their RCH proofs — but it was ALREADY committed in D1
  (6f1ddc447); the BLOCKED reports predate the commit → root cleared. Remaining dirty core-src
  (cod_3 ingest.rs/storage.rs doc-hidden bench setters, uncommitted) compiles (cod_3's own proof is
  progressing) → nudged cod_3 to commit code-first, cod_1+cod_5 to retry. cod_2 (scroll/mem benches),
  cc_1 (bocpd --lib), cc_2 (D1 proof), cc_3 (E1 M6 proof), cod_4 (gui M3+C1) working; several on slow
  remote core builds (25-29min, not RED). No beads closed yet (proofs pending — no close without green).
