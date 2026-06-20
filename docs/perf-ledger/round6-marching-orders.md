# Round-6 Alien Optimization Gauntlet — Swarm Marching Orders

Epic: **ft-round6-gauntlet-\*** (+ carryover proofs under **ft-round5-gauntlet-lw0s7**). Discipline:
`docs/perf-ledger/round4-negative-results.md` (10 keep-gate rules, 8 retry forms) + AGENTS.md. Goal:
quantify the 6 round-5 ideas, promote Q1/adaptive-M4, mine NEW profiled algorithmic/bandwidth wins, ship
v0.9.0. Ledgers: `round6-keep-ledger.md` + `round6-negative-results.md`.

## NON-NEGOTIABLE RULES (every pane)

1. **Claim your bead first:** `br update --status in_progress <bead> --assignee <your-pane-name>`.
2. **File ownership is EXCLUSIVE.** Edit ONLY the files in your section. Keep the tree COMPILING at all
   times — a mid-edit non-compiling `frankenterm-core` file fails EVERY sibling's RCH proof (ft-ch3nm).
3. **Commit code-first, FAST.** `git add <your files> && git commit` as a SINGLE invocation (siblings
   sweep staged files otherwise). Run `ubs <changed-files>` first.
4. **Proofs RCH-remote, fail-closed.** Never count `[RCH] local`. Template:
   ```
   RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 CARGO_NET_GIT_FETCH_WITH_CLI=true \
     rch --no-self-healing exec -- env CARGO_TARGET_DIR=/tmp/ft-<bead>-<purpose> \
     cargo <check|test|bench --no-run> -p <pkg> <filters>
   ```
   ovh-a/ovh-b drained; vmi*/hz1/hz2 work. `rch cache clean` on ENOSPC. TARGETED filters — never whole
   `core --lib` in one job (times out). **A dirty antigravity/session_resume tree is in flight — isolate
   your proofs from it (prove crates that don't pull the dirty core files).**
5. **Every optimization stays DEFAULT-OFF** behind its feature/env/config gate. Bench work never flips a
   default — the orchestrator promotes winners after the local A/B.
6. **Profile-first for NEW ideas:** no new code without ≥0.5% self-time attribution on a realistic
   workload. **Grep round4+round5-negative-results.md before proposing ANY idea** — do not re-attempt a
   pre-rejected class (custom-vs-stdlib structures, overhead≈savings prefilters, quality-not-speed
   controller swaps, sub-µs micro-opts) unless its exact retry predicate is now satisfied.
7. **Report back:** `DONE <bead>: <sha> — <one-line proof result>` or `BLOCKED <bead>: <reason>` (commit
   code-first, then stop hammering infra; orchestrator reroutes).

## A/B bench contract

Orchestrator runs the local A/B on the Mac (large-effect ≥2× only). Your bench must express two arms via
`--gate feature:<NAME>` or `--gate env:VAR=ON[/OFF]`; the benched fn must honor the gate and BOTH arms
must be reachable from the public API the bench calls. If a config flag isn't bench-reachable, expose a
minimal `#[doc(hidden)] pub` setter — NOT a public config field (sibling full-literal ctors break).

---

## PANE ASSIGNMENTS (round-6)

### cod_1 (pane 0) — scrollback_tiers.rs owner — A1/A2 + EV3
Own: `crates/frankenterm-core/src/scrollback_tiers.rs` + `benches/scrollback_prefix_index.rs` + a NEW
`benches/scrollback_cdc_adaptive.rs`.
- **EV3 (.21)** finish RCH proof (blocked-page index, already committed f026fa808).
- **A1 Q1 promotion:** add a SHALLOW-scrollback non-regression bench arm (hot-tier-only, index falls back
  to linear via seqlock consistency check) proving env-ON ≈ env-OFF in the common case. (Orchestrator runs
  the deep-scroll ≥2× arm + this one.)
- **A2 M4 adaptive:** add a cheap redundancy probe (estimate dedup ratio on first N pages) that
  auto-enables CDC only above a threshold; bench that 19× holds when on AND the probe adds no measurable
  cost when redundancy is low. Default-off gate unchanged.

### cod_2 (pane 1) — term/performer.rs owner — A3 EV1 + render-term mining
Own: `frankenterm/term/src/terminalstate/performer.rs` + term benches.
- **EV1 (.18)** finish RCH proof (bulk-ASCII row writer, committed f53f624eb).
- **A3:** author a term-throughput A/B bench for EV1 (pure-ASCII dense-row workload) and for D1/D2 paths.
- **Mining:** profile-gated term-path bandwidth ideas only (grep ledgers first).

### cod_3 (pane 2) — storage.rs FTS owner — A4 EV4 + FTS mining
Own: `crates/frankenterm-core/src/storage.rs` (FTS paths) + `storage/fts_sync_tests.rs` + a NEW FTS bench.
- **EV4 (.22)** finish RCH proof (set-based FTS INSERT…SELECT batcher, committed 8a0d7be39).
- **A4:** author a deferred-FTS-sync throughput bench (per-segment insert vs set-based batch) for the A/B.
- **Mining:** profile-gated FTS/storage bandwidth ideas only.

### cod_4 (pane 3) — frankenterm-gui owner — B3 M3 SoA + render-bandwidth mining
Own: `crates/frankenterm-gui/` (benches + render).
- **B3:** turn the SoA glyph-quad bench into a real GPU frame-time A/B under glyph-dense frames
  (`FT_MOONSHOT_INSTANCED_GLYPH_QUADS`); orchestrator runs it locally (Mac has the GUI stack).
- **Mining:** profile-gated GPU vertex-bandwidth ideas only.

### cod_5 (pane 4) — infra owner — C2 lane splits + B4 push-mode audit
Own: `crates/frankenterm-core/Cargo.toml` (`[[test]]`/`[[bench]]` entries) + NEW integration test targets
+ `runtime.rs`/`native_events.rs` (read-mostly for the audit).
- **C2 (.15):** split more slow core `--lib` lanes into `[[test]]` integration targets (round-5 split only
  mTLS) so the suite compiles within the 3600s RCH SSH limit. Identify the next-slowest compile units.
- **B4:** audit whether any residual O(#panes)-per-tick poll path survives at high pane count despite
  native push mode; file a bead with profiled evidence if so.

### cc_1 (pane 5) — patterns.rs owner — B1 incremental cross-chunk Aho-Corasick (FLAGSHIP)
Own: `crates/frankenterm-core/src/patterns.rs` + `scan_pipeline.rs` + patterns benches.
- **B1:** profile the AC re-scan-at-flush double-work (README 1828-1830). If hot (≥0.5%), carry streaming
  AC automaton state across chunk boundaries instead of re-scanning `trigger_data_buffer`. Default-OFF
  behind a new `FT_MOONSHOT_*` gate; byte-equiv detection-stream proof across chunk splits. This is the
  one genuinely-new algorithmic lead — the highest-EV new idea.

### cc_2 (pane 6) — ingest.rs owner — B2 Q3 KMP forced A/B + D1/D2 A/B
Own: `crates/frankenterm-core/src/ingest.rs` + delta_extraction bench.
- **B2:** fix the Q3 measurement footgun — add a `#[doc(hidden)] pub` forced-algorithm API so the A/B can
  express OFF vs ON (current `env::var_os().is_some()` makes empty-but-set = ON). Run the forced A/B on
  adversarial repeated-first-byte input where the O(n²) overlap dominates; promote KMP if it wins ≥2×.
- **D1/D2:** co-own the A/B benches for parser printable-run batching + CSI/OSC dispatch table with cod_2.

### cc_3 (pane 7) — profiling + quality-harness owner — B0 + A5
Own: NEW profiling harness + `benches/` quality-metric files (no contamination of sibling-owned src).
- **B0 (the gate):** build/drive a realistic-workload profiling harness (high-pane capture, ANSI-dense
  render, deep-scroll, search-heavy) and capture flamegraphs ranking hot frames by self-time. Feed the
  orchestrator a scored target list — NO new idea gets a bead without ≥0.5% attribution.
- **A5 (.20):** build the quality-metric bench harness (evicted-bytes / reclaim-oscillation / hit-rate)
  so M9/S3-FIFO/Q4/M2 become adjudicable on a future quiet host.

## Orchestrator-owned (do NOT start unless reassigned)
- Local A/B runs (all flags), Q1/M4 promotion decision, ledger adjudication, B5+ open-idea mining seeds,
  C1 arm64 cross-build (trj), C3 v0.9.0 release cut.
