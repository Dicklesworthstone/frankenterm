# Terminal/Mux Compatibility Certification Matrix (FTUI-08.3)

**Bead:** wa-e69a (FTUI-08.3)
**Date:** 2026-02-09
**Parent:** wa-1kut (FTUI-08 Performance, Compatibility, and Resilience Hardening)

---

## 1  Certification Summary

| Environment | Priority | Headless | PTY E2E | Manual | Verdict |
|-------------|----------|----------|---------|--------|---------|
| WezTerm (Linux) | P0 | PASS | N/A (CI) | Deferred | CONDITIONAL PASS |
| WezTerm (macOS) | P0 | — | — | — | Not started |
| tmux + xterm-256color (Linux) | P1 | — | — | — | Not started |
| iTerm2 (macOS) | P1 | — | — | — | Not started |
| screen + xterm-256color (Linux) | P2 | — | — | — | Not started |
| Ghostty (Linux) | P2 | — | — | — | Not started |
| Alacritty (Linux) | P2 | — | — | — | Not started |
| Kitty (Linux) | P2 | — | — | — | Not started |
| VS Code integrated terminal | P2 | — | — | — | Not started |
| zellij (Linux) | P3 | — | — | — | Not started |

## 2  Certified Environment: WezTerm (Linux)

### 2.1  Environment Metadata

| Field | Value |
|-------|-------|
| **Environment name** | WezTerm (Linux) — CI headless |
| **Terminal emulator** | WezTerm (TERM_PROGRAM=WezTerm) |
| **Multiplexer** | none |
| **OS** | Linux 6.17.0-8-generic x86_64 |
| **$TERM** | xterm-256color |
| **$TERM_PROGRAM** | WezTerm |
| **Shell** | zsh |
| **Locale** | en_US.UTF-8 |
| **Color support** | truecolor (256 minimum) |
| **Unicode support** | Yes |
| **wa build** | main branch, features: ftui |
| **Rust toolchain** | rustc 1.95.0-nightly (873d4682c 2026-01-25) |
| **Test date** | 2026-02-09 |
| **Tester** | CalmLynx (automated) |

### 2.2  Headless Test Results

| Test Suite | Command | Result | Count | Notes |
|-----------|---------|--------|-------|-------|
| View adapters | `cargo test -p wa-core --features ftui --lib -- view_adapters` | pass | 76/76 | |
| TextInput edge cases | `cargo test -p wa-core --features ftui --lib -- edge_` | pass | 21/21 | |
| Terminal session lifecycle | `cargo test -p wa-core --features tui --lib -- tui::terminal_session` | pass | 50/50 | |
| Command handoff traces | `cargo test -p wa-core --features tui --lib -- tui::command_handoff` | pass | 19/19 | |
| Output gate | `cargo test -p wa-core --features tui --lib -- output_gate` | pass | 21/21 | |
| E2E headless scenarios | `cargo test -p wa-core --features ftui --lib -- e2e_` | pass | 38/38 | |
| Snapshot/golden suite | `cargo test -p wa-core --features ftui --lib -- snapshot_` | pass | 75/75 | |
| Chaos/resilience | `cargo test -p wa-core --features ftui --lib -- chaos_` | pass | 20/20 | 8 ftui + 12 terminal_session |

**Total headless: 320 tests, 320 pass, 0 fail**

### 2.3  PTY E2E Results

Not executed in this certification cycle (headless CI environment — no interactive PTY available). PTY E2E requires interactive terminal access per the runbook template (`docs/ftui-compat-runbook-template.md`).

### 2.4  Manual Check Results

Deferred — requires interactive terminal session. See runbook template Section 2.3 for the 9 manual checks required.

### 2.5  Known Limitations

| ID | Description | Severity | Mitigation |
|----|-------------|----------|------------|
| KL-001 | ftui `Frame::new()` panics on zero-width or zero-height | S2 | Application must guard against zero-sized terminal reports before constructing frames |
| KL-002 | Events/Triage filter only accepts digits; letter keys are no-ops | Cosmetic | By design — pane IDs are numeric |
| KL-003 | Digit keys 1-7 are consumed by global view-switch handler except on Events/Triage/History views | Cosmetic | By design — filter views suppress global digit shortcuts |

### 2.6  Verdict

| Field | Value |
|-------|-------|
| **Verdict** | CONDITIONAL PASS |
| **Blocking issues** | None |
| **Conditions** | PTY E2E and manual checks deferred to interactive session |
| **Reviewer** | CalmLynx |
| **Date** | 2026-02-09 |

## 3  Cross-Environment Compatibility Notes

### 3.1  Terminal Capability Requirements

The ftui TUI requires these terminal capabilities:

| Capability | Required | Fallback |
|------------|----------|----------|
| Alternate screen buffer | Yes | No fallback — required for TUI mode |
| Raw mode | Yes | No fallback — required for key input |
| 256 color | Recommended | Graceful degradation to 16 colors |
| Truecolor | Optional | Falls back to 256 color palette |
| Unicode/UTF-8 | Yes | ASCII fallback for box drawing only |
| Mouse events | No | Keyboard-only navigation |
| Clipboard (OSC 52) | No | Not used by wa TUI |
| Sixel/Kitty graphics | No | Not used by wa TUI |

### 3.2  Known Multiplexer Interactions

| Multiplexer | Concern | Impact | Mitigation |
|-------------|---------|--------|------------|
| tmux | Modifies $TERM to `screen-256color` | May affect color detection | Set `set -g default-terminal "tmux-256color"` |
| tmux | Captures alternate screen | TUI renders inside tmux pane | Expected behavior — no issue |
| screen | Limited truecolor support | Color degradation | Use 256-color palette |
| zellij | Own TUI rendering layer | Potential key capture conflicts | Test Tab/BackTab routing |
| SSH | Adds latency to key events | Slower response | Increase tick interval in config |

### 3.3  Rollout Gate Criteria

An environment must achieve at least CONDITIONAL PASS to be included in a rollout:

1. All headless tests pass (320/320)
2. No S1 (crash) or S2 (terminal corruption) issues in PTY E2E
3. Terminal restored to usable state after all exit paths (normal, Ctrl+C, SIGTERM)
4. Known limitations documented with severity and mitigations

## 4  References

- `docs/ftui-compat-runbook-template.md` — per-environment runbook template (wa-3mus)
- `docs/ftui-pty-failure-artifacts.md` — PTY failure artifact schema (wa-308u)
- `docs/ftui-pty-fixture-strategy.md` — PTY fixture seed/timing (wa-1qr1)
- `docs/ftui-teardown-harness.md` — restoration invariants (wa-3fed)
- `evidence/ftui-08.4/stability-report.md` — chaos validation report (wa-1f4u)
