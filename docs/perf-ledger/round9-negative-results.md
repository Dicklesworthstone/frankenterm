# Round-9 Negative-Evidence Ledger — v0.10.2 (Targeted-Finish / Final Convergence)

> The Alien Optimization Gauntlet, round 9. **Load-bearing:** every round-9 item that is *rejected*,
> *carried-over*, *deferred*, or *caught-as-false-open* gets an entry here, closed with exactly one of the 8
> grep-able **retry-condition predicate** forms. Negative evidence is a *win* — it hardens the moat for any
> round-10. After 5 rounds (v0.7→v0.10.2) the per-op CPU micro-space is **exhausted**; both round-9 wins were
> *removals/parked-levers*, not new mining.
>
> The 10 keep-gate rules, the 8 retry forms, the forbidden anti-vocabulary, and the rejected-entry template
> are defined once in [`round4-negative-results.md`](round4-negative-results.md) — they carry over unchanged.
> Kept/shipped → [`round9-keep-ledger.md`](round9-keep-ledger.md). Campaign record →
> [`../../tests/artifacts/perf/v102-round9-campaign.md`](../../tests/artifacts/perf/v102-round9-campaign.md).

## PRE-REJECTED / already-resolved (rounds 4–8 — do NOT re-propose without NEW evidence)

Grep `round{4,5,6,7,8}-negative-results.md` before any pattern touches these. After five rounds, assume any
speculative new micro-idea LOSES until profiled + live-proven.
- Custom replacements of stdlib HashMap/Vec (M5 MPHF, Q6 fingerprint, Q5 Teddy) — lost at real size.
- Prefilters/caches whose overhead ≈ savings — and now, the *existing* `quick_reject` Bloom prefilter
  REMOVED in round-9 for being net-negative (ft-ui1xn). The class is fully mined in both directions.
- Controller/policy swaps whose "win" is a quality metric, not wall-clock (M9 PID, S3-FIFO conditional).
- COW/snapshot to dodge a lock (M6 — sub-µs contention). Serial-vs-SWAR (M1 ANSI-DFA).
- GUI vertex-bandwidth (M3 — Apple Metal readback-bound). Table-driven CSI/OSC (D2). KMP overlap (Q3).
- Built-but-unwired engines as targets (scan_pipeline dead; distributed `DistributedHttpClient`; web
  publisher-less `/stream/events`).
- Re-attempting something already shipped/optimal (round-7 redactor RegexSet trap).

---

## Entries

### 2026-06-23 | ft-uyt88 | mux reader BufReader — FALSE-OPEN CAUGHT, regression test hangs (host artifact, NOT the change)

**Status:** kept-open (false-open caught). The bead's BufReader optimization (`ec61880ef`, ~30 syscalls/PDU →
~1 per refill) shipped in v0.10.1 having only run `cargo check`, never its own regression test. That test —
`reader_receives_delayed_handshake_reply_ft_connect_fix` — **HANGS** (12 s readiness-regression watchdog) on
this Apple-Silicon macOS host.

**Decisive experiment:** I reverted the BufReader change to the proven unbuffered reader and **the test still
hangs**; the sibling `main_thread_pane_write_round_trips_ft_connect_fix` (also a real-reader test:
`client_thread → block_on_io → asupersync reactor`) **also hangs** on the unbuffered reader. So the hang is
NOT caused by the BufReader optimization — both real-reader `_ft_connect_fix` tests hang on this macOS
multi-runtime test harness. v0.10.1 ships a working macOS GUI mux client (the production reader wakes on
socket readability), so this is a **macOS-local test-harness artifact, not a production hang.** The BufReader
change stays in main (the revert was stashed then discarded — hypothesis falsified).

**Why it matters (moat lesson):** this validates the false-open-needs-green-proof discipline — a bead that
"looks done in HEAD" with a confident "Proven-by …" note can be RED (here: never executed). Always run the
bead's own test, not just `cargo check`.

**Retry-condition predicate (Form 8):** Blocked until `reader_receives_delayed_handshake_reply_ft_connect_fix`
runs GREEN on a host where the asupersync reactor drives the multi-runtime test harness (Linux / CI), OR the
macOS test harness is fixed so the reader thread's reactor is driven. Do NOT close ft-uyt88 until that green
proof exists. The optimization itself is not implicated.

