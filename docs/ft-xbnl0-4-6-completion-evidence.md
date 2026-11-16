# ft-xbnl0.4.6 Completion Evidence

Bead: `ft-xbnl0.4.6`

Claim:
- Finish-line release-readiness now has an executable gate contract instead of ad hoc release notes.
- Leak behavior, permanent guard health, soak confidence, and runtime performance budgets are named gates with machine-readable diagnostics.

Primary artifacts:
- `/Users/jemanuel/projects/frankenterm/tests/e2e/artifacts/goal-line/ft-xbnl0.4.6/release_gates/20260419T192421Z`
- `/Users/jemanuel/projects/frankenterm/tests/e2e/artifacts/goal-line/ft-xbnl0.4.6/release_gates/20260419T192421Z/summary.json`
- `/Users/jemanuel/projects/frankenterm/tests/e2e/artifacts/goal-line/ft-xbnl0.4.6/release_gates/20260419T192421Z/structured.log`
- `/Users/jemanuel/projects/frankenterm/tests/e2e/artifacts/goal-line/ft-xbnl0.4.6/release_gates/20260419T192421Z/release_gate_repo_eval.json`
- `/Users/jemanuel/projects/frankenterm/tests/e2e/artifacts/goal-line/ft-xbnl0.4.6/release_gates/20260419T192421Z/frankenterm_core_release_gate_tests.log.rch_meta.json`

Policy and code surfaces:
- `docs/ft-xbnl0-4-6-release-gates.json`
- `scripts/check_ft_xbnl0_4_6_release_gates.sh`
- `tests/e2e/test_ft_xbnl0_4_6_release_gates.sh`
- `crates/frankenterm-core/src/release_readiness_gates.rs`
- `crates/frankenterm-core/tests/ft_xbnl0_4_6_release_gates_contract.rs`

Exact commands:
- `cargo test -p frankenterm-core --test ft_xbnl0_4_6_release_gates_contract -- --nocapture`
- `bash scripts/check_ft_xbnl0_4_6_release_gates.sh --self-test --output /tmp/ft-xbnl0-4-6-self-test.json`
- `bash scripts/check_ft_xbnl0_4_6_release_gates.sh --output /tmp/ft-xbnl0-4-6-eval.json`
- `bash tests/e2e/test_ft_xbnl0_4_6_release_gates.sh`
- Harness remote lane:
  `rch exec -- env CARGO_TARGET_DIR=/tmp/ft-$(whoami)-target cargo test -p frankenterm-core --lib ft_xbnl0_4_6_release_gate_ -- --nocapture`

Observed results:
- Local contract test passed: `3 passed; 0 failed`.
- Harness summary status passed: the harness itself completed its source audit, rustfmt check, evaluator self-test, remote lib-test lane, and persisted the repo-eval artifact bundle.
- Repo evaluator status is intentionally `failed` today because the newly defined gates are doing their job:
  - `REL-01-leak-oracle` failed with `leak_gate_missing_summary` because no latest `ft-xbnl0.4.4` summary artifact was found under `tests/e2e/artifacts/goal-line/ft-xbnl0.4.4/leak_oracle_regressions`.
  - `REL-04-performance-budget` failed with `performance_budget_failed` because the latest `ft-xbnl0.4.5` soak wrapper reported `max_duration_s=3.785115519` against the new `max_duration_s=3.0` budget.
  - `REL-02-guard-surface` passed against `docs/ft-xbnl0-5-2-finish-line-guards-validation.json`.
  - `REL-03-soak-confidence` passed against `/Users/jemanuel/projects/frankenterm/tests/e2e/artifacts/goal-line/ft-xbnl0.4.5/swarm_soak_matrix/20260419T125312Z/summary.json`.
- Harness remote lib-test lane passed on worker `vmi1152480` with `remote_exit_code=0`.

Consumed upstream evidence:
- `ft-xbnl0.4.5` -> `/Users/jemanuel/projects/frankenterm/tests/e2e/artifacts/goal-line/ft-xbnl0.4.5/swarm_soak_matrix/20260419T125312Z/summary.json`
- `ft-xbnl0.5.2` -> `/Users/jemanuel/projects/frankenterm/docs/ft-xbnl0-5-2-finish-line-guards-validation.json`

Residual risks:
- The evaluator currently blocks release because the upstream leak-oracle summary is missing and the current soak duration exceeds the new hard budget.
- That is expected for this bead: `ft-xbnl0.4.6` defines and persists the gates plus their actionable diagnostics; it does not claim the release is green today.
