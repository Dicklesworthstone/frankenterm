# RCH Proof Ledger Execution Policy

**Bead:** `ft-kvs1e`
**Version:** `3.2.0`
**Status:** Active proof-ledger foundation
**Worker-targeted extension:** `ft-ilxky.1`

## Purpose

This policy removes ambiguity in FrankenTerm validation runs by requiring `rch`
for heavy compute workloads and a standard proof-ledger evidence contract for
every verifier run that future agents cite as proof. Agents should cite
validated proof-ledger artifacts instead of treating RCH setup, sync, or
transfer chatter as proof that a verifier ran.

## Scope

Applies to all `ft-*` FrankenTerm beads whenever commands are expected to
create material CPU/IO contention (build/test/bench/soak workloads). It
generalizes the original asupersync migration policy and carries forward the
closed `ft-uz2gg` proof-lane requirement that RCH closeouts identify the target
directory lifecycle instead of leaving temp-target state ambiguous.

The historical file names still contain `asupersync` because this contract
started in the asupersync migration lane. The contract is now repository-wide:
normal `ft-*` bead IDs are accepted, and the schema is no longer restricted to
`ft-e34d9.10.*`.

## Heavy vs Light Classifier

Commands are classified as follows:

| Category | Command examples | rch required |
|---|---|---|
| Heavy | `cargo check`, `cargo build`, `cargo test`, `cargo clippy`, `cargo bench`, `cargo run`, `cargo install`, soak/perf loops that invoke Cargo repeatedly | Yes |
| Light | `cargo fmt --check`, `cargo metadata`, `cargo locate-project`, docs/scripts that do not compile/test | No |

Classifier implementation is canonical in:

- `scripts/validate_asupersync_rch_execution_policy.sh --classify "<cmd>"`

The classifier is intentionally strict: setup or sync chatter that merely
mentions `rch` is not proof that Cargo ran remotely.

## Proof Vocabulary

Use this vocabulary in proof-ledger artifacts, Beads comments, Agent Mail
closeouts, and Robot/MCP summaries. The terms describe evidence quality, not
agent effort.

The canonical closeout phrases are: remote RCH proof, light local proof,
approved local fallback, invalid local-heavy claim, static-only check, and
blocked verifier.

| Term | Ledger shape | Closeout meaning |
|---|---|---|
| Remote RCH proof | `command_class: "heavy"`, `used_rch: true`, `execution_mode: "remote_rch"`, `validation_status: "valid"`, retained artifacts, and non-local worker context. | The material heavy command ran through an RCH-recognized remote path and may prove source behavior when the command passed. |
| Light local proof | `command_class: "light"`, `execution_mode: "local_light"`, `validation_status: "valid"`, and `target_dir: "not_applicable"`. | A local non-heavy check, such as `cargo fmt --check`, docs validation, JSON parsing, or shell syntax validation, proved only the static surface it exercised. |
| Approved local fallback | Heavy command evidence with `execution_mode: "approved_local_fallback"`, `validation_status: "approved_fallback"`, `fallback_reason_code`, and `fallback_approved_by`. | Human-approved degraded evidence. It is partial-risk evidence and must not be described as clean remote proof. |
| Invalid local-heavy claim | Heavy command evidence without remote RCH confirmation and without fallback approval metadata. | The validator and aggregate gate must reject this as `rejected_local_heavy`; keep the bead open or rerun remotely. |
| Static-only check | A light proof or non-Cargo static guard that never compiles, tests, benches, soaks, or runs the product. | Useful for docs, schemas, shell syntax, JSON shape, and grep-style assertions. It cannot close a source-behavior claim by itself. |
| Blocked verifier | Evidence with `execution_mode: "refused_local_fallback"` or `validation_status: "fallback_refused"`/`"timeout"` records that the intended verifier could not produce valid proof because workers were unavailable, the selected worker mirror was stale, artifacts were missing, the command timed out, or policy forbade fallback. | Report the blocker with reason code and retained logs. Do not convert the blocked verifier into a pass or approved fallback. |

## Mandatory Rule

For heavy commands, execution must use one of the validator-recognized
RCH-backed execution shapes:

```bash
rch exec -- <command>
VAR=value rch exec -- <command>
```

