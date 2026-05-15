# Robot Swarm Capacity Operator Surface

Contract id: `ft.robot.swarm_capacity.operator.v1`

This surface exposes the capacity planner through read-only robot and MCP
contracts. It never sends input to panes, spawns panes, changes runtime knobs,
updates Beads, or calls external services.

## Robot Commands

| Command | Purpose |
| --- | --- |
| `ft robot swarm-capacity status` | Return the current redacted capacity summary plus doctor guidance. |
| `ft robot swarm-capacity plan --add-panes N` | Dry-run adding `N` panes through workload-admission and resource-budget models. |
| `ft robot swarm-capacity explain <decision-id>` | Explain a redacted capacity decision by stable-id hash or audit record id. |

All commands accept `--level 0..3` for the nested capacity summary and
`--generated-at-ms` for deterministic fixtures.

## MCP Resources

| URI | Shape |
| --- | --- |
| `wa://swarm-capacity/current` | Latest redacted status, doctor guidance, and resource pointers. |
| `wa://swarm-capacity/runs/{run_id}` | Per-run artifact shape for retained status/plan/explain evidence. |

Both MCP resources are `application/json`, read-only, and advertise
`live_mutation_allowed=false`.

## Privacy Contract

Payloads carry `raw_pane_content_stored=false`. The explain command hashes the
query before returning it, so a raw prompt, cookie, or token accidentally passed
as `<decision-id>` is not echoed in the response body.

## Doctor Integration

The `doctor` object is embedded in every response. `status=stale_or_missing_evidence`
means the capacity summary was not attached to the runtime health snapshot; safe
next steps are `ft robot health` and `ft doctor --json`, both read-only.
