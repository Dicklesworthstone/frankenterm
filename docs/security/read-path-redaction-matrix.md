# Pane-content read-path redaction matrix

**Bead:** ft-h8da2 — audit every path that serves pane-sourced content out of
`frankenterm-core` and confirm it runs through `Redactor::redact` (or an
equivalent policy-engine wrapper).

**Design invariant:** pane content is stored raw at ingest
(`storage::append_segment_sync`). **Every** outbound read path must redact.
A single miss is a secret leak; this matrix is the regression benchmark.

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
| `wa.state` | `data[].title`, `data[].cwd` | **✗ LEAK** | `mcp_types.rs:817-834` `McpPaneState::from_pane_info` clones raw `info.title` / `info.cwd` with no redaction. Compare with `web/handlers.rs:64-70` which DOES redact on the web surface. Filed: ft-\<new\>. |
| `wa.events` | `data.events[].matched_text`, `data.events[].extracted` | indirect ✓ | Events are redacted at emission in `events.rs:280` (`normalized_extracted`) + `:321` (`redactor.redact(&rendered)`), so rows in storage are already clean; wa.events reads from storage and serves them out. Invariant depends on the emission path never being bypassed. |
| `wa.events_annotate` / `_triage` / `_label` | echoes back the note/label the caller wrote | indirect ✓ | `storage::set_event_note_sync` at `:12603` redacts on write; wa.events_annotate re-reads the stored (already-redacted) value. |
| `wa.send` (reflects `text` back to caller on dry-run) | `data.injection.summary` | ✓ | Goes through `PolicyGatedInjector` → `policy::Redactor` before audit + summary emission. |

### Robot CLI surfaces (`crates/frankenterm/src/main.rs`)

| Read path | Field(s) | Redacts? | Evidence |
|-----------|----------|----------|----------|
| `ft robot state` (no `--include-text`) | `data[].title`, `data[].cwd` | **✗ LEAK** | `main.rs:5501-5518` `PaneState::from_pane_info` clones raw title / cwd. Same root cause as `wa.state`. |
| `ft robot state --include-text` | `data.pane_text[pane_id]` | ✓ | Text flows through `get_pane_text` → `REDACTOR.redact(text)` helper at `main.rs:6867-6869`. |
| `ft robot get-text` | pane text payload | ✓ | Same static `REDACTOR` helper at `main.rs:6867-6869`. |
| `ft robot search` | `results[].snippet`, `results[].content` | ✓ | Goes through the same `wa.search` code path via the shared handler. |

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

### Known gap NOT in this matrix

- **Distributed mode aggregator forwarding**: `crates/frankenterm-core/src/distributed.rs` shows zero `Redactor` usages. The wire-protocol forwarding path is across multiple files and a full audit is out of scope for this pass; filed separately if needed. The default `Config.distributed.enabled = false` limits exposure, but when enabled the threat model should be re-evaluated.

## Actionable findings from this audit

- **ft-\<new\> [P1]** — `wa.state` and `ft robot state` return pane `title` / `cwd` unredacted. Fix: either add redaction in `McpPaneState::from_pane_info` / `PaneState::from_pane_info`, or redact at the serving handler. Web surface already does this consistently (`web/handlers.rs:64-70`); MCP/CLI should match.

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
