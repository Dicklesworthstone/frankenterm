# `.ftreplay` Schema v1 Specification

> Defines the file format, section layout, and field-level schema for
> deterministic replay artifacts. This schema captures everything needed
> to reproduce detection, workflow, and policy decision sequences offline.

**Bead:** ft-og6q6.2.1
**Status:** Living document
**Parent:** ft-og6q6.2 (T1 — Capture Artifact and Trace Data Plane)
**Consumed by:** Replay kernel (T2), counterfactual engine (T3), decision-diff (T4)

---

## 1. Design Principles

1. **Determinism first.** Every field needed for deterministic replay is
   required. Convenience fields are optional.
2. **Self-describing.** The file contains its own schema version, so the
   replay kernel can detect incompatibility without external metadata.
3. **Streaming-friendly.** The timeline section is JSONL (one event per line),
   allowing incremental reads and bounded memory usage.
4. **Auditable.** Integrity metadata (checksums, event counts) allows
   verification without replaying.
5. **Redaction-aware.** The sensitivity tier is declared in the header;
   consumers can reject artifacts above their clearance.

---

## 2. File Structure

An `.ftreplay` file is a concatenation of four sections, each separated
by a section marker line.

```
--- ftreplay-header ---
{header JSON object, single line}
--- ftreplay-entities ---
{entity JSON object, one per line (JSONL)}
--- ftreplay-timeline ---
{event JSON object, one per line (JSONL), sorted by RecorderMergeKey}
--- ftreplay-decisions ---
{decision JSON object, one per line (JSONL), sorted by position}
--- ftreplay-footer ---
{footer JSON object, single line}
```

### 2.1 Section Markers

Section markers are lines matching the exact pattern:
```
--- ftreplay-<section_name> ---
```

The parser must reject files with missing or out-of-order section markers.
Unknown section names after the footer are ignored (forward compatibility).

### 2.2 Encoding

- All text is UTF-8.
- Each JSON object occupies exactly one line (no pretty-printing).
- No blank lines within sections (blank lines between sections are tolerated).

---

## 3. Header Section

The header is a single JSON object containing capture metadata.

```json
{
  "schema_version": "ftreplay.v1",
  "format_version": 1,
  "created_at": "2026-02-22T10:30:00.000Z",
  "created_by": "ft watch v0.4.0",
  "ft_version": "0.4.0",
  "ft_commit": "abc123def456",

  "capture": {
    "session_id": "sess-abc123",
    "hostname": "devbox-01",
    "os": "Darwin 25.2.0",
    "started_at_ms": 1740222600000,
    "ended_at_ms": 1740226200000,
    "duration_ms": 3600000,
    "capture_mode": "full"
  },

  "content": {
    "event_count": 15230,
    "decision_count": 472,
    "entity_count": 8,
    "pane_count": 6,
    "stream_domains": 24,
    "gap_count": 2,
    "clock_anomaly_count": 0
  },

  "sensitivity": {
    "tier": "T1",
    "redaction_applied": true,
    "redaction_version": "1.0",
    "redaction_patterns_checked": 12,
    "redactions_made": 47
  },

  "integrity": {
    "timeline_sha256": "abcdef0123456789...",
    "decisions_sha256": "fedcba9876543210...",
    "entities_sha256": "1234567890abcdef..."
  }
}
```

