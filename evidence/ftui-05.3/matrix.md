# Parity Matrix: Panes View (FTUI-05.3)

**Bead:** wa-2y1y (FTUI-05.3.a)
**Date:** 2026-02-09
**Reviewer:** LavenderGrove

## Matrix

| id | category | description | severity | legacy_behavior | ftui_behavior | verdict | evidence | delta_id | notes |
|----|----------|-------------|----------|-----------------|---------------|---------|----------|----------|-------|
| P-PANES-001 | layout | Two-panel layout: list (left) + detail (right) | blocking | 60/40 split via ratatui Layout | 60/40 split via column arithmetic | intentional-delta | ftui_stub.rs snapshot tests | D4 | Cell-based layout instead of ratatui chunks |
| P-PANES-002 | data | PaneView fields: id, title, domain, cwd, agent, state, unhandled | blocking | Direct from QueryClient | Via `adapt_pane()` → `PaneRow` | pass | view_adapters.rs `fixture_pane_normal_all_fields` | | |
| P-PANES-003 | keybinding | j/Down moves selection down | blocking | Increments selected_index, wraps at end | Same | pass | ftui_stub.rs panes navigation tests | | |
| P-PANES-004 | keybinding | k/Up moves selection up | blocking | Decrements selected_index, wraps at start | Same | pass | ftui_stub.rs panes navigation tests | | |
| P-PANES-005 | selection | Selection wrapping bottom→top and top→bottom | blocking | Modular wrapping | `saturating_sub` with wrap-around | pass | ftui_stub.rs panes wrapping tests | | |
| P-PANES-006 | filter | u toggles unhandled-only filter | blocking | Toggles `unhandled_only` bool, resets index | Same | pass | ftui_stub.rs panes filter tests | | |
| P-PANES-007 | filter | b toggles bookmarked-only filter | blocking | Toggles `bookmarked_only` bool | Same | pass | ftui_stub.rs panes filter tests | | |
| P-PANES-008 | filter | a cycles agent filter | blocking | Cycles through all/codex/claude/gemini/unknown | Same cycle via `panes_agent_filter` | pass | ftui_stub.rs panes agent filter tests | | |
| P-PANES-009 | filter | d cycles domain filter | blocking | Cycles through all/local/ssh | Same cycle via `panes_domain_filter` | pass | ftui_stub.rs panes domain filter tests | | |
| P-PANES-010 | keybinding | Enter executes selected pane action | blocking | Triggers action on selected pane | Same | pass | ftui_stub.rs panes action tests | | |
| P-PANES-011 | render | Selected pane highlighted | blocking | ratatui highlight style on selected row | Selected row rendered with bold style | intentional-delta | ftui_stub.rs render tests | D4 | Different highlight mechanism |
| P-PANES-012 | render | Detail panel shows pane info | blocking | Right panel shows CWD, agent, state, events | Right panel shows same fields | pass | ftui_stub.rs `render_panes_shows_detail` | D4 | |
| P-PANES-013 | render | Header shows count: "Panes (n/total)" | blocking | Header in list widget | Header as first rendered line | pass | ftui_stub.rs panes render header tests | | |
| P-PANES-014 | data | Agent label and style from adapter | blocking | Color by agent type | Same via `adapt_pane()` agent_style | pass | view_adapters.rs `fixture_pane_agent_variants` | | |
| P-PANES-015 | data | State label and style from adapter | blocking | Color by pane state | Same via `adapt_pane()` state_style | pass | view_adapters.rs `fixture_pane_state_variants` | | |
| P-PANES-016 | data | Unhandled badge: red+bold when > 0 | blocking | Red bold badge | Red bold via adapter `unhandled_style` | pass | view_adapters.rs `adapt_pane_formats_correctly` | | |
| P-PANES-017 | terminal | Zero-height renders without panic | blocking | No explicit guard | Guard at top | pass | ftui_stub.rs `render_panes_zero_height_no_panic` | | |
| P-PANES-018 | render | Empty pane list shows "No panes" | blocking | Empty state message | Same | pass | ftui_stub.rs `render_panes_empty_shows_message` | | |

## Intentional Deltas

| delta_id | Referenced Rows | Justification |
|----------|-----------------|---------------|
| D4 | P-PANES-001, P-PANES-011 | Widget rendering: cell-based layout and highlight via bold instead of ratatui's `ListState` highlight. Information content identical. |

## Summary

- Total rows: 18
- Pass: 16
- Intentional delta: 2 (layout, highlight)
- Fail: 0
- Untested: 0

## Sign-off

- [x] All blocking rows pass or have accepted deltas
- [x] All evidence artifacts exist and are redaction-safe
- [x] Matrix reviewed by at least one other agent/developer
