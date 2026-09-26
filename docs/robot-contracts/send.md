# Robot Command Contract: `send`

**Beads:** `ft-xxfwy.36` (paste-mode contract).
**Status:** Shipped. `ft send` and `ft robot send` share the input path and
the policy gate; the robot receipt reports which input mode ran.

## Input modes

`ft send <pane> <text>` and `ft robot send <pane> <text>` deliver text in one
of two modes:

| Mode | Flag | What the pane sees |
|---|---|---|
| Bracketed paste (default) | none | The text as one paste event, then a separately typed Enter that submits it |
| Typed | `--no-paste` | The characters as if typed, ending in a newline |

The default submits in agent composers (Claude Code, Codex) and shell prompts
alike: a line editor inserts a pasted newline instead of accepting it, so the
trailing line ending is split off and typed as its own Enter. After a
multi-line paste the Enter waits 750 ms, because Claude Code 2.1 folds an
Enter that arrives right behind one into the paste (measured against the real
agent).

`--no-newline` sends the text without the Enter in either mode.

## Receipt fields

Every `ft robot send` success or policy-refusal envelope carries the mode that
ran, so a calling agent never has to infer it:

| Field | Type | Meaning |
|---|---|---|
| `pane_id` | integer | Target pane |
| `injection` | object | Policy-gated injection result, tagged by `status`: `allowed`, `denied`, `requires_approval`, or `error` |
| `no_paste` | bool | `false` = bracketed paste, `true` = typed (`--no-paste`) |
| `no_newline` | bool | `true` when `--no-newline` suppressed the trailing newline |
| `wait_for` | object, optional | Present with `--wait-for`: pattern, `matched`, `elapsed_ms`, `polls` (the pattern is redacted like other echoed pane content) |
| `verification_error` | string, optional | Why post-send verification failed |
| `submit` | object, optional | Verified-submit receipt when requested |

The human `ft send` text output prints `Mode: no-paste` when typing was used.

## Tests

- `robot_send_receipt_tests::robot_send_receipt_reports_paste_mode`
  (`crates/frankenterm/src/main.rs`) pins the `no_paste` / `no_newline`
  receipt fields.
