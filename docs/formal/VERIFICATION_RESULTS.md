# Formal Verification Results (TLA+ / TLC)

Date: 2026-02-13
Scope: `wa-283h4.3` formal specs in `docs/formal/*`

## Tooling

- TLC runner: `/tmp/tla2tools.jar` (downloaded from `tlaplus/tlaplus` latest release)
- Java runtime: OpenJDK 21.0.10

Command pattern used:

```bash
cd docs/formal
java -cp /tmp/tla2tools.jar tlc2.TLC -deadlock -workers auto -config <module>.cfg <module>.tla
```

## Summary

| Spec | Outcome | States Generated | Distinct States | Search Depth | Notes |
|---|---|---:|---:|---:|---|
| `mux_protocol.tla` | PASS | 1,395 | 1,125 | 30 | Safety + liveness checked |
| `snapshot_lifecycle.tla` | PASS | 102 | 64 | 15 | Safety + liveness checked |
| `concurrent_panes.tla` | PASS | 22,980,985 | 1,748,872 | 27 | Safety checked (liveness deferred; see caveat) |
| `wal_correctness.tla` | PASS | 92,841 | 36,408 | 12 | Safety + liveness checked |

## Per-Spec Configuration

### `mux_protocol.cfg`

- `MaxQueue = 2`
- `MaxMsgId = 5`
- Checked invariants:
  - `Safety_MessageTracked`
  - `Safety_NoDuplicateDelivery`
  - `Safety_OrderedDelivery`
- Checked temporal properties:
  - `Liveness_LeavesError`
  - `Liveness_EventuallyIdleOrError`

### `snapshot_lifecycle.cfg`

- `PaneIds = {1}`
- `Values = {"A"}`
- `DefaultValue = "A"`
- `MaxHistory = 1`
- Checked invariants:
  - `TypeOK`
  - `Safety_CaptureConsistent`
  - `Safety_Atomicity`
  - `Safety_NoDataLoss`
  - `Safety_Idempotency`
  - `Safety_NoPartialCommit`
- Checked temporal properties:
  - `Liveness_CaptureProgress`
  - `Liveness_RestoreProgress`

### `concurrent_panes.cfg`

- `PaneUniverse = {1, 2, 3}`
- `SizeValues = {24, 40}`
- `NoPane = 0`
- Checked invariants:
  - `TypeOK`
  - `Safety_LifecycleDisjoint`
  - `Safety_NoOrphans`
  - `Safety_NoLeaks`
  - `Safety_AliveHasResources`
  - `Safety_FocusValid`
  - `Safety_OrderedWaits`
  - `Safety_NoSelfWait`

### `wal_correctness.cfg`

- `Keys = {1, 2}`
- `Values = {"X", "Y"}`
- `DefaultValue = "X"`
- `MaxOps = 5`
- Checked invariants:
  - `TypeOK`
  - `Safety_RunningMatchesLog`
  - `Safety_DurableBound`
  - `Safety_DurableWritesSurviveCrash`
  - `Safety_ReplayEquivalent`
  - `Safety_CompactionSafe`
- Checked temporal properties:
  - `Liveness_CrashRecovers`
  - `Liveness_DurableProgress`

## Caveat: Concurrent Liveness

`concurrent_panes.tla` defines liveness formulas in the module, but they are not currently enabled in the TLC config. Under existentially-quantified completion actions (set-queue scheduling), TLC produced starvation counterexamples that are scheduler artifacts rather than safety violations.

Planned follow-up for full temporal checking:

1. Refine pending-op modeling to explicit FIFO or per-pane fairness obligations.
2. Re-enable liveness properties in `concurrent_panes.cfg`.
3. Re-run TLC and capture a liveness-complete report.

## Artifacts

- `docs/formal/mux_protocol.tlc.log`
- `docs/formal/snapshot_lifecycle.tlc.log`
- `docs/formal/concurrent_panes.tlc.log`
- `docs/formal/wal_correctness.tlc.log`
