# ft-3681t Execution Plan: Critical Path, Tracks, and Success Gates

> **Bead**: ft-3681t.1.4
> **Author**: RusticDeer
> **Date**: 2026-03-02
> **Architecture ref**: `docs/ft-3681t-convergence-architecture.md` (ft-3681t.1.3)

## 1. Program Summary

FrankenTerm convergence replaces NTM (Named Tmux Manager) and integrates
flywheel_connectors as native subsystems. The program spans 9 epics and 66 leaf
tasks across native mux, swarm orchestration, robot APIs, connector fabric,
policy/governance, observability, migration, and operator UX.

**Current state (historical snapshot)**: The paragraph above reflects the
original 2026-03-02 planning snapshot that this document was written against.

**Current reality check (2026-03-12, code-grounded)**:
- Tracker status from raw `.beads/issues.jsonl` now shows epics
  `ft-3681t.1`, `ft-3681t.2`, `ft-3681t.5`, `ft-3681t.6`, `ft-3681t.8`, and
  `ft-3681t.9` as CLOSED; `ft-3681t.3`, `ft-3681t.4`, and `ft-3681t.7` remain
  OPEN as top-level rollups.
- The swarm-orchestration implementation surface is no longer theoretical:
  `frankenterm-core` exports `fleet_launcher`, `swarm_work_queue`,
  `swarm_scheduler`, `mission_loop`, `mission_agent_mail`, and
  `swarm_pipeline` from `src/lib.rs`.
- Swarm validation coverage is also present in-tree today via
  `crates/frankenterm-core/tests/swarm_orchestration_integration.rs`,
  `crates/frankenterm-core/tests/swarm_simulation_regression.rs`,
  `crates/frankenterm-core/tests/proptest_mission_loop.rs`, and
  `crates/frankenterm-core/tests/proptest_swarm_pipeline.rs`.
- Treat the tables below as the original sequencing model, then cross-check
  against current bead state before using them for active claim/priority
  decisions.

## 2. Execution Tracks

The program is organized into 5 execution tracks. Tracks 1-3 are sequential
gates; Tracks 4-5 run in parallel once their inputs are satisfied.

### Track 1: Foundation (Must-Complete-First)

**Goal**: Establish the native mux substrate that everything else builds on.

| Phase | Bead | Title | Status | Assignee | Unblocks |
|-------|------|-------|--------|----------|----------|
| 1a | ft-3681t.1.1 | NTM capability census | IN_PROGRESS | PinkOtter | .1.5 |
| 1a | ft-3681t.1.2 | FCP capability inventory | IN_PROGRESS | PinkOtter | .1.5 |
| 1b | ft-3681t.1.3 | Convergence architecture spec | CLOSED | CopperCompass | .2.1, .5.1, .6.1, .1.4 |
| 1c | ft-3681t.1.4 | Execution tracks (this doc) | IN_PROGRESS | RusticDeer | .1.5 |
| 1d | ft-3681t.1.5 | Traceability verification | OPEN | -- | (quality gate) |
| **2a** | **ft-3681t.2.1** | **Core lifecycle engine** | **IN_PROGRESS** | **CopperCompass** | **.2.2-.2.7, .3.1, .5.1, .9.2** |
| 2b | ft-3681t.2.3 | Command transport primitives | OPEN | -- | .4.1, .6.1 |
| 2b | ft-3681t.2.5 | Durable state + snapshots | OPEN | -- | .2.6, .3.3, .4.2, .9.3 |
| 2b | ft-3681t.2.2 | Topology/layout orchestration | OPEN | -- | .3.2 |
| 2b | ft-3681t.2.4 | Session profile/template engine | OPEN | -- | .3.1 |
| 2c | ft-3681t.2.6 | Headless/federated mux server | OPEN | -- | .5.8, .7.1 |
| 2d | ft-3681t.2.7 | Native mux validation harness | OPEN | -- | (quality gate) |

**Critical gate**: ft-3681t.2.1 is the single most important bead in the
program. It unblocks 6 direct dependents and indirectly gates every downstream
epic. Prioritize completion above all else.

**Parallelism after 2.1**: Beads 2.2, 2.3, 2.4, 2.5 can all start
simultaneously once 2.1 is done.

### Track 2: Core Platform (Parallel Pillars)