### 3.1 Header Field Reference

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `schema_version` | string | Yes | Always `"ftreplay.v1"` for this schema |
| `format_version` | integer | Yes | Always `1` for this schema |
| `created_at` | ISO 8601 | Yes | When the artifact was produced |
| `created_by` | string | Yes | Tool and version that created it |
| `ft_version` | string | Yes | FrankenTerm version |
| `ft_commit` | string | Yes | Git commit hash |
| `capture.session_id` | string | Yes | Unique capture session identifier |
| `capture.hostname` | string | Yes | Machine that ran the capture (may be redacted) |
| `capture.os` | string | Yes | Operating system |
| `capture.started_at_ms` | u64 | Yes | Capture start (epoch ms) |
| `capture.ended_at_ms` | u64 | Yes | Capture end (epoch ms) |
| `capture.duration_ms` | u64 | Yes | `ended_at_ms - started_at_ms` |
| `capture.capture_mode` | string | Yes | `"full"` or `"filtered"` |
| `content.event_count` | u64 | Yes | Total events in timeline section |
| `content.decision_count` | u64 | Yes | Total entries in decisions section |
| `content.entity_count` | u64 | Yes | Total entries in entities section |
| `content.pane_count` | u64 | Yes | Distinct pane IDs |
| `content.stream_domains` | u64 | Yes | Distinct `(pane_id, stream_kind)` pairs |
| `content.gap_count` | u64 | Yes | Events with `is_gap=true` |
| `content.clock_anomaly_count` | u64 | Yes | Events with clock anomaly markers |
| `sensitivity.tier` | string | Yes | `"T1"` (standard), `"T2"` (elevated), `"T3"` (restricted) |
| `sensitivity.redaction_applied` | bool | Yes | Whether redaction was applied |
| `integrity.timeline_sha256` | string | Yes | SHA-256 of all timeline lines concatenated |
| `integrity.decisions_sha256` | string | Yes | SHA-256 of all decision lines concatenated |
| `integrity.entities_sha256` | string | Yes | SHA-256 of all entity lines concatenated |

---

## 4. Entities Section

The entities section declares all panes and sessions referenced in the
timeline. One JSON object per line.

### 4.1 Pane Entity

```json
{
  "entity_type": "pane",
  "pane_id": 42,
  "pane_uuid": "550e8400-e29b-41d4-a716-446655440000",
  "title": "agent-codex-01",
  "domain": "local",
  "workspace": "/home/user/project",
  "created_at_ms": 1740222600000,
  "closed_at_ms": 1740226100000,
  "agent_type": "codex",
  "agent_provider": "openai",
  "initial_size": { "rows": 24, "cols": 80 },
  "metadata": {}
}
```

### 4.2 Session Entity

```json
{
  "entity_type": "session",
  "session_id": "sess-abc123",
  "started_at_ms": 1740222600000,
  "pane_ids": [42, 43, 44, 45, 46, 47],
  "metadata": {}
}
```

### 4.3 Entity Field Reference

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `entity_type` | string | Yes | `"pane"` or `"session"` |
| `pane_id` | u64 | Pane only | Stable pane identifier |
| `pane_uuid` | string | No | Optional UUID |
| `title` | string | No | Pane title (may be redacted) |
| `domain` | string | No | WezTerm domain |
| `workspace` | string | No | Working directory (may be redacted) |
| `created_at_ms` | u64 | No | Pane creation time |
| `closed_at_ms` | u64 | No | Pane close time (null if still open at capture end) |
| `agent_type` | string | No | Detected agent type |
| `agent_provider` | string | No | Agent provider (openai, anthropic, etc.) |
| `initial_size` | object | No | Terminal dimensions at pane creation |
| `session_id` | string | Session only | Session identifier |
| `pane_ids` | array[u64] | Session only | Panes in this session |
| `metadata` | object | Yes | Extension point (arbitrary key-value pairs) |

---

## 5. Timeline Section

The timeline is the core of the replay artifact. It contains all events
in `RecorderMergeKey` order, one JSON object per line.

### 5.1 Timeline Entry Format

Each line is a `RecorderEvent` serialized to JSON, augmented with
merge-key fields:

```json
{
  "schema_version": "ft.recorder.event.v1",
  "event_id": "a1b2c3d4e5f6...",
  "pane_id": 42,
  "session_id": "sess-abc123",
  "workflow_id": null,
  "correlation_id": null,
  "source": "capture",
  "occurred_at_ms": 1740222601234,
  "recorded_at_ms": 1740222601235,
  "sequence": 1,
  "causality": {
    "parent_event_id": null,
    "trigger_event_id": null,
    "root_event_id": "a1b2c3d4e5f6..."
  },
  "stream_kind": "egress",
  "merge_position": 0,

  "egress_output": {
    "text": "$ ls -la\n",
    "encoding": "utf8",
    "redaction": "none",
    "segment_kind": "delta",
    "is_gap": false
  }
}
```

### 5.2 Timeline-Specific Fields

These fields are added to the standard `RecorderEvent` for the replay
artifact format:

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `stream_kind` | string | Yes | `"lifecycle"`, `"control"`, `"ingress"`, `"egress"` |
| `merge_position` | u64 | Yes | 0-indexed position in merge-key order |

### 5.3 Payload Encoding

