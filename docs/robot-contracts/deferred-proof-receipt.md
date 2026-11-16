# Deferred Proof Receipt Contract

`ft.deferred_proof_receipt.v1` is the static receipt shape for proof lanes that
are otherwise ready but cannot be replayed safely yet. The receipt is not proof.
It is a durable queue entry that lets a later operator or robot decide whether a
remote-required command can be replayed without excavating Beads comments.

The schema is `docs/json-schema/ft-deferred-proof-receipt.json`; the retained
fixture corpus lives under `fixtures/deferred-proof-replay/receipt/`.

## Required Semantics

- Material Cargo proof must use `RCH_REQUIRE_REMOTE=1`,
  `RCH_NO_SELF_HEALING=1`, and `rch --no-self-healing exec --`.
- Static verifier receipts may use `static-verifier-v1` and must set
  `material_cargo_required` to `false`.
- A receipt with local fallback evidence is ineligible. `[RCH] local`, `running
  locally`, and similar output are blocker evidence, not proof.
- `owned_paths` must be non-empty. Replay is unsafe without an ownership slice.
- Dirty overlap must fail closed unless the eligibility state is
  `dirty_overlap`.
- Blocked prerequisite beads must produce `prerequisite_blocked`, not
  `eligible`.
- Operator cancellation must set `operator_cancelled` and `replay_allowed:
  false`.
- A receipt where RCH selected a worker but failed remote topology setup before
  Cargo/test must use `rch_admission_state: topology_preflight_failed`, preserve
  `selected_worker` and `rch_job_id` when known, and keep
  `remote_failure_phase: topology_preflight`.
- A `topology_preflight_failed` receipt must never claim `eligibility.state:
  eligible` — topology preflight never reached Cargo, so the proof is not
  replayable and the verifier rejects it as `topology_preflight_not_eligible`.
- `active_project_exclusion` is also an RCH wait state. It means another
  FrankenTerm proof lane currently owns the project admission window, so the
  receipt must stay `wait_rch` with reason `rch.active_project_exclusion` and
  must not suggest worker mutation, build cancellation, or service repair.
- `insufficient_slots` and `telemetry_gap` are RCH wait states. The rich
  receipt state must be preserved, while extractor and queue surfaces project
  both to coarse `blocked_worker_pressure`; each receipt must stay `wait_rch`
  with `proof.remote_required`, the specific `rch.insufficient_slots` or
  `rch.telemetry_gap` reason code, and `replay_allowed: false`.
- Artifact paths are repository-relative and may only point under
  `docs/json-schema/`, `docs/robot-contracts/`,
  `fixtures/deferred-proof-replay/receipt/`, or `tests/e2e/`.

## Eligibility States

| State | Meaning |
| --- | --- |
| `eligible` | The receipt is structurally replayable in a clean ownership state. |
| `wait_rch` | The command is valid but RCH admission or remote topology setup is currently blocked. |
| `dirty_overlap` | Current or captured dirty paths overlap the receipt ownership slice. |
| `stale_command_shape` | The command omits required remote/no-self-healing shape. |
| `prerequisite_blocked` | One or more prerequisite beads remain unresolved. |
| `operator_cancelled` | A human or policy surface cancelled replay. |
| `local_fallback_invalid` | The receipt captured local fallback evidence for a remote-required lane. |

## Golden Corpus

The static verifier freezes valid and invalid examples:

- `remote-required-cargo-proof`
- `selected-worker-topology-preflight-block`
- `active-project-exclusion-block`
- `insufficient-slots-block`
- `telemetry-gap-block`
- `static-only-proof`
- `dirty-overlap-block`
- `prerequisite-bead-block`
- `operator-cancelled-replay`
- `stale-command-shape`
- `missing-no-self-healing`
- `local-fallback-evidence`
- `missing-owned-paths`
- `ambiguous-dirty-overlap`
- `fake-rch-command-shape`
- `env-not-allowlisted`
- `duplicate-env`
- `payload-env-not-allowlisted`
- `target-dir-drift`
- `unsafe-artifact-path`
- `missing-require-remote`
- `operator-cancelled-replayable`
- `prerequisite-bypass`
- `duplicate-env-allowlist`
- `topology-preflight-eligible-bypass`
- `active-project-exclusion-eligible-bypass`
- `insufficient-slots-eligible-bypass`
- `telemetry-gap-eligible-bypass`

Run:

```bash
bash tests/e2e/test_deferred_proof_receipt_contract.sh
```

The verifier is intentionally static. Rust implementation and replay-runner proof
belong to later beads and must use remote-required RCH.
