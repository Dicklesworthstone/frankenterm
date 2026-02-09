# Compatibility Runbook Template: Terminal/Mux Environment

**Bead:** wa-3mus (FTUI-08.3.a)
**Date:** 2026-02-09
**Parent:** wa-e69a (FTUI-08.3 Terminal/mux compatibility certification matrix)

---

*Copy this template for each target environment. Fill in all sections.
Delete instructions in italics before submitting.*

## 1  Environment Metadata

| Field | Value |
|-------|-------|
| **Environment name** | *e.g., WezTerm 20250101-000000* |
| **Terminal emulator** | *Name + version* |
| **Multiplexer** | *tmux/screen/zellij/none + version* |
| **OS** | *e.g., Ubuntu 24.04, macOS 15.2* |
| **$TERM** | *e.g., xterm-256color, screen-256color* |
| **$TERM_PROGRAM** | *e.g., WezTerm, iTerm2.app* |
| **Shell** | *e.g., bash 5.2, zsh 5.9* |
| **Locale** | *e.g., en_US.UTF-8* |
| **Color support** | *256 / truecolor / none* |
| **Unicode support** | *Yes / Limited / No* |
| **wa build** | *commit hash + features (ftui)* |
| **Test date** | *YYYY-MM-DD* |
| **Tester** | *Agent name or human* |

## 2  Test Scope

### 2.1  Headless Tests

| Test Suite | Command | Expected |
|-----------|---------|----------|
| View adapters | `cargo test -p wa-core --features ftui -- view_adapters` | All pass |
| TextInput edge cases | `cargo test -p wa-core --features ftui -- edge_` | All pass |
| Terminal session lifecycle | `cargo test -p wa-core --features tui -- tui::terminal_session` | All pass |
| Command handoff traces | `cargo test -p wa-core --features tui -- tui::command_handoff` | All pass |
| Output gate | `cargo test -p wa-core --features tui -- output_gate` | All pass |
| E2E headless scenarios | `cargo test -p wa-core --features ftui -- e2e_` | All pass |
| Snapshot/golden suite | `cargo test -p wa-core --features ftui -- snapshot_` | All pass |

### 2.2  PTY E2E Tests

*Run in the actual target terminal environment.*

| Scenario | Seed | Expected |
|----------|------|----------|
| startup_and_quit | 0x0001 | Clean startup, quit exits cleanly |
| view_navigation | 0x0002 | All views accessible via Tab |
| events_filter | 0x0003 | Filter input works, results update |
| resize_during_render | 0x0004 | No crash on resize |
| command_handoff | 0x0005 | Suspend/resume restores TUI |
| rapid_input | 0x0006 | No corruption under input storm |

### 2.3  Manual Checks

| Check | Procedure | Expected |
|-------|-----------|----------|
| **Alternate screen** | Start wa TUI, quit | Terminal restored, no leftover alt-screen |
| **Raw mode** | Start wa TUI, quit | Terminal echoes input normally |
| **Cursor visibility** | Start wa TUI, quit | Cursor visible after exit |
| **Color rendering** | View all 7 TUI views | Colors match design (severity, status labels) |
| **Unicode rendering** | Add event with Unicode chars | Characters display correctly |
| **Wide characters** | CJK text in pane titles | Characters occupy correct width |
| **Resize** | Resize terminal during TUI | Layout redraws correctly |
| **Ctrl+C** | Press Ctrl+C during TUI | Graceful exit, terminal restored |
| **SIGTERM** | `kill <pid>` during TUI | Terminal restored (raw mode off) |

## 3  Expected Outcomes

### 3.1  Pass Criteria

All of the following must be true:
- All headless tests pass
- All PTY E2E scenarios pass (or fail only on known limitations documented below)
- All manual checks pass
- Terminal is restored to usable state after every exit path (normal, Ctrl+C, SIGTERM)
- No rendering artifacts visible during normal operation

### 3.2  Known Limitations

*Document any known environment-specific issues here. Example:*

| ID | Description | Severity | Workaround |
|----|-------------|----------|------------|
| *KL-001* | *tmux does not forward OSC 52 (clipboard)* | *Cosmetic* | *N/A — clipboard not used by wa TUI* |

