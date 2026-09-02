# Robot Family Contract: `kill-switch`

**Bead:** `ft-xxfwy.14` (closes `ft-l59nq`, reality-check G56).
**Status:** Wired. `ft robot kill-switch status|trip|reset` reads and writes
one persisted operator kill switch, and every production `PolicyEngine`
restores it at construction.

## Why this exists

`PolicyEngine` is process-local. Every `ft` invocation and the watcher's
auto-handler build their own engine, so the graduated SoftStop / HardStop /
EmergencyHalt gate in `PolicyEngine::evaluate_authorization` (fix
`f8c674376`, June 2026) had no production trigger: nothing outside a unit
test ever tripped it, and a tier tripped in one process was invisible to every
other. `ft doctor` labelled its policy rows `process-local` for that reason.

## State persistence

The kill switch persists in the workspace database's generic `config` KV
table (baseline schema; present in every DB) under key
`policy.kill_switch_v1` as one JSON object:

```json
{"schema":1,"level":"hard_stop","changed_at_ms":1756800000000,
 "changed_by":"operator","reason":"incident 42","auto_disarm_at_ms":0}
```

- **Missing key** ⇒ never armed ⇒ the engine keeps `disarmed`.
- **Unreadable or corrupt value** (not JSON, an array or scalar, an unknown
  field, a foreign `schema`, an unknown `level`) ⇒ the engine being restored
  is armed to **HardStop** by actor `kill_switch_restore`, and the envelope
  reports `restore: "failed_closed"` with `restore_error`. A corrupt row must
  never silently disarm the switch.
- **Restore never audits.** `PolicyEngine::restore_kill_switch` is
  persistence rehydration; the trip or reset was audited in the process that
  wrote the row.
- **Lapsed auto-disarm** deadlines are applied during restore; `status`
  writes the disarmed state back so every process agrees.
- **Concurrency:** load, act, save; concurrent invocations are last-writer
  wins on the single row. A `trip` is refused (`robot.kill_switch.trip_rejected`)
  when it would not raise the tier; `reset` always disarms.

Restore points (all through
`frankenterm_core::policy_kill_switch_state`): `ft robot kill-switch`,
`ft robot connector`, and the other one-shot CLI engines listed in the
module docs as they are wired. The watcher restores the tier when it starts.

## Commands

| Command | Effect | Envelope `data` |
|---|---|---|
| `ft robot kill-switch status` | Restore into a fresh engine and report | `action: "status"` + state fields |
| `ft robot kill-switch trip --level {soft-stop,hard-stop,emergency-halt} --reason R [--by A]` | `PolicyEngine::trip_kill_switch` (audit chain + compliance counter), then persist | `action: "trip"`, `persisted: true` |
| `ft robot kill-switch reset [--by A]` | Disarm, then persist | `action: "reset"`, `persisted: true` |

State fields in every envelope: `level` (`disarmed`, `soft_stop`,
`hard_stop`, `emergency_halt`), `changed_at_ms`, `changed_by`, `reason`,
`auto_disarm_at_ms`, `persisted`, `restore` (`absent`, `restored`,
`failed_closed`), `restore_error` (string or null), `state_key`.

## Error codes

| Code | Meaning |
|---|---|
| `robot.kill_switch.trip_rejected` | Requested tier is not above the current tier; reset first |
| `robot.kill_switch.state_save_failed` | The tier changed in this process but the row write failed; other processes still see the previous state |
| `robot.kill_switch.state_load_failed` | Reported in `restore_error`; the engine ran under HardStop |
| `robot.kill_switch.state_corrupt` | Reported in `restore_error`; the engine ran under HardStop |
| `config` / `storage` family codes | Workspace layout or DB open failed before any policy work |

## Tier semantics (unchanged, `policy_quarantine::KillSwitchLevel`)

| Tier | Workflow launches (`WorkflowRun`, `ConnectorTriggerWorkflow`) | Every other non-read-only action | Read-only actions (`ReadOutput`, `SearchOutput`, `Activate`) |
|---|---|---|---|
| `disarmed` | allowed | allowed | allowed |
| `soft_stop` | **blocked** (pane-less `WorkflowRun` included) | allowed | allowed |
| `hard_stop` | blocked | **blocked** | allowed |
| `emergency_halt` | blocked | blocked | **blocked** |

The kill switch never depends on a pane id. `ActionKind::is_read_only` and
`ActionKind::is_workflow_launch` are the classifications the gate uses;
`Activate` (focus a pane) is read-only by that classification.

Proof: `policy::tests::killswitch_*` and
`robot_connector_handler::tests::kill_switch_fails_closed_with_typed_code`
(tier gate), `policy_kill_switch_state::tests` (persistence, fail-closed
restore, backend round trip), and the committed matrix
`docs/attestations/proofs/killswitch-tier-enforcement.json` (4 tiers × every
`ActionKind` × pane/no-pane), which
`crates/frankenterm-core/tests/killswitch_tier_matrix.rs` regenerates and
compares against the live engine.

## No-claim

Persistence makes the tier visible to every *new* engine. A long-running
watcher restores it at start and does not poll for later changes; in-flight
actions are not interrupted by a trip from another process.
