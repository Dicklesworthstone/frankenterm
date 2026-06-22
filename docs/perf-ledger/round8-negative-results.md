# Round-8 Negative-Evidence Ledger — v0.10.1 (Convergence / Consolidation)

> The Alien Optimization Gauntlet, round 8. **Load-bearing:** every round-8 optimization that is
> *rejected*, *measured-no-win*, *carried-over*, or *deferred* gets an entry here closed with exactly one
> of the 8 grep-able **retry-condition predicate** forms. Negative evidence is a *win* — it hardens the moat.
>
> The 10 keep-gate rules, the 8 retry forms, the forbidden anti-vocabulary, and the rejected-entry template
> are defined once in [`round4-negative-results.md`](round4-negative-results.md) — they carry over unchanged.
> Kept/shipped → [`round8-keep-ledger.md`](round8-keep-ledger.md). Campaign record →
> [`../../tests/artifacts/perf/v101-round8-campaign.md`](../../tests/artifacts/perf/v101-round8-campaign.md).

## PRE-REJECTED / already-resolved (rounds 4–7 — do NOT re-propose without NEW evidence)

Grep `round{4,5,6,7}-negative-results.md` before any pattern touches these. After four rounds the per-op
CPU micro-space is exhausted; assume any speculative new micro-idea LOSES until profiled + live-proven.
- Custom replacements of stdlib HashMap/Vec (M5 MPHF, Q6 fingerprint, Q5 Teddy) — lost at real size.
- Prefilters/caches whose overhead ≈ savings.
- Controller/policy swaps whose "win" is a quality metric, not wall-clock (M9 PID, S3-FIFO conditional).
- COW/snapshot to dodge a lock (M6 — sub-µs contention).
- Serial replacements of vectorized/SWAR code (M1 ANSI-DFA).
- GUI vertex-bandwidth (M3 — Apple Metal is readback-bound).
- Built-but-unwired engines as targets (scan_pipeline dead; distributed `DistributedHttpClient`; web
  publisher-less `/stream/events`).
- Re-attempting something already shipped/optimal (round-7 redactor RegexSet trap).

---

## Entries

### 2026-06-22 | ft-ui1xn / ft-8cpho | quick_reject Bloom prefilter vs Aho-Corasick-direct — CARRYOVER (promising, profile-gated)

**Status:** measured-promising-but-gate-unmet — **NOT refuted, NOT promoted.** Carried to round-9.
`quick_reject` stays **default-on** in production.

**Gate:** bench-only `PatternEngine::set_quick_reject_enabled(false)` arm (`ac_direct`); no production
default changed.

**Profile attribution:** UNMET. The frame is `PatternEngine::detect_with_context` (`patterns.rs`, driven
per segment from `runtime.rs:3748`), where `quick_reject` runs the Bloom prefilter before the exact
Aho-Corasick matcher. Round-7 B0′ did **not** isolate `detect_with_context`'s self-time at ≥0.5% (it
scored the dead `scan_pipeline`, `bocpd.observe_text_chunk`, and `redactor.redact`). Establishing that
attribution is `ft-zhj63`.

**Measurement (focused A/B, now unblocked — Form-8 dep `round7_fts_promote.rs` is committed):**
RCH-remote `vmi1227854 (1485.1s)`, `cargo bench -p frankenterm-core --bench pattern_detection
quick_reject_vs_ac_direct`, no-match-dominant text. Median `quick_reject_on` vs `ac_direct`:
- 1KB: 2.87µs vs **2.05µs** (ac_direct ~29% faster, non-overlapping CIs)
- 4KB: 10.39µs vs **9.12µs** (ac_direct ~12% faster)
- 16KB: 42.0µs vs **31.99µs** (ac_direct ~24% faster, non-overlapping CIs)
- 64KB: **178.03µs** vs 182.58µs (tied; quick_reject slightly ahead, overlapping CIs)

So on synthetic no-match text, **`ac_direct` (Bloom disabled) is consistently faster at 1–16KB** and only
ties at the 64KB segment cap — a real signal that the Bloom prefilter is net-overhead when the exact AC
scan is itself cheap on no-match input. This is a **legitimate carryover candidate**, not a refutation.

**Why not promoted now:** (1) profile-liveness gate unmet (`detect_with_context` self-time not yet
attributed ≥0.5% — `ft-zhj63`); (2) the A/B is synthetic no-match text on a noisy remote (wide CIs, no
cv≤5%), and does **not** cover the match-present mixed workload where AC must run anyway and the seam's
effect inverts; (3) the realistic per-segment **size distribution** and **match rate** are unmeasured —
the win is size-dependent and neutralizes at the 64KB cap.

