# Round-9 Keep / Promotion Ledger — v0.10.2 (Targeted-Finish / Convergence)

> The Alien Optimization Gauntlet, round 9. Round-8 declared the campaign "substantially converged" with
> one live carryover (`ft-ui1xn`) and a parked lever (`ft-yjihu.1` WAL). Round-9 is the **targeted-finish**
> round (operator-chosen): resolve the carryover, promote the parked lever, harvest the already-landed
> hot-path beads, and declare the optimization campaign **fully converged**. Two clean promotions landed —
> both are *removals/parked-levers*, exactly the round-8 nuance ("the next optimization may be REMOVING an
> existing one"). No speculative new mining.
>
> Discipline + 10 keep-gate rules + 8 retry forms: [`round4-negative-results.md`](round4-negative-results.md).
> Rejects/carryovers → [`round9-negative-results.md`](round9-negative-results.md). Campaign record →
> [`../../tests/artifacts/perf/v102-round9-campaign.md`](../../tests/artifacts/perf/v102-round9-campaign.md).

**Bench host:** local Apple-Silicon Mac + deterministic harnesses (operator choice). Byte-equivalence /
correctness proofs RCH-remote / fail-closed.

---

## Round-9 promotions → default flip

### 2026-06-23 | ft-ui1xn (+ ft-zhj63) | Remove the net-negative `quick_reject` Bloom prefilter — FLAGSHIP

**Status:** PROMOTED. Production default `PatternsConfig::quick_reject_enabled` flipped `true → false`
(commit `9137b11ab`). Kept as a per-config opt-in lever. Byte-equivalent.

**The pattern:** REMOVE an existing prefilter whose overhead exceeds its savings — the round-8 nuance, and
the moat's PRE-REJECTED "prefilters/caches whose overhead ≈ savings" class applied to an *existing* one.

**Profile-first (ft-zhj63 B0-correction):** round-6/7 B0 ranked the dead `scan_pipeline.process` as the #1
per-delta frame (72%/55%) — a phantom (0 production callers). The real per-capture-delta production frame is
`patterns::detect_with_context` (`patterns.rs:4436`, driven per pane segment from `runtime.rs:3748`). The new
`tests/round9_profile_realistic_workloads.rs` puts the LIVE frame in that slot:

| Frame | realistic self-time share | gate (≥0.5%) |
|---|---:|---|
| `patterns.detect_with_context` (CORRECTED #1) | **63.82%** | PASS |
| `bocpd.observe_text_chunk` | 16.69% | PASS |
| `redactor.redact` | 15.53% | PASS |
| `storage.wal_recovery_dirty` | 3.47% | PASS |
| `events.event_bus_publish` | 0.45% | below |
| `storage.wal_recovery_clean` | 0.04% | below |

Within `detect_with_context`, the `quick_reject` Bloom prefilter is **22.76% of total fleet detection
self-time** (it runs ~15 SipHash window-hashes/byte + 32 memchr sweeps on every no-match delta to avoid one
exact Aho-Corasick pass that is already built and does zero hashing).

**Liveness:** `detect_with_context` is the production per-delta entry (`runtime.rs:3748`), 192 captures/s ×
fleet. Verified non-test caller.

**A/B (realistic, cv≤5%, Mac — the operator bench host):** a count-weighted 256-delta corpus (70% small /
22% medium / 5% large / 3% match-present) through the **production `detect_with_context` entry**:

| | median ns/call | cv |
|---|---:|---:|
| `quick_reject_on` (old default) | 9376 / 8-run var | 2.06% |
| `ac_direct` (Bloom off, new default) | 6032 | 2.01% |

→ **ac_direct +35.67% (run 1) / +43.49% (run 2)** faster, cv≤5% both arms, non-overlapping. No regime
regresses — ac_direct does strictly less work (worst case ties at the 64KB cap). The match-present case is
covered (6/256 segments) and does not invert the result.

**Behavior-preservation / byte-equivalence:** a Bloom filter has no false negatives, so disabling the
prefilter only runs the exact matcher on more inputs → identical detection output.
`quick_reject_disabled_is_byte_equivalent` proves serde-identical `Vec<Detection>` across all 256 deltas
(6 match-present, 7 detections). **RCH-remote `[RCH] hz2 (1183.9s)` fail-closed
(`RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 CARGO_NET_GIT_FETCH_WITH_CLI=true`): 2 passed / 0 failed.**

**Bench-host note:** the harness's own auto-VERDICT printed `promote=false` on the RCH **remote** because
that run's cv exceeded 5% (noisy shared host) — this is precisely the artifact the operator's Mac-bench-host
choice avoids. The remote independently confirmed the *direction* (+70.97%). The promotion rests on the
quiet-Mac cv≤5% runs (timing) + the host-independent RCH-remote byte-equivalence (correctness).

**Blast radius (surgical):** production builds via `from_config_with_root` → `PatternsConfig::default()`, so
the single production default is `config.rs:2192`. `PatternEngine::new()` has zero production callers (all
`cfg(test)`); it stays default-on so the 60 in-crate prefilter-algorithm tests keep exercising the Bloom
path. Only two default-value assertions updated (`config.rs:9314`, `proptest_config.rs:1477`); proptest_config
102 passed, no other regressions.

**Retry/rollback:** opt back in via `[patterns] quick_reject_enabled = true`; `git revert 9137b11ab`.

**Sibling references:** ft-ui1xn, ft-zhj63, ft-8cpho, ft-p4vzl.

---

### 2026-06-23 | ft-yjihu.1 | Promote WAL skip-startup-checkpoint to default-ON

**Status:** PROMOTED. `FT_MOONSHOT_SKIP_STARTUP_WAL_CHECKPOINT` flipped default `false → true` (now an
opt-OUT; commit `dd0043b79`). Round-8 landed it default-off pending exactly this proof.

**Profile-first:** round-7 B0′ — `storage.wal_recovery_dirty` = 3.528% startup self-time, mean ~8 ms on a
4.7 MB dirty WAL; the only round-7 new CPU frame clearing both the ≥0.5% gate and production-liveness.

**Lever:** before the startup `PRAGMA wal_checkpoint(PASSIVE)`, estimate WAL frames from the 32-byte header +
file size (over-counting = safe). If no rollback journal, `quick_check` passes, and estimate ≤
`WAL_RECOVERY_THRESHOLD` (10 000), skip the checkpoint (SQLite replays the WAL on open; checkpointing is
maintenance). Any ambiguity falls back to the checkpoint path; corruption fail-closed preserved.

**Deterministic startup-time A/B (Mac, `tests/round9_wal_startup_time.rs`):**

| small dirty WAL (8000 rows) | median | cv |
|---|---:|---:|
| skip OFF (legacy checkpoint) | 2789 µs | 6.58% |
| skip ON (new default) | 724 µs | 5.31% |

→ **+74.06% faster, ~2.07 ms saved**, material (≥30% AND ≥1 ms), cv≤10% both arms. **No regression:**
clean-start branch identical (`Checkpointed == Checkpointed`); over-threshold branch identical
(`Truncated == Truncated`) — skip-on correctly falls back. Durability was already proven by the round-8
oracle (a fresh reader replays the WAL, zero row loss).

**Proof:** `round8_wal_recovery` **8 passed / 0 failed** (the `t5` child re-exec test renamed to
`t5_child_public_path_default_on_and_opt_out`, verifying unset→skipped, =1→skipped, =0→checkpointed through
the real env→gate→decision wiring) + `round9_wal_startup_time` passed. **RCH-remote `[RCH] hz2 (1143.4s)`
fail-closed: 8 + 1 passed / 0 failed** (the remote's slower I/O showed +62.27% / 8.05 ms saved; its
`cv_ok=false` is the same noisy-host artifact — the no-regression branch-equivalence assertions are the hard
gate and passed on both hosts).

**Naming wart (recorded, not fixed):** the gate keeps its `FT_MOONSHOT_*` name though it is now default-on;
renaming would churn the child-process env-wiring test for no behavior gain.

**Retry/rollback:** opt out via `FT_MOONSHOT_SKIP_STARTUP_WAL_CHECKPOINT=0`; `git revert dd0043b79`.

---

## Harvest (already-landed hot-path beads — closed this round on green proofs)

| Bead | Fix (in main) | Proof | Disposition |
|---|---|---|---|
| ft-4lq0i | `redact()` `SECRET_PATTERN_SET` RegexSet fast-path (`4b500746a`) — clean content: 1 scan + 1 alloc vs 32 passes + 33 allocs; byte-identical | `redactor::tests` green (97-passed core lib run); round-7 ledger certifies shipped/optimal | **CLOSED** |
| ft-gkh4p | bounded straddle-detection window in `redact_segment_for_persistence` — chunk scanned once (redact) + bounded boundary window (detect), not twice in full | 11 `gkh4p_`/`e8hd7_` straddle tests green (incl. `scans_chunk_exactly_once`, `straddle_scan_never_rereads_past_span_budget`) | **CLOSED** |
| ft-uyt88 | mux reader BufReader batching (`ec61880ef`, ~30 syscalls/PDU → 1) | **regression test HANGS on this macOS host** | **KEPT OPEN — see negative ledger** |

---

_(Round-9 carryovers / kept-open / false-open → [`round9-negative-results.md`](round9-negative-results.md).)_
