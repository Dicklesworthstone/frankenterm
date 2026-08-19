# PLAN_TO_DEEPLY_INTEGRATE_process_triage_INTO_FRANKENTERM

Status: `draft`  
Track: `ft-2vuw7.5.3`  
Current slice completed in this revision: `ft-2vuw7.5.3.1`..`ft-2vuw7.5.3.8` (P1-P8), including detailed testing/logging scope `ft-2vuw7.5.3.6.1`..`ft-2vuw7.5.3.6.6` (T1-T6).

## 0. Intent and framing

This plan defines how FrankenTerm (`ft`) should deeply and natively integrate `process_triage` (`pt`) as a first-class process-intelligence and remediation subsystem for swarm operations.

Deep native integration for this track means:
- `pt` capabilities become architecture-level components inside FrankenTerm control flows, not an external bolt-on script;
- policy, audit, and observability semantics are unified with existing FrankenTerm safety and recorder pipelines;
- upstream changes to `/dp/process_triage` are explicitly allowed where they reduce integration debt;
- crate/subcrate extraction is explicitly allowed where it produces stable library-grade boundaries.

Input analysis source for this plan:
- `COMPREHENSIVE_ANALYSIS_OF_process_triage.md`

---

## 1. P1 - Integration objectives, constraints, non-goals, and success criteria

### 1.1 Objectives

1. Replace FrankenTerm’s current local heuristic process triage core (`crates/frankenterm-core/src/process_triage.rs`) with a richer, evidence-driven `pt` pipeline while preserving FrankenTerm’s operator UX and automation controls.
2. Integrate `pt` session lifecycle (`snapshot/plan/apply/verify/diff/sessions`) as native FrankenTerm operational workflows for agent swarm health management.
3. Unify action governance:
- `pt` candidate generation + rationale
- FrankenTerm policy/approval workflow (`policy`, `approval`, `workflows`, `command_guard`).
4. Unify telemetry and auditability:
- ingest `pt` artifacts and telemetry into FrankenTerm observability/audit surfaces (`recorder_audit`, `storage_telemetry`, `reports`, robot/MCP outputs).
5. Preserve operational safety under high swarm load with explicit budgets, bounded actions, and resumable execution.

### 1.2 Hard constraints

1. No safety regressions:
- identity revalidation and protected-process gating must remain mandatory before process-kill actions.
2. No control-plane bypass:
- any `pt`-suggested action still passes FrankenTerm policy gates before execution.
3. No feature loss:
- existing FrankenTerm lifecycle/triage workflows remain available throughout migration.
4. Deterministic rollback:
- every migration phase must have explicit disable/fallback controls.
5. Observability first:
- all integration phases must ship structured logs and test artifacts, not just functional behavior.
6. No permanent compatibility shims:
- transitional adapters are temporary and have removal gates.

### 1.3 Non-goals

1. Not a day-one hard swap of all FrankenTerm process health logic to direct `pt-core` internals.
2. Not immediate full parity exposure of every `pt` command inside `ft` CLI/MCP in phase 1.
3. Not introducing distributed/multi-host orchestration complexity before single-host integration quality gates pass.
4. Not preserving stale interfaces solely for backward compatibility.

### 1.4 Success criteria (P1 exit)

1. Integration role boundaries are explicit (what `pt` owns vs what FrankenTerm keeps).
2. Safety and policy interactions are explicitly defined and testable.
3. Downstream phases (P2-P8) are fully actionable without reopening analysis docs.

---

## 2. P2 - Evaluate integration patterns (embed/service/adapter/hybrid)

### 2.1 Option A: CLI adapter (invoke `pt agent` subprocesses)

Description:
- FrankenTerm shells out to `pt agent` commands and consumes JSON/session artifacts.

Pros:
- Fastest path to production signal.
- Lowest initial code churn in both repos.

Cons:
- Contract drift risk between docs and runtime behavior.
- Extra process overhead and weaker type safety.
- Harder deep optimization/co-scheduling.

Best use:
- phase-1 bootstrap only.

### 2.2 Option B: Library embed (direct `pt-core` crate APIs)

Description:
- FrankenTerm links stable orchestration APIs directly from `pt`.

