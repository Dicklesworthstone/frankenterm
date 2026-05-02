# ft-35yac.1.1 — Pane status overview corpus entry.
#
# Scenario: operator opens the TUI, lands on the home/panes view, cycles
# the unhandled-only filter, opens a pane detail, and returns. Exercises
# the most common landing path under the BR-RC-CUTOVERS.G5.1 differential
# render oracle.
#
# Re-record: see docs/tui/parity-corpus.md §"Recording protocol".
# Replay shape: this file is a VHS script (charmbracelet/vhs). Each input
# line maps to a deterministic KeymapAction in the parity-oracle harness;
# the comment-only header lines are ignored.

Output ./01_panes_status_overview.gif
Set Width 80
Set Height 24
Set Shell "bash"
Set FontSize 14
Set Padding 0

# Launch the TUI.
Type "ft tui"
Enter
Sleep 800ms

# Settle on the panes view (view_index=2).
Type "2"
Sleep 300ms

# Toggle unhandled-only filter on, off.
Type "u"
Sleep 200ms
Type "u"
Sleep 200ms

# Toggle bookmarked-only filter on.
Type "b"
Sleep 200ms

# Cycle the agent filter.
Type "a"
Sleep 200ms
Type "a"
Sleep 200ms

# Quit cleanly.
Type "q"
Sleep 400ms
