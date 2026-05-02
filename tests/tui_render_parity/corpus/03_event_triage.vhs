# ft-35yac.1.1 — Event triage corpus entry.
#
# Scenario: operator switches to the events view (view_index=3), exercises
# the digit-filter cycle (EventsFilterDigit) across three values, then
# opens the triage panel (view_index=4) and runs a TriageNumberedAction
# pass before quitting. Exercises the highest-density per-frame redraw
# path in the TUI.
#
# Re-record: see docs/tui/parity-corpus.md §"Recording protocol".

Output ./03_event_triage.gif
Set Width 80
Set Height 24
Set Shell "bash"
Set FontSize 14
Set Padding 0

Type "ft tui"
Enter
Sleep 800ms

# Events view.
Type "3"
Sleep 300ms

# Digit-filter cycle: 1, 5, 0.
Type "1"
Sleep 150ms
Type "5"
Sleep 150ms
Type "0"
Sleep 150ms

# Switch to triage view.
Type "4"
Sleep 300ms

# TriageNumberedAction(1..3).
Type "1"
Sleep 150ms
Type "2"
Sleep 150ms
Type "3"
Sleep 150ms

# Primary action.
Enter
Sleep 200ms

Type "q"
Sleep 400ms