or the shared fail-closed harness helpers:

```bash
run_rch_cargo_logged <log-file> env CARGO_TARGET_DIR=<repo-relative-target> cargo <args...>
run_rch_cargo_logged_with_timeout <seconds> <log-file> env CARGO_TARGET_DIR=<repo-relative-target> cargo <args...>
```

The helper forms are evidence-equivalent to direct `rch exec -- ...` because
they route through `tests/e2e/lib_rch_guards.sh`, reject local fallback markers,
and emit RCH metadata sidecars.

RCH status checks, worker probes, sync logs, or shell wrappers that merely echo
`rch exec -- ...` are not proof. If the material Cargo command is local, the
ledger must classify it as a fallback and include approval metadata.

## Worker-Targeted Proof Contract

Worker-targeted proof is required when a bead or closeout claim is about a
specific RCH worker, source mirror, remote target directory, or per-worker
failure mode. A green run on a different worker can still be useful evidence,
but it is not equivalent to target-worker proof.

### Confidence Levels

| Level | Meaning | May close a worker-specific blocker? |
|---|---|---|
| `target_worker_remote_proof` | The material command ran on the named worker, reached remote Cargo/rustc when required, and retained worker/mirror metadata. | Yes, if the command itself passed and artifacts validate. |
| `target_worker_mirror_attestation` | The named worker was checked for repo snapshot and required tracked files, but no material Cargo proof ran. | No; it can unblock or diagnose a later proof run. |
| `scheduler_selected_remote_proof` | RCH selected a worker and the retained metadata identifies it, but the command did not require that exact worker up front. | Only for non-worker-specific source/test claims, or as weaker supporting evidence with residual risk. |
| `worker_self_test_only` | `rch check`, `rch status`, `rch workers probe`, SSH reachability, or smoke preflight ran without the material proof command. | No; this proves availability only. |
| `sync_or_transfer_only` | Logs show source sync, transfer, detached process setup, queue placement, or wrapper startup, but not Cargo/rustc/test execution. | No. |
| `inconclusive_worker_evidence` | The selected worker, repo snapshot, target dir, mirror status, or remote Cargo evidence is absent or contradictory. | No; rerun or collect better artifacts. |

### Required Evidence

When a claim depends on a specific worker, the proof-ledger entry, Beads
comment, or retained artifact bundle must include all of these fields or
explicitly classify the result as `inconclusive_worker_evidence`:

1. The intended worker id or host label, plus the observed selected worker when
   RCH reports one.
2. The intended command class: heavy remote Cargo, light local check, preflight,
   mirror attestation, or approved fallback.
3. Queue or admission state when available: ready, busy/waiting, unhealthy,
   unsupported worker selection, queue timeout, or unknown.
4. Repository snapshot: branch, HEAD commit, project path, and whether the
   worker source tree matched that snapshot.
5. Source mirror status: required tracked files present, missing, stale, or not
   checked. Missing tracked files are infrastructure blockers, not source
   failures.
6. Target directory and lifecycle, including whether it was retained,
   inventory-only, or not applicable.
7. Elapsed time, wrapper exit status, remote exit status when available, and
   whether Cargo, rustc, and the test binary were actually reached remotely.
8. Retained artifact paths for logs, metadata sidecars, mirror attestation, and
   proof-ledger JSONL.
9. Residual risk notes when proof was scheduler-selected rather than
   target-worker-enforced.

The schema-v3 proof-ledger fields for this contract are
`worker_evidence_confidence`, `intended_worker_id`, `selected_worker_id`,
`worker_queue_state`, `repo_snapshot_head`, `source_mirror_status`,
`source_mirror_reason_code`, `remote_cargo_reached`,
`remote_rustc_reached`, and `test_binary_reached`. Older records without
these fields are legacy/unknown worker evidence; do not silently promote them
to target-worker proof.

### Allowed And Forbidden Incident Actions

Allowed during worker-specific RCH incidents:

- Read-only `rch status`, `rch check`, worker probe, Beads, git, and log
  inspection.
