# Robot Family Contract: `connector`

**Bead:** `ft-pohny` (split from `ft-7h5da.5.11` W4 dead-wire closure).
**Status:** Substrate + wired read/dry-run/mutation slice. `status`, dry-run
plan receipts for every intent, and non-dry-run `install` / `update` /
`enable` / `disable` / `restart` are live through
`PolicyEngine::run_connector_lifecycle_intent` (the single gated production
boundary — emergency kill switch fails closed, `op_counter` telemetry
advances). Non-dry-run `uninstall` / `rollback` are **approval-blocked**: they
return `robot.connector.require_approval` until the robot approval-token
redemption gate lands (same shipped-family precedent as `ft robot checkpoint
rollback`; continuation tracked under `ft-pohny.cont.approval`).

The mesh/dispatch half (`route_connector_operation_through_mesh`) is already
production-wired from the runtime outbound path — this family deliberately
does **not** duplicate it. This family is connector **administration**
(lifecycle), not connector **traffic**.

## State persistence

`ConnectorLifecycleManager` state is process-local. The CLI is one-shot, so
managed-connector state persists in the workspace database's `config` KV
table (baseline schema; present in every DB) under key
`connector_lifecycle_state_v1` as a JSON array of `ManagedConnector` records.

- **Load:** missing key ⇒ genuinely fresh ⇒ empty manager. A present-but-
  unparseable value ⇒ `robot.connector.state_load_failed` and **all intents
  are refused** (fail closed — a corrupt state blob must not silently
  resurrect uninstalled/disabled connectors or forget installed ones).
- **Rehydrate:** `ConnectorLifecycleManager::restore_connectors` — persistence
  rehydration only; does not re-run trust gating (state was admitted when
  first installed) and does not advance `op_counter`.
- **Save:** after every successful non-dry-run mutation, the full snapshot is
  written back (`robot.connector.state_save_failed` if the write fails; the
  envelope then reports `persisted: false` and the operation is considered
  NOT durable — retry semantics below).
- **Concurrency:** load–execute–save; concurrent CLI invocations are
  last-writer-wins on the state blob. Two concurrent mutations of different
  connectors may lose one update. Serializing the blob write behind a
  storage-side transaction is wired-pass follow-up scope; the contract only
  guarantees per-invocation atomicity. `op_counter` is per-process telemetry
  and intentionally not persisted.

## Family overview

| Action | Idempotency | Failure semantics | Side effects |
|---|---|---|---|
| `status` | Idempotent | MustNotPartiallyMutate | (read-only) |
| `install` | Rejected on repeat (`AlreadyInstalled`) | MustNotPartiallyMutate | `config` KV state blob |
| `update` | Not idempotent (version transition) | MustNotPartiallyMutate | `config` KV state blob |
| `enable` | Idempotent-on-target-state via manager transition rules | MustNotPartiallyMutate | `config` KV state blob |
| `disable` | Idempotent-on-target-state via manager transition rules | MustNotPartiallyMutate | `config` KV state blob |
| `restart` | Not idempotent (restart-limit windows apply) | MustNotPartiallyMutate | `config` KV state blob |
| `uninstall` | **approval-blocked** (dry-run only in this slice) | MustNotPartiallyMutate | none in this slice |
| `rollback` | **approval-blocked** (dry-run only in this slice) | MustNotPartiallyMutate | none in this slice |

Every mutating action with `dry_run: true` is side-effect-free: no manager
mutation, no `op_counter` advance, no state-blob write. The dry-run receipt
reports what the non-dry-run call *would* attempt plus
`would_require_approval` for the destructive pair.

## Request/response shapes

All actions return the standard robot envelope; `data` shapes below.

### `status`

Request: optional `connector_id`.

```json
{
  "connectors": [
    {
      "connector_id": "github-events-connector",
      "version": "1.2.3",
      "display_name": "GitHub Events",
      "admin_state": "enabled",
      "runtime_phase": "stopped",
      "trust_level": "…",
      "installed_at_ms": 1714560000000,
      "updated_at_ms": 1714560001000
    }
  ],
  "kill_switch_emergency": false,
  "op_counter": 4,
  "state_persisted": true
}
```

With `connector_id` set, an unknown id returns
`robot.connector.not_found` (typed error, not an empty list).

### Mutating intents (`install`/`update`/`enable`/`disable`/`restart`)

