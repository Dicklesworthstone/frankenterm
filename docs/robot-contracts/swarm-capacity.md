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
next steps are `ft robot swarm-capacity status --format json --level 3`,
`ft doctor --json`, and `ft status --health`, all read-only.

The retained doctor-remediation fixture is
`crates/frankenterm/tests/fixtures/golden_artifacts/swarm_capacity_operator/doctor-remediation.json`.
It pins four operator states:

| State | Meaning | Safe command examples |
| --- | --- | --- |
| `stale_telemetry` | Required capacity evidence is missing, stale, or redacted. | `ft robot swarm-capacity status --format json --level 3`, `ft doctor --json`, `ft status --health` |
| `capacity_refused` | The dry-run plan returns `defer`, `shed`, `capacity.red`, or `capacity.black`. | `ft robot swarm-capacity plan --add-panes 12 --format json --level 3`, `ft robot swarm-capacity explain <decision-id> --format json` |
| `target_class_unavailable` | The capacity envelope or target-class summary is `skipped_not_proven`. | Inspect `docs/attestations/perf/swarm-capacity-envelope.json` and `docs/perf/target-class-hardware.md`; keep high-scale claims blocked. |
| `resource_pressure` | Resource, storage, memory, workload, or RCH pressure is red/black/unavailable. | Preserve the pressure artifact and continue only read-only/status work. |

The verifier
`tests/e2e/test_swarm_capacity_doctor_remediation.sh` checks that the fixture,
this contract, and `docs/operator-runbook.md` agree on state names, command
references, artifact links, and forbidden-action boundaries. None of the
doctor states may recommend service restart, Agent Mail repair, RCH worker
mutation, build cancellation, file deletion, local Cargo proof, pane spawning,
or target-class claim graduation from skipped evidence.