- A material proof command through `rch exec -- ...` or a fail-closed harness.
- A read-only mirror attestation that checks required tracked-file hashes and,
  when available, HEAD metadata on the selected worker. RCH rsync mirrors may
  have stale or non-verifiable Git metadata because object data is intentionally
  excluded from transfer, so matching required-file hashes are the readiness
  signal for mirror preflight; HEAD mismatch/unavailability is retained in the
  artifact as residual context. Use
  `scripts/attest_rch_worker_mirror.sh --worker <id> --workspace-member-roots --path <tracked-file> ...`
  for this static-only evidence; `--workspace-member-roots` expands every
  `[workspace].members` root to its tracked `Cargo.toml` plus declared
  lib/bin/build entrypoints that exist locally, so dropped workspace crates fail
  before remote Cargo starts. The script emits
  `kind: "rch_selected_worker_mirror_attestation"` with reason codes such as
  `rch_mirror.project_path_absent`,
  `rch_mirror.required_files_ok_head_mismatch`,
  `rch_mirror.missing_tracked_file`,
  `rch_mirror.tracked_file_hash_mismatch`, and
  `rch_mirror.worker_unreachable`.
- Pool-level mirror preflight may accept partial pool freshness when at least
  `RCH_MIRROR_MIN_PASSING_WORKERS` workers pass the required-file attestation.
  The default threshold is `1`. Stale or missing mirrors on other healthy
  workers remain in the JSON artifact as residual evidence; the material
  `rch exec` run must still prove remote execution on the scheduler-selected
  worker.
- Harnesses that cannot tolerate scheduler selection of any stale checked
  worker can set `RCH_MIRROR_REQUIRE_ALL_CHECKED_WORKERS=1`. That mode blocks
  before Cargo when any scheduler-eligible checked worker has missing or
  mismatched required files. `RCH_MIRROR_BLOCK_ON_STALE_HEAD=1` additionally
  treats matching required-file hashes with unverifiable or mismatched Git
  metadata as blocking evidence.
- Harnesses with a concrete scheduler-selection preflight should also attest
  the selected worker's required files before starting Cargo. This keeps a
  partially stale pool from becoming a false source-test verdict when the
  daemon would otherwise choose a stale worker.
- Reopening or blocking a bead with exact artifact paths and reason codes when
  the worker-specific proof cannot be trusted.

Forbidden during worker-specific RCH incidents:

- Restarting, repairing, stopping, or killing Agent Mail, RCH, or shared
  services.
- Running local heavy Cargo and calling it remote proof.
- Draining or mutating shared workers just to make a proof easier to schedule.
- Deleting remote source files, target dirs, or local files to manufacture a
  negative test.
- Treating sync chatter, worker health, queue placement, or smoke preflight as
  evidence that the source/test command passed.

### Closeout Examples

Valid closeout evidence for a worker-specific blocker:

- `target_worker_remote_proof` on the named worker.
- Remote Cargo/rustc reached when the command class requires it.
- Required tracked files matched local content on that worker; HEAD metadata is
  corroborating context when the RCH mirror can verify it.
- Proof artifacts validate, and the Beads closeout cites the exact command,
  worker, target dir, metadata sidecar, and ledger entry.

Valid diagnostic evidence that is not enough to close:

- `target_worker_mirror_attestation` showing a missing tracked file. This can
  block the proof lane as an infrastructure/mirror issue, but it does not prove
  the source behavior under test.
- `scheduler_selected_remote_proof` on the same worker when the command was not
  explicitly target-worker-enforced. This may reduce residual risk but must be
  described as scheduler-selected evidence.
- `worker_self_test_only` from `rch check` or `rch status`. This only proves the
  substrate was partly reachable.

Required reopen triggers:

- A later run reports `RCH-REMOTE-MIRROR-MISSING-FILE` or equivalent missing
  tracked source on the same worker.
- A closeout claimed target-worker proof but retained artifacts lack the
  selected worker or repo snapshot.
- Local fallback markers appear in a proof cited as remote RCH evidence.

### Worker-Specific Incident Closeout Workflow

Use this workflow when a bead is about an RCH worker, selected-worker source
mirror, remote target directory, queue/preflight result, or worker-specific
failure code. It is the canonical handoff from an observed worker incident to a
Beads closeout decision.

