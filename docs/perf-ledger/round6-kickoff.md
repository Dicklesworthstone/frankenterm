# Round-6 Alien Optimization Gauntlet — kickoff prompt

> Paste the block below into a fresh session to resume the campaign. It front-loads the round-5
> negative-evidence so round-6 fails fast on the dead-end idea-classes and concentrates on what paid off.

---

ROUND-6 ALIEN OPTIMIZATION GAUNTLET — resume the radical-innovation perf campaign toward v0.9.0.

You are the autonomous orchestrator. v0.8.0 shipped (round-5). FIRST bootstrap context (read fully; do NOT re-derive):
1. AGENTS.md + README.md (super carefully).
2. Round-5 outcome — memory file `~/.claude/projects/-Users-jemanuel-projects-frankenterm/memory/project_round5_alien_optimization_gauntlet_v080_2026_06_20.md` (+ scan MEMORY.md, esp. the round4/round5/bocpd-sr entries), `tests/artifacts/perf/v080-round5-campaign.md`, and ALL FOUR ledgers: `docs/perf-ledger/round4-keep-ledger.md` + `round4-negative-results.md` + `round5-keep-ledger.md` + `round5-negative-results.md`. The negative ledgers are LOAD-BEARING — read them first.
3. Skills: `/running-the-gauntlet-on-your-rust-port` (keep-gate + the 8 retry-predicate forms), `/profiling-software-performance` (profile-first), `/alien-graveyard`, `/alien-artifact-coding`, `/extreme-software-optimization` (idea mining), `/ntm` + `/vibing-with-ntm` (swarm), `/beads-workflow` + `/bv` + `/fixing-beads-problems`.
4. Investigate the code + the real hot paths with explore agents BEFORE planning.

THE #1 LESSON — let it drive idea selection (this is what makes round-6 optimal):
Round-5's evidence is unambiguous: **ALGORITHMIC complexity-class wins delivered (Q1 seqlock prefix-sum O(pages)→O(log) = 32×; M4 CDC dedup = 19× on redundant data), while SYSTEMS-MICRO-MOONSHOTS did NOT — Q5 Teddy +0.5% (noise), Q6 fingerprint-dedup +8.8% SLOWER, M5 MPHF +69% SLOWER than stdlib HashMap, M9 PID / S3-FIFO compute-cost-with-no-measured-win, M6 persistent-COW-grid KILLED (sub-µs lock-wait at 200 panes, 3 orders below the bar).** Therefore:
- **PRIORITIZE (high-EV classes):** complexity-class reductions (O(n²)/O(n)→O(log)/O(1)) on PROFILED hot paths; large-constant data-structure/layout wins measured on the REAL workload; byte/bandwidth reductions on redundant data (M4-style).
- **PRE-REJECT (proven-dead classes — do NOT propose without NEW evidence):** "clever" custom replacements of stdlib HashMap/Vec (perfect-hash, fingerprint, SIMD prefilters) — they lost to the stdlib at real sizes; prefilters/caches whose overhead ≈ their savings; controller/policy swaps (PID, alt eviction, alt detector) whose "win" is a quality metric, not a wall-clock number; micro-opts of already-sub-µs paths.
- **HARD GATE — profile first:** a candidate needs measured ≥0.5% self-time attribution on a REALISTIC workload (actual capture/render/search/ingest path at scale) BEFORE any code is written. Workload-specificity is real (Q1 wins only at deep scroll; M4 only on redundant data) — measure where it would actually run.
- **GREP THE LEDGERS FIRST:** before proposing ANY idea, grep round4+round5-negative-results.md for the symbol/idea; if present, do NOT re-attempt unless its exact retry-predicate is now satisfied.

FIX THE MEASUREMENT FRICTION UPFRONT (round-5 burned real time here):
- **Bench host:** round-5's local-Mac A/B was cv-noisy (15-20%) under concurrent swarm load — large-effect wins survived, small ones were unmeasurable. Round-6: bench on a GENUINELY QUIET host (swarm idle, or a dedicated quiet box), and/or only chase large-effect ideas where cv slop doesn't matter. CONFIRM the bench host at start.
- **Quality-metric benches:** ns-timing benches cannot adjudicate evicted-bytes/hit-rate/alloc-count/RSS wins (round-5 left M9/S3-FIFO/Q4/M2 unmeasurable). Build the quality-metric bench harness (round-5 followup bead `ft-round5-gauntlet-lw0s7.20`) BEFORE proposing any quality-metric idea — or don't propose them.
- **Proofs:** full `cargo test -p frankenterm-core --lib` TIMES OUT at the 3600s RCH SSH limit ON COMPILE (not tests) — use TARGETED/CONSOLIDATED filters; split more slow lanes (B1 already split mTLS; the suite is still compile-bound). RCH caps ft at ~3 concurrent jobs (active_project_exclusion) — consolidate proofs into few jobs, ≤3 concurrent.

