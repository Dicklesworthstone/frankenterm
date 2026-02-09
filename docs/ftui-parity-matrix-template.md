# FTUI Parity Matrix Template and Evidence Rubric

**Bead:** wa-33fn (FTUI-01.3.a)
**Date:** 2026-02-09
**Extends:** ADR-0006 (Parity Contract)

## Purpose

Reusable template for every FTUI view/input/terminal parity subtask.  Fill one
copy per migration unit (e.g., FTUI-05.3 Panes view, FTUI-06.1 global input).
The schema, rubric, and naming conventions defined here are mandatory for all
parity evidence.

---

## 1  Parity Matrix Schema

Each parity row documents one observable behavior that must be compared between
the legacy ratatui backend and the ftui backend.

### Required Fields

| Field | Type | Description |
|-------|------|-------------|
| `id` | string | Stable identifier: `P-{view}-{seq}` (e.g., `P-PANES-003`) |
| `category` | enum | One of: `layout`, `data`, `keybinding`, `filter`, `selection`, `render`, `terminal`, `lifecycle` |
| `description` | string | One-sentence description of the observable behavior |
| `severity` | enum | `blocking` = must match exactly; `cosmetic` = visual-only difference permitted; `informational` = tracked but not gated |
| `legacy_behavior` | string | What the ratatui backend does (deterministic reproduction steps) |
| `ftui_behavior` | string | What the ftui backend does (same reproduction steps) |
| `verdict` | enum | `pass` (identical), `intentional-delta` (accepted divergence), `fail` (unaccepted divergence), `untested` |
| `evidence` | string | Path(s) to evidence artifacts (see naming convention below) |
| `delta_id` | string or null | If `intentional-delta`, reference to the Intentional Deltas Ledger in ADR-0006 (e.g., `D4`) |
| `notes` | string | Free-form notes, edge cases, or links to related beads |

### Example Row

```
id:              P-PANES-003
category:        keybinding
description:     'j' moves selection down, wrapping at bottom
severity:        blocking
legacy_behavior: Selection index increments; wraps from last to 0
ftui_behavior:   Selection index increments; wraps from last to 0
verdict:         pass
evidence:        evidence/ftui-05.3/unit/panes_j_moves_down.log
delta_id:        null
notes:           Tested with 0, 1, and 100+ items
```

---

## 2  Scoring Rubric

### Verdict Definitions

| Verdict | Meaning | Gate Effect |
|---------|---------|-------------|
| `pass` | Behavior is identical under deterministic reproduction steps | None — row is green |
| `intentional-delta` | Behavior differs, but the delta is listed in ADR-0006 Intentional Deltas Ledger with an accepted `delta_id` | None — row is green with annotation |
| `fail` | Behavior differs and the delta is NOT in the ledger | **Blocks merge.** Must fix or promote to intentional-delta with team sign-off |
| `untested` | No evidence collected yet | **Blocks merge.** Must produce evidence before the subtask can close |

### Pass Criteria per Subtask

A parity subtask passes when ALL of:

1. Every `blocking` row has verdict `pass` or `intentional-delta`
2. Every `cosmetic` row has verdict `pass`, `intentional-delta`, or is
   annotated with a follow-up bead for cosmetic polish
3. Zero rows have verdict `untested`
4. Every `intentional-delta` row has a valid `delta_id` referencing ADR-0006
5. Every row has at least one evidence artifact path that exists

### Intentional Delta Acceptance

A delta may be accepted only when:

- It is added to ADR-0006 Section "Intentional Deltas Ledger" with fields:
  ID, What Changed, Why, User Impact, Rollback
- At least one reviewer has signed off (comment on the bead or PR)
- The delta does not remove functionality (information content must be preserved)

### Deterministic Reproduction

Every `legacy_behavior` and `ftui_behavior` field must include enough detail
for another agent or developer to reproduce the observation:

- Terminal size (e.g., 80x24)
- Input sequence (e.g., "press j three times")
- Starting state (e.g., "Panes view, 5 items, selection at index 2")
- Expected observable (e.g., "selection moves to index 3, highlight bar shifts")

---

## 3  Evidence Types

### Unit Test Evidence

Unit tests in `ftui_stub.rs` and adapter modules validate reducer logic,
state transitions, and view model correctness.

