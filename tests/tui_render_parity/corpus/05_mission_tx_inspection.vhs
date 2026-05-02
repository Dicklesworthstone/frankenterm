# ft-35yac.1.1 — Mission/TX inspection corpus entry.
#
# Scenario: operator switches to the mission view (view_index=7),
# inspects the TX history panel, drills into a single TX's commit log,
# and exits via primary keymap. Exercises the deepest-nested view chain
# in the TUI — three pop-states deep — which is where backends most often
# disagree on cursor position after Escape.
#
# Re-record: see docs/tui/parity-corpus.md §"Recording protocol".

Output ./05_mission_tx_inspection.gif
Set Width 100
Set Height 30
Set Shell "bash"
Set FontSize 14
Set Padding 0

Type "ft tui"
Enter
Sleep 800ms

# Mission view.
Type "7"
Sleep 300ms

# Open TX panel.
Type "t"
Sleep 200ms

# Move down through the TX list.
Down
Sleep 100ms
Down
Sleep 100ms
Down
Sleep 100ms

# Drill into a TX's commit log.
Enter
Sleep 300ms

# Page-down within the log.
Space
Sleep 200ms
Space
Sleep 200ms

# Pop back to TX list.
Escape
Sleep 200ms

# Pop back to mission overview.
Escape
Sleep 200ms

# Pop back to home.
Escape
Sleep 200ms

Type "q"
Sleep 400ms
