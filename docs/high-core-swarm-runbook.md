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
- `docs/resource-pressure-cockpit-contract.md` for the v1 cockpit schema,
  residual unavailable-domain semantics, and retained conformance artifact.
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
- The retained v1 cockpit conformance artifact
  `tests/e2e/artifacts/goal-line/ft-rz0eb.4/resource_cockpit_conformance/20260510T125418Z/summary.json`
  proves remote-reduced schema/runtime conformance only. It explicitly keeps
  target-class hardware at `skipped_not_proven`.

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

## Capture Fairness Proof Branch

Use this branch when the high-scale question is whether polling capture remains
fair across pane priorities and does not silently starve lower-priority panes.
The contract and operator vocabulary live in
`docs/capture-fairness-slo-contract.md`.

Run the retained reduced 200-pane proof lane through its wrapper:

```bash
set -o pipefail
bash tests/e2e/test_ft_n447z_5_capture_fairness_200.sh \
  | tee "$FT_HIGH_CORE_RUN_DIR/capture-fairness-wrapper.log"
grep '^Summary: ' "$FT_HIGH_CORE_RUN_DIR/capture-fairness-wrapper.log" \
  | tail -1 | sed 's/^Summary: //' \
  | tee "$FT_HIGH_CORE_RUN_DIR/capture-fairness-summary-path.txt"
```

Inspect the wrapper summary and Rust source artifact:

```bash
CAPTURE_SUMMARY="$(cat "$FT_HIGH_CORE_RUN_DIR/capture-fairness-summary-path.txt")"
RUST_CAPTURE_SUMMARY="$(jq -r '.artifacts.rust_summary' "$CAPTURE_SUMMARY")"
jq '{status, proof_interpretation, failure_classification, reason_code}' \
  "$CAPTURE_SUMMARY"
jq '{status:.pass_fail.status,
     panes:.inputs.total_panes,
     target_class:.proof_interpretation.target_class_hardware,
     every_pane_serviced:.pass_fail.every_pane_serviced,
     untruncated:.pass_fail.snapshot_rows_untruncated,
     selected_total:.throughput_counters.selected_total,
     selected_by_priority:.throughput_counters.selected_by_priority,
     max_first_service_round:.capture_lag_histograms.max_first_service_round,
     max_service_gap:.capture_lag_histograms.max_service_gap}' \
  "$RUST_CAPTURE_SUMMARY"
```

Interpretation:

- A passing `ft-n447z.5` lane is `remote_reduced` evidence for the retained
  200-pane scheduler proof. It is useful regression evidence, but it is not
  target-class 64+ CPU / 256 GiB proof.
- `proof_interpretation.target_class_hardware = "skipped_not_proven"` forbids
  high-core support claims even when every pane was serviced.
- `source_or_test` means the RCH lane reached Cargo/test execution and found a
  real source or assertion failure. `environment` and `rch_substrate` mean the
  wrapper did not prove source failure; keep those blockers separate in the
  bead closeout.
- To make a target-class capture-fairness claim, retain the same capture
  fairness artifacts plus `doctor.json` with
  `hardware_profile.proof_predicates.proof_status = "proven_predicate_met"`.

## Resource What-If Proof

Use this branch when evaluating resource-control override candidates through
the replay-backed digital twin. The fixture manifest is
`fixtures/scale-lab/resource-what-if-proof/manifest.v1.json`; it links each
trace, override package, golden report summary, proof classification, and
command transcript.

The live high-scale predicate is exact:

- `proof_status = "PASSED"`
- `evidence_source = "live_hardware"`
- `hardware_evidence_complete = true`
- `hardware_cpu_count >= 64`
- `hardware_memory_bytes >= 274877906944`

Anything else, including replay-backed, synthetic, RCH-reduced, incomplete, or
local smoke evidence, must stay `SKIPPED_NOT_PROVEN` and
`high_scale_claim_allowed = false` for 64-core / 256 GiB claims.

Run the fixed proof contract through `rch` with an isolated target directory:

```bash
export CARGO_TARGET_DIR=/tmp/ft-resource-what-if-proof-target
RCH_NO_UPDATE_CHECK=1 RCH_EXTERNAL_TIMEOUT_ENABLED=false \
  rch exec -- bash -lc \
  'env CARGO_TARGET_DIR=/tmp/ft-resource-what-if-proof-target cargo test -p frankenterm --bin ft resource_what_if_proof_manifest -- --nocapture'
```

Build the CLI and run the command-level smoke. The script emits a final JSON
summary line with the trace, override package, command transcript, proof
status, hardware predicate, and high-scale gate:

```bash
export CARGO_TARGET_DIR=/tmp/ft-resource-what-if-proof-target
RCH_NO_UPDATE_CHECK=1 RCH_EXTERNAL_TIMEOUT_ENABLED=false \
  rch exec -- bash -lc \
  'env CARGO_TARGET_DIR=/tmp/ft-resource-what-if-proof-target cargo build -p frankenterm --bin ft'

FT_BIN=/tmp/ft-resource-what-if-proof-target/debug/ft \
  bash tests/e2e/test_resource_what_if.sh \
  | tee "$FT_HIGH_CORE_RUN_DIR/resource-what-if-summary.jsonl"
```

For a live high-scale run, keep these artifacts together:

- `ft doctor --json` output showing the hardware predicate;
- the source `DigitalTwinTrace` artifact and its `trace_hash`;
- the resource-control override package and its `override_hash`;
- the `ft resource what-if --format json` report;
- the Robot TOON transcript for the same trace/package;
- the command log from the `rch` proof lane above.

Do not promote a candidate from replay fixtures alone. Replay fixtures are
operator evidence for "what would the digital twin decide"; they are not proof
that target-class hardware accepted the same resource settings.

