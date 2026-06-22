# Round-8 Keep / Promotion Ledger — v0.10.1 (Convergence / Consolidation)

> The Alien Optimization Gauntlet, round 8. After four rounds (v0.7→v0.10) the per-op CPU
> micro-space is **exhausted**: the round-7 B0′ profile found exactly one new CPU frame clearing
> both the ≥0.5% gate and production-liveness. Round-8 is therefore a **convergence round**, not a
> mining round — ship the certified carryover, implement the one profiled lever, do hygiene, and
> declare the optimization campaign substantially converged.
>
> Discipline + 10 keep-gate rules + 8 retry forms: [`round4-negative-results.md`](round4-negative-results.md).
> Rejects/no-wins/carryovers → [`round8-negative-results.md`](round8-negative-results.md). Campaign
> record → [`../../tests/artifacts/perf/v101-round8-campaign.md`](../../tests/artifacts/perf/v101-round8-campaign.md).

**Bench host:** local Apple-Silicon Mac + deterministic harnesses (operator choice). Correctness proofs
RCH-remote / fail-closed.

---

## Shipped in v0.10.1 (tag `v0.10.1`, commit `7fb968b17`)

### adaptive-M4 CDC scrollback dedup — SHIPPED (was certified-but-unshipped after round-7)

**Status:** certified in round-7 (`557982cb7`), default-on, but landed ~12 min **after** the v0.10.0 tag
so it was unshipped. Round-8 ships it. **−80.14% fleet RSS** on redundant terminal redraws
(27.87 MB → 5.53 MB @ 200 panes; probe engaged 200/200, deduped to 13 chunks); low-redundancy fleets
**+0.00% TIE** (probe declines 200/200); always-on CDC would regress +11.05% — the adaptive probe is
exactly what avoids that. Byte-equivalent warm-page decode (`proptest_scrollback_cdc_dedup`, 4 passed).
Full proof card: [`round7-keep-ledger.md`](round7-keep-ledger.md) (ft-ykde4). **Round-8 action was purely
to ship it** — no code change.

---

## Round-8 new lever

### 2026-06-22 | ft-yjihu.1 | WAL skip-checkpoint for small healthy WALs — KEPT, default-OFF

**Status:** kept, **default-OFF** behind a dedicated env gate. Lands in v0.10.1; promotion to default-on
deferred to a future round pending a clean startup-time non-regression (see retry note below).

**Gate:** `FT_MOONSHOT_SKIP_STARTUP_WAL_CHECKPOINT` — dedicated `skip_startup_wal_checkpoint_enabled()`
with its OWN `.unwrap_or(false)` (so a future promotion is a one-line flip); honors `FT_MOONSHOT_ALL`;
the shared `storage_env_flag_enabled` default is **untouched**.

**Profile attribution:** round-7 B0′ (`round7-profile-targets.md`) — `storage.wal_recovery_dirty`
(`storage.rs` `check_and_recover_wal`, the writer-open path at the single call site) = **3.528%** startup
self-time, mean **8.21 ms** on a 4.7 MB dirty WAL, LIVE (every `StorageHandle` writer-open runs it). The
only round-7 new CPU frame clearing both the ≥0.5% profile gate AND production-liveness.

**Lever:** before the startup `PRAGMA wal_checkpoint(PASSIVE)`, conservatively estimate WAL frames from the
32-byte WAL header + file size (over-counting — the safe direction). If the gate is on, no rollback
journal exists, `quick_check` passes, and the estimate is `<= WAL_RECOVERY_THRESHOLD` (10 000), skip the
checkpoint (SQLite replays the WAL on open/read; checkpointing is maintenance/compaction). Any ambiguity
(gate off, journal present, unreadable/bad-magic/bad-page-size header, estimate over threshold) falls back
to the existing checkpoint path. Corruption fail-closed and large-WAL TRUNCATE semantics preserved.

**Behavior-preservation / proof:** RCH-remote `vmi1227854 (1219.4s)`, fail-closed
(`RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 CARGO_NET_GIT_FETCH_WITH_CLI=true`),
`cargo test -p frankenterm-core --test round8_wal_recovery`: **8 passed, 0 failed**. Cases:
T1 small dirty WAL → skipped + **durability oracle** (a fresh reader replays the WAL; zero row loss);
T2 over-threshold WAL → not skipped; T3 malformed/short/bad-magic/bad-page-size headers → `Unreadable`
fallback + exact-frame positive controls; T4 rollback journal present → fallback; T5 gate-off legacy
parity + **child-process end-to-end** of the real env→gate→decision wiring (default-off honored, `=1`
skips, `=0` doesn't); T6 corruption (`quick_check != ok`) → `Err(Corruption)` under **both** gate states.

**A/B verdict:** kept default-off. The win is **structural** (skip ~8 ms of startup checkpoint on a dirty
WAL, ~1×/min in the fleet model) — recorded as evidence, not promoted this round.

**Pattern applied:** unconditional startup maintenance checkpoint → cheap header-estimate guard that skips
it when provably safe.

**Retry / promotion predicate (Form 1):** promote to default-on only after a deterministic startup-time
measurement shows the skip path materially faster on a dirty-WAL open with **no regression** on the
clean-start and large-WAL paths, and a soak confirms no durability surprise across crash/replay. Until
then it ships default-off (zero-risk) on this 8-case correctness proof.

**Rollback:** `FT_MOONSHOT_SKIP_STARTUP_WAL_CHECKPOINT` defaults off; `git revert 70ee7c9dd`.

---

_(Round-8 promoted-to-default-on entries would land above with a full same-run-window proof card. None
this round — convergence. Carryovers/no-wins → `round8-negative-results.md`.)_
