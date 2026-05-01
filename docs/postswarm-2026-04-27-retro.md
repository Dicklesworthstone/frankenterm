# Postswarm Retrospective — 2026-04-27 Swarm Session

Closing retrospective for the 1h45m / 35-commit / 16-bead-close
swarm session on 2026-04-27. Tracked under
[ft-v5lz3](#).

## What worked

### Throughput per agent-hour was unambiguously high

35 commits across ~7 agent-equivalents over 1h45m. The 4-minute
tick cadence kept agents from drifting into long deliberations
without producing artefacts.

### The FINAL exploratory pass surfaced real work

Dispatching one of `/security-audit-for-saas`,
`/modes-of-reasoning-project-analysis`,
`/profiling-software-performance`,
`/deadlock-finder-and-fixer`, `/mock-code-finder`,
`/reality-check-for-project` per pane filed 13 new beads, **15 of
which closed inside the same session**. The pattern works because
each skill is a deep, focused sweep that produces concrete,
actionable findings — agents could pick up the resulting beads
immediately without needing to triage what to work on.

### Operator agent + 4-minute ticks

A dedicated operator pane driving periodic check-ins — rather than
treating coordination as ambient — kept stuck panes from going
unnoticed for >4 minutes. The 8 SO-rules in the AGENTS.md addendum
came from observing this loop in action.

### Per-agent `CARGO_TARGET_DIR`

The operator-helper script `scripts/clean-stale-targets.sh`
cleans `/tmp/ft-*-target/` dirs once an agent's bead closes; the
`CARGO_TARGET_DIR=/tmp/ft-<slug>-target` convention prevented
build-lock contention. Disk peaked at 96% (7 active 98GB targets)
and self-cleaned to 33GB by session end.

## What didn't

### Cobalt's silent abandonment of wa-nu4.3.9

Codex-cobalt claimed wa-nu4.3.9 for 1h36m+ with the last commit
(25bb607a) landing 36m before session end and no further activity.
The bead stayed `in_progress` under an unresponsive assignee. Fix
applied via SO-8 in the new playbook: **broadcast first, force-
release only on silence** — preserves agent autonomy when they're
just slow, surfaces the abandonment when they're truly stuck.

### Parent-child propagation bug

A `br dep add child parent --type blocks` that should have been
`--type related` (or `--type parent-child` with reversed direction)
created blocked-by edges that confused the dependency resolver.
The pattern this session settled on: **continuations use `--type
related`; only true preconditions use `blocks`**.

### Pane 3's zsh crash

Pane 3 fell back to a bare zsh prompt at tick #11 after `ntm
--robot-interrupt --interrupt-msg "..."` leaked the message text
to zsh as `Reply: command not found`. Recovery required `tmux
send-keys -t S:0.N "cod" Enter` to restart the codex CLI. SO-1 in
the new playbook codifies the `--robot-send` preference for
cooperative agents.

### Memory-system drift

Three subsystem-sweep memories (patterns / capture-storage /
mux-term-codec) had become stale — the original sweeps had landed
the relevant fixes, but the memory entries lingered as if work
were still pending. Fixed in ft-v5lz3.1 family with `.1.4`
shipping an automation script to detect this pattern and prevent
recurrence.

## What we'd do differently

### File the FINAL pass beads BEFORE the swarm starts

The FINAL pass filed 13 beads; the pre-FINAL pass had 4. The
swarm did its best work when there were many ready beads to pull
from. Future sessions: pre-file the exploratory-skill beads at
session-open so the swarm has saturating depth from tick #1.

### Make `br ready` the per-tick filter, not `br list --status open`

Operator dispatch at tick boundaries should pull from `br ready`
(no blockers) rather than the full open list. Several panes pulled
beads with unmet preconditions and stalled briefly until the
blockers cleared.

### Rotate `--robot-send` vs `ntm send` per-tick-purpose

`ntm send` runs through CASS dedup which blocks repeat sends
without `--no-cass-check`. For the 4-minute periodic tick-check
nudges, `--robot-send` is non-interactive and bypasses dedup
prompts. SO-7 in the new playbook captures this.

### Tighten the convergence threshold

`commits-1h ≤ 2` lagged real convergence by ~45 min; the
session's actual convergence point had `commits-1h = 6`. SO-5
captures the empirical adjustment.

## Concrete process changes shipped

The 8 SO-rules in AGENTS.md (ft-v5lz3.3 / commit e289e84c) are
the durable artifact. The operator-helper scripts +
`docs/operator-runbook.md` (ft-v5lz3.2 family) are the toolchain
support. Future swarms operate with both.

## Session-end state for the record

- 35 commits total; newest 25 (oldest first):
  25bb607a, 357fb60c, 719a05d7, d2525149, afd692a3, 46cdcffe,
  b665e256, 15223aed, 2e51b631, 652600f4, c1aea540, a64810e4,
  a6e0c2cd, 17bea2d5, 9407287e, 82c8aa78, 6a0590d2, d05e5b7b,
  d858725c, 1658d0f6, 3fee5c13, 069db5ce, 056b58e0, 76d82223,
  bf9db5d5.
- 16 beads closed inside the session.
- 1 new bead filed but not started: ft-ombfl (later retired
  during the 2026-05-01 swarm).
- 1 epic in_progress under unresponsive assignee at session end:
  wa-nu4.3.9 (audited + released in ft-v5lz3.4).
- 3 stale MEMORY.md entries flagged + cleaned.
- 2 ad-hoc operator scripts committed (`scripts/swarm-tick.sh`
  + `scripts/clean-stale-targets.sh`).
