# High-Core Swarm Operator Runbook

Date: 2026-05-04
Bead: `ft-bd1eb`
Status: operator runbook for 64+ CPU / 256 GiB hosts; local smoke is not release proof

## Purpose

Use this runbook when configuring, validating, or troubleshooting FrankenTerm
on a target-class high-core host. A target-class host means at least 64 logical
CPUs and at least 256 GiB memory, as checked by the `ft doctor` hardware profile
predicate. Anything below that can run local smoke, but it must be reported as
`skipped_not_proven` for 64-core / 256 GiB performance claims.

This document depends on existing repo surfaces instead of inventing a parallel
operator flow:

- `docs/perf/swarm-capacity-baseline.md` for the workload taxonomy and artifact contract.
- `docs/ft-xbnl0-5-3-blessed-tuning-playbook.md` for starting profiles.
- `docs/ft-xbnl0-verification-contract.md` for proof levels and artifact shape.
- `docs/release/checklist.md` for the high-scale release evidence gate.
- `ft doctor --json` for `hardware_profile`, `swarm_capacity`, and
  `large_swarm_proof_gauntlet`.
- `ft robot capacity --level 2` for the machine-readable resource cockpit.

All branch and release examples in this runbook refer to `main`.

## Pre-Flight

Create a run directory first. Keep every command output there.

```bash
export RUN_ID=$(date -u +%Y%m%dT%H%M%SZ)
export FT_HIGH_CORE_RUN_DIR="tests/e2e/artifacts/high-core-swarm/${RUN_ID}"
mkdir -p "$FT_HIGH_CORE_RUN_DIR"
git branch --show-current | tee "$FT_HIGH_CORE_RUN_DIR/branch.txt"
git rev-parse HEAD | tee "$FT_HIGH_CORE_RUN_DIR/git-head.txt"
```

If `branch.txt` is not `main`, stop and coordinate before collecting proof.

Collect the local operator snapshot:

```bash
ft doctor --json > "$FT_HIGH_CORE_RUN_DIR/doctor.json"
ft robot capacity --level 2 > "$FT_HIGH_CORE_RUN_DIR/robot-capacity.json"
ft robot --format toon capacity --level 2 > "$FT_HIGH_CORE_RUN_DIR/robot-capacity.toon"
```

Check the hardware predicate and proof-gauntlet truth status:

```bash
jq '.hardware_profile.proof_predicates' "$FT_HIGH_CORE_RUN_DIR/doctor.json"
jq '{status:.large_swarm_proof_gauntlet.status,
     evidence_mode:.large_swarm_proof_gauntlet.run_context.evidence_mode,
     skip_reasons:.large_swarm_proof_gauntlet.skip_reasons,
     failure_reasons:.large_swarm_proof_gauntlet.failure_reasons}' \
  "$FT_HIGH_CORE_RUN_DIR/doctor.json"
```

Interpretation:

- `hardware_profile.proof_predicates.proof_status = "proven_predicate_met"` means the
  host has enough CPU and memory for the high-scale proof predicate.
- `hardware_profile.proof_predicates.proof_status = "skipped_not_proven"` means any
  64-core / 256 GiB performance claim remains unproven on this host.
- `large_swarm_proof_gauntlet.status = "skipped_not_proven"` is expected for
  synthetic smoke. Do not promote it to release proof.
- `large_swarm_proof_gauntlet.status = "proven"` only counts when the manifest
  was collected in real-hardware mode and linked to retained replay artifacts.

## Initial Profile

Start from the blessed profile that matches the operational posture:

| Situation | Initial profile | Reason |
| --- | --- | --- |
| Local or small validation run | `fleet_10` | Tighter latency warnings and smaller windows. |
| Routine multi-agent run | `fleet_50` | Balanced default for normal operator use. |
| Controlled 200+ pane or target-class proof run | `fleet_200_plus` | More queue and telemetry headroom. |

Apply profiles through the existing config-profile command surface:

```bash
ft config profile list --json --path ./ft.toml > "$FT_HIGH_CORE_RUN_DIR/profiles.json"
ft config profile diff fleet_200_plus --path ./ft.toml > "$FT_HIGH_CORE_RUN_DIR/profile.diff"
ft config profile apply fleet_200_plus --path ./ft.toml
ft config validate --path ./ft.toml
```

If the proof predicate is not met, use `fleet_10` or `fleet_50` and record the run
as local smoke. Do not use `fleet_200_plus` to imply the host is target-class.

## Proof Gauntlet

Use `rch` and an isolated target directory for cargo work:

```bash
export CARGO_TARGET_DIR=/tmp/ft-high-core-gauntlet-target
rch exec -- env CARGO_TARGET_DIR="$CARGO_TARGET_DIR" \
  cargo test -p frankenterm-core --test large_swarm_replay_corpus \
    release_evidence --no-default-features -- --nocapture \
  | tee "$FT_HIGH_CORE_RUN_DIR/release-evidence.log"
```

Expected outcomes:

- On local or undersized hosts, the gate should keep high-scale claims below
  real-hardware proof and render unsupported claims as not proven.
- On a target-class host, keep the full `doctor.json`, `release-evidence.log`,
  and any replay artifacts with the run. A chat transcript is not proof.
- If `rch` fails before cargo starts, record the wrapper failure separately from
  test failure. Follow the repo fallback rules before claiming a code defect.

## Resource Cockpit

The resource cockpit lives under `.swarm_capacity.resource_cockpit` in
`ft doctor --json` and `ft robot capacity --level 2` output. The schema anchor is
`SwarmResourceCockpitSnapshot` with `SWARM_RESOURCE_COCKPIT_SCHEMA_VERSION`.

Inspect the compact cockpit fields:

```bash
jq '.swarm_capacity.resource_cockpit |
    {schema_version, status, proof_gate, memory_pressure,
     memory_tiers, slowest_latency_cohorts,
     resource_admission_decisions, storage_io,
     mitigation_history, drilldowns}' \
  "$FT_HIGH_CORE_RUN_DIR/doctor.json"
```

Operator interpretation:

| Field | Green path | Escalation path |
| --- | --- | --- |
| `proof_gate` | `healthy` or `pressured` during expected bursts | `degraded` or `skipped_proof` needs artifact capture before promotion. |
| `memory_pressure` | `unknown`, `nominal`, or equivalent low-pressure state | `critical` or `emergency` means reduce concurrency or trigger memory-tier mitigation. |
| `memory_tiers` | Hot resident, warm compressed, and cold disk stay within budget | Over-budget hot or warm tiers mean demote, compress, or shed noncritical work. |
| `slowest_latency_cohorts` | Known bottleneck stage with bounded p99 | Repeated p99 over budget means tune or pause fanout before adding panes. |
| `resource_admission_decisions` | `admit` or planned `degrade` under controlled pressure | `defer`, `degrade`, or `shed` without a planned soak requires triage. |
| `storage_io` | `io_pressure_tier=green` with bounded queues and zero write errors | `yellow`, `red`, `black`, growing lag, or write errors mean treat storage as its own pressure domain. |
| `drilldowns` | One clear reason per mitigation | Missing or vague reasons mean collect support artifacts and stop tuning upward. |

## Troubleshooting Branches

### CPU Saturation

Signals:

```bash
jq '.hardware_profile.cpu' "$FT_HIGH_CORE_RUN_DIR/doctor.json"
jq '.swarm_capacity.resource_cockpit.slowest_latency_cohorts' \
  "$FT_HIGH_CORE_RUN_DIR/doctor.json"
```

Actions:

- If CPU is below the target predicate, mark the run `skipped_not_proven`.
- If CPU is target-class but latency cohorts keep regressing, reduce fanout,
  hold at `fleet_50`, and rerun `ft robot capacity --level 2`.
- Keep a note of the stage name and reason code from `slowest_latency_cohorts`.

### Memory Pressure