**Goal**: Build the orchestration runtime and connector fabric in parallel,
since they share the same mux substrate dependency.

#### Pillar A: Swarm Orchestration (ft-3681t.3)

| Phase | Bead | Title | Needs | Unblocks |
|-------|------|-------|-------|----------|
| 3a | ft-3681t.3.1 | Fleet launcher | .2.1, .2.4 | .3.2, .3.4, .3.7, .4.1, .9.2 |
| 3a | ft-3681t.3.5 | Workflow/pipeline runtime | .2.5 | .3.6, .7.1 |
| 3a | ft-3681t.3.3 | Dependency-aware work queue | .2.5 | .3.6 |
| 3b | ft-3681t.3.2 | Scheduler/rebalancer | .3.1, .2.2 | .3.6 |
| 3b | ft-3681t.3.4 | Agent Mail coordination kernel | .3.1 | .3.6 |
| 3b | ft-3681t.3.7 | Conflict detection/handoff | .3.1 | .3.6 |
| 3c | ft-3681t.3.6 | Swarm simulation/regression suite | all above | (quality gate) |

**Implementation note (2026-03-12)**: the `ft-3681t.3.1` through
`ft-3681t.3.7` child beads are all CLOSED in `.beads/issues.jsonl`, and the
corresponding code/test surfaces are present in the current tree. The parent
epic `ft-3681t.3` still appears OPEN, so use the child-bead state plus code
anchors above as the source of truth for what has already landed.

#### Pillar B: Connector Fabric (ft-3681t.5)

| Phase | Bead | Title | Needs | Unblocks |
|-------|------|-------|-------|----------|
| 5a | ft-3681t.5.1 | FCP host runtime embedding | .2.1 | .5.2-.5.10, .5.12 |
| 5b | ft-3681t.5.2 | Registry + signed packages | .5.1 | .5.4 |
| 5b | ft-3681t.5.3 | Sandbox zone execution | .5.1, .6.1 | .5.4, .5.5, .6.2 |
| 5b | ft-3681t.5.6 | Outbound bridge | .5.1 | .5.9 |
| 5b | ft-3681t.5.7 | Inbound bridge | .5.1 | .5.9 |
| 5b | ft-3681t.5.10 | Connector SDK/devkit | .5.1 | .5.13 |
| 5b | ft-3681t.5.12 | Event/data model + schema | .5.1 | .7.1 |
| 5c | ft-3681t.5.4 | Lifecycle manager | .5.2, .5.3, .6.1 | .5.9 |
| 5c | ft-3681t.5.5 | Credential broker | .5.3, .6.1 | .5.9 |
| 5c | ft-3681t.5.8 | Mesh federation | .5.1, .2.6 | .5.9 |
| 5d | ft-3681t.5.9 | Tier-1 connectors | .5.4-.5.8 | .5.11, .5.14, .5.15 |
| 5e | ft-3681t.5.11 | Rate-limit/quota governor | .5.9 | .5.13 |
| 5e | ft-3681t.5.14 | Data classification/redaction | .5.9 | .7.1 |
| 5e | ft-3681t.5.15 | Circuit-breaker/dead-letter | .5.9 | .5.13 |
| 5f | ft-3681t.5.13 | Connector integration testbed | all above | (quality gate) |

### Track 3: Control Surfaces (After Core Platform)

**Goal**: Build the robot API, policy engine, and observability that rely on
the core platform being functional.

#### Pillar C: Robot API (ft-3681t.4)

| Phase | Bead | Title | Needs | Unblocks |
|-------|------|-------|-------|----------|
| 4a | ft-3681t.4.1 | ft robot NTM-superset commands | .2.3, .3.1 | .4.2-.4.6, .9.2 |
| 4b | ft-3681t.4.2 | Streaming event/wait | .2.5, .4.1 | .7.1 |
| 4b | ft-3681t.4.3 | Transactional action engine | .4.1 | .4.4 |
| 4b | ft-3681t.4.6 | Idempotency/dedupe | .4.1, .6.1 | .4.4 |
| 4c | ft-3681t.4.4 | Machine contracts/SDK/compat | .4.3, .4.6 | .4.5 |
| 4d | ft-3681t.4.5 | Robot API contract test matrix | all above | (quality gate) |