1. Identify the material claim.
   - Source/test behavior claims need the material command log, proof-ledger
     JSONL, aggregate report, and metadata sidecar.
   - Mirror or queue claims may use static-only attestation, but must say that
     no material Cargo proof ran.
   - Operator/runbook claims may close on docs/static proof only when the bead
     scope is explicitly documentation or policy wiring.
2. Classify the evidence before deciding status.
   - Close as clean only for `target_worker_remote_proof` on the intended
     worker, or for non-worker-specific source/test claims with
     `proven_remote` and a named selected worker.
   - Block or keep open for `target_worker_mirror_attestation` with
     `source_mirror_status` of `missing`, `stale`, or `unreachable`.
   - Defer with residual risk for `scheduler_selected_remote_proof` when the
     scheduler selected a worker but the command was not target-worker
     enforced.
   - Reopen for `rejected_local_heavy`, `missing_artifact`, `malformed`,
     `inconclusive_worker_evidence`, local fallback markers without approval,
     or sync/transfer/self-test-only evidence cited as source proof.
3. Record the decision in Beads using exact artifacts.
   - Include bead id, scenario id, command, worker id, queue state if known,
     `worker_evidence_confidence`, `source_mirror_status`,
     `source_mirror_reason_code`, target-dir lifecycle, metadata sidecar,
     proof-ledger JSONL, aggregate report, and residual-risk note.
   - If Agent Mail is unavailable, put the same content in a Beads comment and
     do not attempt service repair.
4. Keep the action proportional.
   - Do not repair, restart, drain, or mutate shared services/workers unless the
     operator explicitly approves that exact action.
   - Do not delete local or remote files to create a negative fixture.
   - Do not describe RCH setup, queue placement, sync chatter, transfer logs,
     worker health, or smoke preflight as a source/test pass.
   - Do not run local heavy Cargo unless the evidence is explicitly
     `approved_local_fallback`.

Close a worker-specific source/mirror blocker only with a comment shaped like:

```toon
worker_specific_closeout{
  bead_id: ft-example.2
  decision: close
  claim: "loopback verifier reached and passed on intended worker"
  command: "run_rch_cargo_logged ... cargo test ..."
  worker_evidence_confidence: target_worker_remote_proof
  intended_worker_id: vmi1152480
  selected_worker_id: vmi1152480
  worker_queue_state: ready
  source_mirror_status: present
  source_mirror_reason_code: none
  target_dir_lifecycle: retained
  metadata_sidecar: tests/e2e/logs/.../loopback_test.log.rch_meta.json
  proof_ledger: tests/e2e/logs/.../proof-ledger.jsonl
  aggregate_report: tests/e2e/logs/.../proof-ledger-aggregate.json
  residual_risk: "none"
}
```

Block or reopen when the retained evidence says the worker substrate failed:

```toon
worker_specific_closeout{
  bead_id: ft-example.2
  decision: block
  reason_code: RCH-REMOTE-MIRROR-MISSING-FILE
  worker_evidence_confidence: target_worker_mirror_attestation
  selected_worker_id: vmi1153651
  source_mirror_status: missing
  source_mirror_reason_code: rch_mirror.missing_tracked_file
  artifact: tests/e2e/logs/.../rch_mirror_preflight.json
  action: "keep source/test bead open; fix mirror or rerun on an attested worker"
}
```

Use residual-risk wording when evidence is useful but weaker than the claim:

```toon
worker_specific_closeout{
  bead_id: ft-example.3
  decision: defer_with_residual_risk
  worker_evidence_confidence: scheduler_selected_remote_proof
  selected_worker_id: vmi1293453
  source_mirror_status: present
  residual_risk: "RCH scheduler selected the worker; the proof was not target-worker-enforced."
}
```

## Local Fallback Rule

Local fallback for heavy commands is allowed only when all are true:

1. `rch` is unavailable or remote workers are unhealthy.
2. Evidence entry includes non-empty `fallback_reason_code`.
3. Evidence entry includes non-empty `fallback_approved_by`.
4. `execution_mode` is `approved_local_fallback`.
5. `validation_status` is `approved_fallback`.
6. Residual risk note explains impact on comparability/reproducibility.

## Evidence Contract

Every proof-ledger run must be logged with fields:

1. `timestamp`
2. `command`
3. `command_fingerprint`
4. `command_class` (`heavy` or `light`)
5. `is_heavy`
6. `used_rch`
7. `worker_context`
8. `worker_context_fingerprint`
9. Optional `worker_evidence_confidence`
   (`target_worker_remote_proof`, `target_worker_mirror_attestation`,
   `scheduler_selected_remote_proof`, `worker_self_test_only`,
   `sync_or_transfer_only`, `inconclusive_worker_evidence`, or
   `legacy_unknown_worker_evidence`)
10. Optional `intended_worker_id`
11. Optional `selected_worker_id`
12. Optional `worker_queue_state` (`ready`, `busy_wait`, `unhealthy`,
   `unsupported_worker_selection`, `queue_timeout`, `unknown`, or
   `not_applicable`)
13. Optional `repo_snapshot_head` (40-character git SHA, `unknown`, or
   `not_applicable`)
14. Optional `source_mirror_status` (`present`, `missing`, `stale`,
   `unreachable`, `not_checked`, `unknown`, or `not_applicable`)
15. Optional `source_mirror_reason_code`
16. Optional `remote_cargo_reached`
17. Optional `remote_rustc_reached`
18. Optional `test_binary_reached`
19. `execution_mode` (`remote_rch`, `local_light`, `approved_local_fallback`,
   `local_fallback`, or `refused_local_fallback`)
20. `target_dir`
21. `target_dir_fingerprint`
22. `target_dir_lifecycle` (`not_applicable`, `retained`, `inventory_only`, or `cleanup_approved`)
23. `artifact_paths` (each path must exist when evidence is validated)
24. `artifact_paths_fingerprint`
25. `elapsed_seconds`
26. `exit_status`
27. `residual_risk_notes`
28. `residual_risk_notes_fingerprint`
29. `validation_status` (`valid`, `approved_fallback`, `fallback_required`,
    `fallback_refused`, `timeout`, or `invalid`)
30. Optional fallback fields when a heavy run is not confirmed as remote RCH:
   - `fallback_reason_code`
   - `fallback_approved_by`

Heavy runs must include a real `target_dir` and a non-`not_applicable`
`target_dir_lifecycle`. Light local checks should use `target_dir:
"not_applicable"` and `target_dir_lifecycle: "not_applicable"`.

Ledger-visible fields must already be redacted before they are written or
cited. The validator rejects known provider tokens, bearer/JWT-like secrets,
credential-looking key/value pairs, and SSH private-key paths in command,
worker context, target-dir, artifact-path, and residual-risk fields. The
`*_fingerprint` fields preserve stable correlation without requiring agents to
print raw sensitive values.

Machine-readable schema:

- `docs/asupersync-rch-evidence-schema.json` (`schema_version: 3`)

## Machine-Readable Examples

These examples use schema-v3 field names and the same category language emitted
by `--aggregate-ledger`. They are intentionally compact so a fresh agent can
copy the shape into a Beads comment, Robot/MCP closeout summary, or retained
artifact manifest without consulting chat history.

Valid remote RCH proof:

```json
{
  "schema_version": 3,
  "bead_id": "ft-example.1",
  "scenario_id": "aegis_diagnostics",
  "policy_version": "3.2.0",
  "runs": [
    {
      "timestamp": "2026-05-09T00:00:00Z",
      "command": "run_rch_cargo_logged tests/e2e/logs/ft-example/rch.log env CARGO_TARGET_DIR=target/rch-ft-example cargo test -p frankenterm-core --lib aegis -- --nocapture",
      "command_fingerprint": "sha256:95e9a352dd547b31424d2dd5da248d6bb8492d421f079edb23d9558e0a6d3432",
      "command_class": "heavy",
      "is_heavy": true,
      "used_rch": true,
      "worker_context": "worker=vmi1149989; queue=ready; head=dfde9e8ea; remote_cargo_reached=true",
      "worker_context_fingerprint": "sha256:303e72f290a5c036e562ec5df47d9e0e13da629685a2829cd1bedf94b0eef3f4",
      "execution_mode": "remote_rch",
      "target_dir": "target/rch-ft-example",
      "target_dir_fingerprint": "sha256:291cad282f1c5c51c5c200844c46b6e2a5447eb98bb8eb279c6f74f9cbeeb140",
      "target_dir_lifecycle": "retained",
      "artifact_paths": ["tests/e2e/test_asupersync_rch_execution_policy.sh"],
      "artifact_paths_fingerprint": "sha256:3ad0cb87e568876768f423eb2b4384212e3b8628674d8b337cc8e848def77cd2",
      "elapsed_seconds": 897.071,
      "exit_status": 0,
      "residual_risk_notes": "",
      "residual_risk_notes_fingerprint": "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
      "validation_status": "valid"
    }
  ]
}
```

