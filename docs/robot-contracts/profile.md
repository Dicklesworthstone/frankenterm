# Robot Family Contract: `profile`

**Bead:** [BR-RC-ROBOT-CONTRACT.1] / `ft-hac7w.2`.
**Status:** Substrate slice shipped. Schema-DSL contract (under
`profile_family_contract()` in
`crates/frankenterm-core/src/robot_family_contract.rs`) plus
state-space proof + 7 conformance tests in
`tests/robot_family_conformance.rs` are live; wiring
`RobotCommands::Profile` to the existing config/profile
machinery + `agent_profiles` schema migration is the integration
follow-on. ntm differential test (action #5 of the parent
bead's actions) consumes the state-machine model + the
`crate::robot_ntm_differential::DifferentialHarness` from
`ft-hac7w.1.1`.

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
  "name":           "<echo>",
  "applied":        true,
  "panes_spawned":  [ "<pane_id>", ... ],
  "is_duplicate":   false,
  "applied_at_ms":  1714560000000
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
   never mutates `agent_profiles` and never spawns panes; the
   response is the same shape as a real apply but with
   `applied: false`.

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

## What this contract is NOT

- Not the actual handler. Wiring `RobotCommands::Profile`
  into the existing config/profile machinery + the new
  `agent_profiles` schema migration is the integration follow-on
  (filed as `ft-hac7w.2.cont.handler`).
- Not the differential test against `ntm profile`. That uses
  `crate::robot_ntm_differential::DifferentialHarness` from
  `ft-hac7w.1.1` and runs in a separate harness once the real
  handler is wired (filed as `ft-hac7w.2.cont.differential`).
- Not the schema migration. The `agent_profiles` table is
  declared in this doc + the contract factory; the actual DDL +
  migration step lands under `ft-hac7w.2.cont.handler`.
- Not the README e2e example. Filed as `ft-hac7w.2.cont.readme`.

## Substrate vs wired-pass scope

Same substrate-pass / wired-pass split pattern as ft-2okh0.5,
ft-t9a6q.1 / .2 / .3:

**Substrate-pass (this bead):**
- Contract doc (this file).
- `profile_family_contract()` factory in
  `robot_family_contract.rs` (already exists per ft-hac7w.1).
- 7 schema-level conformance tests (already in
  `tests/robot_family_conformance.rs`).
- State-space proof at
  `crate::robot_profile_state_machine`.

**Wired-pass (named follow-ups):**
- `ft-hac7w.2.cont.handler`: `agent_profiles` schema migration
  + `RobotCommands::Profile` handler replacing the
  `build_ntm_not_implemented_response` site at
  `crates/frankenterm/src/main.rs:23227`.
- `ft-hac7w.2.cont.differential`: ntm differential test using
  `DifferentialHarness` once the real handler is wired.
- `ft-hac7w.2.cont.readme`: README e2e example.

## Cross-references

- `crate::robot_family_contract::profile_family_contract` — the schema-DSL contract.
- `crate::robot_profile_state_machine` — the BFS state-space model.
- `crate::robot_ntm_surface::ProfileCommand` — the wire-format request types.
- `crate::config_profiles` — existing config-side profile management (the wiring follow-up integrates here).
- `crate::session_profiles` — session-level profile types (ft-3681t.2.4).
- `tests/robot_family_conformance.rs` — schema + state-machine conformance harness.
- ft-hac7w (parent epic — Robot Family Closure).
- ft-hac7w.1 (closed — schema infra).
- ft-hac7w.3 (closed — checkpoint family, sibling).
- ft-hac7w.5 (closed — work family, sibling).
- ft-hac7w.6 (closed — fleet family, sibling).