Non-dry-run success `data` mirrors `LifecycleResult`:

```json
{
  "connector_id": "github-events-connector",
  "operation": "enable",
  "dry_run": false,
  "success": true,
  "admin_state": "enabled",
  "runtime_phase": "stopped",
  "detail": "…manager detail line…",
  "at_ms": 1714560002000,
  "persisted": true
}
```

Dry-run receipt `data`:

```json
{
  "connector_id": "github-events-connector",
  "operation": "enable",
  "dry_run": true,
  "current_admin_state": "disabled",
  "currently_installed": true,
  "would_require_approval": false,
  "kill_switch_emergency": false
}
```

`install`/`update` take the manifest by `--manifest-file <path>` (JSON,
`ConnectorManifest` schema — validated by `ConnectorManifest::validate` plus
the manager's trust-policy gate on execution).

## Error codes

| Code | Meaning |
|---|---|
| `robot.connector.kill_switch_active` | Emergency kill switch active; all lifecycle mutations fail closed (dry-run receipts still render, flagged) |
| `robot.connector.not_found` | `status`/intent target id not in managed state |
| `robot.connector.already_installed` | `install` on an existing id |
| `robot.connector.lifecycle_failed` | Manager rejected the transition (invalid transition, restart limit, trust gate, precondition) — `detail` carries the manager error |
| `robot.connector.manifest_invalid` | Manifest file unreadable/unparseable/failed validation |
| `robot.connector.require_approval` | Non-dry-run `uninstall`/`rollback` in this slice |
| `robot.connector.state_load_failed` | Persisted state blob unparseable — fail closed |
| `robot.connector.state_save_failed` | Post-mutation persistence write failed (`persisted: false`) |

## Retry semantics on `state_save_failed`

The manager mutation succeeded in-process but was not persisted. The CLI is
one-shot, so the in-process result is lost on exit; the operator must treat
the operation as NOT applied and re-run it. `install` re-run will succeed
(nothing persisted); `enable`/`disable` re-runs are transition-rule-safe.

## Invariants (enforced by unit tests in `robot_connector_handler.rs`)

1. `status_is_read_only` — `status` never mutates manager, storage, or
   telemetry.
2. `dry_run_pure` — dry-run receipts never advance `op_counter`, never
   mutate the manager, never write state.
3. `kill_switch_fails_closed` — with the emergency kill switch active, every
   non-dry-run intent returns `robot.connector.kill_switch_active` and the
   manager is not perturbed (boundary property inherited from
   `run_connector_lifecycle_intent`; re-asserted at the handler level).
4. `destructive_requires_approval` — non-dry-run `uninstall`/`rollback`
   return `robot.connector.require_approval` without touching the manager.
5. `state_round_trip` — snapshot → persist → rehydrate reproduces managed
   state (`restore_connectors` + `managed_connectors`).
6. `corrupt_state_fails_closed` — an unparseable blob refuses intents with
   `state_load_failed` rather than starting empty.

## Substrate vs wired-pass scope

**This slice (shipped):**
- Contract doc (this file).
- `robot_connector_handler.rs` — typed actions, data envelopes, error codes,
  dry-run receipts, kill-switch + approval gating, unit invariants above.
- `ConnectorLifecycleManager::{managed_connectors, restore_connectors}`.
- `config`-KV persistence helpers + `StorageHandle` async wrappers.
- `RobotCommands::Connector` CLI dispatch in `crates/frankenterm/src/main.rs`.

**Wired-pass follow-ups:**
- Approval-token redemption for `uninstall`/`rollback`
  (`ft-pohny.cont.approval`).
- Schema-DSL `connector_family_contract()` + conformance harness +
  state-space model per the full ft-hac7w pattern.
- Storage-transactional state writes (concurrent-CLI serialization).
- MCP mirror (`wa.connector_*`).

## Cross-references

- `crate::policy::PolicyEngine::run_connector_lifecycle_intent` — the gated
  production boundary (kill-switch fail-closed, telemetry).
- `crate::connector_lifecycle` — manager, intents, transition rules.
- `crate::connector_registry::ConnectorManifest` — install/update payload.
- `docs/robot-contracts/profile.md` — the canonical family exemplar this
  contract follows.
- `ft-7h5da.5.11` — the W4 dead-wire audit that filed this split.
