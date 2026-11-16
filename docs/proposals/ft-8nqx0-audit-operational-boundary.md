# Audit and operational boundary audit (ft-8nqx0)

## Summary

The concern is real, but the live shape is slightly different from the bead
title. The ARS cluster is already physically extracted to
`crates/frankenterm-core-ars`, while `frankenterm-core` still contains a large
set of audit, forensics, evidence, report, and decision-log modules beside
operational runtime, policy, recorder, storage, crash, and CLI surfaces.

The current co-location is acceptable as a transitional state only where the
module is a storage writer, crash capture hook, or policy hot-path recorder that
must sit next to the owning subsystem. It should not be the long-term boundary
for portable evidence types, hash-chain audit logic, policy decision history,
forensic exports, rehearsal packages, or traceability verification.

The recommended strategy is a staged extraction rather than one broad move:

1. Extract stable audit/evidence DTOs into a leaf crate.
2. Move recorder and policy audit engines behind that leaf API.
3. Move forensics, rehearsal, cutover, and traceability evidence modules into a
   dedicated evidence/forensics crate.
4. Split retention policy models from live deletion/cleanup execution.
5. Split the storage audit query/persistence surface after the storage split
   has made writer ownership smaller.
6. Decouple `frankenterm-core-ars` from operational core types even though it is
   already physically extracted.

## Source Commands

```bash
rg --files crates/frankenterm-core/src crates/frankenterm-core-ars/src \
  | rg '(audit|forensic|evidence|ars|decision|ledger|policy.*log|recorder.*audit|proof|trace|history|report)'

wc -c crates/frankenterm-core/src/{recorder_audit.rs,recorder_retention.rs,session_retention.rs,policy_decision_log.rs,forensic_export.rs,resize_crash_forensics.rs,migration_rehearsal.rs,cutover_evidence.rs,canary_rehearsal.rs,bayesian_ledger.rs,traceability_verification.rs,reports.rs} \
  crates/frankenterm-core-ars/src/*.rs

rg -n "recorder_audit|recorder_retention|session_retention|policy_decision_log|forensic_export|resize_crash_forensics|migration_rehearsal|cutover_evidence|canary_rehearsal|bayesian_ledger|traceability_verification|reports" \
  crates/frankenterm-core/src crates/frankenterm/src -g'*.rs'
```

## Inventory

The core-side candidate set scanned here is 596,509 bytes. The ARS extracted
crate side is 585,898 bytes including `lib.rs`. The bead's 239KB figure should
therefore be treated as a lower-bound estimate, not the live total.

| Module | Size | Current role | Boundary classification |
| --- | ---: | --- | --- |
| `recorder_audit.rs` | 75,949 bytes | Tamper-evident recorder audit hash chain, access tiers, authorization decisions, audit entries. | Audit engine plus DTOs. Should move after DTO extraction. |
| `recorder_retention.rs` | 56,009 bytes | Recorder data retention, partitioning, archival lifecycle, sensitivity tiers, purge planning. | Mixed policy/execution. Policy DTOs should move; live deletion execution stays near recorder storage until storage boundary is smaller. |
| `session_retention.rs` | 41,405 bytes | Session persistence cleanup by age, count, size, and orphan rows. | Operational cleanup with policy config. Keep deletion execution near storage, but split retention contracts from cleanup mechanics. |
| `policy_decision_log.rs` | 21,364 bytes | Bounded append-only policy decision history used by `policy.rs` and config. | Audit engine plus DTOs. Should move after policy-facing DTO split. |
| `forensic_export.rs` | 36,383 bytes | Canonical forensic records and export/query model for compliance reconstruction. | Evidence DTO/query surface. Should move to forensics/evidence crate. |
| `resize_crash_forensics.rs` | 35,148 bytes | Process-global resize crash context consumed by crash bundle writing. | Operational capture hook plus forensic DTOs. DTOs can move; hook can stay near crash/resize until an adapter exists. |
| `migration_rehearsal.rs` | 45,626 bytes | Migration rehearsal scenarios, execution, drill metrics, reports. | Evidence/rehearsal domain. Should move out of core operational module set. |
| `cutover_evidence.rs` | 62,272 bytes | Go/no-go evidence package, prerequisites, regression gates, risk registry. | Evidence package domain. Should move out of operational core. |
| `canary_rehearsal.rs` | 60,368 bytes | Canary rollout rehearsal and fail-safe drill modeling. | Evidence/rehearsal domain. Should move with cutover evidence. |
| `bayesian_ledger.rs` | 54,424 bytes | Pane-state classifier with evidence ledger explaining state decisions. | Mixed: operational classifier plus explainability ledger. Keep classifier near runtime until a clean state DTO exists; ledger types should move. |
| `traceability_verification.rs` | 54,228 bytes | Static traceability matrix verification types. | Standalone evidence verification. Move to evidence/forensics crate. |
| `reports.rs` | 53,333 bytes | Markdown session reports over events, workflows, gaps, and audit highlights. | Reporting adapter over storage/policy. Keep as core facade or move after storage/report API split. |
| `frankenterm-core-ars/src/*.rs` | 585,898 bytes | Automated reasoning modules including evidence ledger, replay, symbolic exec, drift, explainability, and workflow interception. | Physically extracted, but still depends on `frankenterm-core` for operational types. Needs dependency decoupling, not another physical move. |

