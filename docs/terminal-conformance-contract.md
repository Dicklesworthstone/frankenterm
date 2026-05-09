# Terminal Conformance Contract

**Bead:** `ft-hme39.1`
**Parent:** `ft-hme39`
**Status:** planning contract

This document defines the first artifact contract for the terminal protocol
differential and no-mock mux loopback lane. It exists so future terminal
correctness work has a single place to answer:

- which protocol families are in scope;
- what counts as no-mock proof;
- where fixtures and harness artifacts should live;
- which Bead owns each deferred artifact;
- which verification commands are proof and which are only setup chatter.

The contract is intentionally artifact-first. A terminal conformance row is not
covered because the code probably handles it; it is covered only when this
document points to a committed fixture, harness, matrix row, or RCH-backed proof
artifact.

## Scope

Terminal conformance covers the behavior at the boundary between a controlled
process, PTY, mux/session state, terminal parser state, transcript capture, and
operator/robot read surfaces. Unit tests are useful, but they are not enough for
this lane unless the row explicitly says the behavior is parser-only.

In scope:

- PTY and mux loopback behavior for spawn, send, resize, read, attach, detach,
  and pane lifecycle;
- CSI, OSC, DCS, input-mode, graphics, and UTF-8 transcript behavior;
- differential fixtures that compare expected terminal state, emitted text, or
  transcript metadata;
- minimization and quarantine rules for failing transcripts;
- performance budgets for replaying large transcript sets;
- closeout evidence that distinguishes remote RCH proof from local static
  checks and from RCH setup or sync output.

Out of scope:

- adding a second mux backend;
- bulk-copying upstream WezTerm or tmux test directories;
- local heavy Cargo proof;
- deleting or replacing existing fixtures;
- claiming production support for a protocol family before a row has artifacts.

## No-Mock Boundary

A no-mock terminal conformance proof should use the real code path wherever the
repository can do so safely:

1. Spawn or attach to a controlled process.
2. Route IO through the PTY/mux/session boundary.
3. Exercise pane read/write and resize behavior through the same surface a
   user-facing or robot-facing command would use.
4. Capture output and state into a committed fixture or structured run
   artifact.
5. Fail closed if RCH tries to run a heavy Cargo proof locally.

Mocks and stubs are allowed only for deterministic outer coordination, such as
fixed fixture input, log normalization, or replacing a remote service that is
not the subject of the terminal assertion. A row that uses a stub must say what
was stubbed and why that does not weaken the terminal boundary under test.

## Artifact Locations

The terminal conformance lane should keep artifacts predictable:

| Artifact kind | Preferred location | Notes |
|---|---|---|
| Contract and matrix docs | `docs/terminal-conformance-contract.md` | This document owns the first matrix. |
| Transcript fixtures | `tests/fixtures/terminal-conformance/` | Additive fixture family owned by `ft-hme39.3`. |
| No-mock harness scripts | `tests/e2e/` | Harnesses must use fail-closed RCH helpers for heavy proof. |
| Rust integration tests | `crates/frankenterm-core/tests/` or relevant vendored crate tests | Choose the narrowest crate that owns the behavior. |
| Run artifacts | `tests/e2e/logs/terminal-conformance/<bead>/<run_id>/` | Use summary plus raw step logs when adding executable harnesses. |
| Minimized failing cases | `tests/fixtures/terminal-conformance/minimized/` | Each case needs provenance and residual-risk metadata. |

## Status Taxonomy

| Status | Meaning |
|---|---|
| `CONTRACTED` | The row is specified here but implementation artifacts are deferred to a named Bead. |
| `FIXTURED` | Committed fixtures exist, but no no-mock or RCH-backed proof consumes them yet. |
| `NO_MOCK_PASS` | A no-mock harness consumes the row and passes through RCH where heavy proof is required. |
| `PARSER_ONLY_PASS` | The behavior is parser-only and a deterministic unit or integration test is the correct proof. |
| `QUARANTINED` | A known case is kept out of the main gate with explicit reason and follow-up Bead. |
| `BLOCKED` | The row cannot be proven because of an infrastructure or architecture blocker named in the row. |

Rows must not use vague statuses such as "mostly covered" or "probably
works". Deferred rows must name a concrete Bead ID.

## Initial Coverage Matrix

