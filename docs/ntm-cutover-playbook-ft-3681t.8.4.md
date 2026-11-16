# ft-3681t.8.4 Staged Cutover Playbook with Rollback Gates

This playbook defines staged migration from NTM workflows to FrankenTerm-native control surfaces with explicit go/no-go and rollback criteria.

## Scope

- Coordinate cutover sequencing for migration track `ft-3681t.8.*`.
- Enforce objective gates using parity/shadow evidence.
- Define rollback triggers and operator actions that avoid partial unsafe states.

## Required Inputs

Before advancing past Stage 0, these inputs must exist:

1. Parity corpus and acceptance matrix (`ft-3681t.8.1`)
   - `docs/ntm-parity-corpus-ft-3681t.8.1.md`
   - `fixtures/e2e/ntm_parity/corpus.v1.json`
   - `fixtures/e2e/ntm_parity/acceptance_matrix.v1.json`
2. Shadow comparator outputs (`ft-3681t.8.2`) with divergence summary.
3. Importer validation outputs (`ft-3681t.8.3`) for sessions/workflows/config.
4. Policy and audit readiness from robot/policy tracks (no ungated mutation paths).

## Stage Model

### Stage 0: Preflight Readiness

Goal: verify prerequisites and freeze cutover candidate scope.

Checks:
- Blocking parity scenarios from `acceptance_matrix.v1.json` are all `pass`.
- Shadow comparator has deterministic run artifacts for agreed scenario set.
- Importers can dry-run on representative NTM snapshots without destructive writes.
- On-call operator + rollback approver roster is explicit.

Outputs:
- `cutover_preflight_summary.json`
- `cutover_candidate_scope.md`

Go/No-Go:
- Go if all checks pass.
- No-Go if any blocking scenario fails or artifacts are incomplete.

### Stage 1: Shadow-Only Verification Window

Goal: run NTM and ft in parallel with ft as observer/comparator only.

Checks:
- Divergence budget within agreed threshold (see Gate Table).
- No critical envelope contract violations (`ok/error.code` schema stability).
- Event ordering and idempotency are stable across repeated runs.

Outputs:
- `shadow_divergence_report.json`
- `shadow_replay_artifacts/`

Go/No-Go:
- Go if divergence is within budget for two consecutive windows.
- No-Go if divergence exceeds budget or unexplained nondeterminism appears.

### Stage 2: Canary Cutover (Low-Risk Cohort)

Goal: route a small, low-risk cohort to ft-native execution.

Checks:
- Canary cohort definition approved (workflows, operators, windows).
- Policy denials/approvals observable and auditable in real time.
- Recovery path tested during canary window (forced rollback drill).

Outputs:
- `canary_runbook_log.jsonl`
- `canary_outcome_summary.json`

Go/No-Go:
- Go if SLO and safety thresholds hold for the full canary window.
- No-Go if rollback trigger threshold is crossed once for critical severity.

### Stage 3: Progressive Expansion

Goal: increase traffic/cohort share in controlled increments.

Checks:
- Each increment passes the same canary gates before further expansion.
- Drift/divergence remains bounded after each increment.
- No backlog of unresolved high-severity incidents.

Outputs:
- `progressive_rollout_timeline.json`
- `increment_gate_evaluations.json`

Go/No-Go:
- Go per increment if all checks pass.
- Halt expansion on first failed gate.

### Stage 4: Default Cutover

Goal: make ft-native path default for migration scope.

Checks:
- Final pre-switch gate review signed by engineering + operations.
- Rollback procedure rehearsed in the last 24 hours.
- Communication and operator runbook updates published.

Outputs:
- `default_cutover_decision_record.md`
- `default_cutover_change_log.json`

Go/No-Go:
- Go only with explicit sign-off and complete artifact bundle.
- Otherwise remain in Stage 3 and remediate gaps.

## Gate Table (Objective Criteria)

| Gate ID | Category | Threshold | Source Artifact |
|---|---|---|---|
| G-01 | Blocking parity scenarios | 100% pass | `assertion_results.json` (8.2) |
| G-02 | High-priority parity scenarios | >= 90% pass, <= 1 intentional delta | `assertion_results.json` (8.2) |
| G-03 | Envelope contract stability | 0 blocking violations | `raw_command_outputs.jsonl` (8.2) |
| G-04 | Divergence budget | <= agreed budget per window | `shadow_divergence_report.json` |
| G-05 | Policy safety | 0 ungated mutation events | policy/audit exports |
| G-06 | Rollback readiness | rollback drill pass in last 24h | `rollback_drill_report.json` |

## Rollback Triggers

Trigger immediate rollback when any of the following occurs:

1. Blocking parity gate failure (`G-01`) after Stage 1.
2. Envelope contract break causing automation incompatibility.
3. Policy enforcement bypass or audit-chain break.
4. Sustained divergence above budget for one full evaluation window.
5. Operator-declared safety incident with unresolved critical severity.

## Rollback Procedure

1. Announce rollback start in operator channel + incident thread.
2. Freeze further rollout increments.
3. Restore last known-good execution path (NTM primary, ft shadow-only).
4. Capture recovery artifacts:
   - `rollback_trigger_event.json`
   - `rollback_actions.jsonl`
   - `post_rollback_health_snapshot.json`
5. Validate health:
   - control-plane responsiveness
   - event ingestion continuity
   - policy/audit integrity
6. Publish incident summary and remediation owners.

## Evidence Bundle Contract

Each stage transition requires a stage artifact bundle:

- `stage_summary.json`
- `gate_evaluation.json`
- `decision_record.md`
- `raw_logs/` (structured, correlation IDs preserved)

All bundles must be stored under:

- `artifacts/migration/cutover/<stage>/<run_id>/`

## Ownership and Approvals

- Migration lead: owns gate evaluation and transition proposal.
- Operations approver: validates safety and rollback readiness.
- Policy approver: verifies mutation-path guardrails and audit integrity.

No stage transition is valid without explicit migration + operations approval records.

## Relationship to Other Beads

- `ft-3681t.8.1`: defines the parity corpus and base matrix used by this playbook.
- `ft-3681t.8.2`: supplies shadow comparator results consumed by gates.
- `ft-3681t.8.3`: ensures import readiness for session/workflow/config migration.
- `ft-3681t.8.6`: formal rehearsal and rollback-drill suite validating this playbook.

## Open Items

1. Finalize divergence budget numeric thresholds with `ft-3681t.8.2` outputs.
2. Add concrete artifact-generation scripts for stage bundle scaffolding.
3. Wire gate evaluation into CI/reporting surfaces for automated go/no-go summaries.
