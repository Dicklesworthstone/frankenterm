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
| `DOCS_PASS` | The row is documentation/process-only and its static docs proof passed. |
| `FIXTURED` | Committed fixtures exist, but no no-mock or RCH-backed proof consumes them yet. |
| `NO_MOCK_PASS` | A no-mock harness consumes the row and passes through RCH where heavy proof is required. |
| `PARSER_ONLY_PASS` | The behavior is parser-only and a deterministic unit or integration test is the correct proof. |
| `METADATA_PASS` | Metadata-only fixture validation is the proof; the case is not a passing terminal behavior scenario yet. |
| `BUDGET_PASS` | The row has a structured, RCH-backed performance or resource-budget lane with retained metric artifacts. |
| `QUARANTINED` | A known case is kept out of the main gate with explicit reason and follow-up Bead. |
| `BLOCKED` | The row cannot be proven because of an infrastructure or architecture blocker named in the row. |

Rows must not use vague statuses such as "mostly covered" or "probably
works". Deferred rows must name a concrete Bead ID.

## Initial Coverage Matrix

| Row | Family | Required artifact | Proof lane | Owner | Status |
|---|---|---|---|---|---|
| 1 | Contract and coverage matrix | This document | `git diff --check -- docs/terminal-conformance-contract.md` | `ft-hme39.1` | `DOCS_PASS` |
| 2 | No-mock PTY/mux loopback smoke | Harness under `tests/e2e/` plus run artifacts | RCH-backed smoke with real spawn, send, resize, read, and pane/session assertions | `ft-hme39.2` | `CONTRACTED` |
| 3 | Mux attach/detach and pane lifecycle | Loopback scenario fixture or harness row | RCH-backed no-mock harness if Cargo/e2e is required | `ft-hme39.2` | `CONTRACTED` |
| 4 | Resize plus wrapped text and reflow | `tests/fixtures/terminal-conformance/manifest.json` scenario `tc-resize-wrap-001` | `frankenterm-escape-parser` parser corpus test; no no-mock reflow claim yet | `ft-hme39.3` | `PARSER_ONLY_PASS` |
| 5 | Bracketed paste and focus events | `tests/fixtures/terminal-conformance/manifest.json` scenario `tc-bracketed-paste-focus-001` | `frankenterm-escape-parser` parser corpus test with scenario ID | `ft-hme39.3` | `PARSER_ONLY_PASS` |
| 6 | OSC 8 hyperlink | `tests/fixtures/terminal-conformance/manifest.json` scenario `tc-osc8-hyperlink-001` | `frankenterm-escape-parser` parser corpus test with scenario ID | `ft-hme39.3` | `PARSER_ONLY_PASS` |
| 7 | CSI cursor shape and terminal modes | `tests/fixtures/terminal-conformance/manifest.json` scenario `tc-cursor-mode-001` | `frankenterm-escape-parser` parser corpus test with scenario ID | `ft-hme39.3` | `PARSER_ONLY_PASS` |
| 8 | Alternate screen enter/exit | `tests/fixtures/terminal-conformance/manifest.json` scenario `tc-alt-screen-001` | `frankenterm-escape-parser` parser corpus test; no no-mock screen-state claim yet | `ft-hme39.3` | `PARSER_ONLY_PASS` |
| 9 | UTF-8 and grapheme boundaries | `tests/fixtures/terminal-conformance/manifest.json` scenario `tc-utf8-grapheme-001` | `frankenterm-escape-parser` parser corpus test; no cell-width/render claim yet | `ft-hme39.3` | `PARSER_ONLY_PASS` |
| 10 | Graphics negative fixture | `tests/fixtures/terminal-conformance/manifest.json` scenario `tc-graphics-negative-001` | `frankenterm-escape-parser` parser corpus test; no visual image claim | `ft-hme39.3` | `PARSER_ONLY_PASS` |
| 11 | Failing transcript minimization | `tests/fixtures/terminal-conformance/minimized/tc-minimized-synthetic-failure-001.json` plus quarantine policy | `frankenterm-escape-parser` corpus metadata validator; no live failure claim yet | `ft-hme39.4` | `METADATA_PASS` |
| 12 | Large transcript performance budget | `LargeSwarmScenario::required_scale_points()` plus `tests/e2e/test_ft_hme39_5_large_transcript_budget.sh` | RCH-backed `large_transcript_budget::terminal_conformance_large_transcript_budget` run with retained JSONL metrics | `ft-hme39.5` | `BUDGET_PASS` |
| 13 | Closeout and proof-ledger usage | Docs tying artifacts to Beads and proof ledger | Static docs check; later proof-ledger validator when available | `ft-hme39.6` | `DOCS_PASS` |

## Large Transcript Budget

`ft-hme39.5` uses the deterministic large-swarm replay generator as the
terminal-conformance large-transcript scale fixture. The scale factors are the
required `LargeSwarmScenario` points: 10, 50, 200, and 1 000 panes. Each point
generates stable recorder events for pane output bursts, compaction waves,
Robot Mode search traffic, and workflow mission actions, then replays them
through `ReplaySession` and telemetry collectors.

