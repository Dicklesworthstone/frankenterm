# ft-xbnl0.5.5 Final Closure Bundle

Bead: `ft-xbnl0.5.5`

This is the finish-line closure reference for the `ft-xbnl0` program.
It is intentionally evidence-first: every status claim below points at the
canonical verification contract, retained artifact bundles, or current
operator-facing docs instead of relying on chat history.

## Final Status

Overall program posture:

- The finish-line evidence bundle now exists in one place and is sufficient for
  a future maintainer to audit the campaign without reopening the original plan.
- The release candidate is **not yet green**.
- The operator acceptance story is **passed with retained artifacts**.
- The permanent guard surface is **passed**.
- The blessed tuning package is **present and validated**.
- Release promotion remains **blocked** by two red gates from
  `docs/ft-xbnl0-4-6-release-gates.json`:
  - `REL-01-leak-oracle`
  - `REL-04-performance-budget`

Canonical contract anchor:

- Verification contract: `docs/ft-xbnl0-verification-contract.md`

Release-candidate checklist:

1. Shared verification contract exists and remains the canonical finish-line bar.
   Status: passed.
   Evidence: `docs/ft-xbnl0-verification-contract.md`
2. Permanent guard surface rejects runtime and fake-capability regressions.
   Status: passed.
   Evidence: `docs/ft-xbnl0-5-2-finish-line-guards-validation.json`
3. Blessed tuning profiles and operator playbooks exist for `10`, `50`, and
   `200+` pane fleets.
   Status: passed.
   Evidence: `docs/ft-xbnl0-5-3-completion-evidence.md`
4. Operator acceptance scenarios replay clean bootstrap, incident triage,
   recovery, and evidence cross-checks.
   Status: passed.
   Evidence:
   `/Users/jemanuel/projects/frankenterm/tests/e2e/artifacts/goal-line/ft-xbnl0.5.4/operator_acceptance/20260419T205125Z/summary.json`
5. First-run, health, doctor, and session-recovery pathways are surfaced as
   supported operator entrypoints.
   Status: passed.
   Evidence: `docs/ft-xbnl0-5-7-completion-evidence.md`
6. Leak-oracle proof exists at the gate-required artifact root.
   Status: blocked.
   Evidence:
   `/Users/jemanuel/projects/frankenterm/tests/e2e/artifacts/goal-line/ft-xbnl0.4.6/release_gates/20260419T192421Z/release_gate_repo_eval.json`
7. Performance budget remains within the hard release threshold.
   Status: blocked.
   Evidence:
   `/Users/jemanuel/projects/frankenterm/tests/e2e/artifacts/goal-line/ft-xbnl0.4.6/release_gates/20260419T192421Z/release_gate_repo_eval.json`

Current narrative/docs surfaces that should be treated as the live operator and
contributor references alongside this bundle:

- `README.md`
- `AGENTS.md`
- `docs/operator-playbook.md`
- `docs/tuning-reference.md`
- `docs/ft-xbnl0-5-4-operator-acceptance-scenarios.md`

## Remaining Risks

1. Release gate `REL-01-leak-oracle` is still red because the evaluator could
   not find a latest passing summary under
   `tests/e2e/artifacts/goal-line/ft-xbnl0.4.4/leak_oracle_regressions`.
   Source:
   `/Users/jemanuel/projects/frankenterm/tests/e2e/artifacts/goal-line/ft-xbnl0.4.6/release_gates/20260419T192421Z/release_gate_repo_eval.json`
2. Release gate `REL-04-performance-budget` is still red because the latest
   retained soak wrapper reports `max_duration_s=3.785115519` against a hard
   `max_duration_s=3.0` budget.
   Source:
   `/Users/jemanuel/projects/frankenterm/tests/e2e/artifacts/goal-line/ft-xbnl0.4.6/release_gates/20260419T192421Z/release_gate_repo_eval.json`
3. The exact user-requested `CC=/opt/homebrew/opt/llvm/bin/clang` `rch` recipe
   is not worker-portable on Linux hosts. The operator-acceptance and recent
   async-cutover verification both required a fallback worker-native `rch`
   command after the macOS-only compiler path failed before Rust compilation.
4. The live pane/session path still crosses the current WezTerm-backed mux
   boundary. The operator-acceptance bundle treats this as an evidence-backed
   supported story, not as proof of backend independence.
5. `ft doctor --json` can still classify a clean bootstrap workspace as blocked
   when backend compatibility metadata is unavailable. The current supported
   operator path uses `ft status --health -f json` as the canonical first-run
   classifier and records the doctor output honestly.

## Verification Summary

Contract and release bar:

- Canonical finish-line contract:
  `docs/ft-xbnl0-verification-contract.md`
- Release-gate policy:
  `docs/ft-xbnl0-4-6-release-gates.json`