#### Pillar D: Policy + Governance (ft-3681t.6)

| Phase | Bead | Title | Needs | Unblocks |
|-------|------|-------|-------|----------|
| 6a | ft-3681t.6.1 | Policy DSL + decision engine | .2.3, .5.3 | .5.4, .5.5, .4.6, .6.2-.6.6, .7.1 |
| 6b | ft-3681t.6.2 | Identity graph + authz | .6.1 | .6.3 |
| 6b | ft-3681t.6.3 | Approvals/quarantine/kill-switch | .6.2 | .6.4 |
| 6c | ft-3681t.6.4 | Forensics + compliance export | .6.3, .7.1 | .6.5 |
| 6c | ft-3681t.6.6 | Multi-tenant isolation | .6.1 | .6.5 |
| 6d | ft-3681t.6.5 | Policy decision-proof suite | all above | (quality gate) |

#### Pillar E: Observability + Reliability (ft-3681t.7)

| Phase | Bead | Title | Needs | Unblocks |
|-------|------|-------|-------|----------|
| 7a | ft-3681t.7.1 | Unified telemetry schema | .2.6, .3.5, .4.2, .5.12, .5.14, .6.1 | .7.2-.7.6, .9.4 |
| 7b | ft-3681t.7.2 | Real-time dashboards/alerts | .7.1 | .7.5 |
| 7b | ft-3681t.7.3 | Capacity governor | .7.1 | .7.5 |
| 7c | ft-3681t.7.4 | Chaos + scale validation | .7.2, .7.3 | .7.5 |
| 7d | ft-3681t.7.5 | SLO conformance audit | all above | (quality gate) |
| 7e | ft-3681t.7.6 | Disaster recovery drills | .7.4 | (quality gate) |

### Track 4: Migration + Cutover (Final Gate)

**Goal**: Prove parity, migrate users, and decommission NTM.

| Phase | Bead | Title | Needs | Unblocks |
|-------|------|-------|-------|----------|
| 8-early | ft-3681t.8.1 | NTM parity corpus | .4.1 (can start draft now) | .8.2, .8.4 |
| 8a | ft-3681t.8.2 | Dual-run shadow comparator | .8.1, .4.4 | .8.4 |
| 8a | ft-3681t.8.3 | NTM session/workflow importers | .2.5, .3.1 | .8.4 |
| 8b | ft-3681t.8.4 | Staged cutover playbook | .8.1-.8.3 | .8.5, .8.6 |
| 8c | ft-3681t.8.5 | NTM decommission + docs | .8.4 | .8.6 |
| 8d | ft-3681t.8.6 | Migration rehearsal/rollback drills | all above | (quality gate) |

Cutover playbook artifact (living draft): `docs/ntm-cutover-playbook-ft-3681t.8.4.md`

### Track 5: Operator UX (Parallel with Track 3-4)

**Goal**: Build the human-facing surfaces that make the system usable.

| Phase | Bead | Title | Needs | Unblocks |
|-------|------|-------|-------|----------|
| 9a | ft-3681t.9.2 | Command center/dashboard | .2.1, .3.1, .4.1 | .9.3-.9.7 |
| 9a | ft-3681t.9.3 | Session/workflow explorer | .2.5, .4.2 | .9.4 |
| 9b | ft-3681t.9.4 | Context budget observability | .7.1, .9.2 | .9.5 |
| 9b | ft-3681t.9.5 | Intervention/approval console | .6.3, .9.2 | .9.6 |
| 9c | ft-3681t.9.6 | Runbooks/tutorials/overlays | .9.5 | .9.7, .9.1 |
| 9c | ft-3681t.9.7 | Explainability console | .6.1, .7.1, .9.2 | .9.1 |
| 9d | ft-3681t.9.1 | UX scenario validation suite | all above | (quality gate) |

## 3. Critical Path

The longest sequential dependency chain from start to full cutover:

```
ft-3681t.1.3 [DONE]
  -> ft-3681t.2.1 [IN_PROGRESS] ........... (~2 weeks remaining?)
    -> ft-3681t.2.3 (command transport)
      -> ft-3681t.4.1 (robot surface) ...... needs .3.1 too
        -> ft-3681t.4.2 (streaming events)
          -> ft-3681t.7.1 (telemetry schema) . needs many inputs
            -> ft-3681t.7.2 (dashboards)
              -> ft-3681t.7.4 (chaos validation)
                -> ft-3681t.8.4 (staged cutover)
                  -> ft-3681t.8.6 (rehearsal drills)
```

