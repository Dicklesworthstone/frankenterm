# ft-xbnl0.5.3 Completion Evidence

Bead: `ft-xbnl0.5.3`

Claim:
- FrankenTerm now has an operator-facing blessed tuning package for `10`, `50`, and `200+` pane fleets instead of leaving tuning advice scattered across backlog notes.
- The package includes a machine-readable contract, profile overlays, an operator playbook, an executable verifier, and a replayable E2E harness.

Primary surfaces:
- `docs/ft-xbnl0-5-3-blessed-tuning-playbook.md`
- `docs/ft-xbnl0-5-3-blessed-tuning-profiles.json`
- `fixtures/e2e/blessed_tuning_profiles/manifest.json`
- `fixtures/e2e/blessed_tuning_profiles/fleet_10.toml`
- `fixtures/e2e/blessed_tuning_profiles/fleet_50.toml`
- `fixtures/e2e/blessed_tuning_profiles/fleet_200_plus.toml`
- `scripts/check_ft_xbnl0_5_3_blessed_tuning_profiles.sh`
- `tests/e2e/test_ft_xbnl0_5_3_blessed_tuning_profiles.sh`
- `docs/tuning-reference.md`
- `docs/operator-playbook.md`
- `docs/ft-xbnl0-verification-contract.md`

Primary artifacts:
- `docs/ft-xbnl0-5-3-blessed-tuning-validation.json`
- `/Users/jemanuel/projects/frankenterm/tests/e2e/artifacts/goal-line/ft-xbnl0.5.3/blessed_tuning_profiles/20260419T193608Z`
- `/Users/jemanuel/projects/frankenterm/tests/e2e/artifacts/goal-line/ft-xbnl0.5.3/blessed_tuning_profiles/20260419T193608Z/profile_contract_report.json`

Exact commands:
- `bash scripts/check_ft_xbnl0_5_3_blessed_tuning_profiles.sh --output docs/ft-xbnl0-5-3-blessed-tuning-validation.json`
- `bash tests/e2e/test_ft_xbnl0_5_3_blessed_tuning_profiles.sh`
- `rch exec -- env CC=/opt/homebrew/opt/llvm/bin/clang CARGO_TARGET_DIR=/tmp/ft-$(whoami)-target cargo check -p frankenterm`

Observed results:
- The contract verifier passed and wrote `docs/ft-xbnl0-5-3-blessed-tuning-validation.json`.
- The verifier confirms the package still points at:
  - `docs/ft-xbnl0-4-6-release-gates.json`
  - `docs/ft-xbnl0-4-6-release-gates-validation.json`
  - `tests/e2e/artifacts/goal-line/ft-xbnl0.4.5/swarm_soak_matrix/20260419T125312Z/summary.json`
- The playbook explicitly distinguishes:
  - `fleet_10` as the low-latency small-fleet profile
  - `fleet_50` as the default blessed medium-fleet profile
  - `fleet_200_plus` as a controlled-operations large-fleet profile that is not yet a release-promotion claim while the 4.6 gates remain red
- `bash -n scripts/check_ft_xbnl0_5_3_blessed_tuning_profiles.sh` passed.
- `bash -n tests/e2e/test_ft_xbnl0_5_3_blessed_tuning_profiles.sh` passed.
- The user-requested `rch exec -- env CC=/opt/homebrew/opt/llvm/bin/clang ... cargo check -p frankenterm` failed on Linux worker `vmi1227854` before project compilation because `/opt/homebrew/opt/llvm/bin/clang` does not exist there and `openssl-sys` aborted during compiler detection.
- The retained harness artifact bundle under `/Users/jemanuel/projects/frankenterm/tests/e2e/artifacts/goal-line/ft-xbnl0.5.3/blessed_tuning_profiles/20260419T193608Z` captures the source audit, shell syntax pass, contract verification, and the remote `cargo check -p frankenterm` log. That remote check compiled through `frankenterm-core`, `frankenterm`, and `codec` on the worker before `rch` fell into the known silent-worker state instead of returning a summary.

Residual risks:
- The 5.3 package is intentionally evidence-backed but not a claim that release gates are green today; the playbook still defers to the failing 4.6 gate evaluation for release posture.
- Remote `rch` validation is still subject to the existing worker-portability and silent-worker issues; the retained artifact bundle records those conditions explicitly so later closure work can replay them without rediscovery.
