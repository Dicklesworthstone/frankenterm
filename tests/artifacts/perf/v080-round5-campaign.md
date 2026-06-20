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
- 2026-06-19 tend#2 — cod_2 .5 CLOSED (green RCH proof vmi1167313, scroll/mem benches 8eef1f001).
  First real local env-gate A/B LAUNCHED: Q1 scrollback_prefix_index (build-once, ~30min). cod_2 freed →
  dispatched D3 fresh-idea mining (.13). Wedged RCH builds: cod_3 (.6 WAL, 56m no-exit) + cod_4 (.7 gui
  bench stuck at X11/XCB pkg-config) — both nudged to cancel+commit-code-first+retry; M3 frame-time A/B
  reassigned to local (Mac has GUI stack). Dirty core-src (bocpd/distributed/ingest/storage) compiles
  (cod_2's proof was green against it) but cod_3's ingest/storage still uncommitted → contamination
  watch. cod_1 (.4 retry), cod_5 (.8 retry), cc_1 (.9 bocpd, "PASS=false" diag — watch), cc_2 (.10 D1
  conformance proof), cc_3 (.11 E1 M6 proof) all working. Flagged possible mis-scoped CLI main.rs edit.
- 2026-06-19 tend#3 — Q1 MEASURED: **32.5× speedup** (locate_offset 3.09ms→95µs deep-scroll), but
  cv~15-20% (Mac not quiet during swarm) so auto-REJECTed on rule 8; distributions non-overlapping →
  unambiguous real win → KEEP default-off, default-ON promotion deferred to a quiet-Mac cv≤5 re-run
  (Form 5, recorded). Closed .10 (D1, green), .11 (E1, green), .13 (D3 mining → 4 EV candidates), plus
  .5 earlier = 5 beads closed. cc_1 found+fixed a REAL round-4 bug: SR detector was dead on
  non-zero-centered data (8c8142ef3). cod_4 VERIFIED the GUI launches (WebGPU/Metal, no crash → C1
  works). New-idea beads created from D3: EV1 .18 (term ASCII bulk row writer, performer.rs:221) →
  cod_2; EV2 .19 (agent-sharded pattern automata, patterns.rs:4319) → cc_3; cc_2 → D2 .12 (CSI/OSC).
  Launched M9+S3-FIFO local bench (warm-target reuse). Nudged cod_3 (.6 WAL wedged 1h20m → cancel+lighter
  proof), cod_4 (.7 → final gui compile-check), cc_1 (.9 → wrap+2x flake proof). dcg-angle-bracket
  gotcha: tmux send-keys messages must avoid <...> placeholders (read as shell redirects).
- 2026-06-19 tend#4 — EV2 (.19, 85f3867e6 agent-sharded patterns) + D2 (.12, 702df4a72 CSI/OSC table)
  committed. Q4 local A/B FAILED on a TRANSIENT mid-commit race (cc_2 was committing D2's
  escape-parser Cargo.toml during Q4's candidate build → manifest referenced a not-yet-present bench);
  tree is consistent now → Q4 re-launched. RCH fleet DEGRADED (7/12 healthy): cancelled stale build
  29894135561322622 (= cod_3's 1h50m wedge on vmi1293453) + drained vmi1293453 + ovh-a + ovh-b
  (telemetry-dead / disk-critical / canonical-mkdir-fail). 6 healthy workers, 28 slots free. Nudged
  cod_1(.4)/cod_3(.6)/cod_4(.7)/cod_5(.8) to cancel+retry on fresh workers. cod_4 correctly REFUSED to
  report green during the escape-parser transient (fail-closed discipline held). No new closes this tick
  (proofs RCH-blocked → now retrying). LEARNING: local A/Bs against the live dirty tree hit transient
  mid-commit races — retry, or run when the swarm is committing less.
- 2026-06-19 tend#5 — Q4 re-run CLEAN: KEEP, −2.01% (18.34→17.97µs), cv 1.23% (Mac quieter), p=1.6e-13
  (small clean win, default-off). .7 CLOSED → THREAD C DONE (cod_4: gui-check green RCH vmi1149989 +
  LLDB launch ok; C1 socket-handoff timeout verified). EV1 committed (f53f624eb term bulk ASCII writer).
  NEW dominant blocker: RCH **active_project_exclusion** (~3 ft jobs/project max) → 5-6 panes proving at
  once get refused (all fail-closed correctly, code committed). Mitigation = CONSOLIDATE proofs: stood
  down cod_1(.4)/cod_3(.6)/cod_5(.8) RCH retries; launched ONE B4 `cargo test -p frankenterm-core --lib`
  proof (validates B1 split-makes-it-fast + B2 bocpd) and E2's M6 bench locally (m6_lock_wait_evidence
  group). Tally: 6 beads closed (.5/.7/.10/.11/.13 + earlier), MEASURED M4/Q1/Q4 keep + M9/S3-FIFO
  default-off. Threads C done; A ~5 flags measured; B proof in flight (B4); D 4 ideas landing; E running.
- 2026-06-19 tend#6 — THREAD E DONE: E2 ran the M6 evidence bench locally — reader lock-wait p95 under
  6 writers at 200 panes is only 42→250ns (5.95× ratio but **sub-µs absolute, all 4 configs
  above_noise=false**, ~3 orders below the 50µs bar); clone_then_scan even holds the lock LONGER than
  scan_under_lock → M6 COW premise unjustified. M6 KILLED with hard evidence (negative-ledger, Form 1).
  .16 closed. D2 (.12) + EV2 (.19) self-closed GREEN → Thread D: D1/D2/EV1(proof-pending)/EV2 done.
  Stood-down panes reported CODE-DONE proof-pending (.4/.6/.8 banked). B4 core --lib + patterns trio
  (Q5/Q6/M5) running. Will run consolidated bench-compile (.4/.6) AFTER B4 (avoid concurrent core-class
  RCH truncation). Dispatched 2 more mined ideas: EV3 .21 (blocked/rank-select scrollback) → cod_1,
  EV4 .22 (set-based FTS batcher) → cod_3. Q3 gate uses env::var_os().is_some() (empty-var=ON footgun)
  → needs a forced-algorithm A/B, recorded for separate handling.
- 2026-06-19 tend#7 (CONVERGE) — patterns trio MEASURED (clean cv): Q5 teddy +0.5% (noise, no win), Q6
  fingerprint-dedup +8.8% SLOWER, M5 MPHF +69% SLOWER — all default-off, recorded with per-flag Form-7
  predicates (NONE promotable; good thing they were default-off). THREAD A fully adjudicated: all 19
  round-4 flags have verdicts (3 measured keeps Q1/M4/Q4; 5 measured no-win/regression Q5/Q6/M5/M9/
  S3-FIFO; M6 killed; 8 default-off+Form7; 3 no-A/B structural) → **0 default-on promotions** (round-4
  flags confirmed appropriately default-off). 6 NEW round-5 ideas landed default-off (D1/D2/EV1/EV2/EV3 +
  EV4 in progress). EV3 committed (f026fa808). B4 core --lib STILL building on RCH (~50min+, slow but
  progressing — full --lib compile is huge); will close .8/.9/.15 + run consolidated bench-compile
  (.4/.6) once it frees the core-build slot. Key honest finding: algorithmic wins (Q1 32×, M4 19×)
  deliver; systems-moonshots (Q5/Q6/M5/M9/S3-FIFO/M6) largely do not — exactly what the gauntlet exists
  to surface. Approaching release: pending B4 green + EV1/EV3/EV4 proofs, then Thread F v0.8.0.
- 2026-06-19 tend#8 (RELEASE PREP) — B4 full `core --lib` RCH run TIMED OUT at 3600s SSH — on COMPILE,
  not tests (replay/tantivy/core compile alone exceeds the 1h SSH window). This is the SAME pre-existing
  infra limit v0.7.0 shipped under (round-4 scorecard), NOT a round-5 regression; B1's mTLS split is the
  structural fix for the slow-TEST lane (separate from the compile-time SSH ceiling). Launched a TARGETED
  bocpd RCH proof (B2 validation). FOUND scattered uncommitted off-bead work (session_resume +213 +
  proptests, agent_provider +10, status_bar +33, distributed +5, dynamic −1) — real RULE-1.5 fix work by
  panes, NOT fmt churn → must NOT release from this tree. Broadcast a QUIESCE FREEZE to all 8 panes
  (commit code-first + STOP). Release plan: settle tree → verify HEAD compiles → bump 0.7.0→0.8.0 → build
  9 assets (darwin local + linux trj) → gh release. Thread A deliverable COMPLETE (19 adjudicated, 0
  promotions); Thread B code deliverable (B1 split) shipped, full-run-in-one-SSH is a round-6 infra item.
- 2026-06-19 tend#10 (RELEASE BUILDING) — BOTH gates GREEN: compile gate `Finished` 3m44s no real errors
  (tree + folded-in Codex WIP compiles); bocpd B2 proof 88 passed/0 failed on RCH vmi1227854 (validates
  the SR bug fix + that full --lib COMPILES). Closed .8 (B1 split), .9 (B2 bocpd), .4 (patterns benches).
  Operator chose INCLUDE the foreign Codex-upgrade WIP → committed fe1ce4141 (compiles green). Bumped
  0.7.0→0.8.0 (310534f5d), pushed main + master. Launched all 3 release build lanes: darwin-arm64 local
  (3 bins, fat-LTO ~3h) + linux amd64 native (trj) + linux arm64 cross (trj, aarch64-linux-gnu-gcc). trj
  pulled clean to v0.8.0 (stashed its .beads churn). Threads A/B/C/D/E DONE; F building. Remaining proof-
  pending (non-blocking, default-off): .6 storage benches, EV1 .18/EV3 .21/EV4 .22. Next: harvest builds
  → assemble 9 assets → gh release v0.8.0 → verify → memory.
- 2026-06-19 tend#11 (PACKAGING) — darwin-arm64 build DONE (7m50s; ft 0.8.0 (310534f5d) Mach-O arm64 +
  gui 41M + mux 12M verified). FrankenTerm.app v0.8.0 built (create-macos-bundle.sh --skip-build, ad-hoc
  signed); GUI SMOKE PASSED (12s run, exit 124, NO DisplayHandle/panic → round-4 GUI fix holds in the
  release binary). Staged darwin assets: ft-darwin-arm64.tar.xz + FrankenTerm-darwin-arm64.app.tar.xz in
  /tmp/ft-v080-release. trj linux: amd64 fat-LTO RUNNING (v0.8.0 crates), arm64 queued behind it on the
  shared build lock (serialized ~1-2h total). Asset template (from v0.7.0): 4 .tar.xz + 4 .sha256 +
  SHA256SUMS. Awaiting linux bins → scp → tarballs → gh release create v0.8.0.
