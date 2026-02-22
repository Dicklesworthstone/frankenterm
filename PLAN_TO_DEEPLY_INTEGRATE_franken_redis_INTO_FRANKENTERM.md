# PLAN_TO_DEEPLY_INTEGRATE_franken_redis_INTO_FRANKENTERM

Status: `draft`  
Track: `ft-2vuw7.2.3`  
Current slice completed in this revision: `ft-2vuw7.2.3.1` (P1)

## 0. Intent and framing

This plan defines how FrankenTerm (`ft`) can deeply and natively integrate FrankenRedis (`frankenredis`) as a control-plane state substrate for agent-swarm coordination, while preserving FrankenTerm's existing canonical capture/search durability model.

Deep native integration for this track means:
- integration is architecture-level and policy-aware, not a bolt-on command wrapper;
- runtime, security, and observability contracts are explicit and testable;
- upstream changes in `frankenredis` and crate extraction are allowed where they improve integration quality.

Note on path resolution for this environment:
- beads reference `/dp/franken_redis`;
- local checked-out repo path is `/Users/jemanuel/projects/frankenredis`.

---

## 1. P1 - Integration objectives, hard constraints, non-goals, and success criteria

### 1.1 Objectives

1. Define a native role for FrankenRedis inside FrankenTerm's control plane:
- low-latency mutable coordination state (leases, queues, heartbeats, control flags);
- not canonical transcript durability (which remains in FrankenTerm storage contracts).
2. Preserve FrankenTerm platform direction:
- swarm-native orchestration,
- machine-first control surfaces (`ft robot`, MCP),
- policy-gated operations with auditable decisions.
3. Align integration with FrankenRedis design center:
- strict/hardened mode split,
- fail-closed compatibility gating,
- evidence-ledger emission for policy-sensitive decisions.
4. Preserve deterministic command and ordering semantics for integration-critical flows (especially expiration, persistence replay ordering, and replication-offset reasoning).
5. Keep integration phased and evidence-driven, with explicit risk controls and rollback posture.
6. Enable upstream crate/interface extraction where needed for clean embedding (for example protocol/runtime/store surfaces) without long-lived adapters that accumulate debt.

### 1.2 Hard constraints

1. Safety/language constraints:
- Rust 2024 and `#![forbid(unsafe_code)]` posture preserved across integration boundaries.
2. Control-plane safety constraints:
- FrankenTerm policy engine remains the authority for mutating actions;
- integration must not bypass existing approval/audit surfaces.
3. Data-plane separation constraints:
- FrankenTerm canonical recorder data remains in existing storage abstractions (`storage.rs` and recorder contracts);
- FrankenRedis integration targets coordination/state acceleration, not replacement of canonical output/event durability.
4. Runtime maturity constraints:
- frankenredis currently has an early vertical slice (RESP, command router, in-memory store + TTL semantics, persistence/replication scaffolding) and is still on a parity program;
- therefore production-critical integration must phase-gate by conformance evidence, not by repository ambition alone.
5. Security and drift-governance constraints:
- strict/hardened policy behavior and threat-class evidence contracts must remain explicit and auditable when embedded in FrankenTerm workflows.
6. Operational constraints:
- integration must include backpressure/resource-clamp strategy and clear failure semantics under swarm load;
- no hidden unbounded queues or silent degradation.
7. Compatibility constraints:
- no feature loss/regression in existing FrankenTerm user-visible control surfaces during rollout;
- no irreversible migration path without deterministic rollback.

### 1.3 Non-goals

1. Not an immediate replacement of FrankenTerm's SQLite/FTS-backed canonical storage/query pipeline with Redis-like storage.
2. Not exposing full external Redis compatibility mode through FrankenTerm on day one.
3. Not requiring complete Redis-parity completion before any scoped internal integration can begin.
4. Not immediate rollout of full distributed replication/cluster topology as a prerequisite for initial in-process integration.
5. Not building long-lived compatibility shims that preserve suboptimal interim APIs.
6. Not allowing policy or audit bypass in the name of low-latency control operations.
7. Not introducing integration behavior that is unmeasured, unlogged, or unverifiable in conformance/perf gates.

### 1.4 Success criteria (P1 exit gate)

P1 is complete when the following are true:

1. The integration intent is explicit: frankenredis is positioned as coordination/control-plane acceleration, not canonical transcript storage replacement.
2. Hard constraints are concrete and sufficient to guide P2 pattern selection and P3 placement decisions without ambiguity.
3. Security and governance expectations (strict/hardened, threat/evidence contracts) are captured as first-class integration constraints.
4. Non-goals clearly prevent scope explosion into "replace everything with redis" or "ship without parity evidence."

---

## 2. P2 - Evaluate integration patterns (embed/service/adapter/hybrid)

Status: `pending` (`ft-2vuw7.2.3.2`)

## 3. P3 - Target placement within FrankenTerm subsystems

Status: `pending` (`ft-2vuw7.2.3.3`)

## 4. P4 - API contracts and crate/subcrate extraction roadmap

Status: `pending` (`ft-2vuw7.2.3.4`)

## 5. P5 - Data migration/state synchronization and compatibility posture

Status: `pending` (`ft-2vuw7.2.3.5`)

## 6. P6 - Testing strategy and detailed logging/observability

Status: `pending` (`ft-2vuw7.2.3.6`)

## 7. P7 - Rollout, rollback, risk mitigation, acceptance gates

Status: `pending` (`ft-2vuw7.2.3.7`)

## 8. P8 - Finalize cohesive integration plan

Status: `pending` (`ft-2vuw7.2.3.8`)
