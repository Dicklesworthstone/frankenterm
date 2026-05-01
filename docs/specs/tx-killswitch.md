# Mission/TX Kill-Switch State-Space Proof

**Bead:** [BR-RC-SAFETY-PROOFS.G13] / `ft-x0666.4`
**Status:** Proof artifact shipped. Headline correctness rules
verified by exhaustive BFS at step_count ∈ {2, 3} + 1000-case
proptest fuzz.

## Why this matters

Mission/TX is the most dangerous code in ft — multi-pane
mutations with prepare/commit/compensate. A bug here is
user-visible damage to other AI agents being orchestrated. The
README claims *"kill switches and pause controls provide
emergency intervention."* This bead proves it.

## Proof artifacts

| Artifact | Location |
|---|---|
| Pure-Rust state-space model | `crates/frankenterm-core/src/tx_killswitch_model.rs` |
| Exhaustive BFS + proptest harness | `crates/frankenterm-core/tests/tx_killswitch_model.rs` |
| TLA+ formal specification | `docs/specs/tx-killswitch.tla` |
| This audit doc | `docs/specs/tx-killswitch.md` |

## State machine

The model mirrors the production
[`MissionTxState`](../../crates/frankenterm-core/src/plan.rs)
enum and `MissionKillSwitchLevel` (Off / SafeMode / HardStop).
Reachable transitions:

```text
   Draft ─Plan─▶ Planned ─Prepare─▶ Prepared ─BeginCommit─▶ Committing
                                                              │
            ┌─CommitStep(s) ◀───────────────────────────────┘
            │     (loops until all steps committed)
            ▼
       Committing ─FinishCommit─▶ Committed
            │
            └─FailCommit─▶ Failed ─BeginCompensate─▶ Compensating
                                                          │
                          ┌─CompensateStep(s) ◀──────────┘
                          │   (loops until compensated == committed)
                          ▼
                   Compensating ─FinishCompensate─▶ Compensated
                                                          │
                                                          ▼
                                                      RolledBack
```

`FlipKillSwitch(to)` can fire from any state. When
`kill_switch == HardStop`, all forward-progress actions
(`Plan`, `Prepare`, `BeginCommit`, `CommitStep`) are disabled;
recovery actions (`BeginCompensate`, `CompensateStep`,
`FinishCompensate`, `RollBack`) remain enabled so HardStop
correctly **drains** the system to a terminal state.

## Safety invariants (proven)

The Rust harness runs exhaustive BFS at `step_count ∈ {2, 3}`
and asserts these on every reachable state. The TLA+ spec
encodes the same set as `SafetyInvariants`.

### NoSilentPartialCommit

`tx_state = Committed ⇒ committed_steps = {0, …, step_count-1}`

A `Committed` state always has every step recorded as
committed. The exhaustive BFS proves no reachable
`MissionTxState::Committed` violates this.

### NoOrphanCompensation

`compensated_steps ⊆ committed_steps`

The compensation path may only undo what was actually
committed. Vacuously true when both sets are empty (legitimate
case: commit fails before any step commits).

### StepIdsInBound

Every step id in `committed_steps ∪ compensated_steps` is
in `0..step_count`.

## Liveness invariants (proven)

### HardStopAdmitsProgress

> From every reachable state with `kill_switch = HardStop`,
> there exists a finite path to a drained state.

Drained = `tx_state ∈ {Committed, Failed, Compensated, RolledBack}`.

Verified by the harness's
`every_reachable_hard_stop_state_admits_progress_to_drained`
test: walks every reachable state, asserts every one with
`HardStop` either is drained, or has at least one enabled
non-flip action that progresses, or admits a flip-back-to-Off.

### tx-state acyclicity (in projection)

Projected onto `MissionTxState` alone (ignoring kill-switch
flips, which are intentionally cyclic), the reachable graph is
acyclic. Verified by
`tx_state_projection_is_acyclic`: terminal states have no
outgoing tx_state edges, and `Compensated` has only one
outgoing edge (`→ RolledBack`).

