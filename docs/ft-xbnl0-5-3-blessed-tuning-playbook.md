# Blessed Tuning Profiles and Operator Playbook (`ft-xbnl0.5.3`)

Date: 2026-04-19
Status: Published for operator use; not a release-promotion claim because `docs/ft-xbnl0-4-6-release-gates-validation.json` still shows blocking failures
Depends on: `ft-xbnl0.4.5`, `ft-xbnl0.4.6`, `ft-xbnl0.5.6`, `ft-xbnl0.1.4`

## Purpose

Give operators one place to answer three practical questions:

1. Which tuning profile should I start with for a 10, 50, or 200+ pane fleet?
2. Which exact commands prove the profile is applied and the system is still healthy?
3. When do I keep tuning, and when do I stop and escalate?

This document is the operator-facing companion to:

- `docs/tuning-reference.md` for knob-by-knob ranges
- `docs/operator-playbook.md` for incident triage and recovery
- `docs/ft-xbnl0-5-3-blessed-tuning-profiles.json` for the machine-readable contract
- `scripts/check_ft_xbnl0_5_3_blessed_tuning_profiles.sh` and `tests/e2e/test_ft_xbnl0_5_3_blessed_tuning_profiles.sh` for repeatable validation

## Evidence Base

The profiles below are derived from measured repo artifacts, not from ad hoc guesses.

| Input | Why it matters | Exact source |
| --- | --- | --- |
| Soak matrix exercised 1, 50, 100, and 200 pane classes and passed its wrapper checks | Confirms the project has at least one retained large-fleet artifact bundle across the target pane classes | `tests/e2e/artifacts/goal-line/ft-xbnl0.4.5/swarm_soak_matrix/20260419T125312Z/summary.json` |
| Release gates encode the hard finish-line thresholds | Prevents the playbook from inventing softer private thresholds | `docs/ft-xbnl0-4-6-release-gates.json` |
| Current gate evaluation still fails | Forces the guidance to stay honest: operators may use these profiles, but they do not imply release readiness | `docs/ft-xbnl0-4-6-release-gates-validation.json` |
| Verification contract requires exact commands and retained artifacts for operator-facing lanes | Sets the proof bar for this bead and for future replay | `docs/ft-xbnl0-verification-contract.md` |
| Doctor and status surfaces expose live health, leak, and backpressure signals | Gives operators a concrete monitoring loop after applying a profile | `docs/operator-playbook.md`, `docs/tuning-reference.md` |

## What the Evidence Says Right Now

- The soak wrapper for `ft-xbnl0.4.5` passed and recorded the target pane scales `1`, `50`, `100`, and `200`.
- The release-gate policy for `ft-xbnl0.4.6` requires:
  - `max_duration_s <= 3.0`
  - `max_peak_rss_mb <= 32.0`
  - `required_backpressure_tier = Black`
  - `required_pane_scales = [1, 50, 100, 200]`
- The current gate evaluation shows:
  - leak-oracle summary missing
  - performance budget failed because `max_duration_s = 3.785115519` exceeded the `3.0` second budget
  - peak RSS remained within budget at `16.607958793640137 MiB`

Operational consequence:

- `fleet_10` and `fleet_50` are blessed starting points for routine operator use.
- `fleet_200_plus` is blessed only as a controlled-operations profile.
- None of these profiles should be cited as proof that the final closure bundle can ship.

## Blessed Profiles

All profile overlays live under `fixtures/e2e/blessed_tuning_profiles/`.
To apply one into a workspace-local config:

```bash
ft config profile list --json --path ./ft.toml
ft config profile diff fleet_50 --path ./ft.toml
ft config profile apply fleet_50 --path ./ft.toml
ft config validate --path ./ft.toml
```

### `fleet_10`

File: `fixtures/e2e/blessed_tuning_profiles/fleet_10.toml`

Use this when:

- you are running a local swarm or single-operator session around 10 panes
- lower input-to-observation latency matters more than absorbing long burst windows
- you want earlier backpressure warning and tighter watchdog thresholds

Profile intent:

- short coalesce windows
- smaller batched writes
- early backpressure warning at `warn_ratio = 0.70`
- tighter lock and watchdog warnings so local regressions surface quickly

Promotion rule:

- safe for routine operator use
- if `ft status --health | jq '.health.ingest_lag_max_ms'` stays above `5000` for three checks, revert to default config and follow `docs/operator-playbook.md`

