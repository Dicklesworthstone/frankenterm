# Semantic Chunking + Windowing Policy v1 (`wa-oegrb.5.2`)

Date: 2026-02-12  
Status: Approved design contract for semantic chunk construction  
Owners: `GentleSnow` (+ semantic index implementers)

## Purpose
Define deterministic chunking/windowing for embedding generation over recorder events so semantic retrieval is reproducible, auditable, and lifecycle-safe.

This contract specifies:
- chunk boundaries
- window sizing/overlap behavior
- required chunk metadata linking back to source log offsets
- edge-case fixture expectations

## Inputs
- `docs/flight-recorder/recorder-event-schema.md`
- `docs/flight-recorder/sequence-correlation-model.md`
- `docs/flight-recorder/embedding-provider-governance-contract.md`
- `docs/flight-recorder/tantivy-schema-v1.md`

## Contract IDs
- Recorder event: `ft.recorder.event.v1`
- Chunking policy: `ft.recorder.chunking.v1`

Breaking changes to boundary semantics, metadata requirements, or deterministic ordering rules require `ft.recorder.chunking.v(N+1)`.

## Deterministic Chunk Model
One chunk is an ordered group of recorder events with a single embedding payload text and canonical source span.

Deterministic key:
- `chunk_id = sha256("{policy_version}:{pane_id}:{direction}:{start_ordinal}:{end_ordinal}:{content_hash}")`

Determinism rule:
- Same input event stream + same policy version + same normalization pipeline => identical chunk sequence and `chunk_id`s.

## Boundary Rules

### Hard boundaries (must split)
1. `pane_id` change
2. direction change (`ingress_text` vs `egress_output`)
3. `is_gap=true` event
4. control/lifecycle events that represent phase boundaries (prompt boundary, compaction markers, lifecycle transition)
5. explicit time discontinuity greater than `hard_gap_ms`

### Soft boundaries (split when threshold exceeded)
1. `max_chunk_chars`
2. `max_chunk_events`
3. `max_window_ms`

### Glue rules (reduce fragmentation)
1. tiny ingress command fragments below `min_chunk_chars` may be merged with immediately following related egress output in same pane within `merge_window_ms`
2. tiny trailing fragments may be attached to preceding chunk if no hard boundary crossed

## Tunable Parameters (v1 defaults)

| Parameter | Default | Meaning |
|---|---:|---|
| `max_chunk_chars` | 1800 | Hard cap on normalized text length per chunk |
| `max_chunk_events` | 48 | Max events per chunk |
| `max_window_ms` | 120000 | Max temporal width of a chunk |
| `hard_gap_ms` | 30000 | Time jump that forces hard split |
| `min_chunk_chars` | 80 | Minimum target size before glue/merge |
| `merge_window_ms` | 8000 | Max adjacency window for glue rule |
| `overlap_chars` | 120 | Prefix overlap from previous chunk for long streams |

## Text Assembly Rules
1. Preserve event order by `(segment_id, ordinal)`.
2. Normalize line endings to `\n`.
3. Prefix each event contribution with compact marker:
   - ingress: `[IN] `
   - egress: `[OUT] `
4. Trim trailing whitespace per line; preserve internal spacing.
5. For overlap windows, prepend last `overlap_chars` (character-safe) from previous chunk text when previous and current chunks share pane+direction and no hard boundary occurred.

## Required Chunk Metadata (non-optional)
Each produced chunk must include:
- `chunk_id`
- `policy_version` (`ft.recorder.chunking.v1`)
- `pane_id`
- `session_id` (nullable)
- `direction` (`ingress` | `egress` | `mixed_glued`)
- `start_offset`: `{segment_id, ordinal, byte_offset}`
- `end_offset`: `{segment_id, ordinal, byte_offset}`
- `event_ids[]` (ordered)
- `event_count`
- `occurred_at_start_ms`
- `occurred_at_end_ms`
- `text_chars`
- `content_hash` (sha256 of normalized chunk text)

Traceability requirement:
- metadata must be sufficient to replay the exact source span from append log without heuristic lookup.

## Reference Algorithm (normative)
```text
for event in ordered_events:
  if hard_boundary(current_chunk, event):
    flush(current_chunk)
    start_new_chunk(event)
    continue

  append_event_text(current_chunk, event)

  if exceeds_soft_limits(current_chunk):
    flush(current_chunk)
    start_new_chunk_with_overlap(previous_chunk, event)

after stream:
  flush(last_chunk)

post-pass glue:
  merge_tiny_adjacent_chunks_when_allowed()
```

## Edge-Case Fixture Expectations

| Case | Input shape | Expected behavior |
|---|---|---|
| Very long single egress burst | 1 event > `max_chunk_chars` | deterministic split into multiple chunks with overlap |
| Tiny command + immediate output | short ingress then egress within `merge_window_ms` | optional `mixed_glued` chunk with full offset trace |
| Mixed directions with delay | ingress then egress after `hard_gap_ms` | hard split, no glue |
| Explicit gap marker | egress with `is_gap=true` | force boundary and start fresh chunk |
| Prompt/control transition | control marker between outputs | hard split at control marker |
| Reindex replay | same source events replayed twice | identical chunk ids/order across runs |

## Validation Harness Requirements
The chunking implementation must include fixture-driven tests:
1. reproducibility test: two runs produce identical chunk sequence + IDs
2. traceability test: each chunk’s source offset span replays to same normalized text hash
3. boundary test suite for all cases in table above
4. overlap consistency test: overlap only on eligible adjacent windows

## Downstream Mapping
- `wa-oegrb.5.3`: vector store uses `chunk_id` + offset span as primary identity
- `wa-oegrb.5.4`: rank fusion explanation may surface chunk boundaries/offset spans
- `wa-oegrb.7.3`: invariants include deterministic chunk replay and offset traceability