## Coupling Findings

`recorder_audit.rs` imports `crate::policy::ActorKind`,
`crate::recorder_storage::RecorderBackendKind`, and
`crate::tuning_config::AuditTuning`. That makes portable audit records depend on
policy, recorder storage, and tuning internals. The hash-chain engine should be
separable from these operational owners.

`policy_decision_log.rs` imports `crate::policy::{ActionKind, ActorKind,
PolicySurface}` and `crate::policy_dsl::DslDecision`. `policy.rs` also imports
`PolicyDecisionLog`, exposes `PolicyDecisionEntry` in status snapshots, and
serializes `DecisionLogConfig` through `config.rs`. The direction is currently
operational policy owns its forensic history implementation; the better boundary
is policy emits a policy decision DTO and the audit engine records it.

`recorder_retention.rs` imports recorder redaction and provides retention
classes, sensitivity tiers, purge plans, and lifecycle transitions. It is partly
audit/retention policy and partly operational retention execution. Its policy
types are portable; its purge execution belongs near recorder storage until
storage exposes a smaller retention adapter.

`session_retention.rs` imports `rusqlite::Connection` and deletes session,
checkpoint, and pane-state rows directly. That deletion path is operational and
should stay near storage/session persistence. The retention config and result
contracts should be separable so review of retention policy does not require
reading live cleanup SQL.

`resize_crash_forensics.rs` imports resize scheduler types and maintains a
process-global singleton included by `crash::write_crash_bundle`. That capture
hook is an acceptable local coupling because it records live scheduler state for
panic diagnostics. The portable structs should still move behind a forensics
DTO boundary so crash bundles do not need to import scheduler internals
directly.

`reports.rs` imports `StorageHandle`, storage query types, and policy redaction
helpers. It is a facade over operational storage and should not move until
storage exposes a narrower reporting API. Moving it before the storage split
would mostly re-create the same dependency edge in another crate.

`storage.rs` owns `audit_actions`, `action_undo`, `workflow_step_logs`,
`policy_denied_audit`, and `secret_scan_reports` DDL plus insert/query helpers.
SQLite connection confinement makes the DDL and writer command handling
reasonable to keep in storage for now. Query DTOs and audit-specific builders
should still move behind a dedicated storage audit module as `ft-dn2tu` reduces
the size of `storage.rs`.

`frankenterm-core-ars` is already a separate crate, but its own `lib.rs` states
that it depends on `frankenterm-core` for `mdl_extraction`, `token_bucket`, and
`workflows` types. That is physically better than co-location, but it is not a
clean reasoning/evidence boundary because the extracted crate still consumes
operational core internals.

## Recommended Extraction Plan

### Phase 1: audit DTO leaf

Create a small leaf crate such as `frankenterm-core-audit-types` for stable
serializable records:

- recorder audit entries, event kinds, access tiers, authorization outcomes
- policy decision entries, decision outcomes, decision log config/snapshot
- forensic actor/action/target/outcome/correlation/sensitivity records
- recorder/session retention policy records and cleanup summaries, excluding
  direct SQL execution
- evidence package and traceability matrix record types that are pure serde/std

This crate should not depend on `frankenterm-core`. Operational modules should
convert from their internal enums into these DTOs at the boundary.

Follow-on: `ft-rqu5e`.

### Phase 2: audit engines

Move the stateful audit engines after DTOs are leaf-clean:

- `AuditLog`, hash-chain verification, and retention config from
  `recorder_audit.rs`
- `PolicyDecisionLog` bounded append/eviction logic from
  `policy_decision_log.rs`

The policy and recorder call sites should emit typed records to the audit engine
rather than importing the audit implementation as part of policy or recorder
business logic.

Follow-on: `ft-kldww`.

### Phase 3: forensics and rehearsal evidence

Move evidence assembly and rehearsal modules together:

- `forensic_export.rs`
- `resize_crash_forensics.rs` DTOs, leaving only the scheduler/crash hook in core
  until an adapter exists
- `migration_rehearsal.rs`
- `cutover_evidence.rs`
- `canary_rehearsal.rs`
- `traceability_verification.rs`

These modules mostly model evidence, reports, and verification artifacts. They
should consume operational snapshots instead of living next to runtime and
policy implementation modules.

Follow-on: `ft-mq7fl`.

### Phase 4: retention lifecycle boundaries

Split recorder and session retention into policy contracts and execution
adapters:

- retention configs, sensitivity tiers, lifecycle classes, purge plans, and
  cleanup summaries can move to audit/retention types
- direct SQLite deletion, recorder segment mutation, and active-session safety
  checks stay near storage/session persistence until those owners expose
  adapter traits

Follow-on: `ft-xcsm0`.

### Phase 5: storage audit boundary

Do not move SQLite writer ownership as part of this bead. The operational
storage writer owns connection confinement and migrations. Instead, as the
storage split proceeds, isolate the audit-specific record/query surface:

- `audit_actions`
- `workflow_step_logs`
- `policy_denied_audit`
- `secret_scan_reports`
- undo/audit query helpers

The goal is for storage to persist audit records without also owning the
forensic domain model.

Follow-on: `ft-4ses2`.

### Phase 6: ARS dependency decoupling

Keep `frankenterm-core-ars` physically extracted, but remove its dependency on
operational core types where practical. Stable workflow, token bucket,
extraction, and evidence DTOs should move into leaf types crates or ARS-owned
contracts so ARS consumes reasoning/evidence contracts instead of core runtime
internals.

Follow-on: `ft-nsoxc`.

## Acceptable Co-location Until Then

- Crash and resize capture hooks may remain near their operational producers
  while they snapshot live process state.
- Storage DDL and writer commands may remain in `storage.rs` until storage's own
  split makes the writer boundary smaller.
- Direct retention cleanup and purge execution may remain near storage while
  they own live deletion safety and active-session checks.
- Report facades that directly query `StorageHandle` may remain in core until a
  narrow reporting storage API exists.
- Operational classifiers such as `BayesianClassifier` may remain near pane
  state logic while their ledger/explainability DTOs are extracted.

## Non-goals

- Do not move ARS back into core.
- Do not create compatibility shims for old module paths unless a migration
  phase needs a short-lived re-export.
- Do not split `storage.rs` broadly under this bead; this audit only identifies
  the audit-specific storage boundary.
- Do not replace project async or storage ownership patterns while extracting
  evidence types.

## Verification

This bead is a documentation and follow-on planning audit. The verification is
the source scan above plus the follow-on Beads:

| Bead | Scope |
| --- | --- |
| `ft-rqu5e` | Extract audit DTOs into a leaf audit-types crate. |
| `ft-kldww` | Extract recorder and policy audit engines. |
| `ft-mq7fl` | Extract forensics and rehearsal evidence modules. |
| `ft-xcsm0` | Split recorder and session retention lifecycle boundaries. |
| `ft-4ses2` | Split storage audit persistence/query surface. |
| `ft-nsoxc` | Decouple `frankenterm-core-ars` from operational core types. |
