# ft-35yac.1.1 — Workflow execution monitoring corpus entry.
#
# Scenario: operator switches to the workflows view (view_index=6),
# tab-cycles through every panel, scrolls down/up, opens a workflow's
# step log, and returns home. Exercises NextTab / PrevTab boundaries
# which historically diverge under the differential oracle when one
# backend wraps and the other clamps.
#
# Re-record: see docs/tui/parity-corpus.md §"Recording protocol".

Output ./04_workflow_execution_monitoring.gif
Set Width 100
Set Height 30
Set Shell "bash"
Set FontSize 14
Set Padding 0

Type "ft tui"
Enter
Sleep 800ms

# Workflows view.
Type "6"
Sleep 300ms

# Cycle every tab forward.
Tab
Sleep 150ms
Tab
Sleep 150ms
Tab
Sleep 150ms
Tab
Sleep 150ms
Tab
Sleep 150ms

# Cycle backward to verify PrevTab parity.
Shift+Tab
Sleep 150ms
Shift+Tab
Sleep 150ms

# Scroll the step-log panel.
Down
Sleep 100ms
Down
Sleep 100ms
Down
Sleep 100ms
Up
Sleep 100ms

# Open a workflow's step log.
Enter
Sleep 300ms

# Back to summary.
Escape
Sleep 200ms

Type "q"
Sleep 400ms
