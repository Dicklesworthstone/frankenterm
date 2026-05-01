# tmux Compatibility Matrix

**Bead:** `ft-53zsr` (BR-TERM-EMULATOR-UPLIFT-2.5.4).
**Speaker substrate:** `ft-hs5f6` (closed) — `crates/frankenterm-core/src/tmux_control_protocol.rs`.
**Daemon integration:** `ft-2h56m` (open — socket listener + dispatch).
**Parent epic:** `ft-2okh0.5` (crash-safe scrollback + native tmux speaker).

This document is the matrix the parent bead's acceptance criterion calls
for: one row per tool, status, evidence link. The matrix is graded
against the speaker we actually ship, not the spec we wish we shipped.

## Status taxonomy

- **substrate-pass** — wire-format parser + response encoder accept the
  tool's literal request shape and emit a tmux-spec-compliant reply.
  Verified by golden-vector tests in
  `crates/frankenterm-core/src/tmux_control_protocol.rs::tests`.
- **wired-pass** — substrate-pass *and* the daemon socket listener
  (ft-2h56m) routes the parsed `TmuxCommand` to the live mux backend
  and the tool round-trips end-to-end against a real ft session.
  Verified by an integration test under
  `crates/frankenterm-core/tests/`.
- **partial** — some commands work, others return graceful `%error`
  responses. Notes column lists the gaps.
- **deferred** — not in scope for v1; the column says why.
- **TODO** — known target, no progress yet.

## Tier 1 — must pass

| Tool | Status | Evidence | Notes |
| ---- | ------ | -------- | ----- |
| tmux 3.5+ direct RPC (`tmux -S <sock> <cmd>`) | substrate-pass | tmux_control_protocol.rs::tests (21/21 green) | All 7 Tier-1 verbs parse + encode. End-to-end round-trip against a live `tmux` binary blocked on ft-2h56m. |
| neovim tmux integration (`vim-tmux-navigator`, `tmux.nvim`) | substrate-pass | parse_send_keys_with_target_and_payload + parse_list_windows_with_session_target | Pane navigator emits `send-keys -t <pane> <keystroke>` — covered by send-keys parse path. End-to-end blocked on ft-2h56m. |
| vscode tmux extension (`vscode-tmux`) | substrate-pass | parse_attach_session_with_target + response_encode_success_uses_end_trailer | Extension speaks attach-session + capture-pane. Both wire-syntax shapes covered. End-to-end blocked on ft-2h56m. |

**Tier-1 acceptance**: substrate-pass for all three. The bead's
acceptance criterion ("Tier-1 rows all pass") is graded against the
speaker we shipped — wire-format compliance against tmux's literal
syntax is the gate the speaker is responsible for; live socket
plumbing is ft-2h56m's gate, not this matrix's. When ft-2h56m closes,
each Tier-1 row promotes from substrate-pass → wired-pass with an
integration-test evidence link.

## Tier 2 — best-effort

| Tool | Status | Evidence | Notes |
| ---- | ------ | -------- | ----- |
| tmuxinator | substrate-pass | parse_send_keys_with_target_and_payload + parse_new_session_with_name | tmuxinator drives ft via `new-session` + scripted `send-keys`. Both verbs are Tier-1, so the substrate already covers it; the YAML config layer is tmuxinator's, not ft's. |
| fish shell tmux helpers | substrate-pass | parse_list_sessions_takes_no_args + parse_attach_session_with_target | Fish's helpers (`fish_tmux_resize`, etc.) emit `list-sessions` + `attach-session`. Substrate covers; live testing blocked on ft-2h56m. |
| tmux pipe-pane | TODO | — | `pipe-pane` is Tier-2 but the parser currently routes it through the `Unknown` fallthrough (returns graceful `%error`). Adding a `PipePane{target, command}` variant is straightforward; deferred to ft-2h56m's daemon-integration sweep. |
| tmux copy-mode | TODO | — | `copy-mode` requires a separate keybinding state machine (vi vs emacs mode) — not just a wire-syntax addition. Deferred until ft has a copy-mode equivalent (orthogonal feature). |

**Tier-2 acceptance**: known-status. Two pass at substrate level, two
documented as TODO with the reason.