Pros:
- Strong type safety and lower runtime overhead.
- Cleanest long-term architecture.

Cons:
- Requires upstream extraction from `pt-core/main.rs` orchestration paths.
- Larger initial refactor cost.

Best use:
- phase-2/phase-3 target.

### 2.3 Option C: MCP bridge (FrankenTerm as MCP client of `pt` server)

Description:
- FrankenTerm consumes `pt` via MCP tool/resource calls.

Pros:
- Agent-native and composable.
- Clean protocol boundary.

Cons:
- Current `pt` MCP tool coverage is narrower than needed for full lifecycle integration.
- Additional serialization overhead.

Best use:
- adjunct interface for external agent orchestration, not primary internal engine.

### 2.4 Option D: Hybrid phased integration (selected)

Description:
- Phase 1 uses hardened CLI/session artifact adapter for speed.
- Phase 2 introduces extracted library interfaces and shifts critical flows to direct embed.
- MCP is extended in parallel for external orchestration parity.

Why selected:
- best tradeoff of delivery speed, architectural quality, and rollback safety.

Decision:
- **Select Option D (Hybrid phased integration).**

---

## 3. P3 - Target placement inside FrankenTerm subsystems

### 3.1 Placement map

1. **Process intelligence core**
- Current: `crates/frankenterm-core/src/process_triage.rs`, `pane_lifecycle.rs`.
- Target: `ft_process_triage` orchestration layer backed by `pt` candidate pipeline.

2. **Workflow and policy gating**
- Integrate candidate/action approval through:
  - `crates/frankenterm-core/src/workflows.rs`
  - `crates/frankenterm-core/src/policy.rs`
  - `crates/frankenterm-core/src/approval.rs`
  - `crates/frankenterm-core/src/command_guard.rs`

3. **Storage/audit/telemetry integration**
- Persist and cross-link triage evidence into:
  - `crates/frankenterm-core/src/recorder_audit.rs`
  - `crates/frankenterm-core/src/storage_telemetry.rs`
  - `crates/frankenterm-core/src/reports.rs`
  - `crates/frankenterm-core/src/storage.rs` (indexing/query integration points)

4. **Robot/MCP/operator surfaces**
- Extend:
  - `crates/frankenterm/src/main.rs` robot/CLI command trees
  - `crates/frankenterm-core/src/mcp.rs`
- Goal: expose triage lifecycle controls with structured machine outputs.

### 3.2 Integration boundary definition

FrankenTerm retains:
- pane/session context, policy authority, operator workflows, recorder/audit canonicality.

`process_triage` contributes:
- process collection/inference/decision candidates, session artifacts, and verification evidence.

---

## 4. P4 - API contracts and crate/subcrate extraction roadmap

### 4.1 Required upstream extraction in `/dp/process_triage`

1. **`pt-orchestrator` crate (new)**
- Extract typed orchestration APIs from `pt-core/src/main.rs`:
  - `snapshot()`, `plan()`, `explain()`, `apply()`, `verify()`, `diff()`, `sessions_*()`
- Remove FrankenTerm reliance on subprocess JSON parsing for critical paths.

2. **`pt-contract` crate (new)**
- Canonical output contracts for CLI/MCP/library responses.
- Single source of truth for versioned schemas and envelope types.

3. **`pt-session-store` split (optional but recommended)**
- Isolate session lifecycle and artifact persistence APIs.

4. **MCP surface expansion**
- Add tools/resources for full lifecycle parity, not only scan/explain/history/signatures/capabilities.

### 4.2 Required FrankenTerm-side interfaces

1. Add `ProcessTriageProvider` trait in `frankenterm-core`:
- `build_plan`, `explain_targets`, `apply_plan`, `verify_plan`, `session_status`, `diff_sessions`.

2. Provide two implementations:
- `PtCliProvider` (bootstrap adapter).
- `PtEmbeddedProvider` (target implementation after upstream extraction).

3. Add a versioned integration envelope (`ft.process_triage.v1`) for internal/event bus usage.

### 4.3 Contract change policy

- Additive-only changes per schema version.
- Mandatory `schema_version`, `session_id`, `generated_at`, and host provenance fields.
- Compatibility test matrix required before bumping contract versions.