**Critical path depth**: 10 sequential tiers (assuming optimal parallelism).

**Bottleneck analysis**:
- **ft-3681t.2.1** (CopperCompass): Single biggest gate. Unblocks 6 direct
  dependents. Program velocity is capped by this bead.
- **ft-3681t.6.1** (policy DSL): Cross-cutting dependency. Needed by
  connectors (.5.3, .5.4, .5.5), robot (.4.6), and observability (.7.1).
  Start early once .2.3 and .5.3 are available.
- **ft-3681t.7.1** (telemetry schema): Depends on 6 inputs from different
  pillars. Is the integration chokepoint where all pillars converge.

## 4. Parallelism Map

After ft-3681t.2.1 completes, maximum parallelism is available:

**Wave 1** (start immediately after 2.1):
- ft-3681t.2.2, 2.3, 2.4, 2.5 (4 mux tasks in parallel)
- ft-3681t.5.1 (connector embedding)

**Wave 2** (start after Wave 1 outputs):
- ft-3681t.3.1, 3.3, 3.5 (3 swarm tasks; needs 2.4, 2.5)
- ft-3681t.5.2, 5.6, 5.7, 5.10, 5.12 (5 connector tasks; needs 5.1)
- ft-3681t.6.1 (policy DSL; needs 2.3 + 5.3 from Wave 1-2)

**Wave 3** (start after Wave 2):
- ft-3681t.4.1 (robot surface; needs 2.3 + 3.1)
- ft-3681t.3.2, 3.4, 3.7 (swarm; needs 3.1)
- ft-3681t.5.3, 5.4, 5.5, 5.8 (connectors; needs 5.1 + 6.1)
- ft-3681t.9.2 (command center; needs 2.1 + 3.1 + 4.1)

**Ideal agent allocation per wave**:
- Wave 1: 5 agents (4 mux + 1 connector)
- Wave 2: 8-9 agents (3 swarm + 5 connector + 1 policy)
- Wave 3: 7-8 agents (1 robot + 3 swarm + 3 connector + 1 UX)
- Wave 4+: diminishing parallelism as chains converge

## 5. Measurable Success Gates

### M1: Foundation Complete
- **Trigger**: ft-3681t.2.1 CLOSED
- **Evidence**: Session/window/pane lifecycle tests pass. Stable identity
  model validated. Snapshot hooks demonstrated.
- **Measurement**: All ft-3681t.2.2-.2.5 beads can start without blockers.

### M2: Core Platform Operational
- **Trigger**: ft-3681t.2 epic CLOSED + ft-3681t.3.1 CLOSED + ft-3681t.5.1 CLOSED
- **Evidence**: Native mux handles 200+ panes. Fleet launcher can
  start/stop/rebalance agent sessions. Connector host runtime loads and
  executes at least one signed connector package.
- **Measurements**:
  - Mux lifecycle operations: p99 < 50ms
  - Fleet launch 50 agents: < 30s
  - Connector load + first action: < 5s

### M3: Control Surfaces Functional
- **Trigger**: ft-3681t.4.1 + ft-3681t.6.1 + ft-3681t.7.1 CLOSED
- **Evidence**: `ft robot` commands cover NTM parity matrix. Policy DSL
  evaluates allow/deny/require_approval. Telemetry schema emits correlated
  events across all planes.
- **Measurements**:
  - ft robot command coverage: >= NTM parity (100% of census)
  - Policy evaluation: p99 < 10ms
  - Telemetry event correlation: 100% of critical paths instrumented

### M4: Migration Ready
- **Trigger**: ft-3681t.8.2 (shadow comparator) produces < 5% divergence
- **Evidence**: Dual-run shows FrankenTerm matches NTM behavior for all
  documented scenarios. Importers handle real NTM session data.
- **Measurements**:
  - Shadow divergence rate: < 5% of operations
  - Import success rate: > 99% of NTM sessions
  - Rollback time: < 60s from trigger to NTM fallback

