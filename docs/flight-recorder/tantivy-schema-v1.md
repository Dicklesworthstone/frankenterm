# Recorder Tantivy Schema v1 (`ft-oegrb.4.1`)

Date: 2026-02-12  
Status: Drafted design contract for lexical indexing implementation  
Owners: `GentleSnow` (+ follow-on indexing implementers)

## Purpose
Define the canonical Tantivy projection schema for recorder events so downstream work (`.4.2`..`.4.6`) can implement ingestion, query, and compatibility tests without schema drift.

This schema is for lexical retrieval over canonical recorder events (`ft.recorder.event.v1`) and is intentionally projection-only: the append log remains source-of-truth.

## Contract IDs
- Recorder event contract: `ft.recorder.event.v1`
- Tantivy projection contract: `ft.recorder.lexical.v1`

Any breaking change to field names/types/tokenizers/index options must increment `ft.recorder.lexical.vN` and trigger generation rebuild.

## Document Model
One Tantivy document per canonical recorder event.

Deterministic identity:
- `event_id` remains the primary logical key.
- `log_offset` is the physical replay pointer from append log.
- `sequence` + `pane_id` preserve deterministic per-pane ordering semantics.

## Field Catalog (v1)

| Field | Tantivy type | Indexed | Stored | Fast field | Tokenizer / options | Required | Source |
|---|---|---|---|---|---|---|---|
| `schema_version` | `STRING` | yes | no | no | exact term | yes | event envelope |
| `lexical_schema_version` | `STRING` | yes | no | no | exact term | yes | index writer constant (`ft.recorder.lexical.v1`) |
| `event_id` | `STRING` | yes | yes | no | exact term | yes | event envelope |
| `pane_id` | `u64` | yes | yes | yes | numeric | yes | event envelope |
| `session_id` | `STRING` | yes | yes | no | exact term | no | event envelope |
| `workflow_id` | `STRING` | yes | yes | no | exact term | no | event envelope |
| `correlation_id` | `STRING` | yes | yes | no | exact term | no | event envelope |
| `parent_event_id` | `STRING` | yes | no | no | exact term | no | `causality` |
| `trigger_event_id` | `STRING` | yes | no | no | exact term | no | `causality` |
| `root_event_id` | `STRING` | yes | no | no | exact term | no | `causality` |
| `source` | `STRING` | yes | yes | no | exact term | yes | event envelope |
| `event_type` | `STRING` | yes | yes | no | exact term | yes | event envelope |
| `ingress_kind` | `STRING` | yes | yes | no | exact term | no | ingress variants |
| `segment_kind` | `STRING` | yes | yes | no | exact term | no | egress variants |
| `control_marker_type` | `STRING` | yes | yes | no | exact term | no | control variants |
| `lifecycle_phase` | `STRING` | yes | yes | no | exact term | no | lifecycle variants |
| `is_gap` | `bool` | yes | yes | yes | boolean | no | egress variants |
| `redaction` | `STRING` | yes | yes | no | exact term | no | text-bearing variants |
| `occurred_at_ms` | `i64` | yes | yes | yes | numeric | yes | event envelope |
| `recorded_at_ms` | `i64` | yes | yes | yes | numeric | yes | event envelope |
| `sequence` | `u64` | yes | yes | yes | numeric | yes | event envelope |
| `log_offset` | `u64` | yes | yes | yes | numeric | yes | append-log metadata |
| `text` | `TEXT` | yes | yes | no | `ft_terminal_text_v1`, positions enabled | no | text-bearing variants |
| `text_symbols` | `TEXT` | yes | no | no | `ft_terminal_symbols_v1`, positions enabled | no | derived from `text` |
| `details_json` | `TEXT` | no | yes | no | stored only | no | `details` object serialized |

Notes:
- `text` and `text_symbols` are both indexed to handle natural language + symbol-heavy terminal text.
- `details_json` is stored only for payload retrieval/debug and is not part of lexical ranking in v1.
- Optional fields are omitted when null; no synthetic placeholder token is indexed.

## Tokenizer Strategy

### `ft_terminal_text_v1` (default body tokenizer)
Goal: robust lexical recall for commands, logs, and natural language mixed in terminal output.

Pipeline:
1. Regex tokenizer preserving common CLI/path tokens: `[A-Za-z0-9_./:-]+`
2. Lowercase filter
3. ASCII folding filter
4. Remove-long filter (hard cap to prevent pathological tokens)

Why:
- Keeps paths like `src/main.rs:42`, flags like `--format`, and namespaces like `foo::bar`.
- Avoids language stemming to prevent code/token corruption.

### `ft_terminal_symbols_v1` (symbol-heavy companion field)
Goal: improve recall for stack traces, module paths, command flags, and punctuated identifiers.

Pipeline:
1. Regex tokenizer biased for symbols: `[A-Za-z0-9_./:\\-]+`
2. Lowercase filter

Why:
- Captures search intents that fail under plain word tokenization (`panic!`, `--timeout-secs`, `std::io`).

## Queryability and Ranking Rules (v1)
- Multi-field lexical query over `text` and `text_symbols`.
- Default boosts:
  - `text`: `1.0`
  - `text_symbols`: `1.25` for terminal-symbol precision.
- Filtering is term/range-based on exact/numeric fields (`pane_id`, `event_type`, `source`, `occurred_at_ms`, etc.).
- Snippet generation uses positions from `text` field.
- Final tie-break ordering for equal scores:
  1. `occurred_at_ms` descending
  2. `sequence` descending
  3. `log_offset` descending

## Compatibility and Migration

### Breaking change triggers
Any of the following requires `ft.recorder.lexical.v(N+1)`:
- field added/removed/renamed
- field type change
- `TEXT` index option changes affecting postings/positions
- tokenizer definition change for indexed fields

### Rollout behavior
1. Detect schema/version mismatch at index open.
2. Create new index generation directory keyed by lexical schema version/hash.
3. Rebuild from canonical append log (`log_offset=0` to head).
4. Atomically switch active generation pointer when build validates.
5. Keep prior generation read-only until retention policy purges it.

This keeps lexical projection rebuildable and non-destructive.

## Golden Test Plan (contract-level)
v1 requires fixed fixtures asserting both compatibility and query behavior.

### Compatibility fixtures
1. **Schema fingerprint stability**
   - Same field order/options/tokenizers => identical fingerprint.
2. **Index-open compatibility**
   - Existing v1 index opens without rebuild.
3. **Breaking change detection**
   - Tokenizer or field-set change forces rebuild path.

### Queryability fixtures
1. **CLI flag retrieval**
   - Query: `--timeout-secs`
   - Expected: matching ingress/egress docs where flag appears.
2. **Stack trace path retrieval**
   - Query: `src/runtime.rs:1450`
   - Expected: stack trace/log events with that location.
3. **Namespace retrieval**
   - Query: `std::io::Error`
   - Expected: symbol-aware hits via `text_symbols`.
4. **Filter correctness**
   - Query: `panic` + filters (`pane_id`, `event_type=egress_output`, time range)
   - Expected: only constrained subset returned.
5. **Ordering determinism**
   - Equal-score docs remain stably ordered via timestamp/sequence/offset tie-break.

## Downstream Mapping
- `ft-oegrb.4.2`: incremental ingestion from canonical offsets using this document model.
- `ft-oegrb.4.3`: commit/merge tuning must preserve query semantics above.
- `ft-oegrb.4.4`: reindex tooling follows the migration behavior in this document.
- `ft-oegrb.4.5`: lexical query service uses this field and boost contract.
- `ft-oegrb.4.6`: quality harness should encode the golden queries listed above.
