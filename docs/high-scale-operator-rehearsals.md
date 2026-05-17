# High-Scale Operator Rehearsals

Status: bounded rehearsal runbook for `ft-bsfb9.13`.

This runbook ties together the high-scale proof ledger, chaos/recovery evidence,
SLO cockpit, and Robot/MCP golden matrix so an operator can rehearse the failure
paths without mutating live GUI state or claiming unproven hardware results.

Use it after reading `docs/operator-runbook.md` and
`docs/high-core-swarm-runbook.md`. The bounded rehearsal is intentionally
shell-only by default: it does not run Cargo, restart Agent Mail, restart RCH,
kill panes, or touch live processes.

## Bounded Script

Run:

```bash
scripts/high-scale-rehearsal.sh
```

Optional live probes are non-mutating status checks only:

```bash
scripts/high-scale-rehearsal.sh --live-probes
```

Verify an existing artifact bundle without running probes or Cargo:

```bash
scripts/high-scale-rehearsal.sh --verify tests/e2e/artifacts/high-scale-rehearsal/<UTC_RUN_ID>
```

Verifier mode checks the documented artifact contract: required files,
summary/event count agreement, required scenario rows, allowed receipts,
zero failure rows, `local_cargo_used=false`, and
`destructive_actions_used=false`. It requires `jq` and reads only the supplied
artifact directory.

The script writes a run directory under
`tests/e2e/artifacts/high-scale-rehearsal/<UTC_RUN_ID>/` unless `--out-dir` or
`FT_REHEARSAL_OUT_DIR` is set. Expected artifacts:

| Artifact | Meaning |
| --- | --- |
| `rehearsal-events.jsonl` | One structured receipt per rehearsal scenario. |
| `rehearsal-summary.json` | Aggregate pass/skip counts and safety flags. |
| `git-head.txt` / `git-branch.txt` | Source snapshot for the run. |
| `git-status-short.txt` | Dirty tracked-file context, with untracked files omitted. |
| Copied fixture JSON | Static proof/control-plane fixtures used by the rehearsal. |

Any unavailable live dependency is recorded as `SKIPPED_NOT_PROVEN`. That state
is not a source failure and cannot support a high-scale performance claim.

## Scenario Matrix

| Scenario | Command | Prerequisites | Expected artifacts | Expected receipt | Escalation |
| --- | --- | --- | --- | --- | --- |
| Synthetic swarm scale | `scripts/high-scale-rehearsal.sh` | `fixtures/scale-lab/massive-swarm-evidence-index.v1.json` exists | `massive-swarm-evidence-index.v1.json`, event row | `READY` for fixture presence; embedded live-hardware row may remain `SKIPPED_NOT_PROVEN` | If fixture missing, block runbook closeout and restore the scale-lab artifact from the owning bead. |
| Degraded Agent Mail | `scripts/high-scale-rehearsal.sh --live-probes` | `am` binary available; do not restart service | `agent-mail-status.txt`, event row | `READY` if status command exits 0; otherwise `SKIPPED_NOT_PROVEN` | Coordinate through Beads. Do not run `am service restart`, repair, or kill shared mail processes. |
| RCH worker loss | `scripts/high-scale-rehearsal.sh --live-probes` | `rch` binary available | `rch-status.txt`, event row | `READY` if status command exits 0; otherwise `SKIPPED_NOT_PROVEN` | Use the proof-lane contract to classify wrapper/pre-Cargo failures; do not call local Cargo proof. |
| Storage/indexing pressure | `scripts/high-scale-rehearsal.sh` | `fixtures/scale-lab/storage-index-heatmap-summary.v1.json` exists | copied heat-map fixture, event row | `READY` for fixture presence | If pressure rows are stale, hand off to the storage/indexing heat-map bead rather than tuning blindly. |
| Policy approval backlog | `scripts/high-scale-rehearsal.sh` plus the active policy bead receipts when available | `docs/risk-scoring.md` exists; richer receipts may come from `ft-bsfb9.10` | copied risk-scoring doc, event row, optional policy receipt fixture | `READY` for static rehearsal input; richer receipts must distinguish `allow`, `deny`, `require_approval`, `delay`, and `ask_human` | If only prose exists, avoid claiming approval automation beyond bounded rehearsal coverage. |
| Robot/MCP control-plane smoke | `scripts/high-scale-rehearsal.sh` | `crates/frankenterm-core/tests/golden_robot_envelope/control_plane_golden_matrix.json` exists | copied golden matrix, event row | `READY` for fixture presence | If matrix drift is found, run the focused RCH proof from `docs/robot-contracts/current-ntm-gap-dispatch.md`. |
| Mission chaos recovery | `scripts/high-scale-rehearsal.sh` | `docs/metrics/mission_chaos_evidence.json` exists and does not mark raw logs unretained | copied chaos summary evidence, event row | `READY` only for retained raw proof; `SKIPPED_NOT_PROVEN` when the metric marks runtime logs as unretained | If missing, stale, or raw logs are unavailable, use the chaos harness owner path; do not synthesize a pass from operator notes. |
| SLO cockpit bottlenecks | `scripts/high-scale-rehearsal.sh` | `SloCockpitSnapshot` exists in `runtime_health.rs` | `slo-cockpit-symbols.txt`, event row | `READY` for core API availability | For missing domains or wrong next steps, reopen the cockpit bead or file a focused follow-up. |