## 4  Results

### 4.1  Headless Test Results

| Test Suite | Result | Count | Notes |
|-----------|--------|-------|-------|
| View adapters | *pass/fail* | *N/N* | |
| TextInput edge cases | *pass/fail* | *N/N* | |
| Terminal session | *pass/fail* | *N/N* | |
| Command handoff | *pass/fail* | *N/N* | |
| Output gate | *pass/fail* | *N/N* | |
| E2E headless | *pass/fail* | *N/N* | |
| Snapshot/golden | *pass/fail* | *N/N* | |

### 4.2  PTY E2E Results

| Scenario | Result | Elapsed | Notes |
|----------|--------|---------|-------|
| startup_and_quit | *pass/fail* | *N.Ns* | |
| view_navigation | *pass/fail* | *N.Ns* | |
| events_filter | *pass/fail* | *N.Ns* | |
| resize_during_render | *pass/fail* | *N.Ns* | |
| command_handoff | *pass/fail* | *N.Ns* | |
| rapid_input | *pass/fail* | *N.Ns* | |

### 4.3  Manual Check Results

| Check | Result | Notes |
|-------|--------|-------|
| Alternate screen | *pass/fail* | |
| Raw mode | *pass/fail* | |
| Cursor visibility | *pass/fail* | |
| Color rendering | *pass/fail* | |
| Unicode rendering | *pass/fail* | |
| Wide characters | *pass/fail* | |
| Resize | *pass/fail* | |
| Ctrl+C | *pass/fail* | |
| SIGTERM | *pass/fail* | |

## 5  Evidence Checklist

*All items must be present for a valid certification.*

- [ ] Environment metadata table filled (Section 1)
- [ ] All headless test suites run with results recorded (Section 4.1)
- [ ] All PTY E2E scenarios run with results recorded (Section 4.2)
- [ ] All manual checks performed with results recorded (Section 4.3)
- [ ] Known limitations documented (Section 3.2)
- [ ] PTY E2E failure artifacts uploaded (if any failures)
- [ ] Screenshot of TUI running in target environment
- [ ] `cargo test` console output saved as artifact
- [ ] `env` output from target terminal saved in env.json

## 6  Verdict

### Decision Rubric

| Verdict | Criteria |
|---------|----------|
| **PASS** | All headless tests pass. All PTY E2E pass. All manual checks pass. No known S1/S2 issues. |
| **CONDITIONAL PASS** | All headless tests pass. PTY E2E or manual checks have known S3+ issues with documented workarounds. No S1/S2 issues. |
| **FAIL** | Any S1 (crash) or S2 (terminal corruption) issue. Or headless tests fail. |

### Final Verdict

| Field | Value |
|-------|-------|
| **Verdict** | *PASS / CONDITIONAL PASS / FAIL* |
| **Blocking issues** | *None / List IDs* |
| **Conditions** | *N/A / List conditions for CONDITIONAL PASS* |
| **Reviewer** | *Agent or human name* |
| **Date** | *YYYY-MM-DD* |

## 7  Target Environment Matrix

*Reference: these are the environments that must be certified before release.*

| Environment | Priority | Status |
|-------------|----------|--------|
| WezTerm (Linux) | P0 | *Not started* |
| WezTerm (macOS) | P0 | *Not started* |
| tmux + xterm-256color (Linux) | P1 | *Not started* |
| screen + xterm-256color (Linux) | P2 | *Not started* |
| iTerm2 (macOS) | P1 | *Not started* |
| Ghostty (Linux) | P2 | *Not started* |
| Alacritty (Linux) | P2 | *Not started* |
| Kitty (Linux) | P2 | *Not started* |
| zellij (Linux) | P3 | *Not started* |
| VS Code integrated terminal | P2 | *Not started* |

## References

- `docs/ftui-pty-failure-artifacts.md` — PTY failure artifact schema (wa-308u)
- `docs/ftui-pty-fixture-strategy.md` — PTY fixture seed/timing (wa-1qr1)
- `docs/ftui-teardown-harness.md` — restoration invariants (wa-3fed)
- `docs/ftui-parity-matrix-template.md` — parity matrix schema (wa-33fn)
