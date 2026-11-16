# Rio Terminal Core Analysis

Bead: `ft-34sko.3`  
Scope: `legacy_rio/rio`

## 1. Parser -> Performer -> State Dataflow

1. **PTY ingest and parser entry**
   - `Machine::pty_read` reads PTY bytes, acquires terminal lock, and calls `state.parser.advance(...)` (`legacy_rio/rio/rio-backend/src/performer/mod.rs:168`, `legacy_rio/rio/rio-backend/src/performer/mod.rs:207`).
   - The parser state object in the IO loop is `handler::Processor` (`legacy_rio/rio/rio-backend/src/performer/mod.rs:75`).

2. **Processor layer (sync-update aware)**
   - `Processor` wraps `BatchedParser<1024>` (`legacy_rio/rio/rio-backend/src/performer/handler.rs:485`, `legacy_rio/rio/rio-backend/src/performer/handler.rs:487`).
   - It routes input through normal parsing or synchronized-update buffering (`legacy_rio/rio/rio-backend/src/performer/handler.rs:510`, `legacy_rio/rio/rio-backend/src/performer/handler.rs:588`).
   - Sync-update limits are explicit: `SYNC_UPDATE_TIMEOUT = 150ms`, `SYNC_BUFFER_SIZE = 2MiB` (`legacy_rio/rio/rio-backend/src/performer/handler.rs:30`, `legacy_rio/rio/rio-backend/src/performer/handler.rs:33`).
   - Sync mode is entered from CSI private mode `?2026h` handling and ended with `stop_sync_internal` (including parser flush) (`legacy_rio/rio/rio-backend/src/performer/handler.rs:1041`, `legacy_rio/rio/rio-backend/src/performer/handler.rs:543`).

3. **Batched parser wrapper**
   - `BatchedParser` uses immediate processing for small chunks and batched buffering for larger ones with threshold `1024` bytes (`legacy_rio/rio/rio-backend/src/batched_parser.rs:34`, `legacy_rio/rio/rio-backend/src/batched_parser.rs:39`, `legacy_rio/rio/rio-backend/src/batched_parser.rs:80`).
   - It also tracks throughput statistics (`BatchStats`) (`legacy_rio/rio/rio-backend/src/batched_parser.rs:158`).

4. **Copa parser mechanics**
   - `copa::Parser` is the VT parser front-end (`legacy_rio/rio/copa/src/lib.rs:57`).
   - `advance_until_terminated` allows byte-by-byte early termination using `Perform::terminated` (`legacy_rio/rio/copa/src/lib.rs:142`, `legacy_rio/rio/copa/src/lib.rs:931`).
   - UTF-8 chunk boundaries are handled via `partial_utf8` state and `advance_partial_utf8` (`legacy_rio/rio/copa/src/lib.rs:70`, `legacy_rio/rio/copa/src/lib.rs:723`).

5. **Performer boundary and terminal state mutation**
   - Rio maps parser callbacks into a `Performer` implementing `copa::Perform`, which delegates actions to a `Handler` trait (`legacy_rio/rio/rio-backend/src/performer/handler.rs:642`, `legacy_rio/rio/rio-backend/src/performer/handler.rs:658`, `legacy_rio/rio/rio-backend/src/performer/handler.rs:140`).
   - `Crosswords` is the concrete `Handler` and owns terminal state mutation (`legacy_rio/rio/rio-backend/src/crosswords/mod.rs:405`, `legacy_rio/rio/rio-backend/src/crosswords/mod.rs:1388`).

## 2. Screen/State Model and Damage Semantics

1. **Primary terminal state object**
   - `Crosswords` owns grid state, mode flags, selection, graphics, cursor metadata, and damage tracking (`legacy_rio/rio/rio-backend/src/crosswords/mod.rs:405`).
   - Main grid has history (10_000 lines), alternate grid has no history (`legacy_rio/rio/rio-backend/src/crosswords/mod.rs:448`, `legacy_rio/rio/rio-backend/src/crosswords/mod.rs:449`).

2. **Damage model**
   - Full render invalidation is explicit via `mark_fully_damaged()` and emits `RioEvent::RenderRoute` once per transition (`legacy_rio/rio/rio-backend/src/crosswords/mod.rs:490`).
   - Fine-grained state is tracked through line damage and selection damage updates (`legacy_rio/rio/rio-backend/src/crosswords/mod.rs:509`, `legacy_rio/rio/rio-backend/src/crosswords/mod.rs:560`).
   - Damage state is reset post-render via `reset_damage` (`legacy_rio/rio/rio-backend/src/crosswords/mod.rs:588`).

