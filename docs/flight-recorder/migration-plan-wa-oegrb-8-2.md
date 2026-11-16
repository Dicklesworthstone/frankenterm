# Recorder Migration Plan (`wa-oegrb.8.2`)

Date: 2026-02-14  
Status: Baseline migration contract for recorder cutover planning  
Depends on: `wa-oegrb.8.1`, `wa-oegrb.6.6`, `wa-oegrb.4.4`

## Purpose

Define a reversible migration path from the existing capture/search behavior to
recorder-first architecture with explicit validation and rollback controls.

This plan is intentionally phase-gated and aligned to rollout controls in
`docs/flight-recorder/rollout-plan-wa-oegrb-8-1.md`.

## Current vs Target Path

| Dimension | Current path (baseline) | Target path (recorder) |
|---|---|---|
| Capture source | Existing watcher/tailer capture stream | Recorder canonical append-log (`ft.recorder.event.v1`) |
| Query serving | Existing FTS-backed retrieval | Recorder-derived lexical/semantic/hybrid projections |
| Source of truth | Existing segment/index surfaces | Canonical recorder log with replay/checkpoints |
| Recovery model | FTS verify/rebuild and DB repair flows | Replay/checkpoint + deterministic reindex/backfill |
| Safety fallback | Lexical path, policy gating, rebuild | Same lexical-safe fallback, plus recorder phase rollback |

## Compatibility Requirements (Non-Negotiable)

| Surface | Requirement | Source |
|---|---|---|
| CLI/Robot/MCP query contract | Preserve response envelope and core search fields (`query`, `results`, `total_hits`, `limit`) | `docs/json-schema/wa-robot-search.json`, `docs/mcp-api-spec.md` |
| Search modes | Preserve `lexical|semantic|hybrid`; keep lexical-safe fallback behavior | `docs/cli-reference.md`, `docs/flight-recorder/rollout-plan-wa-oegrb-8-1.md` |
| Redaction and policy | Maintain policy gating and redaction on query/content fields | `docs/flight-recorder/capture-redaction-policy.md`, `docs/flight-recorder/recorder-governance-policy.md` |
| Ordering/replay semantics | Recorder events remain monotonic and replay-safe across cutover | `docs/flight-recorder/recorder-event-schema.md` |
| Rebuild behavior | Reindex/backfill remains deterministic and resumable | `docs/flight-recorder/recovery-drills-wa-oegrb-7-4.md`, `wa-oegrb.4.4` |

## Migration Phases

### Phase 0: Readiness Gate

Preconditions:
1. Rollout phase controls from `wa-oegrb.8.1` are accepted.
2. Validation gates (`wa-oegrb.7.5`) and recovery drills (`wa-oegrb.7.4`) are green.
3. Ops runbook (`wa-oegrb.8.4`) is available for on-call usage.

Readiness checks:

```bash
scripts/check_recorder_validation_gates.sh
ft search fts verify
ft doctor
```

Exit condition:
- no unresolved critical recorder/security incidents and gates are passing.

### Phase 1: Parallel Run (Shadow)

Goal:
- enable recorder capture path while keeping user-facing retrieval behavior unchanged.

Actions:
1. Enable recorder capture per rollout posture (`recorder.enabled=true`) with lexical-safe defaults.
2. Continue treating existing query behavior as authority for user-facing workflows.
3. Run recorder ingest and projection pipelines in shadow mode.

Validation during parallel run:
- monitor overflow/backpressure metrics (`overflow_gaps_emitted`, `throttle_events`)
- monitor storage tier and degraded state (`health_tier`, `degraded`)
- run periodic `ft search fts verify`

Exit condition:
- stable shadow window with no corruption signals, no policy leaks, and bounded lag.

### Phase 2: Migration Validation and Data Transition

Goal:
- prove recorder-backed data can satisfy existing operational/query expectations.

Actions:
1. Backfill/reindex recorder-derived projections using deterministic tooling (`wa-oegrb.4.4`).
2. Execute replay/recovery drill scenarios and capture artifacts.
3. Run query parity checks on a fixed corpus of operational queries.

Query parity checklist (minimum set):
1. lexical queries for errors/warnings/log tokens
2. pane-filtered queries (`--pane`)
3. time-bounded queries (`--since/--until`)
4. redaction-sensitive queries (no secret leakage)

Required evidence packet:
- validation-gate report JSON
- recovery drill artifacts (`[ARTIFACT][recorder-recovery-drill] ...`)
- query parity worksheet (query set + observed deltas + approval)
- indexing health report before/after reindex

Exit condition:
- migration assumptions are explicit, reproducible, and signed off by change owner + approver.

### Phase 3: Controlled Cutover

Goal:
- promote recorder-backed path to primary while preserving rollback speed.

Actions:
1. Execute cutover in an approved change window with on-call coverage.
2. Keep lexical as default mode during initial cutover window.
3. Keep semantic fallback policy lexical-safe.
4. Announce start/verification milestones in ops channel.

Immediate post-cutover verification:

```bash
ft status
ft search fts verify
ft doctor
ft triage --severity warning
```

Exit condition:
- no unresolved high/urgent alerts during the stabilization window.

### Phase 4: Fallback/Backout (Anytime)

Trigger examples:
- corruption indicators
- policy/redaction regression
- sustained degraded storage state
- unresolved high-severity migration incident

Backout actions:
1. Restore safe rollout posture (disable recorder or revert to previous known-good phase config).
2. Keep lexical serving path operational.
3. Restart services (`ft stop` then `ft watch`) if required.
4. Validate health and publish rollback decision log.

This backout path must remain executable in one operator session without code changes.

## Migration Verification Checklist

Before cutover:
- [ ] validation gates green
- [ ] recovery drills green
- [ ] parity worksheet approved
- [ ] rollback owner assigned and available

During cutover:
- [ ] rollout start notice posted
- [ ] health + indexing verification completed
- [ ] no corruption/leak alerts

After cutover:
- [ ] stabilization window completed
- [ ] outcome logged (`GO`/`ROLLBACK`)
- [ ] follow-up work items captured

## Backout Decision Template

```md
## Migration Backout Decision
- Date/Time (UTC):
- Change owner:
- Trigger(s):
- Impact summary:
- Rollback posture applied:
- Verification steps run:
- Service status after rollback:
- Follow-up bead(s):
```

## Exit Criteria for `wa-oegrb.8.2`

1. Migration phases (parallel run, validation, cutover, fallback) are explicit and reversible.
2. Compatibility requirements are concrete and testable across CLI/Robot/MCP surfaces.
3. Verification tooling/checklists are documented with command-level procedures.
4. Backout plan preserves existing functionality and can be executed rapidly.
