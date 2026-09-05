# Robot Family Contract: `kill-switch`

**Beads:** `ft-xxfwy.14` (persisted startup state), `ft-xxfwy.42` (per-effect
freshness and fencing), `ft-xxfwy.42.1` (independent verification).
**Status:** The operator commands persist workspace state. Storage-backed
`PolicyGatedInjector` instances refresh it before each effect under the same
fence used by operator transitions. Live mux proof remains separate from the
SQLite and simulated-pane regressions.

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
{"schema":1,"revision":1,"level":"hard_stop","changed_at_ms":1756800000000,
 "changed_by":"operator","reason":"incident 42","auto_disarm_at_ms":0}
```

`policy.kill_switch_revision_v1` holds a separate unsigned revision anchor.
Operator transitions update both rows in one transaction. Existing schema-1
rows without a revision start at zero.

- **Both keys missing** means never initialized. A missing state with an
  existing anchor, mismatching revisions, or disappearance after observation
  fails closed.
- **Unreadable or corrupt value** (not JSON, an array or scalar, an unknown
  field, a foreign `schema`, an unknown `level`) ⇒ the engine being restored
  is armed to **HardStop** by actor `kill_switch_restore`, and the envelope
  reports `restore: "failed_closed"` with `restore_error`. A corrupt row must
  never silently disarm the switch.
- **Restore never audits.** It rehydrates state. Injector decisions use the
  normal action-audit path; the durable transition records actor, reason,
  timestamp, and revision in the state row.
- **Lapsed auto-disarm** deadlines are applied during restore. `status` is
  read-only and does not write an expired snapshot over a newer trip.
- **Concurrency:** operator transitions reload current state under an exclusive
  workspace fence. The injector holds that fence through dispatch settlement.
  Contention reports `fence_pending` without applying or acknowledging the
  transition. A trip must raise the current persisted tier.
- **Reset:** an explicit operator reset advances the revision and can repair a
  corrupt state blob when the separate revision anchor is valid. If revision
  authority is also corrupt, reset fails without rewriting it. Reset does not
  replay rejected sends.
- **Workspace identity:** the fence canonicalizes symlinks, rejects hard-linked
  database files on Unix, and binds each injector to its initial workspace.
  Replacing or renaming database/lock files while owners run is unsupported;
  the workspace directory must remain under the operator's control.

Restore points (all through
`frankenterm_core::policy_kill_switch_state`): `ft robot kill-switch`,
`ft robot connector`, and the other one-shot CLI engines listed in the
module docs as they are wired. The watcher restores at startup and its
storage-backed injector refreshes again before every send/control effect.

## Commands

| Command | Effect | Envelope `data` |
|---|---|---|
| `ft robot kill-switch status` | Restore into a fresh engine and report | `action: "status"` + state fields |
| `ft robot kill-switch trip --level {soft-stop,hard-stop,emergency-halt} --reason R [--by A]` | Reload under the workspace fence, raise the tier, atomically persist | `action: "trip"`, `persisted: true`, revision and fence scope |
| `ft robot kill-switch reset [--by A]` | Reload under the workspace fence, disarm, atomically persist | `action: "reset"`, `persisted: true`, revision and fence scope |

State fields in every envelope: `level` (`disarmed`, `soft_stop`,
`hard_stop`, `emergency_halt`), `changed_at_ms`, `changed_by`, `reason`,
`auto_disarm_at_ms`, `persisted`, `state_key`. Status additionally reports
`restore` (`absent`, `restored`, `failed_closed`) and `restore_error`.
Successful transitions report `revision`, `fenced_owner`, and
`pre_admitted_remote_effects`. The latter remains `not_proven_settled`:
acknowledging persisted admission control does not assert cancellation of
previously admitted remote work. `--by` is an audit label, not authentication.

## Error codes

| Code | Meaning |
|---|---|
| `robot.kill_switch.state_save_failed` | Transition not confirmed: non-increasing tier, corrupt revision authority, exhausted revision, or persistence failure; inspect status before retrying |
| `robot.kill_switch.state_load_failed` | Reported in `restore_error`; the engine ran under HardStop |
| `robot.kill_switch.state_corrupt` | Reported in `restore_error`; the engine ran under HardStop |
| `robot.kill_switch.fence_pending` | Another integrated effect/transition owns the fence; this transition was not applied |
| `robot.kill_switch.fence_failed` | Workspace identity or locking authority is unavailable; no transition was applied |
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

Connector lifecycle administration has a separate production boundary using
the same graduated levels: SoftStop allows administrative drain; HardStop and
EmergencyHalt block all lifecycle mutations before manager state or counters
change. There is no special disable bypass at HardStop. An authorized operator
must reset first, then request a fresh recovery action. Read-only status and
dry-run receipts do not mutate lifecycle state.

Proof: `policy::tests::killswitch_*` and
`robot_connector_handler::tests::kill_switch_fails_closed_with_typed_code`
(tier gate), `policy_kill_switch_state::tests` (persistence, fail-closed
restore, backend round trip), and the committed matrix
`docs/attestations/proofs/killswitch-tier-enforcement.json` (4 tiers × every
`ActionKind` × pane/no-pane), which
`crates/frankenterm-core/tests/killswitch_tier_matrix.rs` regenerates and
compares against the live engine.

## No-claim

A successful transition fences subsequent admissions only for the owner
identified by its receipt. This is not proof that every connector, file, exec,
mission, or legacy process participates, nor that already-admitted remote
effects were cancelled or settled. The separate-process module regressions use
real SQLite with a simulated pane; `.42.1` also requires the isolated real mux
and watcher path before claiming live enforcement.
