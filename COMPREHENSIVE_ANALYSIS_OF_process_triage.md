# COMPREHENSIVE_ANALYSIS_OF_process_triage

## 1. Executive Summary

`process_triage` (`pt`) is a Rust-first process classification and action-orchestration system optimized for both human operators and AI agents. It combines multi-source process collection (`quick_scan` + optional Linux deep probes), probabilistic inference, policy-gated action planning/execution, and durable session artifacts. It already exposes machine-facing control surfaces (robot/agent CLI + MCP) that align strongly with FrankenTerm’s swarm-control goals.

High-level conclusion for FrankenTerm integration:
- **Best near-term value**: embed `pt` as FrankenTerm’s policy-grade “process intelligence and remediation subsystem” for host/pane/process health management.
- **Main technical blocker**: `pt-core` is currently a large monolith with command wiring and orchestration concentrated in `main.rs`; deeper integration will benefit from extracting library-grade orchestration APIs.
- **Safety posture is strong**: explicit identity revalidation, policy gates, prechecks, resumable sessions, and integrity-checked artifacts are already present.

---

## 2. Project Purpose and Strategic Fit

### 2.1 What `process_triage` does

From architecture/docs and code:
- Collects process telemetry quickly (`ps`) and deeply (`/proc` on Linux).
- Computes classification and recommendations using Bayesian/statistical decision modules.
- Produces and executes action plans under policy constraints.
- Persists rich session artifacts for replay/diff/verification/audit.
- Exposes agent-friendly interfaces (robot/agent subcommands, MCP server).

### 2.2 Why this matters to FrankenTerm

FrankenTerm needs fleet/pane/process-level intelligence under high concurrency. `pt` directly contributes:
- Host-level process triage for stuck/abandoned agents.
- Action policy enforcement before remediation.
- Sessionized evidence for postmortems and automation traceability.
- Existing machine contracts that can be composed into FrankenTerm robot mode and workflows.

---

## 3. Repository Topology and Crate Boundaries (R1)

Root: `/dp/process_triage`

Workspace crates (from `Cargo.toml`):
- `pt-core` (binary + lib): CLI surface, orchestration, inference/decision/action/session pipelines.
- `pt-common`: shared ids/types/schema/output/capabilities foundations.
- `pt-config`: config schemas/resolution/validation/presets.
- `pt-math`: math-oriented support (used by core pipeline).
- `pt-redact`: redaction utilities.
- `pt-bundle`: bundle packaging surfaces.
- `pt-telemetry`: Arrow/Parquet schema, writer, retention, shadow storage.
- `pt-report`: optional report generation crate.

`pt-core` internal domains (not exhaustive, but core):
- `collect/`: quick/deep scan and platform probes.
- `inference/`: probabilistic/statistical engines and explanation support.
- `decision/`: policy, expected-loss logic, gating, optimization.
- `action/`: execution runners, prechecks, recovery, constraints.
- `session/`: durable session lifecycle + artifact persistence.
- `mcp/`: stdio MCP server + tools/resources.
- `fleet/`: inventory/discovery/transfer support.
- `daemon/`, `audit/`, `logging/`, `capabilities/`, `supervision/`, `plugin/`, `replay/`.

Boundary assessment:
- Crate boundaries are conceptually good at the workspace level.
- Inside `pt-core`, boundary separation is present but orchestration remains highly centralized in `main.rs` command handlers.

---

## 4. Build/Runtime/Dependency and Feature Matrix (R2)

### 4.1 Toolchain and build profile

- Rust edition: workspace 2021.
- Workspace `rust-version`: `1.88`.
- Release profile: LTO fat, single codegen unit, strip, panic abort, plus `release-small` profile.

### 4.2 Core runtime model

- Predominantly synchronous command execution model with scoped parallelism in specific collectors.
- `quick_scan`: `ps` subprocess + watchdog timeout thread.
- `deep_scan`: Linux `/proc` traversal with bounded thread parallelism.
- Session persistence via filesystem artifacts (JSON and JSONL + optional parquet telemetry).

### 4.3 Key dependencies and implications