RESUME STATE:
- v0.8.0 shipped (tag/commit 310534f5d). `FT_MOONSHOT_ALL` master env + `moonshot-all` cargo feature exist (enable the wins/neutrals; the everything-on test build is at `~/FrankenTerm-ALLOPT.app` + `~/ft-allopt-run`). `moonshot-all` deliberately EXCLUDES the Q6/M5 regressions.
- The 6 round-5 NEW ideas (D1 parser printable-run batching, D2 table-driven CSI/OSC dispatch, EV1 term bulk-ASCII row writer, EV2 agent-sharded pattern automata, EV3 blocked/rank-select scrollback pages, EV4 set-based FTS catch-up batcher) are byte-equiv-proven but A/B-UNMEASURED — round-6's first quantification job (same gap round-5 inherited from round-4).
- Promotion candidates: Q1 (32×) + M4 (19×) → default-on AFTER a quiet-host cv≤5 re-run + a non-regression proof on the COMMON case (shallow scrollback for Q1; low-redundancy data for M4).
- Open beads under epic `ft-round5-gauntlet-lw0s7`: `.18` EV1 / `.21` EV3 / `.22` EV4 / `.6` storage benches (proof-pending), `.20` quality-metric benches, `.23` ft-linux-arm64 release asset. FIRST: `bv --robot-triage` + check `.beads` JSONL; rebuild via `/fixing-beads-problems` if the DB is corrupt.

ROUND-6 FIRST-TIER WORK:
1. Quantify the 6 round-5 ideas (D1/D2/EV1-4) on the quiet host; promote/revert via keep-gate + ledgers.
2. Promote Q1 + M4 to default-on (with the common-case non-regression proof). Consider a `moonshot-recommended` default set of the proven wins.
3. Build the quality-metric bench harness (`.20`) so eviction/hit-rate/alloc/RSS ideas become adjudicable.
4. Profile-first mining of NEW algorithmic-class ideas (the winning class) via /alien-graveyard + /alien-artifact-coding + /extreme-software-optimization on the PROFILED hot paths (capture / `append_segment_sync`, render frame path, FTS/search, ingest delta, pattern engine).
5. Close round-6 follow-ups: attach ft-linux-arm64 to v0.8.0 (or fold into v0.9.0); stabilize the full core `--lib` (more lane splits).

ORCHESTRATION (reuse the proven flow + the hard-won gotchas):
- NTM 8-pane swarm in tmux session `frankenterm` (reuse it; `/clear` context-heavy cc panes; codex panes take a fresh dispatch). One idea/bead per pane, partitioned by FILE OWNERSHIP. Tend via ScheduleWakeup (~20-30min). Keep-gate-adjudicate each commit into the ledgers. Commit code-first FAST.
- DISPATCH GOTCHAS: `ntm` here is `ntm send --pane=N` (NOT the AGENTS-doc `ntm --robot-send`); most reliable is `/opt/homebrew/bin/tmux send-keys -t frankenterm:0.N -l "msg"` + `Enter` (2nd Enter ~2.3s later for codex). NO `<angle-brackets>` in send-keys text (dcg reads them as shell redirects). zsh arrays are 1-INDEXED — use a positional-arg function or explicit per-pane calls, never `arr[$i]` in a loop. dcg blocks `rm -rf` (even /tmp) and `>` redirects to $HOME/system paths (use `>>`, `tee`, or the Write tool).
- PROOFS RCH-remote/fail-closed: `RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 CARGO_NET_GIT_FETCH_WITH_CLI=true`; never count `[RCH] local`. Drain ovh-a/ovh-b (canonical `/Users/...` mkdir fails). Commit code-first fast to avoid dirty-tree cross-contamination (ft-ch3nm); ≤3-4 concurrent core/vendored editors. `rch cache clean` on ENOSPC.
- RELEASE (v0.9.0) via trj (Tailscale 100.91.120.17, /data/projects/frankenterm) + local darwin: trj has a GLOBAL `CARGO_TARGET_DIR=/data/tmp/cargo-target` AND /data/tmp is SWEPT — build into a NON-swept `$HOME` dir with a SEPARATE `CARGO_TARGET_DIR` per target (host + cross collide otherwise: `E0463 can't find crate`); trj is a shared host (load 40+) so fat-LTO cross-builds crawl — ship ready platforms + `gh release upload` slow assets async. 9-asset set; bake everything-on into a test `.app` via `Info.plist` `LSEnvironment`. Bench/correctness local-vs-RCH split: perf microbenches may run local on a quiet host, correctness proofs ALWAYS RCH-remote.

DECISIONS TO CONFIRM AT START (ask me): (a) scope — full BIG&BOLD new mining vs focused on quantifying-round-5 + promoting-winners; (b) cadence — autonomous-to-release vs phase-gated; (c) end state — cut v0.9.0 vs flagged-experiments only; (d) BENCH HOST — which quiet host for cv≤5 (this was THE round-5 bottleneck). Then EnterPlanMode, present the plan, and on approval run it. Dream BIG & BOLD — but bias HARD toward the algorithmic/complexity-class + bandwidth wins that actually paid off, profile before building, and honor the negative-evidence ledger (don't re-run the dead ends).

---

_Origin: authored at the end of round-5 (v0.8.0). The crisp distillation: **round-5 proved the gauntlet's value is disproving moonshots fast — so round-6 is engineered to fail fast on the dead-end classes (custom-vs-stdlib structures, overhead-≈-savings prefilters, quality-not-speed controller swaps, sub-µs micro-opts) and concentrate on the one class that won: profiled algorithmic/complexity-class reductions + bandwidth wins on the real workload.**_