Invalid local-heavy claim. This must be rejected as `rejected_local_heavy`
because the material Cargo command is local and there is no fallback approval:

```json
{
  "schema_version": 3,
  "bead_id": "ft-example.1",
  "scenario_id": "bad_local_claim",
  "policy_version": "3.2.0",
  "runs": [
    {
      "timestamp": "2026-05-09T00:01:00Z",
      "command": "cargo test -p frankenterm-core --lib aegis -- --nocapture",
      "command_fingerprint": "sha256:43920385b22f697db883fb74797d81cf4c3b923b3c07475cfb9080f0f7378ddc",
      "command_class": "heavy",
      "is_heavy": true,
      "used_rch": false,
      "worker_context": "local",
      "worker_context_fingerprint": "sha256:25bf8e1a2393f1108d37029b3df5593236c755742ec93465bbafa9b290bddcf6",
      "execution_mode": "remote_rch",
      "target_dir": "target/local-ft-example",
      "target_dir_fingerprint": "sha256:be8bfccac6decadf97a94abb3b6e5d54c78802421bdd3325649aa95e24b516bd",
      "target_dir_lifecycle": "retained",
      "artifact_paths": ["tests/e2e/test_asupersync_rch_execution_policy.sh"],
      "artifact_paths_fingerprint": "sha256:3ad0cb87e568876768f423eb2b4384212e3b8628674d8b337cc8e848def77cd2",
      "elapsed_seconds": 42.0,
      "exit_status": 0,
      "residual_risk_notes": "",
      "residual_risk_notes_fingerprint": "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
      "validation_status": "valid"
    }
  ]
}
```

Approved local fallback changes only the evidence classification, not the fact
that the command ran locally:

```toon
proof_closeout{
  bead_id: ft-example.1
  scenario_id: rch_unavailable_fallback
  category: approved_fallback
  execution_mode: approved_local_fallback
  validation_status: approved_fallback
  fallback_reason_code: RCH-NO-HEALTHY-WORKERS
  fallback_approved_by: human-operator
  residual_risk_notes: "Not comparable to remote RCH proof; rerun remotely before release-readiness."
}
```

Blocked verifier summaries should name the blocker rather than pretending a
proof exists:

```toon
proof_closeout{
  bead_id: ft-example.2
  category: missing_artifact
  blocked_verifier: true
  reason_code: aggregate.missing_artifact
  action: "Keep the bead open and rerun the verifier after retaining the log bundle."
}
```

Remote-required fallback refusals are blocked verifier evidence, not approved
fallbacks:

```toon
proof_closeout{
  bead_id: ft-example.3
  category: blocked_verifier
  blocked_verifier: true
  execution_mode: refused_local_fallback
  validation_status: fallback_refused
  reason_code: RCH-REMOTE-REQUIRED-FALLBACK-REFUSED
  action: "Keep the bead open and wait for an admissible remote worker; no local heavy proof ran."
}
```

## Closeout Migration Rule

Older closeouts sometimes treated RCH setup, sync, transfer, worker selection,
or smoke-preflight chatter as if it proved the source command. New closeouts
must cite proof-ledger artifacts instead:

1. Cite the proof-ledger JSONL path and aggregate report path.
2. Name the category from the aggregate gate: `proven_remote`, `light_local`,
   `approved_fallback`, `blocked_verifier`, `rejected_local_heavy`,
   `missing_artifact`, `malformed`, or `residual_risk_only`.
3. For a clean source/test claim, require `proven_remote` for the material
   heavy command. `light_local` is acceptable only for static-only checks.
4. For `approved_fallback` or `residual_risk_only`, state the residual risk in
   the closeout. Do not claim a clean pass.
5. For `blocked_verifier`, `rejected_local_heavy`, `missing_artifact`, or
   `malformed`, keep the bead open or reopen it with the retained failure
   report.