The budget lane is intentionally optional and operator-gated rather than part of
the default CI matrix. It is still reproducible and fail-closed:

```bash
RCH_REQUIRE_REMOTE=1 tests/e2e/test_ft_hme39_5_large_transcript_budget.sh
```

The harness writes artifacts under
`tests/e2e/logs/terminal-conformance/ft-hme39.5/<run_id>/`:

- `events.jsonl` for suite/preflight/proof step state;
- `large_transcript_budget.log` plus `.rch_meta.json` for the RCH Cargo proof;
- `budget_events.jsonl` containing one JSON metric row per scale point plus a
  summary row;
- `summary.json` with artifact paths, worker id, and max observed resource
  metrics.

The Rust budget test enforces linear, deliberately coarse thresholds so the lane
catches accidental quadratic behavior and unbounded growth without turning
ordinary shared-worker noise into a false source failure:

| Metric | Threshold |
|---|---:|
| `event_count` | Exact `LargeSwarmRegressionThresholds::for_scenario` count |
| `duration_ms` | Exact generated source-duration threshold |
| `output_bytes` | Linear per-pane/per-burst output threshold |
| `max_events_per_pane` | Linear per-pane event threshold |
| `wall_time_ms` | `max(1000, event_count * 10)` |
| `artifact_bytes` | `max(64 KiB, event_count * 4096)` |
| `memory_proxy_bytes` | `max(128 KiB, event_count * 8192)` |

`memory_proxy_bytes` is not an RSS claim. It is a deterministic in-process
proxy derived from the serialized corpus/summary size plus event and pane
counts, suitable for catching resource-shape regressions in this conformance
lane. Real 64-core / 256 GiB release claims remain governed by the high-scale
proof gauntlet and must not be promoted from this synthetic replay budget alone.

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

## Failing Transcript Minimization

Large terminal failures should be reduced before they are committed. The goal is
to keep the smallest transcript that still reproduces the same assertion, while
preserving enough provenance for another agent to understand the failure without
chat history.

Manual minimization procedure:

1. Capture or identify the original failing artifact path. Prefer an RCH run
   artifact under `tests/e2e/logs/terminal-conformance/<bead>/<run_id>/` when the
   failure came from an executable harness.
2. Copy the candidate bytes into a scratch location, then remove unrelated
   prompt text, command echo, idle output, and duplicate protocol sequences one
   reduction at a time.
3. After each reduction, rerun the narrow assertion that produced the original
   failure. The failure signature must remain the same, not merely fail
   somewhere nearby.
4. Stop when the next obvious deletion changes the signature or hides the
   failing assertion.
5. Commit only the minimized input and metadata. Do not commit a large raw
   failure log unless a reviewer explicitly asks for it.

Each minimized case must be listed in `manifest.json` under `minimized_cases`
and must provide:

- `scenario_id`;
- `original_artifact_path`;
- `minimized_input_artifact`;
- `expected_failure.assertion`;
- `expected_failure.failure_signature`;
- `expected_failure.preserved_by_minimized_input`;
- ordered `minimization_steps` with action and evidence;
- `quarantine.reason`, `quarantine.follow_up_bead`, and
  `quarantine.promotion_condition`;
- `promotion.target_manifest`, `promotion.required_proof`, and
  `promotion.criteria`;
- `residual_risk`;
- `redaction_status`.

Quarantine is allowed only when the case is intentionally kept out of the main
passing corpus because it is flaky, environment-sensitive, synthetic, or blocked
by a named product or harness defect. A quarantined case must name the reason,
follow-up Bead, promotion condition, and residual risk. Silent skips are not
allowed: a transcript that cannot name these fields must not be committed.

Promotion into the main corpus requires replacing synthetic or generated-only
provenance with a real artifact when applicable, adding a passing expected
artifact under `expected/`, adding the scenario to `manifest.scenarios`, and
running the exact RCH proof command that consumes the fixture. Metadata
validation alone proves the quarantine contract, not terminal correctness.

## Closeout Template

Terminal conformance closeout comments are the handoff surface until the
repository-wide proof ledger can ingest every terminal harness directly. When a
`ProofAttemptRecord` or validated proof-ledger JSONL row exists, cite it before
the prose summary. When it does not exist yet, cite the retained run artifact
paths that contain the same facts:

- `summary.json` for final outcome, artifact index, worker id, and metrics;
- the RCH log plus `.rch_meta.json` for selected worker, remote exit status,
  timeout state, and fail-open detection;
- harness-specific metric JSONL such as `budget_events.jsonl`;
- fixture metadata paths for parser-only or minimized-transcript work;
- the exact Bead id and matrix row ids covered by the claim.

Use the proof taxonomy in
`docs/proposals/ft-tn6cw-proof-lane-evidence-taxonomy.md` when classifying a
terminal proof attempt. The important states for this lane are:

| State | Terminal conformance meaning |
|---|---|
| `PASS` | The intended terminal assertion ran on the intended backend, exited 0, and retained the artifacts needed for the claimed rows. |
| `INFRA_BLOCKED_PRE_CARGO` | RCH queueing, worker selection, mirror preflight, sync, command classification, or remote launch blocked before Cargo or the harness assertion started. This is not a source verdict. |
| `INFRA_BLOCKED_POST_CARGO` | Remote Cargo or the harness started, but worker environment, artifact retrieval, timeout, or wrapper behavior prevented complete evidence. Record what was reached, but do not close as green. |
| `SOURCE_COMPILE_FAIL` | Remote Cargo/rustc reached first-party code and reported source, feature, lint, or build-script errors. Fix the source or leave the bead red. |
| `TEST_FAIL` | The terminal assertion ran and failed. Fix behavior or record a quarantined minimized follow-up. |
| `LOCAL_INVALID` | A local Cargo run or RCH fail-open is being offered for an RCH-required lane. It cannot close a terminal conformance bead that requires remote evidence. |

Do not treat RCH setup, sync, worker-selection chatter, cache downloads, or a
source mirror preflight as proof that a terminal assertion passed. Those logs
can prove infrastructure state. The terminal claim starts only when the relevant
static check, parser test, no-mock harness, or RCH-backed budget assertion runs.

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

### Passing RCH-Backed Closeout Example

Use this shape when the RCH-backed harness reached the terminal assertion and
returned success:

```text
Closed by fa96ec86d.
Rows covered: 12.
Artifacts:
- tests/e2e/test_ft_hme39_5_large_transcript_budget.sh
- tests/e2e/logs/terminal-conformance/ft-hme39.5/20260512T231714Z/summary.json
- tests/e2e/logs/terminal-conformance/ft-hme39.5/20260512T231714Z/budget_events.jsonl
- tests/e2e/logs/terminal-conformance/ft-hme39.5/20260512T231714Z/large_transcript_budget.log.rch_meta.json
Proof:
- bash -n tests/e2e/lib_rch_guards.sh tests/e2e/test_ft_hme39_5_large_transcript_budget.sh
- shellcheck -x tests/e2e/lib_rch_guards.sh tests/e2e/test_ft_hme39_5_large_transcript_budget.sh
- git diff --check -- tests/e2e/lib_rch_guards.sh docs/asupersync-rch-execution-policy.md
- RCH_STEP_TIMEOUT_SECS=3600 RCH_REQUIRE_REMOTE=1 tests/e2e/test_ft_hme39_5_large_transcript_budget.sh
Remote proof status: PASS, worker vmi1156319, remote_exit_code=0, fail_open_detected=false, timed_out=false.
Metrics: failed_count=0; 1000-pane point produced 7080 events in 64ms, artifact_bytes=3928117, memory_proxy_bytes=6252597.
Residual risk: stale worker mirrors were retained in rch_mirror_preflight.json as residual pool evidence; RCH refreshed the selected worker before Cargo.
```

This example may be copied only when the cited artifact actually exists and the
numbers match the retained run. A future run should cite its own run id and
worker id.

### RCH Infrastructure Blocked Example

Use this shape when the harness fails before terminal assertions start:

```text
Not closed.
Rows intended: 12.
Artifacts:
- tests/e2e/logs/terminal-conformance/ft-hme39.5/20260512T222300Z/summary.json
- tests/e2e/logs/terminal-conformance/ft-hme39.5/20260512T222300Z/ft_hme39_5_large_transcript_budget_20260512T222300Z.rch_worker_selection.json
Proof:
- RCH_STEP_TIMEOUT_SECS=3600 RCH_REQUIRE_REMOTE=1 tests/e2e/test_ft_hme39_5_large_transcript_budget.sh
Remote proof status: INFRA_BLOCKED_PRE_CARGO, final_failure.reason_code=rch_infrastructure_worker_selection_blocked.
Source verdict: none. large_transcript_budget.log was empty, so Cargo and the test binary did not start locally or remotely.
Next action: fix or wait out the RCH worker-selection blocker, then rerun the same harness.
```

This is a valid Beads progress comment, not closure evidence.

### Minimized Transcript Follow-Up Example

Use this shape when a failing terminal transcript has been reduced but not yet
promoted into the passing corpus:

```text
Not closed as terminal correctness proof.
Rows affected: 11 and the eventual promoted behavior row.
Artifacts:
- tests/fixtures/terminal-conformance/minimized/tc-minimized-synthetic-failure-001.json
- tests/fixtures/terminal-conformance/manifest.json
Proof:
- jq empty tests/fixtures/terminal-conformance/manifest.json tests/fixtures/terminal-conformance/minimized/tc-minimized-synthetic-failure-001.json
- git diff --check -- tests/fixtures/terminal-conformance/manifest.json tests/fixtures/terminal-conformance/minimized/tc-minimized-synthetic-failure-001.json
Proof status: METADATA_PASS for quarantine/provenance only.
Residual risk: terminal behavior remains unproven until the minimized input has a passing expected artifact, appears in manifest.scenarios, and the fixture consumer proof command passes through RCH when heavy proof is required.
Follow-up bead: <bead id that will fix/promote the case>.
```

For this contract bead, docs-only closure is sufficient because it creates the
matrix and delegates executable proof to the child Beads above.
