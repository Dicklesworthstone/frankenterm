# Resilience / Chaos Validation Report (FTUI-08.4)

**Bead:** wa-1f4u (FTUI-08.4)
**Date:** 2026-02-09

---

## Test Inventory

| Test | Category | Description | Status |
|------|----------|-------------|--------|
| `chaos_resize_during_key_storm` | concurrent-input | Interleave resize + key input for 3 rounds across 5 size/key combos | pass |
| `chaos_extreme_dimensions` | boundary | Render all 7 views at 9 extreme sizes (1x1 through 500x1) | pass |
| `chaos_rapid_view_switch_with_filter_state` | state-isolation | Type into per-view filters, switch views, verify no cross-contamination | pass |
| `chaos_100_rapid_tab_cycles_with_data` | stress | 100 Tab presses with event data, render every 10th frame | pass |
| `chaos_refresh_during_every_view` | data-race | Force `refresh_data()` on each view, verify view position preserved | pass |
| `chaos_degraded_then_healthy_transition` | recovery | Start degraded, swap to healthy query, render all views both states | pass |
| `chaos_backspace_storm_on_empty_filter` | underflow | 50 Backspace on empty filter (History view), verify no underflow | pass |
| `chaos_alternating_filter_clear_cycles` | rapid-toggle | 20 cycles of type-5-chars then Escape-clear on History filter | pass |

## Categories Covered

| Category | Count | Risk mitigated |
|----------|-------|----------------|
| concurrent-input | 1 | Resize during keypress (layout panic, rendering artifact) |
| boundary | 1 | Degenerate terminal dimensions (buffer underflow, division by zero) |
| state-isolation | 1 | Per-view filter state leaking across view switches |
| stress | 1 | High-frequency Tab cycling causing routing corruption |
| data-race | 1 | Data refresh invalidating view position or causing stale render |
| recovery | 1 | Query backend failure then recovery (empty → populated transition) |
| underflow | 1 | Repeated deletion on empty input (cursor underflow, string panic) |
| rapid-toggle | 1 | Fast filter fill/clear cycles (memory churn, state inconsistency) |

## Key Findings

1. **Zero-dimension guard**: ftui `Frame::new()` requires width and height > 0. The application should guard against zero-sized terminal reports before constructing frames.

2. **Digit routing on Events/Triage views**: Global key handler correctly suppresses digit-to-view-switch when `in_events` or `in_triage` is true (line 1209), allowing digits to flow to per-view filter handlers. This was validated by `chaos_rapid_view_switch_with_filter_state`.

3. **Filter state persistence**: View-specific filter state (`pane_filter`, `filter_input`) persists correctly across view switches since each view owns its own state in `ViewState`.

4. **No panics under stress**: All 8 scenarios completed without panics across 40 test assertions total (including terminal_session chaos tests from parallel work).

## Test Location

`crates/wa-core/src/tui/ftui_stub.rs` — tests module, after line 7319 (search for `FTUI-08.4`)

## Methodology

Each test follows the pattern:
1. Create model with specific MockQuery fixture (healthy, degraded, with_events, with_history)
2. Apply adversarial input sequence (rapid keys, extreme sizes, repeated operations)
3. Assert invariants (no panic, correct state, non-empty render)

Tests use both direct `ftui::Model` API (for resize/render control) and `E2eSession` helpers (for key routing through the full update pipeline).
