# Staged Recorder Rollout Plan (`wa-oegrb.8.1`)

Date: 2026-02-14  
Status: Baseline rollout contract for rollout-track execution  
Unblocks: `wa-oegrb.8.2`, `wa-oegrb.8.4`

## Purpose

Define a deterministic rollout path for recorder + search capabilities using explicit
feature/config posture per phase, measurable go/no-go criteria, and fast rollback
rules.

This plan is the control document for rollout-track operations and should be read
with:

- `docs/flight-recorder/validation-gates-wa-oegrb-7-5.md`
- `docs/flight-recorder/security-privacy-validation-wa-oegrb-7-6.md`
- `docs/flight-recorder/recorder-governance-policy.md`
- `docs/flight-recorder/capture-redaction-policy.md`
- `docs/flight-recorder/storage-abstraction-backend-contract.md`
- `docs/flight-recorder/embedding-provider-governance-contract.md`
- `docs/flight-recorder/migration-plan-wa-oegrb-8-2.md`
- `docs/flight-recorder/ops-runbook-wa-oegrb-8-4.md`

## Rollout Control Surface

The following keys/controls are the rollout levers for this plan.

| Control | Default | Use in rollout decisions |
|---|---:|---|
| `[recorder].enabled` | `false` | Master capture enable/disable |
| `[recorder.storage].backend` | `append_log` | Primary durability backend |
| `[recorder.storage].fallback_backend` | `append_log` | One-way failover target |
| `[recorder.storage].startup_policy` | `fail_closed` | Startup behavior under backend failure |
| `[recorder.storage].runtime_error_policy` | `degrade` | Runtime failure behavior |
| `[recorder.redaction].enabled` | `true` | Capture-stage redaction must remain on |
| `[recorder.redaction].allow_unredacted_capture` | `false` | Must stay off in all rollout phases |
| `[recorder.access].require_justification` | `true` | Privileged raw-access guardrail |
| Search mode default | `lexical` | Safe fallback mode for all interfaces |
| `[semantic].enabled` | rollout-gated | Controls semantic lane participation |
| `[semantic].fallback_mode` | `lexical_only` | Enforces lexical-safe degrade behavior |

## Phase Plan

### Phase 0: Off (safe baseline)

**Target posture**
- `recorder.enabled=false`
- search stays lexical default
- semantic path disabled

**Entry criteria**
- none (this is the initial safety posture)

**Exit criteria (go/no-go to Phase 1)**
1. `wa-oegrb.7.5` validation gates passing in CI/nightly.
2. `wa-oegrb.7.6` security/privacy suite passing.
3. Governance policy (`wa-oegrb.8.3`) accepted and referenced by operators.

**Rollback target**
- Already at safest posture (no rollback action required).

### Phase 1: Shadow (write dark, read unchanged)

**Target posture**
- `recorder.enabled=true`
- keep user-facing behavior effectively unchanged (lexical-safe defaults)
- semantic lane disabled or non-authoritative

**Entry criteria**
1. Phase 0 exit criteria met.
2. On-call owner assigned for shadow window.
3. Rollback checklist pre-approved.

**Exit criteria (go/no-go to Phase 2)**
1. No unbounded overload behavior observed; overflow semantics remain explicit
   (`overflow_gaps_emitted` bounded and explainable).
2. No corruption/regression signals from recovery/invariant suites.
3. No policy or redaction violations in sampled recorder payloads/audit records.

**Rollback triggers**
- Any unexpected data-shape/corruption signal.
- Any redaction leak regression.
- Sustained backpressure/overflow trend indicating instability.

### Phase 2: Limited (canary cohorts)

**Target posture**
- recorder remains enabled
- controlled canary exposure for recorder-backed operational workflows
- semantic/hybrid remains optional and lexical-safe

**Entry criteria**
1. Phase 1 stable window completed with zero critical incidents.
2. Canary cohort scope documented (who, where, for how long).
3. Alert routing and ownership confirmed.

**Exit criteria (go/no-go to Phase 3)**
1. CI/nightly gates keep passing throughout canary period.
2. Recovery drill playbooks remain executable without manual invention.
3. Query quality/latency remains within approved canary budget envelope
   (lexical is always available as safe fallback).
4. Incident handling path validated at least once via drill/tabletop.

**Rollback triggers**
- Canary-only incident with unresolved root cause.
- Breach of approved budget envelope for reliability or query behavior.
- Any privileged-access/audit chain integrity failure.

### Phase 3: Default-on (broad enablement)

**Target posture**
- recorder enabled by default
- lexical/semantic/hybrid available under governance and safety constraints
- semantic fallback remains lexical-safe

**Entry criteria**
1. Phase 2 exit criteria met with explicit go/no-go approval.
2. Migration and runbook documents (downstream `wa-oegrb.8.2` and `.8.4`) accepted.

**Steady-state requirements**
1. Rollback remains one-command/config-change accessible.
2. Validation gates stay green; failures block further expansion.
3. Governance controls remain non-optional (no silent downgrade).

**Rollback target**
- Phase 2 posture first, then Phase 0 if needed.

## Global Hard Stop Criteria

At any phase, stop rollout progression and rollback if any of the following occur:

1. Secret/credential leakage in recorder persistence or response path.
2. Audit tamper-evidence failure (`hash_chain_enabled` integrity break).
3. Data-corruption indicators in canonical append/replay path.
4. Inability to execute rollback within the approved operator response window.

## Rollback Procedure (operator-fast path)

1. Set safe config posture:
   - `recorder.enabled=false` (or previous known-good phase posture)
   - keep lexical as default query mode
   - ensure semantic fallback remains `lexical_only`
2. Restart relevant runtime components (`ft stop`, then `ft watch` / service restart).
3. Verify:
   - health/status checks green
   - no fresh critical recorder/audit alerts
   - user-facing search path remains functional in lexical mode
4. Publish rollback notice with cause, timestamp, and next review checkpoint.

## Stakeholder Communication Checklist

Before each phase change:
1. Announce planned change window, target phase, and rollback owner.
2. Share explicit go/no-go evidence packet links.
3. Confirm on-call and decision approver availability.

During change:
1. Post start timestamp and expected verification checkpoints.
2. Post any triggered alerts and mitigation status in-thread.

After change:
1. Publish result (`go` or `rollback`) with concrete criteria outcomes.
2. Record follow-up actions and owners.

## Decision Log Template

Use this template for every phase gate decision:

```md
## Rollout Decision: <phase transition>
- Date/Time (UTC):
- Change owner:
- Approver(s):
- Evidence packet:
  - validation gates report:
  - security/privacy report:
  - governance/audit checks:
  - reliability/latency snapshot:
- Decision: GO | NO-GO | ROLLBACK
- If rollback:
  - trigger(s):
  - rollback target phase:
  - completion timestamp:
- Follow-up actions:
```

## Exit Criteria for `wa-oegrb.8.1`

1. The four-phase rollout posture is explicitly documented with config/flag posture.
2. Every phase has objective entry/exit criteria and rollback triggers.
3. Communication and approval workflow is documented and reusable.
4. Downstream beads (`wa-oegrb.8.2`, `wa-oegrb.8.4`) can execute without rediscovering rollout assumptions.
