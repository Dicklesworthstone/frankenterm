# Parity Matrix: Events View (FTUI-05.4)

**Bead:** wa-3akp (FTUI-05.4.a)
**Date:** 2026-02-09
**Reviewer:** LavenderGrove

## Matrix

| id | category | description | severity | legacy_behavior | ftui_behavior | verdict | evidence | delta_id | notes |
|----|----------|-------------|----------|-----------------|---------------|---------|----------|----------|-------|
| P-EVENTS-001 | layout | Two-panel layout: list (left) + detail (right) | blocking | 60/40 split | 60/40 split via column arithmetic | intentional-delta | ftui_stub.rs snapshot tests | D4 | Cell-based layout |
| P-EVENTS-002 | data | EventView fields: id, rule_id, pane_id, severity, message, ts, handled, triage, labels, note | blocking | Direct from QueryClient | Via `adapt_event()` → `EventRow` | pass | view_adapters.rs `fixture_event_normal_all_fields` | | |
| P-EVENTS-003 | keybinding | j/Down moves selection down | blocking | Increments, wraps | Same | pass | ftui_stub.rs events navigation tests | | |
| P-EVENTS-004 | keybinding | k/Up moves selection up | blocking | Decrements, wraps | Same | pass | ftui_stub.rs events navigation tests | | |
| P-EVENTS-005 | filter | u toggles unhandled-only filter | blocking | Toggles `unhandled_only`, resets index | Same | pass | ftui_stub.rs events filter tests | | |
| P-EVENTS-006 | filter | 0-9 appends to pane filter | blocking | Pushes digit to filter string | `pane_filter.insert_char(c)` via TextInput | pass | ftui_stub.rs events digit filter tests | | |
| P-EVENTS-007 | keybinding | Backspace removes last filter char | blocking | `pane_filter.pop()` | `pane_filter.delete_back()` via TextInput | pass | ftui_stub.rs `events_backspace_removes_filter_char` | | Cursor-aware deletion |
| P-EVENTS-008 | keybinding | Esc clears pane filter | blocking | Clears filter string | `pane_filter.clear()` via TextInput | pass | ftui_stub.rs events escape test | | |
| P-EVENTS-009 | keybinding | Delete forward deletes char at cursor | cosmetic | Not available in legacy | `pane_filter.delete_forward()` | intentional-delta | ftui_stub.rs `events_delete_forward_in_filter` | D3 | New cursor navigation capability |
| P-EVENTS-010 | keybinding | Left/Right moves cursor in filter | cosmetic | Not available in legacy | TextInput cursor movement | intentional-delta | ftui_stub.rs `events_left_right_in_filter` | D3 | New cursor navigation capability |
| P-EVENTS-011 | keybinding | Home/End moves cursor to start/end | cosmetic | Not available in legacy | TextInput cursor movement | intentional-delta | ftui_stub.rs `events_home_end_in_filter` | D3 | New cursor navigation capability |
| P-EVENTS-012 | data | Severity label and style | blocking | Color by severity string | Same via `adapt_event()` severity_style | pass | view_adapters.rs `fixture_event_severity_variants` | | |
| P-EVENTS-013 | data | Handled/unhandled label and style | blocking | "handled" (gray) / "UNHANDLED" (yellow+bold) | Same via adapter | pass | view_adapters.rs `adapt_event_handled`, `adapt_event_unhandled` | | |
| P-EVENTS-014 | data | Triage state label and style | blocking | Color by triage state | Same via adapter | pass | view_adapters.rs `fixture_event_triage_state_variants` | | |
| P-EVENTS-015 | data | Labels joined with comma separator | blocking | `labels.join(", ")` | Same in adapter | pass | view_adapters.rs `adapt_event_labels_mapped`, `adapt_event_empty_labels` | | |
| P-EVENTS-016 | data | Note preview truncated to 40 chars | blocking | Truncated with "..." | Same via `truncate(n, 40)` in adapter | pass | view_adapters.rs `adapt_event_note_preview`, `adapt_event_no_note` | | |
| P-EVENTS-017 | data | Secret redaction in message | blocking | `redact_secrets()` applied | Same | pass | view_adapters.rs `redact_event_message_integration` | | |
| P-EVENTS-018 | render | Header shows "Events (filtered/total) unhandled_only=X pane/rule='Y'" | blocking | Header string | Same header format | pass | ftui_stub.rs events render tests | | |
| P-EVENTS-019 | render | Detail panel shows full event info | blocking | Right panel with event detail | Same fields in right panel | pass | ftui_stub.rs events detail tests | D4 | |
| P-EVENTS-020 | terminal | Zero-height renders without panic | blocking | No explicit guard | Guard at top | pass | ftui_stub.rs `render_events_zero_height_no_panic` | | |
| P-EVENTS-021 | filter | filtered_indices respects unhandled + pane filter | blocking | Combined filter logic | Same in `EventsViewState::filtered_indices()` | pass | ftui_stub.rs `events_filtered_indices_combined` | | |
| P-EVENTS-022 | selection | Focus tracking: digits→FilterBar, j/k→PrimaryList | cosmetic | No focus tracking | FocusRegion tracks active region | intentional-delta | ftui_stub.rs focus tests | D3 | New accessibility feature |

## Intentional Deltas

| delta_id | Referenced Rows | Justification |
|----------|-----------------|---------------|
| D3 | P-EVENTS-009 through P-EVENTS-011, P-EVENTS-022 | Structured input handling: TextInput with cursor navigation and FocusRegion tracking are new capabilities that improve UX. No legacy behavior removed. |
| D4 | P-EVENTS-001, P-EVENTS-019 | Widget rendering differences. Information content preserved. |

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
