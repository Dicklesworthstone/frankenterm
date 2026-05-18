# Robot Contract: `agent-mail-failover-no-service-action-gate`

Bead: `ft-5lsqo.4`

Status: companion static E2E gate for the canonical Agent Mail failover
no-service-action contract. No runtime command is shipped by this document.

## Purpose

This gate proves that Agent Mail fallback artifacts do not emit guidance to
repair or restart the shared Agent Mail service, kill shared processes, clean
git destructively, delete files, mutate RCH workers, cancel builds, or treat
local Cargo as proof.

The canonical contract id is
`ft.agent_mail_failover_no_service_action_gate.v1`, defined by
`fixtures/agent-mail-failover/no-service-action-gate.json`. The companion
manifest is `fixtures/agent-mail-no-service-action/manifest.json`, and the
companion verifier is
`tests/e2e/test_agent_mail_no_service_action_contract.sh`.

## Checked Surfaces

The verifier checks:

- Agent Mail failover snapshot docs and fixtures
- stale-reopen policy docs and fixtures
- retry-classifier cases and classifier output
- production fallback scripts used by the static contract
- positive fixture guidance that must pass
- negative fixture guidance that must be rejected

The gate allows forbidden action identifiers when they are stored as explicit
deny-list data such as `forbidden_actions`. It rejects those phrases when they
appear as generated recommendations, next actions, summaries, or guidance.

## Required Log

The verifier emits one JSON line with:

- `contract_id`
- `checked_files`
- `positive_cases`
- `negative_cases`
- `forbidden_pattern_count`
- `verdict`

## Static Proof

Run:

```bash
jq empty fixtures/agent-mail-failover/no-service-action-gate.json \
  fixtures/agent-mail-no-service-action/manifest.json \
  fixtures/agent-mail-no-service-action/positive/*.json \
  fixtures/agent-mail-no-service-action/negative/*.json
bash -n tests/e2e/test_agent_mail_no_service_action_contract.sh
bash tests/e2e/test_agent_mail_no_service_action_contract.sh
git diff --check -- fixtures/agent-mail-failover/no-service-action-gate.json \
  docs/robot-contracts/agent-mail-no-service-action-gate.md \
  fixtures/agent-mail-no-service-action \
  tests/e2e/test_agent_mail_no_service_action_contract.sh \
  .beads/issues.jsonl
br dep cycles --json
```

No Cargo or RCH proof is required for this static verifier slice.
