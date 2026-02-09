# Parity Matrix: History View (FTUI-05.6)

**Bead:** wa-2fi0 (FTUI-05.6.a)
**Date:** 2026-02-09
**Reviewer:** LavenderGrove

## Matrix

| id | category | description | severity | legacy_behavior | ftui_behavior | verdict | evidence | delta_id | notes |
|----|----------|-------------|----------|-----------------|---------------|---------|----------|----------|-------|
| P-HIST-001 | layout | Two-panel layout: list (left) + provenance detail (right) | blocking | 60/40 split | 60/40 split via column arithmetic | intentional-delta | ftui_stub.rs snapshot tests | D4 | Cell-based layout |
| P-HIST-002 | data | HistoryEntryView fields: audit_id, timestamp, pane_id, workflow_id, action_kind, result, actor_kind, step_name, undoable, undone, undo_strategy, undo_hint, rule_id, summary | blocking | Direct from QueryClient | Via `adapt_history()` → `HistoryRow` | pass | view_adapters.rs `fixture_history_normal_all_fields` | | |
| P-HIST-003 | keybinding | j/Down moves selection down | blocking | Increments, wraps | Same | pass | ftui_stub.rs history navigation tests | | |
| P-HIST-004 | keybinding | k/Up moves selection up | blocking | Decrements, wraps | Same | pass | ftui_stub.rs history navigation tests | | |
| P-HIST-005 | filter | u toggles undoable-only filter | blocking | Toggles `undoable_only`, resets index | Same | pass | ftui_stub.rs history filter tests | | |
| P-HIST-006 | filter | Free-text filter on any printable char | blocking | Pushes char to filter string | `filter_input.insert_char(c)` via TextInput | pass | ftui_stub.rs history filter tests | | |
| P-HIST-007 | keybinding | Backspace removes last filter char | blocking | `filter_query.pop()` | `filter_input.delete_back()` via TextInput | pass | ftui_stub.rs history backspace test | | Cursor-aware deletion |
| P-HIST-008 | keybinding | Esc clears text filter | blocking | Clears filter string | `filter_input.clear()` via TextInput | pass | ftui_stub.rs history escape test | | |
| P-HIST-009 | keybinding | Delete forward deletes char at cursor | cosmetic | Not available in legacy | `filter_input.delete_forward()` | intentional-delta | ftui_stub.rs `history_delete_forward_in_filter` | D3 | New cursor navigation |
| P-HIST-010 | keybinding | Left/Right moves cursor in filter | cosmetic | Not available in legacy | TextInput cursor movement | intentional-delta | ftui_stub.rs `history_left_right_in_filter` | D3 | New cursor navigation |
| P-HIST-011 | keybinding | Home/End moves cursor to start/end | cosmetic | Not available in legacy | TextInput cursor movement | intentional-delta | ftui_stub.rs `history_home_end_in_filter` | D3 | New cursor navigation |
| P-HIST-012 | data | Result label and style (success/denied/failed) | blocking | Color by result string | Same via `adapt_history()` result_style | pass | view_adapters.rs `fixture_history_result_style_variants` | | |
| P-HIST-013 | data | Undo label: "undoable"/"UNDONE"/"" | blocking | Conditional on undoable/undone flags | Same via adapter; undone takes priority | pass | view_adapters.rs `fixture_history_undo_state_matrix` | | |
| P-HIST-014 | data | Provenance detail: pane_id, workflow_id, step_name, rule_id, undo_strategy, undo_hint | blocking | Shown in detail panel | Same via adapter provenance fields | pass | view_adapters.rs `adapt_history_full_provenance`, `adapt_history_provenance_fields` | | |
| P-HIST-015 | data | Secret redaction in summary | blocking | `redact_secrets()` applied | Same | pass | view_adapters.rs `redact_history_summary_integration`, `fixture_history_redacted_secrets_stripped` | | |
| P-HIST-016 | data | Missing optional fields → empty string | blocking | None → "" | Same via adapter `unwrap_or_default()` | pass | view_adapters.rs `fixture_history_missing_all_fields` | | |
| P-HIST-017 | render | Header shows "History (filtered/total) undoable_only=X filter='Y'" | blocking | Header string | Same header format | pass | ftui_stub.rs history render tests | | |
| P-HIST-018 | render | Detail panel shows provenance info | blocking | Right panel with pane, workflow, step, strategy, hint | Same fields in right panel | pass | ftui_stub.rs history detail tests | D4 | |
| P-HIST-019 | terminal | Zero-height renders without panic | blocking | No explicit guard | Guard at top | pass | ftui_stub.rs history zero-height test | | |
| P-HIST-020 | filter | filtered_indices respects undoable + text filter combined | blocking | Combined lowercase text match on all HistoryRow fields | Same in `HistoryViewState::filtered_indices()` | pass | ftui_stub.rs `history_filtered_indices_combined` | | |
| P-HIST-021 | keybinding | q does not quit in History view (text input active) | blocking | `has_text_input` guard suppresses q | Same via `has_text_input` returning true for History | pass | ftui_stub.rs `history_q_does_not_quit` | | |
| P-HIST-022 | selection | Focus tracking: chars→FilterBar, j/k→PrimaryList | cosmetic | No focus tracking | FocusRegion tracks active region | intentional-delta | ftui_stub.rs focus tests | D3 | New accessibility feature |

## Intentional Deltas

| delta_id | Referenced Rows | Justification |
|----------|-----------------|---------------|
| D3 | P-HIST-009 through P-HIST-011, P-HIST-022 | Structured input handling: TextInput with cursor navigation and FocusRegion tracking are new capabilities. No legacy behavior removed. |
| D4 | P-HIST-001, P-HIST-018 | Widget rendering differences. Cell-based vs ratatui block widgets. Information content preserved. |

## Summary

- Total rows: 22
- Pass: 17
- Intentional delta: 5 (cursor nav, focus tracking, layout)
- Fail: 0
- Untested: 0

## Sign-off

- [x] All blocking rows pass or have accepted deltas
- [x] All evidence artifacts exist and are redaction-safe
- [x] Matrix reviewed by at least one other agent/developer
