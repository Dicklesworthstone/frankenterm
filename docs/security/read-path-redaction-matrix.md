# Pane-content read-path redaction matrix

**Bead:** ft-h8da2 — audit every path that serves pane-sourced content out of
`frankenterm-core` and confirm terminal text is normalized before it runs
through `Redactor::redact` (or an equivalent policy-engine wrapper).

**Design invariant:** current local and distributed capture paths redact pane
content before `output_segments` persistence (`storage::redact_segment_for_persistence`
and the distributed aggregator ingest choke point). **Every** outbound read path
still normalizes terminal control sequences and redacts with the current live
catalog as defense in depth. Normalization must happen first: otherwise ANSI
can split a token that is reconstructed after escape stripping. A single miss
is a secret leak; this matrix is the regression benchmark.

## Matrix (as of 2026-08-05)

Column meanings:
- **Redacts?** `✓` = path normalizes terminal text and then invokes
  `Redactor::redact` (or `PolicyEngine::redact_secrets`, which wraps it);
  `✗` = leak confirmed;
  `n/a` = path does not carry pane-sourced free text.
- **Evidence:** specific `file:line` call site that does (or does not) redact.

### MCP tool surfaces

| Read path | Field(s) | Redacts? | Evidence |
|-----------|----------|----------|----------|
| `wa.get_text` | `data.text` (full pane scrollback) | ✓ | `mcp_tools.rs::redact_mcp_pane_text_with_escape_contract` checks the escape-normalized view before redaction. With the explicit `escapes=true` contract, requested raw escape bytes are preserved; if normalization reveals a split secret, the response fails closed to normalized redacted text. Regression: `get_text_escape_contract_preserves_safe_escapes_but_not_split_secrets`. |
| `wa.search` | `data.results[].content`, `data.results[].snippet`, `data.query` | ✓ | Every field routes through `mcp_tools.rs::redact_mcp_output_secrets`, which calls `output::normalize_terminal_text_for_redaction` before the live `Redactor`, preserving search-text layout while removing escape/control ambiguity. |
| `wa.state` | `data[].pane_uuid`, `data[].domain`, `data[].title`, `data[].cwd`, `data[].ignore_reason` | ✓ | `mcp_tools.rs::redact_mcp_pane_state_fields` funnels the assembled live/distributed state through `redact_mcp_output_secrets` immediately before serialization. |
| `wa.events` | `data.events[].matched_text`, `data.events[].extracted` | ✓ | Redacted at write time in `runtime.rs:detection_to_stored_event` (Redactor + `redact_json_leaves` on matched_text + extracted string leaves) before persisting `StoredEvent`. All consumers (MCP wa.events, robot events, web /events, replay, storage queries) now see clean rows. Matrix line numbers and "emission only" claim were stale; fixed under ft-xt93w redaction gap audit. |
| `wa.events_annotate` / `_triage` / `_label` | echoes back the note/label the caller wrote | indirect ✓ | `storage::set_event_note_sync` at `:12603` redacts on write; wa.events_annotate re-reads the stored (already-redacted) value. |
| `wa.send` (reflects input-derived diagnostics) | `data.injection.summary`, `data.injection.error`, `data.wait_for.pattern`, `data.verification_error` | ✓ | `mcp_tools.rs::bounded_mcp_send_text_summary` applies a 64 KiB preflight and the shared normalization-redaction-truncation pipeline with a 400-column / 1,600-byte cap; the non-dry path replaces the injector's legacy summary with that safe value. `bound_mcp_send_data_output` applies the same response boundary to injection errors/summaries, wait patterns, and verification errors immediately before serialization. Regressions: `wa_send_summary_normalizes_split_secrets_and_omits_oversized_payloads` and `wa_send_response_bounds_injection_wait_and_verification_fields`. |
| `wa.dom` | `data.zones[].text`, `data.command.text`, `data.output.text`, `data.unavailable_reason` (OSC-133 semantic-zone free text) | ✓ | The shared pure `robot_dom` builder routes zone, grid-row, and unavailable-reason text through `redact_dom_output`, which normalizes before redaction; command/output fields derive from those protected zones. `exit_code` is numeric (`n/a`). Regressions: `zone_text_is_redacted` and `ansi_split_secrets_are_normalized_before_dom_redaction`. |

### Robot CLI surfaces (`crates/frankenterm/src/main.rs`)

| Read path | Field(s) | Redacts? | Evidence |
|-----------|----------|----------|----------|
| `ft robot state` (no `--include-text`) | `data[].title`, `data[].cwd`, `data[].ignore_reason` | ✓ | `main.rs::redact_pane_state_fields_for_output` routes metadata through the single-line `redact_single_line_for_output` chokepoint (`sanitize_terminal_text` followed by live-catalog redaction). |
| `ft robot state --include-text` | `data.pane_text[pane_id]` | ✓ | `redact_pane_text_results_for_output` uses the same fail-closed escape contract as get-text: explicitly requested raw escapes survive only when the normalized view contains no newly detectable secret. |
| `ft robot get-text` | pane text payload | ✓ | `main.rs::redact_pane_text_for_output` preserves explicitly requested raw escapes, but emits normalized redacted text whenever normalization reveals a split secret. Regression: `output_redaction_normalizes_ansi_split_secrets_without_breaking_explicit_escape_views`. |
| `ft robot search` | `query`, `results[].snippet`, `results[].content`, `results[].highlight` | ✓ | Query and result fields route through `main.rs::redact_for_output`, which normalizes before redaction. |
| `ft robot dom` (`zones` / `last-command` / `output-of`) | `data.zones[].text`, `data.command.text`, `data.output.text`, `data.unavailable_reason` | ✓ | Same shared `robot_dom` pure builder and `redact_dom_output` normalization chokepoint as `wa.dom`; the unavailable path uses it too. |

### Human CLI pane-read surfaces (`crates/frankenterm/src/main.rs`)

| Read path | Field(s) | Redacts? | Evidence |
|-----------|----------|----------|----------|
| `ft show <pane> --output` | pane metadata and output | ✓ | Metadata is single-line bounded before display; output routes through `redact_pane_text_for_output(..., false)`, which normalizes before redaction while preserving text layout. |
| `ft get-text` | pane text payload | ✓ | The human get-text handler routes the selected/tail-truncated payload through `redact_pane_text_for_output`. With `--escapes`, requested ANSI bytes survive only when normalization neither reveals a secret nor removes a non-ANSI display control; otherwise it fails closed to normalized redacted text. |

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

- No open source-level findings remain in the reviewed MCP, robot CLI, and
  human CLI output chokepoints. These paths normalize before redaction,
  including state/search/DOM/send fields and escape-preserving get-text paths;
  bounded diagnostic surfaces also apply explicit input and output ceilings.
  Escape-preserving paths retain explicitly requested raw escape bytes but fail
  closed to normalized redacted text when the normalized view reveals an
  ANSI-split secret. Runtime and compiler proof remain separate from this
  source-level matrix.
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