- CLI/config: `clap`, `serde`, `serde_json`, `serde_yaml`, `toml`.
- Schema/types: `schemars`, `chrono`, `uuid`.
- Telemetry: `arrow`, `parquet`.
- Security/integrity/signing: `sha2`, `p256`, `base64`.
- Observability: `tracing`, `tracing-subscriber`, optional `prometheus`/`tiny_http`.

### 4.4 Feature flags (notable)

`pt-core` features:
- `deep`: privileged/expensive probe surfaces.
- `daemon`: dormant monitoring mode.
- `metrics`: Prometheus endpoint.
- `ui`: ftui-backed TUI.
- `report`: enables `pt-report` integration.
- `fleet-dns`: DNS discovery scaffold.

Integration implication:
- FrankenTerm can adopt a minimal footprint by embedding with selected features, then selectively enabling `daemon`, `metrics`, and `report` by deployment profile.

---

## 5. Public Surface Inventory (R3)

### 5.1 CLI surfaces

Primary binary: `pt-core`.
Major command groups include:
- run/scan/deep-scan/infer/decide/ui.
- `agent` subtree (with `robot` alias): snapshot/plan/explain/apply/verify/diff/sessions/watch/tail/inbox/capabilities/fleet/export/import/report/init.
- config, daemon, telemetry, shadow, update, bundle, learn.

The `agent`/`robot` interface is the most integration-relevant for FrankenTerm automation.

### 5.2 MCP surface

In-tree stdio MCP implementation supports:
- Methods: `initialize`, `tools/list`, `tools/call`, `resources/list`, `resources/read`, `ping`.
- Tools currently implemented: `pt_scan`, `pt_explain`, `pt_history`, `pt_signatures`, `pt_capabilities`.
- Resources: `pt://config/priors`, `pt://config/policy`, `pt://signatures/builtin`, `pt://version`.

Assessment:
- MCP layer exists and is stable enough for immediate augmentation, but tool surface is relatively narrow versus full CLI capability.

### 5.3 Config surface

Resolution path: CLI overrides -> env (`PROCESS_TRIAGE_CONFIG`) -> XDG -> defaults.
- `priors.json` and `policy.json` typed and validated.
- config snapshot includes provenance/hashes/schema versions.

### 5.4 Event/output contracts

- JSON/markdown/jsonl and additional formats supported by CLI specification.
- Session logs persisted (`logs/session.jsonl`) for progress/event tails.

---

## 6. Execution Flow Tracing (R4)

### 6.1 Golden session flow

Observed operational flow in `agent` pipeline:
1. `agent snapshot`: initialize session + context + capabilities + inventory/inference persistence.
2. `agent plan`: collect/process/filter/infer/decide + policy enforce + generate plan artifacts.
3. `agent explain`: process-level rationale and evidence introspection.
4. `agent apply`: target selection + constraints + identity revalidation + prechecks + action execution.
5. `agent verify`: outcome verification and summary.
6. `agent diff/sessions`: compare/resume/list lifecycle states.

### 6.2 Key orchestration properties

- Global run lock to prevent conflicting destructive runs.
- Session state machine with append-only transitions.
- Resume semantics (`--resume`) backed by outcomes log and session manifests.

### 6.3 Fleet flow

- Fleet subcommands support plan/apply/report/status and transfer (export/import/diff).
- Inventory adapters support static files and discovery patterns.

---

## 7. Data, State, and Persistence Contracts (R5)

### 7.1 Session storage model

Session root resolved by:
- `PROCESS_TRIAGE_DATA` override, else `XDG_DATA_HOME`, else platform data dir.

Per-session structure contains:
- `manifest.json`, `context.json`, optional `capabilities.json`.
- directories: `scan/`, `inference/`, `decision/`, `action/`, `telemetry/`, `logs/`, `exports/`.

### 7.2 Artifact persistence

`session/snapshot_persist.rs` provides:
- typed artifact envelopes with schema version.
- canonicalized payload hashing + integrity SHA-256 checks.
- atomic write/rename semantics.
- checked and unchecked load paths.