## Chaos Restore Scenario Specs

Status: scenario contract for `ft-8xw5v`. These rows are executable
requirements for future harness work. They do not claim that the current
bounded rehearsal script already runs them. Until the named commands exist and
produce the required receipts, the correct rehearsal receipt is
`SKIPPED_NOT_PROVEN`.

All scenarios inherit the safety rules above: no live pane kill, watcher
restart, service restart, or destructive filesystem action is allowed unless the
harness explicitly creates an isolated fixture process and records that sandbox
boundary in the artifact.

### Required Artifact Shape

Every chaos restore scenario must emit:

| Artifact | Requirement |
| --- | --- |
| `commands.txt` | Exact command lines in execution order, including the full `rch exec -- env CARGO_TARGET_DIR=...` command for Cargo-backed proof. |
| `env.txt` | Host, repo SHA, branch, target dir, feature flags, seed, fixture root, and whether any live dependency was used. |
| `scenario.json` | Stable `scenario_id`, schema version, injected fault class, proof mode, expected receipts, and whether the run is `reduced`, `loopback`, or `live_mux`. |
| `events.jsonl` | One row per injected fault, recovery transition, typed unavailable response, policy decision, queue/drop event, and compensation step. |
| `storage-receipts.jsonl` | Observed database/audit rows, keyed by table or typed store, with stable row ids when available. |
| `summary.json` | Pass/fail/skip verdict, reason code, coverage flags, elapsed time, and paths to all artifacts. |

Structured log rows must include `timestamp`, `bead_id`, `scenario_id`,
`correlation_id`, `step`, `status`, `duration_ms`, `backend`, `artifact_dir`,
`fault`, `receipt`, and `reason_code`. A log row may redact pane text, but it
must still include the pane id, sequence number, and response envelope type.

### Scenario Matrix

| Scenario id | Fault and fixture setup | Contract command | Expected events | Expected storage and audit receipts | Pass criteria |
| --- | --- | --- | --- | --- | --- |
| `chaos_mux_disconnect_typed_unavailable` | Use a loopback mux fixture with one healthy pane and one simulated disconnected pane. Inject the disconnect through the mux adapter or `FaultPoint::WeztermCliCall`; do not kill a live GUI process. | `rch exec -- env CARGO_TARGET_DIR=/tmp/ft-8xw5v-mux cargo test -p frankenterm-core --lib ft_8xw5v_mux_disconnect_typed_unavailable -- --nocapture` | `mux.disconnect.injected`, `pane.live_text_unavailable`, `robot.typed_unavailable_returned`, and `search.persisted_fallback_checked`. | Persisted pane/output rows remain readable. No policy-denial row is required. The unavailable live-read response must include a typed error code and hint to persisted search. | Live `get-text` style read fails closed with a typed unavailable envelope; persisted search remains available; no raw panic, generic runtime string, or empty success response is accepted. |
| `chaos_storage_contention_bounded_backoff` | Use a temporary SQLite fixture and a writer/read contention injector. Prefer `FaultPoint::DbWrite` delay or fail-n-times over OS-level file locking. | `rch exec -- env CARGO_TARGET_DIR=/tmp/ft-8xw5v-storage cargo test -p frankenterm-core --lib ft_8xw5v_storage_contention_bounded_backoff -- --nocapture` | `storage.contention.injected`, `storage.backoff.started`, `storage.retry.receipt`, `event.persisted_after_recovery`, and no `panic` row. | `events` or equivalent event persistence shows the post-recovery event. Any denied write must have a typed storage error receipt. If a policy gate fires, `policy_denied_audit` row id must be logged. | Queue depth and retry count stay within declared bounds; after the injected contention clears, at least one event/segment persists successfully; timeout or fail-closed rows must explain whether data was skipped or retried. |
| `chaos_watcher_restart_lock_recovery` | Start an isolated watcher fixture with a temp workspace and lock file. Simulate a stale watcher lock by stopping only the fixture process or by writing a fixture lock record; do not stop a developer's real watcher. | `rch exec -- env CARGO_TARGET_DIR=/tmp/ft-8xw5v-watcher cargo test -p frankenterm-core --lib ft_8xw5v_watcher_restart_lock_recovery -- --nocapture` | `watcher.fixture_started`, `watcher.stale_lock_detected`, `watcher.recovery_started`, `watcher.recovered`, and `robot.state_after_recovery`. | Lock metadata is copied to `storage-receipts.jsonl`; if runtime storage participates, the recovered watcher run id and prior stale owner id are both recorded. | Recovery must not reuse the stale owner id, must not drop newly captured fixture output, and must surface a diagnostic when the lock cannot be proven stale. Ambiguous liveness is `SKIPPED_NOT_PROVEN`, not pass. |
| `chaos_event_bus_subscriber_lag` | Use an in-process `EventBus` with capacity small enough to force lag, one intentionally slow subscriber, and one healthy subscriber. Publish a deterministic event sequence. | `rch exec -- env CARGO_TARGET_DIR=/tmp/ft-8xw5v-event-bus cargo test -p frankenterm-core --lib ft_8xw5v_event_bus_subscriber_lag -- --nocapture` | `event_bus.publish.started`, `event_bus.subscriber_lagged`, `event_bus.latest_retained_delivered`, and `event_bus.healthy_subscriber_unaffected`. | No database row is required unless the implementation persists lag diagnostics. If it does, the diagnostic row id and lag count must be logged. | Slow subscriber observes the documented lag behavior and resumes at the newest retained event; healthy subscriber keeps receiving ordered events; lag does not become an unbounded retry loop. |
| `chaos_tx_partial_commit_compensation` | Use a mission tx fixture with two commit steps and compensations. Force step 2 to fail after step 1 records a committed receipt. Reuse existing tx/compensation test helpers where possible. | `rch exec -- env CARGO_TARGET_DIR=/tmp/ft-8xw5v-tx cargo test -p frankenterm-core --lib ft_8xw5v_tx_partial_commit_compensation -- --nocapture` | `tx.prepare.all_ready`, `tx.commit.partial_failure`, `tx.compensation.started`, `tx.compensation.receipt`, and `tx.final_state.compensated` or typed failure. | `storage-receipts.jsonl` must include the tx id, committed step receipt id, failed step id, compensation receipt id, and final lifecycle state. If compensation fails, include the failure barrier reason. | Every committed step has either a successful compensation receipt or an explicit compensation-failed receipt. No final state may claim committed success after a partial commit failure. |

