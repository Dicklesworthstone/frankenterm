# ft-35yac.1.1 — Search workflow corpus entry.
#
# Scenario: operator switches to the search view (view_index=5), enters a
# query string, deletes a character, clears the filter, and quits.
# Exercises FilterAppendChar / FilterDeleteChar / FilterClear which the
# differential oracle records as separate frames per keystroke — high-
# fidelity input-stream test.
#
# Re-record: see docs/tui/parity-corpus.md §"Recording protocol".

Output ./02_search_workflow.gif
Set Width 80
Set Height 24
Set Shell "bash"
Set FontSize 14
Set Padding 0

Type "ft tui"
Enter
Sleep 800ms

# Search view.
Type "5"
Sleep 300ms

# Enter "hello", per-keystroke.
Type "h"
Sleep 80ms
Type "e"
Sleep 80ms
Type "l"
Sleep 80ms
Type "l"
Sleep 80ms
Type "o"
Sleep 200ms

# Delete one char, append uppercase.
Backspace
Sleep 120ms
Type "O"
Sleep 200ms

# Clear filter (mapped to FilterClear).
Ctrl+u
Sleep 200ms

Type "q"
Sleep 400ms