3. **Mode and private-mode handling**
   - Public mode toggles (`set_mode`/`unset_mode`) and private DEC modes are separated (`legacy_rio/rio/rio-backend/src/crosswords/mod.rs:1390`, `legacy_rio/rio/rio-backend/src/crosswords/mod.rs:1407`, `legacy_rio/rio/rio-backend/src/crosswords/mod.rs:1446`, `legacy_rio/rio/rio-backend/src/crosswords/mod.rs:1528`).
   - Alt-screen transitions are centralized in `swap_alt`, with keyboard mode stack swap and full damage (`legacy_rio/rio/rio-backend/src/crosswords/mod.rs:1177`).

## 3. Correctness and Edge-Case Handling

1. **Wide/zero-width character correctness**
   - `input` branches on display width; zero-width codepoints are attached to the target cell (`push_zerowidth`) (`legacy_rio/rio/rio-backend/src/crosswords/mod.rs:2099`).
   - Wide characters explicitly manage spacer flags and wrap constraints; non-wrapping overflow is guarded (`legacy_rio/rio/rio-backend/src/crosswords/mod.rs:2099`, `legacy_rio/rio/rio-backend/src/crosswords/mod.rs:1004`).
   - Selection extraction handles wide-char spacer edge cases in `line_to_string` (`legacy_rio/rio/rio-backend/src/crosswords/mod.rs:1268`).

2. **Clear/reset behavior**
   - `reset_state` resets grids/tabs/stacks while preserving VI mode and then marks full damage (`legacy_rio/rio/rio-backend/src/crosswords/mod.rs:1930`).
   - `clear_screen` has explicit paths for `Above/Below/All/Saved`, including history/selection interactions (`legacy_rio/rio/rio-backend/src/crosswords/mod.rs:2323`).
   - `clear_line` damages the affected row and filters intersecting selection (`legacy_rio/rio/rio-backend/src/crosswords/mod.rs:2578`).

3. **Selection model stability**
   - Selection transforms under scroll use `rotate(...)` with clamp/drop behavior (`legacy_rio/rio/rio-backend/src/selection.rs:147`).
   - Empty-selection semantics differ by selection type (`legacy_rio/rio/rio-backend/src/selection.rs:206`).
   - Conversion to terminal coordinates is centralized in `to_range` and guarded by grid bounds (`legacy_rio/rio/rio-backend/src/selection.rs:284`).

## 4. Test-Encoded Invariants

1. **Parser UTF-8 boundary invariants**
   - Copa tests assert multi-byte UTF-8 correctness across chunk boundaries and invalid-sequence replacement behavior (`legacy_rio/rio/copa/src/lib.rs:1783`, `legacy_rio/rio/copa/src/lib.rs:1799`, `legacy_rio/rio/copa/src/lib.rs:1851`).

2. **Damage invariants**
   - Crosswords tests check that clear/control actions produce line/full damage rather than cursor-only damage (`legacy_rio/rio/rio-backend/src/crosswords/mod.rs:3193`, `legacy_rio/rio/rio-backend/src/crosswords/mod.rs:3569`, `legacy_rio/rio/rio-backend/src/crosswords/mod.rs:3647`).

3. **Selection invariants**
   - Selection tests encode side-aware semantics and adjacency edge cases (`legacy_rio/rio/rio-backend/src/selection.rs:462`, `legacy_rio/rio/rio-backend/src/selection.rs:483`).
   - Range-intersection behavior is explicitly validated (`legacy_rio/rio/rio-backend/src/selection.rs:785`).
   - Crosswords-level selection serialization is validated for simple/line/block modes (`legacy_rio/rio/rio-backend/src/crosswords/mod.rs:3384`, `legacy_rio/rio/rio-backend/src/crosswords/mod.rs:3459`, `legacy_rio/rio/rio-backend/src/crosswords/mod.rs:3486`).

## 5. FrankenTerm Adoption Recommendations

1. **Keep strict parser/terminal separation with a narrow action trait**
   - Rio’s `Parser` -> `Performer` -> `Handler` boundary isolates protocol decode from state mutation (`legacy_rio/rio/copa/src/lib.rs:828`, `legacy_rio/rio/rio-backend/src/performer/handler.rs:140`, `legacy_rio/rio/rio-backend/src/performer/handler.rs:658`).
   - FrankenTerm should keep this split to simplify audits and protocol evolution.

2. **Adopt bounded synchronized-update buffering with explicit timeout**
   - Rio’s sync-update handling has a hard byte cap and timeout fallback (`legacy_rio/rio/rio-backend/src/performer/handler.rs:30`, `legacy_rio/rio/rio-backend/src/performer/handler.rs:33`, `legacy_rio/rio/rio-backend/src/performer/handler.rs:588`).
   - FrankenTerm should use a similar bounded envelope to avoid unbounded parser buffering under pathological streams.

