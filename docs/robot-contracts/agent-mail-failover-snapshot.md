# Robot Family Contract: `agent-mail-failover-snapshot`

**Bead:** `ft-5lsqo.1`
**Status:** static fallback snapshot contract only; no runtime CLI command is
shipped by this document.

## Purpose

The Agent Mail failover snapshot records the coordination state an agent should
use after Agent Mail fails the single allowed retry. It preserves the repo rule:
retry once, do not repair or restart the shared service, and continue from
Beads plus git evidence when that is safe.

The output contract is `ft.agent_mail_failover_snapshot.v1`, defined by the
fixture-local schema at
`fixtures/agent-mail-failover/fallback-snapshot.schema.json`.

## Non-Authority

This contract does not authorize service repair or cleanup. A valid snapshot
must keep every mutation flag false for Agent Mail repair/restart, process
killing, destructive git, file deletion, worker mutation, build cancellation,
and local Cargo proof.

The snapshot is evidence for choosing a next safe action. It is not a claim that
Agent Mail recovered, that RCH is healthy, or that another agent's work may be
reopened without further checks.

## Required Sections

| Section | Meaning |
| --- | --- |
| `agent_mail` | Registration/inbox attempt count, retry limit, failure class, reason codes, and forbidden actions. |
| `beads` | Ready and in-progress counts plus stale-reopen posture. |
| `git` | Dirty-path counts and overlap risk categories. |
| `safety` | Side-effect flags, raw-content policy, and proof disclaimer. |
| `next_actions` | Reviewable next steps for agents in fallback mode. |

## Fixture Coverage

Fixtures live under `fixtures/agent-mail-failover/valid/`:

- `healthy-agent-mail.json`
- `unavailable-after-retry.json`
- `database-recovery-retry-exhausted.json`
- `registration-failed.json`
- `contact-permission-failed.json`
- `empty-in-progress.json`
- `stale-candidate-clean-tree.json`
- `dirty-tracked-overlap.json`
- `untracked-review-required.json`

The fixtures cover both normal Agent Mail availability and degraded fallback
states. Dirty overlap fixtures must recommend `do_not_reopen`; clean stale work
may recommend only a status check before reopen.

## Retry Classifier

`scripts/agent-mail-failover-classifier.sh` is the pure classifier used by
`scripts/swarm-tick.sh --agent-mail-fallback`. It performs no service calls and
no cleanup. It maps the observed startup outcome to the stable `agent_mail`
fields and leaves Beads/git evidence as the fallback coordination surface.

Every degraded startup fixture represents exactly two attempts: the first
attempt plus the single allowed retry. After that, the classifier must emit
`agent_mail.unavailable_after_retry` and `fallback.beads_only`, then stop using
Agent Mail for that session.

| Fixture | Failure class | Required specific reason |
| --- | --- | --- |
| `healthy-agent-mail.json` | none | `agent_mail.available` |
| `database-recovery-retry-exhausted.json` | `database_recovery_notice` | `agent_mail.database_recovery_retry_exhausted` |
| `unavailable-after-retry.json` | `api_unreachable` | `agent_mail.unavailable_after_retry` |
| `empty-in-progress.json` | `database_error` | `agent_mail.unavailable_after_retry` |
| `stale-candidate-clean-tree.json` | `timeout` | `agent_mail.unavailable_after_retry` |
| `registration-failed.json` | `registration_failed` | `agent_mail.registration_failed_after_retry` |
| `contact-permission-failed.json` | `contact_permission_failed` | `agent_mail.contact_permission_failed_after_retry` |
| `dirty-tracked-overlap.json` | `database_recovery_notice` | `agent_mail.database_recovery_retry_exhausted` |
| `untracked-review-required.json` | `unknown` | `agent_mail.unavailable_after_retry` |

Retained classifier cases live at
`fixtures/agent-mail-failover/retry-classifier-cases.json`. The generated
`error_summary` text must explain that Agent Mail registration or inbox setup
was skipped for the session and must not recommend service repair, restart,
process killing, destructive git cleanup, file deletion, worker mutation, or
local Cargo proof.

## No-Service-Action Gate

`fixtures/agent-mail-failover/no-service-action-gate.json` defines the static
gate for fallback artifacts. It scans only retained contract paths and
manifest-listed fixtures, then checks positive and negative guidance strings.
The gate must fail on service repair/restart commands, process-kill guidance,
destructive git cleanup, deletion guidance, worker mutation, build
cancellation, or local Cargo-as-proof language.

The canonical no-service-action contract id is
`ft.agent_mail_failover_no_service_action_gate.v1`. The companion manifest,
fixtures, and verifier live under
`fixtures/agent-mail-no-service-action/manifest.json`,
`fixtures/agent-mail-no-service-action/{positive,negative}/`, and
`tests/e2e/test_agent_mail_no_service_action_contract.sh`; they intentionally
reuse the canonical failover contract id instead of introducing a second
parallel contract name.

## Operator Runbook

The operator sequence and recovery handoff templates live at
`docs/robot-contracts/agent-mail-failover-runbook.md`. The runbook separates
Agent Mail communication outage handling from source defects, dirty-tree
ownership, and RCH fleet pressure.

## Proof Posture

This is a static contract slice. Validation is:

```text
jq empty fixtures/agent-mail-failover/fallback-snapshot.schema.json fixtures/agent-mail-failover/manifest.json fixtures/agent-mail-failover/retry-classifier-cases.json fixtures/agent-mail-failover/no-service-action-gate.json fixtures/agent-mail-failover/valid/*.json
bash tests/e2e/test_agent_mail_failover_snapshot_contract.sh
bash tests/e2e/test_agent_mail_retry_classifier_contract.sh
bash tests/e2e/test_agent_mail_no_service_action_gate.sh
bash tests/e2e/test_agent_mail_no_service_action_contract.sh
bash tests/e2e/test_agent_mail_failover_runbook_contract.sh
git diff --check -- docs/robot-contracts/agent-mail-failover-snapshot.md docs/robot-contracts/agent-mail-failover-runbook.md docs/robot-contracts/agent-mail-no-service-action-gate.md fixtures/agent-mail-failover fixtures/agent-mail-no-service-action scripts/agent-mail-failover-classifier.sh scripts/swarm-tick.sh tests/e2e/test_agent_mail_failover_snapshot_contract.sh tests/e2e/test_agent_mail_retry_classifier_contract.sh tests/e2e/test_agent_mail_no_service_action_gate.sh tests/e2e/test_agent_mail_no_service_action_contract.sh tests/e2e/test_agent_mail_failover_runbook_contract.sh
br dep cycles --json
```

Any later implementation that compiles Rust, executes `ft`, or claims service
recovery must use RCH for Cargo-heavy proof. Local Cargo output is not closeout
evidence.
