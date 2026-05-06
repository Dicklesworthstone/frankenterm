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
| Mission chaos recovery | `scripts/high-scale-rehearsal.sh` | `docs/metrics/mission_chaos_evidence.json` exists | copied chaos evidence, event row | `READY` for fixture presence | If missing or stale, use the chaos harness owner path; do not synthesize a pass from operator notes. |
| SLO cockpit bottlenecks | `scripts/high-scale-rehearsal.sh` | `SloCockpitSnapshot` exists in `runtime_health.rs` | `slo-cockpit-symbols.txt`, event row | `READY` for core API availability | For missing domains or wrong next steps, reopen the cockpit bead or file a focused follow-up. |

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
- `docs/metrics/mission_chaos_evidence.json` for retained chaos/recovery
  evidence.
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
2. Attach `rehearsal-summary.json` and `rehearsal-events.jsonl` to the Beads
   closeout or handoff.
3. For every `SKIPPED_NOT_PROVEN` row, decide whether it is acceptable for this
   rehearsal or whether the owning bead must provide a real fixture/proof.
4. If a Cargo proof is needed, switch to RCH and the exact command shape from
   the owning proof contract. Keep the bounded rehearsal artifacts separate from
   remote proof artifacts.
5. Never promote a rehearsal pass into a high-scale performance claim unless
   the proof ledger has target-class hardware evidence.