## Resource Cockpit

The resource cockpit lives under `.swarm_capacity.resource_cockpit` in
`ft doctor --json` and `ft robot capacity --level 2` output. The schema anchor is
`SwarmResourceCockpitSnapshot` with `SWARM_RESOURCE_COCKPIT_SCHEMA_VERSION`.
The v1 root fields that matter for high-core truth are `run_identity`,
`domains`, `residency_buckets`, `queue_backpressure`, `admission_decisions`,
`action_receipts`, and `artifact_paths`. Missing telemetry must appear as
`unavailable` or `skipped_not_proven`; it is not a green result.

Inspect the v1 cockpit fields:

```bash
jq '.swarm_capacity.resource_cockpit |
    {schema_version, contract_id, status, proof_gate, evidence_state,
     run_identity, domains, residency_buckets, queue_backpressure,
     admission_decisions, action_receipts, artifact_paths}' \
  "$FT_HIGH_CORE_RUN_DIR/doctor.json"
```

Operator interpretation:

| Field | Green path | Escalation path |
| --- | --- | --- |
| `proof_gate` | `healthy` or `pressured` during expected bursts | `degraded` or `skipped_proof` needs artifact capture before promotion. |
| `run_identity.hardware_predicate.proof_status` | `proven_predicate_met` on a target-class host | `skipped_not_proven` means no 64-core / 256 GiB claim, even if reduced tests passed. |
| `domains.memory` | `normal` with measured, fresh evidence | `critical`, `emergency`, `unknown`, stale, or unavailable means reduce fanout or collect diagnostics. |
| `domains.rss_residency` and `residency_buckets` | Heap, mmap, SQLite page cache, graphics/media, scrollback cache, child process, and unknown residency are separated | Missing classifier evidence or non-zero `unknown` requires a diagnostic bundle before calling it a leak. |
| `domains.queue_backpressure` and `queue_backpressure` | Bounded capture/write/persistence/search queues | `red`, `black`, or stale queue rows mean pause fanout before tuning upward. |
| `domains.storage_io` | `green` with bounded queues and zero write errors | Growing lag or write errors mean treat storage as its own pressure domain. |
| `admission_decisions` | `admit` or planned `degrade` under controlled pressure | `defer`, `degrade`, or `shed` without a planned soak requires triage. |
| `action_receipts` | Applied or succeeded receipts with artifact paths | Missing, failed, rollback, or approval-required receipts are not proof that mitigation happened. |

## Troubleshooting Branches

### CPU Saturation

Signals:

```bash
jq '.hardware_profile.cpu' "$FT_HIGH_CORE_RUN_DIR/doctor.json"
jq '.swarm_capacity.resource_cockpit.domains.worker_pool,
    .swarm_capacity.resource_cockpit.domains.queue_backpressure,
    .swarm_capacity.resource_cockpit.queue_backpressure' \
  "$FT_HIGH_CORE_RUN_DIR/doctor.json"
```

Actions:

- If CPU is below the target predicate, mark the run `skipped_not_proven`.
- If CPU is target-class but latency cohorts keep regressing, reduce fanout,
  hold at `fleet_50`, and rerun `ft robot capacity --level 2`.
- Keep a note of the queue, worker-pool, or drilldown reason code that explains
  the latency pressure.

### Memory Pressure

Signals:

```bash
jq '.swarm_capacity.resource_cockpit.domains.memory,
    .swarm_capacity.resource_cockpit.domains.rss_residency,
    .swarm_capacity.resource_cockpit.residency_buckets,
    .swarm_capacity.resource_cockpit.action_receipts' \
  "$FT_HIGH_CORE_RUN_DIR/doctor.json"
```

Actions:

- For `critical` or `emergency`, stop adding agents and capture a diagnostic bundle.
- Prefer demoting hot resident state to warm/cold tiers before killing work, but
  verify the relevant `action_receipts` before claiming the mitigation happened.
- If `refused_bytes` grows or `unknown` residency dominates, shed optional
  diagnostics and search fanout first, then retain `artifact_paths`.

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
| Retained cockpit v1 conformance summary | Live | `tests/e2e/artifacts/goal-line/ft-rz0eb.4/resource_cockpit_conformance/20260510T125418Z/summary.json` records passed local static and remote-reduced proof, with target hardware still `skipped_not_proven`. |
| `tests/e2e/test_ft_n447z_5_capture_fairness_200.sh` | Live | Retained reduced 200-pane RCH wrapper emits `summary.json`, `proof-ledger.jsonl`, the raw RCH log, and the Rust capture fairness summary. |
| `capture_fairness_200_pane_summary.json` | Live | `tailer.rs` proof artifact records pass/fail, selected-by-priority counters, capture lag histograms, skipped-poll reasons, and target hardware `skipped_not_proven`. |
| Cockpit docs truth smoke | Live | `crates/frankenterm/tests/docs_smoke.rs::resource_pressure_cockpit_docs_truth_gate` guards the v1 field names and legacy default-branch references in live docs. |
| `large_swarm_replay_corpus release_evidence` | Live | Release checklist and `crates/frankenterm-core/tests/large_swarm_replay_corpus.rs` contain the gate. |
| `ft config profile apply fleet_200_plus --path ./ft.toml` | Live | `ConfigProfileCommands::Apply` backs the documented blessed-profile workflow. |
| `ft diag bundle --output ... --events 500` | Live | CLI help and diagnostic command handling expose the support-bundle surface. |
| `ft reproduce export --kind manual --out ...` | Live | Incident bundle export command backs replayable support artifacts. |
| Automated herd-wave control | Planned | Manual branch stays in this runbook until `ft-wks87` closes. |
