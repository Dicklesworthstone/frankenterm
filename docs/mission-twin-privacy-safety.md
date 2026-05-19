# Mission Twin Privacy And Safety Policy

Status: static v1 policy for `ft-u7r37.7`.

The mission twin is a simulation surface. It can explain what a blocked swarm
would do under counterfactual states, but it never grants live authority to
mutate panes, Beads, Agent Mail, RCH workers, services, files, or git state.
Every mission-twin output must carry the policy artifact reference and the full
forbidden-action list from `ft.mission_twin_safety_policy.v1`.

## Retention

Snapshots, replay logs, plan outputs, and evidence artifacts are retained as
metadata-only records. They may include redacted Beads ids, RCH reason codes,
Agent Mail availability state, reservation summaries, git path summaries, and
operating-envelope identifiers. They must not store raw pane text, mail bodies,
secret material, shell output bodies, or unredacted command payloads.

Artifact paths are repository-relative audit pointers. They must not be absolute,
parent-relative, `.git` internals, backslash paths, or generated cleanup
instructions. A retained mission-twin artifact proves only what the simulator
observed; it is not approval to run cleanup, restart services, or claim work.

## Forbidden Actions

The mission twin must carry these forbidden actions in every output:

- `agent_mail_service_repair_restart`
- `rch_service_repair_restart`
- `worker_mutation`
- `build_cancellation`
- `file_deletion`
- `destructive_git`
- `local_cargo_proof`
- `pane_mutation`
- `raw_pane_content_storage`
- `beads_mutation`

## Fail-Closed Behavior

Missing, stale, contradictory, or unredacted inputs lower confidence and produce
simulated recommendations only. They do not become permission to mutate live
state. Unredacted inputs are rejected before replay. Contradictory inputs require
operator review. Missing or stale proof signals keep the proof lane blocked or
unknown rather than upgrading it to passed.

The static verifier for this policy is
`tests/e2e/test_mission_twin_safety_policy.sh`.