**A/B verdict:** no promotion. `quick_reject` stays default-on. Disabling it is unproven on a realistic
workload and the frame's hotness is unestablished.

**Retry-condition predicate (Form 8 + Form 1):** Blocked until `ft-zhj63` attributes ≥0.5% realistic
self-time to `detect_with_context`; THEN re-run a release-perf A/B on **real captured segments**
(realistic size distribution + non-trivial match rate, cv≤5%) and promote `ac_direct` only if it beats
`quick_reject_on` across that distribution without regressing the match-present case.

**Rollback:** n/a (no production default changed).

**Sibling references:** ft-ui1xn, ft-8cpho, ft-zhj63.

### 2026-06-22 | ft-8cpho / ft-z91oa | scan_pipeline deletion — DEFERRED to round-9 (operator)

**Status:** deferred (operator decision, this round). `scan_pipeline` is confirmed functionally dead
(0 non-test/non-bench production callers; round-6 grep proof `df794dca3`), but deletion cascades to
~16-20 files **including the round6/round7 profile harnesses that use `scan_pipeline.process` as their
denominator** (those need rewiring to a live frame). In a convergence release the operator chose to keep
v0.10.1 lean and defer the deletion + denominator-rewiring + its fresh RCH proof. The README reality-gap
(which falsely documented the dead `trigger_data_buffer` as the production cross-chunk engine) **was fixed
this round** (`ft-z91oa`, commit `f1bc1c975`), so the docs no longer mislead.

**Retry-condition predicate (Form 2):** reconsider inside a round-9 hygiene pass that (a) obtains explicit
deletion permission, (b) rewires the round6/round7 profile-harness denominators to `detect_with_context`
(or drops the denominator-only rows), and (c) re-proves the full 16-20-file cascade RCH-remote.

**Rollback:** n/a (nothing deleted).

### 2026-06-22 | round-8 release | Windows (x86_64-pc-windows-msvc) build — RESOLVED / SHIPPED

**Status:** RESOLVED — `ft.exe` for `x86_64-pc-windows-msvc` is **shipped in v0.10.1** (built from commit
`8bdf23979`). The earlier "hard wall" was misdiagnosed: the reported `libsqlite3-sys 0.38.1` /
`cfg_select` error was NOT a repo problem — `rust-toolchain.toml` pins `channel = "nightly"` (unversioned)
and the Windows host merely had a STALE `nightly-2026-02-13`; Linux/macOS already built on the June
`1.98.0-nightly` (which has `cfg_select`). `rustup update nightly` on the host dissolved it entirely.

**Fixes landed (all platform-isolated; the green Linux check `[RCH] hz2 1142.1s` confirmed Linux/macOS
unaffected):**
1. `frankenterm/pty/src/win/mod.rs`: `From<filedescriptor::Error> for HandleCloneError` (via
   `io::Error::other`) — cfg(windows)-only file (`91f8483d7`).
2. `crates/frankenterm/Cargo.toml`: `[target.'cfg(windows)'.dependencies] openssl-sys = {features=["vendored"]}`
   — vendors OpenSSL via Strawberry Perl on Windows; inactive on Linux/macOS (`91f8483d7`).
3. `runtime_async::process`: un-gate `send_unix_signal_to_pid` / `_process_group` from `cfg(unix)` — they
   already delegate to the cross-platform `send_signal_to_pid` whose `PlatformProcessControl` impl has a
   Windows `taskkill` backend (`91f8483d7`).
4. `lib.rs`: `fd_budget` + `ipc` modules `cfg(unix)`→`cfg(any(unix,windows))` (both carry Windows impls);
   `runtime_async`: gate `pub mod unix` to `cfg(unix)`; `config.rs`: Windows-gated `dirs::data_dir()` shim;
   `vendored.rs`: `not(unix)` `PaneDelta` + `mux_pool::MuxPoolStats` shims; `main.rs`: 2 type annotations
   the Windows-excluded `cfg(unix)` branches leave un-inferable (`8bdf23979`).

**Caveats (freshly ported):** the GUI app stays macOS-only; some Unix-socket IPC paths are no-ops on
Windows; full Windows `cargo test` parity + the `ft-51fde` Unix-coupling backlog remain future work, but
the `ft` CLI builds and smoke-tests (`--version`/`--help`/`doctor`).

**Rollback:** the Windows asset is additive; the fixes are cfg-gated so reverting them does not affect
Linux/macOS.

---

_(Further round-8 measured-no-win / reject / carryover entries land below, one per the rejected-entry
template, each closed with exactly one retry-condition form.)_
