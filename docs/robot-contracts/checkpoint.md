# Robot Family Contract: `checkpoint`

**Bead:** [BR-RC-ROBOT-CONTRACT.2] / `ft-hac7w.3`
**Status:** Foundation slice shipped. Schema-DSL contract +
state-space proof + TLA+ spec + 9-test conformance harness all
live; wiring `RobotCommands::Checkpoint` to the existing
`ft snapshot` + `session_restore` machinery is the integration
follow-on. ntm differential test (action #5) consumes the
state-machine model + the
`crate::robot_ntm_differential::DifferentialHarness` from
`ft-hac7w.1.1`.

## Family overview

| Action | Idempotency | Failure semantics | Side effects |
|---|---|---|---|
| `save` | Idempotent | MustNotPartiallyMutate | events: `checkpoint.saved`; tables: `snapshots` |
| `rollback` | Sequential (approval-gated) | MustNotPartiallyMutate | events: `checkpoint.rolled_back`; tables: `snapshots`, `session_state`; ipc: `session_restore` |
| `list` | Idempotent | MustNotPartiallyMutate | (read-only) |

Concurrency: **Serializable** per session.

## Contract semantics

### `save`

> Persist a session snapshot. **Content-addressed**;
> re-issuing with the same source state returns the same
> checkpoint id without a new snapshots-table row.

**Request:**

```json
{
  "action": "save",
  "params": {
    "session_id": "<required, max 32 chars>",
    "label":      "<optional, max 32 chars>",
    "metadata":   { "<key>": "<value>" }
  }
}
```

**Response `data`:**

```json
{
  "checkpoint_id":  "<BLAKE3 hex of session content>",
  "session_id":     "<echo>",
  "created_at_ms":  1714560000000,
  "is_duplicate":   false
}
```

**Invariants enforced:**

1. `save_is_deterministic` — same `(session_id, label,
   metadata)` against the same source state produces the same
   `checkpoint_id`.
2. `save_response_shape` — the response validates against the
   declared schema.
3. `save_is_idempotent` — re-issuing with the same content does
   not produce a second row in `snapshots`.
4. `save_atomic_on_failure` — a failed save leaves no row in
   `snapshots` and emits no `checkpoint.saved` event.
5. `save_content_address_collision_resistance` (Custom) — two
   saves with distinct content produce distinct
   `checkpoint_id`s. Backed by BLAKE3's collision resistance.

### `rollback`

> Restore a session to a previously-saved checkpoint.
> **Requires an approval token** (cross-pane mutation).
> MUST NOT partially mutate.

**Request:**

```json
{
  "action": "rollback",
  "params": {
    "checkpoint_id":  "<required, BLAKE3 hex>",
    "approval_token": "<required>",
    "dry_run":        false
  }
}
```

**Response `data`:**

```json
{
  "checkpoint_id":  "<echo>",
  "session_id":     "<resolved from checkpoint>",
  "panes_restored": 12,
  "dry_run":        false
}
```

**Invariants enforced:**

1. `rollback_is_deterministic` — same `(checkpoint_id,
   approval_token, dry_run)` against the same starting state
   produces identical observable outcome.
2. `rollback_response_shape`.
3. `rollback_atomic_on_failure` — a failed rollback leaves
   `session_state` untouched and emits no
   `checkpoint.rolled_back` event.
4. `rollback_requires_approval` (Custom) — rollback without a
   valid `approval_token` returns `Denied` with no side
   effects. **Verified at the state-machine level:** the BFS
   harness asserts `UnauthorizedRollback` fires iff a
   `RollbackSucceeded` outcome is reached without a non-absent
   non-invalid token.

### `list`

> List checkpoints for a session.

**Request:**

```json
{
  "action": "list",
  "params": {
    "session_id": "<required>",
    "limit":      100
  }
}
```

**Response `data`:**

```json
{
  "session_id":  "<echo>",
  "checkpoints": [ ... newest-first ],
  "truncated":   false
}
```

**Invariants enforced:**

1. `list_is_deterministic`.
2. `list_response_shape`.

## State-space proof

The `crate::robot_checkpoint_state_machine` module ships a
pure-Rust BFS-shape model of the save→rollback state machine.
Five named safety invariants:

| Invariant | Claim |
|---|---|
| `NoOrphanCheckpoint` | every `last_checkpoint` pointer resolves to a row in `snapshots` |
| `NoDoubleSaveOnSameContent` | snapshots-table has at most one entry per content hash |
| `NoUnauthorizedRollback` | a `RollbackSucceeded` outcome implies a non-absent, non-invalid token in the action |
| `AtomicOnRollbackFailure` | `RollbackFail` action leaves `session_state` unchanged |
| `ListIsPureRead` | `List` action does not change any field of the world |

Verified by:

- 16 unit tests in `robot_checkpoint_state_machine::tests`.
- 3 conformance harness tests in
  `tests/robot_family_conformance.rs`:
  `state_machine_canonical_save_rollback_is_clean`,
  `state_machine_unauthorized_rollback_invariant_fires_when_violated`,
  `state_machine_random_schedule_sweep_is_clean` (256 random
  schedules × 10 steps each).

## TLA+ spec

`docs/specs/robot-checkpoint.tla` mirrors the Rust model:

- `VARIABLES snapshots, session_state, events`
- 8 actions: `Save / Rollback / RollbackDryRun /
  RollbackDenied / SaveFail / RollbackFail / MutateContent /
  List`
- Safety invariants: `TypeOK`, `NoOrphanCheckpoint`,
  `NoDoubleSaveOnSameContent`, `AfterSavePointerIsSet`
- Liveness: `SaveLandsContent` (under fairness on Save)

TLC operators run:

```bash
java -jar tla2tools.jar -workers auto docs/specs/robot-checkpoint.tla
```

## ntm differential test (follow-on)

The bead's action #5 ("Differential test against
`ntm checkpoint` — must show zero observable divergence") plugs
the state-machine model into the
`crate::robot_ntm_differential::DifferentialHarness` from
`ft-hac7w.1.1`. Each request is sent to both `ft robot
checkpoint` and `ntm checkpoint`; responses are normalized
(Layer-1 trivial drift + Layer-2 operational drift per
`docs/robot-contracts/ntm-differential-rules.md`) and
compared. Acceptance: zero divergence on a 1000-request
fuzz corpus per PR.

The differential test's input-generation strategy uses the
contract's `proptest_seeds()` directly — same source-of-truth
as the conformance harness.

## CI gate

`tests/robot_family_conformance.rs` ships **9 checkpoint-
specific tests**:

1. `checkpoint_contract_self_validates`
2. `checkpoint_contract_json_schema_accepts_action_exemplars`
3. `checkpoint_contract_json_schema_rejects_rollback_without_required_fields`
4. `checkpoint_contract_proptest_inputs_validate_against_schema`
   (128 random inputs)
5. `checkpoint_contract_mcp_descriptors_are_unique_and_well_formed`
6. `checkpoint_contract_invariants_have_unique_action_invariant_pairs`
7. `checkpoint_contract_save_is_idempotent`
8. `checkpoint_contract_rollback_is_atomic_on_failure`
9. `checkpoint_contract_list_is_read_only`

Plus 3 state-machine harness tests.

Total: **12 always-on tests** for the checkpoint family;
combined with the existing 7 profile tests, the family
conformance harness has 19 tests, 19 passing.

## Re-running

```bash
# Library tests (state machine + family contract):
CARGO_TARGET_DIR=/tmp/ft-pane3-target \
CC=/opt/homebrew/opt/llvm/bin/clang CXX=/opt/homebrew/opt/llvm/bin/clang++ \
cargo test -p frankenterm-core --lib robot_checkpoint_state_machine:: \
    --features asupersync-runtime --no-default-features
# → 16 passed

cargo test -p frankenterm-core --lib robot_family_contract:: \
    --features asupersync-runtime --no-default-features
# → 13 passed

# Conformance harness (profile + checkpoint families):
cargo test -p frankenterm-core --test robot_family_conformance \
    --features asupersync-runtime --no-default-features
# → 19 passed (7 profile + 9 checkpoint contract + 3 state machine)
```

## Bead acceptance status

| Item | Status |
|---|---|
| Contract at docs/robot-contracts/checkpoint.md | ✓ |
| TLA+ spec at docs/specs/robot-checkpoint.tla | ✓ |
| Conformance harness at tests/robot_family_conformance | ✓ (extends existing harness with 9 checkpoint tests + 3 state-machine tests) |
| State-space proof | ✓ (16 unit tests + 256-trial random schedule sweep) |
| Wire RobotCommands::Checkpoint to handler | ⏳ integration follow-on (depends on ft snapshot subsystem wiring) |
| Differential test against ntm checkpoint | ⏳ uses ft-hac7w.1.1 DifferentialHarness; consumes contract's proptest_seeds() |
| TLC verification passes safety + liveness | ⏳ TLA+ spec shipped; operator runs TLC |
| ntm fallback removed | ⏳ depends on handler wiring |
| README E2E example | ⏳ depends on handler wiring |
| Per-release attestation entry | ⏳ depends on `ft-syqcz.1` schema bead |

## Cross-references

- **Schema-DSL infrastructure:**
  `crates/frankenterm-core/src/robot_family_contract.rs`
  (`ft-hac7w.1`).
- **ntm differential harness:**
  `crates/frankenterm-core/src/robot_ntm_differential.rs`
  (`ft-hac7w.1.1`).
- **State-machine model:**
  `crates/frankenterm-core/src/robot_checkpoint_state_machine.rs`.
- **Conformance harness:**
  `crates/frankenterm-core/tests/robot_family_conformance.rs`.
- **TLA+ spec:** `docs/specs/robot-checkpoint.tla`.
- **Sibling family contracts:** `profile` (proof-of-concept,
  shipped at `ft-hac7w.1`); `context`, `work`, `fleet` (open).
- **Sibling state-space proofs** (same Rust+TLA+ shape):
  `tx_killswitch_model` (`ft-x0666.4`),
  `wire_dedup_model` (`ft-x0666.3`).
- **Attestation cross-link:** `BR-RC-FOUNDATION.G3.1`
  (`ft-syqcz.1`).
