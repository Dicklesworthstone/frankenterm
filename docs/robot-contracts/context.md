# Robot Family Contract: `context`

**Bead:** [BR-RC-ROBOT-CONTRACT.3] / `ft-hac7w.4`
**Status:** Native Robot Mode handler shipped under `ft-bs9uh.3`.
Schema-DSL contract + state-space proof + 10-test conformance
harness extension remain live, and the CLI now writes durable
`pane_contexts` / `context_rotations` rows for status, rotate,
and history. The native adapter records metadata and receipts;
it does not persist raw conversation content.

## Family overview

Per-pane conversation context tracking. Mutating action
(`rotate`) routes through a TX-style receipt with a
content-addressed `rotation_id` so failed calls can be retried
by `caller_idempotency_key` without producing duplicate
storage rows.

| Action | Idempotency | Failure semantics | Side effects |
|---|---|---|---|
| `status` | Idempotent | MustNotPartiallyMutate | (read-only) |
| `rotate` | Sequential (idempotency-key replay) | MustNotPartiallyMutate | events: `context.rotated`; tables: `pane_contexts`, `context_rotations`; ipc: `session_restore` |
| `history` | Idempotent | MustNotPartiallyMutate | (read-only) |

Concurrency: **PerPaneSerial** — serializable per `pane_id`,
parallel across distinct panes.

## Stateright invariants (proven)

The `crate::robot_context_state_machine` module ships an
always-on regression net for **4 named safety invariants**:

1. **NoOrphanArchivedContext** — every entry in
   `context_rotations` references a row in `pane_contexts`
   (or, equivalently, in the harness's `archived_contexts`
   set). The bead's `rotate_no_orphan_archived_context`
   Custom invariant.
2. **AtomicRotateFailure** — `RotateFail` action leaves the
   world unchanged.
3. **IdempotencyReplay** — re-issuing rotate with the same
   `(pane_id, caller_idempotency_key)` returns the same
   `rotation_id` with `is_replay=true` and **no second
   event**.
4. **HistoryIsPureRead** — `Status` and `History` actions do
   not mutate any field.

## Contract semantics

### `status`

> Snapshot the active context state for a pane.

**Request:** `{ "action": "status", "params": { "pane_id":
"<required>" } }`

**Response `data`:** `panes`, `fleet_pressure`, and per-pane
fields including `pane_id`, `active_context_id`, `depth`
(rotation count), `last_rotated_at_ms` (optional),
`pressure_tier`, token estimates, and
`raw_context_content_stored: false`. Omitting `pane_id` returns
the tracked context registry across panes.

### `rotate`

> Archive the active context and start a fresh one.
> **Non-idempotent at the rotation level** (every call
> produces a new context_id) but **idempotent at the receipt
> level** when `caller_idempotency_key` is supplied (replay
> returns the same `rotation_id`).

**Request:**

```json
{
  "action": "rotate",
  "params": {
    "pane_id":                 "<required>",
    "reason":                  "<optional>",
    "caller_idempotency_key":  "<optional>"
  }
}
```

**Response `data`:**

```json
{
  "rotation_id":          "<BLAKE3 hex of (pane_id, key, ts)>",
  "pane_id":              "<echo>",
  "previous_context_id":  "<archived context, absent on first rotation>",
  "new_context_id":       "<newly active>",
  "rotated_at_ms":        1714560000000,
  "is_replay":            false
}
```

### `history`

> List past rotations newest-first.

**Request:** `pane_id` (required), `limit` (optional, default
100).

**Response:** `pane_id`, `rotations`, `truncated`.

## Replay semantics

The bead's headline operational property: a rotate request
that doesn't return a response can be safely retried by the
caller using the same `caller_idempotency_key`. The server
returns the same `rotation_id` with `is_replay: true`, no
second `pane_contexts` / `context_rotations` row, no second
event. Verified by:

