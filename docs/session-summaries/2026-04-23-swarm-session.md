# 2026-04-23 Swarm session — RusticMeadow summary

One agent's slice of a multi-pane session. 116 commits across all panes;
**59 commits** carry RusticMeadow co-authorship. Convergence declared after
5 consecutive reviewer rounds landed no substantive defects (ft-fy44g).

## Top P0/P1 finds + fixes landed

1. **ft-ljgyr [P1 compile blocker]** — `WaSendTool::with_wezterm_handle_and_shared_rate_limiter`
   was `#[cfg(test)]` but called from non-test constructors (a slip in pane 2's
   ft-eu0no `SharedRateLimiter` refactor). E0599 was blocking every non-test
   build; un-gated the constructor in `810ad4c9`. Swarm unblocked.

2. **ft-x86z2 [P1 security]** — 8 mutating MCP tools (`wa.tx_run`/`rollback`,
   `wa.mission_abort`/`pause`/`resume`, `wa.events_annotate`/`triage`/`label`)
   were skipping `PolicyEngine::authorize` while their peer mutators gated.
   Shipped a shared `mcp_authorize_mcp_mutation` gate in `a4b7c8c3` covering
   5 of 8 (tx + mission); the 3 events tools needed a constructor-signature
   refactor and got filed as ft-vue5t.

3. **ft-yj375 [P1 redaction leak]** — `wa.state` MCP tool + `ft robot state`
   CLI both returned pane `title` / `cwd` **unredacted**, while the peer web
   `/panes` handler did redact them. Pane titles can carry agent-supplied
   credentials (e.g. `codex --api-key sk-...`). Fixed at the serving handler
   (commit from pane 3); matrix at `docs/security/read-path-redaction-matrix.md`
   flipped to ✓.

4. **ft-h90rh → ft-rsqap → ft-mw1zb [security/persistence]** — built a
   three-layer staircase so every policy-denied MCP mutation now persists to
   a dedicated SQLite audit stream. Storage infra + schema v24 migration +
   `PolicyDeniedAuditRecord` in `4595cc0e`, 5-deny-paths wiring via gate
   helper in `c3d273f0`, 6-more direct sites wired in `97016fe6`. 11 of 12
   deny paths now persist (wa.send deliberately skipped because
   `PolicyGatedInjector` already audits to `audit_actions`).

5. **ft-kbbc3 [P1 tx integrity] (filed, not fixed)** — `wa.tx_run` builds
   synthetic prepare-gate inputs (`tx_prepare_gate_inputs_allow_all`) and
   always-success commit/compensation inputs, then **persists the fabricated
   `Committed` state to the tx contract file on disk**. `PaneStepExecutor` /
   `TxExecutionEngine` exist but aren't wired to the MCP handler. Substantial
   refactor; filed with two interim-mitigation options.

## Open P1 beads still in flight

- **ft-kbbc3** — tx synthetic-success persistence (this pane's find; pane 1
  taking).
- **ft-eu0no.1** — pane 2's shared PolicyEngine rate-limiter regression
  test; the infra landed in `f540a5d5` but the regression test is still
  pending.

## Strategic follow-ups surfaced

- **Architectural centralization (ft-bkgbq)** — `crates/frankenterm/src/main.rs`
  rebuilds PolicyEngine + PolicyGatedInjector + WorkflowRunner at 4 sites with
  slightly different defaults (`:19850` vs `:25526` differ on
  `command_gate_config` + `policy_rules` — concrete drift). Filed with a
  `HumanSendStack` / `McpWorkflowStack` / `RobotWorkflowStack` factory-bundle
  shape. Outside this session's reservation.
- **ft-xkpy1 (UX follow-ups)** — primary MCP_ERR_POLICY hint fix shipped;
  4 secondary items remain: README `robot.*` error-code drift, robot-mode
  hint uniformity, `--format toon` token-savings claim measurement, MCP
  envelope conciseness (6 top-level keys often 3-4× the data payload size).
- **ft-87tdq MR2-MR6 (metamorphic patterns)** — MR1 (idempotence) shipped as
  2 proptest cases. Construction determinism / empty-input neutrality /
  benign-suffix monotonicity / rule reordering / unicode wrap invariance
  enumerated in the bead body for incremental follow-up.
- **ft-vue5t** — thread `Arc<Config>` into wa.events_annotate/triage/label
  constructors so they can close the last 3 of the 8 mutating-tool policy
  gates from ft-x86z2.
- **ft-mw1zb residual** — `wa.send`'s deny path routes through
  `PolicyGatedInjector` which audits to the different `audit_actions` table;
  unified-deny-stream refactor is its own architectural bead (not filed this
  session, mentioned in close reason).

## Test coverage gaps identified

- `mw1zb` 12 policy-deny paths: 11 now persist, 1 skipped by design — covered.
- `ft-87tdq`: PatternEngine metamorphic coverage started at MR1; 5 MRs open.
- `ft-kbbc3`: `wa.tx plan/run/rollback` e2e test (ft-73f9r, landed earlier
  this session) validates synthetic-success as CORRECT, so the tx integrity
  gap doesn't fail any test — needs an integration test that plants a
  policy-failing step and asserts `wa.tx_run` does NOT report Committed.
- `docs/security/read-path-redaction-matrix.md` + the new
  `docs/security/policy-denial-audit-wiring-matrix.md` serve as living
  regression benchmarks; every new pane-text read path or policy-deny path
  should add a row.

## Discipline notes for next session

- Reservation via Agent Mail was unreliable (DB corruption errors) — fall
  back to ride-along-with-clear-commit-flagging worked cleanly when flagged
  explicitly in commit bodies (`810ad4c9`, `c3d273f0`, `97016fe6` all carried
  pane 2's in-flight work with attribution preserved in the commit message).
- Natural serialization beat racing: panes committed in readiness order, no
  rebase explosions despite the contention matrix in `ft-3wow6`.
- "Verification partial" commits were common (`f540a5d5`, `cec75f71`,
  `03eb5831`, `8d1e93dc`): rch/cargo infra flakiness made full-build
  validation unreliable. Not a coordination problem but a real infra quality
  one; flagged in ft-3wow6 close.

Agent: RusticMeadow