- Latest retained gate harness summary:
  `/Users/jemanuel/projects/frankenterm/tests/e2e/artifacts/goal-line/ft-xbnl0.4.6/release_gates/20260419T192421Z/summary.json`
- Latest retained gate evaluator:
  `/Users/jemanuel/projects/frankenterm/tests/e2e/artifacts/goal-line/ft-xbnl0.4.6/release_gates/20260419T192421Z/release_gate_repo_eval.json`

Guard surfaces:

- Guard contract validation:
  `docs/ft-xbnl0-5-2-finish-line-guards-validation.json`
- Guard outcomes currently passed:
  - `scripts/check_no_runtime_regression.sh`
  - `scripts/validate_asupersync_cutover_runtime_guards.sh`
  - `ft_xbnl0_3_6_only_rust_sdk_target_is_finish_line_supported`

Tuning and large-fleet evidence:

- Aggregated tuning evidence:
  `docs/ft-xbnl0-5-3-completion-evidence.md`
- Blessed tuning contract report:
  `docs/ft-xbnl0-5-3-blessed-tuning-validation.json`
- Retained blessed tuning artifact root:
  `/Users/jemanuel/projects/frankenterm/tests/e2e/artifacts/goal-line/ft-xbnl0.5.3/blessed_tuning_profiles/20260419T193608Z`
- Retained soak wrapper consumed by both 5.3 and 4.6:
  `/Users/jemanuel/projects/frankenterm/tests/e2e/artifacts/goal-line/ft-xbnl0.4.5/swarm_soak_matrix/20260419T125312Z/summary.json`

Operator acceptance and recovery surfaces:

- Operator-acceptance playbook:
  `docs/ft-xbnl0-5-4-operator-acceptance-scenarios.md`
- Operator-acceptance contract check:
  `docs/ft-xbnl0-5-4-operator-acceptance-validation.json`
- Latest passing operator-acceptance summary:
  `/Users/jemanuel/projects/frankenterm/tests/e2e/artifacts/goal-line/ft-xbnl0.5.4/operator_acceptance/20260419T205125Z/summary.json`
- Latest passing operator-acceptance structured log:
  `/Users/jemanuel/projects/frankenterm/tests/e2e/artifacts/goal-line/ft-xbnl0.5.4/operator_acceptance/20260419T205125Z/structured.log`

First-run and diagnostic pathways:

- First-run and recovery evidence:
  `docs/ft-xbnl0-5-7-completion-evidence.md`
- Supported diagnostic commands named by the acceptance bundle:
  - `ft doctor --json`
  - `ft status --health -f json`
  - `ft session doctor -f json`
  - `ft watch --foreground`
  - `ft session list -f json`
  - `ft snapshot list -f json`

Representative retained remote verification commands already consumed by this
bundle:

- `rch exec -- env CARGO_TARGET_DIR=/tmp/ft-jemanuel-target cargo check -p frankenterm`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/ft-$(whoami)-target cargo test -p frankenterm-core --lib ft_xbnl0_4_6_release_gate_ -- --nocapture`
- `bash tests/e2e/test_ft_xbnl0_5_4_operator_acceptance.sh`
- `bash tests/e2e/test_ft_xbnl0_5_3_blessed_tuning_profiles.sh`
- `bash tests/e2e/test_ft_xbnl0_4_6_release_gates.sh`

## Mission Acceptance

Original mission question:

- Is FrankenTerm now evidence-backed enough to evaluate as a swarm-native
  terminal and control plane without reopening the old plan?
  Answer: yes.
- Is FrankenTerm ready to promote as a green release candidate?
  Answer: no, not yet.

Accepted today:

- The project has a canonical finish-line verification contract.
- The project has permanent guard validation proving core finish-line invariants
  are still defended.
- The project has retained operator evidence for bootstrap, broken-environment
  diagnosis, incident triage, recovery, and cross-checking against blessed
  tuning plus release-gate policy.
- The project has explicit operator and maintainer entrypoints for diagnostics,
  first-run setup, and recovery.

Not accepted today:

- A fully green release decision.
- A claim that leak-oracle proof is complete.
- A claim that the retained soak evidence satisfies the hard runtime duration
  budget.
- A claim that the current operator story is backend-independent.

Release promotion rule from this bundle:

Do not mark the `ft-xbnl0` finish-line program as release-ready until both of
these are true in retained artifacts:

1. `REL-01-leak-oracle` passes with a latest summary bundle present under
   `tests/e2e/artifacts/goal-line/ft-xbnl0.4.4/leak_oracle_regressions`.
2. `REL-04-performance-budget` passes with the latest retained soak wrapper at
   or below the `3.0` second duration budget while preserving the required
   `Black` backpressure tier and the required pane-scale coverage.
