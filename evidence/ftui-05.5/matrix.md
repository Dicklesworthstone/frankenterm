# Parity Matrix: Triage View (FTUI-05.5)

**Bead:** wa-2sfj (FTUI-05.5.a)
**Date:** 2026-02-09
**Reviewer:** LavenderGrove

## Matrix

| id | category | description | severity | legacy_behavior | ftui_behavior | verdict | evidence | delta_id | notes |
|----|----------|-------------|----------|-----------------|---------------|---------|----------|----------|-------|
| P-TRIAGE-001 | layout | Triage list with optional workflow panel | blocking | Vertical layout: list + detail | Sequential rows: list + workflow progress + detail | intentional-delta | ftui_stub.rs snapshot tests | D4 | Cell-based layout |
| P-TRIAGE-002 | data | TriageItemView fields: section, severity, title, detail, actions, event_id, pane_id, workflow_id | blocking | Direct from QueryClient | Via `adapt_triage()` → `TriageRow` | pass | view_adapters.rs `fixture_triage_normal_all_fields` | | |
| P-TRIAGE-003 | keybinding | j/Down moves selection down | blocking | Increments, wraps | Same | pass | ftui_stub.rs triage navigation tests | | |
| P-TRIAGE-004 | keybinding | k/Up moves selection up | blocking | Decrements, wraps | Same | pass | ftui_stub.rs triage navigation tests | | |
| P-TRIAGE-005 | keybinding | Enter/a executes action on selected item | blocking | Dispatches triage action command | Same via `triage_queued_action` | pass | ftui_stub.rs triage action tests | | |
| P-TRIAGE-006 | keybinding | m mutes selected event | blocking | Calls `mark_event_muted()` | Same | pass | ftui_stub.rs triage mute tests | | |
| P-TRIAGE-007 | keybinding | e toggles workflow expand/collapse | blocking | Toggles expand on selected workflow | Same via `triage_expanded` | pass | ftui_stub.rs triage expand tests | | |
| P-TRIAGE-008 | data | Severity label and style | blocking | Color by severity | Same via `adapt_triage()` severity_style | pass | view_adapters.rs `fixture_triage_normal_all_fields` (severity_style.bold) | | |
| P-TRIAGE-009 | data | Action labels and commands | blocking | Vec of label/command pairs | Same via adapter `action_labels`/`action_commands` | pass | view_adapters.rs `adapt_triage_formats_actions` | | |
| P-TRIAGE-010 | data | Cross-reference IDs (event, pane, workflow) | blocking | Displayed in detail | Same via adapter: `event_id`, `pane_id`, `workflow_id` | pass | view_adapters.rs `adapt_triage_cross_references`, `adapt_triage_workflow_cross_reference` | | |
| P-TRIAGE-011 | data | Secret redaction in detail | blocking | `redact_secrets()` applied | Same | pass | view_adapters.rs `redact_triage_detail_integration`, `fixture_triage_redacted_secrets_stripped` | | |
| P-TRIAGE-012 | data | Missing cross-reference IDs → empty string | blocking | None → "" | Same via adapter `unwrap_or_default()` | pass | view_adapters.rs `fixture_triage_missing_all_fields` | | |
| P-TRIAGE-013 | render | Workflow progress panel (when expanded) | blocking | Shows step progress, status, error | Same: progress_label, status_label, error | pass | ftui_stub.rs triage workflow rendering tests | D4 | |
| P-TRIAGE-014 | data | Workflow status style | blocking | Color by status | Same via `adapt_workflow()` status_style | pass | view_adapters.rs `fixture_workflow_status_variants` | | |
| P-TRIAGE-015 | render | Empty triage list shows message | blocking | Empty state text | Same | pass | ftui_stub.rs triage empty state test | | |
| P-TRIAGE-016 | terminal | Zero-height renders without panic | blocking | No explicit guard | Guard at top | pass | ftui_stub.rs triage zero-height test | | |
| P-TRIAGE-017 | keybinding | Digits 1-9 route to triage action (not global view switch) | blocking | Digit routing guard in app.rs | `in_triage` guard in `handle_global_key()` | pass | ftui_stub.rs digit routing guard tests | | |

## Intentional Deltas

| delta_id | Referenced Rows | Justification |
|----------|-----------------|---------------|
| D4 | P-TRIAGE-001, P-TRIAGE-013 | Widget rendering differences. Cell-based vs ratatui block widgets. Information content preserved. |

## Summary

- Total rows: 17
- Pass: 15
- Intentional delta: 2 (layout, workflow panel rendering)
- Fail: 0
- Untested: 0

## Sign-off

- [x] All blocking rows pass or have accepted deltas
- [x] All evidence artifacts exist and are redaction-safe
- [x] Matrix reviewed by at least one other agent/developer