Signals:

```bash
jq '.swarm_capacity.resource_cockpit.memory_pressure' "$FT_HIGH_CORE_RUN_DIR/doctor.json"
jq '.swarm_capacity.resource_cockpit.memory_tiers' "$FT_HIGH_CORE_RUN_DIR/doctor.json"
```

Actions:

- For `critical` or `emergency`, stop adding agents and capture a diagnostic bundle.
- Prefer demoting hot resident state to warm/cold tiers before killing work.
- If `refused_bytes` grows, shed optional diagnostics and search fanout first.

### IO Stalls

Signals:

```bash
ft status --health > "$FT_HIGH_CORE_RUN_DIR/status-health.txt"
ft robot events --limit 20 > "$FT_HIGH_CORE_RUN_DIR/events.json"
jq '.swarm_capacity.resource_cockpit.storage_io' "$FT_HIGH_CORE_RUN_DIR/doctor.json"
```

Actions:

- Check storage free bytes in `.hardware_profile.storage`.
- Pause noncritical search/index bursts if write queues are also growing.
- Follow `docs/storage-tuning.md` before changing storage knobs.

### Search Stalls

Signals:

```bash
ft robot search "large-swarm" --limit 5 > "$FT_HIGH_CORE_RUN_DIR/search-smoke.json"
ft robot events --limit 50 > "$FT_HIGH_CORE_RUN_DIR/search-events.json"
```

Actions:

- Distinguish query latency from ingestion lag before tuning.
- If search stalls coincide with IO pressure, treat storage as the root cause.
- If search stalls coincide with memory pressure, reduce query fanout and capture the cockpit.

### Herd Waves

Signals:

```bash
ft robot events --limit 100 > "$FT_HIGH_CORE_RUN_DIR/herd-events.json"
```

Actions:

- Look for many panes compacting, retrying, waking, or rate-limit recovering in the
  same window.
- Until `ft-wks87` ships automated herd-wave control, stagger manual nudges and
  avoid broadcasting identical retry commands to every pane.
- Record the event window and pane cohort before continuing.

## Support Bundle

When a run is degraded, failed, or unproven, capture both diagnostics and a replayable incident bundle:

```bash
ft diag bundle --output "$FT_HIGH_CORE_RUN_DIR/diag-bundle" --events 500
ft reproduce export --kind manual --out "$FT_HIGH_CORE_RUN_DIR/incident-bundle"
```

Before sharing, review the bundle for sensitive pane content and keep the
run directory alongside the bead close reason or support ticket.

## Command And Schema Checklist

This checklist is the docs smoke for this runbook. Each row is either checked
against a live command/schema name or explicitly marked planned.

| Reference | Status | Check |
| --- | --- | --- |
| `ft doctor --json` | Live | `DoctorCommands` path emits `hardware_profile`, `large_swarm_proof_gauntlet`, and `swarm_capacity`. |
| `ft robot capacity --level 2` | Live | `RobotCommands::Capacity` parses `--level`; help advertises `ft robot --format toon capacity --level 2`. |
| `SwarmResourceCockpitSnapshot` | Live | `runtime_telemetry.rs` exports the cockpit schema and nested summary. |
| `SWARM_RESOURCE_COCKPIT_SCHEMA_VERSION` | Live | Runtime telemetry tests assert the current schema version. |
| `large_swarm_replay_corpus release_evidence` | Live | Release checklist and `crates/frankenterm-core/tests/large_swarm_replay_corpus.rs` contain the gate. |
| `ft config profile apply fleet_200_plus --path ./ft.toml` | Live | `ConfigProfileCommands::Apply` backs the documented blessed-profile workflow. |
| `ft diag bundle --output ... --events 500` | Live | CLI help and diagnostic command handling expose the support-bundle surface. |
| `ft reproduce export --kind manual --out ...` | Live | Incident bundle export command backs replayable support artifacts. |
| Automated herd-wave control | Planned | Manual branch stays in this runbook until `ft-wks87` closes. |
