# Operator Runbook — Driving a frankenterm Swarm

**Audience:** A new operator (human or agent) about to run a multi-agent
swarm session against this repo. Optimized so you can drive a productive
session without consulting chat history from any prior session.

**Prerequisites assumed:** ntm is configured, `br` and `bv` are on
`$PATH`, you can read AGENTS.md.

---

## Quick Reference (10-line drive-by)

```text
PRE:    rch healthy? disk <90%? beads not corrupted? ntm session up?
TICK 1: dispatch initial marching orders to each pane (slug-based target dirs)
LOOP:   every ~4 min — swarm-tick.sh → tail panes → classify → nudge idle
NUDGE:  ntm --robot-send -t S:0.N "msg" + tmux Enter (twice for codex)
RESET:  in_progress >2h with no commits → broadcast first, force-release after silence
DISK:   >96% → run scripts/clean-stale-targets.sh; nudge agents to rm release/
MODE:   CLAIM → REVIEW → FINAL → DRAIN; pivot when the relevant trigger fires
DONE:   commits-1h ≤ 4 + open=0 + ready=0 + ≥2 cc panes "converged"
END:    write SESSION SUMMARY → close beads → push → notify
```

Read the rest only when the loop deviates from steady-state.

---

For 64+ CPU / 256 GiB hosts, follow
[`docs/high-core-swarm-runbook.md`](high-core-swarm-runbook.md) before
claiming high-scale proof or changing large-fleet tuning.

---

## 1. Pre-flight

Before sending the first marching order, verify the environment.

| Check | Command | Pass condition |
|-------|---------|----------------|
| rch workers healthy | `rch status` | ≥1 worker `online` |
| Disk space | `df -h /System/Volumes/Data` | `Capacity` < 90% |
| Beads DB walkable | `br ready --json \| jq length` | Returns an integer (no errors) |
| ntm session exists | `tmux has-session -t frankenterm` | Exit 0 |
| Agent panes responding | `ntm --robot-snapshot -t frankenterm` | All panes named (cc_1, cc_2, cod_1...) |
| Agent Mail usable | Agent Mail MCP macro/tool call | Succeeds, or fails once and succeeds on one retry |

**If any pre-flight check fails:**
- rch unhealthy → for RCH-required proof lanes, record an infra-blocked
  proof-doctor verdict; use `scripts/cargo-local.sh` only as explicitly
  labeled local smoke when the Bead allows non-proof diagnostics.
- Disk >90% → run `scripts/clean-stale-targets.sh` *before* dispatching work, not after.
- Beads DB locked → wait 10s; if persistent, `lsof .beads/beads.db` to find writer; if no writer, the DB is corrupted (use `bv` for triage instead, per MEMORY.md note `br-db-corruption`).
- ntm pane missing → relaunch via the project's spawn script before the swarm tick begins.
- Agent Mail red/unreachable → retry once after a few seconds; if it still fails, do not repair, restart, or kill the shared service. Continue with a Beads-only handoff snapshot:
  ```bash
  scripts/swarm-tick.sh --agent-mail-fallback frankenterm
  ```
  The snapshot includes the red-mail marker, active assignees from in-progress Beads, freshness/staleness, ready work, and dirty-file conflict hints.

## 2. Proof-Doctor Gate For Proof Lanes

Use proof-doctor for every Bead whose closeout depends on RCH, Cargo, clippy,
tests, benches, E2E, high-scale worker predicates, or proof-lane evidence. It
is the operator vocabulary for separating RCH/tooling blockers, source
failures, dirty-tree ownership, invalid command shapes, and inconclusive logs.

Primary anchors:

- `docs/proposals/ft-wik9p-proof-doctor-verdict-schema.md` for the status,
  reason-code, handoff, and robot-mode envelope contract.
- `docs/proposals/ft-tn6cw-proof-lane-evidence-taxonomy.md` for proof-state
  truthfulness rules and invalid command-shape examples.