The payload is stored as a named sub-object rather than flattened, to
avoid field name collisions:

| Payload Variant | Sub-Object Key | Fields |
|----------------|---------------|--------|
| `IngressText` | `ingress_text` | `text`, `encoding`, `redaction`, `ingress_kind` |
| `EgressOutput` | `egress_output` | `text`, `encoding`, `redaction`, `segment_kind`, `is_gap` |
| `ControlMarker` | `control_marker` | `control_marker_type`, `details` |
| `LifecycleMarker` | `lifecycle_marker` | `lifecycle_phase`, `reason`, `details` |

Exactly one sub-object key must be present per timeline entry.

### 5.4 Ordering Invariant

Timeline entries MUST appear in strict `RecorderMergeKey` ascending order:

```
For all i < j:
  merge_key(timeline[i]) < merge_key(timeline[j])
```

The replay kernel MUST verify this invariant on load and reject files
that violate it (charter principle 5.4: fail loud).

### 5.5 Gap Markers

Gap markers are standard timeline entries with:
- `segment_kind`: `"gap"`
- `is_gap`: `true`
- `text`: Empty string or gap summary
- `details.gap_reason`: Human-readable reason for the gap

Gap markers occupy a sequence number in their domain. They cannot be
omitted without breaking sequence monotonicity.

---

## 6. Decisions Section

The decisions section records all decision-bearing events extracted from
the timeline, enriched with decision-specific context. One JSON object
per line.

### 6.1 Detection Decision

```json
{
  "decision_type": "detection",
  "timeline_position": 47,
  "event_id": "a1b2c3d4...",
  "pane_id": 42,
  "sequence": 15,
  "occurred_at_ms": 1740222610000,

  "detection": {
    "rule_id": "core.codex:usage_reached",
    "rule_definition_hash": "sha256:abcdef...",
    "agent_type": "codex",
    "event_type": "rate_limit",
    "severity": "warning",
    "confidence": 0.95,
    "matched_text": "Usage limit reached",
    "extracted": { "limit": "100", "current": "100" }
  }
}
```

### 6.2 Workflow Step Decision

```json
{
  "decision_type": "workflow_step",
  "timeline_position": 52,
  "event_id": "b2c3d4e5...",
  "pane_id": 42,
  "sequence": 20,
  "occurred_at_ms": 1740222615000,

  "workflow_step": {
    "workflow_id": "wf-handle-rate-limit",
    "workflow_definition_hash": "sha256:123456...",
    "step_name": "check_alternative_provider",
    "step_index": 2,
    "result": "continue",
    "result_data": null,
    "retry_delay_ms": null,
    "abort_reason": null,
    "trigger_event_id": "a1b2c3d4..."
  }
}
```

### 6.3 Policy Decision

```json
{
  "decision_type": "policy",
  "timeline_position": 55,
  "event_id": "c3d4e5f6...",
  "pane_id": 42,
  "sequence": 22,
  "occurred_at_ms": 1740222618000,

  "policy": {
    "action_kind": "send_text",
    "decision": "allow",
    "rule_id": "policy.default.allow_non_alt",
    "policy_definition_hash": "sha256:789abc...",
    "context": {
      "reason": "non_alt_screen_action",
      "input_hash": "sha256:def012..."
    }
  }
}
```

### 6.4 Decision Field Reference

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `decision_type` | string | Yes | `"detection"`, `"workflow_step"`, `"policy"` |
| `timeline_position` | u64 | Yes | Index into timeline section |
| `event_id` | string | Yes | Event ID from timeline |
| `pane_id` | u64 | Yes | Pane context |
| `sequence` | u64 | Yes | Sequence within domain |
| `occurred_at_ms` | u64 | Yes | Source timestamp |
| `detection.rule_id` | string | Detection | Stable rule identifier |
| `detection.rule_definition_hash` | string | Detection | SHA-256 of rule definition source |
| `detection.severity` | string | Detection | `"info"`, `"warning"`, `"error"`, `"critical"` |
| `detection.confidence` | f64 | Detection | 0.0–1.0 match confidence |
| `workflow_step.workflow_id` | string | Workflow | Workflow instance ID |
| `workflow_step.workflow_definition_hash` | string | Workflow | SHA-256 of workflow definition |
| `workflow_step.result` | string | Workflow | `"continue"`, `"done"`, `"retry"`, `"abort"` |
| `workflow_step.trigger_event_id` | string | Workflow | Event that triggered this workflow |
| `policy.action_kind` | string | Policy | What action was evaluated |
| `policy.decision` | string | Policy | `"allow"`, `"deny"`, `"elevate"` |
| `policy.rule_id` | string | Policy | Rule that produced the decision |
| `policy.policy_definition_hash` | string | Policy | SHA-256 of policy configuration |

