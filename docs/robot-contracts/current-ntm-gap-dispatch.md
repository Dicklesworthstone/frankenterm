# Current Robot NTM-Gap Dispatch Manifest

**Bead:** `ft-bs9uh.1`

This file records the live CLI dispatch shape for the remaining Robot Mode
families that still route through `build_ntm_not_implemented_response`.
It complements the per-family contract docs, which describe the target
native semantics and state-machine proofs.

## Current Status

`profile` is not part of this gap: list/show/validate and dry-run apply
dispatch through `robot_profile_handler`; non-dry-run apply returns a typed
profile spawn error until daemon-side pane spawning lands.

The current NTM-gap families are:

| Family | CLI actions currently parsed | Current backend |
|---|---|---|
| `checkpoint` | `save`, `list`, `show`, `delete`, `rollback` | structured `robot.not_implemented` fallback |
| `context` | `status`, `rotate`, `history` | structured `robot.not_implemented` fallback |
| `work` | `claim`, `release`, `complete`, `list`, `ready`, `assign` | structured `robot.not_implemented` fallback |
| `fleet` | `status`, `scale`, `rebalance`, `agents` | structured `robot.not_implemented` fallback |

## Harness Contract

The integration harness at
`crates/frankenterm/tests/robot_ntm_gap_contract_tests.rs` owns the current
fallback/native manifest. For an action marked as fallback, it asserts:

- the command parses and emits a JSON robot envelope;
- `ok == false`;
- `error_code == "robot.not_implemented"`;
- `data.family` and `data.action` match the parsed CLI action;
- `data.is_mutation` matches the NTM surface classification;
- `data.ntm_equivalence.ntm_commands` is non-empty.

When an implementation bead wires a native backend for an action, update the
same harness entry from fallback to native in the implementation commit. The
native assertion intentionally does not require success, because a real
backend can still return typed errors for missing state, unavailable daemons,
or denied policy. It only forbids `robot.not_implemented`, making the gap
closure falsifiable without weakening runtime error honesty.