- `crates/frankenterm-core-audit-types/src/proof_doctor.rs` for the classifier
  DTOs, JSON/TOON golden coverage, and the E2E fixture scenarios.
- `crates/frankenterm-core-audit-types/src/proof_handoff.rs` and
  `crates/frankenterm-core-audit-types/src/proof_lane.rs` for Beads/Agent Mail
  handoff text and durable ledger projections.

### 2.1 Pre-proof checklist

Before launching or claiming a proof lane:

1. Read the Bead and dependency state with `br show <id> --json`.
2. Confirm the exact proof backend, command, package/test filter, and
   `CARGO_TARGET_DIR`. RCH-required lanes use direct remote Cargo argv, for
   example:
   ```bash
   rch exec -- env CARGO_TARGET_DIR=/tmp/<bead>-<purpose>-target cargo test -p <crate> <filter> -- --nocapture
   ```
3. Inspect `git status --short` and active ownership. If a dirty path overlaps
   the proof scope and belongs to another Bead, agent, or reservation, classify
   `dirty_tree_blocked` instead of running over it.
4. Record the RCH binary/tool state, effective timeout setting, selected worker
   if any, and worker predicate if the proof depends on hardware capacity.
5. Keep the intended command as argv in the verdict. Do not translate a shell
   string into proof after the fact.

Invalid for an RCH-required closeout unless retained metadata proves remote
Cargo started:

```bash
cargo test ...
scripts/cargo-local.sh test ...
rch exec -- bash -lc 'cargo test ...'
rch exec -- env CARGO_TARGET_DIR=/tmp/foo bash -lc 'cargo test ...'
```

Local Cargo can be cited only as local smoke or docs-static validation, never
as remote proof for a Bead whose proof lane requires RCH.

### 2.2 Classify the evidence before claiming it

RCH transfer logs are transfer evidence only:

- "Selected worker" means worker selection happened.
- "Sync completed" means workspace transfer happened.
- "Remote Cargo reached" requires retained logs proving Cargo or rustc started
  on the remote worker.
- "Tests passed" requires a terminal pass for the intended test or E2E scope
  plus retained artifacts.

Current scenario mapping:

| Evidence | Proof-doctor status | Required wording |
| --- | --- | --- |
| Installed RCH still emits the stale external-timeout wrapper and fails before Cargo | `infra_blocked` | "RCH wrapper/tooling blocked before Cargo; no source verdict." |
| Patched RCH reaches remote Cargo/rustc, then first-party code fails to compile | `source_blocked` | "Remote Cargo/rustc reached first-party source; source is blocked in `<path>` and owned by `<bead-or-agent>`." |
| Dirty active file overlaps the proof path and another Bead/agent owns it | `dirty_tree_blocked` | "Dirty owned path blocks attribution; do not run or close this proof without owner release." |
| RCH selected a worker or synced but there is no retained Cargo/rustc/test evidence | `inconclusive` | "RCH sync completed, but no remote Cargo proof was retained; rerun with fail-closed logging." |
| Direct RCH Cargo lane exits 0 with complete artifacts and ledger validation | `passed` | "Remote proof passed with retained RCH/Cargo evidence and complete artifacts." |

For `source_blocked`, include the first compiler/test diagnostic path and the
owner source. For `infra_blocked` before Cargo, do not file source findings
against the package under test. For `dirty_tree_blocked`, do not edit the
overlapping file unless the owner releases it or the user explicitly assigns
the conflict.

### 2.3 Closeout adoption gate

Every future proof-lane Bead closeout must include either:

```text
Proof-doctor: <status>; phase <phase>; reason <reason_code>; verdict <verdict_id or artifact>; remote Cargo <reached|not reached>; owner <owner or none>; closeout <safe|blocked>.
```

or an explicit non-applicability sentence:

