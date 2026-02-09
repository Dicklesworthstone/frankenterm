# Parity Matrix: Home View (FTUI-05.2)

**Bead:** wa-20gu (FTUI-05.2.a)
**Date:** 2026-02-09
**Reviewer:** LavenderGrove

## Matrix

| id | category | description | severity | legacy_behavior | ftui_behavior | verdict | evidence | delta_id | notes |
|----|----------|-------------|----------|-----------------|---------------|---------|----------|----------|-------|
| P-HOME-001 | layout | Title row shows "WezTerm Automata" at top | blocking | Bold title at row 0 | Bold title at row 0 via `write_styled` | pass | ftui_stub.rs `render_home_shows_title` | | |
| P-HOME-002 | render | Health badge shows OK/ERROR/WARNING/LOADING | blocking | Badge right of title, color-coded | Badge right of title, style via `CellStyle::bold()` / `.dim()` | pass | ftui_stub.rs `render_home_shows_title`, `render_home_degraded_shows_error_badge`, `render_home_no_health_shows_loading` | | |
| P-HOME-003 | data | Watcher status label (running/stopped) | blocking | From HealthStatus.watcher_running via adapter | Same: `adapt_health()` → `HealthModel.watcher_label` | pass | view_adapters.rs `adapt_health_all_healthy`, `adapt_health_degraded`, `fixture_health_all_down_fields` | | |
| P-HOME-004 | data | Database status label (ok/unavailable) | blocking | From HealthStatus.db_accessible via adapter | Same | pass | view_adapters.rs `adapt_health_all_healthy`, `adapt_health_degraded` | | |
| P-HOME-005 | data | WezTerm CLI status label (ok/unavailable) | blocking | From HealthStatus.wezterm_accessible via adapter | Same | pass | view_adapters.rs `adapt_health_all_healthy`, `fixture_health_half_open_fields` | | |
| P-HOME-006 | data | Circuit breaker label (closed/OPEN/half-open) | blocking | From CircuitBreakerStatus.state via adapter | Same | pass | view_adapters.rs `fixture_health_all_down_fields`, `fixture_health_half_open_fields` | | |
| P-HOME-007 | data | Pane count | blocking | From HealthStatus.pane_count, shown as string | Same: `adapt_health()` → `HealthModel.pane_count` | pass | view_adapters.rs `adapt_health_all_healthy` (pane_count "5") | | |
| P-HOME-008 | data | Event count | blocking | From HealthStatus.event_count, shown as string | Same | pass | view_adapters.rs `adapt_health_all_healthy` (event_count "42") | | |
| P-HOME-009 | render | Unhandled event count with color highlight | blocking | Count shown, red if > 0 | Count shown, bold style if > 0 | pass | ftui_stub.rs `render_home_shows_metrics` | D4 | Style is bold not red — cosmetic widget difference |
| P-HOME-010 | render | Triage count | blocking | Count shown | Count shown | pass | ftui_stub.rs `render_home_shows_metrics` | | |
| P-HOME-011 | render | Loading state when health is None | blocking | Shows placeholder text | Shows "Loading health data..." dimmed | pass | ftui_stub.rs `render_home_no_health_shows_loading` | | |
| P-HOME-012 | render | Quick help section at bottom | cosmetic | Keybinding hints at bottom of Home view | "Quick help" section with Tab/q/?/r hints | pass | ftui_stub.rs `render_home_shows_quick_help` | D4 | Layout differs (line-based vs block widget) |
| P-HOME-013 | layout | 3-region layout: header + body + footer | blocking | Vertical Layout with 3 chunks | Sequential rows: title → status → metrics → help | intentional-delta | | D4 | ftui uses sequential row layout instead of ratatui Layout chunks; information content identical |
| P-HOME-014 | terminal | Zero-height renders without panic | blocking | No explicit guard | `if height == 0 { return; }` at top | pass | ftui_stub.rs `render_home_zero_height_no_panic` | | Strictly better: explicit guard |
| P-HOME-015 | terminal | Minimum height (3 rows) renders partial content | blocking | Truncated by ratatui Layout | Sequential rows with `row < max_row` guard | pass | ftui_stub.rs `render_home_minimum_height_no_panic` | | |
| P-HOME-016 | lifecycle | Manual refresh (r key) reloads health data | blocking | `r` triggers data refresh | `r` dispatches `UiAction::RefreshData` → `refresh_data()` | pass | ftui_stub.rs state reducer tests | | |

## Intentional Deltas

| delta_id | Referenced Rows | Justification |
|----------|-----------------|---------------|
| D4 | P-HOME-009, P-HOME-012, P-HOME-013 | Widget rendering differences between ratatui and ftui. Information content is preserved. ftui uses cell-based `write_styled()` instead of ratatui's `Widget` trait. Layout is sequential rows instead of `Layout::default().direction(Vertical)`. Per ADR-0006 D4: "information content preserved". |

## Summary

- Total rows: 16
- Pass: 14
- Intentional delta: 1 (P-HOME-013, layout strategy)
- Fail: 0
- Untested: 0

## Sign-off

- [x] All blocking rows pass or have accepted deltas
- [x] All evidence artifacts exist and are redaction-safe
- [x] Matrix reviewed by at least one other agent/developer