Artifact categories:
- inventory, inference, plan, run_metadata.

### 7.3 Telemetry persistence

`pt-telemetry` provides:
- Arrow schemas for runs/proc samples/features/inference/outcomes/audit/signature matches.
- batched parquet writer with compression and partitioned layout.
- retention engine with explicit prune events and dry-run preview.
- shadow mode tiered storage lifecycle (hot/warm/cold/archive).

Integration implication:
- telemetry contracts are mature enough to bridge into FrankenTerm observability backends with translation layers instead of reinvention.

---

## 8. Reliability, Performance, Security, and Policy Surfaces (R6)

### 8.1 Reliability strengths

- Session resumability and explicit state transitions.
- Atomic artifact writes.
- Per-action outcome logs.
- Verification and diff commands to close the loop.

### 8.2 Performance characteristics and risks

Strengths:
- `quick_scan` single-command approach is low-overhead and portable.
- `deep_scan` uses capped parallel workers and graceful degradation.
- Parquet batching for telemetry scale.

Risks/hotspots:
- `pt-core/src/main.rs` is very large and heavily orchestrational; maintainability and compile-time locality risks increase.
- Some docs/specs include forward-looking contracts that exceed currently implemented behavior; contract drift risk for machine integrators.

### 8.3 Security/policy strengths

- Policy-driven action gating and robot constraints.
- Identity quality/start-id revalidation to reduce PID reuse TOCTOU errors.
- Protected process filtering and prechecks.
- Signature and capability-aware execution decisions.

Potential hardening opportunities:
- Standardize a single authoritative machine contract generated from live code paths to reduce spec drift.
- Expand cryptographic integrity consistency (some hashing paths rely on custom/minimal or non-cryptographic contexts in config hashing).

---

## 9. Integration Seams and Upstream Tweak Opportunities (R7)

### 9.1 High-value seams for FrankenTerm

1. **Robot/agent CLI seam**
- Immediate consumption via `pt agent/robot` commands for process triage actions per host.
- Minimal initial coupling; good bootstrap path.

2. **Session artifact seam**
- Consume `manifest/context/inventory/inference/plan/outcomes` as structured evidence for FrankenTerm orchestration and postmortems.

3. **Telemetry seam**
- Reuse parquet schemas and retention logic for FrankenTerm-wide operational timelines.

4. **MCP seam**
- Expand existing MCP tools/resources to expose parity with key `agent` flows; useful for AI-native orchestration.

### 9.2 Upstream tweaks strongly recommended

1. **Extract orchestration library from `main.rs`**
- Move run_agent_* orchestration into `pt-core` service APIs callable from FrankenTerm directly.

2. **Contract stabilization pass**
- Align docs/specs with actual command behavior or gate planned features explicitly in generated schemas.

3. **Typed API layer for session pipeline**
- Snapshot/plan/apply/verify callable as typed Rust functions with stable structs (not only CLI invocation).

4. **MCP expansion**
- Add tools for plan/apply/verify/session status and richer resources for session artifacts.

5. **Config hash integrity cleanup**
- Ensure all integrity-sensitive hashes use cryptographic SHA-256 consistently and document intended threat model.

---

## 10. Candidate Crate/Subcrate Extraction Points (R7 extension)

Priority extraction candidates:

1. `pt-orchestrator` (new)
- Encapsulate agent workflow orchestration currently in `main.rs`.
- API: `snapshot()`, `plan()`, `apply()`, `verify()`, `diff()`, `sessions()`.

2. `pt-contract` (new)
- Canonical JSON schema/types for machine outputs (CLI + MCP + internal).
- Auto-generated docs from types to eliminate drift.

3. `pt-policy-runtime` (new or split from decision/action)
- Consolidate policy enforcer + runtime robot constraints + precheck orchestration.

4. `pt-session-store` (possible split)
- Session lifecycle + artifact envelope persistence as standalone library.

5. `pt-mcp-surface` (possible split)
- Reusable MCP server/tool/resource adapter layer.

These extractions would materially reduce FrankenTerm integration friction and improve testability.

