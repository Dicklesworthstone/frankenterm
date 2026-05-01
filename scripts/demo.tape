# scripts/demo.tape — VHS recording script for FrankenTerm
#
# Bead: ft-jjvxg (BR-RC-DEMO)
# Purpose: README-embedded GIF showing what `ft` does at a glance.
#
# This is the "tour" tape — covers the FrankenTerm CLI surface
# without requiring a real multi-agent setup. The full 5-minute
# scenario (10-pane swarm, rate-limit handling, search, mission
# orchestration) is filed as ft-jjvxg.full-scenario follow-up
# because the staging cost (live agents producing real rate
# limits) exceeds a single recording session.
#
# How to render:
#   1. cargo install --locked vhs        # if not already installed
#   2. cargo build --release             # so the demo points at a real binary
#   3. vhs scripts/demo.tape             # writes assets/demo.gif
#
# Conventions:
#   - 1280×720 viewport (matches Asciinema standard).
#   - 14pt monospace, dark background — readable in README + dark-mode
#     GitHub viewers.
#   - Sleep 1s between major sections so the viewer can read the prompt.
#   - Set Width / Height / FontSize first; commands second; flush via
#     Sleep.

Output assets/demo.gif

Set Width 1280
Set Height 720
Set FontSize 14
Set Theme "Catppuccin Mocha"
Set TypingSpeed 50ms
Set Padding 20
Set Margin 10
Set BorderRadius 8

# ---- Title -------------------------------------------------------------

Type "# FrankenTerm — terminal runtime for AI agent fleets"
Sleep 1500ms
Enter
Type "# `ft` is the CLI; below is a quick tour."
Sleep 1500ms
Enter
Sleep 500ms

# ---- Section 1: top-level CLI surface ---------------------------------

Type "ft --help"
Sleep 500ms
Enter
Sleep 4s

# ---- Section 2: init ---------------------------------------------------

Type "# init writes ~/.config/ft/config.toml with sensible defaults"
Sleep 1500ms
Enter
Type "ft init --help"
Sleep 500ms
Enter
Sleep 3s

# ---- Section 3: watch --------------------------------------------------

Type "# watch is the always-on observer over a multi-pane mux"
Sleep 1500ms
Enter
Type "ft watch --help"
Sleep 500ms
Enter
Sleep 3500ms

# ---- Section 4: robot --------------------------------------------------

Type "# robot mode exposes ft to other tools (MCP, JSON, scripts)"
Sleep 1500ms
Enter
Type "ft robot --help"
Sleep 500ms
Enter
Sleep 3500ms

# ---- Section 5: search -------------------------------------------------

Type "# search runs FTS5 + semantic search over the captured corpus"
Sleep 1500ms
Enter
Type "ft search --help"
Sleep 500ms
Enter
Sleep 3500ms

# ---- Section 6: status (the live snapshot) -----------------------------

Type "# status is the operator dashboard"
Sleep 1500ms
Enter
Type "ft status --help"
Sleep 500ms
Enter
Sleep 3500ms

# ---- Outro ------------------------------------------------------------

Type "# Full docs: https://github.com/Dicklesworthstone/frankenterm"
Sleep 2s
Enter
Sleep 1s