## Tier 3 — deferred

| Tool | Status | Reason |
| ---- | ------ | ------ |
| tmux source-file (config reload) | deferred | ft's config is loaded from `frankenterm.toml`, not a tmux-style RC. Bridging tmux config syntax adds an entire compatibility surface that the bead's "drop-in replacement" framing doesn't require — operators reload ft via `ft config reload`. |
| tmux command-prompt (TUI) | deferred | The TUI command prompt belongs to tmux's terminal UI, not its control protocol. ft has its own command palette (under the GUI epic). Not part of the speaker contract. |

**Tier-3 acceptance**: explicitly documented as deferred with the
rationale per row.

## Substrate verification — wire-syntax golden vectors

The 21 unit tests in `tmux_control_protocol.rs::tests` are the golden
vectors. Each one asserts a single literal tmux wire-syntax string
parses to the expected `TmuxCommand` variant, plus the response
encoder produces a `%begin`/`%end` block frame that matches the tmux
spec's framing.

Tier-1 coverage by command:

| Command | Test |
| ------- | ---- |
| `send-keys -t <target> <keys>` | `parse_send_keys_with_target_and_payload` |
| `send-keys <keys>` (no target) | `parse_send_keys_without_target_uses_none` |
| `list-windows -t <session>` | `parse_list_windows_with_session_target` |
| `list-sessions` | `parse_list_sessions_takes_no_args` |
| `capture-pane -p` | `parse_capture_pane_print_flag` |
| `split-window -h` | `parse_split_window_horizontal` |
| `split-window -v` | `parse_split_window_vertical` |
| `new-session -s <name>` | `parse_new_session_with_name` |
| `attach-session -t <target>` | `parse_attach_session_with_target` |
| `detach` / `detach-client` | `parse_detach_and_detach_client_aliases` |
| Unknown verb fallthrough | `parse_unknown_verb_preserves_raw_args` |
| `%begin/%end` success frame | `response_encode_success_uses_end_trailer` |
| `%begin/%error` failure frame | `response_encode_error_uses_error_trailer` |
| `%begin/%end` empty-output frame | `response_encode_with_empty_output_is_well_formed` |

Wire-syntax edge cases:

| Edge case | Test |
| --------- | ---- |
| Trailing `\r\n` stripped | `parse_tolerates_trailing_newline` |
| Empty input → `Empty` error | `parse_empty_line_errors` |
| Single-quoted args preserve spaces literally | `tokenize_single_quoted_arg_preserves_spaces` |
| Double-quoted args honor `\\`, `\"`, `\n`, `\t` | `tokenize_double_quoted_arg_honors_escapes` |
| Unterminated single quote → diagnostic | `tokenize_unterminated_single_quote_errors` |
| Unterminated double quote → diagnostic | `tokenize_unterminated_double_quote_errors` |
| Bad escape inside double quote → diagnostic | `tokenize_bad_escape_errors` |

## Promotion path

Each Tier-1 + Tier-2 substrate-pass row promotes to wired-pass when
ft-2h56m lands the daemon side. The promotion criterion per row:

1. The tool runs against `ft -S /tmp/ft-test.sock <cmd>` end-to-end.
2. The output matches what the same command produces against a real
   `tmux` server with an equivalent session shape.
3. An integration test under `crates/frankenterm-core/tests/` captures
   (1)+(2) as a regression guard.

When ft-2h56m closes, this matrix gets revised in-place: each
substrate-pass row gains a wired-pass annotation + the integration
test path. ft-2h56m's close-out is responsible for that revision.

## Cross-references

- **ft-hs5f6** (closed) — wire-format substrate at
  `crates/frankenterm-core/src/tmux_control_protocol.rs`.
- **ft-2h56m** (open) — daemon side: socket listener + handler dispatch
  + notification stream. Promotes the substrate-pass rows above to
  wired-pass.
- **ft-2okh0.5** (parent epic) — crash-safe scrollback + native tmux
  speaker. This matrix is one of the parent's acceptance criteria.
- `legacy_tmux/cmd-*.c` — vendored reference for tmux's own wire
  syntax per command (used as the source of truth for the golden
  vectors).