### `fleet_50`

File: `fixtures/e2e/blessed_tuning_profiles/fleet_50.toml`

Use this when:

- you need the default balanced fleet posture
- you want a profile aligned with the passing 50-pane soak class from `ft-xbnl0.4.5`
- you want moderate queue headroom without shifting too far from the documented defaults

Profile intent:

- keep runtime batching close to defaults
- expand pattern and search working sets enough for multi-pane operators
- preserve warning thresholds that still surface degradation before queue collapse

Promotion rule:

- this is the recommended first profile for medium fleets
- if queue depth keeps rising across three checks, step down to `fleet_10` or the base config before trying custom tuning

### `fleet_200_plus`

File: `fixtures/e2e/blessed_tuning_profiles/fleet_200_plus.toml`

Use this when:

- you are deliberately operating in the 200+ pane class
- queue stability, memory headroom, and diagnostics matter more than aggressive latency targets
- you are prepared to keep the run in a controlled-operations posture

Profile intent:

- widen coalescing and lock thresholds to avoid pathological churn
- increase telemetry windows and tracked pane counts
- bound API fan-out and stream rates so health surfaces remain legible under sustained pressure

Promotion rule:

- do not present this as a release-default profile while `docs/ft-xbnl0-4-6-release-gates-validation.json` still reports the `REL-04-performance-budget` failure
- treat any unplanned `Black` backpressure tier as an escalation event, not as “expected large fleet noise”

## Live Monitoring Loop

Run these commands after every profile apply and keep the outputs with the run notes:

```bash
ft config show --json --path ./ft.toml
ft status --health | jq '.health.backpressure_tier'
ft status --health | jq '{capture:.health.capture_queue_depth, write:.health.write_queue_depth}'
ft status --health | jq '.health.ingest_lag_max_ms'
ft doctor --json
ft robot events --limit 20
```

Interpretation:

- `backpressure_tier = Green` or `Yellow` is expected during normal operation.
- repeated `Red` means stop tuning upward and capture artifacts.
- any unplanned `Black` for `fleet_10` or `fleet_50` is an immediate rollback signal.
- for `fleet_200_plus`, a planned soak may intentionally hit `Black`, but an operator run should still capture `ft doctor --json` and `ft robot events --limit 20` before continuing.

## Escalation Paths

### Stop-and-revert immediately

Do this for any profile if:

- `health.db_writable == false`
- `health.ingest_lag_max_ms > 15000` for three consecutive checks
- `health.in_crash_loop == true`
- `health.consecutive_crashes >= 3`

Command surface:

```bash
ft status --health | jq '.health.db_writable'
ft status --health | jq '.health.ingest_lag_max_ms'
ft status --health | jq '{in_crash_loop:.health.in_crash_loop, consecutive:.health.consecutive_crashes}'
```

Next step:

- roll back the config profile with `ft config profile rollback --yes --path ./ft.toml`
- follow `docs/operator-playbook.md`

### Hold at operator-only posture

Do this for `fleet_200_plus` if:

- you need the profile to keep a large run stable
- the finish-line release gates are still failing
- the latest leak or soak evidence is incomplete

Next step:

- keep using the profile only with retained artifacts
- do not treat it as a stable release recommendation until `docs/ft-xbnl0-4-6-release-gates-validation.json` is green

## Replayable Validation Path

Use this exact sequence when you need to prove the blessed-profile package still works:

```bash
bash scripts/check_ft_xbnl0_5_3_blessed_tuning_profiles.sh
bash tests/e2e/test_ft_xbnl0_5_3_blessed_tuning_profiles.sh
```

The verifier and harness check:

- the machine-readable contract and playbook still point at real evidence files
- the three blessed profile overlays exist and remain parseable
- the `ft config profile` CLI can list, diff, apply, validate, show, and roll back each profile through `rch`-offloaded execution

## Handoff to the Final Closure Bundle

Future closure work should cite this playbook and its harness directly instead of rewriting the same tuning guidance in another ad hoc note.

The final closure bundle (`ft-xbnl0.5.5`) should consume:

- this document
- `docs/ft-xbnl0-5-3-blessed-tuning-profiles.json`
- the latest artifact bundle from `tests/e2e/test_ft_xbnl0_5_3_blessed_tuning_profiles.sh`