---

## 11. Coupling/Dependency Hotspots

1. **`pt-core/src/main.rs` centralization hotspot**
- Command parsing, orchestration, output shaping, and side-effect execution are tightly co-located.

2. **Cross-domain type movement**
- Strong cross-use among collect/inference/decision/action/session layers inside `pt-core`.

3. **Documentation contract drift risk**
- Some docs/specs describe states/fields/flows that are aspirational or partially implemented.

---

## 12. Recommended FrankenTerm Integration Strategy

### Phase 1: Low-risk embedding
- Invoke `pt agent/robot` from FrankenTerm workflows.
- Ingest session artifacts and selected telemetry outputs.
- Add operational glue without upstream invasive changes.

### Phase 2: Contract hardening
- Upstream: generate and freeze machine contracts from code.
- Align MCP/CLI output envelopes.
- Add integration tests across FrankenTerm↔pt invocation paths.

### Phase 3: Native library integration
- Upstream extraction of orchestration API crates.
- FrankenTerm links direct Rust APIs for lower overhead and stricter type safety.

### Phase 4: Unified control plane
- Expose enriched `pt` capabilities through FrankenTerm’s robot APIs and policy engine.
- Consolidate telemetry retention/observability pipelines.

---

## 13. Evidence Index (Files Investigated)

Core workspace and crates:
- `/dp/process_triage/Cargo.toml`
- `/dp/process_triage/crates/pt-core/Cargo.toml`
- `/dp/process_triage/crates/pt-common/Cargo.toml`
- `/dp/process_triage/crates/pt-config/Cargo.toml`
- `/dp/process_triage/crates/pt-math/Cargo.toml`
- `/dp/process_triage/crates/pt-redact/Cargo.toml`
- `/dp/process_triage/crates/pt-bundle/Cargo.toml`
- `/dp/process_triage/crates/pt-telemetry/Cargo.toml`

Docs/specs:
- `/dp/process_triage/README.md`
- `/dp/process_triage/docs/architecture/README.md`
- `/dp/process_triage/docs/AGENT_INTEGRATION_GUIDE.md`
- `/dp/process_triage/docs/CLI_SPECIFICATION.md`
- `/dp/process_triage/specs/agent-cli-contract.md`

Key code surfaces:
- `/dp/process_triage/crates/pt-core/src/main.rs`
- `/dp/process_triage/crates/pt-core/src/lib.rs`
- `/dp/process_triage/crates/pt-core/src/config/mod.rs`
- `/dp/process_triage/crates/pt-core/src/session/mod.rs`
- `/dp/process_triage/crates/pt-core/src/session/snapshot_persist.rs`
- `/dp/process_triage/crates/pt-core/src/mcp/{mod.rs,protocol.rs,server.rs,tools.rs,resources.rs}`
- `/dp/process_triage/crates/pt-core/src/collect/{types.rs,quick_scan.rs,deep_scan.rs}`
- `/dp/process_triage/crates/pt-core/src/inference/mod.rs`
- `/dp/process_triage/crates/pt-core/src/decision/mod.rs`
- `/dp/process_triage/crates/pt-core/src/action/mod.rs`
- `/dp/process_triage/crates/pt-core/src/fleet/{mod.rs,inventory.rs,transfer.rs}`
- `/dp/process_triage/crates/pt-common/src/lib.rs`
- `/dp/process_triage/crates/pt-config/src/lib.rs`
- `/dp/process_triage/crates/pt-telemetry/src/{schema.rs,writer.rs,retention.rs,shadow.rs}`

---

## 14. Completeness Checklist (R1-R8)

- [x] R1 Repository topology and crate/module boundary inventory
- [x] R2 Build/runtime/dependency and feature-flag matrix
- [x] R3 Public surface inventory (CLI/agent/MCP/config)
- [x] R4 Core execution-flow tracing
- [x] R5 Data/state/persistence contract analysis
- [x] R6 Reliability/performance/security/policy analysis
- [x] R7 Integration seam + upstream tweak + extraction analysis
- [x] R8 Evidence pack and completeness verification

