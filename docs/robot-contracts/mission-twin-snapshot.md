# Mission Twin Snapshot Contract

`ft.mission_twin_snapshot.v1` is the redacted input envelope for mission-twin simulations. It is produced by `ft-u7r37.1` and describes enough Beads, RCH, Agent Mail, git, reservation, and operating-envelope state to run a counterfactual planning pass without storing raw pane text or granting live mutation authority.

The canonical schema is `docs/json-schema/ft-mission-twin-snapshot.json`. The Rust contract types live in `crates/frankenterm-core/src/mission_twin_snapshot.rs`.

## Envelope Rules

- `schema_version` is `1`, `contract_id` is `ft.mission_twin_snapshot.v1`, and `source_bead` is `ft-u7r37.1`.
- All timestamps are positive epoch milliseconds in fields ending with `_at_ms` or `_ms`; ambiguous string timestamps are rejected.
- Every source block must be redacted and must set `raw_pane_content_stored` to `false`.
- Artifact paths are safe repository-relative paths only. Absolute paths, parent traversal, `.git` paths, URL-like paths, backslashes, and trailing slashes are rejected.
- Snapshot output must carry the full forbidden-action set: `agent_mail_service_repair_restart`, `rch_service_repair_restart`, `worker_mutation`, `build_cancellation`, `file_deletion`, `destructive_git`, `local_cargo_proof`, `pane_mutation`, `raw_pane_content_storage`, and `beads_mutation`.

## Source Facts

The envelope records only summarized facts:

- Beads: ready, blocked, and in-progress counts, dependency blockers, owner states, and stale-owner candidates.
- RCH: admission state, healthy and total worker counts, critical-pressure count, admission reasons, and blocked proof lanes.
- Agent Mail: availability state, active-agent summaries, and fallback reason codes.
- Git: current branch, head and remote heads, dirty-path summaries, owned-path overlaps, and deletion-path presence.
- Reservations: active advisory file reservations.
- Operating envelope: verdict, reason codes, and retained source-snapshot artifact paths.

These fields are simulation inputs, not authority. A mission twin may rank options and explain tradeoffs, but it must not repair services, mutate RCH workers, cancel builds, delete files, run local Cargo as proof, send pane input, or mutate Beads as a side effect.

## Golden Fixtures

The retained fixtures live in `fixtures/mission-twin/snapshot/valid/`:

- `healthy.json`
- `agent-mail-red.json`
- `rch-critical-pressure-5.json`
- `active-owner.json`
- `dirty-overlap.json`
- `no-ready-work.json`

Invalid fragments live in `fixtures/mission-twin/snapshot/invalid/fragments.v1.json` and cover raw pane retention, unsafe artifact paths, destructive-action hints, ambiguous timestamps, missing forbidden actions, and unredacted source facts.

Run `tests/e2e/test_mission_twin_snapshot_contract.sh` for the static JSONL verifier. Rust build, test, clippy, and fmt proof remains RCH-only.
