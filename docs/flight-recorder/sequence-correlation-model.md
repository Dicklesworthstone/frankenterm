# Deterministic Sequence and Correlation Model (Recorder v1)

Date: 2026-02-12  
Bead: `wa-oegrb.2.4`  
Status: Accepted baseline contract

## Purpose

Define deterministic ordering and correlation semantics for ingress/egress recorder events so replay and search can reconstruct causality reproducibly.

This contract is intended to unblock:
- `wa-oegrb.7.3` (ordering/completeness/replay invariants)
- storage/index tracks that require stable merge semantics

## Scope

This document defines:
- per-pane sequence domains and assignment rules
- deterministic cross-stream merge order
- causality/correlation field semantics
- race and clock-skew handling
- replay determinism validation plan

## Existing Foundations (Current Code)

- Per-pane egress sequence is already monotonic via `PaneCursor.next_seq` in `crates/frankenterm-core/src/ingest.rs`.
- Sequence discontinuities are detected and recorded as explicit gaps in `persist_captured_segment`.
- Ingress has a monotonic counter (`IngressSequence`) in `crates/frankenterm-core/src/recording.rs`.
- Recorder schema already carries `event_id`, `sequence`, `correlation_id`, and `causality` fields.

## Canonical Ordering Model

### 1. Sequence domains

`sequence` is monotonic within a sequence domain:
- Domain key = `(pane_id, stream_kind)`
- `stream_kind` values:
  - `ingress`
  - `egress`
  - `control`
  - `lifecycle`

Rationale:
- Current implementation produces independent ingress and egress counters.
- Explicit stream domains avoid ambiguity when same-pane events share numeric sequence values.

Required additive metadata:
- `details.sequence_stream` (string; required for text-bearing events)
- `details.sequence_version = "seq.v1"` (string)

### 2. Global deterministic merge order

Replay consumers must merge events using this strict key order:
1. `recorded_at_ms` ascending
2. `pane_id` ascending
3. `details.sequence_stream` rank (`lifecycle` < `control` < `ingress` < `egress`)
4. `sequence` ascending
5. `event_id` lexicographic ascending (final deterministic tie-breaker)

Notes:
- Timestamps are not trusted as sole order source.
- Tie-break chain is total and deterministic.
- When append-log offsets are available (storage track), offset becomes primary sort key; this merge key remains backward-compatible fallback.

### 3. Event identity

`event_id` must be deterministic for identical input traces.

Required generation rule (`event_id.v1`):
- `sha256("{schema_version}|{pane_id}|{sequence_stream}|{sequence}|{event_type}|{occurred_at_ms}|{payload_hash}")`
- encoded as lowercase hex

This avoids random UUID variance across replay runs of identical captures.

## Correlation and Causality Semantics

### Required linkage fields

- `session_id`: stable capture session identifier.
- `workflow_id`: workflow execution ID when applicable.
- `correlation_id`: request/action chain identifier (may span panes).
- `causality.parent_event_id`: immediate predecessor in the same causal chain.
- `causality.trigger_event_id`: event that triggered this event.
- `causality.root_event_id`: root of the chain.

### Causality rules

1. Root events:
   - `parent_event_id = null`
   - `trigger_event_id = null`
   - `root_event_id = event_id`
2. Direct continuations in same stream:
   - `parent_event_id = previous event in same (pane_id, stream_kind)`
3. Cross-stream response (e.g., egress caused by ingress):
   - `trigger_event_id = ingress event_id`
   - `root_event_id = ingress.root_event_id`
4. Gap/control/lifecycle markers:
   - must still carry causal fields, even when text is empty.

## Gap and Discontinuity Semantics

Gap events are first-class and must participate in ordering and causality:
- `segment_kind = gap`
- `is_gap = true`
- `details.gap_reason` populated
- sequence increments like any other segment

Sequence discontinuity handling:
- If storage sequence differs from captured sequence, record explicit `seq_discontinuity` gap.
- Resync cursor to `storage_seq + 1` before continuing normal capture.

This preserves replay determinism under crashes/restarts/races.

## Clock Skew and Race Handling

### Clock behavior

- `occurred_at_ms` may be non-monotonic due to source/runtime jitter.
- `recorded_at_ms` may also collide under high throughput.
- Neither field alone is authoritative for replay order.

### Required flags (additive)

- `details.clock_anomaly` (bool)
- `details.clock_anomaly_reason` (string|null)

Set when:
- `occurred_at_ms` regresses within same `(pane_id, stream_kind)`
- or exceeds a configured future-skew threshold.

Replay order remains sequence/merge-key driven regardless of clock anomalies.

## Implementation Requirements by Surface

Ingress path (`policy.rs` + recorder ingress tap):
- assign `sequence_stream="ingress"`
- deterministic `event_id.v1`
- causal links from workflow/policy context when present

Egress path (`tailer.rs` + `ingest.rs`):
- assign `sequence_stream="egress"`
- propagate gap reasons and discontinuity markers
- deterministic `event_id.v1`

Control/lifecycle producers:
- assign corresponding stream domain
- preserve tie-break determinism with same merge key

## Replay Determinism Contract

For identical input capture traces and policy/config versions:
1. produced event multiset is identical
2. merged replay order is identical
3. reconstructed causal chains are identical

Any violation is a regression.

## Validation Plan

### Unit
- per-domain sequence monotonicity
- deterministic `event_id.v1` for fixed fixtures
- merge comparator total-order stability

### Property
- randomized interleavings still produce stable merge order with comparator
- idempotent replay ordering on repeated runs

### Integration
- ingress->egress causal linkage survives concurrent panes
- gap and `seq_discontinuity` markers appear and maintain order invariants
- restart/resync path produces stable continuation (`storage_seq + 1`)

### Golden replay fixtures
- fixed event corpus with expected merged order list
- fixed causality graph snapshots (`parent/trigger/root`)

## Exit Criteria for `wa-oegrb.2.4`

1. Ordering and correlation rules are explicit and testable.
2. Merge key defines total deterministic order.
3. Causality/gap/clock-anomaly behavior is specified for implementation teams.