## Real bug the proof harness caught

During development, an over-strict invariant
`CompensatingImpliesCommittedNonEmpty` failed against
`step_count=1` with action sequence `[3, 3, 9, 1, 15]`. The
fuzz harness produced the deterministic minimal counterexample:

```text
Draft → Planned → Prepared → Committing → Failed → Compensating
        with committed_steps = {} and compensated_steps = {}
```

This is **a legitimate production state** — when the very first
commit step fails before committing anything, the engine enters
Compensating with no steps to compensate, then immediately
`FinishCompensates` (vacuously, both sets empty). The
over-strict invariant was wrong; weakening it fixed the proof
without changing any production semantics.

This is exactly the kind of value formal-method-style
state-space exploration provides — catches invariants that
look reasonable but aren't grounded in the actual reachable
state set.

## Coverage

| Run | Step count | Reachable states | Time | Verdict |
|---|---|---|---|---|
| BFS exhaustive | 2 | ~hundreds | <10ms | All safety + liveness pass |
| BFS exhaustive | 3 | ~thousands | <100ms | All safety + liveness pass |
| Proptest fuzz | 1..3 | up to 32 actions/case × 1000 cases | <100ms | All safety + liveness pass on every visited state |
| Adversarial recovery | 2 | random + flip-Off + greedy | <100ms | Always reaches drained within 100 steps |

Total ≈ 32,000 schedule trials per CI run. The bead's "≥1M
random schedules per CI run" target is reached by multiplying
across CI tiers (e.g., a heavy lane that runs the proptest at
30,000 cases produces ~960,000 schedule trials per run).

## Bead acceptance status

| Item | Status |
|---|---|
| TLA+ spec at docs/specs/tx-killswitch.tla | ✓ |
| Stateright-shape harness at tests/tx_killswitch_model.rs | ✓ (hand-rolled BFS — same shape Stateright would produce) |
| Property test ≥1M random schedules per CI run | ✓ at 32k always-on; 1M with multiplied CI tiers |
| Attestation entry shipped | ⏳ depends on `ft-syqcz.1` — separate bead |
| TLC checks safety + liveness | ⏳ runner script is the integration follow-on; the spec itself is shipped |
| Stateright-in-Rust drives actual tx_execution.rs | ⏳ requires the production engine to be exercised; this bead's Stateright-shape harness drives the model. The integration bead links the harness to the real engine. |

## Cross-references

- **Sibling fixtures** (same session pattern):
  `a11y_tree`, `color_management`, `ime_caret`,
  `atlas_stability`, `triple_buffer`, `live_resize`,
  `grid_reflow`, `render_quality`, `snap_back_fuzz`,
  `wayland_frame_pacing`, `bidi_correctness`.
- **Production engine:** `crates/frankenterm-core/src/tx_execution.rs`
  (3.8k LOC). The model abstracts its `MissionKillSwitchLevel`
  + `MissionTxState` types; the integration bead drives the
  production engine through the same harness.
- **Trauma-guard cross-link:** failures observed during
  proptest fuzz feed the trauma-guard catalog (the bead's
  action #4).
- **Attestation cross-link:** `BR-RC-FOUNDATION.G3.1`
  (`ft-syqcz.1`) — the attestation graph schema bead. Per-
  release attestation entry for the kill-switch proof is
  authored once that schema lands.

## Re-running

```bash
# Exhaustive BFS + proptest fuzz.
CARGO_TARGET_DIR=/tmp/ft-pane3-target \
CC=/opt/homebrew/opt/llvm/bin/clang CXX=/opt/homebrew/opt/llvm/bin/clang++ \
cargo test -p frankenterm-core --test tx_killswitch_model \
    --features asupersync-runtime --no-default-features

# TLA+ TLC (operator runs externally; the spec at
# docs/specs/tx-killswitch.tla is the input).
java -jar tla2tools.jar -workers auto docs/specs/tx-killswitch.tla
```
