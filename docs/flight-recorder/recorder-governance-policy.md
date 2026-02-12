# Recorder Data Governance Policy

Date: 2026-02-12
Beads: `wa-oegrb.8.3`, `ft-oegrb.8.3`
Status: Accepted policy baseline
Extends: `capture-redaction-policy.md` (wa-oegrb.2.5)

## Purpose

Define the complete governance package for recorder data: retention classes,
access tiers, redaction taxonomy, audit expectations, and privileged access
workflows. This document is the implementation contract for:

- `wa-oegrb.3.5` (retention/partitioning/archival lifecycle)
- `wa-oegrb.6.5` (policy-aware access control in recorder/search interfaces)
- `wa-oegrb.7.6` (security/privacy validation suite)

## Scope

This policy covers all recorder data from capture through deletion:

1. **Data classification** and sensitivity tiers
2. **Retention** windows, partitioning, and archival lifecycle
3. **Access control** tiers and authorization model
4. **Privileged access** workflows and exception handling
5. **Audit** requirements and tamper evidence
6. **Policy-to-implementation traceability**

Out of scope: network-level encryption, host-level OS hardening, CI/CD
pipeline security (covered by separate infrastructure policies).

---

## 1. Data Classification

### 1.1 Sensitivity Tiers

All recorder data is classified into one of three sensitivity tiers:

| Tier | Label | Description | Examples |
|------|-------|-------------|----------|
| T1 | **Standard** | Non-sensitive operational data | Lifecycle markers, control markers, gap events, metadata |
| T2 | **Sensitive** | Contains or may contain PII/secrets after redaction | Redacted ingress/egress text (`redaction=partial`) |
| T3 | **Restricted** | Contains known secrets or unredacted capture | Unredacted text (if `allow_unredacted_capture=true`), raw auth flows |

Tier assignment rules:
- Events with `redaction=none` where no detector matched: **T1**
- Events with `redaction=partial`: **T2**
- Events with `redaction=full`: **T2** (marker-only, but provenance is sensitive)
- Events captured with `allow_unredacted_capture=true`: **T3**
- Lifecycle/control/gap events: **T1**

### 1.2 Redaction Taxonomy

Building on `capture-redaction-policy.md`, the complete redaction taxonomy:

| Class | Scope | Replacement | Tier |
|-------|-------|-------------|------|
| `secret.credential` | API keys, bearer tokens, passwords, DB URL creds | `[REDACTED]` | T3 pre-redaction, T2 post |
| `secret.auth_flow` | OAuth tokens, device codes, auth query params | `[REDACTED]` | T3 pre-redaction, T2 post |
| `secret.session` | Session tokens matching configured patterns | `[REDACTED]` | T3 pre-redaction, T2 post |
| `pii.email` | Email addresses (future v2) | Not redacted in v1 | T2 |
| `pii.path` | Home directory paths (future v2) | Not redacted in v1 | T1 |

Redaction classes `pii.email` and `pii.path` are reserved for future policy
versions. v1 only enforces `secret.*` classes.

---

## 2. Retention Policy

### 2.1 Retention Classes

| Class | Default Window | Applies To | Configurable |
|-------|---------------|------------|--------------|
| `hot` | 24 hours | Active append-log segments (actively written) | Yes |
| `warm` | 7 days | Sealed segments (no longer appended, still queryable) | Yes |
| `cold` | 30 days | Archived segments (queryable with latency penalty) | Yes |
| `purged` | 0 (deleted) | Expired segments beyond cold window | N/A |

### 2.2 Segment Lifecycle

Segments transition through a strict lifecycle:

```
active → sealed → archived → purged
```

Transitions:
- **active → sealed**: When segment reaches size limit or time boundary (configurable).
- **sealed → archived**: After `hot` window expires. Archive means the segment
  file is closed, optionally compressed, and index references remain valid.
- **archived → purged**: After `cold` window expires. Purge deletes the segment
  file and removes index entries referencing purged ordinals.

Invariants:
- A segment MUST NOT be purged while any consumer checkpoint references it.
- Purge MUST update index to remove stale references (tombstone or delete).
- Purge MUST emit an audit event with segment metadata and ordinal range.

