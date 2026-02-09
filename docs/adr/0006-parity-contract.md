# Parity Contract: ratatui -> ftui Migration

**Bead:** wa-136q (FTUI-01.3)
**Date:** 2026-02-09

## Purpose

This document defines what must remain behaviorally identical after the ftui
migration and what can intentionally change. It is the acceptance standard for
FTUI-05 (view migration), FTUI-06 (input migration), and FTUI-07 (testing).

## Parity Matrix: Views

Each row defines a view and what parity means for it.

| View | Layout | Data | Keybindings | Intentional Deltas |
|------|--------|------|-------------|-------------------|
| Home | 3-region: header + body + footer | Health, pane count, event count, workflow status | (none, display only) | None |
| Panes | Filtered list with selection highlight | PaneView (id, title, domain, cwd, agent, state, events) | j/k, u (unhandled filter), b (bookmarked), a (agent cycle), d (domain cycle), f/Backspace/Esc (filter), Enter (action) | None |
| Events | Filtered list with selection highlight | EventView (id, rule_id, pane_id, severity, handled, ts) | j/k, u (unhandled filter), 0-9/Backspace/Esc (pane filter) | None |
| Triage | Ranked list with expand/collapse | TriageItemView (ranked issues + workflow progress) | j/k, Enter/a (action), m (mute), e (expand) | None |
| History | Filtered list with undo indicator | HistoryEntryView (audit id, action, result, undo status) | j/k, u (undoable filter), Backspace/Esc (filter clear), 0-9/alpha (filter input), z (undo action) | None |
| Search | Input field + results list + saved searches | SearchResultView + SavedSearchView + suggestions | Char input, Enter (execute), Backspace (edit), Esc (clear), Ctrl+n/p (saved search nav), Ctrl+r (run saved), Ctrl+e (edit saved), j/k or Down/Up (results nav) | None |
| Help | Static text | Keybinding reference | (none, display only) | None |

## Parity Matrix: Global Keybindings

| Key | Action | Parity Required |
|-----|--------|----------------|
| `q` | Quit application | Yes |
| `?` | Switch to Help view | Yes |
| `r` | Refresh data (non-Ctrl) | Yes |
| `Tab` | Next view | Yes |
| `Shift+Tab` / `BackTab` | Previous view | Yes |
| `1`-`7` | Direct view access (Home..Help) | Yes |

## Parity Matrix: Per-View Keybindings

### Panes View

| Key | Action | Parity Required |
|-----|--------|----------------|
| `j` / `Down` | Move selection down | Yes |
| `k` / `Up` | Move selection up | Yes |
| `u` | Toggle unhandled-only filter | Yes |
| `b` | Toggle bookmarked-only filter | Yes |
| `a` | Cycle agent filter (all/codex/claude/gemini/unknown) | Yes |
| `d` | Cycle domain filter (all/local/ssh) | Yes |
| `f` + char input | Free-text filter | Yes |
| `Backspace` | Delete last filter char | Yes |
| `Esc` | Clear all filters | Yes |
| `Enter` | Execute selected pane action | Yes |

### Events View

| Key | Action | Parity Required |
|-----|--------|----------------|
| `j` / `Down` | Move selection down | Yes |
| `k` / `Up` | Move selection up | Yes |
| `u` | Toggle unhandled-only filter | Yes |
| `0`-`9` | Append to pane ID filter | Yes |
| `Backspace` | Delete last filter char | Yes |
| `Esc` | Clear pane filter | Yes |

### Triage View

| Key | Action | Parity Required |
|-----|--------|----------------|
| `j` / `Down` | Move selection down | Yes |
| `k` / `Up` | Move selection up | Yes |
| `Enter` / `a` | Execute action on selected item | Yes |
| `m` | Mute selected event | Yes |
| `e` | Toggle workflow expand/collapse | Yes |

### History View

| Key | Action | Parity Required |
|-----|--------|----------------|
| `j` / `Down` | Move selection down | Yes |
| `k` / `Up` | Move selection up | Yes |
| `u` | Toggle undoable-only filter | Yes |
| `z` | Undo selected action | Yes |
| `Backspace` | Delete last filter char | Yes |
| `Esc` | Clear filter + reset undoable toggle | Yes |
| Alpha/digit chars | Append to filter query | Yes |

### Search View