```text
Proof-doctor: not applicable; docs-static change only; no Cargo/RCH proof lane claimed.
```

Closeout rules:

- A green claim requires `passed` plus proof-lane ledger validation or equivalent
  retained artifact evidence.
- `runnable` is only a preflight result; it is not a pass.
- `infra_blocked`, `dirty_tree_blocked`, `invalid`, `skipped_not_proven`, and
  `inconclusive` do not prove source health.
- `source_blocked` and `test_blocked` are real red results only after remote
  Cargo/rustc/test execution is positively observed.
- Beads comments and Agent Mail handoffs should carry the same status,
  reason code, command, worker/sync/Cargo evidence, owner, and next action.

---

## 3. Tick #1 — establish baseline

The first tick sets the contract for the session. Skip steps and you
will pay 30+ minutes recovering shared context later.

1. **Snapshot pane state.**
   ```bash
   ntm --robot-snapshot -t frankenterm > /tmp/swarm-baseline.json
   ```
   This is your "before" picture; you will diff against it later when
   classifying panes.

2. **Read AGENTS.md sections RULE 0 / 0.5 / 1 / 2 + Swarm Orchestration Playbook.**
   These are the durable hard rules. If your dispatch contradicts any
   of them, fix the dispatch.

3. **Decide the tick cadence.** Default: 4 minutes. Faster is wasteful;
   slower lets stuck panes drift. Each tick spends ~30 seconds of your
   own context, so 4 minutes is the sweet spot.

4. **Dispatch initial marching orders.** Each pane gets:
   - Their slug (cc_1, cod_1, etc.) — used as `CARGO_TARGET_DIR=/tmp/ft-<slug>-target`
     to avoid lock contention.
   - The setup checklist (read AGENTS.md, identify slug, claim a
     ready bead, confirm rch usage, ship-or-surface within an hour).
   - A reminder that committed changes ship via `br close + sync +
     git push origin main`.

5. **Record session-start timestamp.** Used later for `commits-1h`
   computations during convergence detection.

---

## 4. Steady-state tick (the 4-minute loop)

Every 4 minutes, run this loop. Each step in order; the order matters.

### 4.1 — Run swarm-tick.sh

```bash
scripts/swarm-tick.sh frankenterm > /tmp/swarm-tick.json
```

This emits a compact JSON snapshot: per-pane state, recent commit
attribution, ready/in_progress bead counts, disk/usb-nvme percentages.
It acquires an operator lock so two concurrent operator scripts can't
corrupt shared state.

### 4.2 — Tail each pane

Read the last ~30 lines from each pane via `tmux capture-pane -p
-t S:0.N`. Don't skip this — `--robot-is-working` alone won't catch
the case where a pane is staring at a confirm prompt.

### 4.3 — Classify each pane

| State | Signal | Operator action |
|-------|--------|-----------------|
| WORKING | New commits in last tick OR active tool calls visible | Leave alone |
| IDLE | No tool calls, no new commits, but pane prompt shows agent UI ready | Send a fresh marching order |
| STUCK | Identical TOOL-OUTPUT lines for 2+ ticks AND no commits | Nudge with a status check; if still stuck after 1 tick, force-reset |
| DEAD | Pane fell back to bare zsh prompt | Relaunch agent (`tmux send-keys -t S:0.N "cc" Enter` or similar) |
| AUTH_FAILED | Auth-error in pane output | Rotate account via caam, then re-dispatch |

