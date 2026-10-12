# ft-ro1ht: a title the shell sets at startup reaches the window (native A/B)

**Question.** 17854c3c5 made `TermWindow::new_window` call `update_title()` right after
`subscribe_to_pane_updates()`. Does a pane title set before that subscription (a shell's
first OSC 2) now reach the window, when nothing else (focus, tab switch, a later title
change) would ever refresh it?

**Setup** (mac-studio, macOS 27, `host.txt`; GUI on its console display, launched over SSH):
- Arm A: `frankenterm-gui` dev build of 07f454fb4 as is (contains 17854c3c5).
- Arm B: the same tree with only that `update_title()` call removed (`arm-B.diff`).
- Each run: isolated HOME and XDG_CONFIG_HOME, its own `--class`, `--always-new-process`
  (no socket publication, the operator's app is untouched), Lua config enabled.
- The pane program prints `OSC 2 ; ftgt-ro1ht-<arm>-<n>` immediately, then `exec sleep 40`. It
  prints nothing else, so no further title change follows.
- The config's `format-window-title` handler logs every title evaluation, with the active
  pane's title, and returns nil (the default title).
- The GUI is stopped after 30 s. Arms interleave B A B A B A (`ro1ht-ab.sh`).

**Result** (`ab.txt`, and each run's GUI stderr in `run-*.gui.txt`, empty when nothing was
logged):

| arm | runs | title evaluations in 30 s | with the shell's token |
|-----|------|---------------------------|------------------------|
| B (no startup read) | 3 | 0, 0, 0 | 0 |
| A (17854c3c5) | 3 | 1, 1, 1 | 1, 1, 1 |

- Both arms started normally and created the same runtime files (socket, lock, resources,
  scrollback directory).
- Without the startup read, the window never evaluated its title: it kept its creation title
  "FrankenTerm" for the whole run. This is the nv18/nv19 symptom.
- With it, the window evaluated its title once, and that title was the shell's OSC 2 token.

**Limit.** This observes the title the window computes (the value `update_title_impl` passes on to
`set_title`), not the pixels of the macOS title bar. An SSH session on mac-studio has no Screen
Recording grant to read window titles.