| Key | Action | Parity Required |
|-----|--------|----------------|
| Char input | Append to search query | Yes |
| `Enter` | Execute search | Yes |
| `Backspace` | Delete last query char | Yes |
| `Esc` | Clear query and results | Yes |
| `Ctrl+n` | Next saved search | Yes |
| `Ctrl+p` | Previous saved search | Yes |
| `Ctrl+r` | Run selected saved search | Yes |
| `Ctrl+e` | Edit selected saved search | Yes |
| `j` / `Down` | Next result (when results present) | Yes |
| `k` / `Up` | Previous result (when results present) | Yes |

## Parity Matrix: Terminal Behavior

| Behavior | Current (ratatui) | Post-Migration (ftui) | Delta |
|----------|-------------------|----------------------|-------|
| Screen mode | Alternate screen only | Alternate screen (default) | **Intentional delta**: inline mode available as opt-in |
| Raw mode | Enabled on start | Enabled on start | None |
| Cursor visibility | Hidden during TUI | Hidden during TUI | None |
| Cleanup on quit | Leave alt-screen, disable raw mode, show cursor | Same + panic hook | **Intentional delta**: panic safety added |
| Command handoff | Leave/re-enter alt-screen directly | ftui suspend/resume protocol | **Intentional delta**: structured handoff |
| Refresh rate | 100ms tick, configurable data refresh | Same | None |
| Resize handling | Automatic via crossterm | Automatic via ftui | None |

## Intentional Deltas Ledger

Deltas explicitly accepted and not requiring parity:

| ID | What Changed | Why | User Impact | Rollback |
|----|-------------|-----|-------------|----------|
| D1 | Inline mode available | ftui's primary design path; preserves scrollback | Positive: operators keep terminal context | Disable inline flag; alt-screen remains default |
| D2 | Panic/signal cleanup hook | Current TUI can leave terminal corrupted on panic | Positive: terminal always restored | N/A (strictly better) |
| D3 | Structured command handoff | Eliminates flicker on command execution | Positive: smoother UX during command runs | Revert to direct alt-screen toggle |
| D4 | Widget rendering differences | ftui widgets != ratatui widgets pixel-for-pixel | Neutral: information content preserved | N/A (cosmetic) |
| D5 | Border/color variations | Different default styles between frameworks | Neutral: same information, slightly different appearance | Customize ftui theme to match ratatui |

## Acceptance Checklist

For each migrated view (FTUI-05.*), verify:

- [ ] All keybindings from the parity matrix work identically
- [ ] View renders correct data for: empty state, single item, 100+ items
- [ ] Selection wrapping works (top->bottom, bottom->top)
- [ ] Filters work correctly (text, toggle, cycle)
- [ ] Error messages display and clear correctly
- [ ] Data refresh works (manual `r` and auto-timer)
- [ ] Snapshot test passes at 80x24 and 120x40 terminal sizes
- [ ] No panic under any combination of inputs

For input migration (FTUI-06.*), verify:

- [ ] All global keybindings (q, ?, r, Tab, Shift+Tab, 1-7) work
- [ ] Text input fields (search, filters) handle all printable ASCII
- [ ] Backspace and Esc behave correctly in all contexts
- [ ] Ctrl+key combinations work (search saved search navigation)
- [ ] Command handoff leaves and restores terminal correctly

For testing gates (FTUI-07.*), verify:

- [ ] Unit tests for adapter/reducer logic pass
- [ ] Snapshot tests at multiple sizes pass
- [ ] PTY E2E scenarios pass (startup, input, resize, quit)
- [ ] CI gates run with `--features ftui`

## Data Flow Contract (Unchanged)

The data flow from QueryClient through ViewState to rendering remains identical:

```
QueryClient.list_panes()    -> ViewState.panes    -> render_panes_view()
QueryClient.list_events()   -> ViewState.events   -> render_events_view()
QueryClient.list_history()  -> ViewState.history   -> render_history_view()
QueryClient.list_triage()   -> ViewState.triage    -> render_triage_view()
QueryClient.search()        -> ViewState.results   -> render_search_view()
QueryClient.health()        -> ViewState.health    -> render_home_view()
```

The QueryClient trait, ViewState struct, and all data types in `ui_query.rs`
are unchanged by the migration.

## References

- Current views: `crates/wa-core/src/tui/views.rs`
- Current event loop: `crates/wa-core/src/tui/app.rs`
- Current keybindings: `crates/wa-core/src/tui/app.rs:170-540`
- ADR-0001: Migration decision
- ADR-0003: Migration scope and constraints
- Ring map: ADR-0005
- Downstream: wa-1utb (FTUI-02.1), wa-5htt (FTUI-04.1), wa-co0h (FTUI-01.4)