**Trap:** Codex panes show *idle-placeholder text* ("Find and fix a bug
in @", "Explain this codebase", etc.). That is **not** stuck. See
AGENTS.md Rule SO-3.

### 4.4 — Dispatch nudges to idle/stuck panes

```bash
ntm --robot-send -t frankenterm:0.N "your message here"
tmux send-keys -t frankenterm:0.N Enter
# For codex panes, second Enter ~2s later:
sleep 2 && tmux send-keys -t frankenterm:0.N Enter
```

Why two Enters for codex: see AGENTS.md Rule SO-2.

**Do not** use `ntm --robot-interrupt --interrupt-msg` for cooperative
nudges; it can crash codex panes (Rule SO-1).

### 4.5 — Reset stalled in_progress beads

Beads in_progress >2h with no commit linkage in the last 30 min are
candidates for force-release. Per AGENTS.md Rule SO-8:

1. Broadcast a status check to the assignee first:
   ```text
   "<slug>: ft-XXXX in_progress 2h+ with no commits. Commit and close,
    OR `br update --status open ft-XXXX --assignee=''`."
   ```
2. Wait one full tick.
3. If silent, force-release: `br update ft-XXXX --status open --assignee ''`.

Preserving agent autonomy first is what makes the broadcast work.
Force-releasing immediately erodes trust and wastes work-in-progress.

### 4.6 — Disk pressure check

If `swarm-tick.json` reports `disk_used_pct >= 96`:

```bash
scripts/clean-stale-targets.sh --inventory --threshold-hours 12
scripts/clean-stale-targets.sh --inventory --format json --threshold-hours 12
scripts/clean-stale-targets.sh --dry-run --threshold-hours 12
# Review the would-remove list; if it looks safe:
scripts/clean-stale-targets.sh --threshold-hours 12
```

The inventory commands are read-only. Use the text form for human review and
the JSON form in Beads comments when requesting deletion authorization; it
reports per-target age, size, active-skip status, and total reclaimable bytes.

Also nudge any agent whose bead is closed to clean their own
`/tmp/ft-<slug>-target/release` directory. Per AGENTS.md Rule SO-6,
keep the `debug` subdirectory for incremental rebuilds.

---

## 5. Mode transitions

A swarm session has four modes. The trigger for each transition is
listed below. Do not skip modes; each one prepares the swarm for the
next.

| Mode | Goal | Triggers transition out |
|------|------|-------------------------|
| **CLAIM** | Drain the ready queue | `br ready --json \| jq length` ≤ 2 |
| **REVIEW** | Catch defects in shipped beads, file follow-ons | All in_progress beads have commits in last 30m |
| **FINAL** | Push remaining in_progress to closed | ≥2 cc panes report "converged" |
| **DRAIN** | Wind down: clean disk, write summary, close session | (terminal mode) |

Reverse transitions are possible (e.g., REVIEW → CLAIM if a bug review
files 5 new P1 beads), but rare. Most sessions move forward
monotonically.

---

## 6. Recovery recipes

When a pane shows non-steady-state, match the symptom to the recipe.

### 6.1 — Pane shows bare zsh prompt

The agent process exited (typically codex after a bad
`--robot-interrupt`). Recover:

```bash
tmux send-keys -t frankenterm:0.N "cod" Enter   # or "cc"
sleep 1
tmux send-keys -t frankenterm:0.N Enter          # second Enter for codex
ntm --robot-send -t frankenterm:0.N "Re-dispatching: <original prompt>"
tmux send-keys -t frankenterm:0.N Enter
```

### 6.2 — Pane is rate-limited

The pane's tail shows "rate limited" or similar. Rotate via caam
(`caam list`, `caam switch <profile>`) and re-dispatch. Save the
rate-limited account for a future session.

### 6.3 — Context saturation

Pane's tail shows the agent referring to early-session context
ambiguously, or repeats the same approach despite explicit corrections.
Restart the pane:

```bash
tmux send-keys -t frankenterm:0.N C-d            # exit current agent
sleep 2
tmux send-keys -t frankenterm:0.N "cc" Enter     # relaunch
ntm --robot-send -t frankenterm:0.N "<setup prompt + current bead context>"
```

### 6.4 — Abandoned in_progress bead

Apply the broadcast-first protocol from §4.5. Do not reset the bead's
assignee until you have given the agent one full tick to respond.

### 6.5 — Beads DB busy / corrupted

`br` returns `database is busy` or `database disk image is malformed`:

- Busy: wait 10s, retry. The DB has internal locking that resolves under load.
- Malformed: per MEMORY.md `br-db-corruption`, fall back to `bv --robot-triage`
  for read-only triage. Do not attempt `br close` until the DB is rebuilt.

---

## 7. Convergence detection

Stop when *all four* hold for one full tick:

1. `commits-1h` ≤ 4 (loosened from the spec's `≤ 2`; see AGENTS.md Rule SO-5).
2. `br ready --json | jq length` == 0.
3. `br list --status=in_progress --json | jq '.issues | length'` ≤ 1
   (the surviving in_progress is allowed if it's an EPIC or an
   long-running strategic bead with active commit linkage).
4. ≥ 2 cc panes have replied "converged" or "converged." to a single
   CONFIRM nudge.

Codex panes will not generally reply "converged"; treat their idle-no-
defects state as implicit agreement (Rule SO-4).

**False-positive guard:** Before declaring convergence, verify the
last 30 minutes of commits include at least one bead-closure commit
(`chore(beads):`). If not, the swarm is stalled, not converged.

---

## 8. Wind-down — generating the SESSION SUMMARY

Once convergence is detected:

1. Write a SESSION SUMMARY note (markdown, in your operator scratch
   space — *not* committed). Include:
   - Tick count, session duration, distinct beads touched.
   - Beads opened, closed, and their priorities.
   - Anomalies: pane crashes, force-releases, disk events.
   - Lessons: any rule that fired but didn't fit cleanly.
2. Push the final beads commit and any pending operator-script
   commits.
3. Stop the tick loop.
4. Notify any human stakeholders if applicable.
5. Save the SESSION SUMMARY as a memory entry under `feedback_*` only
   if it contains a *new* lesson; otherwise the runbook covers it.

---

## 9. Anti-patterns

These are the failure modes most often observed. Avoid each one.

- **`--robot-interrupt --interrupt-msg "<text>"` for cooperative agents.**
  Crashes codex panes (Rule SO-1). Use `--robot-send` + tmux Enter.
- **Polling more often than every 4 minutes.** Burns operator context
  for no benefit. Agents need time between nudges to actually work.
- **Trusting `--robot-is-working` alone for stuck-pane detection.**
  Always tail the pane content; idle-placeholder text fools the
  working-bit (Rule SO-3).
- **Force-releasing in_progress beads at a 2h cutoff without a broadcast.**
  Erodes agent autonomy, wastes work-in-progress (Rule SO-8).
- **Skipping per-agent `CARGO_TARGET_DIR`.** Concurrent agents stomp
  each other's lock files; build times balloon.
- **Sending the same nudge text via `ntm send` rather than `--robot-send`.**
  CASS dedup blocks repeats; the agent never sees the second nudge.
- **Committing ad-hoc operator decisions to `MEMORY.md`.** Memory is
  for durable, generalizable lessons. Tick-specific notes belong in
  the SESSION SUMMARY only.

---

## Cross-references

- **AGENTS.md** — Rule 0, 0.5, 1, 2, and the **Swarm Orchestration
  Playbook** section (Rules SO-1 through SO-8). The runbook applies
  those rules; it does not redefine them.
- **`vibing-with-ntm` skill** — operator-tick playbook with concrete
  command sequences for individual primitives.
- **`ntm` skill** — primitive reference for `--robot-send` /
  `--robot-interrupt` / `send`.
- **`scripts/swarm-tick.sh`** — operator script run every tick;
  emits the JSON snapshot this runbook consumes.
- **`scripts/clean-stale-targets.sh`** — disk-pressure relief.
- **`scripts/memory_staleness_check.py`** — monthly memory hygiene.

If anything in this runbook contradicts AGENTS.md, AGENTS.md wins —
file a bead to reconcile.
