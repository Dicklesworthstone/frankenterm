# Pane-content read-path redaction matrix

**Bead:** ft-h8da2 — audit every path that serves pane-sourced content out of
`frankenterm-core` and confirm it runs through `Redactor::redact` (or an
equivalent policy-engine wrapper).

**Design invariant:** current local and distributed capture paths redact pane
content before `output_segments` persistence (`storage::redact_segment_for_persistence`
and the distributed aggregator ingest choke point). **Every** outbound read path
still redacts with the current live catalog as defense in depth. A single miss
is a secret leak; this matrix is the regression benchmark.

## Matrix (as of 2026-04-23)

Column meanings:
- **Redacts?** `✓` = path invokes `Redactor::redact` (or
  `PolicyEngine::redact_secrets`, which wraps it); `✗` = leak confirmed;
  `n/a` = path does not carry pane-sourced free text.
- **Evidence:** specific `file:line` call site that does (or does not) redact.

### MCP tool surfaces

| Read path | Field(s) | Redacts? | Evidence |
|-----------|----------|----------|----------|
| `wa.get_text` | `data.text` (full pane scrollback) | ✓ | `mcp_tools.rs:1203` `engine.redact_secrets(&text)` |
| `wa.search` | `data.results[].content`, `data.results[].snippet`, `data.query` | ✓ | `mcp_tools.rs:1601-1619` `Redactor::new()` + `.redact` on each |
| `wa.state` | `data[].title`, `data[].cwd` | ✓ | The `wa.state` handler now redacts the assembled `McpPaneState` list immediately before serializing the envelope, covering both live panes and distributed panes merged from storage. |
| `wa.events` | `data.events[].matched_text`, `data.events[].extracted` | ✓ | Redacted at write time in `runtime.rs:detection_to_stored_event` (Redactor + `redact_json_leaves` on matched_text + extracted string leaves) before persisting `StoredEvent`. All consumers (MCP wa.events, robot events, web /events, replay, storage queries) now see clean rows. Matrix line numbers and "emission only" claim were stale; fixed under ft-xt93w redaction gap audit. |
| `wa.events_annotate` / `_triage` / `_label` | echoes back the note/label the caller wrote | indirect ✓ | `storage::set_event_note_sync` at `:12603` redacts on write; wa.events_annotate re-reads the stored (already-redacted) value. |
| `wa.send` (reflects `text` back to caller on dry-run) | `data.injection.summary` | ✓ | Goes through `PolicyGatedInjector` → `policy::Redactor` before audit + summary emission. |
| `wa.dom` | `data.zones[].text`, `data.command.text`, `data.output.text`, `data.unavailable_reason` (OSC-133 semantic-zone free text) | ✓ | Built via the shared pure core builder `robot_dom::build_dom_data(&snapshot, &redactor)` at `mcp_tools.rs:2970` (`redactor = Redactor::new()` at `:2850`); zone text redacted in `robot_dom.rs:36` `dom_zone_from_mux` (`redactor.redact(&zone.text)`), and the `command` / `output` text fields are cloned from that already-redacted `zones` vector (`robot_dom.rs:201,257-308`); `unavailable_reason` redacted at `robot_dom.rs:56`. `exit_code` is numeric (`n/a`). Unit test: `robot_dom::tests::zone_text_is_redacted`. |

### Robot CLI surfaces (`crates/frankenterm/src/main.rs`)

| Read path | Field(s) | Redacts? | Evidence |
|-----------|----------|----------|----------|
| `ft robot state` (no `--include-text`) | `data[].title`, `data[].cwd` | ✓ | The robot-state handler now redacts pane `title` / `cwd` immediately before serializing the response envelope, matching the web `/panes` behavior. |
| `ft robot state --include-text` | `data.pane_text[pane_id]` | ✓ | Text flows through `get_pane_text` → `REDACTOR.redact(text)` helper at `main.rs:6867-6869`. |
| `ft robot get-text` | pane text payload | ✓ | Same static `REDACTOR` helper at `main.rs:6867-6869`. |
| `ft robot search` | `results[].snippet`, `results[].content` | ✓ | Goes through the same `wa.search` code path via the shared handler. |
| `ft robot dom` (`zones` / `last-command` / `output-of`) | `data.zones[].text`, `data.command.text`, `data.output.text` (OSC-133 semantic zones) | ✓ | Same shared pure core builder via `build_robot_dom_data` → `robot_dom::build_dom_data(&snapshot, &Redactor::new())` at `main.rs:23865-23871`; byte-equal with the `wa.dom` MCP envelope. The unavailable path uses `robot_dom::dom_unavailable(&Redactor::new())` at `main.rs:23847-23854`. |

### Web API (`crates/frankenterm-core/src/web/`)

