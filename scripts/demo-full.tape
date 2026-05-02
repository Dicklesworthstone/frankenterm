# scripts/demo-full.tape — full multi-agent VHS recording script for FrankenTerm
#
# Bead: ft-xl2kc (BR-RC-DEMO.full-scenario) — substrate authored by cc_3.
# Parent: ft-jjvxg (BR-RC-DEMO — tour tape shipped at scripts/demo.tape).
#
# Purpose: 5-minute README-embedded GIF showing FrankenTerm driving a
# 10-pane swarm of cc/cod/gmi agents through realistic work, with `ft`
# observing rate limits, auto-handling them via workflows, recovering
# past errors via search, and orchestrating a mission.
#
# This script is the deterministic input stream the operator runs once
# the live swarm is staged. The actual recording (assets/demo-full.gif)
# is operator-territory because the staging cost (NTM 10-pane swarm,
# real Anthropic/OpenAI accounts hitting real rate limits, real workflow
# triggering) exceeds any automated harness — see ft-xl2kc.1 follow-up
# for the operator runbook.
#
# How to render (operator):
#   1. Stage the swarm (see ft-xl2kc.1 runbook):
#        ntm session new --weighted "cc=4,cod=3,gmi=3" --workspace ft-demo
#        # Drive the swarm through ~3 minutes of real work so it hits
#        # at least one rate limit and produces ≥1 ft auto-detect event.
#   2. Confirm `ft status` shows ≥1 detection ready to demo.
#   3. cargo build --release --bin ft
#   4. vhs scripts/demo-full.tape  # writes assets/demo-full.gif
#
# Conventions match scripts/demo.tape (1280×720, Catppuccin Mocha, 14pt).
# Where the tour tape uses synthetic outputs, this tape relies on the
# pre-staged swarm's real state — every `ft status` / `ft search` /
# `ft robot events` call reads live data.

Output assets/demo-full.gif

Set Width 1280
Set Height 720
Set FontSize 14
Set Theme "Catppuccin Mocha"
Set TypingSpeed 50ms
Set Padding 20
Set Margin 10
Set BorderRadius 8

# ---- Title --------------------------------------------------------------

Type "# FrankenTerm full scenario — observing a 10-pane agent swarm"
Sleep 2s
Enter
Type "# 4× cc · 3× cod · 3× gmi · staged via NTM"
Sleep 1500ms
Enter
Sleep 500ms

# ---- Section 1: swarm topology ------------------------------------------

Type "# 1. Topology + state"
Sleep 1s
Enter
Type "ft status --format toon"
Sleep 800ms
Enter
Sleep 4s

Type "ft robot --format toon state"
Sleep 800ms
Enter
Sleep 4s

# ---- Section 2: rate-limit detection ------------------------------------

Type "# 2. Rate-limit auto-detection (one of the live swarm panes is hitting it)"
Sleep 2s
Enter
Type "ft robot --format toon events --filter rate_limit"
Sleep 800ms
Enter
Sleep 5s

# Show the workflow that auto-handled it.
Type "# auto-handled by the rate_limit workflow"
Sleep 1500ms
Enter
Type "ft robot workflow recent --filter rate_limit_recovery --format toon"
Sleep 800ms
Enter
Sleep 5s

# ---- Section 3: search recovers a past error ----------------------------

Type "# 3. Search recovers a past error from the swarm corpus"
Sleep 2s
Enter
Type "ft search 'Connection refused' --since 5m --format toon"
Sleep 800ms
Enter
Sleep 5s

# Drill into one event.
Type "ft search 'Connection refused' --since 5m --limit 1 --pane-context"
Sleep 800ms
Enter
Sleep 5s

# ---- Section 4: mission orchestration -----------------------------------

Type "# 4. Mission orchestration — run a multi-pane action plan"
Sleep 2s
Enter
Type "ft robot mission list --format toon"
Sleep 800ms
Enter
Sleep 4s

# Show a single mission's TX log.
Type "ft robot mission show demo-cleanup --format toon"
Sleep 800ms
Enter
Sleep 5s

# ---- Section 5: detections summary --------------------------------------

Type "# 5. The 5-min summary"
Sleep 2s
Enter
Type "ft robot --format toon events --since 5m --summary"
Sleep 800ms
Enter
Sleep 6s

# ---- Outro --------------------------------------------------------------

Type "# This is what FrankenTerm does for an active swarm — observe,"
Sleep 1500ms
Enter
Type "# detect, auto-handle, recover, and orchestrate. Live, not mocked."
Sleep 2500ms
Enter
Sleep 2s