### 2.3 Retention Overrides

Per-sensitivity retention overrides:

| Tier | Override | Rationale |
|------|----------|-----------|
| T3 (Restricted) | Maximum 24 hours, then mandatory purge | Minimize exposure of unredacted secrets |
| T2 (Sensitive) | Standard retention classes apply | Redacted data is operationally useful |
| T1 (Standard) | Extended to 90 days if configured | Lifecycle metadata aids long-term diagnostics |

Configuration keys:
- `recorder.retention.hot_hours` (u32, default `24`)
- `recorder.retention.warm_days` (u32, default `7`)
- `recorder.retention.cold_days` (u32, default `30`)
- `recorder.retention.t3_max_hours` (u32, default `24`)
- `recorder.retention.t1_extended_days` (u32, default `30`, max `90`)

### 2.4 Partitioning Strategy

Segments are partitioned by:
1. **Time**: One segment per configurable time window (default: 1 hour).
2. **Size**: Segment rolls when exceeding size limit (default: 256 MB).
3. **Sensitivity**: T3 data MUST be in separate segments from T1/T2 for
   independent lifecycle management.

Partition identity: `segment_id = {start_ordinal}_{sensitivity_tier}_{created_at_ms}`

---

## 3. Access Control

### 3.1 Access Tiers

| Tier | Label | Capabilities | Default Actors |
|------|-------|-------------|----------------|
| A0 | **Public metadata** | Segment count, health status, retention stats | All actors |
| A1 | **Redacted query** | Search/replay over redacted text, standard filters | Robot, MCP, Human (CLI) |
| A2 | **Full query** | All A1 + cross-pane correlation, aggregate analytics | Human (CLI), Workflow |
| A3 | **Privileged raw** | Unredacted text access (if captured), audit log read | Human (explicit approval) |
| A4 | **Admin** | Retention override, purge, policy change, audit export | Human (explicit approval + MFA if configured) |

### 3.2 Actor-to-Tier Mapping

| Actor | Default Tier | Can Elevate To | Elevation Method |
|-------|-------------|----------------|------------------|
| Robot | A1 | A2 (with workflow context) | Workflow authorization |
| MCP | A1 | A2 (with tool context) | MCP capability negotiation |
| Human (CLI) | A2 | A3, A4 | Explicit approval code |
| Workflow | A2 | A3 (with approval) | Approval checkpoint |

### 3.3 Authorization Enforcement Points

Authorization is checked at three enforcement points:

1. **Query surface** (CLI, Robot, MCP): Before executing search/replay.
   - Check actor tier against query requirements.
   - Apply response-stage redaction per viewer tier.
   - Log query metadata to audit trail.

2. **Export surface** (diagnostic bundles, backup): Before including recorder data.
   - Bundles default to A1 (redacted) content.
   - A3+ content requires explicit flag and audit event.

3. **Admin surface** (retention override, purge, policy change):
   - Requires A4 tier.
   - All admin actions emit audit events with actor identity and justification.

### 3.4 Cross-Pane Isolation

By default, each query is scoped to panes the actor has access to:
- Robot/MCP: Only panes in the actor's domain or explicitly granted panes.
- Human: All panes (A2 default).
- Workflow: Panes referenced in the workflow context.

Cross-pane correlation (joining data across pane boundaries) requires A2+.

---

## 4. Privileged Access Workflow

### 4.1 Approval Flow

Privileged access (A3, A4) requires explicit approval:

```
Actor requests elevated access
  → System generates approval code (sha256-based, time-limited)
  → Actor confirms with approval code via CLI/approval prompt
  → System logs approval event with actor, justification, scope, expiry
  → Elevated access granted for scope and duration
  → Access auto-reverts at expiry
```

This reuses the existing `PolicyDecision::RequireApproval` and
`ApprovalRequest` mechanism from `policy.rs`.

### 4.2 Approval Constraints