### 6.5 Definition Hashes

Every decision includes a `*_definition_hash` field containing the
SHA-256 of the rule/workflow/policy definition that produced it. This
enables the counterfactual engine (T3) to attribute divergences to
specific definition changes (equivalence contract principle 5.3:
evidence over assertion).

---

## 7. Footer Section

The footer is a single JSON object containing verification metadata.

```json
{
  "schema_version": "ftreplay.v1",
  "event_count_verified": 15230,
  "decision_count_verified": 472,
  "entity_count_verified": 8,
  "merge_order_verified": true,
  "sequence_monotonicity_verified": true,
  "causality_integrity_verified": true,
  "integrity_check": {
    "timeline_sha256_match": true,
    "decisions_sha256_match": true,
    "entities_sha256_match": true
  },
  "invariant_report": {
    "violations": 0,
    "warnings": 0,
    "events_checked": 15230,
    "panes_observed": 6,
    "domains_observed": 24
  },
  "finalized_at": "2026-02-22T11:30:00.000Z"
}
```

### 7.1 Footer Field Reference

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `schema_version` | string | Yes | Must match header |
| `event_count_verified` | u64 | Yes | Must match header `content.event_count` |
| `decision_count_verified` | u64 | Yes | Must match header `content.decision_count` |
| `entity_count_verified` | u64 | Yes | Must match header `content.entity_count` |
| `merge_order_verified` | bool | Yes | Whether merge key ordering was verified |
| `sequence_monotonicity_verified` | bool | Yes | Whether per-domain sequence monotonicity holds |
| `causality_integrity_verified` | bool | Yes | Whether causality references are valid |
| `integrity_check.*_match` | bool | Yes | Whether SHA-256 checksums match header |
| `invariant_report` | object | Yes | Summary of `InvariantChecker` run |
| `finalized_at` | ISO 8601 | Yes | When the footer was written |

---

## 8. Validation Rules

### 8.1 On Write (Capture)

The capture pipeline must verify before writing the footer:

1. Timeline is sorted by `RecorderMergeKey`.
2. Per-domain sequence monotonicity holds.
3. All causality references point to existing event IDs.
4. No duplicate event IDs.
5. SHA-256 checksums match computed values.
6. Event/decision/entity counts match.

If any check fails, the artifact is written with a `validation_errors`
array in the footer and a `warnings` count > 0.

### 8.2 On Read (Replay)

The replay kernel must verify before replay:

1. `schema_version` is `"ftreplay.v1"` (fail on mismatch — charter
   principle 5.4).
2. Footer exists and `integrity_check.*_match` are all `true`.
3. `event_count_verified` matches actual line count in timeline section.
4. Merge order is correct (spot-check first, middle, last events or
   full verify if `merge_order_verified` is `false` in footer).

If validation fails, the replay kernel emits `ReplayError::InvalidArtifact`
and halts.

### 8.3 Sensitivity Gate

Before reading timeline/decision content, the replay kernel checks:

1. The artifact's `sensitivity.tier` does not exceed the actor's access tier.
2. T1 artifacts: readable by any authorized actor.
3. T2 artifacts: require elevated access (A2+).
4. T3 artifacts: require restricted access (A3+) and audit log entry.

---

## 9. Schema Evolution

### 9.1 Forward Compatibility

- Unknown JSON fields in any section are ignored (not rejected).
- Unknown section names after the footer are ignored.
- This allows future versions to add fields without breaking v1 readers.

### 9.2 Backward Compatibility

- `schema_version` is checked on read. A v1 reader MUST reject
  `schema_version` values it does not recognize.
- If a future v2 schema is needed, a migration tool converts v1 → v2.
  Cross-version replay is NOT supported (charter non-goal 3.6).

### 9.3 Migration Contract

If schema migration becomes necessary:

1. A `ftreplay-migrate` tool is provided.
2. Migration is lossless: `migrate(v1) → v2` preserves all v1 fields.
3. Migration updates `schema_version`, `format_version`, and re-computes
   integrity checksums.
4. Original v1 artifact is preserved alongside the v2 output (regression
   library append-only principle).

---

## 10. Size and Performance Considerations

### 10.1 Typical Sizes

| Trace Duration | Events | Decisions | Estimated Size |
|---------------|--------|-----------|---------------|
| 5 minutes | ~500 | ~20 | ~200 KB |
| 1 hour | ~15,000 | ~500 | ~6 MB |
| 8 hours | ~120,000 | ~4,000 | ~50 MB |

### 10.2 Compression

`.ftreplay` files may be compressed with gzip or zstd. Compressed files
use extensions `.ftreplay.gz` or `.ftreplay.zst`. The replay kernel
auto-detects compression from file magic bytes.

Compression is applied to the entire file, not per-section.

### 10.3 Chunking

For traces exceeding 100,000 events, the capture pipeline may split into
multiple chunk files:

```
trace-001.ftreplay
trace-002.ftreplay
trace-003.ftreplay
trace.manifest.json  # index with per-chunk metadata and ordering
```

Each chunk is a valid `.ftreplay` file. The manifest provides chunk
ordering and cross-chunk causality references. Chunking details are
specified in ft-og6q6.2.5.

---

## 11. Example: Minimal Valid `.ftreplay`

```
--- ftreplay-header ---
{"schema_version":"ftreplay.v1","format_version":1,"created_at":"2026-02-22T10:00:00Z","created_by":"ft watch v0.4.0","ft_version":"0.4.0","ft_commit":"abc123","capture":{"session_id":"s1","hostname":"dev","os":"Darwin","started_at_ms":1000,"ended_at_ms":2000,"duration_ms":1000,"capture_mode":"full"},"content":{"event_count":1,"decision_count":0,"entity_count":1,"pane_count":1,"stream_domains":1,"gap_count":0,"clock_anomaly_count":0},"sensitivity":{"tier":"T1","redaction_applied":true,"redaction_version":"1.0","redaction_patterns_checked":12,"redactions_made":0},"integrity":{"timeline_sha256":"e3b0c44298fc1c14...","decisions_sha256":"e3b0c44298fc1c14...","entities_sha256":"d7a8fbb307d7809469..."}}
--- ftreplay-entities ---
{"entity_type":"pane","pane_id":1,"metadata":{}}
--- ftreplay-timeline ---
{"schema_version":"ft.recorder.event.v1","event_id":"abc123...","pane_id":1,"session_id":"s1","workflow_id":null,"correlation_id":null,"source":"capture","occurred_at_ms":1500,"recorded_at_ms":1501,"sequence":0,"causality":{"parent_event_id":null,"trigger_event_id":null,"root_event_id":"abc123..."},"stream_kind":"egress","merge_position":0,"egress_output":{"text":"hello\n","encoding":"utf8","redaction":"none","segment_kind":"delta","is_gap":false}}
--- ftreplay-decisions ---
--- ftreplay-footer ---
{"schema_version":"ftreplay.v1","event_count_verified":1,"decision_count_verified":0,"entity_count_verified":1,"merge_order_verified":true,"sequence_monotonicity_verified":true,"causality_integrity_verified":true,"integrity_check":{"timeline_sha256_match":true,"decisions_sha256_match":true,"entities_sha256_match":true},"invariant_report":{"violations":0,"warnings":0,"events_checked":1,"panes_observed":1,"domains_observed":1},"finalized_at":"2026-02-22T10:00:01Z"}
```

---

## 12. Acceptance Criteria for This Document (ft-og6q6.2.1)

- [x] File structure with 5 sections and section markers
- [x] Header section: capture metadata, content counts, sensitivity, integrity checksums
- [x] Entities section: pane and session entity schemas
- [x] Timeline section: RecorderEvent format with merge_position and stream_kind
- [x] Decisions section: detection, workflow_step, and policy decision schemas
- [x] Footer section: verification metadata and invariant report
- [x] Definition hashes for counterfactual root cause attribution
- [x] Validation rules for write and read paths
- [x] Sensitivity gate for access control
- [x] Schema evolution: forward/backward compatibility, migration contract
- [x] Size estimates and compression/chunking guidance
- [x] Minimal valid example
