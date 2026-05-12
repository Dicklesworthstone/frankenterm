# Formal Spec Conventions

`docs/specs` contains TLA+ specifications that back formal-method proof lanes.
Every spec in this directory must be directly runnable by TLC and traceable back
to the Rust model or production code it abstracts.

## Category 6 Doctrine

Every subsystem with an externally visible atomicity, serializability,
ordering, deduplication, or scheduler-liveness invariant ships a TLA+ spec under
`docs/specs/<subsystem>.tla`. Each spec has a sibling TLC config, a mapping doc,
and a Rust model or implementation-side test that cross-checks the same
invariants. If the Rust cross-check is intentionally an abstraction rather than
the production type, the mapping doc must say that directly and cite the
follow-on bead that will connect it to production behavior.

New subsystems with category-6 invariants should not graduate from a planning
or contract bead without either:

- a complete `.tla` / `.cfg` / `-mapping.md` trio in this directory; or
- a child bead that names the missing spec, the invariant, the Rust surface, and
  the release-bundle proof slot it will feed.

When a gap closes, update the coverage inventory below in the same commit that
adds the spec files.

## Coverage Inventory

`ft-tf6g3.18.1` owns this inventory. Rows marked `covered` have a TLA+ spec,
TLC config, and mapping doc in `docs/specs`; rows marked `gap` are tracked by
the listed child bead.

| Subsystem | Category-6 invariant | Spec artifact | Rust cross-check surface | Status | Tracking |
|-----------|----------------------|---------------|--------------------------|--------|----------|
| TX kill-switch | Prepare / commit / compensate cannot strand a transaction when the kill switch flips. | `tx-killswitch.tla` | `crates/frankenterm-core/src/tx_killswitch_model.rs` | covered | `ft-tf6g3.12` |
| Robot work | Work claim, release, completion, and crash-restart transitions remain single-holder and durable. | `robot-work.tla` | `crates/frankenterm-core/src/robot_work_state_machine.rs` | covered | `ft-tf6g3.13` |
| Robot checkpoint | Snapshot, restore, rollback, and completion checkpoints stay atomic across failures. | `robot-checkpoint.tla` | `crates/frankenterm-core/src/robot_checkpoint_state_machine.rs` | covered | existing spec substrate |
| Robot fleet | Fleet scale/rebalance lifecycle transitions stay serializable and terminal states remain stable. | `robot-fleet.tla` | `crates/frankenterm-core/src/robot_fleet_state_machine.rs` | covered | existing spec substrate |
| Wire dedup | Reordered, duplicated, and dropped distributed envelopes converge to one observable session frontier. | `wire-dedup.tla` | `crates/frankenterm-core/src/wire_dedup_model.rs` | covered | existing spec substrate |
| `runtime_async` cancel semantics | Cancelled waits across runtime primitives terminate through the `Cx` path without leaking permits, joins, or messages. | gap | `crates/frankenterm-core/src/runtime_async.rs` | gap | `ft-tf6g3.18.2` |
| Durable state checkpoint / rollback | Checkpoint creation, pre-rollback checkpointing, rollback validation, and failed rollback remain atomic. | gap | `crates/frankenterm-core/src/durable_state.rs` | gap | `ft-tf6g3.18.3` |
| Mux session reentry | Reentrant subscriber/session callbacks cannot double-register panes, leak subscribers, or drop terminal session events. | gap | `frankenterm/mux/src/`, `crates/frankenterm-core/src/headless_mux_server.rs` | gap | `ft-tf6g3.18.4` |
| Blocker-radar source merge | Degraded, stale, conflicting, or unavailable coordination sources always fail closed before work is claimable. | gap | `crates/frankenterm-core/src/blocker_radar.rs` | gap | `ft-tf6g3.18.5` |
| Herd-wave admission control | Synchronized cohorts produce bounded stagger plans, priority protection, cooldown behavior, and missing-telemetry fail-closed decisions. | gap | `crates/frankenterm-core/src/swarm_scheduler.rs` | gap | `ft-tf6g3.18.6` |
| Capture-fairness scheduler liveness | Eligible panes cannot starve under documented priority, low-tier floor, budget, and shutdown assumptions. | gap | `docs/capture-fairness-slo-contract.md`, runtime/tailer scheduler code | gap | `ft-tf6g3.18.7` |

Each gap row closes only when its child bead adds the spec trio, records a TLC
summary with state-space size, wires the release-bundle slot under
`proofs/<subsystem>.json`, and keeps `scripts/check-spec-conventions.sh`
passing.

## File Naming

- Use one kebab-case file per subsystem: `subsystem-contract.tla`.
- The TLA+ module name must be PascalCase and must match the file topic.
- Keep the sibling TLC configuration at `docs/specs/<spec>.cfg`.
- Keep the Rust mapping document at `docs/specs/<spec>-mapping.md`.

## Required TLA+ Sections

Every `.tla` file must contain these sections or definitions:

- State variables: a `VARIABLES` block and a `vars == <<...>>` tuple.
- Initial state: `Init ==`.
- Next-state relation: `Next ==`.
- Full behavior: `Spec == Init /\ [][Next]_vars`.
- Safety invariants: named invariants plus a `SafetyInvariants ==` block.
- Liveness/progress block: temporal properties, fairness notes, convergence, or
  an explicit reason the spec is safety-only.
- TLC run note: a `Run with TLC` comment that points operators at the wrapper.

## Mapping Documents

Each `docs/specs/<spec>-mapping.md` must include these headings:

- `## Rust Correspondence`
- `## Action Mapping`
- `## Invariant Mapping`
- `## TLC Configuration`

The mapping must cite concrete Rust paths with line numbers. Line numbers are a
review aid rather than a semantic dependency; update them when the cited model
or production file moves enough to make the reference misleading.

## TLC Configurations

Each `.cfg` file must:

- Use `SPECIFICATION Spec`.
- Set deterministic constants in a `CONSTANTS` block.
- Check `INVARIANT SafetyInvariants` at minimum.
- Avoid placeholders such as `TODO`, `FIXME`, or `<...>`.

Keep constants intentionally small. The default config is for repeatable smoke
and coverage accounting; larger state-space runs should use a separate artifact
path and record their constants in the bead evidence.

## Scripts

- `scripts/check-spec-conventions.sh` validates this directory.
- `scripts/run-tlc.sh docs/specs/<spec>.tla` runs TLC with the sibling `.cfg`
  and writes a normalized JSON summary.

`scripts/run-tlc.sh` emits the G35 substrate fields:

```json
{
  "state-count": 0,
  "distinct-state-count": 0,
  "time-budget": {"seconds": 300, "enforced": true, "timed-out": false},
  "invariant-results": []
}
```