| Constraint | Value | Rationale |
|-----------|-------|-----------|
| Approval TTL | 15 minutes (default) | Minimize window of elevated access |
| Scope | Single query or bounded time range | Prevent open-ended raw access |
| Audit | Mandatory, non-suppressible | Every privileged access is traceable |
| Justification | Required free-text field | Enables post-hoc review |

Configuration keys:
- `recorder.access.approval_ttl_seconds` (u32, default `900`)
- `recorder.access.require_justification` (bool, default `true`)
- `recorder.access.max_raw_query_rows` (u32, default `100`)

### 4.3 Exception Handling

Exceptions to standard access policy:
- **Incident response mode**: A human operator can enable temporary A3 access
  for all actors in a domain during an active incident. This emits a
  high-priority audit event and auto-expires after the configured window.
- **Debug mode**: `ft` can be started with `--recorder-debug` which enables
  A3 for the current session only. This logs a startup warning and emits
  audit events for every query.

Exception constraints:
- Debug mode MUST NOT be the default.
- Incident response mode requires explicit activation and justification.
- Both modes auto-expire; they cannot be set permanently via config.

---

## 5. Audit Requirements

### 5.1 Audit Event Schema

All policy-relevant operations emit structured audit events:

```
{
  "audit_version": "ft.recorder.audit.v1",
  "event_type": "<audit_event_type>",
  "actor": { "kind": "human|robot|mcp|workflow", "identity": "<id>" },
  "timestamp_ms": <u64>,
  "scope": { "pane_ids": [...], "time_range": {...}, "query": "<redacted>" },
  "decision": "allow|deny|elevate",
  "justification": "<text>",  // for elevated access
  "policy_version": "<version>",
  "details": { ... }
}
```

### 5.2 Auditable Operations

| Operation | Audit Event Type | Required Fields |
|-----------|-----------------|-----------------|
| Query (A1/A2) | `recorder.query` | actor, scope, result count |
| Query (A3 raw) | `recorder.query.privileged` | actor, scope, justification, approval_code |
| Replay | `recorder.replay` | actor, pane_id, time_range |
| Export | `recorder.export` | actor, format, tier, scope |
| Retention override | `recorder.admin.retention_override` | actor, segment_id, new_class, justification |
| Manual purge | `recorder.admin.purge` | actor, segment_range, justification |
| Policy change | `recorder.admin.policy_change` | actor, old_value, new_value, justification |
| Approval grant | `recorder.access.approval_granted` | actor, scope, ttl, justification |
| Approval expire | `recorder.access.approval_expired` | actor, scope |
| Incident mode on | `recorder.access.incident_mode` | actor, domain, ttl, justification |
| Debug mode start | `recorder.access.debug_mode` | actor, session_id |

### 5.3 Audit Integrity

- Audit events are written to a separate append-only log (`audit.log`), not
  the recorder data log.
- Audit log follows the same append-only, checkpoint-based format as recorder
  storage for consistency.
- Audit log is T2 (sensitive) — it contains actor identities and query metadata.
- Audit log retention: minimum 90 days, not subject to standard retention
  classes (separate `recorder.audit.retention_days` config, default `90`).
- Audit entries MUST never contain raw secret values (only redacted summaries
  and metadata).

### 5.4 Tamper Evidence