## Validation Tooling

Policy validator:

```bash
bash scripts/validate_asupersync_rch_execution_policy.sh --self-test
bash scripts/validate_asupersync_rch_execution_policy.sh --classify "cargo test --workspace"
bash scripts/validate_asupersync_rch_execution_policy.sh --redact-text "API_KEY=... cargo test"
bash scripts/validate_asupersync_rch_execution_policy.sh --validate-evidence <path-to-evidence.json>
bash scripts/validate_asupersync_rch_execution_policy.sh --aggregate-ledger <path-to-ledger.jsonl> [more-ledgers.jsonl ...]
```

E2E policy validation:

```bash
bash tests/e2e/test_asupersync_rch_execution_policy.sh
```

Shared RCH wrapper emission is opt-in. Harnesses that source
`tests/e2e/lib_rch_guards.sh` can set:

```bash
RCH_PROOF_LEDGER_FILE=tests/e2e/logs/<run>/proof-ledger.jsonl
RCH_PROOF_LEDGER_BEAD_ID=ft-...
RCH_PROOF_LEDGER_SCENARIO_ID=<scenario>
```

When those variables are present, `ensure_rch_ready`,
`run_rch_cargo_logged`, and `run_rch_cargo_logged_with_timeout` append
schema-v3 proof-ledger JSONL entries for probe/smoke and remote Cargo logs.
Successful remote entries validate directly. Fail-open local fallback,
timeouts, non-zero wrapper exits, missing metadata, or unredacted public fields
produce entries that are intentionally not valid proof.

Aggregate quality gate:

- `--aggregate-ledger` scans one or more proof-ledger JSONL files, validates
  every run in every retained evidence object, and emits an operator report.
- Each row carries the bead ID, scenario ID, command, worker context, artifact
  path(s), category, worker-evidence confidence, worker-evidence category,
  source mirror status, and stable reason code so Beads and release-readiness
  comments can cite the exact proof shape.
- Categories are:
  - `proven_remote`: heavy Cargo proof ran through an RCH-recognized remote path.
  - `light_local`: local non-heavy checks such as formatting.
  - `approved_fallback`: heavy local fallback with explicit approval metadata.
  - `blocked_verifier`: remote-required fallback was refused, the verifier
    timed out, or the wrapper exited without valid proof evidence.
  - `rejected_local_heavy`: heavy local proof without required fallback approval.
  - `malformed`: invalid JSON, stale schema, missing required fields, or malformed bead IDs.
  - `missing_artifact`: an artifact path named by evidence is not retained.
  - `residual_risk_only`: otherwise valid evidence with residual risk notes.
- Blocking categories are `blocked_verifier`, `rejected_local_heavy`,
  `malformed`, and `missing_artifact`. They make the aggregate command exit
  non-zero.
- `approved_fallback` and `residual_risk_only` produce an `overall_verdict` of
  `partial_risk`; this is acceptable evidence only when the operator-facing
  closeout names the residual risk instead of claiming a clean pass.
- `worker_evidence_counts` separately tracks `target_worker_remote`,
  `scheduler_selected_remote`, `mirror_attested`, `mirror_failed`,
  `worker_self_test_only`, `sync_or_transfer_only`,
  `inconclusive_worker_evidence`, and `legacy_unknown_worker_evidence` so
  direct worker proof, indirect scheduler proof, and mirror-failed diagnostics
  are not conflated.

The self-test and E2E cover accepted remote proof, rejected local-heavy proof,
accepted human-approved fallback, light local commands, stale schema versions,
malformed bead IDs, missing artifacts, missing required booleans, unredacted
provider tokens, SSH-style secret paths, wrapper-emitted ledger records, RCH
setup chatter, shell wrappers that mention RCH while running Cargo locally, and
aggregate quality-gate classification for mixed, missing, rejected, malformed,
target-worker, scheduler-selected, and mirror-failed proof-ledger JSONL.

## User Impact

1. Prevents accidental local compilation storms from degrading operator session responsiveness.
2. Preserves reproducibility and auditability for migration, storage, fleet, GUI, search, release, and future proof lanes.
3. Makes degraded-mode exceptions explicit and reviewable instead of implicit.
