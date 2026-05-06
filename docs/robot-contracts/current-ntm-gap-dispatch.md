# Current Robot NTM-Gap Dispatch Manifest

**Bead:** `ft-bs9uh.1`

This file records the live CLI dispatch shape for Robot Mode families that
were once part of the generic NTM-gap fallback. No live family currently routes
through `build_ntm_not_implemented_response`; the table below records the
native backends that replaced it.
It complements the per-family contract docs, which describe the target
native semantics and state-machine proofs.

## Current Status

`profile` is not part of this gap: list/show/validate and dry-run apply
dispatch through `robot_profile_handler`; non-dry-run apply returns a typed
profile spawn error until daemon-side pane spawning lands.

The checkpoint, context, work, and live fleet CLI shapes have graduated from
the NTM-gap fallback:

| Family | CLI actions currently parsed | Current backend |
|---|---|---|
| `checkpoint` | `save`, `list`, `show`, `delete`, `rollback` | native snapshot/session adapter; rollback mutating execution is approval-blocked unless `--dry-run` is used |
| `context` | `status`, `rotate`, `history` | native SQLite `pane_contexts` / `context_rotations` registry; rotation receipts are durable, support optional idempotency-key replay, and store metadata without raw conversation content |
| `work` | `claim`, `release`, `complete`, `list`, `ready`, `assign` | native SQLite `work_claims` queue; claims/assignments are serialized per item and completion is durable |
| `fleet` | `status`, `scale`, `rebalance`, `agents` | native agent-inventory/work-queue read paths for `status` and `agents`; mutating `scale`/`rebalance` parse natively and return typed `robot.fleet.capability_unavailable` until daemon-side mutation is wired |

## Harness Contract

The integration harness at
`crates/frankenterm/tests/robot_ntm_gap_contract_tests.rs` owns the current
native-dispatch manifest. It asserts that each listed action parses, emits a
JSON robot envelope, and does not return the retired
`robot.not_implemented` fallback. The native assertion intentionally does not
require success, because a real backend can still return typed errors for
missing state, unavailable daemons, or denied policy.

The cross-surface Robot/MCP golden matrix lives at
`crates/frankenterm-core/tests/golden_robot_envelope/control_plane_golden_matrix.json`.
`crates/frankenterm-core/tests/control_plane_golden_matrix.rs` validates that
matrix for required families, scenarios, checked-in fixture/schema/doc
references, and proof commands. Use it when updating README or robot-contract
examples so healthy, degraded, blocked, policy-required, unsupported, and
capability-unavailable envelopes stay tied to executable proof lanes.

Re-run the live dispatch proof through RCH:

```bash
rch exec -- env CARGO_TARGET_DIR=/tmp/ft-bs9uh6-ntm-gap \
  cargo test -p frankenterm --test robot_ntm_gap_contract_tests \
  robot_checkpoint_context_work_fleet_dispatch_matches_manifest -- --nocapture
```
