# ft-xbnl0.5.4 Operator Acceptance Scenarios

Bead: `ft-xbnl0.5.4`

This bundle defines the operator-facing acceptance story for FrankenTerm after
the finish-line implementation work in `ft-xbnl0.5.3`, `ft-xbnl0.5.6`, and
`ft-xbnl0.5.7`.

It is intentionally not a vague “looks ready” narrative. Each scenario names:

- the operator claim being tested
- the exact commands to replay
- explicit success and failure criteria
- the retained artifact or upstream evidence that proves the claim
- any remaining rough edge that still needs to be said out loud

## Scope

The acceptance story covers the minimum operator path that the finish-line
campaign promised:

1. clean workspace bootstrap
2. broken-environment diagnosis
3. incident triage through `ft doctor`, `ft status --health`, and `ft session doctor`
4. recovery entry and return to healthy session state
5. cross-checking the operator story against the blessed tuning and release-gate evidence

The live pane/session control path still crosses the current WezTerm-backed mux
boundary. This bundle therefore treats large-fleet and long-haul operation as
evidence-backed operator claims, not as a local fake success story.

## Replay Commands

Use these exact commands when replaying this bead:

```bash
bash scripts/check_ft_xbnl0_5_4_operator_acceptance.sh --output docs/ft-xbnl0-5-4-operator-acceptance-validation.json
bash tests/e2e/test_ft_xbnl0_5_4_operator_acceptance.sh
CC=/opt/homebrew/opt/llvm/bin/clang CXX=/opt/homebrew/opt/llvm/bin/clang++ CARGO_TARGET_DIR=/tmp/ft-cod2-target rch exec -- cargo check -p frankenterm
```

If the exact `clang` recipe fails on a Linux RCH worker before Rust compilation,
that is an infrastructure portability issue, not a reason to claim the operator
story was verified cleanly. Record it and also run the worker-native fallback
`rch exec -- env CARGO_TARGET_DIR=/tmp/ft-cod2-target cargo check -p frankenterm`
so the crate still typechecks remotely.

## Scenario Matrix

### OA-01 Clean Bootstrap

Claim:
- A clean workspace exposes first-run guidance before runtime state exists and
  the first watcher start can bootstrap the runtime directories.

Commands:
- `ft doctor --json`
- `ft status --health -f json`
- `ft watch --foreground`

Pass:
- `ft status --health -f json` classifies the workspace as `bootstrap_required`
- `ft doctor --json` stays machine-readable and preserves actionable next steps,
  even if the current backend-compatibility checks are still degraded
- first watch start creates `.ft/`, `.ft/logs`, and `.ft/ft.db`

Fail:
- `ft status --health` does not identify the first-run state
- `ft doctor --json` stops being machine-readable or loses its next-step guidance
- the watch bootstrap path does not materialize the runtime state

Evidence:
- deterministic harness `tests/e2e/test_ft_xbnl0_5_4_operator_acceptance.sh`

### OA-02 Broken Environment Diagnosis

Claim:
- A backend bridge failure becomes actionable doctor output instead of silent
  operator confusion.

Commands:
- `ft doctor --json`

Pass:
- doctor exits non-zero
- the WezTerm CLI check is an error
- operator guidance status is `blocked`
- the next-step list contains a remediation such as installing or restoring the
  active backend bridge

Fail:
- doctor succeeds despite a broken backend bridge
- the output hides the error behind a vague warning

Evidence:
- deterministic harness `tests/e2e/test_ft_xbnl0_5_4_operator_acceptance.sh`

### OA-03 Incident Triage Entry

Claim:
- When persisted state shows an unclean shutdown, the new diagnostic surfaces
  route operators toward the recovery path.

Commands:
- `ft doctor --json`
- `ft status --health -f json`
- `ft session doctor -f json`

Pass:
- all surfaces classify the incident as `recovery_required`
- next steps include `ft watch --foreground`, `ft session list -f json`, or
  `ft snapshot list -f json`

Fail:
- any surface hides the incident or omits a concrete recovery step

Evidence:
- deterministic harness `tests/e2e/test_ft_xbnl0_5_4_operator_acceptance.sh`
- implementation grounding in `docs/ft-xbnl0-5-7-completion-evidence.md`

### OA-04 Return To Steady State

Claim:
- Once the unclean session marker is cleared, session persistence returns to a
  healthy classification instead of remaining permanently degraded.

Commands:
- `ft session doctor -f json`
- `ft session list -f json`

Pass:
- `ft session doctor -f json` reports `healthy`
- the same session inventory remains readable after the repair

Fail:
- the recovery marker persists after repair
- the operator can no longer inspect the session inventory

Evidence:
- deterministic harness `tests/e2e/test_ft_xbnl0_5_4_operator_acceptance.sh`

### OA-05 Operator Story Cross-Checks

Claim:
- The acceptance story remains anchored to previously landed finish-line
  evidence instead of inventing new proof.

Commands:
- `bash scripts/check_ft_xbnl0_5_3_blessed_tuning_profiles.sh --output docs/ft-xbnl0-5-3-blessed-tuning-validation.json`
- `bash scripts/check_ft_xbnl0_4_6_release_gates.sh --output /tmp/ft-xbnl0-4-6-eval.json`

Pass:
- the blessed tuning contract still validates
- the release-gate evaluator still replays from repository state and retained
  artifact bundles

Fail:
- operator docs refer to stale or missing evidence
- the cross-check commands no longer work from repository state

Evidence:
- `docs/ft-xbnl0-5-3-blessed-tuning-playbook.md`
- `docs/ft-xbnl0-5-3-completion-evidence.md`
- `docs/ft-xbnl0-4-6-completion-evidence.md`
- `docs/operator-playbook.md`

## Remaining Gaps

- Long-haul live pane control is still bounded by the current WezTerm-backed mux
  bridge. This bundle does not pretend that path is fully backend-independent.
- The exact user-requested `CC=/opt/homebrew/.../clang` recipe is preserved as a
  recorded verification command, but Linux RCH workers may fail before Rust
  compilation because that macOS toolchain path does not exist there.
- `ft status --health` only reports a truly ready live steady-state when a
  watcher is actually running; the deterministic harness proves the pre-watch
  bootstrap path, the incident path, and the post-repair session health path,
  then cross-checks the live operator story against previously retained evidence.
- `ft doctor --json` may still classify a clean bootstrap workspace as blocked
  when vendored-backend compatibility metadata is unavailable. This bundle
  treats `ft status --health` as the canonical first-run classifier and records
  the doctor output verbatim instead of pretending that the backend seam is
  already invisible.