- Lib test `idempotency_key_replay_returns_same_rotation_id`
  — same key, world unchanged on replay.
- Conformance test
  `context_state_machine_idempotency_key_replay_no_double_event`
  — pre-replay event count == post-replay event count.

## Coverage

| Run | Cases | Bead's "rotation atomicity" focus |
|---|---|---|
| 11 unit tests in `robot_context_state_machine::tests` | every transition + replay + atomicity + deep history | ✓ |
| Lib test `deep_history_preserves_no_orphan_invariant` | 10 sequential rotations | ✓ — every archived context stays in archive set |
| Conformance test `context_state_machine_random_schedule_sweep_is_clean` | 1024 × 12 = ~12k transitions | ✓ — invariants verified on every reachable state |

## CI gate

`tests/robot_family_conformance.rs` ships **10 context-specific
tests**:

1. `context_contract_self_validates`
2. `context_contract_json_schema_accepts_action_exemplars`
3. `context_contract_json_schema_rejects_status_without_pane_id`
4. `context_contract_proptest_inputs_validate_against_schema`
   (128 random)
5. `context_contract_mcp_descriptors_are_unique_and_well_formed`
6. `context_contract_rotate_is_sequential_with_idempotency_key_replay`
   (validates IdempotencyClass + Idempotence + AtomicOnFailure
   + Custom no-orphan invariant)
7. `context_contract_status_and_history_are_read_only`
8. `context_state_machine_canonical_rotate_sequence_is_clean`
9. `context_state_machine_idempotency_key_replay_no_double_event`
10. `context_state_machine_random_schedule_sweep_is_clean`

**Total conformance harness now: 53 always-on tests**
(7 profile + 12 checkpoint + 12 work + 12 fleet + 10 context)
— closing the bead clears the path to closing the parent epic
ft-hac7w.

## Re-running

```bash
CARGO_TARGET_DIR=/tmp/ft-pane3-target \
CC=/opt/homebrew/opt/llvm/bin/clang CXX=/opt/homebrew/opt/llvm/bin/clang++ \
cargo test -p frankenterm-core --lib robot_context_state_machine:: \
    --features asupersync-runtime --no-default-features
# → 11 passed

cargo test -p frankenterm-core --test robot_family_conformance \
    --features asupersync-runtime --no-default-features
# → 53 passed (all 5 families)
```

## Bead acceptance status

| Item | Status |
|---|---|
| Contract at docs/robot-contracts/context.md | ✓ |
| Schema migrations for pane_contexts + context_rotations | ⏳ integration follow-on |
| Handler with TX-receipt emission for rotate | ⏳ integration follow-on (state machine is the contract; handler wires to cass-types + session-resume) |
| Conformance harness with rotation-atomicity focus | ✓ (10 tests + 1024 × 12 random sweep) |
| ntm fallback removed | ⏳ depends on handler wiring |
| README E2E example | ⏳ depends on handler wiring |
| Per-release attestation entry | ⏳ depends on `ft-syqcz.1` |

## Cross-references

- **Schema-DSL infrastructure:** `ft-hac7w.1`.
- **Sibling family contracts:**
  `profile` (`ft-hac7w.2`), `checkpoint` (`ft-hac7w.3`),
  `work` (`ft-hac7w.5`), `fleet` (`ft-hac7w.6`).
- **Sibling state-space proofs:**
  `tx_killswitch_model` (`ft-x0666.4`),
  `wire_dedup_model` (`ft-x0666.3`),
  `robot_checkpoint_state_machine` (`ft-hac7w.3`),
  `robot_work_state_machine` (`ft-hac7w.5`),
  `robot_fleet_state_machine` (`ft-hac7w.6`).
- **State-machine model:**
  `crates/frankenterm-core/src/robot_context_state_machine.rs`.
- **Conformance harness:**
  `crates/frankenterm-core/tests/robot_family_conformance.rs`.
- **Attestation cross-link:** `ft-syqcz.1`.