### Scenario Implementation Notes

- Use `crates/frankenterm-core/src/chaos.rs` for fault injection where the
  existing `FaultPoint` surface matches the scenario.
- Use `docs/metrics/mission_chaos_evidence.json` and
  `tests/e2e/test_mission_chaos.sh` as precedent for fixed seeds and evidence
  bundling.
- Use `tests/e2e/e2e_tx_run.py`,
  `tests/e2e/test_mission_tx_interfaces.sh`, and
  `crates/frankenterm-core/src/tx_killswitch_model.rs` as the tx compensation
  contract anchors.
- For event-bus lag, prefer a reduced in-process proof first; live stream
  probes must still prove the same typed lag receipt before graduating.
- For storage contention, separate source failures from RCH wrapper or worker
  failures. RCH sync without Cargo is infra evidence only, not a source verdict.

## Proof And Truthfulness Rules

Use these anchors when interpreting rehearsal output:

- `docs/proposals/ft-tn6cw-proof-lane-evidence-contract.md` for proof states,
  reason codes, and invalid local-Cargo command shapes.
- `docs/proposals/ft-tn6cw-proof-lane-evidence-taxonomy.md` for
  `SKIPPED_NOT_PROVEN` semantics.
- `fixtures/scale-lab/massive-swarm-evidence-index.v1.json` for reduced and
  skipped high-scale rows.
- `crates/frankenterm-core/tests/golden_robot_envelope/control_plane_golden_matrix.json`
  and `crates/frankenterm-core/tests/control_plane_golden_matrix.rs` for
  Robot/MCP envelope drift checks.
- `docs/metrics/mission_chaos_evidence.json` for chaos/recovery evidence
  summaries. If it marks raw runtime logs as unretained, the high-scale
  rehearsal must emit `SKIPPED_NOT_PROVEN` rather than treating summary
  counters as retained raw proof.
- `crates/frankenterm-core/src/runtime_health.rs` for the SLO cockpit model.

Closeout wording must distinguish:

| Observation | Correct wording |
| --- | --- |
| Static fixture copied | "bounded rehearsal fixture was present" |
| Live dependency not probed | "`SKIPPED_NOT_PROVEN`; live probe not requested" |
| RCH sync without remote Cargo | "infra blocked before Cargo; no source verdict" |
| Local Cargo output | "local smoke only; not remote proof" |
| Target hardware predicate absent | "`SKIPPED_NOT_PROVEN`; no 64-core / 256 GiB claim" |

## Operator Checklist

1. Create a fresh run directory with `scripts/high-scale-rehearsal.sh`.
2. Verify the bundle with `scripts/high-scale-rehearsal.sh --verify <run-dir>`.
3. Attach `rehearsal-summary.json` and `rehearsal-events.jsonl` to the Beads
   closeout or handoff.
4. For every `SKIPPED_NOT_PROVEN` row, decide whether it is acceptable for this
   rehearsal or whether the owning bead must provide a real fixture/proof.
5. If a Cargo proof is needed, switch to RCH and the exact command shape from
   the owning proof contract. Keep the bounded rehearsal artifacts separate from
   remote proof artifacts.
6. Never promote a rehearsal pass into a high-scale performance claim unless
   the proof ledger has target-class hardware evidence.