| Row | Family | Required artifact | Proof lane | Owner | Status |
|---|---|---|---|---|---|
| 1 | Contract and coverage matrix | This document | `git diff --check -- docs/terminal-conformance-contract.md` | `ft-hme39.1` | `CONTRACTED` |
| 2 | No-mock PTY/mux loopback smoke | Harness under `tests/e2e/` plus run artifacts | RCH-backed smoke with real spawn, send, resize, read, and pane/session assertions | `ft-hme39.2` | `CONTRACTED` |
| 3 | Mux attach/detach and pane lifecycle | Loopback scenario fixture or harness row | RCH-backed no-mock harness if Cargo/e2e is required | `ft-hme39.2` | `CONTRACTED` |
| 4 | Resize plus wrapped text and reflow | Transcript fixture plus expected state | Harness or parser/integration test named by fixture metadata | `ft-hme39.3` | `CONTRACTED` |
| 5 | Bracketed paste and focus events | Transcript fixture with expected mode/state output | Parser or no-mock proof depending on final row shape | `ft-hme39.3` | `CONTRACTED` |
| 6 | OSC 8 hyperlink | Transcript fixture and expected link metadata | Parser/integration proof with scenario ID | `ft-hme39.3` | `CONTRACTED` |
| 7 | CSI cursor shape and terminal modes | Transcript fixture and expected terminal state | Parser/integration proof with scenario ID | `ft-hme39.3` | `CONTRACTED` |
| 8 | Alternate screen enter/exit | Transcript fixture and expected scrollback/screen state | Parser or no-mock proof depending on final artifact | `ft-hme39.3` | `CONTRACTED` |
| 9 | UTF-8 and grapheme boundaries | Transcript fixture with expected cell/text output | Parser/integration proof with scenario ID | `ft-hme39.3` | `CONTRACTED` |
| 10 | Graphics negative fixture | Kitty/sixel/OSC graphics input with expected safe rejection or state | Parser/integration proof; no visual claim without image artifact | `ft-hme39.3` | `CONTRACTED` |
| 11 | Failing transcript minimization | Minimized fixture metadata plus quarantine policy | Static fixture validation and, when executable, RCH-backed repro | `ft-hme39.4` | `CONTRACTED` |
| 12 | Large transcript performance budget | Generator or committed scale fixture plus budget thresholds | RCH-backed performance run or operator-gated reproducible lane | `ft-hme39.5` | `CONTRACTED` |
| 13 | Closeout and proof-ledger usage | Docs tying artifacts to Beads and proof ledger | Static docs check; later proof-ledger validator when available | `ft-hme39.6` | `CONTRACTED` |

## Proof Rules

The minimum local proof for docs-only contract edits is:

```bash
git diff --check -- docs/terminal-conformance-contract.md
```

If a child edits shell harnesses, add:

```bash
bash -n <script>
shellcheck -x <script>
```

If a child runs Cargo, clippy, tests, benches, or an executable E2E harness that
invokes Cargo, the proof must run through RCH:

```bash
RCH_REQUIRE_REMOTE=1 rch exec -- env CARGO_TARGET_DIR=/tmp/<bead>-terminal-conformance cargo test -p <crate> <test> -- --nocapture
```

RCH setup, worker selection, sync, transfer, or cache output is not terminal
conformance proof by itself. Closeout comments must cite the command that
reached the terminal assertion and whether Cargo/rustc/test execution was
actually reached.

## Fixture Metadata

Each transcript fixture added under `ft-hme39.3` should carry enough metadata
for future agents to reason without chat context:

| Field | Required | Meaning |
|---|---:|---|
| `scenario_id` | yes | Stable identifier used in failure output. |
| `family` | yes | One of the matrix families above. |
| `source` | yes | Manual, reduced live failure, upstream-derived with SHA, or synthetic. |
| `input_artifact` | yes | Relative path to the transcript or generator. |
| `expected_artifact` | yes | Relative path to expected output/state. |
| `proof_command` | yes | Exact command expected to consume the fixture. |
| `no_mock_boundary` | yes | Which real boundary is exercised, or why parser-only is correct. |
| `redaction_status` | yes | Confirmation that the fixture contains no secrets or private host paths. |
| `follow_up_bead` | when deferred | Bead ID for incomplete proof or open artifact work. |

## Closeout Template

Terminal conformance closeout comments should include:

```text
Closed by <commit>.
Rows covered: <matrix row ids>.
Artifacts:
- <fixture or harness path>
- <run artifact path if any>
Proof:
- <exact static command>
- <exact RCH command if any>
Remote proof status: <not required for docs-only | worker id and result>
Residual risk: <none | named blocker and follow-up bead>
```

For this contract bead, docs-only closure is sufficient because it creates the
matrix and delegates executable proof to the child Beads above.