| Evidence Type | When Required | What to Capture |
|---------------|---------------|-----------------|
| Test pass log | Every `blocking` row | `cargo test` output showing test name + PASS |
| Assertion detail | Every `fail` row | Full assertion failure with expected/actual values |
| Snapshot output | `render` and `layout` rows | Frame buffer content at specified terminal size |

### PTY E2E Evidence

PTY end-to-end tests validate real terminal behavior: escape sequences, cursor
positioning, resize handling, and full input/output round-trips.

| Evidence Type | When Required | What to Capture |
|---------------|---------------|-----------------|
| PTY transcript | `terminal` and `lifecycle` rows | Raw PTY I/O log (redacted) |
| Screenshot diff | `layout` and `render` rows | Before/after terminal screenshots at specified size |
| Timing log | `lifecycle` rows (command handoff, suspend/resume) | Timestamps for state transitions |

---

## 4  Artifact Naming Convention

All evidence artifacts live under `evidence/` in the repository root, organized
by subtask.

### Directory Structure

```
evidence/
  {bead-id}/
    unit/
      {test_name}.log
      {test_name}.snapshot
    e2e/
      {scenario_name}.pty.log
      {scenario_name}.screenshot.txt
      {scenario_name}.timing.log
    matrix.md          <- filled parity matrix for this subtask
```

### Naming Rules

| Component | Format | Example |
|-----------|--------|---------|
| Bead directory | `ftui-{milestone}.{seq}` | `ftui-05.3/` |
| Unit test log | `{test_function_name}.log` | `panes_j_moves_down.log` |
| Snapshot | `{test_function_name}.snapshot` | `panes_view_80x24.snapshot` |
| PTY transcript | `{scenario_name}.pty.log` | `panes_input_cycle.pty.log` |
| Screenshot | `{scenario_name}.screenshot.txt` | `panes_layout_80x24.screenshot.txt` |
| Timing log | `{scenario_name}.timing.log` | `command_handoff_suspend.timing.log` |

### Redaction

All artifacts must be redaction-safe before commit:

- No absolute file paths from the build machine
- No usernames, hostnames, or PIDs
- No timestamps that would cause diff churn (use relative or epoch offsets)
- PTY transcripts: strip ANSI escape sequences unless the test specifically
  validates escape sequence correctness

---

## 5  Filled Matrix Template

Copy this section into `evidence/{bead-id}/matrix.md` for each subtask.

```markdown
# Parity Matrix: {FTUI Subtask Title}

**Bead:** {bead-id}
**Date:** {date}
**Reviewer:** {agent or human name}

## Matrix

| id | category | description | severity | legacy_behavior | ftui_behavior | verdict | evidence | delta_id | notes |
|----|----------|-------------|----------|-----------------|---------------|---------|----------|----------|-------|
| P-{VIEW}-001 | | | | | | untested | | | |
| P-{VIEW}-002 | | | | | | untested | | | |

## Summary

- Total rows: {n}
- Pass: {n}
- Intentional delta: {n}
- Fail: {n}
- Untested: {n}

## Sign-off

- [ ] All blocking rows pass or have accepted deltas
- [ ] All evidence artifacts exist and are redaction-safe
- [ ] Matrix reviewed by at least one other agent/developer
```

---

## 6  Integration with Existing Contracts

This template operationalizes the high-level parity contract in ADR-0006:

| ADR-0006 Section | Template Section |
|-------------------|------------------|
| Parity Matrix: Views | Fill rows with `category: layout, data, render` |
| Parity Matrix: Global Keybindings | Fill rows with `category: keybinding` for global keys |
| Parity Matrix: Per-View Keybindings | Fill rows with `category: keybinding` for view-specific keys |
| Parity Matrix: Terminal Behavior | Fill rows with `category: terminal, lifecycle` |
| Intentional Deltas Ledger | Referenced by `delta_id` field in matrix rows |
| Acceptance Checklist | Encoded in the scoring rubric (Section 2) |

---

## References

- ADR-0006: Parity Contract (source of truth for what must match)
- ADR-0010: One-Writer Rule Adaptation (terminal ownership evidence)
- `docs/test-logging-contract.md` (log level and artifact conventions)
- `crates/wa-core/src/tui/ftui_stub.rs` (primary test location)
