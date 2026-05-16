# Robot Family Contract: `profile`

**Bead:** [BR-RC-ROBOT-CONTRACT.1] / `ft-hac7w.2`.
**Status:** Contract substrate, wired read/dry-run handler, and live
non-dry-run mux mutation path are live.
Schema-DSL contract (under `profile_family_contract()` in
`crates/frankenterm-core/src/robot_family_contract.rs`) plus state-space proof
+ conformance tests in `tests/robot_family_conformance.rs` are live.
`RobotCommands::Profile` dispatches through
`frankenterm_core::robot_profile_handler::handle_profile_command` against the
workspace `agent_profiles` table. `list`, `show`, `validate`, and dry-run
`apply` are wired through the storage-only handler. Non-dry-run `apply` routes
through `handle_profile_apply_with_executor`, which builds a shared fleet
mutation plan, spawns through the mux bridge, persists `profiles_applied_log`
receipts, and rolls back prior spawned panes on mid-apply failure. The ntm
differential test consumes the state-machine model + the
`crate::robot_ntm_differential::DifferentialHarness` from `ft-hac7w.1.1`.

## Family overview

| Action | Idempotency | Failure semantics | Side effects |
|---|---|---|---|
| `list` | Idempotent | MustNotPartiallyMutate | (read-only) |
| `show` | Idempotent | MustNotPartiallyMutate | (read-only) |
| `apply` | Idempotent on identical input | MustNotPartiallyMutate | tables: `agent_profiles`; mux: spawns `count` panes |
| `validate` | Idempotent | MustNotPartiallyMutate | (read-only) |

Concurrency: **Serializable per profile name**. Two `apply`
actions on the same `(name, count, env_overrides, dry_run)`
tuple are observationally equivalent; concurrent applies on
different names are independent.

## Contract semantics

### `list`

> List available profiles, optionally filtered by role / tag.

**Request:**

```json
{
  "action": "list",
  "params": {
    "role_filter": "<optional, max 64 chars>",
    "tag_filter":  "<optional, max 64 chars>"
  }
}
```

**Response `data`:**

```json
{
  "profiles":  [ ... newest-first by `created_at_ms` ],
  "filtered":  true,
  "truncated": false
}
```

**Invariants enforced:**

1. `list_is_deterministic` — same `(role_filter, tag_filter)`
   against the same `agent_profiles` table produces the same
   ordered response.
2. `list_response_shape` — the response validates against the
   family schema.

### `show`

> Show details of a single profile by name.

**Request:**

```json
{
  "action": "show",
  "params": {
    "name": "<required, 1..=64 chars, /^[a-zA-Z0-9_-]+$/>"
  }
}
```

**Response `data`:**

```json
{
  "name":         "<echo>",
  "role":         "<string>",
  "tags":         [ ... ],
  "shell":        "<string>",
  "command":      "<optional string>",
  "env":          { "<key>": "<value>" },
  "metadata":     { "<key>": "<value>" },
  "created_at_ms": 1714560000000,
  "updated_at_ms": 1714560001000
}
```

**Invariants enforced:**

1. `show_is_deterministic`.
2. `show_response_shape`.
3. `show_does_not_mutate` — verified at the state-machine
   level: `Show` actions never change any field of the world.

### `apply`

> Apply a profile — record the apply event in
> `agent_profiles` and spawn `count` panes with the profile's
> shell + env. Idempotent on identical
> `(name, count, env_overrides, dry_run)` input.

**Request:**

```json
{
  "action": "apply",
  "params": {
    "name":          "<required>",
    "count":         1,
    "env_overrides": { "<key>": "<value>" },
    "dry_run":       false
  }
}
```

**Response `data`:**

```json
{
  "profile_name":              "<echo>",
  "panes_spawned":             [ 10, 11 ],
  "dry_run":                   false,
  "requested_agents":          2,
  "spawned_agents":            2,
  "skipped_existing_agents":   0,
  "policy_decisions":          [ ... ],
  "idempotency_key":           "<profiles_applied_log content_hash>",
  "mutation_idempotency_key":  "<fleet mutation key>",
  "rollback_status":           "succeeded",
  "idempotent_replay":         false,
  "mutation_receipt":          { "...": "full fleet mutation receipt" }
}
```

**Invariants enforced:**

1. `apply_is_deterministic` — same
   `(name, count, env_overrides, dry_run)` against the same
   starting state produces the same observable outcome.
2. `apply_response_shape`.
3. `apply_atomic_on_failure` — a failed apply does not leave
   half the panes spawned. Either all `count` panes spawn or
   none do; intermediate failures roll back.