3. **Codify width/selection/damage semantics as invariants in tests**
   - Rio’s tests directly encode tricky terminal correctness rules (UTF-8 boundary, wide-char/selection, clear-line damage) (`legacy_rio/rio/copa/src/lib.rs:1783`, `legacy_rio/rio/rio-backend/src/crosswords/mod.rs:3193`, `legacy_rio/rio/rio-backend/src/selection.rs:462`).
   - FrankenTerm should mirror this fixture-first invariant style before deeper parser/reflow rewrites.

4. **Use explicit full-vs-partial damage channels at terminal-core boundary**
   - Rio exposes both line-level and full-damage transitions from terminal core (`legacy_rio/rio/rio-backend/src/crosswords/mod.rs:490`, `legacy_rio/rio/rio-backend/src/crosswords/mod.rs:560`).
   - FrankenTerm can reuse this as the control-plane contract for render coalescing.

## 6. Cross-Reference

- `ft-1u90p.3` (layout/reflow correctness track)
- `ft-283h4.4` (I/O and parser pipeline optimization track)

## 7. Downstream Validation Blueprint (Per Adaptation)

Each adaptation from Section 5 must carry explicit unit/property/integration/e2e coverage.

### Adaptation 1: Preserve strict parser/performer/state separation

- Unit tests:
  - action mapping correctness in parser callback adapter (`crates/frankenterm-core/src/patterns.rs` / parser adapter modules as implemented)
  - mutation dispatch path tests in terminal state handlers (`crates/frankenterm-core/src/screen_state.rs`)
- Property tests:
  - parser action stream determinism for identical byte input sequences
- Integration tests:
  - parser -> performer -> state mutation pipeline using transcript fixtures
- Mandatory e2e scenario:
  - `tests/e2e/rio/test_terminal_core_sequences.sh --scenario pipeline_boundary`

### Adaptation 2: Bounded synchronized-update buffering + timeout

- Unit tests:
  - timeout transitions and buffer-cap overflow behavior in buffering layer
  - flush semantics when sync ends or timeout triggers
- Property tests:
  - bounded-memory invariant: buffered bytes never exceed configured cap
- Integration tests:
  - mixed normal + synchronized-update transcripts with timeout edges
- Mandatory e2e scenario:
  - `tests/e2e/rio/test_terminal_core_sequences.sh --scenario sync_buffer_guard`

### Adaptation 3: Fixture-first invariants for width/selection/damage

- Unit tests:
  - wide/zero-width mutation behavior
  - selection transformations on scroll and clear operations
  - damage scope correctness after clear/reset/mode transitions
- Property tests:
  - selection and cursor invariants under randomized mutation sequences
- Integration tests:
  - transcript replay with invariant checks across parser and terminal state
- Mandatory e2e scenario:
  - `tests/e2e/rio/test_terminal_core_sequences.sh --scenario invariant_matrix`

### Adaptation 4: Explicit full-vs-partial damage channels

- Unit tests:
  - state transitions that should emit `full` vs `partial` damage
  - damage reset semantics post-render tick
- Property tests:
  - monotonic sequence/damage ordering under random mutation streams
- Integration tests:
  - render scheduler consumption of damage events from terminal core
- Mandatory e2e scenario:
  - `tests/e2e/rio/test_terminal_core_sequences.sh --scenario damage_channel_contract`

## 8. Parser/Terminal Logging Contract (Minimum Fields)

Downstream parser + terminal-core e2e runs must emit structured JSONL events with:

| Field | Type | Required |
|---|---|---|
| `run_id` | string | yes |
| `scenario_id` | string | yes |
| `pane_id` | integer | yes |
| `sequence_no` | integer | yes |
| `parser_state` | string | yes |
| `mutation_kind` | string | yes |
| `damage_scope` | string | yes |
| `elapsed_ms` | integer | yes |
| `invariants_checked` | array[string] | yes |
| `outcome` | string (`ok`/`warn`/`fail`) | yes |
| `error_code` | string/null | yes |

Example event:

```json
{"run_id":"rio-term-20260218T0505Z-01","scenario_id":"pipeline_boundary","pane_id":7,"sequence_no":412,"parser_state":"ground","mutation_kind":"print_wide_char","damage_scope":"partial","elapsed_ms":93,"invariants_checked":["utf8_boundary","selection_bounds"],"outcome":"ok","error_code":null}
```

## 9. Mandatory Downstream E2E Contract

The adaptation work is incomplete unless all conditions below are met:

1. E2E driver script exists and is executable:
   - `tests/e2e/rio/test_terminal_core_sequences.sh`
2. Deterministic fixtures exist under:
   - `fixtures/rio/terminal_core/`
3. Structured run logs are written to:
   - `e2e-artifacts/rio/terminal_core/<run_id>.jsonl`
4. Every log event includes:
   - `run_id`, `scenario_id`, `pane_id`, `sequence_no`, `parser_state`,
     `mutation_kind`, `invariants_checked`, `outcome`, `error_code`

Any run missing these fields should be treated as invalid, even if terminal output appears visually correct.