### M5: Production Cutover
- **Trigger**: ft-3681t.8.4 (cutover playbook) executed successfully
- **Evidence**: Staged rollout to 10% -> 50% -> 100% of users without
  regression. Rehearsal drills pass. NTM decommission safe.
- **Measurements**:
  - Zero user-reported regressions during staged rollout
  - DR restore time: < 5min from backup to operational
  - Post-cutover soak: 7 days with < 0.1% error rate increase

## 6. Anti-Goals (Failure Modes to Avoid)

### Anti-Goal 1: Partial Mux Without Orchestration
Building a native mux that handles sessions/panes but never gets orchestration
wired in. This creates a "better tmux" instead of an NTM replacement. **Gate**:
Do not declare M2 without fleet launcher (.3.1) working.

### Anti-Goal 2: Connector Bolt-On
Treating connectors as an optional plugin instead of a first-class plane.
If connector fabric is deferred or weakened, FrankenTerm becomes yet another
terminal multiplexer. **Gate**: ft-3681t.5.1 must start within 1 wave of
ft-3681t.2.1 completion.

### Anti-Goal 3: Policy Afterthought
Skipping policy enforcement "for now" and bolting it on later. Every mutation
path must be policy-gated from day one. **Gate**: ft-3681t.6.1 must be
completed before any mutation-path bead (.4.3, .4.6) can close.

### Anti-Goal 4: Migration Without Shadow Proof
Attempting cutover without rigorous dual-run shadow comparison. **Gate**:
ft-3681t.8.2 must produce a machine-verifiable divergence report before
ft-3681t.8.4 can start.

### Anti-Goal 5: Quality Gate Debt
Deferring quality gate beads (.2.7, .3.6, .4.5, .5.13, .6.5, .7.5, .8.6,
.9.1) to "later". Each epic must close its quality gate before the next
track's output is consumed. **Gate**: Each epic's quality bead must be CLOSED
before the epic itself can be marked CLOSED.

## 7. Risk Register

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| ft-3681t.2.1 takes longer than expected | Medium | Critical | Assign additional agent support; decompose into smaller PRs |
| Policy DSL (6.1) creates cross-cutting blocks | High | High | Start DSL design doc in parallel with Track 1; prototype evaluator early |
| Connector host runtime has deep tokio deps | Medium | Medium | Assess asupersync compat early; use compat shim if needed |
| NTM parity gap discovered late | Low | Critical | Start parity corpus (8.1) draft now, before Track 2 completes |
| Agent availability drops during long program | Medium | Medium | Document state thoroughly; beads are fungible across agents |
| rch worker availability for heavy validation | High | Medium | Queue validation runs; use local builds as fallback |

## 8. Immediate Actions (Next 48 Hours)

1. **CopperCompass**: Continue ft-3681t.2.1; report expected completion
2. **PinkOtter**: Finalize .1.1 and .1.2 capability inventories
3. **Available agents**: Prepare for Wave 1 by reviewing:
   - ft-3681t.2.2 (topology/layout) — read screen.rs layout code
   - ft-3681t.2.3 (command transport) — read mux PDU protocol
   - ft-3681t.2.4 (session profiles) — read config/session code
   - ft-3681t.2.5 (durable state) — read SQLite/snapshot code
4. **RusticDeer**: Complete this execution plan (ft-3681t.1.4)
5. **Draft early**: Start ft-3681t.8.1 (parity corpus) as a living document
   that grows alongside implementation
   - Seed artifacts now tracked at:
     - `docs/ntm-parity-corpus-ft-3681t.8.1.md`
     - `fixtures/e2e/ntm_parity/corpus.v1.json`
     - `fixtures/e2e/ntm_parity/acceptance_matrix.v1.json`

## 9. Program Metrics Dashboard

Track these weekly:

| Metric | Current | Target |
|--------|---------|--------|
| Beads CLOSED in ft-3681t | 1 (1.3) | 66 total |
| Beads IN_PROGRESS | 4 (1.1, 1.2, 1.4, 2.1) | max 8-10 parallel |
| Critical path depth remaining | 10 tiers | 0 |
| Quality gate beads completed | 0/9 | 9/9 |
| Active agents on ft-3681t | 3 (PinkOtter, CopperCompass, RusticDeer) | 5-8 |
| Blocked beads (ready but no assignee) | 0 (all wait on 2.1) | minimize |