**Rollback:** n/a (BufReader stays in main; it is not the bug).

**Sibling references:** ft-uyt88; the `_ft_connect_fix` reader-test family.

### 2026-06-23 | ft-ui1xn | RCH-remote auto-VERDICT `promote=false` — noisy-host cv artifact, NOT a refutation

**Status:** recorded (not a reject). The round-9 byte-equivalence RCH-remote run also executed the timing A/B
and its harness printed `cv_ok=false → promote_ac_direct=false`, because the shared remote worker (`hz2`)
could not hold cv≤5% (the round-8 lesson). The *direction* was confirmed there (+70.97%); the cv≤5% gate was
cleared on the quiet Mac (the operator bench host). The promotion is sound; this entry exists so a future
reader who greps the remote log does not mistake the cv artifact for a refutation.

**Retry-condition predicate (Form 7):** retry any timing re-measurement only on a host that can hold cv≤5%
(the quiet Mac when the swarm is idle, or a dedicated quiet host) — never adjudicate the ≤5% gate on a shared
RCH remote.

### 2026-06-23 | scan_pipeline deletion — DONE (operator granted permission)

**Status:** DONE (commit `6f8089935`). The operator granted express written deletion permission this round.
`scan_pipeline` (ScanPipeline / ChunkedPipelineState / ScanPipelineConfig) was confirmed functionally dead
(0 non-test/non-bench production callers; round-6 grep `df794dca3`, re-verified round-9 via ft-zhj63 — the
real per-delta frame is `patterns::detect_with_context`). **Deleted 4260 lines**: `src/scan_pipeline.rs`
(1868) + 6 scan_pipeline-only test/bench files. The round-6/round-7 profile-harness denominators were
**rewired** from the dead `scan_pipeline.process` to the live `detect_with_context`. `pattern_trigger` /
`TriggerScanner` was **kept** (separate module; now test-only-referenced but harmless) to bound the blast
radius. `cargo check -p frankenterm-core --all-targets` GREEN locally; round-6/7/9 harnesses +
`proptest_patterns_metamorphic` + `fuzz_corpus_replay` all pass; RCH-remote cascade compile proof
(`cargo check -p frankenterm-core --all-targets`) on `6f8089935`: **GREEN, `[RCH] hz2 (1325.1s)`
fail-closed** — the deletion compiles cleanly across lib + all tests + all benches on a clean worker.

**Retry-condition predicate:** not applicable — the gain is structural (dead-code removal), not numerical.

**Rollback:** `git revert 6f8089935` (additive-safe — it only removes dead code + rewires test harnesses).

### 2026-06-23 | speculative new-axis mining — NOT attempted (operator: targeted-finish)

**Status:** not attempted (scope). The operator chose (a) targeted-finish, recommending against (c) continued
speculative mining. After five rounds the per-op CPU micro-space is exhausted; the round-9 wins were a removal
(ft-ui1xn) and a parked-lever promotion (ft-yjihu.1), not new mining. Ready speculative seeds (`ft-1dlpt`
term/VTE parser, `ft-p4vzl.12` alien-graveyard) remain OPEN and untouched.

**Retry-condition predicate (Form 1):** mine a new axis only if a fresh B0 profile on a genuinely-unprofiled
axis (memory/RSS beyond CDC, startup beyond WAL, IPC/EventBus if it ever clears 0.5%) attributes a
clearly-above-noise share to a LIVE production frame — and the candidate is not in the PRE-REJECTED list.

---

## Convergence declaration

The optimization campaign is **fully converged.** Five rounds (v0.7→v0.10.2) promoted default-on wins across
every genuinely-hot production path (Q1 prefix-sum, EV4 set-based FTS, the dense-ASCII term-render stack,
adaptive-M4 CDC) and built a load-bearing negative-evidence moat. Round-9 closed the last two live threads —
removed the net-negative `quick_reject` prefilter (ft-ui1xn, the single largest detection-path win, 22.76% of
fleet self-time) and promoted the WAL skip-checkpoint lever (ft-yjihu.1, +74% dirty-WAL startup) — and caught
one false-open (ft-uyt88). The well is dry. Future effort should pivot to product work (the Windows port
`ft-azsnz`, the mlua P1 `ft-47z57`, or the ft-uyt88 Linux/CI verification), not further per-op micro-mining.