4. `apply_idempotent_on_duplicate_input` (Custom) — the
   second apply with identical input returns
   `is_duplicate: true` with the same `panes_spawned` list and
   no new pane spawns. **Verified at the state-machine level:**
   the BFS harness asserts that two consecutive `Apply` actions
   with the same parameters yield identical world state.
5. `apply_validates_profile_exists` (Custom) — apply against
   a non-existent profile returns `ProfileNotFound` with no
   side effects.
6. `apply_dry_run_no_side_effects` (Custom) — `dry_run: true`
   never mutates `agent_profiles` and never spawns panes.

Specific failure envelopes:

- `robot.profile.daemon_unavailable` when the mux bridge cannot spawn.
- `robot.profile.policy_denied` when policy rejects the mutation preflight.
- `robot.profile.require_approval` when policy requires approval.
- `robot.profile.validation_failed` when the profile or env overrides are not
  safe to execute.
- `robot.profile.mux_spawn_failed` / `robot.profile.bootstrap_failed` for
  live spawn/setup failures.
- `robot.profile.compensation_failed` when rollback cannot close a pane that
  was spawned before a later failure.

### `validate`

> Validate a profile definition without applying it.

**Request:**

```json
{
  "action": "validate",
  "params": {
    "name": "<required>"
  }
}
```

**Response `data`:**

```json
{
  "name":     "<echo>",
  "valid":    true,
  "issues":   [ ... ]
}
```

**Invariants enforced:**

1. `validate_is_deterministic`.
2. `validate_response_shape`.
3. `validate_does_not_mutate` — verified at the state-machine
   level.

## State-space proof

The `crate::robot_profile_state_machine` module ships a
pure-Rust BFS-shape model of the profile state machine.
Five named safety invariants:

| Invariant | Claim |
|---|---|
| `NoOrphanProfile` | every `apply` references a profile that exists in `agent_profiles` |
| `NoDoubleSpawnOnDuplicateApply` | repeated apply on same input does not double-spawn panes |
| `ApplyAtomic` | `ApplyFail` leaves `agent_profiles` and `panes` unchanged |
| `DryRunPure` | `Apply { dry_run: true }` does not change any persistent state |
| `ListShowValidatePure` | `List` / `Show` / `Validate` actions do not change any field of the world |

Verified by:

- BFS exhaustive exploration over `step_count ∈ {2, 3}` plus a
  property-test sweep at depth 8.
- Unit tests in `robot_profile_state_machine::tests`.
- Conformance harness tests in `tests/robot_family_conformance.rs`
  (the existing `profile_*` test suite already covers
  schema-level conformance; the state-machine integration
  lands under the same suite as `state_machine_*_profile_*` tests).

## Substrate vs wired-pass scope

Same substrate-pass / wired-pass split pattern as ft-2okh0.5,
ft-t9a6q.1 / .2 / .3:

**Substrate-pass (shipped):**
- Contract doc (this file).
- `profile_family_contract()` factory in
  `robot_family_contract.rs` (already exists per ft-hac7w.1).
- 7 schema-level conformance tests (already in
  `tests/robot_family_conformance.rs`).
- State-space proof at
  `crate::robot_profile_state_machine`.
- NTM mirror differential harness in
  `tests/robot_profile_ntm_differential.rs`.

**Wired-pass (partially shipped):**
- `agent_profiles` schema + SQL primitives are live.
- `RobotCommands::Profile` routes to the DB-backed handler in
  `crates/frankenterm/src/main.rs`.
- `list`, `show`, `validate`, and dry-run `apply` return typed data envelopes.
- Non-dry-run `apply` routes to the mux-backed mutation executor and writes
  `profiles_applied_log` receipts after successful spawn.

## Cross-references

- `crate::robot_family_contract::profile_family_contract` — the schema-DSL contract.
- `crate::robot_profile_state_machine` — the BFS state-space model.
- `crate::robot_ntm_surface::ProfileCommand` — the wire-format request types.
- `crate::config_profiles` — existing config-side profile management; keep it aligned with the native profile handler.
- `crate::session_profiles` — session-level profile types (ft-3681t.2.4).
- `tests/robot_family_conformance.rs` — schema + state-machine conformance harness.
- ft-hac7w (parent epic — Robot Family Closure).
- ft-hac7w.1 (closed — schema infra).
- ft-hac7w.3 (closed — checkpoint family, sibling).
- ft-hac7w.5 (closed — work family, sibling).
- ft-hac7w.6 (closed — fleet family, sibling).