---

## 5. P5 - Data migration, state synchronization, compatibility posture

### 5.1 Session and artifact synchronization

1. Maintain `pt` session store as source-of-truth for triage lifecycle in phase 1.
2. Ingest key artifacts (`inventory`, `inference`, `plan`, `run_metadata`, outcomes) into FrankenTerm telemetry index for cross-tool queries.
3. Add deterministic mapping key: `ft_session_id` <-> `pt_session_id`.

### 5.2 State coexistence strategy

1. During migration, keep current FrankenTerm local triage path available behind feature gate.
2. Route new triage flows through provider abstraction.
3. Compare outputs of legacy vs `pt` mode on canary runs and record divergences.

### 5.3 Compatibility posture

- No long-lived shim policy.
- Transitional adapters are allowed only with explicit deprecation/removal gates.
- Existing commands remain functionally available while backend swaps occur.

---

## 6. P6 - Testing strategy and detailed logging/observability

This section is intentionally exhaustive and directly maps to child tasks T1-T6.

### 6.1 T1 Unit test matrix with edge/error boundaries

Required unit suites:
1. Provider contract conformance (`PtCliProvider`, `PtEmbeddedProvider`).
2. Identity revalidation failure handling and blocked-action propagation.
3. Policy handoff correctness (candidate allow/deny/require-approval mapping).
4. Artifact parsing/validation (including malformed or schema-mismatch artifacts).
5. Retry/backoff logic and timeout/error classification.
6. Feature-gate switching behavior (legacy/local vs `pt` provider).

### 6.2 T2 Integration test harness for FrankenTerm seam contracts

Required integration suites:
1. `ft` workflow -> provider -> session artifacts -> audit/telemetry pipeline.
2. Multi-pane correlation where one triage session spans pane-linked process trees.
3. Policy + approval loop with synthetic high-risk candidates.
4. Storage/query roundtrip: triage evidence discoverable in FrankenTerm query surfaces.
5. MCP and robot command parity for triage lifecycle operations.

### 6.3 T3 End-to-end suites (happy path + failure + recovery + rollback)

Required E2E scenarios:
1. Happy path: snapshot -> plan -> apply -> verify -> report.
2. Safe-block path: policy denied actions with explicit reasons and no side effects.
3. Interrupted apply + resume semantics.
4. Provider failure fallback to legacy/local mode.
5. Rollback drill: disable new provider and confirm continued operator functionality.

### 6.4 T4 Logging/tracing/diagnostics assertion plan

Mandatory structured fields in triage integration logs:
- `ft_session_id`, `pt_session_id`, `pane_id`, `host_id`, `provider_kind`
- `phase` (`snapshot|plan|apply|verify|diff|sessions`)
- `policy_decision`, `action_count`, `blocked_count`, `risk_level`
- latency fields: `collect_ms`, `infer_ms`, `plan_ms`, `apply_ms`, `verify_ms`, `end_to_end_ms`
- failure fields: `error_class`, `retryable`, `rollback_engaged`

Required assertions:
1. every workflow phase emits start + complete (or start + failed) events.
2. every action recommendation includes decision provenance.
3. rollback/fallback transitions are explicitly logged and auditable.

### 6.5 T5 Deterministic fixtures, replay corpus, test-data lifecycle

1. Build fixed fixture corpus for process states/classifications and policy outcomes.
2. Add replayable artifact corpus (`inventory/inference/plan/outcomes`) for regression tests.
3. Version fixture schema with explicit migration scripts.
4. Ensure replay tests run in CI and locally with deterministic output checksums.

### 6.6 T6 Performance/soak/flake-resilience validation

Performance targets (initial):
1. Triaging 1k processes should stay within documented latency budget on target hardware.
2. Provider adapter overhead must remain below defined budget per invocation.
3. 24h soak with periodic triage runs must show no unbounded memory growth.

Soak and resilience checks:
1. repeated timeout/retry storms without lockup.
2. concurrent workflow executions honoring policy/lock constraints.
3. flake tracking with rerun diagnostics and artifact bundling.

---

## 7. P7 - Rollout, rollback, risk mitigation, acceptance gates

