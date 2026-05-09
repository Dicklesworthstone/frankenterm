# Robot Family Contract: `fleet`

**Bead:** [BR-RC-ROBOT-CONTRACT.5] / `ft-hac7w.6`
**Status:** Native read adapter shipped under `ft-bs9uh.5`. Live
`RobotCommands::Fleet` dispatches to `robot_fleet_command_response`: `status`
and `agents` use native agent-inventory/work-queue read paths, `scale`
computes spawn/stop plans and executes commit receipts through the mux-backed
fleet mutation substrate, and `rebalance` computes work-assignment plans and
executes commit receipts through the `work_claims` mutation substrate.
Non-dry-run `scale` / `rebalance` write durable `fleet_mutation_receipts`
rows before reporting success; retries with the same idempotency key replay the
stored receipt across a fresh CLI/daemon boundary, while same-key/different-plan
payloads return an explicit conflict before side effects. Dry-run uses the same
plan path and returns a `dry_run` receipt without side effects or durable writes.
**Cross-link to `ft-x0666.4`
(`tx_killswitch_model`):** the kill-switch invariant
`stop_completes_under_kill_switch_hardstop` reuses that proof's
"HardStop disables forward progress, leaves recovery enabled"
pattern.

## Family overview

| Action | Idempotency | Failure semantics | Side effects |
|---|---|---|---|
| `status` | Idempotent | MustNotPartiallyMutate | (read-only) |
| `agents` | Idempotent | MustNotPartiallyMutate | (read-only) |
| `scale` | Idempotent with receipt key; sequential for new commits | Typed inventory / policy / approval / plan / durable-receipt / mutation errors; failed commits carry receipts and compensate prior side effects | Spawn or stop panes through the mux-backed fleet mutation executor; non-dry-run receipts persist in `fleet_mutation_receipts`; dry-run is read-only |
| `rebalance` | Idempotent with receipt key; sequential for new commits | Typed inventory / work-queue / policy / approval / plan / durable-receipt / mutation errors; failed commits carry receipts and compensate prior side effects | Reassign claimed work in `work_claims`; non-dry-run receipts persist in `fleet_mutation_receipts`; dry-run is read-only |

Concurrency: **PerPaneSerial** — serializable per `fleet_id`,
parallel across distinct fleets.

## Lifecycle state machine

Mirrors the TX-engine flow from `tx_killswitch_model`,
projected onto the fleet domain:

```text
   (none) ─PrepareLaunch─▶ Prepared ─CommitLaunch─▶ Committing ─▶ Running
              │                │                                    │
              │                ├─FailLaunch─▶ Failed                 │
              │                │                │                   │
              │                │                ▼                   │
              │                │         Compensating               │
              │                │                │                   │
              │                │                ▼                   │
              │                │           RolledBack ◀─────────────┘ (no double Running)
              │                │
              │                └─ ... transitions ...
              │
              ▼ HardStop disables forward progress
        (Denied: HardStopActive)

   Running ─BeginStop─▶ Stopping ─CompleteStop─▶ Stopped
                            │                       ▲
                            └─ FailStop ─▶ Failed   │
                                              │     │
                                              └─Compensate─▶ RolledBack
```

## Stateright invariants

The `crate::robot_fleet_state_machine` module ships an always-
on regression net for **4 named safety invariants**:

1. **NoDoubleRunningName** — at most one fleet with any given
   `name` is in `Running` at any reachable state.
2. **AtomicLaunchFailure** — after a successful `FailLaunch`,
   the fleet is in `Failed` / `Compensating` / `RolledBack`,
   never `Running` or partially-committed.
3. **TerminalsAreSticky** — `Stopped` / `RolledBack` do not
   regress to non-terminal states.
4. **HardStopAdmittedForwardProgress** — under
   `kill_switch == HardStop`, only recovery actions
   (`FailLaunch`, `CompensateLaunch`, `CompleteStop`,
   `FailStop`, `IdempotentStop`) succeed; forward-progress
   (`PrepareLaunch`, `CommitLaunch`, `BeginStop`) is denied.

### Real bugs the harness caught

During development, the random schedule sweep caught **three
real bugs** in early `apply_action` drafts:

1. **TerminalRegressed**: `PrepareLaunch` against a
   `Stopped`/`RolledBack` fleet at the same `fleet_id` was
   accidentally allowed to overwrite the terminal state.
   Fix: deny when the fleet_id is already in the world (any
   state); a fresh fleet_id must be allocated for relaunch.
2. **NonAtomicLaunchFailure** false positive: my invariant
   checker fired on `FailLaunch` against a non-prepared/
   committing fleet (which is a NoOp, not a violation).
   Fix: tie the check to `LaunchFailed` outcome only, not the
   action alone.
3. **DoubleRunningName** TOCTOU race: two simultaneous
   `PrepareLaunch` calls for the same name both succeeded
   because the original check only looked at `Running` fleets,
   not pending `Prepared`/`Committing` fleets. The harness
   produced a 4-step counterexample:
   `PrepareLaunch{1, 7} → PrepareLaunch{2, 7} → CommitLaunch{1}
   → CommitLaunch{2}` reaches `Running{1, name: 7}` AND
   `Running{2, name: 7}`. Fix: deny `PrepareLaunch` if any
   non-terminal fleet shares the name.

This is the same kind of value formal-method-style state-space
exploration provides — catches invariants that look reasonable
but aren't grounded in the actual reachable state set.

## TLA+ spec

`docs/specs/robot-fleet.tla`:

- 11 actions: `PrepareLaunch / CommitLaunch / FailLaunch /
  CompensateLaunch / BeginStop / CompleteStop / FailStop /
  IdempotentStop / Status / Describe / FlipKillSwitch`
- `SafetyInvariants`: `TypeOK`, `NoDoubleRunningName`
- Liveness: `EventuallyDrains` under fairness on recovery
  actions

TLC operators run:

```bash
java -jar tla2tools.jar -workers auto docs/specs/robot-fleet.tla
```

## CI gate

`tests/robot_family_conformance.rs` ships **12 fleet-specific
tests**:

1. `fleet_contract_self_validates`
2. `fleet_contract_json_schema_accepts_action_exemplars`
3. `fleet_contract_json_schema_rejects_launch_without_pane_count`
4. `fleet_contract_proptest_inputs_validate_against_schema`
   (128 random)
5. `fleet_contract_mcp_descriptors_are_unique_and_well_formed`
6. `fleet_contract_launch_is_sequential_with_atomic_failure`
   (validates IdempotencyClass + AtomicOnFailure invariant +
   tx_engine ipc target)
7. `fleet_contract_stop_is_idempotent_with_kill_switch_invariant`
   (validates Idempotence + Custom kill-switch invariant
   cross-link)
8. `fleet_contract_status_and_describe_are_read_only`
9. `fleet_state_machine_canonical_launch_run_stop_is_clean`
10. `fleet_state_machine_compensation_path_is_clean`
11. `fleet_state_machine_hardstop_cross_links_tx_killswitch_proof`
    (the bead's "TX kill-switch interleaving" requirement —
    BeginStop fires, then HardStop flips, then CompleteStop
    still drains to Stopped)
12. `fleet_state_machine_random_schedule_sweep_is_clean`
    (1024 × 12 = ~12k transitions including kill-switch
    flips)

Total conformance harness: **43 always-on tests** (7 profile +
12 checkpoint + 12 work + 12 fleet).

## Re-running

```bash
# Live dispatch smoke harness (checkpoint + context + work + fleet):
rch exec -- env CARGO_TARGET_DIR=/tmp/ft-bs9uh6-ntm-gap \
  cargo test -p frankenterm --test robot_ntm_gap_contract_tests \
  robot_checkpoint_context_work_fleet_dispatch_matches_manifest -- --nocapture

# Focused native fleet response tests:
rch exec -- env CARGO_TARGET_DIR=/tmp/ft-0elb9-fleet-backend \
  cargo test -p frankenterm --bin ft robot_fleet -- --nocapture

# Cross-surface Robot/MCP golden matrix:
rch exec -- env CARGO_TARGET_DIR=/tmp/ft-0elb9-golden-matrix \
  cargo test -p frankenterm-core --test control_plane_golden_matrix \
  --features vc-export -- --nocapture

# State-machine model:
rch exec -- env CARGO_TARGET_DIR=/tmp/ft-bs9uh6-fleet-core \
  cargo test -p frankenterm-core --lib robot_fleet_state_machine:: \
  --features asupersync-runtime --no-default-features
# → 12 passed (incl. random schedule sweep with kill-switch flips)

# Conformance harness (all four families):
rch exec -- env CARGO_TARGET_DIR=/tmp/ft-bs9uh6-fleet-conformance \
  cargo test -p frankenterm-core --test robot_family_conformance \
  --features asupersync-runtime --no-default-features
# → 43 passed
```

## Bead acceptance status

| Item | Status |
|---|---|
| Contract at docs/robot-contracts/fleet.md | ✓ |
| Status/agents wired to native reads | ✓ `status` and `agents` use native inventory/work-queue summaries |
| Scale/rebalance parse natively | ✓ live plan/receipt paths for dry-run and commit |
| Launch/stop wired through mutation substrate | ✓ scale uses mux-backed spawn/stop receipts; rebalance uses durable `work_claims` reassignment receipts |
| Durable fleet receipt replay | ✓ non-dry-run scale/rebalance persist `fleet_mutation_receipts`; matching idempotency-key retries replay stored receipts, conflicts stop before side effects |
| Conformance harness with TX kill-switch interleavings | ✓ (cross-links to ft-x0666.4 via stop_completes_under_kill_switch_hardstop invariant + harness test) |
| E2E example | ✓ README implementation-status examples include status/agents/scale dry-run |
| Per-release attestation entry | ⏳ depends on `ft-syqcz.1` |

## Cross-references

- **Schema-DSL infrastructure:** `ft-hac7w.1`.
- **TX engine + kill-switch proof:** `ft-x0666.4` /
  `crates/frankenterm-core/src/tx_killswitch_model.rs` /
  `docs/specs/tx-killswitch.tla`. The fleet contract's
  `stop_completes_under_kill_switch_hardstop` Custom invariant
  is the family-specific projection of that proof's
  `HardStopAdmitsProgress` claim.
- **Fleet sub-crate:** `crates/frankenterm-core-fleet/` —
  already has `fleet_dashboard` extracted; status/describe
  consume it.
- **State-machine model:**
  `crates/frankenterm-core/src/robot_fleet_state_machine.rs`.
- **Conformance harness:**
  `crates/frankenterm-core/tests/robot_family_conformance.rs`.
- **TLA+ spec:** `docs/specs/robot-fleet.tla`.
- **Sibling family contracts:** `profile` (`ft-hac7w.1`);
  `checkpoint` (`ft-hac7w.3`); `work` (`ft-hac7w.5`);
  `context` (`ft-hac7w.4`), now graduated from the generic
  NTM-gap fallback.