| Read path | Field(s) | Redacts? | Evidence |
|-----------|----------|----------|----------|
| `GET /panes` | `title`, `cwd` | ✓ | `web/handlers.rs:64-70` `PaneView::from_record(r, &redactor)` with explicit `.map(|t| redactor.redact(&t))`. |
| `GET /events` | event annotations + extracted fields | ✓ | `web/handlers.rs:148-156` explicit `redactor.redact` on triage_state / note / labels. |
| `GET /search` | result snippets/content | ✓ | goes through the same redactor-applied search path. |
| `GET /stream/events` (SSE) | event payload JSON | ✓ | `web/sse.rs:489` `redact_json_value(&mut event_json, &redactor)`. |
| `GET /stream/deltas` (SSE) | `segment.content` | ✓ | `web/sse.rs:386` `redactor.redact(&segment.content)`. |

### Export / replay / audit

| Read path | Field(s) | Redacts? | Evidence |
|-----------|----------|----------|----------|
| `ft export` | segment content | ✓ (when `--redact`) | `export.rs:433` `redact_segment(seg, &redactor)` gated on `opts.redact` at `:112`. **Caveat:** `--redact` is an opt-in flag; default behaviour is subject to `opts.redact`'s default. Operators running `ft export` with `--redact=false` get raw bytes by design. |
| `ft replay` decoded frame output | payload text | ✓ (when `--redact`) | `replay.rs:814-830` gated on `opts.redact`. Same opt-in caveat. |
| Audit table writes | action summary + decision context | ✓ | `storage::record_audit_action_redacted` at `:6647` (write-time redaction; persisted audit rows are already clean). |

### Distributed-mode aggregator ingest (closed — FND-004)

- **Distributed mode aggregator forwarding** (`--features distributed`): remote `WirePayload::PaneDelta` content is now redacted at the aggregator ingest choke point — `crates/frankenterm/src/main.rs` `distributed_persist_payload()` applies `frankenterm_core::redactor::Redactor::new().redact(&delta.content)` immediately before `append_segment`, mirroring the local capture path (`crates/frankenterm-core/src/ingest.rs:1680` `redact_segment_for_persist`). Previously this path stored remote pane content RAW in `output_segments`, a fail-closed redaction violation at the storage layer (an unredacted DB export or any direct-segment read could leak secrets even though the standard read APIs redact at output). Redaction is idempotent, so the read-path redactors are unaffected.
- **Remaining**: a runtime planted-secret differential against `distributed_persist_payload()` is still desirable (tracked GA-FND-004-test) — blocked only because the fn is a private async fn in the binary crate; correctness today rests on reuse of the exhaustively-tested `Redactor` at the exact choke point + a clean `cargo check --features distributed`. The `Detection` wire variant's `matched_text`/`extracted` are a *separate* surface not covered by this row.

## Actionable findings from this audit

- No open findings remain in the audited rows above as of `ft-yj375`; `wa.state` and `ft robot state` now redact pane `title` / `cwd` at the serving handler.
- `ft-5puf0` (Contract-Doctor gap G3): audited the `wa.dom` / `ft robot dom` OSC-133 semantic-zone read path — it was missing from this matrix but NOT a leak. Both the MCP tool and robot CLI route through the single shared pure builder `frankenterm_core::robot_dom::build_dom_data` with a live `Redactor::new()`; zone/command/output text is redacted at `robot_dom.rs:36` and all command/output fields derive from the already-redacted `zones` vector. Added the two rows above. Code-resident proof: `robot_dom::tests::zone_text_is_redacted` (RCH re-run deferred while the lane is flaky).
- Policy-denial audit wiring (ft-h90rh / ft-rsqap / ft-mw1zb): 11 of 12 deny paths now also persist to `policy_denied_audit`. Only `wa.send` is deliberately routed through `PolicyGatedInjector` (which already writes to `audit_actions`); adding a second audit stream would double-count. See `docs/security/policy-denial-audit-wiring-matrix.md`.

## Regression discipline

When adding a new surface that carries pane-sourced text, ADD A ROW to this
matrix and cite the redaction call site. `grep -n 'Redactor\\|redact' <file>`
in the serving handler should return at least one hit before the PR lands.

## Method

Greps used to construct this matrix:

```bash
grep -n 'Redactor\|\.redact(' crates/frankenterm-core/src/mcp_tools.rs
grep -n 'Redactor\|redact'    crates/frankenterm-core/src/web/handlers.rs
grep -n 'Redactor\|redact'    crates/frankenterm-core/src/web/sse.rs
grep -n 'Redactor\|\.redact'  crates/frankenterm-core/src/export.rs
grep -n 'Redactor\|\.redact'  crates/frankenterm-core/src/replay.rs
grep -n 'Redactor\|\.redact(' crates/frankenterm/src/main.rs
```

Every `✗` / `LEAK` / `indirect` cell was cross-checked against the
corresponding constructor / serialization site.