### 7.1 Phased rollout plan

Phase 0 - Scaffolding:
- Introduce provider abstraction and feature gates.
- No behavior change by default.

Phase 1 - CLI adapter canary:
- Enable `PtCliProvider` for opt-in workloads.
- Collect parity and latency data vs legacy triage.

Phase 2 - Embedded provider alpha:
- Integrate extracted `pt` orchestration crate (`PtEmbeddedProvider`) behind flag.
- Keep CLI provider as fallback.

Phase 3 - Default switch:
- Make embedded provider default after acceptance gates pass.
- Keep explicit rollback toggle for one release cycle.

Phase 4 - Cleanup:
- Remove deprecated transitional surfaces and stale compatibility code.

### 7.2 Rollback controls

1. Runtime flag to force legacy/local triage provider.
2. Runtime flag to force CLI provider if embedded provider regresses.
3. Automatic fallback on provider hard-failure with explicit audit event.
4. Documented operator runbook for immediate rollback.

### 7.3 Risk register (top)

1. **Contract drift risk**
- Mitigation: `pt-contract` extraction + compatibility tests.

2. **Monolith extraction delay risk**
- Mitigation: phase-1 CLI adapter keeps progress while extraction proceeds.

3. **Safety regression risk**
- Mitigation: hard policy gating remains FrankenTerm authority + mandatory precheck tests.

4. **Operational complexity risk**
- Mitigation: provider abstraction + staged rollout + deterministic rollback.

### 7.4 Acceptance gates

Minimum gates before default switch:
1. Unit/integration/E2E suites green.
2. Soak tests pass with bounded memory and no critical flake clusters.
3. Policy and audit assertions fully pass.
4. Latency and error-rate SLOs met in canary.
5. Rollback drill successfully executed.

---

## 8. P8 - Final target architecture and implementation roadmap

### 8.1 Chosen architecture

**Hybrid phased native integration**:
- near-term: `PtCliProvider` adapter + artifact ingestion + strict policy handoff.
- medium-term: upstream extraction -> `PtEmbeddedProvider` direct typed integration.
- long-term: expanded MCP/robot parity and full operational unification.

### 8.2 Concrete implementation roadmap

Wave A (Bootstrap)
1. Add `ProcessTriageProvider` trait and `PtCliProvider` in `frankenterm-core`.
2. Add triage workflow glue in `workflows` and command surfaces.
3. Add artifact ingestion and correlation IDs to audit/telemetry.

Wave B (Contract stabilization)
1. Upstream `pt-contract` extraction.
2. Add contract fixtures and compatibility CI.
3. Expand MCP tools/resources to lifecycle parity.

Wave C (Native embed)
1. Upstream `pt-orchestrator` extraction.
2. Implement `PtEmbeddedProvider`.
3. Shift default path from CLI adapter to embedded provider.

Wave D (Convergence)
1. Retire deprecated transitional paths.
2. Tighten policies and simplify runtime toggles.
3. Finalize docs/runbooks/dashboards.

### 8.3 Deliverable outcomes expected from this plan

1. FrankenTerm gains robust process-intelligence orchestration with stronger evidence and policy controls.
2. Integration remains reversible and operationally safe throughout migration.
3. Architecture converges toward stable library boundaries with reduced long-term debt.

---

## 9. Bead execution mapping (self-contained)

Completed by this plan document:
- `ft-2vuw7.5.3.1` P1 objectives/constraints/non-goals
- `ft-2vuw7.5.3.2` P2 options analysis and decision
- `ft-2vuw7.5.3.3` P3 subsystem placement design
- `ft-2vuw7.5.3.4` P4 API contracts + extraction roadmap
- `ft-2vuw7.5.3.5` P5 migration/state sync posture
- `ft-2vuw7.5.3.6` P6 test/logging strategy
- `ft-2vuw7.5.3.6.1`..`ft-2vuw7.5.3.6.6` T1-T6 detailed coverage
- `ft-2vuw7.5.3.7` P7 rollout/rollback/risk gates
- `ft-2vuw7.5.3.8` P8 finalized target architecture and roadmap
- `ft-2vuw7.5.3` parent integration-plan task