- Audit log includes running hash chain: each entry includes
  `prev_entry_hash` (SHA-256 of previous entry's canonical JSON).
- Integrity can be verified offline by replaying the hash chain.
- Gap detection: missing entries are detectable via ordinal discontinuity.

---

## 6. Policy-to-Implementation Traceability

### 6.1 Traceability Matrix

| Policy Requirement | Implementation Location | Test Requirement |
|-------------------|------------------------|------------------|
| T1/T2/T3 classification | `recorder_storage.rs` segment metadata | Unit: tier assignment from event |
| Redaction classes | `policy.rs::Redactor` + `recording.rs` | Unit + golden corpus (per capture-redaction-policy.md) |
| Retention windows | `storage_telemetry.rs` + new retention module | Unit: lifecycle transitions |
| T3 max retention | Retention module + purge path | Integration: T3 auto-purge after window |
| Segment partitioning | `recorder_storage.rs` segment roll logic | Unit: roll on time/size/tier boundary |
| Access tiers A0-A4 | Query surfaces (CLI, Robot, MCP) | Integration: tier enforcement per actor |
| Approval workflow | `policy.rs::PolicyDecision::RequireApproval` | Integration: approval grant + expiry |
| Audit events | New audit module + audit log writer | Unit: schema conformance |
| Audit hash chain | Audit log writer | Unit: chain verification |
| Cross-pane isolation | Query filter injection | Integration: actor-scoped queries |

### 6.2 Configuration Surface

All governance-related configuration keys:

```toml
[recorder.redaction]
enabled = true                          # Master redaction switch
policy_version = "redaction.policy.v1"  # Active policy version
allow_unredacted_capture = false        # T3 capture (dangerous)
custom_patterns = []                    # Additional regex patterns

[recorder.retention]
hot_hours = 24                          # Active segment window
warm_days = 7                           # Sealed segment window
cold_days = 30                          # Archived segment window
t3_max_hours = 24                       # Restricted data max retention
t1_extended_days = 30                   # Metadata extended retention (max 90)

[recorder.access]
approval_ttl_seconds = 900              # Privileged access window
require_justification = true            # Require text justification
max_raw_query_rows = 100                # Cap on raw-access result size

[recorder.audit]
retention_days = 90                     # Audit log minimum retention
hash_chain_enabled = true               # Tamper-evidence hash chain
```

---

## 7. Compliance and Data-Handling Rationale

### 7.1 Why Always-On Capture is Acceptable

1. **Redaction by default**: Secrets are scrubbed before persistence. The
   stored data is operationally useful but not a credential store.
2. **Bounded retention**: Data expires automatically. No unbounded accumulation.
3. **Access control**: Raw data (if captured) requires explicit approval with
   audit trail and time limits.
4. **Operator control**: Capture can be disabled entirely, scoped to specific
   panes, or run in redact-only mode.

### 7.2 Data-Handling Principles

1. **Minimize by default**: Capture only what is operationally necessary.
   Redaction removes secrets at the earliest possible stage.
2. **Purpose limitation**: Recorder data is for operational diagnostics,
   incident response, and workflow replay. Not for surveillance or performance
   monitoring of individuals.
3. **Transparency**: Operators can see what is being captured (`ft status`),
   what is being retained (`ft recorder stats`), and who accessed what
   (`ft recorder audit`).
4. **Accountability**: Every access and policy change is audited.
   Privileged access requires justification.

---

## 8. Rollout and Compatibility

### 8.1 Rollout Phases

This governance policy integrates with the phased rollout defined in
`capture-redaction-policy.md`:

| Phase | Governance Gate | Prerequisite |
|-------|----------------|--------------|
| 1. Metadata | Tier classification logic + audit schema | Event schema v1 |
| 2. Capture-stage redaction | Redaction operational + audit events flowing | Redaction policy impl |
| 3. Access control | Tier enforcement in query surfaces | Policy engine integration |
| 4. Full governance | Retention lifecycle + privileged access + hash chain | All above + retention module |

### 8.2 Backward Compatibility

- Recorder data written before governance policy activation is treated as T2
  (sensitive, redaction status unknown).
- Legacy data without `redaction_meta` is served at A1 tier (redacted view)
  until explicitly reclassified.
- Audit log format is versioned (`ft.recorder.audit.v1`); readers degrade
  gracefully on unknown versions.

---

## Non-Goals

- Do not build a general-purpose access control framework. Reuse `policy.rs`.
- Do not implement encryption-at-rest in v1 (defer to OS/filesystem encryption).
- Do not build cross-machine audit aggregation in v1.
- Do not implement automated PII detection beyond `secret.*` classes in v1.

## Exit Criteria for `wa-oegrb.8.3` / `ft-oegrb.8.3`

1. This policy is accepted as the governance baseline.
2. Downstream beads (3.5, 6.5, 7.6) have enough detail to implement without
   rediscovering boundaries.
3. Traceability matrix maps every policy requirement to implementation location
   and test requirement.
4. Configuration surface is fully specified with defaults and constraints.
