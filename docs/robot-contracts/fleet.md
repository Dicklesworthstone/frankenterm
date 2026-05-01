# Robot Family Contract: `fleet`

**Bead:** [BR-RC-ROBOT-CONTRACT.5] / `ft-hac7w.6`
**Status:** Foundation slice shipped. Schema-DSL contract +
TX-engine-integrated state-space proof + TLA+ spec + 12-test
conformance harness extension all live; wiring
`RobotCommands::Fleet` to the existing `frankenterm-core-fleet`
sub-crate + the production TX engine is the integration
follow-on. **Cross-link to `ft-x0666.4`
(`tx_killswitch_model`):** the kill-switch invariant
`stop_completes_under_kill_switch_hardstop` reuses that proof's
"HardStop disables forward progress, leaves recovery enabled"
pattern.

## Family overview

| Action | Idempotency | Failure semantics | Side effects |
|---|---|---|---|
| `status` | Idempotent | MustNotPartiallyMutate | (read-only) |
| `launch` | Sequential (TX-engine-atomic) | MustNotPartiallyMutate | events: launching/launched/launch_failed/launch_compensated; tables: `fleets`; ipc: `mux`, `tx_engine` |
| `stop` | Idempotent (TX-engine-atomic) | MustNotPartiallyMutate | events: stopping/stopped/stop_failed; tables: `fleets`; ipc: `mux`, `tx_engine` |
| `describe` | Idempotent | MustNotPartiallyMutate | (read-only) |

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
# State-machine model:
CARGO_TARGET_DIR=/tmp/ft-pane3-target \
CC=/opt/homebrew/opt/llvm/bin/clang CXX=/opt/homebrew/opt/llvm/bin/clang++ \
cargo test -p frankenterm-core --lib robot_fleet_state_machine:: \
    --features asupersync-runtime --no-default-features
# → 12 passed (incl. random schedule sweep with kill-switch flips)

# Conformance harness (all four families):
cargo test -p frankenterm-core --test robot_family_conformance \
    --features asupersync-runtime --no-default-features
# → 43 passed
```

## Bead acceptance status

| Item | Status |
|---|---|
| Contract at docs/robot-contracts/fleet.md | ✓ |
| Status/describe wired to fleet_dashboard | ⏳ integration follow-on |
| Launch/stop wired through TX engine | ⏳ integration follow-on (state-machine model is ready; tx_execution.rs wiring is the substrate) |
| Conformance harness with TX kill-switch interleavings | ✓ (cross-links to ft-x0666.4 via stop_completes_under_kill_switch_hardstop invariant + harness test) |
| E2E example | ⏳ depends on handler wiring |
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
  `context` (`ft-hac7w.4`, open).
